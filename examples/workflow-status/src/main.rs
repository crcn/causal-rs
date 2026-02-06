//! Workflow Status Tracking Example (stateless)

use anyhow::Result;
use seesaw_core::{effect, Engine, HandlerContext, Store, WorkflowStatus};
use seesaw_postgres::PostgresStore;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::time::Duration;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
enum OrderEvent {
    OrderPlaced { order_id: Uuid, total: f64 },
    PaymentCharged { order_id: Uuid },
    InventoryReserved { order_id: Uuid },
    OrderCompleted { order_id: Uuid },
    OrderFailed { order_id: Uuid, reason: String },
}

#[derive(Clone)]
struct Deps {
    payment_service: PaymentService,
    inventory_service: InventoryService,
}

#[derive(Clone)]
struct PaymentService;
impl PaymentService {
    async fn charge(&self, total: f64) -> Result<()> {
        println!("charging ${:.2}", total);
        tokio::time::sleep(Duration::from_millis(500)).await;
        Ok(())
    }
}

#[derive(Clone)]
struct InventoryService;
impl InventoryService {
    async fn reserve(&self, order_id: Uuid) -> Result<()> {
        println!("reserving inventory for {}", order_id);
        tokio::time::sleep(Duration::from_millis(300)).await;
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let database_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://postgres:postgres@localhost/seesaw".to_string());
    let pool = PgPool::connect(&database_url).await?;

    let schema = include_str!("../../../docs/schema.sql");
    sqlx::raw_sql(schema).execute(&pool).await.ok();

    let store = PostgresStore::new(pool.clone());
    let deps = Deps {
        payment_service: PaymentService,
        inventory_service: InventoryService,
    };

    let engine = Engine::new(deps.clone(), store.clone())
        .with_handler(
            handler::on::<OrderEvent>()
                .extract(|e| match e {
                    OrderEvent::OrderPlaced { order_id, total } => Some((*order_id, *total)),
                    _ => None,
                })
                .queued()
                .retry(3)
                .then(|(order_id, total), ctx: HandlerContext<Deps>| async move {
                    ctx.deps().payment_service.charge(total).await?;
                    Ok(OrderEvent::PaymentCharged { order_id })
                }),
        )
        .with_handler(
            handler::on::<OrderEvent>()
                .extract(|e| match e {
                    OrderEvent::PaymentCharged { order_id } => Some(*order_id),
                    _ => None,
                })
                .queued()
                .then(|order_id, ctx: HandlerContext<Deps>| async move {
                    ctx.deps().inventory_service.reserve(order_id).await?;
                    Ok(OrderEvent::InventoryReserved { order_id })
                }),
        )
        .with_handler(
            handler::on::<OrderEvent>()
                .extract(|e| match e {
                    OrderEvent::InventoryReserved { order_id } => Some(*order_id),
                    _ => None,
                })
                .then(|order_id, _ctx: HandlerContext<Deps>| async move {
                    Ok(OrderEvent::OrderCompleted { order_id })
                }),
        );

    let order_id = Uuid::new_v4();
    let handle = engine
        .process(OrderEvent::OrderPlaced {
            order_id,
            total: 99.99,
        })
        .await?;

    let correlation_id = handle.correlation_id;

    for i in 1..=5 {
        tokio::time::sleep(Duration::from_millis(200)).await;

        let status = store.get_workflow_status(correlation_id).await?;
        print_status(&status, i);

        if status.is_settled {
            break;
        }
    }

    let order_id_2 = Uuid::new_v4();
    let _result = engine
        .process(OrderEvent::OrderPlaced {
            order_id: order_id_2,
            total: 149.99,
        })
        .wait(|event| {
            if let Some(evt) = event.downcast_ref::<seesaw_core::WorkflowEvent>() {
                if let Ok(order_event) = serde_json::from_value::<OrderEvent>(evt.payload.clone()) {
                    return match order_event {
                        OrderEvent::OrderCompleted { .. } => Some(Ok(true)),
                        OrderEvent::OrderFailed { .. } => {
                            Some(Err(anyhow::anyhow!("order failed")))
                        }
                        _ => None,
                    };
                }
            }
            None
        })
        .timeout(Duration::from_secs(10))
        .await?;

    Ok(())
}

fn print_status(status: &WorkflowStatus, poll_num: usize) {
    println!("poll #{}", poll_num);
    println!("  correlation_id: {}", status.correlation_id);
    println!("  pending_effects: {}", status.pending_effects);
    println!("  is_settled: {}", status.is_settled);
    println!("  last_event: {:?}", status.last_event);
}
