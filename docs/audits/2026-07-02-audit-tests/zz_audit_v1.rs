//! AUDIT REPRO (finding #1): retention GC ignores the ack-floor.
//!
//! Amendment A1 (docs/plans/2026-07-02-decision-records-design.md:202-210)
//! requires the sweep to honor the floor as a MINIMUM bound — "never remove
//! a record the floor hasn't passed". The shipped sweep
//! (engine.rs:1885-1901) is purely age-based:
//!
//!     let cutoff = clock.now() - window;
//!     store.remove_sealed_before(cutoff).await
//!
//! and the trait signature `remove_sealed_before(cutoff)` cannot express a
//! floor. This test drives the exact sequence:
//!
//!   1. First delivery: body runs, decision sealed, output appended — but
//!      the ack persist is lost (crash between append and checkpoint, the
//!      at-least-once redelivery window promise 1 sanctions).
//!   2. The engine-formula sweep runs while the durable floor is still
//!      BELOW the trigger (window elapsed: wedged partition > window,
//!      engine down > window while a peer sweeps the shared table, or a
//!      short/zero window — with_decision_retention accepts any Duration
//!      unvalidated).
//!   3. Redelivery: replay gate get-misses, the nondeterministic body
//!      re-runs and decides DIFFERENTLY (different output kind → fresh
//!      identity-keyed event_id → clean append, no divergence signal).
//!
//! CORRECT behavior (A1's floor-minimum bound implemented): the record
//! survives step 2, the replay gate hits at step 3, the body never re-runs,
//! the log holds exactly one output. The assertions below encode that; the
//! shipped code makes them FAIL with a merged two-decision batch.

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
use causal::reactor_runner::ReactorRunner;
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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OutA {
    order_id: Uuid,
}
impl Event for OutA {
    const NAME: &'static str = "audit_out_a";
    fn subject_id(&self) -> Uuid {
        self.order_id
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OutB {
    order_id: Uuid,
}
impl Event for OutB {
    const NAME: &'static str = "audit_out_b";
    fn subject_id(&self) -> Uuid {
        self.order_id
    }
}

/// Nondeterministic body: first invocation decides kind A, any re-run
/// decides kind B (a time/feature-flag/external-call branch — exactly the
/// nondeterminism decision records exist to make harmless). Different KIND
/// means a different identity-keyed event_id, so the re-decide appends
/// CLEANLY next to the first decision's row: no divergence, no signal.
struct BranchingEmit {
    calls: Arc<AtomicUsize>,
}
#[async_trait]
impl Reactor for BranchingEmit {
    type Trigger = OrderPlaced;
    const NAME: &'static str = "audit-branching";
    async fn react(&self, trigger: &OrderPlaced, _ctx: Ctx<'_>) -> Result<Events> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let mut out = Events::new();
        if n == 0 {
            out.push(OutA { order_id: trigger.order_id });
        } else {
            out.push(OutB { order_id: trigger.order_id });
        }
        Ok(out)
    }
}

/// Checkpoint wrapper whose cursor writes vanish: models the crash between
/// the output append and the ack-floor persist (the runner believes the ack
/// landed; the durable cursor never moved). Attempt counters delegate.
struct LostAck {
    inner: Arc<MemoryStore>,
}
#[async_trait]
impl CheckpointStore for LostAck {
    async fn get(&self, consumer_id: &str) -> Result<Option<LogCursor>> {
        CheckpointStore::get(self.inner.as_ref(), consumer_id).await
    }
    async fn set(&self, _consumer_id: &str, _pos: LogCursor) -> Result<()> {
        Ok(()) // dropped on the floor — never durable
    }
    async fn advance(&self, _consumer_id: &str, _pos: LogCursor) -> Result<()> {
        Ok(()) // dropped on the floor — never durable
    }
}
#[async_trait]
impl ReactorCheckpoint for LostAck {
    async fn record_reactor_attempt(&self, consumer_id: &str, trigger_id: Uuid) -> Result<u32> {
        self.inner.record_reactor_attempt(consumer_id, trigger_id).await
    }
    async fn clear_reactor_attempts(&self, consumer_id: &str, trigger_id: Uuid) -> Result<()> {
        self.inner.clear_reactor_attempts(consumer_id, trigger_id).await
    }
}

/// Drive `runner.step` until `done()` reports true, with a hard deadline —
/// the public-API stand-in for the crate-internal `quiesce` test helper.
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
            // A few extra reap/persist passes so acks and floors settle.
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

#[tokio::test]
async fn gc_must_not_delete_records_the_floor_has_not_passed() {
    const CONSUMER: &str = "r.audit";

    let store = Arc::new(MemoryStore::new());
    let decisions = Arc::new(InMemoryDecisionStore::new());
    let calls = Arc::new(AtomicUsize::new(0));

    let trigger = OrderPlaced { order_id: Uuid::new_v4(), occurred_at: Utc::now() };
    let trigger_id = append_trigger(&store, &trigger).await;

    // ── 1. First delivery: seal + append succeed, ack persist is lost.
    {
        let lost_ack = Arc::new(LostAck { inner: store.clone() });
        let runner = ReactorRunner::new(
            BranchingEmit { calls: calls.clone() },
            CONSUMER,
            store.clone() as Arc<dyn EventLogBackend>,
            lost_ack as Arc<dyn ReactorCheckpoint>,
        )
        .with_decision_store(decisions.clone() as Arc<dyn DecisionStore>);
        let store_c = store.clone();
        let decisions_c = decisions.clone();
        drive(
            &runner,
            move || {
                let store = store_c.clone();
                let decisions = decisions_c.clone();
                Box::pin(async move {
                    let sealed = decisions.get(CONSUMER, trigger_id).await.unwrap().is_some();
                    let appended =
                        EventLogBackend::read_all(store.as_ref(), LogCursor::ZERO, 50)
                            .await
                            .unwrap()
                            .iter()
                            .any(|e| e.event_type == "audit_out_a");
                    sealed && appended
                })
            },
            "first delivery: seal + append",
        )
        .await;
        runner.halt();
    }
    assert!(
        decisions.get(CONSUMER, trigger_id).await.unwrap().is_some(),
        "decision sealed on first delivery",
    );
    let after_first = EventLogBackend::read_all(store.as_ref(), LogCursor::ZERO, 50)
        .await
        .unwrap();
    assert_eq!(
        after_first.iter().filter(|e| e.event_type == "audit_out_a").count(),
        1,
        "first decision's output appended",
    );
    let trigger_pos = after_first
        .iter()
        .find(|e| e.event_id == trigger_id)
        .expect("trigger in log")
        .position;
    // The durable floor never passed the trigger — it is still redeliverable.
    let durable_floor = CheckpointStore::get(store.as_ref(), CONSUMER).await.unwrap();
    assert!(
        durable_floor.is_none(),
        "durable ack-floor still below the trigger (redelivery window open)",
    );

    // ── 2. The retention sweep, exactly as engine.rs:1889-1890 computes it
    //       (`let cutoff = clock.now() - window; store.remove_sealed_before(cutoff)`).
    //       Window elapsed relative to sealed_at — here compressed via a
    //       zero window, which `with_decision_retention` accepts without
    //       validation; a 7-day window with a >7-day-unacked trigger (wedged
    //       partition / node down while a peer engine sweeps the shared
    //       table) computes the identical cutoff relationship.
    let window = chrono::Duration::from_std(Duration::ZERO).unwrap();
    let cutoff = Utc::now() - window;
    // CONTROL: set AUDIT_SKIP_SWEEP=1 to skip the sweep — the test must then
    // PASS (record survives → replay gate hits → no re-decide), proving the
    // harness only fails because of the GC deletion.
    let removed = if std::env::var("AUDIT_SKIP_SWEEP").is_ok() {
        0
    } else {
        decisions.remove_sealed_before(cutoff).await.unwrap()
    };
    println!(
        "sweep removed {removed} record(s) while the durable floor ({durable_floor:?}) \
         was still below the trigger — A1's floor-minimum bound is not applied",
    );

    // ── 3. Redelivery (restart): fresh runner, real checkpoint (reads the
    //       never-persisted floor → re-ingests from ZERO).
    {
        let runner = ReactorRunner::new(
            BranchingEmit { calls: calls.clone() },
            CONSUMER,
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        )
        .with_decision_store(decisions.clone() as Arc<dyn DecisionStore>);
        let store_c = store.clone();
        drive(
            &runner,
            move || {
                let store = store_c.clone();
                Box::pin(async move {
                    // Redelivery complete once the (real) durable floor
                    // passed the trigger — true on both the replay path and
                    // the re-decide path.
                    CheckpointStore::get(store.as_ref(), CONSUMER)
                        .await
                        .unwrap()
                        .map_or(false, |c| c >= trigger_pos)
                })
            },
            "redelivery: floor past the trigger",
        )
        .await;
        runner.halt();
    }

    let all = EventLogBackend::read_all(store.as_ref(), LogCursor::ZERO, 50)
        .await
        .unwrap();
    let out_a = all.iter().filter(|e| e.event_type == "audit_out_a").count();
    let out_b = all.iter().filter(|e| e.event_type == "audit_out_b").count();
    println!(
        "after redelivery: body ran {} time(s); log holds {out_a} audit_out_a + \
         {out_b} audit_out_b output(s) for ONE trigger",
        calls.load(Ordering::SeqCst),
    );
    if let Some(rec) = decisions.get(CONSUMER, trigger_id).await.unwrap() {
        let kinds: Vec<&str> = rec.outputs.iter().map(|o| o.event_type.as_str()).collect();
        println!("re-sealed decision record now says outputs = {kinds:?} — the log disagrees");
    }

    // ── Promise 2: one decision per trigger. With A1's floor bound the
    //    record survives, the replay gate hits, and both assertions hold.
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the reactor body must never re-run for a trigger whose decision was sealed \
         and which was still redeliverable (chimera reopened)",
    );
    assert_eq!(
        out_a + out_b,
        1,
        "exactly one decision's outputs may reach the log for one trigger \
         (merged chimera batch found instead)",
    );
}
