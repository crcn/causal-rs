//! Web server with SSE and WebSocket endpoints for real-time stream

use axum::{
    extract::{Path, Query, State},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::get,
    Json, Router,
};
use futures::stream::Stream;
use seesaw_core::insight::InsightStore;
use serde::Deserialize;
use std::{convert::Infallible, sync::Arc, time::Duration};
use tower_http::{
    cors::CorsLayer,
    services::ServeDir,
    trace::{DefaultMakeSpan, DefaultOnResponse, TraceLayer},
};
use tracing::{info, Level};
use uuid::Uuid;

/// Web server state
#[derive(Clone)]
pub struct AppState<I: InsightStore> {
    pub insight_store: Arc<I>,
}

/// Query params for /api/stream endpoint
#[derive(Debug, Deserialize)]
pub struct StreamQuery {
    #[serde(default)]
    cursor: Option<i64>,
    #[serde(default = "default_limit")]
    limit: i64,
}

fn default_limit() -> i64 {
    100
}

/// Create web server router
///
/// If `base_path` is None, mounts at root. If Some("/insights"), mounts at /insights
pub fn app<I>(insight_store: I, static_dir: Option<&str>, base_path: Option<&str>) -> Router
where
    I: InsightStore + Clone + 'static,
{
    let state = AppState {
        insight_store: Arc::new(insight_store),
    };

    let api_routes = Router::new()
        .route("/stream", get(sse_stream::<I>))
        .route("/tree/:correlation_id", get(get_tree::<I>))
        .route("/stats", get(get_stats::<I>))
        .with_state(state.clone());

    // WebSocket route with its own state
    let ws_state = crate::websocket::WsState {
        insight_store: state.insight_store.clone(),
    };
    let ws_route = Router::new()
        .route("/ws", get(crate::websocket::ws_handler::<I>))
        .with_state(ws_state);

    let api_routes = api_routes.merge(ws_route);

    let mut insight_app = Router::new().nest("/api", api_routes);

    // Serve static files if directory provided
    if let Some(dir) = static_dir {
        insight_app = insight_app.nest_service("/", ServeDir::new(dir));
    }

    insight_app = insight_app
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
                .on_response(DefaultOnResponse::new().level(Level::INFO)),
        )
        .layer(CorsLayer::permissive());

    // Mount at base_path if provided
    if let Some(base) = base_path {
        Router::new().nest(base, insight_app)
    } else {
        insight_app
    }
}

/// SSE endpoint for real-time stream
///
/// Long-polls with cursor-based pagination, emitting SSE events as new entries arrive
async fn sse_stream<I>(
    Query(query): Query<StreamQuery>,
    State(state): State<AppState<I>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>>
where
    I: InsightStore + Clone + 'static,
{
    let stream = async_stream::stream! {
        let mut cursor = query.cursor;
        let limit = query.limit as usize;

        loop {
            // Fetch entries from store
            match state.insight_store.get_recent_events(cursor, limit).await {
                Ok(entries) => {
                    if entries.is_empty() {
                        // No new entries, sleep briefly and retry
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    } else {
                        // Emit entries as SSE events
                        for entry in &entries {
                            let json = serde_json::to_string(&entry).unwrap_or_else(|_| "{}".to_string());
                            yield Ok(Event::default().data(json));
                        }
                        // Update cursor to last entry's seq
                        if let Some(last) = entries.last() {
                            cursor = Some(last.seq);
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("Error fetching stream: {}", e);
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    };

    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// Tree endpoint - Get workflow causality tree
async fn get_tree<I>(
    Path(correlation_id): Path<Uuid>,
    State(state): State<AppState<I>>,
) -> Response
where
    I: InsightStore + Clone + 'static,
{
    match state.insight_store.get_workflow_tree(correlation_id).await {
        Ok(tree) => Json(tree).into_response(),
        Err(e) => {
            tracing::error!("Error building tree: {}", e);
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to build tree: {}", e),
            )
                .into_response()
        }
    }
}

async fn get_stats<I>(State(state): State<AppState<I>>) -> Response
where
    I: InsightStore + Clone + 'static,
{
    match state.insight_store.get_stats().await {
        Ok(stats) => Json(stats).into_response(),
        Err(e) => {
            tracing::error!("Error fetching stats: {}", e);
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to fetch stats: {}", e),
            )
                .into_response()
        }
    }
}

/// Start the web server
pub async fn serve<I>(
    insight_store: I,
    addr: &str,
    static_dir: Option<&str>,
) -> anyhow::Result<()>
where
    I: InsightStore + Clone + 'static,
{
    let app = app(insight_store, static_dir, None);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!("Seesaw Insight server listening on {}", addr);
    info!("  SSE endpoint: http://{}/api/stream", addr);
    info!("  Tree API: http://{}/api/tree/:correlation_id", addr);
    info!("  Stats API: http://{}/api/stats", addr);
    info!("  Dashboard: http://{}/", addr);
    axum::serve(listener, app).await?;
    Ok(())
}
