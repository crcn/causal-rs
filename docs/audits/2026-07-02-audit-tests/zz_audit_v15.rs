//! AUDIT VERIFIER #15 — park_terminal_failure divergence livelock.
//! (file: modules/causal/tests/zz_audit_v15.rs — deleted after the run)

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use causal::checkpoint_store::{CheckpointStore, ReactorCheckpoint};
use causal::contexts::Ctx;
use causal::event::Event;
use causal::event_log::EventLogBackend;
use causal::memory_store::MemoryStore;
use causal::reactor::{Events, Reactor, RetryPolicy};
use causal::reactor_runner::ReactorRunner;
use causal::types::{EventData, LogCursor, RecordedEvent, StreamRevision, WriteResult};

// ─── Fault injector: one-shot Err on clear_reactor_attempts ─────────

struct Injector {
    inner: Arc<MemoryStore>,
    clear_faults_remaining: AtomicU32,
    divergent_redeliveries: AtomicU32,
}

impl Injector {
    fn new(inner: Arc<MemoryStore>, clear_faults: u32) -> Arc<Self> {
        Arc::new(Self {
            inner,
            clear_faults_remaining: AtomicU32::new(clear_faults),
            divergent_redeliveries: AtomicU32::new(0),
        })
    }
}

#[async_trait]
impl EventLogBackend for Injector {
    async fn read_all(&self, after: LogCursor, limit: usize) -> Result<Vec<RecordedEvent>> {
        EventLogBackend::read_all(self.inner.as_ref(), after, limit).await
    }
    async fn read_stream(
        &self,
        aggregate_type: &str,
        aggregate_id: Uuid,
        after: Option<StreamRevision>,
    ) -> Result<Vec<RecordedEvent>> {
        EventLogBackend::read_stream(self.inner.as_ref(), aggregate_type, aggregate_id, after)
            .await
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
        let r = EventLogBackend::append_to_stream(
            self.inner.as_ref(), aggregate_type, aggregate_id, expected, events,
        ).await;
        if let Err(e) = &r {
            if e.downcast_ref::<causal::event_log::DivergentRedelivery>().is_some() {
                self.divergent_redeliveries.fetch_add(1, Ordering::SeqCst);
            }
        }
        r
    }
}

#[async_trait]
impl CheckpointStore for Injector {
    async fn get(&self, consumer_id: &str) -> Result<Option<LogCursor>> {
        CheckpointStore::get(self.inner.as_ref(), consumer_id).await
    }
    async fn set(&self, consumer_id: &str, pos: LogCursor) -> Result<()> {
        CheckpointStore::set(self.inner.as_ref(), consumer_id, pos).await
    }
}

#[async_trait]
impl ReactorCheckpoint for Injector {
    async fn record_reactor_attempt(&self, consumer_id: &str, trigger_id: Uuid) -> Result<u32> {
        self.inner.record_reactor_attempt(consumer_id, trigger_id).await
    }
    async fn clear_reactor_attempts(&self, consumer_id: &str, trigger_id: Uuid) -> Result<()> {
        // One transient blip — exactly the failure class the park-retry
        // arm is documented to absorb. All later calls pass through.
        let n = self.clear_faults_remaining.load(Ordering::SeqCst);
        if n > 0
            && self.clear_faults_remaining
                .compare_exchange(n, n - 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        {
            return Err(anyhow!("fault: transient clear_reactor_attempts blip"));
        }
        self.inner.clear_reactor_attempts(consumer_id, trigger_id).await
    }
}

// ─── Fixtures ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Trigger { id: Uuid, occurred_at: DateTime<Utc> }
impl Event for Trigger {
    const NAME: &'static str = "trigger";
    fn subject_id(&self) -> Uuid { self.id }
    fn occurred_at(&self) -> Option<DateTime<Utc>> { Some(self.occurred_at) }
}

/// Deterministic poison body: same error text every run; parks on the
/// first attempt of every delivery. The only run-varying park input is
/// the durable attempt counter.
struct AlwaysPoison { calls: Arc<AtomicU32> }

#[async_trait]
impl Reactor for AlwaysPoison {
    type Trigger = Trigger;
    const NAME: &'static str = "always-poison";
    fn retry_policy(&self) -> Option<RetryPolicy> {
        Some(RetryPolicy::fixed(3, 1)) // 1 ms backoff: fast cycles
    }
    async fn react(&self, _trigger: &Trigger, _ctx: Ctx<'_>) -> Result<Events> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(causal::poison(anyhow!("deterministic poison: unprocessable trigger")))
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
        category: Some(<Trigger as Event>::NAME.to_string()),
        subject_id: Some(payload.id),
        metadata: serde_json::Map::new(),
        ephemeral: None,
        persistent: true,
    };
    causal::append_event(store, ev).await.unwrap();
    event_id
}

// ─── The test ────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transient_blip_after_terminal_fact_append_must_not_livelock_the_park() {
    let inner = Arc::new(MemoryStore::new());
    append_trigger(&inner).await;
    // ONE transient clear_reactor_attempts failure. Under the current code:
    //   attempt 1: poison → park → terminal fact appended {attempts:1}
    //              → clear fails (armed blip) → park Err → infra retry.
    //   attempt 2: body re-runs → counter 2 → park re-derives the SAME
    //              event_id with {attempts:2} → DivergentRedelivery →
    //              park Err → retry → attempt 3 → ... ∀n.
    let injector = Injector::new(inner.clone(), 1);

    let calls = Arc::new(AtomicU32::new(0));
    let runner = ReactorRunner::new(
        AlwaysPoison { calls: calls.clone() },
        "r.audit15",
        injector.clone() as Arc<dyn EventLogBackend>,
        injector.clone() as Arc<dyn ReactorCheckpoint>,
    );

    // Correct behavior: blip absorbed, re-park lands idempotently, trigger
    // acks parked=true, floor persists — within ~100s of ms at 1 ms backoff.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut floor_advanced = false;
    while std::time::Instant::now() < deadline {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), runner.step(10))
            .await
            .expect("runner.step wedged");
        if CheckpointStore::get(inner.as_ref(), "r.audit15").await.unwrap().is_some() {
            floor_advanced = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    runner.halt();

    let body_runs = calls.load(Ordering::SeqCst);
    let divergences = injector.divergent_redeliveries.load(Ordering::SeqCst);
    let all = EventLogBackend::read_all(inner.as_ref(), LogCursor::ZERO, 100).await.unwrap();
    let terminal_facts: Vec<_> = all.iter()
        .filter(|e| e.event_type == causal::reactor_runner::REACTION_FAILED_KIND)
        .collect();
    eprintln!(
        "AUDIT15: floor_advanced={floor_advanced} body_runs={body_runs} \
         divergent_redeliveries={divergences} terminal_facts={} \
         (payload of first: {})",
        terminal_facts.len(),
        terminal_facts.first().map(|e| e.payload.to_string()).unwrap_or_default(),
    );

    assert!(
        floor_advanced,
        "LIVELOCK CONFIRMED: one transient clear_reactor_attempts blip after \
         the terminal-fact append left the trigger permanently unackable — \
         the park loop re-ran the reaction body {body_runs} times and hit \
         DivergentRedelivery {divergences} times (same deterministic \
         event_id, attempts-counter payload drift), never advancing the \
         ack floor",
    );
}
