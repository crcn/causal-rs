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
}
