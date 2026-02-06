//! Insight Store trait for observability and visualization
//!
//! This module provides the InsightStore trait which is separate from the Store trait.
//! It's designed for observability systems (like seesaw-insight) to query workflow history,
//! subscribe to events, and get aggregate statistics.
//!
//! Both PostgresStore and MemoryStore can implement this trait to provide visualization
//! capabilities without coupling the core runtime to any specific backend.

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Event for insight visualization stream
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InsightEvent {
    /// Sequence number (for cursor-based pagination)
    pub seq: i64,
    /// Stream entry type
    pub stream_type: StreamType,
    /// Correlation ID (workflow identifier)
    pub correlation_id: Uuid,
    /// Event ID (if event-related)
    pub event_id: Option<Uuid>,
    /// Effect event ID (if effect-related)
    pub effect_event_id: Option<Uuid>,
    /// Effect ID (if effect-related)
    pub effect_id: Option<String>,
    /// Event type name
    pub event_type: Option<String>,
    /// Status (for effect lifecycle)
    pub status: Option<String>,
    /// Error message (if failed)
    pub error: Option<String>,
    /// Payload (JSON)
    pub payload: Option<serde_json::Value>,
    /// When this entry was created
    pub created_at: DateTime<Utc>,
}

/// Type of stream entry
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StreamType {
    /// Event dispatched
    EventDispatched,
    /// Effect started
    EffectStarted,
    /// Effect completed
    EffectCompleted,
    /// Effect failed
    EffectFailed,
}

/// Workflow tree for visualization
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowTree {
    /// Correlation ID
    pub correlation_id: Uuid,
    /// Root events (events with no parent)
    pub roots: Vec<EventNode>,
    /// Workflow state (if exists)
    pub state: Option<serde_json::Value>,
    /// Total event count
    pub event_count: usize,
    /// Total effect count
    pub effect_count: usize,
}

/// Event node in the tree
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventNode {
    /// Event ID
    pub event_id: Uuid,
    /// Event type
    pub event_type: String,
    /// Payload
    pub payload: serde_json::Value,
    /// When created
    pub created_at: DateTime<Utc>,
    /// Child events
    pub children: Vec<EventNode>,
    /// Effects triggered by this event
    pub effects: Vec<EffectNode>,
}

/// Effect node in the tree
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EffectNode {
    /// Effect ID
    pub effect_id: String,
    /// Event ID that triggered this effect
    pub event_id: Uuid,
    /// Status (pending, executing, completed, failed)
    pub status: String,
    /// Result (if completed)
    pub result: Option<serde_json::Value>,
    /// Error (if failed)
    pub error: Option<String>,
    /// Attempts
    pub attempts: i32,
    /// When created
    pub created_at: DateTime<Utc>,
}

/// Aggregate statistics for insight dashboard
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InsightStats {
    /// Total events
    pub total_events: i64,
    /// Active effects (pending or executing)
    pub active_effects: i64,
    /// Completed effects
    pub completed_effects: i64,
    /// Failed effects
    pub failed_effects: i64,
}

/// Effect execution log entry for troubleshooting and timing analysis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EffectExecutionLog {
    /// Workflow identifier.
    pub correlation_id: Uuid,
    /// Triggering event ID.
    pub event_id: Uuid,
    /// Effect identifier.
    pub effect_id: String,
    /// Current status (pending, executing, completed, failed).
    pub status: String,
    /// Attempt count.
    pub attempts: i32,
    /// Event type that triggered the effect.
    pub event_type: Option<String>,
    /// Effect result payload.
    pub result: Option<serde_json::Value>,
    /// Error text (if failed).
    pub error: Option<String>,
    /// Intent creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Scheduled execution time.
    pub execute_at: Option<DateTime<Utc>>,
    /// Last claim timestamp.
    pub claimed_at: Option<DateTime<Utc>>,
    /// Last attempted timestamp.
    pub last_attempted_at: Option<DateTime<Utc>>,
    /// Completion timestamp.
    pub completed_at: Option<DateTime<Utc>>,
    /// Derived execution duration when available.
    pub duration_ms: Option<i64>,
}

