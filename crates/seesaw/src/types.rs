//! Core data types for seesaw event processing.

use chrono::{DateTime, Utc};
use std::any::Any;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

// ── Handler logging ──────────────────────────────────────────────

/// Log level for handler log entries.
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
            LogLevel::Info => write!(f, "INFO"),
            LogLevel::Warn => write!(f, "WARN"),
        }
    }
}

/// A structured log entry captured during handler execution.
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub level: LogLevel,
    pub message: String,
    pub data: Option<serde_json::Value>,
    pub timestamp: DateTime<Utc>,
}

// ── Constants ────────────────────────────────────────────────────

/// UUID v5 namespace for deterministic event ID generation
/// Used to prevent duplicate events on crash+retry
/// This is a custom namespace UUID (derived from URL namespace)
pub const NAMESPACE_SEESAW: Uuid = Uuid::from_u128(0x6ba7b8109dad11d180b400c04fd430c8);

/// Queued event from store
#[derive(Debug, Clone)]
pub struct QueuedEvent {
    /// Internal row ID (for ack)
    pub id: i64,
    /// Event ID (UUID, user-defined or generated)
    pub event_id: Uuid,
    /// Parent event ID (for causality tracking)
    pub parent_id: Option<Uuid>,
    /// Correlation ID (workflow identifier)
    pub correlation_id: Uuid,
    /// Event type name (for routing to handlers)
    pub event_type: String,
    /// Event payload (JSON)
    pub payload: serde_json::Value,
    /// Hop count (for infinite loop detection)
    pub hops: i32,
    /// Retry count (for event-level failure tracking)
    pub retry_count: i32,
    /// Batch identifier for same-batch fan-in semantics.
    pub batch_id: Option<Uuid>,
    /// Position inside a batch (0-based).
    pub batch_index: Option<i32>,
    /// Total number of items in the batch.
    pub batch_size: Option<i32>,
    /// Handler that produced this event (None for root/initial events).
    pub handler_id: Option<String>,
    /// When event was created
    pub created_at: DateTime<Utc>,
    /// Original typed event, available only during the live dispatch cycle.
    /// `None` on replay/hydration from store — handlers fall back to JSON deserialization.
    pub ephemeral: Option<Arc<dyn Any + Send + Sync>>,
}

/// Event emitted by a handler (for atomic insertion)
#[derive(Debug, Clone)]
pub struct EmittedEvent {
    /// Event type name (for deterministic ID generation)
    pub event_type: String,
    /// Event payload (JSON)
    pub payload: serde_json::Value,
    /// Batch identifier for same-batch fan-in semantics.
    pub batch_id: Option<Uuid>,
    /// Position inside a batch (0-based).
    pub batch_index: Option<i32>,
    /// Total number of items in the batch.
    pub batch_size: Option<i32>,
    /// Handler that produced this event.
    pub handler_id: Option<String>,
    /// Original typed event (live dispatch only).
    pub ephemeral: Option<Arc<dyn Any + Send + Sync>>,
}

/// Captured projection failure to persist in DLQ at commit time.
#[derive(Debug, Clone)]
pub struct ProjectionFailure {
    pub handler_id: String,
    pub error: String,
    pub reason: String,
    pub attempts: i32,
}

/// Atomic event processing commit payload.
#[derive(Debug)]
pub struct EventCommit {
    /// Source queue row to acknowledge.
    pub event_row_id: i64,
    /// Source event identifiers.
    pub event_id: Uuid,
    pub correlation_id: Uuid,
    pub event_type: String,
    pub event_payload: serde_json::Value,
    /// Queued handler intents to persist.
    pub queued_handler_intents: Vec<HandlerIntent>,
    /// Projection failures to persist to DLQ.
    pub projection_failures: Vec<ProjectionFailure>,
    /// Original typed event (live dispatch only, shared across all handler intents).
    pub ephemeral: Option<Arc<dyn Any + Send + Sync>>,
    /// Handler gate descriptions (handler_id → serialized describe output).
    pub handler_descriptions: std::collections::HashMap<String, serde_json::Value>,
}

/// Persisted intent for a queued handler execution.
#[derive(Debug, Clone)]
pub struct HandlerIntent {
    /// Stable handler identifier.
    pub handler_id: String,
    /// Parent event for causality tracking.
    pub parent_event_id: Option<Uuid>,
    /// Batch metadata inherited from source event.
    pub batch_id: Option<Uuid>,
    pub batch_index: Option<i32>,
    pub batch_size: Option<i32>,
    /// Earliest time this handler can execute.
    pub execute_at: DateTime<Utc>,
    /// Per-handler timeout in seconds.
    pub timeout_seconds: i32,
    /// Max retry attempts for this handler.
    pub max_attempts: i32,
    /// Queue priority (lower = higher priority).
    pub priority: i32,
    /// Hop count inherited from source event (for infinite loop detection).
    pub hops: i32,
    /// Optional timeout for same-batch accumulation windows.
    pub join_window_timeout_seconds: Option<i32>,
}

