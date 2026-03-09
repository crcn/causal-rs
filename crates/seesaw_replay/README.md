# seesaw_replay

Replay and projection library for [seesaw](https://crates.io/crates/seesaw_core) event processing.

Replay is a lifecycle state of the application, not an external tool. Same `apply()` function runs in both live and replay mode.

## Install

Add to your `Cargo.toml`:

```toml
[dependencies]
seesaw_replay = "0.26"

# For PgPointerStore and PgNotifyTailSource:
seesaw_replay = { version = "0.26", features = ["postgres"] }
```

## Quick Start

```rust
use seesaw_replay::{ProjectionStream, PgPointerStore, PgNotifyTailSource};

let pointer = PgPointerStore::new(db.clone()).await?;
let tail = PgNotifyTailSource::new(&db, "events").await?;

ProjectionStream::new(&log, &pointer)
    .tail(Box::new(tail))
    .promote_if(|| health_check(&neo4j))
    .run(|event| projections.apply(event))
    .await?;
```

```sh
server                                  # live: catch up, tail
REPLAY=1 server                         # replay: full read, promote, exit
REPLAY=1 REPLAY_TARGETS=neo4j server    # replay neo4j only
```

## Modes

**Live** (default) -- catch up from the active pointer, then tail for new events.

**Replay** (`REPLAY=1`) -- read all events from position 0, stage the final position, run the `promote_if` gate, exit.

Mode is detected from the `REPLAY` env var. Override with `.mode(Mode::Replay)` for testing.

## Pointer

Tracks position with two columns: `active` (promoted) and `staged` (last replay).

```rust
pointer.version().await?;  // current active position (= DB version)
pointer.status().await?;   // { active: 48000, staged: 50001 }
pointer.promote().await?;  // staged -> active
pointer.set(0).await?;     // reset for full rebuild
```

`PgPointerStore` auto-creates a singleton `seesaw_replay_pointer` table on construction.

## Tail Source

Pluggable via the `TailSource` trait. The library provides:

- `PollTailSource` -- sleep-based polling (default fallback)
- `PgNotifyTailSource` -- PG NOTIFY wake-up signal (postgres feature)

Custom implementations (KurrentDB subscriptions, etc.) implement `TailSource::wait()`.

In live mode, `ProjectionStream` uses `tokio::select!` between the tail source and poll fallback. In replay mode, the tail source is ignored.

## Error Strategy

- **Replay: fail-fast.** Bugs stop the replay before promotion.
- **Live: log and continue.** One bad event doesn't halt everything.

## Promotion

Without `promote_if` -- auto-promote after replay.

With `promote_if` -- gate function runs after replay. Returns `Ok(true)` to promote, `Ok(false)` or `Err` to stay staged.

## Configuration

```rust
ProjectionStream::new(&log, &pointer)
    .tail(Box::new(tail))           // custom tail source
    .promote_if(|| health_check())  // promotion gate
    .batch_size(5000)               // events per load_from call
    .checkpoint_interval(500)       // stage every N events during replay
    .poll_interval(Duration::from_secs(1))  // fallback poll interval
    .mode(Mode::Replay)             // override env var detection
    .run(|event| apply(event))
    .await?;
```

## Projections Must Be Idempotent

On crash, events between the last checkpoint and the crash are re-delivered. Use `MERGE` / `ON CONFLICT DO UPDATE`.
