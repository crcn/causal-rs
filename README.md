# Seesaw

**Event-driven orchestration for Rust**

Build reactive systems with a simple **Event → Handler → Event** flow. Seesaw handles routing, composition, and settlement — plug in a `Runtime` for durable execution via [Restate](https://restate.dev/) or [Temporal](https://temporal.io/).

```rust
use seesaw_core::{handler, Context, Engine};
use serde::{Deserialize, Serialize};
use anyhow::Result;
use uuid::Uuid;

#[derive(Clone, Serialize, Deserialize)]
struct OrderPlaced { order_id: Uuid, total: f64 }

#[derive(Clone, Serialize, Deserialize)]
struct OrderShipped { order_id: Uuid }

#[derive(Clone)]
struct Deps;

let engine = Engine::new(Deps)
    .with_handler(
        handler::on::<OrderPlaced>()
            .id("ship_order")
            .then(|event, ctx: Context<Deps>| async move {
                Ok(OrderShipped { order_id: event.order_id })
            }),
    );

engine.dispatch(OrderPlaced { order_id: Uuid::new_v4(), total: 99.99 })
    .settled().await?;
```

## Architecture

```
Runtime (durability, retries, state)
  └── Seesaw (routing, composition, DSL, settle loop)
        └── Runtime.run() wraps each handler invocation
        └── Runtime.get_state() / set_state() manages aggregate state
```

- **Seesaw** owns: event routing, handler composition, DSL ergonomics, settle loop, aggregates
- **Runtime** owns: handler execution wrapping, state persistence
- **DirectRuntime** (default): pass-through execution, in-memory DashMap state
- **RestateRuntime** (custom): journals each handler via `ctx.run()`, state via Restate K/V

## Install

```toml
[dependencies]
seesaw_core = "0.13"
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
anyhow = "1"
uuid = { version = "1", features = ["v4", "serde"] }
```

## Handlers

Handlers react to events, perform side effects, and return new events.

### Declarative with `#[handler]` macro

```rust
use seesaw_core::{handler, Context};

#[handler(on = OrderPlaced, id = "ship_order")]
async fn ship_order(event: OrderPlaced, ctx: Context<Deps>) -> Result<OrderShipped> {
    ctx.deps().shipping_api.ship(event.order_id).await?;
    Ok(OrderShipped { order_id: event.order_id })
}

let engine = Engine::new(deps)
    .with_handler(ship_order());
```

### Builder API

```rust
handler::on::<OrderPlaced>()
    .id("ship_order")
    .then(|event, ctx: Context<Deps>| async move {
        ctx.deps().shipping_api.ship(event.order_id).await?;
        Ok(OrderShipped { order_id: event.order_id })
    })
```

### Extract fields from enum variants

```rust
#[handler(
    on = [CrawlEvent::Ingested, CrawlEvent::Regenerated],
    extract(website_id, job_id),
    id = "enqueue_extract"
)]
async fn enqueue_extract(
    website_id: Uuid,
    job_id: Uuid,
    ctx: Context<Deps>,
) -> Result<ExtractEnqueued> {
    Ok(ExtractEnqueued { website_id })
}
```

### Configuration

```rust
#[handler(
    on = PaymentRequested,
    id = "charge_payment",
    queued,
    retry = 3,
    timeout_secs = 30,
    priority = 1
)]
async fn charge_payment(event: PaymentRequested, ctx: Context<Deps>) -> Result<PaymentCharged> {
    ctx.deps().stripe.charge(event.order_id).await?;
    Ok(PaymentCharged { order_id: event.order_id })
}
```

Options: `id`, `queued`, `retry`, `timeout_secs`, `timeout_ms`, `delay_secs`, `delay_ms`, `priority`.

### Module registration

```rust
#[handlers]
mod order_handlers {
    use super::*;

    #[handler(on = OrderPlaced, id = "ship")]
    async fn ship(event: OrderPlaced, ctx: Context<Deps>) -> Result<OrderShipped> {
        Ok(OrderShipped { order_id: event.order_id })
    }

    #[handler(on = OrderShipped, id = "notify")]
    async fn notify(event: OrderShipped, ctx: Context<Deps>) -> Result<()> {
        ctx.deps().email.send(event.order_id).await?;
        Ok(())
    }
}

let engine = Engine::new(deps)
    .with_handlers(order_handlers::handlers());
```

### Batch processing with `accumulate`

```rust
#[handler(on = RowValidated, accumulate, id = "bulk_insert", window_timeout_secs = 5)]
async fn bulk_insert(batch: Vec<RowValidated>, ctx: Context<Deps>) -> Result<BatchInserted> {
    ctx.deps().db.bulk_insert(&batch).await?;
    Ok(BatchInserted { count: batch.len() })
}
```

### Observer (all events)

```rust
handler::on_any()
    .id("logger")
    .then(|event, ctx: Context<Deps>| async move {
        println!("event: {:?}", event.type_id);
        Ok(())
    })
```

## Aggregates

Maintain in-memory aggregate state with transition guards for state-driven handler logic. Aggregate state is managed by the `Runtime`.

```rust
use seesaw_core::{Aggregate, Apply};

#[derive(Default, Clone, Serialize, Deserialize)]
struct Order {
    status: OrderStatus,
    total: f64,
}

#[derive(Default, Clone, Serialize, Deserialize, PartialEq)]
enum OrderStatus { #[default] Draft, Placed, Shipped }

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

Register aggregators and use transition guards:

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
                ctx.deps().notify_shipped(order_id).await?;
                Ok(())
            }),
    );
```

With macros:

```rust
#[aggregators]
mod order_aggregators {
    use super::*;

    #[aggregator(id = "order_id")]
    fn on_placed(order: &mut Order, event: OrderPlaced) {
        order.status = OrderStatus::Placed;
        order.total = event.total;
    }

    #[aggregator(id = "order_id")]
    fn on_shipped(order: &mut Order, _event: OrderShipped) {
        order.status = OrderStatus::Shipped;
    }
}

let engine = Engine::new(deps)
    .with_aggregators(order_aggregators::aggregators());
```

## Settlement

Drive the full causal tree to completion:

```rust
// Fire-and-forget
engine.dispatch(event).await?;

// Wait for all handlers to complete
engine.dispatch(event).settled().await?;
```

## Runtime Trait

The `Runtime` trait is the integration point for durable execution backends. Three primitives:

```rust
pub trait Runtime: Send + Sync {
    /// Wrap a handler invocation (journaling for Restate, pass-through for direct).
    fn run(&self, handler_id: &str, execution: Pin<Box<dyn Future<...> + Send>>)
        -> Pin<Box<dyn Future<...> + Send>>;

    /// Get aggregate state by key.
    fn get_state(&self, key: &str) -> Option<Arc<dyn Any + Send + Sync>>;

    /// Set aggregate state by key.
    fn set_state(&self, key: &str, value: Arc<dyn Any + Send + Sync>);
}
```

### DirectRuntime (default)

Pass-through execution, in-memory state via DashMap. Used automatically — zero config needed.

```rust
// Engine uses DirectRuntime by default
let engine = Engine::new(deps);

// Or explicitly
let engine = Engine::new(deps)
    .with_runtime(DirectRuntime::new());
```

### Restate Integration

[Restate](https://restate.dev/) provides durable execution via journaled replay. Seesaw runs *inside* Restate — each handler becomes a journal entry. User code is unchanged.

```
User writes:     #[handler] async fn ship_order(...)
Seesaw does:     route event → handler, extract fields, transition guard
Restate does:    journal the handler invocation, replay on crash
```

A `RestateRuntime` wraps each handler in `restate_ctx.run()` and uses Restate's K/V store for aggregate state:

```rust
use std::pin::Pin;
use std::future::Future;
use anyhow::Result;
use seesaw_core::{Runtime, handler::EventOutput};

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
        Box::pin(async move {
            ctx.run(|| execution).await
        })
    }

    fn get_state(&self, key: &str) -> Option<Arc<dyn Any + Send + Sync>> {
        // Read JSON from Restate K/V, wrap as Arc<dyn Any>
        let json: serde_json::Value = self.ctx.get(key).await.ok().flatten()?;
        Some(Arc::new(json))
    }

    fn set_state(&self, key: &str, value: Arc<dyn Any + Send + Sync>) {
        // Aggregator passes concrete types; serialize via aggregator closures
        // or downcast to serde_json::Value if pre-serialized
        if let Some(json) = value.downcast_ref::<serde_json::Value>() {
            self.ctx.set(key, json.clone());
        }
    }
}
```

Use it inside a Restate workflow handler:

```rust
impl OrderWorkflow for OrderWorkflowImpl {
    async fn run(&self, ctx: &mut WorkflowContext, input: OrderPlaced) -> Result<()> {
        let engine = Engine::new(deps)
            .with_runtime(RestateRuntime { ctx })
            .with_handler(ship_order())
            .with_handler(notify_shipped());

        engine.dispatch(input).settled().await?;
        Ok(())
    }
}
```

**What changes with Restate:**

| Concern | DirectRuntime | RestateRuntime |
|---------|--------------|----------------|
| Handler execution | Direct call | Journaled via `ctx.run()` |
| Aggregate state | In-memory DashMap | Restate K/V store |
| Crash recovery | State lost | Replay from journal |
| Exactly-once side effects | Idempotency keys | Restate journal |
| Delayed execution | Seesaw delay queue | Restate timers |

**What doesn't change:** `#[handler]`, `Engine`, `handler::on()`, `.extract()`, `.transition()`, `.accumulate()`, `Aggregate`. User code is identical.

### Custom Runtimes

The trait is simple enough for any strategy:

```rust
/// Dry-run runtime — records calls without executing
struct DryRunRuntime {
    calls: Arc<Mutex<Vec<String>>>,
}

impl Runtime for DryRunRuntime {
    fn run(
        &self,
        handler_id: &str,
        _execution: Pin<Box<dyn Future<Output = Result<Vec<EventOutput>>> + Send>>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<EventOutput>>> + Send>> {
        let calls = self.calls.clone();
        let id = handler_id.to_string();
        Box::pin(async move {
            calls.lock().unwrap().push(id);
            Ok(Vec::new())
        })
    }

    fn get_state(&self, _key: &str) -> Option<Arc<dyn Any + Send + Sync>> { None }
    fn set_state(&self, _key: &str, _value: Arc<dyn Any + Send + Sync>) {}
}
```

## Context API

```rust
ctx.deps()              // Shared dependencies
ctx.event_id            // Current event's unique ID
ctx.correlation_id      // Workflow grouping ID
ctx.parent_event_id     // Parent event for causal tracking
ctx.idempotency_key()   // Deterministic key for deduplication
ctx.handler_id()        // Handler identifier
```

## Examples

```bash
cargo run --example simple-order
```

- **[simple-order](examples/simple-order)** — Basic order processing workflow
- **[http-fetcher](examples/http-fetcher)** — HTTP request workflow with retries
- **[ai-summarizer](examples/ai-summarizer)** — AI text summarization pipeline

## Design Philosophy

1. **Events are facts** — Immutable, past-tense descriptions of what happened
2. **Event → Handler → Event** — Simple, direct, composable
3. **Runtime owns the lifecycle** — Execute handlers, manage state, handle journaling
4. **Engine declares, Runtime executes** — Engine routes events and declares aggregates; Runtime wraps execution and persists state
5. **User code is backend-agnostic** — Same `#[handler]` works with DirectRuntime or RestateRuntime
6. **Settle on demand** — `engine.dispatch(event).settled().await` drives the full causal tree

## License

MIT
