# Event Sourcing API Design

## Core Concepts

### 1. Aggregates (Entities with State)

```rust
use seesaw::{Aggregate, EventStore};
use serde::{Serialize, Deserialize};
use uuid::Uuid;

// Your domain aggregate
#[derive(Default, Clone, Serialize, Deserialize)]
struct Order {
    id: Uuid,
    customer_id: Uuid,
    items: Vec<Item>,
    total: f64,
    status: OrderStatus,
    version: u64,  // Optimistic locking
}

#[derive(Clone, Serialize, Deserialize)]
enum OrderStatus {
    Draft,
    Placed,
    Paid,
    Shipped,
    Delivered,
    Cancelled,
}

// Mark as aggregate (provides ID, versioning)
impl Aggregate for Order {
    type Id = Uuid;

    fn id(&self) -> Uuid {
        self.id
    }

    fn version(&self) -> u64 {
        self.version
    }

    fn set_version(&mut self, version: u64) {
        self.version = version;
    }
}
```

### 2. Events (Immutable Facts)

```rust
#[derive(Clone, Serialize, Deserialize)]
enum OrderEvent {
    Created {
        order_id: Uuid,
        customer_id: Uuid
    },
    ItemAdded {
        order_id: Uuid,
        item: Item,
        total: f64
    },
    Placed {
        order_id: Uuid,
        total: f64
    },
    PaymentReceived {
        order_id: Uuid,
        amount: f64
    },
    Shipped {
        order_id: Uuid,
        tracking_number: String
    },
    Delivered {
        order_id: Uuid
    },
    Cancelled {
        order_id: Uuid,
        reason: String
    },
}
```

### 3. Handlers (Pure Projections)

**The API stays almost identical, just pure instead of async!**

```rust
use seesaw::{handler, Context};

// Handlers become pure state transitions
#[handler(on = OrderEvent, extract(order_id, customer_id))]
fn apply_created(
    order_id: Uuid,
    customer_id: Uuid,
    state: &mut Order,
) -> Result<()> {
    state.id = order_id;
    state.customer_id = customer_id;
    state.status = OrderStatus::Draft;
    Ok(())
}

#[handler(on = OrderEvent, extract(order_id, item, total))]
fn apply_item_added(
    order_id: Uuid,
    item: Item,
    total: f64,
    state: &mut Order,
) -> Result<()> {
    state.items.push(item);
    state.total = total;
    Ok(())
}

#[handler(on = OrderEvent, extract(order_id))]
fn apply_placed(order_id: Uuid, state: &mut Order) -> Result<()> {
    state.status = OrderStatus::Placed;
    Ok(())
}

#[handler(on = OrderEvent, extract(order_id))]
fn apply_shipped(order_id: Uuid, state: &mut Order) -> Result<()> {
    state.status = OrderStatus::Shipped;
    Ok(())
}
```

### 4. Module-Level Registration (Keep Current API!)

```rust
#[handlers]
mod order_handlers {
    use super::*;

    #[handler(on = OrderEvent, extract(order_id, customer_id))]
    fn apply_created(order_id: Uuid, customer_id: Uuid, state: &mut Order) -> Result<()> {
        state.id = order_id;
        state.customer_id = customer_id;
        state.status = OrderStatus::Draft;
        Ok(())
    }

    // ... more handlers
}
```

### 5. Event Store (Load & Save Aggregates)

```rust
use seesaw::{EventStore, AggregateLoader};
use seesaw_postgres::PostgresEventStore;

#[tokio::main]
async fn main() -> Result<()> {
    // Create event store (backend-agnostic!)
    let pool = PgPoolOptions::new()
        .connect("postgres://localhost/seesaw")
        .await?;

    let store = PostgresEventStore::new(pool);

    // Load aggregate by replaying events
    let order: Order = store
        .aggregate(order_id)
        .with_handlers(order_handlers::handlers())
        .load()
        .await?;

    println!("Order status: {:?}", order.status);
    println!("Order total: ${}", order.total);

    // Append new events
    store.append(order_id, vec![
        OrderEvent::Placed { order_id, total: order.total }
    ]).await?;

    Ok(())
}
```

### 6. Command Pattern (Business Logic)

Commands are separate from events - they contain validation and business logic:

```rust
// Commands are actions you want to perform
struct PlaceOrder {
    order_id: Uuid,
}

impl PlaceOrder {
    async fn execute(
        &self,
        store: &EventStore,
        deps: &Deps
    ) -> Result<Vec<OrderEvent>> {
        // Load current state
        let order: Order = store
            .aggregate(self.order_id)
            .with_handlers(order_handlers::handlers())
            .load()
            .await?;

        // Validate business rules
        if order.items.is_empty() {
            return Err(anyhow!("Cannot place order with no items"));
        }

        if order.status != OrderStatus::Draft {
            return Err(anyhow!("Order already placed"));
        }

        // Call external services if needed
        let payment_valid = deps.payment_service
            .validate(order.customer_id)
            .await?;

        if !payment_valid {
            return Err(anyhow!("Payment validation failed"));
        }

        // Return events to append
        Ok(vec![
            OrderEvent::Placed {
                order_id: self.order_id,
                total: order.total
            }
        ])
    }
}

// Use command
let events = PlaceOrder { order_id }.execute(&store, &deps).await?;
store.append(order_id, events).await?;
```

### 7. Read Models (CQRS Projections)

Build denormalized views for queries:

