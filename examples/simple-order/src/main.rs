//! Simple Order Processing Example

use anyhow::Result;
use seesaw_core::{effect, Engine, HandlerContext};
use seesaw_memory::MemoryStore;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
enum OrderEvent {
    Placed { order_id: Uuid, total: f64 },
    Shipped { order_id: Uuid },
    Delivered { order_id: Uuid },
}

#[derive(Clone)]
struct Deps {
    shipping_enabled: bool,
}

impl Deps {
    async fn ship(&self, order_id: Uuid) -> Result<()> {
        if self.shipping_enabled {
            println!("shipping order {}", order_id);
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        }
        Ok(())
    }

    async fn notify(&self, order_id: Uuid, message: &str) -> Result<()> {
        println!("notify order {}: {}", order_id, message);
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        Ok(())
    }
}

fn place_order(order_id: Uuid, total: f64) -> OrderEvent {
    OrderEvent::Placed { order_id, total }
}

#[tokio::main]
async fn main() -> Result<()> {
    let store = MemoryStore::new();
    let deps = Deps {
        shipping_enabled: true,
    };

    let engine = Engine::new(deps, store)
        .with_handler(
            handler::on::<OrderEvent>()
                .extract(|e| match e {
                    OrderEvent::Placed { order_id, .. } => Some(*order_id),
                    _ => None,
                })
                .then(|order_id, ctx: HandlerContext<Deps>| async move {
                    ctx.deps().ship(order_id).await?;
                    Ok(OrderEvent::Shipped { order_id })
                }),
        )
        .with_handler(
            handler::on::<OrderEvent>()
                .extract(|e| match e {
                    OrderEvent::Shipped { order_id } => Some(*order_id),
                    _ => None,
                })
                .then(|order_id, ctx: HandlerContext<Deps>| async move {
                    ctx.deps()
                        .notify(order_id, "your order has shipped")
                        .await?;
                    Ok(OrderEvent::Delivered { order_id })
                }),
        );

    for i in 1..=3 {
        let order_id = Uuid::new_v4();
        let total = 99.99 * i as f64;
        engine.process(place_order(order_id, total)).await?;
    }

    Ok(())
}
