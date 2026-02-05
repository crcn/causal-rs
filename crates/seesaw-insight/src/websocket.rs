//! WebSocket support for real-time stream updates

use crate::stream::{StreamEntry, StreamReader};
use axum::{
    extract::{
        ws::{Message, WebSocket},
        Query, State, WebSocketUpgrade,
    },
    response::Response,
};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use std::{sync::Arc, time::Duration};
use tokio::sync::broadcast;
use tracing::{debug, error, info};
use uuid::Uuid;

/// Broadcast channel for stream updates
pub type StreamBroadcast = broadcast::Sender<StreamEntry>;

/// Query params for WebSocket connection
#[derive(Debug, Deserialize)]
pub struct WsQuery {
    #[serde(default)]
    cursor: Option<i64>,
    #[serde(default)]
    correlation_id: Option<Uuid>,
}

/// WebSocket state
#[derive(Clone)]
pub struct WsState {
    pub reader: Arc<StreamReader>,
    pub broadcast: StreamBroadcast,
}

/// Handle WebSocket upgrade
pub async fn ws_handler(
    ws: WebSocketUpgrade,
    Query(query): Query<WsQuery>,
    State(state): State<WsState>,
) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, query, state))
}

/// Handle WebSocket connection
async fn handle_socket(socket: WebSocket, query: WsQuery, state: WsState) {
    let (mut sender, mut receiver) = socket.split();
    let mut rx = state.broadcast.subscribe();

    info!(
        "WebSocket connected - cursor: {:?}, correlation: {:?}",
        query.cursor, query.correlation_id
    );

    // Send historical entries if cursor provided
    if let Some(cursor) = query.cursor {
        let limit = 100;
        let result = if let Some(correlation_id) = query.correlation_id {
            state
                .reader
                .fetch_workflow(correlation_id, Some(cursor), limit)
                .await
        } else {
            state.reader.fetch(Some(cursor), limit).await
        };

        match result {
            Ok((entries, _)) => {
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

    // Spawn task to receive broadcast updates
    let mut send_task = tokio::spawn(async move {
        while let Ok(entry) = rx.recv().await {
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
                Message::Ping(data) => {
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

    info!("WebSocket disconnected");
}

/// Background task to poll stream and broadcast updates
pub async fn stream_broadcaster(reader: Arc<StreamReader>, tx: StreamBroadcast) {
    let mut cursor: Option<i64> = None;
    let limit = 100;

    loop {
        match reader.fetch(cursor, limit).await {
            Ok((entries, next_cursor)) => {
                if entries.is_empty() {
                    // No new entries, sleep briefly
                    tokio::time::sleep(Duration::from_millis(100)).await;
                } else {
                    // Broadcast each entry
                    for entry in entries {
                        debug!("Broadcasting entry seq={}", entry.seq);
                        // Ignore send errors (no receivers)
                        let _ = tx.send(entry);
                    }
                    cursor = next_cursor;
                }
            }
            Err(e) => {
                error!("Error fetching stream: {}", e);
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}
