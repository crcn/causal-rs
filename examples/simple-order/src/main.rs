//! Simple Order Processing Example

use anyhow::Result;
use seesaw_core::{event, handles, Context, Engine};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[event]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct OrderPlaced {
    order_id: Uuid,
    total: f64,
}

#[event]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct OrderShipped {
    order_id: Uuid,
}

#[event]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct OrderDelivered {
    order_id: Uuid,
}

#[derive(Clone)]
struct Deps {
    shipping_enabled: bool,
}

impl Deps {
    async fn ship(&self, order_id: Uuid) -> Result<()> {
        if self.shipping_enabled {
            println!("Shipping order {}", order_id);
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        }
        Ok(())
    }

    async fn notify(&self, order_id: Uuid, message: &str) -> Result<()> {
        println!("Notify order {}: {}", order_id, message);
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        Ok(())
    }
}

#[handles]
mod order_handlers {
    use super::*;

    async fn ship_order(event: OrderPlaced, ctx: Context<Deps>) -> Result<OrderShipped> {
        ctx.deps().ship(event.order_id).await?;
        Ok(OrderShipped { order_id: event.order_id })
    }

    async fn notify_shipped(event: OrderShipped, ctx: Context<Deps>) -> Result<OrderDelivered> {
        ctx.deps()
            .notify(event.order_id, "your order has shipped")
            .await?;
        Ok(OrderDelivered { order_id: event.order_id })
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let deps = Deps {
        shipping_enabled: true,
    };

    let engine = Engine::in_memory(deps).with_handlers(order_handlers::handles());

    println!("Processing orders...\n");

    for i in 1..=3 {
        let order_id = Uuid::new_v4();
        let total = 99.99 * i as f64;
        println!("Placing order {} (${:.2})", order_id, total);
        engine.emit(OrderPlaced { order_id, total }).settled().await?;
    }

    println!("\nAll orders processed successfully!");
    Ok(())
}
