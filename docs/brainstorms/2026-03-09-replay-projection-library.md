---
date: 2026-03-09
topic: replay-projection-library
status: implemented
crate: crates/causal_replay
---

# Replay & Projection Library (`causal_replay`)

## Problem

Causal applications need to rebuild read models (Neo4j, Postgres, etc.) from the event log. Today `rootsignal-replay` does this as a bespoke CLI. With multiple projection targets coming (Neo4j + Postgres + PG NOTIFY), the shared patterns should live in causal.

## Key Insight

The application already knows how to project events — it does it during live operation. The application already has the `EventLog` connection. It can just read events directly. No separate binary. No stdio. No child processes.

Replay is a lifecycle state of the application, not an external tool.

## Architecture

```
Live mode (default):
  EventLog ──→ ProjectionStream (catch up + tail) → apply() → Neo4j, Postgres, ...

Replay mode (REPLAY=1):
  EventLog ──→ ProjectionStream (full read + promote_if) → apply() → Neo4j, Postgres, ...
```

Same process. Same code path. Same `apply()` function. The mode determines whether the stream tails after catch-up or exits after replay.

### Relationship to causal core projections

`ProjectionStream` does NOT replace causal's inline `project("id").then()` projections. They serve different purposes:

- **`project().then()`** — runs inline during event processing, sequentially before reactors. Guarantees read-your-writes within the same causal chain. Required when reactors read graph state projected by earlier events in the same dispatch cycle.
- **`ProjectionStream`** — runs independently, reads from EventLog asynchronously. For rebuild/replay of external read models. Small latency gap between event insertion and projection.

In rootsignal, reactors read graph state projected by earlier events in the same causal chain (e.g., enrichment reads signal nodes projected from events emitted by dedup). Inline projections handle this. `ProjectionStream` handles full graph rebuilds via `REPLAY=1`.

## App Code

### The entire integration

```rust
// main.rs

use causal_replay::{ProjectionStream, PgPointerStore, PgNotifyTailSource};

#[tokio::main]
async fn main() -> Result<()> {
    let db = PgPool::connect(&env::var("DATABASE_URL")?).await?;
    let log = PgEventLog::new(db.clone());
    let pointer = PgPointerStore::new(db.clone()).await?;
    let tail = PgNotifyTailSource::new(&db, "events").await?;

    let stream = ProjectionStream::new(&log, &pointer)
        .tail(Box::new(tail))
        .promote_if(|| health_check(&neo4j));

    // version() returns the right value in both modes:
    // - Live: promoted active position (e.g., 48050)
    // - Replay: snapshots latest_position() from event log, stages it
    let version = stream.version().await?;
    let neo4j_db = format!("neo4j.v{version}");
    let neo4j = GraphClient::connect(&neo4j_db).await?;
    let projections = Projections::new(neo4j, db.clone());

    stream.run(|event| projections.apply(event)).await?;
}
```

```
$ server                                  # live: stream.version() → neo4j.v48050, catch up, tail
$ REPLAY=1 server                         # replay: full read, promote, exit
$ REPLAY=1 REPLAY_TARGETS=neo4j server    # replay neo4j only
```

One binary. One code path. No branching in app code.

### The projection code (100% app-owned)

```rust
// projections.rs

pub struct Projections {
    neo4j: GraphClient,
    db: PgPool,
    targets: Option<HashSet<String>>,  // None == all
}

impl Projections {
    pub fn new(neo4j: GraphClient, db: PgPool) -> Self {
        let targets = env::var("REPLAY_TARGETS").ok().map(|t| {
            t.split(',').map(|s| s.trim().to_string()).collect()
        });
        Self { neo4j, db, targets }
    }

    fn is_active(&self, target: &str) -> bool {
        self.targets.as_ref().map_or(true, |t| t.contains(target))
    }

    /// Single entry point. Idempotent — safe to re-deliver on crash recovery.
    pub async fn apply(&self, event: &PersistedEvent) -> Result<()> {
        if self.is_active("neo4j") {
            self.apply_neo4j(event).await?;
        }
        if self.is_active("postgres") {
            self.apply_postgres(event).await?;
        }
        if self.is_active("notify") {
            self.notify(event).await?;
        }
        Ok(())
    }
}
```

## The Pointer

The pointer tracks event log position and doubles as the version for database selection. Two columns: `active` (promoted — used as DB version on live boot) and `staged` (written during replay, promoted on success).

### Schema

Auto-created on first use:

```sql
CREATE TABLE IF NOT EXISTS causal_replay_pointer (
    id INTEGER PRIMARY KEY DEFAULT 1 CHECK (id = 1),
    active BIGINT NOT NULL DEFAULT 0,
    staged BIGINT,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
```

### Traits

```rust
#[async_trait]
pub trait PointerStore: Send + Sync {
    async fn version(&self) -> Result<Option<u64>>;
    async fn save(&self, position: u64) -> Result<()>;
    async fn stage(&self, position: u64) -> Result<()>;
    async fn promote(&self) -> Result<u64>;
    async fn set(&self, position: u64) -> Result<()>;
    async fn status(&self) -> Result<PointerStatus>;
}

#[async_trait]
pub trait TailSource: Send + Sync {
    async fn wait(&self) -> Result<()>;
}
```

