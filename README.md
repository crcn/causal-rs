# Seesaw

**Event-driven orchestration for Rust**

Build reactive systems with a simple **Event → Handler → Event** flow. Named after the playground equipment that balances back and forth — representing the continuous flow of events through the system.

```rust
use seesaw_core::{handler, Context, Engine};
use seesaw_memory::MemoryBackend;
use serde::{Deserialize, Serialize};
use anyhow::Result;

// Events are facts - what happened
#[derive(Clone, Serialize, Deserialize)]
enum OrderEvent {
    Placed { order_id: Uuid, total: f64 },
    Shipped { order_id: Uuid },
    Delivered { order_id: Uuid },
}

#[derive(Clone)]
struct Deps {
    shipping_api: ShippingApi,
}

// Define handlers with the #[handler] macro
#[handler(on = OrderEvent, extract(order_id), id = "ship_order")]
async fn ship_order(order_id: Uuid, ctx: Context<Deps>) -> Result<OrderEvent> {
    ctx.deps().shipping_api.ship(order_id).await?;
    Ok(OrderEvent::Shipped { order_id })
}

#[tokio::main]
async fn main() -> Result<()> {
    let engine = Engine::new(deps, MemoryBackend::new())
        .with_handler(ship_order());

    // Dispatch events
    engine.dispatch(OrderEvent::Placed { order_id, total: 99.99 }).await?;
    Ok(())
}
```

## Features

- **🎯 Event-Driven**: Events are the only signals — immutable facts about what happened
- **⚡ High Performance**: 50k-100k events/sec with Kafka backend
- **🔄 Handler Pattern**: React to events, perform IO, return new events
- **✨ Declarative Macros**: Clean `#[handler]` syntax for defining handlers
- **🎨 Flexible Backends**: In-memory, PostgreSQL, or Kafka
- **📈 Horizontal Scaling**: Scale via Kafka partitions and consumer groups
- **🔒 Exactly-Once**: Idempotency and atomic commits prevent duplicate processing
- **🔍 Observable**: Real-time visualization with Seesaw Insight
- **🚀 Simple API**: Declarative macros or closure-based builder pattern

## Architecture

```
Event → Handler → Event → Handler → ... (until settled)
          ↓
      Async IO, side effects
          ↓
      Return new Event
```

**Core Principle**: Events flow through handlers. Handlers perform side effects and return new events. Simple, direct, composable.

## Quick Start

Add to your `Cargo.toml`:

```toml
[dependencies]
seesaw_core = "0.10.3"
seesaw-memory = "0.10.3"   # For in-memory backend
# OR
seesaw-postgres = "0.10.3" # For durable PostgreSQL backend
# OR
seesaw-kafka = "0.11.0"    # For high-throughput Kafka backend

tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
anyhow = "1"
uuid = { version = "1", features = ["v4", "serde"] }
```

### Basic Example with `#[handler]`

The `#[handler]` macro provides a clean, declarative way to define handlers:

```rust
use anyhow::Result;
use seesaw_core::{handler, Context, Engine};
use seesaw_memory::MemoryBackend;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// Define events (what happened)
#[derive(Debug, Clone, Serialize, Deserialize)]
enum OrderEvent {
    Placed { order_id: Uuid, total: f64 },
    Shipped { order_id: Uuid },
    Delivered { order_id: Uuid },
}

// Define dependencies (shared services)
#[derive(Clone)]
struct Deps {
    shipping_api: ShippingApi,
    email_service: EmailService,
}

// Handler: Placed → Shipped
#[handler(on = OrderEvent, extract(order_id), id = "ship_order")]
async fn ship_order(order_id: Uuid, ctx: Context<Deps>) -> Result<OrderEvent> {
    ctx.deps().shipping_api.ship(order_id).await?;
    Ok(OrderEvent::Shipped { order_id })
}

// Handler: Shipped → Delivered
#[handler(on = OrderEvent, extract(order_id), id = "notify_shipped")]
async fn notify_shipped(order_id: Uuid, ctx: Context<Deps>) -> Result<OrderEvent> {
    ctx.deps().email_service.send(order_id, "Shipped!").await?;
    Ok(OrderEvent::Delivered { order_id })
}

#[tokio::main]
async fn main() -> Result<()> {
    let backend = MemoryBackend::new();
    let deps = Deps {
        shipping_api: ShippingApi::new(),
        email_service: EmailService::new(),
    };

    // Build engine with handlers (each #[handler] generates a function)
    let engine = Engine::new(deps, backend)
        .with_handler(ship_order())
        .with_handler(notify_shipped());

    // Dispatch event
    let order_id = Uuid::new_v4();
    engine.dispatch(OrderEvent::Placed { order_id, total: 99.99 }).await?;
    // Engine automatically processes: Placed → Shipped → Delivered

    Ok(())
}
```

