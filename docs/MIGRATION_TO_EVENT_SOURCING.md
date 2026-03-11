# Migration Plan: Orchestration → Event Sourcing

## Phase 1: Core Traits (1-2 days)

### 1.1 Add `Aggregate` trait to `causal_core`

**File:** `crates/causal/src/aggregate.rs`

```rust
use uuid::Uuid;

/// Trait for event-sourced aggregates
pub trait Aggregate: Default + Clone + Send + Sync + 'static {
    /// The aggregate's unique identifier type
    type Id: Clone + Send + Sync + 'static;

    /// Get the aggregate's ID
    fn id(&self) -> Self::Id;

    /// Get the current version (for optimistic locking)
    fn version(&self) -> u64;

    /// Set the version (called by event store)
    fn set_version(&mut self, version: u64);
}

/// Helper trait to extract aggregate ID from events
pub trait EventWithId {
    type Id;
    fn aggregate_id(&self) -> Self::Id;
}
```

### 1.2 Add `EventStore` trait

**File:** `crates/causal/src/event_store.rs`

```rust
use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Stored event with metadata
#[derive(Clone, Serialize, Deserialize)]
pub struct StoredEvent<E> {
    /// Event sequence number (per aggregate)
    pub sequence: u64,

    /// When the event was stored
    pub timestamp: DateTime<Utc>,

    /// The event data
    pub data: E,

    /// Event type name (for upcasting)
    pub event_type: String,
}

/// Event store trait (backend-agnostic)
#[async_trait]
pub trait EventStore: Clone + Send + Sync + 'static {
    type Event: Clone + Send + Sync + 'static;

    /// Load events for an aggregate
    async fn load_events<Id>(
        &self,
        aggregate_id: Id,
        from_version: u64,
    ) -> Result<Vec<StoredEvent<Self::Event>>>
    where
        Id: Clone + Send + Sync + 'static;

    /// Append events with optimistic locking
    async fn append_events<Id>(
        &self,
        aggregate_id: Id,
        expected_version: u64,
        events: Vec<Self::Event>,
    ) -> Result<()>
    where
        Id: Clone + Send + Sync + 'static;

    /// Get the current version of an aggregate
    async fn current_version<Id>(&self, aggregate_id: Id) -> Result<Option<u64>>
    where
        Id: Clone + Send + Sync + 'static;
}
```

### 1.3 Add `AggregateLoader`

**File:** `crates/causal/src/aggregate_loader.rs`

```rust
use crate::{Aggregate, EventStore, Handler, StoredEvent};
use anyhow::Result;

/// Builder for loading aggregates from event store
pub struct AggregateLoader<'a, A, E, S>
where
    A: Aggregate,
    S: EventStore<Event = E>,
{
    store: &'a S,
    aggregate_id: A::Id,
    handlers: Vec<Box<dyn Handler<A, E>>>,
    from_version: u64,
    _phantom: std::marker::PhantomData<E>,
}

impl<'a, A, E, S> AggregateLoader<'a, A, E, S>
where
    A: Aggregate,
    S: EventStore<Event = E>,
    E: Clone + Send + Sync + 'static,
{
    pub fn new(store: &'a S, aggregate_id: A::Id) -> Self {
        Self {
            store,
            aggregate_id,
            handlers: Vec::new(),
            from_version: 0,
            _phantom: std::marker::PhantomData,
        }
    }

    /// Add handlers to apply events
    pub fn with_handlers(mut self, handlers: Vec<Box<dyn Handler<A, E>>>) -> Self {
        self.handlers = handlers;
        self
    }

    /// Load from a specific version (for snapshots)
    pub fn from_version(mut self, version: u64) -> Self {
        self.from_version = version;
        self
    }

    /// Build the aggregate by replaying events
    pub async fn load(self) -> Result<A> {
        let events = self.store.load_events(self.aggregate_id.clone(), self.from_version).await?;

        let mut aggregate = A::default();

        for stored_event in events {
            // Apply each handler to the aggregate
            for handler in &self.handlers {
                handler.apply(&mut aggregate, &stored_event.data)?;
            }

            aggregate.set_version(stored_event.sequence);
        }

        Ok(aggregate)
    }
}
```

