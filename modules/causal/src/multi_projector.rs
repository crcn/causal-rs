//! `MultiProjector` — declared-subscription cross-domain event consumer.
//!
//! Single-Event subscription is covered by [`crate::Projector`]; the
//! middle ground (multiple declared categories, body wants raw
//! `&RecordedEvent` access for cross-domain payload routing) is
//! `MultiProjector`. Common consumers: graph projector, search
//! index, audit log, activity stream.
//!
//! Body shape: `&RecordedEvent` plus `Ctx`. The runner filters to
//! events whose `event_type` matches `format!("{CATEGORY}:*")` for any
//! `CATEGORY` in `KINDS` before invoking the body, so the
//! consumer never sees events outside its declared subscription.
//!
//! Backend mapping:
//! - Polling backends (Postgres, MemoryStore): runner reads via
//!   `EventLogBackend::read_all` and applies the category-list filter
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
use crate::clock::{Clock, SystemClock};
use crate::consumer_lease::{ConsumerLeasor, LeaseGuard};
use crate::contexts::Ctx;
use crate::engine::{WorkflowHighWater, DEFAULT_MAX_ATTEMPTS};
use crate::event_log::EventLogBackend;
use crate::failure::{bounded_error_chain, classify_structural, ErrorClass};
use crate::projection_failure::{FailureDecision, FailureState};
use crate::projection_runner::StepOutcome;
use crate::reactor::RetryPolicy;
use crate::types::{LogCursor, RecordedEvent};

/// Cross-domain projection consumer with a declared subscription set.
///
/// Use when:
/// - Body needs raw `&RecordedEvent` (heterogeneous payload routing
///   inside the body, no single typed enum captures all consumed
///   events), AND
/// - Subscription is bounded to a known set of `CATEGORY` values
///   (not "every event").
///
/// If you have exactly one Event type, use [`crate::Projector`]
/// instead — it deserializes the payload for you. If you genuinely
/// need every event in the log regardless of type, you don't have a
/// declared subscription and likely shouldn't be running as a
/// Kurrent-compatible consumer.
///
/// Idempotent on `event.event_id` per C8 — at-least-once delivery.
#[async_trait]
pub trait MultiProjector: Send + Sync {
    /// Persistent-subscription group name. See
    /// [`crate::Projector::NAME`] for the full uniqueness
    /// contract (in-builder enforcement, cross-engine caveat).
    const NAME: &'static str;

    /// Declared subscription. Non-empty. Each entry is a bare
    /// `Event::NAME` value (e.g. `"thing_happened"`). The
    /// runner matches events whose `event_type` starts with
    /// `format!("{CATEGORY}:")`. Compile-time const so the runner can
    /// plan subscriptions at registration time without needing an
    /// instance.
    ///
    /// Runtime panic at runner construction if empty (Rust's stable
    /// const generics can't yet express `where N >= 1`; this is the
    /// strongest enforcement available).
    const KINDS: &'static [&'static str];

    /// Cross-consumer dependency declaration (per C2b). Default empty.
    const DEPENDS_ON: &'static [&'static str] = &[];

    /// Apply an event to external state. The body deserializes the
    /// payload into whichever typed enum matches `event.event_type`,
    /// or routes by string. MUST be idempotent on `event.event_id`.
    async fn project(
        &self,
        event: &RecordedEvent,
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
    observer:     Option<Arc<dyn crate::reactor_observer::ReactorObserver>>,
    /// Optional exclusive-lease provider — see
    /// [`ProjectionRunner`](crate::projection_runner::ProjectionRunner).
    leasor:       Option<Arc<dyn ConsumerLeasor>>,
    lease:        parking_lot::Mutex<Option<Box<dyn LeaseGuard>>>,
    leased:       OnceCell<()>,
    /// Poison-park failure policy + retry accounting (mirrors the reactor
    /// taxonomy) — a deterministically-failing fold/body parks and advances.
    failure:      FailureState,
}

