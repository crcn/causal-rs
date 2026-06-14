# causal next roadmap — 2026-06-14

Five roadmap items and three DX improvements surfaced from an ecosystem review
and production-readiness audit. Each item is grounded in a specific file, line,
or design gap. Two releases: 0.12 (correctness + production basics) and 0.13
(infrastructure).

Every item includes an adversarial pressure test — an attempt to kill it.
Items that survive the kill attempt are in. Items killed are noted and why.

---

## Release 0.12

### Item 1 — Per-reactor retry policy via `#[reactor]` macro

**Problem.** The engine applies a single global retry policy to every reactor.
A reactor that calls an LLM API (expensive, slow to fail, warrants exponential
backoff with high ceiling) should not be governed by the same policy as a
reactor that reads from a local cache (cheap, should fast-fail after 3
attempts). Without per-reactor policy, one of two bad outcomes:

- Global policy is aggressive: cheap reactors pay unnecessary backoff latency
  before fast-failing.
- Global policy is conservative: a bad LLM reactor floods the queue with
  retries before hitting `max_attempts`.

**Design.** Flat params on `#[reactor]`, consistent with existing `ordering`
and `max_in_flight` params parsed in `causal_core_macros/src/lib.rs:946-1010`:

```rust
#[reactors(deps = ScoutDeps)]
mod scout_reactors {
    #[reactor(name = "web.scrape", max_attempts = 10, initial_backoff_ms = 500, backoff_multiplier = 2.0, max_backoff_ms = 60_000)]
    async fn web_scrape(deps: &ScoutDeps, t: &SourcesPrepared, ctx: Ctx<'_>) -> Result<Events> {
        // ...
    }

    // No retry params → inherits engine-wide default
    #[reactor(name = "dedup.web")]
    async fn dedup(deps: &ScoutDeps, t: &WebScrapeCompleted, ctx: Ctx<'_>) -> Result<Events> {
        // ...
    }
}
```

The macro generates a `retry_policy()` method on the impl when any retry param
is present:

```rust
fn retry_policy(&self) -> Option<::causal::RetryPolicy> {
    Some(::causal::RetryPolicy {
        max_attempts: 10,
        initial_backoff_ms: 500,
        backoff_multiplier: 2.0,
        max_backoff_ms: 60_000,
    })
}
```

The `Reactor` trait gains a default method (no breaking change):

```rust
pub trait Reactor: Send + Sync {
    // ...existing...
    fn retry_policy(&self) -> Option<RetryPolicy> { None }
}
```

`RetryPolicy` is a new `pub struct` in `causal`:

```rust
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub initial_backoff_ms: u64,
    pub backoff_multiplier: f64,
    pub max_backoff_ms: u64,
}
```

The reactor runner queries `reactor.retry_policy().unwrap_or(engine_default)`
per invocation — one method call, no overhead.

**Files:**
- `causal/src/reactor.rs` — add `retry_policy()` default method + `RetryPolicy` struct
- `causal_core_macros/src/lib.rs` — add `max_attempts`, `initial_backoff_ms`,
  `backoff_multiplier`, `max_backoff_ms` to `ConsumerFn` + parse in
  `parse_consumer_fn` + generate `retry_policy()` in the reactor impl block
- `causal/src/reactor_runner.rs` — query `reactor.retry_policy()` before retrying

**Pressure test — kill attempt:**

> "Split the reactor instead. A reactor that does expensive I/O is doing too
> much. Break it into a fast reactor (cheap work, fast-fail policy) and a slow
> reactor (expensive I/O, conservative policy). The global policy handles both;
> they just each fit it."

**Verdict: survives.** Splitting works when the expensive I/O is cleanly
separable. It does not work when the trigger carries context needed by both
cheap and expensive work in one atomic reaction — splitting requires adding an
intermediate event (the cheap result becomes an event the slow reactor reacts
to), which adds an event type, a checkpoint, and latency for no domain reason.
The "split the reactor" prescription is correct as design guidance, not as a
hard rule. Per-reactor policy is the escape valve for the cases where the split
would add more complexity than it removes.

---

### Item 2 — `ctx.version(label, max_supported)` — schema evolution

**Problem.** When a reactor's `ctx.effect()` call sites change across deploys,
in-flight triggers replay against new code. Without version markers, the new
code path runs against old cached effect entries (wrong shape) or re-executes
effects whose labels were renamed (re-runs expensive I/O). Silent correctness
hole the moment `ctx.effect()` is in real use.

Temporal addresses this with `GetVersion()`. Restate avoids it by tying the
journal to a gRPC interface version. causal needs its own solution.

**Design.** Specified in full in `docs/plans/2026-06-14-causal-ecosystem-gaps.md`
(Gap 4). Summary:

