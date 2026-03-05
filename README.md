# Seesaw

**Event-driven orchestration for Rust.**

Seesaw is a lightweight runtime for building reactive systems with a simple **Event → Handler → Event** loop. It handles routing, aggregation, settlement, event sourcing, and journaled side effects.

```rust
use seesaw_core::{aggregators, handles, events, Context, Engine, Events};

#[aggregators(id = "order_id")]
mod order_aggregators {
    fn on_placed(order: &mut Order, event: OrderPlaced) {
        order.status = OrderStatus::Placed;
        order.total = event.total;
    }
    fn on_shipped(order: &mut Order, _event: OrderShipped) {
        order.status = OrderStatus::Shipped;
    }
}

#[handles]
mod order_handlers {
    async fn ship(event: OrderPlaced, ctx: Context<Deps>) -> Result<OrderShipped> {
        ctx.run(|| async {
            ctx.deps().shipping_api.ship(event.order_id).await
        }).await?;
        Ok(OrderShipped { order_id: event.order_id })
    }
    async fn notify(event: OrderShipped, ctx: Context<Deps>) -> Result<()> {
        ctx.run(|| async {
            ctx.deps().email.send(event.order_id).await
        }).await?;
        Ok(())
    }
}

let engine = Engine::new(deps)
    .with_aggregators(order_aggregators::aggregators())
    .with_handlers(order_handlers::handles());

engine.emit(OrderPlaced { order_id, total: 99.99 }).settled().await?;
```

## Install

```toml
[dependencies]
seesaw_core = "0.21"
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
anyhow = "1"
uuid = { version = "1", features = ["v4", "serde"] }
```

## Core Concepts

### Handlers

Handlers react to events, perform side effects, and return new events:

```rust
#[handles]
mod order_handlers {
    // Event type inferred from parameter — no #[handle] needed
    async fn ship(event: OrderPlaced, ctx: Context<Deps>) -> Result<OrderShipped> {
        ctx.run(|| async {
            ctx.deps().shipping_api.ship(event.order_id).await
        }).await?;
        Ok(OrderShipped { order_id: event.order_id })
    }

    // Use #[handle] for advanced features (extract, retry, etc.)
    #[handle(on = [OrderEvent::Placed], extract(order_id), id = "enqueue")]
    async fn enqueue(order_id: Uuid, ctx: Context<Deps>) -> Result<Events> {
        Ok(events![Enqueued { order_id }])
    }
}

let engine = Engine::new(deps).with_handlers(order_handlers::handles());
```

The `events![]` macro handles all return shapes:

```rust
Ok(events![])                              // No events
Ok(events![OrderShipped { order_id }])     // Single event
Ok(events![EventA { .. }, EventB { .. }])  // Multiple heterogeneous events
Ok(events![..items])                       // Fan-out batch from iterator
```

### Settlement

`engine.emit(event)` returns a lazy future. Await it for fire-and-forget, or chain `.settled()` to drive the full causal tree to completion:

```rust
engine.emit(event).await?;              // Publish only
engine.emit(event).settled().await?;    // Publish + settle all downstream handlers
```

### Aggregates

Aggregates maintain state by folding events. Define them with `Aggregate` + `Apply<E>`:

```rust
#[derive(Default, Clone, Serialize, Deserialize)]
struct Order { status: OrderStatus, total: f64 }

impl Aggregate for Order {
    fn aggregate_type() -> &'static str { "Order" }
}

impl Apply<OrderPlaced> for Order {
    fn apply(&mut self, event: OrderPlaced) {
        self.status = OrderStatus::Placed;
        self.total = event.total;
    }
}

impl Apply<OrderShipped> for Order {
    fn apply(&mut self, _event: OrderShipped) {
        self.status = OrderStatus::Shipped;
    }
}
```

Register aggregators and use **transition guards** to fire handlers only on specific state changes:

```rust
let engine = Engine::new(deps)
    .with_aggregator::<OrderPlaced, Order, _>(|e| e.order_id)
    .with_aggregator::<OrderShipped, Order, _>(|e| e.order_id)
    .with_handler(
        handler::on::<OrderShipped>()
            .extract(|e| Some(e.order_id))
            .transition::<Order, _>(|prev, next| {
                prev.status != OrderStatus::Shipped && next.status == OrderStatus::Shipped
            })
            .then(|order_id, ctx: Context<Deps>| async move {
                ctx.run(|| async {
                    ctx.deps().notify_shipped(order_id).await
                }).await?;
                Ok(events![])
            }),
    );
```

Or use the macro shorthand — ID specified once at the module level:

```rust
#[aggregators(id = "order_id")]
mod order_aggregators {
    use super::*;

    fn on_placed(order: &mut Order, event: OrderPlaced) {
        order.status = OrderStatus::Placed;
        order.total = event.total;
    }

    fn on_shipped(order: &mut Order, _event: OrderShipped) {
        order.status = OrderStatus::Shipped;
    }
}

let engine = Engine::new(deps)
    .with_aggregators(order_aggregators::aggregators());
```

For single-instance aggregates (no ID field needed), use `singleton`:

```rust
#[aggregators(singleton)]
mod pipeline_aggregators {
    fn on_step(stats: &mut RunStats, event: StepCompleted) {
        stats.event_count += 1;
    }
}
```

## Handler Configuration

### Filter and extract

```rust
// Filter — skip events that don't match
#[handler(on = OrderPlaced, filter = is_high_value, id = "ship_high_value")]
async fn ship_high_value(event: OrderPlaced, ctx: Context<Deps>) -> Result<OrderShipped> {
    Ok(OrderShipped { order_id: event.order_id })
}

// Extract — pull fields from enum variants
#[handler(on = [CrawlEvent::Ingested, CrawlEvent::Regenerated], extract(website_id, job_id), id = "enqueue")]
async fn enqueue(website_id: Uuid, job_id: Uuid, ctx: Context<Deps>) -> Result<EnqueuedEvent> {
    Ok(EnqueuedEvent { website_id })
}
```

### Retry, timeout, delay, priority

```rust
#[handler(on = PaymentRequested, id = "charge", retry = 3, timeout_secs = 30, priority = 1)]
async fn charge(event: PaymentRequested, ctx: Context<Deps>) -> Result<PaymentCharged> {
    ctx.run(|| async {
        ctx.deps().stripe.charge(event.order_id).await
    }).await?;
    Ok(PaymentCharged { order_id: event.order_id })
}
```

### Batch accumulation

Fan-out events and collect them back with `accumulate`:

```rust
#[handler(on = RowValidated, accumulate, id = "bulk_insert", window_timeout_secs = 5)]
async fn bulk_insert(batch: Vec<RowValidated>, ctx: Context<Deps>) -> Result<BatchInserted> {
    ctx.run(|| async {
        ctx.deps().db.bulk_insert(&batch).await
    }).await?;
    Ok(BatchInserted { count: batch.len() })
}
```

### DLQ terminal events

Map exhausted retries to a terminal event:

```rust
handler::on::<FailEvent>()
    .id("risky_op")
    .retry(3)
    .on_failure(|_event, info: ErrorContext| OperationFailed {
        error: info.error,
        attempts: info.attempts,
    })
    .then(|event, ctx| async move { /* ... */ })
```

### Observe all events

```rust
// Macro style
#[handler(on_any, id = "audit_log")]
async fn audit_log(event: AnyEvent, ctx: Context<Deps>) -> Result<()> {
    if let Some(order) = event.downcast::<OrderPlaced>() {
        println!("Order placed: {:?}", order.order_id);
    }
    Ok(())
}

// Builder style
handler::on_any()
    .id("audit_log")
    .then(|event: AnyEvent, ctx: Context<Deps>| async move {
        if let Some(order) = event.downcast::<OrderPlaced>() {
            println!("Order placed: {:?}", order.order_id);
        }
        Ok(events![])
    })
```

### Module registration

Group related handlers into a module. Bare async functions are auto-registered — `#[handle]` is only needed for advanced features:

```rust
#[handles]
mod order_handlers {
    use super::*;

    // Bare fn — event type inferred, id = "ship"
    async fn ship(event: OrderPlaced, ctx: Context<Deps>) -> Result<OrderShipped> {
        Ok(OrderShipped { order_id: event.order_id })
    }

    // Explicit #[handle] for extract, retry, etc.
    #[handle(on = [OrderEvent::Shipped], extract(order_id), id = "notify")]
    async fn notify(order_id: Uuid, ctx: Context<Deps>) -> Result<()> {
        ctx.run(|| async {
            ctx.deps().email.send(order_id).await
        }).await?;
        Ok(())
    }
}

let engine = Engine::new(deps).with_handlers(order_handlers::handles());
```

## Event Sourcing

Event persistence, aggregate hydration, and snapshots are built into the unified `Store` trait. The same store that drives the settle loop also persists events — no dual-write risk.

### Store trait (persistence methods)

The `Store` trait includes optional event persistence methods with default no-ops. Override them to enable durable event sourcing:

```rust
// These have default no-op implementations — override for persistence.
async fn append_event(&self, event: NewEvent) -> Result<u64>;
async fn load_stream(&self, aggregate_type: &str, aggregate_id: Uuid) -> Result<Vec<PersistedEvent>>;
async fn load_stream_from(&self, agg_type: &str, agg_id: Uuid, after: u64) -> Result<Vec<PersistedEvent>>;
async fn load_global_from(&self, after_position: u64, limit: usize) -> Result<Vec<PersistedEvent>>;
async fn load_snapshot(&self, aggregate_type: &str, aggregate_id: Uuid) -> Result<Option<Snapshot>>;
async fn save_snapshot(&self, snapshot: Snapshot) -> Result<()>;
```

Append is idempotent by `event_id`. Every event is persisted — not just those with aggregators.

### Auto-persist and hydration

Configure a store with persistence enabled and the engine persists **every** event to the global log and hydrates aggregates on cold access:

```rust
use seesaw_core::MemoryStore;

let engine = Engine::new(deps)
    .with_store(MemoryStore::with_persistence())
    .with_aggregator::<OrderPlaced, Order, _>(|e| e.order_id)
    .with_handler(on_order_placed());

// All events are persisted. Aggregate-scoped events get aggregate_type/aggregate_id.
// On restart, aggregates hydrate from the Store automatically.
engine.emit(OrderPlaced { order_id, total: 100 }).settled().await?;
```

`persist_event` is available for manual persistence with short type names (e.g. `"OrderPlaced"` not `"my_crate::events::OrderPlaced"`) so refactoring modules never breaks replay.

### Event metadata

Stamp application-level context on every persisted event with `with_event_metadata`. Metadata travels with the event through the Store, letting adapters pull fields like `run_id` or `schema_v` without holding state:

```rust
let engine = Engine::new(deps)
    .with_store(MemoryStore::with_persistence())
    .with_event_metadata(serde_json::json!({
        "run_id": "scrape-abc123",
        "schema_v": 1,
        "actor": "bot-7"
    }))
    .with_aggregator::<OrderPlaced, Order, _>(|e| e.order_id);
```

Metadata is available on both `NewEvent` and `PersistedEvent` as a `serde_json::Map<String, serde_json::Value>`. Without `with_event_metadata`, the map is empty.

### Snapshots

Snapshots accelerate cold-start hydration by saving aggregate state at a point-in-time, so only the delta needs replaying.

**Auto-checkpoint** — save snapshots automatically every N events:

```rust
let engine = Engine::new(deps)
    .with_store(my_store)
    .snapshot_every(100)
    .with_aggregator::<OrderPlaced, Order, _>(|e| e.order_id);
```

On cold start, the engine loads the latest snapshot and replays only events after it.

| Configuration | Behavior |
|---|---|
| Default store (no persistence overrides) | No persistence, no snapshots |
| Store with persistence | Events persisted, manual snapshots via `save_snapshot()` |
| Store with persistence + `snapshot_every(N)` | Auto-checkpoint every N events |

### Journaled side effects

`ctx.run()` journals closure results in the Store. On retry, completed steps are replayed from the journal instead of re-executing:

```rust
#[handler(on = OrderPlaced, id = "ship_order")]
async fn ship_order(event: OrderPlaced, ctx: Context<Deps>) -> Result<OrderShipped> {
    // Journaled: if this handler retries, the shipping API call won't re-execute
    let tracking_id: String = ctx.run(|| async {
        ctx.deps().shipping_api.ship(event.order_id).await
    }).await?;

    Ok(OrderShipped { order_id: event.order_id, tracking_id })
}
```

**How it works:**
- Each `run()` call gets a sequence number within the handler execution
- On first execution, the closure runs and its result is persisted to the Store
- On retry (after crash or error), journaled results are replayed — the closure is skipped
- Journal entries are cleared atomically when the handler completes successfully
- Errors are not journaled — they propagate normally and trigger the retry/DLQ path

**Determinism contract:** Code between `run()` calls must be deterministic. The same input event must produce the same sequence of `run()` calls. Non-determinism (random values, wall clock reads) between `run()` calls will break replay.

The return type must implement `Serialize + DeserializeOwned`. Store implementations provide the journal backend — the built-in `MemoryStore` includes a working implementation, and Postgres stores can use the same transaction for journal writes.

### Ephemeral sidecar (live dispatch optimization)

When an event is published or emitted, seesaw stashes the original typed object alongside the JSON payload. During the **live dispatch cycle**, handlers receive this original object directly — preserving `#[serde(skip)]` fields that would be lost through serialization.

On **replay or hydration** (e.g. after a crash), the ephemeral is `None` and handlers fall back to JSON deserialization. Skipped fields get their `Default` values, which is correct by design since durable state is the record of truth.

