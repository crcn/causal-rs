---
date: 2026-02-28
topic: backend-simplification
---

# Backend Simplification: Seesaw + Restate

## What We're Building

Strip seesaw down to its core value — event routing, handler composition, ES primitives — and delete all backend infrastructure that Restate replaces. Seesaw runs *inside* Restate handlers; it doesn't wrap or manage Restate.

## Mental Model

```
Restate (durability, retries, state)
  └── Seesaw (routing, composition, DSL, settle loop)
        └── HandlerRunner wraps ctx.run() for durable execution
```

```rust
// Restate workflow
impl OrderWorkflow for OrderWorkflowImpl {
    async fn run(&self, ctx: &mut WorkflowContext, input: OrderCreated) -> Result<()> {
        let engine = Engine::new(deps)
            .with_runner(RestateRunner::new(ctx))
            .with_handler(on::<OrderCreated>().then(...))
            .with_handler(on::<OrderValidated>().then(...));

        engine.dispatch(input).settled().await?;
        Ok(())
    }
}
```

- **Seesaw** owns: event routing, handler composition, DSL ergonomics, settle loop, ES primitives
- **Restate** owns: durability, retries, state persistence, observability
- **HandlerRunner** bridges them: wraps each handler call in `ctx.run()`

## Why This Approach

Seesaw's value is its ergonomic handler DSL (`on::<E>().then(...)`) and deterministic settle loop — not its queue infrastructure. Restate is purpose-built for durable execution and does queuing, retries, DLQ, and observability better than a hand-rolled Postgres backend ever will.

By running seesaw inside Restate, we get durability for free without changing seesaw's public API. The `HandlerRunner` trait already exists as the integration point.

## Key Decisions

- **Delete `seesaw-postgres`**: Its durable queue store and InsightStore impl are both replaced by Restate.
- **Delete `seesaw-memory` (as a separate crate)**: Fold `MemoryEventStore` into seesaw core. Drop the `MemoryStore` InsightStore impl.
- **Delete `seesaw-insight`**: Restate's built-in dashboard and event projection replace it.
- **Keep Engine internals**: The settle loop, `memory_store.rs`, `JobExecutor`, `HandlerRegistry`, `EventCodec` all stay — they're the in-process orchestration that makes seesaw useful.
- **Keep `HandlerRunner` trait as-is**: It's already the right shape. A `RestateRunner` wraps `ctx.run()` around each handler invocation.
- **Keep `EventStore` trait + `MemoryEventStore`**: Event sourcing is orthogonal to Restate (Restate is key-value, not event-sourced). Fold the in-memory impl into seesaw core for tests/examples.

## What Gets Deleted

| Crate/Module | Reason |
|---|---|
| `crates/seesaw-postgres/` | Queue store → Restate. InsightStore → dropped. |
| `crates/seesaw-memory/` | InsightStore impl → dropped. MemoryEventStore → folded into core. |
| `crates/seesaw-insight/` | Restate dashboard replaces it. |

## What Stays

| Module | Purpose |
|---|---|
| `engine.rs` | Settle loop, dispatch, in-process orchestration |
| `handler/` | DSL (`on::<E>().then(...)`), builders, context, join mode |
| `handler_runner.rs` | Pluggable execution — Restate integration point |
| `handler_registry.rs` | Handler storage and lookup |
| `job_executor.rs` | Event routing, codec dispatch, handler execution |
| `memory_store.rs` | In-memory queue for settle loop |
| `event_codec.rs` | Event serialization/deserialization |
| `es/` | EventStore trait, Aggregate, AggregateLoader, projectors |
| `types.rs` | Internal plumbing types |
| `process.rs` | DispatchFuture, ProcessHandle, SettleFuture |
| `insight.rs` | InsightEvent types (used by on_insight callback) |
| `seesaw_core_macros/` | #[handler], #[handles], etc. |

## Open Questions

- **`PostgresEventStore`**: Does ES need a Postgres impl, or is `MemoryEventStore` sufficient with Restate handling durability? Could live as a thin optional crate later if needed.
- **Insight callback**: `Engine::with_on_insight()` still emits `InsightEvent`s. Keep for user-land observability, or remove since Restate provides tracing?
- **Worker configs**: `EventWorkerConfig` and `HandlerWorkerConfig` are only used by the settle loop. Keep as tuning knobs, or simplify?

## Next Steps

1. Move `MemoryEventStore` from `seesaw-memory` into `seesaw/src/es/`
2. Delete `crates/seesaw-postgres/`, `crates/seesaw-memory/`, `crates/seesaw-insight/`
3. Update workspace `Cargo.toml` members
4. Update examples that depend on deleted crates
5. Build a `RestateRunner` implementation (separate task)
