# H2 — No event versioning / upcasting (HIGH, correctness)

**Status:** open. **Decided direction:** build the upcaster seam. Treat as its
own design + review cycle (largest item).

## Finding
Events are immutable and live forever, but there is **no upcasting, version
tag, or migration layer**. `_schema_v` is a user-set metadata key the engine
never reads. Consequences:
- **Field removed/renamed/retyped within a still-registered type** →
  `serde_json::from_value` fails during fold → poison (wedges projectors per
  **H1**; parks reactors, but the old event then never folds successfully
  anywhere).
- **Event-type rename** → the old `event_type` string matches no aggregator's
  `event_prefix` → `apply_event`'s matching filter is empty → **silent identity
  skip** (no fold, no error); projector `step` does `checkpoint.set + continue`.
  A silent correctness loss, distinct from the wedge.

## Evidence
- `aggregator.rs:224-231` (`apply_to` → `from_value?`), `:531-535` (fatal wrap).
- `aggregator.rs:440-444` (event-type match filter → empty ⇒ silent skip).
- `projection_runner.rs:218-220` (skip + checkpoint advance on no-match).
- No `upcast`/`migrate`/per-type version in `modules/causal/src` (grep-confirmed).

## Reproduction / RED tests
- **Schema drift:** register `EventV2 {a,b}`; append a `{"a":1}` (V1) payload;
  fold/replay → currently poison (RED). Target: an upcaster `V1→V2` makes it
  fold successfully.
- **Type rename:** append old-named events; rename the type in code; replay →
  currently a silent skip (assert the events are ignored with no error — RED for
  the silent-loss hazard). Target: a registered rename/upcaster folds them.

## Recommended fix (decided: build the seam)
Introduce an event-upcasting seam, the KurrentDB-standard approach:
- a per-event-type **version** (stamped in metadata or the type registration);
- a registered **upcaster** chain `oldJSON → … → currentJSON` applied at
  read/fold time (in `apply_event` / `restore` / consumer fold), before
  `from_value`;
- handles type renames (old name → current type) and field evolution.
Root enabler for H1 (old events fold instead of parking) and informs **H8**
(snapshot fold-version). Design questions to resolve: where the version lives
(payload vs metadata vs registry), upcaster registration API, ordering with the
existing stream-alignment assertion, and migration of the existing log.

## Why it matters
This is the structural enabler behind the poison class. Without it, every
schema change is a latent poison/silent-skip; with it, schema evolution becomes
a first-class, safe operation.
