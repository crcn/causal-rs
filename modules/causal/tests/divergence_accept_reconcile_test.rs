//! AUDIT REPRO (finding #21): the divergence-accept path seals a record
//! that permanently contradicts the log and never reconciles it — the NEXT
//! redelivery poison-parks a succeeded trigger as a fake integrity
//! violation.
//!
//! Sequence:
//!   1. First delivery: body emits Reminder{nonce:0}, record D1 sealed,
//!      output appended, ack persisted normally.
//!   2. Retention GC removes D1 (legal: age-based sweep, floor has passed
//!      — remove_sealed_before, exactly the engine-sweep primitive).
//!   3. Checkpoint regression (PG restore / operator truncate) redelivers
//!      the trigger. Gate get-misses; the nondeterministic body re-decides
//!      Reminder{nonce:1} with the SAME identity-keyed event_id; the runner
//!      seals D2{nonce:1} FIRST (reactor_runner.rs:1412), THEN appends
//!      (:1423). The append diverges; the accept-and-advance branch
//!      (from_record=false, :1574-1603) keeps the log's canonical nonce-0
//!      row and returns Done — but D2{nonce:1} stays sealed, durably
//!      asserting outputs the log rejected.
//!   4. Any later routine redelivery (crash between ack and floor persist —
//!      the at-least-once window promise 1 sanctions): the gate now HITS
//!      D2, takes the replay path (from_record=true), the append diverges
//!      again → RecordIntegrityError → poison park → a spurious
//!      causal:reaction_failed terminal fact appended into the trigger's
//!      subject history, for work that SUCCEEDED.
//!
//! CORRECT behavior: a trigger whose reaction succeeded (and whose
//! re-decide was reconciled by accept-and-advance) must never be
//! poison-parked as an integrity violation on a routine redelivery, and no
//! causal:reaction_failed fact may enter its history. The accept branch's
//! own comment states the intent: "do NOT park (parking a SUCCEEDED
//! reaction would storm terminal facts on every replay)". The assertions
//! below encode that; the shipped code makes them FAIL.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use causal::checkpoint_store::{CheckpointStore, ReactorCheckpoint};
use causal::contexts::Ctx;
use causal::decision_store::{DecisionStore, InMemoryDecisionStore};
use causal::event::Event;
use causal::event_log::EventLogBackend;
use causal::memory_store::MemoryStore;
use causal::reactor::{Events, Reactor};
use causal::reactor_observer::ReactorObserver;
use causal::reactor_runner::{ReactorRunner, REACTION_FAILED_KIND};
use causal::types::{EventData, LogCursor};

// ── fixtures ─────────────────────────────────────────────────────────

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

/// Same KIND and SUBJECT every run → same identity-keyed event_id; only
/// the payload (nonce) differs across runs. That is the payload-level
/// nondeterminism (wall clock, rand, feature flag) A3 explicitly covers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct Reminder {
    order_id: Uuid,
    nonce: usize,
}
impl Event for Reminder {
    const NAME: &'static str = "reminder";
    fn subject_id(&self) -> Uuid {
        self.order_id
    }
}

struct NondeterministicEmit {
    calls: Arc<AtomicUsize>,
}
#[async_trait]
impl Reactor for NondeterministicEmit {
    type Trigger = OrderPlaced;
    const NAME: &'static str = "audit-nondet";
    async fn react(&self, trigger: &OrderPlaced, _ctx: Ctx<'_>) -> Result<Events> {
        let nonce = self.calls.fetch_add(1, Ordering::SeqCst);
        let mut out = Events::new();
        out.push(Reminder { order_id: trigger.order_id, nonce });
        Ok(out)
    }
}

#[derive(Default)]
struct CountingObserver {
    divergences: AtomicUsize,
    terminal_failures: AtomicUsize,
    last_terminal_error: parking_lot::Mutex<Option<String>>,
}
impl ReactorObserver for CountingObserver {
    fn reactor_divergence(&self, _e: Uuid, _r: &str, _w: Uuid, _d: &str) {
        self.divergences.fetch_add(1, Ordering::SeqCst);
    }
    fn reactor_terminal_failure(
        &self,
        _e: Uuid,
        _r: &str,
        _w: Uuid,
        _attempts: u32,
        error: &str,
        _at: DateTime<Utc>,
    ) {
        self.terminal_failures.fetch_add(1, Ordering::SeqCst);
        *self.last_terminal_error.lock() = Some(error.to_string());
    }
}

