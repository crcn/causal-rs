---
title: "feat: Strict Event trait with #[event] macro"
type: feat
date: 2026-03-09
brainstorm: docs/brainstorms/2026-03-08-kurrentdb-store-composition.md
depends_on: docs/plans/2026-03-08-refactor-store-trait-split-plan.md
---

# Strict Event Trait with `#[event]` Macro

## Overview

Replace causal's implicit event identity (`std::any::type_name`) with a strict
`Event` trait enforced by a `#[event]` proc macro. Every event gets a compile-time
guaranteed, domain-prefixed durable name. Ephemeral events are a type-level concern.
No fallbacks, no runtime string parsing, no optional fields.

Clean slate — no legacy data to protect.

## Problem Statement

Causal currently has no explicit `Event` trait. Events just need `Clone + Send + Sync + 'static`.
Identity is derived at runtime from `std::any::type_name::<E>()` → `"ScrapeEvent"` for all
variants. This causes:

1. **Ambiguous storage** — All variants share one type name. The real identity is buried in
   the JSON payload's serde tag.
2. **Hardcoded mappings** — Rootsignal's admin-app has `event_domain_prefix()` and
   `event_layer()` functions to reconstruct the real name.
3. **KurrentDB incompatibility** — KurrentDB needs unique per-event type strings.
4. **No compile-time guarantees** — Forgetting to register an event type is a runtime error.

## Proposed Solution

### The `Event` Trait

```rust
pub trait Event: Serialize + DeserializeOwned + Clone + Send + Sync + 'static {
    /// The stable, domain-prefixed name for this specific event instance.
    /// Enum: "scrape:web_scrape_completed" (depends on variant)
    /// Struct: "order_placed"
    fn durable_name(&self) -> &str;

    /// Type-level prefix for codec/aggregator registration and lookup.
    /// Enum: "scrape" (shared by all variants)
    /// Struct: "order_placed" (same as durable_name)
    fn event_prefix() -> &'static str;

    /// Whether this event is ephemeral (not persisted to EventLog).
    fn is_ephemeral() -> bool { false }
}
```

### The `#[event]` Macro

```rust
// Enum with domain prefix
#[event(prefix = "scrape")]
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ScrapeEvent {
    WebScrapeCompleted { urls_scraped: usize },
    SourcesResolved { sources: Vec<Uuid> },
}
// durable_name: "scrape:web_scrape_completed", "scrape:sources_resolved"
// event_prefix: "scrape"

// Ephemeral enum
#[event(prefix = "synthesis", ephemeral)]
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SynthesisEvent {
    SimilarityComputed,
    ResponsesMapped,
    SeverityInferred,
}
// durable_name: "synthesis:similarity_computed", etc.
// is_ephemeral: true

// Plain struct (no prefix needed)
#[event]
#[derive(Clone, Serialize, Deserialize)]
pub struct OrderPlaced { pub order_id: Uuid }
// durable_name: "order_placed"
// event_prefix: "order_placed"

// Ephemeral struct
#[event(ephemeral)]
#[derive(Clone, Serialize, Deserialize)]
pub struct EnrichmentReady { pub correlation_id: Uuid }
// durable_name: "enrichment_ready"
// is_ephemeral: true
```

### What the Macro Generates

For enums with `#[serde(tag = "type", rename_all = "snake_case")]`:

```rust
impl causal::Event for ScrapeEvent {
    fn durable_name(&self) -> &str {
        match self {
            ScrapeEvent::WebScrapeCompleted { .. } => "scrape:web_scrape_completed",
            ScrapeEvent::SourcesResolved { .. } => "scrape:sources_resolved",
        }
    }
    fn event_prefix() -> &'static str { "scrape" }
    fn is_ephemeral() -> bool { false }
}
```

For structs:

```rust
impl causal::Event for OrderPlaced {
    fn durable_name(&self) -> &str { "order_placed" }
    fn event_prefix() -> &'static str { "order_placed" }
    fn is_ephemeral() -> bool { false }
}
```

All names are static strings baked into the binary. No `std::any::type_name`, no
runtime string concatenation, no JSON payload inspection.

The macro reads `#[serde(rename_all = "...")]` and applies the same case transformation
at compile time. Supported: `snake_case`, `camelCase`, `PascalCase`, `SCREAMING_SNAKE_CASE`,
`kebab-case`. If no `rename_all`, variant names are used as-is.

### Serde Tag Parsing

The macro inspects `#[serde(tag = "...")]` on the enum to confirm it's internally tagged.
If an enum has `#[event(prefix = "x")]` but no `#[serde(tag = "...")]`, the macro emits
a compile error:

```
error: #[event] on enums requires #[serde(tag = "...")] for variant discrimination
```

For enums with `#[serde(tag = "type", content = "data")]` (adjacent tagging), the macro
works the same way — the tag value is still the variant name.