```rust
// Read model for queries
#[derive(Default, Serialize, Deserialize)]
struct OrderSummary {
    order_id: Uuid,
    customer_name: String,
    status: String,
    total: f64,
    item_count: usize,
}

// Project events into read model
#[handlers]
mod order_summary_projection {
    use super::*;

    #[handler(on = OrderEvent, extract(order_id, customer_id))]
    async fn project_created(
        order_id: Uuid,
        customer_id: Uuid,
        state: &mut OrderSummary,
        ctx: Context<Deps>,
    ) -> Result<()> {
        state.order_id = order_id;
        // Can do async lookups!
        state.customer_name = ctx.deps().db.get_customer_name(customer_id).await?;
        Ok(())
    }

    #[handler(on = OrderEvent, extract(item))]
    fn project_item_added(item: Item, state: &mut OrderSummary) -> Result<()> {
        state.item_count += 1;
        Ok(())
    }
}

// Build read model from events
let summary: OrderSummary = store
    .projection(order_id)
    .with_handlers(order_summary_projection::handlers())
    .with_deps(deps)  // For async projections
    .build()
    .await?;
```

## API Comparison

### Before (Orchestration)
```rust
#[handler(on = OrderEvent, extract(order_id), id = "ship_order")]
async fn ship_order(order_id: Uuid, ctx: Context<Deps>) -> Result<OrderEvent> {
    // ASYNC SIDE EFFECT
    ctx.deps().shipping_api.ship(order_id).await?;
    Ok(OrderEvent::Shipped { order_id })
}

// Engine processes events automatically
let engine = Engine::new(deps, backend)
    .with_handler(ship_order());

engine.dispatch(OrderEvent::Placed { order_id }).await?;
```

### After (Event Sourcing)
```rust
#[handler(on = OrderEvent, extract(order_id))]
fn apply_shipped(order_id: Uuid, state: &mut Order) -> Result<()> {
    // PURE STATE TRANSITION
    state.status = OrderStatus::Shipped;
    Ok(())
}

// Load aggregate
let order: Order = store
    .aggregate(order_id)
    .with_handlers(order_handlers::handlers())
    .load()
    .await?;

// Commands do side effects + return events
let events = ShipOrder { order_id }.execute(&store, &deps).await?;
store.append(order_id, events).await?;
```

## Key Differences

| Aspect | Before (Orchestration) | After (Event Sourcing) |
|--------|----------------------|----------------------|
| **Handlers** | Async, side effects, return events | Pure, state transitions |
| **Events** | Trigger work | Store state |
| **Context** | Always available | Only for projections |
| **Engine** | Automatically processes | Manual command execution |
| **Storage** | Event log for coordination | Event log as source of truth |
| **State** | External (DBs, APIs) | Derived from events |

## What Stays the Same

✅ `#[handler]` macro syntax
✅ Field extraction: `extract(order_id, total)`
✅ Type-safe event matching
✅ Module registration: `#[handlers]`
✅ Backend abstraction (Memory, PostgreSQL, Kafka)
✅ Context for dependencies

## What Changes

1. **Handlers are pure** (sync, no async by default)
2. **Add `state: &mut Aggregate` parameter**
3. **Commands separate from projections**
4. **EventStore replaces Engine**
5. **Manual append instead of automatic dispatch**

## Backwards Compatibility Strategy

Keep orchestration as a feature flag!

```toml
[features]
default = ["event-sourcing"]
event-sourcing = []
orchestration = []  # Legacy mode
```

Or keep both patterns:
- `EventStore` for event sourcing
- `Engine` for orchestration (if people still want it)

## Migration Examples

### Example 1: Simple State Machine

**Before:**
```rust
#[handler(on = OrderEvent, extract(order_id))]
async fn complete_order(order_id: Uuid, ctx: Context<Deps>) -> Result<OrderEvent> {
    ctx.deps().db.mark_complete(order_id).await?;
    Ok(OrderEvent::Completed { order_id })
}
```

**After:**
```rust
#[handler(on = OrderEvent, extract(order_id))]
fn apply_completed(order_id: Uuid, state: &mut Order) -> Result<()> {
    state.status = OrderStatus::Completed;
    Ok(())
}

// Side effects move to commands
struct CompleteOrder { order_id: Uuid }
impl CompleteOrder {
    async fn execute(&self, store: &EventStore, deps: &Deps) -> Result<Vec<OrderEvent>> {
        deps.db.mark_complete(self.order_id).await?;
        Ok(vec![OrderEvent::Completed { order_id: self.order_id }])
    }
}
```

### Example 2: Multiple Handlers (Fan-out)

**Before:**
```rust
#[handler(on = OrderEvent, extract(order_id))]
async fn send_email(order_id: Uuid, ctx: Context<Deps>) -> Result<()> {
    ctx.deps().email.send(order_id).await?;
    Ok(())
}

#[handler(on = OrderEvent, extract(order_id))]
async fn update_inventory(order_id: Uuid, ctx: Context<Deps>) -> Result<()> {
    ctx.deps().inventory.reserve(order_id).await?;
    Ok(())
}
```

**After (Side Effects in Commands):**
```rust
struct PlaceOrder { order_id: Uuid }
impl PlaceOrder {
    async fn execute(&self, store: &EventStore, deps: &Deps) -> Result<Vec<OrderEvent>> {
        // Do ALL side effects here
        deps.email.send(self.order_id).await?;
        deps.inventory.reserve(self.order_id).await?;

        // Return events
        Ok(vec![OrderEvent::Placed { order_id: self.order_id }])
    }
}

// OR use process managers/sagas (separate concern)
```

## Next Steps

1. ✅ Define core traits (`Aggregate`, `EventStore`)
2. ✅ Adapt handler macro for pure functions
3. ✅ Implement PostgreSQL event store
4. ✅ Add snapshot support
5. ✅ Build command pattern helpers
6. ✅ Add CQRS projection support
7. ✅ Update examples and docs
