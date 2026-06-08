# PG observability read-store (best-effort, fleet-shared)

**Date:** 2026-06-08 (final, after two pressure-test passes)
**Status:** Building.
**Author:** craig + Claude

## Decision (locked)

**PG is a best-effort, lossy, async, fleet-shared read + observability store for
the inspector. Kurrent stays the durable source of truth. PG is NOT used for
coordination, leasing, or anything that must be bulletproof** (per craig: "PG is
for logging only").

That single constraint settles everything the pressure-tests churned on:

- **No PG leasing.** Reactor competing-consumer coordination is a *separate*
  concern and, if ever needed, belongs to Kurrent (persistent-subscription
  consumer groups), not PG. Deferred — and likely unneeded if reactors run on one
  worker while only the API/inspector tier is load-balanced.
- **No synchronous mirror.** PG is populated **off the hot path** by an idempotent
  async projector — a PG outage/lag never gates an emit.
- **No durable PG DLQ.** The DLQ already lives as a durable fact in the log
  (`on_dlq` mapper); the read model surfaces it from the projected events.
- **Best-effort throughout.** The observer drops on overflow; the projector
  catches up; losing rows just means the inspector is briefly behind. Fine.

## Architecture

```
 emit / reactor outputs ──► Kurrent (durable source of truth)
                               │
        (1) idempotent async projector: read $all from a checkpoint,
            INSERT … ON CONFLICT(event_id) DO NOTHING into PG causal_log
                               ▼
        reactor hot path ──(2) lossy async PgReactorObserver──► PG observability tables
                               ▼
                         Postgres  ◄── (3) PgInspectorReadModel ◄── inspector (any box)
```

Every box writes to one Kurrent + one PG; the inspector on any box reads the whole
fleet from PG. PG is the read seq authority (its own `BIGSERIAL`); observability
rows key on `event_id` and the read model joins `causal_log` to assign display
`seq` — so there is **one seq space** and the old mirror seq-mismatch can't occur.

## Components

1. **Schema** (`docs/schema.sql` + migration). Reuse `causal_log` (events). Add
   best-effort observability tables:
   - `causal_reactor_executions` (one row per **physical** `react()` call, PK =
     `execution_id UUID`; cols: event_id, reactor_id, correlation_id, attempt
     (informational), status, error, started_at, completed_at). Status/attempts
     derived by grouping on `(event_id, reactor_id)` — never PK'd on `attempt`
     (it resets on restart).
   - `causal_reactor_logs` (execution_id, event_id, reactor_id, correlation_id,
     ord, level, message, data jsonb, logged_at).
   - `causal_reactor_descriptions` (event_id, reactor_id, correlation_id,
     description jsonb).
   - `causal_aggregate_snapshots` (event_id, aggregate_key, state jsonb,
     correlation_id) — **deduped**: engine-level fold only + drop byte-identical
     successive states. Keyed by `(event_id, aggregate_key)`.
   - All indexed by `correlation_id` and `(event_id, reactor_id)`. Retention by
     whole correlation (or partition + DROP), never row-age.
2. **`PgReactorObserver`** (`causal_replay`, write side — the genuinely-new piece):
   hooks are cheap (`try_send` to a bounded channel, drop on overflow); a spawned
   batch-writer multi-row-INSERTs per table. `ON CONFLICT DO NOTHING` for
   at-least-once idempotency. Engine wires it via `.with_observer(...)`.
3. **`PgInspectorReadModel`** (`causal_replay`, read side): `impl
   InspectorReadModel` over `causal_log` + the 4 tables. **Port `MemoryStore`'s
   semantics** (the reference impl), not rootsignal's stubbed seesaw SQL. Key
   observability by `event_id`; join `causal_log` for `seq`.
4. **PG events projector** (`causal_replay`): an idempotent background catch-up
   consumer — `read_all` from a PG-checkpointed cursor over Kurrent → batch
   `INSERT … ON CONFLICT(event_id) DO NOTHING` into `causal_log`. Best-effort;
   any/all boxes can run it (dedupes on event_id).
5. **Live fan-out**: each inspector tails PG (poll the projected `causal_log`, or
   `pg_notify` later) into its local broadcast, so all boxes' WS clients see the
   fleet.
6. **Demo/wiring**: a production-shape example (Kurrent log + PG observability +
   inspector) — switch `inspector-demo` to it or add `inspector-demo-pg`.

## Validation

- `PgInspectorReadModel` runs the same conformance shape as the other PG suites,
  via `./dev.sh test pg` against the dev stack.
- A round-trip test: drive reactors → assert PG observability tables populate →
  assert the read model returns the same data `MemoryStore` would.

## Version

New public surface (`PgReactorObserver`, `PgInspectorReadModel`, projector) +
schema ⇒ `0.7.0`.

## Build order

1. Schema + `PgReactorObserver` + `PgInspectorReadModel` + PG round-trip test (the core).
2. PG events projector + live tail.
3. Demo + docs.

## Reference pointers

- Reference impl (both traits, current types): `modules/causal/src/memory_store.rs`.
- Write contract: `modules/causal/src/reactor_observer.rs`.
- Read contract + entry types: `modules/causal_inspector/src/read_model.rs`.
- Existing PG backend patterns: `modules/causal_replay/src/{event_log,reactor_checkpoint,snapshot_store}.rs`.
