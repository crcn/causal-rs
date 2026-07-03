//! Audit verification (finding #3): terminal park sits OUTSIDE the
//! decision protocol. Crash model identical to tests/crash_injection.rs:
//! an armed Err on checkpoint `set` is the moral kill -9 between the
//! park's terminal-fact append and the ack-floor persist.
//!
//! Claim under test: park appends a REACTION_FAILED fact durably, seals
//! NO decision, and only later persists the floor. A crash in that
//! window redelivers the trigger; the decision-replay gate misses; the
//! body re-runs; if the failure was environmental and has cleared, the
//! reaction now SUCCEEDS, seals, and appends its outputs — leaving BOTH
//! a terminal-failure fact and success outputs for one trigger.
//!
//! The final assertion states the library's promise ("one decision per
//! trigger — never two contradictory durable outcomes"). If the defect
//! is real, the assertion FAILS.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering as AOrd};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use causal::checkpoint_store::{CheckpointStore, ReactorCheckpoint};
use causal::contexts::Ctx;
use causal::decision_store::{DecisionStore, InMemoryDecisionStore};
use causal::event::Event;
use causal::event_log::EventLogBackend;
use causal::memory_store::MemoryStore;
use causal::poison;
use causal::reactor::{Events, Reactor};
use causal::reactor_observer::ReactorObserver;
use causal::reactor_runner::{ReactorRunner, REACTION_FAILED_KIND};
use causal::types::{EventData, LogCursor, RecordedEvent, StreamRevision, WriteResult};

// ── Fault injector (copied shape from tests/crash_injection.rs) ──────

#[derive(Clone, Debug)]
enum FaultPoint {
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
    fn take_if_checkpoint_set(&self) -> bool {
        let mut armed = self.armed.lock();
        if matches!(&*armed, Some(FaultPoint::CheckpointSet)) {
            *armed = None;
            true
        } else {
            false
        }
    }
}

#[async_trait]
impl EventLogBackend for FaultInjector {
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
        EventLogBackend::append_to_stream(
            self.inner.as_ref(),
            aggregate_type,
            aggregate_id,
            expected,
            events,
        )
        .await
    }
}

#[async_trait]
impl CheckpointStore for FaultInjector {
    async fn get(&self, consumer_id: &str) -> Result<Option<LogCursor>> {
        CheckpointStore::get(self.inner.as_ref(), consumer_id).await
    }
    async fn set(&self, consumer_id: &str, pos: LogCursor) -> Result<()> {
        if self.take_if_checkpoint_set() {
            return Err(anyhow!("fault: checkpoint set (simulated kill -9)"));
        }
        CheckpointStore::set(self.inner.as_ref(), consumer_id, pos).await
    }
}

#[async_trait]
impl ReactorCheckpoint for FaultInjector {
    async fn record_reactor_attempt(&self, consumer_id: &str, trigger_id: Uuid) -> Result<u32> {
        self.inner.record_reactor_attempt(consumer_id, trigger_id).await
    }
    async fn clear_reactor_attempts(&self, consumer_id: &str, trigger_id: Uuid) -> Result<()> {
        self.inner.clear_reactor_attempts(consumer_id, trigger_id).await
    }
}

// ── Fixtures ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OrderPlaced {
    order_id: Uuid,
    occurred_at: DateTime<Utc>,
}
impl Event for OrderPlaced {
    const NAME: &'static str = "order_placed";
    fn subject_id(&self) -> Uuid {
        self.order_id
    }
    fn occurred_at(&self) -> Option<DateTime<Utc>> {
        Some(self.occurred_at)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OrderShipped {
    order_id: Uuid,
}
impl Event for OrderShipped {
    const NAME: &'static str = "order_shipped";
    fn subject_id(&self) -> Uuid {
        self.order_id
    }
}

/// Body fails (poison → parks on first attempt) while `outage` is true;
/// succeeds once the outage clears. Models an environmental failure that
/// resolves between the crash and the restart.
struct Flappy {
    outage: Arc<AtomicBool>,
    calls: Arc<AtomicUsize>,
}
#[async_trait]
impl Reactor for Flappy {
    type Trigger = OrderPlaced;
    const NAME: &'static str = "flappy";
    async fn react(&self, t: &OrderPlaced, _ctx: Ctx<'_>) -> Result<Events> {
        self.calls.fetch_add(1, AOrd::SeqCst);
        if self.outage.load(AOrd::SeqCst) {
            Err(poison(anyhow!("external dependency down")))
        } else {
            let mut out = Events::new();
            out.push(OrderShipped { order_id: t.order_id });
            Ok(out)
        }
    }
}

/// Counts the terminal-failure hook — the DLQ side effect.
#[derive(Default)]
struct DlqObserver {
    terminal_failures: AtomicUsize,
}
impl ReactorObserver for DlqObserver {
    fn reactor_terminal_failure(
        &self,
        _event_id: Uuid,
        _reactor_id: &str,
        _workflow_id: Uuid,
        _attempts: u32,
        _error: &str,
        _at: DateTime<Utc>,
    ) {
        self.terminal_failures.fetch_add(1, AOrd::SeqCst);
    }
}

async fn append_trigger(store: &MemoryStore) -> Uuid {
    let event_id = Uuid::new_v4();
    let payload = OrderPlaced { order_id: Uuid::new_v4(), occurred_at: Utc::now() };
    let ev = EventData {
        event_id,
        causation_id: None,
        workflow_id: Uuid::new_v4(),
        event_type: <OrderPlaced as Event>::NAME.to_string(),
        payload: serde_json::to_value(&payload).unwrap(),
        created_at: Utc::now(),
        category: Some(<OrderPlaced as Event>::NAME.to_string()),
        subject_id: Some(payload.order_id),
        metadata: serde_json::Map::new(),
        ephemeral: None,
        persistent: true,
    };
    causal::append_event(store, ev).await.unwrap();
    event_id
}

#[tokio::test]
async fn park_then_crash_then_success_double_outcome() {
    let inner = Arc::new(MemoryStore::new());
    let trigger_id = append_trigger(&inner).await;
    let injector = FaultInjector::new(inner.clone());

    let outage = Arc::new(AtomicBool::new(true));
    let calls = Arc::new(AtomicUsize::new(0));
    let ds: Arc<dyn DecisionStore> = Arc::new(InMemoryDecisionStore::new());
    let obs = Arc::new(DlqObserver::default());

    // ── Phase A: outage → poison park; crash before floor persist ────
    let runner = ReactorRunner::new(
        Flappy { outage: outage.clone(), calls: calls.clone() },
        "r.audit3",
        injector.clone() as Arc<dyn EventLogBackend>,
        injector.clone() as Arc<dyn ReactorCheckpoint>,
    )
    .with_decision_store(ds.clone())
    .with_observer(obs.clone() as Arc<dyn ReactorObserver>);

    injector.arm(FaultPoint::CheckpointSet);
    let overall = tokio::time::timeout(Duration::from_secs(20), async {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            match runner.step(10).await {
                Err(_) => break, // the "crash": floor persist failed
                Ok(_) => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "armed checkpoint fault never surfaced"
                    );
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
            }
        }
    })
    .await;
    overall.expect("phase A wedged");
    runner.halt(); // process takes no further steps

