//! Runner for v0.3 [`Reactor`](crate::reactor_v3::Reactor) consumers.
//!
//! Implements C12 (runtime-side outbox + atomic batch commit) end-to-end:
//!
//!   1. Read trigger fact from log (load_from polling, like Phase 4b).
//!   2. Filter by `R::Trigger::type_prefix()`.
//!   3. Call `reactor.react(trigger, ctx)` → `Result<Events>`.
//!   4. Convert `Events::outputs` → `Vec<OutboxRow>` with deterministic
//!      `event_id = uuid_v5(NS_REACTOR_OUTPUT, [reactor_id, trigger_id, idx])`.
//!   5. `commit_reactor_batch(rows, Some((reactor_id, trigger.position)))`
//!      atomically commits BOTH the outbox rows AND the cursor advance.
//!   6. Idempotency property: a runtime crash before step 5 redoes the
//!      whole batch from scratch on restart (no mixed-batch); a crash
//!      after step 5 leaves the relay to drain.
//!
//! On `react()` Err: cursor unchanged, no rows written, function
//! propagates the error. Outer supervisor decides retry timing.
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
use crate::checkpoint_store::{InsertableOutboxRow, ReactorOutbox};
use crate::contexts::Ctx;
use crate::engine_v3::{DlqInfo, DlqMapperArc};
use crate::event_log::EventLogBackend;
use crate::fact::Fact;
use crate::projection_runner::StepOutcome;
use crate::reactor_v3::Reactor;
use crate::types::LogCursor;

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

pub struct ReactorRunner<R: Reactor> {
    reactor:     R,
    consumer_id: String,
    log:         Arc<dyn EventLogBackend>,
    outbox:      Arc<dyn ReactorOutbox>,
    aggregators: Option<Arc<AggregatorRegistry>>,
    hydrated:    OnceCell<()>,
    /// DLQ mapper for terminal-failure handling. When `react()`
    /// errors `max_attempts` times on the same trigger, the mapper
    /// is invoked; if it returns `Some(fact)`, the synthesized
    /// fact is emitted through the outbox and the cursor advances
    /// past the failing event.
    dlq_mapper:  Option<DlqMapperArc>,
    /// Retry budget — applies only when `dlq_mapper` is set. Without
    /// a mapper, reactors retry indefinitely (supervisor backoff).
    max_attempts: u32,
}

