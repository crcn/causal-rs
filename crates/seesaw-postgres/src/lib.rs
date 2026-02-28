pub mod event_store;

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use seesaw_core::insight::*;
use sqlx::{FromRow, PgPool};
use std::collections::HashSet;
use uuid::Uuid;

/// PostgreSQL store for Seesaw.
///
/// Provides InsightStore implementation for observability and the ES
/// event store (via the `event_store` module).
pub struct PostgresStore {
    pool: PgPool,
}

impl PostgresStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Check if an event has already been processed (for idempotency).
    pub async fn is_processed(&self, event_id: Uuid) -> Result<bool> {
        let result: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM seesaw_processed WHERE event_id = $1)",
        )
        .bind(event_id)
        .fetch_one(&self.pool)
        .await?;

        Ok(result)
    }
}

impl Clone for PostgresStore {
    fn clone(&self) -> Self {
        Self {
            pool: self.pool.clone(),
        }
    }
}

// ============================================================================
// Row types for InsightStore queries
// ============================================================================

#[derive(FromRow)]
struct EventRow {
    #[allow(dead_code)]
    id: i64,
    event_id: Uuid,
    parent_id: Option<Uuid>,
    #[allow(dead_code)]
    correlation_id: Uuid,
    event_type: String,
    payload: serde_json::Value,
    #[allow(dead_code)]
    hops: i32,
    batch_id: Option<Uuid>,
    batch_index: Option<i32>,
    batch_size: Option<i32>,
    created_at: DateTime<Utc>,
}

#[derive(FromRow)]
struct EffectTreeRow {
    event_id: Uuid,
    handler_id: String,
    status: String,
    result: Option<serde_json::Value>,
    error: Option<String>,
    attempts: i32,
    created_at: DateTime<Utc>,
    batch_id: Option<Uuid>,
    batch_index: Option<i32>,
    batch_size: Option<i32>,
}

#[derive(FromRow)]
struct StreamRow {
    seq: i64,
    stream_type: String,
    correlation_id: Uuid,
    event_id: Option<Uuid>,
    effect_event_id: Option<Uuid>,
    handler_id: Option<String>,
    status: Option<String>,
    error: Option<String>,
    payload: Option<serde_json::Value>,
    created_at: DateTime<Utc>,
}

#[derive(FromRow)]
struct EffectLogRow {
    correlation_id: Uuid,
    event_id: Uuid,
    handler_id: String,
    status: String,
    attempts: i32,
    event_type: String,
    result: Option<serde_json::Value>,
    error: Option<String>,
    created_at: DateTime<Utc>,
    execute_at: DateTime<Utc>,
    claimed_at: Option<DateTime<Utc>>,
    last_attempted_at: Option<DateTime<Utc>>,
    completed_at: Option<DateTime<Utc>>,
}

#[derive(FromRow)]
struct DeadLetterRow {
    correlation_id: Uuid,
    event_id: Uuid,
    handler_id: String,
    event_type: String,
    event_payload: serde_json::Value,
    error: String,
    reason: String,
    attempts: i32,
    failed_at: DateTime<Utc>,
    resolved_at: Option<DateTime<Utc>>,
}

#[derive(FromRow)]
struct FailedWorkflowRow {
    correlation_id: Uuid,
    failed_effects: i64,
    active_effects: i64,
    dead_letters: i64,
    last_failed_at: Option<DateTime<Utc>>,
    last_error: Option<String>,
}

// ============================================================================
// InsightStore implementation
// ============================================================================

