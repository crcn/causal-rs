# Workflow scheduler seam — 2026-06-25

**Status: DESIGN — adversarially pressure-tested 2026-06-25 (self-pass + an
external best-practices pass against primary sources: transactional outbox,
exactly-once, Postgres-as-queue, Temporal/Step Functions/Oban/River start
semantics, mechanism/policy separation). The two passes converged on the
headline change — admission control over slot-holding — independently. The
external pass forced one correctness fix (status ordering, §1) and pushed back
hardest on enforcement (§8) — the one open decision below. A later code read
narrowed §5: byte-identity is real but `created_at` is exempt and verbatim
re-emit covers it, so it is not the poison-loop I first called it. A
RootSignal-side verification pass (2026-06-25) then **resolved §8** (enforcement
is a RootSignal `ScoutEngine` wrapper routing on the macro attribute, not
structural confinement — api has 22 non-root emits that can't move) and
**overturned §4** (roots use `new_v4()`, so dedup is by entity key, not
`workflow_id`). See "Verification findings (RootSignal)".**

How RootSignal routes workflow *starts* through a Postgres-backed job queue
that caps concurrent execution, **without** causal growing a queue/scheduler
abstraction. The headline finding: the seam belongs in RootSignal, not causal.
causal's net change is **one additive method** — a caller-supplied `event_id`
for idempotent exogenous appends. (`.settled()` stays; an earlier draft removed
it, which §0 walks back.)

Every design decision below carries an adversarial pressure test — an attempt
to kill it (house style, per the 0.10 and roadmap docs).

---

## Problem

RootSignal uses causal as its workflow engine. Every workflow entry point —
`SourceScanRequested`, `CoalesceRequested`, `SituationSynthesisRequested`,
`FeedGroupRequested`, `ActorEnrichmentRequested` — is a workflow ROOT and is
started identically: a raw `tokio::spawn` wrapping
`engine.emit(XRequested).settled().await`, at the API layer (the due-sweep loop
and GraphQL mutations).

There is no concurrency cap. A source scan discovers new sources (link
promotion, actor expansion); each becomes a new `SourceScanRequested`; the
due-sweep fires all of them via `tokio::spawn` with nothing throttling
parallelism. Discovery is *meant* to be unbounded (it's a crawler) — but
parallel *execution* is not, because it blows the budget and resource ceiling.

The fix is a Postgres job queue (`rootsignal-jobs`, a new standalone crate)
that caps concurrent execution per job type, supports priority, retries with
backoff, dead-lettering, heartbeat/stale detection across server instances, and
an admin view. The question this doc answers: **what, if anything, does causal
need to expose so that all workflow starts route through that queue?**

---

## The core decision: the seam lives in RootSignal, not causal

### What we considered first (and rejected)

The intuitive seam is *inside* causal: `emit(root)` auto-detects a workflow root
(it already does — `execute_emit` collects `declared_workflow_id()` at
`engine.rs:1563-1586`) and routes it to a registered `WorkflowScheduler` trait
object instead of appending. causal stays ignorant of Postgres; the queue
implements the trait.

This forces a second engine verb. Once `emit(root)` auto-routes, the queue
worker can no longer use `emit` to *execute* a dequeued start — it would
re-route to the scheduler and re-enqueue forever (the recursion hazard). So the
worker needs a non-routing twin, e.g. `engine.begin(root)`. Now causal carries a
`WorkflowScheduler` trait, a `WorkflowStart` DTO, a routing branch, and a
`begin` method — all to express a policy (when does a start begin) that causal
has no opinion about.

### What we landed on

**Take routing out of `emit` entirely.** Then there is nothing to recurse
through, and no second verb is needed:

- **Request side** calls a RootSignal facade — `workflows.start(fact)` — which
  serializes the root and INSERTs a job row. It never touches `engine.emit`.
- **Worker side** dequeues, deserializes, and calls plain `engine.emit(fact)` to
  append the root and run the chain. `emit` does not route, so there is no
  recursion and no `begin`.
- **Enforcement** ("a root can't bypass the queue") moves into RootSignal: its
  API layer is handed the `workflows` facade, not a raw `Engine`. There is no
  `emit` in scope to forget.

This is strictly simpler, and it dissolves the recursion hazard rather than
managing it. Every original constraint is still satisfied (scorecard below) —
the constraints just stop being causal's concern, which is correct: a Postgres
job queue with per-type caps, priority, leases, and dead-letters is an
*application* concern, not an event-sourcing-runtime concern.

> A `WorkflowScheduler` trait in causal would have been a thin pass-through to
> the same SQL the queue runs anyway. It adds an indirection, not a capability.

**Pressure test — *"You're giving up a real invariant. causal-enforced
no-bypass is stronger than a RootSignal facade; one stray `engine.emit(root)`
silently skips the queue and the budget blows again."*** Survives, conditionally.
The invariant is real but is recoverable in RootSignal at equal strength by not
exposing `Engine` to the API layer (hand out the facade; keep the raw engine in
the worker crate only). Constraint #4 framed causal-side enforcement as an
*opportunity*, not a requirement. The trade is: a hard compile/runtime guarantee
in causal vs. a structural-visibility guarantee in RootSignal. For a single team
owning both sides, the facade is sufficient — and it keeps causal generic. *If*
RootSignal later finds roots leaking from many call sites, revisit the
causal-side trait. (Flagged for the pressure-test pass to challenge harder.)

> **OVERTURNED by verification (see "Verification findings" below).** Structural
> confinement is not viable — api has 22 non-root `emit` sites that can't move to
> a worker, so it must hold the engine. Enforcement instead lives in a RootSignal
> **wrapper** (`ScoutEngine`) that routes on the macro attribute
> (`declared_workflow_id`), with a `begin`/`execute` bypass and a `RunCancelled`
> exemption. causal still changes only by `event_id`.

---

## Verification findings (RootSignal, 2026-06-25) — authoritative

RootSignal's side verified the design against its actual code
(`rootsignal-api`/`-graph`/`-scout`/`-actors`). Two findings broke stated
assumptions and **resolve the §8 open decision**; the rest confirm or refine.
This section supersedes the earlier §8 "structural confinement" verdict and the
§4 epoch advice.

### Resolved: enforcement is a RootSignal *wrapper that routes on the macro attribute* — not structural confinement, not a causal trait

The structural-confinement plan ("only the worker holds `Engine`, hand the API a
read-only facade") is **dead**: `rootsignal-api` has **22 non-root
`engine.emit(...).settled()` sites** — admin corrections (`SourceChanged`,
`GroupMerged`, `ActorReclassified`, `SourcesDiscovered`, curator-group CRUD,
`DemandReceived`) that return synchronously to a GraphQL caller and **cannot move
to a worker**. (They force a workflow with `.workflow_id(Uuid::new_v4())` at the
call site; none declare `workflow_id` in their macro.) So api must hold the
engine, and `engine.emit(root)` is reachable there.

It also turns out api **never reads through the engine** (zero `state_of`/`load`/
`snapshot` — reads go via the graph/cache layer), so the read-only-facade idea
was unneeded anyway.

The resolution keeps causal generic *and* gets non-bypassable enforcement: make
**`ScoutEngine` a wrapper** (it is a bare `type ScoutEngine = causal::Engine`
alias today, `engine.rs:142`) holding a private `Arc<Engine>` + the queue. Its
`emit` classifies on the **macro attribute** — `fact.declared_workflow_id()`,
which causal already computes:

- **root** (`Some`) → enqueue (entity-key dedup; store the serialized fact).
- **member** (`None`) → passthrough to `inner.emit` — the 22 admin emits are
  untouched.
- a separate `execute()`/`begin()` method (worker-only) → `inner.emit(f).event_id(..)`,
  **no routing**, so no recursion.

This is the "route-on-root-ness + a begin twin" model from the very first design
pass — but living in a **RootSignal wrapper**, not in causal. causal stays at the
single additive `event_id`. Because api only ever holds `ScoutEngine` (not raw
`causal::Engine`), routing is mechanism-level, not advisory — and existing emit
call sites **don't change** (members pass through, roots auto-route), which is
exactly the brief's "route without each call site remembering."

> **Cancel exemption (required).** `RunCancelled` declares `workflow_id = "run_id"`
> (so it classifies as a root) but is a cancellation that must emit *promptly* and
> bypass the queue. The wrapper's routing rule needs an explicit carve-out:
> roots route **except** `RunCancelled`, which passes through. `engine.cancel_workflow`
> must also stay reachable from api (`edges/mod.rs:317,319`).

### Resolved: dedup on the entity key, not `workflow_id` — and no epoch needed

Every live root's `workflow_id` is a fresh `Uuid::new_v4()` minted at the emit
site (`edges/mod.rs:44,79,144,184,229,252,276`) — random, no epoch. So
`ON CONFLICT (workflow_id)` would **never** fire and duplicates pile up. My §4
"epoch the workflow_id" advice is **retracted**. The right mechanism (confirming
R5): **`UNIQUE (entity_key) WHERE status IN ('queued','running')`** — at-most-one
active job per entity. Entity keys (from RootSignal): `source_id`;
`region_id+flow_type`; coalesce→`region_id`; synthesis/feed→`group_id`;
actor→`actor_id`; media→`file_id`. The `workflow_id` can stay a random v4: it is
minted/stored at enqueue and the worker re-emits the **stored payload verbatim**
(rule #5) so it's retry-stable; re-runs after completion are naturally allowed
(the unique index only blocks active rows). This also **replaces** RootSignal's
inconsistent ad-hoc guards (`is_source_busy` is TOCTOU-racy by its own admission;
feed and actor have *no* guard today).

### Confirmed: R1 is clean — no reactor-emitted roots

All **9** root types (more than my list of 6: add `ScoutRunRequested` [legacy,
no live emitter], `RegionScanRequested`, `RunCancelled`, `MediaEnriched`) are
emitted at the **API boundary only**. Greps of `rootsignal-scout`/`-actors` for
roots in `Ok(events![..])`/`.push`/`.emit` returned nothing; no `derive_id`
feeds any `workflow_id` in an emission path. The media-enrichment path I worried
about is boundary code (an mpsc drain on the api side), not a `react()` body. So
the wrapper seam covers everything — **nothing to untangle in the reactive
layer.** (The §9/R1 "reactor-child-root bypass" risk does not exist here; the
guard test is still worth keeping as a regression fence.)

### Confirmed: the start-cap alone won't bound spend — `max_in_flight` is required too (R3)

Budget is dollar spend, LLM-dominated, and **fan-out happens inside one
workflow** (a region scan scrapes all its sources in a single chain). No reactor
sets `MAX_IN_FLIGHT` today (all `usize::MAX`). So the queue's start-cap throttles
*runs begun*, but a single fat run is unbounded — the spend lever is causal's
per-reactor `MAX_IN_FLIGHT` on the scrape / curiosity / coalescer / actor
reactors. **Both are required and complementary.** This is a RootSignal change
(set `max_in_flight` on those reactors), not a causal one.

### Multi-server: single-instance today; the engine is NOT yet multi-safe (deferred causal gap)

RootSignal runs a **single instance** (one process: HTTP + engine + reactors +
due-sweep). It is **not** multi-server safe: no consumer lease is wired, and
`EngineBuilder` does not even expose `with_consumer_leasor` (verified — only the
lower-level `ReactorRunner` takes one; `PgConsumerLeasor` exists in
`causal_replay` but is unreachable from the builder). Boot also runs
`UPDATE runs SET cancelled_at=now() WHERE finished_at IS NULL`, so a second
instance would cancel the first's live runs.

Implication: the slot-release-via-completion-projector model is **fine
in-process now**. The queue's multi-server premise is met by the **job table**
(`FOR UPDATE SKIP LOCKED`), not the engine. Running worker+engine on >1 instance
would double-process reactors until a lease is wired — which needs a **new causal
affordance: `EngineBuilder::with_consumer_leasor(..)`** (a second causal change,
deferred until RootSignal scales out). **Recommendation: single worker instance
now; revisit the lease before horizontal scaling.**

Also verify (slot-leak, risk #10): every type emits a uniform completion fact,
**and** the failure/cancel paths release the slot — a run that dies without a
terminal fact leaks one. Confirm `RunCancelled` fires on *all* abort paths.

### Policy decisions (resolved 2026-06-25)

1. **Per-type cap numbers** — *a tunable, not a blocker, and not a causal
   concern.* The cap is a `running < cap` check on RootSignal's `type_slots`
   counter; causal has no notion of it. Make it a config row (changeable without
   redeploy), seeded conservatively from the cost ordering (RegionScan heaviest →
   Coalesce → SourceScan → Synthesis → Feed → ActorEnrich). Source scans (no
   per-tick ceiling today) are the prime target. Tune empirically against the
   budget.
2. **Manual-trigger semantics — queue-and-forget, with an optional `run_now`
   override.** Default: a manual trigger just enqueues and returns the
   `workflow_id` — which *matches today's behavior* (the GraphQL mutations already
   `spawn + settled` fire-and-forget; no caller waits). No reserved capacity
   needed by default. Escape hatch: a **`run_now` flag on the job** (set by the
   mutation, or flipped from the admin view on a queued job) makes the dispatcher
   **admit it ignoring the cap gate** — still tracked, still counted (the count
   transiently exceeds cap; completion decrements it), still durable/retryable.
   **`run_now` overrides the resource *cap*, NOT the entity-uniqueness invariant**
   (`UNIQUE (entity_key) WHERE active` still holds — no two scans of one source,
   even manually). Keep `run_now` operator/rare, or it defeats the cap. No causal
   change — it reuses the worker's existing `execute()` path.

---

## causal-side surface (the entire change)

The causal change is **one additive method**. (An earlier draft also removed
`.settled()`; that was a holdover from the rejected "route inside `emit`" model
— see §0 below.)

### 0. KEEP `EmitBuilder::settled()` — it never lies in this design

`.settled()` (`engine.rs:453`) is sugar for `execute_emit` then `engine.settle`.
The case for removing it was "a wait chained onto an *enqueued* start is
meaningless." But in this design **`emit` never routes** — the only way to
enqueue is `workflows.start()` (the RootSignal facade, which returns a
`workflow_id`, not an `EmitBuilder`). You therefore *cannot* chain `.settled()`
onto a start; the lie is structurally impossible. In causal, `.settled()` stays
exactly what it was — emit + drain-this-engine — used only by tests and
single-instance code, where it is honest. Removing it would churn 64 call sites
(tests, replay, inspector, examples, CLI) for zero correctness gain. **Keep it.**

The request/wait split that motivated the redesign still holds — it just lives
in RootSignal: `workflows.start()` returns a handle (`workflow_id`), and waiting
is a *separate* concern (the completion-fact projector, never a chained
`.settled()`).

### 1. Add `EmitBuilder::event_id(Uuid)` — idempotent exogenous append

Today every `emit` mints a fresh `event_id` per fact (`Uuid::new_v4()`,
`engine.rs:1632`); the docs say so explicitly: *"retrying an emit that succeeded
writes the facts again — retry only on `Err`"* (`engine.rs:1279`). That is the
correct default for spontaneous emits, but it is fatal for a *queue worker*: a
worker that crashes after `emit` commits the root but before it marks the job
done will, on re-lease, re-emit a **duplicate root** with a new `event_id` —
spawning a second reactor chain. The duplicate is not caught by reactor-output
dedup, because the two roots have different `event_id`s and therefore derive
different output ids.

The backend already dedups appends on `event_id` (`event_log.rs:27-31`: a
second append carrying an already-persisted `event_id` returns an equivalent
`WriteResult` without persisting). So the fix is to let the caller *supply* the
root's `event_id`:

```rust
// EmitBuilder gains one field alongside workflow_id / causation_id / metadata
// (engine.rs:394-400):
pub fn event_id(mut self, id: Uuid) -> Self { self.event_id = Some(id); self }
```

- Generic: it is "idempotent exogenous append," not workflow-specific. No
  workflow vocabulary enters causal.
- RootSignal owns the derivation: `event_id = v5(NS_WORKFLOW_ROOT, workflow_id)`,
  mirroring the deterministic-id recipe reactors already use
  (`derive_output_event_id`, `reactor_runner.rs:123`). The workflow_id is itself
  derived/stable (the queue's dedup key), so the root's event_id is stable across
  retries.
- Scope: valid only for a **single-fact** emit (the root case). A multi-fact
  batch with one `event_id` is ambiguous; reject it, consistent with the
  existing "emit roots separately" rule (`engine.rs:1578-1581`).

**Pressure test — *"Caller-set `event_id` lets callers forge collisions and
corrupt the log — you're exposing a footgun the framework deliberately
hid."*** Survives. The footgun already exists framework-internally (reactor
outputs set deterministic ids); exposing it for exogenous appends is consistent,
and the alternative (the worker reads-the-stream-before-emit to dedup) is racier
and slower. The byte-identity contract (`event_log.rs:31`) is the real sharp
edge — see the caveat below — and is addressed by modeling, not by hiding the
setter.

---

## RootSignal integration (concrete shape — not patched here)

Per the release flow ([causal→rootsignal](../../) memory): verify usage
statically, publish causal, then bump RootSignal. Code below is the target
shape, not a patch to apply now.

### (a) A root fact — `occurred_at` is a STORED field (rule #5)

```rust
#[event(name = "source_scan_requested", subject_id = "source_id",
        workflow_id = "run_id", occurred_at = "requested_at")]
pub struct SourceScanRequested {
    pub run_id:       Uuid,            // the workflow id (macro → declared_workflow_id)
    pub source_id:    Uuid,
    pub requested_at: DateTime<Utc>,   // frozen at request; survives re-emit byte-identical
}
```

The worker re-emits the *stored payload verbatim*, so `requested_at` (and every
other field) is identical across retries — no divergence poison-loop (§5).

### (b) Opt each root into the queue — a RootSignal marker (compile-time)

```rust
pub trait WorkflowRoot: Event + Sized {                  // lives in rootsignal-jobs
    fn priority(&self) -> Priority { Priority::Normal }
}
impl WorkflowRoot for SourceScanRequested {
    fn priority(&self) -> Priority { Priority::Discovery }   // manual triggers override
}
// … the other ~5 roots
```

`start::<F: WorkflowRoot>` then refuses non-roots at compile time.

### (c) Request call sites — what the API-layer dev writes (one verb)

```rust
// GraphQL mutation — manual trigger jumps the queue
let wf = workflows.start(
    SourceScanRequested { run_id: derive_run_id(source_id, epoch_now()), source_id,
                          requested_at: Utc::now() }
).priority(Priority::Manual).await?;     // returns workflow_id immediately

// due-sweep — identical call, no special-casing, no tokio::spawn
for s in due_sources { workflows.start(SourceScanRequested { .. }).await?; }
```

The dev never sees `emit`, `settle`, queue mechanics, or `spawn`. The unbounded
parallelism is gone by construction — `start` enqueues, it does not execute.

### (d) The facade — `start` is an INSERT, transactional with domain writes (§1)

```rust
impl Workflows {
    pub async fn start<F: WorkflowRoot>(&self, fact: F) -> Result<Uuid> {
        let wf = fact.declared_workflow_id().expect("WorkflowRoot declares its workflow");
        sqlx::query!(
            "INSERT INTO jobs (workflow_id, job_type, payload, priority, status, created_at)
             VALUES ($1,$2,$3,$4,'queued',now()) ON CONFLICT (workflow_id) DO NOTHING",
            wf, <F as Event>::NAME, serde_json::to_value(&fact)?, fact.priority() as i32,
        ).execute(&self.pg).await?;
        Ok(wf)
    }
    // start_tx(fact, &mut txn) — same, inside the caller's transaction → no outbox (§1)
}
```

`run_id` must be stable + **epoch'd** (rule #4) so `ON CONFLICT` dedups
double-requests without forbidding a later legitimate re-scan.

### (e) The dispatcher — admission, then idempotent emit, with start-error handling

```rust
// rootsignal-jobs worker crate — the ONLY holder of `Engine` (§8 confinement)
loop {
    // admit_one(): cap-check + counter++ + status='running' in ONE statement,
    // committed BEFORE the emit I/O (§2/§7). Race-free; no txn held across a workflow.
    let Some(job) = admit_one(&pg).await? else { wake.notified().await; continue };
    let eid = derive_root_event_id(job.workflow_id);   // v5(NS, workflow_id) — stable

    // Deserialize (RootSignal's ~6-arm match) then append the root + run the chain.
    let started = dispatch_emit(&engine, &job, eid).await;   // -> Result<()>

    if let Err(e) = started {
        // START failure — the root never durably entered the log. The queue owns this.
        match classify(&e) {
            // un-deserializable / divergence poison → code or modeling bug; don't loop.
            StartError::Poison => dead_letter(&pg, &job, &e).await?,        // + counter--
            // transient (log backend down) → requeue with backoff, give the slot back.
            StartError::Transient if job.attempts < MAX_START_ATTEMPTS =>
                requeue_with_backoff(&pg, &job).await?,                     // + counter--
            _ => dead_letter(&pg, &job, &e).await?,                         // + counter--
        }
    }
    // On Ok we do NOT wait. The slot stays counted until the workflow's
    // terminal fact frees it (below) — success OR failure.
}
```

### (f) Slot release — projectors over the workflow's terminal facts (§3)

```rust
// SUCCESS: folds the workflow's completion fact.
#[async_trait]
impl Projector for SlotReleaser {
    type Event = SourceScanCompleted;
    const NAME: &'static str = "jobs.release.source_scan";
    async fn project(&self, _f: &SourceScanCompleted, ctx: Ctx<'_>) -> Result<()> {
        free_slot(&self.pg, ctx.workflow_id, "done").await?;   // idempotent (below)
        self.wake.notify_one();
        Ok(())
    }
}
// FAILURE: a sibling projector over the on_terminal_failure-synthesized fact runs the
// SAME free_slot(.., "failed") — so a terminally-failed workflow frees its slot too.

// free_slot: the WHERE status='running' guard makes the decrement fire EXACTLY once,
// even though project() is at-least-once (rule #10):
//   WITH done AS (UPDATE jobs SET status=$2, finished_at=now()
//                 WHERE workflow_id=$1 AND status='running' RETURNING job_type)
//   UPDATE type_slots SET running = running - 1 WHERE job_type IN (SELECT job_type FROM done);
```

### (g) Wiring — where the `Engine` lives IS the enforcement (§8)

```rust
// worker crate only:
let engine = EngineBuilder::new(log, ckpt, rckpt)
    .with_reactors(scout_reactors())                       // the workflow chains
    .on_terminal_failure(|tf: TerminalFailure| Some(       // EVERY terminal failure → a fact
        WorkflowFailed { workflow_id: tf.workflow_id, consumer: tf.consumer,
                         class: tf.class, error: tf.error, attempts: tf.attempts }))
    .with_projector(SlotReleaser { pg: pg.clone(), wake: wake.clone() })
    .with_projector(WorkflowFailedReleaser { pg: pg.clone(), wake: wake.clone() })
    .build().await?;
// The API layer is built with `Workflows { pg }` and NEVER receives `engine`.
// There is no `emit` in scope to bypass the cap.
```

`engine.settle(handle)` stays available for **single-instance tests**; production
never uses it for slot release (its high-water is in-process, `engine.rs:1835`).

---

## Errors & failure handling

**No new causal API.** Failures are handled entirely by mechanisms causal
already has — per-reactor `RetryPolicy`, the mandatory `on_terminal_failure`
path (`engine.rs:706-733`), and `Projector` idempotency — plus RootSignal
queue policy. The model splits into **two domains** with one bridge.

### Domain A — start failures (the root never durably entered the log)

The QUEUE owns retry/dead-letter; the reactor chain never ran, so causal is not
involved. Handled in the dispatcher's error arm (e):

| Failure | Handling |
|---|---|
| Enqueue INSERT fails | `start()` returns `Err` to the caller (HTTP 5xx, or due-sweep retries next tick). Nothing began. |
| Deserialize error / divergence poison (§5) | Dead-letter the job + `counter--` + alert. It's a code/modeling bug; retrying loops forever. |
| `emit` fails transiently (log backend down) | Requeue with backoff + `counter--`; after `MAX_START_ATTEMPTS` → dead-letter. |
| Crash in the admit→emit gap | Reaper re-emits (idempotent); after K failures → dead-letter + `counter--` (rule #10). |

The counter is **always decremented** on a start failure, because no terminal
fact will ever come to decrement it.

### Domain B — execution failures (root ran; a reactor failed)

causal owns the retry; on exhaustion it produces a **terminal fact**, which the
queue treats as **completion** (slot freed, job `failed`, NOT auto-restarted):

1. A reactor's `react()` returns `Err`. Its `RetryPolicy` retries per the failure
   class (transient backoff vs. fast-fail) up to `max_attempts`.
2. On exhaustion, `on_terminal_failure(TerminalFailure)` synthesizes
   `WorkflowFailed { workflow_id, consumer, class, error, attempts }` — appended
   by the runner directly (no `emit`, no queue routing), carrying the run's
   `workflow_id`.
3. `WorkflowFailedReleaser` folds it → marks the job `failed`, records the error
   for the admin view, **decrements the counter**. The slot frees.

So a crashed reactor does not leak a slot, and does not silently re-run.

### The bridge, and the two retry *levels*

The single idea tying the domains together: **a terminal-failure fact is
completion as far as the queue is concerned.** That is what makes rule #3
load-bearing — fold terminal facts, or execution failures leak slots.

There are therefore two distinct retry levels, and they must not be confused:

| Level | Who | Granularity | On exhaustion |
|---|---|---|---|
| **Reactor retry** | causal `RetryPolicy` | one step, *within* a run | `on_terminal_failure` → terminal fact |
| **Workflow re-run** | RootSignal policy | a whole new run | dead-letter / operator |

A workflow re-run is a **fresh enqueue** with a new epoch'd `workflow_id` — never
a re-emit of the same root (causal already exhausted the in-run retries; the same
root would just re-run the steps that already succeeded). This can be automatic
(a reactor on `WorkflowFailed` re-enqueues, bounded to K attempts with backoff)
or manual (admin re-trigger). Compensation, if needed, is ordinary causal: a
reactor that reacts to `WorkflowFailed`.

### The one genuine leak vector

A workflow that *neither completes nor terminally-fails* — e.g. a reactor stuck
in **transient-class retry against a permanent outage** (causal's transient class
uses ceilinged backoff and does not declare terminal failure). No terminal fact
→ no decrement → the slot is held. Two mitigations, both RootSignal-side:

1. **Bound transient retries** — give queued workflows' reactors a finite
   `max_attempts` so even transient exhaustion fires `on_terminal_failure`. This
   is the clean fix and needs no timeout.
2. **A per-type execution SLA reaper** as a backstop: after `T_max` in `running`,
   mark `stuck` + decrement + alert. Use with care — decrementing a *still-running*
   workflow under-counts and can briefly exceed the cap; it is an ops alarm, not
   routine flow.

### What the developer / operator sees

A job's lifecycle is a small status enum, all of it queryable for the admin view:

```
queued ──admit──▶ running ──completion fact──▶ done
   │                  │     ──terminal fact───▶ failed   (error recorded)
   │                  └─────start failure──────▶ dead_letter (error recorded)
   └──(never admitted: cap saturated → visible backlog, alert on age)
```

The API caller polls `workflow_id`: `queued` (intent, not yet begun — rule #7) /
`running` / `done` / `failed`. The reactor author writes `react() -> Result<…>`
and configures `on_terminal_failure` *once*; they never touch the queue.

---

## Constraint scorecard

The six constraints + two non-goals from the brief, against Design A:

1. **Durability separates schedule-time from begin-time across processes.**
   RootSignal extracts `(event_type, payload, workflow_id)` via public `Event`
   APIs, stores them in Postgres, reconstructs the typed fact on any process via
   `match event_type → from_value`, and calls `emit`. No in-memory handle
   bridges the gap. ✓
2. **Recursion hazard.** `emit` does not route, so "request" (facade INSERT) and
   "execute" (`engine.emit`) are already distinct operations in distinct code.
   The hazard does not exist. ✓
3. **`settled()` semantics.** The request/wait split lives in RootSignal:
   `workflows.start()` returns a handle; waiting is separate (completion-fact
   projector). causal's `.settled()` is untouched — it cannot be chained onto an
   enqueue, so it never lies (§0). ✓
4. **causal knows what a root is / enforce no-bypass.** causal knows
   (`declared_workflow_id`, public). Enforcement moves to the RootSignal facade.
   ✓ (with the trade flagged above)
5. **Deserialization stays in RootSignal.** It is the only place it can be;
   causal never deserializes facts. ✓
6. **`emit` for chain members untouched.** `emit` is untouched for *everyone* —
   members and roots both just append. Reactor outputs untouched. ✓

Non-goals: no queue/priority/Postgres types in causal (nothing enters causal);
no backpressure inside a running workflow (only the start is gated — once `emit`
appends, the chain runs uninterrupted). ✓

---

## Feature-to-layer map

| Feature | Lives in |
|---|---|
| Concurrency cap per job type | dequeue SQL — RootSignal |
| Priority (manual jumps queue) | `priority` column + `ORDER BY` — RootSignal |
| Retries + backoff | attempt counter + `available_at` — RootSignal |
| Dead-lettering | status transition after N attempts — RootSignal |
| Heartbeat / stale detection across instances | `heartbeat_at` + reaper — RootSignal |
| Admin view | `SELECT … FROM jobs` — RootSignal |
| Cross-instance start dedup | `ON CONFLICT (workflow_id)` — RootSignal |
| **Append root + run chain** | `engine.emit(fact)` — **causal** |
| **Idempotent re-execution** | `.event_id(derive(workflow_id))` — **causal** |
| Hold slot until workflow done | completion-fact read-model — RootSignal (causal log) |

causal touches three cells.

---

## Rules RootSignal must honor (surfaced by the pressure test)

The causal change is tiny, but the design is only correct if RootSignal's queue
honors all of these. None require anything from causal beyond the two
additions above.

1. **Status ordering** (§1) — `running` + cap-increment commit *before* the
   append (cap enforcement); terminal status is driven *only* by the completion
   fact (never set speculatively); a reaper reconciles the admit→emit gap.
2. **Cap by atomic admission, not by blocked workers** (§2) — increment a
   counter in the same statement that checks `running < cap`; decrement on the
   completion fact. Never hold a transaction/connection across a workflow.
3. **Fold terminal-failure facts as terminal**, not just success facts (§3) —
   or failed workflows leak their slot. causal *guarantees* a terminal fact
   exists (`engine.rs:706-733`); RootSignal must consume it.
4. **Epoch the `workflow_id`** so dedup protects against double-starts without
   forbidding legitimate re-runs (§4) — an explicit `WorkflowIdReusePolicy`
   choice, not a default.
5. **Re-emit the stored payload verbatim** (§5) — `from_value(job.payload)`,
   never rebuild the fact from columns with a fresh `Utc::now()` in a payload
   field. Verbatim re-emit makes the divergence-checked fields (`payload`,
   `event_type`, `workflow_id`, `causation_id`) identical by construction;
   `created_at`/`metadata` are exempt (`memory_store.rs:416-433`). Rebuilding a
   payload field from the wall clock would poison-loop.
6. **Dedup window ≥ max redelivery delay** (§4/§5) — job-row retention (and the
   backend's append-dedup scope) must outlast the slowest possible retry, or a
   late redelivery becomes a genuine duplicate. Verify the backend dedups
   against durable history, not a TTL cache.
7. **Status model distinguishes `queued` / `running` / `done`** (§1) — the
   facade returns a `workflow_id` immediately, but the ROOT may not exist for an
   unbounded time. Callers must read "queued" as intent, not as "running."
8. **Keep the admit transaction tiny** — commit before the `emit` I/O; never
   hold a row lock across the append (§7). Set
   `idle_in_transaction_session_timeout`; archive `done` rows off the hot table.
9. **Pick a starvation mitigation** (aging or reserved capacity) before priority
   ships (§6).
10. **The reaper closes only the admit→emit gap** — re-emit (idempotent) if a
    `running` job's root never landed; it must *not* touch the counter (the
    completion fact owns the decrement) (§3).
11. **Confine the `Engine` handle to the worker crate** (§8) — the API layer
    gets the `workflows` facade only, so the cap can't be bypassed by a stray
    `engine.emit(root)`. (Or decide on mechanism-level enforcement — §8.)

---

## Adversarial pressure test

Eight kill-attempts against the design, grounded in the causal codebase and in
durable-queue best practice. Each is rated **survives** (design holds),
**design change** (the attack forced a change, now folded in), or **killed**.
External citations are being gathered by a parallel best-practices pass and will
be appended; the engineering content below stands on its own.

### 1. Dual-write hazard — "you need a transactional outbox"

**Attack.** The job-INSERT (Postgres) and the event-log append (worker, later,
possibly KurrentDB) are *different stores*. Classic dual-write: one commits, the
other doesn't, and you've lied to someone.

**Verdict: survives — and the attack reinforces the design.** Decompose the two
writes:

- **Request side writes ONE store.** `workflows.start` only INSERTs a job row;
  it never touches the event log. So there is *no* dual-write at request time.
  And because the job table lives in RootSignal's *own Postgres*, the INSERT can
  be wrapped in the **same transaction** as any domain mutation the GraphQL
  handler makes (`BEGIN; UPDATE sources …; INSERT INTO jobs …; COMMIT`). That is
  the outbox pattern's payoff (atomic state-change + intent) achieved for free,
  *because the queue is co-located with the domain* — a property a separate
  queue (SQS/Redis) would have to recover with an actual outbox. Keeping the
  queue in causal would have *lost* this.
- **Execution side is the only dual-write** (append to log + job status). The
  ordering is subtle and the external pass corrected my first version:
  - `running` is marked (with the cap increment) **before** the append — it
    *has* to be, or the cap isn't enforced (you'd append N roots before any
    count moves). So there is a window: admitted, counted, root not yet in the
    log. A **reaper** closes it (running job whose root never landed within T →
    re-emit; idempotent, so safe).
  - `done`/`failed` is marked **after** the append — in fact *downstream* of it,
    driven by the workflow's completion fact (the projector). So Brandur's
    nefarious "job marked done but the event never happened" case is structurally
    impossible: terminal status requires completion, which requires the append.

The derived-id + idempotent append is sufficient; no outbox is required. The
hard rules: **cap-increment + `running` precede the append; terminal status is
driven only by the completion fact (never set speculatively); a reaper
reconciles the admit→emit gap.** (Brandur, *Transactionally-staged job drain* —
co-locating the queue with the domain DB removes the enqueue-side dual-write
entirely.)

### 2. Slot-holding as concurrency control — "blocking a worker per workflow is an anti-pattern"

**Attack.** The first sketch had each worker `emit` then **block** on
`await_workflow_done` before freeing the slot. A workflow runs for minutes
(LLM/scrape) and asynchronously, possibly on another instance. Blocking a task
for the whole duration wastes a task per in-flight workflow, couples slot
accounting to *worker* liveness (restart mid-workflow → slot stuck until a
reaper guesses), and the naive cap check (`WHERE (SELECT count(*) … running) <
cap`) is a **race**: two dispatchers both read `cap−1` and both admit → `cap+1`.

**Verdict: design change (folded in above).** Replaced with **admission
control**: an atomic counter incremented at admit (one `UPDATE … SET running =
running + 1 WHERE running < cap RETURNING …` — race-free by construction) and
decremented by the workflow's **completion fact** via a projector. No blocked
worker; slot accounting is fact-driven and *independent of worker liveness*.
This simultaneously kills the race (atomic admit), the wasted tasks (no
blocking), and the restart-leak (count lives in a row, not a task). This is the
token-bucket / semaphore admission pattern; durable-execution systems
(Temporal et al.) likewise cap *task* execution rather than parking a worker for
a whole workflow. The blocked-worker model is retired.

### 3. The completion fact never arrives — "the slot leaks"

**Attack.** Admission control hinges on a decrement that only fires when the
workflow completes. A workflow that errors out and emits no completion fact
leaks its slot forever; eventually `running == cap` permanently and the type
wedges.

**Verdict: survives — *because* of an existing causal guarantee, but it imposes
a hard requirement.** causal's terminal path is mandatory (`engine.rs:706-733`):
every workflow reaches *either* its success-completion fact *or* an
`on_terminal_failure`-synthesized terminal fact. So a decrement signal always
eventually exists. The requirement this places on RootSignal: its
`workflow_status` projection **must fold terminal-failure facts as terminal**
(decrement + mark `failed`), not just success facts. Miss that and §3 becomes
real. (causal's own docs already flag the symmetric "workflows leak / busy-gate
forever" hazard — same root cause.) The admit→emit gap (admitted, counter
incremented, but the process died before `emit` landed the root) is the *only*
window the completion guarantee doesn't cover; a reaper closes it by checking
"running jobs whose root isn't in the log after T → re-emit," which is safe
because re-emit is idempotent.

### 4. `ON CONFLICT (workflow_id) DO NOTHING` — "legitimate re-runs are silently dropped"

**Attack.** If `workflow_id = derive(source_id)` (stable, no time component),
the first scan of source X runs and completes; the next due-sweep wants to scan
X again, INSERTs the same `workflow_id`, hits `ON CONFLICT`, and is **dropped**.
X never re-scans. The dedup that protects against double-starts also forbids
re-runs.

**Verdict: survives with a stated modeling rule.** This is exactly Temporal's
`WorkflowIdReusePolicy` problem. The `workflow_id` must encode an **epoch**:
`derive(source_id, sweep_epoch)` (or run-window). Then a re-request *within* an
epoch dedups (the protection we want), while the *next* epoch is a new
`workflow_id` (the re-run we want). The epoch granularity is a RootSignal
decision with teeth: too coarse forbids timely re-scans, too fine defeats dedup.
For "allow re-run only after the prior one finished," the alternative is
`ON CONFLICT … DO UPDATE … WHERE jobs.status='done'` (reuse-on-completion) rather
than a new id. Either is fine; it must be *chosen*, not defaulted. causal is
unaffected — `declared_workflow_id` reads whatever field RootSignal derived.

> **UPDATED by verification.** RootSignal's roots use `Uuid::new_v4()`, not a
> derived id, so `ON CONFLICT (workflow_id)` never fires at all — the epoch advice
> is moot. The real mechanism is **entity-key** uniqueness
> (`UNIQUE (entity_key) WHERE status IN ('queued','running')`); the `workflow_id`
> stays a v4, minted/stored at enqueue, re-emitted verbatim by the worker. See
> "Verification findings."

### 5. Idempotent re-emit isn't byte-identical — "dedup silently keeps stale bytes"

**Attack.** Append dedups on `event_id`, but a reused `event_id` must carry
**byte-identical** bytes, and causal **errors loudly, always** on a divergent
redelivery — it does not silently keep-first (`memory_store.rs:453`, test at
`:674`; a deliberate 0.10 fix). The external pass flagged byte-identity as the
headline idempotency foot-gun (Stripe and AWS Powertools both *surface*
key/payload mismatch rather than keep-first), and noted the worker has **no
fresh-key escape hatch** — it reuses `derive(workflow_id)`, so it cannot recover
by minting a new key.

**Verdict: survives — and the real rule is narrower than my first pass claimed.**
I initially called a volatile `created_at` a poison loop. Reading the divergence
check corrected me: it compares **`payload`, `event_type`, `workflow_id`,
`causation_id`** and **exempts `created_at` and `metadata`** (`memory_store.rs:416-433`).
So `created_at` *cannot* trip it — `occurred_at = None` is fine. The actual rule
is about *how the worker rebuilds the fact*:

> **The worker must re-emit the STORED payload verbatim** —
> `serde_json::from_value(job.payload)` — never reconstruct the fact from job
> columns with a fresh `Utc::now()` in a payload field. Deserialize-verbatim
> makes the four compared fields identical by construction (the payload was
> frozen at request time), so the retry dedups. *Reconstructing* a payload field
> from the wall clock is what would trip the divergence check on every retry and
> poison-loop.

The dispatch sketch already does the right thing (`from_value`). Declaring
`occurred_at` from a stored field is still good practice for time-semantics, but
it is **not** load-bearing for idempotency — `created_at`'s exemption and
verbatim re-emit cover it. No causal change. One thing still to verify: the
durable backend (Pg/Kurrent) must dedup against **durable history**, not a TTL
window, or a delayed redelivery past the window re-appends a genuine duplicate.

### 6. Priority starvation — "manual triggers starve discovered scans forever"

**Attack.** `ORDER BY priority DESC, created_at` means a sustained stream of
high-priority manual triggers indefinitely starves low-priority discovered
scans. The crawler's discovery backlog never drains.

**Verdict: survives — known foot-gun, RootSignal's to tune.** Standard
mitigations: **aging** (effective priority rises with wait time) or
**reserved capacity** (per-priority sub-caps so low-priority always has *some*
slots). Out of scope for causal; flagged so the queue design picks one
deliberately rather than discovering starvation in production.

### 7. `FOR UPDATE SKIP LOCKED` foot-guns — "table bloat and lock pileups"

**Attack.** High-churn status updates bloat the table and its indexes; a
dispatcher holding a transaction open across the `emit` (network I/O to the log)
holds row locks for the I/O duration; autovacuum can't keep up under load.

**Verdict: survives with implementation rules — and this is the failure mode the
admission change (§2) exists to prevent.** The external pass made the mechanism
concrete: a transaction left open across the workflow (the *original*
slot-holding model) is the documented queue-killer — Brandur measured a single
long transaction leaving ~100k dead tuples in the queue index, after which "every
worker trying to lock a job would cycle through this loop 100,000 times" and lock
times rose ~15× (autovacuum cannot reclaim tuples visible to an open
transaction). Slot-holding *also* pins one DB connection per in-flight workflow →
pool exhaustion at `pool_size` concurrent workflows, starving OLTP. Admission
control (§2) removes both: the admit transaction is a single fast `UPDATE` that
**commits before** the `emit` I/O — no transaction, row lock, or connection is
held across a workflow. Remaining hygiene: partial index on `status='queued'`,
archive/partition `done` rows off the hot table, set
`idle_in_transaction_session_timeout`, tune autovacuum. None touches causal.

### 8. Framework/application boundary — "the cap is advisory, not enforced" *(RESOLVED by verification)*

> **RESOLVED.** Verification killed structural confinement (22 non-root api
> emits can't move) and the answer is neither "confinement" nor a causal trait:
> a RootSignal **wrapper** (`ScoutEngine`) routes on `declared_workflow_id`, with
> a `begin`/`execute` bypass and a `RunCancelled` exemption. Mechanism-level,
> non-bypassable (api only holds the wrapper), and causal stays at `event_id`.
> See "Verification findings." The analysis below is retained for the reasoning.


**Attack.** This is the external pass's strongest, and it partially dents my
verdict. Mechanism/policy separation (Hydra; "smart endpoints, dumb pipes")
supports keeping the *policy* — what the cap is, what jumps the line — in the
app. **But** if the cap is enforced *only* in the RootSignal facade, it is
**advisory**: `engine.emit(root)` stays callable (the worker needs it), so any
code path that appends a ROOT directly bypasses the cap and the budget blows
again. And Hydra is explicit that the *one* policy that must live at the
mechanism layer is "arbitrating conflicting requests for physical resources … to
the extent of guaranteeing fairness" — which is exactly what a concurrency cap
is. Oban, River, and Temporal all put the limit at the mechanism layer precisely
so it can't be bypassed.

**Verdict: survives on the *policy* axis; the *enforcement* axis is a genuine
trade, and it's the open decision.** Two ways to make the cap non-bypassable:

- **Structural confinement (Design A as written).** Enforcement = encapsulation:
  the worker crate is the *only* holder of `Engine`; the API layer holds only the
  `workflows` facade. There is no `emit` in scope to bypass. This is stronger
  than "advisory" (you can't accidentally call what you weren't given) but weaker
  than a mechanism guarantee (a future dev *could* wire the engine somewhere it
  shouldn't be — closed by the §9 guard test). It keeps the causal surface at one
  additive method and the recursion hazard gone.
- **Mechanism enforcement (a causal-side admission hook ≈ Design B).** causal
  refuses to append a ROOT except through an admission gate. This is
  non-bypassable by construction — but it is routing-in-`emit` again, which drags
  back the recursion hazard and the second verb (`begin`) the whole simplification
  removed. It buys a hard guarantee at the cost of the simplicity you asked for.

For one team owning both crates, **structural confinement is almost certainly
enough** — and it's the only option consistent with "no `begin`, keep it
simple." But it *is* a softer guarantee than the prior art uses, so it's yours to
ratify rather than mine to assume. (Decision flagged below.)

### Open production risks (staff review — not closed by the design)

These survive as *implementation obligations on RootSignal*. None changes the
causal surface; all three fail **silently** if missed, which is why they're
called out rather than left to discovery.

#### 9. Enforcement rots, and there is a second bypass

"Confine the `Engine` to the worker crate" (§8) is a *social* guarantee — over
time someone wires an `Arc<Engine>` into a debug endpoint or a new feature and
the cap silently goes advisory. **Worse: a reactor that emits a workflow root
bypasses the queue entirely** — reactor outputs are appended by the runner, not
through `emit` (`reactor_runner.rs:1269`), so an inline-spawned child workflow
never consumes a slot. Today discovery is mediated by the due-sweep so this is
latent; it is the same hole as a stray `engine.emit`. **Mitigation (required,
not convention):** a CI/architecture test asserting (a) no root `event_type` is
emitted outside the dispatcher, and (b) no reactor's output set declares a root.

#### 10. Per-workflow completion wiring is forgettable → silent slot leak

causal has no ambient "workflow done"; each workflow must *announce* its
completion fact for a `SlotReleaser` to fire. The failure path is uniform
(`on_terminal_failure` → one `WorkflowFailed`), but the **success path is
per-type** and asymmetric: add workflow #7, forget its completion fact or its
projector, and that type wedges its cap with nothing failing. **Mitigation:** a
uniform `WorkflowCompleted { workflow_id, job_type }` that every workflow's
terminal reactor emits (joining the run via a declared `workflow_id`), so slot
release is **one** projector for all types, not 2N; plus a test that every
`WorkflowRoot` has a registered terminal handler. The SLA reaper (§3) is the
backstop, not the guarantee.

#### 11. No backlog/staleness story — and this is a crawler

The cap bounds *execution*, not the *queue table*. With discovery outpacing
completion (the steady state for a crawler), `queued` grows monotonically: table
bloat, slower dequeue, and scans queued weeks ago that are worthless to run.
"Unbounded discovery is intended" makes this **more** urgent. **Mitigation:** a
queued-job TTL (drop/refresh stale `queued` rows), a max-depth or per-type queue
ceiling with an age alert, and a fresh-vs-stale ordering policy. Scope before the
crawler runs at volume.

### What the pressure test moved

The causal-side surface was **not** moved by any attack — it remains the single
additive `.event_id()` *(unless §8 is decided toward mechanism enforcement, which
reopens the causal-side hook)*. The headline holds: the risk
concentrates almost entirely in RootSignal's queue design (admission control,
status ordering, epoch'd ids, terminal folding, byte-identity, Postgres
hygiene). Every attack that forced a change (§1 ordering, §2 admission control,
§3 terminal folding, §4 epoch'd ids, §5 poison-loop rule) landed on RootSignal —
which is itself the strongest evidence the boundary is drawn in the right place.
The two independent passes agreeing on admission control is the second.

---

## Pressure test — against RootSignal's actual requirements

The passes above used generic best practice. This one uses RootSignal's real
shape, grounded in `docs/plans/2026-06-12-rootsignal-migration-map.md` (consuming
crates: `rootsignal-api`, `rootsignal-graph`, `rootsignal-scout`,
`rootsignal-scout-supervisor`) and the brief. Two findings change the verdict;
the rest are requirement-specific obligations.

### R1 — Reactor-emitted detached roots BYPASS the queue *(scope risk — the big one)*

The brief says all six roots start at the API layer (`engine.emit(X).settled()`
in the due-sweep / GraphQL). But the migration map, point 4, says otherwise for
*slow labor*: **"enrichment-style facts declare `workflow_id = "<field>"` (value
via `ctx.derive_id`) so they stop holding their parent's settle hostage —
candidates: the media-enrichment dispatch path."** `ctx.derive_id` is a *reactor*
method: those roots are minted and emitted **inside a reactor**, as reactor
outputs — appended by the runner (`reactor_runner.rs:1269`), never through
`emit`/the facade. **Design A does not route them, so they escape the cap.**
`ActorEnrichmentRequested` (one of the six) is exactly this shape.

So the unbounded-execution problem is **not fully solved** by an API-layer
facade: discovery/enrichment that dispatches detached children from reactors
keeps blowing the budget. This is §9's reactor-child-root bypass, *confirmed as a
live RootSignal pattern, not hypothetical*.

**Resolution (a RootSignal refactor, required):** a reactor that dispatches a
detached child must **enqueue** it (call the `Workflows` facade) instead of
emitting the root inline. The queue *subsumes* the detach pattern — enqueuing is
detached AND capped. The enqueue is a reactor side effect, so it must be
idempotent: derive the child `workflow_id` via `ctx.derive_id` (as today) and
`ON CONFLICT (workflow_id) DO NOTHING` makes redelivery a no-op. This needs the
facade in reactor `deps` (not just the API layer), and the §9 guard test ("no
reactor output declares a root") becomes the thing that *enforces* the refactor
stuck. Verify which of the six are reactor-emitted before sizing anything.

### R2 — Enforcement is not free: api/graph hold the Engine for reads *(may flip §8)*

The §8 recommendation (structural confinement: "only the worker holds the
`Engine`") assumed the API layer needs nothing from the engine. The migration map
refutes that: `engine.state_of` / `engine.snapshot` / `load_aggregate` total ~19
engine-level read sites in `rootsignal-api` / `rootsignal-graph`. **Those crates
already hold the `Engine`** — so raw `engine.emit(root)` is in scope there, and
the facade is advisory, not enforced.

Confinement is still achievable, but it costs more than stated: wrap the engine
in a **read-only facade** (`state_of`/`load` only) for api/graph, expose
`Workflows` for starts, and keep the raw `Engine` in the worker/scout crates
only. If any api/graph path legitimately needs general `emit` (non-root facts),
confinement breaks and the **causal-side mechanism hook (§8 option B) becomes the
honest choice for RootSignal**. Decide §8 *after* auditing what api/graph emit —
not before.

### R3 — The start-cap is a coarse proxy for the budget; cap spend with `max_in_flight`

"Blows our budget and resources" is about LLM/Neo4j/HTTP spend (migration map:
"Neo4j/HTTP/LLM-infra calls"). Capping workflow *starts* does not cap that: one
`SourceScan` fans out to many expensive reactors, so N concurrent scans = N×M
concurrent LLM calls. The direct lever is causal's existing per-reactor
**`max_in_flight`** on the expensive reactors (a global semaphore on that
reactor, *not* per-workflow — so it does not violate the "no backpressure inside
a workflow" non-goal). **Use both:** the queue caps workflow-level parallelism
(memory, coordination, fairness); `max_in_flight` caps the spend. Sizing the
start-cap alone, without `max_in_flight`, either starves or blows the budget
depending on fan-out.

### R4 — "rootsignal-jobs independent of causal" forces a two-layer split

The brief requires the queue crate to be **independent of causal**. Design A's
glue — the facade's `serde`, the dispatch `from_value` + `emit`, the completion
`Projector` — *depends* on causal. So the layering must be explicit:

- **`rootsignal-jobs`** (no causal dep): generic Postgres queue —
  `enqueue(job_type, payload, priority, dedup_key)`, `admit_one() -> Job`,
  `mark_done/failed`, lease/heartbeat, reaper, the `type_slots` cap.
- **glue layer** (`rootsignal-workflows` or in api/scout; depends on both): the
  `WorkflowRoot` marker, the facade, the dispatch match, the slot-release
  projectors, `on_terminal_failure`.

Put the `emit`/`Projector` into `rootsignal-jobs` and you've violated the
requirement. The "what to build" list below is split accordingly.

### R5 — Per-source serialization vs the due-sweep re-firing

`ON CONFLICT (workflow_id)` dedups *exact* retries, but with epoch'd ids (§4) a
due-sweep that re-ticks every 60s re-enqueues a still-pending source under a
*new* `workflow_id` each tick → duplicate jobs for the same source pile up. The
natural unit RootSignal wants is **at most one active scan per source** — a
*different* dedup from the retry-dedup. Add a partial unique index on the entity
key for active rows (`UNIQUE (source_id) WHERE status IN ('queued','running')`),
or have the due-sweep skip sources with an active job. Two dedup levels:
`workflow_id` (retry) and entity-key (don't double-process the entity).

### R6 — The job payload is a rolling-deploy wire format

Server A (old binary) enqueues; server B (new binary) dequeues minutes later
(constraint #1). The stored payload must `from_value` cleanly across that skew —
so root-fact serde must be **additively evolution-safe across one deploy**, or
in-flight jobs dead-letter on dequeue. Same discipline the migration map already
demands for the durable curiosity restream, now applied to the (short-lived) job
payload.

### R7 — "Manual triggers jump the queue" ≠ preempt *(resolved)*

Priority ordering puts a manual trigger at the front of the queued set, but a
saturated cap still makes it wait. **Resolved (decision #2 above): default is
queue-and-forget** (matches current fire-and-forget behavior — no reserved
capacity needed), **plus a `run_now` flag** that admits a specific job ignoring
the cap (but not the entity-uniqueness invariant). So no standing reserved lane;
the override is per-job and operator-gated.

### R8 — Confirmation: keeping `.settled()` was right for RootSignal

The migration map counts **52 `settled()` sites** in RootSignal, and per-workflow
settle was already migrated in the 0.10 pass. Removing `.settled()` (the rejected
draft) would have re-churned all of them; keeping it (§0) leaves RootSignal's
workflow-test path untouched — only the production call sites move to
`workflows.start()`.

### Net

R3–R8 are obligations that don't move the architecture. **R1 and R2 do.** R1 says
the facade seam is *insufficient on its own* — reactor-dispatched detached roots
must be refactored to enqueue, or the cap has a hole exactly where the budget
leaks. R2 says the §8 enforcement decision must wait on an audit of what api/graph
emit. Neither changes the causal surface (`event_id` still the only addition);
both are real work and a real risk that the API-layer-only framing hid.

---

## RootSignal: what to build

Everything queue-shaped lives here, split across **two crates** per the
"independent of causal" requirement (R4): `rootsignal-jobs` = the generic Postgres
queue (no causal dep — items 3, parts of 4, the reaper, dead-letter); a **glue
layer** (depends on both) = the workflow-specific items (facade, marker, dispatch
match, projectors, `on_terminal_failure`). Grouped by when it's needed.

**Correctness (before any traffic):**

0. **`ScoutEngine` → a routing wrapper** (resolved §8 / R2) — change the bare
   `type ScoutEngine = causal::Engine` alias (`engine.rs:142`) into a struct
   holding a private `Arc<Engine>` + the queue. `emit` routes roots
   (`declared_workflow_id().is_some()`, **except `RunCancelled`** — cancel
   exemption) to the queue and passes members through (the 22 admin emits
   untouched); a worker-only `execute()` calls `inner.emit(..).event_id(..)` with
   no routing. api holds only the wrapper → non-bypassable, and existing emit
   sites don't change. R1 verified **no reactor emits a root**, so reactor deps
   need nothing; the §9 guard test stays as a regression fence.
1. **Entity-key dedup** — `UNIQUE (entity_key) WHERE status IN ('queued','running')`,
   keys per type (`source_id`; `region_id+flow_type`; `region_id`; `group_id`;
   `actor_id`; `file_id`). Replaces RootSignal's racy/missing guards
   (`is_source_busy` is TOCTOU; feed/actor have none). `workflow_id` stays
   `new_v4()`, minted+stored at enqueue, re-emitted verbatim by the worker.
2. **`WorkflowRoot` marker trait** + one impl per root type + `priority()`.
3. **Schema** — `jobs` (workflow_id, job_type, payload, priority, **`run_now`**,
   `entity_key`, status, timestamps, attempts, error) + a `type_slots` counter row
   per job_type + the partial unique index `UNIQUE (entity_key) WHERE status IN
   ('queued','running')` (decision #1/§4). `cap(job_type)` is a config row, not a
   constant.
4. **Dispatcher** — `admit_one()` (atomic cap-check + counter++ + `running`, one
   statement, committed before emit); **`run_now` jobs skip the cap gate** but
   still increment the counter and still honor the entity-key index (decision #2);
   dispatch match (`from_value` **verbatim**, rule #5) →
   `engine.emit(f).event_id(derive_root_event_id(wf))`; start-error handling
   (requeue+backoff / dead-letter / poison, §"Domain A").
5. **Two slot-release projectors** — `SlotReleaser` over the uniform
   `WorkflowCompleted`, and one over `WorkflowFailed`; both call the idempotent
   `free_slot` (rule #10).
6. **`on_terminal_failure` mapper** → `WorkflowFailed { workflow_id, .. }` (rule #3).
7. **Uniform `WorkflowCompleted`** emitted by every workflow's terminal reactor
   (risk #10) — joins the run via a declared `workflow_id`.
8. **Engine confinement** — the worker crate is the only holder of `Engine`
   (risk #9).
9. **Bypass guard test** — no root emitted outside the dispatcher/facade; no
   reactor output declares a root (risk #9, enforces item 0 / R1).
10. **Admit→emit reaper** — re-emit (idempotent) a `running` job whose root never
    landed; never touches the counter (rule #10).
11. **Per-entity active-uniqueness** (R5) — `UNIQUE (source_id) WHERE status IN
    ('queued','running')` (or due-sweep pre-check) so the sweep re-firing a
    still-pending source doesn't pile up duplicate jobs. Distinct from the
    `workflow_id` retry-dedup.
12. **`max_in_flight` on the expensive reactors** (R3) — the real budget lever
    (LLM/Neo4j). The queue caps workflow-level parallelism; `max_in_flight` caps
    spend. Size both.

**Before volume:**

13. **Backlog policy** — queued-job TTL, per-type queue ceiling + age alert,
    fresh-vs-stale ordering (risk #11).
14. **Starvation mitigation** — aging so low-priority discovery work isn't starved
    by a flood of high-priority jobs (§6). Manual-promptness is *not* this — it's
    `run_now` (decision #2), so no reserved lane is needed.
15. **SLA reaper** — `running` > T_max → `stuck` + decrement + alert; ops alarm,
    not routine (§3, risk #10).
16. **Status read-model** — `queued`/`running`/`done`/`failed` for the API to
    poll by `workflow_id` (rule #7) + the admin view.

The causal side provides exactly: `engine.emit(f).event_id(..)`, the guaranteed
terminal fact (`on_terminal_failure`), `Projector` idempotency, and `settle` for
single-instance tests. Nothing else.

## Fault-injection test matrix (for the RootSignal queue)

The implementation should be validated against these, not just unit-tested
happy-path. (Adapted from the external pass's matrix.)

| # | Inject | Assert |
|---|---|---|
| 1 | Kill the dispatcher between `engine.emit` and the next admit | Exactly one ROOT in the log after re-lease (idempotent `event_id`) |
| 2 | Make the append fail after a job is marked `running` | Reaper re-emits; job not silently stuck |
| 3 | Re-emit with the same `event_id`: (a) stored payload verbatim → dedups to one event; (b) a payload field rebuilt from `now()` → errors as divergence (proves the verbatim rule, #5) |
| 4 | Delay a redelivery past job-row retention / dedup window | No duplicate ROOT (rule #6) |
| 5 | Call `engine.emit(root)` directly, outside the worker | Document whether the cap holds — the §8 enforcement boundary in practice |
| 6 | Kill the admitter mid-workflow, then drive new arrivals | Cap reflects real running count (no double-count, no leak) — slot ≠ watcher |
| 7 | Sustained load, realistic (long) workflow durations | Dead-tuple growth, lock-acquisition latency, connection/`idle_in_transaction` health stay flat (proves §2 removed the held-transaction) |
| 8 | Second legitimate start of a completed `workflow_id` | Re-run semantics behave as chosen (AllowDuplicate vs Reject) — not a silent drop (rule #4) |

## Sources

External best-practices pass (primary sources):

- **Outbox / dual-write:** microservices.io *Transactional Outbox*; Brandur,
  *Transactionally-staged job drain* (co-locating the queue with the domain DB
  removes the enqueue-side dual write); AWS Prescriptive Guidance.
- **Idempotency / exactly-once:** Stripe *Idempotent Requests* (surfaces
  key/payload mismatch); AWS Lambda Powertools idempotency; Temporal on
  deterministic crash-survivable idempotency keys; "you cannot have exactly-once
  delivery."
- **Postgres-as-a-queue:** Brandur, *Postgres queues* (the 100k-dead-tuple hot
  loop); Stormatics on idle-in-transaction bloat; Oban Pro *Smart Engine*
  (counted running-state, not held transactions); River / pg-boss / SolidQueue.
- **Durable-workflow start semantics:** Temporal `WorkflowIdReusePolicy` /
  `WorkflowIdConflictPolicy` (retention-bounded dedup); AWS Step Functions
  `StartExecution` (same-name-different-input → error).
- **Mechanism/policy boundary:** Hydra (Wulf et al.) on separating mechanism from
  policy — and that resource arbitration/fairness is the kernel's to keep;
  Fowler, *Smart endpoints and dumb pipes*; Google SRE, *Handling Overload*.

(Full URLs in the research transcript; this list is the citation spine for the
verdicts above.)

## Release

One causal minor (0.14): the single additive `EmitBuilder::event_id()`
(`.settled()` kept — §0) — *unless* §8 is decided toward mechanism-level
enforcement, which reopens a causal-side admission hook. Then per the release
flow: verify the RootSignal call sites compile against the new surface
statically, publish causal, bump RootSignal. No patching RootSignal to test.