impl<R: Reactor> ReactorRunner<R>
where
    R::Trigger: DeserializeOwned,
{
    pub fn new(
        reactor: R,
        consumer_id: impl Into<String>,
        log: Arc<dyn EventLogBackend>,
        outbox: Arc<dyn ReactorOutbox>,
    ) -> Self {
        Self {
            reactor,
            consumer_id: consumer_id.into(),
            log,
            outbox,
            aggregators: None,
            hydrated: OnceCell::new(),
            dlq_mapper: None,
            max_attempts: 0,
        }
    }

    /// Attach a per-runner [`AggregatorRegistry`] copy. See
    /// [`crate::projection_runner::ProjectionRunner::with_aggregators`]
    /// for semantics.
    pub fn with_aggregators(mut self, aggregators: Arc<AggregatorRegistry>) -> Self {
        self.aggregators = Some(aggregators);
        self
    }

    /// Configure terminal-failure DLQ handling. After `max_attempts`
    /// consecutive `react()` errors on the same trigger, the mapper
    /// is invoked; on `Some(fact)`, the fact is emitted through the
    /// outbox and the cursor advances past the failing event.
    /// Without this, errored reactors retry indefinitely.
    pub fn with_dlq(mut self, mapper: DlqMapperArc, max_attempts: u32) -> Self {
        self.dlq_mapper = Some(mapper);
        self.max_attempts = max_attempts;
        self
    }

    pub fn consumer_id(&self) -> &str { &self.consumer_id }

    pub async fn step(&self, batch: usize) -> Result<StepOutcome> {
        let cursor = self.outbox.get(&self.consumer_id).await?
            .unwrap_or(LogCursor::ZERO);

        self.ensure_hydrated(cursor).await?;

        let events = self.log.load_from(cursor, batch).await?;
        if events.is_empty() {
            return Ok(StepOutcome::Idle);
        }

        let prefix = <R::Trigger as Fact>::CATEGORY;
        let mut applied = 0usize;
        for event in events {
            // Fold every event into the aggregator registry, with
            // capture/restore around the reactor call to avoid double-
            // application on retry. Mirrors legacy engine semantics.
            let rollback = self.aggregators.as_ref().map(|reg| {
                let r = reg.capture_for_rollback(&event.event_type, &event.payload);
                reg.apply_event(&event.event_type, &event.payload);
                r
            });

            if !event.event_type.starts_with(prefix) {
                // Non-matching trigger: advance cursor (no react call,
                // no outbox rows). Atomic batch with empty rows.
                self.outbox.commit_reactor_batch(
                    Vec::new(),
                    Some((self.consumer_id.clone(), event.position)),
                ).await?;
                continue;
            }

            let trigger: R::Trigger = serde_json::from_value(event.payload.clone())?;
            let ctx = Ctx {
                event_id:       event.event_id,
                log_position:   event.position,
                occurred_at:    trigger.occurred_at().unwrap_or(event.created_at),
                correlation_id: event.correlation_id,
                metadata:       &event.metadata,
                aggregators:    self.aggregators.as_ref(),
            };

            // ── Decision. On Err, cursor stays where it was; no rows
            //    persisted. The whole batch from this trigger forward
            //    is retried on next step.
            //
            //    UNLESS a DLQ mapper is configured AND attempts have
            //    reached max — then synthesize the mapper's fact,
            //    commit it through the outbox, advance the cursor past
            //    the failing event, and clear the attempt counter.
            let emitted = match self.reactor.react(&trigger, ctx).await {
                Ok(events) => {
                    // Clear persisted attempt count on success so a
                    // future failure starts fresh.
                    self.outbox
                        .clear_reactor_attempts(&self.consumer_id, event.event_id)
                        .await?;
                    events
                }
                Err(e) => {
                    if let (Some(reg), Some(r)) = (self.aggregators.as_ref(), rollback) {
                        reg.restore_state(r);
                    }

                    // Increment via the persistent store. Survives
                    // ReactorRunner reconstruction; durable backends
                    // (Postgres, etc.) survive process crashes too.
                    let attempts = self.outbox
                        .record_reactor_attempt(&self.consumer_id, event.event_id)
                        .await?;

                    if let Some(mapper) = self.dlq_mapper.as_ref() {
                        if attempts >= self.max_attempts {
                            let info = DlqInfo {
                                group_name:        self.consumer_id.clone(),
                                source_event_id:   event.event_id,
                                source_event_type: event.event_type.clone(),
                                error:             format!("{:#}", e),
                                attempts,
                            };
                            let mapped = mapper(info);
                            self.outbox
                                .clear_reactor_attempts(&self.consumer_id, event.event_id)
                                .await?;

                            // Build outbox row (if mapper returned a
                            // Fact) and atomically commit + advance.
                            // `output_index = u32::MAX` distinguishes
                            // DLQ-synthesized outputs from normal
                            // react() outputs (idx 0..N).
                            let mut rows: Vec<InsertableOutboxRow> = Vec::new();
                            if let Some(fact) = mapped {
                                rows.push(InsertableOutboxRow {
                                    reactor_id:      self.consumer_id.clone(),
                                    source_event_id: event.event_id,
                                    output_index:    u32::MAX,
                                    event_id:        derive_output_event_id(
                                        &self.consumer_id,
                                        event.event_id,
                                        u32::MAX,
                                    ),
                                    event_type: format!(
                                        "{}:{}",
                                        fact.category(),
                                        fact.variant_name(),
                                    ),
                                    fact_payload: fact.to_value()?,
                                    correlation_id: event.correlation_id,
                                });
                            }

                            self.outbox.commit_reactor_batch(
                                rows,
                                Some((self.consumer_id.clone(), event.position)),
                            ).await?;
                            applied += 1;
                            continue;
                        }
                    }
                    return Err(e);
                }
            };

            // ── Convert Events → Vec<InsertableOutboxRow> with
            //    deterministic ids. Backend assigns id + created_at.
            let rows: Vec<InsertableOutboxRow> = emitted.iter().enumerate().map(|(idx, out)| {
                let idx = idx as u32;
                InsertableOutboxRow {
                    reactor_id:      self.consumer_id.clone(),
                    source_event_id: event.event_id,
                    output_index:    idx,
                    event_id:        derive_output_event_id(
                        &self.consumer_id,
                        event.event_id,
                        idx,
                    ),
                    event_type:      out.durable_name.clone(),
                    fact_payload:    out.payload.clone(),
                    correlation_id:  event.correlation_id,
                }
            }).collect();

            // ── C12 atomic commit: rows + cursor in one transaction.
            self.outbox.commit_reactor_batch(
                rows,
                Some((self.consumer_id.clone(), event.position)),
            ).await?;

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
                let batch = self.log.load_from(from, 1024).await?;
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
    use crate::reactor_v3::Events;
    use crate::types::NewEvent;
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
    impl Fact for OrderPlaced {
        const CATEGORY: &'static str = "order";
        fn name(&self) -> &str { "order_placed" }
        fn stream_id(&self) -> Uuid { self.order_id }
        fn occurred_at(&self) -> Option<DateTime<Utc>> { Some(self.occurred_at) }
    }

    // ── Output fact ──
    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct ShippedNotification {
        order_id: Uuid,
    }
    impl Fact for ShippedNotification {
        const CATEGORY: &'static str = "shipping";
        fn name(&self) -> &str { "shipped_notification" }
        fn stream_id(&self) -> Uuid { self.order_id }
    }

    fn append_trigger(store: &MemoryStore, payload: &OrderPlaced) -> Uuid {
        let event_id = Uuid::new_v4();
        let ev = NewEvent {
            event_id,
            parent_id:       None,
            correlation_id:  Uuid::new_v4(),
            event_type:      format!("{}:{}", <OrderPlaced as Fact>::CATEGORY, payload.name()),
            payload:         serde_json::to_value(payload).unwrap(),
            created_at:      Utc::now(),
            aggregate_type:  None,
            aggregate_id:    None,
            metadata:        serde_json::Map::new(),
            ephemeral:       None,
            persistent:      true,
        };
        let store_ref: &MemoryStore = store;
        let fut = EventLogBackend::append(store_ref, ev);
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
    async fn reactor_step_emits_outbox_row_and_advances_cursor_atomically() {
        let store = Arc::new(MemoryStore::new());
        let trigger = OrderPlaced { order_id: Uuid::new_v4(), occurred_at: Utc::now() };
        let trigger_event_id = append_trigger(&store, &trigger);

        let runner = ReactorRunner::new(
            EmitOne,
            "r.shipper",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn ReactorOutbox>,
        );

        let outcome = runner.step(10).await.unwrap();
        assert!(matches!(outcome, StepOutcome::Progressed { applied: 1 }));

        // Cursor advanced
        assert!(store.get("r.shipper").await.unwrap().is_some());

        // One outbox row
        let pending = store.outbox_pending(10).await.unwrap();
        assert_eq!(pending.len(), 1);

        let row = &pending[0];
        assert_eq!(row.reactor_id, "r.shipper");
        assert_eq!(row.source_event_id, trigger_event_id);
        assert_eq!(row.output_index, 0);
        assert_eq!(row.event_type, "shipping:shipped_notification");
        // Deterministic id matches the helper's output.
        assert_eq!(
            row.event_id,
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
        let persisted = EventLogBackend::load_from(
            store.as_ref(), LogCursor::ZERO, 10,
        ).await.unwrap();
        let trigger_correlation = persisted[0].correlation_id;

        let runner = ReactorRunner::new(
            EmitOne,
            "r.trace",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn ReactorOutbox>,
        );
        runner.step(10).await.unwrap();

        // Outbox row carries trigger's correlation_id.
        let row = &store.outbox_pending(10).await.unwrap()[0];
        assert_eq!(row.correlation_id, trigger_correlation,
                   "outbox row MUST carry trigger's correlation_id");
        assert_eq!(row.source_event_id, trigger_event_id,
                   "outbox row source_event_id MUST be the trigger's event_id");

        // After relay drains, the persisted output also carries it.
        let relay = crate::relay::RelayLoop::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn ReactorOutbox>,
        );
        relay.drain_once(10).await.unwrap();

        let all_events = EventLogBackend::load_from(
            store.as_ref(), LogCursor::ZERO, 10,
        ).await.unwrap();
        let output_event = all_events.iter()
            .find(|e| e.event_type == "shipping:shipped_notification")
            .expect("relay-drained output present in log");
        assert_eq!(output_event.correlation_id, trigger_correlation,
                   "relay-drained output MUST carry trigger's correlation_id");
        assert_eq!(output_event.parent_id, Some(trigger_event_id),
                   "relay-drained output's parent_id MUST be the trigger's event_id");
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
            store.clone() as Arc<dyn ReactorOutbox>,
        );

        runner.step(10).await.unwrap();
        assert!(store.get("r.silent").await.unwrap().is_some());
        assert!(store.outbox_pending(10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn reactor_step_with_n_outputs_writes_n_rows_in_order() {
        let store = Arc::new(MemoryStore::new());
        let trigger = OrderPlaced { order_id: Uuid::new_v4(), occurred_at: Utc::now() };
        let trigger_event_id = append_trigger(&store, &trigger);

        let runner = ReactorRunner::new(
            EmitN(5),
            "r.fanout",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn ReactorOutbox>,
        );

        runner.step(10).await.unwrap();

        let rows = store.outbox_pending(20).await.unwrap();
        assert_eq!(rows.len(), 5);

        // Output indexes 0..5 in order; deterministic event_ids per index.
        for (i, row) in rows.iter().enumerate() {
            assert_eq!(row.output_index, i as u32);
            assert_eq!(
                row.event_id,
                derive_output_event_id("r.fanout", trigger_event_id, i as u32),
            );
        }
    }

    #[tokio::test]
    async fn reactor_failure_leaves_cursor_and_outbox_untouched() {
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
            store.clone() as Arc<dyn ReactorOutbox>,
        );

        let result = runner.step(10).await;
        assert!(result.is_err());

        // Cursor advanced past the FIRST trigger only (idx 1 errored
        // before commit). Outbox has the first trigger's output.
        let rows = store.outbox_pending(20).await.unwrap();
        assert_eq!(rows.len(), 1, "only the first trigger's output committed");

        let cursor = store.get("r.flaky").await.unwrap().unwrap();

        let events = EventLogBackend::load_from(
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
            store.clone() as Arc<dyn ReactorOutbox>,
        );
        let outcome = runner.step(10).await.unwrap();
        assert_eq!(outcome, StepOutcome::Idle);
    }

    #[tokio::test]
    async fn reactor_skips_non_matching_trigger_type() {
        let store = Arc::new(MemoryStore::new());
        let foreign = NewEvent {
            event_id:        Uuid::new_v4(),
            parent_id:       None,
            correlation_id:  Uuid::new_v4(),
            event_type:      "other.thing".into(),
            payload:         serde_json::json!({}),
            created_at:      Utc::now(),
            aggregate_type:  None,
            aggregate_id:    None,
            metadata:        serde_json::Map::new(),
            ephemeral:       None,
            persistent:      true,
        };
        EventLogBackend::append(store.as_ref(), foreign).await.unwrap();

        let runner = ReactorRunner::new(
            EmitOne,
            "r.skip",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn ReactorOutbox>,
        );

        let outcome = runner.step(10).await.unwrap();
        assert_eq!(outcome, StepOutcome::Progressed { applied: 0 });
        assert!(store.outbox_pending(10).await.unwrap().is_empty());
        assert!(store.get("r.skip").await.unwrap().is_some());
    }

    // ── DLQ — terminal-failure mapper after retry exhaustion ──

    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct HandlerFailed {
        group_name: String,
        attempts:   u32,
    }
    impl Fact for HandlerFailed {
        const CATEGORY: &'static str = "ops";
        fn name(&self) -> &str { "handler_failed" }
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
        // After fix: attempts live in the ReactorOutbox via
        // `record_reactor_attempt` / `clear_reactor_attempts`. A
        // new ReactorRunner pointing at the same outbox sees the
        // same persisted count.
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
                }) as Box<dyn crate::engine_v3::ErasedFact>)
            })
        };

        // Runner A: fail twice. Attempts persist to outbox.
        let runner_a = ReactorRunner::new(
            AlwaysFails(std::sync::Arc::new(AtomicUsize::new(0))),
            "persist-attempts",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn ReactorOutbox>,
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
            store.clone() as Arc<dyn ReactorOutbox>,
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

        let mapper: DlqMapperArc = std::sync::Arc::new(move |info: DlqInfo| {
            dlq_calls_c.fetch_add(1, Ordering::SeqCst);
            Some(Box::new(HandlerFailed {
                group_name: info.group_name,
                attempts:   info.attempts,
            }) as Box<dyn crate::engine_v3::ErasedFact>)
        });

        let runner = ReactorRunner::new(
            AlwaysFails(calls.clone()),
            "always-fails",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn ReactorOutbox>,
        ).with_dlq(mapper, 3);

        // Attempt 1: fails, no DLQ yet (1 < 3).
        assert!(runner.step(10).await.is_err());
        assert_eq!(dlq_calls.load(Ordering::SeqCst), 0);

        // Attempt 2: fails, no DLQ yet (2 < 3).
        assert!(runner.step(10).await.is_err());
        assert_eq!(dlq_calls.load(Ordering::SeqCst), 0);

        // Attempt 3: fails, DLQ mapper fires (3 >= 3). Cursor
        // advances; outbox row written.
        let outcome = runner.step(10).await.unwrap();
        assert_eq!(outcome, StepOutcome::Progressed { applied: 1 });
        assert_eq!(dlq_calls.load(Ordering::SeqCst), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 3);

        // The outbox carries the synthesized HandlerFailed Fact.
        let pending = store.outbox_pending(10).await.unwrap();
        assert_eq!(pending.len(), 1);
        let row = &pending[0];
        assert_eq!(row.source_event_id, trigger_id);
        assert_eq!(row.event_type, "ops:handler_failed");
        assert_eq!(row.output_index, u32::MAX,
                   "DLQ outputs use u32::MAX to distinguish from react() outputs");

        // Cursor is past the failing event — runner is done with it.
        let next = runner.step(10).await.unwrap();
        assert_eq!(next, StepOutcome::Idle);
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
            store.clone() as Arc<dyn ReactorOutbox>,
        ).with_dlq(mapper, 2);

        // 1st fails, 2nd hits max + maps to None: cursor advances,
        // no outbox row.
        let _ = runner.step(10).await;
        let outcome = runner.step(10).await.unwrap();
        assert_eq!(outcome, StepOutcome::Progressed { applied: 1 });
        assert!(store.outbox_pending(10).await.unwrap().is_empty());
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
            store.clone() as Arc<dyn ReactorOutbox>,
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
