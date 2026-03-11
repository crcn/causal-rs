# API Comparison: Orchestration vs Event Sourcing

## Complete Example: Order Processing

### BEFORE (Orchestration)

```rust
use causal::{reactor, Context, Engine};
use causal_postgres::PostgresBackend;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ============================================================================
// Events (trigger work)
// ============================================================================

#[derive(Clone, Serialize, Deserialize)]
enum OrderEvent {
    Placed { order_id: Uuid, total: f64 },
    PaymentProcessed { order_id: Uuid },
    Shipped { order_id: Uuid, tracking: String },
    Delivered { order_id: Uuid },
}

// ============================================================================
// Dependencies (services)
// ============================================================================

#[derive(Clone)]
struct Deps {
    payment_api: PaymentApi,
    shipping_api: ShippingApi,
    db: Database,
}

// ============================================================================
// Reactors (async, side effects, return events)
// ============================================================================

#[reactor(on = OrderEvent, extract(order_id), id = "process_payment", retry = 3)]
async fn process_payment(
    order_id: Uuid,
    ctx: Context<Deps>
) -> Result<OrderEvent> {
    // SIDE EFFECT: Call external API
    ctx.deps().payment_api.charge(order_id).await?;

    // SIDE EFFECT: Update database
    ctx.deps().db.mark_paid(order_id).await?;

    // Return new event (triggers next reactors)
    Ok(OrderEvent::PaymentProcessed { order_id })
}

#[reactor(on = OrderEvent, extract(order_id), id = "ship_order", retry = 3)]
async fn ship_order(
    order_id: Uuid,
    ctx: Context<Deps>
) -> Result<OrderEvent> {
    // SIDE EFFECT: Call shipping API
    let tracking = ctx.deps().shipping_api.ship(order_id).await?;

    // Return new event
    Ok(OrderEvent::Shipped { order_id, tracking })
}

#[reactor(on = OrderEvent, extract(order_id), id = "send_notification")]
async fn send_notification(
    order_id: Uuid,
    ctx: Context<Deps>
) -> Result<()> {
    // SIDE EFFECT: Send email
    ctx.deps().email.send(order_id, "Order shipped!").await?;

    // No new event (terminal reactor)
    Ok(())
}

// ============================================================================
// Usage (automatic event processing)
// ============================================================================

#[tokio::main]
async fn main() -> Result<()> {
    let deps = Deps {
        payment_api: PaymentApi::new(),
        shipping_api: ShippingApi::new(),
        db: Database::connect().await?,
    };

    let backend = PostgresBackend::new(pool);

    // Engine automatically processes events through reactors
    let engine = Engine::new(deps, backend)
        .with_reactor(process_payment())
        .with_reactor(ship_order())
        .with_reactor(send_notification());

    // Dispatch event - engine handles the rest
    engine.dispatch(OrderEvent::Placed {
        order_id: Uuid::new_v4(),
        total: 99.99
    }).await?;

    // Engine automatically:
    // 1. Calls process_payment() → PaymentProcessed
    // 2. Calls ship_order() → Shipped
    // 3. Calls send_notification() → ()

    Ok(())
}
```

### AFTER (Event Sourcing)

