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

/// Persisted intent for a queued effect execution.
#[derive(Debug, Clone)]
pub struct QueuedEffectIntent {
    /// Stable effect identifier.
    pub effect_id: String,
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
}

/// Captured inline effect failure to persist in DLQ at commit time.
#[derive(Debug, Clone)]
pub struct InlineEffectFailure {
    pub effect_id: String,
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
    pub queued_effect_intents: Vec<QueuedEffectIntent>,
    /// Inline effect failures to persist to DLQ.
    pub inline_effect_failures: Vec<InlineEffectFailure>,
    /// Inline emitted events to publish.
    pub emitted_events: Vec<QueuedEvent>,
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

/// Store trait - combines queue and effect operations
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
/// - Transactional effect intent persistence + event publishing
/// - Inline effects in same transaction as event acknowledgement
/// - Atomic event acknowledgement + side-effect persistence
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

    /// Atomically commit event processing side effects.
    ///
    /// Default implementation composes existing store methods sequentially.
    /// Stores can override this with a single transaction for stronger
    /// crash-consistency guarantees.
    async fn commit_event_processing(&self, commit: EventProcessingCommit) -> Result<()> {
        for intent in commit.queued_effect_intents {
            self.insert_effect_intent(
                commit.event_id,
                intent.effect_id,
                commit.correlation_id,
                commit.event_type.clone(),
                commit.event_payload.clone(),
                intent.parent_event_id,
                intent.batch_id,
                intent.batch_index,
                intent.batch_size,
                intent.execute_at,
                intent.timeout_seconds,
                intent.max_attempts,
                intent.priority,
            )
            .await?;
        }

        for event in commit.emitted_events {
            self.publish(event).await?;
        }

        for failure in commit.inline_effect_failures {
            self.dlq_effect(
                commit.event_id,
                failure.effect_id,
                failure.error,
                failure.reason,
                failure.attempts,
            )
            .await?;
        }

        self.ack(commit.event_row_id).await?;
        Ok(())
    }

    // =========================================================================
    // Effect Execution Operations
    // =========================================================================

    /// Insert effect execution intent
    ///
    /// Called by event worker after effect routing/planning.
    /// Effect workers poll this table.
    async fn insert_effect_intent(
        &self,
        event_id: Uuid,
        effect_id: String,
        correlation_id: Uuid,
        event_type: String,
        event_payload: serde_json::Value,
        parent_event_id: Option<Uuid>,
        batch_id: Option<Uuid>,
        batch_index: Option<i32>,
        batch_size: Option<i32>,
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
    /// Generates deterministic event IDs from
    /// hash(parent_event_id, effect_id, event_type, emitted_index).
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

    /// Move effect to DLQ and atomically publish mapped terminal events.
    ///
    /// Stores that don't support this should return an error when `emitted_events`
    /// is non-empty so the caller can fall back to plain DLQ behavior.
    async fn dlq_effect_with_events(
        &self,
        event_id: Uuid,
        effect_id: String,
        error: String,
        reason: String,
        attempts: i32,
        emitted_events: Vec<EmittedEvent>,
    ) -> Result<()> {
        if emitted_events.is_empty() {
            return self
                .dlq_effect(event_id, effect_id, error, reason, attempts)
                .await;
        }
        anyhow::bail!("dlq_effect_with_events is not implemented for this store")
    }

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

    // =========================================================================
    // Join Operations (durable same-batch fan-in)
    // =========================================================================

    /// Append a terminal item into a same-batch join window and attempt to claim
    /// the window for processing when complete.
    ///
    /// Returns `Ok(Some(entries))` for exactly one caller when the batch is ready.
    /// Returns `Ok(None)` when still waiting for more terminal items.
    async fn join_same_batch_append_and_maybe_claim(
        &self,
        join_effect_id: String,
        correlation_id: Uuid,
        source_event_id: Uuid,
        source_event_type: String,
        source_payload: serde_json::Value,
        source_created_at: DateTime<Utc>,
        batch_id: Uuid,
        batch_index: i32,
        batch_size: i32,
    ) -> Result<Option<Vec<JoinEntry>>> {
        let _ = (
            join_effect_id,
            correlation_id,
            source_event_id,
            source_event_type,
            source_payload,
            source_created_at,
            batch_id,
            batch_index,
            batch_size,
        );
        anyhow::bail!("join_same_batch is not implemented for this store")
    }

    /// Mark a claimed same-batch window as completed and clear durable join rows.
    async fn join_same_batch_complete(
        &self,
        join_effect_id: String,
        correlation_id: Uuid,
        batch_id: Uuid,
    ) -> Result<()> {
        let _ = (join_effect_id, correlation_id, batch_id);
        anyhow::bail!("join_same_batch is not implemented for this store")
    }

    /// Release a claimed same-batch window back to `open` status after a handler
    /// error so retries can claim it again.
    async fn join_same_batch_release(
        &self,
        join_effect_id: String,
        correlation_id: Uuid,
        batch_id: Uuid,
        _error: String,
    ) -> Result<()> {
        let _ = (join_effect_id, correlation_id, batch_id);
        anyhow::bail!("join_same_batch is not implemented for this store")
    }
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
pub struct QueuedEffectExecution {
    pub event_id: Uuid,
    pub effect_id: String,
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
