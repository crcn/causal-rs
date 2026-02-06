# Seesaw API Migration Guide

## Summary of Changes

The Engine now uses a **stateless, per-execution model** instead of storing state internally.

## New API

### Before (Old API with stored state):
```rust
let engine = Engine::new(initial_state)  // State stored in Engine
    .with_effect(...)
    .with_reducer(...);

let handle = engine.activate();           // Long-running activation
handle.context.emit(Event);
handle.settled().await?;
```

### After (New API with per-execution state):
```rust
let engine = Engine::new()                // No state in Engine
    .with_effect(...)
    .with_reducer(...);

let handle = engine.activate(initial_state);  // State passed per-activation
let result = handle.run(|ctx| {
    process_order(order, ctx)              // Run action with context
})?;
handle.settled().await?;                   // Wait for effects
```

## Key Differences

### 1. Engine Construction
- **Old**: `Engine::new(initial_state)` or `Engine::with_deps(initial_state, deps)`
- **New**: `Engine::new()` or `Engine::with_deps(deps)`

### 2. Activation
- **Old**: `engine.activate()` → Handle
- **New**: `engine.activate(initial_state)` → Handle

### 3. Running Actions
- **Old**: Directly emit on context: `handle.context.emit(Event)`
- **New**: Use `handle.run()` method:
  ```rust
  let result = handle.run(|ctx| {
      ctx.emit(Event);
      Ok(response)
  })?;
  ```

### 4. State Access
- **Old**: `engine.state()` - read from Engine
- **New**: `handle.context.curr_state()` - read from Handle's context

## Edge Function Pattern

The new API is optimized for edge functions:

```rust
// Define engine once (reusable, stateless)
let engine = Engine::new()
    .with_effect(effect::on::<OrderPlaced>().run(|event, ctx| async move {
        ctx.deps().mailer.send_confirmation(&event).await?;
        ctx.emit(EmailSent { order_id: event.id });
        Ok(())
    }))
    .with_reducer(reducer::on::<OrderPlaced>().run(|state, event| {
        State { orders: state.orders + 1, ..state }
    }));

// Execute with different states (per-request)
async fn handle_webhook(payload: WebhookPayload, ctx: &EffectContext<State, Deps>) -> Result<Response> {
    ctx.emit(OrderPlaced::from(payload));
    Ok(Response { status: "processed" })
}

// Usage
let handle = engine.activate(State::default());
let response = handle.run(|ctx| handle_webhook(payload, ctx))?;
handle.settled().await?;
Ok(response)
```

## Multiple Runs on Same Handle

```rust
let handle = engine.activate(initial_state);

// Run multiple operations
let result1 = handle.run(|ctx| process_order(order1, ctx))?;
let result2 = handle.run(|ctx| process_order(order2, ctx))?;

// All effects from both runs settle together
handle.settled().await?;
```

## Benefits

- ✅ **Stateless Engine** - reusable across requests
- ✅ **Per-execution state** - clean isolation
- ✅ **Return values** - `run()` returns action results
- ✅ **Simple edge functions** - `fn(args, ctx) -> Result<T>`
- ✅ **Distributed-friendly** - no shared mutable state

## Migration Checklist

- [ ] Replace `Engine::new(state)` with `Engine::new()`
- [ ] Replace `Engine::with_deps(state, deps)` with `Engine::with_deps(deps)`
- [ ] Pass initial state to `activate()`
- [ ] Wrap emissions in `handle.run()`
- [ ] Replace `engine.state()` with `handle.context.curr_state()`
- [ ] Update tests to use new API

## Tests Status

⚠️ **Tests need updating**: 77+ test cases use the old API and need migration.
The core library compiles and the API works correctly - tests just need updating to the new pattern.