For untagged enums (`#[serde(untagged)]`), the macro emits a compile error — untagged
enums have no stable variant discriminator.

## Storage: `event_type` Becomes the Durable Name

The `event_type` field on `NewEvent` and `PersistedEvent` stores the **durable name**
directly. No more Rust type name. No separate `durable_type` field.

```rust
// Before: event_type = "ScrapeEvent" (useless for discrimination)
// After:  event_type = "scrape:web_scrape_completed" (the durable name)
```

| Where | Before | After |
|---|---|---|
| `NewEvent.event_type` | `"ScrapeEvent"` | `"scrape:web_scrape_completed"` |
| `PersistedEvent.event_type` | `"ScrapeEvent"` | `"scrape:web_scrape_completed"` |
| Postgres `events.event_type` column | `"ScrapeEvent"` | `"scrape:web_scrape_completed"` |
| KurrentDB event type | N/A | `"scrape:web_scrape_completed"` |
| Admin-app display | reassembled from hardcoded maps | read directly from column |
| `QueuedHandler.event_type` | `"ScrapeEvent"` | `"scrape:web_scrape_completed"` |

### Codec/Aggregator/Upcaster Registration

These systems register by **`event_prefix()`** (type-level, one per Rust type):

```rust
// Codec registration (happens in on::<ScrapeEvent>().then(...))
codec.event_prefix = E::event_prefix();  // "scrape"
codec.type_id = TypeId::of::<E>();
codec.decode = |payload| serde_json::from_value::<E>(payload);

// Codec lookup on load from store
fn find_codec_by_durable_name(durable_name: &str) -> Option<&EventCodec> {
    // "scrape:web_scrape_completed" → extract prefix "scrape"
    let prefix = durable_name.split(':').next().unwrap_or(durable_name);
    codecs.iter().find(|c| c.event_prefix == prefix)
}
```

For structs (no colon in durable name), the prefix IS the full name:
`"order_placed"` → prefix = `"order_placed"` → exact match.

Aggregator and upcaster lookup work the same way — register by prefix, look up by
extracting prefix from the durable name.

### Deterministic Event ID

Currently uses `event_type` (Rust short name) in the UUID v5 seed. After this change,
it uses `durable_name`:

```rust
// engine.rs build_new_events
let seed = format!("{}-{}{}-{}-{}", event_id, handler_id, id_infix, e.durable_name, idx);
let new_event_id = Uuid::new_v5(&NAMESPACE_CAUSAL, seed.as_bytes());
```

This produces different IDs than before — acceptable on a clean slate with no
production data.

## Ephemeral Events (Two-Tier Persistence)

Events with `#[event(ephemeral)]` or `#[event(prefix = "x", ephemeral)]` carry
`persistent: false`. They are persisted to the operational store (Postgres) for
causal chain durability, but are NOT forwarded to the permanent event store
(KurrentDB). The `persistent` flag is metadata that the EventLog implementation
layer uses to decide whether to replicate.

### Engine Behavior

```
All events (persistent and ephemeral) follow the same path:

  → log.append(event)          // Always persisted to operational store
  → checkpoint advances
  → next settle iteration reads it via load_from()

For persistent events (persistent=true):
  → hydrate + apply aggregators
  → auto-snapshot if configured
  → run projections
  → build handler intents

For ephemeral events (persistent=false):
  → skip aggregator hydration/apply/snapshot
  → skip projections
  → build handler intents (routing still works)
```

### Design Decisions

| Concern | Decision | Rationale |
|---|---|---|
| Operational store (Postgres) | **Always persist** | Causal chain durability for retries and replay |
| Permanent store (KurrentDB) | **Skip** | Not domain facts; `persistent` flag controls forwarding |
| Aggregate apply | **Skip** | Not domain facts; would diverge on KurrentDB replay |
| Projections | **Skip** | Not domain facts; must be replayable from permanent log |
| Handler routing | **Yes** | The whole point — coordination signals trigger handlers |
| Hop counting | **Yes** | Infinite loop protection applies equally |
| parent_id | **Set** | Full causal chain preserved in operational store |
| Crash recovery | **Durable** | Event is in Postgres; handler intents rebuild from log |
| Admin visualization | **Visible** | In Postgres events table with `persistent=false` |
| Checkpoint | **Advances** | Normal checkpoint advancement via load_from() |
| Retention | **TTL** | Postgres events table has retention; KurrentDB retains permanent events forever |

## Implementation

### Phase 1: `Event` Trait + `#[event]` Proc Macro

#### `crates/causal/src/handler/mod.rs` → new `event.rs` module

```rust
// crates/causal/src/event.rs
pub trait Event: serde::Serialize + serde::de::DeserializeOwned + Clone + Send + Sync + 'static {
    fn durable_name(&self) -> &str;
    fn event_prefix() -> &'static str;
    fn is_ephemeral() -> bool { false }
}
```

