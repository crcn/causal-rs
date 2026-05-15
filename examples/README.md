# Examples

Runnable causal-rs examples backed by KurrentDB.

## What's here

| Example | Backend | What it shows |
|---------|---------|---------------|
| [`http-fetcher`](http-fetcher/) | KurrentDB + Postgres | Production-shape wiring: Kurrent for the event log, `PgReactorOutbox` for outbox + checkpoints. Reactor fans out HTTP fetches, emits success/failure events per request. |
| [`ai-summarizer`](ai-summarizer/) | KurrentDB + `MemoryStore` | Minimal wiring: Kurrent for the log, in-memory outbox + checkpoints (single-process, ephemeral). Reactor calls the Anthropic API and emits `Summarized` / `SummaryFailed`. |

## The shape

Every example follows the same three steps:

1. **Define `Event`s.** Typed structs implementing `causal::Event` — one `CATEGORY` per logical stream, `stream_id` for routing.
2. **Define a `Reactor`.** A struct implementing `causal::Reactor` with `type Trigger = …` and `async fn react(…) -> Result<Events>`.
3. **Build the engine.** `EngineBuilder::new(log, checkpoint, outbox)` casts three backend trait objects, `.with_reactor(R)` registers each consumer, `.build()` returns the live engine. `engine.emit(...).settled().await?` runs the full causal chain to quiescence.

## Why no adapters

Earlier drafts shipped `causal-http` / `causal-anthropic` wrapper crates. They were deleted. The library-side helpers added nothing over `reqwest` directly:

```rust
// adapter:
ctx.deps().http.fetch(&url).await?

// direct:
self.http_client.get(&url).send().await?
```

Same code, no extra crate. Every HTTP use case has different needs (rate limits, retries, auth) — one adapter can't cover them. Put the standard-library client in your reactor's state and use it.

## Running

Each example has its own README. The short version:

```sh
cd examples/http-fetcher      # or ai-summarizer
docker compose up -d           # starts KurrentDB (+ Postgres for http-fetcher)
cargo run
```
