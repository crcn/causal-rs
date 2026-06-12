//! Acceptance suite for the partitioned runner (0.10 step 2).
//!
//! These tests define what the partitioned runner must make true. The
//! `#[ignore]`d ones are RED against today's serial runner — written
//! first, per the plan, so the build has a gate instead of a hope.
//! Un-ignore them as the runner lands; the non-ignored ones pin
//! behavior that must SURVIVE the rewrite.
//!
//! Run the red set: cargo test -p causal --test partitioned_runner_acceptance -- --ignored

use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::time::timeout;
use uuid::Uuid;

use causal::checkpoint_store::{CheckpointStore, ReactorCheckpoint};
use causal::contexts::Ctx;
use causal::event::Event;
use causal::event_log::EventLogBackend;
use causal::memory_store::MemoryStore;
use causal::reactor::{Events, Reactor};
use causal::EngineBuilder;

fn backend(store: &Arc<MemoryStore>) -> EngineBuilder {
    EngineBuilder::new(
        store.clone() as Arc<dyn EventLogBackend>,
        store.clone() as Arc<dyn CheckpointStore>,
        store.clone() as Arc<dyn ReactorCheckpoint>,
    )
}

// ── facts ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SlowJob { id: Uuid, occurred_at: DateTime<Utc> }
impl Event for SlowJob {
    const NAME: &'static str = "acc_slow_job";
    fn subject_id(&self) -> Uuid { self.id }
    fn occurred_at(&self) -> Option<DateTime<Utc>> { Some(self.occurred_at) }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FastPing { id: Uuid, occurred_at: DateTime<Utc> }
impl Event for FastPing {
    const NAME: &'static str = "acc_fast_ping";
    fn subject_id(&self) -> Uuid { self.id }
    fn occurred_at(&self) -> Option<DateTime<Utc>> { Some(self.occurred_at) }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FastPong { id: Uuid }
impl Event for FastPong {
    const NAME: &'static str = "acc_fast_pong";
    fn subject_id(&self) -> Uuid { self.id }
}

// ── reactors ─────────────────────────────────────────────────────────

/// Wedges forever on the first trigger it sees (a 4-minute Whisper
/// call, idealized). Releasable via the Notify for orderly shutdown.
struct Wedge {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}
#[async_trait]
impl Reactor for Wedge {
    type Trigger = SlowJob;
    const NAME: &'static str = "acc.wedge";
    async fn react(&self, _t: &SlowJob, _ctx: Ctx<'_>) -> Result<Events> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(Events::new())
    }
}

struct FastEcho;
#[async_trait]
impl Reactor for FastEcho {
    type Trigger = FastPing;
    const NAME: &'static str = "acc.fast_echo";
    async fn react(&self, t: &FastPing, _ctx: Ctx<'_>) -> Result<Events> {
        Ok(causal::events![FastPong { id: t.id }])
    }
}

/// Barrier reactor: returns only once `need` invocations are in-flight
/// SIMULTANEOUSLY — the direct probe for cross-partition concurrency.
struct Barrier {
    need: u64,
    inside: Arc<AtomicU64>,
    peak: Arc<AtomicU64>,
    gate: Arc<tokio::sync::Notify>,
}
#[async_trait]
impl Reactor for Barrier {
    type Trigger = FastPing;
    const NAME: &'static str = "acc.barrier";
    async fn react(&self, _t: &FastPing, _ctx: Ctx<'_>) -> Result<Events> {
        let now = self.inside.fetch_add(1, AtomicOrdering::SeqCst) + 1;
        self.peak.fetch_max(now, AtomicOrdering::SeqCst);
        if now >= self.need {
            self.gate.notify_waiters();
        } else {
            // wait (bounded) for the others — under a serial runner this
            // times out because the second trigger can never start.
            let _ = timeout(Duration::from_secs(2), self.gate.notified()).await;
        }
        self.inside.fetch_sub(1, AtomicOrdering::SeqCst);
        Ok(Events::new())
    }
}

// ─────────────────────────────────────────────────────────────────────
// RED until step 2 — the partitioned runner's definition of done
// ─────────────────────────────────────────────────────────────────────

/// THE settle-hostage acceptance: a reactor wedged on an UNRELATED
/// workflow's trigger must not delay another workflow's `settled()`.
/// Red today because settle waits on every consumer's global cursor,
/// and acc.wedge's cursor can never pass the fast workflow's
/// high-water while wedged.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "RED until 0.10 step 2 (partitioned runner) lands"]
async fn settle_is_not_hostage_to_an_unrelated_wedged_reactor() {
    let store = Arc::new(MemoryStore::new());
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let engine = backend(&store)
        .with_reactor(Wedge { entered: entered.clone(), release: release.clone() })
        .with_reactor(FastEcho)
        .build().await.unwrap();

    // Wedge the slow reactor on its own workflow…
    engine.emit(SlowJob { id: Uuid::new_v4(), occurred_at: Utc::now() }).await.unwrap();
    entered.notified().await;

    // …and settle an unrelated fast workflow. Must complete promptly.
    let r = timeout(Duration::from_secs(5), async {
        engine.emit(FastPing { id: Uuid::new_v4(), occurred_at: Utc::now() })
            .settled().await
    }).await;
    release.notify_one(); // free the wedge before asserting
    r.expect("settled() was hostage to an unrelated wedged reactor")
        .expect("settled emit failed");
    engine.shutdown().await.unwrap();
}

/// Emit twice ⇒ two partitions ⇒ both reactor invocations in flight at
/// once. The barrier proves real overlap, not interleaving.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "RED until 0.10 step 2 (partitioned runner) lands"]
async fn two_emits_run_concurrently_in_the_same_reactor() {
    let store = Arc::new(MemoryStore::new());
    let inside = Arc::new(AtomicU64::new(0));
    let peak = Arc::new(AtomicU64::new(0));
    let engine = backend(&store)
        .with_reactor(Barrier {
            need: 2,
            inside: inside.clone(),
            peak: peak.clone(),
            gate: Arc::new(tokio::sync::Notify::new()),
        })
        .build().await.unwrap();

    let a = engine.emit(FastPing { id: Uuid::new_v4(), occurred_at: Utc::now() }).settled();
    let b = engine.emit(FastPing { id: Uuid::new_v4(), occurred_at: Utc::now() }).settled();
    let (ra, rb) = tokio::join!(
        timeout(Duration::from_secs(10), a),
        timeout(Duration::from_secs(10), b),
    );
    ra.expect("settle a timed out").unwrap();
    rb.expect("settle b timed out").unwrap();
    assert!(
        peak.load(AtomicOrdering::SeqCst) >= 2,
        "two independent emits never overlapped in the reactor \
         (peak in-flight = {})",
        peak.load(AtomicOrdering::SeqCst),
    );
    engine.shutdown().await.unwrap();
}

/// A poison trigger (no terminal-failure mapper) wedges ONLY its own
/// partition; other subjects keep flowing and settling.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "RED until 0.10 step 2 (partitioned runner) lands"]
async fn poison_isolates_to_its_partition() {
    let store = Arc::new(MemoryStore::new());
    let engine = backend(&store)
        .with_reactor(FastEcho)
        .build().await.unwrap();

    // Poison: an acc_fast_ping-kind event whose payload can't deserialize.
    let poison = causal::types::EventData {
        event_id: Uuid::new_v4(),
        causation_id: None,
        workflow_id: Uuid::new_v4(),
        event_type: "acc_fast_ping".into(),
        payload: serde_json::json!({ "not": "a ping" }),
        created_at: Utc::now(),
        category: Some("acc_fast_ping".into()),
        subject_id: Some(Uuid::new_v4()),
        metadata: serde_json::Map::new(),
        ephemeral: None,
        persistent: true,
    };
    causal::append_event(store.as_ref(), poison).await.unwrap();

    // A healthy ping on a DIFFERENT subject must still settle.
    let r = timeout(Duration::from_secs(5), async {
        engine.emit(FastPing { id: Uuid::new_v4(), occurred_at: Utc::now() })
            .settled().await
    }).await;
    r.expect("a poison event on another subject wedged this workflow's settle")
        .expect("settled emit failed");
    engine.shutdown().await.unwrap();
}

// ─────────────────────────────────────────────────────────────────────
// MUST SURVIVE the rewrite — green today, stay green
// ─────────────────────────────────────────────────────────────────────

/// Same subject ⇒ log order, regardless of runner internals
/// (Ordering::PerSubject, the memo's default).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn same_subject_triggers_process_in_log_order() {
    #[derive(Default, Clone)]
    struct Recorder { seen: Arc<parking_lot::Mutex<Vec<DateTime<Utc>>>> }
    #[async_trait]
    impl Reactor for Recorder {
        type Trigger = FastPing;
        const NAME: &'static str = "acc.recorder";
        async fn react(&self, t: &FastPing, _ctx: Ctx<'_>) -> Result<Events> {
            self.seen.lock().push(t.occurred_at);
            Ok(Events::new())
        }
    }
    let store = Arc::new(MemoryStore::new());
    let rec = Recorder::default();
    let seen = rec.seen.clone();
    let engine = backend(&store).with_reactor(rec).build().await.unwrap();

