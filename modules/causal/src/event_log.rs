//! Durable event log trait.
//!
//! The [`EventLog`] trait abstracts append-only event persistence. It is one
//! half of the split from the monolithic `Store` trait (the other half being
//! [`ReactorQueue`](crate::reactor_queue::ReactorQueue)).

use anyhow::Result;
use async_trait::async_trait;
use uuid::Uuid;

use crate::types::{AppendResult, NewEvent, PersistedEvent, Snapshot};

/// Append-only event log with aggregate stream support.
///
/// Implementations must be `Send + Sync` for use across async tasks.
///
/// # Idempotency contract
///
/// `append` **must** be totally idempotent: if called with an `event_id`
/// that has been successfully persisted at any point in the past, the
/// implementation MUST return an equivalent [`AppendResult`] and MUST NOT
/// create a duplicate entry.
#[async_trait]
pub trait EventLog: Send + Sync {
    /// Append a single event to the global log.
    ///
    /// Returns the global position and optional per-aggregate stream version.
    async fn append(&self, event: NewEvent) -> Result<AppendResult>;

    /// Load events from the global log after a given position.
    ///
    /// Returns up to `limit` events with `position > after_position`,
    /// ordered by position. Used by the engine's settle loop to read
    /// new events via checkpoint cursor.
    async fn load_from(
        &self,
        after_position: u64,
        limit: usize,
    ) -> Result<Vec<PersistedEvent>>;

    /// Load events for an aggregate stream (for hydration).
    ///
    /// Pass `after_version: Some(v)` to load only events with a stream
    /// version greater than `v` (for snapshot + partial replay), or `None`
    /// for full replay.
    async fn load_stream(
        &self,
        aggregate_type: &str,
        aggregate_id: Uuid,
        after_version: Option<u64>,
    ) -> Result<Vec<PersistedEvent>>;

    /// Latest global position in the log (0 if empty).
    async fn latest_position(&self) -> Result<u64>;

    /// Load the latest snapshot for an aggregate.
    async fn load_snapshot(
        &self,
        _aggregate_type: &str,
        _aggregate_id: Uuid,
    ) -> Result<Option<Snapshot>> {
        Ok(None)
    }

    /// Save a snapshot of aggregate state.
    async fn save_snapshot(&self, _snapshot: Snapshot) -> Result<()> {
        Ok(())
    }
}
