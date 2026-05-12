---
title: "Audit: rootsignal v0.3 → v0.4 deep dive"
type: audit
date: 2026-05-11
target: rootsignal-scout (pre-launch) + dependent crates
companion_to:
  - docs/plans/2026-05-11-v0.4-implementation-plan.md
  - docs/migrations/rootsignal-v04-runbook.md
status: complete
---

# Rootsignal v0.3 → v0.4 deep-dive audit

A senior-engineer briefing covering rootsignal's architecture, every
causal-rs touchpoint, and a concrete v0.4 migration plan.

Read in order: §1 frames the system, §§2–7 enumerate what's actually
there, §8 is the load-bearing gap list, §9 sequences the work, §10
calls out the things that need a design call before code lands.

---

## 1. Architecture overview

### 1.1 Per-crate purpose

```
modules/
├── rootsignal-common         # value types, world/system/telemetry Facts
├── rootsignal-world          # WorldEvent (one Fact enum, 7 variants)
├── rootsignal-events         # *separate*, domain-agnostic event store
├── rootsignal-graph          # Neo4j writer + read API + GraphProjector
├── rootsignal-archive        # web/feed/social scraping (HTTP layer)
├── rootsignal-scout          # the brain: facts, aggregates, reactors, projectors
├── rootsignal-scout-supervisor  # standalone background process (cron)
├── rootsignal-api            # GraphQL + inspector mount
├── rootsignal-migrate        # one-off binary for schema migrations
├── simweb                    # test fixtures, web fixtures
├── ai-client                 # LLM client wrapper
├── apify-client              # social scraper REST client
├── browserless-client        # headless-browser content client
└── twilio-rs                 # SMS client
```

Three crates carry the bulk of causal-rs coupling:

- **`rootsignal-scout`** — *every Fact, every Aggregate, every
  Materializer (= v0.4 Projector), every reactor, every engine
  builder*. The migration is mostly this crate. 136 .rs files;
  ~141 `ctx.aggregate` / `ctx.deps` call sites.
- **`rootsignal-api`** — mounts the `causal_inspector` HTTP surface
  for the inspector UI (`modules/rootsignal-api/src/main.rs:358-572`
  and `src/kernel/inspector/read_model.rs`); uses
  `causal::reactor_queue::ReactorQueue` directly in
  `src/domains/scout/activities/runner.rs:19` (for crash-recovery
  / cancellation paths).
- **`rootsignal-graph`** — owns `GraphProjector`. The projector
  *consumes* `causal::types::PersistedEvent` in `src/projector.rs`
  and `src/pipeline.rs`. Loose coupling: only the persisted event
  type, no traits.

Lighter touch:

- `rootsignal-common` — uses `#[causal_core_macros::event(prefix=…)]`
  for `SystemEvent` and `TelemetryEvent`; defines a totally-separate
  `EventDomain` enum used for runtime classification (the v0.3
  legacy projector branches on it).
- `rootsignal-world` — uses the same macro for `WorldEvent`.
- `rootsignal-events` — has its *own* `StoredEvent` / `AppendEvent`
  types completely unrelated to `causal::PersistedEvent`. Used by
  the inspector PG read path. Out of scope for the v0.4 migration.
- `rootsignal-scout-supervisor` — uses `causal::Events` once in
  `src/checks/batch_review.rs:6` as a return-collection type. No
  engine construction.
- `rootsignal-archive`, `rootsignal-migrate`, `simweb`,
  `ai-client`, `apify-client`, `browserless-client`, `twilio-rs`
  — zero causal dependencies. Out of scope.

### 1.2 Dependency graph (rootsignal-internal)

```
                ┌──────────────────────┐
                │ rootsignal-common    │ ← Fact-emitters: SystemEvent,
                │ (+ rootsignal-world) │   TelemetryEvent, WorldEvent
                └──────┬───────────────┘
                       │
            ┌──────────┴──────────┬──────────────┐
            ▼                     ▼              ▼
   ┌─────────────────┐    ┌──────────────┐  ┌──────────────────┐
   │ rootsignal-     │    │ rootsignal-  │  │ rootsignal-      │
   │ graph           │    │ scout-       │  │ archive          │
   │  (GraphProj.)   │    │ supervisor   │  └─────────┬────────┘
   └────────┬────────┘    └──────┬───────┘            │
            │                    │                    │
            └──────┬─────────────┴─────────┬──────────┘
                   ▼                       ▼
            ┌───────────────────────────────────┐
            │ rootsignal-scout                  │   ← all Facts, reactors,
            │  (causal engine construction,     │     projectors, aggregates
            │   domain logic, engine variants)  │
            └────────┬──────────────────────────┘
                     │
                     ▼
            ┌──────────────────┐
            │ rootsignal-api   │   ← engine spawner, inspector mount,
            │  (GraphQL,       │     GraphQL resolvers
            │   inspector)     │
            └──────────────────┘
```

`rootsignal-events` and `rootsignal-migrate` are top-level (the
api uses rootsignal-events directly for the inspector PG read).

### 1.3 Typical event flow

Production path for a region scrape (the dominant case):

```
GraphQL mutation
  → rootsignal_api::domains::scout::activities::runner::run_scrape
    → ScoutDeps::build_scrape_engine
      → rootsignal_scout::core::engine::build_engine(deps, store)
        ↓
        engine.emit(LifecycleEvent::ScoutRunRequested { run_id, … })
          .correlation_id(run_id)
          .settled().await
        ↓
[engine inner loop, multi-stage causal chain:]
ScoutRunRequested
  → lifecycle::find_stale_signals       → SystemEvent::SignalsExpired
  → lifecycle::prepare_sources          → LifecycleEvent::SourcesPrepared
                                          + DiscoveryEvent::*
  → scrape::start_web_scrape            → ScrapeEvent::WebScrapeCompleted
  → scrape::start_social_scrape         → ScrapeEvent::SocialScrapeCompleted
  → signals::dedup_signals              → SignalEvent::DedupCompleted
                                          + WorldEvent::* (citations, signals)
                                          + SystemEvent::* (review verdicts)
  → discovery::expand_sources           → DiscoveryEvent::SourcesDiscovered
  → scrape::process_web_results
  → scrape::process_social_results
  → scrape::discover_topics
  → enrichment::review_gate (fan-in)    → EnrichmentEvent::EnrichmentReady
  → enrichment::run_enrichment          → SystemEvent::* (locations, actors)
  → expansion::expand_signals           → ExpansionEvent::ExpansionCompleted
  → synthesis::compute_similarity       → SynthesisEvent::SimilarityComputed
  → synthesis::map_responses            → SynthesisEvent::ResponsesMapped
  → synthesis::infer_severity           → SynthesisEvent::SeverityInferred
                                          + LifecycleEvent::PhaseCompleted
  → core::projection::run_completion_handler  → LifecycleEvent::ScoutRunCompleted
        ↓
[parallel projection consumers per event:]
  → neo4j_projection_handler   → Cypher writes
  → runs_projection            → INSERT/UPDATE runs (PG)
  → system_log_projection      → tracing::info!
  → scheduled_scrapes_projection → INSERT scheduled_scrapes (PG)
  → schedules_projection       → INSERT/UPDATE schedules (PG)
```

### 1.4 Where the engine is constructed

The engine is built in **6 distinct variants** (5 in `engine.rs`,
1 in `workflows/mod.rs:248`):

| Variant | File | Trigger Fact |
|---|---|---|
| `build_engine` (scrape) | `core/engine.rs:110` | `LifecycleEvent::ScoutRunRequested` |
| `build_weave_engine` | `core/engine.rs:193` | `LifecycleEvent::GenerateSituationsRequested` |
| `build_coalesce_engine` | `core/engine.rs:259` | `LifecycleEvent::CoalesceRequested` |
| `build_cluster_weave_engine` | `core/engine.rs:320` | `LifecycleEvent::ClusterWeaveRequested` |
| `build_feed_group_engine` | `core/engine.rs:381` | `LifecycleEvent::FeedGroupRequested` |
| `build_news_engine` | `core/engine.rs:475` | `LifecycleEvent::NewsScanRequested` |
| `build_infra_only_engine` | `core/engine.rs:442` | (no domain reactors) |
| `build_infra_engine` | `workflows/mod.rs:235` | (projection-only) |

Each variant has the same shape:

```rust
causal::Engine::new(deps)
    .with_aggregators(pipeline_aggregators::aggregators())
    .with_reactors(some_domain::reactors::reactors())
    …
    .on_dlq(|info: DlqTerminalInfo| PipelineEvent::HandlerFailed { … })
    .with_store(s)                                  // optional
    .with_event_metadata(json!({"run_id":…}))
    .snapshot_every(100)
    .with_reactor(projection::neo4j_projection_handler(projector))
    .with_reactor(projection::run_completion_handler())
    .with_projection(projection::runs_projection())
    .with_projection(projection::system_log_projection())
    .with_projection(projection::scheduled_scrapes_projection())
    .with_projection(projection::schedules_projection())
```

A **parallel v0.3-target builder** already exists in
`core/v03_engine.rs` — `V03EngineBuilder::build()` at line 94
uses the (now-stale) v0.3 `EngineBuilder::new(log, checkpoint, outbox)`
shape. This is the path the migration extends.

---

## 2. Event / Fact taxonomy

### 2.1 The 16 Facts found (vs. the runbook's 15)

| # | Fact enum | File | v0.3 prefix | v0.3 stream categories | Ephemeral? | Manual `impl Fact`? |
|---|---|---|---|---|---|---|
| 1 | `WorldEvent` | `rootsignal-world/src/events.rs:19` | `world` | — | no | macro |
| 2 | `SystemEvent` | `rootsignal-common/src/system_events.rs:34` | `system` | — | no | macro |
| 3 | `TelemetryEvent` | `rootsignal-common/src/telemetry_events.rs:16` | `telemetry` | — | no | macro |
| 4 | `DiscoveryEvent` | `rootsignal-scout/src/domains/discovery/events.rs:12` | `discovery` | `discovery` (singleton) | no | **manual** (line 53) |
| 5 | `ScrapeEvent` | `rootsignal-scout/src/domains/scrape/events.rs:16` | `scrape` | `scout_run` | no | **manual** (line 100) |
| 6 | `LifecycleEvent` | `rootsignal-scout/src/domains/lifecycle/events.rs:19` | `lifecycle` | `scout_run` | no | **manual** (line 174) |
| 7 | `SchedulingEvent` | `rootsignal-scout/src/domains/scheduling/events.rs:11` | `scheduling` | `schedule`, `scrape_schedule` | no | **manual** (line 95) |
| 8 | `SignalEvent` | `rootsignal-scout/src/domains/signals/events.rs:114` | `signal` | (no manual stream) | no | macro |
| 9 | `SynthesisEvent` | `rootsignal-scout/src/domains/synthesis/events.rs:8` | `synthesis` | — | no | macro |
| 10 | `EnrichmentEvent` | `rootsignal-scout/src/domains/enrichment/events.rs:8` | `enrichment` | — | no | macro |
| 11 | `ExpansionEvent` | `rootsignal-scout/src/domains/expansion/events.rs:8` | `expansion` | — | no | macro |
| 12 | `CuriosityEvent` | `rootsignal-scout/src/domains/curiosity/events.rs:9` | `curiosity` | — | no | macro |
| 13 | `PipelineEvent` | `rootsignal-scout/src/core/pipeline_events.rs:13` | `pipeline` | — | no | macro |
| 14 | `CoalescingEvent` | `rootsignal-scout/src/domains/coalescing/events.rs:7` | `coalescing` | — | **yes** | macro |
| 15 | `ClusterWeavingEvent` | `rootsignal-scout/src/domains/cluster_weaving/events.rs:6` | `cluster_weaving` | — | **yes** | macro |
| 16 | `SituationWeavingEvent` | `rootsignal-scout/src/domains/situation_weaving/events.rs:8` | `situation_weaving` | — | **yes** | macro |
| 17 | `SupervisorEvent` | `rootsignal-scout/src/domains/supervisor/events.rs:8` | `supervisor` | — | **yes** | macro |

