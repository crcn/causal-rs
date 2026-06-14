//! Partitioned runner for [`Reactor`](crate::reactor::Reactor)
//! consumers (0.10 step 2).
//!
//! One dispatcher per consumer, many partitions, one worker task per
//! active partition:
//!
//!   1. **Ingest** — the dispatcher reads the log from an in-memory
//!      ingestion cursor, routes each matching trigger to its
//!      partition per the reactor's declared
//!      [`Ordering`](crate::reactor::Ordering) (BLOCKING-4 memo:
//!      `PerSubject` keys on the trigger's `(SUBJECT, subject_id)`,
//!      `PerWorkflow` on its `workflow_id`, `None` per event), and
//!      skips non-matching events.
//!   2. **Process** — each partition's worker pops triggers FIFO and
//!      runs the reaction: deserialize → `react()` → append each
//!      output directly to its own stream with a deterministic
//!      identity-keyed `event_id` (see [`derive_output_event_id`]).
//!      Failures retry **worker-local** per the BLOCKING-2 class
//!      taxonomy (see [`crate::failure`]): transient backs off under a
//!      liveness-time ceiling, poison parks immediately, domain /
//!      unclassified parks after bounded attempts — always via a
//!      terminal fact (the registered mapper's, or the built-in
//!      [`REACTION_FAILED_KIND`] on the trigger's own subject
//!      history). A retrying trigger occupies only its own partition,
//!      never the consumer.
//!   3. **Ack-floor checkpoint** (BLOCKING-3) — the durable cursor is
//!      the highest position with no unacked matching trigger at or
//!      below it. Crash redelivery replays at most the pending-work
//!      window (`MAX_PENDING` matching triggers), never the log
//!      distance a slow partition fell behind. Redelivered reactions
//!      dedup on their identity-keyed `event_id`s (C1).
//!
//! State reads inside `react()` (`ctx.state_of`) are position-bounded
//! **fold-on-read** from the log (BLOCKING-1): deterministic at the
//! trigger's position regardless of partition interleaving, cached
//! incrementally per worker (the cache dies with its partition).
//!
//! `settle` no longer reads this consumer's durable cursor — it asks
//! [`ReactorRunner::drained`]: ingestion reached the workflow's
//! high-water AND no queued/in-flight triggers remain for that
//! workflow here. A reactor wedged on one workflow's trigger cannot
//! hold another workflow's settle hostage.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::Arc;

use anyhow::Result;
use futures::FutureExt;
use serde::de::DeserializeOwned;
use std::panic::AssertUnwindSafe;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::aggregator::{AggregatorRegistry, FoldOnReadCache};
use crate::checkpoint_store::ReactorCheckpoint;
use crate::clock::Clock;
use crate::contexts::{Ctx, LabelSet, StateSource};
use crate::effect_store::EffectKey;
use crate::engine::{TerminalFailure, TerminalFailureMapper};
use crate::failure::{classify, poison, ErrorClass, FailureClass};
use crate::event_log::EventLogBackend;
use crate::event::Event;
use crate::projection_runner::StepOutcome;
use crate::reactor_observer::ReactorObserver;
use crate::reactor::{Ordering, Reactor};
use crate::types::{EventData, LogCursor, RecordedEvent, StreamState};

/// Pending-work window (BLOCKING-3): the maximum number of matching
/// triggers queued + in-flight per consumer. Ingestion pauses at the
/// cap, which bounds crash-redelivery span by *work outstanding* —
/// never by how far the log head ran ahead of a slow partition.
const MAX_PENDING: usize = 4096;

/// The transient-class retry ceiling (BLOCKING-2): how long a
/// transient-classified failure may back off before parking as
/// `transient_exhausted`. Measured in **liveness time** (tokio
/// `Instant` — virtualizable under `start_paused`), never chrono:
/// a wall-clock ceiling would make this machinery testable only in
/// production. Hours, not attempts — a ten-minute infra outage must
/// self-heal instead of mass-parking every graph-touching trigger.
/// Ceilinged rather than unbounded because misclassification is
/// inevitable, and an unbounded-transient partition is a silent
/// permanent wedge.
const TRANSIENT_CEILING: std::time::Duration = std::time::Duration::from_secs(6 * 60 * 60);

/// The built-in terminal fact's kind, appended when no
/// `on_terminal_failure` mapper is registered. The colon namespaces it
/// out of user vocabularies (kinds are matched exactly; colons are just
/// characters since 0.10 step 1).
pub const REACTION_FAILED_KIND: &str = "causal:reaction_failed";


/// Namespace UUID for deriving deterministic reactor-output event_ids
/// via uuid v5. Hardcoded so the same identity inputs always produce
/// the same event_id across processes / restarts — that's what makes
/// the log's idempotent-append-on-event_id collapse retried reactor
/// runs into a single durable entry (C1 + C12).
pub const NS_REACTOR_OUTPUT: Uuid = Uuid::from_bytes([
    0x4d, 0xfe, 0xc4, 0xf2, 0x6e, 0x88, 0x4f, 0x3a,
    0xa9, 0xb1, 0x7c, 0x5e, 0xb3, 0x39, 0x21, 0x0a,
]);

/// Compute the deterministic event_id for one reactor output —
/// **identity-keyed, not position-keyed** (0.10 step 1, chunk 7d):
///
/// ```text
/// v5( consumer NAME ∥ trigger event_id ∥ fact NAME ∥ subject_id ∥ nth )
/// ```
///
/// `nth` is the ordinal among outputs of the SAME (kind, subject)
/// within one reaction — the only place position survives, exactly
/// where it's semantically honest (a second identical-identity output
/// IS a new item). The old positional key (`v5(reactor ∥ trigger ∥
/// index)`) was deploy-brittle: a reactor emitting `[A, B]` today and
/// `[A, NEW, B]` tomorrow shifted B's index, so a redelivery straddling
/// the deploy derived a different id for the same logical output — B
/// appended twice (0.8) or tripped the divergence check (0.9). Keys
/// derived from what the fact declares are stable under reordering and
/// insertion by construction.
///
/// Public so backend impls and tests can reproduce the derivation.
pub fn derive_output_event_id(
    consumer: &str,
    trigger_event_id: Uuid,
    kind: &str,
    subject_id: Uuid,
    nth: u32,
) -> Uuid {
    // NUL-byte separator: never appears in valid identifiers or UUIDs,
    // so the encoding is unambiguous regardless of name contents.
    let mut key: Vec<u8> =
        Vec::with_capacity(consumer.len() + kind.len() + 16 + 16 + 4 + 4);
    key.extend_from_slice(consumer.as_bytes());
    key.push(0);
    key.extend_from_slice(trigger_event_id.as_bytes());
    key.push(0);
    key.extend_from_slice(kind.as_bytes());
    key.push(0);
    key.extend_from_slice(subject_id.as_bytes());
    key.push(0);
    key.extend_from_slice(&nth.to_le_bytes());
    Uuid::new_v5(&NS_REACTOR_OUTPUT, &key)
}

/// Metadata stamped on every reactor output so consumers (the inspector in
/// particular) can attribute the emitted event to the reactor that produced
/// it. Read back via `metadata["reactor_id"]`.
fn reactor_output_metadata(reactor_id: &str) -> serde_json::Map<String, serde_json::Value> {
    let mut m = serde_json::Map::new();
    m.insert(
        "reactor_id".to_string(),
        serde_json::Value::String(reactor_id.to_string()),
    );
    m
}

// ─────────────────────────────────────────────────────────────────────
// Dispatch state
// ─────────────────────────────────────────────────────────────────────

/// Partition identity per the reactor's declared `Ordering`.
#[derive(Clone, PartialEq, Eq, Hash)]
enum PartitionKey {
    /// `Ordering::PerSubject` — the trigger's placement stream.
    Subject(String, Uuid),
    /// `Ordering::PerWorkflow` — the trigger's workflow.
    Workflow(Uuid),
    /// `Ordering::None` — every trigger is its own partition.
    Event(Uuid),
}

fn partition_key_for(ordering: Ordering, event: &RecordedEvent) -> PartitionKey {
    match ordering {
        Ordering::PerSubject => {
            PartitionKey::Subject(event.category.clone(), event.subject_id)
        }
        Ordering::PerWorkflow => PartitionKey::Workflow(event.workflow_id),
        Ordering::None => PartitionKey::Event(event.event_id),
    }
}

struct PartitionHandle {
    tx: mpsc::UnboundedSender<RecordedEvent>,
    /// Generation guard for self-eviction: a worker deregisters its
    /// partition only if the map still holds *its* generation, so an
    /// exit racing a respawn can't evict the successor.
    gen: u64,
}

struct PendingTrigger {
    event_id: Uuid,
    acked: bool,
    /// True when the trigger parked as a terminal failure — its effect
    /// entries are exempt from floor-GC (failure replay needs them).
    parked: bool,
    /// `ctx.effect` labels this trigger used (union across attempts) —
    /// the floor-GC's deletion list.
    effect_labels: Vec<String>,
}

struct DispatchState {
    /// True once the durable checkpoint has seeded `ingest_pos`.
    started: bool,
    /// In-memory ingestion cursor — how far this consumer has SCANNED
    /// (not how far it has finished; that's `floor`).
    ingest_pos: LogCursor,
    /// Matching triggers outstanding (queued or in-flight), by log
    /// position. The ack-floor is the contiguous acked prefix.
    pending: BTreeMap<LogCursor, PendingTrigger>,
    /// Durable-checkpoint mirror.
    floor: LogCursor,
    /// Last floor actually persisted (so a failed persist retries).
    persisted_floor: Option<LogCursor>,
    /// Live partitions.
    partitions: HashMap<PartitionKey, PartitionHandle>,
    next_gen: u64,
    /// Outstanding trigger count per workflow — the `drained` probe's
    /// "no queued/in-flight work for this workflow" half.
    wf_pending: HashMap<Uuid, u32>,
}

struct Completion {
    position: LogCursor,
    workflow: Uuid,
    parked: bool,
    effect_labels: Vec<String>,
}

enum AttemptOutcome {
    /// Reaction succeeded; outputs are durable.
    Done,
    /// The reaction body failed (deserialization, `react()` error or
    /// panic, OCC-fence violation). Carries the error — classified per
    /// the BLOCKING-2 taxonomy where the failure is structurally
    /// deterministic — and the persisted attempt count. The retry-loop
    /// owner ([`Core::process_trigger`]) decides retry vs. park.
    /// Runner-infrastructure failures (log/checkpoint I/O) are `Err`,
    /// never this variant — they retry indefinitely and can never park
    /// a trigger.
    BodyFailed { error: anyhow::Error, attempts: u32 },
}

/// What `process_trigger` reports back to the worker loop: how the
/// trigger finished, and which effect labels it used (the floor-GC's
/// deletion list).
struct TriggerDone {
    parked: bool,
    effect_labels: Vec<String>,
}

// ─────────────────────────────────────────────────────────────────────
// Runner
// ─────────────────────────────────────────────────────────────────────

pub struct ReactorRunner<R: Reactor> {
    core: Arc<Core<R>>,
}

struct Core<R: Reactor> {
    reactor:     R,
    consumer_id: String,
    log:         Arc<dyn EventLogBackend>,
    checkpoint:  Arc<dyn ReactorCheckpoint>,
    /// Fold-function table for `ctx.state_of` fold-on-read. The
    /// registry's own mutable state is unused on this path — the
    /// partitioned runner reads bounded folds from the log instead of
    /// maintaining a scan-folded copy (which would race ahead of a
    /// worker's trigger).
    fold_registry: Option<Arc<AggregatorRegistry>>,
    /// Terminal-failure mapper. When a trigger exhausts its retry
    /// policy (per the BLOCKING-2 class taxonomy), the mapper is
    /// invoked; on `Some(fact)`, the synthesized fact is appended
    /// directly to its stream. Without a mapper the runner appends the
    /// built-in [`REACTION_FAILED_KIND`] fact to the TRIGGER's own
    /// subject history — the terminal path is mandatory; there is no
    /// retry-forever mode.
    failure_mapper:  Option<TerminalFailureMapper>,
    /// Retry budget and backoff shape for domain-class / unclassified
    /// errors. Transient errors are governed by [`TRANSIENT_CEILING`]
    /// (liveness time); poison parks immediately.
    retry_policy: crate::reactor::RetryPolicy,
    /// Inspector / telemetry hook. Default `None` = zero overhead.
    observer:    Option<Arc<dyn ReactorObserver>>,
    /// Reaction-result cache. Surfaced to the reactor body via
    /// `ctx.effect_store()` so side-effecting reactors memoize their
    /// external call under the reaction key — safe under retry/redelivery.
    effect_store: Option<Arc<dyn crate::effect_store::EffectStore>>,
    /// Engine-level aggregator registry. Reactor outputs are folded into
    /// it after they're appended, so `engine.state_of::<A>(id)` reflects
    /// reactor-emitted events (not just caller-emitted ones).
    engine_aggregators: Option<Arc<AggregatorRegistry>>,
    /// Shared per-workflow high-water tracker for scoped `Engine::settle`.
    settle_tracker: Option<crate::engine::WorkflowHighWater>,
    /// Durable snapshot store for save-after-output-fold (shared engine
    /// registry). `None` = no durable persistence.
    snapshot_store: Option<Arc<dyn crate::snapshot_store::SnapshotStore>>,
    snapshot_every: u64,
    /// Categories registered `StreamPolicy::OccRequired`. A reactor
    /// output whose routing category is in this set is rejected —
    /// reactors append with `StreamState::Any` and cannot uphold an
    /// aggregate's OCC invariant. Empty = no fence.
    occ_categories: Arc<std::collections::HashSet<String>>,
    /// Calendar clock for every timestamp this runner stamps
    /// (`created_at` on outputs and terminal facts, observer attempt
    /// times). Injected so tests pin it; liveness decisions never use
    /// it (those are tokio time).
    clock: Arc<dyn Clock>,

