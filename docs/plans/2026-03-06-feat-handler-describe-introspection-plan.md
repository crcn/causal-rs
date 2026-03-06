---
title: "feat: Handler describe() for flow UI introspection"
type: feat
date: 2026-03-06
---

# feat: Handler `.describe()` for flow UI introspection

## Overview

Add an optional `.describe()` method to `FilteredHandlerBuilder` that returns a typed
struct (`T: Serialize`) representing the handler's gate status. The output is serialized
to JSON, persisted to the Store per `(correlation_id, handler_id)`, and queryable by
external UIs for rendering per-handler progress on flow visualization nodes.

## Problem Statement

Filtered handlers in seesaw use opaque closures (`Fn(&E, &Context<D>) -> bool`) as
gates. There is no way for an admin UI or flow visualization to ask "how close is this
handler to opening?" without hardcoding domain knowledge about each gate. Adding a
handler or changing a gate requires matching frontend changes.

## Proposed Solution

A generic `describe()` builder method that co-locates gate progress reporting with the
filter itself. The handler declares its own progress shape — the UI renders whatever it
gets. No domain coupling.

```rust
on::<SynthesisRoleCompleted>()
    .id("lifecycle:finalize")
    .filter(|event, ctx: &Context<Deps>| {
        let (_, state) = ctx.singleton::<PipelineState>();
        state.completed_synthesis_roles.is_superset(&all_synthesis_roles())
    })
    .describe(|ctx: &Context<Deps>| {
        let (_, state) = ctx.singleton::<PipelineState>();
        FinalizeGate {
            completed: state.completed_synthesis_roles.iter().collect(),
            remaining: needed.difference(&state.completed_synthesis_roles).collect(),
        }
    })
    .then(|event, ctx| async move { Ok(events![]) })
```

Flow UI queries:
```rust
let descriptions = store.get_handler_descriptions(correlation_id).await?;
// { "lifecycle:finalize": { "completed": ["RoleA"], "remaining": ["RoleC"] } }
```

## Key Decisions

- **Describe returns `T: Serialize`, not `serde_json::Value`** — user code stays typed, seesaw serializes internally.
- **Runs during `execute_event`** — after aggregates update, while iterating matching handlers to create intents. NOT during `execute_handler`.
- **Persisted to Store** — external UIs query the Store directly, no coupling to the live Engine process.
- **Merge semantics** — `set_handler_descriptions` upserts per `(correlation_id, handler_id)`. A flow with 5 gated handlers triggered by different event types shows all 5 simultaneously.
- **Errors are non-fatal** — describe panics are caught with `catch_unwind`, serialize failures logged as warnings. Never fail `execute_event` because of describe.
- **Context-only signature** — `describe` receives `&Context<D>`, not the event. Purpose is aggregate-level gate progress, not per-event detail.
- **FilteredHandlerBuilder only** — expand to other builders later if needed.
- **No initial seeding** — UI handles missing descriptions gracefully.
- **Upsert latest, no history** — Store overwrites per handler per correlation.

## Acceptance Criteria

- [x] `FilteredHandlerBuilder` has a `.describe()` method accepting `Fn(&Context<D>) -> T` where `T: Serialize`
- [x] `Handler<D>` stores describe as `Option<Arc<dyn Fn(&Context<D>) -> serde_json::Value + Send + Sync>>`
- [x] `execute_event` calls describe for matching handlers, collects results into `EventCommit`
- [x] Engine persists describe results via `store.set_handler_descriptions()` after `complete_event`
- [x] Store trait has `set_handler_descriptions` and `get_handler_descriptions` with default no-ops
- [x] MemoryStore implements both methods
- [x] Describe panics and serialize failures are caught and logged, never crash the settle loop
- [x] Handlers without `.describe()` are unaffected (describe field is `None`)
- [x] `cargo test` passes with no regressions

## MVP

### 1. `handler/types.rs` — Handler struct

Add `describe` field and accessors.

```rust
// crates/seesaw/src/handler/types.rs

// In Handler<D> struct, after `priority`:
pub(crate) describe: Option<Arc<dyn Fn(&Context<D>) -> serde_json::Value + Send + Sync>>,

// In Clone impl, after priority:
describe: self.describe.clone(),

// New methods on impl<D> Handler<D>:
pub fn has_describe(&self) -> bool {
    self.describe.is_some()
}

pub fn call_describe(&self, ctx: &Context<D>) -> Option<serde_json::Value> {
    self.describe.as_ref().map(|f| f(ctx))
}
```

