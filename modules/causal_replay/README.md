# causal_replay

Durable backends for the [`causal`](https://crates.io/crates/causal) runtime,
plus the cross-backend conformance suite and replay/mirroring utilities.

## Install

Add to your `Cargo.toml`:

```toml
[dependencies]
# Postgres backends (PgEventLogBackend, PgReactorCheckpoint, PgSnapshotStore,
# the inspector read-model, replay pointers, PG NOTIFY tail source):
causal_replay = { version = "0.7", features = ["postgres"] }

# KurrentDB event log:
causal_replay = { version = "0.7", features = ["kurrent"] }
```

## What lives here

- **Event logs** — `KurrentEventLogBackend` (feature `kurrent`) and
  `PgEventLogBackend` (feature `postgres`), both implementing
  `causal::EventLogBackend`.
- **Cursors & snapshots** — `PgReactorCheckpoint` (projector/reactor cursors +
  retry counters), `PgSnapshotStore` (aggregate snapshots).
- **Observability** — `PgReactorObserver`, `PgInspectorReadModel`, and
  `PgEventProjector` (mirrors the durable log into Postgres `causal_log` for the
  inspector).
- **Conformance** — `conformance`: every `EventLogBackend` impl runs the same
  scenario suite (append idempotency on `event_id`, CAS via `StreamState`,
  monotonic revisions, strict-after reads, stream isolation).
- **Replay** — `ProjectionStream` / `PointerStore`: blue/green read-model
  rebuilds driven by the `REPLAY` env var.

The Postgres schema is `docs/schema.sql` in the repository; apply it via your
migration runner — backends never auto-create the core tables.

## Recommended production shape (hybrid)

KurrentDB as the event log; Postgres for the cursor/snapshot work, which is
inherently relational:

```rust
use std::sync::Arc;
use causal::{CheckpointStore, EngineBuilder, EventLogBackend, ReactorCheckpoint};
use causal_replay::{KurrentEventLogBackend, PgReactorCheckpoint};

let kurrent = KurrentEventLogBackend::connect("kurrentdb://localhost:2113?tls=false")?;
let pg = Arc::new(PgReactorCheckpoint::new(pool));

let engine = EngineBuilder::new(
    Arc::new(kurrent) as Arc<dyn EventLogBackend>,
    pg.clone() as Arc<dyn CheckpointStore>,
    pg as Arc<dyn ReactorCheckpoint>,
).build().await?;
```

## Backend conformance

Backends drift silently without a shared suite. `conformance` pins every
property an `EventLogBackend` must satisfy — append idempotency on `event_id`,
CAS via `StreamState`, monotonic 0-indexed `StreamRevision`, strict-after
`read_stream` / `read_all` semantics, stream isolation — and every backend
(`PgEventLogBackend`, `KurrentEventLogBackend`, `MemoryStore`) runs every
scenario. Adding a new property extends the suite once and it's enforced
everywhere. See `src/conformance.rs`.

## Replay mode

Replay is a lifecycle state of the application, not an external tool. The same
`apply()` function runs in both live and replay mode — `ProjectionStream`
checks the `REPLAY` env var internally:

```sh
server                                  # live: catch up from active, then tail
REPLAY=1 server                         # replay: full read, stage, promote, exit
REPLAY=1 REPLAY_TARGETS=neo4j server    # replay neo4j only
```

**Live** (default) — catch up from the active pointer position, then tail for
new events. One bad event is logged and skipped so it doesn't halt everything.

**Replay** (`REPLAY=1`) — read all events from `LogCursor::ZERO`, stage the
final position, run the `promote_if` gate, exit. Fail-fast: a bug stops the
replay before promotion.

The pointer (`PointerStore`, e.g. `PgPointerStore`) tracks two positions:
`active` (promoted, used in live mode) and `staged` (written during replay,
promoted on success). Tail sources are pluggable via the `TailSource` trait;
the crate ships `PollTailSource` (sleep-based polling) and `PgNotifyTailSource`
(PG NOTIFY wake-up, `postgres` feature).

## Projections must be idempotent

On crash, events between the last staged checkpoint and the crash are
re-delivered. Use `MERGE` / `ON CONFLICT DO UPDATE`.

## License

MIT
