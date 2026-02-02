//! Simple Order Processing Example
//!
//! Demonstrates the new stateless Engine API with:
//! - Closure-based effects and reducers
//! - Per-execution state
//! - Handle::run() pattern
//! - Edge function style

use anyhow::Result;
use seesaw_core::{effect, reducer, Engine, EffectContext};
use uuid::Uuid;

// Events - facts about what happened
#[derive(Debug, Clone)]
enum OrderEvent {
    Placed { order_id: Uuid, total: f64 },
    Shipped { order_id: Uuid },
    Delivered { order_id: Uuid },
}

// State - accumulates as events flow
#[derive(Clone, Default, Debug)]
struct OrderState {
    total_orders: u32,
    total_revenue: f64,
    last_order_id: Option<Uuid>,
}

// Dependencies - shared services
#[derive(Clone)]
struct Deps {
    shipping_enabled: bool,
}

impl Deps {
    async fn ship(&self, order_id: Uuid) -> Result<()> {
        if self.shipping_enabled {
            println!("📦 Shipping order {}", order_id);
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        }
        Ok(())
    }

    async fn notify(&self, order_id: Uuid, message: &str) -> Result<()> {
        println!("📧 Notifying customer about order {}: {}", order_id, message);
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        Ok(())
    }
}

// Edge function - simple signature
fn place_order(
    order_id: Uuid,
    total: f64,
    ctx: &EffectContext<OrderState, Deps>,
) -> Result<Uuid> {
    ctx.emit(OrderEvent::Placed { order_id, total });
    Ok(order_id)
}

#[tokio::main]
async fn main() -> Result<()> {
    println!("🚀 Simple Order Processing Example\n");

    // 1. Define engine once (stateless, reusable)
    let engine = Engine::with_deps(Deps {
        shipping_enabled: true,
    })
    // Reducer - pure state transformation
    .with_reducer(reducer::on::<OrderEvent>().run(|state: OrderState, event| {
        match event {
            OrderEvent::Placed { order_id, total } => OrderState {
                total_orders: state.total_orders + 1,
                total_revenue: state.total_revenue + total,
                last_order_id: Some(*order_id),
            },
            _ => state,
        }
    }))
    // Effect - ship when order placed
    .with_effect(effect::on::<OrderEvent>().run(|event, ctx: EffectContext<OrderState, Deps>| async move {
        if let OrderEvent::Placed { order_id, .. } = event.as_ref() {
            ctx.deps().ship(*order_id).await?;
            ctx.emit(OrderEvent::Shipped {
                order_id: *order_id,
            });
        }
        Ok(())
    }))
    // Effect - notify when shipped
    .with_effect(effect::on::<OrderEvent>().run(|event, ctx: EffectContext<OrderState, Deps>| async move {
        if let OrderEvent::Shipped { order_id } = event.as_ref() {
            ctx.deps().notify(*order_id, "Your order has shipped!").await?;
            ctx.emit(OrderEvent::Delivered {
                order_id: *order_id,
            });
        }
        Ok(())
    }));

    println!("✓ Engine configured with effects and reducers\n");

    // 2. Execute multiple orders (engine is reusable)
    for i in 1..=3 {
        println!("--- Order #{} ---", i);

        // Activate with initial state
        let handle = engine.activate(OrderState::default());

        // Run the edge function
        let order_id = Uuid::new_v4();
        let result = handle.run(|ctx| place_order(order_id, 99.99 * i as f64, ctx))?;

        println!("✓ Order {} placed", result);

        // Wait for all effects to complete
        handle.settled().await?;

        // Check final state
        let final_state = handle.context.curr_state();
        println!(
            "✓ State: {} orders, ${:.2} revenue\n",
            final_state.total_orders, final_state.total_revenue
        );
    }

    println!("🎉 All orders processed successfully!");

    Ok(())
}