```rust
use causal::{reactor, Aggregate, Context};
use causal_postgres::PostgresEventStore;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ============================================================================
// Aggregate (entity with state)
// ============================================================================

#[derive(Default, Clone, Serialize, Deserialize)]
struct Order {
    id: Uuid,
    total: f64,
    status: OrderStatus,
    tracking_number: Option<String>,
    version: u64,  // For optimistic locking
}

#[derive(Default, Clone, Serialize, Deserialize)]
enum OrderStatus {
    #[default]
    Draft,
    Placed,
    Paid,
    Shipped,
    Delivered,
}

impl Aggregate for Order {
    type Id = Uuid;

    fn id(&self) -> Uuid { self.id }
    fn version(&self) -> u64 { self.version }
    fn set_version(&mut self, v: u64) { self.version = v; }
}

// ============================================================================
// Events (state changes)
// ============================================================================

#[derive(Clone, Serialize, Deserialize)]
enum OrderEvent {
    Placed { order_id: Uuid, total: f64 },
    PaymentProcessed { order_id: Uuid },
    Shipped { order_id: Uuid, tracking: String },
    Delivered { order_id: Uuid },
}

// ============================================================================
// Reactors (pure state transitions)
// ============================================================================

#[reactors]
mod order_projections {
    use super::*;

    #[reactor(on = OrderEvent, extract(order_id, total))]
    fn apply_placed(order_id: Uuid, total: f64, state: &mut Order) -> Result<()> {
        state.id = order_id;
        state.total = total;
        state.status = OrderStatus::Placed;
        Ok(())
    }

    #[reactor(on = OrderEvent, extract(order_id))]
    fn apply_payment_processed(order_id: Uuid, state: &mut Order) -> Result<()> {
        state.status = OrderStatus::Paid;
        Ok(())
    }

    #[reactor(on = OrderEvent, extract(order_id, tracking))]
    fn apply_shipped(order_id: Uuid, tracking: String, state: &mut Order) -> Result<()> {
        state.status = OrderStatus::Shipped;
        state.tracking_number = Some(tracking);
        Ok(())
    }

    #[reactor(on = OrderEvent, extract(order_id))]
    fn apply_delivered(order_id: Uuid, state: &mut Order) -> Result<()> {
        state.status = OrderStatus::Delivered;
        Ok(())
    }
}

// ============================================================================
// Commands (business logic + side effects)
// ============================================================================

#[derive(Clone)]
struct Deps {
    payment_api: PaymentApi,
    shipping_api: ShippingApi,
}

struct ProcessPayment {
    order_id: Uuid,
}

impl ProcessPayment {
    async fn execute(
        &self,
        store: &PostgresEventStore<OrderEvent>,
        deps: &Deps,
    ) -> Result<Vec<OrderEvent>> {
        // Load current state
        let order: Order = store
            .aggregate(self.order_id)
            .with_reactors(order_projections::reactors())
            .load()
            .await?;

        // Validate business rules
        if order.status != OrderStatus::Placed {
            return Err(anyhow!("Order must be placed to process payment"));
        }

        // SIDE EFFECT: Call external API (with idempotency)
        deps.payment_api.charge(self.order_id, order.total).await?;

        // Return events to append
        Ok(vec![
            OrderEvent::PaymentProcessed { order_id: self.order_id }
        ])
    }
}

struct ShipOrder {
    order_id: Uuid,
}

impl ShipOrder {
    async fn execute(
        &self,
        store: &PostgresEventStore<OrderEvent>,
        deps: &Deps,
    ) -> Result<Vec<OrderEvent>> {
        let order: Order = store
            .aggregate(self.order_id)
            .with_reactors(order_projections::reactors())
            .load()
            .await?;

        if order.status != OrderStatus::Paid {
            return Err(anyhow!("Order must be paid before shipping"));
        }

        // SIDE EFFECT: Call shipping API
        let tracking = deps.shipping_api.ship(self.order_id).await?;

        Ok(vec![
            OrderEvent::Shipped {
                order_id: self.order_id,
                tracking
            }
        ])
    }
}

// ============================================================================
// Usage (explicit command execution)
// ============================================================================

#[tokio::main]
async fn main() -> Result<()> {
    let deps = Deps {
        payment_api: PaymentApi::new(),
        shipping_api: ShippingApi::new(),
    };

    let pool = sqlx::PgPool::connect("postgres://localhost/causal").await?;
    let store = PostgresEventStore::new(pool);

    let order_id = Uuid::new_v4();

    // 1. Place order
    store.append_events(order_id, 0, vec![
        OrderEvent::Placed { order_id, total: 99.99 }
    ]).await?;

    // 2. Process payment (explicit command)
    let events = ProcessPayment { order_id }
        .execute(&store, &deps)
        .await?;
    store.append_events(order_id, 1, events).await?;

    // 3. Ship order (explicit command)
    let events = ShipOrder { order_id }
        .execute(&store, &deps)
        .await?;
    store.append_events(order_id, 2, events).await?;

    // 4. Load final state
    let order: Order = store
        .aggregate(order_id)
        .with_reactors(order_projections::reactors())
        .load()
        .await?;

    println!("Order status: {:?}", order.status);
    println!("Tracking: {:?}", order.tracking_number);

    Ok(())
}
```

## Key Differences

### 1. State Management