**Reconciliation with the runbook:** the runbook lists 15 Facts. It
**misses `PipelineEvent`** (`rootsignal-scout/src/core/pipeline_events.rs`).
PipelineEvent has two variants — `HandlerFailed { handler_id, source_event_type, error, attempts }`
and `BudgetSpent { cents }` — and the legacy `on_dlq` mapper in every
engine variant emits PipelineEvent::HandlerFailed. It absolutely
needs a P12 row.

Counts also disagree on how many "ephemeral" — runbook says 4 and
that's correct (CoalescingEvent, ClusterWeavingEvent,
SituationWeavingEvent, SupervisorEvent). Good.

### 2.2 The 5 known-explicit Facts — confirmed

DiscoveryEvent, TelemetryEvent, LifecycleEvent, ScrapeEvent,
SchedulingEvent all have manual `impl Fact` blocks. Confirmed
against source:

- **DiscoveryEvent** — singleton stream
  (`StreamRef { category: "discovery", id: Uuid::nil() }`,
  `events.rs:62`). The author flags this as a "hot stream" footgun
  in the comment. Under v0.4 `CATEGORY="discovery"`,
  `stream_id() = Uuid::nil()` — preserved, but now the hot-stream
  property is more visible because it's the per-Fact const.
- **TelemetryEvent** — comment in `telemetry_events.rs:117-120`
  acknowledges the macro emits `impl Fact` with
  `stream_id() = Uuid::nil()` by default. Same hot-stream concern;
  same migration treatment.
- **LifecycleEvent** — `stream(run_id) -> "scout_run"`. Runbook
  says CATEGORY moves to `"lifecycle"`, breaking the `scout_run`
  shared category. **This is the load-bearing rename.**
- **ScrapeEvent** — same `stream(run_id) -> "scout_run"`. Same rename
  to `"scrape"`.
- **SchedulingEvent** — splits per variant: 5 variants → `"schedule"`,
  1 variant (`ScrapeScheduled`) → `"scrape_schedule"`. v0.4 forbids
  a Fact enum spanning >1 category, so this Fact splits.

### 2.3 The 4 ephemeral Facts — confirmed; recipe stands

Confirmed shape from `domains/*/events.rs`:

- `CoalescingEvent` has 3 variants. **None carry `run_id`** today
  (CoalescingCompleted, CoalescingSkipped { reason },
  GroupFeedCompleted { group_id, signals_added, queries_refined }).
  P1.5 recipe (add `run_id: Uuid` to each variant) applies cleanly.
- `ClusterWeavingEvent` — 2 unit-ish variants
  (ClusterWeaveCompleted, ClusterWeaveSkipped { reason }).
  Both need `run_id: Uuid`.
- `SituationWeavingEvent` — same shape (SituationsWeaved,
  NothingToWeave { reason }).
- `SupervisorEvent` — same. `SupervisionCompleted` is a unit
  variant today, exactly as the runbook flags.

The runbook's P1.5 recipe applies. **One amendment:** the emitters
for these events (in each domain's `mod.rs`) take `ctx: Context<…>`
and have access to `ctx.deps().run_id` — that's the source of
`run_id` for the carrier-event upgrade. There's no
`ctx.aggregate::<PipelineState>().curr.run_id` because
PipelineState doesn't carry a `run_id` field today (it has
`run_scope`, not a run id). The mapping is straight from
`deps.run_id`.

### 2.4 Two notable Fact-implementation details

**`SchedulingEvent::ScheduleCreated` uses `Uuid::new_v5(NAMESPACE_OID,
schedule_id.as_bytes())`** because `schedule_id` is a `String`, not a
`Uuid` (`scheduling/events.rs:111`). When the split lands
(`ScheduleEvent` per the runbook), `stream_id()` keeps the v5
derivation: there's no Uuid field in the variant. This is fine
under v0.4 — Fact::stream_id() returns Uuid regardless of how the
Fact computes it — but reviewers should expect this oddity.

**`LifecycleEvent::SourcesPrepared` carries `run_id: Uuid` only
with `#[serde(default)]`**, meaning pre-cutover payloads
deserialize with `run_id = Uuid::nil()`. The comment
(`lifecycle/events.rs:50-53`) says cutover prevents v0.3 from ever
seeing those payloads. Under v0.4 same property holds — but if a
fact's `stream_id()` returns `Uuid::nil()` for a legacy row, that
row lands in stream `lifecycle-00000000-…` which co-mingles with
any future Uuid::nil() emission. Worth noting in the migration
runbook; today it's not a blocker because the cutover skip
protects.

---

## 3. Aggregate patterns

### 3.1 The three Aggregate types

| Aggregate | File | Today's `aggregate_type()` | Folds (legacy `Apply<E>`) |
|---|---|---|---|
| `PipelineState` | `core/aggregate.rs:553` | `"ScoutRun"` | 12 event types via `#[aggregators(singleton)]` |
| `SignalLifecycle` | `domains/curiosity/aggregates.rs:13` | `"SignalLifecycle"` | `CuriosityEvent` (keyed by `lifecycle_signal_id()`) |
| `ConcernLifecycle` | `domains/curiosity/aggregates.rs:26` | `"ConcernLifecycle"` | `CuriosityEvent` (keyed by `lifecycle_concern_id()`) |

**PipelineState** is a saga-pattern read-only aggregate:

- Singleton (one per run, keyed by `Uuid::nil()` via `#[aggregators(singleton)]`).
- Folded from 12 different Fact types (SignalEvent, ScrapeEvent,
  DiscoveryEvent, PipelineEvent, SynthesisEvent, EnrichmentEvent,
  ExpansionEvent, LifecycleEvent, WorldEvent, SystemEvent,
  TelemetryEvent, SituationWeavingEvent, SupervisorEvent).
- **Does not gate any writes** — reactors emit freely against any
  of those 12 Fact streams. PipelineState provides read-side
  context for reactor filter predicates and for stat accumulation
  consumed by `run_completion_handler`.
