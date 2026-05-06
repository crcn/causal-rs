//! `AnyMaterializer` — heterogeneous event consumer.
//!
//! v0.3's `Materializer<Fact = F>` is typed: one Fact enum per
//! materializer, with backend-side prefix filtering. Some legacy
//! consumers (notably the system_log projection and graph projector
//! in rootsignal) handle ANY event regardless of type — they pattern
//! match across the full event union.
//!
//! `AnyMaterializer` is the migration shape for those: takes
//! `PersistedEvent` directly, returns `Result<()>`. The body
//! deserializes into whatever typed enum the consumer expects, or
//! reads `event.event_type` for routing.
//!
//! Same C2 (per-fact cursor advance) and C2b (DEPENDS_ON fence)
//! semantics as `ProjectionRunner<M>`. Same C8 (caller idempotency
//! on `event.event_id`) responsibility.
//!
//! v0.3 macro work (Phase 6) and a true `Projection<D>` drop-in
//! adapter (Phase 7+) build on top of this. For now it's the lowest-
//! friction migration target for legacy consumers.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::OnceCell;

use crate::aggregator::AggregatorRegistry;
use crate::checkpoint_store::CheckpointStore;
use crate::contexts::Ctx;
use crate::event_log::EventLogBackend;
use crate::projection_runner::StepOutcome;
use crate::types::{LogCursor, PersistedEvent};

/// Heterogeneous event consumer. Sees every event in the log; no
/// type-prefix filter. Idempotent on `event.event_id` per C8.
#[deprecated(
    since = "0.3.4",
    note = "AnyMaterializer reads every event from the log without a \
            declared subscription, which doesn't compose with Kurrent's \
            stream/type subscription model. Use the typed `Materializer` \
            trait — declare your subscription via `Fact::type_prefix()`. \
            Cross-domain consumers split into one typed `Materializer` \
            per consumed enum, all delegating to a shared body."
)]
#[async_trait]
pub trait AnyMaterializer: Send + Sync {
    /// Cross-consumer dependency declaration (per C2b). Default empty.
    const DEPENDS_ON: &'static [&'static str] = &[];

    /// Apply a fact to external state. Body decides whether/how to
    /// route based on `event.event_type` or by deserializing
    /// `event.payload`. MUST be idempotent on `event.event_id`.
    async fn materialize(
        &self,
        event: &PersistedEvent,
        ctx: Ctx<'_>,
    ) -> Result<()>;
}

#[deprecated(
    since = "0.3.4",
    note = "Paired with the deprecated `AnyMaterializer` trait. See its \
            note for the migration to typed `Materializer<Fact = F>` \
            consumers."
)]
#[allow(deprecated)]
pub struct AnyMaterializerRunner<M: AnyMaterializer> {
    materializer: M,
    consumer_id:  String,
    log:          Arc<dyn EventLogBackend>,
    checkpoint:   Arc<dyn CheckpointStore>,
    aggregators:  Option<Arc<AggregatorRegistry>>,
    hydrated:     OnceCell<()>,
}

#[allow(deprecated)]
impl<M: AnyMaterializer> AnyMaterializerRunner<M> {
    pub fn new(
        materializer: M,
        consumer_id: impl Into<String>,
        log: Arc<dyn EventLogBackend>,
        checkpoint: Arc<dyn CheckpointStore>,
    ) -> Self {
        Self {
            materializer,
            consumer_id: consumer_id.into(),
            log,
            checkpoint,
            aggregators: None,
            hydrated: OnceCell::new(),
        }
    }

    /// Attach a per-runner [`AggregatorRegistry`] copy. See
    /// [`crate::projection_runner::ProjectionRunner::with_aggregators`]
    /// for semantics.
    pub fn with_aggregators(mut self, aggregators: Arc<AggregatorRegistry>) -> Self {
        self.aggregators = Some(aggregators);
        self
    }

    pub fn consumer_id(&self) -> &str { &self.consumer_id }

