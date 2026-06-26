# Scope — `EngineBuilder::with_consumer_leasor` (deferred)

**Status: SCOPE ONLY — not implemented. Deferred until a consumer needs to run
the engine on more than one instance.** Surfaced by the RootSignal job-queue work
(`2026-06-25-workflow-scheduler-seam.md`, "Verification findings"): the queue's
multi-server premise is met by the Postgres job table, but the *engine* is not
yet multi-instance-safe through the high-level builder.

## The gap

The lease machinery exists at the **low level** but is unreachable from the
**high level**:

- `ConsumerLeasor` trait — `async fn acquire(&self, consumer_id: &str) -> Result<Box<dyn LeaseGuard>>`
  (`consumer_lease.rs:34`). A `PgConsumerLeasor` ships in `causal_replay`.
- `ReactorRunner::with_consumer_leasor(self, leasor)` (`reactor_runner.rs:504`)
  wires it; the runner calls `leasor.acquire(consumer_id)` before reading its
  checkpoint (`reactor_runner.rs:657-665`), so a competing instance blocks until
  the holder releases.
- **But `EngineBuilder` exposes no way to pass a leasor** to the runners it builds
  (no `leasor` field on the builder or on `ConsumerWiring`, `engine.rs`). An app
  that uses `EngineBuilder` (the normal path) cannot wire leasing at all — it
  would have to drive `ReactorRunner` directly, which production code does not.

Consequence: two engine instances against one shared checkpoint **double-process
reactors** (raced side effects; the effect store dedups *outputs* but not the
wasted work / ordering races). The job table's `FOR UPDATE SKIP LOCKED` makes the
*queue* multi-safe; it does nothing for the *reactor consumers*.

## Proposed surface

```rust
impl EngineBuilder {
    /// Wire an exclusive per-consumer lease. Every reactor runner acquires
    /// `leasor.acquire(consumer_id)` before reading its checkpoint, so only one
    /// engine instance processes a given consumer at a time. Without this
    /// (the default), behavior is unchanged — correct for single-instance.
    pub fn with_consumer_leasor(
        mut self,
        leasor: Arc<dyn ConsumerLeasor>,
    ) -> Self { self.leasor = Some(leasor); self }
}
```

Mirrors the existing `with_clock` / `with_effect_store` / `with_observer`
builder pattern (final-config-at-build, so registration order is irrelevant —
the no-lying-defaults rule).

## Integration points

1. Add `leasor: Option<Arc<dyn ConsumerLeasor>>` to `EngineBuilder` (default
   `None`) and to `ConsumerWiring` (`engine.rs:504`, the struct handed to each
   `RunnerFactory` at `build()`).
2. In the reactor `RunnerFactory`, when `wiring.leasor` is `Some`, call the
   existing `ReactorRunner::with_consumer_leasor(..)` — pure threading, the
   runner already does the work.
3. **Projectors are NOT covered today** — `projection_runner` / `multi_projector`
   have no leasor support (`grep` confirms absent). For a complete
   multi-instance story they need the same treatment, or projector
   double-processing must be acceptable (it is *safe* given the mandatory
   `Projector` idempotency-on-`event_id`, just wasteful). Decide per consumer:
   reactors (side-effecting) want the lease; idempotent projectors may not need
   it. If projector leasing is wanted, add a `leasor` field + acquire to the
   projector runner symmetrically — a larger change than the reactor threading.

## Interaction with drain

`Engine::drain` already documents that the consumer lease is held until all
handles complete and that the drain timeout must be shorter than the lease TTL
(`engine.rs` drain docs). That contract is unchanged; this just makes the lease
reachable from the builder.

## Test plan

- Two engines sharing one `MemoryStore`-backed checkpoint + a test
  `ConsumerLeasor` (in-process mutex): assert only one processes a given
  reactor's triggers at a time; on the holder's `drain`, the other acquires and
  continues from the committed checkpoint (no gap, no double-process).
- A leasor whose `acquire` blocks: assert the second engine's runner parks at
  acquire and does not advance the checkpoint.

## Why deferred

RootSignal is single-instance today (one process: HTTP + engine + reactors +
due-sweep; boot even cancels all unfinished runs, so a second instance would
fight the first). No consumer needs this yet. Implement when the first
multi-instance deployment is on the roadmap — it is additive and independent of
the `event_id` work.