```rust
// Returns the version recorded on FIRST invocation of this trigger.
// On redelivery, returns the stored version — not max_supported.
// If stored version > max_supported, returns Err(version_mismatch).
pub async fn version(&self, label: &str, max_supported: u32) -> Result<u32>
```

Stored under `(consumer, trigger_id, "_version:{label}")` in the effect store.
Callers gate behavior:

```rust
let v = ctx.version("embed_model", 2).await?;
let embedding = if v >= 2 {
    ctx.effect("embed", || embed_v2(&text)).await?
} else {
    ctx.effect("embed", || embed_v1(&text)).await?
};
```

Old in-flight triggers (stored version = 1) replay on the v1 path. New
triggers (stored version = 2) take the v2 path. No silent corruption.

**Files:**
- `causal/src/contexts.rs` — add `ctx.version()`
- `causal/src/effect_store.rs` — sentinel key encoding (`_version:{label}`)

**Pressure test — kill attempt:**

> "Divergence detection already catches label sequence changes. If you rename
> an effect label, the reactor logs a tracing::error! on retry. That's enough
> signal to investigate and fix without adding the complexity of explicit
> version markers."

**Verdict: survives, but scope narrows.** Divergence detection catches label
sequence changes AFTER a retry occurs. In production, many in-flight triggers
are retried during a rolling deploy — by the time the error fires, some
triggers have already re-run the wrong code path. Version markers prevent the
corruption; divergence detection only detects it after the fact.

The scope that gets killed: version markers are NOT needed for label renames
caught by the compiler (rename a label string constant and the old effect store
entry is a cache miss — it re-runs, which is correct behavior for a first
invocation). Version markers are needed only for behavioral path changes —
where the same label now calls different code. This is a narrower scope than
originally specced. The API is unchanged; the documentation shrinks.

---

### Item 3 — Ack-floor durability

**Problem.** The reactor runner advances the ack-floor optimistically — it
calls `effect_store.remove(trigger_id)` before verifying that the checkpoint
write to the durable store is complete. Under a crash between `remove()` and
the checkpoint commit, the effect store entry for an in-flight trigger is gone
but the trigger will be redelivered (checkpoint was not written). The reactor
re-executes its side effects. This is the failure mode `ctx.effect()` exists
to prevent.

The window is small. At scale (thousands of concurrent reactors), small
probability becomes near-certainty over time.

**Design.** Read the current ack-floor advancement code to confirm the exact
call sequence, then tighten it so `remove()` is called only after the
checkpoint write is durable. Two options:

**Option A — Sequential:** move `effect_store.remove()` to after the
checkpoint commit in the runner loop. Simplest. Works if the checkpoint write
and the floor advance are in the same sequential code path (likely).

**Option B — Two-phase:** stage the removal (mark as tombstone, not yet
deleted), commit checkpoint, then finalize the deletion. Needed only if the
checkpoint and floor advance are in different async tasks with no ordering
guarantee between them.

Needs a code read of `reactor_runner.rs:790-830` to confirm which option
applies before implementation.

**Files:**
- `causal/src/reactor_runner.rs` — reorder or two-phase the floor advance

**Pressure test — kill attempt:**

> "The race window is a crash between two async operations that happen in
> microseconds. In practice, this never fires. The complexity of two-phase
> deletion is not worth it."

**Verdict: survives for Option A, defer Option B.** Option A is a one-line
reorder — `remove()` after checkpoint, not before. Zero added complexity. The
fix is smaller than the risk of leaving a known correctness hole. Option B
(two-phase tombstone) is deferred unless the code read reveals that the
sequential reorder is impossible.

---

### Item 4 — Workflow lineage (auto-parent-stamping)

**Problem.** When a reactor emits a workflow root fact, the spawned workflow's
events carry no reference back to the parent workflow. The inspector cannot
render a parent→child workflow graph. Debugging "which run triggered which run"
requires scanning every emitter reactor in the log — O(events).

**Design.** Auto-stamp, not a new API. When the engine emits an event whose
`declared_workflow_id()` is `Some(...)` (i.e. a workflow root fact) from
within a reactor body (i.e. `ctx.workflow_id` is not nil), stamp
`_parent_workflow_id = ctx.workflow_id` into the event's metadata envelope.

No API change for reactor authors. They emit facts as today:

```rust
Ok(events![AnotherRequest { run_id: ctx.derive_id("child_run")?, .. }])
```

The engine detects the root fact and stamps the parent. The inspector reads
`_parent_workflow_id` from the metadata and renders the tree.

**Files:**
- `causal/src/engine.rs` — in the emit path, check `event.declared_workflow_id().is_some()` and `ctx.workflow_id != Uuid::nil()`; if both true, stamp metadata
- `causal_inspector` — read `_parent_workflow_id` and render tree (0.12 material for the stamping; inspector rendering can be 0.13)

