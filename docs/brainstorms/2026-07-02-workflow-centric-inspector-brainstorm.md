# Workflow-Centric Inspector — Brainstorm

**Date:** 2026-07-02
**Status:** Design pressure-tested against code + external DX research
**Depends on:** `2026-07-01-workflow-topology-brainstorm.md` (the pure
`causal_utils::topology` module, shipping in 0.17.2)

## What We're Building

Reorient the inspector from "a viewer for the event log" to "a viewer for what
the system does" by making workflows the navigation spine:

1. **Workflow catalog** — distinct root event types (no `causation_id`), derived
   from the log: run count, last seen. Zero registration; a workflow appears the
   first time it ever runs.
2. **Workflow page** — the **observed topology** (aggregate graph: event-type
   nodes, parent→child edges with counts) above a **runs list** scoped to that
   root type.
3. **Run page** — the existing per-run causal flow, with its path overlaid on the
   topology graph.

The inspector is **headless** (`causal_inspector` = GraphQL API + WS subscriptions
as a nestable axum router); the UI lives in the consumer (rootsignal frontend).
So causal-rs ships queries; consumers ship screens.

## What Already Exists (found 2026-07-01)

- `inspector_workflows(search, limit, cursor)` → paginated runs with
  `root_event_type`, `event_count`, timestamps, `has_errors`
  (`causal_inspector/src/graphql.rs:290`).
- `inspector_causal_flow(workflow_id)` → all events for one run.
- `inspector_causal_tree(seq)` → single-event lineage.
- `inspector_reactor_dependencies` → *declared* reactor topology.

Net-new is only the catalog level and the aggregate topology.

## The Three Additive Changes (causal-rs side)

1. **`inspector_workflow_types`** — catalog query: distinct `root_event_type`
   over root events, with run count + last-seen. O(root events), not O(log).
2. **`inspector_topology(root_event_type, limit_runs?)`** — two indexed SQL
   steps (find matching correlation_ids → fetch `(event_id, causation_id,
   event_type)` rows), then `causal_utils::extract_topology` in Rust. SQL never
   aggregates — the collapse logic lives once.
3. **`root_event_type` filter param** on existing `inspector_workflows`.

Plus GraphQL types mirroring `Topology` (defined in `causal_inspector`, so
`causal_utils` stays free of `async-graphql`), and both new read-model methods
implemented for the `memory` feature so dev-mode DX matches prod.

## Pressure-Test Amendments (2026-07-02)

- **Partial index for roots:** the existing causation index excludes the
  NULL-causation rows the catalog groups by. Add
  `CREATE INDEX ... ON causal_log (event_type) WHERE causation_id IS NULL`.
- **Sample runs, not events:** `inspector_topology` bounds input by sampling the
  most recent K runs (default ~200) but fetches each sampled run *completely*.
  Whole trees keep `orphans` a real integrity signal, not a windowing artifact.
  Counts mean "within sampled runs" — say so on the GraphQL field.
- **Default trait impls:** `InspectorReadModel` is public with external
  implementors possible; the two new methods need default implementations to
  stay patch-additive.
- **`(unknown)` catalog bucket:** correlations can lack any parentless event
  (existing SQL COALESCEs root_event_type to `''`). Surface these explicitly
  rather than as an empty-string workflow type.
- **Exclude the zero-UUID sentinel correlation**, matching existing queries.
- **Inherited scale debt (flagged, not fixed):** `list_workflows` full-scans and
  groups the entire log per page load
  (`causal_replay/src/inspector_read_model.rs:379`). The catalog family will
  eventually want a summary/materialized read model. Known, deferred.

## DX Research: Alignment with Temporal / Replay / Inngest / Restate / Step Functions

Researched 2026-07-02. The closest disciplines: **AWS Step Functions** (graph +
per-execution path overlay) for the interaction model, and **process mining**
(directly-follows graphs mined from event logs — Disco/Celonis/PM4Py) for the
aggregate topology, which is isomorphic to our `(root, event_type, causal edge)`
data.

Patterns adopted (most are frontend guidance for consumers, recorded here so the
API doesn't preclude them):

1. **Stable frame + run coloring (Step Functions):** users learn the topology
   shape once, then read runs as colorings of it — traversed/failed/not-taken,
   click a node → its event payload. Overlay must be **bidirectional** (select
   event ↔ highlight node).
2. **The overlay never lies (process mining):** a run can traverse an edge the
   sampled aggregate doesn't show. Always render the run's actual path, adding
   ghost nodes/edges to the frame as needed. Enabled cheaply: run
   `extract_topology` over the single run's events for its exact edge set.
3. **Frequency-annotated edges + threshold slider (process mining):** edge
   thickness ∝ count, slider hides sub-N% edges to keep mined graphs readable.
   This is why `count` stays in the API. Caution from the DFG literature:
   frequency-filtered graphs can mislead; the per-run overlay is ground truth,
   the aggregate is a map.
4. **Query-language filtering (Temporal) / SQL-as-substrate (Restate):** typed
   predicate filtering over runs beats checkbox widgets; keep every screen
   expressible as an API query (already true — the inspector is headless
   GraphQL, so agents and scripts get parity with the UI for free).
5. **Master-detail run view (Inngest/Restate):** DAG/timeline left, selected
   node's payload/error right; never navigate away to see a payload.
6. **Failure-first affordances:** one click from runs list to "where did this
   run die" (red node → cause). `has_errors` exists; a per-run "first failed
   event" pointer is a candidate follow-up field.
7. **Repetition collapse (Temporal):** fan-out of N identical children collapses
   to "×N, expand" in run views — essential past ~50 events per run.

Anti-patterns to avoid (from the same research):

- Unbounded, cursor-less lists (Temporal at volume) — keyset pagination exists;
  keep it.
- Status taxonomy sprawl (Temporal's 7 statuses confuse) — keep run status
  minimal and behavior-defined; if a workflow spawns another, make the linkage a
  navigation edge, not a status.
- Raw event dump as landing view — full history is the escape hatch, not the
  default (Temporal's own redesign lesson).
- Aggregate numbers without drill-through — any count on the topology must click
  through to contributing runs, or users won't trust it.

## Future (explicitly out of scope now)

- **Variant analysis (process mining's middle layer):** cluster runs by path
  signature — "variant A: 62% of runs, variant B: 21%" — between topology and
  runs list. Per-run signatures already derivable via `extract_topology`.
- **Declared vs. observed diff:** `ReactorDependencyEntry` (declared) vs. the
  mined topology (observed) → dead reactors, never-taken branches.
- **Focus window (Replay.io):** one global time/sequence range scoping every
  panel at once.
- **Cross-workflow navigation:** terminal fact of one graph = root of another;
  the deliberate chain-severing becomes a link, not a dead end.
- Summary/materialized read model for the catalog query family.

## Sequencing

1. **0.17.2** — pure `causal_utils::topology` module (other brainstorm).
   Unblocks the rootsignal test gate immediately.
2. **Follow-up causal-rs release** — the three GraphQL additions + partial
   index + memory-feature impls.
3. **rootsignal frontend** — catalog/workflow/run screens, on its own timeline.