**Before:**
```rust
// State is external (in databases, APIs)
// Reactors don't build state, they trigger work

#[reactor(on = OrderEvent)]
async fn ship_order(...) -> Result<OrderEvent> {
    // No aggregate state to mutate
    // Just call APIs and return events
}
```

**After:**
```rust
// State is derived from events
// Reactors build aggregate state

#[reactor(on = OrderEvent)]
fn apply_shipped(..., state: &mut Order) -> Result<()> {
    state.status = OrderStatus::Shipped;  // Pure state transition
    Ok(())
}
```

### 2. Side Effects

**Before:**
```rust
// Side effects mixed with event handling
#[reactor(on = OrderEvent)]
async fn process_payment(ctx: Context<Deps>) -> Result<OrderEvent> {
    ctx.deps().payment_api.charge(...).await?;  // Side effect HERE
    Ok(OrderEvent::PaymentProcessed { ... })
}
```

**After:**
```rust
// Side effects in commands, separate from projections
struct ProcessPayment { order_id: Uuid }

impl ProcessPayment {
    async fn execute(&self, store: &EventStore, deps: &Deps) -> Result<Vec<Event>> {
        deps.payment_api.charge(...).await?;  // Side effect HERE
        Ok(vec![OrderEvent::PaymentProcessed { ... }])
    }
}

// Reactors are pure
#[reactor(on = OrderEvent)]
fn apply_payment_processed(state: &mut Order) -> Result<()> {
    state.status = OrderStatus::Paid;  // No side effects
    Ok(())
}
```

### 3. Control Flow

**Before:**
```rust
// Automatic - engine processes events
engine.dispatch(OrderEvent::Placed { ... }).await?;
// Engine automatically calls all matching reactors
```

**After:**
```rust
// Explicit - you control when commands execute
let events = ProcessPayment { order_id }.execute(&store, &deps).await?;
store.append_events(order_id, expected_version, events).await?;
```

### 4. Query State

**Before:**
```rust
// Query external systems
let status = deps.db.get_order_status(order_id).await?;
```

**After:**
```rust
// Rebuild from events
let order: Order = store
    .aggregate(order_id)
    .with_reactors(order_projections::reactors())
    .load()
    .await?;

println!("Status: {:?}", order.status);
```

### 5. Testing

**Before:**
```rust
#[tokio::test]
async fn test_payment_processing() {
    let deps = MockDeps::new();
    let backend = MemoryBackend::new();
    let engine = Engine::new(deps.clone(), backend)
        .with_reactor(process_payment());

    engine.dispatch(OrderEvent::Placed { ... }).await?;

    // Verify side effects happened
    assert!(deps.payment_api.was_charged());
}
```

**After:**
```rust
#[tokio::test]
async fn test_payment_processing() {
    let deps = MockDeps::new();
    let store = MemoryEventStore::new();

    // Execute command
    let events = ProcessPayment { order_id }
        .execute(&store, &deps)
        .await?;

    // Verify correct events returned
    assert_eq!(events.len(), 1);
    assert!(matches!(events[0], OrderEvent::PaymentProcessed { .. }));

    // Verify side effects
    assert!(deps.payment_api.was_charged());
}
```

## What Stays the Same

✅ `#[reactor]` macro syntax
✅ Field extraction: `extract(order_id, total)`
✅ Type-safe event matching
✅ Module registration: `#[reactors]`
✅ Context for dependencies
✅ Backend abstraction (Memory, PostgreSQL, Kafka)

## What Changes

❌ Reactors are **pure** (no async, no side effects)
❌ Reactors mutate **state** instead of returning events
❌ **Commands** separate from reactors
❌ **Explicit** control flow (no automatic dispatch)
❌ State **derived** from events (event sourcing)

## Summary Table

| Aspect | Orchestration | Event Sourcing |
|--------|--------------|----------------|
| **Purpose** | Coordinate distributed work | Store & rebuild state |
| **Reactors** | Async, side effects | Pure, state transitions |
| **Events** | Trigger work | Store state changes |
| **Control** | Automatic dispatch | Explicit commands |
| **State** | External (DBs, APIs) | Derived from events |
| **Testing** | Mock dependencies | Pure functions + commands |
| **Complexity** | Higher (distributed) | Lower (local state) |
| **Compete with** | Restate, Temporal | postgres-es |
