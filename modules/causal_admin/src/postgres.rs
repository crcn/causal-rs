//! Postgres-backed [`AdminReadModel`] implementation.
//!
//! Wraps a `sqlx::PgPool` and delegates to the raw SQL queries in [`crate::queries`].

use anyhow::Result;
use async_trait::async_trait;
use sqlx::PgPool;
use uuid::Uuid;

use crate::queries;
use crate::read_model::{
    AdminReadModel, EventQuery, ReactorDescriptionEntry, ReactorLogEntry, ReactorOutcomeEntry,
    StoredEvent,
};

/// Postgres-backed admin read model.
///
/// # Example
///
/// ```ignore
/// let store = PostgresAdminStore::new(pool.clone());
/// schema_builder.data(Arc::new(store) as Arc<dyn AdminReadModel>);
/// ```
pub struct PostgresAdminStore {
    pool: PgPool,
}

impl PostgresAdminStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl AdminReadModel for PostgresAdminStore {
    async fn list_events(&self, query: &EventQuery) -> Result<Vec<StoredEvent>> {
        queries::list_events_paginated(
            &self.pool,
            query.search.as_deref(),
            query.cursor,
            query.from,
            query.to,
            query.run_id.as_deref(),
            query.limit as i64,
        )
        .await
    }

    async fn get_event(&self, seq: i64) -> Result<Option<StoredEvent>> {
        queries::get_event_by_seq(&self.pool, seq).await
    }

    async fn causal_tree(&self, seq: i64) -> Result<(Vec<StoredEvent>, i64)> {
        queries::causal_tree(&self.pool, seq).await
    }

    async fn causal_flow(&self, run_id: &str) -> Result<Vec<StoredEvent>> {
        queries::causal_flow(&self.pool, run_id).await
    }

    async fn events_from_seq(&self, start_seq: i64, limit: usize) -> Result<Vec<StoredEvent>> {
        queries::get_events_from_seq(&self.pool, start_seq, limit as i64).await
    }

    async fn reactor_logs(
        &self,
        event_id: Uuid,
        reactor_id: &str,
    ) -> Result<Vec<ReactorLogEntry>> {
        let rows = queries::reactor_logs(&self.pool, &event_id, reactor_id).await?;
        Ok(rows
            .into_iter()
            .map(|r| ReactorLogEntry {
                event_id: r.event_id,
                reactor_id: r.reactor_id,
                level: r.level,
                message: r.message,
                data: r.data,
                logged_at: r.logged_at,
            })
            .collect())
    }

    async fn reactor_logs_by_run(&self, run_id: &str) -> Result<Vec<ReactorLogEntry>> {
        let rows = queries::reactor_logs_by_run(&self.pool, run_id).await?;
        Ok(rows
            .into_iter()
            .map(|r| ReactorLogEntry {
                event_id: r.event_id,
                reactor_id: r.reactor_id,
                level: r.level,
                message: r.message,
                data: r.data,
                logged_at: r.logged_at,
            })
            .collect())
    }

    async fn reactor_outcomes(&self, run_id: &str) -> Result<Vec<ReactorOutcomeEntry>> {
        let rows = queries::reactor_outcomes(&self.pool, run_id).await?;
        Ok(rows
            .into_iter()
            .map(|r| ReactorOutcomeEntry {
                reactor_id: r.reactor_id,
                status: r.status,
                error: r.error,
                attempts: r.attempts,
                started_at: r.started_at,
                completed_at: r.completed_at,
                triggering_event_ids: r.triggering_event_ids,
            })
            .collect())
    }

    async fn reactor_descriptions(&self, run_id: &str) -> Result<Vec<ReactorDescriptionEntry>> {
        let rows = queries::reactor_descriptions(&self.pool, run_id).await?;
        Ok(rows
            .into_iter()
            .map(|r| ReactorDescriptionEntry {
                reactor_id: r.reactor_id,
                description: r.description,
            })
            .collect())
    }
}
