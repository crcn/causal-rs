# Seesaw

**Event-driven orchestration for Rust**

Build reactive systems with a simple **Event → Handler → Event** flow. Named after the playground equipment that balances back and forth — representing the continuous flow of events through the system.

```rust
use seesaw_core::{handler, Context, Engine};
use seesaw_memory::MemoryBackend;

// Events are facts - what happened
#[derive(Clone, Serialize, Deserialize)]
enum OrderEvent {
    Placed { order_id: Uuid, total: f64 },
    Shipped { order_id: Uuid },
    Delivered { order_id: Uuid },
}

// Handlers react to events and return new events
let engine = Engine::new(deps, MemoryBackend::new())
    .with_handler(
        handler::on::<OrderEvent>()
            .extract(|e| match e {
                OrderEvent::Placed { order_id, .. } => Some(*order_id),
                _ => None,
            })
            .then(|order_id, ctx: Context<Deps>| async move {
                ctx.deps().ship(order_id).await?;
                Ok(OrderEvent::Shipped { order_id })
            })
    );

// Dispatch events
engine.dispatch(OrderEvent::Placed { order_id, total: 99.99 }).await?;
```

## Features

- **🎯 Event-Driven**: Events are the only signals — immutable facts about what happened
- **⚡ High Performance**: 50k-100k events/sec with Kafka backend
- **🔄 Handler Pattern**: React to events, perform IO, return new events
- **🎨 Flexible Backends**: In-memory, PostgreSQL, or Kafka
- **📈 Horizontal Scaling**: Scale via Kafka partitions and consumer groups
- **🔒 Exactly-Once**: Idempotency and atomic commits prevent duplicate processing
- **🔍 Observable**: Real-time visualization with Seesaw Insight
- **🚀 Simple API**: Closure-based builder pattern, no trait implementations

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

### Basic Example

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

#[tokio::main]
async fn main() -> Result<()> {
    // Create backend
    let backend = MemoryBackend::new();

    // Create dependencies
    let deps = Deps {
        shipping_api: ShippingApi::new(),
        email_service: EmailService::new(),
    };

    // Build engine with handlers
    let engine = Engine::new(deps, backend)
        // Handler: Placed → Shipped
        .with_handler(
            handler::on::<OrderEvent>()
                .id("ship_order")
                .extract(|e| match e {
                    OrderEvent::Placed { order_id, .. } => Some(*order_id),
                    _ => None,
                })
                .then(|order_id, ctx: Context<Deps>| async move {
                    // Perform async IO
                    ctx.deps().shipping_api.ship(order_id).await?;
                    // Return new event
                    Ok(OrderEvent::Shipped { order_id })
                }),
        )
        // Handler: Shipped → Delivered
        .with_handler(
            handler::on::<OrderEvent>()
                .id("notify_shipped")
                .extract(|e| match e {
                    OrderEvent::Shipped { order_id } => Some(*order_id),
                    _ => None,
                })
                .then(|order_id, ctx: Context<Deps>| async move {
                    ctx.deps().email_service.send(order_id, "Shipped!").await?;
                    Ok(OrderEvent::Delivered { order_id })
                }),
        );

    // Dispatch event
    let order_id = Uuid::new_v4();
    engine.dispatch(OrderEvent::Placed { order_id, total: 99.99 }).await?;
    // Engine automatically processes: Placed → Shipped → Delivered

    Ok(())
}
```

## Handler Pattern

Handlers are the core abstraction in Seesaw. They react to events, perform side effects, and return new events.

### Basic Handler

```rust
handler::on::<OrderEvent>()
    .extract(|e| match e {
        OrderEvent::Placed { order_id, .. } => Some(*order_id),
        _ => None,
    })
    .then(|order_id, ctx: Context<Deps>| async move {
        ctx.deps().process(order_id).await?;
        Ok(OrderEvent::Processed { order_id })
    })
```

**Pattern breakdown:**
1. **`.on::<Event>()`** - Listen for specific event type
2. **`.extract()`** - Filter and extract data from events
3. **`.then()`** - Async closure that performs IO and returns event

### Handler Configuration

Handlers support configuration for retry, timeout, priority, and more:

```rust
handler::on::<PaymentEvent>()
    .id("charge_payment")              // Custom identifier
    .retry(3)                          // Retry 3 times on failure
    .timeout(Duration::from_secs(30))  // 30 second timeout
    .priority(1)                       // Higher priority (lower number = higher priority)
    .delay(Duration::from_secs(60))    // Delay 1 minute before execution
    .then(|event, ctx| async move {
        ctx.deps().stripe.charge(&event).await?;
        Ok(PaymentCharged { order_id: event.order_id })
    })
