# Journaled `ctx.run()` for Replay-Safe Side Effects

**Date:** 2026-03-01
**Status:** Design complete, ready to plan

## What We're Building

A `ctx.run(|| async { ... }).await` method on `Context` that executes side-effect closures with automatic journaling. During live execution, the closure runs and the result is persisted. During replay, the closure is skipped and the result is loaded from the journal.

### Before

```rust
// Manual guard — easy to forget, no result capture
if !ctx.is_replay() {
    ctx.deps().shipping_api.ship(event.order_id).await?;
}
```

### After

```rust
// Declarative, journaled, replay-safe
let tracking_id: String = ctx.run(|| async {
    ctx.deps().shipping_api.ship(event.order_id).await
}).await?;

Ok(OrderShipped { order_id, tracking_id })
```

## Why This Approach

- **Piggybacks on Restate's `ctx.run()`** — Restate already journals side effect results. Seesaw's `ctx.run()` delegates to `Runtime::run()`, getting durable journaling for free.
- **DirectRuntime just executes inline** — No journaling overhead for non-durable use cases. Replay safety is a durable-runtime concern.
- **Eliminates the `is_replay()` foot-gun** — Users don't need to remember the guard. `is_replay()` will be deprecated.

## Key Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Storage | Delegate to `Runtime::run()` | Restate already journals. No new storage layer needed. |
| Runtime API | Use existing `Runtime::run()` | No new trait methods. ctx.run() wraps closure and passes through. |
| DirectRuntime behavior | Execute inline, no journal | Simplest path. Journaling is a durable-runtime feature. |
| Signature | `ctx.run(\|\| async { ... }).await` | No label needed — Restate uses ordinal position. Clean API. |
| Serde bounds | Always require `Serialize + DeserializeOwned` | Consistent API. No surprises when switching to Restate. |
| `is_replay()` | Deprecate | `ctx.run()` covers the primary use case. Guide users toward the safer pattern. |

## API Design

```rust
impl<D> Context<D> {
    /// Execute a side-effect closure with journaling support.
    ///
    /// - **Live execution:** Runs the closure, journals the result via Runtime.
    /// - **Replay:** Skips the closure, returns the journaled result.
    /// - **DirectRuntime:** Runs inline with no journaling.
    pub async fn run<F, Fut, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce() -> Fut + Send,
        Fut: Future<Output = Result<T>> + Send,
        T: Serialize + DeserializeOwned + Send + 'static,
    {
        // Delegates to self.runtime.run(...)
    }

    #[deprecated(note = "Use ctx.run() for replay-safe side effects")]
    pub fn is_replay(&self) -> bool { ... }
}
```

## Open Questions

- **Error journaling:** If the closure returns `Err`, should the error be journaled too? Restate retries on error, so probably not — but worth confirming.
- **Multiple ctx.run() calls:** Ordinal-based journaling means the order of `ctx.run()` calls must be deterministic across live/replay. This is naturally true if handler code is deterministic (no branching on wall-clock time etc). Document this constraint.
- **Unit return:** `ctx.run(|| async { api.fire_and_forget().await; Ok(()) }).await?` — works fine, `()` is serializable. No special case needed.