- The state lives in process memory (per `AggregatorRegistry`,
  shared across consumers via the engine's registry copy).
- Maps to **`Aggregator::fold::<A, F>` × 12** under v0.4 — not
  `for_type`, because OCC must NOT gate any of those 12 streams.

**SignalLifecycle / ConcernLifecycle** — also saga-pattern but
keyed (not singleton). Used in `domains/curiosity/mod.rs:48-75`
via `ctx.aggregate_of::<SignalLifecycle>(signal_id).curr` to
prevent re-investigation of an already-investigated signal /
concern. Both fold `CuriosityEvent` only.

Both map to **`Aggregator::fold::<A, CuriosityEvent>`** under v0.4
— same reasoning, read-only.

### 3.2 No write-OCC aggregates anywhere

Searched the workspace for `engine.append`, `engine.load`,
`expected_version`, `with_aggregator(...for_type::...)`, OCC-gate
patterns. Zero hits.

**Implication:** every Aggregate in rootsignal is a saga / read-fold.
The v0.4 `Aggregator::for_type` API (which marks a stream as
OCC-required and refuses non-`.expecting()` emits) is **not used**
by any current rootsignal code. All three Aggregates migrate to
`Aggregator::fold::<A, F>` — and the OCC gate is dead code from
rootsignal's perspective.

This is a meaningful finding: it means rootsignal can avoid the
"how do I migrate command-handler patterns?" question entirely.
Pure consumer / saga shape throughout.

### 3.3 Multi-Fact apply (PipelineState specifically)

The `#[aggregators(singleton)]` macro in `core/aggregate.rs:559`
generates today's per-Fact apply registration. Under v0.4, this
becomes 12 separate `impl Apply<F> for PipelineState` blocks plus
12 `Aggregator::fold::<PipelineState, F>()` in a `pub fn aggregators()`
helper. Concretely:

```rust
impl Aggregate for PipelineState {
    const NAME: &'static str = "ScoutRun";       // unchanged string
}

impl Apply<ScrapeEvent>     for PipelineState { fn apply(&mut self, e: &ScrapeEvent)     { self.apply_scrape(e); } }
impl Apply<SignalEvent>     for PipelineState { fn apply(&mut self, e: &SignalEvent)     { self.apply_signal(e); } }
// … etc, 12 total

pub fn aggregators() -> Vec<Aggregator> {
    vec![
        Aggregator::fold::<PipelineState, ScrapeEvent>(),
        Aggregator::fold::<PipelineState, SignalEvent>(),
        // … 12 total
    ]
}
```

The `#[aggregators]` macro (singular `#[aggregator]` is deleted in
P11; plural `#[aggregators]` survives — see plan P11.c) needs to
emit this v0.4 shape. There are also **TWO `#[aggregators]`-tagged
modules** today:
- `pipeline_aggregators` in `core/aggregate.rs:559` (singleton variant)
- `curiosity_aggregators` in `domains/curiosity/aggregates.rs:33` (keyed variant)

The macro must support both. If the macro doesn't, the alternative
is hand-rolling 14 Apply impls + 14 Aggregator::fold entries — not
hard, just verbose.

### 3.4 Aggregate hydration cost — what changes under v0.4

In v0.3 today, `AggregatorRegistry::apply_event` consumes events
flowing through projection/reactor runners and folds them into
in-memory state. Hydration is "fold from the start of the log,
once per engine startup" — no `load_stream` happens.

In v0.4 same — `Aggregator::fold` keeps that behavior; only
`Aggregator::for_type` aggregates call `engine.load::<A, F>` (which
none do). The `for_type` API would change cost (read the entire
aggregate stream on each command-handler call), but rootsignal
never enters that path.

**One subtle wrinkle.** Under v0.4 stream-layout change:
`LifecycleEvent` events move out of `aggregate_type="scout_run"`
and into `aggregate_type="lifecycle"`. Same for ScrapeEvent →
`aggregate_type="scrape"`. The legacy projector's
`load_from(after, limit)` reads the global log and doesn't care
about aggregate_type, so v0.4 PipelineState folding continues to
see all 12 Fact streams. **Read-fold aggregators don't subscribe
per-stream**; they subscribe per-event-type-prefix on the global
log. No change to fold cost.

---

## 4. Projector / Materializer inventory

### 4.1 Five typed Materializers (already partial v0.3-style)

Already implemented against `causal::Materializer` trait (v0.3.5
shape — needs only the GROUP_NAME const + rename to Projector).
File-by-file:

| Materializer | File | `type Fact` | Effect | Notes |
|---|---|---|---|---|
| `RunsMaterializer` | `core/runs_materializer.rs:38` | `LifecycleEvent` | INSERT/UPDATE `runs` table (PG) | Skips SourcesPrepared / NewsScanRequested |
| `ScheduledScrapesMaterializer` | `core/scheduled_scrapes_materializer.rs:37` | `SchedulingEvent` | INSERT into `scheduled_scrapes` (PG); only acts on `ScrapeScheduled` variant | Filters in body; under v0.4 split this becomes `type Fact = ScrapeScheduleEvent` |
| `SchedulesMaterializer` | `core/schedules_materializer.rs` | `SchedulingEvent` | INSERT/UPDATE `schedules` table (PG) | Skips ScrapeScheduled; under v0.4 split becomes `type Fact = ScheduleEvent` |
| `SystemLogMaterializer` | `core/system_log_materializer.rs` | (TelemetryEvent — needs verification) | `tracing::info!` (no DB) | Trivial migration |
| `Neo4jMaterializer` | `core/neo4j_materializer.rs:64` | (uses `MultiPrefixMaterializer`, NOT `Materializer`) | Cypher writes via `GraphProjector::project` | **Migrates to `MultiProjector` not `Projector`** |

The first four migrate to `Projector` cleanly:
1. Rename file `*_materializer.rs` → `*_projector.rs`
2. Rename impl `Materializer` → `Projector`
3. Rename method `materialize` → `project`
4. Add `const GROUP_NAME: &'static str = "..."` (use the same
   string as today's `RUNS_CONSUMER_ID` etc. from
   `core/v03_engine.rs:207-219`)
5. Drop the second-arg `consumer_id` from `EngineBuilder::with_*`
   call sites (v0.4 reads the id from `P::GROUP_NAME`)

Effort: ~30 minutes per materializer for the mechanical change.

### 4.2 Neo4jMaterializer → MultiProjector

Today (`core/neo4j_materializer.rs:64-71`):

```rust
impl MultiPrefixMaterializer for Neo4jMaterializer {
    const TYPE_PREFIXES: &'static [&'static str] = &[
        "world:", "system:", "telemetry:", "discovery:", "pipeline:",
    ];
    async fn materialize(&self, event: &PersistedEvent, _ctx: Ctx<'_>) -> Result<()> { … }
}
```

v0.4 (`modules/causal/src/multi_projector.rs`):

```rust
#[async_trait]
impl MultiProjector for Neo4jProjector {
    const GROUP_NAME: &'static str = "neo4j_projector";
    const CATEGORIES: &'static [&'static str] = &[
        "world", "system", "telemetry", "discovery", "pipeline",
    ];          // bare categories, no trailing colon
    async fn project(&self, event: &PersistedEvent, _ctx: Ctx<'_>) -> Result<()> { … }
}
```

Body is unchanged; the trait name + const names + category-string
shape (no trailing `:`) change. The v0.4 runner does the right
prefix match (`starts_with_category` in `multi_projector.rs:236`).

### 4.3 Legacy projections that still live in `core/projection.rs`

`core/projection.rs` (740 lines) holds the **v0.2-style legacy
projection factory functions** that `build_engine` in
`core/engine.rs` registers via `with_projection(...)`:

- `runs_projection()` — duplicates `RunsMaterializer` logic; targets
  the same table. Delete after Projector cutover.
- `system_log_projection()` — duplicates `SystemLogMaterializer`.
- `scheduled_scrapes_projection()` — duplicates `ScheduledScrapesMaterializer`.
- `schedules_projection()` — duplicates `SchedulesMaterializer`.
- `neo4j_projection_handler()` — duplicates `Neo4jMaterializer`.
- `run_completion_handler()` — duplicates `RunCompletion` reactor.
- `capture_handler()` — test capture handler; trivial port.

These exist for the dual-write parity validation period
(`docs/plans/2026-05-06-causal-v03-phase-4f-first-consumer-migration-plan.md`).
**Under v0.4 they're dead** — the legacy `with_projection`,
`with_reactor` (legacy form), `on_any`, `project` APIs are deleted
in P11.c/d. `core/projection.rs` and `core/engine.rs` get deleted
entirely. The v0.3 typed materializers + reactors in `core/*.rs`
take over.

### 4.4 GraphProjector — domain-side projector

`rootsignal-graph/src/projector.rs` exposes `GraphProjector::project(&PersistedEvent) -> ApplyResult`.
It deserializes the payload internally, dispatches by event_type
to per-domain Cypher writers in `rootsignal-graph/src/writer.rs`.

This is **not** a `causal::Projector` impl — it's a domain object
that the `Neo4jMaterializer` wraps. Under v0.4 it stays exactly
as-is: `MultiProjector::project` still delegates to
`graph_projector.project(&persisted_event)`. The PersistedEvent
shape is what graph_projector cares about, and v0.4 preserves
PersistedEvent (with the field rename `aggregate_type/aggregate_id`
remaining `Option<String>`/`Option<Uuid>` in v0.4 too — see
`modules/causal/src/types.rs`).

---

## 5. Reactor inventory

### 5.1 Two reactor styles coexisting in scout today

**Legacy `#[reactor]` macro form** — lives in
`domains/*/mod.rs`. Uses `causal::reactor::Reactor<D>` (the builder
struct), `Context<D>`, `Events`. Example
(`domains/coalescing/mod.rs:75-90`):

```rust
fn is_coalesce_requested(e: &LifecycleEvent, _ctx: &Context<ScoutEngineDeps>) -> bool { … }

#[reactors]
pub mod reactors {
    use super::*;
    #[reactor(on = LifecycleEvent, id = "coalescing:coalesce", filter = is_coalesce_requested)]
    async fn coalesce(
        event: LifecycleEvent,
        ctx: Context<ScoutEngineDeps>,
    ) -> Result<Events> { … }
}
```

The macro generates a registration helper `reactors()` that
returns `Vec<Reactor<ScoutEngineDeps>>`. `EngineBuilder::with_reactors`
consumes it.

**v0.3 typed `Reactor` trait form** — lives in `core/*_reactor.rs`,
already migrated. Example (`core/web_scrape_reactor.rs:66`):

```rust
impl Reactor for WebScrape {
    type Trigger = LifecycleEvent;
    async fn react(&self, trigger: &LifecycleEvent, ctx: Ctx<'_>) -> Result<Events> { … }
}
```

These are registered by `core/v03_engine.rs`.

### 5.2 Every reactor in the codebase

**`core/*.rs` (8 typed reactors — already partially v0.3):**

| Reactor | File | Trigger | Output |
|---|---|---|---|
| `WebScrape` | `core/web_scrape_reactor.rs:67` | `LifecycleEvent::SourcesPrepared` (filter) | `ScrapeEvent::WebScrapeCompleted` + signal/discovery side-events |
| `SocialScrape` | `core/social_scrape_reactor.rs` | `LifecycleEvent::SourcesPrepared` (filter) | `ScrapeEvent::SocialScrapeCompleted` |
| `ProcessWebResults` | `core/process_web_results_reactor.rs` | `ScrapeEvent::SourcesResolved`/related | various |
| `ProcessSocialResults` | `core/process_social_results_reactor.rs` | (similar) | various |
| `TopicDiscovery` | `core/topic_discovery_reactor.rs` | `DiscoveryEvent::SocialTopicsDiscovered`/related | `ScrapeEvent::TopicDiscoveryCompleted` |
| `BootstrapSources` | `core/bootstrap_sources_reactor.rs` | (lifecycle gate) | bootstrap-related |
| `FilterDomains` | `core/filter_domains_reactor.rs` | (discovery filter) | filtered DiscoveryEvent |
| `RunCompletion` | `core/run_completion_reactor.rs:60` | `LifecycleEvent::PhaseCompleted` | `LifecycleEvent::ScoutRunCompleted` |

**`domains/*/mod.rs` (legacy `#[reactor]` macro form):**

Per-domain reactor counts (from grepping `#[reactor(` over
`domains/`):

| Domain | Reactor count | Key reactors |
|---|---|---|
| `news_scanning` | 1 | `scan_news` (LifecycleEvent::NewsScanRequested) |
| `coalescing` | 4-5 | `coalesce` (CoalesceRequested), `feed_group`, completion handlers |
| `cluster_weaving` | 1 | `weave_cluster` (ClusterWeaveRequested) |
| `signals` | 1 | `dedup_signals` (ScrapeEvent completion) |
| `discovery` | 3-4 | `bootstrap_sources`, `expand_sources`, `promote_links`, `prepare_response_sources` |
| `lifecycle` | 2 | `find_stale_signals`, `prepare_sources` |
| `scrape` | (delegate to core/*_reactor) | — |
| `synthesis` | 3 | `compute_similarity`, `map_responses`, `infer_severity` |
| `enrichment` | ~6 | `review_gate`, `run_enrichment`, geocoding, actor_location, etc. |
| `expansion` | 1 | `expand_signals` |
| `curiosity` | 4-5 | `investigate_signal`, `link_concern`, `scout_responses`, `scout_gatherings` |
| `situation_weaving` | 1 | `weave_situations` |
| `supervisor` | 1 | `supervise_region` |

**Approximate total: 30-35 reactors today.**

Each `#[reactor]`-form reactor migrates to v0.4 by:
1. Adding `GROUP_NAME` const (= today's `id = "domain:name"`).
2. Replacing `Context<ScoutEngineDeps>` with `Ctx<'_>` and rewiring
   deps access (today `ctx.deps()` returns `Arc<ScoutEngineDeps>`;
   v0.4 `Ctx` has no `deps` accessor — see §8 gap analysis).
3. Replacing `ctx.logger` calls (today `Context<D>` carries a
   `Logger`; v0.4 `Ctx` doesn't — see §8).
4. Replacing `ctx.idempotency_key` (same — gone in v0.4).
5. Reactor "filter" predicate becomes an inline `match self.trigger { … }`
   guard at the top of `react()`. See `RunCompletion::react`
   (`core/run_completion_reactor.rs:64-70`) for the pattern.

### 5.3 Reactor chain diagram (in prose)

```
LifecycleEvent::ScoutRunRequested
  → lifecycle::find_stale_signals  → SystemEvent::SignalsExpired
  → lifecycle::prepare_sources     → LifecycleEvent::SourcesPrepared
                                     (+ TelemetryEvent::SearchPerformed)

LifecycleEvent::SourcesPrepared
  → scrape::start_web_scrape       → ScrapeEvent::WebScrapeCompleted
                                     (+ WorldEvent::*, SystemEvent::*)
  → scrape::start_social_scrape    → ScrapeEvent::SocialScrapeCompleted

ScrapeEvent (any completion)
  → signals::dedup_signals          → SignalEvent::DedupCompleted
                                     (+ WorldEvent::Created, SystemEvent::ReviewVerdictReached)
  → discovery::promote_links        → DiscoveryEvent::SourcesDiscovered
                                     (when collected_links non-empty + tension done)
  → discovery::expand_sources       → DiscoveryEvent::SocialTopicsDiscovered
                                     (when tension done + expansion pending)

[fan-in:]  [SystemEvent::ReviewVerdictReached, SignalEvent::DedupCompleted]
  → enrichment::review_gate         → EnrichmentEvent::EnrichmentReady (filter: review+response complete)

EnrichmentEvent::EnrichmentReady
  → enrichment::run_enrichment      → SystemEvent::ActorLocationIdentified, …

ExpansionEvent::ExpansionCompleted
  → synthesis::compute_similarity   → SynthesisEvent::SimilarityComputed (parallel)
  → synthesis::map_responses        → SynthesisEvent::ResponsesMapped     (parallel)
[fan-in (filter: both above set)]
  → synthesis::infer_severity       → SynthesisEvent::SeverityInferred
                                     + LifecycleEvent::PhaseCompleted

LifecycleEvent::PhaseCompleted
  → core::run_completion_reactor    → LifecycleEvent::ScoutRunCompleted

LifecycleEvent::ScoutRunCompleted   → (terminal — projectors materialize)
```

The diagram for situation_weaving / coalescing / cluster_weaving is
similar, each rooted at its own `LifecycleEvent::*Requested` and
terminating at `LifecycleEvent::PhaseCompleted { phase: … }`.

### 5.4 Retry / DLQ usage

Every engine variant registers an `on_dlq` mapper
(`core/engine.rs:142-147` and 5 other sites):

```rust
.on_dlq(|info: causal::DlqTerminalInfo| PipelineEvent::HandlerFailed {
    handler_id: info.reactor_id.clone(),
    source_event_type: info.source_event_type.clone(),
    error: info.error.clone(),
    attempts: info.attempts,
})
```

The mapped `PipelineEvent::HandlerFailed` is folded into
PipelineState (`core/aggregate.rs:520-541`), where the handler_id
is matched against a hard-coded table to unblock downstream gates
(`"scrape:start_web_scrape"` → `tension_web_done = true`, etc.).
This is **load-bearing for pipeline forward-progress**: tests
exist (`core/aggregate.rs:728-832`) pinning the behavior.

**Per the v0.4 implementation plan P9.b, `on_dlq` is deferred.**
The trait-level surface doesn't have it. This is the **single
biggest gap** for the migration — see §8.1.

Retry semantics (no retry policy in user code — relies on
framework defaults) are independent of on_dlq. The v0.4 supervisor
loop in `engine_v3.rs:747-798` has backoff-on-error built in, but
no terminal-failure trigger that calls a user mapper.

---

## 6. Engine wiring

### 6.1 Two engine constructors coexist

**`core/engine.rs`** — the production path today. Uses legacy
`causal::Engine::new(deps)` (the `Engine<D>` form being deleted in
P11.c). All six `build_*_engine` functions live here, plus
`build_infra_only_engine`.

**`core/v03_engine.rs`** — the migration target. Uses
`engine_v3::EngineBuilder::new(log, checkpoint, outbox)`. Today
registers a subset: 5 materializers + 8 typed reactors, no
domain `#[reactor]`-form reactors (because they don't compile
against `engine_v3::EngineBuilder::with_reactor` — different
trait, no `Vec<Reactor<D>>` adapter).

Production today runs `core/engine.rs` — `v03_engine.rs` exists for
parity-audit integration tests (the `tests/v03_*.rs` files).

### 6.2 Custom backend: `PostgresStore`

`rootsignal-scout/src/core/postgres_store.rs` (959 lines) implements
**both legacy `causal::event_log::EventLog` AND `causal::reactor_queue::ReactorQueue`**.

Key methods:

- `EventLog::append` — dual-writes to legacy `events` table AND
  v0.3 `causal_log` mirror in one transaction (`postgres_store.rs:62-180`).
  Production read path uses `events`; v0.3 reads use `causal_log`.
- `EventLog::load_from` — global log read (line 182+); scoped by
  `correlation_id == Uuid::nil()` mode (full read) vs. specific
  correlation (per-engine settle loop).
- `ReactorQueue` impl — line ~600+. Provides intent commit,
  reactor resolution, completion, DLQ, journaling. Used by the
  legacy engine's reactor loop.

**Under v0.4 this file is deeply affected:**

- `EventLog` trait is **deleted in P11.e** (`event_log.rs`
  comment: "legacy EventLog... is gone as of P11.e"). The
  `EventLogBackend` trait survives — `causal_replay::PgEventLogBackend`
  implements it. PostgresStore needs to either:
  (a) **add `EventLogBackend` impl** alongside its existing
  `EventLog` impl, or
  (b) **delete PostgresStore entirely** and use `causal_replay::PgEventLogBackend`.
  Option (b) is cleaner but loses the dual-write to legacy `events`
  table. Option (a) requires writing the `EventLogBackend`
  surface against the same dual-write semantics.

  Looking closer: `v03_engine.rs` already uses
  `causal_replay::PgEventLogBackend` (line 96). That backend reads
  from `causal_log`. PostgresStore today is consumed by the legacy
  engine path; **after the legacy engine deletes**, PostgresStore's
  `EventLog` impl has no consumer. So PostgresStore's role narrows
  to:
   - The dual-write to legacy `events` table (during transition).
   - Its `ReactorQueue` impl (also deleted in P11.b!).
   - Convenience methods for the rest of rootsignal (`has_pending_work`,
     `reclaim_stale` — line 37-49, used by api/runner.rs:586).

  **Recommendation:** keep `PostgresStore` only for the
  legacy-`events`-table writes (called by some legacy backfill path,
  needs verification) plus the convenience methods. Delete its
  `EventLog`+`ReactorQueue` impls; replace event-log reads with
  `causal_replay::PgEventLogBackend` system-wide.

- `causal::reactor_queue::ReactorQueue` is deleted in P11.b
  (`2026-05-11-v0.4-implementation-plan.md` line 894+). The
  `ReactorQueue` impl in PostgresStore goes away. Code that
  imports it must migrate to `CheckpointStore` + `ReactorOutbox`
  (split by v0.3, already used by v03_engine):
   - `rootsignal-scout/src/core/postgres_store.rs:15`
   - `rootsignal-api/src/domains/scout/activities/runner.rs:19`

  The `reactor_queue::ReactorQueue` import in `runner.rs:19`
  appears to only be the trait import for `store.reclaim_stale()`
  / `store.has_pending_work()` calls. Migration: those methods
  move to direct PG queries on `causal_checkpoints` /
  `causal_reactor_outbox` (the v0.3-shaped tables).

### 6.3 The `with_*` registration migration table

| Today | v0.4 |
|---|---|
| `engine.with_aggregators(some::aggregators())` | `builder.with_aggregators(some::aggregators())` (same name, but takes `IntoIterator<Item=Aggregator>` instead of legacy Vec) |
| `engine.with_reactors(some::reactors())` | `builder.with_reactors(some::reactors())` (takes `Vec<Box<dyn ReactorRegistration>>`) |
| `engine.with_reactor(handler)` | `builder.with_reactor(reactor_impl)` (takes one Reactor trait impl) |
| `engine.with_projection(p)` | `builder.with_projector(p)` (rename + Projector trait impl) |
| `engine.with_store(store)` | `EngineBuilder::new(log, checkpoint, outbox)` (set at construction; no per-builder method) |
| `engine.with_event_metadata(json!(…))` | `builder.with_default_metadata(metadata_map)` (takes `serde_json::Map`, not `Value`) |
| `engine.on_dlq(mapper)` | **deferred (P9.b)** — no v0.4 equivalent yet |
| `engine.snapshot_every(100)` | **deferred (P9.c)** — no v0.4 equivalent yet |

### 6.4 Engine emit usage

`engine.emit(fact).settled().await` — used **63 times** in the
codebase (production and tests combined; biggest concentrations in
`testing.rs` and `domains/signals/activities/engine_tests.rs`).

The v0.4 `Engine::emit` shape (`engine_v3.rs:590`) returns
`EmitBuilder<'_>` which is `IntoFuture` — so `engine.emit(f).await`
works for the simple case. The longer form is:

```rust
engine.emit(fact)
    .correlation_id(run_id)
    .parent_id(trigger_event_id)
    .metadata("_phase", "pre_migration")
    .await?;
```

**The `.settled()` method does NOT exist on v0.4 `EmitBuilder`.**
This is a critical migration point — see §8.2.

---

## 7. Test infrastructure

### 7.1 Test files in scout

```
modules/rootsignal-scout/tests/
├── conversion_test.rs           # legacy
├── domain_filter_test.rs        # legacy
├── dual_write_test.rs           # PostgresStore dual-write
├── extraction_test.rs           # legacy
├── firsthand_filter_test.rs     # legacy
├── investigation_triage_test.rs # legacy
├── quality_scenarios_test.rs    # legacy
├── v03_backfill_test.rs         # v0.3 path
├── v03_consumer_test.rs         # v0.3 ProjectionRunner exercise
├── v03_cutover_test.rs          # cutover seed + skip
├── v03_full_engine_smoke_test.rs # supervisor + 4 materializers
├── v03_parity_audit_test.rs     # dual-path parity validation
├── v03_process_results_test.rs  # ProcessWebResults reactor
├── v03_run_completion_test.rs   # RunCompletion reactor
├── v03_scheduled_scrapes_test.rs
├── v03_schedules_test.rs
├── v03_social_scrape_test.rs
├── v03_system_log_test.rs
└── v03_web_scrape_test.rs
```

The 13 `v03_*` files cover the typed-materializer / typed-reactor
path. All build `NewEvent { ephemeral: None, persistent: true }`
(see `tests/v03_web_scrape_test.rs:148` etc.) — the `ephemeral`
field is deleted in P1.5 / P11. Tests need a sweep to remove that
field from every NewEvent construction.

All 13 import `causal::event_log::EventLog` — that trait is
deleted. Imports rename to `EventLogBackend` (and `store.append(…)`
becomes `EventLogBackend::append(store, …)`).

The 7 legacy tests (`conversion_test.rs` etc.) exercise the
production `build_engine` path. They depend on legacy reactor /
materializer / aggregator wiring. Disposition:
- Tests that pin domain *behavior* (extraction logic, dedup
  verdicts, etc.) should survive — they translate to v0.4 engine
  use cleanly.
- Tests that pin *legacy framework semantics* (DLQ propagation,
  on_dlq mapping) need careful handling — the framework underneath
  changes.

### 7.2 Test fixture pattern

Tests use `Arc<MemoryStore>` cast to both `Arc<dyn EventLogBackend>`
and `Arc<dyn CheckpointStore>` (`core/runs_materializer.rs:250-251`).
This pattern works under v0.4 unchanged — MemoryStore implements
both traits.

The bigger test setup (`testing.rs` — 2721 lines!) provides
`MockFetcher`, `MockSignalReader`, `FixedEmbedder`, `MockExtractor`
plus engine builders. `testing.rs:1961` and friends call
`build_engine(deps, None)` — that path goes through legacy
`core/engine.rs` and breaks the moment v0.4 lands. The fix is
either (a) testing.rs migrates to a v0.4 builder helper or (b)
testing.rs builds the engine inline using `EngineBuilder::new(…)`.
Probably (a): add a `build_engine_v04(deps, …)` helper in
`core/engine.rs` (or wherever) and have testing.rs call it.

### 7.3 Integration-test gating

- `v03_full_engine_smoke_test.rs` — `#[ignore]`, requires
  `DATABASE_URL` pointing at local Postgres with migrations 054 +
  055 applied. Same shape as parity_audit + scheduled_scrapes etc.
- `tests/scenarios/` — directory exists; not investigated. Likely
  scenario fixtures.
- `domains/signals/activities/engine_tests.rs` — uses MemoryStore
  via `testing.rs::build_engine_with_capture`. Doesn't gate on PG.

Neo4j-dependent tests live in `rootsignal-graph`. Most scout tests
either mock Neo4j (`graph_client: None` in deps) or run against
Postgres-only fixtures.

---

## 8. v0.4 API gap list (the load-bearing section)

For each: what rootsignal does, why v0.4 doesn't fit, options,
severity.

### 8.1 GAP: Reactor retry / on_dlq semantics

**What rootsignal does today:** every `build_*_engine` registers
`.on_dlq(|info| PipelineEvent::HandlerFailed { … })`
(`core/engine.rs:142, 216, 281, 342, 403`). PipelineEvent::HandlerFailed
is folded into PipelineState which then unblocks downstream gates
based on `handler_id`. Without this, a transient reactor failure
(e.g. browserless timeout in `scrape:start_web_scrape`) wedges the
pipeline at the tension-scrape gate forever.

**Why v0.4 doesn't fit:** P9.b (on_dlq) is **deferred** per the
implementation plan (line 826-832). The v0.4 `EngineBuilder` has
no `.on_dlq()`. The supervisor loop catches errors and backs off
(`engine_v3.rs:772`) but has no terminal-failure trigger and no
retry-budget state. Same for `snapshot_every` (P9.c, deferred).

**Migration options:**

1. **Land P9.b before P12.** Adds retry-budget state to
   `ReactorRunner` + on_dlq mapper invocation on exhaustion. Plan
   estimates 1 day. This is the cleanest path. Note that the
   v0.4 mapper signature in the plan returns `Option<Fact>` (one
   Fact), not `Events` — rootsignal's current mapper returns
   `PipelineEvent::HandlerFailed` (one Fact) so this matches.

2. **Work around with per-reactor inline retry-and-emit-failure.**
   Each `react()` body wraps its activity call in a retry loop;
   on terminal failure, return `Ok(events![PipelineEvent::HandlerFailed { … }])`
   directly. Loses framework-level uniformity; spreads retry logic
   across 30+ reactors. Not a serious option for rootsignal-scale.

3. **Defer until post-launch; tolerate wedge-on-failure pre-launch.**
   Acceptable only if scout's pre-launch traffic is tiny and
   restartable. Given the scout pipeline times out on browserless
   fetches in normal operation, this is risky.

**Recommendation:** option 1. Land P9.b in causal-rs before P12
starts. Re-estimate causal-rs work: P9.b + P9.c are ~2 days
combined; P12 is 5-7 days. Adding 2 days to land P9.b is cheap
insurance.

**Severity: MUST-FIX-BEFORE-MIGRATION.**

### 8.2 GAP: `.settled()` on emit doesn't exist in v0.4

**What rootsignal does:** `engine.emit(fact).settled().await` is
used everywhere — production (`api/domains/scout/activities/runner.rs:56`)
and tests (everywhere). `.settled()` blocks until the entire
causal tree triggered by `fact` has been fully processed.

**Why v0.4 doesn't fit:** v0.4 `Engine::emit` returns an
`EmitBuilder` whose `IntoFuture` impl just writes the fact to the
log. There's no "wait for derived consumers to catch up" hook.
v0.4 provides `Engine::await_observed_by(consumer_id, position)`
(`engine_v3.rs:723`) for the per-consumer case, but no global
"settled" that waits for all consumers.

The async-consumer model is fundamentally different: in v0.3,
emit triggered an inline settle that ran reactors synchronously
in the same task. In v0.4, emit just appends; runners run
independently in supervisor tasks and reactors react to log
events with their own cursors.

**Migration options:**

1. **Re-implement `.settled()` as "wait until every registered
   consumer cursor ≥ emit position".** Add an
   `Engine::settle_emit(emit_result)` method that loops over every
   registered consumer id and `await_observed_by`s each. Composes
   naturally over the existing primitive. ~50-100 lines in
   `engine_v3.rs`.

2. **Switch rootsignal to fully async consumer model.** Drop
   `.settled()` calls in production; rely on cursor monotonicity
   for "eventually processed". Drop `.settled()` calls in tests
   too; replace with a "wait for terminal event" pattern (poll
   the captured-events sink until `ScoutRunCompleted` shows up).
   Production fits this. Tests need a meaningful rewrite — ~30
   test files use `.settled()`.

3. **Hybrid.** Add `.settle()` in causal-rs (option 1) AND migrate
   production-style code paths to use it as a porting bridge. In
   parallel, gradually shift tests to "wait for terminal" so
   they're explicit about what they're testing. Lowest churn.

**Recommendation:** option 1 first (a few hours of causal-rs work),
then option 3 over time. Documenting the difference is important
— `.settled()` in v0.3 was an in-process synchronous wait;
`.settle()` in v0.4 is a cursor-poll loop with bounded latency
based on consumer batch size + supervisor poll interval. Test
authors need to know it's not the same.

**Severity: MUST-FIX-BEFORE-MIGRATION.**

### 8.3 GAP: `Ctx<'_>` has no `deps()` or `idempotency_key()`

**What rootsignal does:** reactors get `Context<ScoutEngineDeps>`
where `ctx.deps()` returns `Arc<ScoutEngineDeps>` and
`ctx.idempotency_key` is a String derived by the framework.
Examples:
- `domains/coalescing/mod.rs:75-90` — `ctx.deps()` for
  `Coalescer::new(deps.…)`
- `core/web_scrape_reactor.rs:62-64` — `Self::idempotency_key_for(ctx.event_id)`
  manually deriving since v0.3 typed reactor uses `Ctx<'_>` which
  doesn't have it. (This is **already a partial workaround**:
  v0.3-typed reactors compute their own idempotency key from
  `ctx.event_id`.)

**Why v0.4 doesn't fit:** v0.4 `Ctx` (`contexts.rs:51-61`)
contains: `event_id, log_position, occurred_at, correlation_id,
metadata, aggregators` — no `deps`, no `idempotency_key`. v0.4's
philosophy: reactors are pure decision (per C5/C11 in the design
plan); dependency injection happens at reactor construction
(reactor structs hold their deps). v0.4 has no generic
`Context<D>` type at all.

**Migration options:**

1. **Move deps onto reactor structs (already partially done).**
   `WebScrape` in `core/web_scrape_reactor.rs:53` already does
   this: `struct WebScrape { deps: Arc<ScoutEngineDeps> }`. The
   `#[reactor]`-macro form takes `ctx.deps()` to access shared
   state — under v0.4 each macro-generated reactor becomes a
   struct that holds `Arc<ScoutEngineDeps>`. ~30 reactors get
   this treatment. Pattern is already proven (8 typed reactors
   in `core/*.rs` do it).

   The `#[reactor]`/`#[reactors]` macros need updating to emit
   the new shape. Per the plan P11.c they may be deleted entirely
   — if so, hand-write reactors as structs. Mechanical conversion.

2. **Provide a generic `Ctx<D>`-like helper outside causal-rs.**
   Don't — it leaks across crates and re-introduces the surface
   v0.4 explicitly cut.

For `idempotency_key`: do what `core/web_scrape_reactor.rs:62-64`
already does — derive it inside the reactor from
`format!("{}:{}", GROUP_NAME, ctx.event_id)`. Encapsulate in a
helper if the same derivation is repeated:
`pub fn deterministic_idempotency_key(group: &str, event_id: Uuid) -> String`.

**Severity: WORKS-BUT-AWKWARD.** Mechanical migration; just lots
of touch points (30+ reactors).

### 8.4 GAP: `ctx.logger` doesn't exist in v0.4

**What rootsignal does:** reactor bodies use `ctx.logger.info(…)`
/ `ctx.logger.debug(…)` (e.g. `domains/discovery/mod.rs:48-52`,
`domains/signals/mod.rs:49`, etc.) for diagnostic logs that show
up in the inspector UI.

**Why v0.4 doesn't fit:** v0.4 `Ctx` has no logger field. The
runtime side that consumed it (the `Logger` accumulator typed in
`reactor::Logger`) is deleted along with the legacy reactor
module.

**Migration options:**

1. **Switch to `tracing::*` macros.** Replace
   `ctx.logger.info("…")` → `tracing::info!("…")`. Loss: inspector
   UI no longer sees the line. Acceptable if inspector currently
   doesn't surface logger output anyway. (Worth verifying with the
   inspector UI; per quick read of
   `rootsignal-api/src/kernel/inspector/read_model.rs` the
   InspectorReadModel reads `causal_handler_logs` etc. — those
   tables get populated by the framework's Logger sink. So
   migrating off `ctx.logger` does lose UI visibility.)

2. **Add `Ctx::log(level, message)` back to v0.4.** Small surface
   addition. Would need a sink to write to (CheckpointStore? new
   trait?). The plan doesn't list this as a v0.4 feature; lifting
   it from v0.3 is ~half-day of work.

3. **Leave the logger out and accept the inspector regression.**
   Decide what inspector-UI feature matters; if logger view is
   unused in practice, do nothing.

**Recommendation:** check with someone who actually uses the
inspector UI what's lost. If logger output isn't critical, option
1. If it is, option 2 needs a brief design discussion (sink
contract, formatting, etc.).

**Severity: NICE-TO-HAVE** (assuming `tracing` is sufficient).

`core/web_scrape_reactor.rs:117` is instructive: the typed reactor
already constructs `let logger = Logger::new()` locally and
discards it. Pattern documented in the file's comment block. This
suggests the rootsignal authors anticipated the loss.

### 8.5 GAP: `with_default_metadata` shape

**What rootsignal does:**
```rust
engine.with_event_metadata(serde_json::json!({"run_id": run_id, "schema_v": 1}))
```
in every engine variant.

**Why v0.4 doesn't fit:** v0.4 `with_default_metadata(Metadata)`
takes `Metadata = serde_json::Map<String, Value>` not a `Value`.
Plus the method renamed (`with_event_metadata` →
`with_default_metadata`).

**Migration option:**
```rust
let mut defaults = causal::Metadata::new();
defaults.insert("run_id".into(), serde_json::json!(run_id));
defaults.insert("schema_v".into(), serde_json::json!(1));
builder.with_default_metadata(defaults)
```
or a `metadata!` helper macro if it gets noisy.

**Severity: WORKS-BUT-AWKWARD.** Mechanical, applies to 6 sites.

### 8.6 GAP: `engine.singleton::<A>()` accessor

**What rootsignal does:** `engine.singleton::<PipelineState>()`
in tests (`testing.rs:2578`, `domains/signals/activities/engine_tests.rs:140`
and many places) reads the current PipelineState directly off the
engine for assertions.

**Why v0.4 doesn't fit:** v0.4 `Engine` doesn't expose the
AggregatorRegistry to callers. Read-side aggregate access is via
`Ctx::aggregate` inside a consumer body — there's no
out-of-band reader from outside a reactor.

**Migration options:**

1. **Add `Engine::singleton::<A>()` (and `aggregate_of`) to v0.4.**
   Small addition; the registry exists, just isn't exposed. Pure
   addition, no breakage.

2. **Switch tests to read state via a capture-projector pattern.**
   Register a small `Projector<Fact=LifecycleEvent>` that records
   `ScoutRunCompleted.stats` into a `Arc<Mutex<Option<…>>>`; tests
   read that. Less direct but doesn't need a new API.

**Recommendation:** option 1. Trivial to add and the tests want it.

**Severity: WORKS-BUT-AWKWARD** (tests-only impact; reasonable to
add the surface).

### 8.7 GAP: `AnyMaterializer` / `on_any` patterns

**What rootsignal does:**
- `core/projection.rs:54-134` — `neo4j_projection_handler` uses
  `on_any().then(move |event: AnyEvent, ctx| { … classify_event(event); })`.
  The body does runtime type classification across 12 Fact types.
- `core/projection.rs:238-260` — `run_completion_handler` uses
  `on_any()` to detect terminal events across 4 different domain
  enums.
- `core/projection.rs:655-668` — `capture_handler` (test
  capture sink) uses `on_any()` to record every event.
- `domains/enrichment/mod.rs:104-110` — `review_gate` uses
  `#[reactor(on = [SystemEvent, SignalEvent], …)]` which compiles
  into a multi-trigger reactor that takes `AnyEvent`.

**Why v0.4 doesn't fit:** v0.4 `Reactor::Trigger: Fact` — one
typed trigger per reactor. v0.4 `MultiProjector` covers the
cross-domain consumer case (used by Neo4jMaterializer already).
But MultiProjector is for *projection*, not for *reactor* — it
can't emit new events.

The legacy `on_any` pattern doesn't have a v0.4 equivalent for
reactors.

**Migration options:**

1. **Decompose `on_any` reactors into per-Fact reactors.** For
   `run_completion_reactor` this is already done — `RunCompletion`
   in `core/run_completion_reactor.rs` triggers on
   `LifecycleEvent::PhaseCompleted` only (instead of pattern-matching
   across 4 enums via `on_any`). The carrier-event refactor
   (`LifecycleEvent::PhaseCompleted` added) made this clean.

   The `review_gate` (fan-in across [SystemEvent, SignalEvent])
   needs the same treatment: introduce a carrier event, or
   register **two separate Reactor structs** (one per Fact) that
   each emit a `ReviewSubgateReady` event when their local condition
   is met, plus a third reactor that fans in on a `ReviewSubgateReady`
   that pattern-matches whether the *other* sub-gate also fired
   (via `ctx.aggregate::<PipelineState>()` state).

   Or: introduce a unified `ReviewEvent` Fact that has variants
   `SystemReviewSeen { … }` and `SignalReviewSeen { … }`; both
   SystemEvent and SignalEvent reactor handlers emit one of these;
   the gate reactor triggers on `ReviewEvent` only. This is the
   "carrier event" pattern from the v0.4 design.

   ~3-5 reactor decompositions needed across the codebase. Each
   is ~30 lines.

2. **Use `MultiProjector` for the `capture_handler` test sink.**
   Already-supported pattern (`CATEGORIES` = every category
   rootsignal uses). Body just appends to `Arc<Mutex<Vec<PersistedEvent>>>`.
   Tests then downcast from `PersistedEvent.payload` (vs. today
   downcasting from `AnyEvent`). Slight API shift in tests but the
   mechanism works.

3. **Use `MultiProjector` for `neo4j_projection_handler`.** Already
   done — `Neo4jMaterializer` is a `MultiPrefixMaterializer`. Just
   the trait rename.

**Severity: WORKS-BUT-AWKWARD for reactors; CLEAN for projections.**
The `on_any` reactor decomposition is the second-biggest design
work after on_dlq.

### 8.8 GAP: `SnapshotStore` wiring (`snapshot_every`)

**What rootsignal does:** every engine variant calls
`.snapshot_every(100)` (`core/engine.rs:156, 230, 295, 356, 417, 504`).

**Why v0.4 doesn't fit:** P9.c (snapshot_every) is deferred along
with on_dlq. SnapshotStore trait exists; the integration to
AggregatorRegistry doesn't. PostgresStore today implements
snapshot save/load via the legacy path which is going away.

**Migration options:**

1. **Land P9.c before P12.** Plan estimates ~1 day. Wires
   SnapshotStore through engine + AggregatorRegistry, makes
   `snapshot_every(N)` work.

2. **Drop snapshot_every (no-op the call).** Cold start cost goes
   up: PipelineState hydration replays the entire log per engine
   startup. For a pre-launch system with 0 events, free. For a
   post-launch system with 1M events, painful — multi-minute cold
   starts that compound across the 6 engine variants.

3. **Implement snapshot save out-of-band.** Persist PipelineState
   directly via a custom projector that triggers on a count-mod-N
   event. Decouples from causal-rs's snapshot trait. Reasonable
   but reinvents what P9.c gives you.

**Recommendation:** option 2 for pre-launch + option 1 for
post-launch. Initial migration can land without `snapshot_every`
because there's no event volume yet; add it as the volume crosses
some threshold.

**Severity: MUST-FIX-BEFORE-PRODUCTION-VOLUMES** (not for
pre-launch migration).

### 8.9 GAP: `causal_inspector` dependency

**What rootsignal does:** `rootsignal-api` mounts the inspector
UI:

- `main.rs:358-388` — constructs `causal_inspector::router(…)` and
  mounts at `/api/inspector`.
- `kernel/inspector/read_model.rs:1-347` — implements
  `causal_inspector::InspectorReadModel` against `events` /
  `causal_handler_logs` / `causal_handler_attempts` /
  `causal_handler_dependencies` etc. PG tables.
- `domains/scout/activities/inspector_display.rs` — implements
  `causal_inspector::EventDisplay` with the scout taxonomy.
- `kernel/inspector/event_broadcast.rs` (implied) — broadcasts
  inspector events via `causal_inspector::StoredEvent` over a
  tokio broadcast channel.

**Why v0.4 doesn't fit:** `causal_inspector` is **parked** during
P11.f. The crate doesn't compile against v0.4 (depends on
deleted traits like legacy `Event`, `ProjectionStore`,
`ReactorQueue`). `rootsignal-api` pins to `causal_inspector` 0.3.6
from crates.io (Cargo.toml workspace dep line 115).

So today the situation is:
- `causal` is path-pinned to v0.4 (workspace dep line 105-108).
- `causal_inspector` is version-pinned to 0.3.6 from crates.io.
- `causal_inspector` 0.3.6 depends on `causal = "0.3"`.
- → cargo will refuse to resolve once `causal` (path) hits v0.4
  because `causal_inspector` requires `causal = "0.3"`.

**This will fail to compile.** The Cargo.toml comment on line
109-114 acknowledges this is a real cost. Need to either:

1. **Pin a v0.4-compatible `causal_inspector` build.** Pre-launch
   the inspector might not be load-bearing for go-live; ship
   without it and fix post-launch.

2. **Re-instrument `causal_inspector` against v0.4.** Maps:
   `Event` → `Fact`, `ReactorQueue` → split into `CheckpointStore`
   + `ReactorOutbox`, etc. Substantial work — the inspector reads
   from `causal_handler_*` tables that change shape.

3. **Drop the inspector route entirely.** `rootsignal-api/src/main.rs`
   short-circuits the inspector mount; remove the `kernel/inspector`
   subdir and the `domains/scout/activities/inspector_display.rs`
   bits.

**Recommendation:** option 3 short-term to unblock the migration.
Option 2 post-launch if the inspector UI proves useful.

**Severity: MUST-FIX-BEFORE-MIGRATION** (compile-blocker).

### 8.10 GAP: PostgresStore's `EventLog` impl needs to migrate to `EventLogBackend`

(Discussed in §6.2.) PostgresStore's `EventLog` and `ReactorQueue`
trait impls die when P11.b/e lands. Production today doesn't go
through v03_engine, so this gap manifests only after the cutover.
Critical for the cutover commit. Likely option: delete PostgresStore's
`EventLog` impl (replaced by `causal_replay::PgEventLogBackend`),
keep its convenience methods (`has_pending_work`, etc.) as inherent
methods.

**Severity: MUST-FIX-BEFORE-MIGRATION.**

### 8.11 GAP: `with_aggregators` argument type

**What rootsignal does:**
```rust
engine.with_aggregators(pipeline_aggregators::aggregators())
```
where `pipeline_aggregators::aggregators()` is generated by
`#[aggregators(singleton)]` and returns `Vec<causal::Aggregator>`
(the v0.3 form — registry-loaded read-fold).

**v0.4:**
```rust
builder.with_aggregators([Aggregator::for_type::<A, F>(), …])
// or
builder.with_aggregators(pipeline_aggregators::aggregators())
// where the macro emits Aggregator::fold::<A, F>() per registration
```

Compatible if the macro is updated. If the macro is deleted in
P11.c, hand-rolling 14 Aggregator::fold calls is the work.

**Severity: WORKS-BUT-AWKWARD** (macro update or one-time hand-roll).

### 8.12 GAP: `causal::AnyEvent` is gone in v0.4

**What rootsignal does:** `AnyEvent` is used in
- `core/projection.rs:54-134` — `on_any().then(move |event: AnyEvent, ctx|`
- `testing.rs:1970, 2533, 2587` — test capture sinks
  (`Arc<Mutex<Vec<causal::AnyEvent>>>`).
- `domains/coalescing/tests.rs:26, 45, 52, 59, 66, 75` — test
  inspection helpers (`has_run_completed(captured: &[AnyEvent])`).

**Why v0.4 doesn't fit:** `AnyEvent` belonged to the legacy
`event` module; the trait it wrapped (`Event`) is deleted in
P11.d. v0.4 has no type-erased event wrapper for the cross-Fact
case — `PersistedEvent` (with `payload: serde_json::Value` and
typed event_type string) is the closest equivalent.

**Migration options:**

1. **Tests capture `PersistedEvent` instead of `AnyEvent`.** Use a
   `MultiProjector` whose CATEGORIES is the full set rootsignal
   uses. Body appends to `Arc<Mutex<Vec<PersistedEvent>>>`. Helper
   functions like `has_run_completed` change from
   `captured.iter().any(|e| e.downcast_ref::<LifecycleEvent>()…)` to
   `captured.iter().any(|e| e.event_type == "lifecycle:scout_run_completed")`.
   Idiomatic for v0.4. ~50 helper-function rewrites.

2. **Tests deserialize PersistedEvent.payload back to typed enums.**
   For test-helpers that want to *match on enum variants*, write
   `e.payload.deserialize::<LifecycleEvent>().ok()` per event.
   Loses compile-time type safety on the downcast but matches v0.4
   shape.

**Severity: WORKS-BUT-AWKWARD.** Test-only impact, mechanical.

### 8.13 SUMMARY: Severity-bucketed gap list

| Severity | Gap |
|---|---|
| **MUST-FIX-BEFORE-MIGRATION** | 8.1 on_dlq |
| | 8.2 `.settled()` |
| | 8.9 causal_inspector compile-blocker |
| | 8.10 PostgresStore EventLog → EventLogBackend |
| **MUST-FIX-BEFORE-PRODUCTION-VOLUMES** | 8.8 SnapshotStore (`snapshot_every`) |
| **WORKS-BUT-AWKWARD** | 8.3 Ctx has no deps / idempotency_key |
| | 8.5 with_default_metadata shape |
| | 8.6 engine.singleton accessor |
| | 8.7 on_any reactor decomposition |
| | 8.11 #[aggregators] macro update |
| | 8.12 AnyEvent removal in tests |
| **NICE-TO-HAVE** | 8.4 ctx.logger |

---

## 9. Recommended migration order

### Slice 0 — causal-rs prerequisites (before any rootsignal work)

| PR | Effort | Description |
|---|---|---|
| P9.b | S (1d) | `EngineBuilder::on_dlq(mapper)` — wires retry-budget state + terminal-failure trigger |
| (new) | S (½d) | `Engine::settle(emit_result)` — loops over consumer cursors |
| (new) | S (½d) | `Engine::singleton::<A>()` / `aggregate_of::<A>(id)` accessor |
| P9.c | S (1d) | `snapshot_every(N)` (optional pre-launch; required for post-launch volumes) |

Total: ~2 days of causal-rs work to ship the surface rootsignal
expects.

### Slice 1 — rootsignal-events.rs (P12 step 1)

**Goal:** every Fact compiles against v0.4 trait. Pure source
change in rootsignal-scout.

- `rootsignal-common/src/system_events.rs` — change `#[event(prefix="system")]` to whatever the v0.4 macro shape is (`#[fact(category="system")]` per the plan, or update the existing macro). Verify the macro emits the new shape.
- `rootsignal-common/src/telemetry_events.rs` — same
- `rootsignal-world/src/events.rs` — same
- `rootsignal-scout/src/domains/discovery/events.rs` — manual Fact impl rewrite to v0.4 shape
- `rootsignal-scout/src/domains/lifecycle/events.rs` — manual rewrite + CATEGORY rename `scout_run` → `lifecycle`
- `rootsignal-scout/src/domains/scrape/events.rs` — manual rewrite + CATEGORY rename `scout_run` → `scrape`
- `rootsignal-scout/src/domains/scheduling/events.rs` — **split** into `ScheduleEvent` (5 variants) and `ScrapeScheduleEvent` (1 variant); update emitters; manual Fact impls per new enum
- `rootsignal-scout/src/domains/signals/events.rs` — macro-emitted Fact impl; verify
- `rootsignal-scout/src/domains/synthesis/events.rs` — same
- `rootsignal-scout/src/domains/enrichment/events.rs` — same
- `rootsignal-scout/src/domains/expansion/events.rs` — same
- `rootsignal-scout/src/domains/curiosity/events.rs` — same
- `rootsignal-scout/src/core/pipeline_events.rs` — same
- `rootsignal-scout/src/domains/coalescing/events.rs` — P1.5 recipe: add `run_id: Uuid` to every variant, drop `ephemeral`, update emitter
- `rootsignal-scout/src/domains/cluster_weaving/events.rs` — same
- `rootsignal-scout/src/domains/situation_weaving/events.rs` — same
- `rootsignal-scout/src/domains/supervisor/events.rs` — same

Effort: **M (2-3 days)**. Mostly mechanical; SchedulingEvent split
is the biggest single piece.

### Slice 2 — aggregates (P12 step 2)

- `rootsignal-scout/src/core/aggregate.rs` — rewrite `Aggregate` impl to v0.4 (`const NAME`); rewrite `#[aggregators(singleton)]` body to emit 12 `Apply<F>` impls + 12 `Aggregator::fold::<…>()` registrations
- `rootsignal-scout/src/domains/curiosity/aggregates.rs` — same for SignalLifecycle + ConcernLifecycle

Effort: **S (½ day)**. Trivial if the `#[aggregators]` macro
supports v0.4; ~½ day to hand-roll if not.

### Slice 3 — materializers → projectors (P12 step 3)

For each:
- Rename `materializer.rs` → `projector.rs` (or keep file name)
- Rename impl `Materializer` → `Projector`, `materialize` → `project`
- Add `const GROUP_NAME` const
- Update file's internal docs

Files:
- `rootsignal-scout/src/core/runs_materializer.rs` → projector
- `rootsignal-scout/src/core/scheduled_scrapes_materializer.rs` — also update `type Fact = ScrapeScheduleEvent` per the split
- `rootsignal-scout/src/core/schedules_materializer.rs` — also update `type Fact = ScheduleEvent`
- `rootsignal-scout/src/core/system_log_materializer.rs`
- `rootsignal-scout/src/core/neo4j_materializer.rs` — rename `MultiPrefixMaterializer` → `MultiProjector`, update CATEGORIES (drop trailing `:`)

Effort: **S (½ day)**.

### Slice 4 — typed reactors (P12 step 4)

Existing typed reactors in `core/*_reactor.rs` (8 files). Add
`GROUP_NAME` const to each.

Effort: **S (½ day)**.

### Slice 5 — legacy reactors → typed reactors (P12 step 5)

This is the **big lift**. 30+ reactors in `domains/*/mod.rs` migrate
from `#[reactor]` macro form to `Reactor` trait impl.

Per reactor (~20-40 lines):
1. Move from `mod reactors { #[reactor(...)] async fn name(...) ... }` to a standalone struct + `impl Reactor for X`
2. Reactor struct holds `Arc<ScoutEngineDeps>` (or a domain-specific subset)
3. `react()` body checks the trigger variant via match-guard
4. `ctx.deps()` calls become `self.deps.*`
5. `ctx.logger.*` calls become `tracing::*`
6. `ctx.idempotency_key` becomes `format!("{}:{}", GROUP_NAME, ctx.event_id)`
7. The domain's `reactors()` helper returns `Vec<Box<dyn ReactorRegistration>>` (auto-impl'd for any `Reactor`)

Effort: **L (3-5 days)**. By far the most volume. Could parallelize
by domain (each domain is independent).

Strategy: write one carefully (e.g. `coalescing::coalesce`), use
it as the template, replicate.

### Slice 6 — `on_any` decomposition (P12 step 6)

The 4 `on_any` patterns:
1. `neo4j_projection_handler` (`core/projection.rs:54`) — **already
   replaced** by `Neo4jMaterializer` / `MultiProjector`. Delete
   the legacy version after Slice 5.
2. `run_completion_handler` (`core/projection.rs:238`) — **already
   replaced** by `RunCompletion` typed reactor. Delete.
3. `capture_handler` (`core/projection.rs:655`) — rewrite as a
   `MultiProjector` over all-categories. Test infra change.
4. `enrichment::review_gate` (`domains/enrichment/mod.rs:104`) —
   introduce a carrier event (or two separate reactors). Design
   call needed: option A is two reactors each emitting their
   sub-gate event, plus a third that ties them together; option B
   is a unified `ReviewGateEvent` with two variants.

Effort: **M (1 day)**.

### Slice 7 — engine wiring (P12 step 7)

- Delete `rootsignal-scout/src/core/engine.rs` (legacy `build_engine`
  family).
- Promote `rootsignal-scout/src/core/v03_engine.rs` to be the
  single engine constructor.
- Rename `V03EngineBuilder` → `ScoutEngineBuilder`.
- Add the 6 engine variants (build_scrape, build_weave, etc.) as
  methods on `ScoutEngineBuilder` or as functions taking a
  partially-configured builder.
- Each variant calls `with_default_metadata(...)`,
  `with_aggregators(pipeline_aggregators::aggregators())`,
  `with_aggregators(curiosity_aggregators::aggregators())`,
  domain reactor registrations, projector registrations, and
  `on_dlq(...)` (assuming Slice 0 P9.b landed).
- Update `rootsignal-scout/src/workflows/mod.rs` to call the new
  builder.

Effort: **M (1-2 days)**.

### Slice 8 — PostgresStore narrowing (P12 step 8)

- Delete `PostgresStore::impl EventLog` + `impl ReactorQueue`.
- Keep inherent methods: `has_pending_work`, `reclaim_stale`,
  `new()`, the `correlation_id` field.
- Move event-log writes (legacy `events` table dual-write) to
  either:
  (a) a thin wrapper around `causal_replay::PgEventLogBackend` that
  *also* writes to legacy `events` (transition layer), or
  (b) a separate writer that lives outside causal-rs and is called
  by application code, not by the engine.
- Update `rootsignal-api/src/domains/scout/activities/runner.rs:19`
  to import the right traits (or not — depends on what survives
  on PostgresStore).

Effort: **S-M (½-1 day)** depending on how clean the dual-write
strategy can be.

### Slice 9 — tests (P12 step 9)

- 13 `tests/v03_*.rs` files: remove `ephemeral: None, persistent: true`
  from every NewEvent construction; rename
  `causal::event_log::EventLog` import to `EventLogBackend`.
- `testing.rs`: replace `Arc<Mutex<Vec<AnyEvent>>>` captures with
  `Arc<Mutex<Vec<PersistedEvent>>>` + helper rewrites.
- `domains/*/tests.rs` files: rewrite helper functions
  (`has_run_completed` etc.) to match on `event_type` strings
  instead of downcast.
- Replace `.settled()` calls — if Slice 0 added `.settle()`, just
  rename; if not, replace each with the equivalent cursor-wait
  loop.

Effort: **M-L (2-3 days)** because there are many call sites.

### Slice 10 — inspector (P12 step 10)

Decide:
- Drop the inspector route entirely (option A).
- Or re-instrument `causal_inspector` against v0.4 in parallel
  (option B, separate workstream).

For initial migration: option A.

- Delete `rootsignal-api/src/kernel/inspector/` directory.
- Delete `rootsignal-api/src/domains/scout/activities/inspector_display.rs`.
- Remove `causal_inspector` from `rootsignal-api/Cargo.toml`.
- Remove the inspector router mount in `main.rs:358-388, 567-576`.

Effort: **S (½ day)**.

### Slice 11 — data migration (P0 runbook execution)

Already covered exhaustively in
`docs/migrations/rootsignal-v04-runbook.md`. Pre-launch: this is
trivial because there's no production data.

Effort: **S (½ day)** in pre-launch; **M (1 day + downtime
window)** post-launch.

### Total estimated effort

| Slice | Effort | Cumulative |
|---|---|---|
| 0 — causal-rs prereqs | 2d | 2d |
| 1 — events.rs | 2-3d | 4-5d |
| 2 — aggregates | ½d | ~5d |
| 3 — projectors | ½d | ~5.5d |
| 4 — typed reactors | ½d | ~6d |
| 5 — legacy reactors | 3-5d | 9-11d |
| 6 — on_any decomp | 1d | 10-12d |
| 7 — engine wiring | 1-2d | 11-14d |
| 8 — PostgresStore | ½-1d | 12-15d |
| 9 — tests | 2-3d | 14-18d |
| 10 — inspector | ½d | 14.5-18.5d |
| 11 — data migration | ½d | 15-19d |

**Range: ~3-4 weeks of focused work** for one engineer. Plan's
estimate of "5-7 days" for P12 (line 1068) was pre-this-audit;
the real surface is bigger because of the legacy-reactor mass.

### Smallest meaningful first slice

**Slice 0 + Slice 1 + Slice 2 + a single domain's Slice 3+4+5** —
get one event-aggregate-reactor chain working end-to-end against
v0.4 (recommend: lifecycle + signals + scrape, the scrape chain
core). This is the "vertical slice" that proves the migration
shape works. Builds the muscle for the broader rollout.

Approximate: ~5-7 days of work, gets a runnable
`build_scrape_engine`-equivalent against v0.4 with the bare
minimum domains wired.

---

## 10. Risks / open questions

### 10.1 Open: How does the `#[aggregators]` macro evolve under v0.4?

The runbook and implementation plan disagree slightly:
- Plan P11.c (line 894-908): "Update the macros (`#[reactor]`,
  `#[reactors]`, `#[projection]`) to drop legacy-Engine emission
  OR delete the macros entirely if their only consumers were the
  deleted tests"
