//! Axum router for the causal inspector.
//!
//! Provides a drop-in `Router` that serves the inspector's GraphQL API
//! and WebSocket subscriptions. Nest it at any path:
//!
//! ```ignore
//! let inspector = causal_inspector::router(read_model, display, event_tx);
//! app.nest("/api/inspector", inspector);
//! ```

use std::sync::Arc;

use async_graphql::{EmptyMutation, Schema};
use async_graphql_axum::{GraphQLProtocol, GraphQLRequest, GraphQLResponse, GraphQLWebSocket};
use axum::extract::State;
use axum::routing::{get, post};
use axum::Router;
use tokio::sync::broadcast;

use crate::display::EventDisplay;
use crate::graphql::{CausalInspectorQuery, CausalInspectorSubscription};
use crate::read_model::{InspectorReadModel, StoredEvent};

type InspectorSchema<D> = Schema<
    CausalInspectorQuery<D>,
    EmptyMutation,
    CausalInspectorSubscription<D>,
>;

/// Build an Axum router serving the inspector GraphQL API.
///
/// Mount it with `Router::nest`:
/// ```ignore
/// app.nest("/api/inspector", causal_inspector::router(read_model, display, event_tx));
/// ```
///
/// Routes:
/// - `POST /` -- GraphQL queries
/// - `GET /ws` -- WebSocket subscriptions
pub fn router<D: EventDisplay + Clone + 'static>(
    read_model: Arc<dyn InspectorReadModel>,
    display: D,
    event_tx: broadcast::Sender<StoredEvent>,
) -> Router {
    let schema = Schema::build(
        CausalInspectorQuery::new(display.clone()),
        EmptyMutation,
        CausalInspectorSubscription::new(display),
    )
    .data(read_model)
    .data(event_tx)
    .finish();

    Router::new()
        .route("/", post(graphql_handler::<D>))
        .route("/ws", get(ws_handler::<D>))
        .with_state(schema)
}

async fn graphql_handler<D: EventDisplay + 'static>(
    State(schema): State<InspectorSchema<D>>,
    req: GraphQLRequest,
) -> GraphQLResponse {
    schema.execute(req.into_inner()).await.into()
}

async fn ws_handler<D: EventDisplay + 'static>(
    State(schema): State<InspectorSchema<D>>,
    protocol: GraphQLProtocol,
    ws: axum::extract::WebSocketUpgrade,
) -> axum::response::Response {
    ws.protocols(["graphql-transport-ws", "graphql-ws"])
        .on_upgrade(move |stream| {
            let ws = GraphQLWebSocket::new(stream, schema, protocol);
            async move { ws.serve().await }
        })
}
