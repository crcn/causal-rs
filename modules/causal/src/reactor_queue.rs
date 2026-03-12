//! Reactor work-distribution queue trait.
//!
//! The [`ReactorQueue`] trait abstracts reactor intent persistence, dequeue,
//! resolution, journaling, and coordination. It is one half of the split from
//! the monolithic `Store` trait (the other half being
//! [`EventLog`](crate::event_log::EventLog)).

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use uuid::Uuid;

use crate::types::{
    ReactorResolution, IntentCommit, JournalEntry, QueueStatus, QueuedReactor,
};

/// Work-distribution queue for reactor executions.
///
/// Implementations must be `Send + Sync` for use across async tasks.
///
/// # Checkpoint contract
///
/// The queue maintains a checkpoint cursor representing the last fully
/// processed position in the EventLog. The engine advances this via
/// `enqueue()` which atomically persists reactor intents AND advances
/// the checkpoint.
#[async_trait]
pub trait ReactorQueue: Send + Sync {
    /// Atomically enqueue reactor intents and advance the checkpoint.
    ///
    /// The `commit.checkpoint` value becomes the new checkpoint position.
    /// For parked events, a DLQ entry is also recorded.
    async fn enqueue(&self, commit: IntentCommit) -> Result<()>;

    /// Return the current checkpoint position.
    ///
    /// The engine uses this to determine where to resume reading from
    /// the EventLog via `load_from(checkpoint, limit)`.
    async fn checkpoint(&self) -> Result<crate::types::LogCursor>;

    /// Claim the next ready reactor execution.
    ///
    /// Transitions the reactor from **pending** → **running**.
    async fn dequeue(&self) -> Result<Option<QueuedReactor>>;

    /// Returns the earliest `execute_at` of any pending reactor, if any.
    ///
    /// Used by the settle loop to sleep until the next reactor becomes
    /// ready instead of exiting prematurely.
    async fn earliest_pending_at(&self) -> Result<Option<DateTime<Utc>>>;

    /// Resolve a running reactor.
    ///
    /// Single exit point for every claimed reactor:
    /// - `Complete`: mark done, clear journal entries
    /// - `Retry`: reset to pending with new attempt count and schedule
    /// - `DeadLetter`: mark dead-lettered
    async fn resolve(&self, resolution: ReactorResolution) -> Result<()>;

    /// Reclaim reactors stuck in "running" state beyond their timeout.
    /// Default: no-op (suitable for in-memory stores).
    async fn reclaim_stale(&self) -> Result<()> {
        Ok(())
    }

    // ── Journaling ────────────────────────────────────────────────────

    /// Load all journal entries for a reactor execution.
    async fn load_journal(
        &self,
        reactor_id: &str,
        event_id: Uuid,
    ) -> Result<Vec<JournalEntry>>;

    /// Append a single journal entry (one `ctx.run()` result).
    async fn append_journal(
        &self,
        reactor_id: &str,
        event_id: Uuid,
        seq: u32,
        value: serde_json::Value,
    ) -> Result<()>;

    /// Clear journal entries after successful reactor completion.
    async fn clear_journal(
        &self,
        reactor_id: &str,
        event_id: Uuid,
    ) -> Result<()>;

    // ── Coordination ──────────────────────────────────────────────────

    /// Mark a correlation ID as cancelled.
    async fn cancel(&self, _correlation_id: Uuid) -> Result<()> {
        Ok(())
    }

    /// Check whether a correlation ID has been cancelled.
    async fn is_cancelled(&self, _correlation_id: Uuid) -> Result<bool> {
        Ok(false)
    }

    /// Return a summary of pending work for a correlation ID.
    async fn status(&self, _correlation_id: Uuid) -> Result<QueueStatus> {
        Ok(QueueStatus::default())
    }

    /// Upsert reactor gate descriptions for a correlation.
    async fn set_descriptions(
        &self,
        _correlation_id: Uuid,
        _descriptions: HashMap<String, serde_json::Value>,
    ) -> Result<()> {
        Ok(())
    }

    /// Read all reactor gate descriptions for a correlation.
    async fn get_descriptions(
        &self,
        _correlation_id: Uuid,
    ) -> Result<HashMap<String, serde_json::Value>> {
        Ok(HashMap::new())
    }
}