- lib.rs (`modules/causal/src/lib.rs:147-150`): "`#[event]` and
  `#[aggregators]` (plural) survive — they emit v0.4-compatible
  code."

So `#[aggregators]` plural survives. But: does it support both
`#[aggregators(singleton)]` (the pipeline_aggregators shape) and
the keyed shape (`#[aggregator(id_fn="...")]` inside)? Today
the keyed variant uses `id_fn = "lifecycle_signal_id"` to extract
the aggregate-id from a non-Uuid event. Under v0.4 with
`Apply<F>` per-trait-impl, the id-extraction lives in `Fact::stream_id()`.

**Decision needed:** does P12 require updating the macro, or
hand-rolling all 14 Apply impls? Either path works; a one-time
afternoon updating the macro is probably cleaner.

### 10.2 Open: Stream-layout change cost

`scout_run-{run_id}` → `lifecycle-{run_id}` + `scrape-{run_id}`
means PipelineState is now folded from TWO logical streams instead
of ONE. As long as the read-fold approach (`Aggregator::fold`) sees
events in log-order (which it does — runners read globally), this
is fine for aggregator state.

**But:** any code that does `engine.load::<PipelineState, LifecycleEvent>(run_id)`
or `engine.load::<PipelineState, ScrapeEvent>(run_id)` (the per-stream
fold API) would see different views. Searched the codebase: zero
uses of `engine.load`. So this is purely a Kurrent-future
consideration; pre-launch rootsignal isn't affected.

