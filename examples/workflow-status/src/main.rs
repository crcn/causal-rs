//! Workflow Status Tracking Example
//!
//! Shows how to:
//! 1. Start a workflow and get correlation_id
//! 2. Check workflow status (is_settled)
//! 3. Wait for terminal events (true completion)

use anyhow::Result;
use seesaw_core::{effect, reducer, EffectContext, QueueEngine, Store, WorkflowStatus};
use seesaw_postgres::PostgresStore;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::time::Duration;
use uuid::Uuid;

// Events
#[derive(Debug, Clone, Serialize, Deserialize)]
enum OrderEvent {
    OrderPlaced { order_id: Uuid, total: f64 },
    PaymentProcessing { order_id: Uuid },
    PaymentCharged { order_id: Uuid },
    InventoryReserved { order_id: Uuid },
    ShipmentScheduled { order_id: Uuid },

    // Terminal events - signal true completion
    OrderCompleted { order_id: Uuid },
    OrderFailed { order_id: Uuid, reason: String },
}

// State
#[derive(Clone, Default, Serialize, Deserialize)]
struct OrderState {
    order_id: Option<Uuid>,
    status: String,
    steps_completed: Vec<String>,
}

// Dependencies
#[derive(Clone)]
struct Deps {
    payment_service: PaymentService,
    inventory_service: InventoryService,
}

#[derive(Clone)]
struct PaymentService;
impl PaymentService {
    async fn charge(&self, total: f64) -> Result<()> {
        println!("💳 Charging ${:.2}...", total);
        tokio::time::sleep(Duration::from_millis(500)).await;
        Ok(())
    }
}