/// Drive `runner.step` until `done()` reports true, with a hard deadline.
async fn drive<R, F>(runner: &ReactorRunner<R>, mut done: F, what: &str)
where
    R: Reactor + 'static,
    R::Trigger: serde::de::DeserializeOwned,
    F: FnMut() -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>>,
{
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        runner.step(64).await.unwrap();
        if done().await {
            // Extra reap/persist passes so acks and floors settle.
            for _ in 0..5 {
                runner.step(64).await.unwrap();
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out driving runner: {what}",
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn append_trigger(store: &MemoryStore, t: &OrderPlaced) -> Uuid {
    let event_id = Uuid::new_v4();
    let ev = EventData {
        event_id,
        causation_id: None,
        workflow_id: Uuid::new_v4(),
        event_type: <OrderPlaced as Event>::NAME.to_string(),
        payload: serde_json::to_value(t).unwrap(),
        created_at: Utc::now(),
        category: Some(<OrderPlaced as Event>::NAME.to_string()),
        subject_id: Some(t.order_id),
        metadata: serde_json::Map::new(),
        ephemeral: None,
        persistent: true,
    };
    causal::append_event(store, ev).await.unwrap();
    event_id
}

/// One full delivery pass: fresh runner over the shared store/decisions,
/// driven until the durable floor passes `trigger_pos`.
async fn deliver(
    store: &Arc<MemoryStore>,
    decisions: &Arc<InMemoryDecisionStore>,
    calls: &Arc<AtomicUsize>,
    obs: &Arc<CountingObserver>,
    consumer: &str,
    trigger_pos: LogCursor,
    what: &str,
) {
    let runner = ReactorRunner::new(
        NondeterministicEmit { calls: calls.clone() },
        consumer,
        store.clone() as Arc<dyn EventLogBackend>,
        store.clone() as Arc<dyn ReactorCheckpoint>,
    )
    .with_decision_store(decisions.clone() as Arc<dyn DecisionStore>)
    .with_observer(obs.clone() as Arc<dyn ReactorObserver>);
    let store_c = store.clone();
    let consumer_s = consumer.to_string();
    drive(
        &runner,
        move || {
            let store = store_c.clone();
            let consumer = consumer_s.clone();
            Box::pin(async move {
                CheckpointStore::get(store.as_ref(), &consumer)
                    .await
                    .unwrap()
                    .map_or(false, |c| c >= trigger_pos)
            })
        },
        what,
    )
    .await;
    runner.halt();
}

#[tokio::test]
async fn accept_and_advance_must_not_arm_a_future_integrity_park() {
    const CONSUMER: &str = "r.audit21";

    let store = Arc::new(MemoryStore::new());
    let decisions = Arc::new(InMemoryDecisionStore::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let obs = Arc::new(CountingObserver::default());

    let trigger = OrderPlaced { order_id: Uuid::new_v4(), occurred_at: Utc::now() };
    let trigger_id = append_trigger(&store, &trigger).await;
    let trigger_pos = EventLogBackend::read_all(store.as_ref(), LogCursor::ZERO, 10)
        .await
        .unwrap()
        .iter()
        .find(|e| e.event_id == trigger_id)
        .unwrap()
        .position;

    // ── 1. First delivery: normal, fully-acked success.
    deliver(&store, &decisions, &calls, &obs, CONSUMER, trigger_pos,
            "first delivery").await;
    assert_eq!(calls.load(Ordering::SeqCst), 1, "body ran once");
    assert!(
        decisions.get(CONSUMER, trigger_id).await.unwrap().is_some(),
        "decision D1 sealed",
    );
    // The durable floor HAS passed the trigger — the record is now
    // age-GC-able even under A1's floor-minimum bound.
    assert!(
        CheckpointStore::get(store.as_ref(), CONSUMER).await.unwrap()
            .map_or(false, |c| c >= trigger_pos),
        "ack floor persisted past the trigger",
    );

    // ── 2. Retention GC (days later): the engine-sweep primitive, legal
    //       under A1 (floor passed AND window elapsed — compressed here).
    let removed = decisions
        .remove_reclaimable(
            CONSUMER,
            Utc::now() + chrono::Duration::seconds(1),
            trigger_pos,
        )
        .await
        .unwrap();
    assert_eq!(removed, 1, "D1 GC'd by the age sweep");

    // ── 3. Checkpoint regression (PG restore / operator truncate — the
    //       exact scenario A3 names) redelivers the trigger. Get-miss →
    //       body re-decides nonce=1 under the SAME event_id → seal D2 →
    //       append diverges → accept-and-advance keeps the canonical
    //       nonce-0 row. This is A3's intended behavior and must NOT park.
    CheckpointStore::set(store.as_ref(), CONSUMER, LogCursor::ZERO).await.unwrap();
    deliver(&store, &decisions, &calls, &obs, CONSUMER, trigger_pos,
            "regression redelivery (record GC'd)").await;
    assert_eq!(calls.load(Ordering::SeqCst), 2, "body re-decided (record was gone)");
    assert_eq!(
        obs.divergences.load(Ordering::SeqCst), 1,
        "accept-and-advance fired the divergence warn",
    );
    assert_eq!(
        obs.terminal_failures.load(Ordering::SeqCst), 0,
        "accept-and-advance did not park (A3)",
    );
    let all = EventLogBackend::read_all(store.as_ref(), LogCursor::ZERO, 50).await.unwrap();
    let reminders: Vec<_> = all.iter().filter(|e| e.event_type == "reminder").collect();
    assert_eq!(reminders.len(), 1, "log kept the single canonical row");
    assert_eq!(
        reminders[0].payload["nonce"], serde_json::json!(0),
        "canonical row is the FIRST decision's nonce-0",
    );
    // Diagnostic: what does the store now claim?
    if let Some(rec) = decisions.get(CONSUMER, trigger_id).await.unwrap() {
        for o in &rec.outputs {
            println!(
                "post-accept sealed record asserts {} payload={} — log holds nonce=0",
                o.event_type, o.payload,
            );
        }
    } else {
        println!("post-accept: no record (reconciled by removal)");
    }

    // ── 4. A ROUTINE later redelivery: crash between ack and floor persist
    //       (the at-least-once window promise 1 explicitly sanctions).
    CheckpointStore::set(store.as_ref(), CONSUMER, LogCursor::ZERO).await.unwrap();
    deliver(&store, &decisions, &calls, &obs, CONSUMER, trigger_pos,
            "routine redelivery after accept").await;

    let all = EventLogBackend::read_all(store.as_ref(), LogCursor::ZERO, 50).await.unwrap();
    let park_facts: Vec<_> = all
        .iter()
        .filter(|e| e.event_type == REACTION_FAILED_KIND)
        .collect();
    println!(
        "after routine redelivery: body ran {} time(s) total; terminal_failures={}; \
         {} causal:reaction_failed fact(s) in the log",
        calls.load(Ordering::SeqCst),
        obs.terminal_failures.load(Ordering::SeqCst),
        park_facts.len(),
    );
    if let Some(err) = obs.last_terminal_error.lock().as_ref() {
        println!("terminal error: {err}");
    }
    for f in &park_facts {
        println!("spurious park fact in subject history: {}", f.payload);
    }

    // ── THE PROMISE: a trigger whose reaction SUCCEEDED must never be
    //    poison-parked as an integrity violation by a routine redelivery,
    //    and no failure fact may enter its subject history.
    assert_eq!(
        calls.load(Ordering::SeqCst), 2,
        "body must not run a third time",
    );
    assert_eq!(
        obs.terminal_failures.load(Ordering::SeqCst), 0,
        "a SUCCEEDED trigger was poison-parked as a fake integrity violation — \
         the accept-and-advance path left a sealed record contradicting the log",
    );
    assert!(
        park_facts.is_empty(),
        "a spurious causal:reaction_failed fact was appended into the subject \
         history of a trigger that succeeded",
    );
}
