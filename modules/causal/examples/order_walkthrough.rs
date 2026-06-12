//! The README walkthrough, as a runnable example — keeps the docs honest.
//!
//! Run: `cargo run -p causal --example order_walkthrough`
//!
//! An order is placed, a reactor requests its shipment, and an aggregate
//! folds both events into queryable state.

use causal::{
    Aggregate, Aggregator, Apply, CheckpointStore, Ctx, EngineBuilder, Event,
    EventLogBackend, Events, MemoryStore, Reactor, ReactorCheckpoint,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;

// 1. Events are plain data. `CATEGORY` groups a type's streams;
//    `subject_id` picks which stream this value belongs to.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct OrderPlaced {
    order_id: Uuid,
    total: f64,
}
impl Event for OrderPlaced {
    const NAME: &'static str = "placed";
    fn subject_id(&self) -> Uuid {
        self.order_id
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ShipmentRequested {
    order_id: Uuid,
}
impl Event for ShipmentRequested {
    const NAME: &'static str = "requested";
    fn subject_id(&self) -> Uuid {
        self.order_id
    }
}

// 2. An aggregate is read-model state folded from events. `Apply<E>` is
//    the per-event fold; impl it once per event type it cares about.
#[derive(Default, Clone, Serialize, Deserialize)]
struct Order {
    total: f64,
    shipped: bool,
}
impl Aggregate for Order {
    const NAME: &'static str = "Order";
}
impl Apply<OrderPlaced> for Order {
    fn apply(&mut self, e: &OrderPlaced) {
        self.total = e.total;
    }
}
impl Apply<ShipmentRequested> for Order {
    fn apply(&mut self, _: &ShipmentRequested) {
        self.shipped = true;
    }
}

// 3. A reactor turns one event into others — a *pure* decision. The
//    runtime appends the returned events to the log for you.
struct ShipOnPlaced;
#[async_trait::async_trait]
impl Reactor for ShipOnPlaced {
    type Trigger = OrderPlaced;
    const NAME: &'static str = "ship_on_placed";
    async fn react(&self, t: &OrderPlaced, _ctx: Ctx<'_>) -> anyhow::Result<Events> {
        Ok(causal::events![ShipmentRequested { order_id: t.order_id }])
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 4. Wire the engine. `MemoryStore` plays every backend role for
    //    tests; swap in KurrentDB + Postgres for production.
    let store = Arc::new(MemoryStore::new());
    let engine = EngineBuilder::new(
        store.clone() as Arc<dyn EventLogBackend>,
        store.clone() as Arc<dyn CheckpointStore>,
        store.clone() as Arc<dyn ReactorCheckpoint>,
    )
    .with_aggregators([
        Aggregator::for_type::<Order, OrderPlaced>(),
        Aggregator::for_type::<Order, ShipmentRequested>(),
    ])
    .with_reactor(ShipOnPlaced)
    .build() // async + fallible: seeds reactor cursors
    .await?;

    // 5. Emit one event. `.settled()` resolves only after the whole
    //    chain drains: here, after ShipOnPlaced reacted and its
    //    ShipmentRequested output landed in the log.
    let order_id = Uuid::new_v4();
    engine
        .emit(OrderPlaced {
            order_id,
            total: 99.99,
        })
        .settled()
        .await?;

    // 6. Read the folded state back.
    let order = engine.state_of::<Order>(order_id).await.unwrap().expect("order exists");
    assert_eq!(order.total, 99.99);
    assert!(order.shipped, "set by the reactor's downstream event");

    println!("order {order_id}: total={}, shipped={}", order.total, order.shipped);
    engine.shutdown().await?;
    Ok(())
}
