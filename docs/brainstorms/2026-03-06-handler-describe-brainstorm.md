---
date: 2026-03-06
topic: handler-describe
---

# Handler Describe — Introspectable Gate Progress

## What We're Building

An optional `.describe()` method on filtered handlers that returns a typed struct
(serialized to JSON) representing the handler's gate status. The output is persisted
to the Store so external UIs (flow visualization) can render per-handler progress
without domain knowledge.

## Why This Approach

**Considered and rejected:**

- **Gate-as-aggregate (Approach A):** A new `HandlerGate` trait registered in the
  `AggregatorRegistry`. Elegant but over-engineered — only 2 real gates exist today,
  both trivial set comparisons. Doesn't earn its weight.

- **Separate `describe()` on Engine:** Engine has the typed closures + aggregates,
  but persistence belongs on the Store. The flow UI shouldn't need access to the
  live Engine process.

**Chosen: `describe()` on `FilteredHandlerBuilder`, persisted to Store.**

- `describe` is paired with `filter` — they read the same aggregate state
- `describe` runs right before filter evaluation during `execute_event`
- Output is `T: Serialize`, erased to `serde_json::Value` internally
- Engine persists results to Store via `set_handler_descriptions()`
- Any consumer queries `store.get_handler_descriptions(correlation_id)`

## Key Decisions

- **`describe` returns a typed struct, not `serde_json::Value`:** User code is
  `|ctx| FinalizeGate { completed, remaining }`. Seesaw serializes internally.

- **Runs during `execute_event`, not `execute_handler`:** Aggregates are already
  updated (line 406 in engine.rs). We're already iterating matching handlers to
  create intents. Natural place to call `describe` and collect results.

- **Persisted to Store, not live-queried from Engine:** The flow UI is a separate
  process. It queries the Store (Postgres) directly. No coupling to the live Engine.

- **`filter` stays as-is:** Simple event-match handlers keep using closures without
  `describe`. Gates are for stateful multi-event convergence points that need
  introspection.

## Implementation Plan

### 1. `handler/types.rs` — Handler struct

```rust
pub(crate) describe: Option<Arc<dyn Fn(&Context<D>) -> serde_json::Value + Send + Sync>>,

pub fn has_describe(&self) -> bool { self.describe.is_some() }
pub fn call_describe(&self, ctx: &Context<D>) -> Option<serde_json::Value> {
    self.describe.as_ref().map(|f| f(ctx))
}
```

### 2. `handler/builders.rs` — FilteredHandlerBuilder

```rust
pub fn describe<T, Desc>(self, f: Desc) -> Self
where
    T: Serialize + Send + Sync + 'static,
    Desc: Fn(&Context<D>) -> T + Send + Sync + 'static,
```

Wraps `f` to serialize `T` → `serde_json::Value`. All other `.then()` impls
(6 total) set `describe: None`.

### 3. `store.rs` — Store trait

```rust
async fn set_handler_descriptions(
    &self, _correlation_id: Uuid,
    _descriptions: HashMap<String, serde_json::Value>,
) -> Result<()> { Ok(()) }

async fn get_handler_descriptions(
    &self, _correlation_id: Uuid,
) -> Result<HashMap<String, serde_json::Value>> { Ok(HashMap::new()) }
```

### 4. `job_executor.rs` — execute_event

After matching handlers, before building intents: for each handler with `describe`,
build a lightweight context and call it. Return descriptions via `EventCommit`.

### 5. `types.rs` — EventCommit

```rust
pub handler_descriptions: HashMap<String, serde_json::Value>,
```

### 6. `engine.rs` — settle loop

After `complete_event`, if `commit.handler_descriptions` is non-empty, call
`store.set_handler_descriptions(correlation_id, descriptions)`.

### 7. `memory_store.rs`

Implement the two new Store methods with `DashMap<Uuid, HashMap<String, Value>>`.

## User-Facing API

```rust
on::<SynthesisRoleCompleted>()
    .id("lifecycle:finalize")
    .filter(|event, ctx: &Context<Deps>| {
        let (_, state) = ctx.singleton::<PipelineState>();
        state.completed_synthesis_roles.is_superset(&all_synthesis_roles())
    })
    .describe(|ctx: &Context<Deps>| {
        let (_, state) = ctx.singleton::<PipelineState>();
        let needed = all_synthesis_roles();
        FinalizeGate {
            completed: state.completed_synthesis_roles.iter().map(|r| format!("{r:?}")).collect(),
            remaining: needed.difference(&state.completed_synthesis_roles).map(|r| format!("{r:?}")).collect(),
        }
    })
    .then(|event, ctx| async move { Ok(events![]) })
```

## Flow UI Query

```rust
// API endpoint — no Engine access needed
let descriptions = store.get_handler_descriptions(correlation_id).await?;
// { "lifecycle:finalize": { "completed": ["RoleA"], "remaining": ["RoleC"] } }
```

## Open Questions

- Should `describe` also be available on non-filtered handlers (e.g. `on::<E>().describe(...)`)?
  Not needed today but trivial to add later.
- Should the Store persist describe history (append) or just latest (upsert)?
  Upsert is simpler and sufficient for the flow UI.

## Next Steps

→ Implement in seesaw-rs, then wire up in consuming app's API + flow UI.
