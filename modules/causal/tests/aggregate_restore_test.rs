//! Durable aggregate restore (read-through, revision-based) over `MemoryStore`
//! as both the event log and the snapshot store.
//!
//! Exercises the spec's acceptance scenarios: restart restore, snapshot
//! acceleration == full replay, self-heal on a bad snapshot, and unchanged
//! behavior with no snapshot store.

use std::sync::Arc;

use anyhow::Result;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use causal::aggregate::{Aggregate, Apply};
use causal::aggregator::Aggregator;
use causal::types::{Snapshot, StreamRevision};
use causal::{
    CheckpointStore, Engine, EngineBuilder, Event, EventLogBackend, MemoryStore,
    ReactorCheckpoint, SnapshotStore,
};

// ── Fixtures: two DISTINCT event types co-located in ONE stream ──────────
// Both override STREAM_CATEGORY to "account" (shared stream) while keeping
// distinct routing categories ("deposit" / "withdraw").

#[derive(Clone, Serialize, Deserialize)]
struct Deposited {
    account: Uuid,
    amount: i64,
}
impl Event for Deposited {
    const CATEGORY: &'static str = "deposit";
    const STREAM_CATEGORY: &'static str = "account";
    fn event_type(&self) -> &str { "v1" }
    fn stream_id(&self) -> Uuid { self.account }
}

#[derive(Clone, Serialize, Deserialize)]
struct Withdrawn {
    account: Uuid,
    amount: i64,
}
impl Event for Withdrawn {
    const CATEGORY: &'static str = "withdraw";
    const STREAM_CATEGORY: &'static str = "account";
    fn event_type(&self) -> &str { "v1" }
    fn stream_id(&self) -> Uuid { self.account }
}

#[derive(Default, Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Balance {
    value: i64,
}
impl Aggregate for Balance {
    const NAME: &'static str = "Balance";
    const STREAM_CATEGORY: &'static str = "account";
}
impl Apply<Deposited> for Balance {
    fn apply(&mut self, e: &Deposited) { self.value += e.amount; }
}
impl Apply<Withdrawn> for Balance {
    fn apply(&mut self, e: &Withdrawn) { self.value -= e.amount; }
}

fn build(mem: &Arc<MemoryStore>, store: Option<Arc<MemoryStore>>, every: u64) -> Engine {
    let mut b = EngineBuilder::new(
        mem.clone() as Arc<dyn EventLogBackend>,
        mem.clone() as Arc<dyn CheckpointStore>,
        mem.clone() as Arc<dyn ReactorCheckpoint>,
    )
    .with_aggregators(vec![
        Aggregator::for_type::<Balance, Deposited>(),
        Aggregator::for_type::<Balance, Withdrawn>(),
    ]);
    if let Some(s) = store {
        b = b
            .with_snapshot_store(s as Arc<dyn SnapshotStore>)
            .with_snapshot_every(every);
    }
    b.build()
}

// 1. Restart restore — the bug this feature fixes. Engine #2 (fresh registry,
//    same store) must recover the FULL state folded by engine #1.
#[tokio::test]
async fn restart_restores_full_aggregate_state() -> Result<()> {
    let mem = Arc::new(MemoryStore::new());
    let acct = Uuid::new_v4();

    {
        let engine = build(&mem, Some(mem.clone()), 100);
        engine.emit(Deposited { account: acct, amount: 100 }).await?;
        engine.emit(Deposited { account: acct, amount: 50 }).await?;
        engine.emit(Withdrawn { account: acct, amount: 30 }).await?;
        assert_eq!(
            engine.snapshot::<Balance>(acct),
            Some(Balance { value: 120 }),
            "engine #1 in-memory fold"
        );
        engine.shutdown().await?;
    }

    // Engine #2 — fresh registry, SAME MemoryStore.
    let engine2 = build(&mem, Some(mem.clone()), 100);
    let restored = engine2.load_aggregate::<Balance>(acct).await?;
    assert_eq!(
        restored,
        Some(Balance { value: 120 }),
        "engine #2 must recover full aggregate state after restart"
    );
    Ok(())
}

