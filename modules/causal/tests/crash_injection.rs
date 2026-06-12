//! Crash-injection tests for C2 + C12.
//!
//! Phase 4c verified the runtime contracts via post-condition assertions
//! on the happy path. These tests verify the same contracts hold under
//! failure: a configured backend method returns `Err` mid-flight (the
//! moral equivalent of a kill -9 between two backend writes), and the
//! tests assert that no partial state is observable and that a fresh
//! step recovers correctly.
//!
//! For MemoryStore, atomic operations are guarded by a single Mutex,
//! so the `commit_reactor_batch` atomicity is structurally guaranteed
//! at the impl level. These tests therefore focus on the runner-level
//! recovery contract: when an inner backend method returns Err, the
//! next step picks up where it left off without duplicating state.
//!
//! Faults are armed before the call and consumed atomically — armed
//! fault returns Err exactly once, then the wrapper passes through to
//! the inner backend for subsequent calls. This models the "process
//! crashed once mid-step, restarted, retry succeeds" recovery shape.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use causal::checkpoint_store::{CheckpointStore, ReactorCheckpoint};
use causal::contexts::Ctx;
use causal::event_log::EventLogBackend;
use causal::event::Event;
use causal::projector::Projector;
use causal::memory_store::MemoryStore;
use causal::projection_runner::{ProjectionRunner, StepOutcome};
use causal::reactor::Events;
use causal::reactor_runner::ReactorRunner;
use causal::reactor::Reactor;
use causal::types::{WriteResult, LogCursor, EventData, RecordedEvent, StreamRevision};

// ─────────────────────────────────────────────────────────────────────
// Fault injector — wraps MemoryStore; lets tests arm one Err per fault
// point. Once consumed, the wrapper passes through.
// ─────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
enum FaultPoint {
    Append,
    CheckpointSet,
}

struct FaultInjector {
    inner: Arc<MemoryStore>,
    armed: Mutex<Option<FaultPoint>>,
}

impl FaultInjector {
    fn new(inner: Arc<MemoryStore>) -> Arc<Self> {
        Arc::new(Self { inner, armed: Mutex::new(None) })
    }

    fn arm(&self, fault: FaultPoint) {
        *self.armed.lock() = Some(fault);
    }

    fn take_if_matches(&self, target: FaultPoint) -> bool {
        let mut armed = self.armed.lock();
        match (&*armed, &target) {
            (Some(FaultPoint::Append),        FaultPoint::Append)
          | (Some(FaultPoint::CheckpointSet), FaultPoint::CheckpointSet) => {
                *armed = None;
                true
            }
            _ => false,
        }
    }
}

#[async_trait]
impl EventLogBackend for FaultInjector {
    async fn read_all(
        &self, after: LogCursor, limit: usize,
    ) -> Result<Vec<RecordedEvent>> {
        EventLogBackend::read_all(self.inner.as_ref(), after, limit).await
    }
    async fn read_stream(
        &self, aggregate_type: &str, aggregate_id: Uuid,
        after: Option<StreamRevision>,
    ) -> Result<Vec<RecordedEvent>> {
        EventLogBackend::read_stream(
            self.inner.as_ref(), aggregate_type, aggregate_id, after,
        ).await
    }
    async fn latest_position(&self) -> Result<LogCursor> {
        EventLogBackend::latest_position(self.inner.as_ref()).await
    }

    async fn append_to_stream(
        &self,
        aggregate_type: &str,
        aggregate_id: Uuid,
        expected: causal::types::StreamState,
        events: Vec<EventData>,
    ) -> Result<WriteResult> {
        // All appends flow through this one primitive now, so the
        // `Append` fault point fires here.
        if self.take_if_matches(FaultPoint::Append) {
            return Err(anyhow!("fault: append"));
        }
        EventLogBackend::append_to_stream(
            self.inner.as_ref(), aggregate_type, aggregate_id, expected, events,
        ).await
    }
}

#[async_trait]
impl CheckpointStore for FaultInjector {
    async fn get(&self, consumer_id: &str) -> Result<Option<LogCursor>> {
        CheckpointStore::get(self.inner.as_ref(), consumer_id).await
    }
    async fn set(&self, consumer_id: &str, pos: LogCursor) -> Result<()> {
        if self.take_if_matches(FaultPoint::CheckpointSet) {
            return Err(anyhow!("fault: checkpoint set"));
        }
        CheckpointStore::set(self.inner.as_ref(), consumer_id, pos).await
    }
}

#[async_trait]
impl ReactorCheckpoint for FaultInjector {
    async fn record_reactor_attempt(
        &self,
        consumer_id: &str,
        trigger_id: Uuid,
    ) -> Result<u32> {
        self.inner.record_reactor_attempt(consumer_id, trigger_id).await
    }

