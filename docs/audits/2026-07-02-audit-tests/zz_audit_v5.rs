//! Audit verifier test (FINDING #5): durable restore (`replay_events_onto`)
//! vs live fold (`apply_event`) must fold the SAME event set.
//!
//! Live fold skips a fact when the aggregator's `id_fn` returns `None`
//! (documented "skip this aggregator on this fact" semantics). Durable
//! restore replays the stream with `replay_events_onto`, which matches only
//! on (aggregate_type, event_prefix) and never consults the id extractor.
//! If those disagree, aggregate state is no longer a pure fold of the log:
//! it depends on whether the process was up when the event arrived.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::time::timeout;
use uuid::Uuid;

use causal::aggregate::{Aggregate, Apply};
use causal::aggregator::Aggregator;
use causal::{
    CheckpointStore, Engine, EngineBuilder, Event, EventLogBackend, MemoryStore,
    ReactorCheckpoint, SnapshotStore,
};

// One fact type; `draft: true` marks facts the aggregator's id_fn skips.
#[derive(Clone, Serialize, Deserialize)]
struct Tick {
    account: Uuid,
    draft: bool,
    amount: i64,
}
impl Event for Tick {
    const NAME: &'static str = "audit5_tick";
    const SUBJECT: &'static str = "audit5_acct";
    fn subject_id(&self) -> Uuid {
        self.account
    }
}

#[derive(Default, Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Total {
    value: i64,
}
impl Aggregate for Total {
    const NAME: &'static str = "Audit5Total";
    // Restorable: state survives restart via snapshot store + stream replay.
    const SUBJECT: &'static str = "audit5_acct";
}
impl Apply<Tick> for Total {
    fn apply(&mut self, e: &Tick) {
        self.value += e.amount;
    }
}

async fn build(mem: &Arc<MemoryStore>) -> Engine {
    EngineBuilder::new(
        mem.clone() as Arc<dyn EventLogBackend>,
        mem.clone() as Arc<dyn CheckpointStore>,
        mem.clone() as Arc<dyn ReactorCheckpoint>,
    )
    .with_aggregators(vec![Aggregator::for_type_with_id_fn::<Total, Tick, _>(
        // Documented skip semantics: None => this aggregator does not fold
        // this fact. Stream-aligned otherwise (id == subject_id).
        |t: &Tick| if t.draft { None } else { Some(t.account) },
    )])
    // Snapshot store wired so `state_of` does read-through restore; the
    // threshold is high so NO snapshot is written — restore replays from
    // genesis, which is exactly the pure-fold-of-the-log promise.
    .with_snapshot_store(mem.clone() as Arc<dyn SnapshotStore>)
    .with_snapshot_every(1_000)
    .build()
    .await
    .unwrap()
}

/// Replay determinism: state after a restart must equal state before it.
/// CORRECT behavior passes; the defect (restore applying id_fn-skipped
/// events) makes the final assertion fail.
#[tokio::test]
async fn restore_folds_the_same_event_set_as_live() -> Result<()> {
    let mem = Arc::new(MemoryStore::new());
    let acct = Uuid::new_v4();

    // ── Life 1: live fold ────────────────────────────────────────────
    let live = {
        let engine = build(&mem).await;
        timeout(Duration::from_secs(10), async {
            engine
                .emit(Tick { account: acct, draft: false, amount: 100 })
                .await?;
            engine
                .emit(Tick { account: acct, draft: true, amount: 1_000 })
                .await?; // id_fn -> None: live fold skips this fact
            engine
                .emit(Tick { account: acct, draft: false, amount: 20 })
                .await?;
            anyhow::Ok(())
        })
        .await??;
        let live = timeout(Duration::from_secs(10), engine.state_of::<Total>(acct))
            .await??
            .expect("live state present");
        timeout(Duration::from_secs(10), engine.shutdown()).await??;
        live
    };
    assert_eq!(
        live,
        Total { value: 120 },
        "live fold must skip the draft fact (id_fn returned None)"
    );

    // ── Life 2: restart — fresh engine over the SAME durable store ──
    let engine2 = build(&mem).await;
    let restored = timeout(Duration::from_secs(10), engine2.state_of::<Total>(acct))
        .await??
        .expect("restored state present");
    timeout(Duration::from_secs(10), engine2.shutdown()).await??;

    assert_eq!(
        restored, live,
        "replay determinism violated: restore folded a different event set \
         than the live fold (restore must skip exactly what live skipped)"
    );
    Ok(())
}
