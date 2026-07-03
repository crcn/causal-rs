//! AUDIT VERIFICATION (finding #6): maybe_save_snapshots reads
//! (version, state) via two separate DashMap reads. A concurrent fold
//! between the reads persists Snapshot{revision: V-1, state@V+k}, and
//! restore then re-applies revisions V..V+k — silent double-count.
//!
//! CORRECT behavior: every persisted snapshot is internally consistent
//! (counter n == revision + 1, since every event increments by exactly 1
//! and folds exactly once). The defect makes the assertion FAIL.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use causal::aggregate::{Aggregate, Apply};
use causal::aggregator::Aggregator;
use causal::types::Snapshot;
use causal::{
    CheckpointStore, EngineBuilder, Event, EventLogBackend, MemoryStore,
    ReactorCheckpoint, SnapshotStore,
};

// ── Fixtures: a pure counter — one event type, +1 per fold ─────────────

#[derive(Clone, Serialize, Deserialize)]
struct Ticked {
    account: Uuid,
}
impl Event for Ticked {
    const NAME: &'static str = "audit6_tick";
    const SUBJECT: &'static str = "audit6acct";
    fn subject_id(&self) -> Uuid {
        self.account
    }
}

#[derive(Default, Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Count {
    n: i64,
}
impl Aggregate for Count {
    const NAME: &'static str = "Audit6Count";
    const SUBJECT: &'static str = "audit6acct";
}
impl Apply<Ticked> for Count {
    fn apply(&mut self, _e: &Ticked) {
        self.n += 1;
    }
}

// ── Checking snapshot store: flags any internally-inconsistent save ────
//
// Invariant of a CORRECT snapshot of this aggregate: state.n == revision+1
// (revisions are 0-based and dense; each event adds exactly 1; the registry
// gate folds each revision exactly once). A snapshot violating it captured
// a (version, state) pair from two different instants.

struct CheckingStore {
    inner: Arc<MemoryStore>,
    skewed: Mutex<Vec<Snapshot>>,
}

#[async_trait]
impl SnapshotStore for CheckingStore {
    async fn load_snapshot(
        &self,
        aggregate_type: &str,
        aggregate_id: Uuid,
    ) -> Result<Option<Snapshot>> {
        SnapshotStore::load_snapshot(self.inner.as_ref(), aggregate_type, aggregate_id).await
    }

    async fn save_snapshot(&self, snapshot: Snapshot) -> Result<()> {
        let n = snapshot
            .state
            .get("n")
            .and_then(|v| v.as_i64())
            .unwrap_or(i64::MIN);
        if n != snapshot.revision.raw() as i64 + 1 {
            self.skewed.lock().unwrap().push(snapshot.clone());
        }
        SnapshotStore::save_snapshot(self.inner.as_ref(), snapshot).await
    }

    async fn delete_snapshot(&self, aggregate_type: &str, aggregate_id: Uuid) -> Result<()> {
        SnapshotStore::delete_snapshot(self.inner.as_ref(), aggregate_type, aggregate_id).await
    }
}

const TASKS: usize = 8;
const EMITS_PER_TASK: usize = 1000;

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_folds_never_persist_skewed_snapshots() -> Result<()> {
    let mem = Arc::new(MemoryStore::new());
    let checker = Arc::new(CheckingStore {
        inner: mem.clone(),
        skewed: Mutex::new(Vec::new()),
    });

    let engine = Arc::new(
        EngineBuilder::new(
            mem.clone() as Arc<dyn EventLogBackend>,
            mem.clone() as Arc<dyn CheckpointStore>,
            mem.clone() as Arc<dyn ReactorCheckpoint>,
        )
        .with_aggregators(vec![Aggregator::for_type::<Count, Ticked>()])
        .with_snapshot_store(checker.clone() as Arc<dyn SnapshotStore>)
        .with_snapshot_every(1)
        .build()
        .await
        .unwrap(),
    );

    let acct = Uuid::new_v4();

    // Hammer: TASKS concurrent emitters to the SAME aggregate stream.
    // Every committed fold attempts a snapshot (snapshot_every = 1).
    let successes = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let mut handles = Vec::new();
    for _ in 0..TASKS {
        let e = engine.clone();
        let ok = successes.clone();
        handles.push(tokio::spawn(async move {
            for _ in 0..EMITS_PER_TASK {
                if e.emit(Ticked { account: acct }).await.is_ok() {
                    ok.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }));
    }
    for h in handles {
        tokio::time::timeout(Duration::from_secs(180), h)
            .await
            .expect("emitter task wedged")
            .expect("emitter task panicked");
    }
    let total = successes.load(std::sync::atomic::Ordering::Relaxed);
    println!("total successful emits: {total}");

    let skewed = checker.skewed.lock().unwrap().clone();
    println!("skewed snapshots persisted: {}", skewed.len());
    for s in skewed.iter().take(5) {
        println!(
            "  Snapshot{{ revision: {}, state: {} }}  (expected n == {})",
            s.revision.raw(),
            s.state,
            s.revision.raw() + 1,
        );
    }

    // End-to-end corruption demo: simulate a crash immediately after the
    // engine persisted a skewed snapshot (the store then holds exactly that
    // snapshot — a state the engine itself produced), restart, restore.
    if let Some(s) = skewed.first() {
        SnapshotStore::save_snapshot(mem.as_ref(), s.clone()).await?;
        let engine2 = EngineBuilder::new(
            mem.clone() as Arc<dyn EventLogBackend>,
            mem.clone() as Arc<dyn CheckpointStore>,
            mem.clone() as Arc<dyn ReactorCheckpoint>,
        )
        .with_aggregators(vec![Aggregator::for_type::<Count, Ticked>()])
        .with_snapshot_store(mem.clone() as Arc<dyn SnapshotStore>)
        .with_snapshot_every(1_000_000)
        .build()
        .await
        .unwrap();
        let restored = tokio::time::timeout(
            Duration::from_secs(60),
            engine2.state_of::<Count>(acct),
        )
        .await
        .expect("restore wedged")?;
        println!(
            "restored after crash-at-skewed-snapshot: {:?}; pure fold of the log = {}",
            restored, total,
        );
        let _ = engine2.shutdown().await;
    }

    if let Ok(engine) = Arc::try_unwrap(engine) {
        let _ = engine.shutdown().await;
    }

    assert!(
        skewed.is_empty(),
        "maybe_save_snapshots persisted {} snapshot(s) whose revision disagrees \
         with the state blob (first: revision {}, state {}); restore will \
         re-apply already-folded events — silent corruption",
        skewed.len(),
        skewed[0].revision.raw(),
        skewed[0].state,
    );
    Ok(())
}
