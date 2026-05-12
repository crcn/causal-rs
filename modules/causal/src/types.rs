//! Core data types for the causal event log.

use chrono::{DateTime, Utc};
use std::any::Any;
use std::fmt;
use std::sync::Arc;
use uuid::Uuid;

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

/// Opaque per-aggregate stream version.
///
/// Contiguous within a stream (1, 2, 3, …). Used for optimistic
/// concurrency (`emit(...).expecting(v)`), snapshot thresholds, and
/// hydration replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct StreamVersion(u64);

impl StreamVersion {
    pub const ZERO: StreamVersion = StreamVersion(0);

    pub fn from_raw(version: u64) -> Self {
        StreamVersion(version)
    }

    pub fn raw(self) -> u64 {
        self.0
    }
}

impl fmt::Display for StreamVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ─────────────────────────────────────────────────────────────────────
// Append result / persisted-event / new-event / snapshot
// ─────────────────────────────────────────────────────────────────────

/// Result of appending an event to the global log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppendResult {
    /// Opaque global ordering cursor.
    pub position: LogCursor,
    /// Per-aggregate stream version at the time of append. `None` for
    /// events that aren't scoped to an aggregate.
    pub version: Option<StreamVersion>,
}

/// A persisted event loaded from the store.
#[derive(Clone)]
pub struct PersistedEvent {
    pub position: LogCursor,
    pub event_id: Uuid,
    /// Parent event that caused this event (None for root events).
    pub parent_id: Option<Uuid>,
    /// Correlation ID linking the full causal tree.
    pub correlation_id: Uuid,
    /// Canonical `{CATEGORY}:{name}` event_type string.
    pub event_type: String,
    /// JSON payload.
    pub payload: serde_json::Value,
    /// When the event was persisted (backend-authoritative —
    /// `NewEvent::created_at` is a hint that backends MAY override).
    pub created_at: DateTime<Utc>,
    /// Aggregate type (only present for aggregate-scoped events).
    pub aggregate_type: Option<String>,
    /// Aggregate instance ID (only present for aggregate-scoped events).
    pub aggregate_id: Option<Uuid>,
    /// Per-aggregate stream version (only present for aggregate-scoped
    /// events).
    pub version: Option<StreamVersion>,
    /// Application-level metadata (e.g. `_run_id`, `_schema_v`,
    /// `_actor`). Set via `EmitBuilder::metadata`.
    pub metadata: serde_json::Map<String, serde_json::Value>,
    /// Original typed event, available only during live in-process
    /// dispatch. `None` on load from durable store — consumers fall
    /// back to JSON deserialization.
    pub ephemeral: Option<Arc<dyn Any + Send + Sync>>,
    /// Whether this event should be forwarded to the permanent event
    /// store. Always `true` under v0.4 — facts are persistent (P1.5).
    /// Field retained for backend compatibility.
    pub persistent: bool,
}

impl PersistedEvent {
    /// The category prefix of this event's `event_type`. For v0.4
    /// events with the canonical `{CATEGORY}:{name}` shape, this
    /// returns the category. For legacy events without a colon,
    /// returns the full `event_type` string.
    ///
    /// Useful in `MultiProjector::project` bodies that need to route
    /// across categories without parsing the string themselves.
    pub fn category(&self) -> &str {
        self.event_type.split(':').next().unwrap_or(&self.event_type)
    }
}

impl fmt::Debug for PersistedEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PersistedEvent")
            .field("position", &self.position)
            .field("event_id", &self.event_id)
            .field("parent_id", &self.parent_id)
            .field("correlation_id", &self.correlation_id)
            .field("event_type", &self.event_type)
            .field("payload", &self.payload)
            .field("created_at", &self.created_at)
            .field("aggregate_type", &self.aggregate_type)
            .field("aggregate_id", &self.aggregate_id)
            .field("version", &self.version)
            .field("metadata", &self.metadata)
            .field("ephemeral", &self.ephemeral.as_ref().map(|_| "..."))
            .finish()
    }
}

/// A new event to be appended to the global log.
#[derive(Clone)]
pub struct NewEvent {
    pub event_id: Uuid,
    pub parent_id: Option<Uuid>,
    pub correlation_id: Uuid,
    pub event_type: String,
    pub payload: serde_json::Value,
    /// Hint for `created_at` — backends MAY override server-side.
    pub created_at: DateTime<Utc>,
    pub aggregate_type: Option<String>,
    pub aggregate_id: Option<Uuid>,
    pub metadata: serde_json::Map<String, serde_json::Value>,
    /// Original typed event for zero-cost in-process dispatch. `None`
    /// for events loaded from durable stores.
    pub ephemeral: Option<Arc<dyn Any + Send + Sync>>,
    /// Always `true` under v0.4 (facts are persistent, per P1.5).
    pub persistent: bool,
}

impl fmt::Debug for NewEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NewEvent")
            .field("event_id", &self.event_id)
            .field("parent_id", &self.parent_id)
            .field("correlation_id", &self.correlation_id)
            .field("event_type", &self.event_type)
            .field("payload", &self.payload)
            .field("created_at", &self.created_at)
            .field("aggregate_type", &self.aggregate_type)
            .field("aggregate_id", &self.aggregate_id)
            .field("metadata", &self.metadata)
            .field("ephemeral", &self.ephemeral.as_ref().map(|_| "..."))
            .finish()
    }
}

/// Serialized snapshot of aggregate state at a specific stream version.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub aggregate_type: String,
    pub aggregate_id: Uuid,
    pub version: StreamVersion,
    pub state: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

#[cfg(test)]
mod persisted_event_tests {
    use super::*;

    fn mk(event_type: &str) -> PersistedEvent {
        PersistedEvent {
            position:        LogCursor::ZERO,
            event_id:        Uuid::nil(),
            parent_id:       None,
            correlation_id:  Uuid::nil(),
            event_type:      event_type.into(),
            payload:         serde_json::Value::Null,
            created_at:      Utc::now(),
            aggregate_type:  None,
            aggregate_id:    None,
            version:         None,
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
