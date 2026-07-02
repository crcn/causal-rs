# Incident analysis — rootsignal crash-loop (2026-07-01)

**Status: analyzed; library fixes shipped in 0.18.0.** This note
records the *corrected* causal story, because the incident brief misattributed
two mechanisms and we don't want them re-investigated from the wrong premise.

## Symptom

rootsignal's API crash-looped with a **stack overflow** (16 MiB stack); its
reactor consumer appeared wedged, re-detecting the same divergence every drive
pass. Downstream self-mitigated by hand-creating `causal_reactor_divergences`
and canonicalizing HashMap serialization.

## Correction 1 — the "observer-tx wedge" does not exist in any published version

The brief's chain — "synchronous `PgReactorObserver` INSERT into
`causal_reactor_divergences`, in a shared transaction with cursor + execution
writes, aborts on the missing table → cursor never advances → permanent wedge"
— is **not supported by code at any version (0.14.0 → 0.17.1)**, confirmed by
git archaeology:

- `PgReactorObserver` has always been a **lossy async channel sink**: hooks
  `try_send` (`reactor_observer.rs`), a background `writer_loop` batches and
  writes, and flush errors were already caught + dropped with a warn. The hook
  **cannot error into the runner**.
- Cursor advance goes through `ReactorRunner::advance_floor` →
  `PgReactorCheckpoint::advance` (a standalone monotonic `GREATEST` upsert on
  its own pool). It shares **no transaction** with observer writes.

The *real* observer defect (fixed in **0.18.0**): `flush()` wrote all record
classes in one transaction, so a missing table poisoned the **whole batch**,
silently dropping co-batched execution/log rows — observability data loss, not
a cursor wedge. Plus the crate never owned its DDL (schema ownership, 0.18.0).

The re-detection symptom pattern ("same divergence every pass") matches a
**cursor reset / replay-from-behind-tip** ("divergence storm", see
`reactor_checkpoint.rs` `clamp_ahead_of` and `engine.rs` PITR-clamp), which the
0.17.0 heals already guard — *not* the missing table. If rootsignal still has
it, pull `causal_checkpoints` history for the wedged consumer around the
incident to confirm.

## Correction 2 — no recursion exists in causal's drive path

An exhaustive sweep found **no `Box::pin`/`#[async_recursion]`** on the drive
path; `settle`, `settle_tree`, `supervise_one`, `step` → `worker_loop` →
`process_trigger` → `attempt_trigger` are all iterative loops. A repeated
identical failure reuses one stack frame — it cannot overflow.

Prime suspect for the 16 MiB overflow: a **recursive `Display`/`Error::source`
cycle in rootsignal's own error type**, walked by causal's `format!("{e:#}")`
on the per-pass failure-logging path (expanded in 0.17.1's tracing work). One
format call recurses the cyclic `source()` chain until the stack overflows.

Causal-side defense shipped in **0.18.0**: `failure::bounded_error_chain` caps
the chain walk at 32 hops / 8 KiB and is used on all reactor retry/park/
supervisor logging sites; plus a small-stack regression guard
(`repeated_identical_failures_never_overflow_a_small_stack`). This bounds
chain-walk recursion (the field-observed shape). A single self-recursive
`Display` impl is not fixable from here — that remains a downstream bug. **Ask
rootsignal for the crash backtrace to confirm** (expect a repeating 2-frame
`Display::fmt`/`source` cycle under a `tracing`/`format!` frame).

## What actually shipped (all in 0.18.0)

- Observer fail-soft per-record fallback + `ensure_schema` /
  `INSPECTOR_SCHEMA_SQL` schema ownership (additive).
- H1 projector poison-park; H7 causation-depth ceiling (default-on, behavioral
  breaking); bounded error formatting + small-stack overflow regression guard.

## Not a library concern

- The full merge-gate/H5 live-PG validation the brief listed as outstanding was
  **already done** (cleared 2026-06-30, shipped 0.17.0). rootsignal needs only
  to be on ≥0.17.0 for the H3/H4/H5/H9 heals.