// 2. A snapshot accelerates restore but does not change the result —
//    snapshot + tail == full replay from genesis.
#[tokio::test]
async fn snapshot_accelerates_without_changing_result() -> Result<()> {
    let mem = Arc::new(MemoryStore::new());
    let acct = Uuid::new_v4();

    let engine = build(&mem, Some(mem.clone()), 3); // snapshot every 3 folds
    let mut expected = 0i64;
    for _ in 0..10 {
        engine.emit(Deposited { account: acct, amount: 10 }).await?;
        expected += 10;
    }
    // A snapshot must have been saved (>= 3 events folded).
    let snap = SnapshotStore::load_snapshot(mem.as_ref(), "Balance", acct).await?;
    assert!(snap.is_some(), "expected a saved snapshot after >= 3 events");
    engine.shutdown().await?;

    // Engine #2 restores via snapshot + tail.
    let engine2 = build(&mem, Some(mem.clone()), 3);
    let restored = engine2.load_aggregate::<Balance>(acct).await?;
    assert_eq!(
        restored,
        Some(Balance { value: expected }),
        "snapshot + tail must equal a full from-genesis fold"
    );
    Ok(())
}

// 3. A corrupt/incompatible snapshot self-heals: delete + rebuild from genesis,
//    result still correct.
#[tokio::test]
async fn corrupt_snapshot_self_heals() -> Result<()> {
    let mem = Arc::new(MemoryStore::new());
    let acct = Uuid::new_v4();

    {
        let engine = build(&mem, Some(mem.clone()), 100);
        engine.emit(Deposited { account: acct, amount: 100 }).await?;
        engine.emit(Withdrawn { account: acct, amount: 40 }).await?;
        engine.shutdown().await?;
    }

    // Inject a snapshot whose blob cannot deserialize as `Balance`.
    SnapshotStore::save_snapshot(
        mem.as_ref(),
        Snapshot {
            aggregate_type: "Balance".into(),
            aggregate_id: acct,
            revision: StreamRevision::from_raw(1),
            state: serde_json::json!("not a balance object"),
            created_at: Utc::now(),
        },
    )
    .await?;

    let engine2 = build(&mem, Some(mem.clone()), 100);
    let restored = engine2.load_aggregate::<Balance>(acct).await?;
    assert_eq!(
        restored,
        Some(Balance { value: 60 }),
        "self-heal: rebuild from genesis after a bad snapshot (100 - 40)"
    );
    let snap = SnapshotStore::load_snapshot(mem.as_ref(), "Balance", acct).await?;
    assert!(snap.is_none(), "corrupt snapshot must be deleted during self-heal");
    Ok(())
}

// 5. The `#[event(..., stream = "...")]` macro generates a `STREAM_CATEGORY`
//    distinct from the routing `CATEGORY`; omitting it defaults to `CATEGORY`.
#[test]
fn event_macro_stream_attribute_sets_stream_category() {
    #[causal::event(prefix = "deposit", stream_id = "account", stream = "ledger")]
    #[derive(Clone, Serialize, Deserialize)]
    struct MacroDeposited {
        account: Uuid,
        occurred_at: chrono::DateTime<Utc>,
    }

    #[causal::event(prefix = "withdraw", stream_id = "account")]
    #[derive(Clone, Serialize, Deserialize)]
    struct MacroWithdrawn {
        account: Uuid,
        occurred_at: chrono::DateTime<Utc>,
    }

    assert_eq!(<MacroDeposited as Event>::CATEGORY, "deposit", "routing category");
    assert_eq!(
        <MacroDeposited as Event>::STREAM_CATEGORY, "ledger",
        "stream = co-located placement category"
    );
    // No `stream` → STREAM_CATEGORY defaults to CATEGORY.
    assert_eq!(<MacroWithdrawn as Event>::CATEGORY, "withdraw");
    assert_eq!(<MacroWithdrawn as Event>::STREAM_CATEGORY, "withdraw");
}

// 4. No snapshot store → unchanged behavior (no read-through restore).
#[tokio::test]
async fn without_snapshot_store_behavior_unchanged() -> Result<()> {
    let mem = Arc::new(MemoryStore::new());
    let acct = Uuid::new_v4();

    {
        let engine = build(&mem, None, 0); // no snapshot store
        engine.emit(Deposited { account: acct, amount: 100 }).await?;
        assert_eq!(
            engine.snapshot::<Balance>(acct),
            Some(Balance { value: 100 }),
            "in-memory fold still works with no store"
        );
        engine.shutdown().await?;
    }

    // Fresh engine, no store → load_aggregate does NOT restore.
    let engine2 = build(&mem, None, 0);
    assert_eq!(
        engine2.load_aggregate::<Balance>(acct).await?,
        None,
        "without a snapshot store, load_aggregate does not restore (unchanged)"
    );
    let snap = SnapshotStore::load_snapshot(mem.as_ref(), "Balance", acct).await?;
    assert!(snap.is_none(), "no snapshot store → no snapshots ever saved");
    Ok(())
}
