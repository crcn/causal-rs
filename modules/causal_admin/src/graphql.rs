use std::sync::Arc;

use async_graphql::{Context, Object, Result, Subscription};
use chrono::{DateTime, Utc};
use futures::Stream;
use uuid::Uuid;

use crate::display::EventDisplay;
use crate::types::*;

/// Generic admin query resolvers for causal event tables.
///
/// The `D` parameter controls how events are displayed (names, layers, summaries).
/// Consumers should compose this into their own schema and add their own auth guards.
///
/// # Usage
///
/// ```ignore
/// use causal_admin::{CausalAdminQuery, DefaultEventDisplay};
///
/// let admin = CausalAdminQuery::new(DefaultEventDisplay);
/// // Merge into your async-graphql schema
/// ```
pub struct CausalAdminQuery<D: EventDisplay> {
    display: Arc<D>,
}

impl<D: EventDisplay> CausalAdminQuery<D> {
    pub fn new(display: D) -> Self {
        Self {
            display: Arc::new(display),
        }
    }
}

#[Object]
impl<D: EventDisplay + 'static> CausalAdminQuery<D> {
    /// Paginated reverse-chronological event listing with optional filters.
    /// Tries in-memory cache first, falls through to Postgres on miss.
    async fn admin_events(
        &self,
        ctx: &Context<'_>,
        limit: Option<i32>,
        cursor: Option<i64>,
        search: Option<String>,
        from: Option<DateTime<Utc>>,
        to: Option<DateTime<Utc>>,
        run_id: Option<String>,
    ) -> Result<AdminEventsPage> {
        let lim = (limit.unwrap_or(50) as usize).min(200);

        #[cfg(feature = "cache")]
        if let Some(cache) = ctx.data_opt::<crate::cache::SharedEventCache>() {
            let cache = cache.read().await;
            let (events, next_cursor) = cache.search(
                search.as_deref(),
                cursor,
                from,
                to,
                run_id.as_deref(),
                lim,
            );
            let events: Vec<AdminEvent> = events.into_iter().map(|e| (*e).clone()).collect();
            return Ok(AdminEventsPage { events, next_cursor });
        }

        let pool = ctx.data::<sqlx::PgPool>()?;

        let events = crate::queries::list_events_paginated(
            pool,
            search.as_deref(),
            cursor,
            from,
            to,
            run_id.as_deref(),
            lim as i64,
            self.display.as_ref(),
        )
        .await
        .map_err(|e| async_graphql::Error::new(format!("Failed to load events: {e}")))?;

        let next_cursor = if events.len() == lim {
            events.last().map(|e| e.seq)
        } else {
            None
        };

        Ok(AdminEventsPage { events, next_cursor })
    }

    /// Walk the causal tree for an event (ancestors + descendants).
    async fn admin_causal_tree(&self, ctx: &Context<'_>, seq: i64) -> Result<AdminCausalTree> {
        #[cfg(feature = "cache")]
        if let Some(cache) = ctx.data_opt::<crate::cache::SharedEventCache>() {
            let cache = cache.read().await;
            if let Some((events, root_seq)) = cache.causal_tree(seq) {
                let events: Vec<AdminEvent> = events.into_iter().map(|e| (*e).clone()).collect();
                return Ok(AdminCausalTree { events, root_seq });
            }
        }

        let pool = ctx.data::<sqlx::PgPool>()?;

        let (events, root_seq) = crate::queries::causal_tree(pool, seq, self.display.as_ref())
            .await
            .map_err(|e| async_graphql::Error::new(format!("Failed to load causal tree: {e}")))?;

        Ok(AdminCausalTree { events, root_seq })
    }

    /// Fetch all events for a run (causal flow DAG viewer).
    async fn admin_causal_flow(
        &self,
        ctx: &Context<'_>,
        run_id: String,
    ) -> Result<AdminCausalFlow> {
        #[cfg(feature = "cache")]
        if let Some(cache) = ctx.data_opt::<crate::cache::SharedEventCache>() {
            let cache = cache.read().await;
            if let Some(events) = cache.causal_flow(&run_id) {
                let events: Vec<AdminEvent> = events.into_iter().map(|e| (*e).clone()).collect();
                return Ok(AdminCausalFlow { events });
            }
        }

        let pool = ctx.data::<sqlx::PgPool>()?;

        let events = crate::queries::causal_flow(pool, &run_id, self.display.as_ref())
            .await
            .map_err(|e| async_graphql::Error::new(format!("Failed to load causal flow: {e}")))?;

        Ok(AdminCausalFlow { events })
    }

    /// Fetch handler logs for a specific event + handler.
    async fn admin_handler_logs(
        &self,
        ctx: &Context<'_>,
        event_id: String,
        handler_id: String,
    ) -> Result<Vec<HandlerLog>> {
        let pool = ctx.data::<sqlx::PgPool>()?;

        let event_uuid = Uuid::parse_str(&event_id)
            .map_err(|e| async_graphql::Error::new(format!("Invalid event_id: {e}")))?;

        let rows = crate::queries::handler_logs(pool, &event_uuid, &handler_id)
            .await
            .map_err(|e| async_graphql::Error::new(format!("Failed to load handler logs: {e}")))?;

        Ok(rows
            .into_iter()
            .map(|r| HandlerLog {
                event_id: event_id.clone(),
                handler_id: handler_id.clone(),
                level: r.level,
                message: r.message,
                data: r.data,
                logged_at: r.logged_at,
            })
            .collect())
    }

    /// Fetch all handler logs for a run.
    async fn admin_handler_logs_by_run(
        &self,
        ctx: &Context<'_>,
        run_id: String,
    ) -> Result<Vec<HandlerLog>> {
        let pool = ctx.data::<sqlx::PgPool>()?;

        let rows = crate::queries::handler_logs_by_run(pool, &run_id)
            .await
            .map_err(|e| async_graphql::Error::new(format!("Failed to load handler logs: {e}")))?;

        Ok(rows
            .into_iter()
            .map(|r| HandlerLog {
                event_id: r.event_id.to_string(),
                handler_id: r.handler_id,
                level: r.level,
                message: r.message,
                data: r.data,
                logged_at: r.logged_at,
            })
            .collect())
    }

    /// Fetch handler describe() blocks for a run.
    async fn admin_handler_descriptions(
        &self,
        ctx: &Context<'_>,
        run_id: String,
    ) -> Result<Vec<HandlerDescription>> {
        let pool = ctx.data::<sqlx::PgPool>()?;

        let rows = crate::queries::handler_descriptions(pool, &run_id)
            .await
            .map_err(|e| async_graphql::Error::new(format!("Failed to load handler descriptions: {e}")))?;

        Ok(rows
            .into_iter()
            .map(|r| HandlerDescription {
                handler_id: r.handler_id,
                blocks: r.description,
            })
            .collect())
    }

    /// Fetch aggregated handler execution outcomes for a run.
    async fn admin_handler_outcomes(
        &self,
        ctx: &Context<'_>,
        run_id: String,
    ) -> Result<Vec<HandlerOutcome>> {
        let pool = ctx.data::<sqlx::PgPool>()?;

        let rows = crate::queries::handler_outcomes(pool, &run_id)
            .await
            .map_err(|e| async_graphql::Error::new(format!("Failed to load handler outcomes: {e}")))?;

        Ok(rows
            .into_iter()
            .map(|r| HandlerOutcome {
                handler_id: r.handler_id,
                status: r.status,
                error: r.error,
                attempts: r.attempts,
                started_at: r.started_at,
                completed_at: r.completed_at,
                triggering_event_ids: r.triggering_event_ids,
            })
            .collect())
    }
}