**Pressure test — kill attempt:**

> "The inspector can reconstruct the parent–child relationship without metadata
> stamping: find the reactor invocation that emitted the child workflow root,
> read its trigger's workflow_id — that's the parent. One query, not O(events),
> because the inspector already indexes by consumer+event. The metadata is
> redundant."

**Verdict: kill the metadata stamping for 0.12, defer to 0.13 with inspector.**
The inspector reconstruction argument is correct IF the inspector indexes
emitter reactor invocations by the events they emitted. If it does, the
metadata stamp is genuinely redundant — it adds 16 bytes to every workflow
root event in perpetuity to save one inspector query. Ship the metadata stamp
only when the inspector is ready to consume it (0.13), so the two ship
together and the value is immediately visible. Stamping without the inspector
UI is invisible value.

**Revised scope:** defer entirely to 0.13, ship alongside inspector tree view.

---

## Release 0.13

### Item 5 — Multi-engine `settle()`

**Problem.** `Engine::settle(workflow_id)` drains the local engine's in-process
queue. In a multi-process deployment (two API instances), `settle()` on
instance A returns `Ok` once instance A's queue is empty — but instance B may
still be processing events for the same workflow. The caller gets a false
"settled" signal.

This blocks horizontal scaling. rootsignal runs single-process today, so this
is a 0.13 item.

**Design.** Two options:

**Option A — Poll the log for a named completion event.**
The caller names the event type that signals completion:

```rust
engine.settle_until::<WorkflowCompleted>(workflow_id).await?
```

The method polls the durable log (with backoff) until it finds a
`WorkflowCompleted` event with a matching `workflow_id`. Works across
processes because the log is the shared source of truth.

Downside: the caller must know which event type signals completion, and must
import it. Couples the settle call to domain event types.

**Option B — Backend open-workflow counter.**
The engine backend tracks a per-workflow "open reactor count" — incremented
when a trigger is dispatched, decremented when a reactor parks or completes.
`settle()` polls the backend counter until it reaches zero.

Advantage: generic, no domain coupling. Disadvantage: requires backend schema
change (new counter table or atomic), and the counter must be crash-consistent
(a crash mid-decrement leaves a phantom open count).

**Recommendation: Option A for 0.13.** Option B is the right long-term
solution but requires significant backend work. Option A ships multi-engine
settle with a one-method change. The domain coupling is acceptable — the
caller who needs settle() already knows what "done" means for their workflow.
Option B can replace Option A in 0.14 if the coupling becomes painful.

**Files:**
- `causal/src/engine.rs` — add `settle_until::<E>(workflow_id)` method
- `causal_replay/src/postgres/` — add log-poll implementation

**Pressure test — kill attempt:**

> "settle() is a test and developer convenience primitive. Production code
> should not depend on it — if you need to know when a workflow completed,
> subscribe to the completion event via a projector. Multi-engine settle() adds
> complexity to solve a problem that well-designed production code doesn't have."

**Verdict: survives, scope clarified.** The argument is correct for async
event-driven consumers (projectors, downstream reactors). It is NOT correct for
synchronous API endpoints that need to return the result of a workflow in the
same request: `POST /run → wait for WorkflowCompleted → return result`. That
pattern is legitimate and common (rootsignal's scout runner uses it). Those
callers cannot use a projector subscription without a push/websocket layer that
adds complexity settle() avoids. Keep multi-engine settle() but document it as
a request/response bridge, not a general synchronization primitive.

---

## DX improvements (0.12)

### DX1 — `TestCtx` owner struct

**Problem.** Every test that hand-constructs a `Ctx` breaks when a new field
is added. This has already happened once: adding `cancelled_workflows` required
mechanical `cancelled_workflows: None` fixes across 4 files
(`multi_projector.rs`, `projection_runner.rs`, `projector.rs`, `reactor.rs`).
It will happen every release.

Root cause: `Ctx<'a>` holds references (`&'a Metadata`, `&'a LabelSet`, etc.),
so tests must create owned state in the test body and pass references. Each
test becomes a 12-line struct literal. Each new `Ctx` field multiplies that
cost across every test in every consuming crate.

**Design.** A `TestCtx` struct that owns the referenced state and hands out a
`Ctx<'_>` from it. Lives in `causal/src/testing.rs`, available behind
`#[cfg(any(test, feature = "testing"))]`:

```rust
pub struct TestCtx {
    meta: Metadata,
    labels: LabelSet,
    store: Arc<dyn EffectStore>,
    event_id: Uuid,
    occurred_at: DateTime<Utc>,
    workflow_id: Uuid,
}

impl TestCtx {
    pub fn new() -> Self { /* sensible defaults: nil ids, empty meta, InMemoryEffectStore */ }
    pub fn with_event_id(mut self, id: Uuid) -> Self { ... }
    pub fn with_effect_store(mut self, s: Arc<dyn EffectStore>) -> Self { ... }
    pub fn ctx(&self) -> Ctx<'_> { /* constructs Ctx borrowing from self */ }
}
```

Tests shrink from:
```rust
let meta = Metadata::new();
let labels = LabelSet::default();
let ctx = Ctx {
    event_id: Uuid::nil(),
    log_position: LogCursor::ZERO,
    occurred_at: Utc::now(),
    workflow_id: Uuid::nil(),
    metadata: &meta,
    consumer: "test",
    labels: Some(&labels),
    state: StateSource::None,
    logs: None,
    effect_store: None,
    cancelled_workflows: None,
};
```

to:
```rust
let tc = TestCtx::new();
let ctx = tc.ctx();
```

Adding a new `Ctx` field in future requires updating `TestCtx::ctx()` in one
place, not every test file in every consuming crate.

**Files:**
- `causal/src/testing.rs` — new file, `TestCtx` struct
- `causal/src/lib.rs` — re-export `testing` behind the feature gate
- Existing tests in `contexts.rs`, `reactor.rs`, `projector.rs` — migrate to
  `TestCtx` (removes ~80 lines of boilerplate, verifies the builder works)

---

### DX2 — Honest `expect()` message for `ctx.effect()` in projector bodies

**Problem.** Projectors receive `effect_store: None` in their `Ctx`. If a
projector body accidentally calls `ctx.effect()`, the current panic message is:

> `"effect_store is always Some — EngineBuilder defaults to InMemoryEffectStore"`

That message is factually wrong from a projector's perspective (the store IS
None for projectors by design) and will send developers in the wrong direction
for several minutes.

