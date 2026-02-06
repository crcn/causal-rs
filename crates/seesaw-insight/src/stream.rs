//! Stream reader for observability events

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};
use uuid::Uuid;

/// Stream entry from seesaw_stream table
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamEntry {
    pub seq: i64,
    pub stream_type: String,
    pub correlation_id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effect_event_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effect_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Value>,
    pub created_at: DateTime<Utc>,
}

/// Stream reader for cursor-based pagination
pub struct StreamReader {
    pool: PgPool,
}

impl StreamReader {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Fetch stream entries starting from cursor
    ///
    /// Returns (entries, next_cursor) where next_cursor is None if no more entries
    pub async fn fetch(
        &self,
        cursor: Option<i64>,
        limit: i64,
    ) -> Result<(Vec<StreamEntry>, Option<i64>)> {
        let cursor = cursor.unwrap_or(0);
        let limit = limit.min(1000); // Cap at 1000 entries per request

        let rows = sqlx::query(
            r#"
            SELECT
                seq,
                stream_type,
                correlation_id,
                event_id,
                effect_event_id,
                effect_id,
                status,
                error,
                payload,
                created_at
            FROM seesaw_stream
            WHERE seq > $1
            ORDER BY seq ASC
            LIMIT $2
            "#,
        )
        .bind(cursor)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        let entries = rows
            .into_iter()
            .map(|row| StreamEntry {
                seq: row.get("seq"),
                stream_type: row.get("stream_type"),
                correlation_id: row.get("correlation_id"),
                event_id: row.get("event_id"),
                effect_event_id: row.get("effect_event_id"),
                effect_id: row.get("effect_id"),
                status: row.get("status"),
                error: row.get("error"),
                payload: row.get("payload"),
                created_at: row.get("created_at"),
            })
            .collect::<Vec<_>>();

        let next_cursor = entries.last().map(|e| e.seq);

        Ok((entries, next_cursor))
    }

    /// Fetch entries for a specific workflow
    pub async fn fetch_workflow(
        &self,
        correlation_id: Uuid,
        cursor: Option<i64>,
        limit: i64,
    ) -> Result<(Vec<StreamEntry>, Option<i64>)> {
        let cursor = cursor.unwrap_or(0);
        let limit = limit.min(1000);

        let rows = sqlx::query(
            r#"
            SELECT
                seq,
                stream_type,
                correlation_id,
                event_id,
                effect_event_id,
                effect_id,
                status,
                error,
                payload,
                created_at
            FROM seesaw_stream
            WHERE correlation_id = $1 AND seq > $2
            ORDER BY seq ASC
            LIMIT $3
            "#,
        )
        .bind(correlation_id)
        .bind(cursor)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        let entries = rows
            .into_iter()
            .map(|row| StreamEntry {
                seq: row.get("seq"),
                stream_type: row.get("stream_type"),
                correlation_id: row.get("correlation_id"),
                event_id: row.get("event_id"),
                effect_event_id: row.get("effect_event_id"),
                effect_id: row.get("effect_id"),
                status: row.get("status"),
                error: row.get("error"),
                payload: row.get("payload"),
                created_at: row.get("created_at"),
            })
            .collect::<Vec<_>>();

        let next_cursor = entries.last().map(|e| e.seq);

        Ok((entries, next_cursor))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_entry_serialization() {
        let entry = StreamEntry {
            seq: 123,
            stream_type: "event_dispatched".to_string(),
            correlation_id: Uuid::new_v4(),
            event_id: Some(Uuid::new_v4()),
            effect_event_id: None,
            effect_id: None,
            status: None,
            error: None,
            payload: Some(serde_json::json!({"order_id": 456})),
            created_at: Utc::now(),
        };

        let json = serde_json::to_string(&entry).expect("should serialize");
        let _deserialized: StreamEntry = serde_json::from_str(&json).expect("should deserialize");
    }
}