#[async_trait]
impl InsightStore for PostgresStore {
    async fn subscribe_events(
        &self,
    ) -> Result<Box<dyn futures::Stream<Item = InsightEvent> + Send + Unpin>> {
        use futures::stream::StreamExt;
        use sqlx::postgres::PgListener;

        // Create a new listener connection
        let mut listener = PgListener::connect_with(&self.pool).await?;
        listener.listen("seesaw_stream").await?;

        // Convert listener into a stream of InsightEvent
        let pool = self.pool.clone();
        let stream = listener.into_stream().filter_map(move |result| {
            let pool = pool.clone();
            Box::pin(async move {
                match result {
                    Ok(_notification) => {
                        // Fetch latest entry from stream table
                        // (notification payload is just correlation_id for wake-up)
                        if let Ok(row) = sqlx::query_as::<_, StreamRow>(
                            "SELECT seq, stream_type, correlation_id, event_id, effect_event_id,
                                    handler_id, status, error, payload, created_at
                             FROM seesaw_stream
                             ORDER BY seq DESC
                             LIMIT 1",
                        )
                        .fetch_one(&pool)
                        .await
                        {
                            Some(stream_row_to_insight_event(row))
                        } else {
                            None
                        }
                    }
                    Err(_) => None,
                }
            })
        });

        Ok(Box::new(stream))
    }

    async fn get_workflow_tree(&self, correlation_id: Uuid) -> Result<seesaw_core::WorkflowTree> {
        // Get all events for this correlation
        let events = sqlx::query_as::<_, EventRow>(
            "SELECT id, event_id, parent_id, correlation_id, event_type, payload, hops,
                    batch_id, batch_index, batch_size, created_at
             FROM seesaw_events
             WHERE correlation_id = $1
             ORDER BY created_at ASC",
        )
        .bind(correlation_id)
        .fetch_all(&self.pool)
        .await?;

        // Get all effects for this correlation
        let effects = sqlx::query_as::<_, EffectTreeRow>(
            "SELECT event_id, handler_id, status, result, error, attempts, created_at,
                    batch_id, batch_index, batch_size
             FROM seesaw_handler_executions
             WHERE correlation_id = $1
             ORDER BY created_at ASC",
        )
        .bind(correlation_id)
        .fetch_all(&self.pool)
        .await?;

        // Build tree structure
        let event_ids: HashSet<Uuid> = events.iter().map(|event| event.event_id).collect();
        let roots = build_event_tree(&events, &effects, None, &event_ids, true);

        Ok(seesaw_core::WorkflowTree {
            correlation_id,
            roots,
            event_count: events.len(),
            effect_count: effects.len(),
        })
    }

    async fn get_stats(&self) -> Result<seesaw_core::InsightStats> {
        let total_events = sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM seesaw_events")
            .fetch_one(&self.pool)
            .await?
            .0;

        let active_effects = sqlx::query_as::<_, (i64,)>(
            "SELECT COUNT(*) FROM seesaw_handler_executions
             WHERE status IN ('pending', 'executing')",
        )
        .fetch_one(&self.pool)
        .await?
        .0;

        let completed_effects = sqlx::query_as::<_, (i64,)>(
            "SELECT COUNT(*) FROM seesaw_handler_executions WHERE status = 'completed'",
        )
        .fetch_one(&self.pool)
        .await?
        .0;

        let failed_effects = sqlx::query_as::<_, (i64,)>(
            "SELECT COUNT(*) FROM seesaw_handler_executions WHERE status = 'failed'",
        )
        .fetch_one(&self.pool)
        .await?
        .0;

        Ok(seesaw_core::InsightStats {
            total_events,
            active_effects,
            completed_effects,
            failed_effects,
        })
    }

    async fn get_recent_events(
        &self,
        cursor: Option<i64>,
        limit: usize,
    ) -> Result<Vec<InsightEvent>> {
        let rows = if let Some(cursor_seq) = cursor {
            sqlx::query_as::<_, StreamRow>(
                "SELECT seq, stream_type, correlation_id, event_id, effect_event_id,
                        handler_id, status, error, payload, created_at
                 FROM seesaw_stream
                 WHERE seq > $1
                 ORDER BY seq ASC
                 LIMIT $2",
            )
            .bind(cursor_seq)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_as::<_, StreamRow>(
                "SELECT seq, stream_type, correlation_id, event_id, effect_event_id,
                        handler_id, status, error, payload, created_at
                 FROM seesaw_stream
                 ORDER BY seq DESC
                 LIMIT $1",
            )
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await?
        };

        Ok(rows.into_iter().map(stream_row_to_insight_event).collect())
    }

