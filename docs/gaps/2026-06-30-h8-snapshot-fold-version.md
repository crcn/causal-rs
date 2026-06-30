# H8 — Stale-but-deserializable snapshot serves silently-wrong state (MEDIUM, correctness)

**Status:** open. **Decided direction:** add a fold-version tag to `Snapshot`;
discard + rebuild on mismatch.

## Finding
The `Snapshot` struct has **no schema/fold-version tag** — only
`aggregate_type, aggregate_id, revision, state, created_at` (`types.rs:268-274`).
`snapshot_at_version` (`aggregator.rs`) tracks stream *position*, not fold-*code*
version. A snapshot produced by an older `Apply` impl deserializes fine and is
trusted **verbatim** (`restore_aggregate` `Ok(st) => (st, …)`,
`aggregator.rs:1101-1102`), with only newer events replayed on top → **silently
wrong state**, no error, no self-heal.

The existing self-heal only triggers on a snapshot blob that **fails to
deserialize** (delete + rebuild from genesis, `aggregator.rs:1101-1113`). A
snapshot that deserializes but is *semantically* stale (the fold logic changed)
has no detection.

## Evidence
- `types.rs:268-274` (`Snapshot` has no version/schema tag).
- `aggregator.rs:1101-1113` (self-heal only on deserialize failure; valid blob trusted).
- `snapshot_store.rs:27-33` (`delete_snapshot` default is a no-op — weak backends
  could even re-load a bad blob; secondary).

## Reproduction / RED test
Save a snapshot under an `Apply` v1 that computes a field one way. Change the
`Apply` impl (v2) to compute it differently. Restore via `state_of` →
assert it returns the **stale v1 value** with no error today (RED). GREEN target:
a fold-version mismatch discards the snapshot and rebuilds from genesis (or from
the last compatible snapshot).

## Recommended fix (decided: fold-version tag)
- Add a `fold_version: u32` (or schema hash) to `Snapshot` and a per-aggregate
  `Aggregate::FOLD_VERSION` the author bumps when fold logic changes.
- On load, if the tag mismatches the registered aggregator's current version,
  treat it like a bad blob: discard + rebuild from genesis.
- Consider deriving the version automatically where feasible to avoid a
  forgotten bump (the failure mode this guards against is itself a forgotten
  invalidation).
Coordinate with **H2** (event versioning) — same "schema evolution" theme; a
shared version/registry convention may cover both.