## Handler Pattern

Handlers are the core abstraction in Seesaw. They react to events, perform side effects, and return new events.

### Declarative Handlers with `#[handler]`

The `#[handler]` macro provides a clean way to define handlers:

```rust
use seesaw_core::{handler, Context};

// Basic handler that extracts a field
#[handler(on = OrderEvent, extract(order_id))]
async fn process_order(order_id: Uuid, ctx: Context<Deps>) -> Result<OrderEvent> {
    ctx.deps().process(order_id).await?;
    Ok(OrderEvent::Processed { order_id })
}
```

**Macro breakdown:**
1. **`on = Event`** - Listen for specific event type
2. **`extract(field1, field2, ...)`** - Extract fields from matching events
3. Function receives extracted fields directly

### Handler Configuration

Configure handlers with retry, timeout, priority, and more:

```rust
#[handler(
    on = PaymentEvent,
    extract(order_id, total),
    id = "charge_payment",
    retry = 3,
    timeout_secs = 30,
    priority = 1,
    delay_secs = 60
)]
async fn charge_payment(
    order_id: Uuid,
    total: f64,
    ctx: Context<Deps>
) -> Result<PaymentEvent> {
    ctx.deps().stripe.charge(order_id, total).await?;
    Ok(PaymentEvent::Charged { order_id })
}
```

**Configuration options:**
- `id = "name"` - Custom handler identifier (for tracing/debugging)
- `retry = N` - Max retry attempts on failure
- `timeout_secs = N` / `timeout_ms = N` - Execution timeout
- `delay_secs = N` / `delay_ms = N` - Delay before execution
- `priority = N` - Execution priority (lower = higher priority)
- `queued` - Force queued execution (required when using retry/timeout/delay)

### Multiple Handlers, Same Event

Multiple handlers can react to the same event (fan-out pattern):

```rust
// Handler 1: Update inventory
#[handler(on = OrderEvent, extract(order_id, items), id = "reserve_inventory")]
async fn reserve_inventory(
    order_id: Uuid,
    items: Vec<Item>,
    ctx: Context<Deps>
) -> Result<InventoryEvent> {
    ctx.deps().inventory.reserve(&items).await?;
    Ok(InventoryEvent::Reserved { order_id })
}

// Handler 2: Charge payment
#[handler(on = OrderEvent, extract(order_id, total), id = "charge_payment")]
async fn charge_payment(
    order_id: Uuid,
    total: f64,
    ctx: Context<Deps>
) -> Result<PaymentEvent> {
    ctx.deps().payment.charge(total).await?;
    Ok(PaymentEvent::Charged { order_id })
}

// Handler 3: Send notification (returns () = no new event)
#[handler(on = OrderEvent, extract(order_id), id = "send_notification")]
async fn send_notification(order_id: Uuid, ctx: Context<Deps>) -> Result<()> {
    ctx.deps().email.send(order_id, "Order received!").await?;
    Ok(())
}

let engine = Engine::new(deps, backend)
    .with_handler(reserve_inventory())
    .with_handler(charge_payment())
    .with_handler(send_notification());
```

### Handling Specific Enum Variants

Match specific enum variants with the `on = [...]` syntax:

```rust
#[handler(
    on = [OrderEvent::Placed, OrderEvent::Updated],
    extract(order_id, total),
    id = "process_order_changes"
)]
async fn process_order_changes(
    order_id: Uuid,
    total: f64,
    ctx: Context<Deps>
) -> Result<OrderEvent> {
    // Only runs for Placed or Updated variants
    ctx.deps().process_order(order_id, total).await?;
    Ok(OrderEvent::Processed { order_id })
}
```

### Batch Processing with `accumulate`

Use `accumulate` to process events in batches:

```rust
#[handler(on = ImportEvent, accumulate, id = "batch_import", window_timeout_secs = 5)]
async fn batch_import(batch: Vec<ImportEvent>, ctx: Context<Deps>) -> Result<()> {
    // Accumulates all ImportEvents, processes them together
    let items: Vec<_> = batch.into_iter()
        .filter_map(|e| match e {
            ImportEvent::RowParsed { data, .. } => Some(data),
            _ => None,
        })
        .collect();

    ctx.deps().bulk_insert(&items).await?;
    Ok(())
}
```

### Module-level Registration

Use `#[handlers]` to automatically generate registration functions:

```rust
#[handlers]
mod order_handlers {
    use super::*;

    #[handler(on = OrderEvent, extract(order_id), id = "ship")]
    async fn ship_order(order_id: Uuid, ctx: Context<Deps>) -> Result<OrderEvent> {
        ctx.deps().ship(order_id).await?;
        Ok(OrderEvent::Shipped { order_id })
    }

    #[handler(on = OrderEvent, extract(order_id), id = "email")]
    async fn email_customer(order_id: Uuid, ctx: Context<Deps>) -> Result<()> {
        ctx.deps().email.send(order_id).await?;
        Ok(())
    }
}

// Register all handlers at once
let engine = Engine::new(deps, backend)
    .with_handlers(order_handlers::handlers());
```

## Backends

Seesaw supports multiple backends for different use cases:

### In-Memory Backend (Development)

Fast, ephemeral, perfect for development and testing.

```rust
use seesaw_memory::MemoryBackend;

let backend = MemoryBackend::new();
let engine = Engine::new(deps, backend);
```

**Use when:**
- Development and testing
- No durability needed
- Latency < 1ms is critical

### PostgreSQL Backend (Production)

Durable, ACID transactions, queue-backed execution, with built-in Dead Letter Queue (DLQ).

```rust
use seesaw_postgres::{PostgresStore, PostgresBackend, DeadLetterQueue};
use sqlx::postgres::PgPoolOptions;

let pool = PgPoolOptions::new()
    .max_connections(20)
    .connect("postgres://localhost/seesaw")
    .await?;

let store = PostgresStore::new(pool.clone());
let backend = PostgresBackend::new(store);
let engine = Engine::new(deps, backend);

// Access DLQ for failed handlers
let dlq = DeadLetterQueue::new(pool);
let failed_entries = dlq.list(50).await?;
```

**Use when:**
- Production deployments
- Durability required
- ~1-5k events/sec throughput
- Single-node simplicity preferred
- Need DLQ for handling persistent failures

**PostgreSQL-Specific Features:**
- **Dead Letter Queue**: Handlers that exhaust retries are moved to the DLQ for manual inspection and replay
- **ACID Guarantees**: Full transactional consistency across event processing
- **LISTEN/NOTIFY**: Real-time event streaming via PostgreSQL's pub/sub

### Kafka Backend (High Scale)

High throughput, horizontal scaling, event replay capabilities.

```rust
use seesaw_kafka::{KafkaBackend, KafkaBackendConfig};
use seesaw_postgres::PostgresStore;

// PostgreSQL for coordination state
let pool = PgPoolOptions::new()
    .connect("postgres://localhost/seesaw")
    .await?;
let store = PostgresStore::new(pool);

// Kafka for event streaming
let kafka_config = KafkaBackendConfig::new(vec!["localhost:9092".to_string()])
    .with_topic_events("seesaw.events")
    .with_num_partitions(16);

let backend = KafkaBackend::new(kafka_config, store)?;
let engine = Engine::new(deps, backend);
```

