# Brainstorm: Pivoting Causal to Event Sourcing

**Date:** 2026-02-27
**Context:** Strategic pivot from orchestration to event sourcing library

## Why Event Sourcing?

### The Strategic Rationale

1. **Play to Strengths**: Our `#[handler]` macro API is genuinely nice - that's the differentiator
2. **Don't Compete with Restate**: Restate owns orchestration. They have backing, production deployments, superior runtime
3. **Natural Fit**: Event sourcing is simpler than orchestration and actually suits our API better
4. **Weak Competition**: Rust event sourcing libs are either low-level (postgres-es) or abandoned
5. **Ergonomics Matter**: Event sourcing requires writing lots of projections - our declarative API shines here

### What Makes Sense

Event sourcing is a BETTER fit for our API than orchestration because:
- Handlers naturally map to projections (event → state transition)
- Simpler operational model (no distributed coordination)
- Lower complexity (local state reconstruction vs saga compensation)
- The macro API provides more value (you write more projections than orchestrators)

## The API Vision

### Core Philosophy: Keep What Works

The `#[handler]` macro stays essentially identical, just changes from:
```rust
// Orchestration: async side effects
async fn handler(ctx: Context<Deps>) -> Result<Event>
```

To:
```rust
// Event Sourcing: pure state transitions
fn handler(state: &mut Aggregate) -> Result<()>
```

### Key API Elements

**Aggregates (Domain Entities):**
```rust
#[derive(Default, Clone)]
struct Order {
    id: Uuid,
    status: OrderStatus,
    total: f64,
    version: u64,
}

impl Aggregate for Order {
    type Id = Uuid;
    fn id(&self) -> Uuid { self.id }
    fn version(&self) -> u64 { self.version }
    fn set_version(&mut self, v: u64) { self.version = v; }
}
```

**Events (State Changes):**
```rust
#[derive(Clone, Serialize, Deserialize)]
enum OrderEvent {
    Placed { order_id: Uuid, total: f64 },
    Paid { payment_id: Uuid },
    Shipped { tracking: String },
}
```

**Handlers/Projections (Pure Functions):**
```rust
#[handler(on = OrderEvent, extract(order_id, total))]
fn apply_placed(order_id: Uuid, total: f64, state: &mut Order) -> Result<()> {
    state.id = order_id;
    state.total = total;
    state.status = OrderStatus::Placed;
    Ok(())
}
```

**Commands (Business Logic + Side Effects):**
```rust
struct PlaceOrder { order_id: Uuid, items: Vec<Item> }

impl PlaceOrder {
    async fn execute(&self, store: &EventStore, deps: &Deps) -> Result<Vec<OrderEvent>> {
        // Load current state
        let order = store.aggregate(self.order_id).load().await?;

        // Validate
        ensure!(order.status == OrderStatus::Draft, "Already placed");

        // Side effects
        deps.inventory.reserve(&self.items).await?;

        // Return events
        Ok(vec![OrderEvent::Placed {
            order_id: self.order_id,
            total: self.calculate_total()
        }])
    }
}
```

**Event Store (Backend Agnostic):**
```rust
let store = PostgresEventStore::new(pool);

// Load aggregate by replaying events
let order: Order = store
    .aggregate(order_id)
    .with_handlers(order_handlers::handlers())
    .load()
    .await?;

// Execute command & append events
let events = PlaceOrder { order_id, items }.execute(&store, &deps).await?;
store.append_events(order_id, order.version, events).await?;
```

## Pressure Test: Fowler's Patterns

Validated against Martin Fowler's authoritative articles on Event Sourcing.

### ✅ What We Got Right

1. **Event Storage**: Append-only event log as source of truth
2. **State Reconstruction**: Rebuild by replaying events
3. **Snapshots**: Planned from the start
4. **Pure Projections**: Handlers are deterministic
5. **Backend Abstraction**: Memory/PostgreSQL/Kafka

### ❌ Critical Gaps (High Priority)

#### 1. Pattern Conflation
**Issue**: We're mixing Event Sourcing (state storage) with Event Notification (integration events).

**Fix**: Separate concerns
- `Aggregate` + `EventStore` = Event Sourcing (domain events)
- `EventBus` = Event Notification (integration events)
- `Projection` + `Context` = CQRS (read models with async)

#### 2. No Event Versioning
**Issue**: Can't evolve event schemas without breaking old events.

**Fix**: Add versioning from day one
```rust
pub struct VersionedEvent<E> {
    pub version: u32,
    pub event_type: String,
    pub data: E,
}

pub trait EventUpcast {
    fn upcast_from(version: u32, data: Value) -> Result<Self>;
}
```

#### 3. No Replay Safety
**Issue**: Replaying events would trigger external API calls again (duplicate shipments, charges, emails).

**Fix**: Gateway pattern
```rust
pub struct ReplayContext {
    mode: ReplayMode,  // Live or Replay
}

pub trait ExternalGateway {
    async fn execute<T>(&self, ctx: &ReplayContext, op: impl FnOnce() -> T) -> T;
}
```

### ⚠️ Medium Priority Gaps

#### 4. No Temporal Queries
**Issue**: Can't query state at a specific point in time (key ES feature).

**Fix**: Add time-travel support
```rust
let order_yesterday = store.aggregate_at(order_id, Utc::now() - Duration::days(1)).await?;
let order_v5 = store.aggregate_at_version(order_id, 5).await?;
```

#### 5. No Query Result Logging
**Issue**: Non-deterministic external queries (exchange rates, timestamps) will differ on replay.

