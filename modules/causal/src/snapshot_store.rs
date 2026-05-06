//! Snapshot persistence trait — v0.3 extraction from `EventLog`.
//!
//! In 0.2.x, snapshot read/write methods lived on the `EventLog` trait
//! itself. v0.3 extracts them into a separate `SnapshotStore` so the
//! snapshot backend can be swapped independently of the event log
//! backend (Postgres-backed log + S3-backed snapshots, Kurrent log +
//! Postgres snapshots, etc.).
//!
//! Existing `EventLog` impls automatically satisfy `SnapshotStore` via
//! a blanket impl that delegates to the corresponding methods, so no
//! existing backend needs code changes for Phase 1.

use anyhow::Result;
use async_trait::async_trait;
use uuid::Uuid;

use crate::event_log::EventLog;
use crate::types::Snapshot;

#[async_trait]
pub trait SnapshotStore: Send + Sync {
    async fn load_snapshot(
        &self,
        aggregate_type: &str,
        aggregate_id: Uuid,
    ) -> Result<Option<Snapshot>>;

    async fn save_snapshot(&self, snapshot: Snapshot) -> Result<()>;

    /// Delete a snapshot. Default is a no-op for backends that do not
    /// support targeted deletion. Phase 4 self-heal (BS3 / BS10) relies
    /// on this when a snapshot blob fails to deserialize after schema
    /// drift; backends without a working delete will accumulate stale
    /// snapshots. Override on backends that can support it.
    async fn delete_snapshot(
        &self,
        _aggregate_type: &str,
        _aggregate_id: Uuid,
    ) -> Result<()> {
        Ok(())
    }
}

#[async_trait]
impl<T: EventLog + ?Sized> SnapshotStore for T {
    async fn load_snapshot(
        &self,
        aggregate_type: &str,
        aggregate_id: Uuid,
    ) -> Result<Option<Snapshot>> {
        EventLog::load_snapshot(self, aggregate_type, aggregate_id).await
    }

    async fn save_snapshot(&self, snapshot: Snapshot) -> Result<()> {
        EventLog::save_snapshot(self, snapshot).await
    }
}
