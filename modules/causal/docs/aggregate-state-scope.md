# Aggregate state scope and snapshots

> **TL;DR** — Snapshot acceleration for `AggregatorRegistry` is a real
> v0.2.x feature that v0.3 has *not* ported. This is correct. The
> snapshot machinery solves a problem that v0.3's current usage patterns
> do not have. Read this before assuming v0.3 is missing essential
> infrastructure and "porting" it.

## Three patterns, only two of them in scope

Causal applications use the framework at one of three scopes. Pattern
matters because each has a different cost profile and a different
relationship to the `AggregatorRegistry`.

### 1. Per-run engine (in scope)

```rust
let engine = causal::Engine::new(deps)
    .with_aggregators(pipeline_aggregators::aggregators())
    .with_reactors(...)
    .build();
// run a single workflow, then drop the engine
```

A fresh `Engine` constructed for **a single workflow execution**.
In-memory `MemoryStore` log. Aggregator registry lives for the
duration of the run, dies when the engine is dropped. State never
crosses run boundaries.

`ctx.aggregate::<X>()` reads the **saga-pattern** aggregate state
shared across reactors *within this run*. Cursor starts at `ZERO`,
grows incrementally as events are emitted. **No cold-start replay
happens** — the runner's `ensure_hydrated` short-circuits when cursor
is `ZERO`.

Cost: per-event apply, bounded by per-run event volume.

### 2. Service-level materializer (in scope)

```rust
EngineBuilder::new(log, checkpoint, reactor_checkpoint)
    .with_materializer(RunsMaterializer::new(...), "runs")
    .build()
```

Long-lived consumer reading from a persistent event store
(Postgres, future Kurrent), projecting to other persistent state
(Postgres tables, Neo4j). Typed `Materializer<Fact = F>` or
`AnyMaterializer` (deprecated). The materializer body **does not
call `ctx.aggregate`** — it projects events directly into external
storage.

`self.aggregators` on the runner is `None`. `ensure_hydrated`
returns early. **The registry is never touched regardless of how
big the upstream log gets.**

Cost: per-event materialize, bounded by upstream event throughput.

### 3. Long-lived cross-session aggregate (NOT in scope)

A `User`, `Order`, `Account` aggregate whose state is rebuilt from
its event stream every time you load it. Hundreds or thousands of
events accumulated over months or years. Read/write across many
sessions or processes against the same persistent log.

This pattern *does* need snapshot acceleration — replaying the full
stream on every load is O(stream size) and quickly becomes
unacceptable. v0.2.x has the integration: `hydrate_for_event` →
`load_snapshot` → `replay_events_onto` → `set_state`, plus
`maybe_auto_snapshot` to write fresh snapshots periodically.

**Causal v0.3 does not currently expose this integration in its
runners.** This is fine because no current consumer is in pattern 3.

## The trap

The conversation that lands the library off the rails:

> "We're going to hit millions of events soon. We need snapshot
> integration for the AggregatorRegistry so cold-start hydration
> doesn't take forever."

Wrong premise. Re-anchor:

- Are millions of events flowing through a **per-run engine
  (pattern 1)**? Highly unlikely — runs are bounded by their own
  workflow logic. If it happens, the bottleneck is the workflow
  design, not registry hydration.
- Are millions of events accumulating in a **service-level event
  store (pattern 2)** consumed by long-lived materializers? Yes,
  this happens. **But pattern-2 materializers don't use
  `ctx.aggregate`.** Registry hydration cost is zero regardless of
  total log size. Their scaling concern is server-side filtering
  on the read path, which is a backend concern.
- Are you maintaining a **long-lived aggregate that spans engine
  instances (pattern 3)**? If yes, then snapshot integration is
  the right answer. If no, importing the v0.2.x machinery solves
  no problem.

The default mistake is conflating patterns 2 and 3 because both
have "long-lived" and "lots of events" in their description.
They're different. Pattern 2's longevity belongs to the
*projection*, not to an in-memory aggregate. Pattern 3's longevity
belongs to the aggregate itself.

## What v0.3's hydration actually does

`ProjectionRunner::ensure_hydrated`, `ReactorRunner::ensure_hydrated`,
`AnyMaterializerRunner::ensure_hydrated` all share this shape:

```rust
async fn ensure_hydrated(&self, cursor: LogCursor) -> Result<()> {
    if self.aggregators.is_none() {
        return Ok(());                                   // pattern 2
    }
    self.hydrated.get_or_try_init(|| async {
        if cursor == LogCursor::ZERO {
            return Ok::<(), anyhow::Error>(());          // pattern 1
        }
        // Pattern-3 path: replay log[ZERO..cursor] into the registry.
        // Only fires when a runner inherits a non-zero checkpoint —
        // i.e., a long-lived runner against a persistent backend that
        // survived a process restart. O(log size), no snapshot
        // acceleration. Acceptable for current consumers because none
        // are in pattern 3; would need redesign before pattern 3 is
        // viable at scale.
        let mut from = LogCursor::ZERO;
        loop {
            let batch = self.log.load_from(from, 1024).await?;
            // ...fold each event up to cursor
        }
        Ok(())
    }).await?;
    Ok(())
}
```

The pattern-3 branch exists for correctness — a runner finding
itself with a non-zero checkpoint *will* replay events to recreate
registry state — but it is the slow path. It is currently untuned.
Adding snapshot acceleration is the cure when pattern 3 enters
scope; until then the cure has no patient.

## When to reconsider

Add snapshot integration to v0.3 runners only when **all** of these
are true:

1. A specific aggregate is long-lived across engine instances.
2. It folds events from a persistent log that grows unboundedly.
3. Cold-start hydration cost is observed to be a real operational
   problem in production, not a hypothetical worry.

If (1) and (2) are true but (3) is not, document the deferral.
Don't preemptively port machinery for a problem you don't have.

## Reference — what v0.2.x has, ready to adopt if needed

If pattern 3 emerges, these are the existing primitives. The work
is integrating them into the v0.3 runners, not redesigning them:

- `crate::aggregator::AggregatorRegistry::set_state` — restore an
  aggregate's state from a deserialized snapshot.
- `crate::aggregator::AggregatorRegistry::get_state` — read current
  raw state for snapshot serialization.
- `crate::aggregator::AggregatorRegistry::get_version` — last
  applied stream version for the aggregate.
- `crate::aggregator::AggregatorRegistry::replay_events_onto` —
  apply a tail of events onto a state restored from snapshot.
- `crate::aggregator::AggregatorRegistry::update_snapshot_at_version`
  — bookkeeping for snapshot frequency.
- `crate::SnapshotStore` — read/write trait, blanket-implemented
  for any `EventLog`.

The orchestration to copy from v0.2.x: `crate::engine::Engine::{
hydrate_for_event, hydrate_aggregate, maybe_auto_snapshot}`. That
file is the spec for what pattern-3 integration would look like in
v0.3 runners.

## Why this doc exists

Because two agents in two separate conversations have looked at the
v0.3 runner's `ensure_hydrated` next to v0.2.x's `hydrate_aggregate`
and concluded that v0.3 needs the legacy snapshot machinery ported.
Both times, the conclusion was wrong: the consumer in question was
in pattern 1 or pattern 2, not pattern 3. This document exists so
the third agent can read it and avoid the same mistake.
