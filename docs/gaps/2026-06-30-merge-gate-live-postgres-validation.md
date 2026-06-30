# Merge gate — live-Postgres validation (branch `hardening/recoverability-hazards`)

**Status:** REQUIRED before merging this branch to `main`. The shipped fixes
include a Postgres-specific code path that was authored and reviewed but **could
not be executed** in the environment where the work was done (no live database).
The MemoryStore equivalents are verified; the PG path rests on code review until
this gate is cleared.

## Why this gate exists
H5 (`fix(checkpoint): monotonic advance() split from absolute set()`,
`d57f8e8`) added an **atomic `GREATEST` upsert** to `PgReactorCheckpoint::advance`
(`modules/causal_replay/src/reactor_checkpoint.rs`) so a lagging concurrent
writer cannot regress a more-advanced cursor. Its correctness — that two racing
`advance`s collapse to the maximum, and that `set` remains absolute — is the
whole point of the H5 fix, and it is SQL-engine behavior that only a real
Postgres can confirm. The conformance test exists but is `#[ignore]`'d (needs
`DATABASE_URL`).

## What to run
With a local Postgres and the schema migrated (the PG conformance tests already
document the `DATABASE_URL` + migration requirement):

```sh
# H5 — the new monotonic-advance contract on the real backend:
cargo test -p causal_replay --test pg_checkpoint_conformance_test \
  checkpoint_advance_is_monotonic -- --ignored --exact

# Full PG checkpoint conformance (regression safety for the set/advance split):
cargo test -p causal_replay --test pg_checkpoint_conformance_test -- --ignored

# PG crash-recovery + event-log conformance (the advance() default now routes
# the hot path; confirm the FaultingCheckpoint paths and ordering still hold):
cargo test -p causal_replay --test pg_crash_recovery_test -- --ignored
cargo test -p causal_replay --test pg_event_log_conformance_test -- --ignored
```

## Pass criteria
- `checkpoint_advance_is_monotonic` GREEN on PG: `advance(100)` then
  `advance(40)` leaves the cursor at 100; `advance(150)` moves it to 150; `set(5)`
  installs 5 (absolute). This proves the `GREATEST` upsert is monotonic and that
  `set` is unaffected.
- No regression in the existing PG conformance / crash-recovery suites.

## If it fails
The `GREATEST` clause or the `ON CONFLICT` target is the first suspect:
```sql
INSERT INTO causal_checkpoints (consumer_id, position) VALUES ($1, $2)
  ON CONFLICT (consumer_id) DO UPDATE
    SET position = GREATEST(causal_checkpoints.position, EXCLUDED.position),
        updated_at = now()
```
Confirm the qualified `causal_checkpoints.position` (not bare `position`) on the
`GREATEST` left arm — an unqualified `position` there is ambiguous/incorrect.

## Related
- H10 (`docs/gaps/2026-06-30-h10-pg-lock-regression-test.md`) is a *separate*
  deferred PG test (the ordering-lock regression guard) and also needs a live DB
  to author — fold it into the same validation session if you implement it.
