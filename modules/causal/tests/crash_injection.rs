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

use causal::checkpoint_store::{
    CheckpointStore, InsertableOutboxRow, OutboxRow, ReactorOutbox,
};
use causal::contexts::Ctx;
use causal::event_log::EventLogBackend;
use causal::fact::Fact;
use causal::projector::Projector;
use causal::memory_store::MemoryStore;
use causal::projection_runner::{ProjectionRunner, StepOutcome};
use causal::reactor_v3::Events;
use causal::reactor_runner::ReactorRunner;
use causal::reactor_v3::Reactor;
use causal::relay::RelayLoop;
use causal::types::{AppendResult, LogCursor, NewEvent, PersistedEvent, StreamVersion};

// ─────────────────────────────────────────────────────────────────────
// Fault injector — wraps MemoryStore; lets tests arm one Err per fault
// point. Once consumed, the wrapper passes through.
// ─────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
enum FaultPoint {
    Append,
    OutboxDelete,
    CheckpointSet,
    CommitReactorBatch,
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
            (Some(FaultPoint::Append),             FaultPoint::Append)
          | (Some(FaultPoint::OutboxDelete),       FaultPoint::OutboxDelete)
          | (Some(FaultPoint::CheckpointSet),      FaultPoint::CheckpointSet)
          | (Some(FaultPoint::CommitReactorBatch), FaultPoint::CommitReactorBatch) => {
                *armed = None;
                true
            }
            _ => false,
        }
    }
}

#[async_trait]
impl EventLogBackend for FaultInjector {
    async fn append(&self, event: NewEvent) -> Result<AppendResult> {
        if self.take_if_matches(FaultPoint::Append) {
            return Err(anyhow!("fault: append"));
        }
        EventLogBackend::append(self.inner.as_ref(), event).await
    }
    async fn load_from(
        &self, after: LogCursor, limit: usize,
    ) -> Result<Vec<PersistedEvent>> {
        EventLogBackend::load_from(self.inner.as_ref(), after, limit).await
    }
    async fn load_stream(
        &self, aggregate_type: &str, aggregate_id: Uuid,
        after_version: Option<StreamVersion>,
    ) -> Result<Vec<PersistedEvent>> {
        EventLogBackend::load_stream(
            self.inner.as_ref(), aggregate_type, aggregate_id, after_version,
        ).await
    }
    async fn latest_position(&self) -> Result<LogCursor> {
        EventLogBackend::latest_position(self.inner.as_ref()).await
    }

