//! Runner for [`Reactor`](crate::reactor::Reactor) consumers.
//!
//! At-least-once + idempotent reactor execution (no outbox):
//!
//!   1. Read trigger fact from the log (`read_all` from the cursor).
//!   2. Filter by `R::Trigger::CATEGORY`.
//!   3. Call `reactor.react(trigger, ctx)` → `Result<Events>`.
//!   4. Append each output **directly** to its own stream
//!      (`append_to_stream(category, stream_id, Any, …)`) with a
//!      deterministic `event_id = uuid_v5(NS_REACTOR_OUTPUT,
//!      [reactor_id, trigger_id, idx])`.
//!   5. Advance the cursor (`checkpoint.set`).
//!   6. Idempotency: a crash between step 4 and step 5 re-runs `react()`
//!      on restart; the re-append dedups on `event_id` (C1), so exactly
//!      one output lands and the cursor advances. The reaction itself is
//!      kept replayable by the side-effect `EffectStore`.
//!
//! On `react()` Err: cursor unchanged, nothing appended; the function
//! propagates the error and the outer supervisor decides retry timing.
//!
//! Per C5, a fresh reactor cursor would initialize at
//! `EventLogBackend::latest_position()` rather than at `LogCursor::ZERO`
//! — but that initialization is the engine builder's job, not the
//! runner's. The runner just consumes from the cursor it finds.

use std::sync::Arc;

use anyhow::Result;
use serde::de::DeserializeOwned;
use tokio::sync::OnceCell;
use uuid::Uuid;

use crate::aggregator::AggregatorRegistry;
use crate::checkpoint_store::ReactorCheckpoint;
use crate::contexts::Ctx;
use crate::engine::{TerminalFailure, TerminalFailureMapper};
use crate::event_log::EventLogBackend;
use crate::event::Event;
use crate::projection_runner::StepOutcome;
use crate::reactor_observer::ReactorObserver;
use crate::reactor::Reactor;
use crate::types::{EventData, LogCursor, RecordedEvent, StreamState};

/// Namespace UUID for deriving deterministic reactor-output event_ids
/// via uuid v5. Hardcoded so that the same `(reactor_id, trigger_id,
/// idx)` always produces the same event_id across processes / restarts
/// — that's what makes the log's idempotent-append-on-event_id collapse
/// retried reactor runs into a single durable entry (C1 + C12).
pub const NS_REACTOR_OUTPUT: Uuid = Uuid::from_bytes([
    0x4d, 0xfe, 0xc4, 0xf2, 0x6e, 0x88, 0x4f, 0x3a,
    0xa9, 0xb1, 0x7c, 0x5e, 0xb3, 0x39, 0x21, 0x0a,
]);

