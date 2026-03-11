# Brainstorm: Visualization Inspiration from FBP (n8n, NofLo, Temporal, Restate)

**Date:** 2026-03-11
**Context:** Exploring what seesaw's CausalFlowPane can learn from flow-based programming tools and durable execution UIs

## Current State

Seesaw already has two graph visualizations in the rootsignal admin app:

1. **CausalFlowPane** — ReactFlow + dagre DAG showing the `Event → Handler → Event` causal tree for a specific run. Event-type nodes grouped by `(handlerId, name)`, handler nodes with live status (pending/running/done/error), causal chain highlighting, handler `describe()` blocks rendered inline (progress bars, checklists, counters, status indicators), duration badges, handler filtering. Polls every 5s.

2. **ForceGraph** — d3-force canvas renderer for the Neo4j domain knowledge graph (Gatherings, Resources, Actors, Locations). Separate concern from seesaw flow visualization.

The `describe()` → Block DSL → visual node pattern is the unique primitive. Handlers self-report their state into the graph. Everything below extends what that primitive can do.

## Comparison: Where Seesaw Sits

|  | n8n / NofLo | Temporal | Restate | Seesaw |
|---|---|---|---|---|
| **Graph type** | Static wiring (design-time) | Linear timeline (event list) | Journal list | Causal DAG (runtime) |
| **Data inspection** | Click any edge to see payload | Click event for details | Click invocation for journal | Counts only (today) |
| **Replay** | Pin data + re-run downstream | Replay from event history | Re-invoke from journal | Event sourcing (infrastructure exists, no UI) |
| **Self-reporting nodes** | No | No | No | Yes (describe blocks) |
| **State visualization** | No domain state | Workflow state only | Handler state only | Aggregates (not visualized yet) |

## Ideas (ordered by impact vs effort)

### 1. Time Scrubber — Replay the Graph Growing

The causal flow graph is built from `flowData` (array of `AdminEvent`). Today it renders the final state. Add a timeline scrubber at the bottom — drag left and the graph shrinks back in time, showing only events up to that point. Handlers pulse as they activate. Events appear as they're emitted.

Temporal's timeline view shows the *unfolding* of execution, but as a linear list. This would be an animated DAG — nobody has that.

**Implementation sketch:** Filter `flowData` by `seq <= scrubberPosition` before passing to `buildFlowGraph()`. Add a `<input type="range">` at the bottom. Animate with `requestAnimationFrame` for auto-play mode.

**Why it matters:** Event sourcing's promise is "replay the past." This makes that promise visual.

### 2. Data Inspection on Nodes/Edges (n8n's Killer Feature)

n8n's best UX: click any connection and see the data that flowed through it — table view, JSON view, schema view.

Event nodes already show counts (`x47`). Clicking one should expand a detail drawer showing actual event payloads. Clicking a handler node should show input event, output events, journal entries, logs, duration, retry history.

**Implementation sketch:** Add a side panel / drawer component. On node click, query `adminEventPayloads(groupKey)` via GraphQL. Render with tabs: Table | JSON | Schema (inferred from first payload).

**Why it matters:** Highest immediate value. The data is already persisted — just needs a window into it.

### 3. Static Topology View (NofLo's Contribution)

Today the graph requires a specific run to render. NofLo's insight: the *static wiring* is also valuable. A companion view showing all registered handlers, event types they listen to, and event types they can emit — derived from the handler registry at startup, no run needed.

The `#[handles]` macro already knows input/output types at compile time. The `HandlerRegistry` has the runtime information.

**Implementation sketch:** Two tabs in the flow pane — **Topology** (static, what *can* happen) and **Flow** (runtime, what *did* happen). Topology derived from a new `GET /api/admin/topology` endpoint that serializes the handler registry.

**Why it matters:** The "architecture diagram that's always up to date" because it's generated from code. Teams would use this daily even outside debugging.

### 4. Aggregate State Timeline (Unique to Event Sourcing)

Nobody else has this because nobody else has aggregates. Show the aggregate's state evolving alongside the causal graph.