## Phase 2: Adapt Handler Macro (2-3 days)

### 2.1 Make handlers pure (remove async)

**File:** `crates/causal/src/handler/types.rs`

Update the `Handler` trait:

```rust
/// Handler trait for pure projections
pub trait Handler<A, E>: Send + Sync + 'static
where
    A: Aggregate,
    E: Clone + Send + Sync + 'static,
{
    /// Apply event to aggregate state
    fn apply(&self, state: &mut A, event: &E) -> Result<()>;

    /// Handler ID for debugging
    fn id(&self) -> &str;
}
```

### 2.2 Update macro to generate pure handlers

**File:** `crates/causal_core_macros/src/handler.rs`

Change generated code from:

```rust
// Old: async handler returning events
async fn execute(&self, ctx: Context<D>) -> Result<Option<Event>>
```

To:

```rust
// New: pure handler mutating state
fn apply(&self, state: &mut Aggregate, event: &Event) -> Result<()>
```

### 2.3 Keep Context optional (for projections with async)

```rust
#[handler(on = OrderEvent, extract(order_id, customer_id))]
async fn project_with_lookup(
    order_id: Uuid,
    customer_id: Uuid,
    state: &mut OrderView,
    ctx: Context<Deps>,  // Optional for projections
) -> Result<()> {
    state.customer_name = ctx.deps().db.get_name(customer_id).await?;
    Ok(())
}
```

## Phase 3: PostgreSQL Event Store (2-3 days)

### 3.1 Create events table schema

**File:** `docs/schema.sql`

```sql
-- Event store table (replaces current causal_events)
CREATE TABLE causal_event_store (
    -- Aggregate identification
    aggregate_id UUID NOT NULL,
    aggregate_type TEXT NOT NULL,

    -- Event sequence per aggregate
    sequence BIGINT NOT NULL,

    -- Event data
    event_type TEXT NOT NULL,
    event_data JSONB NOT NULL,

    -- Metadata
    timestamp TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    correlation_id UUID,
    causation_id UUID,

    -- Constraints
    PRIMARY KEY (aggregate_id, sequence),

    -- Index for loading events
    CONSTRAINT causal_event_store_sequence_check CHECK (sequence > 0)
);

-- Index for querying by type and time
CREATE INDEX idx_causal_event_store_type_time
    ON causal_event_store(aggregate_type, timestamp DESC);

-- Snapshots table for performance
CREATE TABLE causal_snapshots (
    aggregate_id UUID NOT NULL,
    aggregate_type TEXT NOT NULL,
    version BIGINT NOT NULL,
    state JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    PRIMARY KEY (aggregate_id, version)
);
```

### 3.2 Implement PostgresEventStore

**File:** `crates/causal-postgres/src/event_store.rs`