**Fix**: Document best practice - log query results in events
```rust
enum OrderEvent {
    PriceCalculated {
        base_price: f64,
        exchange_rate: f64,  // ← Store the query result
        queried_at: DateTime<Utc>,
        final_price: f64,
    },
}
```

### 🟢 Low Priority

#### 6. No Event Reversal Support
**Issue**: Can't undo events (though compensating events work).

**Fix**: Prefer compensating events (natural pattern)
```rust
enum OrderEvent {
    ItemAdded { item: Item },
    ItemRemoved { item: Item },  // Compensation
}
```

## Five Key Gaps (User's Analysis)

### 1. No Replay Tooling ⚠️
**Impact**: Can't actually replay events (biggest missed opportunity)

**Current**: Have foundation (idempotent operations, causal chains, append-only store)
**Missing**: No API to trigger replay

**Fix Needed:**
```rust
// Replay all events for an aggregate
store.replay_aggregate(order_id).await?;

// Replay events in a time range
store.replay_range(start_time, end_time).await?;

// Replay specific events
store.replay_events(vec![event_id_1, event_id_2]).await?;
```

### 2. Mid-Chain Failure Recovery ⚠️
**Impact**: Event 104 fails, but 100-103 already persisted with no compensation

**Current**: Manual investigation required
**Missing**: DLQ or retry mechanism

**Fix Options:**
- Keep DLQ for projections (even though they're pure)
- Add transactional batch processing
- Implement saga-style compensation

### 3. Event Schema Evolution 🔴
**Impact**: Can't change event structure without breaking existing events

**Current**: `schema_v` on every event (good foresight!)
**Missing**: Upcaster logic to transform old shapes

**Fix Needed:** High priority (covered above in #2)

### 4. No Snapshots ⚠️
**Impact**: Full replay from event 0 to rebuild state (slow at scale)

**Current**: Planned in schema
**Missing**: Implementation

**Fix Needed:**
```rust
// Automatic snapshots every N events
impl EventStore {
    pub async fn save_snapshot<A: Aggregate>(&self, aggregate: &A) -> Result<()>;
    pub async fn load_snapshot<A: Aggregate>(&self, id: A::Id) -> Result<Option<(A, u64)>>;
}
```

### 5. Replay-Aware Gateways 🔴
**Impact**: Non-idempotent side effects (email, webhooks) trigger on replay

**Current**: Safe because operations are idempotent (MERGE)
**Future**: Need gateway pattern for non-idempotent ops

**Fix Needed:** High priority (covered above in #3)

## Migration Timeline

### Phase 1: Core Traits (1-2 days)
- Add `Aggregate` trait
- Add `EventStore` trait
- Add `AggregateLoader` builder
- Add `VersionedEvent` + `EventUpcast` (NEW)

### Phase 2: Handler Macro (2-3 days)
- Update to generate pure handlers
- Support optional `Context` for async projections
- Add `#[reversible]` attribute (optional)

### Phase 3: PostgreSQL Backend (2-3 days)
- New event store schema
- Implement `PostgresEventStore`
- Add `ReplayContext` + Gateway pattern (NEW)
- Implement snapshot support

### Phase 4: Replay Tooling (1-2 days) (NEW)
- Add replay APIs to EventStore trait
- Implement time-range replay
- Add replay-aware gateway detection

### Phase 5: Examples & Docs (2-3 days)
- Event-sourced order example
- CQRS blog example
- Bank account example
- Update all docs

**Total Estimate:** 10-15 days (~2-3 weeks)

## Next Steps

### Immediate Actions

1. **Create Tracking Issues** for five critical gaps:
   - [ ] Replay tooling API
   - [ ] Mid-chain failure recovery
   - [ ] Event schema evolution (upcasting)
   - [ ] Snapshot implementation
   - [ ] Replay-aware gateways

2. **Prototype Core APIs**:
   - [ ] `Aggregate` trait
   - [ ] `EventStore` trait with replay methods
   - [ ] `VersionedEvent` + `EventUpcast`
   - [ ] `ReplayContext` + `ExternalGateway`

3. **Update Migration Plan** with:
   - Replay tooling phase
   - Event versioning implementation
   - Gateway pattern examples

### Open Questions

1. **Keep orchestration as feature flag?**
   - Pro: No breaking changes
   - Con: Maintenance burden

2. **Command pattern: macro or manual?**
   - Could add `#[command]` macro
   - Or keep simple with structs

3. **Snapshot strategy?**
   - Automatic every N events?
   - Manual snapshots?
   - Both?

4. **How to handle async projections?**
   - Current proposal: Optional `Context<Deps>`
   - Makes handler `async` if Context present

## Positioning

**Tagline:** "Ergonomic Event Sourcing for Rust"

**Value Props:**
- Declarative projections with `#[handler]` macro
- Type-safe event matching and field extraction
- Backend-agnostic (Memory → PostgreSQL → Kafka)
- CQRS support built-in
- Zero boilerplate

**Compete Against:**
- postgres-es: "We're higher-level with better ergonomics"
- EventStore: "Native Rust, better type safety, simpler"

**Complement With:**
- Restate: "Use Restate for orchestration, Causal for event sourcing"

## Conclusion

This pivot makes strategic sense:
1. ✅ Plays to our API strengths
2. ✅ Avoids competing with Restate
3. ✅ Simpler than orchestration
4. ✅ Weak competition in Rust
5. ✅ Natural fit for the current design

But we need to address the five critical gaps (especially replay tooling, event versioning, and replay safety) before shipping.

**Grade:** B- (Solid foundation, needs critical gaps fixed)

**Recommendation:** Fix high-priority gaps, then ship MVP.