impl<P: MultiProjector> MultiProjectorRunner<P> {
    /// # Panics
    /// Panics if `P::KINDS` is empty. An empty subscription
    /// declaration is a programmer error — use [`crate::Projector`]
    /// for single-Event consumers, or reconsider whether you need a
    /// consumer at all.
    pub fn new(
        projector: P,
        consumer_id: impl Into<String>,
        log: Arc<dyn EventLogBackend>,
        checkpoint: Arc<dyn CheckpointStore>,
    ) -> Self {
        assert!(
            !P::KINDS.is_empty(),
            "MultiProjector::KINDS must be non-empty. \
             For single-Event consumers, use the typed `Projector` \
             trait instead."
        );
        Self {
            projector,
            consumer_id: consumer_id.into(),
            log,
            checkpoint,
            aggregators: None,
            hydrated: OnceCell::new(),
            observer: None,
            leasor: None,
            lease: parking_lot::Mutex::new(None),
            leased: OnceCell::new(),
            failure: FailureState::new(
                RetryPolicy::from_max_attempts(DEFAULT_MAX_ATTEMPTS),
                Arc::new(SystemClock),
            ),
        }
    }

    /// Attach an exclusive-lease provider. Before the runner's first
    /// `step`, it acquires `leasor.acquire(consumer_id)` — blocking until
    /// any current holder releases or crashes. See
    /// [`ProjectionRunner::with_consumer_leasor`](crate::projection_runner::ProjectionRunner::with_consumer_leasor).
    pub fn with_consumer_leasor(mut self, leasor: Arc<dyn ConsumerLeasor>) -> Self {
        self.leasor = Some(leasor);
        self
    }

    /// Override the retry policy (bounded attempts before parking
    /// domain/unclassified failures). Defaults to [`DEFAULT_MAX_ATTEMPTS`].
    pub(crate) fn with_retry_policy(mut self, p: RetryPolicy) -> Self {
        self.failure.retry_policy = p;
        self
    }

    /// Override the clock used to stamp parked terminal facts (test clocks).
    pub(crate) fn with_clock(mut self, c: Arc<dyn Clock>) -> Self {
        self.failure.clock = c;
        self
    }

    /// Wire the engine's settle high-water tracker so a parked
    /// `causal:projection_failed` fact is accounted for by `settle`.
    pub(crate) fn with_settle_tracker(mut self, t: WorkflowHighWater) -> Self {
        self.failure.settle_tracker = Some(t);
        self
    }

    /// Acquire the exclusive consumer lease once, before the first cursor
    /// read. No-op when no leasor is configured.
    async fn ensure_leased(&self) -> Result<()> {
        let Some(leasor) = self.leasor.as_ref() else {
            return Ok(());
        };
        self.leased
            .get_or_try_init(|| async {
                let guard = leasor.acquire(&self.consumer_id).await?;
                *self.lease.lock() = Some(guard);
                Ok::<(), anyhow::Error>(())
            })
            .await?;
        Ok(())
    }

    /// Attach a per-runner [`AggregatorRegistry`] copy. See
    /// [`crate::projection_runner::ProjectionRunner::with_aggregators`]
    /// for semantics.
    pub fn with_aggregators(mut self, aggregators: Arc<AggregatorRegistry>) -> Self {
        self.aggregators = Some(aggregators);
        self
    }

    /// Attach a [`ReactorObserver`](crate::reactor_observer::ReactorObserver)
    /// for inspector / telemetry capture.
    pub fn with_observer(
        mut self,
        observer: Arc<dyn crate::reactor_observer::ReactorObserver>,
    ) -> Self {
        self.observer = Some(observer);
        self
    }

    pub fn consumer_id(&self) -> &str { &self.consumer_id }