**Recommendation:** document this as a future cost but don't gate
the migration on it.

### 10.3 Open: Where does PipelineState's "run_id" live?

PipelineState today is a singleton per engine (`Uuid::nil()` key).
Each engine variant scopes itself to one `run_id` via
`ScoutEngineDeps::run_id`. So one engine instance = one run_id =
one PipelineState fold.

Under v0.4 if the engine is long-lived across runs (which is the
direction `EngineBuilder` pushes), PipelineState becomes ambiguous
— which run's events does it reflect?

**Two options:**
- **Option A: keep one engine per run.** Today's model. Each
  `run_*` GraphQL mutation spawns a fresh engine via `ScoutDeps::build_*_engine`.
  Compatible with v0.4 as long as engine construction is cheap
  enough.
- **Option B: one persistent engine + PipelineState keyed by run_id.**
  Larger refactor. PipelineState becomes a keyed (non-singleton)
  aggregate. Every reactor that reads it via `ctx.aggregate::<PipelineState>()`
  switches to `ctx.aggregate_of::<PipelineState>(run_id)`.

The plan implies Option A (engine = one-per-run, short-lived).
Sticks with current model. **Confirm with someone who knows the
production deployment pattern.**

### 10.4 Open: Is `causal_inspector` worth re-instrumenting?

