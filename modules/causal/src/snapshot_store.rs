//! Snapshot persistence trait — separate from the event log so the
//! snapshot backend can be swapped independently (Postgres log + S3
//! snapshots, Kurrent log + Postgres snapshots, etc.). Backends
//! implement this trait directly.

use anyhow::Result;
use async_trait::async_trait;
use uuid::Uuid;

use crate::types::Snapshot;

#[async_trait]
pub trait SnapshotStore: Send + Sync {
    async fn load_snapshot(
        &self,
        aggregate_type: &str,
        aggregate_id: Uuid,
    ) -> Result<Option<Snapshot>>;

    async fn save_snapshot(&self, snapshot: Snapshot) -> Result<()>;

    /// Delete a snapshot. Default is a no-op for backends that don't
    /// support targeted deletion. Self-heal relies on this when a
    /// snapshot blob fails to deserialize after schema drift; backends
    /// without a working delete will accumulate stale snapshots.
    /// Override on backends that can support it.
    async fn delete_snapshot(
        &self,
        _aggregate_type: &str,
        _aggregate_id: Uuid,
    ) -> Result<()> {
        Ok(())
    }
}