    /// This consumer's durable cursor — its settle progress (serial
    /// consumers finish each event before advancing it).
    pub(crate) async fn cursor(&self) -> Result<Option<crate::types::LogCursor>> {
        self.checkpoint.get(&self.consumer_id).await
    }

    pub async fn step(&self, batch: usize) -> Result<StepOutcome> {
        // Acquire the exclusive lease before reading the cursor (see
        // ProjectionRunner::step).
        self.ensure_leased().await?;

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

        let events = self.log.read_all(cursor, batch).await?;
        if events.is_empty() {
            return Ok(StepOutcome::Idle);
        }

        let mut applied = 0usize;
        for event in events {
            // Fold every event into the aggregator registry regardless
            // of subscription match, so aggregators that span categories
            // still see all events. Folds are idempotent on the event's
            // stream coordinates — a failing body retried by the
            // supervisor re-delivers harmlessly. Mirrors ProjectionRunner.
            if let Some(reg) = self.aggregators.as_ref() {
                match crate::aggregator::fold_event(
                    reg,
                    None,
                    self.log.as_ref(),
                    &event.event_type,
                    &event.payload,
                    event.subject_id,
                    &event.category,
                    event.revision,
                    event.position,
                    /* strict_to_event = */ true,
                )
                .await
                {
                    Ok(outcome) => {
                        if outcome.applied {
                            if let Some(obs) = self.observer.as_ref() {
                                reg.notify_observer(
                                    &outcome.snapshots,
                                    obs.as_ref(),
                                    event.workflow_id,
                                    event.position,
                                    event.event_id,
                                );
                            }
                        }
                    }
                    Err(e) => {
                        // Deterministic fold failure (poison payload) parks +
                        // advances; transient / under-budget propagates to retry.
                        self.park_or_propagate(&event, e).await?;
                        continue;
                    }
                }
            }

            // Subscription filter: skip + advance for events whose
            // category doesn't match any declared CATEGORY. Body never
            // sees them.
            let matches_subscription = P::KINDS
                .iter()
                .any(|k| is_subscribed_kind(&event.event_type, k));
            if !matches_subscription {
                self.checkpoint.advance(&self.consumer_id, event.position).await?;
                continue;
            }

            let occurred_at = event.created_at;
            let ctx = Ctx {
                event_id:       event.event_id,
                log_position:   event.position,
                occurred_at,
                workflow_id: event.workflow_id,
                metadata:       &event.metadata,
                consumer: &self.consumer_id,
                labels:   None,
                state:    match self.aggregators.as_ref() {
                    Some(reg) => crate::contexts::StateSource::Registry(reg),
                    None => crate::contexts::StateSource::None,
                },
                logs:           None,
                effect_store: None,
                cancelled_workflows: None,
            };
            match self.projector.project(&event, ctx).await {
                Ok(()) => {
                    self.failure.clear(event.event_id);
                    self.checkpoint.advance(&self.consumer_id, event.position).await?;
                    applied += 1;
                }
                Err(e) => {
                    // The fold above is NOT rolled back — registry state
                    // reflects the log regardless of body success. A
                    // deterministic (poison) body failure parks + advances;
                    // transient / under-budget propagates to retry.
                    self.park_or_propagate(&event, e).await?;
                    continue;
                }
            }
        }

        Ok(StepOutcome::Progressed { applied })
    }