#[derive(Clone)]
struct InventoryService;
impl InventoryService {
    async fn reserve(&self, order_id: Uuid) -> Result<()> {
        println!("📦 Reserving inventory for order {}...", order_id);
        tokio::time::sleep(Duration::from_millis(300)).await;
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Setup database
    let database_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://postgres:postgres@localhost/seesaw".to_string());
    let pool = PgPool::connect(&database_url).await?;

    // Apply schema (you'd normally do this once)
    let schema = include_str!("../../../docs/schema.sql");
    sqlx::raw_sql(schema).execute(&pool).await.ok();

    let store = PostgresStore::new(pool.clone());

    // Create engine
    let deps = Deps {
        payment_service: PaymentService,
        inventory_service: InventoryService,
    };

    let engine = QueueEngine::new(deps.clone(), store.clone())
        // Reducer - track state
        .with_reducer(
            reducer::fold::<OrderEvent>().into(|mut state: OrderState, event| {
                match event {
                    OrderEvent::OrderPlaced { order_id, .. } => {
                        state.order_id = Some(*order_id);
                        state.status = "placed".to_string();
                        state.steps_completed.push("placed".to_string());
                    }
                    OrderEvent::PaymentCharged { .. } => {
                        state.status = "payment_charged".to_string();
                        state.steps_completed.push("payment".to_string());
                    }
                    OrderEvent::InventoryReserved { .. } => {
                        state.status = "inventory_reserved".to_string();
                        state.steps_completed.push("inventory".to_string());
                    }
                    OrderEvent::OrderCompleted { .. } => {
                        state.status = "completed".to_string();
                        state.steps_completed.push("completed".to_string());
                    }
                    OrderEvent::OrderFailed { reason, .. } => {
                        state.status = format!("failed: {}", reason);
                    }
                    _ => {}
                }
                state
            }),
        )
        // Effect 1: Process payment
        .with_effect(
            effect::on::<OrderEvent>()
                .extract(|e| match e {
                    OrderEvent::OrderPlaced { order_id, total } => Some((*order_id, *total)),
                    _ => None,
                })
                .queued() // Run in background
                .retry(3)
                .then(
                    |(order_id, total), ctx: EffectContext<OrderState, Deps>| async move {
                        ctx.deps().payment_service.charge(total).await?;
                        Ok(OrderEvent::PaymentCharged { order_id })
                    },
                ),
        )
        // Effect 2: Reserve inventory
        .with_effect(
            effect::on::<OrderEvent>()
                .extract(|e| match e {
                    OrderEvent::PaymentCharged { order_id } => Some(*order_id),
                    _ => None,
                })
                .queued()
                .then(
                    |order_id, ctx: EffectContext<OrderState, Deps>| async move {
                        ctx.deps().inventory_service.reserve(order_id).await?;
                        Ok(OrderEvent::InventoryReserved { order_id })
                    },
                ),
        )
        // Effect 3: Mark complete (terminal event)
        .with_effect(
            effect::on::<OrderEvent>()
                .extract(|e| match e {
                    OrderEvent::InventoryReserved { order_id } => Some(*order_id),
                    _ => None,
                })
                .then(
                    |order_id, _ctx: EffectContext<OrderState, Deps>| async move {
                        println!("✅ Order complete!");
                        Ok(OrderEvent::OrderCompleted { order_id })
                    },
                ),
        );

    println!("🚀 Workflow Status Tracking Example\n");

    // =================================================================
    // Pattern 1: Fire and forget - get correlation_id
    // =================================================================
    println!("📍 Pattern 1: Fire and forget\n");

    let order_id = Uuid::new_v4();
    let handle = engine
        .process(OrderEvent::OrderPlaced {
            order_id,
            total: 99.99,
        })
        .await?;

    let correlation_id = handle.correlation_id;
    println!(
        "✓ Started workflow with correlation_id: {}\n",
        correlation_id
    );

    // =================================================================
    // Pattern 2: Poll workflow status
    // =================================================================
    println!("📍 Pattern 2: Poll workflow status\n");

    // Poll status a few times
    for i in 1..=5 {
        tokio::time::sleep(Duration::from_millis(200)).await;

        let status = store.get_workflow_status(correlation_id).await?;
        print_status(&status, i);

        if status.is_settled {
            println!("  → Workflow is settled (no effects running)\n");
            break;
        }
    }

    // =================================================================
    // Pattern 3: Wait for terminal event (true completion)
    // =================================================================
    println!("📍 Pattern 3: Wait for terminal event\n");

    let order_id_2 = Uuid::new_v4();
    let result = engine
        .process(OrderEvent::OrderPlaced {
            order_id: order_id_2,
            total: 149.99,
        })
        .wait(|event| {
            if let Some(evt) = event.downcast_ref::<seesaw_core::WorkflowEvent>() {
                if let Ok(order_event) = serde_json::from_value::<OrderEvent>(evt.payload.clone()) {
                    return match order_event {
                        OrderEvent::OrderCompleted { .. } => {
                            println!("✅ Received terminal event: OrderCompleted");
                            Some(Ok(true))
                        }
                        OrderEvent::OrderFailed { .. } => {
                            println!("❌ Received terminal event: OrderFailed");
                            Some(Err(anyhow::anyhow!("Order failed")))
                        }
                        _ => {
                            println!("   ... still processing: {}", evt.event_type);
                            None
                        }
                    };
                }
            }

            None
        })
        .timeout(Duration::from_secs(10))
        .await?;

    println!("\n✓ Workflow completed: {}", result);

    // =================================================================
    // Compare: settled vs terminal
    // =================================================================
    println!("\n📍 Key Difference:\n");

    let final_status = store.get_workflow_status(correlation_id).await?;
    println!("Status for correlation_id {}:", correlation_id);
    println!(
        "  is_settled: {} (no effects running right now)",
        final_status.is_settled
    );
    println!(
        "  last_event: {:?} (might be terminal event)",
        final_status.last_event
    );
    println!("\n  → is_settled = momentary state (can start up again)");
    println!("  → Terminal event = user declares 'done forever'\n");

    println!("🎉 Example complete!");

    Ok(())
}

fn print_status(status: &WorkflowStatus, poll_num: usize) {
    println!("Poll #{}: ", poll_num);
    println!("  correlation_id: {}", status.correlation_id);
    println!("  pending_effects: {}", status.pending_effects);
    println!("  is_settled: {}", status.is_settled);
    println!("  last_event: {:?}", status.last_event);

    if let Some(state) = &status.state {
        if let Ok(order_state) = serde_json::from_value::<OrderState>(state.clone()) {
            println!("  status: {}", order_state.status);
            println!("  steps: {:?}", order_state.steps_completed);
        }
    }
    println!();
}
