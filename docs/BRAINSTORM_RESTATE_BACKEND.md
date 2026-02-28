# Brainstorm: Restate Backend

## Core Insight

Seesaw's value is project organization — decomposing complex flows into small, named, typed handlers connected by events. Restate's value is durable execution — journaled replay, exactly-once, crash recovery.

They compose when seesaw owns the routing graph and Restate owns the execution durability. Each seesaw handler becomes a single Restate journal entry. The user never touches Restate directly.

```
User writes:        #[handler] async fn ship_order(...)
Seesaw does:        route event → handler, extract, transition guard
Restate does:       journal the handler invocation, replay on crash
```

## The Key Idea

Wrap every handler invocation in `restate_ctx.run()` automatically. The handler body *is* the journal entry.

```rust
// User writes this — zero Restate awareness
#[handler(on = OrderEvent, extract(order_id), id = "ship_order")]
async fn ship_order(order_id: Uuid, ctx: Context<Deps>) -> Result<OrderEvent> {
    ctx.deps().shipping_api.ship(order_id).await?;
    Ok(OrderEvent::Shipped { order_id })
}

// Seesaw wraps it internally:
let result = restate_ctx.run(|| {
    handler.call(event, seesaw_ctx)
}).await?;
```

The decomposition pattern IS the journaling strategy. Small handlers = fine-grained journal entries. Seesaw already pushes you toward this structure.

## Why They Stop Fighting

| Concern | Seesaw owns | Restate owns |
|---|---|---|
| Handler routing (event → handler) | Yes | No |
| Extract/filter/transition guards | Yes | No |
| Handler naming + registry | Yes | No |
| Accumulate/join fan-in | Yes | No |
| Insight/observability of flow graph | Yes | No |
| Durable execution (crash recovery) | No (drop retry/timeout) | Yes |
| Exactly-once side effects | No | Yes |
| Journal-based replay | No | Yes |
| Timers/delayed execution | No (drop delay) | Yes |
| Virtual objects (keyed state) | No | Yes |

Seesaw's retry/timeout/delay/DLQ machinery becomes optional — only used when running without Restate (MemoryBackend, PostgresBackend). With RestateBackend, Restate handles all of that natively.

## Architecture

```
Engine::dispatch(event)
    |
    v
RestateBackend::publish(event)
    |
    v
Restate service: SeesawRouter
    |
    v
JobExecutor::execute_event(event)      <-- existing seesaw code
    |
    +-- inline handlers: each wrapped in restate_ctx.run()
    |       |
    |       v
    |   handler body executes durably
    |       |
    |       v
    |   emitted events --> RestateBackend::publish() (loop)
    |
    +-- queued handlers: restate_ctx.run() with Restate-native delay/retry
            |
            v
        JobExecutor::execute_handler()
            |
            v
        handler body executes durably
```

## RestateBackend Implementation Sketch

```rust
pub struct RestateBackend {
    restate_client: restate_sdk::Client,
    endpoint_addr: SocketAddr,
}

#[async_trait]
impl Backend for RestateBackend {
    fn name(&self) -> &'static str { "restate" }

    async fn publish(&self, event: DispatchedEvent) -> Result<()> {
        // Send event to the Restate SeesawRouter service
        self.restate_client
            .service::<SeesawRouter>()
            .process_event(event)
            .send()
            .await?;
        Ok(())
    }

    async fn serve<D>(
        &self,
        executor: Arc<JobExecutor<D>>,
        config: BackendServeConfig,
        shutdown: CancellationToken,
    ) -> Result<()>
    where
        D: Send + Sync + 'static,
    {
        // Register Restate endpoint with handler
        let endpoint = restate_sdk::endpoint()
            .bind(SeesawRouter::new(executor))
            .listen(self.endpoint_addr)
            .await?;

        // Run until shutdown
        tokio::select! {
            result = endpoint.serve() => result?,
            _ = shutdown.cancelled() => {},
        }
        Ok(())
    }
}
```

## Restate Service (the glue)

```rust
#[restate_sdk::service]
struct SeesawRouter<D> {
    executor: Arc<JobExecutor<D>>,
}

impl<D: Send + Sync + 'static> SeesawRouter<D> {
    async fn process_event(
        &self,
        restate_ctx: restate_sdk::Context<'_>,
        event: DispatchedEvent,
    ) -> Result<()> {
        let queued = to_queued_event(event);

        // Execute event routing (find matching handlers)
        let commit = self.executor
            .execute_event(&queued, &NullBackend, &config)
            .await?;

        // Each inline handler was already called inside execute_event.
        // But we want Restate durability — so we need to change
        // execute_event to accept a "runner" that wraps each handler call.

        // For emitted events, recurse through Restate:
        for emitted in commit.emitted_events {
            restate_ctx
                .service::<SeesawRouter>()
                .process_event(to_dispatched(emitted))
                .send()
                .await?;
        }

        // For queued handler intents, schedule via Restate:
        for intent in commit.queued_effect_intents {
            if let Some(delay) = intent.delay() {
                restate_ctx
                    .service::<SeesawRouter>()
                    .execute_handler(intent)
                    .send_with_delay(delay)
                    .await?;
            } else {
                restate_ctx
                    .service::<SeesawRouter>()
                    .execute_handler(intent)
                    .send()
                    .await?;
            }
        }

        Ok(())
    }

    async fn execute_handler(
        &self,
        restate_ctx: restate_sdk::Context<'_>,
        intent: QueuedHandlerIntent,
    ) -> Result<()> {
        // Wrap the handler execution in Restate's journal
        let result = restate_ctx.run(|| {
            self.executor.execute_handler(execution, &NullBackend, &config)
        }).await?;

        // Emitted events go back through the router
        for emitted in result.emitted_events {
            restate_ctx
                .service::<SeesawRouter>()
                .process_event(to_dispatched(emitted))
                .send()
                .await?;
        }

        // Restate handles retry natively — no need for seesaw's retry loop
        Ok(())
    }
}
```