    /// `Reactor::MAX_IN_FLIGHT` enforcement: workers acquire a permit
    /// around each executing attempt (not around backoff waits — a
    /// sleeping retry holds no external resource). `None` = unbounded.
    in_flight: Option<Arc<tokio::sync::Semaphore>>,

    dispatch: parking_lot::Mutex<DispatchState>,
    completions_tx: mpsc::UnboundedSender<Completion>,
    completions_rx: parking_lot::Mutex<mpsc::UnboundedReceiver<Completion>>,
    /// Halt flag: workers stop taking new triggers / retry attempts.
    stopped: AtomicBool,
    stop_notify: tokio::sync::Notify,
    /// Shared cancel fence. Triggers whose `workflow_id` is present are
    /// acked at the dispatch gate without reaching the reactor body;
    /// workers also check it on each loop boundary for already-queued
    /// triggers. Cancel markers in the log are ingested inline.
    cancelled_workflows: Option<crate::engine::CancelledWorkflows>,
}

impl<R: Reactor + 'static> ReactorRunner<R>
where
    R::Trigger: DeserializeOwned,
{
    pub fn new(
        reactor: R,
        consumer_id: impl Into<String>,
        log: Arc<dyn EventLogBackend>,
        checkpoint: Arc<dyn ReactorCheckpoint>,
    ) -> Self {
        let retry_policy = reactor
            .retry_policy()
            .unwrap_or_else(|| crate::reactor::RetryPolicy::from_max_attempts(
                crate::engine::DEFAULT_MAX_ATTEMPTS,
            ));
        let (completions_tx, completions_rx) = mpsc::unbounded_channel();
        Self {
            core: Arc::new(Core {
                reactor,
                consumer_id: consumer_id.into(),
                log,
                checkpoint,
                fold_registry: None,
                failure_mapper: None,
                retry_policy,
                observer: None,
                effect_store: None,
                engine_aggregators: None,
                settle_tracker: None,
                snapshot_store: None,
                snapshot_every: 0,
                occ_categories: Arc::new(std::collections::HashSet::new()),
                clock: Arc::new(crate::clock::SystemClock),
                in_flight: (R::MAX_IN_FLIGHT != usize::MAX).then(|| {
                    Arc::new(tokio::sync::Semaphore::new(
                        // Semaphore's permit ceiling is below usize::MAX;
                        // any practical cap fits.
                        R::MAX_IN_FLIGHT.min(tokio::sync::Semaphore::MAX_PERMITS),
                    ))
                }),
                dispatch: parking_lot::Mutex::new(DispatchState {
                    started: false,
                    ingest_pos: LogCursor::ZERO,
                    pending: BTreeMap::new(),
                    floor: LogCursor::ZERO,
                    persisted_floor: None,
                    partitions: HashMap::new(),
                    next_gen: 0,
                    wf_pending: HashMap::new(),
                }),
                completions_tx,
                completions_rx: parking_lot::Mutex::new(completions_rx),
                stopped: AtomicBool::new(false),
                stop_notify: tokio::sync::Notify::new(),
                cancelled_workflows: None,
            }),
        }
    }

    /// Builder-time mutable access. Valid only before any worker has
    /// been spawned (construction happens before `step` ever runs, so
    /// the Arc is still uniquely owned).
    fn core_mut(&mut self) -> &mut Core<R> {
        Arc::get_mut(&mut self.core)
            .expect("ReactorRunner builder methods must run before the runner starts")
    }

    /// Plumb the engine's OCC-required category set so the runner can
    /// reject reactor outputs that would bypass an aggregate's
    /// optimistic-concurrency fence.
    pub(crate) fn with_occ_categories(
        mut self,
        occ_categories: Arc<std::collections::HashSet<String>>,
    ) -> Self {
        self.core_mut().occ_categories = occ_categories;
        self
    }

    /// Attach a [`ReactorObserver`] for inspector / telemetry capture.
    pub fn with_observer(mut self, observer: Arc<dyn ReactorObserver>) -> Self {
        self.core_mut().observer = Some(observer);
        self
    }

    /// Attach the aggregator fold-function table backing
    /// `ctx.state_of` fold-on-read. (The partitioned runner keeps no
    /// scan-folded registry state — reads are position-bounded folds
    /// from the log.)
    pub fn with_aggregators(mut self, aggregators: Arc<AggregatorRegistry>) -> Self {
        self.core_mut().fold_registry = Some(aggregators);
        self
    }

    /// Attach the reaction-result cache, surfaced to the reactor body via
    /// `ctx.effect_store()`.
    pub fn with_effect_store(
        mut self,
        cache: Arc<dyn crate::effect_store::EffectStore>,
    ) -> Self {
        self.core_mut().effect_store = Some(cache);
        self
    }

    /// Attach the engine-level aggregator registry so reactor outputs
    /// fold into it after append (keeps `engine.state_of` current).
    pub fn with_engine_aggregators(
        mut self,
        engine_aggregators: Option<Arc<AggregatorRegistry>>,
    ) -> Self {
        self.core_mut().engine_aggregators = engine_aggregators;
        self
    }

    /// Attach the shared per-workflow high-water tracker so appended
    /// outputs advance their run's `settle` mark.
    pub(crate) fn with_settle_tracker(mut self, tracker: crate::engine::WorkflowHighWater) -> Self {
        self.core_mut().settle_tracker = Some(tracker);
        self
    }

    /// Wire durable snapshot persistence for the engine-registry folds
    /// of reactor outputs. `None` store disables (unchanged behavior).
    pub(crate) fn with_snapshot_persistence(
        mut self,
        store: Option<Arc<dyn crate::snapshot_store::SnapshotStore>>,
        every: u64,
    ) -> Self {
        self.core_mut().snapshot_store = store;
        self.core_mut().snapshot_every = every;
        self
    }

    /// Replace the built-in terminal fact with a user mapper. When a
    /// trigger exhausts its retry policy (BLOCKING-2 taxonomy), the
    /// mapper is invoked; on `Some(fact)`, the fact is appended
    /// directly to its stream and the trigger parks.
    pub(crate) fn with_terminal_failure(mut self, mapper: TerminalFailureMapper) -> Self {
        self.core_mut().failure_mapper = Some(mapper);
        self
    }

    /// Override the retry policy for this runner. Replaces both the
    /// attempt budget and the backoff shape. Called by the engine builder
    /// after resolving the effective policy (per-reactor override →
    /// engine-wide default → `DEFAULT_MAX_ATTEMPTS`).
    pub(crate) fn with_retry_policy(mut self, p: crate::reactor::RetryPolicy) -> Self {
        self.core_mut().retry_policy = p;
        self
    }

    /// Convenience shim: set only `max_attempts`, keep existing backoff shape.
    pub(crate) fn with_max_attempts(mut self, n: u32) -> Self {
        self.core_mut().retry_policy = crate::reactor::RetryPolicy::from_max_attempts(n);
        self
    }

    /// Inject the calendar clock for runner-stamped timestamps.
    pub(crate) fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.core_mut().clock = clock;
        self
    }

    /// Wire the engine's shared cancel fence so this runner skips
    /// triggers belonging to cancelled workflows.
    pub(crate) fn with_cancelled_workflows(
        mut self,
        cancelled_workflows: crate::engine::CancelledWorkflows,
    ) -> Self {
        self.core_mut().cancelled_workflows = Some(cancelled_workflows);
        self
    }

    pub fn consumer_id(&self) -> &str { &self.core.consumer_id }

    /// Stop taking new work: workers exit at their next loop boundary
    /// (an in-flight `react()` completes; it is not cancelled).
    pub fn halt(&self) {
        self.core.stopped.store(true, AtomicOrdering::SeqCst);
        self.core.stop_notify.notify_waiters();
    }

    /// One dispatcher turn: reap completions (advancing the ack-floor
    /// checkpoint), then ingest up to `batch` events into partitions,
    /// window permitting. Body failures never surface here — they are
    /// the owning worker's business (retry / park). `Err` means
    /// infrastructure failure (log read, checkpoint write).
    pub async fn step(&self, batch: usize) -> Result<StepOutcome> {
        self.core.ensure_started().await?;
        let reaped = self.core.reap().await?;

        // Ingest, window permitting.
        let (from, room) = {
            let d = self.core.dispatch.lock();
            (d.ingest_pos, MAX_PENDING.saturating_sub(d.pending.len()))
        };
        let mut ingested = 0usize;
        if room > 0 {
            let events = self.core.log.read_all(from, batch).await?;
            let kind = <R::Trigger as Event>::NAME;
            let mut d = self.core.dispatch.lock();
            for event in events {
                if crate::event_type::matches_kind(&event.event_type, kind) {
                    if d.pending.len() >= MAX_PENDING {
                        break; // window full — do not scan past this trigger
                    }
                    let fenced = self.core.cancelled_workflows.as_ref()
                        .map(|f| f.lock().unwrap().contains(&event.workflow_id))
                        .unwrap_or(false);
                    if fenced {
                        // Dispatch-gate: ack at the gate without entering
                        // pending or wf_pending — the floor catches up naturally.
                        d.ingest_pos = event.position;
                    } else {
                        d.pending.insert(
                            event.position,
                            PendingTrigger {
                                event_id: event.event_id,
                                acked: false,
                                parked: false,
                                effect_labels: Vec::new(),
                            },
                        );
                        *d.wf_pending.entry(event.workflow_id).or_insert(0) += 1;
                        d.ingest_pos = event.position;
                        let key = partition_key_for(R::ORDERING, &event);
                        Core::enqueue(&self.core, &mut d, key, event);
                    }
                } else {
                    // Non-matching: scan past. Recognise cancel markers
                    // inline so the fence is current for subsequent triggers
                    // in the same batch.
                    if event.event_type == crate::engine::WORKFLOW_CANCELLED_KIND {
                        if let Some(fence) = &self.core.cancelled_workflows {
                            if let Some(target) = event.payload.get("target")
                                .and_then(|v| v.as_str())
                                .and_then(|s| s.parse::<Uuid>().ok())
                            {
                                fence.lock().unwrap().insert(target);
                            }
                        }
                    }
                    d.ingest_pos = event.position;
                }
                ingested += 1;
            }
        }
        // A scan with no outstanding triggers moves the floor too.
        self.core.advance_floor().await?;

        if reaped + ingested > 0 {
            Ok(StepOutcome::Progressed { applied: reaped })
        } else {
            Ok(StepOutcome::Idle)
        }
    }

    /// The settle probe: has this consumer fully finished workflow `wf`
    /// up to high-water `hw`? True iff ingestion has scanned to `hw`
    /// AND no trigger of `wf` is queued or in flight here. Reaps
    /// completions first so acks register without waiting on the
    /// supervisor's next step.
    pub async fn drained(&self, wf: Uuid, hw: LogCursor) -> Result<bool> {
        let started = { self.core.dispatch.lock().started };
        if !started {
            // Not yet stepped: the durable cursor is all there is.
            let c = self.core.checkpoint.get(&self.core.consumer_id).await?
                .unwrap_or(LogCursor::ZERO);
            return Ok(c >= hw);
        }
        self.core.reap().await?;
        let d = self.core.dispatch.lock();
        Ok(d.ingest_pos >= hw && !d.wf_pending.contains_key(&wf))
    }

    /// Test helper: step until every ingested trigger is acked and the
    /// log is fully scanned. Panics if quiescence takes > 10s (a wedged
    /// fixture — use explicit waits for those scenarios instead).
    #[cfg(test)]
    pub(crate) async fn quiesce(&self) -> Result<()> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            self.step(256).await?;
            let (done_pending, from) = {
                let d = self.core.dispatch.lock();
                (d.pending.is_empty(), d.ingest_pos)
            };
            if done_pending && self.core.log.read_all(from, 1).await?.is_empty() {
                return Ok(());
            }
            assert!(
                std::time::Instant::now() < deadline,
                "runner did not quiesce within 10s",
            );
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    }
}

