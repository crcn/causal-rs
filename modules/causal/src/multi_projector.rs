//! `MultiProjector` — declared-subscription cross-domain event consumer.
//!
//! Single-Fact subscription is covered by [`crate::Projector`]; the
//! middle ground (multiple declared categories, body wants raw
//! `&PersistedEvent` access for cross-domain payload routing) is
//! `MultiProjector`. Common consumers: graph projector, search
//! index, audit log, activity stream.
//!
//! Body shape: `&PersistedEvent` plus `Ctx`. The runner filters to
//! events whose `event_type` matches `format!("{CATEGORY}:*")` for any
//! `CATEGORY` in `CATEGORIES` before invoking the body, so the
//! consumer never sees events outside its declared subscription.
//!
//! Backend mapping:
//! - Polling backends (Postgres, MemoryStore): runner reads via
//!   `EventLogBackend::load_from` and applies the category-list filter
//!   client-side. Same query shape as the typed runner.
//! - KurrentDB (future): runner subscribes to `$et-{CATEGORY}:*` per
//!   listed category and merges by commit position. Native subscription
//!   primitive — no `$all` permission escalation.
//!
//! Same C2 (per-event cursor advance on Ok), C2b (`DEPENDS_ON` fence),
//! C8 (caller idempotency on `event.event_id`) semantics as
//! `ProjectionRunner<P>`.

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

/// Cross-domain projection consumer with a declared subscription set.
///
/// Use when:
/// - Body needs raw `&PersistedEvent` (heterogeneous payload routing
///   inside the body, no single typed enum captures all consumed
///   events), AND
/// - Subscription is bounded to a known set of `CATEGORY` values
///   (not "every event").
///
/// If you have exactly one Fact type, use [`crate::Projector`]
/// instead — it deserializes the payload for you. If you genuinely
/// need every event in the log regardless of type, you don't have a
/// declared subscription and likely shouldn't be running as a
/// Kurrent-compatible consumer.
///
/// Idempotent on `event.event_id` per C8 — at-least-once delivery.
#[async_trait]
pub trait MultiProjector: Send + Sync {
    /// Declared subscription. Non-empty. Each entry is a bare
    /// `Fact::CATEGORY` value (e.g. `"world"`, `"discovery"`). The
    /// runner matches events whose `event_type` starts with
    /// `format!("{CATEGORY}:")`. Compile-time const so the runner can
    /// plan subscriptions at registration time without needing an
    /// instance.
    ///
    /// Runtime panic at runner construction if empty (Rust's stable
    /// const generics can't yet express `where N >= 1`; this is the
    /// strongest enforcement available).
    const CATEGORIES: &'static [&'static str];

    /// Cross-consumer dependency declaration (per C2b). Default empty.
    const DEPENDS_ON: &'static [&'static str] = &[];

    /// Apply an event to external state. The body deserializes the
    /// payload into whichever typed enum matches `event.event_type`,
    /// or routes by string. MUST be idempotent on `event.event_id`.
    async fn project(
        &self,
        event: &PersistedEvent,
        ctx: Ctx<'_>,
    ) -> Result<()>;
}

pub struct MultiProjectorRunner<P: MultiProjector> {
    projector:    P,
    consumer_id:  String,
    log:          Arc<dyn EventLogBackend>,
    checkpoint:   Arc<dyn CheckpointStore>,
    aggregators:  Option<Arc<AggregatorRegistry>>,
    hydrated:     OnceCell<()>,
}

