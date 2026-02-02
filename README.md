# Seesaw

An event-driven runtime for building reactive systems with a simple **Event → Effect → Event** flow.

Named after the playground equipment that balances back and forth — representing the continuous flow of events through the system.

## Workspace Structure

This repository is organized as a Cargo workspace:

- **[seesaw-core](./crates/seesaw)** - Core event-driven runtime
- **[seesaw-job-postgres](./crates/seesaw-job-postgres)** - PostgreSQL job queue implementation
- **[seesaw-outbox](./crates/seesaw-outbox)** - Transactional outbox pattern for durable events
- **[seesaw-persistence](./crates/seesaw-persistence)** - State persistence for crash recovery
- **[seesaw-testing](./crates/seesaw-testing)** - Testing utilities for event-driven workflows

## Core Principle

**Event → Effect → Event.** Simple, direct flow with clean state management.

## What Seesaw Is / Is Not

Seesaw **is**:

> An event-driven runtime where events flow through effects, which can emit new events.
> State flows through reducers. Edges provide clean entry points.

Seesaw is **not**:

- Full event sourcing
- A saga engine
- An actor framework
- A job system replacement

## Features

- **Event-Driven**: Events are the only signals - facts about what happened
- **Effect Handlers**: React to events, perform IO, emit new events
- **Reducers**: Pure state transformations before effects
- **Edges**: Clean entry points for triggering event flows
- **Type-Erased Bus**: Broadcast events across heterogeneous effects
- **Correlation Tracking**: `emit_and_await` for waiting on cascading work
- **Request/Response Pattern**: `dispatch_request` for edge code that needs responses
- **State Flow**: Per-execution state flows through reducers and effects
- **Testing Utilities**: Ergonomic test helpers for event-driven workflows

## Architecture

```
Edge → Event → Reducer → Effect → Event → Effect → ... (until settled)
                  ↓         ↓
              State'    Returns Event
```

## Quick Start

```rust
use seesaw_core::{
    Effect, EffectContext, EngineBuilder,
};
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
impl Effect<OrderEvent, MyDeps> for ShipEffect {
    type Event = OrderEvent;

    async fn handle(
        &mut self,
        event: OrderEvent,
        ctx: EffectContext<MyDeps>,
    ) -> Result<Option<OrderEvent>> {
        match event {
            OrderEvent::Placed { order_id } => {
                // Do IO: ship the order
                ctx.deps().shipping_api.ship(order_id).await?;
                // Return new event (Runtime emits it)
                Ok(Some(OrderEvent::Shipped { order_id }))
            }
            _ => Ok(None), // Event doesn't apply to this effect
        }
    }
}

struct NotifyEffect;

#[async_trait::async_trait]
impl Effect<OrderEvent, MyDeps> for NotifyEffect {
    type Event = OrderEvent;

    async fn handle(
        &mut self,
        event: OrderEvent,
        ctx: EffectContext<MyDeps>,
    ) -> Result<Option<OrderEvent>> {
        match event {
            OrderEvent::Shipped { order_id } => {
                // Do IO: notify customer
                ctx.deps().email_service.send(order_id, "Shipped!").await?;
                // Return new event
                Ok(Some(OrderEvent::Delivered { order_id }))
            }
            _ => Ok(None),
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let engine = EngineBuilder::new(MyDeps::new())
        .with_effect::<OrderEvent, _>(ShipEffect)
        .with_effect::<OrderEvent, _>(NotifyEffect)
        .build();

    // Start the engine (runs in background)
    let handle = engine.start();

    // Fire-and-forget
    handle.emit(OrderEvent::Placed { order_id: Uuid::new_v4() });

    // Or wait for all cascading work to complete
    handle.emit_and_await(OrderEvent::Placed { order_id: Uuid::new_v4() }).await?;

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
| Input  | Edge-originated requests              | `SignupRequested`  |
| Fact   | Effect-produced ground truth          | `SignedUp`         |
| Signal | Ephemeral UI updates (via `signal()`) | Typing indicators  |

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
    ) -> Result<Option<UserEvent>> {
        match event {
            UserEvent::SignupRequested { email, name } => {
                // Execute IO in one transaction
                let user = ctx.deps().db.transaction(|tx| async {
                    let user = User::create(&email, &name, tx).await?;
                    UserProfile::create(user.id, tx).await?;
                    Ok(user)
                }).await?;

                // Return new event (Runtime emits it)
                Ok(Some(UserEvent::SignedUp {
                    user_id: user.id,
                    email,
                }))
            }
            _ => Ok(None), // Event doesn't apply
        }
    }
}
```