    // Durable state after the crash: terminal fact IS in the log, the
    // floor is NOT persisted, and — this is the gap — NO decision sealed.
    let after_crash =
        EventLogBackend::read_all(inner.as_ref(), LogCursor::ZERO, 50).await.unwrap();
    let parked_facts = after_crash
        .iter()
        .filter(|e| e.event_type == REACTION_FAILED_KIND)
        .count();
    assert_eq!(parked_facts, 1, "terminal-failure fact appended before the crash");
    assert!(
        CheckpointStore::get(inner.as_ref(), "r.audit3").await.unwrap().is_none(),
        "floor not persisted — trigger WILL redeliver"
    );
    assert!(
        ds.get("r.audit3", trigger_id).await.unwrap().is_none(),
        "park sealed no decision record"
    );
    assert_eq!(
        obs.terminal_failures.load(AOrd::SeqCst),
        1,
        "DLQ hook fired before the crash"
    );

    // ── Phase B: restart; the outage is over ─────────────────────────
    outage.store(false, AOrd::SeqCst);
    let recovered = ReactorRunner::new(
        Flappy { outage: outage.clone(), calls: calls.clone() },
        "r.audit3",
        injector.clone() as Arc<dyn EventLogBackend>,
        injector.clone() as Arc<dyn ReactorCheckpoint>,
    )
    .with_decision_store(ds.clone())
    .with_observer(obs.clone() as Arc<dyn ReactorObserver>);

    let overall = tokio::time::timeout(Duration::from_secs(20), async {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while CheckpointStore::get(inner.as_ref(), "r.audit3").await.unwrap().is_none() {
            recovered.step(10).await.unwrap();
            assert!(
                std::time::Instant::now() < deadline,
                "recovery never advanced the floor"
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await;
    overall.expect("phase B wedged");
    recovered.halt();

    // ── The contract ─────────────────────────────────────────────────
    // One decision per trigger: the log must not carry BOTH a terminal-
    // failure fact and success outputs for the same trigger. Correct
    // behavior passes this; the defect fails it.
    let all = EventLogBackend::read_all(inner.as_ref(), LogCursor::ZERO, 50).await.unwrap();
    let failed: Vec<&RecordedEvent> = all
        .iter()
        .filter(|e| {
            e.event_type == REACTION_FAILED_KIND && e.causation_id == Some(trigger_id)
        })
        .collect();
    let shipped: Vec<&RecordedEvent> = all
        .iter()
        .filter(|e| e.event_type == "order_shipped" && e.causation_id == Some(trigger_id))
        .collect();
    println!(
        "post-recovery: body_calls={} terminal_facts={} success_outputs={} \
         decision_sealed={}",
        calls.load(AOrd::SeqCst),
        failed.len(),
        shipped.len(),
        ds.get("r.audit3", trigger_id).await.unwrap().is_some(),
    );
    assert!(
        !(failed.is_empty() && shipped.is_empty()),
        "the trigger must have SOME durable outcome"
    );
    assert!(
        failed.is_empty() || shipped.is_empty(),
        "CHIMERA: one trigger produced two contradictory durable outcomes — \
         {} terminal-failure fact(s) AND {} success output(s)",
        failed.len(),
        shipped.len(),
    );
}