Combined with the time scrubber (#1): as you scrub, the aggregate panel shows state at that point. Click an event node → see aggregate state *before* and *after* that event was applied.

Temporal shows event history. Restate shows journal entries. Neither shows *domain state evolution*.

**Implementation sketch:** New pane or sidebar showing aggregate JSON. Query `load_stream(aggregate_type, aggregate_id)` and fold events up to the scrubber position. Diff view (before/after) using a JSON diff component.

**Why it matters:** This is the "time travel debugging" that event sourcing promises but nobody visualizes.

### 5. Pin & Replay (n8n's Data Pinning for Event Sourcing)

n8n lets you freeze a node's output and re-run downstream with frozen data. For seesaw: pick any event in the graph, "pin" it, and re-run just the downstream handlers from that point.

Seesaw already has `ctx.run()` journaling and event replay infrastructure.

**Implementation sketch:** Right-click event node → "Replay from here." Backend creates a new correlation with the pinned event as root, runs settlement. New run appears in the flow pane.

**Why it matters:** Development superpower. "This handler failed on event #47 — pin it, tweak handler code, re-run just that subtree."

### 6. Handler Heat Map / Throughput Overlay

Color-code handler nodes by performance: green (fast), yellow (slow), red (erroring). Size event-type nodes by volume. Animate edge thickness by throughput.

The describe blocks already push progress data into nodes — extend that to visual encoding on the graph itself.

NofLo's FBP inspiration article references "packets moving through running systems, similar to control panels in industrial settings."

**Implementation sketch:** Compute `avg_duration`, `error_rate`, `throughput` per handler from `adminHandlerOutcomes`. Map to HSL color scale. Apply as node background gradient or border glow.

**Why it matters:** Turns the flow graph into a live operational dashboard.

### 7. Diff Two Runs

Side-by-side or overlay view: pick run A and run B, see where they diverge. Same topology, different execution paths highlighted in different colors.

**Implementation sketch:** Load two `flowData` arrays. Union the nodes, color edges by which run they belong to (blue = run A, orange = run B, green = both). Divergence points get a visual marker.

**Why it matters:** "Why did Tuesday's run fail but Monday's didn't?" becomes a visual question.

### 8. Subgraph Collapse (NofLo's Composability)

NofLo's subgraphs let you collapse a sub-network into a single node. The handler filter dropdown already does a version of this (hiding handlers). But a proper "group these N handlers into a named subgraph" with expand/collapse would help at scale. Double-click to zoom in, like a filesystem.

**Implementation sketch:** Let users define named groups in handler registration (e.g., `#[handles(group = "scraping")]`). Render collapsed groups as a single meta-node with aggregate stats. Click to expand inline.

**Why it matters:** Scale. When you have 30+ handlers, the graph needs hierarchy.

### 9. Sticky Notes / Annotations (n8n's Simplest Good Idea)

Markdown-formatted annotations on the canvas. Drop a note explaining *why* a handler exists or *what to watch for*. Persisted per-run or per-topology.

**Implementation sketch:** Add a "note" node type to ReactFlow. Persist in a `seesaw_flow_annotations` table keyed by `(run_id | "topology", position_x, position_y, content)`.

**Why it matters:** Low effort, surprisingly high value for team communication.

## Priority Recommendation

If picking three to build first:

1. **Time scrubber** — nobody has animated DAG replay, and the data model already supports it
2. **Data inspection on nodes** — highest immediate value, all data already persisted
3. **Static topology view** — "always-accurate architecture diagram" used daily outside debugging

## Lineage

The deeper lineage: **FBP** (Morrison) → **Actor model** (Hewitt) → **Event sourcing** (Fowler) → **Durable execution** (Temporal) → Seesaw takes the event sourcing + durable execution branch but keeps FBP-style "compose by wiring reactive nodes" ergonomics.

The n8n/NofLo vibe is real at the topological level — all are "data flows through a graph of processing nodes." But seesaw diverges in having persistent event logs, aggregates, journaled side effects, and settlement. The visualization should lean into those differences, not imitate the FBP tools.

The `describe()` Block DSL is the unique primitive that none of the others have. Every idea above extends what that primitive can do.

## Sources

- [NofLo FBP UI Inspiration (Henri Bergius)](https://bergie.iki.fi/blog/inspiration-for-fbp-ui/)
- [n8n Data Pinning Docs](https://docs.n8n.io/data/data-pinning/)
- [n8n Debug & Replay Docs](https://docs.n8n.io/workflows/executions/debug/)
- [Temporal Timeline View Blog](https://temporal.io/blog/lets-visualize-a-workflow)
- [Restate UI Announcement](https://www.restate.dev/blog/announcing-restate-ui)
- [NofLo Project](https://noflojs.org/)
- [n8n Sticky Notes Docs](https://docs.n8n.io/workflows/components/sticky-notes/)