    async fn clear_reactor_attempts(
        &self,
        consumer_id: &str,
        trigger_id: Uuid,
    ) -> Result<()> {
        self.inner.clear_reactor_attempts(consumer_id, trigger_id).await
    }
}

// ─────────────────────────────────────────────────────────────────────
// Test fixtures
// ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Recorded { id: Uuid, occurred_at: DateTime<Utc> }
impl Event for Recorded {
    const NAME: &'static str = "recorded";
    fn subject_id(&self) -> Uuid { self.id }
    fn occurred_at(&self) -> Option<DateTime<Utc>> { Some(self.occurred_at) }
}

async fn append_n(store: &MemoryStore, n: usize) {
    for _ in 0..n {
        let payload = Recorded { id: Uuid::new_v4(), occurred_at: Utc::now() };
        let ev = EventData {
            event_id: Uuid::new_v4(),
            causation_id: None,
            workflow_id: Uuid::new_v4(),
            event_type: <Recorded as Event>::NAME.to_string(),
            payload: serde_json::to_value(&payload).unwrap(),
            created_at: Utc::now(),
            // Honest stream coordinates — what Engine::emit writes.
            category: Some(<Recorded as Event>::NAME.to_string()),
            subject_id: Some(payload.id),
            metadata: serde_json::Map::new(),
            ephemeral: None,
            persistent: true,
        };
        causal::append_event(store, ev).await.unwrap();
    }
}

#[derive(Default, Clone)]
struct CountingProjector {
    seen_event_ids: Arc<Mutex<Vec<Uuid>>>,
}