```rust
use causal_core::{EventStore, StoredEvent};
use sqlx::PgPool;

#[derive(Clone)]
pub struct PostgresEventStore<E> {
    pool: PgPool,
    _phantom: std::marker::PhantomData<E>,
}

impl<E> PostgresEventStore<E> {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            _phantom: std::marker::PhantomData,
        }
    }
}

#[async_trait]
impl<E> EventStore for PostgresEventStore<E>
where
    E: Clone + Serialize + DeserializeOwned + Send + Sync + 'static,
{
    type Event = E;

    async fn load_events<Id>(
        &self,
        aggregate_id: Id,
        from_version: u64,
    ) -> Result<Vec<StoredEvent<E>>>
    where
        Id: Clone + Send + Sync + 'static + Into<Uuid>,
    {
        let aggregate_id: Uuid = aggregate_id.into();

        let rows = sqlx::query!(
            r#"
            SELECT sequence, event_type, event_data, timestamp
            FROM causal_event_store
            WHERE aggregate_id = $1 AND sequence > $2
            ORDER BY sequence ASC
            "#,
            aggregate_id,
            from_version as i64,
        )
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|row| {
                let data: E = serde_json::from_value(row.event_data)?;
                Ok(StoredEvent {
                    sequence: row.sequence as u64,
                    timestamp: row.timestamp,
                    data,
                    event_type: row.event_type,
                })
            })
            .collect()
    }

    async fn append_events<Id>(
        &self,
        aggregate_id: Id,
        expected_version: u64,
        events: Vec<E>,
    ) -> Result<()>
    where
        Id: Clone + Send + Sync + 'static + Into<Uuid>,
    {
        let aggregate_id: Uuid = aggregate_id.into();
        let mut tx = self.pool.begin().await?;

        // Optimistic locking check
        let current_version = self.current_version(aggregate_id).await?.unwrap_or(0);
        if current_version != expected_version {
            return Err(anyhow!("Version conflict: expected {}, got {}", expected_version, current_version));
        }

        // Append events
        for (i, event) in events.into_iter().enumerate() {
            let sequence = expected_version + i as u64 + 1;
            let event_data = serde_json::to_value(&event)?;
            let event_type = std::any::type_name::<E>();

            sqlx::query!(
                r#"
                INSERT INTO causal_event_store
                    (aggregate_id, aggregate_type, sequence, event_type, event_data)
                VALUES ($1, $2, $3, $4, $5)
                "#,
                aggregate_id,
                event_type,
                sequence as i64,
                event_type,
                event_data,
            )
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(())
    }

    async fn current_version<Id>(&self, aggregate_id: Id) -> Result<Option<u64>>
    where
        Id: Clone + Send + Sync + 'static + Into<Uuid>,
    {
        let aggregate_id: Uuid = aggregate_id.into();

        let result = sqlx::query!(
            r#"
            SELECT MAX(sequence) as version
            FROM causal_event_store
            WHERE aggregate_id = $1
            "#,
            aggregate_id,
        )
        .fetch_one(&self.pool)
        .await?;

        Ok(result.version.map(|v| v as u64))
    }
}
```

## Phase 4: Update Examples (1-2 days)

### 4.1 Create new event sourcing example

**File:** `examples/event-sourced-order/src/main.rs`

```rust
use anyhow::Result;
use causal::{handler, Aggregate, AggregateLoader};
use causal_postgres::PostgresEventStore;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// Aggregate
#[derive(Default, Clone, Serialize, Deserialize)]
struct Order {
    id: Uuid,
    items: Vec<String>,
    total: f64,
    status: OrderStatus,
    version: u64,
}

#[derive(Default, Clone, Serialize, Deserialize)]
enum OrderStatus {
    #[default]
    Draft,
    Placed,
    Shipped,
}

impl Aggregate for Order {
    type Id = Uuid;

    fn id(&self) -> Uuid { self.id }
    fn version(&self) -> u64 { self.version }
    fn set_version(&mut self, v: u64) { self.version = v; }
}

// Events
#[derive(Clone, Serialize, Deserialize)]
enum OrderEvent {
    Created { order_id: Uuid },
    ItemAdded { item: String, total: f64 },
    Placed,
    Shipped,
}

// Handlers (projections)
#[handlers]
mod order_projections {
    use super::*;

    #[handler(on = OrderEvent, extract(order_id))]
    fn apply_created(order_id: Uuid, state: &mut Order) -> Result<()> {
        state.id = order_id;
        state.status = OrderStatus::Draft;
        Ok(())
    }

    #[handler(on = OrderEvent, extract(item, total))]
    fn apply_item_added(item: String, total: f64, state: &mut Order) -> Result<()> {
        state.items.push(item);
        state.total = total;
        Ok(())
    }

    #[handler(on = OrderEvent)]
    fn apply_placed(state: &mut Order) -> Result<()> {
        state.status = OrderStatus::Placed;
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let pool = sqlx::PgPool::connect("postgres://localhost/causal").await?;
    let store = PostgresEventStore::new(pool);

    let order_id = Uuid::new_v4();

    // Create order
    store.append_events(order_id, 0, vec![
        OrderEvent::Created { order_id }
    ]).await?;

    // Add items
    store.append_events(order_id, 1, vec![
        OrderEvent::ItemAdded { item: "Widget".into(), total: 99.99 }
    ]).await?;

    // Load aggregate
    let order: Order = store
        .aggregate(order_id)
        .with_handlers(order_projections::handlers())
        .load()
        .await?;

    println!("Order: {:?}", order);

    Ok(())
}
```

