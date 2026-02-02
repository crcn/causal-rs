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
use seesaw_core::{Effect, EffectContext, EngineBuilder, RunContext};
use anyhow::Result;
use uuid::Uuid;

// Events are facts - what happened
// Note: Event trait is auto-implemented for Clone + Send + Sync + 'static
#[derive(Debug, Clone)]
enum OrderEvent {
    Placed { order_id: Uuid },
    Shipped { order_id: Uuid },
    Delivered { order_id: Uuid },
}

// Effects react to events and emit new events
struct ShipEffect;

#[async_trait::async_trait]
impl Effect<OrderEvent, MyDeps, ()> for ShipEffect {
    type Event = OrderEvent;

    async fn handle(
        &mut self,
        event: OrderEvent,
        ctx: EffectContext<MyDeps, ()>,
    ) -> Result<()> {
        match event {
            OrderEvent::Placed { order_id } => {
                // Do IO: ship the order
                ctx.deps().shipping_api.ship(order_id).await?;
                // Emit new event
                ctx.emit(OrderEvent::Shipped { order_id });
                Ok(())
            }
            _ => Ok(()), // Skip unhandled events
        }
    }
}

struct NotifyEffect;

#[async_trait::async_trait]
impl Effect<OrderEvent, MyDeps, ()> for NotifyEffect {
    type Event = OrderEvent;

    async fn handle(
        &mut self,
        event: OrderEvent,
        ctx: EffectContext<MyDeps, ()>,
    ) -> Result<()> {
        match event {
            OrderEvent::Shipped { order_id } => {
                // Do IO: notify customer
                ctx.deps().email_service.send(order_id, "Shipped!").await?;
                // Emit new event
                ctx.emit(OrderEvent::Delivered { order_id });
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut engine = EngineBuilder::new(MyDeps::new())
        .with_effect::<OrderEvent, _>(ShipEffect)
        .with_effect::<OrderEvent, _>(NotifyEffect)
        .build();

    // Use closure to trigger event flow
    engine.run(
        |ctx: &RunContext<MyDeps, ()>| {
            let order_id = Uuid::new_v4();
            ctx.emit(OrderEvent::Placed { order_id });
            Ok(())
        },
        (),
    ).await?;

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

Effects are event handlers that react to events, perform IO, and optionally emit new events.

```rust
struct SignupEffect;

#[async_trait::async_trait]
impl Effect<UserEvent, MyDeps> for SignupEffect {
    type Event = UserEvent;

    async fn handle(
        &mut self,
        event: UserEvent,
        ctx: EffectContext<MyDeps>,
    ) -> Result<()> {
        match event {
            UserEvent::SignupRequested { email, name } => {
                // Execute IO in one transaction
                let user = ctx.deps().db.transaction(|tx| async {
                    let user = User::create(&email, &name, tx).await?;
                    UserProfile::create(user.id, tx).await?;
                    Ok(user)
                }).await?;

                // Emit new event
                ctx.emit(UserEvent::SignedUp {
                    user_id: user.id,
                    email,
                });
                Ok(())
            }
            _ => Ok(()), // Skip unhandled events
        }
    }
}
```

Key properties:

- **Can be stateful**: Effects have `&mut self` and can maintain state across invocations
- **Emit events**: Use `ctx.emit()` to emit new events
- **Access state**: Via `ctx.state()` for per-execution state
- **Narrow context**: Only `deps()`, `state()`, `emit()`, and `correlation_id()` available
- **Batch support**: Override `handle_batch` for optimized bulk operations

### Reducers

Reducers are pure functions that transform state in response to events. They run before effects.

```rust
struct IncrementReducer;

impl Reducer<CountEvent, AppState> for IncrementReducer {
    fn reduce(&self, state: &AppState, event: &CountEvent) -> AppState {
        match event {
            CountEvent::Incremented { amount } => AppState {
                counter: state.counter + amount,
            },
            _ => state.clone(),
        }
    }
}
```

Key properties:

- **Pure**: No side effects, deterministic
- **Transform state**: Take current state and event, return new state
- **Run before effects**: Updated state is passed to effects via `ctx.state()`

### EffectContext

`EffectContext` provides a narrow API to effects:

```rust
// Access shared dependencies (database, APIs, config)
ctx.deps()

// Access per-execution state
ctx.state()

// Emit new events
ctx.emit(OrderEvent::Shipped { order_id });

// Get correlation ID for outbox writes
ctx.outbox_correlation_id()

// Get correlation ID directly
ctx.correlation_id()
```

## Engine Usage

The `EngineBuilder` is the primary way to wire up seesaw:

```rust
let mut engine = EngineBuilder::new(deps)
    .with_reducer::<OrderEvent, _>(OrderReducer)
    .with_effect::<OrderEvent, _>(ShipEffect)
    .with_effect::<OrderEvent, _>(NotifyEffect)
    .build();

// Run with a closure that emits initial events
engine.run(
    |ctx: &RunContext<Deps, OrderState>| {
        ctx.emit(OrderEvent::Placed { order_id });
        Ok(())
    },
    OrderState::default(),
).await?;
```

Builder methods:

- `.with_reducer::<Event, _>(reducer)` — Register pure state transformations
- `.with_effect::<Event, _>(effect)` — Register event handlers
- `.with_bus(bus)` — Use an existing EventBus
- `.with_inflight(tracker)` — Use an existing InflightTracker
- `.with_arc(deps)` — Use Arc-wrapped dependencies

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
