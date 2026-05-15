//! Append-only event log trait — the write side of the durable event store.
//!
//! Backends implement [`EventLogBackend`]; the `Engine` reads and writes
//! through this trait. The shapes match KurrentDB's primitives:
//!
//! - [`EventData`] / [`RecordedEvent`] mirror Kurrent's pre-/post-write
//!   event types.
//! - [`StreamState`] is the expected-state parameter on
//!   [`EventLogBackend::append_to_stream`] (matches Kurrent's enum
//!   exactly: `Any`, `NoStream`, `StreamExists`, `StreamRevision(u64)`).
//! - [`StreamRevision`] is the 0-indexed concrete revision of a recorded
//!   event.

use anyhow::Result;
use async_trait::async_trait;
use uuid::Uuid;

use crate::types::{
    WriteResult, EventData, LogCursor, RecordedEvent, StreamRevision, StreamState,
};

/// Append-only event log with aggregate stream support.
///
/// # Idempotency contract
///
/// `append` MUST be totally idempotent on `event_id`: a second call with
/// an already-persisted `event_id` returns an equivalent [`WriteResult`]
/// without creating a duplicate entry.
#[async_trait]
pub trait EventLogBackend: Send + Sync {
    /// Append a non-aggregate event to the log (no concurrency check).
    ///
    /// **`EventData::created_at` is a hint, not authoritative.** Backends
    /// MAY override with a server-assigned timestamp on write (KurrentDB
    /// does this unconditionally; `MemoryStore` preserves the client
    /// value). Consumers reading `RecordedEvent::created_at` see
    /// whatever the backend persisted; replay determinism follows from
    /// THAT value.
    ///
    /// `Engine::emit` derives `EventData::created_at` from
    /// `fact.occurred_at().unwrap_or_else(Utc::now)`. For backends that
    /// override server-side, stamp the producer-claimed
    /// `Event::occurred_at` into `EventData::metadata` if a consumer needs
    /// to distinguish event-time from commit-time post-write.
    async fn append(&self, event: EventData) -> Result<WriteResult>;

    /// Read events from `$all` after `after`, up to `limit` events
    /// ordered by position.
    async fn read_all(
        &self,
        after: LogCursor,
        limit: usize,
    ) -> Result<Vec<RecordedEvent>>;

    /// Read events from a single aggregate stream. Pass
    /// `after: Some(revision)` to load only events with revision >
    /// the given value (snapshot + partial replay); `None` for full
    /// replay.
    async fn read_stream(
        &self,
        aggregate_type: &str,
        aggregate_id: Uuid,
        after: Option<StreamRevision>,
    ) -> Result<Vec<RecordedEvent>>;

    /// Latest global position in the log (`LogCursor::ZERO` if empty).
    async fn latest_position(&self) -> Result<LogCursor>;

    /// CAS-protected append to an aggregate stream.
    ///
    /// `expected` matches KurrentDB's [`StreamState`] semantics:
    /// `NoStream` for an empty stream's first event,
    /// `StreamRevision(n)` to assert the last event's revision is `n`,
    /// `StreamExists` for any non-empty stream, `Any` to skip the
    /// check (idempotency guarantees weaken in this mode).
    ///
    /// On a mismatch the backend returns an `anyhow::Error` carrying
    /// a [`ConflictError`] (downcast with `.downcast_ref::<ConflictError>()`).
    /// Command handlers SHOULD reload the aggregate at the new
    /// `current` revision, re-decide, and retry if the command still
    /// applies.
    ///
    /// **Default impl is NOT atomic** — it does read_stream + check +
    /// append in three separate operations. Two concurrent callers can
    /// both see the expected revision, both pass the check, both
    /// append, both succeed. Backends with native CAS primitives
    /// (Postgres `SELECT FOR UPDATE`, in-memory single-mutex,
    /// KurrentDB `StreamState`) SHOULD override.
    async fn append_to_stream(
        &self,
        aggregate_type: &str,
        aggregate_id: Uuid,
        expected: StreamState,
        event: EventData,
    ) -> Result<WriteResult> {
        let stream = self.read_stream(aggregate_type, aggregate_id, None).await?;
        let actual_tail: Option<StreamRevision> =
            stream.last().and_then(|e| e.revision);
        let matches = match (expected, actual_tail) {
            (StreamState::Any, _) => true,
            (StreamState::NoStream, None) => true,
            (StreamState::StreamExists, Some(_)) => true,
            (StreamState::StreamRevision(want), Some(actual)) => {
                actual.raw() == want
            }
            _ => false,
        };
        if !matches {
            return Err(anyhow::Error::new(ConflictError {
                expected,
                current: actual_tail,
            }));
        }
        // Caller MUST set aggregate_type + aggregate_id on `event`
        // matching this method's parameters; the existing append impl
        // assigns the next revision sequentially.
        self.append(event).await
    }
}

/// Returned when [`EventLogBackend::append_to_stream`] sees a stream
/// state that doesn't match the caller's expectation. Command handlers
/// SHOULD reload the aggregate (at `current`) and retry the decision;
/// if the new state still permits the command, append again with the
/// fresh `StreamRevision(current.raw())`.
#[derive(Debug, thiserror::Error)]
#[error("aggregate stream state mismatch: expected {expected}, current {current:?}")]
pub struct ConflictError {
    pub expected: StreamState,
    pub current:  Option<StreamRevision>,
}