#[async_trait]
impl Projector for CountingProjector {
    type Event = Recorded;
    const NAME: &'static str = "counting";
    async fn project(
        &self, _fact: &Recorded, ctx: Ctx<'_>,
    ) -> Result<()> {
        // Idempotent: track unique event_ids; record every call too.
        self.seen_event_ids.lock().push(ctx.event_id);
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn cursor_set_failure_after_project_redelivers_idempotently() {
    // Models: project() returned Ok, then process crashed before
    // cursor.set persisted. On restart, the fact redelivers; idempotent
    // project absorbs the duplicate.
    let inner = Arc::new(MemoryStore::new());
    append_n(&inner, 3).await;
    let injector = FaultInjector::new(inner.clone());

    let m = CountingProjector::default();
    let runner = ProjectionRunner::new(
        m.clone(),
        "m1",
        injector.clone() as Arc<dyn EventLogBackend>,
        injector.clone() as Arc<dyn CheckpointStore>,
    );

    // Arm fault on the FIRST checkpoint.set — happens after fact[0]
    // was projected successfully.
    injector.arm(FaultPoint::CheckpointSet);

    let result = runner.step(10).await;
    assert!(result.is_err(), "first step errored on cursor set");

    // Materialize was called once; cursor did NOT advance.
    assert_eq!(m.seen_event_ids.lock().len(), 1);
    assert!(
        injector.get("m1").await.unwrap().is_none(),
        "cursor must not have advanced past the failed set"
    );

    // Next step (no fault armed) should redeliver fact[0] AND continue
    // through fact[1], fact[2]. Idempotent projector absorbs the
    // duplicate fact[0] call without ill effect.
    let outcome = runner.step(10).await.unwrap();
    assert!(matches!(outcome, StepOutcome::Progressed { applied: 3 }));
    assert_eq!(m.seen_event_ids.lock().len(), 4,
               "fact[0] called twice (idempotent), fact[1] + fact[2] once each");

    // Cursor at last fact's position.
    let cursor = injector.get("m1").await.unwrap().unwrap();
    let events = EventLogBackend::read_all(
        inner.as_ref(), LogCursor::ZERO, 10,
    ).await.unwrap();
    assert_eq!(cursor, events.last().unwrap().position);
}

// Reactor + outbox fault tests
// ─────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Trigger { id: Uuid, occurred_at: DateTime<Utc> }
impl Event for Trigger {
    const NAME: &'static str = "trigger";
    fn subject_id(&self) -> Uuid { self.id }
    fn occurred_at(&self) -> Option<DateTime<Utc>> { Some(self.occurred_at) }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Echoed;
impl Event for Echoed {
    // Distinct category from the trigger ("test") — self-trigger
    // discipline: a reactor must not emit into its own trigger category,
    // else it would consume its own output. Now that outputs land in the
    // log immediately (no outbox), this is enforced in practice.
    const NAME: &'static str = "echoed";
    fn subject_id(&self) -> Uuid { Uuid::nil() }
}

struct EmitOne;
#[async_trait]
impl Reactor for EmitOne {
    type Trigger = Trigger;
    const NAME: &'static str = "emit-one";
    async fn react(
        &self, _trigger: &Trigger, _ctx: Ctx<'_>,
    ) -> Result<Events> {
        let mut out = Events::new();
        out.push(Echoed);
        Ok(out)
    }
}

async fn append_trigger(store: &MemoryStore) -> Uuid {
    let event_id = Uuid::new_v4();
    let payload = Trigger { id: Uuid::new_v4(), occurred_at: Utc::now() };
    let ev = EventData {
        event_id,
        causation_id: None,
        workflow_id: Uuid::new_v4(),
        event_type: <Trigger as Event>::NAME.to_string(),
        payload: serde_json::to_value(&payload).unwrap(),
        created_at: Utc::now(),
        // Honest stream coordinates — what Engine::emit writes.
        category: Some(<Trigger as Event>::NAME.to_string()),
        subject_id: Some(payload.id),
        metadata: serde_json::Map::new(),
        ephemeral: None,
        persistent: true,
    };
    causal::append_event(store, ev).await.unwrap();
    event_id
}

#[tokio::test]
async fn reactor_append_then_checkpoint_crash_redelivers_idempotently() {
    // New crash model (no outbox): the runner appends the output, then
    // advances the cursor. A crash AFTER append but BEFORE checkpoint.set
    // leaves the output in the log with the cursor un-advanced. On the
    // next step the trigger redelivers; the runner re-reacts and
    // re-appends — the log's deterministic-event_id dedup (C1) absorbs
    // the duplicate, so exactly ONE output exists and the cursor advances.
    let inner = Arc::new(MemoryStore::new());
    append_trigger(&inner).await;
    let injector = FaultInjector::new(inner.clone());

    let runner = ReactorRunner::new(
        EmitOne,
        "r.crash",
        injector.clone() as Arc<dyn EventLogBackend>,
        injector.clone() as Arc<dyn ReactorCheckpoint>,
    );

    // Crash at the cursor advance, AFTER the output append.
    injector.arm(FaultPoint::CheckpointSet);
    assert!(runner.step(10).await.is_err());

    // Output is in the log; cursor NOT advanced.
    let after_crash =
        EventLogBackend::read_all(inner.as_ref(), LogCursor::ZERO, 10).await.unwrap();
    let outs1 = after_crash
        .iter()
        .filter(|e| e.event_type == "echoed")
        .count();
    assert_eq!(outs1, 1, "output appended before the crash");
    assert!(inner.get("r.crash").await.unwrap().is_none(), "cursor not advanced");

    // Recovery: next step re-reacts + re-appends (deduped on event_id),
    // then advances the cursor.
    runner.step(10).await.unwrap();
    let after_recovery =
        EventLogBackend::read_all(inner.as_ref(), LogCursor::ZERO, 10).await.unwrap();
    let outs2 = after_recovery
        .iter()
        .filter(|e| e.event_type == "echoed")
        .count();
    assert_eq!(outs2, 1, "re-append deduped on event_id — exactly one output");
    assert!(inner.get("r.crash").await.unwrap().is_some(), "cursor advanced on recovery");
}

// (The outbox/relay crash-recovery tests were removed with slice 3 —
// reactor outputs now append directly; their crash model is covered by
// `reactor_append_then_checkpoint_crash_redelivers_idempotently` above.)

// ─────────────────────────────────────────────────────────────────────
// A2 — fold idempotency under crash/redelivery (2026-06-10 remediation)
// ─────────────────────────────────────────────────────────────────────

#[derive(Default, Clone, Debug, Serialize, Deserialize)]
struct TriggerCount { n: u32 }
impl causal::Aggregate for TriggerCount {
    const NAME: &'static str = "TriggerCount";
}
impl causal::aggregate::Apply<Trigger> for TriggerCount {
    fn apply(&mut self, _: &Trigger) { self.n += 1; }
}

#[derive(Default, Clone, Debug, Serialize, Deserialize)]
struct EchoCount { n: u32 }
impl causal::Aggregate for EchoCount {
    const NAME: &'static str = "EchoCount";
}
impl causal::aggregate::Apply<Echoed> for EchoCount {
    fn apply(&mut self, _: &Echoed) { self.n += 1; }
}

#[tokio::test]
async fn crash_redelivery_folds_exactly_once_in_both_registries() {
    // The full A2 crash model: a reactor with aggregators attached
    // crashes between output-append and checkpoint-set. Redelivery
    // must be exactly-once at EVERY layer:
    //   log     — re-append dedups on the deterministic event_id
    //             (pinned by the earlier test);
    //   consumer registry — the trigger's re-fold is an idempotent
    //             skip (pre-A2: double-counted);
    //   engine registry   — the output's re-fold arrives with the
    //             ORIGINAL WriteResult coordinates and skips
    //             (pre-A2: reactor_runner re-folded unconditionally).
    let inner = Arc::new(MemoryStore::new());
    let trigger_event_id = append_trigger(&inner).await;
    let _ = trigger_event_id;
    let injector = FaultInjector::new(inner.clone());

    let mut consumer_reg = causal::aggregator::AggregatorRegistry::new();
    consumer_reg.register(causal::aggregator::Aggregator::for_type::<TriggerCount, Trigger>());
    let consumer_reg = Arc::new(consumer_reg);

    let mut engine_reg = causal::aggregator::AggregatorRegistry::new();
    engine_reg.register(causal::aggregator::Aggregator::for_type::<EchoCount, Echoed>());
    let engine_reg = Arc::new(engine_reg);

    let runner = ReactorRunner::new(
        EmitOne,
        "r.fold-once",
        injector.clone() as Arc<dyn EventLogBackend>,
        injector.clone() as Arc<dyn ReactorCheckpoint>,
    )
    .with_aggregators(consumer_reg.clone())
    .with_engine_aggregators(Some(engine_reg.clone()));

    // The trigger's stream id (Trigger::subject_id = payload.id).
    let events = EventLogBackend::read_all(inner.as_ref(), LogCursor::ZERO, 10).await.unwrap();
    let trigger_subject_id = events[0].subject_id;

    // Crash at the cursor advance, AFTER react + output append + folds.
    injector.arm(FaultPoint::CheckpointSet);
    assert!(runner.step(10).await.is_err());

    // Recovery: redelivery re-reacts, re-appends (deduped), re-folds
    // (skipped).
    runner.step(10).await.unwrap();

    let (_, trigger_count) = consumer_reg.get_transition_arc::<TriggerCount>(trigger_subject_id);
    assert_eq!(trigger_count.n, 1,
               "consumer registry folded the redelivered trigger exactly once");

    let (_, echo_count) = engine_reg.get_transition_arc::<EchoCount>(Uuid::nil());
    assert_eq!(echo_count.n, 1,
               "engine registry folded the deduped output exactly once");
}

#[tokio::test]
async fn cursor_set_failure_does_not_double_count_aggregates() {
    // Transient checkpoint-set failure on a projection runner with
    // aggregators: the retried step re-delivers already-folded events;
    // each fold must be an idempotent skip. Pre-A2, the rollback
    // machinery restored state only when the BODY failed — a
    // checkpoint-set failure re-folded every event in the retried
    // batch (Balance += amount double-counted).
    #[derive(Default, Clone, Debug, Serialize, Deserialize)]
    struct RecordedCount { n: u32 }
    impl causal::Aggregate for RecordedCount {
        const NAME: &'static str = "RecordedCount";
    }
    impl causal::aggregate::Apply<Recorded> for RecordedCount {
        fn apply(&mut self, _: &Recorded) { self.n += 1; }
    }

    let inner = Arc::new(MemoryStore::new());
    append_n(&inner, 3).await;
    let injector = FaultInjector::new(inner.clone());

    let mut reg = causal::aggregator::AggregatorRegistry::new();
    reg.register(causal::aggregator::Aggregator::for_type::<RecordedCount, Recorded>());
    let reg = Arc::new(reg);

    let m = CountingProjector::default();
    let runner = ProjectionRunner::new(
        m.clone(),
        "m.fold-once",
        injector.clone() as Arc<dyn EventLogBackend>,
        injector.clone() as Arc<dyn CheckpointStore>,
    )
    .with_aggregators(reg.clone());

    // First checkpoint.set fails after fact[0] folded + projected.
    injector.arm(FaultPoint::CheckpointSet);
    assert!(runner.step(10).await.is_err());

    // Retry processes all three facts (fact[0] redelivered).
    let outcome = runner.step(10).await.unwrap();
    assert!(matches!(outcome, StepOutcome::Progressed { applied: 3 }));

    // Every Recorded event has a unique stream id, so each aggregate
    // entry must hold exactly one fold — a double-count shows as n=2
    // on fact[0]'s entry.
    let events = EventLogBackend::read_all(inner.as_ref(), LogCursor::ZERO, 10).await.unwrap();
    for event in &events {
        let (_, count) = reg.get_transition_arc::<RecordedCount>(event.subject_id);
        assert_eq!(count.n, 1,
                   "event at position {} folded exactly once across the retry",
                   event.position.raw());
    }
}
