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

> An event-driven runtime where events flow through effects, which emit new events.
> State flows through reducers. Clean entry points via closures.

Seesaw is **not**:

- Full event sourcing
- A saga engine
- An actor framework

## Features

- **Stateless Engine**: Reusable runtime, state passed per-execution
- **Event-Driven**: Events are the only signals - facts about what happened
- **Closure-Based API**: Simple builder pattern with closures (no trait implementations)
- **Effect Handlers**: React to events, perform IO, emit new events via `ctx.emit()`
- **Reducers**: Pure state transformations before effects
- **Pipedream Integration**: Stream composition and bidirectional piping
- **State Flow**: Per-execution state flows through reducers and effects
- **Handle Pattern**: `activate(state).run(action).settled()` for clean execution

## Architecture

```
Closure → Event → Reducer → Effect → Event → Effect → ... (until settled)
                     ↓         ↓
                 State'    ctx.emit(Event)
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
        .with_effect(effect::on::<OrderEvent>().run(|event, ctx| async move {
            match event.as_ref() {
                OrderEvent::Placed { order_id, .. } => {
                    ctx.deps().shipping_api.ship(*order_id).await?;
                    ctx.emit(OrderEvent::Shipped { order_id: *order_id });
                }
                OrderEvent::Shipped { order_id } => {
                    ctx.deps().email_service.send(*order_id, "Shipped!").await?;
                    ctx.emit(OrderEvent::Delivered { order_id: *order_id });
                }
                _ => {}
            }
            Ok(())
        }));

    // Activate with initial state
    let handle = engine.activate(State::default());

    // Run logic that emits events
    let result = handle.run(|ctx| {
        let order_id = Uuid::new_v4();
        ctx.emit(OrderEvent::Placed { order_id, total: 99.99 });
        Ok(())
    })?;

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

Effects are event handlers that react to events, perform IO, and optionally emit new events. Use the closure-based builder API:

```rust
use seesaw_core::effect;

// Basic effect
effect::on::<UserEvent>().run(|event, ctx| async move {
    match event.as_ref() {
        UserEvent::SignupRequested { email, name } => {
            let user = ctx.deps().db.transaction(|tx| async {
                let user = User::create(email, name, tx).await?;
                UserProfile::create(user.id, tx).await?;
                Ok(user)
            }).await?;

            ctx.emit(UserEvent::SignedUp {
                user_id: user.id,
                email: email.clone(),
            });
        }
        _ => {}
    }
    Ok(())
})

// Effect with filter
effect::on::<OrderEvent>()
    .filter(|e| matches!(e, OrderEvent::HighPriority { .. }))
    .run(|event, ctx| async move {
        // Only runs for high-priority orders
        ctx.deps().notify_urgent(&event).await?;
        Ok(())
    })

// Effect with filter_map (combines filter + destructure)
effect::on::<CrawlEvent>()
    .filter_map(|e| match e {
        CrawlEvent::PagesReady { website_id, job_id, page_ids } => {
            Some((*website_id, *job_id, page_ids.clone()))
        }
        _ => None
    })
    .run(|(website_id, job_id, page_ids), ctx| async move {
        // Handler receives extracted fields directly!
        ctx.deps().extract(website_id, job_id, page_ids).await?;
        Ok(())
    })

// Effect with state transition guard
effect::on::<StatusEvent>()
    .transition(|prev, next| prev.status != next.status)
    .run(|event, ctx| async move {
        // Only runs when status actually changed
        ctx.deps().notify_status_change(&event).await?;
        Ok(())
    })
```

Key properties:

- **Closure-based**: No trait implementations required
- **Emit events**: Use `ctx.emit()` to emit new events
- **Access state**: Via `ctx.prev_state()`, `ctx.next_state()`, and `ctx.curr_state()`
- **Filter events**: Use `.filter()` to skip events that don't match
- **Filter + map**: Use `.filter_map()` to filter and extract fields in one step
- **State transitions**: Use `.transition()` to react only when state changes
- **Narrow context**: Only `deps()`, state access, and `emit()` available

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

// Emit new events
ctx.emit(OrderEvent::Shipped { order_id });

// Spawn tracked sub-tasks
ctx.within(|ctx| async move {
    // Background work that keeps the engine alive
    Ok(())
});

// Check if cancelled
ctx.is_cancelled()
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
    .with_effect(effect::on::<OrderEvent>().run(|event, ctx| async move {
        // Side effects
        ctx.deps().process(&event).await?;
        ctx.emit(NextEvent);
        Ok(())
    }));

// Activate per-request with initial state
let handle = engine.activate(State::default());

// Run logic that emits events
let result = handle.run(|ctx| {
    ctx.emit(OrderEvent::Placed { order_id, total: 99.99 });
    Ok(Response { status: "ok" })
})?;

// Wait for all effects to complete
handle.settled().await?;
```

Builder methods:

- `.with_deps(deps)` — Set shared dependencies
- `.with_reducer(reducer)` — Register pure state transformations
- `.with_effect(effect)` — Register event handlers
- `.with_effect_registry(registry)` — Use existing effect registry

Handle methods:

- `.run(closure)` — Execute logic, returns result
- `.settled().await` — Wait for all effects to complete
- `.cancel()` — Cancel all tasks

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
async fn handle(&mut self, event: OrderEvent, ctx: EffectContext<Deps>) -> Result<()> {
    match event {
        OrderEvent::PlaceRequested { customer_id, items } => {
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
            ctx.emit(OrderEvent::Placed { order_id: order.id });
            Ok(())
        }
        _ => Ok(()),
    }
}
```

**Key differences from in-memory events:**

| Aspect      | Effect emit           | Outbox                |
| ----------- | --------------------- | --------------------- |
| Durability  | Lost on crash         | Survives crash        |
| Delivery    | At-most-once          | At-least-once         |
| Performance | Immediate             | Poll-based latency    |
| Use case    | Internal coordination | External side effects |

## Design Philosophy

1. **Events are Facts**: Immutable descriptions of what happened
2. **Event → Effect → Event**: Simple, direct flow
3. **Effects Can Be Stateful or Stateless**: Your choice with `&mut self`
4. **Reducers are Pure**: State transformations with no side effects
5. **Effects Have Narrow Context**: Only deps, state, signal, and tool_context
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

Test effects by calling `handle` directly:

```rust
#[tokio::test]
async fn test_effect_handles_event() {
    let mut effect = ShipEffect;
    let deps = Arc::new(MockDeps::new());
    let bus = EventBus::new();
    let mut receiver = bus.subscribe();
    let ctx = EffectContext::new(deps, (), bus);

    effect.handle(
        OrderEvent::Placed { order_id },
        ctx,
    ).await.unwrap();

    // Check emitted event
    let envelope = receiver.recv().await.unwrap();
    let event = envelope.downcast_ref::<OrderEvent>().unwrap();
    assert!(matches!(event, OrderEvent::Shipped { .. }));
}
```

Test reducers as pure functions:

```rust
#[test]
fn test_reducer_transforms_state() {
    let reducer = OrderReducer;
    let state = OrderState { status: None };
    let event = OrderEvent::Placed { order_id };

    let new_state = reducer.reduce(&state, &event);

    assert_eq!(new_state.status, Some(Status::Placed));
}
```

## License

MIT
