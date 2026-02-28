//! Event Sourcing + Handler Layer Example
//!
//! Shows both layers working together:
//! - EventStore: permanent storage + state reconstruction
//! - Engine: reactions via handler system

use anyhow::Result;
use seesaw_core::es::*;
use seesaw_core::{handler, Context, Engine};
use seesaw_memory::event_store::MemoryEventStore;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// -- Domain events --

#[derive(Debug, Clone, Serialize, Deserialize)]
enum OrderEvent {
    Placed { order_id: Uuid, total: f64 },
    Shipped { order_id: Uuid },
    Confirmed { order_id: Uuid },
}

impl EventUpcast for OrderEvent {}

// -- Aggregate (ES layer) --

#[derive(Debug, Default)]
struct Order {
    status: OrderStatus,
    total: f64,
}

#[derive(Debug, Default, PartialEq)]
enum OrderStatus {
    #[default]
    Draft,
    Placed,
    Shipped,
}

impl Aggregate for Order {
    type Event = OrderEvent;

    fn aggregate_type() -> &'static str {
        "Order"
    }

    fn apply(&mut self, event: OrderEvent) {
        match event {
            OrderEvent::Placed { total, .. } => {
                self.status = OrderStatus::Placed;
                self.total = total;
            }
            OrderEvent::Shipped { .. } => {
                self.status = OrderStatus::Shipped;
            }
            OrderEvent::Confirmed { .. } => {}
        }
    }
}

// -- Dependencies --

#[derive(Clone)]
struct Deps;

impl Deps {
    async fn ship(&self, order_id: Uuid) -> Result<()> {
        println!("  [shipping] Order {} sent to warehouse", order_id);
        Ok(())
    }

    async fn send_email(&self, order_id: Uuid, msg: &str) -> Result<()> {
        println!("  [email] Order {}: {}", order_id, msg);
        Ok(())
    }
}

// -- Main --

#[tokio::main]
async fn main() -> Result<()> {
    // === ES layer: permanent event storage ===
    let event_store = MemoryEventStore::new();

    // === Handler layer: reactions ===
    let engine = Engine::new(Deps)
        .with_handler(
            handler::on::<OrderEvent>()
                .id("ship_order")
                .extract(|e| match e {
                    OrderEvent::Placed { order_id, .. } => Some(*order_id),
                    _ => None,
                })
                .then(|order_id, ctx: Context<Deps>| async move {
                    ctx.deps().ship(order_id).await?;
                    Ok(OrderEvent::Shipped { order_id })
                }),
        )
        .with_handler(
            handler::on::<OrderEvent>()
                .id("send_confirmation")
                .extract(|e| match e {
                    OrderEvent::Placed { order_id, .. } => Some(*order_id),
                    _ => None,
                })
                .then(|order_id, ctx: Context<Deps>| async move {
                    ctx.deps()
                        .send_email(order_id, "Your order has been placed!")
                        .await?;
                    Ok(())
                }),
        );

    // === Use both layers together ===
    let order_id = Uuid::new_v4();

    // 1. Load current state (empty -- brand new aggregate)
    let order: Versioned<Order> = event_store.aggregate(order_id).load().await?;
    assert_eq!(order.status, OrderStatus::Draft);
    println!(
        "Order {} loaded: {:?} (version {})",
        order_id,
        order.status,
        order.version()
    );

    // 2. Append domain events to permanent store
    let events = vec![OrderEvent::Placed {
        order_id,
        total: 149.99,
    }];

    let new_version = event_store
        .aggregate::<Order>(order_id)
        .append(order.version(), events.clone())
        .await?;
    println!("Appended events, new version: {}", new_version);

    // 3. Dispatch to handler layer -- triggers reactions
    for event in &events {
        engine.dispatch(event.clone()).await?;
    }

    // 4. Verify state reconstruction
    let order: Versioned<Order> = event_store.aggregate(order_id).load().await?;
    println!(
        "Order {} reconstructed: {:?}, total=${:.2} (version {})",
        order_id,
        order.status,
        order.total,
        order.version()
    );
    assert_eq!(order.status, OrderStatus::Placed);

    // 5. Demonstrate optimistic concurrency
    println!("\nSimulating concurrent writes...");
    let stale_version = 0; // pretend we loaded this earlier
    let result = event_store
        .aggregate::<Order>(order_id)
        .append(stale_version, vec![OrderEvent::Shipped { order_id }])
        .await;

    match result {
        Err(e) if e.downcast_ref::<ConcurrencyError>().is_some() => {
            let conflict = e.downcast_ref::<ConcurrencyError>().unwrap();
            println!(
                "Caught concurrency conflict: expected v{}, actual v{}",
                conflict.expected, conflict.actual
            );
        }
        other => panic!("Expected ConcurrencyError, got: {:?}", other),
    }

    // 6. Correct append with right version
    event_store
        .aggregate::<Order>(order_id)
        .append(new_version, vec![OrderEvent::Shipped { order_id }])
        .await?;

    let final_order: Versioned<Order> = event_store.aggregate(order_id).load().await?;
    println!(
        "Final state: {:?} (version {})",
        final_order.status,
        final_order.version()
    );
    assert_eq!(final_order.status, OrderStatus::Shipped);

    println!("\nDone! Both layers compose independently.");
    Ok(())
}