    async fn get_effect_logs(
        &self,
        correlation_id: Option<Uuid>,
        limit: usize,
    ) -> Result<Vec<EffectExecutionLog>> {
        let rows = sqlx::query_as::<_, EffectLogRow>(
            "SELECT
                correlation_id,
                event_id,
                handler_id,
                status,
                attempts,
                event_type,
                result,
                error,
                created_at,
                execute_at,
                claimed_at,
                last_attempted_at,
                completed_at
             FROM seesaw_handler_executions
             WHERE ($1::uuid IS NULL OR correlation_id = $1)
             ORDER BY COALESCE(last_attempted_at, created_at) DESC, event_id DESC
             LIMIT $2",
        )
        .bind(correlation_id)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|row| {
                let started_at = row.claimed_at.or(row.last_attempted_at);
                let duration_ms = match (started_at, row.completed_at) {
                    (Some(start), Some(end)) => Some((end - start).num_milliseconds().max(0)),
                    _ => None,
                };

                EffectExecutionLog {
                    correlation_id: row.correlation_id,
                    event_id: row.event_id,
                    handler_id: row.handler_id,
                    status: row.status,
                    attempts: row.attempts,
                    event_type: Some(row.event_type),
                    result: row.result,
                    error: row.error,
                    created_at: row.created_at,
                    execute_at: Some(row.execute_at),
                    claimed_at: row.claimed_at,
                    last_attempted_at: row.last_attempted_at,
                    completed_at: row.completed_at,
                    duration_ms,
                }
            })
            .collect())
    }

    async fn get_dead_letters(
        &self,
        unresolved_only: bool,
        limit: usize,
    ) -> Result<Vec<DeadLetterEntry>> {
        let rows = sqlx::query_as::<_, DeadLetterRow>(
            "SELECT
                correlation_id,
                event_id,
                handler_id,
                event_type,
                event_payload,
                error,
                reason,
                attempts,
                failed_at,
                resolved_at
             FROM seesaw_dlq
             WHERE (NOT $1 OR resolved_at IS NULL)
             ORDER BY failed_at DESC
             LIMIT $2",
        )
        .bind(unresolved_only)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|row| DeadLetterEntry {
                correlation_id: row.correlation_id,
                event_id: row.event_id,
                handler_id: row.handler_id,
                event_type: row.event_type,
                event_payload: row.event_payload,
                error: row.error,
                reason: row.reason,
                attempts: row.attempts,
                failed_at: row.failed_at,
                resolved_at: row.resolved_at,
            })
            .collect())
    }

    async fn get_failed_workflows(&self, limit: usize) -> Result<Vec<FailedWorkflow>> {
        let rows = sqlx::query_as::<_, FailedWorkflowRow>(
            "WITH effect_agg AS (
                SELECT
                    correlation_id,
                    COUNT(*) FILTER (WHERE status = 'failed')::BIGINT AS failed_effects,
                    COUNT(*) FILTER (WHERE status IN ('pending', 'executing'))::BIGINT AS active_effects,
                    MAX(last_attempted_at) FILTER (WHERE status = 'failed') AS last_failed_at,
                    MAX(error) FILTER (WHERE status = 'failed') AS last_error
                FROM seesaw_handler_executions
                GROUP BY correlation_id
             ),
             dlq_agg AS (
                SELECT
                    correlation_id,
                    COUNT(*) FILTER (WHERE resolved_at IS NULL)::BIGINT AS dead_letters,
                    MAX(failed_at) FILTER (WHERE resolved_at IS NULL) AS last_dlq_at,
                    MAX(error) FILTER (WHERE resolved_at IS NULL) AS last_dlq_error
                FROM seesaw_dlq
                GROUP BY correlation_id
             )
             SELECT
                COALESCE(e.correlation_id, d.correlation_id) AS correlation_id,
                COALESCE(e.failed_effects, 0) AS failed_effects,
                COALESCE(e.active_effects, 0) AS active_effects,
                COALESCE(d.dead_letters, 0) AS dead_letters,
                GREATEST(e.last_failed_at, d.last_dlq_at) AS last_failed_at,
                COALESCE(d.last_dlq_error, e.last_error) AS last_error
             FROM effect_agg e
             FULL OUTER JOIN dlq_agg d ON d.correlation_id = e.correlation_id
             WHERE COALESCE(e.failed_effects, 0) > 0 OR COALESCE(d.dead_letters, 0) > 0
             ORDER BY last_failed_at DESC NULLS LAST
             LIMIT $1",
        )
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|row| FailedWorkflow {
                correlation_id: row.correlation_id,
                failed_effects: row.failed_effects,
                active_effects: row.active_effects,
                dead_letters: row.dead_letters,
                last_failed_at: row.last_failed_at,
                last_error: row.last_error,
            })
            .collect())
    }
}

