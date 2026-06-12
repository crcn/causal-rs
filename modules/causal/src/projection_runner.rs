//! Runner that drives [`Projector`](crate::Projector) consumers via
//! polling on `EventLogBackend::read_all`.
//!
//! Implements:
//!   - C2 — per-fact cursor advance (cursor advances iff `apply` /
//!     `project` returned `Ok` for that specific fact).
//!   - C2b — `DEPENDS_ON` fence (refuses to advance past any dep's
//!     cursor; returns `WaitOnDep` so the supervisor task can pause
//!     this consumer until the dep catches up).
//!
//! Failure handling is `BlockUntilFixed` only (per the impl plan):
//! `project` errors stop cursor advance and propagate up; the
//! supervisor decides retry timing. `AdvanceAfter` (park-and-skip) lands
//! in a later phase together with the runner-level `RetryPolicy`.

use std::sync::Arc;

use anyhow::Result;
use serde::de::DeserializeOwned;
use tokio::sync::OnceCell;

use crate::aggregator::AggregatorRegistry;
use crate::checkpoint_store::CheckpointStore;
use crate::contexts::Ctx;
use crate::event_log::EventLogBackend;
use crate::event::Event;
use crate::projector::Projector;
use crate::reactor_observer::ReactorObserver;
use crate::types::LogCursor;

/// Outcome of a single `step()` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepOutcome {
    /// Applied N facts to the consumer.
    Progressed { applied: usize },
    /// Log returned no new facts past the cursor.
    Idle,
    /// One of `DEPENDS_ON` is behind us; this consumer waits.
    WaitOnDep {
        dep:        String,
        cursor:     LogCursor,
        dep_cursor: LogCursor,
    },
}

// ─────────────────────────────────────────────────────────────────────
// ProjectionRunner — drives a Projector
// ─────────────────────────────────────────────────────────────────────

pub struct ProjectionRunner<M: Projector> {
    projector: M,
    consumer_id:  String,
    log:          Arc<dyn EventLogBackend>,
    checkpoint:   Arc<dyn CheckpointStore>,
    aggregators:  Option<Arc<AggregatorRegistry>>,
    hydrated:     OnceCell<()>,
    observer:     Option<Arc<dyn ReactorObserver>>,
}