This is useful when events carry transient, non-serializable data (parsed structs, pre-computed results, file handles) that downstream handlers need during the same dispatch cycle but that shouldn't be persisted:

```rust
#[derive(Clone, Serialize, Deserialize)]
struct PageScraped {
    url: String,
    raw_html: String,

    /// Pre-parsed batches — available during live dispatch, empty on replay.
    #[serde(skip)]
    extracted_batches: Vec<Batch>,
}

// The scrape handler emits PageScraped with extracted_batches populated.
// The downstream dedup handler receives the original typed event (with batches intact)
// during live dispatch — no need to re-parse or stash in shared state.
```

**Semantics:**

| Path | Source | `#[serde(skip)]` fields |
|------|--------|------------------------|
| Live dispatch | Original typed object | Preserved |
| Replay / hydration | JSON deserialization | `Default` values |
| Store persistence | JSON payload only | Not persisted |

No code changes are needed to benefit — this is automatic for all events published via `engine.emit()` or returned from handlers via `events![]`.

## Durable Execution

Seesaw provides durable execution natively through its `Store` trait:

| Concern | MemoryStore (default) | Postgres Store |
|---------|----------------------|----------------|
| Handler execution | Direct call | Direct call |
| Side effect journaling | In-memory (lost on crash) | Durable (survives crash) |
| Aggregate state | In-memory DashMap | Hydrated from event log |
| Crash recovery | State lost | Replay from journal + event log |
| Handler retries | In-memory queue | Persistent queue with reclaim |

All durability features are built into the `Store` trait with default no-ops, so `MemoryStore` works out of the box for development and testing. Swap in a Postgres store for production durability — no code changes needed.

## Context API

Every handler receives a `Context<D>` with:

```rust
ctx.deps()              // Shared dependencies (&D)
ctx.handler_id()        // Handler identifier
ctx.event_id            // Current event's unique ID
ctx.correlation_id      // Workflow grouping ID
ctx.parent_event_id     // Parent event for causal tracking
ctx.idempotency_key()   // Deterministic key for deduplication
ctx.run(|| async { })   // Replay-safe side effect execution (journaled)
ctx.logger              // Structured logging (see below)
```

### Structured Logging

Handlers can emit structured log entries via `ctx.logger`. Entries are captured during execution and drained into `HandlerCompletion` / `HandlerDlq`, so Store implementations can persist them keyed by `(event_id, handler_id)`.

```rust
#[handler(on = OrderPlaced, id = "ship_order")]
async fn ship_order(event: OrderPlaced, ctx: Context<Deps>) -> Result<OrderShipped> {
    ctx.logger.info("Starting shipment");
    ctx.logger.debug_with("Order details", &serde_json::json!({
        "order_id": event.order_id,
        "total": event.total,
    }));

    let tracking_id: String = ctx.run(|| async {
        ctx.deps().shipping_api.ship(event.order_id).await
    }).await?;

    ctx.logger.info_with("Shipment created", &serde_json::json!({
        "tracking_id": &tracking_id,
    }));

    Ok(OrderShipped { order_id: event.order_id, tracking_id })
}
```

Available methods: `debug`, `debug_with`, `info`, `info_with`, `warn`, `warn_with`. The `_with` variants accept any `Serialize` value as structured data.

## Examples

```bash
cargo run --example simple-order
```

- **[simple-order](examples/simple-order)** — Order processing workflow
- **[http-fetcher](examples/http-fetcher)** — HTTP request pipeline with fan-out
- **[ai-summarizer](examples/ai-summarizer)** — AI text summarization with Claude

## Multi-Node Sync

Three primitives enable syncing events between seesaw instances:

1. **Idempotent append** — `Store::append_event` deduplicates by `event_id`. Appending the same event twice returns the existing position without inserting.

2. **Global log tailing** — `Store::load_global_from(after_position, limit)` returns events after a given position, enabling a follower node to poll for new events.

3. **Aggregate invalidation** — `Engine::invalidate_aggregate::<A>(id)` evicts cached aggregate state, forcing re-hydration from the Store on the next settle loop.

**Sync flow:**

```
Node B polls Node A:  load_global_from(cursor, 100)
                      ↓
For each event:       append_event(event)    ← idempotent, safe to re-apply
                      invalidate_aggregate   ← evict stale cache
                      ↓
Next settle loop:     hydrates from Store (includes foreign events)
```

## Architecture

```
Engine (routing, composition, settle loop)
  ├── Handlers (filter → extract → transition guard → execute → emit)
  ├── Aggregators (event folding, state transitions)
  └── Store (event/handler queue + persistent event log + snapshots + journal)
```

## License

MIT
