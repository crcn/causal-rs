//! Seesaw Insight Demo
//!
//! This demo generates realistic workflow events to showcase the insight dashboard.

use anyhow::Result;
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{Html, Json},
    routing::{get, post},
    Router,
};
use seesaw_core::{
    effect, reducer,
    runtime::{Runtime, RuntimeConfig},
    Engine,
};
use seesaw_memory::MemoryStore;
use std::{sync::Arc, time::Duration};
use tower_http::cors::CorsLayer;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum DemoMode {
    Normal,
    Slow,
    BatchJoin,
    FailPayment,
    FailInventory,
    FailShipping,
    FailEmail,
}

impl Default for DemoMode {
    fn default() -> Self {
        Self::Normal
    }
}

// Domain events
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
enum OrderEvent {
    OrderPlaced {
        order_id: Uuid,
        customer_id: Uuid,
        total: f64,
        mode: DemoMode,
    },
    OrderValidated {
        order_id: Uuid,
        total: f64,
        mode: DemoMode,
    },
    PaymentProcessed {
        order_id: Uuid,
        amount: f64,
        mode: DemoMode,
    },
    InventoryReserved {
        order_id: Uuid,
        amount: f64,
        mode: DemoMode,
    },
    OrderShipped {
        order_id: Uuid,
        tracking_number: String,
        mode: DemoMode,
    },
    EmailSent {
        order_id: Uuid,
        email_type: String,
        mode: DemoMode,
    },
    OrderCompleted {
        order_id: Uuid,
        mode: DemoMode,
    },
    OrderFailed {
        order_id: Uuid,
        reason: String,
        mode: DemoMode,
    },
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct BatchWorkItem {
    order_id: Uuid,
    item_index: usize,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct BatchWorkResult {
    order_id: Uuid,
    item_index: usize,
    success: bool,
    message: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct BatchJoinReady {
    order_id: Uuid,
    total: usize,
    succeeded: usize,
    failed: usize,
}

// Simple state tracking
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
struct OrderState {
    validated: bool,
    payment_processed: bool,
    inventory_reserved: bool,
    shipped: bool,
    batch_items_total: usize,
    batch_items_done: usize,
    batch_failed: usize,
    batch_joined: bool,
}

// Mock dependencies
#[derive(Clone)]
struct Deps;

#[derive(Debug, serde::Deserialize)]
struct CreateOrderQuery {
    mode: Option<DemoMode>,
    total: Option<f64>,
}

#[derive(Debug, serde::Deserialize)]
struct SeedFailuresQuery {
    count: Option<usize>,
}

async fn enqueue_order(
    engine: &Engine<OrderState, Deps, MemoryStore>,
    mode: DemoMode,
    total: f64,
) -> Result<serde_json::Value, StatusCode> {
    let order_id = Uuid::new_v4();
    let customer_id = Uuid::new_v4();

    engine
        .process(OrderEvent::OrderPlaced {
            order_id,
            customer_id,
            total,
            mode,
        })
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(serde_json::json!({
        "order_id": order_id,
        "customer_id": customer_id,
        "total": total,
        "mode": mode,
    }))
}

// API handler to create new orders
async fn create_order(
    Query(query): Query<CreateOrderQuery>,
    State(engine): State<Arc<Engine<OrderState, Deps, MemoryStore>>>,
) -> Result<(StatusCode, Json<serde_json::Value>), StatusCode> {
    let mode = query.mode.unwrap_or_default();
    let total = query
        .total
        .filter(|total| *total > 0.0)
        .unwrap_or_else(|| 99.99 + (rand::random::<f64>() * 200.0));
    let data = enqueue_order(&engine, mode, total).await?;

    Ok((
        StatusCode::CREATED,
        Json(data),
    ))
}

async fn seed_scenarios(
    State(engine): State<Arc<Engine<OrderState, Deps, MemoryStore>>>,
) -> Result<(StatusCode, Json<serde_json::Value>), StatusCode> {
    let scenarios = [
        DemoMode::Normal,
        DemoMode::Slow,
        DemoMode::BatchJoin,
        DemoMode::FailPayment,
        DemoMode::FailInventory,
        DemoMode::FailShipping,
        DemoMode::FailEmail,
    ];

    let mut created = Vec::with_capacity(scenarios.len());
    for mode in scenarios {
        let total = match mode {
            DemoMode::Slow => 199.0,
            DemoMode::BatchJoin => 219.0,
            DemoMode::FailPayment => 149.0,
            DemoMode::FailInventory => 179.0,
            DemoMode::FailShipping => 249.0,
            DemoMode::FailEmail => 129.0,
            DemoMode::Normal => 99.0,
        };
        created.push(enqueue_order(&engine, mode, total).await?);
    }

    Ok((StatusCode::CREATED, Json(serde_json::json!({ "created": created }))))
}

async fn seed_failures(
    Query(query): Query<SeedFailuresQuery>,
    State(engine): State<Arc<Engine<OrderState, Deps, MemoryStore>>>,
) -> Result<(StatusCode, Json<serde_json::Value>), StatusCode> {
    let count = query.count.unwrap_or(8).clamp(1, 40);
    let failure_modes = [
        DemoMode::FailPayment,
        DemoMode::FailInventory,
        DemoMode::FailShipping,
        DemoMode::FailEmail,
    ];

    let mut created = Vec::with_capacity(count);
    for i in 0..count {
        let mode = failure_modes[i % failure_modes.len()];
        let total = 120.0 + (i as f64 * 17.5);
        created.push(enqueue_order(&engine, mode, total).await?);
    }

    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({
            "created": created,
            "count": count,
            "modes": failure_modes,
        })),
    ))
}

