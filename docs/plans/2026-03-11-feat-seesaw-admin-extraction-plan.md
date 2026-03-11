# Plan: Extract Seesaw Admin UI into Reusable Modules

**Date:** 2026-03-11
**Status:** Draft
**Context:** The rootsignal admin app has a mature event browser (timeline, causal tree, causal flow DAG, handler logs) built on top of seesaw's event tables. This functionality is generic — it queries seesaw's own schema, not rootsignal-specific tables. Extracting it into reusable modules lets any seesaw-based project get the same observability out of the box.

## Directory Structure

Adopt a unified `modules/` directory for all seesaw modules (Rust crates + JS packages). Migrate existing `crates/` into `modules/`.

```
seesaw-rs/
  modules/
    # ── Existing (moved from crates/) ──
    seesaw/                    # core runtime
    seesaw_core_macros/        # proc macros
    seesaw_replay/             # replay library
    seesaw_utils/              # utilities

    # ── New: Rust crate ──
    seesaw_admin/              # backend for admin UI
      Cargo.toml
      src/
        lib.rs
        types.rs               # AdminEvent, AdminEventsPage, AdminCausalTree, etc.
        display.rs             # EventDisplay trait + DefaultEventDisplay
        queries.rs             # Postgres queries against seesaw tables
        cache.rs               # EventCache (bounded in-memory cache)
        broadcast.rs           # EventBroadcast (pg_notify → broadcast channel)
        graphql.rs             # async-graphql resolvers (behind feature flag)

    # ── New: React package ──
    seesaw-admin-ui/           # frontend components (npm package)
      package.json
      tsconfig.json
      src/
        index.ts               # public API
        types.ts               # AdminEvent, FlowSelection, LogsFilter
        queries.ts             # GraphQL queries + subscription
        context.tsx            # SeesawAdminProvider (generic EventsPaneContext)
        theme.ts               # Default color maps, configurable
        panes/
          TimelinePane.tsx     # Event stream with filters, infinite scroll, live sub
          CausalTreePane.tsx   # Recursive parent→child tree view
          CausalFlowPane.tsx   # ReactFlow DAG with describe blocks
          LogsPane.tsx         # Structured handler logs
        components/
          EventNode.tsx        # ReactFlow event-type node
          HandlerNode.tsx      # ReactFlow handler node with Block rendering
          BlockRenderer.tsx    # Describe block renderers (checklist, progress, etc.)
          JsonSyntax.tsx       # Colorized JSON renderer
          CopyablePayload.tsx  # Expandable payload with copy/fullscreen
          FilterBar.tsx        # Layer/search/date/run filters
        hooks/
          useEventStream.ts    # Live WebSocket subscription hook

  examples/                    # existing examples
```

## Migration: crates/ → modules/

1. `mv crates/ modules/`
2. Update `Cargo.toml` workspace members: `crates/*` → `modules/*`
3. Update all internal `path = "../seesaw"` references in sub-crate Cargo.tomls
4. Verify `cargo build` and `cargo test` pass

## What Moves from Rootsignal → seesaw_admin (Rust)

### Generic (extract)

| Rootsignal location | Destination | Notes |
|---|---|---|
| `db/models/scout_run.rs` → `list_events_paginated()` | `queries.rs` | Queries `events` table (seesaw schema) |
| `db/models/scout_run.rs` → `causal_tree()` | `queries.rs` | Queries by `correlation_id` |
| `db/models/scout_run.rs` → `causal_flow()` | `queries.rs` | Queries by `run_id` |
| `db/models/scout_run.rs` → `get_event_by_seq()` | `queries.rs` | Single event lookup |
| `db/models/scout_run.rs` → `handler_logs()` | `queries.rs` | Queries `seesaw_handler_logs` |
| `db/models/scout_run.rs` → `handler_logs_by_run()` | `queries.rs` | Queries `seesaw_handler_logs` |
| `db/models/scout_run.rs` → `handler_outcomes()` | `queries.rs` | Queries `seesaw_effect_executions` |
| `db/models/scout_run.rs` → `handler_descriptions()` | `queries.rs` | Queries `seesaw_handler_descriptions` |
| `event_cache.rs` → `EventCache` | `cache.rs` | Bounded in-memory cache with indexes |
| `event_broadcast.rs` → `EventBroadcast` | `broadcast.rs` | pg_notify listener → broadcast |
| `graphql/schema.rs` → `admin_events()` etc. | `graphql.rs` | async-graphql resolvers |
| `graphql/schema.rs` → `AdminEvent`, `AdminEventsPage`, etc. | `types.rs` | GraphQL output types |

