# Design Pressure Test: Fowler's Event Sourcing Patterns

## Overview

Validating our event sourcing design against:
- [Martin Fowler: Event Sourcing](https://martinfowler.com/eaaDev/EventSourcing.html)
- [Martin Fowler: What do you mean by "Event-Driven"?](https://martinfowler.com/articles/201701-event-driven.html)

## Pattern Clarity

### ❌ PROBLEM: Pattern Conflation

Fowler's key warning: **Event Notification, Event-Carried State Transfer, Event Sourcing, and CQRS are often confused.**

**Our Design:**
```rust
// Are these Event Sourcing events or Event Notification?
enum OrderEvent {
    Placed { order_id: Uuid },      // Event Sourcing: state change
    PaymentProcessed { order_id: Uuid },  // Event Sourcing: state change
}

// What about these?
#[reactor(on = OrderEvent)]
async fn send_notification(...) {  // Event Notification: trigger side effect
    ctx.deps().email.send(...).await?;
}
```

**Issue:** We're mixing Event Sourcing (state storage) with Event Notification (triggering work).

### ✅ SOLUTION: Clear Pattern Separation

#### Pattern 1: Event Sourcing (Core)
**Purpose:** Store state as events

```rust
// Domain events (Event Sourcing)
#[derive(Clone, Serialize, Deserialize)]
enum OrderEvent {
    Created { order_id: Uuid, customer_id: Uuid },
    ItemAdded { item: Item, total: f64 },
    Placed { total: f64 },
    Paid { payment_id: Uuid },
    Shipped { tracking: String },
}

// Pure projections rebuild state
#[reactor(on = OrderEvent)]
fn apply_placed(state: &mut Order) -> Result<()> {
    state.status = OrderStatus::Placed;  // Pure state transition
    Ok(())
}
```

#### Pattern 2: Event Notification (Separate)
**Purpose:** Trigger side effects in other systems

```rust
// Integration events (Event Notification)
// These are published AFTER domain events are committed
#[derive(Clone, Serialize, Deserialize)]
struct OrderPlacedNotification {
    order_id: Uuid,
    customer_id: Uuid,
    total: f64,
    timestamp: DateTime<Utc>,
}

// Commands can publish notifications after committing events
impl PlaceOrder {
    async fn execute(&self, store: &EventStore, bus: &EventBus) -> Result<()> {
        // 1. Append domain events (Event Sourcing)
        let events = vec![OrderEvent::Placed { total: self.total }];
        store.append_events(self.order_id, self.version, events).await?;

        // 2. Publish integration event (Event Notification)
        bus.publish(OrderPlacedNotification {
            order_id: self.order_id,
            customer_id: self.customer_id,
            total: self.total,
            timestamp: Utc::now(),
        }).await?;

        Ok(())
    }
}
```

#### Pattern 3: CQRS (Read Models)
**Purpose:** Denormalized views for queries

```rust
// Read model (CQRS)
#[derive(Default)]
struct OrderListView {
    orders: HashMap<Uuid, OrderSummary>,
}

// Async projection for read model
#[reactor(on = OrderEvent, extract(order_id, customer_id))]
async fn project_order_list(
    order_id: Uuid,
    customer_id: Uuid,
    view: &mut OrderListView,
    ctx: Context<Deps>,
) -> Result<()> {
    // Can do async lookups for denormalization
    let customer_name = ctx.deps().db.get_customer_name(customer_id).await?;

    view.orders.insert(order_id, OrderSummary {
        order_id,
        customer_name,
        status: "placed".into(),
    });

    Ok(())
}
```

**Recommendation:** Add three distinct concepts to the API:
1. `Aggregate` + `#[reactor]` = Event Sourcing (pure projections)
2. `EventBus` = Event Notification (publish/subscribe)
3. `Projection` + `Context` = CQRS (async read models)

---

## Event Reversibility

### ❌ PROBLEM: No Support for Event Reversal

Fowler: "Events need to be Reversible... If you send out an event that adds $5 to Martin's account, you need to be able to reverse that event."

**Our Design:**
```rust
// Can we reverse these events?
enum OrderEvent {
    ItemAdded { item: Item, total: f64 },  // ❌ Where was total BEFORE?
    Shipped { tracking: String },          // ❌ What was status BEFORE?
}
```

### ✅ SOLUTION: Event Compensation Patterns

#### Option 1: Compensating Events (Recommended)
```rust
enum OrderEvent {
    ItemAdded { item: Item, new_total: f64 },
    ItemRemoved { item: Item, new_total: f64 },  // Compensation

    Shipped { tracking: String },
    ShipmentCancelled { reason: String },  // Compensation
}
```

#### Option 2: Store Previous State (If Needed)
```rust
enum OrderEvent {
    ItemAdded {
        item: Item,
        previous_total: f64,  // For reversal
        new_total: f64,
    },
}
```

#### Option 3: Difference-Based Events (Fowler's Preference)
```rust
enum AccountEvent {
    CreditApplied { amount: f64 },   // Reversal: debit same amount
    DebitApplied { amount: f64 },    // Reversal: credit same amount
}

// Easier to reverse than:
enum AccountEvent {
    BalanceSet { balance: f64 },  // How do we reverse this?
}
```

**Recommendation:** Prefer compensating events. Add `#[reversible]` attribute to flag events that need reversal support:

```rust
#[reversible(ItemRemoved)]  // Specifies the compensating event type
#[reactor(on = OrderEvent)]
fn apply_item_added(state: &mut Order) -> Result<()> {
    // ...
}
```

---

## External System Integration

### ❌ PROBLEM: No Gateway Pattern for Replay Safety

Fowler: "You need to wrap any external systems with a Gateway... so that when replaying events the Gateway can detect it's in replay mode and not actually send the notification."

**Our Design:**
```rust
impl ShipOrder {
    async fn execute(&self, store: &EventStore, deps: &Deps) -> Result<Vec<Event>> {
        // ❌ What happens if we replay events?
        let tracking = deps.shipping_api.ship(self.order_id).await?;

        Ok(vec![OrderEvent::Shipped { tracking }])
    }
}
```

**Problem:** Replaying events would call `shipping_api.ship()` again, creating duplicate shipments!

### ✅ SOLUTION: Replay-Safe Gateway Pattern

```rust
/// Replay context tracks whether we're replaying or executing live
#[derive(Clone)]
pub struct ReplayContext {
    mode: ReplayMode,
}

#[derive(Clone, Copy)]
pub enum ReplayMode {
    Live,        // Actually execute external calls
    Replay,      // Skip external calls, use cached results
}

/// Gateway pattern for external systems
pub trait ExternalGateway: Clone + Send + Sync {
    /// Execute operation, checking replay mode
    async fn execute<T>(&self, ctx: &ReplayContext, op: impl FnOnce() -> T) -> T;
}

#[derive(Clone)]
pub struct ShippingGateway {
    api: ShippingApi,
}

impl ExternalGateway for ShippingGateway {
    async fn execute<T>(&self, ctx: &ReplayContext, op: impl FnOnce() -> T) -> T {
        match ctx.mode {
            ReplayMode::Live => op(),
            ReplayMode::Replay => {
                // Skip actual call during replay
                panic!("External call during replay detected!")
            }
        }
    }
}

// Usage in commands
impl ShipOrder {
    async fn execute(
        &self,
        store: &EventStore,
        deps: &Deps,
        ctx: &ReplayContext,  // ← Added
    ) -> Result<Vec<Event>> {
        // Gateway prevents calls during replay
        let tracking = deps.shipping_gateway
            .execute(ctx, || deps.shipping_api.ship(self.order_id))
            .await?;

        Ok(vec![OrderEvent::Shipped { tracking }])
    }
}
```

**Recommendation:** Add `ReplayContext` to all command execution paths.

---

## Non-Deterministic External Queries

### ❌ PROBLEM: No Query Result Logging

Fowler: "If I ask for an exchange rate on December 5th and replay that event on December 20th, I will need the exchange rate on Dec 5 not the later one. The way to deal with this is to save the query result in the event log as well."

**Our Design:**
```rust
impl CalculateTotal {
    async fn execute(&self, store: &EventStore, deps: &Deps) -> Result<Vec<Event>> {
        // ❌ Exchange rate will be different if replayed later!
        let rate = deps.currency_api.get_rate("USD", "EUR").await?;
        let total = self.amount * rate;

        Ok(vec![OrderEvent::TotalCalculated { total }])
    }
}
```

### ✅ SOLUTION: Log Query Results in Events

```rust
enum OrderEvent {
    TotalCalculated {
        amount: f64,
        exchange_rate: f64,      // ← Store the query result
        currency_from: String,
        currency_to: String,
        total: f64,
    },
}

impl CalculateTotal {
    async fn execute(&self, store: &EventStore, deps: &Deps) -> Result<Vec<Event>> {
        // Query external system
        let rate = deps.currency_api.get_rate("USD", "EUR").await?;
        let total = self.amount * rate;

        // ✅ Store query result in event
        Ok(vec![OrderEvent::TotalCalculated {
            amount: self.amount,
            exchange_rate: rate,  // Logged for replay
            currency_from: "USD".into(),
            currency_to: "EUR".into(),
            total,
        }])
    }
}
```

**Recommendation:** Events should capture ALL data needed for deterministic replay, including external query results.

---

## Event Schema Evolution

### ❌ PROBLEM: No Event Versioning/Upcasting

Fowler: "Code evolution complicates event sourcing... bug fixes require reprocessing."

**Our Design:**
```rust
// What happens when we change event structure?
enum OrderEvent {
    Placed { order_id: Uuid, total: f64 },
}

// Later, we want to add currency:
enum OrderEvent {
    Placed { order_id: Uuid, total: f64, currency: String },  // ❌ Breaks old events!
}
```

### ✅ SOLUTION: Event Versioning + Upcasting

```rust
/// Event with version metadata
#[derive(Clone, Serialize, Deserialize)]
pub struct VersionedEvent<E> {
    pub version: u32,
    pub event_type: String,
    pub data: E,
}

/// Trait for upcasting events
pub trait EventUpcast: Sized {
    fn upcast_from(version: u32, data: Value) -> Result<Self>;
}

// V1: Original event
#[derive(Serialize, Deserialize)]
struct OrderPlacedV1 {
    order_id: Uuid,
    total: f64,
}

// V2: Added currency field
#[derive(Serialize, Deserialize)]
struct OrderPlacedV2 {
    order_id: Uuid,
    total: f64,
    currency: String,
}

// Current event type
#[derive(Clone, Serialize, Deserialize)]
enum OrderEvent {
    Placed {
        order_id: Uuid,
        total: f64,
        currency: String,
    },
}

impl EventUpcast for OrderEvent {
    fn upcast_from(version: u32, data: Value) -> Result<Self> {
        match version {
            1 => {
                let v1: OrderPlacedV1 = serde_json::from_value(data)?;
                // Upcast V1 -> V2 (add default currency)
                Ok(OrderEvent::Placed {
                    order_id: v1.order_id,
                    total: v1.total,
                    currency: "USD".into(),  // Default for old events
                })
            }
            2 => {
                let v2: OrderPlacedV2 = serde_json::from_value(data)?;
                Ok(OrderEvent::Placed {
                    order_id: v2.order_id,
                    total: v2.total,
                    currency: v2.currency,
                })
            }
            _ => Err(anyhow!("Unknown event version: {}", version)),
        }
    }
}
```

**Recommendation:** Add event versioning from day one:
1. Store version with each event
2. Implement `EventUpcast` trait
3. Provide migration helpers

---

## Snapshots

### ✅ GOOD: We Have Snapshots Planned

Fowler: "You can speed up the temporal query by taking a snapshot of the application state at a particular event... you can then start from that snapshot and replay only the events since the snapshot."

**Our Design:**
```sql
CREATE TABLE causal_snapshots (
    aggregate_id UUID NOT NULL,
    version BIGINT NOT NULL,
    state JSONB NOT NULL,
    PRIMARY KEY (aggregate_id, version)
);
```

```rust
impl AggregateLoader {
    pub fn from_snapshot(self, snapshot_version: u64) -> Self {
        self.from_version(snapshot_version)
    }
}
```

**Recommendation:** ✅ Keep as-is. Add automatic snapshot creation every N events.

---

## Temporal Queries

### ⚠️ MISSING: Time-Travel Queries

Fowler: "Event Sourcing allows you to query the application state at any point in time."

**Our Design:**
```rust
// ❌ Can only load current state
let order: Order = store.aggregate(order_id).load().await?;
```

### ✅ SOLUTION: Add Temporal Query Support

```rust
impl EventStore {
    /// Load aggregate state at a specific point in time
    async fn aggregate_at<A>(
        &self,
        aggregate_id: A::Id,
        timestamp: DateTime<Utc>,
    ) -> Result<A>
    where
        A: Aggregate;

    /// Load aggregate state at a specific version
    async fn aggregate_at_version<A>(
        &self,
        aggregate_id: A::Id,
        version: u64,
    ) -> Result<A>
    where
        A: Aggregate;
}

// Usage
let order_yesterday: Order = store
    .aggregate_at(order_id, Utc::now() - Duration::days(1))
    .await?;

let order_v5: Order = store
    .aggregate_at_version(order_id, 5)
    .await?;
```

**Recommendation:** Add temporal query support to EventStore trait.

---

## Summary: Design Gaps & Fixes

### Critical Gaps

| Issue | Impact | Fix Priority |
|-------|--------|--------------|
| Pattern conflation | Confusing mental model | 🔴 High |
| No event versioning | Can't evolve schema | 🔴 High |
| No replay safety | Duplicate external calls | 🔴 High |
| No query logging | Non-deterministic replay | 🟡 Medium |
| No temporal queries | Missing key ES feature | 🟡 Medium |
| No event reversal | Can't undo mistakes | 🟢 Low |

### Recommended Changes

#### 1. **Pattern Separation** (High Priority)
```rust
// Event Sourcing (state storage)
pub trait Aggregate { ... }
pub trait EventStore { ... }

// Event Notification (integration)
pub trait EventBus { ... }
pub fn publish<E>(...) { ... }

// CQRS (read models)
pub trait Projection { ... }
pub struct ReadModel<T> { ... }
```

#### 2. **Event Versioning** (High Priority)
```rust
pub struct VersionedEvent<E> {
    pub version: u32,
    pub data: E,
}

pub trait EventUpcast {
    fn upcast_from(version: u32, data: Value) -> Result<Self>;
}
```

#### 3. **Replay Safety** (High Priority)
```rust
pub struct ReplayContext {
    mode: ReplayMode,
}

pub trait ExternalGateway {
    async fn execute<T>(&self, ctx: &ReplayContext, op: impl FnOnce() -> T) -> T;
}
```

#### 4. **Temporal Queries** (Medium Priority)
```rust
impl EventStore {
    async fn aggregate_at<A>(&self, id: A::Id, timestamp: DateTime<Utc>) -> Result<A>;
    async fn aggregate_at_version<A>(&self, id: A::Id, version: u64) -> Result<A>;
}
```

#### 5. **Query Logging** (Medium Priority)
```rust
// Document best practice: log all external query results in events
enum OrderEvent {
    PriceCalculated {
        base_price: f64,
        exchange_rate: f64,  // ← Log the query result
        final_price: f64,
    },
}
```

---

## Conclusion

**Overall Grade: B- (Needs Work)**

**Strengths:**
- ✅ Clean reactor API
- ✅ Backend abstraction
- ✅ Snapshot support planned
- ✅ Pure projections

**Critical Gaps:**
- ❌ Pattern conflation (Event Sourcing vs Event Notification)
- ❌ No event versioning/upcasting
- ❌ No replay safety (Gateway pattern)
- ❌ No temporal queries

**Recommendation:** Address the three High Priority issues before shipping:
1. Separate Event Sourcing from Event Notification
2. Add event versioning from day one
3. Implement replay-safe external system gateways

With these fixes, the design will be solid and aligned with Fowler's patterns.