**5 `Handler { .. }` literals need `describe: None`:**
- `JoinHandlerBuilder::then()` (line ~448)
- `HandlerBuilder<Typed<E>, Filter, Started>::then()` (line ~566)
- `HandlerBuilder<Untyped, NoFilter, NoStarted>::then()` (line ~610)
- `HandlerBuilder<Untyped, NoFilter, WithStarted>::then()` (line ~655)
- `TransitionHandlerBuilder::then()` (line ~848)

### 2. `handler/builders.rs` — FilteredHandlerBuilder

Add `describe` field and builder method.

```rust
// crates/seesaw/src/handler/builders.rs

// Add field to FilteredHandlerBuilder:
pub struct FilteredHandlerBuilder<E, Started, D, G> {
    inner: HandlerBuilder<Typed<E>, NoFilter, Started>,
    filter_fn: G,
    describe_fn: Option<Arc<dyn Fn(&Context<D>) -> serde_json::Value + Send + Sync>>,
    _deps: PhantomData<D>,
}

// Add .describe() method:
impl<E, Started, D, G> FilteredHandlerBuilder<E, Started, D, G>
where
    E: Clone + Send + Sync + 'static,
    D: Send + Sync + 'static,
{
    pub fn describe<T, Desc>(mut self, f: Desc) -> Self
    where
        T: serde::Serialize + Send + Sync + 'static,
        Desc: Fn(&Context<D>) -> T + Send + Sync + 'static,
    {
        self.describe_fn = Some(Arc::new(move |ctx| {
            serde_json::to_value(f(ctx)).unwrap_or_else(|e| {
                tracing::warn!("describe serialization failed: {}", e);
                serde_json::Value::Null
            })
        }));
        self
    }
}

// In FilteredHandlerBuilder::then(), pass describe_fn to Handler:
Handler {
    // ... existing fields ...
    describe: self.describe_fn,
}

// Update .filter() to initialize describe_fn: None:
pub fn filter<D, F>(self, predicate: F) -> FilteredHandlerBuilder<E, Started, D, F> {
    FilteredHandlerBuilder {
        inner: self,
        filter_fn: predicate,
        describe_fn: None,
        _deps: PhantomData,
    }
}
```

### 3. `types.rs` — EventCommit

```rust
// crates/seesaw/src/types.rs

// Add to EventCommit:
pub struct EventCommit {
    // ... existing fields ...
    pub handler_descriptions: std::collections::HashMap<String, serde_json::Value>,
}
```

### 4. `store.rs` — Store trait

```rust
// crates/seesaw/src/store.rs

// Add two new optional methods with default no-ops:

/// Upsert handler gate descriptions for a correlation.
///
/// Merges `descriptions` into any existing entries for `correlation_id`.
/// Each key is a handler ID, each value is the serialized describe output.
async fn set_handler_descriptions(
    &self,
    _correlation_id: Uuid,
    _descriptions: std::collections::HashMap<String, serde_json::Value>,
) -> Result<()> {
    Ok(())
}

/// Read all handler gate descriptions for a correlation.
async fn get_handler_descriptions(
    &self,
    _correlation_id: Uuid,
) -> Result<std::collections::HashMap<String, serde_json::Value>> {
    Ok(std::collections::HashMap::new())
}
```

### 5. `memory_store.rs` — MemoryStore implementation

```rust
// crates/seesaw/src/memory_store.rs

// Add field to MemoryStore:
handler_descriptions: Arc<DashMap<Uuid, HashMap<String, serde_json::Value>>>,

// Initialize in new():
handler_descriptions: Arc::new(DashMap::new()),

// Implement Store methods:
async fn set_handler_descriptions(
    &self,
    correlation_id: Uuid,
    descriptions: HashMap<String, serde_json::Value>,
) -> Result<()> {
    let mut entry = self.handler_descriptions.entry(correlation_id).or_default();
    entry.extend(descriptions); // merge semantics
    Ok(())
}

async fn get_handler_descriptions(
    &self,
    correlation_id: Uuid,
) -> Result<HashMap<String, serde_json::Value>> {
    Ok(self.handler_descriptions
        .get(&correlation_id)
        .map(|e| e.value().clone())
        .unwrap_or_default())
}
```