/// Dead letter queue row for permanently failed effects.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeadLetterEntry {
    /// Workflow identifier.
    pub correlation_id: Uuid,
    /// Triggering event ID.
    pub event_id: Uuid,
    /// Effect identifier.
    pub effect_id: String,
    /// Triggering event type.
    pub event_type: String,
    /// Triggering event payload snapshot.
    pub event_payload: serde_json::Value,
    /// Failure reason text.
    pub error: String,
    /// DLQ classification.
    pub reason: String,
    /// Retry attempts before DLQ.
    pub attempts: i32,
    /// Timestamp when moved to DLQ.
    pub failed_at: DateTime<Utc>,
    /// Timestamp when manually resolved.
    pub resolved_at: Option<DateTime<Utc>>,
}

/// Aggregated failure summary per workflow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailedWorkflow {
    /// Workflow identifier.
    pub correlation_id: Uuid,
    /// Number of failed effects in executions table.
    pub failed_effects: i64,
    /// Number of pending/executing effects.
    pub active_effects: i64,
    /// Number of unresolved dead letters.
    pub dead_letters: i64,
    /// Most recent failure timestamp.
    pub last_failed_at: Option<DateTime<Utc>>,
    /// Most recent failure message.
    pub last_error: Option<String>,
}

/// Trait for insight/observability queries
///
/// This is separate from the Store trait because:
/// - Store is for runtime execution (queue, state, effects)
/// - InsightStore is for visualization and observability
/// - Different backends may implement them differently
/// - Keeps core runtime decoupled from observability concerns
#[async_trait]
pub trait InsightStore: Send + Sync + 'static {
    /// Subscribe to live events for all workflows
    ///
    /// Returns a stream that yields InsightEvent entries as they occur.
    /// Used by SSE/WebSocket endpoints for real-time updates.
    async fn subscribe_events(
        &self,
    ) -> Result<Box<dyn futures::Stream<Item = InsightEvent> + Send + Unpin>>;

    /// Get workflow tree for visualization
    ///
    /// Reconstructs the full event tree with effects for a given correlation ID.
    async fn get_workflow_tree(&self, correlation_id: Uuid) -> Result<WorkflowTree>;

    /// Get aggregate statistics
    ///
    /// Returns counts for dashboard display.
    async fn get_stats(&self) -> Result<InsightStats>;

    /// Get recent events with cursor-based pagination
    ///
    /// Returns events starting from cursor (sequence number).
    /// If cursor is None, returns most recent events.
    async fn get_recent_events(&self, cursor: Option<i64>, limit: usize)
        -> Result<Vec<InsightEvent>>;

    /// Get effect execution logs for operational debugging.
    ///
    /// Default implementation returns no rows for stores that do not expose this.
    async fn get_effect_logs(
        &self,
        correlation_id: Option<Uuid>,
        limit: usize,
    ) -> Result<Vec<EffectExecutionLog>> {
        let _ = (correlation_id, limit);
        Ok(Vec::new())
    }

    /// Get dead letter queue entries.
    ///
    /// Default implementation returns no rows for stores that do not expose DLQ state.
    async fn get_dead_letters(
        &self,
        unresolved_only: bool,
        limit: usize,
    ) -> Result<Vec<DeadLetterEntry>> {
        let _ = (unresolved_only, limit);
        Ok(Vec::new())
    }

    /// Get workflows that currently have failures or dead letters.
    ///
    /// Default implementation returns no rows for stores without failure aggregation.
    async fn get_failed_workflows(&self, limit: usize) -> Result<Vec<FailedWorkflow>> {
        let _ = limit;
        Ok(Vec::new())
    }
}
