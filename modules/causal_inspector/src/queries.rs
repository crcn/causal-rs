//! Raw SQL queries for Postgres-backed inspector stores.
//!
//! These functions return [`StoredEvent`] and raw row types — display
//! transformation happens in the resolver layer via [`EventDisplay`].

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::read_model::StoredEvent;

// ── Row conversion ──

fn row_to_stored(r: &sqlx::postgres::PgRow) -> StoredEvent {
    use sqlx::Row;
    StoredEvent {
        seq: r.get("seq"),
        ts: r.get("ts"),
        event_type: r.get("event_type"),
        payload: r.get::<serde_json::Value, _>("data"),
        id: r.get("id"),
        parent_id: r.get("parent_id"),
        correlation_id: r.get("correlation_id"),
        reactor_id: r.get("reactor_id"),
    }
}

fn rows_to_stored(rows: impl IntoIterator<Item = sqlx::postgres::PgRow>) -> Vec<StoredEvent> {
    rows.into_iter().map(|r| row_to_stored(&r)).collect()
}

// ── Queries ──

/// Single event lookup by sequence number.
pub async fn get_event_by_seq(pool: &PgPool, seq: i64) -> anyhow::Result<Option<StoredEvent>> {
    let row = sqlx::query(
        r#"
        SELECT seq, ts, event_type, payload AS data, id, parent_id,
               correlation_id, reactor_id
        FROM events
        WHERE seq = $1
        "#,
    )
    .bind(seq)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|r| row_to_stored(&r)))
}

/// Fetch events starting from a given seq (for subscription catch-up).
pub async fn get_events_from_seq(
    pool: &PgPool,
    start_seq: i64,
    limit: i64,
) -> anyhow::Result<Vec<StoredEvent>> {
    let limit = limit.min(500);

    let rows = sqlx::query(
        r#"
        SELECT seq, ts, event_type, payload AS data, id, parent_id,
               correlation_id, reactor_id
        FROM events
        WHERE seq >= $1
        ORDER BY seq ASC
        LIMIT $2
        "#,
    )
    .bind(start_seq)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    Ok(rows_to_stored(rows))
}

/// Paginated reverse-chronological event listing with optional filters.
pub async fn list_events_paginated(
    pool: &PgPool,
    search: Option<&str>,
    cursor: Option<i64>,
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    correlation_id: Option<&str>,
    limit: i64,
) -> anyhow::Result<Vec<StoredEvent>> {
    let limit = limit.min(200);

    let correlation_uuid = correlation_id
        .and_then(|s| Uuid::parse_str(s).ok());

    let rows = sqlx::query(
        r#"
        SELECT seq, ts, event_type, payload AS data, id, parent_id,
               correlation_id, reactor_id
        FROM events
        WHERE ($1::bigint IS NULL OR seq < $1)
          AND ($2::timestamptz IS NULL OR ts >= $2)
          AND ($3::timestamptz IS NULL OR ts <= $3)
          AND ($4::text IS NULL
               OR payload::text ILIKE '%' || $4 || '%'
               OR event_type ILIKE '%' || $4 || '%'
               OR correlation_id::text ILIKE '%' || $4 || '%')
          AND ($6::uuid IS NULL OR correlation_id = $6)
        ORDER BY seq DESC
        LIMIT $5
        "#,
    )
    .bind(cursor)
    .bind(from)
    .bind(to)
    .bind(search)
    .bind(limit)
    .bind(correlation_uuid)
    .fetch_all(pool)
    .await?;

    Ok(rows_to_stored(rows))
}

/// Get all events sharing the same correlation_id as the given event.
pub async fn causal_tree(pool: &PgPool, seq: i64) -> anyhow::Result<(Vec<StoredEvent>, i64)> {
    let rows = sqlx::query(
        r#"
        SELECT e.seq, e.ts, e.event_type, e.payload AS data, e.id, e.parent_id,
               e.correlation_id, e.reactor_id
        FROM events e
        WHERE e.correlation_id = (SELECT correlation_id FROM events WHERE seq = $1)
          AND e.correlation_id IS NOT NULL
        ORDER BY e.seq
        "#,
    )
    .bind(seq)
    .fetch_all(pool)
    .await?;

    use sqlx::Row;
    let root_seq = rows
        .iter()
        .find(|r| r.get::<Option<Uuid>, _>("parent_id").is_none())
        .map(|r| r.get::<i64, _>("seq"))
        .unwrap_or(seq);

    Ok((rows_to_stored(rows), root_seq))
}

/// Get all events for a correlation_id, ordered by seq ascending.
pub async fn causal_flow(pool: &PgPool, correlation_id: &str) -> anyhow::Result<Vec<StoredEvent>> {
    let cid = Uuid::parse_str(correlation_id)
        .map_err(|e| anyhow::anyhow!("Invalid correlation_id as UUID: {e}"))?;

    let rows = sqlx::query(
        r#"
        SELECT seq, ts, event_type, payload AS data, id, parent_id,
               correlation_id, reactor_id
        FROM events
        WHERE correlation_id = $1
        ORDER BY seq ASC
        "#,
    )
    .bind(cid)
    .fetch_all(pool)
    .await?;

    Ok(rows_to_stored(rows))
}