- [ ] Define `Event` trait
- [ ] Re-export from `lib.rs`
- [ ] Add to existing handler bounds: `on::<E>()` requires `E: Event`

#### `crates/causal_core_macros/src/lib.rs`

- [ ] Add `#[event]` proc macro attribute
- [ ] Parse args: `prefix = "..."` (optional for structs, required for enums), `ephemeral` (flag)
- [ ] For enums: parse `#[serde(tag = "...", rename_all = "...")]`, apply rename rule to variant names
- [ ] For enums: generate `durable_name(&self) -> &str` with match on all variants
- [ ] For structs: convert struct name to snake_case, generate `durable_name` returning static str
- [ ] Generate `event_prefix() -> &'static str`
- [ ] Generate `is_ephemeral() -> bool`
- [ ] Compile error if enum has `#[event]` but no `#[serde(tag = "...")]`
- [ ] Compile error if enum has `#[serde(untagged)]`

#### `crates/causal/src/lib.rs`

- [ ] `pub mod event;`
- [ ] Re-export: `pub use event::Event;`
- [ ] Re-export macro: `pub use causal_core_macros::event;`

#### Tests (`crates/causal_core_macros/tests/`)

- [ ] Enum with `prefix + snake_case` → correct durable_name per variant
- [ ] Enum with `prefix + PascalCase` (no rename) → correct durable_name
- [ ] Enum with `prefix + camelCase` → correct durable_name
- [ ] Struct → snake_case of struct name
- [ ] `ephemeral` flag → `is_ephemeral() == true`
- [ ] Combined `prefix + ephemeral`
- [ ] Compile-fail: enum without `#[serde(tag)]`
- [ ] Compile-fail: untagged enum

### Phase 2: Wire `Event` Trait Through Engine

#### `crates/causal/src/handler/types.rs`

- [ ] `EventOutput::new::<E: Event>(event: E)` → set `durable_name = event.durable_name().to_string()`
- [ ] `EventOutput::new::<E: Event>(event: E)` → set `persistent = !E::is_ephemeral()`
- [ ] Add `durable_name: String` and `persistent: bool` fields to `EventOutput`

#### `crates/causal/src/types.rs`

- [ ] `EmittedEvent`: add `durable_name: String`, `persistent: bool`
- [ ] `NewEvent.event_type`: now stores durable_name (the field name stays `event_type` for minimal churn)
- [ ] `NewEvent`: add `persistent: bool` (default true)
- [ ] `PersistedEvent.event_type`: now stores durable_name
- [ ] Remove `event_type_short_name` usage from engine paths (no longer needed)

#### `crates/causal/src/event_codec.rs`

- [ ] `EventCodec`: replace `event_type: String` with `event_prefix: String`
- [ ] `typed_event_codec::<E: Event>()`: use `E::event_prefix()` instead of `std::any::type_name`

#### `crates/causal/src/handler_registry.rs`

- [ ] `find_codec_by_event_type` → `find_codec_by_durable_name`: extract prefix, match by prefix
- [ ] Helper: `fn extract_prefix(durable_name: &str) -> &str` (split on `:`, take first, or full string)

#### `crates/causal/src/aggregator.rs`

- [ ] Aggregator registration: use `E::event_prefix()` instead of `type_name`
- [ ] `find_by_event_type` → `find_by_durable_name`: prefix-based lookup

#### `crates/causal/src/upcaster.rs`

- [ ] Upcaster registration: use `E::event_prefix()` instead of short name
- [ ] `upcast`: prefix-based lookup

#### `crates/causal/src/engine.rs`

- [ ] `publish_event`: use `event.durable_name()` for `NewEvent.event_type`
- [ ] `build_new_events`: use `EmittedEvent.durable_name` for `NewEvent.event_type`
- [ ] Deterministic UUID v5 seed: use `durable_name` instead of Rust type name
- [ ] Remove `event_type_short_name` calls from event building paths

#### `crates/causal/src/job_executor.rs`

- [ ] `serialize_emitted_events`: carry `durable_name` and `persistent` from EventOutput to EmittedEvent
- [ ] `decode_event`: use prefix-based codec lookup

#### `crates/causal/src/event_store.rs`

- [ ] `persist_event<E: Event>`: use `event.durable_name()` instead of `event_type_short_name(type_name::<E>())`
- [ ] Keep `event_type_short_name` for backward compat (deprecated), or remove entirely

#### `crates/causal/src/memory_store.rs`

- [x] `append()`: copy `persistent` flag from `NewEvent` to `PersistedEvent`.
  All events (including ephemeral) are persisted to the operational store.

### Phase 3: Ephemeral Event Processing

#### `crates/causal/src/engine.rs`