**Fix.** One-line change in `causal/src/contexts.rs` at the `expect()` call:

```rust
// before
.expect("effect_store is always Some — EngineBuilder defaults to InMemoryEffectStore")

// after
.expect(
    "ctx.effect() is only available in reactor bodies — \
     projectors are side-effect-free by contract (effect_store is None in projector Ctx)"
)
```

**File:** `causal/src/contexts.rs:382`

---

### DX3 — `RetryPolicy` named constructors

**Problem.** Once `RetryPolicy` exists (Item 1), the two common shapes require
spelling out all four fields every time:

```rust
RetryPolicy { max_attempts: 10, initial_backoff_ms: 500, backoff_multiplier: 2.0, max_backoff_ms: 60_000 }
RetryPolicy { max_attempts: 3, initial_backoff_ms: 100, backoff_multiplier: 1.0, max_backoff_ms: 100 }
```

**Design.** Two named constructors on `RetryPolicy`:

```rust
impl RetryPolicy {
    /// Exponential backoff: multiplier = 2.0, max_backoff = 60s.
    pub fn exponential(max_attempts: u32, initial_backoff_ms: u64) -> Self {
        Self { max_attempts, initial_backoff_ms, backoff_multiplier: 2.0, max_backoff_ms: 60_000 }
    }

    /// Fixed delay: multiplier = 1.0 (no growth).
    pub fn fixed(max_attempts: u32, delay_ms: u64) -> Self {
        Self { max_attempts, initial_backoff_ms: delay_ms, backoff_multiplier: 1.0, max_backoff_ms: delay_ms }
    }
}
```

The macro still generates the full struct literal internally. These are for
engine wiring, `EngineBuilder::with_default_retry_policy()`, and tests.

**File:** `causal/src/reactor.rs` (alongside the `RetryPolicy` struct)

---

## Summary

| Item | Release | Status |
|------|---------|--------|
| Per-reactor retry policy (macro) | 0.12 | In |
| `ctx.version()` schema evolution | 0.12 | In (scope: behavioral path changes only) |
| Ack-floor durability fix | 0.12 | In (Option A — reorder; Option B deferred) |
| Workflow lineage / parent stamping | 0.13 | Deferred (ship with inspector tree view) |
| Multi-engine `settle()` | 0.13 | In (Option A — poll log for named completion event) |
| DX1 — `TestCtx` owner struct | 0.12 | In |
| DX2 — honest projector `expect()` message | 0.12 | In |
| DX3 — `RetryPolicy` named constructors | 0.12 | In |

**0.12 implementation order:**
1. DX2 — one-line fix, do it first (tiny, sets the right tone)
2. Ack-floor fix — read runner code, confirm Option A, fix
3. `RetryPolicy` struct + named constructors (DX3) + `Reactor::retry_policy()` default
4. Macro: retry params in `parse_consumer_fn`, generate `retry_policy()` — TDD
5. DX1 — `TestCtx` builder + migrate existing boilerplate tests
6. `ctx.version()` — last, depends on effect store stability from step 2
