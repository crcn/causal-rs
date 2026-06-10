# causal

**Event-sourcing runtime for Rust, Kurrent-aligned.**

A typed `Event → Reactor → Event` loop over an append-only log. Designed to
run against [KurrentDB](https://www.kurrent.io/) (formerly EventStoreDB), with
Postgres for cursors/snapshots, and an in-memory backend for tests. The
vocabulary mirrors KurrentDB's where the concepts overlap (`EventData` /
`RecordedEvent`, `causation_id` / `correlation_id`, `StreamRevision`,
`StreamState`).

```rust
use std::sync::Arc;
use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use causal::{
    CheckpointStore, Ctx, EngineBuilder, Event, EventLogBackend, Events,
    MemoryStore, Reactor, ReactorCheckpoint,
};

// 1. Define events. CATEGORY + stream_id() route each event to its stream
//    `{CATEGORY}-{stream_id}`; event_type() names it within the stream.
#[derive(Clone, Serialize, Deserialize)]
struct OrderPlaced { order_id: Uuid, occurred_at: DateTime<Utc> }

impl Event for OrderPlaced {
    const CATEGORY: &'static str = "order";
    fn event_type(&self) -> &str { "placed" }
    fn stream_id(&self) -> Uuid { self.order_id }
    fn occurred_at(&self) -> Option<DateTime<Utc>> { Some(self.occurred_at) }
}

#[derive(Clone, Serialize, Deserialize)]
struct ShipmentQueued { order_id: Uuid, occurred_at: DateTime<Utc> }

impl Event for ShipmentQueued {
    const CATEGORY: &'static str = "shipment";
    fn event_type(&self) -> &str { "queued" }
    fn stream_id(&self) -> Uuid { self.order_id }
    fn occurred_at(&self) -> Option<DateTime<Utc>> { Some(self.occurred_at) }
}

// 2. A Reactor: a pure decision triggered by one event type, emitting new
//    events. At-least-once + idempotent — outputs append directly to the log
//    with deterministic ids, so redelivery dedups.
struct ShipOnPlaced;

#[async_trait]
impl Reactor for ShipOnPlaced {
    type Trigger = OrderPlaced;
    const GROUP_NAME: &'static str = "ship_on_placed";

    async fn react(&self, trigger: &OrderPlaced, _ctx: Ctx<'_>) -> Result<Events> {
        Ok(Events::new().add(ShipmentQueued {
            order_id:    trigger.order_id,
            occurred_at: Utc::now(),
        }))
    }
}

// 3. Wire the engine. The three backend trait objects usually point at one
//    store (here, one MemoryStore cast three ways).
async fn run() -> Result<()> {
    let store = Arc::new(MemoryStore::new());
    let engine = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        )
        .with_reactor(ShipOnPlaced)
        .build()
        .await?;

    // emit() persists the event and drives the reactor chain;
    // .settled() waits for the chain to quiesce.
    engine.emit(OrderPlaced { order_id: Uuid::new_v4(), occurred_at: Utc::now() })
        .settled()
        .await?;

    engine.shutdown().await?;
    Ok(())
}
```

## Trait taxonomy

| Trait | Purpose |
|---|---|
| `Event` | A logged fact. `CATEGORY` + `stream_id()` define its stream; `event_type()` names it. |
| `Aggregate` + `Apply<F>` | Write-side consistency boundary. Folded from its stream; mutated only via the OCC command path. |
| `Reactor` | Pure decision triggered by one event type, emitting new events. At-least-once, idempotent. |
| `Projector` / `MultiProjector` | Idempotent at-least-once read-model / external-state consumer. |

## Two write paths

- **`engine.emit(events)`** — append-only facts to their own streams, no
  concurrency check (idempotency rests on `event_id`). Rejects categories
  registered as OCC-required.
- **`engine.append::<A, F>(id, |agg| decide)`** — the OCC command path
  (load-decide-append). Folds aggregate `A` from `{F::CATEGORY}-{id}`, runs
  the pure `decide` closure, and appends the whole decision as **one atomic
  batch** at the expected revision; on a concurrent write it reloads,
  re-decides, and retries.

## Runtime guarantees

- **C1** — `append_to_stream` is idempotent on `event_id`.
- **C2** — per-consumer cursor advances iff the consumer returned `Ok`.
- **C2b** — `DEPENDS_ON` fence: a consumer won't advance past a dependency's cursor.
- **C5** — reactors are forward-only (catch-up consumers; no replay by default).
- **C6** — OCC on aggregate streams via expected `StreamRevision`.
- **C8** — at-least-once delivery + caller idempotency ⇒ exactly-once effect.
- **C11** — reactor outputs use the non-OCC append path; OCC writes go through `append`.
- **C12** — reactor output durability: outputs append directly to the log with
  deterministic `event_id`s, so a redelivered reaction dedups on append.

Reactors and projectors are **catch-up subscriptions**: each reads the log from
a client-managed cursor (`CheckpointStore`) and advances it as it goes — there
is no outbox and no server-side persistent subscription.

## Backends

- In-memory `MemoryStore` (this crate) — tests, examples, single-process.
- `causal_replay::{PgEventLogBackend, PgReactorCheckpoint, PgSnapshotStore,
  KurrentEventLogBackend}` — durable backends behind feature flags, plus the
  cross-backend conformance suite.

## Install

```toml
[dependencies]
causal = "0.7"
# The `macros` feature is on by default and re-exports `#[event]` +
# `#[aggregator]` + `#[aggregators]`. Disable it with default-features = false:
causal = { version = "0.7", default-features = false }
```

## Design notes

- [`docs/aggregate-state-scope.md`](docs/aggregate-state-scope.md) — when the
  `AggregatorRegistry` should (and shouldn't) carry snapshot machinery.

See [`CHANGELOG.md`](../../CHANGELOG.md) for the version history.

## License

MIT
