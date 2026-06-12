# Decision memo: the partition key (BLOCKING-4)

**Status: DECIDED 2026-06-12.** Required before any partitioned-runner
code lands (no-lying-defaults BLOCKING-4: "30+ reactors will silently
come to depend on whatever ordering the key implies; switching keys
later changes concurrency semantics under all of them at once").

## Decision

**There is no global partition key.** Each reactor declares its
ordering requirement on its own type:

```rust
const ORDERING: Ordering = Ordering::PerSubject;   // the default
//                         Ordering::PerWorkflow   // run-pipeline shape
//                         Ordering::None          // commutative: per-event concurrency
```

The runner partitions per `(consumer, declared key)`. Dependence on
ordering is declared, reviewable in the same diff as the body it
governs, and changeable per-reactor without touching the rest. The
irreversible global decision decomposes into local reversible ones.

## Why `PerSubject` is the default

Worked in the design doc (BLOCKING-4), summarized:

- **Causation provides most ordering under any key** — a trigger doesn't
  exist until its producer appended it; partitioning only affects
  *siblings*.
- **For run-style workflows the keys coincide** (run subject ≡ run
  workflow), so the default costs pipeline reactors nothing.
- **For shared subjects, the subject key wins**: two workflows touching
  signal X serialize X's events in log order — subject gates become
  race-free by construction, and the BLOCKING-1 fold cache gains true
  exclusivity over the trigger's own subject history (version check
  needed only for cross-subject reads).
- **The cost** — same-workflow siblings on different subjects may
  interleave — breaks no constructed case: reads are position-bounded
  folds (identical answers regardless of processing order), and settle
  is key-independent (per-workflow pending counters + high-water).
- Subject-less facts share one partition per consumer under
  `PerSubject`; a high-volume subject-less feed declares
  `Ordering::None` (its consumer's property, in its attribute).

## What each variant means, precisely

| Variant | Partition key for this consumer | Guarantee | Use |
|---|---|---|---|
| `PerSubject` | trigger's `(SUBJECT, subject_id)` | one subject's events process in log order, across all workflows | default; entity lifecycles, dedup gates |
| `PerWorkflow` | trigger's `workflow_id` | one workflow's triggers process in log order, across subjects | run pipelines reading run-scoped state |
| `None` | none (per-event) | no ordering beyond causation; max concurrency under `max_in_flight` | commutative work (per-file enrichment via workflow-root facts) |

## Explicitly rejected

- **Global trigger-subject key** (fallback in the design doc): correct
  but re-centralizes a semantic 30 reactors would silently couple to.
  Rejected because the dissolution is nearly free: partitions were
  never shared across consumers in any drafted design.
- **Global workflow key** (the conversation's original instinct):
  loses the race-free-subject-gates property and the fold-cache
  exclusivity; survives only inside `PerWorkflow` where a reactor
  genuinely wants it.

## Invariants the runner build must honor (acceptance-tested)

1. Settle is key-independent: per-workflow pending counters +
   high-water; `drained(workflow, hw)` = ingestion reached hw ∧ no
   queued/in-flight triggers for that workflow at this consumer.
2. Dispatch order: among ready partitions, lowest-position head first —
   `max_in_flight = 1` reproduces today's global log-order serial
   runner exactly.
3. BLOCKING-3 window: ingestion bounded by pending matching triggers,
   never log distance; acked-past-floor tracked as merged runs.
4. BLOCKING-1 fold cache: per-partition, version-checked against the
   subject-history head; dies with its partition (eviction-on-drain).