- [x] All events (persistent and ephemeral) go through `log.append()` — single uniform path
- [x] Settle loop checks `event.persistent` to skip aggregator hydration/apply/snapshot
- [x] Settle loop passes `skip_projections = !event.persistent` to `process_event_inner`
- [x] Removed `ephemeral_inbox` — no separate ephemeral routing
- [x] Removed Phase 1b/2b — no deferred ephemeral processing
- [x] `append_emitted_events` always appends all events (no partitioning)

#### Tests

- [x] `#[event(ephemeral)]` event → in operational store with `persistent=false`
- [x] `#[event(ephemeral)]` event → downstream handler executes
- [x] Ephemeral event does NOT apply to aggregates
- [x] Ephemeral event does NOT run projections
- [x] Mixed: persistent + ephemeral in same `events![]` return
- [x] Ephemeral handler emits persistent event → persistent event IS in log with `persistent=true`
- [x] Hop counting works for ephemeral events
- [x] Chained ephemeral → ephemeral → persistent works
- [x] Ephemeral-only root event settles correctly
- [x] Multiple sequential ephemeral root events all fire
- [x] `on_any` handler fires for ephemeral events
- [x] `emit_output` path handles ephemeral events
- [x] Retry handler works with ephemeral events

### Phase 4: Update All Existing Events

#### `crates/causal/tests/` and `examples/`

- [x] Add `#[event]` or `#[event(prefix = "...")]` to all event types in tests
- [x] Add `#[event]` to all event types in examples
- [x] Verify all tests pass

## Acceptance Criteria

### `#[event]` macro
- [ ] Enum with `#[event(prefix = "scrape")]` + `#[serde(tag = "type", rename_all = "snake_case")]`
      → `durable_name` returns `"scrape:web_scrape_completed"` per variant
- [ ] Struct with `#[event]` → `durable_name` returns `"order_placed"` (snake_case of name)
- [ ] `#[event(ephemeral)]` → `is_ephemeral() == true`
- [ ] `#[event(prefix = "x", ephemeral)]` → combines both
- [ ] Compile error on enum without `#[serde(tag)]`
- [ ] Compile error on `#[serde(untagged)]` enum

### Event identity
- [ ] `NewEvent.event_type` stores the durable name (e.g., `"scrape:web_scrape_completed"`)
- [ ] Codec lookup works by prefix extraction from durable name
- [ ] Aggregator lookup works by prefix extraction
- [ ] Upcaster lookup works by prefix extraction
- [ ] Deterministic UUID uses durable name in seed
- [ ] No `std::any::type_name` in any event identity path

### Ephemeral events (two-tier persistence)
- [x] `#[event(ephemeral)]` events persisted to operational store with `persistent=false`
- [x] `persistent` flag NOT used by EventLog trait — it's metadata for downstream forwarders
- [x] Ephemeral events route to handlers (intents created)
- [x] Ephemeral events skip aggregate apply
- [x] Ephemeral events skip projections
- [x] Hop counting works
- [x] Crash recovery: events durable in Postgres; handler intents rebuild from log
- [x] All existing tests pass (with `#[event]` added to test types)

### Backward compatibility (rootsignal migration)
- [ ] PostgresStore `events.event_type` column can store the new durable name format
- [ ] Admin-app can read durable name directly (no hardcoded mapping)
- [ ] KurrentDB event type = durable name (no transformation needed)

## Migration Notes

### Rootsignal

When upgrading causal, rootsignal needs:
1. Add `#[event(prefix = "...")]` to all domain event enums
2. Add `#[event(prefix = "...", ephemeral)]` to gate/trampoline events
3. Delete `event_domain_prefix()` and `event_layer()` hardcoded maps
4. Update admin-app GraphQL resolvers to read `event_type` directly
5. No data migration needed — old events with `"ScrapeEvent"` as event_type won't be
   in production (clean slate)

### Test events

All test events need `#[event]`:

```rust
#[event]
#[derive(Clone, Serialize, Deserialize)]
struct TestEvent { value: i32 }

#[event(prefix = "test")]
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum TestEnum {
    VariantA { x: i32 },
    VariantB,
}
```

## References

- Brainstorm: `docs/brainstorms/2026-03-08-kurrentdb-store-composition.md`
- Store trait split: PR #2 (`refactor/store-trait-split`)
- Rootsignal migration plan: `docs/plans/2026-03-09-refactor-rootsignal-postgres-store-migration-plan.md`
- KurrentDB roadmap: `docs/plans/2026-03-06-kurrentdb-integration-roadmap.md`
- Event codec: `crates/causal/src/event_codec.rs`
- Engine event building: `crates/causal/src/engine.rs:1067-1120`
- Existing macros: `crates/causal_core_macros/src/lib.rs`
- Rootsignal hardcoded mappings: `rootsignal-api/src/db/models/scout_run.rs:499-528`