impl<P: MultiProjector> MultiProjectorRunner<P> {
    /// # Panics
    /// Panics if `P::CATEGORIES` is empty. An empty subscription
    /// declaration is a programmer error — use [`crate::Projector`]
    /// for single-Fact consumers, or reconsider whether you need a
    /// consumer at all.
    pub fn new(
        projector: P,
        consumer_id: impl Into<String>,
        log: Arc<dyn EventLogBackend>,
        checkpoint: Arc<dyn CheckpointStore>,
    ) -> Self {
        assert!(
            !P::CATEGORIES.is_empty(),
            "MultiProjector::CATEGORIES must be non-empty. \
             For single-Fact consumers, use the typed `Projector` \
             trait instead."
        );
        Self {
            projector,
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

        for dep in P::DEPENDS_ON {
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
            // Fold every event into the aggregator registry (capture/
            // restore around the body call) regardless of subscription
            // match, so aggregators that span categories still see all
            // events. Mirrors ProjectionRunner.
            let rollback = self.aggregators.as_ref().map(|reg| {
                let r = reg.capture_for_rollback(&event.event_type, &event.payload);
                reg.apply_event(&event.event_type, &event.payload);
                r
            });

            // Subscription filter: skip + advance for events whose
            // category doesn't match any declared CATEGORY. Body never
            // sees them.
            let matches_subscription = P::CATEGORIES
                .iter()
                .any(|c| starts_with_category(&event.event_type, c));
            if !matches_subscription {
                self.checkpoint.set(&self.consumer_id, event.position).await?;
                continue;
            }

            let occurred_at = event.created_at;
            let ctx = Ctx {
                event_id:       event.event_id,
                log_position:   event.position,
                occurred_at,
                correlation_id: event.correlation_id,
                metadata:       &event.metadata,
                aggregators:    self.aggregators.as_ref(),
            };
            match self.projector.project(&event, ctx).await {
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

/// True iff `event_type` matches `format!("{category}:*")`. Avoids
/// the false-positive that plain `starts_with` has when one category
/// is a prefix of another (e.g. `"world"` matching `"worldwide:..."`).
fn starts_with_category(event_type: &str, category: &str) -> bool {
    event_type.len() > category.len()
        && event_type.as_bytes()[category.len()] == b':'
        && event_type.starts_with(category)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_log::EventLogBackend;
    use crate::memory_store::MemoryStore;
    use crate::types::NewEvent;
    use chrono::Utc;
    use parking_lot::Mutex;
    use uuid::Uuid;

    /// Test projector that records every event_id + event_type the
    /// runner delivers (i.e., post-filter — only subscribed events).
    #[derive(Default, Clone)]
    struct AuditTrail {
        seen: Arc<Mutex<Vec<(Uuid, String)>>>,
    }

    impl AuditTrail {
        fn new() -> Self { Self::default() }
    }

    #[async_trait]
    impl MultiProjector for AuditTrail {
        const CATEGORIES: &'static [&'static str] = &["world", "system"];

        async fn project(
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
    async fn delivers_only_events_matching_declared_categories() {
        let store = Arc::new(MemoryStore::new());
        let id_world  = append_event(&store, "world:thing_happened").await;
        let _ignored1 = append_event(&store, "discovery:source_seen").await;
        let id_system = append_event(&store, "system:tag_assigned").await;
        let _ignored2 = append_event(&store, "scheduling:schedule_created").await;

        let trail = AuditTrail::new();
        let seen = trail.seen.clone();
        let runner = MultiProjectorRunner::new(
            trail,
            "graph",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
        );

        let outcome = runner.step(10).await.unwrap();
        assert!(matches!(outcome, StepOutcome::Progressed { applied: 2 }),
                "applied count counts only delivered events, not skipped ones");

        let s = seen.lock();
        assert_eq!(s.len(), 2, "body invoked exactly for matched events");
        assert_eq!(s[0].0, id_world);
        assert_eq!(s[0].1, "world:thing_happened");
        assert_eq!(s[1].0, id_system);
        assert_eq!(s[1].1, "system:tag_assigned");
    }

    #[tokio::test]
    async fn cursor_advances_past_filtered_events() {
        let store = Arc::new(MemoryStore::new());
        append_event(&store, "discovery:a").await;          // skip
        append_event(&store, "world:b").await;              // deliver
        append_event(&store, "scheduling:c").await;         // skip

        let trail = AuditTrail::new();
        let seen = trail.seen.clone();
        let runner = MultiProjectorRunner::new(
            trail,
            "graph",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
        );

        runner.step(10).await.unwrap();

        let cursor = store.get("graph").await.unwrap().unwrap();
        let last_pos = EventLogBackend::load_from(
            store.as_ref(), LogCursor::ZERO, 10,
        ).await.unwrap()
            .last()
            .map(|e| e.position)
            .unwrap();
        assert_eq!(cursor, last_pos);
        assert_eq!(seen.lock().len(), 1, "only the world: event delivered");
    }

    #[tokio::test]
    async fn idle_when_caught_up_with_no_matching_events_pending() {
        let store = Arc::new(MemoryStore::new());
        let runner = MultiProjectorRunner::new(
            AuditTrail::new(),
            "graph",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
        );
        let outcome = runner.step(10).await.unwrap();
        assert_eq!(outcome, StepOutcome::Idle);
    }

    #[tokio::test]
    async fn dep_fence_holds() {
        struct DepM { seen: Arc<Mutex<Vec<Uuid>>> }
        #[async_trait]
        impl MultiProjector for DepM {
            const CATEGORIES: &'static [&'static str] = &["world"];
            const DEPENDS_ON: &'static [&'static str] = &["upstream"];
            async fn project(
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
            append_event(&store, "world:event").await;
        }
        let last_pos = EventLogBackend::load_from(
            store.as_ref(), LogCursor::ZERO, 10,
        ).await.unwrap()[0].position;
        store.set("downstream", last_pos).await.unwrap();

        let runner = MultiProjectorRunner::new(
            DepM { seen: Arc::new(Mutex::new(Vec::new())) },
            "downstream",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
        );

        let outcome = runner.step(10).await.unwrap();
        assert!(matches!(outcome, StepOutcome::WaitOnDep { ref dep, .. } if dep == "upstream"));
    }

    #[tokio::test]
    #[should_panic(expected = "CATEGORIES must be non-empty")]
    async fn empty_categories_panics_at_construction() {
        struct BadlyDeclared;
        #[async_trait]
        impl MultiProjector for BadlyDeclared {
            const CATEGORIES: &'static [&'static str] = &[];
            async fn project(
                &self,
                _event: &PersistedEvent,
                _ctx: Ctx<'_>,
            ) -> Result<()> { Ok(()) }
        }

        let store = Arc::new(MemoryStore::new());
        let _runner = MultiProjectorRunner::new(
            BadlyDeclared,
            "bad",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
        );
    }
}