### Stays in rootsignal (domain-specific)

| Item | Reason |
|---|---|
| `event_summary()` | Maps rootsignal event types to human summaries |
| `event_domain_prefix()` | Maps rootsignal event types to domain prefixes |
| `event_layer()` | Categorizes rootsignal events into world/system/telemetry |
| `AdminGuard` | Rootsignal's auth guard |
| `AdminScoutRun` query + type | Scout-specific run stats |
| `InvestigatePane` | AI investigation (rootsignal-specific) |

## Customization Point: EventDisplay Trait

The one thing that varies per project is how raw events get displayed. This becomes a trait:

```rust
// seesaw_admin/src/display.rs

/// Customizes how events are displayed in the admin UI.
/// Implement this trait to provide domain-specific naming and summaries.
pub trait EventDisplay: Send + Sync {
    /// Human-readable name for an event (e.g. "discovery:source_discovered").
    fn display_name(&self, event_type: &str, payload: &serde_json::Value) -> String;

    /// Optional one-line summary for the timeline view.
    fn summary(&self, event_type: &str, payload: &serde_json::Value) -> Option<String>;

    /// Layer classification for filtering (e.g. "world", "system", "telemetry").
    fn layer(&self, event_type: &str) -> &str;
}

/// Default implementation — uses the payload's "type" field as the name.
pub struct DefaultEventDisplay;

impl EventDisplay for DefaultEventDisplay {
    fn display_name(&self, event_type: &str, payload: &serde_json::Value) -> String {
        payload.get("type")
            .and_then(|v| v.as_str())
            .unwrap_or(event_type)
            .to_string()
    }

    fn summary(&self, _: &str, _: &serde_json::Value) -> Option<String> {
        None
    }

    fn layer(&self, _: &str) -> &str {
        "system"
    }
}
```

Used in the GraphQL resolvers and cache hydration:

```rust
// seesaw_admin/src/graphql.rs

pub struct SeesawAdminQuery<D: EventDisplay> {
    display: Arc<D>,
}

impl<D: EventDisplay + 'static> SeesawAdminQuery<D> {
    pub fn new(display: D) -> Self {
        Self { display: Arc::new(display) }
    }
}
```

Rootsignal provides its implementation:

```rust
// rootsignal-api/src/event_display.rs

pub struct RootSignalEventDisplay;

impl EventDisplay for RootSignalEventDisplay {
    fn display_name(&self, event_type: &str, payload: &Value) -> String {
        let prefix = event_domain_prefix(event_type);
        let variant = json_str(payload, "type").unwrap_or_else(|| event_type.to_string());
        format!("{prefix}:{variant}")
    }

    fn summary(&self, event_type: &str, payload: &Value) -> Option<String> {
        event_summary(&json_str(payload, "type").unwrap_or_default(), payload)
    }

    fn layer(&self, event_type: &str) -> &str {
        event_layer(event_type)
    }
}
```

## Feature Flags (Rust Crate)

```toml
# modules/seesaw_admin/Cargo.toml

[features]
default = []
graphql = ["async-graphql"]    # async-graphql resolvers
cache = []                      # in-memory EventCache
broadcast = ["sqlx/postgres"]   # pg_notify → broadcast channel

[dependencies]
seesaw_core = { path = "../seesaw" }
sqlx = { version = "0.8", features = ["postgres", "runtime-tokio", "chrono", "uuid"], optional = true }
async-graphql = { version = "7", optional = true }
chrono = { version = "0.4", features = ["serde"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tokio = { version = "1", features = ["sync"] }
uuid = { version = "1", features = ["v4", "serde"] }
tracing = "0.1"
anyhow = "1"
```

## What Moves from Rootsignal → seesaw-admin-ui (React)

### Generic (extract)

| Rootsignal location | Destination | Notes |
|---|---|---|
| `EventsPaneContext.tsx` | `context.tsx` | Remove `InvestigateMode` reference; expose as generic hook |
| `TimelinePane.tsx` | `panes/TimelinePane.tsx` | Remove `onInvestigate` (make optional callback prop) |
| `CausalTreePane.tsx` | `panes/CausalTreePane.tsx` | Remove `InvestigateMode` dependency |
| `CausalFlowPane.tsx` | `panes/CausalFlowPane.tsx` | Remove `ADMIN_SCOUT_RUN` query; add `headerExtra` render prop |
| `LogsPane.tsx` | `panes/LogsPane.tsx` | Remove `setInvestigation` (make optional callback prop) |
| `eventColor.ts` | `theme.ts` | Export as configurable color map |
| GraphQL queries | `queries.ts` | The 7 seesaw admin queries + subscription |

