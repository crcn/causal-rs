---
title: "feat: Add journaled ctx.run() for replay-safe side effects"
type: feat
date: 2026-03-01
brainstorm: docs/brainstorms/2026-03-01-journaled-ctx-run-brainstorm.md
---

# Add journaled `ctx.run()` for replay-safe side effects

## Overview

Add `ctx.run(|| async { ... }).await` to `Context<D>` so handlers can execute side effects that are automatically replay-safe. For DirectRuntime, the closure executes inline. For durable runtimes (Restate), the result is journaled. Deprecate `is_replay()`.

## Problem Statement

Handlers currently guard side effects manually:

```rust
if !ctx.is_replay() {
    ctx.deps().shipping_api.ship(event.order_id).await?;
}
```

This is error-prone (easy to forget), loses the return value during replay, and provides no journaling. The `ctx.run()` pattern makes replay safety declarative and captures results.

## Proposed Solution

```rust
let tracking_id: String = ctx.run(|| async {
    ctx.deps().shipping_api.ship(event.order_id).await
}).await?;

Ok(OrderShipped { order_id, tracking_id })
```

## Technical Approach

### The Runtime Connection Problem

Context currently has no reference to Runtime. We need one for `ctx.run()` to delegate.

**Approach: Add a `SideEffectRunner` trait object to Context.**

The existing `Runtime::run()` wraps entire handler futures — it's the wrong granularity for per-side-effect journaling. Instead, add a new narrowly-scoped trait:

```rust
// crates/seesaw/src/handler/context.rs

/// Trait for executing side effects with optional journaling.
pub trait SideEffectRunner: Send + Sync {
    fn run_side_effect(
        &self,
        f: Pin<Box<dyn Future<Output = Result<serde_json::Value>> + Send>>,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value>> + Send>>;
}
```

- **DirectRunner** (default): Executes the future directly, returns result. No journaling.
- **Restate backend**: Journals the serialized result. On replay, returns journaled value.

### Phase 1: Core `ctx.run()` API

#### 1.1 Add `SideEffectRunner` trait

**File:** `crates/seesaw/src/handler/context.rs`

```rust
/// Executes side-effect closures with optional journaling.
///
/// Implementations determine how results are captured:
/// - `DirectRunner`: executes inline, no journaling
/// - Restate backend: journals result, replays from journal
pub trait SideEffectRunner: Send + Sync {
    fn run_side_effect(
        &self,
        f: Pin<Box<dyn Future<Output = Result<serde_json::Value>> + Send>>,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value>> + Send>>;
}

/// Default runner — executes side effects directly with no journaling.
pub struct DirectRunner;

impl SideEffectRunner for DirectRunner {
    fn run_side_effect(
        &self,
        f: Pin<Box<dyn Future<Output = Result<serde_json::Value>> + Send>>,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value>> + Send>> {
        f // pass-through
    }
}
```

#### 1.2 Add runner field to Context

**File:** `crates/seesaw/src/handler/context.rs`

```rust
pub struct Context<D> {
    // ... existing fields ...
    pub(crate) side_effect_runner: Arc<dyn SideEffectRunner>,
}
```

- Default to `Arc::new(DirectRunner)` in `Context::new()`
- Add `with_side_effect_runner()` builder method (pub(crate))
- Update `Clone` impl to clone the Arc

#### 1.3 Add `ctx.run()` method

**File:** `crates/seesaw/src/handler/context.rs`

```rust
impl<D> Context<D>
where
    D: Send + Sync + 'static,
{
    /// Execute a side-effect closure with replay safety.
    ///
    /// - **DirectRuntime:** Executes inline, returns result directly.
    /// - **Durable runtime (Restate):** Journals the result. On replay,
    ///   skips execution and returns the journaled value.
    ///
    /// The return type must implement `Serialize + DeserializeOwned` so
    /// durable runtimes can persist the result.
    ///
    /// ```rust,ignore
    /// let tracking_id: String = ctx.run(|| async {
    ///     ctx.deps().shipping_api.ship(order_id).await
    /// }).await?;
    /// ```
    pub async fn run<F, Fut, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce() -> Fut + Send,
        Fut: Future<Output = Result<T>> + Send,
        T: serde::Serialize + serde::de::DeserializeOwned + Send + 'static,
    {
        // Wrap user closure: execute, serialize result to JSON
        let fut = async move {
            let result = f().await?;
            let json = serde_json::to_value(&result)?;
            Ok(json)
        };

        // Delegate to runner (DirectRunner passes through, Restate journals)
        let json = self.side_effect_runner.run_side_effect(Box::pin(fut)).await?;

        // Deserialize back to T
        let result: T = serde_json::from_value(json)?;
        Ok(result)
    }
}
```

**Note on DirectRunner optimization:** For DirectRunner, the serialize/deserialize round-trip is unnecessary overhead. We can optimize later with a specialized path that skips serde for DirectRunner. Start simple, optimize if profiling shows it matters.

#### 1.4 Deprecate `is_replay()`

**File:** `crates/seesaw/src/handler/context.rs`

```rust
#[deprecated(since = "0.16.0", note = "Use ctx.run() for replay-safe side effects")]
pub fn is_replay(&self) -> bool {
    self.is_replay
}
```

Also deprecate on the `HandlerContext` trait.

### Phase 2: Wire into JobExecutor

#### 2.1 Pass runner to Context creation

**File:** `crates/seesaw/src/job_executor.rs`

In `execute_handler()` and `run_inline_effect()`, attach the side effect runner to Context:

```rust
let ctx = Context::new(...)
    .with_aggregator_registry(self.aggregator_registry.clone())
    .with_side_effect_runner(self.side_effect_runner.clone());
