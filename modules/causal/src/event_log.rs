//! Append-only event log trait — the write side of the durable event store.
//!
//! Backends implement [`EventLogBackend`]. The v0.4 `Engine` reads/writes
//! through this trait; legacy `EventLog` (which used to live here, plus
//! the blanket impl that gave it `EventLogBackend` for free) is gone as
//! of P11.e.

use anyhow::Result;
use async_trait::async_trait;
use uuid::Uuid;

use crate::types::{AppendResult, LogCursor, NewEvent, PersistedEvent, StreamVersion};

/// Append-only event log with aggregate stream support.
///
/// # Idempotency contract
///
/// `append` MUST be totally idempotent on `event_id`: a second call with
/// an already-persisted `event_id` returns an equivalent [`AppendResult`]
/// without creating a duplicate entry.
#[async_trait]
pub trait EventLogBackend: Send + Sync {
    /// Append a `NewEvent` to the log.
    ///
    /// **`NewEvent::created_at` is a hint, not authoritative.** Backends
    /// MAY override with a server-assigned timestamp on write (KurrentDB
    /// does this unconditionally; `MemoryStore` preserves the client
    /// value). Consumers reading `PersistedEvent::created_at` see
    /// whatever the backend persisted; replay determinism follows from
    /// THAT value.
    ///
    /// `Engine::emit` derives `NewEvent::created_at` from
    /// `fact.occurred_at().unwrap_or_else(Utc::now)`. For backends that
    /// override server-side, stamp the producer-claimed
    /// `Fact::occurred_at` into `NewEvent::metadata` if a consumer needs
    /// to distinguish event-time from commit-time post-write.
    async fn append(&self, event: NewEvent) -> Result<AppendResult>;

    /// Load events from the global log after `after_position`, up to
    /// `limit` events ordered by position.
    async fn load_from(
        &self,
        after: LogCursor,
        limit: usize,
    ) -> Result<Vec<PersistedEvent>>;

    /// Load events for an aggregate stream. Pass `after_version: Some(v)`
    /// to load only events with stream version > v (snapshot + partial
    /// replay), or `None` for full replay.
    async fn load_stream(
        &self,
        aggregate_type: &str,
        aggregate_id: Uuid,
        after_version: Option<StreamVersion>,
    ) -> Result<Vec<PersistedEvent>>;

    /// Latest global position in the log (`LogCursor::ZERO` if empty).
    async fn latest_position(&self) -> Result<LogCursor>;

    /// CAS-protected append to an aggregate stream (per C6).
    ///
    /// Reads the current stream version, errors with [`ConflictError`]
    /// if it doesn't match `expected`, otherwise appends and returns
    /// the [`AppendResult`].
    ///
    /// **Default impl is NOT atomic** — it does load_stream + check +
    /// append in three separate operations. Two concurrent callers can
    /// both see the expected version, both pass the check, both append,
    /// both succeed. Backends with native CAS primitives (Postgres
    /// `SELECT FOR UPDATE`, in-memory single-mutex) SHOULD override.
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
            return Err(anyhow::Error::new(ConflictError { expected, current }));
        }
        // Caller MUST set aggregate_type + aggregate_id on `event`
        // matching this method's parameters; the existing append impl
        // assigns the next version sequentially.
        self.append(event).await
    }
}

/// Returned when `append_to_stream` finds a stream version other than
/// `expected`. Command handlers SHOULD reload the aggregate (with the
/// new `current` version) and retry the decision; if the new state
/// still permits the command, append again.
#[derive(Debug, thiserror::Error)]
#[error("aggregate stream version mismatch: expected {expected:?}, current {current:?}")]
pub struct ConflictError {
    pub expected: StreamVersion,
    pub current:  StreamVersion,
}
