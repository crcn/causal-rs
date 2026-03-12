//! Store-agnostic GraphQL resolvers for the causal inspector.
//!
//! Queries read from `Arc<dyn InspectorReadModel>` (injected into context).
//! Subscriptions read from `tokio::sync::broadcast::Sender<PersistedEvent>`
//! (injected into context — how events get broadcast is the consumer's concern).

use std::sync::Arc;

use async_graphql::{Context, Object, Result, Subscription};
use chrono::{DateTime, Utc};
use futures::Stream;

use crate::display::EventDisplay;
use crate::read_model::{InspectorReadModel, EventQuery, StoredEvent};
use crate::types::*;

/// Generic inspector query resolvers for causal event tables.
///
/// The `D` parameter controls how events are displayed (names, summaries).
///
/// # Context requirements
///
/// The async-graphql context must contain:
/// - `Arc<dyn InspectorReadModel>` — the store to query
pub struct CausalInspectorQuery<D: EventDisplay> {
    display: Arc<D>,
}

impl<D: EventDisplay> CausalInspectorQuery<D> {
    pub fn new(display: D) -> Self {
        Self {
            display: Arc::new(display),
        }
    }
}

fn stored_to_inspector(events: Vec<StoredEvent>, display: &dyn EventDisplay) -> Vec<InspectorEvent> {
    events.iter().map(|e| e.to_inspector_event(display)).collect()
}

