//! Audit v14 — EngineBuilder::build() clamp TOCTOU.
//!
//! build() snapshots the log tip, then calls `clamp_ahead_of(tip)` on both
//! checkpoint stores with no lease and no atomicity. A live peer engine that
//! appends events and monotonically advances its cursor *between* the tip
//! read and the clamp gets its legitimately-advanced cursor regressed
//! (misclassified as "restored past the tip").
//!
//! The wrapper store below does not change any store semantics — it only
//! schedules a perfectly legal peer action (append + monotonic advance)
//! inside the unguarded window between build()'s two awaits, which in a
//! real deployment is a Kurrent RPC return followed by a PG UPDATE.
//!
//! CORRECT behavior: the peer's cursor (== live tip) survives the boot.
//! DEFECT: the cursor is clamped back down to the booting engine's stale
//! tip snapshot.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use uuid::Uuid;

use causal::checkpoint_store::{CheckpointStore, ReactorCheckpoint};
use causal::event_log::EventLogBackend;
use causal::memory_store::MemoryStore;
use causal::types::{EventData, LogCursor, StreamState};
use causal::EngineBuilder;

fn ev() -> EventData {
    EventData {
        event_id: Uuid::new_v4(),
        causation_id: None,
        workflow_id: Uuid::new_v4(),
        event_type: "audit:thing".into(),
        payload: serde_json::json!({}),
        created_at: chrono::Utc::now(),
        category: Some("audit".into()),
        subject_id: Some(Uuid::nil()),
        metadata: serde_json::Map::new(),
        ephemeral: None,
        persistent: true,
    }
}

/// Delegating CheckpointStore that, on the FIRST `clamp_ahead_of` call
/// (i.e., inside build()'s window after the tip was already snapshotted),
/// performs the peer's legal concurrent activity: append 3 events to the
/// shared log and monotonically advance the peer's cursor to the new tip.
struct RacingCheckpoint {
    store: Arc<MemoryStore>,
    fired: AtomicBool,
}

#[async_trait]
impl CheckpointStore for RacingCheckpoint {
    async fn get(&self, consumer_id: &str) -> Result<Option<LogCursor>> {
        CheckpointStore::get(&*self.store, consumer_id).await
    }

    async fn set(&self, consumer_id: &str, pos: LogCursor) -> Result<()> {
        CheckpointStore::set(&*self.store, consumer_id, pos).await
    }

    async fn advance(&self, consumer_id: &str, pos: LogCursor) -> Result<()> {
        CheckpointStore::advance(&*self.store, consumer_id, pos).await
    }

    async fn list_consumers(&self) -> Result<Vec<String>> {
        CheckpointStore::list_consumers(&*self.store).await
    }

    async fn clamp_ahead_of(&self, tip: LogCursor) -> Result<u64> {
        if !self.fired.swap(true, Ordering::SeqCst) {
            // ── the TOCTOU window ──
            // build() already read latest_position() == `tip`. The live
            // peer now does what it does all day: append outputs and
            // advance its cursor (monotonic, hot-path-legal).
            for _ in 0..3 {
                self.store
                    .append_to_stream("audit", Uuid::nil(), StreamState::Any, vec![ev()])
                    .await?;
            }
            let new_tip = EventLogBackend::latest_position(&*self.store).await?;
            CheckpointStore::advance(&*self.store, "peer-projector", new_tip).await?;
        }
        CheckpointStore::clamp_ahead_of(&*self.store, tip).await
    }
}

#[tokio::test]
async fn build_clamp_must_not_regress_concurrently_advanced_peer_cursor() -> Result<()> {
    let store = Arc::new(MemoryStore::new());

    // Peer engine A has processed the whole log: 5 events, durable cursor
    // at the true tip.
    for _ in 0..5 {
        store
            .append_to_stream("audit", Uuid::nil(), StreamState::Any, vec![ev()])
            .await?;
    }
    let t0 = EventLogBackend::latest_position(&*store).await?;
    CheckpointStore::advance(&*store, "peer-projector", t0).await?;

    let racing = Arc::new(RacingCheckpoint {
        store: store.clone(),
        fired: AtomicBool::new(false),
    });

    // Engine B boots against the same durable stores (deploy overlap /
    // second service sharing the DB). No consumers registered — the clamp
    // block in build() runs unconditionally.
    let _engine = tokio::time::timeout(
        Duration::from_secs(10),
        EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            racing.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        )
        .build(),
    )
    .await
    .expect("build wedged")?;

    let cursor = CheckpointStore::get(&*store, "peer-projector")
        .await?
        .expect("peer cursor exists");
    let live_tip = EventLogBackend::latest_position(&*store).await?;

    assert_eq!(
        cursor, live_tip,
        "a booting engine's stale-tip clamp regressed a live peer's \
         legitimately-advanced cursor: cursor={:?} live_tip={:?} \
         (stale snapshot was {:?}) — redelivers ({:?}, {:?}] on handover",
        cursor, live_tip, t0, t0, live_tip,
    );
    Ok(())
}