// ============================================================================
// Helper functions
// ============================================================================

fn stream_row_to_insight_event(row: StreamRow) -> InsightEvent {
    let stream_type = match row.stream_type.as_str() {
        "event_dispatched" => StreamType::EventDispatched,
        "effect_started" => StreamType::EffectStarted,
        "effect_completed" => StreamType::EffectCompleted,
        "effect_failed" => StreamType::EffectFailed,
        _ => StreamType::EventDispatched, // Default fallback
    };

    // Extract event_type from payload if it's an event
    let event_type = if stream_type == StreamType::EventDispatched {
        row.payload
            .as_ref()
            .and_then(|p| p.get("event_type"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    } else {
        None
    };

    InsightEvent {
        seq: row.seq,
        stream_type,
        correlation_id: row.correlation_id,
        event_id: row.event_id,
        effect_event_id: row.effect_event_id,
        handler_id: row.handler_id,
        event_type,
        status: row.status,
        error: row.error,
        payload: row.payload,
        created_at: row.created_at,
    }
}

fn build_event_tree(
    events: &[EventRow],
    effects: &[EffectTreeRow],
    parent_id: Option<Uuid>,
    event_ids: &HashSet<Uuid>,
    is_root_pass: bool,
) -> Vec<EventNode> {
    events
        .iter()
        .filter(|event| {
            if is_root_pass {
                event.parent_id.is_none()
                    || event
                        .parent_id
                        .map(|parent| !event_ids.contains(&parent))
                        .unwrap_or(false)
            } else {
                event.parent_id == parent_id
            }
        })
        .map(|event| {
            // Get effects for this event
            let event_effects: Vec<HandlerNode> = effects
                .iter()
                .filter(|eff| eff.event_id == event.event_id)
                .map(|eff| HandlerNode {
                    handler_id: eff.handler_id.clone(),
                    event_id: eff.event_id,
                    status: eff.status.clone(),
                    result: eff.result.clone(),
                    error: eff.error.clone(),
                    attempts: eff.attempts,
                    created_at: eff.created_at,
                    batch_id: eff.batch_id,
                    batch_index: eff.batch_index,
                    batch_size: eff.batch_size,
                })
                .collect();

            // Recursively build children
            let children =
                build_event_tree(events, effects, Some(event.event_id), event_ids, false);

            EventNode {
                event_id: event.event_id,
                event_type: event.event_type.clone(),
                payload: event.payload.clone(),
                created_at: event.created_at,
                batch_id: event.batch_id,
                batch_index: event.batch_index,
                batch_size: event.batch_size,
                children,
                effects: event_effects,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn build_event_tree_treats_orphan_parent_as_root() {
        let correlation_id = Uuid::new_v4();
        let event_id = Uuid::new_v4();
        let missing_parent = Uuid::new_v4();
        let now = Utc::now();

        let events = vec![EventRow {
            id: 1,
            event_id,
            parent_id: Some(missing_parent),
            correlation_id,
            event_type: "OrphanEvent".to_string(),
            payload: serde_json::json!({"ok": true}),
            hops: 1,
            batch_id: None,
            batch_index: None,
            batch_size: None,
            created_at: now,
        }];

        let effects: Vec<EffectTreeRow> = Vec::new();
        let event_ids: HashSet<Uuid> = events.iter().map(|event| event.event_id).collect();

        let roots = build_event_tree(&events, &effects, None, &event_ids, true);

        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].event_id, event_id);
    }
}
