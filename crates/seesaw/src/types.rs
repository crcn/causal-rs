//! Core data types for seesaw event processing.

use chrono::{DateTime, Utc};
use std::any::Any;
use std::collections::HashMap;
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

/// Event emitted by a handler (for atomic insertion)
#[derive(Debug, Clone)]
pub struct EmittedEvent {
    /// Event type name (for deterministic ID generation)
    pub event_type: String,
    /// Event payload (JSON)
    pub payload: serde_json::Value,
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

// ── New types (EventLog + HandlerQueue split) ─────────────────────

/// Atomic intent creation payload.
///
/// Produced by the engine's `process_event` during Phase 1 of the settle
/// loop. Contains handler intents to enqueue, projection failures to DLQ,
/// and the checkpoint position to advance to.
#[derive(Debug)]
pub struct IntentCommit {
    /// Source event identifiers.
    pub event_id: Uuid,
    pub correlation_id: Uuid,
    pub event_type: String,
    pub event_payload: serde_json::Value,
    /// Queued handler intents to persist.
    pub intents: Vec<HandlerIntent>,
    /// Projection failures to persist to DLQ.
    pub projection_failures: Vec<ProjectionFailure>,
    /// Handler gate descriptions (handler_id → serialized describe output).
    pub handler_descriptions: HashMap<String, serde_json::Value>,
    /// Advance checkpoint to this EventLog position.
    pub checkpoint: u64,
    /// Park this event in DLQ (exceeded hops, exceeded retry, etc.)
    pub park: Option<EventPark>,
}

impl IntentCommit {
    /// Park an event in DLQ and advance checkpoint past it.
    pub fn park(event: &PersistedEvent, reason: impl Into<String>) -> Self {
        Self {
            event_id: event.event_id,
            correlation_id: event.correlation_id,
            event_type: event.event_type.clone(),
            event_payload: event.payload.clone(),
            intents: Vec::new(),
            projection_failures: Vec::new(),
            handler_descriptions: HashMap::new(),
            checkpoint: event.position,
            park: Some(EventPark {
                reason: reason.into(),
            }),
        }
    }

    /// Skip an event (advance checkpoint, no intents, no DLQ).
    pub fn skip(event: &PersistedEvent) -> Self {
        Self {
            event_id: event.event_id,
            correlation_id: event.correlation_id,
            event_type: event.event_type.clone(),
            event_payload: event.payload.clone(),
            intents: Vec::new(),
            projection_failures: Vec::new(),
            handler_descriptions: HashMap::new(),
            checkpoint: event.position,
            park: None,
        }
    }
}

/// Reason for parking (DLQ-ing) an event during processing.
#[derive(Debug, Clone)]
pub struct EventPark {
    pub reason: String,
}

/// Persisted intent for a queued handler execution.
#[derive(Debug, Clone)]
pub struct HandlerIntent {
    /// Stable handler identifier.
    pub handler_id: String,
    /// Parent event for causality tracking.
    pub parent_event_id: Option<Uuid>,
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
    pub execute_at: DateTime<Utc>,
    pub timeout_seconds: i32,
    pub max_attempts: i32,
    pub priority: i32,
    pub hops: i32,
    pub attempts: i32,
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
    /// Maximum retry count for event-level failures.
    pub max_event_retry_attempts: i32,
}

impl Default for EventWorkerConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_millis(100),
            max_hops: 50,
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
}

impl Default for HandlerWorkerConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_millis(100),
            default_timeout: Duration::from_secs(30),
        }
    }
}

/// Atomic handler completion payload.
#[derive(Debug)]
pub struct HandlerCompletion {
    pub event_id: Uuid,
    pub handler_id: String,
    pub result: serde_json::Value,
    /// Log entries captured during handler execution.
    pub log_entries: Vec<LogEntry>,
}

/// Atomic DLQ + terminal-event payload.
#[derive(Debug)]
pub struct HandlerDlq {
    pub event_id: Uuid,
    pub handler_id: String,
    pub error: String,
    pub reason: String,
    pub attempts: i32,
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
#[derive(Clone)]
pub struct PersistedEvent {
    /// Opaque global ordering cursor. Monotonically increasing; gaps between
    /// values are allowed. Used for checkpoint-based cursor tracking —
    /// no arithmetic should be performed on this value.
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
    /// Original typed event, available only during live dispatch in-process.
    /// `None` on load from durable store — handlers fall back to JSON deserialization.
    pub ephemeral: Option<Arc<dyn Any + Send + Sync>>,
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
    /// Original typed event for zero-cost dispatch in-process.
    /// `None` for events loaded from durable stores.
    pub ephemeral: Option<Arc<dyn Any + Send + Sync>>,
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
    pub pending_handlers: usize,
    pub dead_lettered: usize,
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