impl<R: Reactor + 'static> Core<R>
where
    R::Trigger: DeserializeOwned,
{
    fn stopped(&self) -> bool {
        self.stopped.load(AtomicOrdering::SeqCst)
    }

    /// Seed `ingest_pos` / `floor` from the durable checkpoint, once.
    async fn ensure_started(&self) -> Result<()> {
        if self.dispatch.lock().started {
            return Ok(());
        }
        let cursor = self.checkpoint.get(&self.consumer_id).await?
            .unwrap_or(LogCursor::ZERO);
        let mut d = self.dispatch.lock();
        if !d.started {
            d.ingest_pos = cursor;
            d.floor = cursor;
            d.persisted_floor = Some(cursor);
            d.started = true;
        }
        Ok(())
    }

    /// Route one trigger into its partition, spawning the worker if the
    /// partition is cold (or its previous worker died out-of-band — the
    /// failed send returns the event, so nothing is lost). Caller holds
    /// the dispatch lock.
    fn enqueue(
        core: &Arc<Self>,
        d: &mut DispatchState,
        key: PartitionKey,
        mut event: RecordedEvent,
    ) {
        if let Some(handle) = d.partitions.get(&key) {
            match handle.tx.send(event) {
                Ok(()) => return,
                Err(mpsc::error::SendError(ev)) => {
                    // Receiver gone without deregistering (worker task
                    // killed out-of-band). Reclaim the event, respawn.
                    d.partitions.remove(&key);
                    event = ev;
                }
            }
        }
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(event).expect("fresh channel send cannot fail");
        let gen = d.next_gen;
        d.next_gen += 1;
        d.partitions.insert(key.clone(), PartitionHandle { tx, gen });
        let core = core.clone();
        tokio::spawn(async move {
            Self::worker_loop(core, key, gen, rx).await;
        });
    }

    /// One partition's worker: pop FIFO, process to completion (retry /
    /// park), ack. Exits when the queue is empty (re-checked under the
    /// dispatch lock so a racing enqueue can't strand a trigger) or on
    /// halt. The fold-on-read cache lives here — partition-scoped by
    /// construction.
    async fn worker_loop(
        core: Arc<Self>,
        key: PartitionKey,
        gen: u64,
        mut rx: mpsc::UnboundedReceiver<RecordedEvent>,
    ) {
        let fold_cache = FoldOnReadCache::default();
        loop {
            if core.stopped() {
                return;
            }
            let event = match rx.try_recv() {
                Ok(ev) => ev,
                Err(mpsc::error::TryRecvError::Empty) => {
                    // Deregister-or-continue, atomically vs. enqueue.
                    let mut d = core.dispatch.lock();
                    match rx.try_recv() {
                        Ok(ev) => ev,
                        Err(_) => {
                            if d.partitions.get(&key).map(|h| h.gen) == Some(gen) {
                                d.partitions.remove(&key);
                            }
                            return;
                        }
                    }
                }
                Err(mpsc::error::TryRecvError::Disconnected) => return,
            };
            // Worker-level fence: ack immediately for already-queued
            // triggers whose workflow was cancelled after dispatch.
            if let Some(fence) = &core.cancelled_workflows {
                if fence.lock().unwrap().contains(&event.workflow_id) {
                    let _ = core.completions_tx.send(Completion {
                        position: event.position,
                        workflow: event.workflow_id,
                        parked: false,
                        effect_labels: vec![],
                    });
                    continue;
                }
            }
            match core.process_trigger(&event, &fold_cache).await {
                Some(done) => {
                    let _ = core.completions_tx.send(Completion {
                        position: event.position,
                        workflow: event.workflow_id,
                        parked: done.parked,
                        effect_labels: done.effect_labels,
                    });
                }
                None => {
                    // Halted mid-processing: no ack; floor stays below
                    // the trigger so restart redelivers it.
                    return;
                }
            }
        }
    }

    /// Drive one trigger to completion under the BLOCKING-2 retry
    /// taxonomy. Returns false only on halt.
    ///
    /// - **Poison** (declared, or structurally deterministic: trigger
    ///   deserialization failure, OCC-fence violation) parks
    ///   immediately — retry reproduces the failure.
    /// - **Transient** backs off (capped exponential) up to
    ///   [`TRANSIENT_CEILING`] of liveness time since the first
    ///   transient failure, then parks as `transient_exhausted`. The
    ///   attempt budget does NOT apply — a ten-minute outage must not
    ///   mass-park triggers. (The ceiling clock is in-memory; a process
    ///   restart restarts it — at worst an outage straddling crashes
    ///   parks later, never silently looping forever.)
    /// - **Domain / unclassified** retries up to `max_attempts`, then
    ///   parks (labeled `domain` vs `unclassified` honestly).
    /// - **Runner-infrastructure errors** (`Err` from the attempt:
    ///   log/checkpoint I/O) back off and retry indefinitely — the
    ///   runner's own failures never park a trigger.
    async fn process_trigger(
        self: &Arc<Self>,
        event: &RecordedEvent,
        fold_cache: &FoldOnReadCache,
    ) -> Option<TriggerDone> {
        let mut local_attempt: u32 = 0;
        let mut transient_since: Option<tokio::time::Instant> = None;
        // Union of ctx.effect labels across attempts — the floor-GC's
        // deletion list (an early attempt may have memoized an effect
        // a later attempt's path never touches).
        let mut used_effects: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        // Effect label set from the first attempt — divergence backstop.
        // Compared as a SET (not sequence) so concurrent effects via
        // try_join_all don't produce false positives from poll-order variance.
        let mut first_attempt_effects: Option<std::collections::HashSet<String>> = None;
        loop {
            if self.stopped() {
                return None;
            }
            let labels = LabelSet::default();
            // MAX_IN_FLIGHT permit spans the executing attempt only —
            // released before any backoff sleep.
            let permit = match self.in_flight.as_ref() {
                Some(sem) => {
                    tokio::select! {
                        _ = self.stop_notify.notified() => return None,
                        p = sem.clone().acquire_owned() => match p {
                            Ok(p) => Some(p),
                            Err(_) => return None, // semaphore closed = halt
                        },
                    }
                }
                None => None,
            };
            let attempted = self.attempt_trigger(event, fold_cache, &labels).await;
            drop(permit);
            let this_effects: Vec<String> = labels
                .into_inner()
                .into_iter()
                .filter(|(kind, _)| *kind == "effect")
                .map(|(_, label)| label)
                .collect();

            match &first_attempt_effects {
                None => {
                    first_attempt_effects =
                        Some(this_effects.iter().cloned().collect());
                }
                Some(first) => {
                    let this_set: std::collections::HashSet<&str> =
                        this_effects.iter().map(String::as_str).collect();
                    let first_set: std::collections::HashSet<&str> =
                        first.iter().map(String::as_str).collect();
                    if first_set != this_set {
                        tracing::error!(
                            consumer = self.consumer_id,
                            event_id = %event.event_id,
                            first    = ?first,
                            retry    = ?this_effects,
                            "reactor effect labels diverged across retries — \
                             nondeterminism inside react() is likely",
                        );
                    }
                }
            }

            used_effects.extend(this_effects);
            let failure = match attempted {
                Ok(AttemptOutcome::Done) => {
                    return Some(TriggerDone {
                        parked: false,
                        effect_labels: used_effects.into_iter().collect(),
                    });
                }
                Ok(AttemptOutcome::BodyFailed { error, attempts }) => Some((error, attempts)),
                Err(e) => {
                    tracing::warn!(
                        consumer = self.consumer_id,
                        event_id = %event.event_id,
                        error = %format!("{e:#}"),
                        "reactor runner infrastructure error; retrying in partition",
                    );
                    None // infra: backoff + retry, never park
                }
            };

            if let Some((error, attempts)) = failure {
                let class = classify(&error);
                let park = match class {
                    Some(ErrorClass::Poison) => Some(FailureClass::Poison),
                    Some(ErrorClass::Transient) => {
                        let since = *transient_since
                            .get_or_insert_with(tokio::time::Instant::now);
                        if since.elapsed() >= TRANSIENT_CEILING {
                            Some(FailureClass::TransientExhausted)
                        } else {
                            None
                        }
                    }
                    Some(ErrorClass::Domain) if attempts >= self.retry_policy.max_attempts => {
                        Some(FailureClass::Domain)
                    }
                    None if attempts >= self.retry_policy.max_attempts => {
                        Some(FailureClass::Unclassified)
                    }
                    _ => None,
                };
                if let Some(failure_class) = park {
                    // Park is itself made of log/checkpoint I/O; its
                    // errors are infra errors — retry the park, don't
                    // lose the trigger.
                    match self
                        .park_terminal_failure(
                            event,
                            attempts,
                            format!("{:#}", error),
                            failure_class,
                            self.clock.now(),
                        )
                        .await
                    {
                        Ok(()) => {
                            return Some(TriggerDone {
                                parked: true,
                                effect_labels: used_effects.into_iter().collect(),
                            });
                        }
                        Err(e) => {
                            tracing::warn!(
                                consumer = self.consumer_id,
                                event_id = %event.event_id,
                                error = %format!("{e:#}"),
                                "terminal-failure park hit an infrastructure error; retrying",
                            );
                        }
                    }
                } else {
                    tracing::warn!(
                        consumer = self.consumer_id,
                        event_id = %event.event_id,
                        attempts,
                        class = ?class,
                        error = %format!("{error:#}"),
                        "reactor attempt failed; retrying in partition",
                    );
                }
            }

            local_attempt += 1;
            tokio::select! {
                _ = self.stop_notify.notified() => return None,
                _ = tokio::time::sleep(self.retry_policy.backoff_for(local_attempt)) => {}
            }
        }
    }

    /// One attempt at one trigger — the reaction body from the serial
    /// runner, minus cursor movement (the ack-floor owns checkpoints).
    async fn attempt_trigger(
        self: &Arc<Self>,
        event: &RecordedEvent,
        fold_cache: &FoldOnReadCache,
        labels: &LabelSet,
    ) -> Result<AttemptOutcome> {
        // Record the attempt FIRST (before deserialization), so a
        // poison trigger engages the terminal-failure budget instead of
        // wedging before the counter ever increments.
        let attempt_seq = self.checkpoint
            .record_reactor_attempt(&self.consumer_id, event.event_id)
            .await?;
        let started_at = self.clock.now();

        // Deserialize the trigger. A failure here is structurally
        // poison — deterministic, so retrying never helps. Classified
        // as such; the policy loop parks it immediately.
        let trigger: R::Trigger = match serde_json::from_value(event.payload.clone()) {
            Ok(t) => t,
            Err(deser_err) => {
                let msg = format!(
                    "trigger deserialization failed for {} (event {}): {deser_err}",
                    event.event_type, event.event_id,
                );
                return Ok(AttemptOutcome::BodyFailed {
                    error: poison(anyhow::anyhow!(msg)),
                    attempts: attempt_seq,
                });
            }
        };

        if let Some(obs) = self.observer.as_ref() {
            obs.reactor_started(
                event.event_id,
                &self.consumer_id,
                event.workflow_id,
                attempt_seq,
                started_at,
            );
            if let Some(descr) = self.reactor.describe(&trigger) {
                obs.reactor_description(
                    event.workflow_id,
                    event.position,
                    event.event_id,
                    &self.consumer_id,
                    descr,
                );
            }
        }

        // Per-attempt log sink — react body pushes via `ctx.log(...)`.
        let log_sink: parking_lot::Mutex<Vec<crate::types::LogEntry>> =
            parking_lot::Mutex::new(Vec::new());
        let state = match self.fold_registry.as_ref() {
            Some(reg) => StateSource::FoldOnRead {
                registry: reg,
                log: self.log.as_ref(),
                bound: event.position,
                cache: fold_cache,
            },
            None => StateSource::None,
        };
        let ctx = Ctx {
            event_id:       event.event_id,
            log_position:   event.position,
            occurred_at:    trigger.occurred_at().unwrap_or(event.created_at),
            workflow_id: event.workflow_id,
            metadata:       &event.metadata,
            consumer:       &self.consumer_id,
            labels:         Some(labels),
            state,
            logs:           Some(&log_sink),
            effect_store: self.effect_store.as_ref(),
            cancelled_workflows: self.cancelled_workflows.as_ref(),
        };

        // ── Decision. A panic is caught and treated as an attempt
        //    failure (worker-local retry), mirroring the serial
        //    supervisor's catch_unwind.
        let reacted = AssertUnwindSafe(self.reactor.react(&trigger, ctx))
            .catch_unwind()
            .await
            .unwrap_or_else(|panic| {
                Err(anyhow::anyhow!(
                    "reactor panicked: {}",
                    crate::engine::panic_payload_message(&panic),
                ))
            });

        let emitted = match reacted {
            Ok(events) => {
                let completed_at = self.clock.now();
                if let Some(obs) = self.observer.as_ref() {
                    let drained = log_sink.into_inner();
                    obs.reactor_completed(
                        event.event_id,
                        &self.consumer_id,
                        event.workflow_id,
                        attempt_seq,
                        started_at,
                        completed_at,
                        &drained,
                    );
                }
                // Clear persisted attempt count on success so a
                // future failure starts fresh.
                self.checkpoint
                    .clear_reactor_attempts(&self.consumer_id, event.event_id)
                    .await?;
                events
            }
            Err(e) => {
                let completed_at = self.clock.now();
                if let Some(obs) = self.observer.as_ref() {
                    let drained = log_sink.into_inner();
                    obs.reactor_failed(
                        event.event_id,
                        &self.consumer_id,
                        event.workflow_id,
                        attempt_seq,
                        started_at,
                        completed_at,
                        &format!("{:#}", e),
                        &drained,
                    );
                }
                return Ok(AttemptOutcome::BodyFailed { error: e, attempts: attempt_seq });
            }
        };

        // ── OCC fence (pre-flight). A reactor appends its outputs with
        //    `StreamState::Any`, so it cannot uphold the OCC invariant
        //    of an INVARIANT aggregate's stream. A deterministic config
        //    error — structurally poison: park rather than append.
        if !self.occ_categories.is_empty() {
            if let Some(bad) = emitted.iter().find(|out| {
                self.occ_categories.contains(out.durable_name.as_str())
            }) {
                let cat = bad.durable_name.as_str().to_string();
                let msg = format!(
                    "reactor '{}' emitted a fact in OCC-required category '{cat}' — \
                     reactor outputs append with StreamState::Any and cannot uphold \
                     the aggregate's optimistic-concurrency fence; model this as an \
                     Engine::append command, not a reactor output",
                    self.consumer_id,
                );
                return Ok(AttemptOutcome::BodyFailed {
                    error: poison(anyhow::anyhow!(msg)),
                    attempts: attempt_seq,
                });
            }
        }

        // ── Append each output directly to its own stream with a
        //    deterministic identity-keyed event_id (idempotent under
        //    redelivery via the log's append-dedup, C1).
        let mut nth: HashMap<(&str, Uuid), u32> = HashMap::new();
        for out in emitted.iter() {
            let n = nth
                .entry((out.durable_name.as_str(), out.subject_id))
                .or_insert(0);
            let this_nth = *n;
            *n += 1;
            // Which workflow this output belongs to: a workflow-ROOT
            // fact roots the one its own payload field names (the 0.8
            // always-inherit hardcode is deleted, not escaped); a chain
            // member inherits the trigger's. Causation links either
            // way — it's how settle_tree discovers the boundary.
            let out_workflow = out.workflow.unwrap_or(event.workflow_id);
            let out_event = EventData {
                event_id: derive_output_event_id(
                    &self.consumer_id,
                    event.event_id,
                    &out.durable_name,
                    out.subject_id,
                    this_nth,
                ),
                causation_id: Some(event.event_id),
                workflow_id: out_workflow,
                event_type: out.durable_name.clone(),
                payload: out.payload.clone(),
                created_at: self.clock.now(),
                // Placement uses the STREAM category; routing stays on
                // `event_type` (durable_name). Equal unless overridden.
                category: Some(out.subject.clone()),
                subject_id: Some(out.subject_id),
                metadata: reactor_output_metadata(&self.consumer_id),
                ephemeral: None,
                persistent: true,
            };
            let write = self.log
                .append_to_stream(
                    &out.subject, out.subject_id, StreamState::Any, vec![out_event],
                )
                .await?;
            // Advance the OUTPUT's workflow high-water. For a chain
            // member that's the trigger's run; for a workflow root it
            // seeds the CHILD's — the parent's settle does not wait for
            // the child's chain (that's the point of rooting), only for
            // this append, which the trigger's ack already covers.
            if let Some(tracker) = &self.settle_tracker {
                tracker.lock().unwrap().bump(out_workflow, write.position);
            }
            // Fold the output into the shared engine registry (folds
            // are idempotent on stream coordinates, so a deduped
            // re-append skips instead of double-counting).
            if let Some(reg) = &self.engine_aggregators {
                let outcome = crate::aggregator::fold_event(
                    reg.as_ref(),
                    self.snapshot_store.as_deref(),
                    self.log.as_ref(),
                    &out.durable_name,
                    &out.payload,
                    out.subject_id,
                    &out.subject,
                    write.revision,
                    write.position,
                    /* strict_to_event = */ false,
                )
                .await?;
                if outcome.applied {
                    if let Some(store) = self.snapshot_store.as_ref() {
                        crate::aggregator::maybe_save_snapshots(
                            reg.as_ref(),
                            store.as_ref(),
                            self.snapshot_every,
                            &outcome.snapshots,
                        )
                        .await;
                    }
                }
            }
        }
        Ok(AttemptOutcome::Done)
    }

    /// Terminal-failure routing — **mandatory** (BLOCKING-2 / Primitive
    /// 5): with a mapper, its synthesized fact (if any) appends to that
    /// fact's own stream; without one, the built-in
    /// [`REACTION_FAILED_KIND`] fact appends to the **trigger's own
    /// subject history**, so a completion fold over that subject can
    /// fold the failure as completion-with-error. Folds into the engine
    /// registry, then clears the attempt counter **last** (so a failed
    /// park can't reset the budget). The caller acks the trigger
    /// afterwards — the ack-floor, not this function, moves the
    /// durable cursor.
    async fn park_terminal_failure(
        self: &Arc<Self>,
        event: &RecordedEvent,
        attempts: u32,
        error: String,
        class: FailureClass,
        completed_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<()> {
        if let Some(obs) = self.observer.as_ref() {
            obs.reactor_terminal_failure(
                event.event_id,
                &self.consumer_id,
                event.workflow_id,
                attempts,
                &error,
                completed_at,
            );
        }
        // (kind, subject placement, subject_id, workflow, payload) of
        // the terminal fact to append — mapper's fact, the built-in,
        // or nothing (mapper returned None: park silently). A mapper
        // fact that declares a workflow root keeps that declaration
        // (the named-mailbox pattern: a per-run HandlerFailed joins
        // its run's workflow); the built-in inherits the trigger's.
        let terminal: Option<(String, String, Uuid, Uuid, serde_json::Value)> =
            match self.failure_mapper.as_ref() {
                Some(mapper) => {
                    let info = TerminalFailure {
                        consumer:        self.consumer_id.clone(),
                        trigger_id:   event.event_id,
                        trigger_event_type: event.event_type.clone(),
                        subject:         event.category.clone(),
                        subject_id:      event.subject_id,
                        class,
                        error,
                        attempts,
                        workflow_id:    event.workflow_id,
                    };
                    match mapper(info) {
                        Some(fact) => Some((
                            fact.name().to_string(),
                            fact.subject().to_string(),
                            fact.subject_id(),
                            fact.declared_workflow_id().unwrap_or(event.workflow_id),
                            fact.to_value()?,
                        )),
                        None => None,
                    }
                }
                None => Some((
                    REACTION_FAILED_KIND.to_string(),
                    // The trigger's own subject history — where its
                    // completion fold reads.
                    event.category.clone(),
                    event.subject_id,
                    event.workflow_id,
                    serde_json::json!({
                        "consumer": self.consumer_id,
                        "trigger_id": event.event_id,
                        "trigger_event_type": event.event_type,
                        "class": class.as_str(),
                        "error": error,
                        "attempts": attempts,
                    }),
                )),
            };

        // `nth = u32::MAX` keeps the terminal fact's deterministic id
        // distinct from react() outputs of the same identity.
        if let Some((event_type, cat, sid, wf, payload)) = terminal {
            let out_event = EventData {
                event_id: derive_output_event_id(
                    &self.consumer_id, event.event_id, &event_type, sid, u32::MAX,
                ),
                causation_id: Some(event.event_id),
                workflow_id: wf,
                event_type: event_type.clone(),
                payload: payload.clone(),
                created_at: self.clock.now(),
                category: Some(cat.clone()),
                subject_id: Some(sid),
                metadata: reactor_output_metadata(&self.consumer_id),
                ephemeral: None,
                persistent: true,
            };
            let write = self
                .log
                .append_to_stream(&cat, sid, StreamState::Any, vec![out_event])
                .await?;
            if let Some(tracker) = &self.settle_tracker {
                tracker.lock().unwrap().bump(wf, write.position);
            }
            if let Some(reg) = &self.engine_aggregators {
                crate::aggregator::fold_event(
                    reg.as_ref(),
                    self.snapshot_store.as_deref(),
                    self.log.as_ref(),
                    &event_type,
                    &payload,
                    sid,
                    &cat,
                    write.revision,
                    write.position,
                    /* strict_to_event = */ false,
                )
                .await?;
            }
        }
        self.checkpoint
            .clear_reactor_attempts(&self.consumer_id, event.event_id)
            .await?;
        Ok(())
    }

    /// Drain the completion channel, mark acks, advance + persist the
    /// floor. Returns the number of completions reaped.
    async fn reap(&self) -> Result<usize> {
        let mut acks = Vec::new();
        {
            let mut rx = self.completions_rx.lock();
            while let Ok(c) = rx.try_recv() {
                acks.push(c);
            }
        }
        let count = acks.len();
        if !acks.is_empty() {
            let mut d = self.dispatch.lock();
            for c in acks.drain(..) {
                if let Some(p) = d.pending.get_mut(&c.position) {
                    p.acked = true;
                    p.parked = c.parked;
                    p.effect_labels = c.effect_labels;
                }
                if let Some(n) = d.wf_pending.get_mut(&c.workflow) {
                    *n -= 1;
                    if *n == 0 {
                        d.wf_pending.remove(&c.workflow);
                    }
                }
            }
        }
        self.advance_floor().await?;
        Ok(count)
    }

    /// Advance the ack-floor: pop the contiguous acked prefix of
    /// `pending`; with nothing outstanding the floor is the ingestion
    /// cursor itself. Persist when it moved (or a prior persist failed),
    /// then garbage-collect passed triggers' effect entries — they
    /// existed only to make redelivery deterministic, and the floor
    /// passing a trigger is what retires redelivery. Parked terminal
    /// failures keep theirs (failure replay restores from them).
    async fn advance_floor(&self) -> Result<()> {
        let mut gc: Vec<EffectKey> = Vec::new();
        let to_persist = {
            let mut d = self.dispatch.lock();
            if !d.started {
                return Ok(());
            }
            while let Some(entry) = d.pending.first_entry() {
                if !entry.get().acked {
                    break;
                }
                let (pos, done) = entry.remove_entry();
                if pos > d.floor {
                    d.floor = pos;
                }
                if !done.parked && self.effect_store.is_some() {
                    gc.extend(done.effect_labels.into_iter().map(|label| {
                        EffectKey::new(&self.consumer_id, done.event_id, label)
                    }));
                }
            }
            if d.pending.is_empty() && d.ingest_pos > d.floor {
                d.floor = d.ingest_pos;
            }
            if d.persisted_floor != Some(d.floor) {
                Some(d.floor)
            } else {
                None
            }
        };
        if let Some(pos) = to_persist {
            self.checkpoint.set(&self.consumer_id, pos).await?;
            let mut d = self.dispatch.lock();
            if d.persisted_floor.map_or(true, |p| p < pos) {
                d.persisted_floor = Some(pos);
            }
        }
        // Best-effort: a GC failure must never wedge the floor — a
        // missed delete is a leaked entry, not a correctness problem.
        if let Some(store) = self.effect_store.as_ref() {
            for key in gc {
                if let Err(e) = store.remove(&key).await {
                    tracing::warn!(
                        consumer = self.consumer_id,
                        error = %format!("{e:#}"),
                        "effect-store GC failed; entry leaked",
                    );
                }
            }
        }
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checkpoint_store::CheckpointStore;
    use crate::memory_store::MemoryStore;
    use crate::reactor::Events;
    use crate::types::EventData;
    use anyhow::anyhow;
    use async_trait::async_trait;
    use chrono::{DateTime, Utc};
    use serde::{Deserialize, Serialize};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    /// Poll until `cond` is true, panicking after `secs` seconds.
    async fn wait_until(secs: u64, what: &str, mut cond: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(secs);
        while !cond() {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for: {what}",
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    // ── Trigger fact ──
    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct OrderPlaced {
        order_id:    Uuid,
        occurred_at: DateTime<Utc>,
    }
    impl Event for OrderPlaced {
        const NAME: &'static str = "order_placed";
        fn subject_id(&self) -> Uuid { self.order_id }
        fn occurred_at(&self) -> Option<DateTime<Utc>> { Some(self.occurred_at) }
    }

    // ── Output fact ──
    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct ShippedNotification {
        order_id: Uuid,
    }
    impl Event for ShippedNotification {
        const NAME: &'static str = "shipped_notification";
        fn subject_id(&self) -> Uuid { self.order_id }
    }

    fn append_trigger(store: &MemoryStore, payload: &OrderPlaced) -> Uuid {
        let event_id = Uuid::new_v4();
        let ev = EventData {
            event_id,
            causation_id:       None,
            workflow_id:  Uuid::new_v4(),
            event_type:      <OrderPlaced as Event>::NAME.to_string(),
            payload:         serde_json::to_value(payload).unwrap(),
            created_at:      Utc::now(),
            // Stamp placement like Engine::emit does — fold-on-read
            // reads the subject history, so fixtures must put triggers
            // in their stream.
            category:  Some(<OrderPlaced as Event>::NAME.to_string()),
            subject_id:    Some(payload.order_id),
            metadata:        serde_json::Map::new(),
            ephemeral:       None,
            persistent:      true,
        };
        let store_ref: &MemoryStore = store;
        let fut = crate::append_event(store_ref, ev);
        // Call from sync context only in test.
        let _ = futures::executor::block_on(fut).unwrap();
        event_id
    }

    // ── Reactor that emits one ShippedNotification per trigger ──
    struct EmitOne;
    #[async_trait]
    impl Reactor for EmitOne {
        type Trigger = OrderPlaced;
        const NAME: &'static str = "emit-one";
        async fn react(
            &self,
            trigger: &OrderPlaced,
            _ctx: Ctx<'_>,
        ) -> Result<Events> {
            let mut out = Events::new();
            out.push(ShippedNotification { order_id: trigger.order_id });
            Ok(out)
        }
    }

    // ── Reactor that emits N outputs ──
    struct EmitN(usize);
    #[async_trait]
    impl Reactor for EmitN {
        type Trigger = OrderPlaced;
        const NAME: &'static str = "emit-n";
        async fn react(
            &self,
            trigger: &OrderPlaced,
            _ctx: Ctx<'_>,
        ) -> Result<Events> {
            let mut out = Events::new();
            for _ in 0..self.0 {
                out.push(ShippedNotification { order_id: trigger.order_id });
            }
            Ok(out)
        }
    }

    // ── Reactor that fails on the Nth call ──
    struct FailsOnNth { n: usize, calls: Arc<AtomicUsize> }
    #[async_trait]
    impl Reactor for FailsOnNth {
        type Trigger = OrderPlaced;
        const NAME: &'static str = "fails-on-nth";
        async fn react(
            &self,
            _trigger: &OrderPlaced,
            _ctx: Ctx<'_>,
        ) -> Result<Events> {
            let now = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if now == self.n {
                Err(anyhow!("simulated reactor failure on call {}", now))
            } else {
                let mut out = Events::new();
                out.push(ShippedNotification { order_id: Uuid::nil() });
                Ok(out)
            }
        }
    }

    #[tokio::test]
    async fn derive_event_id_is_deterministic() {
        let trigger = Uuid::new_v4();
        let sid = Uuid::new_v4();
        let a = derive_output_event_id("reactor.a", trigger, "shipped", sid, 0);
        let b = derive_output_event_id("reactor.a", trigger, "shipped", sid, 0);
        assert_eq!(a, b, "same identity MUST produce same event_id");

        let c = derive_output_event_id("reactor.a", trigger, "shipped", sid, 1);
        assert_ne!(a, c, "different nth (same kind+subject) MUST differ");

        let d = derive_output_event_id("reactor.b", trigger, "shipped", sid, 0);
        assert_ne!(a, d, "different consumer MUST produce different ids");

        let e = derive_output_event_id("reactor.a", trigger, "billed", sid, 0);
        assert_ne!(a, e, "different KIND must produce different ids — reordering
                          or inserting other kinds never re-keys this output");

        let f = derive_output_event_id("reactor.a", trigger, "shipped", Uuid::new_v4(), 0);
        assert_ne!(a, f, "different SUBJECT must produce different ids");
    }

    #[tokio::test]
    async fn reactor_appends_output_directly_and_advances_floor() {
        let store = Arc::new(MemoryStore::new());
        let trigger = OrderPlaced { order_id: Uuid::new_v4(), occurred_at: Utc::now() };
        let order_id = trigger.order_id;
        let trigger_event_id = append_trigger(&store, &trigger);

        let runner = ReactorRunner::new(
            EmitOne,
            "r.shipper",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        );

        runner.quiesce().await.unwrap();

        // Floor advanced (ack-floor checkpoint persisted).
        assert!(store.get("r.shipper").await.unwrap().is_some());

        // Output appended DIRECTLY to its own stream in the log (no outbox).
        let all = EventLogBackend::read_all(store.as_ref(), LogCursor::ZERO, 10)
            .await
            .unwrap();
        let out = all
            .iter()
            .find(|e| e.event_type == "shipped_notification")
            .expect("reactor output appended to the log");
        assert_eq!(out.category, "shipped_notification"); // SUBJECT defaults to NAME
        assert_eq!(out.causation_id, Some(trigger_event_id));
        // Deterministic id matches the helper's output (idempotent on redelivery).
        assert_eq!(
            out.event_id,
            derive_output_event_id("r.shipper", trigger_event_id,
                                   "shipped_notification", order_id, 0),
        );
        runner.halt();
    }

    #[tokio::test]
    async fn reactor_output_propagates_trigger_workflow_id() {
        // A fact emitted in response to a trigger carries the trigger's
        // workflow_id so cross-system tracing chains through the reactor.
        let store = Arc::new(MemoryStore::new());
        let trigger = OrderPlaced { order_id: Uuid::new_v4(), occurred_at: Utc::now() };
        let trigger_event_id = append_trigger(&store, &trigger);

        let persisted = EventLogBackend::read_all(
            store.as_ref(), LogCursor::ZERO, 10,
        ).await.unwrap();
        let trigger_correlation = persisted[0].workflow_id;

        let runner = ReactorRunner::new(
            EmitOne,
            "r.trace",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        );
        runner.quiesce().await.unwrap();

        let all_events = EventLogBackend::read_all(
            store.as_ref(), LogCursor::ZERO, 10,
        ).await.unwrap();
        let output_event = all_events.iter()
            .find(|e| e.event_type == "shipped_notification")
            .expect("reactor output present in log");
        assert_eq!(output_event.workflow_id, trigger_correlation,
                   "output MUST carry trigger's workflow_id");
        assert_eq!(output_event.causation_id, Some(trigger_event_id),
                   "output's causation_id MUST be the trigger's event_id");
        runner.halt();
    }

    #[tokio::test]
    async fn empty_react_advances_floor_only() {
        struct Silent;
        #[async_trait]
        impl Reactor for Silent {
            type Trigger = OrderPlaced;
            const NAME: &'static str = "silent";
            async fn react(
                &self,
                _trigger: &OrderPlaced,
                _ctx: Ctx<'_>,
            ) -> Result<Events> {
                Ok(Events::new())
            }
        }

        let store = Arc::new(MemoryStore::new());
        let trigger = OrderPlaced { order_id: Uuid::new_v4(), occurred_at: Utc::now() };
        append_trigger(&store, &trigger);

        let runner = ReactorRunner::new(
            Silent,
            "r.silent",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        );

        runner.quiesce().await.unwrap();
        assert!(store.get("r.silent").await.unwrap().is_some());
        assert_eq!(
            EventLogBackend::read_all(store.as_ref(), LogCursor::ZERO, 10).await.unwrap().len(),
            1,
            "only the trigger in the log — no reactor output",
        );
        runner.halt();
    }

    #[tokio::test]
    async fn n_outputs_append_n_with_identity_keyed_ids() {
        let store = Arc::new(MemoryStore::new());
        let trigger = OrderPlaced { order_id: Uuid::new_v4(), occurred_at: Utc::now() };
        let order_id = trigger.order_id;
        let trigger_event_id = append_trigger(&store, &trigger);

        let runner = ReactorRunner::new(
            EmitN(5),
            "r.fanout",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        );

        runner.quiesce().await.unwrap();

        let all = EventLogBackend::read_all(store.as_ref(), LogCursor::ZERO, 20)
            .await
            .unwrap();
        let outs: Vec<_> = all
            .iter()
            .filter(|e| e.event_type == "shipped_notification")
            .collect();
        assert_eq!(outs.len(), 5, "all 5 outputs appended to the log");

        for i in 0..5u32 {
            let want = derive_output_event_id("r.fanout", trigger_event_id,
                                              "shipped_notification", order_id, i);
            assert!(
                outs.iter().any(|e| e.event_id == want),
                "output nth {i} present in log",
            );
        }
        runner.halt();
    }

    #[tokio::test]
    async fn failed_attempt_retries_in_worker_without_skipping() {
        // Worker-local retry: a transient failure on the 2nd call
        // retries the SAME trigger in its partition until it succeeds —
        // the floor never skips it, and every trigger's output lands.
        let store = Arc::new(MemoryStore::new());
        let subject = Uuid::new_v4(); // same subject → one partition, strict order
        for _ in 0..3 {
            let t = OrderPlaced { order_id: subject, occurred_at: Utc::now() };
            append_trigger(&store, &t);
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let runner = ReactorRunner::new(
            FailsOnNth { n: 2, calls: calls.clone() },
            "r.flaky",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        );

        runner.quiesce().await.unwrap();

        let appended = EventLogBackend::read_all(store.as_ref(), LogCursor::ZERO, 20)
            .await
            .unwrap();
        let outs = appended
            .iter()
            .filter(|e| e.event_type == "shipped_notification")
            .count();
        assert_eq!(outs, 3, "all three triggers produced outputs (2nd recovered)");
        assert_eq!(calls.load(Ordering::SeqCst), 4, "exactly one retry happened");

        // Floor reached the end of the log.
        let cursor = store.get("r.flaky").await.unwrap().unwrap();
        let last = appended.last().unwrap().position;
        assert_eq!(cursor, last, "ack-floor caught up after recovery");
        runner.halt();
    }

    #[tokio::test]
    async fn floor_holds_below_failing_trigger_while_other_partitions_proceed() {
        // BLOCKING-3's honesty during an outage: a transient-classified
        // failure backs off under the liveness ceiling, pinning the
        // durable floor BELOW its position — restart redelivers it —
        // while other subjects keep flowing. (Transient is the
        // sanctioned wait-it-out class; domain/unclassified would park
        // after bounded attempts instead.)
        struct FailsForSubject { bad: Uuid, calls: Arc<AtomicUsize> }
        #[async_trait]
        impl Reactor for FailsForSubject {
            type Trigger = OrderPlaced;
            const NAME: &'static str = "per-subject-fail";
            async fn react(&self, t: &OrderPlaced, _: Ctx<'_>) -> Result<Events> {
                if t.order_id == self.bad {
                    self.calls.fetch_add(1, Ordering::SeqCst);
                    return Err(crate::failure::transient(anyhow!(
                        "graph store connection refused"
                    )));
                }
                let mut out = Events::new();
                out.push(ShippedNotification { order_id: t.order_id });
                Ok(out)
            }
        }

        let store = Arc::new(MemoryStore::new());
        let bad = Uuid::new_v4();
        let good = Uuid::new_v4();
        append_trigger(&store, &OrderPlaced { order_id: bad, occurred_at: Utc::now() });
        append_trigger(&store, &OrderPlaced { order_id: good, occurred_at: Utc::now() });

        let calls = Arc::new(AtomicUsize::new(0));
        let runner = ReactorRunner::new(
            FailsForSubject { bad, calls: calls.clone() },
            "r.isolate",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        );

        // Step until the good subject's output lands (can't quiesce — the
        // bad partition never drains).
        let store_c = store.clone();
        let saw_output = move || {
            futures::executor::block_on(async {
                EventLogBackend::read_all(store_c.as_ref(), LogCursor::ZERO, 20)
                    .await.unwrap()
                    .iter()
                    .any(|e| e.event_type == "shipped_notification")
            })
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            runner.step(64).await.unwrap();
            if saw_output() { break; }
            assert!(std::time::Instant::now() < deadline,
                    "good subject's output never landed");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        // The failing trigger is position 1 (first in the log); the
        // floor must NOT have passed it.
        let all = EventLogBackend::read_all(store.as_ref(), LogCursor::ZERO, 20)
            .await.unwrap();
        let bad_pos = all.iter()
            .find(|e| e.subject_id == bad && e.event_type == "order_placed")
            .unwrap().position;
        runner.step(64).await.unwrap(); // one more reap/persist pass
        let cursor = store.get("r.isolate").await.unwrap().unwrap_or(LogCursor::ZERO);
        assert!(cursor < bad_pos,
                "floor {cursor} must hold below the unacked trigger at {bad_pos}");
        assert!(calls.load(Ordering::SeqCst) >= 1, "bad subject was attempted");
        runner.halt();
    }

    #[tokio::test]
    async fn step_idle_when_log_caught_up() {
        let store = Arc::new(MemoryStore::new());
        let runner = ReactorRunner::new(
            EmitOne,
            "r.empty",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        );
        let outcome = runner.step(10).await.unwrap();
        assert_eq!(outcome, StepOutcome::Idle);
    }

    #[tokio::test]
    async fn reactor_skips_non_matching_trigger_type() {
        let store = Arc::new(MemoryStore::new());
        let foreign = EventData {
            event_id:        Uuid::new_v4(),
            causation_id:       None,
            workflow_id:  Uuid::new_v4(),
            event_type:      "other.thing".into(),
            payload:         serde_json::json!({}),
            created_at:      Utc::now(),
            category:  None,
            subject_id:    None,
            metadata:        serde_json::Map::new(),
            ephemeral:       None,
            persistent:      true,
        };
        crate::append_event(store.as_ref(), foreign).await.unwrap();

        let runner = ReactorRunner::new(
            EmitOne,
            "r.skip",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        );

        runner.quiesce().await.unwrap();
        assert_eq!(
            EventLogBackend::read_all(store.as_ref(), LogCursor::ZERO, 10).await.unwrap().len(),
            1,
            "only the foreign event in the log — no reactor output",
        );
        assert!(store.get("r.skip").await.unwrap().is_some(),
                "floor advanced past the non-matching event");
        runner.halt();
    }

    // ── terminal failure — mapper after retry exhaustion ──

    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct HandlerFailed {
        consumer: String,
        attempts:   u32,
    }
    impl Event for HandlerFailed {
        const NAME: &'static str = "handler_failed";
        fn subject_id(&self) -> Uuid { Uuid::nil() }
    }

    struct AlwaysFails(std::sync::Arc<AtomicUsize>);
    #[async_trait]
    impl Reactor for AlwaysFails {
        type Trigger = OrderPlaced;
        const NAME: &'static str = "always-fails";
        async fn react(&self, _t: &OrderPlaced, _: Ctx<'_>) -> Result<Events> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(anyhow!("boom"))
        }
    }

    #[tokio::test]
    async fn terminal_failure_attempt_count_persists_across_runner_instances() {
        // Attempts live in the ReactorCheckpoint (not in-memory) so a
        // process crash mid-retry can't reset the budget. Runner A
        // accumulates failures; runner B (fresh memory, same store)
        // continues the count instead of starting from zero.
        let store = Arc::new(MemoryStore::new());
        let payload = OrderPlaced {
            order_id:    Uuid::new_v4(),
            occurred_at: Utc::now(),
        };
        let _ = append_trigger(&store, &payload);

        let mapper_calls = std::sync::Arc::new(AtomicUsize::new(0));
        let seen_attempts = std::sync::Arc::new(AtomicUsize::new(0));
        let mk_mapper = || -> TerminalFailureMapper {
            let mapper_calls = mapper_calls.clone();
            let seen_attempts = seen_attempts.clone();
            std::sync::Arc::new(move |info: TerminalFailure| {
                mapper_calls.fetch_add(1, Ordering::SeqCst);
                seen_attempts.store(info.attempts as usize, Ordering::SeqCst);
                Some(Box::new(HandlerFailed {
                    consumer: info.consumer,
                    attempts:   info.attempts,
                }) as Box<dyn crate::engine::ErasedFact>)
            })
        };

        // Runner A: budget too high to park — accumulate 2+ attempts,
        // then halt mid-retry (the "crash").
        let calls_a = std::sync::Arc::new(AtomicUsize::new(0));
        let runner_a = ReactorRunner::new(
            AlwaysFails(calls_a.clone()),
            "persist-attempts",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        ).with_terminal_failure(mk_mapper()).with_max_attempts(100);
        runner_a.step(10).await.unwrap();
        wait_until(5, "runner A to fail twice", || {
            calls_a.load(Ordering::SeqCst) >= 2
        }).await;
        runner_a.halt();
        assert_eq!(mapper_calls.load(Ordering::SeqCst), 0, "no park under budget 100");

        // Runner B: same store, budget 3. Its FIRST react failure sees
        // a persisted attempt count >= 3 → parks immediately.
        let calls_b = std::sync::Arc::new(AtomicUsize::new(0));
        let runner_b = ReactorRunner::new(
            AlwaysFails(calls_b.clone()),
            "persist-attempts",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        ).with_terminal_failure(mk_mapper()).with_max_attempts(3);
        runner_b.quiesce().await.unwrap();
        assert_eq!(mapper_calls.load(Ordering::SeqCst), 1,
                   "attempt count persisted: runner B parks without re-earning 3 failures");
        assert!(seen_attempts.load(Ordering::SeqCst) >= 3,
                "mapper saw the cross-instance attempt total");
        assert!(calls_b.load(Ordering::SeqCst) <= 2,
                "runner B did not restart the retry budget from zero");
        runner_b.halt();
    }

    #[tokio::test]
    async fn prefix_colliding_category_does_not_reach_the_reactor() {
        // B1 regression: routing is exact-kind equality. A reactor on
        // "order_placed" must not match "orders:created" (or any other
        // sharing-a-prefix kind) — the foreign payload would hit the
        // trigger deserializer.
        let store = Arc::new(MemoryStore::new());
        let foreign = EventData {
            event_id:       Uuid::new_v4(),
            causation_id:   None,
            workflow_id: Uuid::new_v4(),
            event_type:     "orders:created".into(),
            payload:        serde_json::json!({ "name": "not an OrderPlaced" }),
            created_at:     Utc::now(),
            category:       Some("orders".into()),
            subject_id:      Some(Uuid::new_v4()),
            metadata:       serde_json::Map::new(),
            ephemeral:      None,
            persistent:     true,
        };
        crate::append_event(store.as_ref(), foreign).await.unwrap();

        struct PanicsIfCalled;
        #[async_trait]
        impl Reactor for PanicsIfCalled {
            type Trigger = OrderPlaced;
            const NAME: &'static str = "no-collision";
            async fn react(&self, _t: &OrderPlaced, _: Ctx<'_>) -> Result<Events> {
                panic!("reactor must never see a foreign-kind event");
            }
        }

        let runner = ReactorRunner::new(
            PanicsIfCalled,
            "r.no-collision",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        );

        runner.quiesce().await.unwrap();
        assert!(store.get("r.no-collision").await.unwrap().is_some(),
                "foreign kind skipped, floor advanced, no deser, no wedge");
        runner.halt();
    }

    #[tokio::test]
    async fn poison_trigger_parks_as_terminal_failure_and_does_not_wedge() {
        // B4: a payload that can't deserialize into the reactor's
        // Trigger is a poison pill — deterministic, so retrying never
        // helps. With a mapper it parks immediately and the floor
        // advances.
        let store = Arc::new(MemoryStore::new());

        let poison = EventData {
            event_id:       Uuid::new_v4(),
            causation_id:   None,
            workflow_id: Uuid::new_v4(),
            event_type:     "order_placed".into(),
            payload:        serde_json::json!({ "not": "an order" }),
            created_at:     Utc::now(),
            category:       Some("order".into()),
            subject_id:      Some(Uuid::new_v4()),
            metadata:       serde_json::Map::new(),
            ephemeral:      None,
            persistent:     true,
        };
        crate::append_event(store.as_ref(), poison).await.unwrap();

        let mapper_calls = std::sync::Arc::new(AtomicUsize::new(0));
        let mapper_calls_c = mapper_calls.clone();
        let mapper: TerminalFailureMapper = std::sync::Arc::new(move |info: TerminalFailure| {
            mapper_calls_c.fetch_add(1, Ordering::SeqCst);
            assert!(info.error.contains("deserialization failed"),
                    "TerminalFailure carries the deser error: {}", info.error);
            None // no synthesized fact needed for this test
        });

        struct NeverReacts;
        #[async_trait]
        impl Reactor for NeverReacts {
            type Trigger = OrderPlaced;
            const NAME: &'static str = "poison.park";
            async fn react(&self, _t: &OrderPlaced, _: Ctx<'_>) -> Result<Events> {
                panic!("react must never run on a poison trigger");
            }
        }

        let runner = ReactorRunner::new(
            NeverReacts,
            "poison.park",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        )
        .with_terminal_failure(mapper).with_max_attempts(3);

        runner.quiesce().await.unwrap();
        assert_eq!(mapper_calls.load(Ordering::SeqCst), 1,
                   "poison parked as a terminal failure immediately — no retries (deterministic)");
        assert!(store.get("poison.park").await.unwrap().is_some(),
                "floor advanced past the poison event — not wedged");
        runner.halt();
    }

    #[tokio::test]
    async fn poison_without_mapper_parks_immediately_with_builtin_fact() {
        // The taxonomy's poison policy is mapper-independent: a payload
        // that can't deserialize is deterministic, so it parks on the
        // FIRST attempt — via the built-in causal:reaction_failed fact
        // on the trigger's own subject history — and the floor moves
        // on. (Pre-taxonomy, no-mapper poison wedged its partition
        // forever; the mandatory terminal path replaced that.)
        let store = Arc::new(MemoryStore::new());
        let poison_subject = Uuid::new_v4();
        let poison = EventData {
            event_id:       Uuid::new_v4(),
            causation_id:   None,
            workflow_id: Uuid::new_v4(),
            event_type:     "order_placed".into(),
            payload:        serde_json::json!({ "not": "an order" }),
            created_at:     Utc::now(),
            category:       Some("order_placed".into()),
            subject_id:      Some(poison_subject),
            metadata:       serde_json::Map::new(),
            ephemeral:      None,
            persistent:     true,
        };
        crate::append_event(store.as_ref(), poison).await.unwrap();
        // A healthy trigger on a different subject, after the poison.
        append_trigger(&store, &OrderPlaced { order_id: Uuid::new_v4(), occurred_at: Utc::now() });

        let runner = ReactorRunner::new(
            EmitOne,
            "poison.park.builtin",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        );

        runner.quiesce().await.unwrap();

        let all = EventLogBackend::read_all(store.as_ref(), LogCursor::ZERO, 20)
            .await.unwrap();
        assert_eq!(
            all.iter().filter(|e| e.event_type == "shipped_notification").count(),
            1,
            "the healthy subject processed normally",
        );
        let parked = all.iter()
            .find(|e| e.event_type == REACTION_FAILED_KIND)
            .expect("built-in terminal fact for the poison");
        assert_eq!(parked.payload["class"], "poison");
        assert_eq!(parked.payload["attempts"], 1,
                   "poison parks on the FIRST attempt — deterministic, no retries");
        assert_eq!(parked.subject_id, poison_subject,
                   "terminal fact lands in the poison trigger's subject history");
        // Floor passed the poison: it is parked work, not pending work.
        let cursor = store.get("poison.park.builtin").await.unwrap().unwrap();
        let last = all.last().unwrap().position;
        assert_eq!(cursor, last, "floor caught up past the parked poison");
        runner.halt();
    }

    #[tokio::test]
    async fn fold_on_read_state_is_bounded_at_trigger() {
        // ctx.state_of in a reactor folds the subject history bounded
        // at THE TRIGGER's position — even when later events for the
        // same subject are already in the log when the worker runs.
        #[derive(Default, Clone, Debug, Serialize, Deserialize)]
        struct OrderCount { n: u32 }
        impl crate::aggregate::Aggregate for OrderCount {
            const NAME: &'static str = "OrderCount";
        }
        impl crate::aggregate::Apply<OrderPlaced> for OrderCount {
            fn apply(&mut self, _: &OrderPlaced) { self.n += 1; }
        }

        #[derive(Clone)]
        struct CaptureCounts { seen: Arc<parking_lot::Mutex<Vec<(u32, u32)>>> }
        #[async_trait]
        impl Reactor for CaptureCounts {
            type Trigger = OrderPlaced;
            const NAME: &'static str = "capture-counts";
            async fn react(&self, t: &OrderPlaced, ctx: Ctx<'_>) -> Result<Events> {
                let s = ctx.state_of::<OrderCount>(t.order_id).await?;
                self.seen.lock().push((s.prev.n, s.curr.n));
                Ok(Events::new())
            }
        }

        let store = Arc::new(MemoryStore::new());
        let subject = Uuid::new_v4();
        // Both triggers are in the log BEFORE the runner takes a step —
        // an at-ingest fold would show the second event to the first.
        append_trigger(&store, &OrderPlaced { order_id: subject, occurred_at: Utc::now() });
        append_trigger(&store, &OrderPlaced { order_id: subject, occurred_at: Utc::now() });

        let mut reg = crate::aggregator::AggregatorRegistry::new();
        reg.register(crate::aggregator::Aggregator::for_type::<OrderCount, OrderPlaced>());

        let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let runner = ReactorRunner::new(
            CaptureCounts { seen: seen.clone() },
            "fold-bounded",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        )
        .with_aggregators(Arc::new(reg));

        runner.quiesce().await.unwrap();
        assert_eq!(*seen.lock(), vec![(0, 1), (1, 2)],
                   "each reaction sees the fold AT ITS OWN trigger, not the log head");
        runner.halt();
    }

    #[tokio::test]
    async fn reactor_invokes_failure_mapper_after_retry_exhaustion() {
        let store = Arc::new(MemoryStore::new());
        let payload = OrderPlaced {
            order_id:    Uuid::new_v4(),
            occurred_at: Utc::now(),
        };
        let trigger_id = append_trigger(&store, &payload);

        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let mapper_calls = std::sync::Arc::new(AtomicUsize::new(0));
        let mapper_calls_c = mapper_calls.clone();
        let seen_corr = std::sync::Arc::new(std::sync::Mutex::new(None::<Uuid>));
        let seen_corr_c = seen_corr.clone();

        let mapper: TerminalFailureMapper = std::sync::Arc::new(move |info: TerminalFailure| {
            mapper_calls_c.fetch_add(1, Ordering::SeqCst);
            *seen_corr_c.lock().unwrap() = Some(info.workflow_id);
            Some(Box::new(HandlerFailed {
                consumer: info.consumer,
                attempts:   info.attempts,
            }) as Box<dyn crate::engine::ErasedFact>)
        });

        let runner = ReactorRunner::new(
            AlwaysFails(calls.clone()),
            "always-fails",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        ).with_terminal_failure(mapper).with_max_attempts(3);

        // Worker retries internally; after 3 attempts the mapper fires,
        // the synthesized fact appends, and the floor advances.
        runner.quiesce().await.unwrap();
        assert_eq!(mapper_calls.load(Ordering::SeqCst), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 3);

        let all = EventLogBackend::read_all(store.as_ref(), LogCursor::ZERO, 10)
            .await
            .unwrap();
        let terminal_failure = all
            .iter()
            .find(|e| e.event_type == "handler_failed")
            .expect("terminal-failure-synthesized fact in log");
        assert_eq!(terminal_failure.causation_id, Some(trigger_id));
        assert_eq!(
            terminal_failure.event_id,
            derive_output_event_id("always-fails", trigger_id,
                                   "handler_failed", Uuid::nil(), u32::MAX),
            "terminal-failure output uses u32::MAX for its deterministic id",
        );

        // The terminal-failure fact inherits the trigger's workflow_id.
        let trigger = all
            .iter()
            .find(|e| e.event_id == trigger_id)
            .expect("trigger in log");
        assert_eq!(
            terminal_failure.workflow_id, trigger.workflow_id,
            "terminal-failure-synthesized fact MUST inherit trigger workflow_id",
        );
        let seen = seen_corr.lock().unwrap().expect("mapper ran");
        assert_eq!(
            seen, trigger.workflow_id,
            "TerminalFailure.workflow_id MUST be the failing trigger's run",
        );

        // Floor is past the failing trigger; no further reaction occurs.
        let next = runner.step(10).await.unwrap();
        assert!(
            matches!(next, StepOutcome::Idle | StepOutcome::Progressed { applied: 0 }),
            "no further reaction on the failed trigger; got {next:?}",
        );
        assert_eq!(calls.load(Ordering::SeqCst), 3, "no retry storm after park");
        runner.halt();
    }

    #[tokio::test]
    async fn failure_mapper_returning_none_still_advances_floor() {
        let store = Arc::new(MemoryStore::new());
        let payload = OrderPlaced {
            order_id:    Uuid::new_v4(),
            occurred_at: Utc::now(),
        };
        let _ = append_trigger(&store, &payload);

        let mapper: TerminalFailureMapper = std::sync::Arc::new(move |_info| None);

        let runner = ReactorRunner::new(
            AlwaysFails(std::sync::Arc::new(AtomicUsize::new(0))),
            "drop-on-park",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        ).with_terminal_failure(mapper).with_max_attempts(2);

        runner.quiesce().await.unwrap();
        assert!(store.get("drop-on-park").await.unwrap().is_some(),
                "floor advanced past the parked trigger");
        assert_eq!(
            EventLogBackend::read_all(store.as_ref(), LogCursor::ZERO, 10).await.unwrap().len(),
            1,
            "only the trigger in the log — mapper returned None, nothing appended",
        );
        runner.halt();
    }

    #[tokio::test]
    async fn unclassified_failure_without_mapper_parks_with_builtin_fact() {
        // The mandatory terminal path (Primitive 5): no mapper means
        // the BUILT-IN causal:reaction_failed fact — appended to the
        // TRIGGER's own subject history so a completion fold over that
        // subject sees the failure — not retry-forever. (There is no
        // infinite-retry mode; transient-class backoff is the
        // sanctioned wait-out-the-outage behavior.)
        let store = Arc::new(MemoryStore::new());
        let payload = OrderPlaced {
            order_id:    Uuid::new_v4(),
            occurred_at: Utc::now(),
        };
        let trigger_id = append_trigger(&store, &payload);

        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let runner = ReactorRunner::new(
            AlwaysFails(calls.clone()),
            "no-mapper-parks",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        );

        runner.quiesce().await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst),
                   crate::engine::DEFAULT_MAX_ATTEMPTS as usize,
                   "domain policy for unclassified errors: bounded attempts");

        let all = EventLogBackend::read_all(store.as_ref(), LogCursor::ZERO, 10)
            .await.unwrap();
        let parked = all.iter()
            .find(|e| e.event_type == REACTION_FAILED_KIND)
            .expect("built-in terminal fact in the log");
        assert_eq!(parked.category, "order_placed",
                   "built-in fact adopts the TRIGGER's subject placement");
        assert_eq!(parked.subject_id, payload.order_id,
                   "built-in fact adopts the TRIGGER's subject_id");
        assert_eq!(parked.causation_id, Some(trigger_id));
        assert_eq!(parked.payload["class"], "unclassified",
                   "never-classified errors are labeled honestly, not as 'domain'");
        assert_eq!(parked.payload["consumer"], "no-mapper-parks");
        assert!(store.get("no-mapper-parks").await.unwrap().is_some(),
                "floor advanced past the parked trigger");
        runner.halt();
    }

    struct AlwaysFailsOneAttempt(std::sync::Arc<AtomicUsize>);
    #[async_trait]
    impl Reactor for AlwaysFailsOneAttempt {
        type Trigger = OrderPlaced;
        const NAME: &'static str = "always-fails-one-attempt";
        async fn react(&self, _t: &OrderPlaced, _: Ctx<'_>) -> Result<Events> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(anyhow!("boom"))
        }
        fn retry_policy(&self) -> Option<crate::reactor::RetryPolicy> {
            Some(crate::reactor::RetryPolicy::fixed(1, 0))
        }
    }

    #[tokio::test]
    async fn per_reactor_retry_policy_overrides_engine_default_max_attempts() {
        // A reactor that declares max_attempts = 1 via retry_policy() must
        // park after a single failure, regardless of the engine-wide default
        // (DEFAULT_MAX_ATTEMPTS = 3).
        let store = Arc::new(MemoryStore::new());
        let payload = OrderPlaced {
            order_id:    Uuid::new_v4(),
            occurred_at: Utc::now(),
        };
        append_trigger(&store, &payload);

        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let runner = ReactorRunner::new(
            AlwaysFailsOneAttempt(calls.clone()),
            "always-fails-one-attempt",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        );

        runner.quiesce().await.unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst), 1,
            "reactor's own retry_policy (max_attempts=1) must take precedence over engine default of {}",
            crate::engine::DEFAULT_MAX_ATTEMPTS,
        );
        runner.halt();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn different_subjects_process_concurrently() {
        // The point of partitioning: two subjects' triggers run in
        // overlapping time in the SAME consumer.
        use std::sync::atomic::AtomicU64;
        struct Barrier2 { inside: Arc<AtomicU64>, peak: Arc<AtomicU64>, gate: Arc<tokio::sync::Notify> }
        #[async_trait]
        impl Reactor for Barrier2 {
            type Trigger = OrderPlaced;
            const NAME: &'static str = "barrier2";
            async fn react(&self, _t: &OrderPlaced, _: Ctx<'_>) -> Result<Events> {
                let now = self.inside.fetch_add(1, Ordering::SeqCst) + 1;
                self.peak.fetch_max(now, Ordering::SeqCst);
                if now >= 2 {
                    self.gate.notify_waiters();
                } else {
                    let _ = tokio::time::timeout(
                        Duration::from_secs(2), self.gate.notified(),
                    ).await;
                }
                self.inside.fetch_sub(1, Ordering::SeqCst);
                Ok(Events::new())
            }
        }

        let store = Arc::new(MemoryStore::new());
        append_trigger(&store, &OrderPlaced { order_id: Uuid::new_v4(), occurred_at: Utc::now() });
        append_trigger(&store, &OrderPlaced { order_id: Uuid::new_v4(), occurred_at: Utc::now() });

        let inside = Arc::new(AtomicU64::new(0));
        let peak = Arc::new(AtomicU64::new(0));
        let runner = ReactorRunner::new(
            Barrier2 {
                inside: inside.clone(),
                peak: peak.clone(),
                gate: Arc::new(tokio::sync::Notify::new()),
            },
            "r.concurrent",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        );

        runner.quiesce().await.unwrap();
        assert!(peak.load(Ordering::SeqCst) >= 2,
                "two subjects' reactions overlapped (peak in-flight = {})",
                peak.load(Ordering::SeqCst));
        runner.halt();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn ordering_none_runs_same_subject_concurrently() {
        // Ordering::None — per-event partitions: even SAME-subject
        // triggers overlap. (Under the PerSubject default they would
        // serialize; this is the declared opt-out for commutative work.)
        use std::sync::atomic::AtomicU64;
        struct FreeBarrier { inside: Arc<AtomicU64>, peak: Arc<AtomicU64>, gate: Arc<tokio::sync::Notify> }
        #[async_trait]
        impl Reactor for FreeBarrier {
            type Trigger = OrderPlaced;
            const NAME: &'static str = "free-barrier";
            const ORDERING: crate::reactor::Ordering = crate::reactor::Ordering::None;
            async fn react(&self, _t: &OrderPlaced, _: Ctx<'_>) -> Result<Events> {
                let now = self.inside.fetch_add(1, Ordering::SeqCst) + 1;
                self.peak.fetch_max(now, Ordering::SeqCst);
                if now >= 2 {
                    self.gate.notify_waiters();
                } else {
                    let _ = tokio::time::timeout(
                        Duration::from_secs(2), self.gate.notified(),
                    ).await;
                }
                self.inside.fetch_sub(1, Ordering::SeqCst);
                Ok(Events::new())
            }
        }

        let store = Arc::new(MemoryStore::new());
        let subject = Uuid::new_v4(); // SAME subject for both triggers
        append_trigger(&store, &OrderPlaced { order_id: subject, occurred_at: Utc::now() });
        append_trigger(&store, &OrderPlaced { order_id: subject, occurred_at: Utc::now() });

        let peak = Arc::new(AtomicU64::new(0));
        let runner = ReactorRunner::new(
            FreeBarrier {
                inside: Arc::new(AtomicU64::new(0)),
                peak: peak.clone(),
                gate: Arc::new(tokio::sync::Notify::new()),
            },
            "r.free",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        );

        runner.quiesce().await.unwrap();
        assert!(peak.load(Ordering::SeqCst) >= 2,
                "Ordering::None must overlap same-subject triggers (peak = {})",
                peak.load(Ordering::SeqCst));
        runner.halt();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn ordering_per_workflow_serializes_across_subjects() {
        // Ordering::PerWorkflow — one workflow's triggers process in
        // log order even across DIFFERENT subjects (the run-pipeline
        // shape). Probe: track overlap and order; expect strictly
        // serial, log-ordered execution.
        use std::sync::atomic::AtomicU64;
        struct SerialProbe {
            inside: Arc<AtomicU64>,
            peak: Arc<AtomicU64>,
            order: Arc<parking_lot::Mutex<Vec<Uuid>>>,
        }
        #[async_trait]
        impl Reactor for SerialProbe {
            type Trigger = OrderPlaced;
            const NAME: &'static str = "serial-probe";
            const ORDERING: crate::reactor::Ordering = crate::reactor::Ordering::PerWorkflow;
            async fn react(&self, t: &OrderPlaced, _: Ctx<'_>) -> Result<Events> {
                let now = self.inside.fetch_add(1, Ordering::SeqCst) + 1;
                self.peak.fetch_max(now, Ordering::SeqCst);
                // Dwell long enough that a concurrent dispatch WOULD
                // overlap if the partitioning were per-subject.
                tokio::time::sleep(Duration::from_millis(25)).await;
                self.order.lock().push(t.order_id);
                self.inside.fetch_sub(1, Ordering::SeqCst);
                Ok(Events::new())
            }
        }

        let store = Arc::new(MemoryStore::new());
        let workflow = Uuid::new_v4();
        let subjects: Vec<Uuid> = (0..3).map(|_| Uuid::new_v4()).collect();
        for s in &subjects {
            let payload = OrderPlaced { order_id: *s, occurred_at: Utc::now() };
            let ev = EventData {
                event_id: Uuid::new_v4(),
                causation_id: None,
                workflow_id: workflow, // SAME workflow, different subjects
                event_type: <OrderPlaced as Event>::NAME.to_string(),
                payload: serde_json::to_value(&payload).unwrap(),
                created_at: Utc::now(),
                category: Some(<OrderPlaced as Event>::NAME.to_string()),
                subject_id: Some(*s),
                metadata: serde_json::Map::new(),
                ephemeral: None,
                persistent: true,
            };
            crate::append_event(store.as_ref(), ev).await.unwrap();
        }

        let peak = Arc::new(AtomicU64::new(0));
        let order = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let runner = ReactorRunner::new(
            SerialProbe {
                inside: Arc::new(AtomicU64::new(0)),
                peak: peak.clone(),
                order: order.clone(),
            },
            "r.workflow",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        );

        runner.quiesce().await.unwrap();
        assert_eq!(peak.load(Ordering::SeqCst), 1,
                   "one workflow's triggers must never overlap under PerWorkflow");
        assert_eq!(*order.lock(), subjects,
                   "one workflow's triggers process in log order across subjects");
        runner.halt();
    }

    // ── BLOCKING-2: the retry taxonomy ──

    /// Fails with the given classification until `until` calls, then
    /// succeeds (emits one ShippedNotification).
    struct FailsClassified {
        class: crate::failure::ErrorClass,
        until: usize,
        calls: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl Reactor for FailsClassified {
        type Trigger = OrderPlaced;
        const NAME: &'static str = "fails-classified";
        async fn react(&self, t: &OrderPlaced, _: Ctx<'_>) -> Result<Events> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if n <= self.until {
                let e = anyhow!("classified failure on call {n}");
                return Err(match self.class {
                    crate::failure::ErrorClass::Transient => crate::failure::transient(e),
                    crate::failure::ErrorClass::Poison => crate::failure::poison(e),
                    crate::failure::ErrorClass::Domain => crate::failure::domain(e),
                });
            }
            let mut out = Events::new();
            out.push(ShippedNotification { order_id: t.order_id });
            Ok(out)
        }
    }

    #[tokio::test(start_paused = true)]
    async fn transient_retries_past_the_domain_attempt_budget() {
        // A ten-minute outage must NOT mass-park triggers: transient-
        // classified failures ignore max_attempts and wait it out under
        // the liveness ceiling. Here: 10 failures >> budget of 3, then
        // recovery — the output lands, the mapper never fires.
        let store = Arc::new(MemoryStore::new());
        append_trigger(&store, &OrderPlaced { order_id: Uuid::new_v4(), occurred_at: Utc::now() });

        let mapper_calls = std::sync::Arc::new(AtomicUsize::new(0));
        let mapper_calls_c = mapper_calls.clone();
        let mapper: TerminalFailureMapper = std::sync::Arc::new(move |_info| {
            mapper_calls_c.fetch_add(1, Ordering::SeqCst);
            None
        });

        let calls = Arc::new(AtomicUsize::new(0));
        let runner = ReactorRunner::new(
            FailsClassified {
                class: crate::failure::ErrorClass::Transient,
                until: 10,
                calls: calls.clone(),
            },
            "r.transient",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        )
        .with_terminal_failure(mapper)
        .with_max_attempts(3);

        runner.quiesce().await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 11, "10 failures + the success");
        assert_eq!(mapper_calls.load(Ordering::SeqCst), 0,
                   "transient never consults the domain attempt budget");
        let outs = EventLogBackend::read_all(store.as_ref(), LogCursor::ZERO, 20)
            .await.unwrap()
            .iter()
            .filter(|e| e.event_type == "shipped_notification")
            .count();
        assert_eq!(outs, 1, "the trigger recovered and produced its output");
        runner.halt();
    }

    #[tokio::test(start_paused = true)]
    async fn transient_exhausts_at_the_liveness_ceiling() {
        // The ceiling exists because misclassification is inevitable —
        // an unbounded-transient partition is a silent permanent wedge.
        // Under virtual time (start_paused), six hours of capped
        // backoff elapse in milliseconds of real time: the machinery is
        // testable precisely because the ceiling is liveness time, not
        // chrono.
        let store = Arc::new(MemoryStore::new());
        append_trigger(&store, &OrderPlaced { order_id: Uuid::new_v4(), occurred_at: Utc::now() });

        let seen_class = std::sync::Arc::new(parking_lot::Mutex::new(None));
        let seen_class_c = seen_class.clone();
        let mapper: TerminalFailureMapper = std::sync::Arc::new(move |info: TerminalFailure| {
            *seen_class_c.lock() = Some(info.class);
            None
        });

        let calls = Arc::new(AtomicUsize::new(0));
        let runner = ReactorRunner::new(
            FailsClassified {
                class: crate::failure::ErrorClass::Transient,
                until: usize::MAX, // never recovers
                calls: calls.clone(),
            },
            "r.transient.ceiling",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        )
        .with_terminal_failure(mapper);

        // Drive with virtual-MINUTE polls: the worker's 5s-capped
        // backoff timers auto-advance between them. (quiesce()'s 2ms
        // poll would force virtual time forward in 2ms hops — six
        // hours of those is millions of iterations.)
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while seen_class.lock().is_none() {
            runner.step(64).await.unwrap();
            assert!(std::time::Instant::now() < deadline,
                    "ceiling never fired (real-time guard)");
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
        runner.quiesce().await.unwrap(); // reap the ack, persist the floor
        assert_eq!(
            *seen_class.lock(),
            Some(crate::failure::FailureClass::TransientExhausted),
            "parked as transient_exhausted — mass-replayable as a class",
        );
        // 6h ceiling / 5s backoff cap ⇒ thousands of attempts, not 3.
        assert!(calls.load(Ordering::SeqCst) > 1000,
                "the ceiling is time, not attempts (got {} attempts)",
                calls.load(Ordering::SeqCst));
        assert!(store.get("r.transient.ceiling").await.unwrap().is_some(),
                "floor advanced past the exhausted trigger");
        runner.halt();
    }

    #[tokio::test]
    async fn mapper_receives_class_and_the_triggers_subject_identity() {
        // BLOCKING-2's mapper contract: the mapper must be able to
        // stamp its fact with the failing trigger's subject identity
        // (so completion folds can fold the failure) and see the
        // declared class.
        let store = Arc::new(MemoryStore::new());
        let payload = OrderPlaced { order_id: Uuid::new_v4(), occurred_at: Utc::now() };
        append_trigger(&store, &payload);

        let seen = std::sync::Arc::new(parking_lot::Mutex::new(None));
        let seen_c = seen.clone();
        let mapper: TerminalFailureMapper = std::sync::Arc::new(move |info: TerminalFailure| {
            *seen_c.lock() = Some((info.subject.clone(), info.subject_id, info.class));
            None
        });

        let runner = ReactorRunner::new(
            FailsClassified {
                class: crate::failure::ErrorClass::Domain,
                until: usize::MAX,
                calls: Arc::new(AtomicUsize::new(0)),
            },
            "r.domain",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        )
        .with_terminal_failure(mapper)
        .with_max_attempts(2);

        runner.quiesce().await.unwrap();
        let (subject, subject_id, class) = seen.lock().clone().expect("mapper fired");
        assert_eq!(subject, "order_placed", "the trigger's subject placement");
        assert_eq!(subject_id, payload.order_id, "the trigger's subject_id");
        assert_eq!(class, crate::failure::FailureClass::Domain,
                   "a declared-domain park is labeled domain (not unclassified)");
        runner.halt();
    }

    #[tokio::test]
    async fn fenced_workflow_is_acked_at_dispatch_gate_without_calling_reactor() {
        // Engine::cancel_workflow sets a fence; any trigger whose
        // workflow_id is in the fence must be skipped at the dispatch
        // gate — the reactor body must not be called and the floor must
        // still advance (the trigger is not pending work).
        let store = Arc::new(MemoryStore::new());
        let wf_cancelled = Uuid::new_v4();
        let wf_live      = Uuid::new_v4();

        let append_for_wf = |store: Arc<MemoryStore>, wf: Uuid, order_id: Uuid| {
            let ev = EventData {
                event_id:    Uuid::new_v4(),
                causation_id: None,
                workflow_id: wf,
                event_type:  <OrderPlaced as Event>::NAME.to_string(),
                payload:     serde_json::to_value(OrderPlaced {
                    order_id, occurred_at: Utc::now(),
                }).unwrap(),
                created_at:  Utc::now(),
                category:    Some(<OrderPlaced as Event>::NAME.to_string()),
                subject_id:  Some(order_id),
                metadata:    serde_json::Map::new(),
                ephemeral:   None,
                persistent:  true,
            };
            futures::executor::block_on(crate::append_event(store.as_ref(), ev)).unwrap();
        };

        // One cancelled-workflow trigger, one live trigger.
        append_for_wf(store.clone(), wf_cancelled, Uuid::new_v4());
        append_for_wf(store.clone(), wf_live,      Uuid::new_v4());

        let calls = Arc::new(AtomicUsize::new(0));
        let calls_c = calls.clone();

        struct CountingReactor(Arc<AtomicUsize>);
        #[async_trait]
        impl Reactor for CountingReactor {
            type Trigger = OrderPlaced;
            const NAME: &'static str = "counting-reactor";
            async fn react(&self, t: &OrderPlaced, _: Ctx<'_>) -> Result<Events> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut out = Events::new();
                out.push(ShippedNotification { order_id: t.order_id });
                Ok(out)
            }
        }

        let fence: crate::engine::CancelledWorkflows =
            Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
        fence.lock().unwrap().insert(wf_cancelled);

        let runner = ReactorRunner::new(
            CountingReactor(calls_c),
            "r.fence-gate",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        )
        .with_cancelled_workflows(fence);

        runner.quiesce().await.unwrap();

        // Reactor called exactly once — for the live workflow only.
        assert_eq!(calls.load(Ordering::SeqCst), 1,
                   "reactor must not be called for a fenced workflow");

        // One output: the live workflow's trigger produced it.
        let all = EventLogBackend::read_all(store.as_ref(), LogCursor::ZERO, 20)
            .await.unwrap();
        assert_eq!(
            all.iter().filter(|e| e.event_type == "shipped_notification").count(),
            1,
            "only the live workflow produced output",
        );

        // Floor advanced past both triggers.
        assert!(store.get("r.fence-gate").await.unwrap().is_some(),
                "ack-floor advanced past the fenced trigger");
        runner.halt();
    }

    #[tokio::test]
    async fn poison_classified_react_error_parks_on_first_attempt() {
        // A reactor can declare its OWN error deterministic — parks
        // immediately, no retry, even without a mapper.
        let store = Arc::new(MemoryStore::new());
        append_trigger(&store, &OrderPlaced { order_id: Uuid::new_v4(), occurred_at: Utc::now() });

        let calls = Arc::new(AtomicUsize::new(0));
        let runner = ReactorRunner::new(
            FailsClassified {
                class: crate::failure::ErrorClass::Poison,
                until: usize::MAX,
                calls: calls.clone(),
            },
            "r.poison.declared",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        );

        runner.quiesce().await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1,
                   "poison parks on the first attempt — retrying reproduces it");
        let all = EventLogBackend::read_all(store.as_ref(), LogCursor::ZERO, 10)
            .await.unwrap();
        let parked = all.iter()
            .find(|e| e.event_type == REACTION_FAILED_KIND)
            .expect("built-in terminal fact");
        assert_eq!(parked.payload["class"], "poison");
        runner.halt();
    }
}