/// Compute the deterministic event_id for the `idx`-th output of a
/// reactor with id `reactor_id` triggered by event `trigger_event_id`.
///
/// Public so backend impls and tests can reproduce the derivation.
pub fn derive_output_event_id(
    reactor_id: &str,
    trigger_event_id: Uuid,
    output_index: u32,
) -> Uuid {
    // NUL-byte separator: never appears in valid identifiers or UUIDs,
    // so the encoding is unambiguous regardless of reactor_id contents.
    let mut key: Vec<u8> = Vec::with_capacity(reactor_id.len() + 36 + 16);
    key.extend_from_slice(reactor_id.as_bytes());
    key.push(0);
    key.extend_from_slice(trigger_event_id.as_bytes());
    key.push(0);
    key.extend_from_slice(&output_index.to_le_bytes());
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

pub struct ReactorRunner<R: Reactor> {
    reactor:     R,
    consumer_id: String,
    log:         Arc<dyn EventLogBackend>,
    checkpoint:  Arc<dyn ReactorCheckpoint>,
    aggregators: Option<Arc<AggregatorRegistry>>,
    hydrated:    OnceCell<()>,
    /// terminal-failure mapper for terminal-failure handling. When `react()`
    /// errors `max_attempts` times on the same trigger, the mapper
    /// is invoked; if it returns `Some(fact)`, the synthesized
    /// fact is appended directly to its stream and the cursor advances
    /// past the failing event.
    failure_mapper:  Option<TerminalFailureMapper>,
    /// Retry budget — applies only when `failure_mapper` is set. Without
    /// a mapper, reactors retry indefinitely (supervisor backoff).
    max_attempts: u32,
    /// Inspector / telemetry hook. Default `None` = zero overhead.
    observer:    Option<Arc<dyn ReactorObserver>>,
    /// Reaction-result cache (Phase 4). Surfaced to the reactor body via
    /// `ctx.effect_store()` so side-effecting reactors memoize their
    /// external call under the reaction key — safe under retry/redelivery.
    effect_store: Option<Arc<dyn crate::effect_store::EffectStore>>,
    /// Engine-level aggregator registry. Reactor outputs are folded into
    /// it after they're appended, so `engine.state_of::<A>(id).await.unwrap()` reflects
    /// reactor-emitted events (not just caller-emitted ones). Separate
    /// from the per-runner `aggregators` clone above.
    engine_aggregators: Option<Arc<AggregatorRegistry>>,
    /// Shared per-correlation high-water tracker for scoped `Engine::settle`.
    /// After appending an output, the runner records the output's position
    /// under the trigger's `correlation_id` so `settle` knows the run's chain
    /// has advanced. `None` outside an engine (e.g. unit tests).
    settle_tracker: Option<crate::engine::CorrHighWater>,
    /// Durable snapshot store for aggregate restore-before-fold (per-consumer
    /// registry) and save-after-output-fold (shared engine registry).
    /// `None` = no durable restore.
    snapshot_store: Option<Arc<dyn crate::snapshot_store::SnapshotStore>>,
    /// Snapshot cadence (events between saves). See `with_snapshot_persistence`.
    snapshot_every: u64,
    /// Categories registered `StreamPolicy::OccRequired` via
    /// `EngineBuilder::with_aggregate`. A reactor output whose routing
    /// category is in this set is rejected — reactors append with
    /// `StreamState::Any` and cannot uphold an aggregate's OCC invariant
    /// (model it as an `Engine::append` command instead). Empty = no fence.
    occ_categories: Arc<std::collections::HashSet<String>>,
}

impl<R: Reactor> ReactorRunner<R>
where
    R::Trigger: DeserializeOwned,
{
    pub fn new(
        reactor: R,
        consumer_id: impl Into<String>,
        log: Arc<dyn EventLogBackend>,
        checkpoint: Arc<dyn ReactorCheckpoint>,
    ) -> Self {
        Self {
            reactor,
            consumer_id: consumer_id.into(),
            log,
            checkpoint,
            aggregators: None,
            hydrated: OnceCell::new(),
            failure_mapper: None,
            max_attempts: 0,
            observer: None,
            effect_store: None,
            engine_aggregators: None,
            settle_tracker: None,
            snapshot_store: None,
            snapshot_every: 0,
            occ_categories: Arc::new(std::collections::HashSet::new()),
        }
    }

    /// Plumb the engine's OCC-required category set so the runner can
    /// reject reactor outputs that would bypass an aggregate's
    /// optimistic-concurrency fence. See the field docs.
    pub(crate) fn with_occ_categories(
        mut self,
        occ_categories: Arc<std::collections::HashSet<String>>,
    ) -> Self {
        self.occ_categories = occ_categories;
        self
    }

    /// Attach a [`ReactorObserver`] for inspector / telemetry capture.
    /// Default: no observer = noop hot path.
    pub fn with_observer(mut self, observer: Arc<dyn ReactorObserver>) -> Self {
        self.observer = Some(observer);
        self
    }

    /// Attach a per-runner [`AggregatorRegistry`] copy. See
    /// [`crate::projection_runner::ProjectionRunner::with_aggregators`]
    /// for semantics.
    pub fn with_aggregators(mut self, aggregators: Arc<AggregatorRegistry>) -> Self {
        self.aggregators = Some(aggregators);
        self
    }

    /// Attach the reaction-result cache, surfaced to the reactor body via
    /// `ctx.effect_store()`.
    pub fn with_effect_store(
        mut self,
        cache: Arc<dyn crate::effect_store::EffectStore>,
    ) -> Self {
        self.effect_store = Some(cache);
        self
    }

    /// Attach the engine-level aggregator registry so reactor outputs
    /// fold into it after append (keeps `engine.state_of` current).
    pub fn with_engine_aggregators(
        mut self,
        engine_aggregators: Option<Arc<AggregatorRegistry>>,
    ) -> Self {
        self.engine_aggregators = engine_aggregators;
        self
    }

    /// Attach the shared per-correlation high-water tracker so appended
    /// outputs advance their run's `settle` mark.
    pub(crate) fn with_settle_tracker(mut self, tracker: crate::engine::CorrHighWater) -> Self {
        self.settle_tracker = Some(tracker);
        self
    }

    /// Wire durable snapshot persistence: restore aggregates before folding
    /// (so `ctx.aggregate` survives restart) and snapshot every `every`
    /// events. `None` store disables both (unchanged behavior).
    pub(crate) fn with_snapshot_persistence(
        mut self,
        store: Option<Arc<dyn crate::snapshot_store::SnapshotStore>>,
        every: u64,
    ) -> Self {
        self.snapshot_store = store;
        self.snapshot_every = every;
        self
    }

    /// Configure terminal-failure terminal-failure handling. After `max_attempts`
    /// consecutive `react()` errors on the same trigger, the mapper
    /// is invoked; on `Some(fact)`, the fact is appended directly to its
    /// stream and the cursor advances past the failing event.
    /// Without this, errored reactors retry indefinitely.
    pub(crate) fn with_terminal_failure(mut self, mapper: TerminalFailureMapper, max_attempts: u32) -> Self {
        self.failure_mapper = Some(mapper);
        self.max_attempts = max_attempts;
        self
    }

    pub fn consumer_id(&self) -> &str { &self.consumer_id }

    /// Terminal-failure routing into the failure store. Invokes the mapper, appends
    /// its synthesized fact (if any) to that fact's own stream, folds it
    /// into the engine registry, advances the cursor past `event`, and
    /// clears the attempt counter **last** (so a failed terminal-failure append can't
    /// reset the budget and a crash mid-terminal-failure can't replay it). Caller
    /// guarantees `self.failure_mapper` is `Some` and the failure is terminal.
    ///
    /// Shared by the react()-error path (terminal after `max_attempts`)
    /// and the trigger-deserialization-poison path (terminal immediately —
    /// re-parsing is deterministic, so retries never help).
    async fn park_terminal_failure(
        &self,
        event: &RecordedEvent,
        attempts: u32,
        error: String,
        completed_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<()> {
        let mapper = self
            .failure_mapper
            .as_ref()
            .expect("park_terminal_failure requires a configured terminal-failure mapper");
        if let Some(obs) = self.observer.as_ref() {
            obs.reactor_terminal_failure(
                event.event_id,
                &self.consumer_id,
                event.correlation_id,
                attempts,
                &error,
                completed_at,
            );
        }
        let info = TerminalFailure {
            consumer:        self.consumer_id.clone(),
            trigger_id:   event.event_id,
            trigger_event_type: event.event_type.clone(),
            error,
            attempts,
            correlation_id:    event.correlation_id,
        };
        // terminal-failure-synthesized output (if any) is appended directly to its own
        // stream. `output_index = u32::MAX` keeps its deterministic id
        // distinct from react() outputs.
        if let Some(fact) = mapper(info) {
            // `cat` is the STREAM placement category; `event_type` keeps
            // the routing category.
            let cat = fact.stream_category().to_string();
            let sid = fact.stream_id();
            let event_type = crate::event_type::compose(fact.category(), fact.variant_name());
            let payload = fact.to_value()?;
            let out_event = EventData {
                event_id: derive_output_event_id(&self.consumer_id, event.event_id, u32::MAX),
                causation_id: Some(event.event_id),
                correlation_id: event.correlation_id,
                event_type: event_type.clone(),
                payload: payload.clone(),
                created_at: chrono::Utc::now(),
                category: Some(cat.clone()),
                stream_id: Some(sid),
                metadata: reactor_output_metadata(&self.consumer_id),
                ephemeral: None,
                persistent: true,
            };
            let write = self
                .log
                .append_to_stream(&cat, sid, StreamState::Any, vec![out_event])
                .await?;
            if let Some(tracker) = &self.settle_tracker {
                tracker.lock().unwrap().bump(event.correlation_id, write.position);
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
        self.checkpoint.set(&self.consumer_id, event.position).await?;
        self.checkpoint
            .clear_reactor_attempts(&self.consumer_id, event.event_id)
            .await?;
        Ok(())
    }

    pub async fn step(&self, batch: usize) -> Result<StepOutcome> {
        let cursor = self.checkpoint.get(&self.consumer_id).await?
            .unwrap_or(LogCursor::ZERO);

        self.ensure_hydrated(cursor).await?;

        let events = self.log.read_all(cursor, batch).await?;
        if events.is_empty() {
            return Ok(StepOutcome::Idle);
        }

        let prefix = <R::Trigger as Event>::CATEGORY;
        let mut applied = 0usize;
        for event in events {
            // Fold every event into the per-consumer aggregator registry.
            // Folds are idempotent on the event's stream coordinates (see
            // `AggregatorRegistry::apply_event`), so a step retry after a
            // checkpoint-set failure, crash redelivery, or a terminal-failure advance
            // re-delivers harmlessly — fold tracks the log, not body
            // success. (The old capture/restore rollback this replaces
            // un-folded state when the *body* failed, permanently
            // desyncing registry from cursor on the terminal-failure path.)
            if let Some(reg) = self.aggregators.as_ref() {
                let outcome = crate::aggregator::fold_event(
                    reg,
                    None,
                    self.log.as_ref(),
                    &event.event_type,
                    &event.payload,
                    event.stream_id,
                    &event.category,
                    event.revision,
                    event.position,
                    /* strict_to_event = */ true,
                )
                .await?;
                if outcome.applied {
                    if let Some(obs) = self.observer.as_ref() {
                        reg.notify_observer(
                            &outcome.snapshots,
                            obs.as_ref(),
                            event.correlation_id,
                            event.position,
                            event.event_id,
                        );
                    }
                }
            }

            if !crate::event_type::matches_category(&event.event_type, prefix) {
                // Non-matching trigger: just advance the cursor.
                self.checkpoint.set(&self.consumer_id, event.position).await?;
                continue;
            }

            // ── Record this attempt FIRST (before deserialization), so a
            //    poison trigger engages the terminal-failure budget instead of wedging
            //    the cursor before the counter ever increments.
            let attempt_seq = self.checkpoint
                .record_reactor_attempt(&self.consumer_id, event.event_id)
                .await?;
            let started_at = chrono::Utc::now();

            // Deserialize the trigger. A failure here is a poison pill —
            // deterministic, so retrying never helps. Route it straight to
            // the failure store (if a mapper is configured) so one malformed/
            // schema-drifted payload can't block the cursor forever;
            // without a mapper, propagate (block-until-fixed, the same
            // contract a react() error has without a mapper — an operator
            // fixes the schema or code, then it processes).
            let trigger: R::Trigger = match serde_json::from_value(event.payload.clone()) {
                Ok(t) => t,
                Err(deser_err) => {
                    let msg = format!(
                        "trigger deserialization failed for {} (event {}): {deser_err}",
                        event.event_type, event.event_id,
                    );
                    if self.failure_mapper.is_some() {
                        self.park_terminal_failure(&event, attempt_seq, msg, chrono::Utc::now()).await?;
                        applied += 1;
                        continue;
                    }
                    return Err(anyhow::anyhow!(msg));
                }
            };

            if let Some(obs) = self.observer.as_ref() {
                obs.reactor_started(
                    event.event_id,
                    &self.consumer_id,
                    event.correlation_id,
                    attempt_seq,
                    started_at,
                );
                // describe() runs once per attempt, BEFORE react().
                // Optional: reactors that don't override the default
                // return `None` and the observer hook is skipped.
                if let Some(descr) = self.reactor.describe(&trigger) {
                    obs.reactor_description(
                        event.correlation_id,
                        event.position,
                        event.event_id,
                        &self.consumer_id,
                        descr,
                    );
                }
            }

            // Per-attempt log sink — react body pushes via `ctx.log(...)`.
            // Drained below into the observer's reactor_completed /
            // reactor_failed hooks.
            let log_sink: parking_lot::Mutex<Vec<crate::types::LogEntry>> =
                parking_lot::Mutex::new(Vec::new());
            let ctx = Ctx {
                event_id:       event.event_id,
                log_position:   event.position,
                occurred_at:    trigger.occurred_at().unwrap_or(event.created_at),
                correlation_id: event.correlation_id,
                metadata:       &event.metadata,
                aggregators:    self.aggregators.as_ref(),
                logs:           Some(&log_sink),
                effect_store: self.effect_store.as_ref(),
            };

            // ── Decision. On Err, cursor stays where it was; no rows
            //    persisted. The whole batch from this trigger forward
            //    is retried on next step.
            //
            //    UNLESS a terminal-failure mapper is configured AND attempts have
            //    reached max — then synthesize the mapper's fact,
            //    append it directly to its stream, advance the cursor past
            //    the failing event, and clear the attempt counter.
            let emitted = match self.reactor.react(&trigger, ctx).await {
                Ok(events) => {
                    let completed_at = chrono::Utc::now();
                    if let Some(obs) = self.observer.as_ref() {
                        let drained = log_sink.into_inner();
                        obs.reactor_completed(
                            event.event_id,
                            &self.consumer_id,
                            event.correlation_id,
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
                    let completed_at = chrono::Utc::now();
                    // Note: the fold above is NOT rolled back — registry
                    // state reflects the log regardless of body success.

                    // attempt counter already incremented for this run;
                    // use attempt_seq for the cap check.
                    let attempts = attempt_seq;

                    if self.failure_mapper.is_some() && attempts >= self.max_attempts {
                        self.park_terminal_failure(&event, attempts, format!("{:#}", e), completed_at)
                            .await?;
                        applied += 1;
                        continue;
                    }
                    // Retry path: record failed-attempt telemetry, then
                    // propagate to supervisor for backoff.
                    if let Some(obs) = self.observer.as_ref() {
                        let drained = log_sink.into_inner();
                        obs.reactor_failed(
                            event.event_id,
                            &self.consumer_id,
                            event.correlation_id,
                            attempts,
                            started_at,
                            completed_at,
                            &format!("{:#}", e),
                            &drained,
                        );
                    }
                    return Err(e);
                }
            };

            // ── OCC fence (pre-flight). A reactor appends its outputs
            //    with `StreamState::Any`, so it cannot uphold the
            //    optimistic-concurrency invariant of an aggregate stream
            //    registered via `with_aggregate`. Emitting into such a
            //    category silently corrupts that aggregate's OCC
            //    guarantee — a deterministic config error, so route the
            //    whole trigger to the failure store (no retries) rather than append.
            if !self.occ_categories.is_empty() {
                if let Some(bad) = emitted.iter().find(|out| {
                    self.occ_categories
                        .contains(crate::event_type::category_of(&out.durable_name))
                }) {
                    let cat = crate::event_type::category_of(&bad.durable_name).to_string();
                    let msg = format!(
                        "reactor '{}' emitted a fact in OCC-required category '{cat}' — \
                         reactor outputs append with StreamState::Any and cannot uphold \
                         the aggregate's optimistic-concurrency fence; model this as an \
                         Engine::append command, not a reactor output",
                        self.consumer_id,
                    );
                    if self.failure_mapper.is_some() {
                        self.park_terminal_failure(&event, attempt_seq, msg, chrono::Utc::now()).await?;
                        applied += 1;
                        continue;
                    }
                    return Err(anyhow::anyhow!(msg));
                }
            }

            // ── Append each output directly to its own stream with a
            //    deterministic event_id (idempotent under redelivery via
            //    the log's append-dedup, C1), then advance the cursor.
            //    At-least-once + idempotent emit: a crash between append
            //    and cursor-advance re-runs react() on restart; the
            //    re-appends dedup on event_id.
            for (idx, out) in emitted.iter().enumerate() {
                let out_event = EventData {
                    event_id: derive_output_event_id(
                        &self.consumer_id, event.event_id, idx as u32,
                    ),
                    causation_id: Some(event.event_id),
                    correlation_id: event.correlation_id,
                    event_type: out.durable_name.clone(),
                    payload: out.payload.clone(),
                    created_at: chrono::Utc::now(),
                    // Placement uses the STREAM category; routing stays on
                    // `event_type` (durable_name). Equal unless overridden.
                    category: Some(out.stream_category.clone()),
                    stream_id: Some(out.stream_id),
                    metadata: reactor_output_metadata(&self.consumer_id),
                    ephemeral: None,
                    persistent: true,
                };
                let write = self.log
                    .append_to_stream(
                        &out.stream_category, out.stream_id, StreamState::Any, vec![out_event],
                    )
                    .await?;
                // Advance this run's scoped-settle high-water: the output
                // inherits the trigger's correlation_id, so it belongs to the
                // same chain.
                if let Some(tracker) = &self.settle_tracker {
                    tracker.lock().unwrap().bump(event.correlation_id, write.position);
                }
                // Fold the output into the shared engine registry. The
                // fold is idempotent on stream coordinates, so a
                // redelivered (deduped) append — whose WriteResult
                // carries the ORIGINAL position/revision — skips here
                // instead of double-counting. Gap repair inside
                // fold_event also restores cold aggregates from
                // snapshots before folding (read-through), replacing
                // the explicit pre-append restore this path used to do.
                if let Some(reg) = &self.engine_aggregators {
                    let outcome = crate::aggregator::fold_event(
                        reg.as_ref(),
                        self.snapshot_store.as_deref(),
                        self.log.as_ref(),
                        &out.durable_name,
                        &out.payload,
                        out.stream_id,
                        &out.stream_category,
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
            self.checkpoint.set(&self.consumer_id, event.position).await?;

            applied += 1;
        }

        Ok(StepOutcome::Progressed { applied })
    }

    async fn ensure_hydrated(&self, cursor: LogCursor) -> Result<()> {
        if self.aggregators.is_none() {
            return Ok(());
        }
        self.hydrated.get_or_try_init(|| async {
            if cursor == LogCursor::ZERO {
                return Ok::<(), anyhow::Error>(());
            }
            let reg = self.aggregators.as_ref().unwrap();
            let mut from = LogCursor::ZERO;
            loop {
                let batch = self.log.read_all(from, 1024).await?;
                if batch.is_empty() { break; }
                let last_pos = batch.last().unwrap().position;
                let mut hit_cursor = false;
                for event in batch {
                    if event.position > cursor { hit_cursor = true; break; }
                    crate::aggregator::fold_event(
                        reg,
                        None,
                        self.log.as_ref(),
                        &event.event_type,
                        &event.payload,
                        event.stream_id,
                        &event.category,
                        event.revision,
                        event.position,
                        /* strict_to_event = */ true,
                    )
                    .await?;
                }
                if hit_cursor || last_pos >= cursor { break; }
                from = last_pos;
            }
            Ok(())
        }).await?;
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

    // ── Trigger fact ──
    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct OrderPlaced {
        order_id:    Uuid,
        occurred_at: DateTime<Utc>,
    }
    impl Event for OrderPlaced {
        const CATEGORY: &'static str = "order";
        fn event_type(&self) -> &str { "order_placed" }
        fn stream_id(&self) -> Uuid { self.order_id }
        fn occurred_at(&self) -> Option<DateTime<Utc>> { Some(self.occurred_at) }
    }

    // ── Output fact ──
    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct ShippedNotification {
        order_id: Uuid,
    }
    impl Event for ShippedNotification {
        const CATEGORY: &'static str = "shipping";
        fn event_type(&self) -> &str { "shipped_notification" }
        fn stream_id(&self) -> Uuid { self.order_id }
    }

    fn append_trigger(store: &MemoryStore, payload: &OrderPlaced) -> Uuid {
        let event_id = Uuid::new_v4();
        let ev = EventData {
            event_id,
            causation_id:       None,
            correlation_id:  Uuid::new_v4(),
            event_type:      format!("{}:{}", <OrderPlaced as Event>::CATEGORY, payload.event_type()),
            payload:         serde_json::to_value(payload).unwrap(),
            created_at:      Utc::now(),
            category:  None,
            stream_id:    None,
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
        let a = derive_output_event_id("reactor.a", trigger, 0);
        let b = derive_output_event_id("reactor.a", trigger, 0);
        assert_eq!(a, b, "same inputs MUST produce same event_id");

        let c = derive_output_event_id("reactor.a", trigger, 1);
        assert_ne!(a, c, "different output_index MUST produce different ids");

        let d = derive_output_event_id("reactor.b", trigger, 0);
        assert_ne!(a, d, "different reactor_id MUST produce different ids");
    }

    #[tokio::test]
    async fn reactor_step_appends_output_directly_and_advances_cursor() {
        let store = Arc::new(MemoryStore::new());
        let trigger = OrderPlaced { order_id: Uuid::new_v4(), occurred_at: Utc::now() };
        let trigger_event_id = append_trigger(&store, &trigger);

        let runner = ReactorRunner::new(
            EmitOne,
            "r.shipper",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        );

        let outcome = runner.step(10).await.unwrap();
        assert!(matches!(outcome, StepOutcome::Progressed { applied: 1 }));

        // Cursor advanced
        assert!(store.get("r.shipper").await.unwrap().is_some());

        // Output appended DIRECTLY to its own stream in the log (no outbox).
        let all = EventLogBackend::read_all(store.as_ref(), LogCursor::ZERO, 10)
            .await
            .unwrap();
        let out = all
            .iter()
            .find(|e| e.event_type == "shipping:shipped_notification")
            .expect("reactor output appended to the log");
        assert_eq!(out.category, "shipping");
        assert_eq!(out.causation_id, Some(trigger_event_id));
        // Deterministic id matches the helper's output (idempotent on redelivery).
        assert_eq!(
            out.event_id,
            derive_output_event_id("r.shipper", trigger_event_id, 0),
        );
    }

    #[tokio::test]
    async fn reactor_output_propagates_trigger_correlation_id() {
        // The whole point of correlation_id propagation: a fact
        // emitted in response to a trigger should carry the trigger's
        // correlation_id so cross-system tracing chains through the
        // reactor.
        let store = Arc::new(MemoryStore::new());
        let trigger = OrderPlaced { order_id: Uuid::new_v4(), occurred_at: Utc::now() };
        let trigger_event_id = append_trigger(&store, &trigger);

        // Read the persisted event to get the correlation_id the helper
        // generated.
        let persisted = EventLogBackend::read_all(
            store.as_ref(), LogCursor::ZERO, 10,
        ).await.unwrap();
        let trigger_correlation = persisted[0].correlation_id;

        let runner = ReactorRunner::new(
            EmitOne,
            "r.trace",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        );
        runner.step(10).await.unwrap();

        // The output is appended directly to the log and carries the
        // trigger's correlation_id (cross-system tracing chains through
        // the reactor) + the trigger as its causation_id.
        let all_events = EventLogBackend::read_all(
            store.as_ref(), LogCursor::ZERO, 10,
        ).await.unwrap();
        let output_event = all_events.iter()
            .find(|e| e.event_type == "shipping:shipped_notification")
            .expect("reactor output present in log");
        assert_eq!(output_event.correlation_id, trigger_correlation,
                   "output MUST carry trigger's correlation_id");
        assert_eq!(output_event.causation_id, Some(trigger_event_id),
                   "output's causation_id MUST be the trigger's event_id");
    }

    #[tokio::test]
    async fn reactor_step_with_empty_react_advances_cursor_only() {
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

        runner.step(10).await.unwrap();
        assert!(store.get("r.silent").await.unwrap().is_some());
        assert_eq!(
            EventLogBackend::read_all(store.as_ref(), LogCursor::ZERO, 10).await.unwrap().len(),
            1,
            "only the trigger in the log — no reactor output",
        );
    }

    #[tokio::test]
    async fn reactor_step_with_n_outputs_appends_n_in_order() {
        let store = Arc::new(MemoryStore::new());
        let trigger = OrderPlaced { order_id: Uuid::new_v4(), occurred_at: Utc::now() };
        let trigger_event_id = append_trigger(&store, &trigger);

        let runner = ReactorRunner::new(
            EmitN(5),
            "r.fanout",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        );

        runner.step(10).await.unwrap();

        let all = EventLogBackend::read_all(store.as_ref(), LogCursor::ZERO, 20)
            .await
            .unwrap();
        let outs: Vec<_> = all
            .iter()
            .filter(|e| e.event_type == "shipping:shipped_notification")
            .collect();
        assert_eq!(outs.len(), 5, "all 5 outputs appended to the log");

        // Each output index has its deterministic event_id present.
        for i in 0..5u32 {
            let want = derive_output_event_id("r.fanout", trigger_event_id, i);
            assert!(
                outs.iter().any(|e| e.event_id == want),
                "output index {i} present in log",
            );
        }
    }

    #[tokio::test]
    async fn reactor_failure_leaves_cursor_at_first_trigger() {
        let store = Arc::new(MemoryStore::new());
        // 3 triggers; reactor fails on the 2nd.
        for _ in 0..3 {
            let t = OrderPlaced { order_id: Uuid::new_v4(), occurred_at: Utc::now() };
            append_trigger(&store, &t);
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let runner = ReactorRunner::new(
            FailsOnNth { n: 2, calls: calls.clone() },
            "r.flaky",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        );

        let result = runner.step(10).await;
        assert!(result.is_err());

        // First trigger's output was appended + cursor advanced; the
        // second trigger failed before appending, so exactly one reactor
        // output is in the log.
        let appended = EventLogBackend::read_all(store.as_ref(), LogCursor::ZERO, 20)
            .await
            .unwrap();
        let outs = appended
            .iter()
            .filter(|e| e.event_type == "shipping:shipped_notification")
            .count();
        assert_eq!(outs, 1, "only the first trigger's output was appended");

        let cursor = store.get("r.flaky").await.unwrap().unwrap();

        let events = EventLogBackend::read_all(
            store.as_ref(),
            LogCursor::ZERO,
            10,
        ).await.unwrap();
        assert_eq!(cursor, events[0].position,
                   "cursor at first trigger; second's failure rolled back");
    }

    #[tokio::test]
    async fn reactor_step_idle_when_log_caught_up() {
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
            correlation_id:  Uuid::new_v4(),
            event_type:      "other.thing".into(),
            payload:         serde_json::json!({}),
            created_at:      Utc::now(),
            category:  None,
            stream_id:    None,
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

        let outcome = runner.step(10).await.unwrap();
        assert_eq!(outcome, StepOutcome::Progressed { applied: 0 });
        assert_eq!(
            EventLogBackend::read_all(store.as_ref(), LogCursor::ZERO, 10).await.unwrap().len(),
            1,
            "only the trigger in the log — no reactor output",
        );
        assert!(store.get("r.skip").await.unwrap().is_some());
    }

    // ── terminal-failure — terminal-failure mapper after retry exhaustion ──

    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct HandlerFailed {
        consumer: String,
        attempts:   u32,
    }
    impl Event for HandlerFailed {
        const CATEGORY: &'static str = "ops";
        fn event_type(&self) -> &str { "handler_failed" }
        fn stream_id(&self) -> Uuid { Uuid::nil() }
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
        // The bug: ReactorRunner tracked attempts in an in-memory
        // HashMap. Process crash → on restart attempts reset to 0
        // → retry storm.
        //
        // After fix: attempts live in the ReactorCheckpoint via
        // `record_reactor_attempt` / `clear_reactor_attempts`. A
        // new ReactorRunner pointing at the same checkpoint store sees
        // the same count.
        let store = Arc::new(MemoryStore::new());
        let payload = OrderPlaced {
            order_id:    Uuid::new_v4(),
            occurred_at: Utc::now(),
        };
        let _ = append_trigger(&store, &payload);

        let mapper_calls = std::sync::Arc::new(AtomicUsize::new(0));
        let mk_mapper = || -> TerminalFailureMapper {
            let mapper_calls = mapper_calls.clone();
            std::sync::Arc::new(move |info: TerminalFailure| {
                mapper_calls.fetch_add(1, Ordering::SeqCst);
                Some(Box::new(HandlerFailed {
                    consumer: info.consumer,
                    attempts:   info.attempts,
                }) as Box<dyn crate::engine::ErasedFact>)
            })
        };

        // Runner A: fail twice. Attempts tracked in the checkpoint store.
        let runner_a = ReactorRunner::new(
            AlwaysFails(std::sync::Arc::new(AtomicUsize::new(0))),
            "persist-attempts",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        ).with_terminal_failure(mk_mapper(), 3);
        assert!(runner_a.step(10).await.is_err());
        assert!(runner_a.step(10).await.is_err());
        assert_eq!(mapper_calls.load(Ordering::SeqCst), 0,
                   "no terminal-failure mapper yet (2 attempts < 3)");

        // Drop runner_a — simulates engine restart.
        drop(runner_a);

        // Runner B: same backend, same consumer_id, fresh in-memory
        // state. With persistent attempts, one more failure triggers
        // the mapper.
        let runner_b = ReactorRunner::new(
            AlwaysFails(std::sync::Arc::new(AtomicUsize::new(0))),
            "persist-attempts",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        ).with_terminal_failure(mk_mapper(), 3);

        let outcome = runner_b.step(10).await.unwrap();
        assert_eq!(outcome, StepOutcome::Progressed { applied: 1 });
        assert_eq!(mapper_calls.load(Ordering::SeqCst), 1,
                   "third attempt across runners triggers mapper");
    }

    #[tokio::test]
    async fn prefix_colliding_category_does_not_reach_the_reactor() {
        // B1 regression: the trigger filter must be colon-aware. With a
        // bare starts_with, a reactor on category "order" matched
        // "orders:created" — the foreign payload then hit the trigger
        // deserializer and either wedged the consumer or, with a
        // permissive serde shape, silently invoked react() on the
        // wrong event.
        let store = Arc::new(MemoryStore::new());
        let foreign = EventData {
            event_id:       Uuid::new_v4(),
            causation_id:   None,
            correlation_id: Uuid::new_v4(),
            // Same prefix bytes as "order", different category.
            event_type:     "orders:created".into(),
            payload:        serde_json::json!({ "name": "not an OrderPlaced" }),
            created_at:     Utc::now(),
            category:       Some("orders".into()),
            stream_id:      Some(Uuid::new_v4()),
            metadata:       serde_json::Map::new(),
            ephemeral:      None,
            persistent:     true,
        };
        crate::append_event(store.as_ref(), foreign).await.unwrap();

        struct PanicsIfCalled;
        #[async_trait]
        impl Reactor for PanicsIfCalled {
            type Trigger = OrderPlaced; // CATEGORY = "order"
            const NAME: &'static str = "no-collision";
            async fn react(&self, _t: &OrderPlaced, _: Ctx<'_>) -> Result<Events> {
                panic!("reactor must never see a foreign-category event");
            }
        }

        let runner = ReactorRunner::new(
            PanicsIfCalled,
            "r.no-collision",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        );

        let outcome = runner.step(10).await.unwrap();
        assert_eq!(outcome, StepOutcome::Progressed { applied: 0 },
                   "foreign category skipped, cursor advanced, no deser, no wedge");
        assert!(store.get("r.no-collision").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn poison_trigger_parks_as_terminal_failure_and_does_not_wedge() {
        // B4: a payload that can't deserialize into the reactor's Trigger
        // is a poison pill — deterministic, so retrying never helps. With
        // a terminal-failure mapper it must route to the failure store immediately and advance
        // the cursor (not block forever before the attempt counter even
        // increments, the pre-B4 bug). A following valid trigger must
        // still process.
        let store = Arc::new(MemoryStore::new());

        // A malformed event in the reactor's trigger category ("order")
        // whose payload is NOT a valid OrderPlaced.
        let poison = EventData {
            event_id:       Uuid::new_v4(),
            causation_id:   None,
            correlation_id: Uuid::new_v4(),
            event_type:     "order:placed".into(),
            payload:        serde_json::json!({ "not": "an order" }),
            created_at:     Utc::now(),
            category:       Some("order".into()),
            stream_id:      Some(Uuid::new_v4()),
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

        // A reactor that would PANIC if it ever saw a (successfully
        // deserialized) trigger — proving the poison never reaches react().
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
        .with_terminal_failure(mapper, 3);

        let outcome = runner.step(10).await.unwrap();
        assert_eq!(outcome, StepOutcome::Progressed { applied: 1 });
        assert_eq!(mapper_calls.load(Ordering::SeqCst), 1,
                   "poison parked as a terminal failure immediately — no retries (deterministic)");
        assert!(store.get("poison.park").await.unwrap().is_some(),
                "cursor advanced past the poison event — not wedged");
    }

    #[tokio::test]
    async fn poison_trigger_without_terminal_failure_propagates() {
        // Without a terminal-failure mapper, a poison trigger propagates (block-until-
        // fixed) rather than silently skipping — the same contract a
        // react() error has without a mapper.
        let store = Arc::new(MemoryStore::new());
        let poison = EventData {
            event_id:       Uuid::new_v4(),
            causation_id:   None,
            correlation_id: Uuid::new_v4(),
            event_type:     "order:placed".into(),
            payload:        serde_json::json!({ "not": "an order" }),
            created_at:     Utc::now(),
            category:       Some("order".into()),
            stream_id:      Some(Uuid::new_v4()),
            metadata:       serde_json::Map::new(),
            ephemeral:      None,
            persistent:     true,
        };
        crate::append_event(store.as_ref(), poison).await.unwrap();

        struct NeverReacts;
        #[async_trait]
        impl Reactor for NeverReacts {
            type Trigger = OrderPlaced;
            const NAME: &'static str = "poison.block";
            async fn react(&self, _t: &OrderPlaced, _: Ctx<'_>) -> Result<Events> {
                Ok(Events::new())
            }
        }

        let runner = ReactorRunner::new(
            NeverReacts,
            "poison.block",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        );
        assert!(runner.step(10).await.is_err(), "poison propagates without a terminal-failure");
        assert!(store.get("poison.block").await.unwrap().is_none(),
                "cursor did NOT advance — block until fixed");
    }

    #[tokio::test]
    async fn terminal_failure_advance_keeps_aggregate_fold() {
        // A2: "fold tracks the log, not body success." When a trigger
        // dead-letters, the cursor advances past it — and the fold MUST
        // stay applied, or the registry is permanently missing one fold
        // relative to its cursor (the pre-A2 corruption: restore-then-
        // advance left state diverging from fold(log[..cursor]) forever,
        // and snapshots persisted the divergence durably).
        #[derive(Default, Clone, Debug, Serialize, Deserialize)]
        struct OrderCount { n: u32 }
        impl crate::aggregate::Aggregate for OrderCount {
            const NAME: &'static str = "OrderCount";
        }
        impl crate::aggregate::Apply<OrderPlaced> for OrderCount {
            fn apply(&mut self, _: &OrderPlaced) { self.n += 1; }
        }

        let store = Arc::new(MemoryStore::new());
        let payload = OrderPlaced {
            order_id:    Uuid::new_v4(),
            occurred_at: Utc::now(),
        };
        let order_id = payload.order_id;
        let _ = append_trigger(&store, &payload);

        let mut reg = crate::aggregator::AggregatorRegistry::new();
        reg.register(crate::aggregator::Aggregator::for_type::<OrderCount, OrderPlaced>());
        let reg = Arc::new(reg);

        let mapper: TerminalFailureMapper = std::sync::Arc::new(
            |_info: TerminalFailure| -> Option<Box<dyn crate::engine::ErasedFact>> { None },
        );
        let runner = ReactorRunner::new(
            AlwaysFails(std::sync::Arc::new(AtomicUsize::new(0))),
            "park-keeps-fold",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        )
        .with_aggregators(reg.clone())
        .with_terminal_failure(mapper, 1);

        // max_attempts = 1 → the first failure dead-letters and advances.
        let outcome = runner.step(10).await.unwrap();
        assert_eq!(outcome, StepOutcome::Progressed { applied: 1 });
        assert!(store.get("park-keeps-fold").await.unwrap().is_some(),
                "cursor advanced past the dead-lettered trigger");

        let (_, curr) = reg.get_transition_arc::<OrderCount>(order_id);
        assert_eq!(curr.n, 1,
                   "the dead-lettered trigger's fold is KEPT — state reflects \
                    the log even when the body failed terminally");

        // The consumer is not wedged: a second trigger dead-letters the
        // same way and ALSO folds.
        let payload2 = OrderPlaced { order_id: Uuid::new_v4(), occurred_at: Utc::now() };
        let order_id2 = payload2.order_id;
        let _ = append_trigger(&store, &payload2);
        let outcome = runner.step(10).await.unwrap();
        assert_eq!(outcome, StepOutcome::Progressed { applied: 1 });
        let (_, curr) = reg.get_transition_arc::<OrderCount>(order_id2);
        assert_eq!(curr.n, 1);
    }

    #[tokio::test]
    async fn reactor_step_invokes_failure_mapper_after_retry_exhaustion() {
        let store = Arc::new(MemoryStore::new());
        let payload = OrderPlaced {
            order_id:    Uuid::new_v4(),
            occurred_at: Utc::now(),
        };
        let trigger_id = append_trigger(&store, &payload);

        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let mapper_calls = std::sync::Arc::new(AtomicUsize::new(0));
        let mapper_calls_c = mapper_calls.clone();
        // Capture the correlation_id the mapper is handed, so we can assert the
        // mapper can see the failing trigger's run (per-run terminal-failure keying).
        let seen_corr = std::sync::Arc::new(std::sync::Mutex::new(None::<Uuid>));
        let seen_corr_c = seen_corr.clone();

        let mapper: TerminalFailureMapper = std::sync::Arc::new(move |info: TerminalFailure| {
            mapper_calls_c.fetch_add(1, Ordering::SeqCst);
            *seen_corr_c.lock().unwrap() = Some(info.correlation_id);
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
        ).with_terminal_failure(mapper, 3);

        // Attempt 1: fails, no terminal-failure mapper yet (1 < 3).
        assert!(runner.step(10).await.is_err());
        assert_eq!(mapper_calls.load(Ordering::SeqCst), 0);

        // Attempt 2: fails, no terminal-failure mapper yet (2 < 3).
        assert!(runner.step(10).await.is_err());
        assert_eq!(mapper_calls.load(Ordering::SeqCst), 0);

        // Attempt 3: fails, terminal-failure mapper fires (3 >= 3). terminal-failure fact
        // appended; cursor advances.
        let outcome = runner.step(10).await.unwrap();
        assert_eq!(outcome, StepOutcome::Progressed { applied: 1 });
        assert_eq!(mapper_calls.load(Ordering::SeqCst), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 3);

        // The terminal-failure-synthesized HandlerFailed Event is appended directly to
        // the log (its own `ops` stream), with the deterministic u32::MAX
        // id that distinguishes it from react() outputs.
        let all = EventLogBackend::read_all(store.as_ref(), LogCursor::ZERO, 10)
            .await
            .unwrap();
        let terminal_failure = all
            .iter()
            .find(|e| e.event_type == "ops:handler_failed")
            .expect("terminal-failure-synthesized fact in log");
        assert_eq!(terminal_failure.causation_id, Some(trigger_id));
        assert_eq!(
            terminal_failure.event_id,
            derive_output_event_id("always-fails", trigger_id, u32::MAX),
            "terminal-failure output uses u32::MAX for its deterministic id",
        );

        // The terminal-failure fact inherits the trigger's correlation_id — without
        // this, a failing reactor's downstream "HandlerFailed" would be
        // untraceable back to the trigger (the causal-chain debugging
        // story rootsignal depends on).
        let trigger = all
            .iter()
            .find(|e| e.event_id == trigger_id)
            .expect("trigger in log");
        assert_eq!(
            terminal_failure.correlation_id, trigger.correlation_id,
            "terminal-failure-synthesized fact MUST inherit trigger correlation_id",
        );

        // TerminalFailure exposes that same correlation_id to the mapper, so a mapper
        // can key its terminal-failure event per-run (rootsignal's use case).
        let seen = seen_corr.lock().unwrap().expect("mapper ran");
        assert_eq!(
            seen, trigger.correlation_id,
            "TerminalFailure.correlation_id MUST be the failing trigger's run",
        );

        // Cursor is past the failing trigger — the runner is done with
        // it. A further step only sees the non-matching terminal-failure output (a
        // different category) and produces no new reaction.
        let next = runner.step(10).await.unwrap();
        assert!(
            matches!(next, StepOutcome::Idle | StepOutcome::Progressed { applied: 0 }),
            "no further reaction on the failed trigger; got {next:?}",
        );
    }

    #[tokio::test]
    async fn reactor_step_failure_mapper_returning_none_still_advances_cursor() {
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
        ).with_terminal_failure(mapper, 2);

        // 1st fails, 2nd hits max + maps to None: cursor advances,
        // nothing appended.
        let _ = runner.step(10).await;
        let outcome = runner.step(10).await.unwrap();
        assert_eq!(outcome, StepOutcome::Progressed { applied: 1 });
        assert_eq!(
            EventLogBackend::read_all(store.as_ref(), LogCursor::ZERO, 10).await.unwrap().len(),
            1,
            "only the trigger in the log — no reactor output",
        );
    }

    #[tokio::test]
    async fn reactor_step_without_failure_mapper_returns_err_indefinitely() {
        let store = Arc::new(MemoryStore::new());
        let payload = OrderPlaced {
            order_id:    Uuid::new_v4(),
            occurred_at: Utc::now(),
        };
        let _ = append_trigger(&store, &payload);

        let runner = ReactorRunner::new(
            AlwaysFails(std::sync::Arc::new(AtomicUsize::new(0))),
            "no-terminal_failure",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        );

        // Many attempts, all Err — without a terminal-failure mapper, no cap on retries.
        for _ in 0..10 {
            assert!(runner.step(10).await.is_err());
        }
        // Cursor stayed at zero.
        let cursor = store.get("no-terminal_failure").await.unwrap();
        assert!(cursor.is_none() || cursor == Some(LogCursor::ZERO));
    }
}
