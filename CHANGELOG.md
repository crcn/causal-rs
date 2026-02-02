# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.5.0] - 2026-02-02

### 🚨 BREAKING CHANGES - Complete Architecture Overhaul

Migrated from Shay's Redux-style stateful engine to a **stateless, per-execution architecture** optimized for distributed edge functions.

### Changed - Breaking

**Engine is Now Stateless**
- Engine no longer stores state internally
- State is passed per-activation via `activate(initial_state)`
- Removed `Engine::state()` method
- State is scoped to Handle lifetime only

**API Pattern Changes**
- `Engine::new(state)` → `Engine::new()` (no state parameter)
- `Engine::with_deps(state, deps)` → `Engine::with_deps(deps)`
- `activate()` → `activate(initial_state)` (now requires state)
- Added `Handle::run()` for executing actions with results

**Closure-Based API** (from Shay)
- Replaced trait implementations with builder pattern
- `effect::on::<Event>().run(|event, ctx| async { ... })`
- `reducer::on::<Event>().run(|state, event| { ... })`
- No more `#[async_trait]` or trait implementations needed

**Effect Context Changes**
- Removed: `state()`, `correlation_id()`, `outbox_correlation_id()`
- Added: `prev_state()`, `next_state()`, `curr_state()`, `within()`
- Effects see both pre and post-reducer state

### Added

- **`Handle::run()` method**: Execute actions and return results
- **Pipedream integration**: Stream composition via pipedream-rs
- **State transition filters**: `effect::on().transition()`
- **Event filters**: `effect::on().filter()`
- **Lifecycle hooks**: `effect::on().started()`

### Removed

- `EngineBuilder` - use `Engine::new()` directly
- `RunContext` - use `Handle::run()` instead
- `engine.run(closure, state)` - use `activate(state).run()`
- State storage in Engine
- Trait-based Effect/Reducer API (use closure builders)

### Migration Guide

**Before (v0.4.0 - Trait-based):**
```rust
// Define effect struct
struct ShipEffect;

#[async_trait]
impl Effect<OrderEvent, Deps, State> for ShipEffect {
    type Event = OrderEvent;

    async fn handle(&mut self, event: OrderEvent, ctx: EffectContext) -> Result<()> {
        match event {
            OrderEvent::Placed { order_id } => {
                ctx.deps().ship(order_id).await?;
                ctx.emit(OrderEvent::Shipped { order_id });
                Ok(())
            }
            _ => Ok(())
        }
    }
}

// Create engine with state
let engine = EngineBuilder::new(deps)
    .with_effect::<OrderEvent, _>(ShipEffect)
    .build();

// Execute
engine.run(|ctx| {
    ctx.emit(OrderEvent::Placed { order_id });
    Ok(())
}, initial_state).await?;
```

**After (v0.5.0 - Closure-based, stateless):**
```rust
// Define engine once (stateless, reusable)
let engine = Engine::new()
    .with_effect(effect::on::<OrderEvent>().run(|event, ctx| async move {
        if let OrderEvent::Placed { order_id } = event.as_ref() {
            ctx.deps().ship(*order_id).await?;
            ctx.emit(OrderEvent::Shipped { order_id: *order_id });
        }
        Ok(())
    }));

// Execute per-request
let handle = engine.activate(State::default());
let result = handle.run(|ctx| {
    ctx.emit(OrderEvent::Placed { order_id });
    Ok(Response { status: "ok" })
})?;
handle.settled().await?;
```

See [API_MIGRATION.md](./API_MIGRATION.md) for complete migration guide.

## [0.4.0] - 2025-01-31

### Changed

- Updated `Effect::handle()` to return `Result<Option<Event>>` instead of `Result<Event>`
- Effects can now skip unhandled events by returning `Ok(None)`
- Removed need for verbose `unreachable!()` blocks in effect handlers

### Migration Guide

Before (v0.3.x):
```rust
async fn handle(&mut self, event: E, ctx: EffectContext<D, S>) -> Result<E> {
    match event {
        Event::Started => {
            Ok(Event::Completed)
        }
        _ => unreachable!("This effect only handles Started events")
    }
}
```

After (v0.4.0):
```rust
async fn handle(&mut self, event: E, ctx: EffectContext<D, S>) -> Result<Option<E>> {
    match event {
        Event::Started => {
            Ok(Some(Event::Completed))
        }
        _ => Ok(None) // Clean skip
    }
}
```

[0.5.0]: https://github.com/crcn/seesaw-rs/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/crcn/seesaw-rs/compare/v0.3.0...v0.4.0
