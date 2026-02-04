//! Simple Order Processing Example
//!
//! Demonstrates the new stateless Engine API with:
//! - Closure-based effects with `.then()` returning events
//! - Per-execution state
//! - Handle::run() pattern
//! - Event chains: Placed -> Shipped -> Delivered

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

// Edge function - returns event to dispatch
fn place_order(order_id: Uuid, total: f64) -> OrderEvent {
    OrderEvent::Placed { order_id, total }
}

#[tokio::main]
async fn main() -> Result<()> {
    println!("🚀 Simple Order Processing Example\n");

    // 1. Define engine once (stateless, reusable)
    let engine = Engine::with_deps(Deps {
        shipping_enabled: true,
    })
    // Reducer - pure state transformation
    .with_reducer(reducer::fold::<OrderEvent>().into(|state: OrderState, event| {
        match event {
            OrderEvent::Placed { order_id, total } => OrderState {
                total_orders: state.total_orders + 1,
                total_revenue: state.total_revenue + *total,
                last_order_id: Some(*order_id),
            },
            _ => state,
        }
    }))
    // Effect chain: Placed -> ship -> Shipped
    .with_effect(
        effect::on::<OrderEvent>()
            .extract(|e| match e {
                OrderEvent::Placed { order_id, .. } => Some(*order_id),
                _ => None,
            })
            .then(|order_id, ctx: EffectContext<OrderState, Deps>| async move {
                ctx.deps().ship(order_id).await?;
                Ok(OrderEvent::Shipped { order_id })
            })
    )
    // Effect chain: Shipped -> notify -> Delivered
    .with_effect(
        effect::on::<OrderEvent>()
            .extract(|e| match e {
                OrderEvent::Shipped { order_id } => Some(*order_id),
                _ => None,
            })
            .then(|order_id, ctx: EffectContext<OrderState, Deps>| async move {
                ctx.deps().notify(order_id, "Your order has shipped!").await?;
                Ok(OrderEvent::Delivered { order_id })
            })
    );

    println!("✓ Engine configured with effects and reducers\n");

    // 2. Execute multiple orders (engine is reusable)
    for i in 1..=3 {
        println!("--- Order #{} ---", i);

        // Activate with initial state
        let handle = engine.activate(OrderState::default());

        // Process the edge function - returns event, waits for completion
        let order_id = Uuid::new_v4();
        let total = 99.99 * i as f64;
        handle.process(|_| async move { Ok(place_order(order_id, total)) }).await?;

        println!("✓ Order {} placed", order_id);

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
