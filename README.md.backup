# Seesaw

An event-driven runtime for building reactive systems with a simple **Event → Effect → Event** flow.

Named after the playground equipment that balances back and forth — representing the continuous flow of events through the system.

## Workspace Structure

This repository is organized as a Cargo workspace:

- **[seesaw-core](./crates/seesaw)** - Core event-driven runtime
- **[seesaw-postgres](./crates/seesaw-postgres)** - PostgreSQL backend for queue-backed execution
- **[seesaw-insight](./crates/seesaw-insight)** - Real-time observability and visualization

## Documentation

- **[IMPLEMENTATION_SUMMARY.md](./IMPLEMENTATION_SUMMARY.md)** - Tree visualization & WebSocket implementation details
- **[INTEGRATION_MNDIGITALAID.md](./INTEGRATION_MNDIGITALAID.md)** - Upgrading from v0.7.8 to v0.8.1 guide

## Core Principle

**Event → Effect → Event.** Simple, direct flow with clean state management.

## What Seesaw Is / Is Not

Seesaw **is**:

> An event-driven runtime where events flow through effects, which return new events.
> State flows through reducers. Clean entry points via closures.

Seesaw is **not**:

- Full event sourcing
- A workflow engine
- An actor framework

## Features

- **Stateless Engine**: Reusable runtime, state passed per-execution
- **Event-Driven**: Events are the only signals - facts about what happened
- **Closure-Based API**: Simple builder pattern with closures (no trait implementations)
- **Procedural Macros**: Optional `#[effect]` and `#[reducer]` attributes for concise syntax (v0.10.2+)
- **Effect Handlers**: React to events, perform IO, return new events
- **Reducers**: Pure state transformations before effects
- **DLQ Terminal Mapping**: Map exhausted retries to terminal events for explicit failure handling (v0.10.2+)
- **Event Batching**: Emit and join multiple events for bulk operations (v0.10.0+)
- **Pipedream Integration**: Stream composition and bidirectional piping
- **State Flow**: Per-execution state flows through reducers and effects
- **Handle Pattern**: `activate(state).run(|_| Ok(event)).settled()` for clean execution

## Architecture

```
Closure → Event → Reducer → Effect → Event → Effect → ... (until settled)
                     ↓         ↓
                 State'    returns Event
```

## Quick Start

```rust
use seesaw_core::{effect, reducer, Engine};
use seesaw_memory::MemoryStore;
use anyhow::Result;
use uuid::Uuid;

// Events are facts - what happened
#[derive(Debug, Clone)]
enum OrderEvent {
    Placed { order_id: Uuid, total: f64 },
    Shipped { order_id: Uuid },
    Delivered { order_id: Uuid },
}

#[derive(Clone, Default)]
struct State {
    order_count: usize,
    total_revenue: f64,
}

#[derive(Clone)]
struct Deps {
    shipping_api: ShippingApi,
    email_service: EmailService,
}

#[tokio::main]
async fn main() -> Result<()> {
    let store = MemoryStore::new();
    let deps = Deps { /* ... */ };

    // Define engine with deps and store
    let engine = Engine::new(deps, store)
        .with_reducer(reducer::fold::<OrderEvent>().into(|state: State, event| {
            match event {
                OrderEvent::Placed { total, .. } => State {
                    order_count: state.order_count + 1,
                    total_revenue: state.total_revenue + total,
                },
                _ => state,
            }
        }))
        // Effect that reacts to Placed and returns Shipped
        .with_effect(
            effect::on::<OrderEvent>()
                .extract(|e| match e {
                    OrderEvent::Placed { order_id, .. } => Some(*order_id),
                    _ => None,
                })
                .then(|order_id, ctx| async move {
                    ctx.deps().shipping_api.ship(order_id).await?;
                    Ok(OrderEvent::Shipped { order_id })
                })
        )
        // Effect that reacts to Shipped and returns Delivered
        .with_effect(
            effect::on::<OrderEvent>()
                .extract(|e| match e {
                    OrderEvent::Shipped { order_id } => Some(*order_id),
                    _ => None,
                })
                .then(|order_id, ctx| async move {
                    ctx.deps().email_service.send(order_id, "Shipped!").await?;
                    Ok(OrderEvent::Delivered { order_id })
                })
        );

    // Process event directly - engine handles everything
    let order_id = Uuid::new_v4();
    engine.process(OrderEvent::Placed { order_id, total: 99.99 }).await?;

    Ok(())
}
```

