//! WebSocket support for real-time stream updates

use axum::{
    extract::{
        ws::{Message, WebSocket},
        Query, State, WebSocketUpgrade,
    },
    response::Response,
};
use futures::{stream::StreamExt as FuturesStreamExt, SinkExt};
use seesaw_core::InsightStore;
use serde::Deserialize;
use std::sync::Arc;
use tracing::{debug, error};
use uuid::Uuid;

/// Query params for WebSocket connection
#[derive(Debug, Deserialize)]
pub struct WsQuery {
    #[serde(default)]
    cursor: Option<i64>,
    #[serde(default)]
    correlation_id: Option<Uuid>,
}

/// WebSocket state using InsightStore
#[derive(Clone)]
pub struct WsState<I> {
    pub insight_store: Arc<I>,
}

/// Handle WebSocket upgrade
pub async fn ws_handler<I>(
    ws: WebSocketUpgrade,
    Query(query): Query<WsQuery>,
    State(state): State<WsState<I>>,
) -> Response
where
    I: InsightStore + Clone + 'static,
{
    ws.on_upgrade(move |socket| handle_socket(socket, query, state))
}

/// Handle WebSocket connection
async fn handle_socket<I>(socket: WebSocket, query: WsQuery, state: WsState<I>)
where
    I: InsightStore + Clone + 'static,
{
    let (mut sender, mut receiver) = socket.split();

    debug!(
        "WebSocket connected - cursor: {:?}, correlation: {:?}",
        query.cursor, query.correlation_id
    );

    // Send historical entries (always fetch recent events on connect)
    let limit = 100;
    match state
        .insight_store
        .get_recent_events(query.cursor, limit)
        .await
    {
        Ok(entries) => {
            for entry in entries {
                if let Ok(json) = serde_json::to_string(&entry) {
                    if sender.send(Message::Text(json)).await.is_err() {
                        return;
                    }
                }
            }
        }
        Err(e) => {
            error!("Failed to fetch historical entries: {}", e);
        }
    }

    // Send initial connected message
    let _ = sender
        .send(Message::Text(
            serde_json::json!({
                "type": "connected",
                "message": "WebSocket stream active"
            })
            .to_string(),
        ))
        .await;

    // Subscribe to live events
    let mut event_stream = match state.insight_store.subscribe_events().await {
        Ok(stream) => stream,
        Err(e) => {
            error!("Failed to subscribe to events: {}", e);
            return;
        }
    };

    // Spawn task to receive stream updates
    let mut send_task = tokio::spawn(async move {
        while let Some(entry) = event_stream.next().await {
            // Filter by correlation_id if specified
            if let Some(filter_id) = query.correlation_id {
                if entry.correlation_id != filter_id {
                    continue;
                }
            }

            // Send entry to client
            if let Ok(json) = serde_json::to_string(&entry) {
                debug!("Broadcasting entry seq={} to WebSocket", entry.seq);
                if sender.send(Message::Text(json)).await.is_err() {
                    break;
                }
            }
        }
    });

    // Receive messages from client (for keepalive/control)
    let mut recv_task = tokio::spawn(async move {
        while let Some(Ok(msg)) = receiver.next().await {
            match msg {
                Message::Text(text) => {
                    debug!("Received WebSocket message: {}", text);
                    // Handle control messages if needed
                }
                Message::Close(_) => {
                    debug!("WebSocket close message received");
                    break;
                }
                Message::Ping(_data) => {
                    debug!("WebSocket ping received");
                    // Pong is sent automatically by axum
                }
                _ => {}
            }
        }
    });

    // Wait for either task to complete
    tokio::select! {
        _ = (&mut send_task) => {
            recv_task.abort();
        }
        _ = (&mut recv_task) => {
            send_task.abort();
        }
    }

    debug!("WebSocket disconnected");
}
