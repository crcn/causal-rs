# Seesaw

An event-driven runtime for building reactive systems with a simple **Event → Effect → Event** flow.

Named after the playground equipment that balances back and forth — representing the continuous flow of events through the system.

## Workspace Structure

This repository is organized as a Cargo workspace:

- **[seesaw-core](./crates/seesaw)** - Core event-driven runtime
- **[seesaw-outbox](./crates/seesaw-outbox)** - Transactional outbox pattern for durable events

## Core Principle

**Event → Effect → Event.** Simple, direct flow with clean state management.

## What Seesaw Is / Is Not

Seesaw **is**:

> An event-driven runtime where events flow through effects, which return new events.
> State flows through reducers. Clean entry points via closures.

Seesaw is **not**:

- Full event sourcing
- A saga engine
- An actor framework

## Features

- **Stateless Engine**: Reusable runtime, state passed per-execution
- **Event-Driven**: Events are the only signals - facts about what happened
- **Closure-Based API**: Simple builder pattern with closures (no trait implementations)
- **Effect Handlers**: React to events, perform IO, return new events
- **Reducers**: Pure state transformations before effects
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
    let deps = Deps { /* ... */ };

    // Define stateless engine with closure-based effects
    let engine = Engine::new()
        .with_deps(deps)
        .with_reducer(reducer::on::<OrderEvent>().run(|state, event| {
            match event {
                OrderEvent::Placed { total, .. } => State {
                    order_count: state.order_count + 1,
                    total_revenue: state.total_revenue + total,
                },
                _ => state.clone(),
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

    // Activate with initial state
    let handle = engine.activate(State::default());

    // Run logic - return event to dispatch
    let order_id = Uuid::new_v4();
    handle.run(|_ctx| Ok(OrderEvent::Placed { order_id, total: 99.99 }))?;

    // Wait for all effects to complete
    handle.settled().await?;

    Ok(())
}
```

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

        Ok(UserEvent::SignedUp {
            user_id: user.id,
            email,
        })
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
        Ok(CrawlEvent::Extracted { website_id, job_id })
    })

// Effect with state transition guard
effect::on::<StatusEvent>()
    .transition(|prev, next| prev.status != next.status)
    .then(|event, ctx| async move {
        // Only runs when status actually changed
        ctx.deps().notify_status_change(&event).await?;
        Ok(StatusEvent::Notified { id: event.id })
    })

// Observer effect - returns () to dispatch nothing
effect::on::<OrderEvent>().then(|event, ctx| async move {
    ctx.deps().logger.log(&event);
    Ok(())  // No event dispatched
})

// Observe ALL events
effect::on_any().then(|event, ctx| async move {
    ctx.deps().metrics.track(event.type_id);
    Ok(())
})
```

#### `on!` Macro for Multi-Variant Matching

When handling enum events with multiple variants, the `on!` macro provides concise syntax that mirrors Rust's `match`:

```rust
use seesaw_core::on;

// Match-like syntax with Event::Variant patterns
let effects = on! {
    // Multiple variants with | - same fields required
    CrawlEvent::WebsiteIngested { website_id, job_id, .. } |
    CrawlEvent::WebsitePostsRegenerated { website_id, job_id, .. } => |ctx| async move {
        ctx.deps().jobs.enqueue(ExtractPostsJob {
            website_id,
            parent_job_id: job_id,
        }).await?;
        Ok(CrawlEvent::ExtractJobEnqueued { website_id })
    },

    // Single variant
    CrawlEvent::PostsExtractedFromPages { website_id, posts, .. } => |ctx| async move {
        ctx.deps().jobs.enqueue(SyncPostsJob { website_id, posts }).await?;
        Ok(CrawlEvent::SyncJobEnqueued { website_id })
    },
};

// Returns Vec<Effect<S, D>> - add to engine
let engine = effects.into_iter().fold(Engine::new(), |e, eff| e.with_effect(eff));
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
        Ok(PaymentCharged { order_id: event.order_id })
    });

// Delayed execution
effect::on::<OrderPlaced>()
    .delayed(Duration::from_secs(3600))  // Run 1 hour later
    .then(|event, ctx| async move {
        Ok(FollowupSent { order_id: event.order_id })
    });

// Force queued execution
effect::on::<AnalyticsEvent>()
    .queued()  // Execute in background worker
    .then(|event, ctx| async move { Ok(()) });
```

**Configuration methods:**
- `.id(String)` - Custom identifier
- `.retry(u32)` - Max retry attempts (default: 1)
- `.timeout(Duration)` - Execution timeout
- `.delayed(Duration)` - Delay before execution
- `.priority(i32)` - Priority (lower = higher)
- `.queued()` - Force queued execution

**Execution modes:**
- **Inline** (default): Immediate, no retry
- **Queued**: Triggered by `.queued()`, `.delayed()`, `.timeout()`, `.retry() > 1`, or `.priority()`

Key properties:

- **Closure-based**: No trait implementations required
- **Return events**: Effects return `Ok(Event)` to dispatch, or `Ok(())` to dispatch nothing
- **Access state**: Via `ctx.prev_state()`, `ctx.next_state()`, and `ctx.curr_state()`
- **Filter + map**: Use `.extract()` to filter and extract fields in one step
- **Multi-variant matching**: Use `on!` macro for ergonomic enum variant handling
- **State transitions**: Use `.transition()` to react only when state changes
- **Narrow context**: Only `deps()` and state access available

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

// Wait for terminal event using LISTEN/NOTIFY
let result = engine
    .process(CreateOrder { user_id: 123 })
    .wait(|event| {
        // Pattern match on terminal events
        if let Some(created) = event.downcast_ref::<OrderCreated>() {
            Some(Ok(created.clone()))
        } else if let Some(failed) = event.downcast_ref::<OrderFailed>() {
            Some(Err(anyhow!("Failed: {}", failed.reason)))
        } else {
            None  // Keep waiting
        }
    })
    .timeout(Duration::from_secs(30))
    .await?;
```

**Benefits:**
- **Push-based**: Uses PostgreSQL LISTEN/NOTIFY (no polling)
- **Durable**: Events survive crashes
- **Distributed**: Multiple workers process from queue
- **Wait for completion**: `.wait()` blocks until terminal event

**EffectContext in queue-backed mode:**
```rust
ctx.saga_id        // Saga ID from event envelope
ctx.event_id       // Current event's unique ID
ctx.effect_id      // Effect's identifier
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