## Procedural Macro API (Recommended)

For a more concise syntax, use the `#[effect]` and `#[reducer]` procedural macros (enabled by default via the `macros` feature):

```rust
use seesaw_core::{effect, reducer, EffectContext, DlqTerminalInfo};
use anyhow::Result;

#[derive(Clone)]
enum OrderEvent {
    OrderPlaced { order_id: Uuid, total: f64 },
    PaymentCharged { order_id: Uuid },
    OrderShipped { order_id: Uuid },
    OrderFailed { order_id: Uuid, reason: String },
}

#[derive(Clone)]
struct OrderState {
    order_count: usize,
    total_revenue: f64,
}

#[derive(Clone)]
struct Deps {
    payment: PaymentService,
    shipping: ShippingService,
}

// Effect with attribute macro
#[effect(
    on = OrderEvent,
    extract(order_id, total),
    retry = 3,
    timeout_secs = 30,
    priority = 1,
    dlq_terminal = order_payment_failed
)]
async fn charge_payment(
    order_id: Uuid,
    total: f64,
    ctx: EffectContext<OrderState, Deps>
) -> Result<OrderEvent> {
    ctx.deps().payment.charge(order_id, total).await?;
    Ok(OrderEvent::PaymentCharged { order_id })
}

// DLQ terminal mapper
fn order_payment_failed(event: OrderEvent, info: DlqTerminalInfo) -> OrderEvent {
    OrderEvent::OrderFailed {
        order_id: match event {
            OrderEvent::OrderPlaced { order_id, .. } => order_id,
            _ => panic!("unexpected event"),
        },
        reason: format!("Payment failed after {} attempts: {}", info.attempts, info.error),
    }
}

// Reducer with attribute macro
#[reducer(on = OrderEvent, extract(total))]
fn track_revenue(state: OrderState, total: f64) -> OrderState {
    OrderState {
        order_count: state.order_count + 1,
        total_revenue: state.total_revenue + total,
    }
}

// Multi-variant matching with macro
#[effect(
    on = [OrderEvent::PaymentCharged, OrderEvent::OrderFailed],
    extract(order_id)
)]
async fn ship_order(
    order_id: Uuid,
    ctx: EffectContext<OrderState, Deps>
) -> Result<OrderEvent> {
    ctx.deps().shipping.ship(order_id).await?;
    Ok(OrderEvent::OrderShipped { order_id })
}

// Join handler with macro
#[effect(on = OrderEvent, join)]
async fn process_batch(
    batch: Vec<OrderEvent>,
    ctx: EffectContext<OrderState, Deps>
) -> Result<()> {
    // Process entire batch together
    let order_ids: Vec<_> = batch.iter()
        .filter_map(|e| match e {
            OrderEvent::OrderPlaced { order_id, .. } => Some(*order_id),
            _ => None,
        })
        .collect();
    ctx.deps().bulk_process(&order_ids).await?;
    Ok(())
}
```

### Module-level registration

Use `#[effects]` and `#[reducers]` to automatically generate registration functions:

```rust
#[effects]
mod order_effects {
    use super::*;

    #[effect(on = OrderEvent, extract(order_id))]
    async fn charge_payment(
        order_id: Uuid,
        ctx: EffectContext<OrderState, Deps>
    ) -> Result<OrderEvent> {
        // ... implementation
    }

    #[effect(on = OrderEvent, extract(order_id))]
    async fn ship_order(
        order_id: Uuid,
        ctx: EffectContext<OrderState, Deps>
    ) -> Result<OrderEvent> {
        // ... implementation
    }
}

#[reducers]
mod order_reducers {
    use super::*;

    #[reducer(on = OrderEvent, extract(total))]
    fn track_revenue(state: OrderState, total: f64) -> OrderState {
        // ... implementation
    }

    #[reducer(on = OrderEvent, extract(order_id))]
    fn track_orders(state: OrderState, order_id: Uuid) -> OrderState {
        // ... implementation
    }
}

// Register all at once
let engine = Engine::new(deps, store)
    .with_effects(order_effects::effects())
    .with_reducers(order_reducers::reducers());
```

### Macro attributes

**Effect attributes:**
- `on = EventType` - Single event type
- `on = [Enum::Variant1, Enum::Variant2]` - Multiple enum variants
- `extract(field1, field2, ...)` - Extract fields from event
- `join` - Accumulate batch for bulk processing
- `id = "name"` - Custom effect identifier
- `retry = N` - Max retry attempts
- `timeout_secs = N` / `timeout_ms = N` - Execution timeout
- `delay_secs = N` / `delay_ms = N` - Delayed execution
- `priority = N` - Execution priority (lower = higher)
- `group = "name"` - Effect group (used in ID if no explicit `id`)
- `dlq_terminal = handler` - Terminal event mapper for exhausted retries

**Reducer attributes:**
- `on = EventType` - Single event type
- `on = [Enum::Variant1, Enum::Variant2]` - Multiple enum variants
- `extract(field1, field2, ...)` - Extract fields from event

**Requirements:**
- Effect functions must be `async` and return `Result<T>` where `T` is an event or `()`
- Reducer functions must be synchronous and return the state type
- All functions must include exactly one `EffectContext<S, D>` parameter (for effects)
- Parameter names must match extracted field names when using `extract(...)`
- For `join`, first parameter must be `Vec<EventType>`

## Core Concepts

### Events

Events are immutable facts describing what happened. The `Event` trait is **auto-implemented** for any type that is `Clone + Send + Sync + 'static`.

```rust
#[derive(Debug, Clone)]
enum UserEvent {
    // Input - requests from edges
    SignupRequested { email: String, name: String },
    // Fact - what actually happened
    SignedUp { user_id: Uuid, email: String },
    Verified { user_id: Uuid },
    Deleted { user_id: Uuid },
}
// Event is automatically implemented!
```

**Event Roles:**

| Role   | Description                           | Example            |
| ------ | ------------------------------------- | ------------------ |
| Input  | User/closure-originated requests      | `SignupRequested`  |
| Fact   | Effect-produced ground truth          | `SignedUp`         |

### Effects

Effects are event handlers that react to events, perform IO, and return new events. Use the closure-based builder API with `.then()`:

```rust
use seesaw_core::effect;

// Basic effect - returns event to dispatch
effect::on::<UserEvent>()
    .extract(|e| match e {
        UserEvent::SignupRequested { email, name } => Some((email.clone(), name.clone())),
        _ => None,
    })
    .then(|(email, name), ctx| async move {
        let user = ctx.deps().db.transaction(|tx| async {
            let user = User::create(&email, &name, tx).await?;
            UserProfile::create(user.id, tx).await?;
            Ok(user)
        }).await?;

        Ok(Emit::One(UserEvent::SignedUp {
            user_id: user.id,
            email,
        }))
    })

// Effect with filter_map (combines filter + destructure)
effect::on::<CrawlEvent>()
    .extract(|e| match e {
        CrawlEvent::PagesReady { website_id, job_id, page_ids } => {
            Some((*website_id, *job_id, page_ids.clone()))
        }
        _ => None
    })
    .then(|(website_id, job_id, page_ids), ctx| async move {
        // Handler receives extracted fields directly!
        ctx.deps().extract(website_id, job_id, page_ids).await?;
        Ok(Emit::One(CrawlEvent::Extracted { website_id, job_id }))
    })

// Effect with state transition guard
effect::on::<StatusEvent>()
    .transition(|prev, next| prev.status != next.status)
    .then(|event, ctx| async move {
        // Only runs when status actually changed
        ctx.deps().notify_status_change(&event).await?;
        Ok(Emit::One(StatusEvent::Notified { id: event.id }))
    })

// Observer effect - returns Emit::None to dispatch nothing
effect::on::<OrderEvent>().then(|event, ctx| async move {
    ctx.deps().logger.log(&event);
    Ok(Emit::None)  // No event dispatched
})

// Observe ALL events
effect::on_any().then(|event, ctx| async move {
    ctx.deps().metrics.track(event.type_id);
    Ok(Emit::None)
})
```