Pre-migration the inspector mount is the **#1 compile-blocker**.
Going dark on the inspector is fine pre-launch — but if the
inspector UI is a primary debugging tool for the team during
launch, the loss matters.

**Recommendation:** ship without it for v0.4 migration. Bring it
back as a separate workstream after launch stabilizes. Note that
its read model
(`rootsignal-api/src/kernel/inspector/read_model.rs`) reads from
the legacy `events` table — which the dual-write keeps populating —
so the *data* the inspector needs is still there; only the UI
crate doesn't compile.

### 10.5 Open: The `core/projection.rs` removal also removes the
legacy SystemLog stdout printer behavior

Today `system_log_projection` (`core/projection.rs:448-459`) prints
`TelemetryEvent::SystemLog` payloads to `tracing::info!`. The v0.3
typed `SystemLogMaterializer` should do the same thing — verify it
does, and if not, port the body.

### 10.6 Open: Reactor `on = [Fact1, Fact2]` multi-trigger

`domains/enrichment/mod.rs:104` registers a reactor that triggers
on TWO different Fact types: `on = [SystemEvent, SignalEvent]`.
This is the `review_gate` fan-in. v0.4 `Reactor::Trigger: Fact`
allows only ONE.

Already enumerated in §8.7 (option: carrier event). **Needs design
call.** Easiest: introduce `ReviewEvent` Fact carrying both signals
(one variant per source). Both SystemEvent and SignalEvent reactor
chains emit the appropriate `ReviewEvent::*` variant; `review_gate`
triggers on `ReviewEvent`.