// Simple HTML toy app
async fn toy_app() -> Html<&'static str> {
    Html(r#"
<!DOCTYPE html>
<html>
<head>
    <title>Seesaw Demo</title>
    <style>
        body {
            font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif;
            max-width: 800px;
            margin: 50px auto;
            padding: 20px;
            background: #0a0a0a;
            color: #e0e0e0;
        }
        h1 { color: #60a5fa; }
        .button {
            background: #3b82f6;
            color: white;
            border: none;
            padding: 12px 18px;
            font-size: 16px;
            border-radius: 8px;
            cursor: pointer;
            font-weight: 600;
            transition: all 0.2s;
            margin-right: 8px;
            margin-bottom: 8px;
        }
        .button:hover { background: #2563eb; transform: translateY(-2px); }
        .button:active { transform: translateY(0); }
        .button.slow { background: #a855f7; }
        .button.slow:hover { background: #9333ea; }
        .button.fail { background: #dc2626; }
        .button.fail:hover { background: #b91c1c; }
        .button.seed { background: #0891b2; }
        .button.seed:hover { background: #0e7490; }
        .status {
            margin-top: 20px;
            padding: 16px;
            background: #1a1a1a;
            border-radius: 8px;
            border-left: 4px solid #3b82f6;
        }
        .success { border-left-color: #10b981; color: #10b981; }
        .error { border-left-color: #ef4444; color: #ef4444; }
        a { color: #60a5fa; text-decoration: none; }
        a:hover { text-decoration: underline; }
        .info {
            background: #1a1a1a;
            padding: 16px;
            border-radius: 8px;
            margin: 20px 0;
        }
    </style>
</head>
<body>
    <h1>🚀 Seesaw Demo</h1>
    <div class="info">
        <p>This is a live demo of the Seesaw event-driven runtime with in-memory store.</p>
        <p>Create normal/slow/batch+join/failing workflows and watch them in the <a href="/">Insights Dashboard →</a></p>
    </div>

    <button class="button" onclick="createOrder('normal')">Create Normal</button>
    <button class="button slow" onclick="createOrder('slow')">Create Slow</button>
    <button class="button seed" onclick="createOrder('batch_join')">Create Batch + Join</button>
    <button class="button fail" onclick="createOrder('fail_payment')">Fail Payment (DLQ)</button>
    <button class="button fail" onclick="createOrder('fail_inventory')">Fail Inventory (DLQ)</button>
    <button class="button fail" onclick="createOrder('fail_shipping')">Fail Shipping (DLQ)</button>
    <button class="button fail" onclick="createOrder('fail_email')">Fail Email (DLQ)</button>
    <button class="button seed" onclick="seedScenarios()">Seed All Scenarios</button>
    <button class="button seed" onclick="seedFailures()">Seed Failures Burst</button>
    <div id="status"></div>

    <script>
        async function createOrder(mode) {
            const status = document.getElementById('status');

            try {
                const response = await fetch(`/api/create-order?mode=${mode}`, {
                    method: 'POST',
                    headers: { 'Content-Type': 'application/json' }
                });

                const data = await response.json();

                status.className = 'status success';
                status.innerHTML = `✅ Order created!<br>
                    Order ID: <code>${data.order_id}</code><br>
                    Mode: <code>${data.mode}</code><br>
                    Total: $${data.total.toFixed(2)}<br>
                    <a href="/">View in Insights Dashboard →</a>`;
            } catch (error) {
                status.className = 'status error';
                status.textContent = '❌ Failed to create order: ' + error.message;
            }
        }

        async function seedScenarios() {
            const status = document.getElementById('status');

            try {
                const response = await fetch('/api/seed-scenarios', { method: 'POST' });
                const data = await response.json();

                status.className = 'status success';
                status.innerHTML = `✅ Seeded ${data.created.length} workflows<br>
                    <a href="/">Open dashboard and inspect failures/bottlenecks →</a>`;
            } catch (error) {
                status.className = 'status error';
                status.textContent = '❌ Failed to seed scenarios: ' + error.message;
            }
        }

        async function seedFailures() {
            const status = document.getElementById('status');

            try {
                const response = await fetch('/api/seed-failures?count=8', { method: 'POST' });
                const data = await response.json();

                status.className = 'status success';
                status.innerHTML = `✅ Seeded ${data.count} failing workflows<br>
                    Modes: <code>${data.modes.join(', ')}</code><br>
                    <a href="/">Open dashboard and toggle Failed / Dead filters →</a>`;
            } catch (error) {
                status.className = 'status error';
                status.textContent = '❌ Failed to seed failures: ' + error.message;
            }
        }
    </script>
</body>
</html>
    "#)
}

#[tokio::main]
async fn main() -> Result<()> {
    // Setup tracing
    tracing_subscriber::fmt()
        .with_target(false)
        .with_level(true)
        .init();

    println!("🚀 Seesaw Insight Demo Starting...\n");
    println!("📝 Using in-memory store (no database required)\n");

    // Create in-memory store
    let store = MemoryStore::new();
    let deps = Deps;

    let engine = Engine::new(deps, store.clone())
        .with_reducer(
            reducer::fold::<OrderEvent>().into_queue(|state: OrderState, event| match event {
                OrderEvent::OrderValidated { .. } => OrderState {
                    validated: true,
                    ..state
                },
                OrderEvent::PaymentProcessed { .. } => OrderState {
                    payment_processed: true,
                    ..state
                },
                OrderEvent::InventoryReserved { .. } => OrderState {
                    inventory_reserved: true,
                    ..state
                },
                OrderEvent::OrderShipped { .. } => OrderState {
                    shipped: true,
                    ..state
                },
                _ => state,
            }),
        )
        .with_reducer(
            reducer::fold::<BatchWorkItem>().into_queue(|state: OrderState, _event| OrderState {
                batch_items_total: state.batch_items_total + 1,
                ..state
            }),
        )
        .with_reducer(
            reducer::fold::<BatchWorkResult>().into_queue(|state: OrderState, event| OrderState {
                batch_items_done: state.batch_items_done + 1,
                batch_failed: state.batch_failed + usize::from(!event.success),
                ..state
            }),
        )
        .with_reducer(
            reducer::fold::<BatchJoinReady>().into_queue(|state: OrderState, _event| OrderState {
                batch_joined: true,
                ..state
            }),
        )
        // Effect: Validate order
        .with_effect(
            effect::on::<OrderEvent>()
                .extract(|e| match e {
                    OrderEvent::OrderPlaced {
                        order_id,
                        total,
                        mode,
                        ..
                    } => Some((*order_id, *total, *mode)),
                    _ => None,
                })
                .id("validate_order")
                .then_queue(|(order_id, total, mode), _ctx| async move {
                    let wait = if mode == DemoMode::Slow { 900 } else { 350 };
                    tokio::time::sleep(Duration::from_millis(wait)).await;
                    Ok(OrderEvent::OrderValidated {
                        order_id,
                        total,
                        mode,
                    })
                }),
        )
        // Effect: Fan out batch items for same-batch join demo
        .with_effect(
            effect::on::<OrderEvent>()
                .extract(|e| match e {
                    OrderEvent::OrderValidated { order_id, mode, .. }
                        if *mode == DemoMode::BatchJoin =>
                    {
                        Some(*order_id)
                    }
                    _ => None,
                })
                .id("fan_out_batch_items")
                .then_queue::<OrderState, Deps, Uuid, _, _, _, BatchWorkItem>(
                    |order_id, _ctx| async move {
                    tokio::time::sleep(Duration::from_millis(260)).await;

                    let item_count = 6usize;
                    let items: Vec<_> = (0..item_count)
                        .map(|item_index| BatchWorkItem {
                            order_id,
                            item_index,
                        })
                        .collect();

                        Ok(items)
                    },
                ),
        )
        // Effect: Process each batch item independently
        .with_effect(
            effect::on::<BatchWorkItem>()
                .id("process_batch_item")
                .retry(2)
                .then_queue(
                    |item: Arc<BatchWorkItem>, _ctx: seesaw_core::EffectContext<OrderState, Deps>| async move {
                        tokio::time::sleep(Duration::from_millis(220 + (item.item_index as u64 * 70)))
                            .await;

                        let success = (item.item_index + 1) % 4 != 0;
                        Ok(BatchWorkResult {
                            order_id: item.order_id,
                            item_index: item.item_index,
                            success,
                            message: if success {
                                format!("item {} settled", item.item_index)
                            } else {
                                format!("item {} failed validation", item.item_index)
                            },
                        })
                    },
                ),
        )
        // Effect: Join all results for one emitted batch
        .with_effect(
            effect::on::<BatchWorkResult>()
                .id("join_batch_items")
                .join()
                .same_batch()
                .then(
                    |items: Vec<BatchWorkResult>, _ctx: seesaw_core::EffectContext<OrderState, Deps>| async move {
                        let Some(order_id) = items.first().map(|item| item.order_id) else {
                            anyhow::bail!("join_batch_items received empty batch");
                        };

                        let failed = items.iter().filter(|item| !item.success).count();
                        let total = items.len();
                        let succeeded = total.saturating_sub(failed);

                        Ok(BatchJoinReady {
                            order_id,
                            total,
                            succeeded,
                            failed,
                        })
                    },
                ),
        )
        // Effect: Convert joined summary back into domain completion/failure
        .with_effect(
            effect::on::<BatchJoinReady>()
                .id("finalize_batch_join")
                .then_queue(
                    |summary: Arc<BatchJoinReady>, _ctx: seesaw_core::EffectContext<OrderState, Deps>| async move {
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        if summary.failed > 0 {
                            Ok(OrderEvent::OrderFailed {
                                order_id: summary.order_id,
                                reason: format!(
                                    "batch join completed with {} failed items out of {}",
                                    summary.failed, summary.total
                                ),
                                mode: DemoMode::BatchJoin,
                            })
                        } else {
                            Ok(OrderEvent::OrderCompleted {
                                order_id: summary.order_id,
                                mode: DemoMode::BatchJoin,
                            })
                        }
                    },
                ),
        )
        // Effect: Process payment
        .with_effect(
            effect::on::<OrderEvent>()
                .extract(|e| match e {
                    OrderEvent::OrderValidated {
                        order_id,
                        total,
                        mode,
                    } if *mode != DemoMode::BatchJoin => Some((*order_id, *total, *mode)),
                    _ => None,
                })
                .id("process_payment")
                .retry(2)
                .timeout(Duration::from_secs(5))
                .then_queue(|(order_id, total, mode), _ctx| async move {
                    if mode == DemoMode::FailPayment {
                        anyhow::bail!("Payment gateway timeout - simulated permanent failure");
                    }
                    let wait = if mode == DemoMode::Slow { 2600 } else { 750 };
                    tokio::time::sleep(Duration::from_millis(wait)).await;
                    Ok(OrderEvent::PaymentProcessed {
                        order_id,
                        amount: total,
                        mode,
                    })
                }),
        )
        // Effect: Reserve inventory
        .with_effect(
            effect::on::<OrderEvent>()
                .extract(|e| match e {
                    OrderEvent::PaymentProcessed {
                        order_id,
                        amount,
                        mode,
                    } => Some((*order_id, *amount, *mode)),
                    _ => None,
                })
                .id("reserve_inventory")
                .priority(1)
                .then_queue(|(order_id, amount, mode), _ctx| async move {
                    if mode == DemoMode::FailInventory {
                        anyhow::bail!("Inventory reservation conflict - simulated stock failure");
                    }
                    let wait = if mode == DemoMode::Slow { 2100 } else { 520 };
                    tokio::time::sleep(Duration::from_millis(wait)).await;
                    Ok(OrderEvent::InventoryReserved {
                        order_id,
                        amount,
                        mode,
                    })
                }),
        )
        // Effect: Ship order
        .with_effect(
            effect::on::<OrderEvent>()
                .extract(|e| match e {
                    OrderEvent::InventoryReserved {
                        order_id,
                        mode,
                        ..
                    } => Some((*order_id, *mode)),
                    _ => None,
                })
                .id("ship_order")
                .retry(2)
                .then_queue(|(order_id, mode), _ctx| async move {
                    if mode == DemoMode::FailShipping {
                        anyhow::bail!("Carrier API unavailable - simulated shipping failure");
                    }
                    let wait = if mode == DemoMode::Slow { 2800 } else { 680 };
                    tokio::time::sleep(Duration::from_millis(wait)).await;
                    Ok(OrderEvent::OrderShipped {
                        order_id,
                        tracking_number: format!(
                            "TRACK-{}",
                            order_id.to_string()[..8].to_uppercase()
                        ),
                        mode,
                    })
                }),
        )
        // Effect: Send confirmation email
        .with_effect(
            effect::on::<OrderEvent>()
                .extract(|e| match e {
                    OrderEvent::OrderShipped {
                        order_id,
                        mode,
                        ..
                    } => Some((*order_id, *mode)),
                    _ => None,
                })
                .id("send_confirmation_email")
                .retry(2)
                .then_queue(|(order_id, mode), _ctx| async move {
                    if mode == DemoMode::FailEmail {
                        anyhow::bail!("Email provider throttled - simulated notification failure");
                    }
                    tokio::time::sleep(Duration::from_millis(400)).await;
                    Ok(OrderEvent::EmailSent {
                        order_id,
                        email_type: "shipping_confirmation".to_string(),
                        mode,
                    })
                }),
        )
        // Effect: Complete order
        .with_effect(
            effect::on::<OrderEvent>()
                .extract(|e| match e {
                    OrderEvent::EmailSent {
                        order_id,
                        mode,
                        ..
                    } => Some((*order_id, *mode)),
                    _ => None,
                })
                .id("complete_order")
                .then_queue(|(order_id, mode), _ctx| async move {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    Ok(OrderEvent::OrderCompleted { order_id, mode })
                }),
        );

    println!("✅ Engine configured with 10 effects (includes batch + join flow)");

    // Wrap engine in Arc for sharing across threads
    let engine = Arc::new(engine);

    // Start runtime workers
    let runtime = Runtime::start(
        &engine,
        RuntimeConfig {
            event_workers: 2,
            effect_workers: 4,
            ..Default::default()
        },
    );

    println!("✅ Runtime started (2 event workers, 4 effect workers)\n");
    println!("🎬 Generating demo workflows...\n");

    // Generate demo workflows
    let startup_scenarios = [
        (1usize, DemoMode::Normal),
        (2usize, DemoMode::Slow),
        (3usize, DemoMode::BatchJoin),
        (4usize, DemoMode::FailPayment),
        (5usize, DemoMode::FailInventory),
        (6usize, DemoMode::FailShipping),
        (7usize, DemoMode::FailEmail),
    ];
    for (i, mode) in startup_scenarios {
        let order_id = Uuid::new_v4();
        let customer_id = Uuid::new_v4();

        println!(
            "📦 Workflow {}: Order {} starting ({:?})",
            i,
            &order_id.to_string()[..8],
            mode
        );

        engine
            .process(OrderEvent::OrderPlaced {
                order_id,
                customer_id,
                total: 99.99 * i as f64,
                mode,
            })
            .await?;

        // Stagger workflow starts
        tokio::time::sleep(Duration::from_millis(900)).await;
    }

    println!("\n✨ Demo workflows initiated!");
    println!("\n📊 Open http://localhost:3000 to see them in action!\n");
    // Build unified server with proper state type handling
    let static_dir = Some("/Users/crcn/Developer/crcn/seesaw-rs/crates/seesaw-insight/static");

    // Create insights app at root (dashboard uses /assets which requires root mounting)
    let insights_app = seesaw_insight::web::app(store.clone(), static_dir, None);

    // Create demo routes with their own state
    let demo_routes = Router::new()
        .route("/demo", get(toy_app))
        .route("/api/create-order", post(create_order))
        .route("/api/seed-scenarios", post(seed_scenarios))
        .route("/api/seed-failures", post(seed_failures))
        .with_state(engine.clone());

    // Merge both apps (they have different state types, but that's ok with merge)
    let app = Router::new()
        .merge(demo_routes)
        .merge(insights_app)
        .layer(CorsLayer::permissive());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000")
        .await
        .expect("Failed to bind port 3000");

    println!("\n📊 Server started on http://localhost:3000");
    println!("   Insights:  http://localhost:3000/ (main dashboard)");
    println!("   Toy App:   http://localhost:3000/demo");
    println!("   API:       POST http://localhost:3000/api/create-order\n");
    println!("Features to try:");
    println!("  • Create normal, slow, batch+join, and failing orders from /demo");
    println!("  • Use /api/seed-failures for burst DLQ/failed-flow simulation");
    println!("  • Watch batch chips + join waiting/completion in the flow canvas at /");
    println!("  • Watch pinned events, effect logs, and payloads at /");
    println!("  • Inspect dead letters and failed flows in left rail");
    println!("  • Open bottlenecks panel to spot slow effects\n");
    println!("Press Ctrl+C to stop...\n");

    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("Server failed");
    });

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Keep running
    tokio::signal::ctrl_c().await?;
    println!("\n🛑 Shutting down gracefully...");

    let _ = runtime.shutdown().await;
    println!("✅ Runtime stopped");

    Ok(())
}
