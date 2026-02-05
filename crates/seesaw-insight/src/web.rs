//! Web server with SSE and WebSocket endpoints for real-time stream

use crate::{
    stream::StreamReader,
    tree::TreeBuilder,
    websocket::{stream_broadcaster, ws_handler, StreamBroadcast, WsState},
};
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
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};
use std::{convert::Infallible, sync::Arc, time::Duration};
use tokio::sync::broadcast;
use tower_http::{
    cors::CorsLayer,
    services::ServeDir,
    trace::{DefaultMakeSpan, DefaultOnResponse, TraceLayer},
};
use tracing::{info, Level};
use uuid::Uuid;

/// Web server state
#[derive(Clone)]
pub struct AppState {
    pub reader: Arc<StreamReader>,
    pub tree_builder: Arc<TreeBuilder>,
    pub pool: PgPool,
    pub broadcast: StreamBroadcast,
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
pub fn app(pool: PgPool, static_dir: Option<&str>) -> Router {
    let reader = Arc::new(StreamReader::new(pool.clone()));
    let tree_builder = Arc::new(TreeBuilder::new(pool.clone()));

    // Create broadcast channel for WebSocket updates
    let (broadcast, _) = broadcast::channel(1000);

    let state = AppState {
        reader: reader.clone(),
        tree_builder,
        pool,
        broadcast: broadcast.clone(),
    };

    let ws_state = WsState {
        reader: reader.clone(),
        broadcast: broadcast.clone(),
    };

    // Start background broadcaster task
    tokio::spawn(stream_broadcaster(reader, broadcast));

    let api_routes = Router::new()
        .route("/stream", get(sse_stream))
        .route("/ws", get(ws_handler).with_state(ws_state))
        .route("/tree/:correlation_id", get(get_tree))
        .route("/stats", get(get_stats))
        .with_state(state);

    let mut app = Router::new().nest("/api", api_routes);

    // Serve static files if directory provided
    if let Some(dir) = static_dir {
        app = app.nest_service("/", ServeDir::new(dir));
    }

    app.layer(
        TraceLayer::new_for_http()
            .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
            .on_response(DefaultOnResponse::new().level(Level::INFO)),
    )
    .layer(CorsLayer::permissive())
}

/// SSE endpoint for real-time stream
///
/// Long-polls with cursor-based pagination, emitting SSE events as new entries arrive
async fn sse_stream(
    Query(query): Query<StreamQuery>,
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = async_stream::stream! {
        let mut cursor = query.cursor;
        let limit = query.limit;

        loop {
            // Fetch entries from stream table
            match state.reader.fetch(cursor, limit).await {
                Ok((entries, next_cursor)) => {
                    if entries.is_empty() {
                        // No new entries, sleep briefly and retry
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    } else {
                        // Emit entries as SSE events
                        for entry in entries {
                            let json = serde_json::to_string(&entry).unwrap_or_else(|_| "{}".to_string());
                            yield Ok(Event::default().data(json));
                        }
                        cursor = next_cursor;
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
async fn get_tree(
    Path(correlation_id): Path<Uuid>,
    State(state): State<AppState>,
) -> Response {
    match state.tree_builder.build_tree(correlation_id).await {
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

/// Stats endpoint for dashboard metrics
#[derive(Debug, Serialize)]
pub struct Stats {
    pub total_events: i64,
    pub active_effects: i64,
    pub recent_entries: i64,
}

async fn get_stats(State(state): State<AppState>) -> Response {
    let result = sqlx::query(
        r#"
        SELECT
            (SELECT COUNT(*) FROM seesaw_events) as total_events,
            (SELECT COUNT(*) FROM seesaw_effect_executions WHERE status IN ('pending', 'executing')) as active_effects,
            (SELECT COUNT(*) FROM seesaw_stream WHERE created_at > NOW() - INTERVAL '5 minutes') as recent_entries
        "#
    )
    .fetch_one(&state.pool)
    .await;

    match result {
        Ok(row) => {
            let stats = Stats {
                total_events: row.try_get::<i64, _>("total_events").unwrap_or(0),
                active_effects: row.try_get::<i64, _>("active_effects").unwrap_or(0),
                recent_entries: row.try_get::<i64, _>("recent_entries").unwrap_or(0),
            };
            Json(stats).into_response()
        }
        Err(e) => {
            tracing::error!("Error fetching stats: {}", e);
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("Database error: {}", e),
            )
                .into_response()
        }
    }
}

/// Start the web server
pub async fn serve(pool: PgPool, addr: &str, static_dir: Option<&str>) -> anyhow::Result<()> {
    let app = app(pool, static_dir);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!("Seesaw Insight server listening on {}", addr);
    info!("  SSE endpoint: http://{}/api/stream", addr);
    info!("  WebSocket endpoint: ws://{}/api/ws", addr);
    info!("  Tree API: http://{}/api/tree/:correlation_id", addr);
    info!("  Dashboard: http://{}/", addr);
    axum::serve(listener, app).await?;
    Ok(())
}