### 10.7 Watch out: the `seesaw_*` and `causal_*` Postgres tables

Rootsignal's Postgres schema includes:
- `events` (legacy seesaw store) — written by PostgresStore today
- `causal_log` (v0.3 mirror) — written by PostgresStore's dual-write
- `causal_checkpoints` (v0.3 cursors) — written by `PgReactorOutbox`
- `causal_reactor_outbox` (v0.3 reactor intent) — written by `PgReactorOutbox`
- `causal_handler_logs` etc. (inspector tables) — written by legacy framework

Under v0.4 the canonical event log is `causal_log` (read by
`PgEventLogBackend`). The legacy `events` table is for inspector
read + transition. The migration removes the dual-write
eventually; until then, schema doesn't change.

`causal_handler_*` tables are written by legacy framework
internals. Under v0.4 they may stop getting populated (inspector
goes dark). That's acceptable per §10.4.

### 10.8 Watch out: `causal_replay` is a separate crate that must move in lockstep

`rootsignal-api/src/kernel/graph_projection.rs:20` uses
`causal_replay::{PgNotifyTailSource, PgPointerStore, PointerStore, ProjectionStream}`.
`rootsignal-scout/src/core/v03_engine.rs:33` uses
`causal_replay::{PgEventLogBackend, PgReactorOutbox}`.

