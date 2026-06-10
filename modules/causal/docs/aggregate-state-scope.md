# Aggregate state scope and snapshots

> **Superseded (2026-06-10).** Durable aggregate restore *shipped* in
> **0.7.4** (`EngineBuilder::with_snapshot_store`, `Engine::load_aggregate`,
> `Aggregate::STREAM_CATEGORY`), and the revision-gated idempotent fold
> model landed in the 2026-06-10 audit-remediation pass. The historical
> framing below — written for a v0.3 line that "does not expose snapshot
> integration in its runners" — is no longer accurate as a *capability*
> claim. The **scoping advice** it gives (which usage patterns actually
> *need* snapshots) is still sound; read the rest of this file for that
> reasoning, but treat its "this integration does not exist" statements
> as historical.

> **TL;DR (current).** Durable, restart-surviving aggregate restore
> exists. Wire `with_snapshot_store(...)` and set `Aggregate::STREAM_CATEGORY`
> and folded state survives a restart via read-through restore
> (`Engine::load_aggregate`, and the consumer runners restore before they
> fold). Snapshots are an *acceleration* on top of that restore, not a
> prerequisite for it — without a snapshot store, restore replays the
> stream from genesis. The question this doc answers is therefore not
> "does v0.3 have snapshots" (it does, as of 0.7.4) but **which
> aggregates actually benefit from snapshot acceleration**.

## The fold model (current)

Two consumers fold aggregate state from the log, and they are *not* the
same store:

- **The shared engine registry** is eager, read-your-write state used
  inside a single in-flight run so `ctx.aggregate::<A>(id)` sees writes
  this run just emitted. Folds are eager (the run drives them forward as
  it emits).
- **Per-consumer runners** (reactor / projector) each restore the
  aggregate read-through (snapshot, if wired, + stream-tail replay)
  *before* folding the triggering event, so a fresh or restarted runner
  recomputes the same state.

Both paths fold through `fold_event`, which is **revision-gated and
idempotent**: an event whose revision is `<` the folded watermark is a
redelivery and is skipped; an event whose revision is `>` the watermark
(a gap — e.g. the eager engine-registry race where a later event is seen
before an earlier one) triggers a read-through repair on the aggregate's
own stream before it folds. That makes folds safe under at-least-once
redelivery and out-of-order observation.

## Three patterns, and which ones want snapshots

Causal applications use the framework at one of three scopes. Pattern
matters because each has a different cost profile and a different
relationship to the `AggregatorRegistry`.

### 1. Per-run engine (snapshots not needed)

```rust
let engine = EngineBuilder::new(log, checkpoint, reactor_checkpoint)
    .with_aggregators(pipeline_aggregators::aggregators())
    .with_reactors(...)
    .build()
    .await?;
// run a single workflow, then drop the engine
```

A fresh `Engine` constructed for **a single workflow execution**.
In-memory `MemoryStore` log. Aggregator registry lives for the
duration of the run, dies when the engine is dropped. State never
crosses run boundaries.

`ctx.aggregate::<X>(id)` reads the **saga-pattern** aggregate state
shared across reactors *within this run*. The registry folds eagerly as
events are emitted, so there's no cold-start replay. Snapshot
acceleration buys nothing here — there's no stored stream to skip past.

Cost: per-event apply, bounded by per-run event volume.

### 2. Service-level projector (snapshots not needed)

```rust
EngineBuilder::new(log, checkpoint, reactor_checkpoint)
    .with_projector(RunsProjector::new(...))
    .build()
    .await?;
```

Long-lived consumer reading from a persistent event store
(Postgres, Kurrent), projecting to other persistent state
(Postgres tables, Neo4j). A `Projector` / `MultiProjector` body
projects events directly into external storage and **does not call
`ctx.aggregate`** — it folds nothing into the aggregator registry, so
no aggregate state needs restoring regardless of how big the upstream
log gets.

Cost: per-event project, bounded by upstream event throughput. The
projector's own cursor (`CheckpointStore`) makes it resumable; that is
unrelated to aggregate snapshots.

### 3. Long-lived cross-session aggregate (snapshots earn their keep)

A `User`, `Order`, `Account` aggregate whose state is rebuilt from
its event stream every time you load it. Hundreds or thousands of
events accumulated over months or years. Read/write across many
sessions or processes against the same persistent log.

This is the pattern snapshot acceleration is *for*. Restore replays
the stream tail; without a snapshot, "tail" means "the whole stream",
which is O(stream size) per load and eventually unacceptable. Set
`Aggregate::STREAM_CATEGORY` so the aggregate has a single physical
stream (`{STREAM_CATEGORY}-{id}`) to restore from, wire
`EngineBuilder::with_snapshot_store(...)`, and restore loads the latest
snapshot and replays only the events after it
(`read_stream(stream_category, id, after_snapshot_revision)`).

## Durable restore (current)

Restore is read-through and lives in `restore_aggregate` (called by
`Engine::load_aggregate` and by consumer runners before they fold):

1. If the registry already has state for the key, return it (idempotent,
   no I/O).
2. Otherwise load the snapshot (if a snapshot store is wired) and seed
   state from it; a blob that fails to deserialize self-heals (deleted,
   rebuilt from revision 0).
3. Replay the stream tail with revision **>** the snapshot revision (all
   events if there was no snapshot) via `replay_events_onto`.
4. Install the folded state into the registry **monotonically** — a
   concurrent install at a higher revision wins, so an out-of-order
   restore can't clobber fresher state.

Restore is disabled (a no-op returning "nothing to restore") when
`Aggregate::STREAM_CATEGORY` is unset — that's the opt-out for
aggregates that only ever live within a single run (pattern 1).

## Choosing whether to snapshot

Restore works without a snapshot store; a snapshot is purely an
acceleration on the replay tail. Decide per aggregate:

- **Per-run engine (pattern 1)** — millions of events are unlikely
  (runs are bounded by their own workflow logic), and there's no
  persisted stream to skip past. Don't snapshot.
- **Service-level projector (pattern 2)** — the projector doesn't call
  `ctx.aggregate`, so it folds no aggregate state. Its scaling concern
  is its cursor and server-side read filtering, not aggregate
  snapshots.
- **Long-lived cross-session aggregate (pattern 3)** — set
  `STREAM_CATEGORY`, wire a snapshot store, and tune snapshot frequency
  once cold-start restore latency is an observed operational problem,
  not a hypothetical one.

The common mistake is conflating patterns 2 and 3 because both have
"long-lived" and "lots of events" in their description. They're
different: pattern 2's longevity belongs to the *projection*; pattern
3's belongs to the aggregate itself, and only pattern 3 restores
aggregate state on a cold start.

## Why this doc exists

Earlier agents repeatedly looked at the runner's restore path and
concluded the library was missing snapshot machinery that needed
"porting" from an older line. Durable restore now ships (0.7.4); the
remaining judgement call is *which* aggregates benefit from snapshot
acceleration on top of it — which is pattern 3, and only pattern 3.