## What Changes in Seesaw Core

Minimal changes needed:

1. **JobExecutor needs a "runner" abstraction** — currently it calls handlers directly. It needs a hook so RestateBackend can wrap each call in `restate_ctx.run()`. Something like:

```rust
pub trait HandlerRunner: Send + Sync {
    async fn run<F, T>(&self, f: F) -> Result<T>
    where
        F: Future<Output = Result<T>> + Send;
}

// Default: just run it
struct DirectRunner;
impl HandlerRunner for DirectRunner {
    async fn run<F, T>(&self, f: F) -> Result<T> { f.await }
}

// Restate: wrap in journal
struct RestateRunner<'a> {
    ctx: &'a restate_sdk::Context<'a>,
}
impl HandlerRunner for RestateRunner<'_> {
    async fn run<F, T>(&self, f: F) -> Result<T> {
        self.ctx.run(|| f).await
    }
}
```

2. **Queued handler scheduling becomes pluggable** — currently JobExecutor returns `QueuedHandlerIntent` structs for the backend to persist. RestateBackend would send them as Restate calls instead of DB rows. This already works — the backend handles the intents.

3. **Retry/timeout/delay become hints** — with Restate, these are informational. Restate manages retry natively. Seesaw's `max_attempts`, `timeout`, `delay` become parameters passed to Restate's scheduling rather than managed by seesaw's worker loop.

## What Doesn't Change

- `#[handler]` macro — completely unchanged
- `Engine`, `handler::on()`, `.extract()`, `.transition()` — unchanged
- `Handler<D>`, `HandlerRegistry`, `EventCodec` — unchanged
- `EventStore`, `Aggregate`, `Versioned` — unchanged (ES layer is independent)
- `Insight` — still works (events still flow through the same graph)
- User code — zero changes when switching backends

## EventStore + Restate Virtual Objects

Bonus composition: Restate virtual objects give single-writer-per-key semantics. Combined with seesaw's EventStore, you get:

- Virtual object key = aggregate ID
- Restate serializes all access to that aggregate (no optimistic concurrency conflicts)
- EventStore appends are always safe (no ConcurrencyError possible)
- Transition guards still work (load prev/next from EventStore)

```rust
#[restate_sdk::object]  // keyed by order_id
impl OrderProcessor {
    async fn process_event(&self, ctx: ObjectContext<'_>, event: OrderEvent) {
        // Restate guarantees single-writer per order_id
        // So EventStore append never conflicts
        let es = deps.event_store();
        let order = es.aggregate::<Order>(ctx.key()).load().await?;
        es.aggregate::<Order>(ctx.key())
            .append(order.version(), vec![event])  // always succeeds
            .await?;
    }
}
```

## Open Questions

1. **Restate SDK compatibility** — Restate's Rust SDK uses specific context types. Need to verify `ctx.run()` can wrap arbitrary async closures that capture seesaw state.

2. **Serialization boundary** — Restate journals serialize/deserialize inputs and outputs. Handler inputs (events) are already JSON-serializable. Handler outputs (emitted events) are also serializable. But `Context<Deps>` is not — it needs to be reconstructed on replay, not serialized. This means the runner needs to separate "journaled data" from "reconstructed context."

3. **Inline vs queued distinction** — With Restate, the inline/queued distinction might collapse. Everything runs durably. Do we still want inline handlers for fast-path, or just make everything go through Restate?

4. **Testing story** — MemoryBackend stays for tests (fast, no Restate dependency). RestateBackend for production. The handler code is identical either way.

5. **Gradual adoption** — Could start with PostgresBackend for simple handlers and RestateBackend only for handlers that need durable execution. Mix and match per handler? Or all-or-nothing per Engine?

## Summary

Seesaw = routing + organization + event sourcing.
Restate = durable execution.

The integration is thin: wrap handler calls in `restate_ctx.run()`, route emitted events through Restate service calls, let Restate manage retry/delay/timeout natively. User code doesn't change. The decomposition into small handlers that seesaw enforces is exactly the granularity Restate journals at.
