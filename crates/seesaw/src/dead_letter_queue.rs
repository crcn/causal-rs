//! Dead Letter Queue for handlers that fail permanently
//!
//! When a handler exhausts its retry attempts, it's moved to the DLQ where
//! operators can inspect, retry, or resolve the failure.

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::PgPool;
use std::fmt;
use uuid::Uuid;

/// Status of a dead letter queue entry
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "seesaw_dlq_status", rename_all = "lowercase")]
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
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
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

/// Dead Letter Queue operations
#[derive(Clone)]
pub struct DeadLetterQueue {
    pool: PgPool,
}

impl DeadLetterQueue {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Insert a handler failure into the DLQ
    pub async fn insert(
        &self,
        event_id: Uuid,
        handler_id: &str,
        intent_id: Uuid,
        error_message: &str,
        error_details: Option<Value>,
        retry_count: i32,
        first_failed_at: DateTime<Utc>,
        last_failed_at: DateTime<Utc>,
        event_payload: Value,
    ) -> Result<Uuid> {
        let id = sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO seesaw_dead_letter_queue
             (event_id, handler_id, intent_id, error_message, error_details,
              retry_count, first_failed_at, last_failed_at, event_payload, status)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 'open')
             ON CONFLICT (intent_id) DO UPDATE SET
               error_message = EXCLUDED.error_message,
               error_details = EXCLUDED.error_details,
               retry_count = EXCLUDED.retry_count,
               last_failed_at = EXCLUDED.last_failed_at,
               status = 'open'
             RETURNING id",
        )
        .bind(event_id)
        .bind(handler_id)
        .bind(intent_id)
        .bind(error_message)
        .bind(error_details)
        .bind(retry_count)
        .bind(first_failed_at)
        .bind(last_failed_at)
        .bind(event_payload)
        .fetch_one(&self.pool)
        .await?;

        Ok(id)
    }

    /// List open or retrying DLQ entries
    pub async fn list(&self, limit: i64) -> Result<Vec<DeadLetter>> {
        let entries = sqlx::query_as::<_, DeadLetter>(
            "SELECT * FROM seesaw_dead_letter_queue
             WHERE status IN ('open', 'retrying')
             ORDER BY created_at DESC
             LIMIT $1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(entries)
    }

    /// List DLQ entries for a specific handler
    pub async fn list_by_handler(&self, handler_id: &str, limit: i64) -> Result<Vec<DeadLetter>> {
        let entries = sqlx::query_as::<_, DeadLetter>(
            "SELECT * FROM seesaw_dead_letter_queue
             WHERE handler_id = $1 AND status IN ('open', 'retrying')
             ORDER BY created_at DESC
             LIMIT $2",
        )
        .bind(handler_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(entries)
    }

    /// Get a single DLQ entry by ID
    pub async fn get(&self, dlq_id: Uuid) -> Result<DeadLetter> {
        let entry =
            sqlx::query_as::<_, DeadLetter>("SELECT * FROM seesaw_dead_letter_queue WHERE id = $1")
                .bind(dlq_id)
                .fetch_one(&self.pool)
                .await?;

        Ok(entry)
    }

    /// Mark a DLQ entry as retrying and return it (with lock)
    pub async fn start_retry(&self, dlq_id: Uuid) -> Result<DeadLetter> {
        let mut tx = self.pool.begin().await?;

        // Lock row so only one operator retries this entry at a time
        let entry = sqlx::query_as::<_, DeadLetter>(
            "SELECT * FROM seesaw_dead_letter_queue
             WHERE id = $1
             FOR UPDATE",
        )
        .bind(dlq_id)
        .fetch_one(&mut *tx)
        .await?;

        // Update status to retrying
        sqlx::query(
            "UPDATE seesaw_dead_letter_queue
             SET status = 'retrying',
                 retry_attempts = retry_attempts + 1,
                 last_retry_at = NOW()
             WHERE id = $1",
        )
        .bind(dlq_id)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;

        Ok(entry)
    }

    /// Mark a DLQ entry as successfully replayed
    pub async fn mark_replayed(&self, dlq_id: Uuid, resolution_note: Option<&str>) -> Result<()> {
        sqlx::query(
            "UPDATE seesaw_dead_letter_queue
             SET status = 'replayed',
                 resolved_at = NOW(),
                 resolution_note = $2
             WHERE id = $1",
        )
        .bind(dlq_id)
        .bind(resolution_note)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Mark a DLQ entry as failed (retry failed, back to open)
    pub async fn mark_retry_failed(
        &self,
        dlq_id: Uuid,
        error_message: &str,
        error_details: Option<Value>,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE seesaw_dead_letter_queue
             SET status = 'open',
                 last_failed_at = NOW(),
                 error_message = $2,
                 error_details = $3
             WHERE id = $1",
        )
        .bind(dlq_id)
        .bind(error_message)
        .bind(error_details)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Mark a DLQ entry as resolved (won't retry, manual fix)
    pub async fn mark_resolved(&self, dlq_id: Uuid, resolution_note: &str) -> Result<()> {
        sqlx::query(
            "UPDATE seesaw_dead_letter_queue
             SET status = 'resolved',
                 resolved_at = NOW(),
                 resolution_note = $2
             WHERE id = $1",
        )
        .bind(dlq_id)
        .bind(resolution_note)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Get statistics about the DLQ
    pub async fn stats(&self) -> Result<DlqStats> {
        #[derive(sqlx::FromRow)]
        struct StatsRow {
            open: Option<i64>,
            retrying: Option<i64>,
            replayed: Option<i64>,
            resolved: Option<i64>,
            total: Option<i64>,
        }

        let row: StatsRow = sqlx::query_as(
            "SELECT
                COUNT(*) FILTER (WHERE status = 'open') as open,
                COUNT(*) FILTER (WHERE status = 'retrying') as retrying,
                COUNT(*) FILTER (WHERE status = 'replayed') as replayed,
                COUNT(*) FILTER (WHERE status = 'resolved') as resolved,
                COUNT(*) as total
             FROM seesaw_dead_letter_queue",
        )
        .fetch_one(&self.pool)
        .await?;

        Ok(DlqStats {
            open: row.open.unwrap_or(0) as usize,
            retrying: row.retrying.unwrap_or(0) as usize,
            replayed: row.replayed.unwrap_or(0) as usize,
            resolved: row.resolved.unwrap_or(0) as usize,
            total: row.total.unwrap_or(0) as usize,
        })
    }
}

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
