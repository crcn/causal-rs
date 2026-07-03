# Workflow Topology Extraction — Brainstorm

**Date:** 2026-07-01
**Status:** Design pressure-tested, ready for planning

## What We're Building

A pure topology module in `causal_utils` that collapses persisted event envelopes
into deduplicated parent→child event-type edges, plus a deterministic mermaid
renderer. The causal log already *is* the workflow topology: every persisted
envelope carries `causation_id` + `event_type`, so collapsing a workflow's events
to distinct `(parent.event_type → child.event_type)` edges yields the true
event-flow graph — derived from actual behavior, can't lie, can't rot.

```rust
pub struct EnvelopeMeta {
    pub event_id: Uuid,
    pub causation_id: Option<Uuid>,   // codebase vocabulary — NOT parent_id
    pub event_type: String,
}

pub struct TopologyEdge { pub from: String, pub to: String, pub count: u64 }

pub struct Topology {
    pub edges: Vec<TopologyEdge>,     // sorted (from, to)
    pub roots: Vec<String>,           // parentless event types
    pub orphans: Vec<String>,         // causation_id resolves to nothing in the input set
}

pub fn extract_topology(events: impl IntoIterator<Item = EnvelopeMeta>) -> Topology;
pub fn to_mermaid(root: &str, topology: &Topology) -> String;
```

No I/O in the module. Two consumers:

1. **rootsignal test gate** — integration tests run each workflow root through the
   real engine against `MemoryStore`, capture via `store.global_log().to_vec()`,
   extract + render, diff against checked-in `docs/architecture/topology/<workflow>.md`.
   Rewiring a reactor fails the test; the regenerated diagram is the PR diff artifact.
2. **causal_inspector view (follow-up)** — GraphQL queries over the same
   `extract_topology`, as the first step toward a workflow-centric inspector.
   Design lives in its own brainstorm:
   `docs/brainstorms/2026-07-02-workflow-centric-inspector-brainstorm.md`.

## Why This Approach

- **Log-derived beats static extraction.** Several rootsignal reactors are
  macro-generated (`type Trigger = $trigger`) and emissions are conditional —
  static analysis is a dead end. The log records what actually happened.
- **App-agnostic, so it belongs in causal-rs.** Touches only causal's own envelope
  fields (`event_id`, `causation_id`, `event_type`); nothing consumer-specific.
- **Plain struct, no new dependencies.** `causal_utils` does not depend on `causal`
  today and doesn't need to: `EnvelopeMeta` is a trivial field-copy from both
  `RecordedEvent` (`modules/causal/src/types.rs`) and the inspector's `StoredEvent`
  (`modules/causal_inspector/src/read_model.rs`). Only a `uuid` dep is added.
- **One collapsing implementation.** The inspector fetches rows and calls
  `extract_topology` in Rust rather than doing a SQL self-join — the collapse
  logic lives once and can't drift between consumers.
- **CLI deprioritized.** The inspector is already the "look at what the log did"
  surface; a CLI would be a thin wrapper over the same two functions.

## Key Decisions (from pressure test)

1. **Return `Topology { edges, roots, orphans }`, not bare `Vec<TopologyEdge>`.**
   Orphans (children whose `causation_id` resolves to nothing in the input set) are
   otherwise invisible — hiding exactly the lost-parentage bugs and
   windowed-query boundary effects the tool should surface. Painful to retrofit
   into checked-in artifacts later; nearly free now.
2. **Caller pre-filters to one workflow — explicit contract.** `EnvelopeMeta`
   carries no `workflow_id`, so the module *cannot* partition; both consumers
   already filter naturally (single-workflow test run; SQL `correlation_id` filter).
   `to_mermaid`'s `root` is a label/entry-node marker only — it renders all edges
   and does **not** filter by reachability (silent dropping would hide orphan-class bugs).
3. **Determinism is mandatory for the gate.** Edges sorted lexicographically by
   `(from, to)`; no HashMap iteration order in output. `count` stays on
   `TopologyEdge` (inspector wants it, it's free) but is **omitted from default
   mermaid output** — nondeterministic test-data volume must not flake the gate.
4. **Two-pass extraction, honest signature.** Children carry `causation_id`, not
   the parent's type — build an `event_id → event_type` map first, then resolve
   edges. Input is `IntoIterator`, buffered internally; not streaming.
5. **Sanitize mermaid node ids.** Event types like `CATEGORY:name` aren't valid
   mermaid identifiers — use generated ids with quoted labels (`n0["scan:SourceScanRequested"]`).
6. **No DAG assumption.** Retry/poll reactors legally produce self-loops and
   cycles; nothing may topologically sort.
7. **Release as 0.18.1, all workspace crates aligned.** Additive-in-patch has
   precedent (0.7.4, 0.17.1); every crate publishes at the same version per
   workspace convention.

## Known Boundaries (features, not bugs)

- **Workflow-root events sever the causal chain by design** — each diagram is
  self-contained per workflow root. Cross-workflow handoffs appear as a terminal
  fact in one graph and a root in another.
- **The gate documents observed-under-test topology, not total topology.** A
  conditional branch tests never exercise silently vanishes from the checked-in
  diagram — and the gate passes. Removing an exercised edge fails the diff; adding
  an untested branch changes nothing. The generated artifact should carry a header
  stating "observed topology under test"; exercising branches is rootsignal's
  test-suite burden, not this module's.

## Open Questions

- **Header/preamble format** for the generated markdown artifact (title, caveat
  line, mermaid fence) — settle during planning; must itself be deterministic.
- **Mermaid vs. structured output split (resolved):** `to_mermaid` serves the
  checked-in test-gate artifact only. The inspector consumes `Topology` as
  structured data (GraphQL types) and renders its own interactive graph — no
  counts-in-mermaid option needed. `count` stays on `TopologyEdge`; process-mining
  research (see inspector brainstorm) confirms frequency-annotated edges are the
  core affordance of observed-topology UIs.
- **Per-run path signatures come free:** running `extract_topology` over a single
  run's events yields that run's edge set — the basis for path overlay and variant
  analysis in the inspector. No API change needed; worth a unit test asserting the
  single-run case behaves.
- **Declared vs. observed cross-check (future):** the inspector already exposes a
  *declared* reactor topology (`ReactorDependencyEntry.input_event_types` /
  `output_event_types`). Diffing declared vs. log-observed graphs is a free
  integrity check someday. Out of scope now.

## Next Step

Roadmapped: see "Added 2026-07-02 — workflow topology module" in
`docs/plans/2026-06-14-causal-next-roadmap.md` (implementation detail, TDD test
list, and release steps live there; target 0.18.1).