Key properties:

- **Can be stateful**: Effects have `&mut self` and can maintain state across invocations
- **Return Option<Event>**: Return `Some(event)` to emit, `None` if event doesn't apply
- **Access state**: Via `ctx.state()` for per-execution state
- **Narrow context**: Only `deps()`, `state()`, `signal()`, and `tool_context()` available
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

### Edges

Edges are clean entry points that trigger event flows.

```rust
struct SignupEdge {
    email: String,
    name: String,
}

impl Edge<RequestState> for SignupEdge {
    type Data = User;

    fn execute(&self, _ctx: &EdgeContext<RequestState>) -> Option<Box<dyn Event>> {
        // Return initial event to trigger flow
        Some(Box::new(UserEvent::SignupRequested {
            email: self.email.clone(),
            name: self.name.clone(),
        }))
    }

    fn read(&self, state: &RequestState) -> Option<User> {
        // Read final result from state
        state.user.clone()
    }
}

// Usage
let user = engine.run(SignupEdge { email, name }, initial_state).await?
    .ok_or_else(|| anyhow!("signup failed"))?;
```

Key properties:

- **Entry points**: Where external inputs enter the system
- **Execute once**: Return initial event to trigger event flow
- **Read result**: Extract final data from settled state

### EffectContext

`EffectContext` provides a narrow API to effects:

```rust
// Access shared dependencies (database, APIs, config)
ctx.deps()

// Access per-execution state
ctx.state()

// Fire-and-forget signal for UI observability (typing indicators, progress)
ctx.signal(MySignalEvent::Progress { percent: 50 });

// Get correlation ID for outbox writes
ctx.outbox_correlation_id()

// Get correlation ID directly
ctx.correlation_id()
```

## Engine Usage

The `EngineBuilder` is the primary way to wire up seesaw:

```rust
let engine = EngineBuilder::new(deps)
    .with_effect::<OrderEvent, _>(ShipEffect)
    .with_effect::<OrderEvent, _>(NotifyEffect)
    .with_effect::<OrderEvent, _>(AuditEffect)
    .build();

let handle = engine.start();

// Fire-and-forget
handle.emit(OrderEvent::Placed { order_id });

// Wait for all cascading work to complete
handle.emit_and_await(OrderEvent::Placed { order_id }).await?;
```

Other builder methods:

- `.with_bus(bus)` — Use an existing EventBus
- `.with_inflight(tracker)` — Use an existing InflightTracker
- `.with_arc(deps)` — Use Arc-wrapped dependencies

### Using Edges

For structured workflows with state:

```rust
let mut engine = EngineBuilder::new(deps)
    .with_effect::<UserEvent, _>(SignupEffect)
    .build();

let user = engine.run(
    SignupEdge { email, name },
    RequestState::new(),
).await?
    .ok_or_else(|| anyhow!("signup failed"))?;
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

## Background Jobs

For background execution, effects can be combined with a job queue system. See `seesaw-job-postgres` for a PostgreSQL-based implementation.

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
async fn handle(&mut self, event: OrderEvent, ctx: EffectContext<Deps>) -> Result<Option<OrderEvent>> {
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
            Ok(Some(OrderEvent::Placed { order_id: order.id }))
        }
        _ => Ok(None),
    }
}
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
- Jobs for durable command execution
- Reapers for crash recovery

## Testing

Test effects by calling `handle` directly:

```rust
#[tokio::test]
async fn test_effect_handles_event() {
    let mut effect = ShipEffect;
    let deps = Arc::new(MockDeps::new());
    let bus = EventBus::new();
    let ctx = EffectContext::new(deps, (), bus);

    let result = effect.handle(
        OrderEvent::Placed { order_id },
        ctx,
    ).await.unwrap();

    assert!(matches!(result, Some(OrderEvent::Shipped { .. })));
}
```

### Testing Utilities

The `seesaw-testing` crate provides ergonomic test helpers for event-driven workflows.

```toml
[dev-dependencies]
seesaw_core = { version = "0.1" }
seesaw-testing = { version = "0.1" }
```

## License

MIT
