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
//!      kept replayable by the side-effect `ReactionCache`.
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
use crate::engine::{DlqInfo, DlqMapperArc};
use crate::event_log::EventLogBackend;
use crate::event::Event;
use crate::projection_runner::StepOutcome;
use crate::reactor_observer::ReactorObserver;
use crate::reactor::Reactor;
use crate::types::{EventData, LogCursor, StreamState};

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
    /// DLQ mapper for terminal-failure handling. When `react()`
    /// errors `max_attempts` times on the same trigger, the mapper
    /// is invoked; if it returns `Some(fact)`, the synthesized
    /// fact is appended directly to its stream and the cursor advances
    /// past the failing event.
    dlq_mapper:  Option<DlqMapperArc>,
    /// Retry budget — applies only when `dlq_mapper` is set. Without
    /// a mapper, reactors retry indefinitely (supervisor backoff).
    max_attempts: u32,
    /// Inspector / telemetry hook. Default `None` = zero overhead.
    observer:    Option<Arc<dyn ReactorObserver>>,
    /// Reaction-result cache (Phase 4). Surfaced to the reactor body via
    /// `ctx.reaction_cache()` so side-effecting reactors memoize their
    /// external call under the reaction key — safe under retry/redelivery.
    reaction_cache: Option<Arc<dyn crate::reaction_cache::ReactionCache>>,
    /// Engine-level aggregator registry. Reactor outputs are folded into
    /// it after they're appended, so `engine.snapshot::<A>(id)` reflects
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
            dlq_mapper: None,
            max_attempts: 0,
            observer: None,
            reaction_cache: None,
            engine_aggregators: None,
            settle_tracker: None,
            snapshot_store: None,
            snapshot_every: 0,
        }
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
    /// `ctx.reaction_cache()`.
    pub fn with_reaction_cache(
        mut self,
        cache: Arc<dyn crate::reaction_cache::ReactionCache>,
    ) -> Self {
        self.reaction_cache = Some(cache);
        self
    }

    /// Attach the engine-level aggregator registry so reactor outputs
    /// fold into it after append (keeps `engine.snapshot` current).
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

    /// Configure terminal-failure DLQ handling. After `max_attempts`
    /// consecutive `react()` errors on the same trigger, the mapper
    /// is invoked; on `Some(fact)`, the fact is appended directly to its
    /// stream and the cursor advances past the failing event.
    /// Without this, errored reactors retry indefinitely.
    pub(crate) fn with_dlq(mut self, mapper: DlqMapperArc, max_attempts: u32) -> Self {
        self.dlq_mapper = Some(mapper);
        self.max_attempts = max_attempts;
        self
    }

    pub fn consumer_id(&self) -> &str { &self.consumer_id }

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
            // Fold every event into the aggregator registry, with
            // capture/restore around the reactor call to avoid double-
            // application on retry. Mirrors legacy engine semantics.
            // (Per-consumer state is rebuilt from genesis by `ensure_hydrated`
            // on restart, so no read-through restore is needed here.)
            let rollback = self.aggregators.as_ref().map(|reg| {
                let r = reg.capture_for_rollback(&event.event_type, &event.payload);
                let snapshots = reg.apply_event(&event.event_type, &event.payload);
                if let Some(obs) = self.observer.as_ref() {
                    reg.notify_observer(
                        &snapshots,
                        obs.as_ref(),
                        event.correlation_id,
                        event.position,
                        event.event_id,
                    );
                }
                r
            });

            if !event.event_type.starts_with(prefix) {
                // Non-matching trigger: just advance the cursor.
                self.checkpoint.set(&self.consumer_id, event.position).await?;
                continue;
            }

            let trigger: R::Trigger = serde_json::from_value(event.payload.clone())?;

            // ── Telemetry: record this attempt's start. `attempt_seq`
            //    is the persistent-store counter — pre-incremented to
            //    treat this very attempt as attempt #(prev+1).
            let attempt_seq = self.checkpoint
                .record_reactor_attempt(&self.consumer_id, event.event_id)
                .await?;
            let started_at = chrono::Utc::now();
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
                reaction_cache: self.reaction_cache.as_ref(),
            };

            // ── Decision. On Err, cursor stays where it was; no rows
            //    persisted. The whole batch from this trigger forward
            //    is retried on next step.
            //
            //    UNLESS a DLQ mapper is configured AND attempts have
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
                    if let (Some(reg), Some(r)) = (self.aggregators.as_ref(), rollback) {
                        reg.restore_state(r);
                    }

                    // attempt counter already incremented for this run;
                    // use attempt_seq for the cap check.
                    let attempts = attempt_seq;

                    if let Some(mapper) = self.dlq_mapper.as_ref() {
                        if attempts >= self.max_attempts {
                            if let Some(obs) = self.observer.as_ref() {
                                obs.reactor_dlq(
                                    event.event_id,
                                    &self.consumer_id,
                                    event.correlation_id,
                                    attempts,
                                    &format!("{:#}", e),
                                    completed_at,
                                );
                            }
                            let info = DlqInfo {
                                group_name:        self.consumer_id.clone(),
                                source_event_id:   event.event_id,
                                source_event_type: event.event_type.clone(),
                                error:             format!("{:#}", e),
                                attempts,
                                correlation_id:    event.correlation_id,
                            };
                            let mapped = mapper(info);
                            self.checkpoint
                                .clear_reactor_attempts(&self.consumer_id, event.event_id)
                                .await?;

                            // DLQ-synthesized output (if any) is appended
                            // directly to its own stream; then the cursor
                            // advances past the failing event.
                            // `output_index = u32::MAX` keeps its
                            // deterministic id distinct from react() outputs.
                            if let Some(fact) = mapped {
                                // `cat` is the STREAM placement category;
                                // `event_type` keeps the routing category.
                                let cat = fact.stream_category().to_string();
                                let sid = fact.stream_id();
                                let event_type =
                                    format!("{}:{}", fact.category(), fact.variant_name());
                                let payload = fact.to_value()?;
                                let out_event = EventData {
                                    event_id: derive_output_event_id(
                                        &self.consumer_id, event.event_id, u32::MAX,
                                    ),
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
                                let write = self.log
                                    .append_to_stream(&cat, sid, StreamState::Any, vec![out_event])
                                    .await?;
                                if let Some(tracker) = &self.settle_tracker {
                                    tracker.lock().unwrap().bump(event.correlation_id, write.position);
                                }
                                if let Some(reg) = &self.engine_aggregators {
                                    reg.apply_event(&event_type, &payload);
                                }
                            }
                            self.checkpoint.set(&self.consumer_id, event.position).await?;
                            applied += 1;
                            continue;
                        }
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

            // ── Append each output directly to its own stream with a
            //    deterministic event_id (idempotent under redelivery via
            //    the log's append-dedup, C1), then advance the cursor.
            //    At-least-once + idempotent emit: a crash between append
            //    and cursor-advance re-runs react() on restart; the
            //    re-appends dedup on event_id.
            for (idx, out) in emitted.iter().enumerate() {
                // Restore the engine-level aggregate(s) for this output from
                // durable storage BEFORE appending — so the fold below builds on
                // full prior history and does not double-count the output we are
                // about to append. (Per-consumer registries are handled by
                // `ensure_hydrated`; this is the shared engine registry only.)
                if let Some(reg) = &self.engine_aggregators {
                    if self.snapshot_store.is_some() {
                        crate::aggregator::restore_aggregates_for_event(
                            reg.as_ref(),
                            self.snapshot_store.as_deref(),
                            self.log.as_ref(),
                            &out.durable_name,
                            &out.payload,
                        )
                        .await?;
                    }
                }
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
                if let Some(reg) = &self.engine_aggregators {
                    let snapshots = reg.apply_event(&out.durable_name, &out.payload);
                    if let Some(store) = self.snapshot_store.as_ref() {
                        crate::aggregator::maybe_save_snapshots(
                            reg.as_ref(),
                            store.as_ref(),
                            self.snapshot_every,
                            &snapshots,
                        )
                        .await;
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
                    reg.apply_event(&event.event_type, &event.payload);
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
        const GROUP_NAME: &'static str = "emit-one";
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
        const GROUP_NAME: &'static str = "emit-n";
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
        const GROUP_NAME: &'static str = "fails-on-nth";
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
            const GROUP_NAME: &'static str = "silent";
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

    // ── DLQ — terminal-failure mapper after retry exhaustion ──

    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct HandlerFailed {
        group_name: String,
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
        const GROUP_NAME: &'static str = "always-fails";
        async fn react(&self, _t: &OrderPlaced, _: Ctx<'_>) -> Result<Events> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(anyhow!("boom"))
        }
    }

    #[tokio::test]
    async fn dlq_attempt_count_persists_across_runner_instances() {
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

        let dlq_calls = std::sync::Arc::new(AtomicUsize::new(0));
        let mk_mapper = || -> DlqMapperArc {
            let dlq_calls = dlq_calls.clone();
            std::sync::Arc::new(move |info: DlqInfo| {
                dlq_calls.fetch_add(1, Ordering::SeqCst);
                Some(Box::new(HandlerFailed {
                    group_name: info.group_name,
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
        ).with_dlq(mk_mapper(), 3);
        assert!(runner_a.step(10).await.is_err());
        assert!(runner_a.step(10).await.is_err());
        assert_eq!(dlq_calls.load(Ordering::SeqCst), 0,
                   "no DLQ yet (2 attempts < 3)");

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
        ).with_dlq(mk_mapper(), 3);

        let outcome = runner_b.step(10).await.unwrap();
        assert_eq!(outcome, StepOutcome::Progressed { applied: 1 });
        assert_eq!(dlq_calls.load(Ordering::SeqCst), 1,
                   "third attempt across runners triggers mapper");
    }

    #[tokio::test]
    async fn reactor_step_invokes_dlq_mapper_after_retry_exhaustion() {
        let store = Arc::new(MemoryStore::new());
        let payload = OrderPlaced {
            order_id:    Uuid::new_v4(),
            occurred_at: Utc::now(),
        };
        let trigger_id = append_trigger(&store, &payload);

        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let dlq_calls = std::sync::Arc::new(AtomicUsize::new(0));
        let dlq_calls_c = dlq_calls.clone();
        // Capture the correlation_id the mapper is handed, so we can assert the
        // mapper can see the failing trigger's run (per-run DLQ keying).
        let seen_corr = std::sync::Arc::new(std::sync::Mutex::new(None::<Uuid>));
        let seen_corr_c = seen_corr.clone();

        let mapper: DlqMapperArc = std::sync::Arc::new(move |info: DlqInfo| {
            dlq_calls_c.fetch_add(1, Ordering::SeqCst);
            *seen_corr_c.lock().unwrap() = Some(info.correlation_id);
            Some(Box::new(HandlerFailed {
                group_name: info.group_name,
                attempts:   info.attempts,
            }) as Box<dyn crate::engine::ErasedFact>)
        });

        let runner = ReactorRunner::new(
            AlwaysFails(calls.clone()),
            "always-fails",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        ).with_dlq(mapper, 3);

        // Attempt 1: fails, no DLQ yet (1 < 3).
        assert!(runner.step(10).await.is_err());
        assert_eq!(dlq_calls.load(Ordering::SeqCst), 0);

        // Attempt 2: fails, no DLQ yet (2 < 3).
        assert!(runner.step(10).await.is_err());
        assert_eq!(dlq_calls.load(Ordering::SeqCst), 0);

        // Attempt 3: fails, DLQ mapper fires (3 >= 3). DLQ fact
        // appended; cursor advances.
        let outcome = runner.step(10).await.unwrap();
        assert_eq!(outcome, StepOutcome::Progressed { applied: 1 });
        assert_eq!(dlq_calls.load(Ordering::SeqCst), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 3);

        // The DLQ-synthesized HandlerFailed Event is appended directly to
        // the log (its own `ops` stream), with the deterministic u32::MAX
        // id that distinguishes it from react() outputs.
        let all = EventLogBackend::read_all(store.as_ref(), LogCursor::ZERO, 10)
            .await
            .unwrap();
        let dlq = all
            .iter()
            .find(|e| e.event_type == "ops:handler_failed")
            .expect("DLQ-synthesized fact in log");
        assert_eq!(dlq.causation_id, Some(trigger_id));
        assert_eq!(
            dlq.event_id,
            derive_output_event_id("always-fails", trigger_id, u32::MAX),
            "DLQ output uses u32::MAX for its deterministic id",
        );

        // The DLQ fact inherits the trigger's correlation_id — without
        // this, a failing reactor's downstream "HandlerFailed" would be
        // untraceable back to the trigger (the causal-chain debugging
        // story rootsignal depends on).
        let trigger = all
            .iter()
            .find(|e| e.event_id == trigger_id)
            .expect("trigger in log");
        assert_eq!(
            dlq.correlation_id, trigger.correlation_id,
            "DLQ-synthesized fact MUST inherit trigger correlation_id",
        );

        // DlqInfo exposes that same correlation_id to the mapper, so a mapper
        // can key its terminal-failure event per-run (rootsignal's use case).
        let seen = seen_corr.lock().unwrap().expect("mapper ran");
        assert_eq!(
            seen, trigger.correlation_id,
            "DlqInfo.correlation_id MUST be the failing trigger's run",
        );

        // Cursor is past the failing trigger — the runner is done with
        // it. A further step only sees the non-matching DLQ output (a
        // different category) and produces no new reaction.
        let next = runner.step(10).await.unwrap();
        assert!(
            matches!(next, StepOutcome::Idle | StepOutcome::Progressed { applied: 0 }),
            "no further reaction on the failed trigger; got {next:?}",
        );
    }

    #[tokio::test]
    async fn reactor_step_dlq_mapper_returning_none_still_advances_cursor() {
        let store = Arc::new(MemoryStore::new());
        let payload = OrderPlaced {
            order_id:    Uuid::new_v4(),
            occurred_at: Utc::now(),
        };
        let _ = append_trigger(&store, &payload);

        let mapper: DlqMapperArc = std::sync::Arc::new(move |_info| None);

        let runner = ReactorRunner::new(
            AlwaysFails(std::sync::Arc::new(AtomicUsize::new(0))),
            "drop-on-dlq",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        ).with_dlq(mapper, 2);

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
    async fn reactor_step_without_dlq_mapper_returns_err_indefinitely() {
        let store = Arc::new(MemoryStore::new());
        let payload = OrderPlaced {
            order_id:    Uuid::new_v4(),
            occurred_at: Utc::now(),
        };
        let _ = append_trigger(&store, &payload);

        let runner = ReactorRunner::new(
            AlwaysFails(std::sync::Arc::new(AtomicUsize::new(0))),
            "no-dlq",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        );

        // Many attempts, all Err — without DLQ, no cap on retries.
        for _ in 0..10 {
            assert!(runner.step(10).await.is_err());
        }
        // Cursor stayed at zero.
        let cursor = store.get("no-dlq").await.unwrap();
        assert!(cursor.is_none() || cursor == Some(LogCursor::ZERO));
    }
}
