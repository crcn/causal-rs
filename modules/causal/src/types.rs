//! Core data types for the causal event log.

use chrono::{DateTime, Utc};
use std::any::Any;
use std::fmt;
use std::sync::Arc;
use uuid::Uuid;

// ─────────────────────────────────────────────────────────────────────
// Reactor logging
// ─────────────────────────────────────────────────────────────────────

/// Log level for reactor log entries captured via `Ctx::log`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
}

impl fmt::Display for LogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LogLevel::Debug => write!(f, "DEBUG"),
            LogLevel::Info  => write!(f, "INFO"),
            LogLevel::Warn  => write!(f, "WARN"),
        }
    }
}

/// A structured log entry captured during reactor execution. The
/// reactor body pushes entries via `ctx.log(...)`; the runner drains
/// them after `react()` returns and routes them through the
/// `ReactorObserver`.
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub level:     LogLevel,
    pub message:   String,
    pub data:      Option<serde_json::Value>,
    pub timestamp: DateTime<Utc>,
}

// ─────────────────────────────────────────────────────────────────────
// Cursors + versions
// ─────────────────────────────────────────────────────────────────────

/// Opaque cursor into the global event log.
///
/// Monotonically increasing position; gaps between values are allowed
/// (e.g. Postgres BIGSERIAL). Consumers must never perform arithmetic —
/// use it only for ordering comparisons and checkpoint cursors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct LogCursor(u64);

impl LogCursor {
    pub const ZERO: LogCursor = LogCursor(0);

    /// Wrap a raw u64 from a storage boundary (e.g. Postgres row).
    pub fn from_raw(position: u64) -> Self {
        LogCursor(position)
    }

    /// Unwrap to raw u64 for a storage boundary (e.g. SQL parameter).
    pub fn raw(self) -> u64 {
        self.0
    }
}

impl fmt::Display for LogCursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Per-stream revision (0-indexed, matching Kurrent).
///
/// The Nth event written to a stream lands at revision `N - 1`:
/// first event = 0, second = 1, etc. A fresh stream has NO
/// revision — see [`StreamState::NoStream`] for the
/// "expect-an-empty-stream" sentinel used on append.
///
/// Backend-internal — used by `EventLogBackend::append_to_stream`,
/// snapshot thresholds, and hydration replay. Not surfaced on the
/// user-facing `Engine::emit` path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct StreamRevision(u64);

impl StreamRevision {
    /// Revision 0 — the FIRST event in a stream. **Not** "empty
    /// stream" — for that, see [`StreamState::NoStream`].
    pub const ZERO: StreamRevision = StreamRevision(0);

    pub fn from_raw(revision: u64) -> Self {
        StreamRevision(revision)
    }

    pub fn raw(self) -> u64 {
        self.0
    }
}

impl fmt::Display for StreamRevision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Expected state of a stream on append, matching KurrentDB's
/// `StreamState` enum exactly.
///
/// Passed as `expected` to
/// [`EventLogBackend::append_to_stream`](crate::EventLogBackend::append_to_stream).
/// The backend rejects appends whose expected state doesn't match
/// the stream's current shape ([`AppendError::WrongExpectedState`]
/// surfaced via `anyhow::Error` today).
///
/// # Variants
///
/// - [`StreamState::Any`] — no concurrency check. Idempotency
///   guarantees are weakest in this mode (Kurrent server-side may
///   still dedupe on EventId, but it's not promised).
/// - [`StreamState::NoStream`] — caller expects an empty stream.
///   Use for the FIRST event written to an aggregate. Errors if the
///   stream already has any event.
/// - [`StreamState::StreamExists`] — caller expects at least one
///   prior event. Errors if the stream is empty.
/// - [`StreamState::StreamRevision`] — caller expects the last
///   event in the stream to have THIS revision. The standard
///   optimistic-concurrency check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamState {
    Any,
    NoStream,
    StreamExists,
    StreamRevision(u64),
}

impl fmt::Display for StreamState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StreamState::Any => write!(f, "Any"),
            StreamState::NoStream => write!(f, "NoStream"),
            StreamState::StreamExists => write!(f, "StreamExists"),
            StreamState::StreamRevision(r) => write!(f, "StreamRevision({r})"),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────
// Append result / persisted-event / new-event / snapshot
// ─────────────────────────────────────────────────────────────────────

/// Result of appending an event to the global log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteResult {
    /// Opaque global ordering cursor.
    pub position: LogCursor,
    /// Per-stream revision (0-indexed) of the just-written event. Every
    /// append targets a stream, so this is always present.
    pub revision: StreamRevision,
}

