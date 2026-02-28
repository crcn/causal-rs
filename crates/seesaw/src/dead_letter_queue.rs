//! Dead Letter Queue for handlers that fail permanently
//!
//! When a handler exhausts its retry attempts, it's moved to the DLQ where
//! operators can inspect, retry, or resolve the failure.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;
use uuid::Uuid;

/// Status of a dead letter queue entry
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DlqStatus {
    /// Entry is open and can be retried
    Open,
    /// Currently being retried
    Retrying,
    /// Event was successfully replayed
    Replayed,
    /// Issue resolved by operator (manual fix, won't retry)
    Resolved,
}

impl fmt::Display for DlqStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DlqStatus::Open => write!(f, "open"),
            DlqStatus::Retrying => write!(f, "retrying"),
            DlqStatus::Replayed => write!(f, "replayed"),
            DlqStatus::Resolved => write!(f, "resolved"),
        }
    }
}

/// A dead letter queue entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeadLetter {
    pub id: Uuid,
    pub event_id: Uuid,
    pub handler_id: String,
    pub intent_id: Uuid,
    pub error_message: String,
    pub error_details: Option<Value>,
    pub retry_count: i32,
    pub first_failed_at: DateTime<Utc>,
    pub last_failed_at: DateTime<Utc>,
    pub event_payload: Value,
    pub status: DlqStatus,
    pub retry_attempts: i32,
    pub last_retry_at: Option<DateTime<Utc>>,
    pub resolved_at: Option<DateTime<Utc>>,
    pub resolution_note: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// Summary of a bulk retry operation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetrySummary {
    pub succeeded: usize,
    pub failed: usize,
    pub total: usize,
}

impl RetrySummary {
    pub fn new(succeeded: usize, failed: usize) -> Self {
        Self {
            succeeded,
            failed,
            total: succeeded + failed,
        }
    }
}

// DeadLetterQueue implementation has been moved to seesaw-postgres crate

/// Statistics about the DLQ
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DlqStats {
    pub open: usize,
    pub retrying: usize,
    pub replayed: usize,
    pub resolved: usize,
    pub total: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_dlq_lifecycle() {
        // This would be a real integration test with a test database
        // For now, just a placeholder structure

        // 1. Insert DLQ entry
        // 2. List entries
        // 3. Start retry (with lock)
        // 4. Mark as replayed
        // 5. Verify status changed
    }

    #[tokio::test]
    async fn test_dlq_prevent_duplicate_retry() {
        // Test that two operators can't retry the same entry simultaneously
        // Uses FOR UPDATE lock
    }

    #[tokio::test]
    async fn test_dlq_stats() {
        // Test statistics calculation
    }
}
