//! Core data types for seesaw event processing.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::time::Duration;
use uuid::Uuid;

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
    /// Retry count (for inline effect failure tracking)
    pub retry_count: i32,
    /// Batch identifier for same-batch fan-in semantics.
    pub batch_id: Option<Uuid>,
    /// Position inside a batch (0-based).
    pub batch_index: Option<i32>,
    /// Total number of items in the batch.
    pub batch_size: Option<i32>,
    /// When event was created
    pub created_at: DateTime<Utc>,
}

/// Event emitted by an effect (for atomic insertion)
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
}

/// Captured inline effect failure to persist in DLQ at commit time.
#[derive(Debug, Clone)]
pub struct InlineHandlerFailure {
    pub handler_id: String,
    pub error: String,
    pub reason: String,
    pub attempts: i32,
}

/// Atomic event processing commit payload.
#[derive(Debug)]
pub struct EventProcessingCommit {
    /// Source queue row to acknowledge.
    pub event_row_id: i64,
    /// Source event identifiers.
    pub event_id: Uuid,
    pub correlation_id: Uuid,
    pub event_type: String,
    pub event_payload: serde_json::Value,
    /// Queued effect intents to persist.
    pub queued_effect_intents: Vec<QueuedHandlerIntent>,
    /// Inline effect failures to persist to DLQ.
    pub inline_effect_failures: Vec<InlineHandlerFailure>,
    /// Inline emitted events to publish.
    pub emitted_events: Vec<QueuedEvent>,
}

/// Persisted intent for a queued effect execution.
#[derive(Debug, Clone)]
pub struct QueuedHandlerIntent {
    /// Stable effect identifier.
    pub handler_id: String,
    /// Parent event for causality tracking.
    pub parent_event_id: Option<Uuid>,
    /// Batch metadata inherited from source event.
    pub batch_id: Option<Uuid>,
    pub batch_index: Option<i32>,
    pub batch_size: Option<i32>,
    /// Earliest time this effect can execute.
    pub execute_at: DateTime<Utc>,
    /// Per-effect timeout in seconds.
    pub timeout_seconds: i32,
    /// Max retry attempts for this effect.
    pub max_attempts: i32,
    /// Queue priority (lower = higher priority).
    pub priority: i32,
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

/// Event notification for workflow events.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WorkflowEvent {
    pub event_id: Uuid,
    pub correlation_id: Uuid,
    pub event_type: String,
    pub payload: serde_json::Value,
}

/// Workflow status for a correlation ID
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowStatus {
    /// Correlation ID (workflow identifier)
    pub correlation_id: Uuid,
    /// Number of effects pending/executing
    pub pending_effects: i64,
    /// True when no effects are pending/executing (workflow is idle)
    pub is_settled: bool,
    /// Most recent event type
    pub last_event: Option<String>,
}

/// Queued effect execution from store
#[derive(Debug, Clone)]
pub struct QueuedHandlerExecution {
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
    pub attempts: i32,
    pub join_window_timeout_seconds: Option<i32>,
}

/// Event worker configuration.
#[derive(Debug, Clone)]
pub struct EventWorkerConfig {
    /// Polling interval when no events available.
    pub poll_interval: Duration,
    /// Maximum hop count before DLQ (infinite loop detection).
    pub max_hops: i32,
    /// Maximum number of events an effect may emit in one batch.
    pub max_batch_size: usize,
    /// Maximum retry count for event-level failures in inline processing path.
    pub max_inline_retry_attempts: i32,
}

impl Default for EventWorkerConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_millis(100),
            max_hops: 50,
            max_batch_size: 10_000,
            max_inline_retry_attempts: 3,
        }
    }
}

/// Handler worker configuration.
#[derive(Debug, Clone)]
pub struct HandlerWorkerConfig {
    /// Polling interval when no effects available.
    pub poll_interval: Duration,
    /// Default timeout for effect execution.
    pub default_timeout: Duration,
    /// Maximum number of events an effect may emit in one batch.
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

impl fmt::Display for QueuedHandlerExecution {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Handler(event_id={}, effect_id={}, correlation_id={}, priority={}, attempts={}/{})",
            self.event_id,
            self.handler_id,
            self.correlation_id,
            self.priority,
            self.attempts,
            self.max_attempts
        )
    }
}
