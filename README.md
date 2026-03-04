# Seesaw

**Event-driven orchestration for Rust.**

Seesaw is a lightweight runtime for building reactive systems with a simple **Event → Handler → Event** loop. It handles routing, aggregation, settlement, and event sourcing — and plugs into [Restate](https://restate.dev/) for durable execution with zero code changes.

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
seesaw_core = "0.18"
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

### Replay-safe side effects

Use `ctx.run()` for side effects that should be skipped during replay. With durable runtimes (Restate), results are journaled and replayed automatically:

```rust
#[handler(on = OrderPlaced, id = "ship_order")]
async fn ship_order(event: OrderPlaced, ctx: Context<Deps>) -> Result<OrderShipped> {
    let tracking_id: String = ctx.run(|| async {
        ctx.deps().shipping_api.ship(event.order_id).await
    }).await?;

    Ok(OrderShipped { order_id: event.order_id, tracking_id })
}
```

- **DirectRuntime (default):** Executes the closure inline, returns the result directly.
- **Durable runtime (Restate):** Journals the result. On replay, skips execution and returns the journaled value.

The return type must implement `Serialize + DeserializeOwned`. Custom runners implement the `SideEffectRunner` trait.

## Restate Integration

Seesaw's `Runtime` trait is the integration point for durable execution. Implement it to run inside [Restate](https://restate.dev/) — each handler invocation becomes a journal entry, and aggregate state persists in Restate's K/V store. User code stays identical.

```
Your code:    #[handler] async fn ship_order(...)
Seesaw:       route event → match handler → extract fields → check transition guard
Restate:      journal the invocation, replay on crash, persist K/V state
```

### Runtime trait

```rust
pub trait Runtime: Send + Sync {
    fn run(&self, handler_id: &str, execution: Pin<Box<dyn Future<...> + Send>>)
        -> Pin<Box<dyn Future<...> + Send>>;
    fn get_state(&self, key: &str) -> Option<Arc<dyn Any + Send + Sync>>;
    fn set_state(&self, key: &str, value: Arc<dyn Any + Send + Sync>);
}
```

### RestateRuntime

```rust
struct RestateRuntime<'a> {
    ctx: &'a restate_sdk::Context<'a>,
}

impl Runtime for RestateRuntime<'_> {
    fn run(
        &self,
        _handler_id: &str,
        execution: Pin<Box<dyn Future<Output = Result<Vec<EventOutput>>> + Send>>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<EventOutput>>> + Send>> {
        let ctx = self.ctx;
        Box::pin(async move { ctx.run(|| execution).await })
    }

    fn get_state(&self, key: &str) -> Option<Arc<dyn Any + Send + Sync>> {
        let json: serde_json::Value = self.ctx.get(key).await.ok().flatten()?;
        Some(Arc::new(json))
    }

    fn set_state(&self, key: &str, value: Arc<dyn Any + Send + Sync>) {
        if let Some(json) = value.downcast_ref::<serde_json::Value>() {
            self.ctx.set(key, json.clone());
        }
    }
}
```

Use it inside a Restate workflow:

```rust
impl OrderWorkflow for OrderWorkflowImpl {
    async fn run(&self, ctx: &mut WorkflowContext, input: OrderPlaced) -> Result<()> {
        let engine = Engine::new(deps)
            .with_runtime(RestateRuntime { ctx })
            .with_handler(ship_order())
            .with_handler(notify_shipped());

        engine.emit(input).settled().await?;
        Ok(())
    }
}
```

### What changes with Restate

| Concern | DirectRuntime (default) | RestateRuntime |
|---------|------------------------|----------------|
| Handler execution | Direct call | Journaled via `ctx.run()` |
| Aggregate state | In-memory DashMap | Restate K/V store |
| Crash recovery | State lost | Replay from journal |
| Exactly-once effects | Idempotency keys | Restate journal |
| Delayed execution | Seesaw delay queue | Restate timers |

**What doesn't change:** `#[handler]`, `Engine`, `events![]`, `.extract()`, `.transition()`, `.accumulate()`, `Aggregate`, `Store`. User code is backend-agnostic.

## Context API

Every handler receives a `Context<D>` with:

```rust
ctx.deps()              // Shared dependencies (&D)
ctx.handler_id()        // Handler identifier
ctx.event_id            // Current event's unique ID
ctx.correlation_id      // Workflow grouping ID
ctx.parent_event_id     // Parent event for causal tracking
ctx.idempotency_key()   // Deterministic key for deduplication
ctx.run(|| async { })   // Replay-safe side effect execution (journaled with Restate)
```

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
  ├── Store (event/effect queue + persistent event log + snapshots)
  └── Runtime (pluggable execution backend)
        ├── DirectRuntime (default: in-memory, pass-through)
        └── RestateRuntime (durable: journaled, crash-safe)
```

## License

MIT