// ── Reactor queries ──

/// Reactor log row from `causal_handler_logs`.
pub struct ReactorLogRow {
    pub event_id: Uuid,
    pub reactor_id: String,
    pub level: String,
    pub message: String,
    pub data: Option<serde_json::Value>,
    pub logged_at: DateTime<Utc>,
}

/// Fetch reactor logs for a specific event + reactor.
pub async fn reactor_logs(
    pool: &PgPool,
    event_id: &Uuid,
    reactor_id: &str,
) -> anyhow::Result<Vec<ReactorLogRow>> {
    let rows = sqlx::query_as::<_, (Uuid, String, String, String, Option<serde_json::Value>, DateTime<Utc>)>(
        "SELECT event_id, reactor_id, level, message, data, logged_at \
         FROM causal_handler_logs \
         WHERE event_id = $1 AND reactor_id = $2 \
         ORDER BY id",
    )
    .bind(event_id)
    .bind(reactor_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(event_id, reactor_id, level, message, data, logged_at)| ReactorLogRow {
            event_id, reactor_id, level, message, data, logged_at,
        })
        .collect())
}

/// Fetch all reactor logs for a correlation chain.
pub async fn reactor_logs_by_correlation(
    pool: &PgPool,
    correlation_id: &str,
) -> anyhow::Result<Vec<ReactorLogRow>> {
    let cid = Uuid::parse_str(correlation_id)
        .map_err(|e| anyhow::anyhow!("Invalid correlation_id as UUID: {e}"))?;

    let rows = sqlx::query_as::<_, (Uuid, String, String, String, Option<serde_json::Value>, DateTime<Utc>)>(
        "SELECT event_id, reactor_id, level, message, data, logged_at \
         FROM causal_handler_logs \
         WHERE correlation_id = $1 \
         ORDER BY id",
    )
    .bind(cid)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(event_id, reactor_id, level, message, data, logged_at)| ReactorLogRow {
            event_id, reactor_id, level, message, data, logged_at,
        })
        .collect())
}

/// Aggregated reactor outcome row from `causal_effect_executions`.
pub struct ReactorOutcomeRow {
    pub reactor_id: String,
    pub status: String,
    pub error: Option<String>,
    pub attempts: i64,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub triggering_event_ids: Vec<String>,
}

/// Fetch aggregated reactor execution outcomes for a correlation chain.
pub async fn reactor_outcomes(
    pool: &PgPool,
    correlation_id: &str,
) -> anyhow::Result<Vec<ReactorOutcomeRow>> {
    let cid = Uuid::parse_str(correlation_id)
        .map_err(|e| anyhow::anyhow!("Invalid correlation_id as UUID: {e}"))?;

    let rows = sqlx::query_as::<_, (String, String, Option<String>, i64, Option<DateTime<Utc>>, Option<DateTime<Utc>>, Option<Vec<String>>)>(
        "SELECT reactor_id, \
                CASE WHEN bool_or(status = 'error') THEN 'error' \
                     WHEN bool_or(status = 'running') THEN 'running' \
                     WHEN bool_or(status = 'pending') AND bool_or(status = 'completed') THEN 'running' \
                     WHEN bool_or(status = 'pending') THEN 'pending' \
                     ELSE 'completed' END AS status, \
                string_agg(DISTINCT error, '; ') FILTER (WHERE error IS NOT NULL) AS error, \
                COALESCE(SUM(attempts), 0) AS attempts, \
                MIN(created_at) AS started_at, \
                MAX(updated_at) FILTER (WHERE status = 'completed') AS completed_at, \
                array_agg(DISTINCT event_id::text) AS triggering_event_ids \
         FROM causal_effect_executions \
         WHERE correlation_id = $1 \
         GROUP BY reactor_id",
    )
    .bind(cid)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(reactor_id, status, error, attempts, started_at, completed_at, triggering_event_ids)| ReactorOutcomeRow {
            reactor_id, status, error, attempts, started_at, completed_at,
            triggering_event_ids: triggering_event_ids.unwrap_or_default(),
        })
        .collect())
}

/// Reactor description row from `causal_reactor_descriptions`.
pub struct ReactorDescriptionRow {
    pub reactor_id: String,
    pub description: serde_json::Value,
}

/// Fetch reactor describe() blocks for a correlation chain.
pub async fn reactor_descriptions(
    pool: &PgPool,
    correlation_id: &str,
) -> anyhow::Result<Vec<ReactorDescriptionRow>> {
    let cid = Uuid::parse_str(correlation_id)
        .map_err(|e| anyhow::anyhow!("Invalid correlation_id as UUID: {e}"))?;

    let rows = sqlx::query_as::<_, (String, serde_json::Value)>(
        "SELECT reactor_id, description \
         FROM causal_reactor_descriptions \
         WHERE correlation_id = $1",
    )
    .bind(cid)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(reactor_id, description)| ReactorDescriptionRow {
            reactor_id,
            description,
        })
        .collect())
}