    /// On a per-event failure: **park** (append `causal:projection_failed` +
    /// advance — returns `Ok`, caller `continue`s) or **propagate** (retry).
    /// Mirrors [`ProjectionRunner::park_or_propagate`](crate::projection_runner).
    async fn park_or_propagate(&self, event: &RecordedEvent, err: anyhow::Error) -> Result<()> {
        match self.failure.on_failure(event.event_id, &err) {
            FailureDecision::Park { class, attempts } => {
                self.failure
                    .park(
                        self.log.as_ref(),
                        &self.consumer_id,
                        event,
                        class,
                        attempts,
                        bounded_error_chain(&err),
                        self.observer.as_deref(),
                    )
                    .await?;
                self.checkpoint
                    .advance(&self.consumer_id, event.position)
                    .await?;
                tracing::warn!(
                    consumer = %self.consumer_id,
                    event_id = %event.event_id,
                    class = %class,
                    attempts,
                    "multi-projector parked a deterministically-failing event and advanced",
                );
                Ok(())
            }
            FailureDecision::Retry => Err(err),
        }
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
                    if let Err(e) = crate::aggregator::fold_event(
                        reg,
                        None,
                        self.log.as_ref(),
                        &event.event_type,
                        &event.payload,
                        event.subject_id,
                        &event.category,
                        event.revision,
                        event.position,
                        /* strict_to_event = */ true,
                    )
                    .await
                    {
                        // A deterministic fold error at position <= cursor means
                        // the event was parked in a prior life — skip it rather
                        // than re-wedge on every boot. Backend errors propagate.
                        if classify_structural(&e) == Some(ErrorClass::Poison) {
                            tracing::warn!(
                                consumer = %self.consumer_id,
                                event_id = %event.event_id,
                                error = %bounded_error_chain(&e),
                                "skipping a previously-parked poison event during hydration",
                            );
                            continue;
                        }
                        return Err(e);
                    }
                }
                if hit_cursor || last_pos >= cursor { break; }
                from = last_pos;
            }
            Ok(())
        }).await?;
        Ok(())
    }
}