#### Effect Execution Configuration (v0.8.0+)

Configure effects for retry, timeout, delay, priority, and queued execution:

```rust
// Queued effect with retry and timeout
effect::on::<PaymentRequested>()
    .id("charge_payment")               // Custom ID for tracing
    .retry(5)                            // Retry up to 5 times
    .timeout(Duration::from_secs(30))   // 30 second timeout
    .priority(1)                         // Higher priority (lower = higher)
    .then(|event, ctx| async move {
        ctx.deps().stripe.charge(&event).await?;
        Ok(Emit::One(PaymentCharged { order_id: event.order_id }))
    });

// Delayed execution
effect::on::<OrderPlaced>()
    .delayed(Duration::from_secs(3600))  // Run 1 hour later
    .then(|event, ctx| async move {
        Ok(Emit::One(FollowupSent { order_id: event.order_id }))
    });

// Force queued execution
effect::on::<AnalyticsEvent>()
    .queued()  // Execute in background worker
    .then(|event, ctx| async move { Ok(Emit::None) });
```

**Configuration methods:**
- `.id(String)` - Custom identifier
- `.retry(u32)` - Max retry attempts (default: 1)
- `.timeout(Duration)` - Execution timeout
- `.delayed(Duration)` - Delay before execution
- `.priority(i32)` - Priority (lower = higher)
- `.queued()` - Force queued execution
- `.dlq_terminal(mapper)` - Map exhausted retries to terminal events (v0.10.2+)

**Execution modes:**
- **Inline** (default): Immediate, no retry
- **Queued**: Triggered by `.queued()`, `.delayed()`, `.timeout()`, `.retry() > 1`, or `.priority()`

#### DLQ Terminal Mapping (v0.10.2+)

When an effect exhausts its retry attempts, you can map the failure to a terminal event using `.dlq_terminal()`:

```rust
use seesaw_core::{effect, DlqTerminalInfo};

// Define terminal events for failures
#[derive(Clone)]
enum OrderEvent {
    OrderPlaced { order_id: Uuid },
    PaymentCharged { order_id: Uuid },
    OrderFailed { order_id: Uuid, reason: String },  // Terminal failure event
}

// Map exhausted retries to terminal event
effect::on::<OrderEvent>()
    .extract(|e| match e {
        OrderEvent::OrderPlaced { order_id } => Some(*order_id),
        _ => None,
    })
    .retry(3)  // Try 3 times
    .dlq_terminal(|event, info: DlqTerminalInfo| {
        // Called when effect fails after max attempts
        OrderEvent::OrderFailed {
            order_id: event.order_id,
            reason: format!("Payment failed after {} attempts: {}",
                info.attempts, info.error),
        }
    })
    .then(|order_id, ctx| async move {
        ctx.deps().payment.charge(order_id).await?;  // Might fail
        Ok(OrderEvent::PaymentCharged { order_id })
    });
```

**DlqTerminalInfo fields:**
- `error: String` - The error message
- `reason: String` - Why it failed ("failed", "timeout", etc.)
- `attempts: i32` - Number of attempts made
- `max_attempts: i32` - Maximum allowed attempts

**When to use:**
- Critical flows where failures must be handled explicitly
- Workflows that need compensation or rollback events
- Situations where silent failures are unacceptable

**Alternative: `EffectError` events**