    pub async fn step(&self, batch: usize) -> Result<StepOutcome> {
        let cursor = self.checkpoint.get(&self.consumer_id).await?
            .unwrap_or(LogCursor::ZERO);

        for dep in M::DEPENDS_ON {
            let dep_cursor = self.checkpoint.get(dep).await?
                .unwrap_or(LogCursor::ZERO);
            if dep_cursor < cursor {
                return Ok(StepOutcome::WaitOnDep {
                    dep:        (*dep).into(),
                    cursor,
                    dep_cursor,
                });
            }
        }

        self.ensure_hydrated(cursor).await?;

        let events = self.log.load_from(cursor, batch).await?;
        if events.is_empty() {
            return Ok(StepOutcome::Idle);
        }

        let mut applied = 0usize;
        for event in events {
            let rollback = self.aggregators.as_ref().map(|reg| {
                let r = reg.capture_for_rollback(&event.event_type, &event.payload);
                reg.apply_event(&event.event_type, &event.payload);
                r
            });

            let occurred_at = event.created_at;
            let ctx = Ctx {
                event_id:       event.event_id,
                log_position:   event.position,
                occurred_at,
                correlation_id: event.correlation_id,
                metadata:       &event.metadata,
                aggregators:    self.aggregators.as_ref(),
            };
            match self.materializer.materialize(&event, ctx).await {
                Ok(()) => {
                    self.checkpoint.set(&self.consumer_id, event.position).await?;
                    applied += 1;
                }
                Err(e) => {
                    if let (Some(reg), Some(r)) = (self.aggregators.as_ref(), rollback) {
                        reg.restore_state(r);
                    }
                    return Err(e);
                }
            }
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

#[cfg(test)]
#[allow(deprecated)] // exercising the deprecated AnyMaterializer surface intentionally
mod tests {
    use super::*;
    use crate::event_log::EventLogBackend;
    use crate::memory_store::MemoryStore;
    use crate::types::NewEvent;
    use chrono::Utc;
    use parking_lot::Mutex;
    use uuid::Uuid;

    /// Test materializer that records every event_id + event_type it sees.
    #[derive(Default, Clone)]
    struct AuditTrail {
        seen: Arc<Mutex<Vec<(Uuid, String)>>>,
    }

    #[async_trait]
    impl AnyMaterializer for AuditTrail {
        async fn materialize(
            &self,
            event: &PersistedEvent,
            _ctx: Ctx<'_>,
        ) -> Result<()> {
            self.seen.lock().push((event.event_id, event.event_type.clone()));
            Ok(())
        }
    }

    async fn append_event(store: &MemoryStore, event_type: &str) -> Uuid {
        let event_id = Uuid::new_v4();
        let ev = NewEvent {
            event_id,
            parent_id:       None,
            correlation_id:  Uuid::new_v4(),
            event_type:      event_type.into(),
            payload:         serde_json::json!({"event_type": event_type}),
            created_at:      Utc::now(),
            aggregate_type:  None,
            aggregate_id:    None,
            metadata:        serde_json::Map::new(),
            ephemeral:       None,
            persistent:      true,
        };
        EventLogBackend::append(store, ev).await.unwrap();
        event_id
    }

    #[tokio::test]
    async fn any_materializer_sees_all_event_types() {
        let store = Arc::new(MemoryStore::new());
        let id_a = append_event(&store, "scheduling.schedule_created").await;
        let id_b = append_event(&store, "scrape.web_scrape_completed").await;
        let id_c = append_event(&store, "system.signal_tagged").await;

        let trail = AuditTrail::default();
        let seen_handle = trail.seen.clone();
        let runner = AnyMaterializerRunner::new(
            trail,
            "audit",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
        );

        let outcome = runner.step(10).await.unwrap();
        assert!(matches!(outcome, StepOutcome::Progressed { applied: 3 }));

        let seen = seen_handle.lock();
        assert_eq!(seen.len(), 3);
        assert_eq!(seen[0].0, id_a);
        assert_eq!(seen[0].1, "scheduling.schedule_created");
        assert_eq!(seen[1].0, id_b);
        assert_eq!(seen[2].0, id_c);
    }

    #[tokio::test]
    async fn any_materializer_advances_cursor_per_event() {
        let store = Arc::new(MemoryStore::new());
        for _ in 0..5 {
            append_event(&store, "any.event").await;
        }

        let runner = AnyMaterializerRunner::new(
            AuditTrail::default(),
            "audit",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
        );

        runner.step(10).await.unwrap();

        let cursor = store.get("audit").await.unwrap().unwrap();
        let last = EventLogBackend::load_from(
            store.as_ref(), LogCursor::ZERO, 10,
        ).await.unwrap()
            .last()
            .map(|e| e.position)
            .unwrap();
        assert_eq!(cursor, last);
    }

    #[tokio::test]
    async fn any_materializer_dep_fence_holds() {
        struct DepM { seen: Arc<Mutex<Vec<Uuid>>> }
        #[async_trait]
        impl AnyMaterializer for DepM {
            const DEPENDS_ON: &'static [&'static str] = &["upstream"];
            async fn materialize(
                &self,
                event: &PersistedEvent,
                _ctx: Ctx<'_>,
            ) -> Result<()> {
                self.seen.lock().push(event.event_id);
                Ok(())
            }
        }

        let store = Arc::new(MemoryStore::new());
        for _ in 0..2 {
            append_event(&store, "any.event").await;
        }
        // Move downstream forward; upstream stays at ZERO.
        let last_pos = EventLogBackend::load_from(
            store.as_ref(), LogCursor::ZERO, 10,
        ).await.unwrap()[0].position;
        store.set("downstream", last_pos).await.unwrap();

        let runner = AnyMaterializerRunner::new(
            DepM { seen: Arc::new(Mutex::new(Vec::new())) },
            "downstream",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
        );

        let outcome = runner.step(10).await.unwrap();
        assert!(matches!(outcome, StepOutcome::WaitOnDep { ref dep, .. } if dep == "upstream"));
    }

    #[tokio::test]
    async fn any_materializer_idle_when_caught_up() {
        let store = Arc::new(MemoryStore::new());
        let runner = AnyMaterializerRunner::new(
            AuditTrail::default(),
            "audit",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
        );
        let outcome = runner.step(10).await.unwrap();
        assert_eq!(outcome, StepOutcome::Idle);
    }
}