**Use when:**
- High throughput (50k-100k events/sec)
- Horizontal scaling needed
- Event replay required
- Cross-platform integration
- Multiple consumers need same events

**Performance comparison:**

| Backend    | Throughput      | Scaling    | Durability | Complexity |
|------------|-----------------|------------|------------|------------|
| Memory     | ~100k evt/sec   | Vertical   | ❌ None    | ⭐ Simple  |
| PostgreSQL | ~1-5k evt/sec   | Vertical   | ✅ ACID    | ⭐⭐ Medium |
| Kafka      | ~50-100k evt/sec| Horizontal | ✅ Durable | ⭐⭐⭐ Complex |

See [`crates/seesaw-kafka/README.md`](crates/seesaw-kafka/README.md) for Kafka backend details.

## Context API

Handlers receive a `Context<Deps>` that provides access to dependencies:

```rust
#[handler(on = MyEvent, extract(data), id = "process")]
async fn process(data: Data, ctx: Context<Deps>) -> Result<MyEvent> {
    // Access shared dependencies
    ctx.deps().database.query(...).await?;
    ctx.deps().api_client.call(...).await?;

    Ok(MyEvent::Processed)
}
```

**Context methods:**
- `ctx.deps()` - Access shared dependencies (database, APIs, config, etc.)

## Real-time Observability (Seesaw Insight)

Visualize your workflows in real-time with the Insight server:

```bash
# Start PostgreSQL
docker run -d -p 5432:5432 -e POSTGRES_PASSWORD=postgres postgres

# Run migrations
cd crates/seesaw-postgres
cargo run --example migrate

# Start insight server
export DATABASE_URL="postgres://postgres:postgres@localhost/seesaw"
cargo run -p seesaw-insight

# Open http://localhost:3000
```

**Features:**
- 📊 **Live Stream**: Real-time view of all workflow events
- 🌳 **Tree Visualization**: Interactive causality tree showing event relationships
- 📈 **Real-time Stats**: Total events, active handlers, recent activity
- 🔄 **Dual Protocols**: WebSocket and SSE support

## Examples

The repository includes several examples demonstrating different patterns:

- **[simple-order](examples/simple-order)** - Basic order processing workflow
- **[http-fetcher](examples/http-fetcher)** - HTTP request workflow with retries
- **[ai-summarizer](examples/ai-summarizer)** - AI text summarization pipeline
- **[batch-processor](examples/batch-processor)** - Bulk data processing with joins
- **[workflow-status](examples/workflow-status)** - Status tracking and queries
- **[insight-demo](examples/insight-demo)** - Real-time observability demo

Run any example:

```bash
cargo run --example simple-order
```

## Workspace Structure

This repository is organized as a Cargo workspace:

- **[seesaw_core](crates/seesaw)** - Core event-driven runtime and handler system (backend-agnostic)
- **[seesaw-postgres](crates/seesaw-postgres)** - PostgreSQL backend (durable, ACID, includes DLQ implementation)
- **[seesaw-kafka](crates/seesaw-kafka)** - Kafka backend (high throughput, horizontal scaling)
- **[seesaw-memory](crates/seesaw-memory)** - In-memory backend (fast, ephemeral)
- **[seesaw-insight](crates/seesaw-insight)** - Real-time observability and visualization

### Architecture Philosophy

**Backend-Agnostic Core**: The core crate (`seesaw_core`) is intentionally decoupled from any specific backend implementation. This means:

- Core types (events, handlers, context) have no database dependencies
- Backend-specific features (like `DeadLetterQueue`) live in their respective backend crates
- The `HandlerContext` trait allows custom backends to provide enhanced capabilities
- SQLx is an optional dependency in core, only needed when using database-backed backends