/// Persisted join entry used for durable same-batch fan-in.
#[derive(Debug, Clone)]
pub struct JoinEntry {
    pub source_event_id: Uuid,
    pub event_type: String,
    pub payload: serde_json::Value,
    pub batch_id: Uuid,
    pub batch_index: i32,
    pub batch_size: i32,
    pub created_at: DateTime<Utc>,
}

/// Expired same-batch accumulation window metadata.
#[derive(Debug, Clone)]
pub struct ExpiredJoinWindow {
    pub join_handler_id: String,
    pub correlation_id: Uuid,
    pub batch_id: Uuid,
    pub source_event_ids: Vec<Uuid>,
}

/// Queued handler execution from store
#[derive(Debug, Clone)]
pub struct QueuedHandler {
    pub event_id: Uuid,
    pub handler_id: String,
    pub correlation_id: Uuid,
    pub event_type: String,
    pub event_payload: serde_json::Value,
    pub parent_event_id: Option<Uuid>,
    pub batch_id: Option<Uuid>,
    pub batch_index: Option<i32>,
    pub batch_size: Option<i32>,
    pub execute_at: DateTime<Utc>,
    pub timeout_seconds: i32,
    pub max_attempts: i32,
    pub priority: i32,
    pub hops: i32,
    pub attempts: i32,
    pub join_window_timeout_seconds: Option<i32>,
    /// Original typed event (live dispatch only).
    pub ephemeral: Option<Arc<dyn Any + Send + Sync>>,
}

/// Event worker configuration.
#[derive(Debug, Clone)]
pub struct EventWorkerConfig {
    /// Polling interval when no events available.
    pub poll_interval: Duration,
    /// Maximum hop count before DLQ (infinite loop detection).
    pub max_hops: i32,
    /// Maximum number of events a handler may emit in one batch.
    pub max_batch_size: usize,
    /// Maximum retry count for event-level failures.
    pub max_event_retry_attempts: i32,
}

impl Default for EventWorkerConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_millis(100),
            max_hops: 50,
            max_batch_size: 10_000,
            max_event_retry_attempts: 3,
        }
    }
}

/// Handler worker configuration.
#[derive(Debug, Clone)]
pub struct HandlerWorkerConfig {
    /// Polling interval when no handlers available.
    pub poll_interval: Duration,
    /// Default timeout for handler execution.
    pub default_timeout: Duration,
    /// Maximum number of events a handler may emit in one batch.
    pub max_batch_size: usize,
}

impl Default for HandlerWorkerConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_millis(100),
            default_timeout: Duration::from_secs(30),
            max_batch_size: 10_000,
        }
    }
}

/// Atomic handler completion payload.
///
/// Bundles the handler result and any events to publish into a single
/// call so Store implementations can commit atomically.
#[derive(Debug)]
pub struct HandlerCompletion {
    pub event_id: Uuid,
    pub handler_id: String,
    pub result: serde_json::Value,
    /// Pre-built queued events with deterministic IDs (generated by Engine).
    pub events_to_publish: Vec<QueuedEvent>,
    /// Log entries captured during handler execution.
    pub log_entries: Vec<LogEntry>,
}

/// Atomic DLQ + terminal-event payload.
///
/// When a handler exhausts retries (or is rejected), the Engine bundles
/// the DLQ metadata and any terminal events from `on_failure`/`on_dlq`
/// into this struct so the Store can persist both atomically.
#[derive(Debug)]
pub struct HandlerDlq {
    pub event_id: Uuid,
    pub handler_id: String,
    pub error: String,
    pub reason: String,
    pub attempts: i32,
    /// Terminal events from on_failure/on_dlq mappers (pre-built with IDs).
    pub events_to_publish: Vec<QueuedEvent>,
    /// Log entries captured during handler execution.
    pub log_entries: Vec<LogEntry>,
}

/// Unified handler resolution outcome.
///
/// Replaces the three separate `complete_handler`, `fail_handler`, `dlq_handler`
/// Store methods with a single `resolve_handler(HandlerResolution)` call.
#[derive(Debug)]
pub enum HandlerResolution {
    /// Handler completed successfully.
    Complete(HandlerCompletion),
    /// Handler failed but can be retried.
    Retry {
        event_id: Uuid,
        handler_id: String,
        error: String,
        new_attempts: i32,
        next_execute_at: DateTime<Utc>,
    },
    /// Handler exhausted retries — send to dead letter queue.
    DeadLetter(HandlerDlq),
}

