use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;
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
}

/// Store trait - combines queue and state operations
///
/// Event notification from LISTEN/NOTIFY for .wait() pattern
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WorkflowEvent {
    pub event_id: Uuid,
    pub correlation_id: Uuid,
    pub event_type: String,
    pub payload: serde_json::Value,
}

/// Single trait because they share same database/pool/transactions.
/// This enables:
/// - Transactional state updates + event publishing
/// - Inline effects in same transaction as state commit
/// - Atomic event acknowledgement + state save
#[async_trait]
pub trait Store: Send + Sync + 'static {
    // =========================================================================
    // Queue Operations
    // =========================================================================

    /// Publish event to queue
    ///
    /// Returns immediately after INSERT. Idempotent via UNIQUE(event_id).
    async fn publish(&self, event: QueuedEvent) -> Result<()>;

    /// Poll next event (per-workflow FIFO with advisory locks)
    ///
    /// Returns None if no events available.
    /// Uses SKIP LOCKED for concurrent workers.
    async fn poll_next(&self) -> Result<Option<QueuedEvent>>;

    /// Acknowledge event processing completion
    ///
    /// Sets processed_at = NOW(), releases advisory lock.
    async fn ack(&self, id: i64) -> Result<()>;

    /// Nack event (failed, retry later)
    ///
    /// Increments retry_count, sets locked_until for exponential backoff.
    async fn nack(&self, id: i64, retry_after_secs: u64) -> Result<()>;

    // =========================================================================
    // State Operations
    // =========================================================================

    /// Load state for workflow
    ///
    /// Returns None if workflow has no state yet.
    async fn load_state<S>(&self, correlation_id: Uuid) -> Result<Option<(S, i32)>>
    where
        S: for<'de> Deserialize<'de> + Send;

    /// Save state for workflow (optimistic locking)
    ///
    /// Returns error if version mismatch (concurrent modification detected).
    async fn save_state<S>(
        &self,
        correlation_id: Uuid,
        state: &S,
        expected_version: i32,
    ) -> Result<i32>
    where
        S: Serialize + Send + Sync;

    // =========================================================================
    // Effect Execution Operations
    // =========================================================================

    /// Insert effect execution intent
    ///
    /// Called by event worker after running reducers.
    /// Effect workers poll this table.
    async fn insert_effect_intent(
        &self,
        event_id: Uuid,
        effect_id: String,
        correlation_id: Uuid,
        event_type: String,
        event_payload: serde_json::Value,
        parent_event_id: Option<Uuid>,
        execute_at: DateTime<Utc>,
        timeout_seconds: i32,
        max_attempts: i32,
        priority: i32,
    ) -> Result<()>;

    /// Poll next ready effect (priority-based)
    ///
    /// Returns None if no effects ready.
    /// Uses SKIP LOCKED for concurrent workers.
    async fn poll_next_effect(&self) -> Result<Option<QueuedEffectExecution>>;

    /// Mark effect execution as completed
    async fn complete_effect(
        &self,
        event_id: Uuid,
        effect_id: String,
        result: serde_json::Value,
    ) -> Result<()>;

    /// Complete effect and atomically publish emitted events (crash-safe)
    ///
    /// Generates deterministic event IDs from hash(parent_event_id, effect_id, event_type).
    /// Uses single transaction so both succeed or both fail (idempotent on retry).
    async fn complete_effect_with_events(
        &self,
        event_id: Uuid,
        effect_id: String,
        result: serde_json::Value,
        emitted_events: Vec<EmittedEvent>,
    ) -> Result<()>;

    /// Mark effect execution as failed and schedule retry (store-defined backoff)
    async fn fail_effect(
        &self,
        event_id: Uuid,
        effect_id: String,
        error: String,
        attempts: i32,
    ) -> Result<()>;

    /// Move effect to DLQ (permanently failed)
    async fn dlq_effect(
        &self,
        event_id: Uuid,
        effect_id: String,
        error: String,
        reason: String,
        attempts: i32,
    ) -> Result<()>;

    // =========================================================================
    // LISTEN/NOTIFY Operations (for .wait() pattern)
    // =========================================================================

    /// Subscribe to events for a specific workflow via LISTEN/NOTIFY.
    ///
    /// Returns a stream that yields events as they are emitted in the workflow.
    /// Used by the .wait() pattern to efficiently wait for terminal events.
    async fn subscribe_workflow_events(
        &self,
        correlation_id: Uuid,
    ) -> Result<Box<dyn futures::Stream<Item = WorkflowEvent> + Send + Unpin>>;

    // =========================================================================
    // Workflow Status Operations
    // =========================================================================

    /// Get workflow status for a correlation ID
    ///
    /// Returns status including pending effects count and whether workflow is settled.
    /// A workflow is "settled" when no effects are pending/executing, but can start
    /// up again if new events arrive. Use terminal events for true completion.
    async fn get_workflow_status(&self, correlation_id: Uuid) -> Result<WorkflowStatus>;
}

/// Workflow status for a correlation ID
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowStatus {
    /// Correlation ID (workflow identifier)
    pub correlation_id: Uuid,
    /// Current state (if exists)
    pub state: Option<serde_json::Value>,
    /// Number of effects pending/executing
    pub pending_effects: i64,
    /// True when no effects are pending/executing (workflow is idle)
    pub is_settled: bool,
    /// Most recent event type
    pub last_event: Option<String>,
}

/// Queued effect execution from store
#[derive(Debug, Clone)]
pub struct QueuedEffectExecution {
    pub event_id: Uuid,
    pub effect_id: String,
    pub correlation_id: Uuid,
    pub event_type: String,
    pub event_payload: serde_json::Value,
    pub parent_event_id: Option<Uuid>,
    pub execute_at: DateTime<Utc>,
    pub timeout_seconds: i32,
    pub max_attempts: i32,
    pub priority: i32,
    pub attempts: i32,
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

impl fmt::Display for QueuedEffectExecution {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Effect(event_id={}, effect_id={}, correlation_id={}, priority={}, attempts={}/{})",
            self.event_id,
            self.effect_id,
            self.correlation_id,
            self.priority,
            self.attempts,
            self.max_attempts
        )
    }
}