    async fn append_to_stream(
        &self,
        aggregate_type: &str,
        aggregate_id: Uuid,
        expected: StreamVersion,
        event: NewEvent,
    ) -> Result<AppendResult> {
        EventLogBackend::append_to_stream(
            self.inner.as_ref(), aggregate_type, aggregate_id, expected, event,
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
impl ReactorOutbox for FaultInjector {
    async fn commit_reactor_batch(
        &self,
        rows: Vec<InsertableOutboxRow>,
        cursor: Option<(String, LogCursor)>,
    ) -> Result<()> {
        if self.take_if_matches(FaultPoint::CommitReactorBatch) {
            return Err(anyhow!("fault: commit_reactor_batch"));
        }
        self.inner.commit_reactor_batch(rows, cursor).await
    }

    async fn outbox_pending(&self, limit: usize) -> Result<Vec<OutboxRow>> {
        self.inner.outbox_pending(limit).await
    }

    async fn outbox_delete(&self, id: i64) -> Result<()> {
        if self.take_if_matches(FaultPoint::OutboxDelete) {
            return Err(anyhow!("fault: outbox_delete"));
        }
        self.inner.outbox_delete(id).await
    }

    async fn record_reactor_attempt(
        &self,
        consumer_id: &str,
        source_event_id: Uuid,
    ) -> Result<u32> {
        self.inner.record_reactor_attempt(consumer_id, source_event_id).await
    }

    async fn clear_reactor_attempts(
        &self,
        consumer_id: &str,
        source_event_id: Uuid,
    ) -> Result<()> {
        self.inner.clear_reactor_attempts(consumer_id, source_event_id).await
    }
}

// ─────────────────────────────────────────────────────────────────────
// Test fixtures
// ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Recorded { id: Uuid, occurred_at: DateTime<Utc> }
impl Fact for Recorded {
    const CATEGORY: &'static str = "test";
    fn name(&self) -> &str { "recorded" }
    fn stream_id(&self) -> Uuid { self.id }
    fn occurred_at(&self) -> Option<DateTime<Utc>> { Some(self.occurred_at) }
}

async fn append_n(store: &MemoryStore, n: usize) {
    for _ in 0..n {
        let payload = Recorded { id: Uuid::new_v4(), occurred_at: Utc::now() };
        let ev = NewEvent {
            event_id: Uuid::new_v4(),
            parent_id: None,
            correlation_id: Uuid::new_v4(),
            event_type: format!("{}:{}", <Recorded as Fact>::CATEGORY, payload.name()),
            payload: serde_json::to_value(&payload).unwrap(),
            created_at: Utc::now(),
            aggregate_type: None,
            aggregate_id: None,
            metadata: serde_json::Map::new(),
            ephemeral: None,
            persistent: true,
        };
        EventLogBackend::append(store, ev).await.unwrap();
    }
}

#[derive(Default, Clone)]
struct CountingProjector {
    seen_event_ids: Arc<Mutex<Vec<Uuid>>>,
}

#[async_trait]
impl Projector for CountingProjector {
    type Fact = Recorded;
    const GROUP_NAME: &'static str = "counting";
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
    let events = EventLogBackend::load_from(
        inner.as_ref(), LogCursor::ZERO, 10,
    ).await.unwrap();
    assert_eq!(cursor, events.last().unwrap().position);
}

// Reactor + outbox fault tests
// ─────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Trigger { id: Uuid, occurred_at: DateTime<Utc> }
impl Fact for Trigger {
    const CATEGORY: &'static str = "test";
    fn name(&self) -> &str { "trigger" }
    fn stream_id(&self) -> Uuid { self.id }
    fn occurred_at(&self) -> Option<DateTime<Utc>> { Some(self.occurred_at) }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Echoed;
impl Fact for Echoed {
    const CATEGORY: &'static str = "test";
    fn name(&self) -> &str { "echoed" }
    fn stream_id(&self) -> Uuid { Uuid::nil() }
}

struct EmitOne;
#[async_trait]
impl Reactor for EmitOne {
    type Trigger = Trigger;
    const GROUP_NAME: &'static str = "emit-one";
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
    let ev = NewEvent {
        event_id,
        parent_id: None,
        correlation_id: Uuid::new_v4(),
        event_type: format!("{}:{}", <Trigger as Fact>::CATEGORY, payload.name()),
        payload: serde_json::to_value(&payload).unwrap(),
        created_at: Utc::now(),
        aggregate_type: None,
        aggregate_id: None,
        metadata: serde_json::Map::new(),
        ephemeral: None,
        persistent: true,
    };
    EventLogBackend::append(store, ev).await.unwrap();
    event_id
}

#[tokio::test]
async fn commit_reactor_batch_failure_leaves_no_partial_state() {
    // C12 atomicity under failure: commit_reactor_batch errors; no
    // outbox rows, no cursor advance. Recovery on next step.
    let inner = Arc::new(MemoryStore::new());
    append_trigger(&inner).await;
    let injector = FaultInjector::new(inner.clone());

    let runner = ReactorRunner::new(
        EmitOne,
        "r.flaky",
        injector.clone() as Arc<dyn EventLogBackend>,
        injector.clone() as Arc<dyn ReactorOutbox>,
    );

    injector.arm(FaultPoint::CommitReactorBatch);
    assert!(runner.step(10).await.is_err());

    // Atomic property: ZERO outbox rows AND cursor unchanged.
    assert_eq!(inner.outbox_pending(10).await.unwrap().len(), 0);
    assert!(inner.get("r.flaky").await.unwrap().is_none());

    // Recovery: next step (no fault) commits the batch.
    runner.step(10).await.unwrap();
    assert_eq!(inner.outbox_pending(10).await.unwrap().len(), 1);
    assert!(inner.get("r.flaky").await.unwrap().is_some());
}

#[tokio::test]
async fn relay_append_failure_leaves_outbox_row_pending() {
    // Relay errors on log.append; outbox row stays pending; next drain
    // succeeds.
    let inner = Arc::new(MemoryStore::new());
    let trigger_event_id = append_trigger(&inner).await;
    let _ = trigger_event_id; // not used; satisfies unused warnings

    // Run reactor to populate outbox.
    {
        let runner = ReactorRunner::new(
            EmitOne,
            "r.x",
            inner.clone() as Arc<dyn EventLogBackend>,
            inner.clone() as Arc<dyn ReactorOutbox>,
        );
        runner.step(10).await.unwrap();
    }
    assert_eq!(inner.outbox_pending(10).await.unwrap().len(), 1);

    // Relay through the injector with append fault armed.
    let injector = FaultInjector::new(inner.clone());
    let relay = RelayLoop::new(
        injector.clone() as Arc<dyn EventLogBackend>,
        injector.clone() as Arc<dyn ReactorOutbox>,
    );

    injector.arm(FaultPoint::Append);
    assert!(relay.drain_once(10).await.is_err());

    // Row is STILL pending — not delivered, not deleted.
    assert_eq!(inner.outbox_pending(10).await.unwrap().len(), 1);

    // Recovery: next drain succeeds.
    relay.drain_once(10).await.unwrap();
    assert!(inner.outbox_pending(10).await.unwrap().is_empty());
}

#[tokio::test]
async fn relay_outbox_delete_failure_redelivers_with_log_dedup() {
    // Relay: log.append succeeded, then outbox_delete errored. On retry
    // the same outbox row re-appends; log idempotent on event_id absorbs
    // the duplicate. After the SECOND retry (no fault), outbox_delete
    // succeeds, row removed.
    let inner = Arc::new(MemoryStore::new());
    append_trigger(&inner).await;
    {
        let runner = ReactorRunner::new(
            EmitOne,
            "r.y",
            inner.clone() as Arc<dyn EventLogBackend>,
            inner.clone() as Arc<dyn ReactorOutbox>,
        );
        runner.step(10).await.unwrap();
    }

    let injector = FaultInjector::new(inner.clone());
    let relay = RelayLoop::new(
        injector.clone() as Arc<dyn EventLogBackend>,
        injector.clone() as Arc<dyn ReactorOutbox>,
    );

    // Fault on outbox_delete: append succeeds, delete errors.
    injector.arm(FaultPoint::OutboxDelete);
    assert!(relay.drain_once(10).await.is_err());

    // Log has the appended event; outbox row STILL pending.
    let log_events = EventLogBackend::load_from(
        inner.as_ref(), LogCursor::ZERO, 10,
    ).await.unwrap();
    let echoed_count = log_events
        .iter()
        .filter(|e| e.event_type == "test:echoed")
        .count();
    assert_eq!(echoed_count, 1);
    assert_eq!(inner.outbox_pending(10).await.unwrap().len(), 1);

    // Second drain (no fault). append called again with same event_id;
    // log dedups (C1); outbox_delete succeeds.
    relay.drain_once(10).await.unwrap();

    // Still exactly one log entry (C1 dedup); outbox empty.
    let log_events = EventLogBackend::load_from(
        inner.as_ref(), LogCursor::ZERO, 10,
    ).await.unwrap();
    let echoed_count = log_events
        .iter()
        .filter(|e| e.event_type == "test:echoed")
        .count();
    assert_eq!(echoed_count, 1, "log dedups duplicate event_id (C1)");
    assert!(inner.outbox_pending(10).await.unwrap().is_empty());
}