    let subject = Uuid::new_v4();
    let t0 = Utc::now();
    for i in 0..10 {
        engine.emit(FastPing {
            id: subject,
            occurred_at: t0 + chrono::Duration::milliseconds(i),
        }).await.unwrap();
    }
    engine.emit(FastPing { id: subject, occurred_at: t0 + chrono::Duration::milliseconds(10) })
        .settled().await.unwrap();

    let got = seen.lock().clone();
    let mut sorted = got.clone();
    sorted.sort();
    assert_eq!(got, sorted, "one subject's triggers must process in log order");
    assert_eq!(got.len(), 11);
    engine.shutdown().await.unwrap();
}

/// Crash redelivery dedups: the identity-keyed output derivation makes
/// a re-run byte-identical, so the log holds exactly one output.
#[tokio::test]
async fn redelivery_after_cursor_rewind_mints_nothing() {
    let store = Arc::new(MemoryStore::new());
    let engine = backend(&store).with_reactor(FastEcho).build().await.unwrap();

    engine.emit(FastPing { id: Uuid::new_v4(), occurred_at: Utc::now() })
        .settled().await.unwrap();

    // Simulate crash-before-checkpoint: rewind the reactor cursor.
    CheckpointStore::set(store.as_ref(), "acc.fast_echo", causal::types::LogCursor::ZERO)
        .await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await; // let it re-run

    let pongs = store.global_log().iter()
        .filter(|e| e.event_type == "acc_fast_pong")
        .count();
    assert_eq!(pongs, 1, "redelivery must dedup, not duplicate");
    engine.shutdown().await.unwrap();
}