This architecture enables:
- **Flexibility**: Swap backends without touching business logic
- **Minimal Dependencies**: Core stays lightweight with minimal dependencies
- **Custom Backends**: Build your own backend (e.g., Restate, Redis) by implementing the backend traits
- **Pluggable Execution**: The `HandlerRunner` trait lets backends wrap individual handler calls
- **Testing**: Use in-memory backend for fast tests, PostgreSQL/Kafka for integration tests

### Handler Runner

Every handler invocation flows through a `HandlerRunner`, a trait that wraps `effect.call_handler()`:

```
executor.execute_event() → runner.run(handler_future) → handler body
```

The default `DirectRunner` is a pass-through — identical to calling the handler directly. Custom runners can wrap handler calls for durable execution, dry-run testing, or tracing:

```rust
use seesaw_core::{HandlerRunner, DirectRunner};

// DirectRunner (default) — pass-through, zero overhead
static RUNNER: DirectRunner = DirectRunner;

// Custom runner example — wrap each handler call
struct TracingRunner;

impl HandlerRunner for TracingRunner {
    fn run(
        &self,
        handler_id: &str,
        execution: Pin<Box<dyn Future<Output = Result<Vec<EventOutput>>> + Send>>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<EventOutput>>> + Send>> {
        Box::pin(async move {
            tracing::info!("Starting handler: {}", handler_id);
            let result = execution.await;
            tracing::info!("Finished handler: {}", handler_id);
            result
        })
    }
}
```

