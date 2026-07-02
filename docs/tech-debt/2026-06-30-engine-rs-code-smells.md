# Tech debt: `engine.rs` code-smell review

- **Date:** 2026-06-30
- **File:** `modules/causal/src/engine.rs`
- **Scope:** code-smell / maintainability review (not a feature audit)

## Framing: the "6410-line file" is misleading

Production code is only **lines 1–2762 (~2760 lines)**. Lines **2763–6410
(~3650 lines, ~57% of the file)** are the `#[cfg(test)]` module — 74 tests
inline. So this is not a 6400-line god-module of logic; it's a ~2760-line
module with an oversized inline test suite. The biggest single readability win
is moving the tests out (finding #5).

## Ranked findings

### 1. `std::sync::Mutex` guard held across `.await` — `engine.rs:1367-1383` — HIGH
In `build()`, `let mut fence = self.cancelled_workflows.lock().unwrap();` is
acquired and then held across `self.log.read_stream(...).await` (line 1368).
Holding a blocking (`std::sync`) mutex guard across an await point is the
canonical async deadlock/starvation hazard: the executor can park this task
while another task on the same worker thread waits on the lock. "Safe today"
only because this runs at build time before runners spawn — an accident of
timing, not a guarantee.
**Fix:** read the stream first, then lock, insert, drop. Check
`clippy::await_holding_lock` is not allow'd.

### 2. `execute_emit` is a god-function — `engine.rs:1828-2053` (~225 lines) — HIGH
One async fn does: empty-batch short-circuit, workflow-root resolution (two bail
paths), event_id/batch validation, metadata merge, per-fact OCC-guard +
envelope construction, same-stream run-chunking, atomic append per run, and
per-fact registry fold + observer notify + snapshot save. The fold block at
2013-2045 is nested 5–6 levels deep. This is the hot write path — a subtle
batching/atomicity bug would hide here.
**Fix:** extract `resolve_workflow_id(...)`, `build_pending(...)`,
`append_runs(...)`, and reuse the existing `fold_committed_fact_into_registry`.

### 3. Duplicated post-append fold logic — `engine.rs:1755-1796` (`append`) vs `1985-2046` (`execute_emit`) — MED/HIGH
The "fold each committed fact into the engine registry, compute
`revision - (n-1-i)`, notify observer, maybe save snapshot" block is
copy-pasted between `Engine::append` and `Engine::execute_emit`, with identical
load-bearing revision arithmetic — and the comments themselves admit it ("same
math as `Engine::append`"). Two copies of off-by-one revision math will drift.
Note `append`'s copy is **missing** the snapshot-save call that `execute_emit`
has at 2034-2042 — confirm that's intentional, not an omission.
**Fix:** extract a single `fold_committed_run(reg, facts, workflow, write)`.

### 4. Builder/wiring config sprawl with a parallel duplicate struct — `engine.rs:712-731`, `750-810` — MED
`EngineBuilder` has 21 fields and 19 `with_*`/registration methods.
`ConsumerWiring` re-declares 13 of the same fields, and `build()` hand-copies
each one (1407-1421). Many fields land a third time on `Engine` (1445-1485) plus
a 12-arg `Engine::start` (1489, annotated `#[allow(clippy::too_many_arguments)]`
— lint suppressed, not addressed). Three hand-synchronized field lists: add one
knob, touch four places. (Note: `ConsumerWiring` exists deliberately to fix an
ordering-dependent capture bug — the smell is the hand-copied mechanism, not the
intent.)
**Fix:** group the cross-cutting wiring (clock, observer, effect_store,
snapshot_store/every, failure_mapper, workflow_hw, cancelled_workflows, leasor)
into one shared `Arc`-able config struct.

### 5. Inline test module dominates the file — `engine.rs:2763-6410` — MED
3650 lines of tests inline. Cheapest large readability win: move to `tests/`
(integration) or a sibling `engine_tests.rs` via `#[path]`. Cuts the file to
~2760 lines.

### 6. Settle barrier mixes busy-poll sleeps with lock churn — `engine.rs:2215-2292` — MED
`settle` is a nested `loop { for consumer { loop { ... sleep(POLL_INTERVAL) } } }`
using `tokio::time::sleep` as the sync primitive (also `await_observed_by`,
2406-2412). Re-locks `workflow_hw` three times per outer iteration (2230, 2286,
2288). Polling is a documented, deliberate backend-agnostic choice — smell, not
bug — but it's an O(consumers × poll-interval) busy-wait with repeated lock
acquisition, and the termination argument lives only in a comment ("heat death
of the universe", 2247). Eventual right shape: notify/condvar or backend
LISTEN-style hook; at minimum collapse the triple-lock to one guard.

### 7. Best-effort fold swallows errors on the write path — `engine.rs:2078-2115` (callers 1775/2017) — MED
`fold_committed_fact_into_registry` catches every fold error, logs WARN, returns
`None`. The reasoning is sound for the engine-level read cache (self-heals), but
a broad `Err(e) => { warn; None }` on the emit/append path also hides genuine
fold bugs (panicking `Apply`, serialization regression) behind a WARN invisible
without a subscriber — the same invisibility the `ConsumerHealth`/wedge
machinery (139-185) was built to fix for runners.
**Fix:** classify — distinguish the expected "gap repair didn't converge" case
from unexpected fold failures and surface the latter.

### 8. `panic!` for configuration errors in library code — `engine.rs:1058-1065`, `1106-1115`, `2146-2151` — LOW/MED
`claim_name` asserts, `with_aggregators` panics on duplicate NAME, `state_of`
panics at runtime when no aggregators registered. Builder-time panics are
defensible; `state_of` panicking at *runtime* (2146) is harsher than returning
`Err`. The codebase already knows this tension — it promoted a `debug_assert!`
to a real `Err` in `append` (1726-1733) for exactly this reason. These panics
are the inconsistent remainder.

### 9. `Box<dyn Any + Send>` taken by reference — `engine.rs:2749` — LOW
`pub(crate) fn panic_payload_message(payload: &Box<dyn Any + Send>)` triggers
clippy `borrowed_box`; should be `&(dyn Any + Send)`.

### 10. Inconsistent field-name alignment — throughout, e.g. `engine.rs:481-511`, `572-579`, `750-810` — LOW
Column-aligned fields with irregular spacing (`trigger_id:   Uuid,` next to
`trigger_event_type: String,`). Cosmetic, but applied inconsistently and will
fight rustfmt / produce noisy diffs on any rename.

### 11. Comment-vs-code naming drift: "category" vs "subject" — `engine.rs:1920-1944`, `1627-1638` — LOW
Comments still say "category" / "routing category" / "STREAM category" while the
code uses `subject` / `subject_id` / `Event::SUBJECT`. Residue of a rename (see
also `lib.rs:128` referencing `extract_prefix`). Vocabulary has two names for one
concept; comments lag the code.

## What's good (do not "fix")

- `SettleTracker` pin/evict (314-396) and wedge-detection (114-185, 2263-2279)
  are carefully reasoned and well-tested; comments are load-bearing.
- `ConsumerWiring` deliberately fixes an ordering-dependent capture bug
  (704-711) — intent is correct.
- No `unwrap()`/`expect()` on fallible I/O in production paths; the 8
  `.lock().unwrap()` are mutex-poison unwraps (acceptable), the only concern
  being #1's placement.
- No TODO/FIXME/HACK markers, no commented-out code, no dead code in production.

## Recommended order of action

1. **#1** — lock-across-await (correctness).
2. **#3** — duplicated revision math (will drift; check the snapshot-save gap).
3. **#5** — extract tests (cheapest large readability win).
