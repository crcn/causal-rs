//! Durable event log trait.
//!
//! The [`EventLog`] trait abstracts append-only event persistence. It is one
//! half of the split from the monolithic `Store` trait (the other half being
//! [`ReactorQueue`](crate::reactor_queue::ReactorQueue)).

use anyhow::Result;
use async_trait::async_trait;
use uuid::Uuid;

use crate::types::{AppendResult, LogCursor, NewEvent, PersistedEvent, Snapshot, StreamVersion};

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
        after: LogCursor,
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
        after_version: Option<StreamVersion>,
    ) -> Result<Vec<PersistedEvent>>;

    /// Latest global position in the log (LogCursor::ZERO if empty).
    async fn latest_position(&self) -> Result<LogCursor>;

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

    /// CAS-protected append to an aggregate stream (per C6).
    ///
    /// Reads the current stream version, errors with [`ConflictError`]
    /// if it doesn't match `expected`, otherwise appends and returns
    /// the [`AppendResult`].
    ///
    /// **Default impl is NOT atomic** — it does load_stream + check +
    /// append in three separate operations. Two concurrent callers
    /// can both see the expected version, both pass the check, both
    /// append, both succeed. Backends with native CAS primitives
    /// (Postgres `SELECT FOR UPDATE`, in-memory single-mutex)
    /// SHOULD override this method.
    async fn append_to_stream(
        &self,
        aggregate_type: &str,
        aggregate_id: Uuid,
        expected: StreamVersion,
        event: NewEvent,
    ) -> Result<AppendResult> {
        let stream = self.load_stream(aggregate_type, aggregate_id, None).await?;
        let current = stream
            .last()
            .and_then(|e| e.version)
            .unwrap_or(StreamVersion::ZERO);
        if current != expected {
            return Err(anyhow::Error::new(ConflictError {
                expected,
                current,
            }));
        }
        // Caller MUST set aggregate_type + aggregate_id on `event`
        // matching this method's parameters; the existing append impl
        // assigns the next version sequentially.
        self.append(event).await
    }
}

/// Returned when `append_to_stream` finds a stream version other
/// than `expected`. Command handlers SHOULD reload the aggregate
/// (with the new `current` version) and retry the decision; if the
/// new state still permits the command, append again.
#[derive(Debug, thiserror::Error)]
#[error("aggregate stream version mismatch: expected {expected:?}, current {current:?}")]
pub struct ConflictError {
    pub expected: StreamVersion,
    pub current:  StreamVersion,
}

/// v0.3 split of `EventLog` — same surface minus the snapshot methods,
/// which moved to [`crate::snapshot_store::SnapshotStore`]. Existing
/// `EventLog` impls automatically satisfy this via the blanket impl
/// below, so backends do not need code changes for Phase 1; new code
/// should target `EventLogBackend` directly.
///
/// Phase 4 will add `append_to_stream` (CAS-protected) and `subscribe`
/// (catch-up subscription) to this trait per the v0.3 API design plan.
#[async_trait]
pub trait EventLogBackend: Send + Sync {
    /// Append a `NewEvent` to the log.
    ///
    /// **`NewEvent::created_at` is a hint, not authoritative.**
    /// Backends MAY override the client-provided `created_at` with a
    /// server-assigned timestamp on write — KurrentDB does this
    /// unconditionally; `MemoryStore` preserves the client value.
    /// Consumers reading via `PersistedEvent::created_at` see whatever
    /// the backend persisted, and replay determinism follows from
    /// THAT value (not from the client's pre-write hint).
    ///
    /// `Engine::emit` derives `NewEvent::created_at` from
    /// `fact.occurred_at().unwrap_or_else(Utc::now)`. For backends
    /// that override server-side, the producer-claimed
    /// `Fact::occurred_at` value (when set) must be stamped into
    /// `NewEvent::metadata` if a consumer needs to distinguish
    /// event-time from commit-time post-write.
    async fn append(&self, event: NewEvent) -> Result<AppendResult>;

    async fn load_from(
        &self,
        after: LogCursor,
        limit: usize,
    ) -> Result<Vec<PersistedEvent>>;

    async fn load_stream(
        &self,
        aggregate_type: &str,
        aggregate_id: Uuid,
        after_version: Option<StreamVersion>,
    ) -> Result<Vec<PersistedEvent>>;

    async fn latest_position(&self) -> Result<LogCursor>;

    async fn append_to_stream(
        &self,
        aggregate_type: &str,
        aggregate_id: Uuid,
        expected: StreamVersion,
        event: NewEvent,
    ) -> Result<AppendResult>;
}

#[async_trait]
impl<T: EventLog + ?Sized> EventLogBackend for T {
    async fn append(&self, event: NewEvent) -> Result<AppendResult> {
        EventLog::append(self, event).await
    }

    async fn load_from(
        &self,
        after: LogCursor,
        limit: usize,
    ) -> Result<Vec<PersistedEvent>> {
        EventLog::load_from(self, after, limit).await
    }

    async fn load_stream(
        &self,
        aggregate_type: &str,
        aggregate_id: Uuid,
        after_version: Option<StreamVersion>,
    ) -> Result<Vec<PersistedEvent>> {
        EventLog::load_stream(self, aggregate_type, aggregate_id, after_version).await
    }

    async fn latest_position(&self) -> Result<LogCursor> {
        EventLog::latest_position(self).await
    }

    async fn append_to_stream(
        &self,
        aggregate_type: &str,
        aggregate_id: Uuid,
        expected: StreamVersion,
        event: NewEvent,
    ) -> Result<AppendResult> {
        EventLog::append_to_stream(self, aggregate_type, aggregate_id, expected, event).await
    }
}