/// Unified event processing outcome.
///
/// Replaces the separate `commit_event_processing` and `reject_event`
/// Store methods with a single `complete_event(EventOutcome)` call.
#[derive(Debug)]
pub enum EventOutcome {
    /// Event processed successfully — commit handler intents and DLQ projection failures.
    Processed(EventCommit),
    /// Event rejected — send to DLQ and ack.
    Rejected {
        event_row_id: i64,
        event_id: Uuid,
        error: String,
        reason: String,
    },
}

/// Parameters for appending to a join window and optionally claiming it.
#[derive(Debug)]
pub struct JoinAppendParams {
    pub join_handler_id: String,
    pub correlation_id: Uuid,
    pub source_event_id: Uuid,
    pub source_event_type: String,
    pub source_payload: serde_json::Value,
    pub source_created_at: DateTime<Utc>,
    pub batch_id: Uuid,
    pub batch_index: i32,
    pub batch_size: i32,
    pub join_window_timeout_seconds: Option<i32>,
}

// ── Event persistence types ───────────────────────────────────────

/// Result of appending an event to the global log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppendResult {
    /// Opaque global ordering cursor. Monotonically increasing; gaps between
    /// values are allowed. Consumers must treat this as an opaque cursor for
    /// [`Store::load_global_from`](crate::store::Store::load_global_from) and
    /// should not perform arithmetic or assume a gapless sequence.
    pub position: u64,
    /// Per-aggregate stream version at the time of append. `None` for events
    /// that are not scoped to an aggregate.
    pub version: Option<u64>,
}

/// A persisted event loaded from the store.
#[derive(Debug, Clone)]
pub struct PersistedEvent {
    /// Opaque global ordering cursor. Monotonically increasing; gaps between
    /// values are allowed. Used only for
    /// [`Store::load_global_from`](crate::store::Store::load_global_from)
    /// cursor tracking — no arithmetic should be performed on this value.
    pub position: u64,
    /// Unique event ID.
    pub event_id: Uuid,
    /// Parent event that caused this event (None for root events).
    pub parent_id: Option<Uuid>,
    /// Correlation ID linking the full causal tree.
    pub correlation_id: Uuid,
    /// Short stable event type name (e.g. "OrderPlaced").
    pub event_type: String,
    /// JSON payload.
    pub payload: serde_json::Value,
    /// When the event was persisted.
    pub created_at: DateTime<Utc>,
    /// Aggregate type (only present for aggregate-scoped events).
    pub aggregate_type: Option<String>,
    /// Aggregate instance ID (only present for aggregate-scoped events).
    pub aggregate_id: Option<Uuid>,
    /// Per-aggregate stream version (only present for aggregate-scoped events).
    pub version: Option<u64>,
    /// Application-level metadata (e.g. run_id, schema_v, actor).
    pub metadata: serde_json::Map<String, serde_json::Value>,
}

/// A new event to be appended to the global log.
#[derive(Debug, Clone)]
pub struct NewEvent {
    /// Unique event ID.
    pub event_id: Uuid,
    /// Parent event that caused this event (None for root events).
    pub parent_id: Option<Uuid>,
    /// Correlation ID linking the full causal tree.
    pub correlation_id: Uuid,
    /// Short stable event type name (e.g. "OrderPlaced").
    pub event_type: String,
    /// JSON payload.
    pub payload: serde_json::Value,
    /// When the event was created.
    pub created_at: DateTime<Utc>,
    /// Aggregate type (set by engine when aggregators match).
    pub aggregate_type: Option<String>,
    /// Aggregate instance ID (set by engine when aggregators match).
    pub aggregate_id: Option<Uuid>,
    /// Application-level metadata (e.g. run_id, schema_v, actor).
    pub metadata: serde_json::Map<String, serde_json::Value>,
}

/// A serialized snapshot of aggregate state at a specific stream version.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub aggregate_type: String,
    pub aggregate_id: Uuid,
    pub version: u64,
    pub state: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

/// A single journaled result from `ctx.run()`.
#[derive(Debug, Clone)]
pub struct JournalEntry {
    pub seq: u32,
    pub value: serde_json::Value,
}

/// Summary of pending work for a correlation ID.
#[derive(Debug, Clone, Default)]
pub struct QueueStatus {
    pub pending_events: usize,
    pub pending_handlers: usize,
    pub dead_lettered: usize,
}

// Helper to format event for logging
impl fmt::Display for QueuedEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Event(id={}, event_id={}, correlation_id={}, type={}, hops={})",
            self.id, self.event_id, self.correlation_id, self.event_type, self.hops
        )
    }
}

impl fmt::Display for QueuedHandler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Handler(event_id={}, handler_id={}, correlation_id={}, priority={}, attempts={}/{})",
            self.event_id,
            self.handler_id,
            self.correlation_id,
            self.priority,
            self.attempts,
            self.max_attempts
        )
    }
}