### 6. `job_executor.rs` — execute_event

Call describe for matching handlers during event processing.

```rust
// crates/seesaw/src/job_executor.rs

// In execute_event(), after matching_handlers (line ~110), before building intents:

let mut handler_descriptions = std::collections::HashMap::new();
for handler in &matching_handlers {
    if handler.has_describe() {
        let ctx = self.make_context(
            handler.id.clone(),
            format!("describe::{}", handler.id),
            event.correlation_id,
            event.event_id,
            event.parent_id,
        );
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            handler.call_describe(&ctx)
        })) {
            Ok(Some(value)) => {
                handler_descriptions.insert(handler.id.clone(), value);
            }
            Ok(None) => {} // no describe on this handler
            Err(_) => {
                tracing::warn!(
                    handler_id = %handler.id,
                    "describe() panicked, skipping"
                );
            }
        }
    }
}

// Add to EventCommit return:
Ok(EventCommit {
    // ... existing fields ...
    handler_descriptions,
})
```

### 7. `engine.rs` — settle loop persistence

```rust
// crates/seesaw/src/engine.rs

// In settle loop, after complete_event (line ~417):
match executor.execute_event(&event, &event_config).await {
    Ok(commit) => {
        // Persist describe results before complete_event
        if !commit.handler_descriptions.is_empty() {
            if let Err(e) = self.store.set_handler_descriptions(
                commit.correlation_id,
                commit.handler_descriptions.clone(),
            ).await {
                tracing::warn!(
                    correlation_id = %commit.correlation_id,
                    "Failed to persist handler descriptions: {}",
                    e
                );
            }
        }
        self.store
            .complete_event(EventOutcome::Processed(commit))
            .await?;
    }
    // ... error arm unchanged ...
}
```

## Testing Strategy

### Unit tests (`handler/builders.rs`)

```rust
// crates/seesaw/src/handler/builders.rs (tests module)

#[test]
fn describe_stores_closure_on_handler() {
    let handler = on::<QueueEvent>()
        .id("described")
        .filter(|_event, _ctx: &Context<Deps>| true)
        .describe(|_ctx: &Context<Deps>| serde_json::json!({"ready": true}))
        .then(|_event: Arc<QueueEvent>, _ctx: Context<Deps>| async move {
            Ok(crate::events![])
        });
    assert!(handler.has_describe());
}

#[test]
fn handler_without_describe_has_none() {
    let handler = on::<QueueEvent>()
        .id("no_describe")
        .filter(|_event, _ctx: &Context<Deps>| true)
        .then(|_event: Arc<QueueEvent>, _ctx: Context<Deps>| async move {
            Ok(crate::events![])
        });
    assert!(!handler.has_describe());
}
```

### Integration test (`tests/engine_integration.rs`)

```rust
// Test that describe output flows through to the Store

#[tokio::test]
async fn describe_output_persisted_to_store() {
    // 1. Build engine with a filtered handler that has .describe()
    // 2. Emit an event that matches the handler
    // 3. Settle
    // 4. Query store.get_handler_descriptions(correlation_id)
    // 5. Assert the description JSON matches expected shape
}

#[tokio::test]
async fn describe_merges_across_events() {
    // 1. Build engine with two filtered handlers for different event types, both with .describe()
    // 2. Emit event A (matches handler 1)
    // 3. Emit event B (matches handler 2)
    // 4. Settle
    // 5. Query descriptions — both should be present
}

#[tokio::test]
async fn describe_panic_does_not_crash_settle() {
    // 1. Build engine with a handler whose describe panics
    // 2. Emit matching event
    // 3. Settle should succeed
    // 4. Handler should still run (filter + then still work)
}
```

## References

- Brainstorm: `docs/brainstorms/2026-03-06-handler-describe-brainstorm.md`
- Handler builders: `crates/seesaw/src/handler/builders.rs`
- Handler types: `crates/seesaw/src/handler/types.rs`
- Job executor: `crates/seesaw/src/job_executor.rs`
- Store trait: `crates/seesaw/src/store.rs`
- Memory store: `crates/seesaw/src/memory_store.rs`
- Engine settle loop: `crates/seesaw/src/engine.rs:350-550`
- EventCommit: `crates/seesaw/src/types.rs:109-124`
