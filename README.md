# causal-rs

**Event-sourcing runtime for Rust, Kurrent-aligned.**

`causal-rs` is an event-driven runtime with a typed
`Event → Reactor → Event` loop, designed to run against
[KurrentDB](https://www.kurrent.io/) (formerly EventStoreDB) as the
durable event log, with Postgres as the reactor/projection cursor
store. It also runs entirely in-memory for tests.

The library's vocabulary mirrors KurrentDB's exactly where the
concepts overlap (`EventData` / `RecordedEvent`, `causation_id` /
`correlation_id`, `StreamRevision`, `StreamState`,
`$correlationId` / `$causationId` metadata). A KurrentDB developer
should be able to read the API and recognize every term.

```rust
use causal::{Engine, EngineBuilder, EventLogBackend, CheckpointStore, ReactorCheckpoint};
use causal::types::StreamState;
use causal::MemoryStore;
use std::sync::Arc;

let store = Arc::new(MemoryStore::new());
let engine = EngineBuilder::new(
        store.clone() as Arc<dyn EventLogBackend>,
        store.clone() as Arc<dyn CheckpointStore>,
        store.clone() as Arc<dyn ReactorCheckpoint>,
    )
    .with_aggregators(order_aggregators())   // Vec<Aggregator>
    .with_reactor(ShipOnPlaced)               // impl Reactor
    .build();

// Emit an event. Engine derives stream name from `Event::CATEGORY +
// Event::stream_id()`, stamps causation/correlation, persists, and
// drives downstream reactors to quiescence on `.settled()`.
engine.emit(OrderPlaced { order_id, total: 99.99 })
    .causation_id(trigger_event_id)
    .settled()
    .await?;
```

## Status

**Pre-1.0; breaking changes expected.** The 2026-05-14 release
finished a KurrentDB-vocabulary alignment pass (`parent_id` →
`causation_id`, `NewEvent` → `EventData`, etc.). See
[`CHANGELOG.md`](CHANGELOG.md) `[Unreleased]` for the migration
matrix and [`docs/MIGRATION_0.4.md`](docs/MIGRATION_0.4.md) for the
step-by-step guide.

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
- **`causal_core_macros`** — `#[fact]`, `#[aggregator]`,
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
).build();
```

## KurrentDB vocabulary mapping

| Kurrent term | causal-rs |
|---|---|
| Event (write) | `EventData` |
| Event (read) | `RecordedEvent` |
| `event_type` (stored field) | composed `{Event::CATEGORY}:{event.event_type()}` |
| Category | `Event::CATEGORY` |
| Stream id | `Event::stream_id() -> Uuid` |
| Stream name | `{CATEGORY}-{stream_id}` (composed automatically; `causal::stream_name_for::<F>(id)` exposes it) |
| Stream revision | `StreamRevision` (0-indexed) |
| `$all` commit position | `LogCursor` |
| `StreamState` for OCC | `causal::types::StreamState` (same variants) |
| ExpectedRevision | `StreamState::StreamRevision(u64)` |
| Persistent subscription | `Reactor` (extends with atomic emit on top) |
| Group name | `Reactor::GROUP_NAME` / `Projector::GROUP_NAME` |
| `correlation_id` | `correlation_id` |
| `causation_id` | `causation_id` |
| `$correlationId` metadata | stamped automatically — feeds the `$by_correlation_id` projection (enable + configure `correlationIdProperty`) |
| `$causationId` metadata | stamped automatically — the `$by_correlation_id` projection uses it to build the causation tree (there is no `$by_causation_id`) |

Deliberate divergence: `Reactor` vs Kurrent's `PersistentSubscription`
— Reactor adds atomic emit on top of the subscription contract. The
rest of the vocabulary is aligned 1:1.

## Backend conformance

Every `EventLogBackend` impl runs the same suite — append
idempotency on `event_id`, CAS via `StreamState`, monotonic
revisions, strict-after `read_stream` / `read_all` semantics,
stream isolation. Adding a new backend or trait method extends the
suite once and the assertion runs against every impl. See
[`modules/causal_replay/src/conformance.rs`](modules/causal_replay/src/conformance.rs).

## Schema

[`docs/schema.sql`](docs/schema.sql) — authoritative Postgres schema
(causal_log, causal_checkpoints, causal_snapshots,
causal_projection_cursors, causal_projection_failures). There is no
outbox table — reactors append their outputs directly to the log. The
Kurrent-alignment column renames are in
[`migrations/20260514_kurrent_alignment.sql`](migrations/20260514_kurrent_alignment.sql).

## License

MIT.