#[Object]
impl<D: EventDisplay + 'static> CausalInspectorQuery<D> {
    /// Paginated reverse-chronological event listing with optional filters.
    async fn inspector_events(
        &self,
        ctx: &Context<'_>,
        limit: Option<i32>,
        cursor: Option<i64>,
        search: Option<String>,
        from: Option<DateTime<Utc>>,
        to: Option<DateTime<Utc>>,
        correlation_id: Option<String>,
    ) -> Result<InspectorEventsPage> {
        let read_model = ctx.data::<Arc<dyn InspectorReadModel>>()?;
        let lim = (limit.unwrap_or(50) as usize).min(200);

        let query = EventQuery {
            limit: lim,
            cursor,
            search,
            from,
            to,
            correlation_id,
        };

        let stored = read_model
            .list_events(&query)
            .await
            .map_err(|e| async_graphql::Error::new(format!("Failed to load events: {e}")))?;

        let next_cursor = if stored.len() == lim {
            stored.last().map(|e| e.seq)
        } else {
            None
        };

        let events = stored_to_inspector(stored, self.display.as_ref());
        Ok(InspectorEventsPage { events, next_cursor })
    }

    /// Walk the causal tree for an event (ancestors + descendants).
    async fn inspector_causal_tree(&self, ctx: &Context<'_>, seq: i64) -> Result<InspectorCausalTree> {
        let read_model = ctx.data::<Arc<dyn InspectorReadModel>>()?;

        let (stored, root_seq) = read_model
            .causal_tree(seq)
            .await
            .map_err(|e| async_graphql::Error::new(format!("Failed to load causal tree: {e}")))?;

        let events = stored_to_inspector(stored, self.display.as_ref());
        Ok(InspectorCausalTree { events, root_seq })
    }

    /// Fetch all events sharing a correlation_id (causal flow DAG viewer).
    async fn inspector_causal_flow(
        &self,
        ctx: &Context<'_>,
        correlation_id: String,
    ) -> Result<InspectorCausalFlow> {
        let read_model = ctx.data::<Arc<dyn InspectorReadModel>>()?;

        let stored = read_model
            .causal_flow(&correlation_id)
            .await
            .map_err(|e| async_graphql::Error::new(format!("Failed to load causal flow: {e}")))?;

        let events = stored_to_inspector(stored, self.display.as_ref());
        Ok(InspectorCausalFlow { events })
    }

    /// Fetch reactor logs for a specific event + reactor.
    async fn inspector_reactor_logs(
        &self,
        ctx: &Context<'_>,
        event_id: String,
        reactor_id: String,
    ) -> Result<Vec<ReactorLog>> {
        let read_model = ctx.data::<Arc<dyn InspectorReadModel>>()?;

        let event_uuid = uuid::Uuid::parse_str(&event_id)
            .map_err(|e| async_graphql::Error::new(format!("Invalid event_id: {e}")))?;

        let entries = read_model
            .reactor_logs(event_uuid, &reactor_id)
            .await
            .map_err(|e| async_graphql::Error::new(format!("Failed to load reactor logs: {e}")))?;

        Ok(entries
            .into_iter()
            .map(|r| ReactorLog {
                event_id: r.event_id.to_string(),
                reactor_id: r.reactor_id,
                level: r.level,
                message: r.message,
                data: r.data,
                logged_at: r.logged_at,
            })
            .collect())
    }

    /// Fetch all reactor logs for a correlation chain.
    async fn inspector_reactor_logs_by_correlation(
        &self,
        ctx: &Context<'_>,
        correlation_id: String,
    ) -> Result<Vec<ReactorLog>> {
        let read_model = ctx.data::<Arc<dyn InspectorReadModel>>()?;

        let entries = read_model
            .reactor_logs_by_correlation(&correlation_id)
            .await
            .map_err(|e| async_graphql::Error::new(format!("Failed to load reactor logs: {e}")))?;

        Ok(entries
            .into_iter()
            .map(|r| ReactorLog {
                event_id: r.event_id.to_string(),
                reactor_id: r.reactor_id,
                level: r.level,
                message: r.message,
                data: r.data,
                logged_at: r.logged_at,
            })
            .collect())
    }

    /// Fetch reactor describe() blocks for a correlation chain.
    async fn inspector_reactor_descriptions(
        &self,
        ctx: &Context<'_>,
        correlation_id: String,
    ) -> Result<Vec<ReactorDescription>> {
        let read_model = ctx.data::<Arc<dyn InspectorReadModel>>()?;

        let entries = read_model
            .reactor_descriptions(&correlation_id)
            .await
            .map_err(|e| {
                async_graphql::Error::new(format!("Failed to load reactor descriptions: {e}"))
            })?;

        Ok(entries
            .into_iter()
            .map(|r| ReactorDescription {
                reactor_id: r.reactor_id,
                blocks: r.description,
            })
            .collect())
    }

    /// Fetch aggregated reactor execution outcomes for a correlation chain.
    async fn inspector_reactor_outcomes(
        &self,
        ctx: &Context<'_>,
        correlation_id: String,
    ) -> Result<Vec<ReactorOutcome>> {
        let read_model = ctx.data::<Arc<dyn InspectorReadModel>>()?;

        let entries = read_model
            .reactor_outcomes(&correlation_id)
            .await
            .map_err(|e| {
                async_graphql::Error::new(format!("Failed to load reactor outcomes: {e}"))
            })?;

        Ok(entries
            .into_iter()
            .map(|r| ReactorOutcome {
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
}

/// Generic inspector subscription — streams live events.
///
/// # Context requirements
///
/// The async-graphql context must contain:
/// - `Arc<dyn InspectorReadModel>` — for catch-up queries
/// - `tokio::sync::broadcast::Sender<StoredEvent>` — for live event stream
pub struct CausalInspectorSubscription<D: EventDisplay> {
    display: Arc<D>,
}

impl<D: EventDisplay> CausalInspectorSubscription<D> {
    pub fn new(display: D) -> Self {
        Self {
            display: Arc::new(display),
        }
    }
}

#[Subscription]
impl<D: EventDisplay + 'static> CausalInspectorSubscription<D> {
    /// Stream live events. If `last_seq` is provided, replays missed events first.
    async fn inspector_event_added(
        &self,
        ctx: &Context<'_>,
        last_seq: Option<i64>,
    ) -> Result<impl Stream<Item = InspectorEvent>> {
        let read_model = ctx.data::<Arc<dyn InspectorReadModel>>()?.clone();
        let tx = ctx
            .data::<tokio::sync::broadcast::Sender<StoredEvent>>()?
            .clone();
        let mut rx = tx.subscribe();
        let display = Arc::clone(&self.display);

        Ok(async_stream::stream! {
            let mut high_water: i64 = 0;

            // Catch-up phase
            if let Some(start) = last_seq {
                let catch_up_start = start + 1;
                match read_model.events_from_seq(catch_up_start, 500).await {
                    Ok(events) => {
                        for event in events {
                            if event.seq > high_water {
                                high_water = event.seq;
                            }
                            yield event.to_inspector_event(display.as_ref());
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
                            yield event.to_inspector_event(display.as_ref());
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