/// A persisted event loaded from the store.
#[derive(Clone)]
pub struct RecordedEvent {
    pub position: LogCursor,
    pub event_id: Uuid,
    /// Parent event that caused this event (None for root events).
    pub causation_id: Option<Uuid>,
    /// Correlation ID linking the full causal tree.
    pub correlation_id: Uuid,
    /// Canonical `{CATEGORY}:{name}` event_type string.
    pub event_type: String,
    /// JSON payload.
    pub payload: serde_json::Value,
    /// When the event was persisted (backend-authoritative —
    /// `EventData::created_at` is a hint that backends MAY override).
    pub created_at: DateTime<Utc>,
    /// Stream category (`Event::CATEGORY`) — the prefix of the
    /// `{category}-{stream_id}` stream name. Every event belongs to a stream.
    pub category: String,
    /// Stream id (`Event::stream_id`).
    pub stream_id: Uuid,
    /// Per-stream revision (0-indexed).
    pub revision: StreamRevision,
    /// Application-level metadata (e.g. `_run_id`, `_schema_v`,
    /// `_actor`). Set via `EmitBuilder::metadata`.
    pub metadata: serde_json::Map<String, serde_json::Value>,
    /// Original typed event, available only during live in-process
    /// dispatch. `None` on load from durable store — consumers fall
    /// back to JSON deserialization.
    pub ephemeral: Option<Arc<dyn Any + Send + Sync>>,
    /// Whether this event should be forwarded to the permanent event
    /// store. Always `true` — facts are persistent. Field retained
    /// for backend compatibility.
    pub persistent: bool,
}

impl RecordedEvent {
    /// The stream category (`Event::CATEGORY`) this event belongs to —
    /// a `&str` accessor over the `category` field. Useful in
    /// `MultiProjector::project` bodies routing across categories.
    pub fn category(&self) -> &str {
        &self.category
    }
}

impl fmt::Debug for RecordedEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RecordedEvent")
            .field("position", &self.position)
            .field("event_id", &self.event_id)
            .field("causation_id", &self.causation_id)
            .field("correlation_id", &self.correlation_id)
            .field("event_type", &self.event_type)
            .field("payload", &self.payload)
            .field("created_at", &self.created_at)
            .field("category", &self.category)
            .field("stream_id", &self.stream_id)
            .field("revision", &self.revision)
            .field("metadata", &self.metadata)
            .field("ephemeral", &self.ephemeral.as_ref().map(|_| "..."))
            .finish()
    }
}

/// A new event to be appended to the global log.
#[derive(Clone)]
pub struct EventData {
    pub event_id: Uuid,
    pub causation_id: Option<Uuid>,
    pub correlation_id: Uuid,
    pub event_type: String,
    pub payload: serde_json::Value,
    /// Hint for `created_at` — backends MAY override server-side.
    pub created_at: DateTime<Utc>,
    /// Stream category (`Event::CATEGORY`). Present for stream-scoped events.
    pub category: Option<String>,
    /// Stream id (`Event::stream_id`). Present for stream-scoped events.
    pub stream_id: Option<Uuid>,
    pub metadata: serde_json::Map<String, serde_json::Value>,
    /// Original typed event for zero-cost in-process dispatch. `None`
    /// for events loaded from durable stores.
    pub ephemeral: Option<Arc<dyn Any + Send + Sync>>,
    /// Always `true` (facts are persistent).
    pub persistent: bool,
}

impl fmt::Debug for EventData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventData")
            .field("event_id", &self.event_id)
            .field("causation_id", &self.causation_id)
            .field("correlation_id", &self.correlation_id)
            .field("event_type", &self.event_type)
            .field("payload", &self.payload)
            .field("created_at", &self.created_at)
            .field("category", &self.category)
            .field("stream_id", &self.stream_id)
            .field("metadata", &self.metadata)
            .field("ephemeral", &self.ephemeral.as_ref().map(|_| "..."))
            .finish()
    }
}

/// Serialized snapshot of aggregate state at a specific stream revision.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub aggregate_type: String,
    pub aggregate_id: Uuid,
    pub revision: StreamRevision,
    pub state: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

#[cfg(test)]
mod persisted_event_tests {
    use super::*;

    fn mk(event_type: &str) -> RecordedEvent {
        let category = crate::event_type::category_of(&event_type).to_string();
        RecordedEvent {
            position:        LogCursor::ZERO,
            event_id:        Uuid::nil(),
            causation_id:       None,
            correlation_id:  Uuid::nil(),
            event_type:      event_type.into(),
            payload:         serde_json::Value::Null,
            created_at:      Utc::now(),
            category,
            stream_id:       Uuid::nil(),
            revision:        StreamRevision::ZERO,
            metadata:        serde_json::Map::new(),
            ephemeral:       None,
            persistent:      true,
        }
    }

    #[test]
    fn category_returns_prefix_before_colon() {
        assert_eq!(mk("scrape:web_scrape_completed").category(), "scrape");
        assert_eq!(mk("world:tick").category(), "world");
    }

    #[test]
    fn category_returns_full_string_when_no_colon() {
        // Legacy events that pre-date the `{CATEGORY}:{name}` convention
        // surface here too — bare `order_placed`-style strings have no
        // separator, so `category()` returns the whole thing.
        assert_eq!(mk("order_placed").category(), "order_placed");
    }
}