```

**Configuration options:**
- `.id(String)` - Custom handler identifier (for tracing/debugging)
- `.retry(u32)` - Max retry attempts on failure
- `.timeout(Duration)` - Execution timeout
- `.delay(Duration)` - Delay before execution
- `.priority(i32)` - Execution priority (lower = higher priority)

### Multiple Handlers, Same Event

Multiple handlers can react to the same event (fan-out pattern):

```rust
let engine = Engine::new(deps, backend)
    // Handler 1: Update inventory
    .with_handler(
        handler::on::<OrderEvent>()
            .extract(|e| match e {
                OrderEvent::Placed { order_id, items } => Some((*order_id, items.clone())),
                _ => None,
            })
            .then(|(order_id, items), ctx| async move {
                ctx.deps().inventory.reserve(&items).await?;
                Ok(InventoryReserved { order_id })
            })
    )
    // Handler 2: Charge payment
    .with_handler(
        handler::on::<OrderEvent>()
            .extract(|e| match e {
                OrderEvent::Placed { order_id, total } => Some((*order_id, *total)),
                _ => None,
            })
            .then(|(order_id, total), ctx| async move {
                ctx.deps().payment.charge(total).await?;
                Ok(PaymentCharged { order_id })
            })
    )
    // Handler 3: Send notification
    .with_handler(
        handler::on::<OrderEvent>()
            .extract(|e| match e {
                OrderEvent::Placed { order_id, .. } => Some(*order_id),
                _ => None,
            })
            .then(|order_id, ctx| async move {
                ctx.deps().email.send(order_id, "Order received!").await?;
                Ok(()) // No new event
            })
    );
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

Durable, ACID transactions, queue-backed execution.

```rust
use seesaw_postgres::PostgresStore;
use sqlx::postgres::PgPoolOptions;

let pool = PgPoolOptions::new()
    .max_connections(20)
    .connect("postgres://localhost/seesaw")
    .await?;

let store = PostgresStore::new(pool);
let backend = PostgresBackend::new(store);
let engine = Engine::new(deps, backend);
```

**Use when:**
- Production deployments
- Durability required
- ~1-5k events/sec throughput
- Single-node simplicity preferred

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
handler::on::<Event>()
    .then(|data, ctx: Context<Deps>| async move {
        // Access shared dependencies
        ctx.deps().database.query(...).await?;
        ctx.deps().api_client.call(...).await?;

        Ok(NewEvent)
    })
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

- **[seesaw_core](crates/seesaw)** - Core event-driven runtime and handler system
- **[seesaw-postgres](crates/seesaw-postgres)** - PostgreSQL backend (durable, ACID)
- **[seesaw-kafka](crates/seesaw-kafka)** - Kafka backend (high throughput, horizontal scaling)
- **[seesaw-memory](crates/seesaw-memory)** - In-memory backend (fast, ephemeral)
- **[seesaw-insight](crates/seesaw-insight)** - Real-time observability and visualization

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
handler::on::<UserEvent>()
    .extract(|e| match e {
        UserEvent::SignupRequested { email, name } => Some((email.clone(), name.clone())),
        _ => None,
    })
    .then(|(email, name), ctx| async move {
        // Perform IO
        let user = ctx.deps().db.create_user(&email, &name).await?;

        // Return new event
        Ok(UserEvent::SignedUp {
            user_id: user.id,
            email,
        })
    })
```

**Key properties:**
- Async (can perform IO)
- Access dependencies via `ctx.deps()`
- Return `Result<Event>` or `Result<()>`
- Can fail and retry (if configured)

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

## Testing

Test handlers using the in-memory backend:

```rust
#[tokio::test]
async fn test_order_processing() {
    let deps = MockDeps::new();
    let backend = MemoryBackend::new();

    let engine = Engine::new(deps.clone(), backend)
        .with_handler(
            handler::on::<OrderEvent>()
                .extract(|e| match e {
                    OrderEvent::Placed { order_id, .. } => Some(*order_id),
                    _ => None,
                })
                .then(|order_id, ctx| async move {
                    ctx.deps().ship(order_id).await?;
                    Ok(OrderEvent::Shipped { order_id })
                })
        );

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

## Contributing

Contributions are welcome! Please feel free to submit a Pull Request.

## License

MIT
