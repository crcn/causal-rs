# causal

A deterministic, event-driven coordination layer where machines decide,
effects execute, and transactions define authority.

## v0.3 — what's new

A database-agnostic, ES-aligned, KurrentDB-compatible trait surface
alongside the existing 0.2.x API. All 0.2.x code continues to work
unchanged; v0.3 is additive.

```rust
use causal::{
    Engine, EngineBuilder, Fact, Materializer, Reactor, Ctx,
    aggregate::Aggregate, memory_store::MemoryStore,
    EventLogBackend, CheckpointStore, ReactorOutbox,
};
use std::sync::Arc;

// 1. Define a fact via #[event] with stream attrs — generates Fact impl
#[causal::event(
    prefix = "schedule",
    stream_category = "schedule",
    stream_id = "schedule_id",
)]
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ScheduleEvent {
    ScheduleCreated {
        schedule_id: uuid::Uuid,
        occurred_at: chrono::DateTime<chrono::Utc>,
        timeout: u32,
    },
}

// 2. An Aggregate (write-side consistency boundary, OCC)
#[derive(Default)]
pub struct Schedule { timeout: u32 }
impl Aggregate for Schedule {
    type Fact = ScheduleEvent;
    const CATEGORY: &'static str = "schedule";
    fn apply(&mut self, fact: &ScheduleEvent) {
        if let ScheduleEvent::ScheduleCreated { timeout, .. } = fact {
            self.timeout = *timeout;
        }
    }
}

// 3. A Materializer (idempotent at-least-once external state)
pub struct SchedulesTable;
#[async_trait::async_trait]
impl Materializer for SchedulesTable {
    type Fact = ScheduleEvent;
    async fn materialize(&self, _fact: &ScheduleEvent, _ctx: Ctx<'_>)
        -> anyhow::Result<()>
    {
        // INSERT ... ON CONFLICT DO NOTHING (idempotent on ctx.event_id)
        Ok(())
    }
}

// 4. Wire up the engine
async fn run() -> anyhow::Result<()> {
    let store = Arc::new(MemoryStore::new());
    let engine = EngineBuilder::new(
        store.clone() as Arc<dyn EventLogBackend>,
        store.clone() as Arc<dyn CheckpointStore>,
        store.clone() as Arc<dyn ReactorOutbox>,
    )
    .with_aggregate::<Schedule>()
    .with_materializer(SchedulesTable, "schedules")
    .build();

    // Command handler: load + decide + append (OCC-protected)
    let id = uuid::Uuid::new_v4();
    let (_agg, ver) = engine.load::<Schedule>(id).await?;
    engine.append::<Schedule>(id, ver, vec![
        ScheduleEvent::ScheduleCreated {
            schedule_id: id,
            occurred_at: chrono::Utc::now(),
            timeout: 60,
        },
    ]).await?;

    engine.shutdown().await?;
    Ok(())
}
```

## Trait taxonomy

| Trait | Purpose |
|---|---|
| `Fact` | Value-level event; logged. Mandatory `stream()` for Kurrent compat |
| `Aggregate` | Write-side consistency boundary, OCC-protected via `Engine::append<A>(id, expected, facts)` |
| `Materializer` | Typed idempotent at-least-once external state |
| `AnyMaterializer` | Heterogeneous-event consumer (legacy migration target) |
| `Reactor` | Pure decision producing new facts via runtime-side outbox |

## Runtime guarantees

- **C1:** `EventLogBackend::append` idempotent on `event_id`
- **C2:** Per-fact cursor advance (consumer cursor advances iff `apply` returned Ok)
- **C2b:** `DEPENDS_ON` fence — runner refuses to advance past a dep's cursor
- **C5:** Reactors forward-only (no replay without explicit `ResetAck`)
- **C6:** OCC on aggregate streams via `expected_version`
- **C7:** Logical clock only — `ctx.now()` returns `fact.occurred_at()`
- **C8:** At-least-once delivery + caller idempotency = exactly-once effect
- **C11:** Reactor outputs non-OCC; OCC writes go through command handlers
- **C12:** Reactor output durability via runtime-side outbox + atomic batch commit
- **C13:** Materializers receive facts only — no cross-state reads at materialize time

## Install

```toml
[dependencies]
causal = "0.3"
causal = { version = "0.3", features = ["macros"] }  # for #[event], #[reactor]
```

## Backward compatibility

All 0.2.x traits and APIs continue to work unchanged. Legacy and v0.3
surfaces coexist; consumers migrate at their own pace. Removal of legacy
traits planned for a future major release.

See `CHANGELOG.md` for the full list of v0.3 additions and known
limitations.

## Design notes

- [`docs/aggregate-state-scope.md`](docs/aggregate-state-scope.md) —
  why v0.3 doesn't carry the v0.2.x snapshot machinery for
  `AggregatorRegistry`, and the conditions under which it should be
  added back. **Read this before "porting" any snapshot code.**

## License

MIT
