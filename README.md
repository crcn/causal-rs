# causal-rs

**Event-sourcing runtime for Rust, Kurrent-aligned.**

`causal-rs` is an event-driven runtime with a typed
`Event → Reactor → Event` loop, designed to run against
[KurrentDB](https://www.kurrent.io/) (formerly EventStoreDB) as the
durable event log, with Postgres as the reactor/projection cursor
store. It also runs entirely in-memory for tests.

## The model in one minute

Everything that happens is an **event** appended to a log. From there:

- **Aggregates** fold events into read-model state — `state = fold(events)`.
  No mutable rows; the log *is* the source of truth.
- **Reactors** are pure decisions: an event comes in, zero or more events
  go out. The runtime appends those outputs back to the log (with
  deterministic ids, so a crash-retry never double-emits), which can in
  turn trigger more reactors. That's the `Event → Reactor → Event` loop.
- **Projectors** build external read models (a Postgres table, a search
  index) from the same stream.
- You `emit()` an event and `await` `.settled()` — which resolves only
  once the whole causal chain that event kicked off has drained.

That's the whole idea: typed events, deterministic folds, and a
self-driving reaction loop with at-least-once + idempotent delivery.

## Install

```toml
[dependencies]
causal = "0.8"

# Durable backends (KurrentDB event log + Postgres cursors/snapshots):
causal_replay = { version = "0.8", features = ["postgres", "kurrent"] }
```

The core `causal` crate runs entirely in-memory — you only need
`causal_replay` for production backends.

## Walkthrough

A complete program: an order is placed, a reactor requests its shipment,
and an aggregate folds both into queryable state. This is a runnable
example —
[`modules/causal/examples/order_walkthrough.rs`](modules/causal/examples/order_walkthrough.rs)
(`cargo run -p causal --example order_walkthrough`).

```rust
use causal::{
    Aggregate, Aggregator, Apply, CheckpointStore, Ctx, EngineBuilder, Event,
    EventLogBackend, Events, MemoryStore, Reactor, ReactorCheckpoint,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;

// 1. Events are plain data. `NAME` is the wire event-type; `SUBJECT` is the
//    subject history a fact joins (stream `{SUBJECT}-{subject_id}`, defaults
//    to `NAME`); `subject_id` picks which subject this value is about.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct OrderPlaced { order_id: Uuid, total: f64 }
impl Event for OrderPlaced {
    const NAME: &'static str = "placed";
    fn subject_id(&self) -> Uuid { self.order_id }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ShipmentRequested { order_id: Uuid }
impl Event for ShipmentRequested {
    const NAME: &'static str = "requested";
    fn subject_id(&self) -> Uuid { self.order_id }
}

// 2. An aggregate is read-model state folded from events. `Apply<E>`
//    is the per-event fold; impl it once per event type the aggregate
//    cares about.
#[derive(Default, Clone, Serialize, Deserialize)]
struct Order { total: f64, shipped: bool }
impl Aggregate for Order { const NAME: &'static str = "Order"; }
impl Apply<OrderPlaced> for Order {
    fn apply(&mut self, e: &OrderPlaced) { self.total = e.total; }
}
impl Apply<ShipmentRequested> for Order {
    fn apply(&mut self, _: &ShipmentRequested) { self.shipped = true; }
}

// 3. A reactor turns one event into others — a *pure* decision. The
//    runtime appends the returned events to the log for you.
struct ShipOnPlaced;
#[async_trait::async_trait]
impl Reactor for ShipOnPlaced {
    type Trigger = OrderPlaced;
    const NAME: &'static str = "ship_on_placed";
    async fn react(&self, t: &OrderPlaced, _ctx: Ctx<'_>) -> anyhow::Result<Events> {
        Ok(causal::events![ShipmentRequested { order_id: t.order_id }])
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 4. Wire the engine. `MemoryStore` plays every backend role for
    //    tests; swap in KurrentDB + Postgres for production (below).
    let store = Arc::new(MemoryStore::new());
    let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        )
        .with_aggregators([
            Aggregator::for_type::<Order, OrderPlaced>(),
            Aggregator::for_type::<Order, ShipmentRequested>(),
        ])
        .with_reactor(ShipOnPlaced)
        // MemoryStore effect memoization is lost on restart, so reactors
        // require an explicit choice: tests opt in here; production wires a
        // durable `.with_effect_store(...)` instead.
        .allow_in_memory_effect_store_for_tests()
        .build()             // async + fallible: seeds reactor cursors
        .await?;

    // 5. Emit one event. `.settled()` resolves only after the whole
    //    chain drains: here, after ShipOnPlaced reacted and its
    //    ShipmentRequested output landed in the log.
    let order_id = Uuid::new_v4();
    engine.emit(OrderPlaced { order_id, total: 99.99 })
        .settled()
        .await?;

    // 6. Read the folded state back.
    let order = engine.state_of::<Order>(order_id).await?.expect("order exists");
    assert_eq!(order.total, 99.99);
    assert!(order.shipped, "set by the reactor's downstream event");
    Ok(())
}
```

Inside a reactor or projector body you read aggregate state with
`ctx.state_of::<Order>(id).await?.curr` — in a reactor this is a
position-bounded fold of the subject's history at your trigger, so the
answer is deterministic under any concurrency. The `#[event]`,
`#[aggregators]`, `#[reactors]`, and `#[projectors]` macros (feature
`macros`, on by default) generate the boilerplate above from struct +
fn declarations; the walkthrough hand-rolls it to show the full
surface.

## Runnable examples

Each lives under [`examples/`](examples/) and runs via
`./dev.sh example run <name>` (which brings up its docker stack first):

- **[`http-fetcher`](examples/http-fetcher)** — fans out HTTP fetches as
  `FetchRequested` events; a reactor calls `reqwest` and emits
  `Fetched` / `FetchFailed`. Production-shape backend (KurrentDB +
  `PgReactorCheckpoint`).
- **[`ai-summarizer`](examples/ai-summarizer)** — a reactor calls the
  Anthropic API and emits `Summarized` / `SummaryFailed`. KurrentDB log
  + in-memory cursors.
- **[`inspector-demo`](examples/inspector-demo)** — a content-ingestion
  pipeline wired to the **causal inspector** (GraphQL API + React UI on
  `:4000`) that visualizes the event flow live, on KurrentDB + Postgres.

## Status

**Pre-1.0; breaking changes expected.** Latest release: **0.8.0**, an
audit-remediation pass (data-integrity fixes, a uniform append-idempotency
guarantee across backends, and a leaner public API). See
[`CHANGELOG.md`](CHANGELOG.md) and the upgrade guide in
[`docs/MIGRATION_0.8.md`](docs/MIGRATION_0.8.md); the earlier
KurrentDB-vocabulary rename matrix is under `[0.5.0]` /
[`docs/MIGRATION_0.4.md`](docs/MIGRATION_0.4.md).

The library is being prepared for production deployment in
[rootsignal](https://rootsignal.com) on KurrentDB.

## Development

[`./dev.sh`](dev.sh) is the entry point for everything — tests, examples, and
the local infrastructure they need. Run it with no arguments for an interactive
menu, or use it directly:

```bash
./dev.sh                          # interactive menu
./dev.sh doctor                   # check your toolchain (cargo, docker, …)

./dev.sh test unit                # in-memory suite (no docker)
./dev.sh test live                # live Postgres + Kurrent + hybrid suites
./dev.sh test all                 # everything

./dev.sh example list             # list runnable examples
./dev.sh example run http-fetcher # spin up its docker stack (health-waited) + run it
./dev.sh example down http-fetcher

./dev.sh stack up | down          # the live-test infra (Postgres :5433, Kurrent :2114)
./dev.sh lint | fmt | check       # cargo clippy / fmt / check
```

`dev.sh` is a thin bootstrap: it ensures a Rust toolchain, then builds and runs
the dev CLI in [`dev/cli`](dev/cli) (release mode, rebuilt only when its sources
change). The CLI discovers examples under `examples/`, brings up each one's
`docker-compose.yml` (waiting for healthchecks) before running it, and manages a
separate live-test stack (`dev/docker-compose.yml`) for the `--ignored` backend
suites — applying migrations automatically.

## Crates

- **`causal`** — core engine, `Event` trait, `Reactor` / `Projector`
  traits, `EngineBuilder`, in-memory `MemoryStore` backend.
- **`causal_replay`** — durable backend implementations:
  `PgEventLogBackend`, `PgReactorCheckpoint`, `PgSnapshotStore` and
  `KurrentEventLogBackend` (behind feature flags), plus the
  cross-backend conformance suite.
- **`causal_core_macros`** — `#[event]`, `#[aggregator]`,
  `#[aggregators]` proc macros.
- **`causal_inspector`** — read-model API for an inspector UI.
- **`causal_utils`** — internal helpers.

## Recommended production shape

The roadmap calls for a **hybrid backend** (Option B):

- **KurrentDB** as the event log
  ([`KurrentEventLogBackend`](modules/causal_replay/src/kurrent_event_log.rs)).
- **Postgres** as the reactor/projection cursors + snapshots
  ([`PgReactorCheckpoint`](modules/causal_replay/src/reactor_checkpoint.rs),
  [`PgSnapshotStore`](modules/causal_replay/src/snapshot_store.rs)).

Kurrent is the event store it excels at being; the cursor / snapshot
work is inherently relational and stays on Postgres.

```rust
let kurrent = KurrentEventLogBackend::connect("kurrentdb://localhost:2113?tls=false")?;
let pg = Arc::new(PgReactorCheckpoint::new(pool));

let engine = EngineBuilder::new(
    Arc::new(kurrent) as Arc<dyn EventLogBackend>,
    pg.clone()       as Arc<dyn CheckpointStore>,
    pg.clone()       as Arc<dyn ReactorCheckpoint>,
).build().await?;
```

## KurrentDB vocabulary mapping

| Kurrent term | causal-rs |
|---|---|
| Event (write) | `EventData` |
| Event (read) | `RecordedEvent` |
| Category | `Event::SUBJECT` |
| Stream id | `Event::subject_id() -> Uuid` |
| Stream name | `{SUBJECT}-{subject_id}` (composed automatically; `causal::stream_name_for::<F>(id)` exposes it) |
| Stream revision | `StreamRevision` (0-indexed) |
| `$all` commit position | `LogCursor` |
| `StreamState` for OCC | `causal::types::StreamState` (same variants) |
| ExpectedRevision | `StreamState::StreamRevision(u64)` |
| Persistent subscription | `Reactor` (extends with atomic emit on top) |
| Group name | `Reactor::NAME` / `Projector::NAME` (one consumer per name — no competing instances) |
| `correlation_id` | `correlation_id` |
| `causation_id` | `causation_id` |
| `$correlationId` metadata | stamped automatically — feeds the `$by_correlation_id` projection (enable + configure `correlationIdProperty`) |
| `$causationId` metadata | stamped automatically — the `$by_correlation_id` projection uses it to build the causation tree (there is no `$by_causation_id`) |

**Deliberate divergence** — two places causal-rs departs from Kurrent on
purpose:

- `Reactor` vs Kurrent's `PersistentSubscription` — Reactor adds atomic
  emit on top of the subscription contract.
- **Identity is split across two declarations.** `Event::NAME` is the wire
  `event_type` (written verbatim, matched by consumers by equality), while
  `Event::SUBJECT` is the stream category (`{SUBJECT}-{subject_id}`). Kurrent
  has only the stream name; causal-rs keeps `NAME` and `SUBJECT` separate so
  several fact families can co-locate in one subject history (shared
  `SUBJECT`) while still routing on distinct `NAME`s.

The rest of the vocabulary is aligned 1:1.

Trait-level append idempotency keys on `event_id` and is now uniform
across every backend — Postgres via `UNIQUE(event_id)`, `MemoryStore`
in-process, and Kurrent via an explicit scan-then-CAS. Raw Kurrent `Any`
best-effort dedup is no longer relied upon.

## Backend conformance

Every `EventLogBackend` impl runs the same suite — append
idempotency on `event_id`, CAS via `StreamState`, monotonic
revisions, strict-after `read_stream` / `read_all` semantics,
stream isolation. Adding a new backend or trait method extends the
suite once and the assertion runs against every impl. See
[`modules/causal_replay/src/conformance.rs`](modules/causal_replay/src/conformance.rs).

## Schema

[`docs/schema.sql`](docs/schema.sql) — authoritative Postgres schema.
The five core tables are `causal_log`, `causal_checkpoints`,
`causal_snapshots`, `causal_projection_cursors`,
`causal_projection_failures`. The schema also ships the 0.7.0
observability tables read by the inspector —
`causal_reactor_executions`, `causal_reactor_logs`,
`causal_reactor_descriptions`, and `causal_aggregate_snapshots`
(best-effort, written off the hot path by `PgReactorObserver`). There is
no outbox table — reactors append their outputs directly to the log. The
Kurrent-alignment column renames are in
[`migrations/20260514_kurrent_alignment.sql`](migrations/20260514_kurrent_alignment.sql).

## License

MIT.