For convenience, use `?` in effects and handle `EffectError` events:

```rust
// Use ? for ergonomics
effect::on::<OrderPlaced>().then(|order, ctx| async move {
    ctx.deps().payment.charge(order.total).await?;  // Propagates error
    Ok(PaymentCharged { order_id: order.id })
});

// Handle EffectError events
effect::on::<EffectError>()
    .extract(|err| {
        // Filter by source event + error type
        if err.source_event_type == TypeId::of::<OrderPlaced>() {
            err.downcast::<PaymentError>()
                .map(|pe| (err.source_event.downcast_ref::<OrderPlaced>().unwrap().clone(), pe.clone()))
        } else {
            None
        }
    })
    .then(|(order, payment_err), ctx| async move {
        // Domain-specific compensation based on error type
        if payment_err.is_retryable() {
            Ok(PaymentRetryScheduled { order_id: order.id })
        } else {
            Ok(OrderCancelled {
                order_id: order.id,
                reason: payment_err.to_string(),
            })
        }
    });
```

Key properties:

- **Closure-based**: No trait implementations required
- **Return events**: Effects return `Ok(Emit::One(event))`, `Ok(Emit::Batch(vec![...]))`, or `Ok(Emit::None)`
- **Access state**: Via `ctx.prev_state()`, `ctx.next_state()`, and `ctx.curr_state()`
- **Filter + map**: Use `.extract()` to filter and extract fields in one step
- **Multi-variant matching**: Use `on!` macro for ergonomic enum variant handling
- **State transitions**: Use `.transition()` to react only when state changes
- **Narrow context**: Only `deps()` and state access available

#### Event Batching (v0.10.0+)

Effects can emit multiple events at once using the `Emit` enum:

```rust
use seesaw_core::effect;

// Emit multiple events in a batch
effect::on::<ImportEvent>()
    .extract(|e| match e {
        ImportEvent::FileUploaded { path } => Some(path.clone()),
        _ => None,
    })
    .then(|path, ctx| async move {
        let rows = ctx.deps().parse_csv(&path).await?;

        // Use Emit::Batch for multiple events
        let events: Vec<_> = rows.into_iter()
            .map(|data| ImportEvent::RowParsed {
                row_id: Uuid::new_v4(),
                data
            })
            .collect();
        Ok(Emit::Batch(events))  // Emits 1000s of events efficiently
    });

// Join pattern - accumulate batch and process together
effect::on::<ImportEvent>()
    .join()  // Accumulates events with same batch_id
    .then(|batch: Vec<ImportEvent>, ctx| async move {
        let items: Vec<_> = batch.into_iter()
            .filter_map(|e| match e {
                ImportEvent::RowParsed { data, .. } => Some(data),
                _ => None,
            })
            .collect();

        // Bulk insert entire batch
        ctx.deps().bulk_insert(&items).await?;
        Ok(Emit::One(ImportEvent::BatchInserted { count: items.len() }))
    });
```