This enables future backends like [Restate](https://restate.dev/) where each handler call is wrapped in `restate_ctx.run()` for journaled replay — the user writes zero Restate code because seesaw's handler decomposition pattern *is* the journaling strategy.

## Core Concepts

### Events

Events are immutable facts describing what happened. Any type that is `Clone + Send + Sync + Serialize + Deserialize + 'static` can be an event.

```rust
#[derive(Clone, Serialize, Deserialize)]
enum UserEvent {
    SignupRequested { email: String, name: String },
    SignedUp { user_id: Uuid, email: String },
    Verified { user_id: Uuid },
    Deleted { user_id: Uuid },
}
```

**Event naming conventions:**
- Use past tense (what happened, not what should happen)
- Be specific and descriptive
- Include relevant identifiers

### Handlers

Handlers are async functions that react to events, perform IO, and return new events:

```rust
#[handler(on = UserEvent, extract(email, name), id = "create_user")]
async fn create_user(
    email: String,
    name: String,
    ctx: Context<Deps>
) -> Result<UserEvent> {
    // Perform IO
    let user = ctx.deps().db.create_user(&email, &name).await?;

    // Return new event
    Ok(UserEvent::SignedUp {
        user_id: user.id,
        email,
    })
}
```

**Key properties:**
- Async (can perform IO)
- Access dependencies via `ctx.deps()`
- Return `Result<Event>` or `Result<()>`
- Can fail and retry (if configured)
- Declarative syntax with `#[handler]` macro

### Dependencies

Dependencies are shared services injected into the engine:

```rust
#[derive(Clone)]
struct Deps {
    db: Arc<Database>,
    api_client: Arc<ApiClient>,
    config: Arc<Config>,
}

let engine = Engine::new(deps, backend);
```

**Best practices:**
- Wrap expensive-to-clone types in `Arc<>`
- Keep dependencies immutable
- Use connection pools for databases
- Share HTTP clients

## Design Philosophy

1. **Events are Facts**: Immutable descriptions of what happened
2. **Event → Handler → Event**: Simple, direct flow
3. **Handlers Return Events**: No imperative "emit" calls, just return values
4. **Async All The Way**: Handlers can perform async IO
5. **Composition Over Inheritance**: Build complex workflows from simple handlers
6. **Pluggable Backends**: Choose the right backend for your use case
7. **Pluggable Execution**: The `HandlerRunner` trait wraps handler calls, enabling durable execution (Restate), dry-run testing, and tracing without changing user code

## Testing

Test handlers using the in-memory backend:

```rust
#[handler(on = OrderEvent, extract(order_id), id = "ship_order")]
async fn ship_order(order_id: Uuid, ctx: Context<MockDeps>) -> Result<OrderEvent> {
    ctx.deps().ship(order_id).await?;
    Ok(OrderEvent::Shipped { order_id })
}

#[tokio::test]
async fn test_order_processing() {
    let deps = MockDeps::new();
    let backend = MemoryBackend::new();

    let engine = Engine::new(deps.clone(), backend)
        .with_handler(ship_order());

    let order_id = Uuid::new_v4();
    engine.dispatch(OrderEvent::Placed { order_id, total: 99.99 }).await?;

    // Verify side effects
    assert!(deps.was_shipped(order_id));
}
```

## Guarantees

**Memory Backend:**
- ✅ Event ordering within single engine instance
- ✅ At-most-once handler execution
- ❌ No durability (events lost on crash)
- ❌ No distribution (single process only)

**PostgreSQL Backend:**
- ✅ Durable (events survive crashes)
- ✅ Distributed (multiple workers can process queue)
- ✅ Exactly-once handler execution (via idempotency)
- ✅ ACID transactions
- ❌ Limited throughput (~1-5k events/sec)

**Kafka Backend:**
- ✅ High throughput (50k-100k events/sec)
- ✅ Horizontal scaling (via partitions)
- ✅ Exactly-once handler execution (via idempotency)
- ✅ Event replay and time-travel
- ✅ Cross-platform integration
- ❌ Operational complexity (requires Kafka cluster)

## Documentation

- **[Kafka Backend Guide](crates/seesaw-kafka/README.md)** - High-throughput Kafka backend
- **[PostgreSQL Backend Guide](crates/seesaw-postgres/README.md)** - Durable PostgreSQL backend
- **[Insight Guide](crates/seesaw-insight/README.md)** - Real-time observability

## Alternative: Builder API

For more dynamic or programmatic handler creation, use the builder API with `handler::on()`:

### Basic Builder Pattern

```rust
use seesaw_core::{handler, Context};

let engine = Engine::new(deps, backend)
    .with_handler(
        handler::on::<OrderEvent>()
            .extract(|e| match e {
                OrderEvent::Placed { order_id, .. } => Some(*order_id),
                _ => None,
            })
            .then(|order_id, ctx: Context<Deps>| async move {
                ctx.deps().process(order_id).await?;
                Ok(OrderEvent::Processed { order_id })
            })
    );
```

### Builder with Configuration

```rust
handler::on::<PaymentEvent>()
    .id("charge_payment")
    .retry(3)
    .timeout(Duration::from_secs(30))
    .priority(1)
    .then(|event, ctx: Context<Deps>| async move {
        ctx.deps().stripe.charge(&event).await?;
        Ok(PaymentEvent::Charged { order_id: event.order_id })
    })
```

### Multiple Event Variants

```rust
handler::on::<OrderEvent>()
    .extract(|e| match e {
        OrderEvent::Placed { order_id, total } | OrderEvent::Updated { order_id, total } => {
            Some((order_id, total))
        }
        _ => None,
    })
    .then(|(order_id, total), ctx| async move {
        ctx.deps().process(order_id, total).await?;
        Ok(OrderEvent::Processed { order_id })
    })
```

### Observer Pattern (All Events)

```rust
handler::on_any()
    .then(|event, ctx: Context<Deps>| async move {
        ctx.deps().logger.log_event(event).await?;
        Ok(()) // No new event
    })
```

**Builder API methods:**
- `.id(String)` - Custom handler identifier
- `.extract(closure)` - Filter and extract data from events
- `.then(closure)` - Async handler implementation
- `.retry(u32)` - Max retry attempts
- `.timeout(Duration)` - Execution timeout
- `.delayed(Duration)` - Delay before execution
- `.priority(i32)` - Execution priority
- `.queued()` - Force queued execution

**When to use:**
- Dynamic handler creation at runtime
- Conditional handler registration
- When closures are more convenient than functions
- Integration with existing closure-heavy code

## Contributing

Contributions are welcome! Please feel free to submit a Pull Request.

## License

MIT