`PgPointerStore` and `PgNotifyTailSource` are provided behind the `postgres` feature flag. `ProjectionStream` is transport-agnostic — it only knows about `TailSource`, not sqlx or PG NOTIFY.

## How `ProjectionStream::run()` Works

Mode is determined by `REPLAY` env var (or overridden with `.mode(Mode::Replay)` for testing).

### Replay mode

1. Read all events from position 0 in batches
2. Call `apply()` for each event — fail-fast on error
3. Checkpoint to `staged` every N events (configurable)
4. After replay completes, run `promote_if` gate
5. Gate passes → promote (`staged` → `active`), exit 0
6. Gate fails → stay staged, exit 1
7. No gate → auto-promote

### Live mode

1. Load `active` position from pointer
2. Catch up: read events after position, call `apply()` — log and continue on error
3. Save pointer after each catch-up batch
4. Tail: wait on `TailSource` (PG NOTIFY, poll, etc.) with poll fallback via `tokio::select!`
5. On wake: read new events, apply, save pointer

## Promotion

### With `promote_if` (recommended for production)

```rust
ProjectionStream::new(&log, &pointer)
    .promote_if(|| health_check(&neo4j))
    .run(|event| projections.apply(event))
    .await?;
```

After replay completes:
- Health check passes → `staged` copied to `active`, exit 0
- Health check fails → stays staged, exit 1

### Without `promote_if` (auto-promote)

No gate = auto-promote after replay. Simpler for development.

### Manual promotion (escape hatch)

```rust
pointer.promote().await?;  // staged → active
pointer.set(0).await?;     // reset for full rebuild
pointer.status().await?;   // { active: 48000, staged: 50001 }
```

## The `causal_replay` Crate

Library only. No binary.

### What it provides:

- `ProjectionStream` — catch up + tail (live) or full replay + promote (replay)
- `PointerStore` trait + `PgPointerStore` (active/staged, auto-create table, behind `postgres` feature)
- `TailSource` trait + `PgNotifyTailSource` (behind `postgres` feature) + `PollTailSource`
- `Mode` enum — `Live` or `Replay`, detectable from env or overridable
- Error strategy (fail-fast in replay, log-and-continue in live)
- `promote_if` gate for safe automated promotion

### What the app provides:

- `EventLog` implementation (Postgres, KurrentDB, etc.)
- `TailSource` implementation (created by app, passed via `.tail()`)
- Projection logic — must be idempotent
- Health checks via `promote_if`
- Target filtering via `REPLAY_TARGETS` (optional)
- Database selection derived from `stream.version()` (e.g., `neo4j.v{version}`)
- Pointer management UI (CLI subcommand, admin endpoint, whatever)

## Design Decisions (Pressure-Tested)

### No separate binary

The application already has the `EventLog` connection and the projection code. It can read events directly — no stdio serialization, no child processes, no pipe buffering.

### Transport-agnostic stream

`ProjectionStream` doesn't know about PG NOTIFY or sqlx. The app creates its `TailSource` and passes it via `.tail()`. PG NOTIFY, KurrentDB subscriptions, or plain polling all go through the same trait.

### Active/staged pointer for safe promotion

Replay always writes to `staged`. Promotion copies `staged` → `active`. The `promote_if` gate runs app-defined health checks before promoting.

### Inline projections are still needed

`ProjectionStream` does NOT replace `project().then()` in causal core. Reactors that read graph state within the same causal chain require inline projections for read-your-writes guarantees. `ProjectionStream` is for async rebuild/replay only.

### Idempotent projections are a hard requirement

On crash, events between the last checkpoint and the crash are re-delivered. All projections must use `MERGE` / `ON CONFLICT DO UPDATE`.

### Error strategy by mode

- **Replay: fail-fast.** Bugs should stop the replay before promotion.
- **Live: log and continue.** One bad event shouldn't halt everything.

## Migration Path from rootsignal-replay

`rootsignal-replay` (354 lines) is replaced by:

```rust
let tail = PgNotifyTailSource::new(&db, "events").await?;

ProjectionStream::new(&log, &pointer)
    .tail(Box::new(tail))
    .promote_if(|| health_check(&neo4j))
    .run(|event| projections.apply(event))
    .await?;
```

Blue-green rebuild (replay creates `neo4j.v{final_position}` automatically):

```bash
REPLAY=1 REPLAY_TARGETS=neo4j rootsignal-server
```

## Prior Art

- `rootsignal-replay` (354 lines) — bespoke CLI for blue-green Neo4j rebuilds. Replaced by this library.
- Causal's `project("id").then()` — inline projections during live event processing (kept, not replaced)
- Causal's `EventLog::load_from(position, limit)` — sequential batch reads from global log
- EventStoreDB catch-up subscriptions — subscribe, read history, transition to live
- Marten async daemon — background projection rebuilds with automatic switch to continuous mode
- Axon tracking event processors — checkpoint-based replay with `ReplayStatus` for mode awareness
- Brandur Leach's notifier pattern — PG NOTIFY as wake-up signal, poll from table for actual data
