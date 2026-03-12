//! Inspector Demo — causal engine + inspector GraphQL API, all in-memory.
//!
//! Starts an order processing pipeline and exposes a GraphQL inspector API
//! that shows events flowing through the system in real time.
//!
//! ```
//! cargo run -p inspector-demo
//! ```
//!
//! Then open:
//!   - http://localhost:4000/causal — Inspector UI
//!   - http://localhost:4000 — GraphQL playground

use std::sync::Arc;

use anyhow::Result;
use async_graphql::{EmptyMutation, Schema};
use axum::{Router, routing::get};
use tower_http::services::ServeDir;
use tokio::sync::broadcast;

use causal::{event, events, reactor, Context, Engine, MemoryStore};
use causal_inspector::{
    InspectorReadModel, CausalInspectorQuery, CausalInspectorSubscription, StoredEvent,
    display::EventDisplay,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ── Event display ────────────────────────────────────────────────

struct DemoEventDisplay;

impl EventDisplay for DemoEventDisplay {
    fn display_name(&self, event_type: &str, _payload: &serde_json::Value) -> String {
        event_type
            .split('_')
            .map(|w| {
                let mut c = w.chars();
                match c.next() {
                    None => String::new(),
                    Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn summary(&self, event_type: &str, payload: &serde_json::Value) -> Option<String> {
        match event_type {
            "order_placed" => {
                let customer = payload.get("customer")?.as_str()?;
                let total = payload.get("total")?.as_f64()?;
                Some(format!("{customer} — ${total:.2}"))
            }
            "payment_processed" => {
                let amount = payload.get("amount")?.as_f64()?;
                Some(format!("${amount:.2}"))
            }
            "order_shipped" => {
                let tracking = payload.get("tracking")?.as_str()?;
                Some(tracking.to_string())
            }
            "customer_notified" => {
                let message = payload.get("message")?.as_str()?;
                Some(message.to_string())
            }
            _ => None,
        }
    }
}

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

type InspectorSchema = Schema<
    CausalInspectorQuery<DemoEventDisplay>,
    EmptyMutation,
    CausalInspectorSubscription<DemoEventDisplay>,
>;

async fn graphql_handler(
    schema: axum::extract::State<InspectorSchema>,
    req: async_graphql_axum::GraphQLRequest,
) -> async_graphql_axum::GraphQLResponse {
    schema.execute(req.into_inner()).await.into()
}

async fn graphql_ws_handler(
    schema: axum::extract::State<InspectorSchema>,
    protocol: async_graphql_axum::GraphQLProtocol,
    ws: axum::extract::WebSocketUpgrade,
) -> axum::response::Response {
    ws.protocols(["graphql-transport-ws", "graphql-ws"])
        .on_upgrade(move |stream| {
            let stream = async_graphql_axum::GraphQLWebSocket::new(stream, schema.0, protocol);
            async move { stream.serve().await }
        })
}

async fn graphiql() -> axum::response::Html<String> {
    axum::response::Html(
        async_graphql::http::GraphiQLSource::build()
            .endpoint("/")
            .subscription_endpoint("/ws")
            .finish(),
    )
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

    // 2. Create a broadcast sender for the inspector subscription.
    //    We bridge MemoryStore's PersistedEvent broadcast → StoredEvent.
    let (inspector_tx, _) = broadcast::channel::<StoredEvent>(1024);
    let bridge_tx = inspector_tx.clone();
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
                        reactor_id: persisted
                            .metadata
                            .get("reactor_id")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string()),
                    };
                    let _ = bridge_tx.send(stored);
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(missed = n, "Inspector bridge lagged");
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
        CausalInspectorQuery::new(DemoEventDisplay),
        EmptyMutation,
        CausalInspectorSubscription::new(DemoEventDisplay),
    )
    .data(store.clone() as Arc<dyn InspectorReadModel>)
    .data(inspector_tx)
    .finish();

    // 5. Start HTTP server
    //    Serve the pre-built inspector UI at /causal (static files from ui/dist).
    let ui_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("ui/dist");
    let app = Router::new()
        .route("/ws", get(graphql_ws_handler))
        .route("/", get(graphiql).post(graphql_handler))
        .nest_service("/causal", ServeDir::new(&ui_dir).fallback(tower_http::services::ServeFile::new(ui_dir.join("index.html"))))
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
    tracing::info!("Inspector UI at http://localhost:4000/causal");
    tracing::info!("GraphQL playground at http://localhost:4000");
    tracing::info!("WebSocket subscriptions at ws://localhost:4000/ws");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