**Return types:**
- `Ok(event)` - auto-converts to `Emit::One(event)` when consistently returning events
- `Ok(Emit::One(event))` - explicit single event (use when mixing return types)
- `Ok(Emit::Batch(vec![...]))` - multiple events atomically
- `Ok(Emit::None)` - no events (use when some paths emit, others don't)

**Rule**: Auto-conversion works when all paths return the same type. Use explicit `Emit` when mixing return types to avoid compiler ambiguity.

**Join semantics:**
- Events emitted in same batch (via `Emit::Batch`) share a `batch_id`
- `.join()` accumulates all events with matching `batch_id`
- Once all events in batch arrive, handler receives the full batch
- Provides 10-50x performance for bulk operations

See [`examples/batch-processor`](./examples/batch-processor) for a complete CSV import example.

### Reducers

Reducers are pure functions that transform state in response to events. They run before effects.

```rust
use seesaw_core::reducer;

// Basic reducer
reducer::on::<CountEvent>().run(|state, event| {
    match event {
        CountEvent::Incremented { amount } => AppState {
            counter: state.counter + amount,
        },
        _ => state.clone(),
    }
})

// Multiple reducers for different events
reducer::on::<OrderEvent>().run(|state, event| {
    match event {
        OrderEvent::Placed { total, .. } => State {
            order_count: state.order_count + 1,
            total_revenue: state.total_revenue + total,
        },
        _ => state.clone(),
    }
})

// Reset reducer
reducer::on::<ResetEvent>().run(|_state, _event| {
    State::default()
})
```

Key properties:

- **Pure**: No side effects, deterministic
- **Closure-based**: Simple builder pattern
- **Transform state**: Take current state and event, return new state
- **Run before effects**: Updated state is passed to effects via `ctx.next_state()`

### EffectContext

`EffectContext` provides a narrow API to effects:

```rust
// Access shared dependencies (database, APIs, config)
ctx.deps()

// Access state snapshots
ctx.prev_state()  // State before reducer ran
ctx.next_state()  // State after reducer ran
ctx.curr_state()  // Current live state (may have changed)

// Spawn tracked sub-tasks
ctx.within(|ctx| async move {
    // Background work that keeps the engine alive
    Ok(())
});

// Check if cancelled
ctx.is_cancelled()

// Note: Effects return events instead of calling ctx.emit()
// Return Ok(SomeEvent) to dispatch, or Ok(()) to dispatch nothing
```

## Engine Usage

The `Engine` API uses a stateless pattern - define once, use many times:

```rust
// Define stateless engine (reusable)
let engine = Engine::new()
    .with_deps(deps)
    .with_reducer(reducer::on::<OrderEvent>().run(|state, event| {
        // Pure state transformation
        State { count: state.count + 1, ..state }
    }))
    .with_effect(
        effect::on::<OrderEvent>()
            .extract(|e| match e {
                OrderEvent::Placed { order_id, .. } => Some(*order_id),
                _ => None,
            })
            .then(|order_id, ctx| async move {
                // Side effects - return event to dispatch
                ctx.deps().process(order_id).await?;
                Ok(OrderEvent::Processed { order_id })
            })
    );

// Option 1: Process to completion (common pattern)
let handle = engine.activate(State::default());
handle.process(|_ctx| async {
    Ok(OrderEvent::Placed { order_id, total: 99.99 })
}).await?;

// Option 2: Run and settle separately (when you need the handle)
let handle = engine.activate(State::default());
handle.run(|_ctx| Ok(OrderEvent::Placed { order_id, total: 99.99 }))?;
handle.settled().await?;

// Return () to dispatch nothing
handle.run(|_ctx| Ok(()))?;
```

Builder methods:

- `.with_deps(deps)` — Set shared dependencies
- `.with_reducer(reducer)` — Register pure state transformations
- `.with_effect(effect)` — Register event handlers (use `.then()` to return events)
- `.with_effect_registry(registry)` — Use existing effect registry

Handle methods:

- `.run(closure)` — Execute logic, return event to dispatch (or `()` for none)
- `.process(async_closure).await` — Async version of `run()`, waits for completion
- `.settled().await` — Wait for all effects to complete
- `.cancel()` — Cancel all tasks
- `.shutdown().await` — Graceful shutdown (waits for in-flight tasks)

## Queue-Backed Engine (v0.8.0+)

For durable, queue-backed execution with `.wait()` pattern:

```rust
use seesaw_core::{QueueEngine, effect};
use seesaw_postgres::PostgresStore;

// Create queue-backed engine
let store = PostgresStore::new(pool);
let engine = QueueEngine::new(deps, store)
    .with_effect(
        effect::on::<OrderPlaced>()
            .id("send_email")
            .retry(3)
            .then(|event, ctx| async move {
                ctx.deps().mailer.send(&event).await?;
                Ok(EmailSent { order_id: event.order_id })
            })
    )
    .with_reducer(reducer);

// Pattern 1: Fire and forget - get correlation_id
let handle = engine.process(CreateOrder { user_id: 123 }).await?;
let correlation_id = handle.correlation_id;  // Return to client

// Pattern 2: Wait for terminal event using LISTEN/NOTIFY
let result = engine
    .process(CreateOrder { user_id: 123 })
    .wait(|event| {
        // Match on WorkflowEvent (contains event_type and JSON payload)
        if let Some(workflow_event) = event.downcast_ref::<seesaw_core::WorkflowEvent>() {
            match workflow_event.event_type.as_str() {
                "OrderCreated" => {
                    let order_id = workflow_event.payload["order_id"].as_str()?;
                    Some(Ok(order_id.to_string()))
                }
                "OrderFailed" => {
                    let reason = workflow_event.payload["reason"].as_str()?;
                    Some(Err(anyhow!("Failed: {}", reason)))
                }
                _ => None  // Keep waiting
            }
        } else {
            None
        }
    })
    .timeout(Duration::from_secs(30))
    .await?;

// Pattern 3: Check workflow status
let status = store.get_workflow_status(correlation_id).await?;
println!("Workflow {} - settled: {}, pending: {}",
    status.correlation_id,
    status.is_settled,  // true when no effects running
    status.pending_effects
);
```

**Benefits:**
- **Push-based**: Uses PostgreSQL LISTEN/NOTIFY (no polling)
- **Durable**: Events survive crashes
- **Distributed**: Multiple workers process from queue
- **Wait for completion**: `.wait()` blocks until terminal event
- **Status tracking**: Query workflow status at any time

**Workflow Status:**
```rust
pub struct WorkflowStatus {
    pub correlation_id: Uuid,
    pub state: Option<serde_json::Value>,
    pub pending_effects: i64,
    pub is_settled: bool,  // true when no effects pending/executing
    pub last_event: Option<String>,
}
```

**Key distinction:**
- `is_settled` - No effects running right now (workflow is idle, but can start again)
- Terminal events - User-defined events that signal true completion (e.g., `OrderCompleted`)

### Real-time Observability (Seesaw Insight)

Visualize your workflows in real-time with the Insight server:

```bash
# Start the insight server
export DATABASE_URL="postgres://localhost/seesaw"
cargo run -p seesaw-insight

# Open http://localhost:3000
```

**Features:**
- **Live Stream**: Real-time view of all workflow events and effects
- **Tree Visualization**: Interactive causality tree showing event relationships
- **Dual Protocols**: Both WebSocket and SSE support (toggle in UI)
- **Real-time Stats**: Total events, active effects, recent activity
- **Cursor-based Pagination**: Efficient streaming of large event volumes

**Dashboard Views:**
- **Live Stream Tab**: Scrolling list of all workflow activity
- **Tree View Tab**: Click any workflow ID to see its causality tree with parent-child relationships and effect executions

The `seesaw_stream` table automatically captures:
- `event_dispatched` - New events published to the queue
- `effect_started` - Effect execution began
- `effect_completed` - Effect finished successfully
- `effect_failed` - Effect failed with error

Triggers populate this table automatically - no code changes needed!

**API Endpoints:**
- `GET /api/stream` - SSE endpoint
- `WS /api/ws` - WebSocket endpoint
- `GET /api/tree/:correlation_id` - Causality tree
- `GET /api/stats` - Current metrics

**EffectContext in queue-backed mode:**
```rust
ctx.correlation_id   // Workflow identifier
ctx.event_id         // Current event's unique ID
ctx.effect_id        // Effect's identifier
ctx.idempotency_key  // UUID v5(event_id + effect_id) for idempotent API calls
```

## Request/Response Pattern

For code that needs a response, use `dispatch_request`:

```rust
use seesaw_core::{dispatch_request, EnvelopeMatch};

let entry = dispatch_request(
    EntryRequestEvent::Create { ... },
    &bus,
    |m| m.try_match(|e: &EntryEvent| match e {
        EntryEvent::Created { entry } => Some(Ok(entry.clone())),
        _ => None,
    })
    .or_try(|denied: &AuthDenied| Some(Err(anyhow!("denied"))))
    .result()
).await?;
```

This emits an event and waits until a correlated event matches the extractor, or times out (default: 30 seconds).

## Durable Event Outbox

For events that must survive crashes, use the transactional outbox pattern:

```rust
use seesaw_outbox::{OutboxEvent, OutboxWriter, CorrelationId};

// 1. Mark event for outbox persistence
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderPlaced {
    pub order_id: Uuid,
    pub customer_id: Uuid,
}

impl OutboxEvent for OrderPlaced {
    fn event_type() -> &'static str { "order.placed.v1" }
}

// 2. Write to outbox in same transaction as business data
effect::on::<OrderEvent>()
    .extract(|e| match e {
        OrderEvent::PlaceRequested { customer_id, items } => {
            Some((*customer_id, items.clone()))
        }
        _ => None,
    })
    .then(|(customer_id, items), ctx| async move {
        let mut tx = ctx.deps().db.begin().await?;

        // Business write
        let order = Order::create(customer_id, &items, &mut tx).await?;

        // Outbox write (same transaction) - survives crashes
        let mut writer = PgOutboxWriter::new(&mut tx);
        writer.write_event(
            &OrderPlaced { order_id: order.id, customer_id },
            ctx.outbox_correlation_id(),
        ).await?;

        tx.commit().await?;
        Ok(OrderEvent::Placed { order_id: order.id })
    })
```

**Key differences from in-memory events:**

| Aspect      | Effect return         | Outbox                |
| ----------- | --------------------- | --------------------- |
| Durability  | Lost on crash         | Survives crash        |
| Delivery    | At-most-once          | At-least-once         |
| Performance | Immediate             | Poll-based latency    |
| Use case    | Internal coordination | External side effects |

## Design Philosophy

1. **Events are Facts**: Immutable descriptions of what happened
2. **Event → Effect → Event**: Simple, direct flow
3. **Effects Return Events**: Return `Ok(Event)` to dispatch, `Ok(())` for nothing
4. **Reducers are Pure**: State transformations with no side effects
5. **Effects Have Narrow Context**: Only deps and state access available
6. **Fan-out via Multiple Effects**: Many effects can react to the same event
7. **State Flows Through System**: Per-execution state via reducers and effects

## Guarantees

- **At-most-once delivery**: Slow receivers may miss events
- **In-memory only**: Events are not persisted by seesaw
- **No replay**: Lagged receivers get errors

For durability, use:

- Entity status fields for workflow state
- Transactional outbox for durable events
- Reapers for crash recovery

## Testing

Test effects using the Engine:

```rust
#[tokio::test]
async fn test_effect_returns_event() {
    let engine = Engine::new()
        .with_deps(MockDeps::new())
        .with_effect(
            effect::on::<OrderEvent>()
                .extract(|e| match e {
                    OrderEvent::Placed { order_id, .. } => Some(*order_id),
                    _ => None,
                })
                .then(|order_id, ctx| async move {
                    ctx.deps().shipping_api.ship(order_id).await?;
                    Ok(OrderEvent::Shipped { order_id })
                })
        );

    let handle = engine.activate(State::default());
    handle.run(|_ctx| Ok(OrderEvent::Placed { order_id, total: 99.99 }))?;
    handle.settled().await?;

    // Verify side effects were called
    assert!(handle.deps().shipping_api.was_called());
}
```

Test reducers as pure functions:

```rust
#[test]
fn test_reducer_transforms_state() {
    let reducer = reducer::on::<OrderEvent>().run(|state, event| {
        match event {
            OrderEvent::Placed { .. } => State { status: Some(Status::Placed), ..state },
            _ => state.clone(),
        }
    });

    let state = State { status: None };
    let event = OrderEvent::Placed { order_id, total: 99.99 };

    // Reducers are pure functions
    let new_state = (reducer.reducer)(&state, &event);
    assert_eq!(new_state.status, Some(Status::Placed));
}
```

## License

MIT