impl<M: Projector> ProjectionRunner<M>
where
    M::Event: DeserializeOwned,
{
    pub fn new(
        projector: M,
        consumer_id: impl Into<String>,
        log: Arc<dyn EventLogBackend>,
        checkpoint: Arc<dyn CheckpointStore>,
    ) -> Self {
        Self {
            projector,
            consumer_id: consumer_id.into(),
            log,
            checkpoint,
            aggregators: None,
            hydrated: OnceCell::new(),
            observer: None,
        }
    }

    /// Attach a per-runner [`AggregatorRegistry`] copy. The runner folds
    /// every event into this registry before invoking the consumer body
    /// so `ctx.state_of::<A>(Uuid::nil())` reads state INCLUDING the current event.
    pub fn with_aggregators(mut self, aggregators: Arc<AggregatorRegistry>) -> Self {
        self.aggregators = Some(aggregators);
        self
    }

    /// Attach a [`ReactorObserver`] for inspector / telemetry capture.
    pub fn with_observer(mut self, observer: Arc<dyn ReactorObserver>) -> Self {
        self.observer = Some(observer);
        self
    }

    pub fn consumer_id(&self) -> &str { &self.consumer_id }

    /// Process up to `batch` facts from the log.
    ///
    /// Per C2: cursor advances per-fact, only after `project`
    /// returns `Ok`. Per C2b: refuses to advance if any dep is behind.
    pub async fn step(&self, batch: usize) -> Result<StepOutcome> {
        let cursor = self.checkpoint.get(&self.consumer_id).await?
            .unwrap_or(LogCursor::ZERO);

        // Dependency fence: every dep must be at least as far as us.
        // (Equality is sufficient — we only care that dep has folded
        // every fact we already folded; we don't gate on facts dep
        // hasn't seen yet.)
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

        let events = self.log.read_all(cursor, batch).await?;
        if events.is_empty() {
            return Ok(StepOutcome::Idle);
        }

        let prefix = <M::Event as Event>::NAME;
        let mut applied = 0usize;
        for event in events {
            // Fold into the per-runner aggregator registry BEFORE
            // matching/dispatch. Folds are idempotent on the event's
            // stream coordinates, so a failing project body retried by
            // the supervisor re-delivers harmlessly — fold tracks the
            // log, not body success (replaces the old capture/restore
            // rollback discipline).
            if let Some(reg) = self.aggregators.as_ref() {
                let outcome = crate::aggregator::fold_event(
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
                .await?;
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

            // Filter by Event prefix. Non-matching events advance the
            // cursor but don't trigger project — the consumer
            // legitimately doesn't handle them.
            if !crate::event_type::matches_kind(&event.event_type, prefix) {
                self.checkpoint.set(&self.consumer_id, event.position).await?;
                continue;
            }

            let fact: M::Event = serde_json::from_value(event.payload.clone())?;
            let ctx = Ctx {
                event_id:       event.event_id,
                log_position:   event.position,
                occurred_at:    fact.occurred_at().unwrap_or(event.created_at),
                workflow_id: event.workflow_id,
                metadata:       &event.metadata,
                aggregators:    self.aggregators.as_ref(),
                logs:           None,
                effect_store: None,
            };

            match self.projector.project(&fact, ctx).await {
                Ok(()) => {
                    // C2: advance cursor ONLY after Ok.
                    self.checkpoint.set(&self.consumer_id, event.position).await?;
                    applied += 1;
                }
                Err(e) => {
                    // The fold above is NOT rolled back — registry state
                    // reflects the log regardless of body success.
                    return Err(e);
                }
            }
        }

        Ok(StepOutcome::Progressed { applied })
    }

    /// Replay log[ZERO..cursor] into the aggregator registry once per
    /// runner lifetime, so consumers picking up at a non-zero cursor
    /// see registry state that reflects every event before the cursor.
    ///
    /// **Read `docs/aggregate-state-scope.md` before assuming this
    /// path needs snapshot acceleration.** Per-run engines (cursor
    /// always ZERO) and service-level projectors (no aggregators)
    /// never reach the replay branch. The only consumer that does is
    /// a hypothetical pattern-3 long-lived aggregate, which scout/
    /// rootsignal does not currently use. Eager replay here is the
    /// correct default until pattern 3 is in scope.
    ///
    /// The cursor==ZERO short-circuit lives INSIDE the OnceCell init
    /// closure on purpose: if we returned early outside it, the cell
    /// would stay uninitialized and a later step (cursor advanced to
    /// >0 via per-event folds) would mistakenly trigger a replay,
    /// double-folding events already applied by step().
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
                        event.subject_id,
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
    use crate::memory_store::MemoryStore;
    use crate::types::EventData;
    use anyhow::anyhow;
    use async_trait::async_trait;
    use chrono::{DateTime, Utc};
    use serde::{Deserialize, Serialize};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use uuid::Uuid;

    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct Recorded {
        id:          Uuid,
        occurred_at: DateTime<Utc>,
    }

    impl Event for Recorded {
        const NAME: &'static str = "recorded";
        fn subject_id(&self) -> Uuid { self.id }
        fn occurred_at(&self) -> Option<DateTime<Utc>> { Some(self.occurred_at) }
    }

    /// Projector that records every event_id it sees in a Vec.
    struct CollectingProjector {
        seen: Arc<parking_lot::Mutex<Vec<Uuid>>>,
    }

    #[async_trait]
    impl Projector for CollectingProjector {
        type Event = Recorded;
        const NAME: &'static str = "collecting";

        async fn project(
            &self,
            _fact: &Recorded,
            ctx: Ctx<'_>,
        ) -> Result<()> {
            self.seen.lock().push(ctx.event_id);
            Ok(())
        }
    }

    /// Projector that fails on the Nth call (1-indexed).
    struct FailsOnNth {
        n:     usize,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Projector for FailsOnNth {
        type Event = Recorded;
        const NAME: &'static str = "fails-on-nth";

        async fn project(
            &self,
            _fact: &Recorded,
            _ctx: Ctx<'_>,
        ) -> Result<()> {
            let now = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if now == self.n {
                Err(anyhow!("simulated failure on call {}", now))
            } else {
                Ok(())
            }
        }
    }

    fn new_event(payload: &Recorded) -> EventData {
        EventData {
            event_id:        Uuid::new_v4(),
            causation_id:       None,
            workflow_id:  Uuid::new_v4(),
            event_type:      <Recorded as Event>::NAME.to_string(),
            payload:         serde_json::to_value(payload).unwrap(),
            created_at:      Utc::now(),
            category:  None,
            subject_id:    None,
            metadata:        serde_json::Map::new(),
            ephemeral:       None,
            persistent:      true,
        }
    }

    async fn append_n(store: &MemoryStore, n: usize) -> Vec<Uuid> {
        let mut ids = Vec::new();
        for _ in 0..n {
            let payload = Recorded {
                id:          Uuid::new_v4(),
                occurred_at: Utc::now(),
            };
            let ev = new_event(&payload);
            ids.push(ev.event_id);
            crate::append_event(store, ev).await.unwrap();
        }
        ids
    }

    #[tokio::test]
    async fn projector_ctx_carries_persisted_event_workflow_id() {
        // The Projector's ctx is built from the persisted event,
        // not regenerated. Tracing through a fold: emit a fact with
        // explicit workflow_id, run the projector, assert the
        // ctx workflow_id matches.

        #[derive(Default, Clone)]
        struct CorrelationCapture {
            seen: Arc<parking_lot::Mutex<Vec<(Uuid, Uuid)>>>,
        }
        #[async_trait]
        impl Projector for CorrelationCapture {
            type Event = Recorded;
            const NAME: &'static str = "workflow-capture";
            async fn project(
                &self, _fact: &Recorded, ctx: Ctx<'_>,
            ) -> Result<()> {
                self.seen.lock().push((ctx.event_id, ctx.workflow_id));
                Ok(())
            }
        }

        let store = Arc::new(MemoryStore::new());
        let cmd_correlation = Uuid::new_v4();

        // Append a fact with explicit workflow_id (the kind of thing
        // a command handler would produce when responding to an HTTP
        // request).
        let payload = Recorded { id: Uuid::new_v4(), occurred_at: Utc::now() };
        let event_id = Uuid::new_v4();
        let ev = EventData {
            event_id,
            causation_id:       None,
            workflow_id:  cmd_correlation,
            event_type:      <Recorded as Event>::NAME.to_string(),
            payload:         serde_json::to_value(&payload).unwrap(),
            created_at:      Utc::now(),
            category:  None,
            subject_id:    None,
            metadata:        serde_json::Map::new(),
            ephemeral:       None,
            persistent:      true,
        };
        crate::append_event(store.as_ref(), ev).await.unwrap();

        let cap = CorrelationCapture::default();
        let seen = cap.seen.clone();
        let runner = ProjectionRunner::new(
            cap,
            "m_corr",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
        );
        runner.step(10).await.unwrap();

        let observed = seen.lock();
        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0].0, event_id, "ctx.event_id matches persisted");
        assert_eq!(observed[0].1, cmd_correlation,
                   "ctx.workflow_id matches the upstream command's id, \
                    not regenerated");
    }

    #[tokio::test]
    async fn projector_runner_advances_cursor_per_fact() {
        let store = Arc::new(MemoryStore::new());
        let _ids = append_n(&store, 3).await;
        let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let runner = ProjectionRunner::new(
            CollectingProjector { seen: seen.clone() },
            "m1",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
        );

        let outcome = runner.step(10).await.unwrap();
        assert!(matches!(outcome, StepOutcome::Progressed { applied: 3 }));
        assert_eq!(seen.lock().len(), 3);

        // Cursor advanced past the last event.
        let cursor = store.get("m1").await.unwrap();
        assert!(cursor.is_some());
    }

    #[tokio::test]
    async fn projector_runner_does_not_advance_past_failure() {
        let store = Arc::new(MemoryStore::new());
        append_n(&store, 5).await;

        let runner = ProjectionRunner::new(
            FailsOnNth { n: 3, calls: Arc::new(AtomicUsize::new(0)) },
            "m_fail",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
        );

        // Step errors on the 3rd call — cursor should be at fact[1]'s
        // position (2nd successful apply), not advanced past fact[2].
        let result = runner.step(10).await;
        assert!(result.is_err());

        // Cursor advanced past 2 facts (the two successful ones).
        let cursor = store.get("m_fail").await.unwrap().unwrap();

        // Re-fetch event positions to compare.
        let events = EventLogBackend::read_all(store.as_ref(), LogCursor::ZERO, 10).await.unwrap();
        assert_eq!(cursor, events[1].position,
                   "cursor at fact[1] (after two successful projects), \
                    not at fact[2] (where project errored)");
    }

    #[tokio::test]
    async fn idle_when_cursor_at_tail() {
        let store = Arc::new(MemoryStore::new());
        let runner = ProjectionRunner::new(
            CollectingProjector {
                seen: Arc::new(parking_lot::Mutex::new(Vec::new())),
            },
            "m_idle",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
        );
        let outcome = runner.step(10).await.unwrap();
        assert_eq!(outcome, StepOutcome::Idle);
    }

    #[tokio::test]
    async fn dep_fence_holds_when_dep_behind() {
        // Build a Projector with a hard-coded dep on "upstream".
        struct DepM {
            seen: Arc<parking_lot::Mutex<Vec<Uuid>>>,
        }
        #[async_trait]
        impl Projector for DepM {
            type Event = Recorded;
            const NAME: &'static str = "downstream";
            const DEPENDS_ON: &'static [&'static str] = &["upstream"];
            async fn project(
                &self,
                _fact: &Recorded,
                ctx: Ctx<'_>,
            ) -> Result<()> {
                self.seen.lock().push(ctx.event_id);
                Ok(())
            }
        }

        let store = Arc::new(MemoryStore::new());
        append_n(&store, 3).await;

        // Set this consumer's cursor to fact[1] manually so the fence
        // check has something to compare against. Upstream is at ZERO.
        let downstream_pos = {
            let events = EventLogBackend::read_all(store.as_ref(), LogCursor::ZERO, 10).await.unwrap();
            events[1].position
        };
        store.set("downstream", downstream_pos).await.unwrap();

        let runner = ProjectionRunner::new(
            DepM { seen: Arc::new(parking_lot::Mutex::new(Vec::new())) },
            "downstream",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
        );

        // Upstream cursor is ZERO < downstream's cursor → fence trips.
        let outcome = runner.step(10).await.unwrap();
        assert!(matches!(outcome, StepOutcome::WaitOnDep { ref dep, .. } if dep == "upstream"));
    }

    #[tokio::test]
    async fn dep_fence_releases_when_dep_catches_up() {
        struct DepM;
        #[async_trait]
        impl Projector for DepM {
            type Event = Recorded;
            const NAME: &'static str = "downstream-2";
            const DEPENDS_ON: &'static [&'static str] = &["upstream"];
            async fn project(
                &self,
                _fact: &Recorded,
                _ctx: Ctx<'_>,
            ) -> Result<()> { Ok(()) }
        }

        let store = Arc::new(MemoryStore::new());
        append_n(&store, 2).await;

        let last_pos = {
            let events = EventLogBackend::read_all(store.as_ref(), LogCursor::ZERO, 10).await.unwrap();
            events.last().unwrap().position
        };

        // Both at the same position → fence does NOT trip.
        store.set("upstream", last_pos).await.unwrap();
        store.set("downstream", last_pos).await.unwrap();

        let runner = ProjectionRunner::new(
            DepM,
            "downstream",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
        );

        // Cursor is at tail; dep is at tail; runner should be Idle, not WaitOnDep.
        let outcome = runner.step(10).await.unwrap();
        assert_eq!(outcome, StepOutcome::Idle);
    }

    #[tokio::test]
    async fn non_matching_event_type_is_skipped_and_cursor_advances() {
        // Append an event with a different type prefix than the
        // projector's `Event::type_prefix()`. Runner should advance
        // past it without calling project.
        let store = Arc::new(MemoryStore::new());

        // Append a "foreign" event (different prefix).
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

        let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let runner = ProjectionRunner::new(
            CollectingProjector { seen: seen.clone() },
            "m_skip",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
        );

        let outcome = runner.step(10).await.unwrap();
        // Applied 0 facts (event filtered out), but cursor still advanced.
        assert_eq!(outcome, StepOutcome::Progressed { applied: 0 });
        assert!(seen.lock().is_empty());
        assert!(store.get("m_skip").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn fact_without_occurred_at_falls_back_to_event_created_at() {
        // 0.3.4: Event::occurred_at returns Option, default None. When
        // a fact opts out, the runner sets ctx.now() to
        // event.created_at (the persistence-side envelope timestamp).
        // Verifies the fallback path: append a fact whose Event impl
        // uses the default `None`, pin event.created_at to a known
        // value, assert ctx.now() resolves to that pinned timestamp.

        #[derive(Debug, Clone, Serialize, Deserialize)]
        struct NoTimeFact { id: Uuid }

        impl Event for NoTimeFact {
            const NAME: &'static str = "notime";
            fn subject_id(&self) -> Uuid { self.id }
            // occurred_at — uses trait default returning None
        }

        #[derive(Clone)]
        struct CaptureNow { snap: Arc<parking_lot::Mutex<Option<DateTime<Utc>>>> }
        #[async_trait]
        impl Projector for CaptureNow {
            type Event = NoTimeFact;
            const NAME: &'static str = "capture-now";
            async fn project(
                &self, _f: &NoTimeFact, ctx: Ctx<'_>,
            ) -> Result<()> {
                *self.snap.lock() = Some(ctx.now());
                Ok(())
            }
        }

        let store = Arc::new(MemoryStore::new());
        let pinned = DateTime::parse_from_rfc3339("2026-05-06T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let id = Uuid::new_v4();
        let fact = NoTimeFact { id };

        crate::append_event(store.as_ref(), crate::types::EventData {
            event_id:        Uuid::new_v4(),
            causation_id:       None,
            workflow_id:  Uuid::new_v4(),
            event_type:      <NoTimeFact as Event>::NAME.to_string(),
            payload:         serde_json::to_value(&fact).unwrap(),
            created_at:      pinned,
            category:  None,
            subject_id:    None,
            metadata:        serde_json::Map::new(),
            ephemeral:       None,
            persistent:      true,
        }).await.unwrap();

        let cap = CaptureNow { snap: Arc::new(parking_lot::Mutex::new(None)) };
        let snap = cap.snap.clone();
        let runner = ProjectionRunner::new(
            cap,
            "fallback.test",
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
        );

        runner.step(10).await.unwrap();

        assert_eq!(*snap.lock(), Some(pinned),
                   "ctx.now() falls back to event.created_at when Event::occurred_at is None");
    }
}