/// Generic admin subscription — streams live events via EventBroadcast.
pub struct CausalAdminSubscription<D: EventDisplay> {
    display: Arc<D>,
}

impl<D: EventDisplay> CausalAdminSubscription<D> {
    pub fn new(display: D) -> Self {
        Self {
            display: Arc::new(display),
        }
    }
}

#[Subscription]
#[cfg(feature = "broadcast")]
impl<D: EventDisplay + 'static> CausalAdminSubscription<D> {
    /// Stream live events. If `last_seq` is provided, replays missed events first.
    async fn admin_event_added(
        &self,
        ctx: &Context<'_>,
        last_seq: Option<i64>,
    ) -> Result<impl Stream<Item = AdminEvent>> {
        let pool = ctx.data::<sqlx::PgPool>()?.clone();
        let broadcast = ctx.data::<crate::broadcast::EventBroadcast>()?.clone();
        let mut rx = broadcast.subscribe();
        let display = Arc::clone(&self.display);

        Ok(async_stream::stream! {
            let mut high_water: i64 = 0;

            // Catch-up phase
            if let Some(start) = last_seq {
                let catch_up_start = start + 1;
                match crate::queries::get_events_from_seq(&pool, catch_up_start, 500, display.as_ref()).await {
                    Ok(events) => {
                        for event in events {
                            if event.seq > high_water {
                                high_water = event.seq;
                            }
                            yield event;
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "Subscription catch-up query failed");
                    }
                }
            }

            // Live phase
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        if event.seq > high_water {
                            high_water = event.seq;
                            yield event;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(missed = n, "Subscription receiver lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        break;
                    }
                }
            }
        })
    }
}