### Stays in rootsignal

| Item | Reason |
|---|---|
| `InvestigatePane.tsx` | AI investigation, rootsignal-specific |
| `EventsPage.tsx` | Layout composition — rootsignal wires in its custom panes |
| `PaneManager.tsx` | Generic but not seesaw-specific; could be a separate package later |
| `ADMIN_SCOUT_RUN` query | Scout-specific |
| `ScoutRunDetailPage.tsx` | Scout-specific |

## Frontend Package API

```tsx
// @seesaw/admin-ui

// Provider — wraps your app, provides the events context
export { SeesawAdminProvider } from './context';
export type { SeesawAdminConfig } from './context';

// Panes — drop into any layout system
export { TimelinePane } from './panes/TimelinePane';
export { CausalTreePane } from './panes/CausalTreePane';
export { CausalFlowPane } from './panes/CausalFlowPane';
export { LogsPane } from './panes/LogsPane';

// Hooks — for building custom UI
export { useSeesawAdmin } from './context';  // the context hook

// Components — for custom compositions
export { EventNode, HandlerNode, BlockRenderer } from './components';
export { JsonSyntax, CopyablePayload } from './components';

// Types
export type { AdminEvent, FlowSelection, LogsFilter } from './types';
```

Consumer usage (rootsignal):

```tsx
import {
  SeesawAdminProvider,
  TimelinePane,
  CausalFlowPane,
  CausalTreePane,
  LogsPane,
} from '@seesaw/admin-ui';
import { InvestigatePane } from './panes/InvestigatePane';
import { ScoutRunStats } from './components/ScoutRunStats';

function EventsPage() {
  return (
    <SeesawAdminProvider endpoint="/graphql" wsEndpoint="ws://localhost:4000/ws">
      <PaneManager panes={[
        { name: "Timeline", render: () => (
          <TimelinePane onInvestigate={(event) => openInvestigation(event)} />
        )},
        { name: "Flow", render: () => (
          <CausalFlowPane headerExtra={<ScoutRunStats />} />
        )},
        { name: "Tree", render: () => <CausalTreePane /> },
        { name: "Logs", render: () => <LogsPane /> },
        { name: "Investigate", render: () => <InvestigatePane /> },
      ]} />
    </SeesawAdminProvider>
  );
}
```

## Implementation Phases

### Phase 1: Directory migration
- Move `crates/` → `modules/`
- Update workspace Cargo.toml
- Verify builds pass

### Phase 2: seesaw_admin Rust crate
- Create `modules/seesaw_admin/`
- Extract types, queries, cache, broadcast from rootsignal
- Implement `EventDisplay` trait
- Add feature-flagged async-graphql resolvers
- Rootsignal imports `seesaw_admin` and provides `RootSignalEventDisplay`

### Phase 3: seesaw-admin-ui React package
- Create `modules/seesaw-admin-ui/`
- Extract pane components, context, queries, theme
- Make investigation/custom panes pluggable via callbacks/render props
- Rootsignal imports `@seesaw/admin-ui` and composes with its custom panes

### Phase 4: Integration testing
- Rootsignal admin app works identically after extraction
- New example project uses `seesaw_admin` + `@seesaw/admin-ui` with defaults

## Open Questions

1. **Monorepo publishing**: Should `seesaw-admin-ui` be published to npm, or consumed via workspace path? If npm, need a build step (tsup/vite library mode).

2. **PaneManager**: The flexlayout-react wrapper (`PaneManager.tsx`) is generic but not seesaw-specific. Extract as a separate package, or bundle with `seesaw-admin-ui`?

3. **Auth**: The admin queries use `AdminGuard` in rootsignal. The generic crate should expose a guard hook point — probably just document that consumers should add their own auth middleware.

4. **Event table name**: The queries assume `events` as the table name. Should this be configurable for projects that use a different schema?

5. **Color theme**: The frontend uses a dark theme with zinc/blue/amber colors. Should the package support light mode, or stay opinionated on dark?
