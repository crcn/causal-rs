use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use seesaw_core::es::{ConcurrencyError, EventStore, NewEvent, StoredEvent};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

/// PostgreSQL-backed event store with optimistic concurrency.
pub struct PostgresEventStore {
    pool: PgPool,
}

impl PostgresEventStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}

#[derive(FromRow)]
struct EventRow {
    id: Uuid,
    position: i64,
    aggregate_id: Uuid,
    sequence: i64,
    event_type: String,
    event_data: serde_json::Value,
    metadata: serde_json::Value,
    schema_version: i32,
    caused_by: Option<Uuid>,
    created_at: DateTime<Utc>,
}

impl From<EventRow> for StoredEvent {
    fn from(row: EventRow) -> Self {
        StoredEvent {
            id: row.id,
            position: row.position as u64,
            aggregate_id: row.aggregate_id,
            sequence: row.sequence as u64,
            event_type: row.event_type,
            data: row.event_data,
            metadata: row.metadata,
            schema_version: row.schema_version as u32,
            caused_by: row.caused_by,
            created_at: row.created_at,
        }
    }
}

#[async_trait]
impl EventStore for PostgresEventStore {
    async fn load_events(
        &self,
        aggregate_id: Uuid,
        from_version: u64,
    ) -> Result<Vec<StoredEvent>> {
        let rows: Vec<EventRow> = sqlx::query_as(
            "SELECT id, position, aggregate_id, sequence, event_type, \
                    event_data, metadata, schema_version, caused_by, created_at \
             FROM seesaw_event_store \
             WHERE aggregate_id = $1 AND sequence > $2 \
             ORDER BY sequence ASC",
        )
        .bind(aggregate_id)
        .bind(from_version as i64)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(StoredEvent::from).collect())
    }

    async fn append(
        &self,
        aggregate_id: Uuid,
        aggregate_type: &str,
        expected_version: u64,
        events: Vec<NewEvent>,
    ) -> Result<u64> {
        if events.is_empty() {
            return Ok(expected_version);
        }

        let mut tx = self.pool.begin().await?;

        // Lock the aggregate row and check current version
        let current_version: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(sequence), 0) \
             FROM seesaw_event_store \
             WHERE aggregate_id = $1 \
             FOR UPDATE",
        )
        .bind(aggregate_id)
        .fetch_one(&mut *tx)
        .await?;

        if current_version as u64 != expected_version {
            return Err(ConcurrencyError {
                aggregate_id,
                expected: expected_version,
                actual: current_version as u64,
            }
            .into());
        }

        let mut seq = expected_version;
        for event in &events {
            seq += 1;
            sqlx::query(
                "INSERT INTO seesaw_event_store \
                    (aggregate_id, aggregate_type, sequence, event_type, \
                     event_data, metadata, schema_version, caused_by) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
            )
            .bind(aggregate_id)
            .bind(aggregate_type)
            .bind(seq as i64)
            .bind(&event.event_type)
            .bind(&event.data)
            .bind(event.metadata.as_ref().unwrap_or(&serde_json::json!({})))
            .bind(event.schema_version as i32)
            .bind(event.caused_by)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(seq)
    }

    async fn exists(&self, aggregate_id: Uuid) -> Result<bool> {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM seesaw_event_store WHERE aggregate_id = $1)",
        )
        .bind(aggregate_id)
        .fetch_one(&self.pool)
        .await?;

        Ok(exists)
    }
}
