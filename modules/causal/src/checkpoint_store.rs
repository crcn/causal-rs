//! Per-consumer cursor storage + reactor retry-tracking traits.
//!
//! - [`CheckpointStore`] — minimal cursor read/write. Required by
//!   `Projector` / `MultiProjector` / `Reactor` runners.
//! - [`ReactorCheckpoint`] — extends `CheckpointStore` with the failure store
//!   attempt-counter surface `ReactorRunner` uses to track retries
//!   across step boundaries.
//!
//! Reactor outputs append **directly** to the log (no outbox); these
//! traits carry only the per-consumer cursor and the failure store-attempt counters.

use anyhow::Result;
use async_trait::async_trait;
use uuid::Uuid;

use crate::types::LogCursor;

/// Minimal cursor read/write surface.
#[async_trait]
pub trait CheckpointStore: Send + Sync {
    async fn get(&self, consumer_id: &str) -> Result<Option<LogCursor>>;
    async fn set(&self, consumer_id: &str, pos: LogCursor) -> Result<()>;
}

// ─────────────────────────────────────────────────────────────────────
// ReactorCheckpoint — checkpoint + terminal-failure retry-attempt tracking
// ─────────────────────────────────────────────────────────────────────

/// Extends [`CheckpointStore`] with reactor retry-attempt tracking for
/// the terminal-failure path. Required only for engines hosting reactors.
#[async_trait]
pub trait ReactorCheckpoint: CheckpointStore {
    /// Increment the attempt counter for a `(consumer_id,
    /// trigger_id)` pair and return the new count. Called by
    /// `ReactorRunner` on every `react()` failure to track retries
    /// for the terminal-failure path. The returned value is the count INCLUDING this
    /// attempt (first failure returns 1).
    ///
    /// This is an in-process retry budget, not durable state. Both
    /// current backends (MemoryStore, Postgres) hold it in memory; a
    /// process restart resets it, which is fine — a restart re-runs the
    /// uncommitted reactor work from its cursor and re-counts toward the
    /// cap before parking. A backend MAY persist it (a future schema
    /// migration could) but is not required to.
    async fn record_reactor_attempt(
        &self,
        consumer_id: &str,
        trigger_id: Uuid,
    ) -> Result<u32>;

    /// Clear the attempt counter for a `(consumer_id,
    /// trigger_id)` pair. Called on successful `react()` (the
    /// next failure should start fresh) and after the terminal-failure mapper
    /// has fired. Idempotent.
    async fn clear_reactor_attempts(
        &self,
        consumer_id: &str,
        trigger_id: Uuid,
    ) -> Result<()>;
}
