//! Optional backend capabilities for v0.11.0
//!
//! These traits enable compile-time checking of backend features.
//! Backends implement only the capabilities they support.

use anyhow::Result;
use async_trait::async_trait;
use futures::Stream;
use uuid::Uuid;

use super::Backend;
use crate::store::{WorkflowEvent, WorkflowStatus};

/// Workflow status query capability.
///
/// Backends that implement this can answer "is my workflow done?"
#[async_trait]
pub trait WorkflowStatusBackend: Backend {
    /// Get workflow status for a correlation ID.
    async fn get_workflow_status(&self, correlation_id: Uuid) -> Result<WorkflowStatus>;
}

/// Workflow event subscription capability.
///
/// Backends that implement this support `.wait()` on ProcessFuture.
#[async_trait]
pub trait WorkflowSubscriptionBackend: Backend {
    /// Subscribe to workflow events (LISTEN/NOTIFY pattern).
    ///
    /// Returns a stream that yields events for the given correlation ID.
    async fn subscribe_workflow_events(
        &self,
        correlation_id: Uuid,
    ) -> Result<Box<dyn Stream<Item = WorkflowEvent> + Send + Unpin>>;
}

/// Dead letter queue capability.
///
/// Backends that implement this support DLQ operations.
#[async_trait]
pub trait DeadLetterQueueBackend: Backend {
    /// List dead letters with optional filters.
    async fn list_dlq(&self, filters: DlqFilters) -> Result<Vec<DeadLetter>>;

    /// Retry a dead letter (re-queue for execution).
    async fn retry_dlq(&self, event_id: Uuid, handler_id: String) -> Result<()>;
}

/// Filters for DLQ listing.
#[derive(Debug, Clone, Default)]
pub struct DlqFilters {
    /// Filter by correlation ID.
    pub correlation_id: Option<Uuid>,
    /// Filter by handler ID.
    pub handler_id: Option<String>,
    /// Filter by reason.
    pub reason: Option<String>,
    /// Limit number of results.
    pub limit: Option<usize>,
}

/// Dead letter entry.
#[derive(Debug, Clone)]
pub struct DeadLetter {
    pub event_id: Uuid,
    pub handler_id: String,
    pub correlation_id: Uuid,
    pub event_type: String,
    pub error: String,
    pub reason: String,
    pub attempts: i32,
}

/// Workflow insight capability.
///
/// Backends that implement this support tree visualization and stats.
#[async_trait]
pub trait InsightBackend: Backend {
    /// Get workflow execution tree.
    async fn get_workflow_tree(&self, correlation_id: Uuid) -> Result<WorkflowTree>;

    /// Get aggregate workflow statistics.
    async fn get_insight_stats(&self) -> Result<InsightStats>;
}

/// Workflow execution tree (for visualization).
#[derive(Debug, Clone)]
pub struct WorkflowTree {
    pub correlation_id: Uuid,
    pub nodes: Vec<TreeNode>,
}

/// Node in workflow tree.
#[derive(Debug, Clone)]
pub struct TreeNode {
    pub event_id: Uuid,
    pub parent_id: Option<Uuid>,
    pub event_type: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Aggregate workflow statistics.
#[derive(Debug, Clone)]
pub struct InsightStats {
    pub total_workflows: i64,
    pub active_workflows: i64,
    pub settled_workflows: i64,
    pub total_events: i64,
    pub total_handlers_executed: i64,
    pub total_dlq_entries: i64,
}