/// True iff `event_type` IS one of the subscribed kinds — exact
/// equality (flat routing, 0.10 chunk 7c). The kinds list is a
/// settle-latency surface: subscribe to what you render, no more.
fn is_subscribed_kind(event_type: &str, kind: &str) -> bool {
    crate::event_type::matches_kind(event_type, kind)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_log::EventLogBackend;
    use crate::memory_store::MemoryStore;
    use crate::types::EventData;
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
        const NAME: &'static str = "audit-trail";
        const KINDS: &'static [&'static str] = &["thing_happened", "tag_assigned"];

        async fn project(
            &self,
            event: &RecordedEvent,
            _ctx: Ctx<'_>,
        ) -> Result<()> {
            self.seen.lock().push((event.event_id, event.event_type.clone()));
            Ok(())
        }
    }

    async fn append_event(store: &MemoryStore, event_type: &str) -> Uuid {
        let event_id = Uuid::new_v4();
        let ev = EventData {
            event_id,
            causation_id:       None,
            workflow_id:  Uuid::new_v4(),
            event_type:      event_type.into(),
            payload:         serde_json::json!({"event_type": event_type}),
            created_at:      Utc::now(),
            category:  None,
            subject_id:    None,
            metadata:        serde_json::Map::new(),
            ephemeral:       None,
            persistent:      true,
        };
        crate::append_event(store, ev).await.unwrap();
        event_id
    }

    #[tokio::test]
    async fn delivers_only_events_matching_declared_categories() {
        let store = Arc::new(MemoryStore::new());
        let id_world  = append_event(&store, "thing_happened").await;
        let _ignored1 = append_event(&store, "source_seen").await;
        let id_system = append_event(&store, "tag_assigned").await;
        let _ignored2 = append_event(&store, "schedule_created").await;

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
        assert_eq!(s[0].1, "thing_happened");
        assert_eq!(s[1].0, id_system);
        assert_eq!(s[1].1, "tag_assigned");
    }

    #[tokio::test]
    async fn cursor_advances_past_filtered_events() {
        let store = Arc::new(MemoryStore::new());
        append_event(&store, "source_seen").await;          // skip (not subscribed)
        append_event(&store, "thing_happened").await;       // deliver
        append_event(&store, "schedule_created").await;     // skip (not subscribed)

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
        let last_pos = EventLogBackend::read_all(
            store.as_ref(), LogCursor::ZERO, 10,
        ).await.unwrap()
            .last()
            .map(|e| e.position)
            .unwrap();
        assert_eq!(cursor, last_pos);
        assert_eq!(seen.lock().len(), 1, "only the subscribed kind delivered");
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
            const NAME: &'static str = "downstream";
            const KINDS: &'static [&'static str] = &["world_event"];
            const DEPENDS_ON: &'static [&'static str] = &["upstream"];
            async fn project(
                &self,
                event: &RecordedEvent,
                _ctx: Ctx<'_>,
            ) -> Result<()> {
                self.seen.lock().push(event.event_id);
                Ok(())
            }
        }

        let store = Arc::new(MemoryStore::new());
        for _ in 0..2 {
            append_event(&store, "world_event").await;
        }
        let last_pos = EventLogBackend::read_all(
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
    #[should_panic(expected = "KINDS must be non-empty")]
    async fn empty_categories_panics_at_construction() {
        struct BadlyDeclared;
        #[async_trait]
        impl MultiProjector for BadlyDeclared {
            const NAME: &'static str = "badly-declared";
            const KINDS: &'static [&'static str] = &[];
            async fn project(
                &self,
                _event: &RecordedEvent,
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

    // ── H1: poison-park for the multi-projector ───────────────────────
    async fn append_thing(store: &MemoryStore, subject: Uuid, payload: serde_json::Value) -> Uuid {
        let event_id = Uuid::new_v4();
        crate::append_event(store, EventData {
            event_id,
            causation_id: None,
            workflow_id: Uuid::new_v4(),
            event_type: "thing_happened".into(),
            payload,
            created_at: Utc::now(),
            category: Some("thing_happened".into()),
            subject_id: Some(subject),
            metadata: serde_json::Map::new(),
            ephemeral: None,
            persistent: true,
        })
        .await
        .unwrap();
        event_id
    }

    #[tokio::test]
    async fn multi_projector_poison_body_parks_and_advances() {
        /// Poisons any event whose payload carries `{"poison": true}`.
        struct PoisonOnFlag {
            seen: Arc<Mutex<Vec<Uuid>>>,
        }
        #[async_trait]
        impl MultiProjector for PoisonOnFlag {
            const NAME: &'static str = "poison-on-flag";
            const KINDS: &'static [&'static str] = &["thing_happened"];
            async fn project(&self, event: &RecordedEvent, _ctx: Ctx<'_>) -> Result<()> {
                if event.payload.get("poison").is_some() {
                    return Err(crate::failure::poison(anyhow::anyhow!("poisoned event")));
                }
                self.seen.lock().push(event.event_id);
                Ok(())
            }
        }

        let store = Arc::new(MemoryStore::new());
        let poison_subject = Uuid::new_v4();
        let poison_id = append_thing(&store, poison_subject, serde_json::json!({"poison": true})).await;
        let good_id = append_thing(&store, Uuid::new_v4(), serde_json::json!({"ok": true})).await;

        let proj = PoisonOnFlag { seen: Arc::new(Mutex::new(Vec::new())) };
        let seen = proj.seen.clone();
        let runner = MultiProjectorRunner::new(
            proj,
            "mp.poison",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
        );

        let outcome = runner.step(10).await.unwrap();
        assert!(matches!(outcome, StepOutcome::Progressed { applied: 1 }));
        assert_eq!(seen.lock().as_slice(), &[good_id], "healthy event delivered");

        let all = EventLogBackend::read_all(store.as_ref(), LogCursor::ZERO, 20).await.unwrap();
        let parked = all.iter()
            .find(|e| e.event_type == crate::projection_failure::PROJECTION_FAILED_KIND)
            .expect("built-in projection_failed fact for the poison body");
        assert_eq!(parked.payload["class"], "poison");
        assert_eq!(parked.payload["event_id"], poison_id.to_string());
        assert_eq!(parked.subject_id, poison_subject);
    }
}