```

#### 2.2 Engine stores runner

**File:** `crates/seesaw/src/engine.rs`

Add `side_effect_runner: Arc<dyn SideEffectRunner>` to Engine. Default to `DirectRunner`. Add builder method:

```rust
pub fn with_side_effect_runner<R: SideEffectRunner + 'static>(mut self, runner: R) -> Self {
    self.side_effect_runner = Arc::new(runner);
    self
}
```

Pass it through to JobExecutor.

### Phase 3: Tests

#### 3.1 Unit test: DirectRunner passes through

**File:** `crates/seesaw/src/handler/context.rs` (test module)

```rust
#[tokio::test]
async fn ctx_run_executes_closure_and_returns_result() {
    let ctx = create_test_context();
    let result: String = ctx.run(|| async { Ok("hello".to_string()) }).await.unwrap();
    assert_eq!(result, "hello");
}

#[tokio::test]
async fn ctx_run_propagates_errors() {
    let ctx = create_test_context();
    let result: Result<String> = ctx.run(|| async { Err(anyhow::anyhow!("boom")) }).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn ctx_run_unit_return() {
    let ctx = create_test_context();
    let result: () = ctx.run(|| async { Ok(()) }).await.unwrap();
    assert_eq!(result, ());
}
```

#### 3.2 Integration test: ctx.run() in handler

**File:** `crates/seesaw/tests/engine_integration.rs`

```rust
#[tokio::test]
async fn ctx_run_executes_side_effect_in_handler() {
    // Handler uses ctx.run() to call an API and capture result
    let engine = Engine::new(deps)
        .with_handler(handler::on::<OrderPlaced>().then(|event, ctx| async move {
            let tracking: String = ctx.run(|| async {
                Ok(format!("TRACK-{}", event.order_id))
            }).await?;
            Ok(OrderShipped { order_id: event.order_id, tracking })
        }));

    let handle = engine.dispatch(OrderPlaced { order_id: Uuid::nil() });
    handle.settled().await.unwrap();
}
```

#### 3.3 Test: multiple ctx.run() calls

```rust
#[tokio::test]
async fn ctx_run_multiple_calls_return_independent_results() {
    let ctx = create_test_context();
    let a: i32 = ctx.run(|| async { Ok(1) }).await.unwrap();
    let b: String = ctx.run(|| async { Ok("two".into()) }).await.unwrap();
    assert_eq!(a, 1);
    assert_eq!(b, "two");
}
```

### Phase 4: Export and docs

#### 4.1 Re-export SideEffectRunner

**File:** `crates/seesaw/src/lib.rs`

Add `SideEffectRunner` and `DirectRunner` to public exports so backends can implement custom runners.

#### 4.2 Update README

Replace the `is_replay()` example with `ctx.run()` usage. Add a section explaining the pattern.

## Acceptance Criteria

- [ ] `ctx.run(|| async { ... }).await` compiles and returns `Result<T>` where `T: Serialize + DeserializeOwned`
- [ ] DirectRunner executes closure inline and returns result
- [ ] `is_replay()` marked `#[deprecated]`
- [ ] `SideEffectRunner` trait exported for backend implementors
- [ ] Unit tests for ctx.run() with success, error, unit return, and multiple calls
- [ ] Integration test with ctx.run() inside a handler
- [ ] All existing tests pass
- [ ] README updated

## Files to Change

| File | Change |
|------|--------|
| `crates/seesaw/src/handler/context.rs` | Add `SideEffectRunner` trait, `DirectRunner`, `ctx.run()`, deprecate `is_replay()` |
| `crates/seesaw/src/engine.rs` | Store `Arc<dyn SideEffectRunner>`, builder method, pass to JobExecutor |
| `crates/seesaw/src/job_executor.rs` | Accept runner, attach to Context |
| `crates/seesaw/src/lib.rs` | Export `SideEffectRunner`, `DirectRunner` |
| `crates/seesaw/tests/engine_integration.rs` | Integration test for ctx.run() in handler |
| `README.md` | Replace is_replay() example with ctx.run() |

## Open Questions

1. **Serde round-trip cost in DirectRunner:** For DirectRunner, we serialize to JSON and back unnecessarily. Could optimize with a `DirectRunner`-specific path that skips serde. Start simple, optimize later.
2. **Error journaling:** Restate retries on error, so errors are NOT journaled — only successful results. This matches the trait design (closure returning `Err` propagates immediately).
3. **Determinism constraint:** Document that `ctx.run()` call order must be deterministic within a handler. No conditional `ctx.run()` based on wall-clock time or IO results.