## Phase 5: Documentation (2-3 days)

### 5.1 Update README.md

- Change tagline: "Event Sourcing for Rust"
- Update examples to show aggregates + projections
- Remove orchestration examples
- Add CQRS examples

### 5.2 Write guides

- `docs/EVENT_SOURCING_GUIDE.md` - Comprehensive guide
- `docs/CQRS_PATTERNS.md` - Read model patterns
- `docs/COMMANDS.md` - Command pattern guide

## Migration Checklist

### Core Changes
- [ ] Add `Aggregate` trait
- [ ] Add `EventStore` trait
- [ ] Add `AggregateLoader` builder
- [ ] Update `Handler` trait for pure functions
- [ ] Modify `#[handler]` macro to generate pure handlers
- [ ] Support optional `Context` for async projections

### Backend Changes
- [ ] Create new event store schema
- [ ] Implement `PostgresEventStore`
- [ ] Add snapshot support
- [ ] Implement `MemoryEventStore`
- [ ] Update Kafka backend for event streaming

### Examples
- [ ] Create `event-sourced-order` example
- [ ] Create `cqrs-blog` example
- [ ] Create `bank-account` example (classic ES)
- [ ] Update all existing examples

### Documentation
- [ ] Update README.md
- [ ] Write EVENT_SOURCING_GUIDE.md
- [ ] Write CQRS_PATTERNS.md
- [ ] Write MIGRATION_FROM_ORCHESTRATION.md
- [ ] Update API docs

### Testing
- [ ] Unit tests for aggregate loading
- [ ] Integration tests for event store
- [ ] Optimistic locking tests
- [ ] Snapshot tests

## Breaking Changes

### What Breaks
1. `Engine` API changes completely
2. Handlers change from `async fn(...) -> Result<Event>` to `fn(..., &mut State) -> Result<()>`
3. No automatic event dispatch
4. Context no longer always available

### Migration Path
- Provide `causal-orchestration` crate for legacy users
- Feature flags: `event-sourcing` vs `orchestration`
- Clear migration guide with before/after examples

## Timeline Estimate

- **Phase 1 (Core Traits)**: 1-2 days
- **Phase 2 (Handler Macro)**: 2-3 days
- **Phase 3 (PostgreSQL)**: 2-3 days
- **Phase 4 (Examples)**: 1-2 days
- **Phase 5 (Docs)**: 2-3 days

**Total**: 8-13 days (~2 weeks)

## Open Questions

1. **Should we keep orchestration as a separate feature?**
   - Pro: No breaking changes for existing users
   - Con: Maintenance burden

2. **How to handle async projections (read models)?**
   - Current proposal: Optional `Context<Deps>` parameter
   - Makes handler `async` if Context present

3. **Command pattern: helper macros or just manual?**
   - Could add `#[command]` macro
   - Or keep it simple with structs + impl

4. **Snapshot strategy?**
   - Automatic every N events?
   - Manual snapshots?
   - Both?

5. **Event versioning/upcasting?**
   - Add now or later?
   - How to handle schema evolution?
