//! Admin Demo — causal engine + admin GraphQL API, all in-memory.
//!
//! Starts an order processing pipeline and exposes a GraphQL admin API
//! that shows events flowing through the system in real time.
//!
//! ```
//! cargo run -p admin-demo
//! ```
//!
//! Then open http://localhost:4000 for the GraphQL playground.
//!
//! ## Example queries
//!
//! ```graphql
//! # List recent events
//! { adminEvents(limit: 20) { events { seq type name payload reactorId } nextCursor } }
//!
//! # Walk the causal tree for event #1
//! { adminCausalTree(seq: 1) { events { seq type name parentId } rootSeq } }
//!
//! # Stream live events
//! subscription { adminEventAdded { seq type name payload } }
//! ```

use std::sync::Arc;

use anyhow::Result;
use async_graphql::{EmptyMutation, Schema};
use axum::{Router, routing::get};
use tokio::sync::broadcast;

use causal::{event, events, reactor, Context, Engine, MemoryStore};
use causal_admin::{
    AdminReadModel, CausalAdminQuery, CausalAdminSubscription, DefaultEventDisplay, StoredEvent,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ── Events ──────────────────────────────────────────────────────

#[event]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct OrderPlaced {
    order_id: Uuid,
    customer: String,
    total: f64,
}

#[event]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PaymentProcessed {
    order_id: Uuid,
    amount: f64,
}

#[event]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct OrderShipped {
    order_id: Uuid,
    tracking: String,
}

#[event]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CustomerNotified {
    order_id: Uuid,
    message: String,
}

// ── Dependencies ────────────────────────────────────────────────

#[derive(Clone)]
struct Deps;

// ── GraphQL wiring ──────────────────────────────────────────────

type AdminSchema = Schema<
    CausalAdminQuery<DefaultEventDisplay>,
    EmptyMutation,
    CausalAdminSubscription<DefaultEventDisplay>,
>;

async fn graphql_handler(
    schema: axum::extract::State<AdminSchema>,
    req: async_graphql_axum::GraphQLRequest,
) -> async_graphql_axum::GraphQLResponse {
    schema.execute(req.into_inner()).await.into()
}

async fn graphql_ws_handler(
    schema: axum::extract::State<AdminSchema>,
    protocol: async_graphql_axum::GraphQLProtocol,
    ws: axum::extract::WebSocketUpgrade,
) -> axum::response::Response {
    ws.protocols(["graphql-transport-ws", "graphql-ws"])
        .on_upgrade(move |stream| {
            let stream = async_graphql_axum::GraphQLWebSocket::new(stream, schema.0, protocol);
            async move { stream.serve().await }
        })
}

// ── Main ────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_target(false)
        .with_level(true)
        .init();

    // 1. Create store with broadcast
    let (store, _rx) = MemoryStore::with_broadcast(1024);
    let store = Arc::new(store);

    // 2. Create a broadcast sender for the admin subscription.
    //    We bridge MemoryStore's PersistedEvent broadcast → StoredEvent.
    let (admin_tx, _) = broadcast::channel::<StoredEvent>(1024);
    let bridge_tx = admin_tx.clone();
    let mut store_rx = store.subscribe().unwrap();
    tokio::spawn(async move {
        loop {
            match store_rx.recv().await {
                Ok(persisted) => {
                    let stored = StoredEvent {
                        seq: persisted.position.raw() as i64,
                        ts: persisted.created_at,
                        event_type: persisted.event_type.clone(),
                        payload: persisted.payload.clone(),
                        id: Some(persisted.event_id),
                        parent_id: persisted.parent_id,
                        correlation_id: Some(persisted.correlation_id),
                        run_id: persisted
                            .metadata
                            .get("run_id")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string()),
                        reactor_id: persisted
                            .metadata
                            .get("reactor_id")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string()),
                    };
                    let _ = bridge_tx.send(stored);
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(missed = n, "Admin bridge lagged");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    // 3. Build the engine
    let engine = Engine::new(Deps)
        .with_store(store.clone())
        .with_reactor(
            reactor::on::<OrderPlaced>()
                .id("process_payment")
                .then(|event, _ctx: Context<Deps>| async move {
                    tracing::info!(order_id = %event.order_id, "Processing payment");
                    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                    Ok(events![PaymentProcessed {
                        order_id: event.order_id,
                        amount: event.total,
                    }])
                }),
        )
        .with_reactor(
            reactor::on::<PaymentProcessed>()
                .id("ship_order")
                .then(|event, _ctx: Context<Deps>| async move {
                    tracing::info!(order_id = %event.order_id, "Shipping order");
                    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                    Ok(events![OrderShipped {
                        order_id: event.order_id,
                        tracking: format!("TRK-{}", &event.order_id.to_string()[..8]),
                    }])
                }),
        )
        .with_reactor(
            reactor::on::<OrderShipped>()
                .id("notify_customer")
                .then(|event, _ctx: Context<Deps>| async move {
                    tracing::info!(order_id = %event.order_id, "Notifying customer");
                    Ok(events![CustomerNotified {
                        order_id: event.order_id,
                        message: format!("Your order shipped! Tracking: {}", event.tracking),
                    }])
                }),
        );

    // 4. Build GraphQL schema
    let schema = Schema::build(
        CausalAdminQuery::new(DefaultEventDisplay),
        EmptyMutation,
        CausalAdminSubscription::new(DefaultEventDisplay),
    )
    .data(store.clone() as Arc<dyn AdminReadModel>)
    .data(admin_tx)
    .finish();

    // 5. Start HTTP server
    let app = Router::new()
        .route("/ws", get(graphql_ws_handler))
        .route("/", get(graphql_handler).post(graphql_handler))
        .with_state(schema.clone());

    // 6. Spawn order generation in background
    let engine = Arc::new(engine);
    let engine_clone = engine.clone();
    tokio::spawn(async move {
        let customers = ["Alice", "Bob", "Charlie", "Diana", "Eve"];
        let mut i = 0;
        loop {
            let customer = customers[i % customers.len()];
            let order_id = Uuid::new_v4();
            let total = 29.99 + (i as f64 * 10.0);

            tracing::info!(
                order_id = %order_id,
                customer,
                total,
                "Placing order"
            );

            if let Err(e) = engine_clone
                .emit(OrderPlaced {
                    order_id,
                    customer: customer.to_string(),
                    total,
                })
                .settled()
                .await
            {
                tracing::error!(error = %e, "Failed to process order");
            }

            i += 1;
            tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
        }
    });

    let addr = "0.0.0.0:4000";
    tracing::info!("Admin GraphQL playground at http://localhost:4000");
    tracing::info!("WebSocket subscriptions at ws://localhost:4000/ws");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
