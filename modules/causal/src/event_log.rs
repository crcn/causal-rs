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

/// Append-only event log. One write primitive — every event belongs to a
/// stream (`{category}-{stream_id}`); see [`append_to_stream`](Self::append_to_stream).
///
/// # Idempotency contract
///
/// `append_to_stream` MUST be idempotent on `event_id`: a second call whose
/// events carry already-persisted `event_id`s returns an equivalent
/// [`WriteResult`] without creating duplicate entries.
///
/// **`EventData::created_at` is a hint, not authoritative.** Backends MAY
/// override with a server-assigned timestamp on write (KurrentDB does this
/// unconditionally; `MemoryStore` preserves the client value). Consumers
/// reading `RecordedEvent::created_at` see whatever the backend persisted.
#[async_trait]
pub trait EventLogBackend: Send + Sync {
    /// Read events from `$all` after `after`, up to `limit` events
    /// ordered by position.
    async fn read_all(
        &self,
        after: LogCursor,
        limit: usize,
    ) -> Result<Vec<RecordedEvent>>;

    /// Read events from a single stream (`{category}-{stream_id}`). Pass
    /// `after: Some(revision)` to load only events with revision >
    /// the given value (snapshot + partial replay); `None` for full
    /// replay.
    async fn read_stream(
        &self,
        category: &str,
        stream_id: Uuid,
        after: Option<StreamRevision>,
    ) -> Result<Vec<RecordedEvent>>;

    /// Latest global position in the log (`LogCursor::ZERO` if empty).
    async fn latest_position(&self) -> Result<LogCursor>;

    /// The append primitive — write `events` to stream
    /// `{category}-{stream_id}` under an optimistic-concurrency check.
    ///
    /// The batch is **atomic**: all events land at consecutive revisions
    /// or none do. This mirrors KurrentDB's `append_to_stream`, which takes
    /// a sequence of events and commits them as a unit — it's what lets a
    /// multi-fact decision ([`Engine::append`](crate::Engine::append))
    /// commit without a torn write. `events` must be non-empty.
    ///
    /// `expected` matches KurrentDB's [`StreamState`] semantics:
    /// `NoStream` for an empty stream's first event,
    /// `StreamRevision(n)` to assert the last event's revision is `n`,
    /// `StreamExists` for any non-empty stream, `Any` to skip the check
    /// (append-only fact streams; idempotency then rests on `event_id`).
    ///
    /// The returned [`WriteResult`] describes the **last** event in the
    /// batch (its position + revision); the rest occupy the consecutive
    /// revisions below it.
    ///
    /// **Idempotency precondition**: within one batch the `event_id`s must be
    /// either ALL new or ALL already-persisted — never a mix. Idempotent
    /// retries resubmit the *identical* batch, which is fine; submitting a
    /// batch that partially overlaps a prior write is a caller bug. Backends
    /// detect redelivery by the batch's last `event_id` and reject a
    /// partially-overlapping batch rather than corrupt the stream. (All
    /// in-tree callers satisfy this: `Engine::append` mints fresh ids per
    /// attempt; reactor outputs are single-event batches.)
    ///
    /// On a mismatch the backend returns an `anyhow::Error` carrying a
    /// [`ConflictError`] (downcast with `.downcast_ref::<ConflictError>()`).
    /// Command handlers SHOULD reload at the new `current` revision,
    /// re-decide, and retry — see [`Engine::append`](crate::Engine::append).
    ///
    /// Backends MUST make this atomic (Postgres transaction, in-memory
    /// single-mutex, KurrentDB native multi-event append).
    async fn append_to_stream(
        &self,
        category: &str,
        stream_id: Uuid,
        expected: StreamState,
        events: Vec<EventData>,
    ) -> Result<WriteResult>;
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

/// Convenience over the single [`EventLogBackend::append_to_stream`]
/// primitive: append `event` to its own stream with [`StreamState::Any`]
/// (append-only, no concurrency check; idempotency rests on `event_id`).
///
/// The destination stream is `event.category` / `event.stream_id` when
/// carried; otherwise it's derived — category from the `{category}:{name}`
/// `event_type` prefix, stream_id from the event's own `event_id` (a
/// standalone single-event stream). So a bare fact always lands somewhere
/// sensible, never a shared `_global`.
///
/// Not a backend method — backends implement only `append_to_stream`.
/// Sugar for seeding fixtures and ad-hoc single appends. Invariant-bearing
/// writes go through [`Engine::append`](crate::Engine::append); the typed
/// emit/reactor paths set `category`/`stream_id` explicitly.
pub async fn append_event<B: EventLogBackend + ?Sized>(
    backend: &B,
    event: EventData,
) -> Result<WriteResult> {
    // Derive the placement category from the event_type prefix when it
    // is absent OR explicitly empty — an empty `Some("")` would match no
    // consumer (and, worse, act as a wildcard in `matches_category`), so
    // normalize it the same as `None`.
    let category = event
        .category
        .clone()
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| {
            let derived = crate::event_type::category_of(&event.event_type);
            if derived.is_empty() { "event" } else { derived }.to_string()
        });
    let stream_id = event.stream_id.unwrap_or(event.event_id);
    backend
        .append_to_stream(&category, stream_id, StreamState::Any, vec![event])
        .await
}