`causal_replay` is pinned to the local path in `Cargo.toml` so it
moves with `causal`. The plan's P13 release sequence (line 1084-1090)
publishes all four causal crates together. As long as
`causal_replay` v0.4 is in path, this is fine.

But — `causal_replay` itself depends on `causal` and its surface
mirrors causal's. If it lags behind causal-rs v0.4 surface
changes, rootsignal-api breaks. **Verify causal_replay compiles
against current causal v0.4** before kicking off rootsignal P12.

---

## Appendix A: file:line index of load-bearing call sites

For ctrl-F efficiency during migration:

### Fact impls
- `rootsignal-scout/src/domains/scheduling/events.rs:95` — SchedulingEvent (manual; SPLITTING)
- `rootsignal-scout/src/domains/lifecycle/events.rs:174` — LifecycleEvent (manual)
- `rootsignal-scout/src/domains/scrape/events.rs:100` — ScrapeEvent (manual)
- `rootsignal-scout/src/domains/discovery/events.rs:53` — DiscoveryEvent (manual)
- `rootsignal-common/src/system_events.rs:31` — SystemEvent (macro)
- `rootsignal-common/src/telemetry_events.rs:13` — TelemetryEvent (macro)
- `rootsignal-world/src/events.rs:16` — WorldEvent (macro)

### Aggregate impls
- `rootsignal-scout/src/core/aggregate.rs:553` — PipelineState
- `rootsignal-scout/src/core/aggregate.rs:559` — `#[aggregators(singleton)]` block
- `rootsignal-scout/src/domains/curiosity/aggregates.rs:13` — SignalLifecycle
- `rootsignal-scout/src/domains/curiosity/aggregates.rs:26` — ConcernLifecycle
- `rootsignal-scout/src/domains/curiosity/aggregates.rs:33` — `#[aggregators]` block

### Materializers (already typed v0.3 form)
- `rootsignal-scout/src/core/runs_materializer.rs:38`
- `rootsignal-scout/src/core/scheduled_scrapes_materializer.rs:37`
- `rootsignal-scout/src/core/schedules_materializer.rs`
- `rootsignal-scout/src/core/system_log_materializer.rs`
- `rootsignal-scout/src/core/neo4j_materializer.rs:64`

### Typed reactors (already v0.3 form)
- `rootsignal-scout/src/core/web_scrape_reactor.rs:67`
- `rootsignal-scout/src/core/social_scrape_reactor.rs`
- `rootsignal-scout/src/core/process_web_results_reactor.rs`
- `rootsignal-scout/src/core/process_social_results_reactor.rs`
- `rootsignal-scout/src/core/topic_discovery_reactor.rs`
- `rootsignal-scout/src/core/bootstrap_sources_reactor.rs`
- `rootsignal-scout/src/core/filter_domains_reactor.rs`
- `rootsignal-scout/src/core/run_completion_reactor.rs:60`

### Legacy reactor declarations (`#[reactor]` form, need rewrite)
- `rootsignal-scout/src/domains/news_scanning/mod.rs:18`
- `rootsignal-scout/src/domains/coalescing/mod.rs:81-90` (and 2-4 more in same file)
- `rootsignal-scout/src/domains/cluster_weaving/mod.rs:18`
- `rootsignal-scout/src/domains/signals/mod.rs:39`
- `rootsignal-scout/src/domains/discovery/mod.rs` (multiple)
- `rootsignal-scout/src/domains/lifecycle/mod.rs:24, 38`
- `rootsignal-scout/src/domains/synthesis/mod.rs` (3 reactors)
- `rootsignal-scout/src/domains/enrichment/mod.rs:97-110, 116` (multi-trigger one is :104)
- `rootsignal-scout/src/domains/expansion/mod.rs` (1 reactor)
- `rootsignal-scout/src/domains/curiosity/mod.rs:48-75` (uses aggregate_of)
- `rootsignal-scout/src/domains/situation_weaving/mod.rs`
- `rootsignal-scout/src/domains/supervisor/mod.rs`

### Engine construction
- `rootsignal-scout/src/core/engine.rs:110` — build_engine (scrape) [LEGACY, DELETING]
- `rootsignal-scout/src/core/engine.rs:193` — build_weave_engine [LEGACY]
- `rootsignal-scout/src/core/engine.rs:259` — build_coalesce_engine [LEGACY]
- `rootsignal-scout/src/core/engine.rs:320` — build_cluster_weave_engine [LEGACY]
- `rootsignal-scout/src/core/engine.rs:381` — build_feed_group_engine [LEGACY]
- `rootsignal-scout/src/core/engine.rs:442` — build_infra_only_engine [LEGACY]
- `rootsignal-scout/src/core/engine.rs:475` — build_news_engine [LEGACY]
- `rootsignal-scout/src/core/v03_engine.rs:94` — V03EngineBuilder::build [v0.4 target — generalize]
- `rootsignal-scout/src/workflows/mod.rs:248` — build_infra_engine [LEGACY]

### Custom backend
- `rootsignal-scout/src/core/postgres_store.rs:54` — `impl EventLog for PostgresStore`
- `rootsignal-scout/src/core/postgres_store.rs:~600` — `impl ReactorQueue for PostgresStore` (estimated)

### Inspector mount
- `rootsignal-api/src/main.rs:358-388` — inspector router construction
- `rootsignal-api/src/main.rs:567-576` — admin_router.nest("/api/inspector", …)
- `rootsignal-api/src/kernel/inspector/read_model.rs:1-347` — PgInspectorReadModel
- `rootsignal-api/src/domains/scout/activities/inspector_display.rs:1` — EventDisplay impl

### causal_replay usage
- `rootsignal-scout/src/core/v03_engine.rs:33` — PgEventLogBackend, PgReactorOutbox
- `rootsignal-api/src/kernel/graph_projection.rs:20` — PgNotifyTailSource, PgPointerStore, ProjectionStream

### `on_dlq` mapper registrations
- `rootsignal-scout/src/core/engine.rs:142, 216, 281, 342, 403, 506` (six call sites; six engine variants)

### `.settled()` usage
- `rootsignal-api/src/domains/scout/activities/runner.rs:56` — production
- `rootsignal-scout/src/testing.rs:2566` — tests
- `rootsignal-scout/src/domains/coalescing/tests.rs:109, 146, 183, 224, 298, 352` — tests
- `rootsignal-scout/src/domains/signals/activities/engine_tests.rs` — ~30 sites

---

End of audit.
