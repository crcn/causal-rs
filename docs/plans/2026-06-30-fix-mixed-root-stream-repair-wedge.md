# Plan: Fix the mixed-root stream gap-repair wedge in Causal

**Status:** design only, no code applied yet (one RED test written).
**Target crate:** `causal` (workspace `causal-rs`), version bump `0.16.0 → 0.16.1` (patch; bugfix).
**File:** `modules/causal/src/aggregator.rs`, fn `repair_gap`.

This document is written to be pressure-tested by an adversarial reviewer. It states the
defect, the exact fix, the correctness invariant the fix must preserve, the proof
obligations, the rejected alternatives (with reasons), the test matrix, and the open
questions a skeptic should attack.

**Revision history:**
- *Rev 1* — initial plan after a three-way internal codebase audit.
- *Rev 2 (2026-06-30, incorporating OpenAI review)* — three changes: **(a)** the seed guard is
  now explicitly **strict-only** (`upto.is_some()`), a correctness requirement (§3, Obligation 8),
  because a non-strict seed would corrupt `engine.state_of`; **(b)** §9 operational guidance
  corrected — wedged consumers **self-heal on redeploy**; manual checkpoint advancement is an
  exceptional, data-lossy last resort, not the default; **(c)** new adversarial test 6 + open
  question Q6 probing whether the tail loop's global `gaps.is_empty()` advance-gate is too broad
  for "foreign-to-B / meaningful-to-C" streams (the likeliest remaining hole).
- *Rev 3 (2026-06-30)* — **the fix is now two coordinated changes, not one.** Q6 is resolved by
  *fixing* it rather than documenting it as unreachable: change **(b)** makes the tail-loop advance
  gate **per-key** (advance unless *this* aggregate's own key gapped) instead of global
  (`gaps.is_empty()`). Investigation confirmed the global gate's hazard is latent-only under
  today's runners (hydrate-from-ZERO + in-order full-fold keep peers caught up), but we are closing
  it now: it is a known correctness hazard, the per-key gate is strictly safe and restores the
  gate's own documented intent ("this entry"), and a tripwire comment would just defer a bug we
  already understand. Test 6b now asserts **convergence** under (b).

---

## 1. The defect (confirmed & reproduced)

`fold_event(..., strict_to_event = true)` is the fold entry point for **consumer** runners.
Its registries promise the invariant:

> **STRICT INVARIANT:** `state == fold(log[..cursor])` — the consumer's cached aggregate
> state equals the in-order fold of its own stream up to (but not including) the event
> currently being delivered.

When a consumer folds an event at revision `r` against a **vacant** aggregate entry, the
idempotency gate sees `version == ZERO` and emits `Gap { expected: 0 }`. Repair
(`repair_gap`) then, in strict mode, **skips** the snapshot/restore fast-path (guarded by
`upto.is_none()`, which is false in strict mode) and instead folds the stream tail,
advancing the watermark per event:

```
let tail = log.read_stream(&gap.subject, gap.id, after).await?;   // [rev0, rev1, ...]
for e in &tail {
    if let Some(bound) = upto { if e.revision >= bound { break; } }  // strict: stop at r
    let repair_outcome = reg.apply_event(...)?;
    if repair_outcome.gaps.is_empty() {
        reg.advance_watermark(&key, e.revision, e.position);          // ← THE TRAP
    }
}
```

**The trap:** `advance_watermark` is a documented **no-op on a vacant entry**
(`if let Some(mut e) = self.state.get_mut(key) { ... }`). It only mutates an *existing*
entry.

For a **mixed-root stream** — one whose lead revision (rev 0) is an event type that **no
registered aggregator folds** — `apply_event(rev0)` matches nothing → empty outcome → we
call `advance_watermark` on a still-**vacant** entry → **no-op**. The watermark never
leaves 0. Next round re-detects the identical `Gap{expected:0}`. After 8 rounds
`fold_event` bails:

```
fold_event: gap repair did not converge for event `b`
(stream s-<id>, revision 1) — the aggregate stream is missing revisions it claims to have
```

### Why it wedges the whole consumer (not just one fold)

- The strict fold is the **first** statement in the per-event loop of both consumer
  runners and is propagated with `?`:
  - `projection_runner.rs:189` (step), `:292` (cold-start hydration)
  - `multi_projector.rs:221` (step), `:308` (hydration)
- The checkpoint/cursor is advanced **only after** a successful fold + project
  (`projection_runner.rs:243` "C2: advance cursor ONLY after Ok"). A bail returns before
  the checkpoint moves.
- The supervisor `supervise_one` (`engine.rs:2708-2724`) treats `Ok(Err(e))` as a fault
  and retries with backoff **forever**. Its own comment: *"a DETERMINISTIC failure here
  retries with no ceiling and never advances the cursor — which is exactly what wedges
  settle."*
- A mixed-root stream is a **deterministic** bail → infinite retry → `settle()` blocks on
  that consumer's floor indefinitely.

### Why it was latent until 0.15

This only triggers under stream-aligned folding where a consumer's stream can begin with a
foreign event. Normal streams begin with the aggregate's own genesis event (e.g.
`ScoutRunRequested`), which folds and self-seeds the entry, so the trap never fires. The
production trigger is the `scout_run` re-extract chain whose stream begins with
`enrichment:reextract_completed`.

### Reproduction

`cargo test -p causal --lib strict_repair_seeds_mixed_root_stream` — currently **RED**,
bails with the exact message above. Test at `aggregator.rs` (added). The other three
aggregator gap-repair tests pass.

---

## 2. Mixed-root streams are intended, not a misuse

Stream placement on append is `(category = fact.subject(), subject_id = fact.subject_id())`
(`engine.rs:1924/1938-1939` emit; `engine.rs:1745-1746` OCC append). `Event::SUBJECT`
defaults to `Self::NAME` (own stream) but events **override `SUBJECT`** to deliberately
co-locate on a shared subject history (`engine.rs:1920-1922`: "they differ only when the
event overrides `SUBJECT`"). Any two event types with the same `subject()` + `subject_id`
share one stream; an aggregator folding only a subset sees a mixed-root stream.

**Conclusion:** the fix addresses a supported, recurring architectural pattern. It is not
papering over a modeling error. (The rootsignal-side change — give `scout_run` a no-op
fold arm for `reextract_completed`, or stop reusing the namespace — is **optional
defense-in-depth**, not the primary fix.)

---

## 3. The fix — two coordinated changes

Both are required. **(a)** without **(b)** leaves the multi-aggregate hazard (§8 Q6 / test 6b)
latent; **(b)** without **(a)** still no-ops on the vacant entry. Together they make `repair_gap`
correct on its own contract, with no dependency on how callers sequence their folds.

### (a) Seed the vacant entry (strict-only)

In `repair_gap`, **after** the restore fast-path block (~line 996) and **before**
`let after = ...`, seed a vacant entry to the empty base so the subsequent
`advance_watermark` has an entry to advance:

```rust
// STRICT-ONLY. A vacant entry makes `advance_watermark` below a silent
// no-op (it only mutates an existing entry), so a stream whose lead
// revisions belong to events this aggregate does NOT fold (a mixed-root
// stream) traps repair in a non-converging loop: nothing folds the entry
// into existence, nothing can advance it. Seed the empty base (default
// state, version ZERO) so the tail fold/advance can carry the watermark
// forward. ZERO == fold(log[..0]), so the strict
// `state == fold(log[..cursor])` invariant holds; genuinely missing
// revisions still fail to converge (read_stream won't return them).
//
// The `upto.is_some()` guard restricts this to the strict (consumer) path.
// The non-strict engine path must NOT seed: (a) it doesn't need to — its
// vacant entries are filled by the `restore_aggregate` fast-path above, and
// its rare bails are swallowed by the engine warmers; and (b) seeding the
// shared engine registry would CORRUPT `engine.state_of`, which uses
// `!has_state` both to trigger restore-on-read (engine.rs:2156) and to
// return `None` (engine.rs:2167). A version-ZERO default seed there would
// short-circuit both — returning an empty aggregate for one that has events.
if upto.is_some() && !reg.has_state(&key) {
    if let Some(agg) = reg.find_first_by_aggregate_type(&gap.aggregate_type) {
        reg.set_state(
            &key,
            Arc::from(agg.default_state()),
            StreamRevision::ZERO,   // version
            StreamRevision::ZERO,   // snapshot_at_version
        );
    }
}
```

### (b) Make the advance gate per-key (not global)

In the tail loop, decide whether to advance **this** aggregate's watermark by whether **this
aggregate's own key** gapped — not whether *any* aggregator gapped. This restores the original
documented intent of the gate (its comment already says "a concurrent restore/fold is mid-flight
on **this entry**") and removes the cross-aggregate suppression hazard.

```rust
let repair_outcome =
    reg.apply_event(&e.event_type, &e.payload, e.subject_id, &e.category, e.revision, e.position)?;

// Advance THIS aggregate's watermark unless THIS aggregate itself gapped.
// A foreign event that matched no aggregator (mixed-root) or that gapped a
// *peer* aggregate sharing the stream must still advance THIS watermark —
// the foreign event is an identity fold for us. Only a gap on OUR OWN key
// (a concurrent restore/fold mid-flight on this entry) must hold us back;
// advancing past that would drop a fold (the original TOCTOU defect).
// `gaps.is_empty()` was a proxy for "did WE gap" that only held when this
// aggregate was the lone gapper on the stream — false on shared streams.
let self_gapped = repair_outcome
    .gaps
    .iter()
    .any(|g| g.aggregate_type == gap.aggregate_type && g.id == gap.id);
if !self_gapped {
    reg.advance_watermark(&key, e.revision, e.position);
}
```

All helpers already exist: `has_state` (:702), `find_first_by_aggregate_type` (:810),
`set_state` (:718, monotonic), `default_state` (:268, used identically at :492); `FoldGap`
carries `aggregate_type`+`id` for the filter (:593-597).

> **Review notes (2026-06-30):**
> - **(a)** The `upto.is_some()` guard is a *correctness* requirement (OpenAI review). Without it
>   the seed can run on the non-strict path (when `restore_aggregate` returns `false`, e.g. an
>   empty-stream race during warming) and materialize a version-ZERO entry in the shared engine
>   registry, defeating `state_of`'s restore-on-read. Confirmed against engine.rs:2156/2167.
> - **(b)** Promoted from "optional follow-up" to part of the fix: we will not knowingly ship a
>   gate we've proven is too broad. It is strictly safe (advancing over a peer's gap is an identity
>   fold for us; a self-gap is still suppressed) and it expresses what the code's own comment
>   already claims. The hazard is unreachable through *today's* runners (§8 Q6), but encoding the
>   correct gate removes the dependency on that external invariant rather than guarding it with a
>   tripwire comment.

### Re-trace with the fix

The `scout_run` shape (single aggregate, foreign lead) — change (a) carries it:
```
round 1: apply(b, rev1) → vacant, version 0 → Gap{expected:0}
         repair_gap (upto=Some(1), strict): seed key = default @ (v0, s0)
           tail[rev0 foreign]: no match → self_gapped=false → advance → v0→1  ✓
           tail[rev1 b]: revision 1 >= bound 1 → break
round 2: apply(b, rev1) → version 1 == revision 1 → FOLD → v2 → gaps empty → Ok  ✓ CONVERGED
```

The multi-aggregate shape (peer C gaps on a foreign-to-B event) — change (b) carries it:
```
B repairs over tail [c@0, d@1, c@2, b@3], C lagging:
  c@0: folds C (or C ahead → Skipped); B not matched → self_gapped(B)=false → advance B
  d@1: no match            → self_gapped(B)=false → advance B
  c@2: C gaps → gaps=[C]   → self_gapped(B)=false (C≠B) → advance B   ← (b) fixes this
  b@3: revision 3 >= bound → break
→ B reaches bound, converges; C heals on its own next delivery.
```

---

## 4. Correctness argument & proof obligations

The fix must preserve **STRICT INVARIANT** `state == fold(log[..cursor])` at every observable
point. Obligations:

1. **Seed preserves the invariant at install.** Default state ≡ `fold(∅)` ≡ `fold(log[..0])`,
   and version ZERO means "0 revisions folded". So immediately post-seed the entry satisfies
   `state == fold(log[..0])`. We are *materializing the empty base the strict gate already
   assumes for a vacant entry* — not jumping to the tail. ✔

2. **advance_watermark over a foreign event preserves it.** A foreign event folds to
   identity (no matching aggregator), so `fold(log[..r+1]) == fold(log[..r])` for that step;
   advancing version `r→r+1` while leaving state unchanged keeps `state == fold(log[..r+1])`. ✔

3. **The strict bound is honored.** The tail loop breaks at `e.revision >= bound` (=`r`), so
   repair never folds the delivered event or anything beyond it. The delivered event is
   folded by the *outer* `fold_event` round. Guarded by the passing test
   `strict_gap_repair_stops_at_the_delivered_event`. ✔

4. **Genuinely-missing rev0 still (correctly) bails.** If rev0 is truly absent from the log,
   `read_stream(after=None)` returns starting at rev1, the loop breaks immediately
   (`rev1 >= bound`), version stays 0, the gap re-detects, and it bails after 8 rounds. This
   is **correct** — it is genuine data loss (corruption / partial-write loss). It is NOT a
   false alarm, because **streams cannot legitimately begin above revision 0** in this
   framework (see §5). ✔ *(Wording fix vs. original analysis: the cause is corruption, not
   "truncation" — truncation is structurally impossible here.)*

5. **Monotonic install is mandatory.** The seed MUST go through `set_state` (monotonic by
   version), never a raw `self.state.insert`. A `version=0` seed can then never clobber a
   higher-version entry installed by a racing live fold/restore — it only ever wins against a
   truly-vacant entry. A bare insert would reintroduce the clobber the monotonic guard exists
   to prevent (the 9c attack remediation). ✔ — **this is the single load-bearing
   implementation constraint.**

6. **`:prev` retry slot stays correct.** The seed writes only the main key, not `:prev`.
   `:prev` is written exclusively by `apply_event`'s `Folded` arm. The first real fold against
   the seeded entry (gate `Equal`) takes that arm and writes `:prev` with the seeded default as
   pre-state — identical to the pre-seed vacant-fold path. No new idempotency window: a freshly
   seeded entry (version 0, nothing folded) can never be an `exact_prev` idempotent skip. ✔

7. **Fan-in entries are unreachable.** `FoldGap` is emitted only in the *aligned* branch of
   `apply_event` (:505). Fan-in aggregates (empty subject, position-gated) never produce a gap,
   so `repair_gap` — and therefore the seed — is never reached for them. The seed cannot corrupt
   position-based fan-in gating. Belt-and-suspenders: `restore_aggregate` also returns false on
   empty subject. ✔

8. **Registry isolation — enforced by the `upto.is_some()` guard.** Each consumer mints a fresh
   private `AggregatorRegistry` (`make_registry()`); the engine registry is a separate instance.
   `engine.state_of` reads the engine registry and uses `has_state`-as-`None` both to trigger
   restore-on-read (engine.rs:2156) and to return "doesn't exist" (engine.rs:2167) — so a
   version-ZERO default seed in *that* registry would defeat restore and return an empty
   aggregate for one that has events. The strict-only guard (`upto.is_some()`) makes this
   impossible *by construction*: the seed runs only on the strict consumer path, which always
   operates on a consumer-private registry; the non-strict engine path never seeds. This is the
   correct boundary because the non-strict path already fills vacant entries via the
   `restore_aggregate` fast-path and swallows its rare bails. ✔ *(Strengthened per OpenAI review
   — previously this relied on isolation as an observation; the guard now enforces it.)*

9. **Snapshot accounting round-trips.** `version` is a *stream watermark*, not a fold count;
   `maybe_save_snapshots` writes `revision = version - 1` and restore reads `after = revision`,
   using the same convention, so foreign events that advanced the watermark don't cause
   double-apply. A freshly seeded entry (delta 0) won't snapshot; only after a real fold, by
   which point version reflects the true watermark. ✔

10. **Per-key advance gate (change b) is strictly safe and strictly more permissive than the old
    global gate.** The old gate advances this aggregate iff `gaps.is_empty()`; the new gate
    advances iff *this aggregate's own key* is not in `gaps`. The new gate advances in a superset
    of cases — specifically it *adds* "advance when only a peer aggregate gapped." That added case
    is correct: the tail event in question does not match this aggregate (a peer gapped on it), so
    for *this* aggregate it is an identity fold and advancing the watermark preserves
    `state == fold(log[..r+1])`. The case the old gate protected — a gap on *this* key, signalling
    a concurrent restore/fold mid-flight on this entry, where advancing would drop a fold (the
    TOCTOU defect) — is **still suppressed** by the new gate (this key ∈ gaps). So (b) fixes the
    cross-aggregate suppression without weakening the TOCTOU protection. ✔

---

## 5. Load-bearing environmental fact: no truncation, ever

Obligation 4 depends on: *a stream can never legitimately start above revision 0.* Verified:

- No DELETE/TRUNCATE/scavenge/retention/`$tb`/max-age/max-count path touches the event log in
  any backend or migration. Only `delete_snapshot` and reactor-attempt history are ever deleted.
- Append assigns `head + 1` densely (memory `count + offset`; PG `BIGSERIAL`).
- The conformance suite **forbids** the only mechanism that could create a hole:
  `expected_revision_ahead_of_head_is_rejected` and `revision_is_monotonic_within_stream`
  ("0,1,2,... no gaps, no skips"); and explicitly disclaims teardown by truncation.
- Even the KurrentDB backend never writes stream metadata, so `$tb`/max-age never activate.

**Therefore bailing on a missing rev0 is never a false positive.** If this fact ever changes
(retention is added), Obligation 4 must be revisited — the strict path skips the snapshot
fast-path, so a scavenged-but-snapshotted stream would then wrongly bail. Flag this as a
**tripwire** comment near the fix.

---

## 6. Rejected alternatives (anticipating the skeptic)

- **"Just make `advance_watermark` create-on-vacant."** Rejected. `advance_watermark` has a
  deliberately pure contract ("identity fold; no-op on vacant"). Although it currently has a
  single caller, conflating advance-with-create spreads entry-materialization policy into a
  primitive and would require it to resolve `aggregate_type → default_state` from the key. Keep
  materialization localized to `repair_gap`, where vacancy is *provably wrong* (we're mid-repair
  of a gap we know exists). Functionally `create-at (rev+1, default)` equals seed-then-advance,
  but the seed expresses the ZERO base explicitly and is easier to reason about against the
  invariant.

- **"Remove the strict skip of the snapshot fast-path; let restore seed it."** Rejected. Restore
  jumps the entry to the **stream tail**, violating `state == fold(log[..cursor])` for a consumer
  mid-stream (it would fold events the consumer hasn't been delivered yet). The strict skip is
  correct by design; the seed is the strict-mode counterpart of what restore does for the eager
  engine path.

- **"Fix it only in rootsignal (no-op fold arm / new namespace)."** Insufficient as the primary
  fix: it patches one event type on one stream. Any future `SUBJECT`-override mixed-root stream
  would re-trip the framework bug. Do the Causal fix; treat rootsignal as optional
  defense-in-depth.

---

## 7. Test matrix

RED-first, then GREEN. Style matches `aggregator.rs` tests (~:1380–:1530).

1. **(written, RED today) `strict_repair_seeds_mixed_root_stream`** — stream `s-<id>`:
   rev0 = `a` (unregistered), rev1 = `b` (folded). Strict-deliver rev1. GREEN expectation:
   `applied == true`, `get_version("B:<id>") == 2`, state `n == 1` (only `b` folded).

2. **All-foreign lead run** — rev0=`a`, rev1=`a`, rev2=`b`. Strict-deliver rev2. Expect
   converge, version 3, `n == 1`. Proves multiple contiguous foreign lead revisions advance.

3. **Negative: genuinely-missing rev0 still bails** — append rev0 then simulate a log whose
   `read_stream` returns only `[rev1=b]` (or append only `b` such that it lands at rev≥1 via a
   crafted stream). Expect the bail to persist. Guards Obligation 4. *(May require a stub log
   backend that returns a non-zero-based tail; if not feasible cleanly, document why and rely on
   the structural argument.)*

4. **Regression: foreign event interleaved after a real fold (entry already exists)** — rev0=`b`,
   rev1=`a`, rev2=`b`. Strict-deliver rev2. Entry exists from rev0, so the seed must be a no-op
   (`!has_state` false). Expect version 3, `n == 2`. Guards that the seed doesn't regress the
   already-working path.

5. **Snapshot round-trip on a mixed-root stream** — restore from a snapshot whose `revision`
   points at a foreign event; verify no double-apply and correct `n`. Guards Obligation 9.

6. **"Foreign to B, meaningful to C" — the lead event is registered for *another* aggregate
   (OpenAI review).** Register both B (folds `b`, subject `s`) and C (folds `c`, subject `s`) in
   one consumer registry. Stream `s-<id>`: rev0 = `c`, rev1 = `b`. Two sub-cases, because the
   tail loop advances B's watermark on `if repair_outcome.gaps.is_empty()` — a check that is
   **global across all aggregators**, not specific to B:
   - **(6a) C is caught up** (normal in-order consumer processing has already folded rev0 into
     C). During B's repair, `apply_event(c)` returns `Skipped` for C → `gaps` empty → B advances.
     Converges. Expect B version 2, `n_b == 1`, `n_c == 1`.
   - **(6b) C is behind and gaps mid-tail — raw `fold_event`.** Manufacture the "C behind" state:
     stream `[c@0, d@1, c@2, b@3]`, deliver `b@3` to a cold registry without pre-folding C's
     events, so during B's repair `apply_event(c@2)` emits C's gap. **With change (b) (per-key
     advance gate) this MUST converge:** C's gap is not B's, so B still advances and reaches its
     bound. Expect B version 4, `n_b == 1`; C is left gapped and heals on its own next delivery.
     This is the regression guard that the global→per-key gate change is present and correct — it
     bails under the old global gate and passes under (b). (The runners never actually deliver in
     this order — see §8 Q6 — so this is the *latent* hazard (b) closes, not a live one.)

7. **(optional, integration) supervisor no longer wedges** — drive a `MultiProjectionRunner`
   over a mixed-root stream end-to-end; assert `settle()` completes instead of blocking.

---

## 8. Open questions for the adversarial reviewer to attack

1. **Do the non-strict engine warmers need parallel treatment?** `fold_committed_fact_into_registry`
   (`engine.rs:2087`, strict=false, **swallows** Err) and `fold_output_into_engine_registry`
   (`reactor_runner.rs:1336`, swallows Err) already document this exact root cause with TODOs
   ("advancing the stale watermark on foreign events"). Hypothesis: the non-strict path goes
   through `restore_aggregate` for vacant entries (which is already mixed-root-safe via
   `version = last_rev + 1` and `replay_events_onto` skipping non-matching types), and advances
   fine for populated entries, so the swallow only masks *other* faults and no change is needed.
   **Verify this hypothesis** — is there a non-strict cold-start path that bails the same way and
   is merely hidden by the swallow, leaving the engine registry watermark stale?

2. **Can `find_first_by_aggregate_type(&gap.aggregate_type)` ever return `None` at the seed?**
   Argument: gaps are produced by `apply_event`, which only runs *registered* aggregators, so the
   type is always registered. The `if let Some` is defensive. Confirm no path constructs a
   `FoldGap` with an unregistered `aggregate_type`.

3. **Multi-aggregator same type, different id_fn.** `for_type_with_id_fn` allows two aggregators
   for the same `aggregate_type` keyed differently. `find_first_by_aggregate_type` returns the
   *first*; `default_state` is identical across them (same `A::default()`), so the seed value is
   correct regardless. Confirm `default_state` can't differ between aggregators sharing a type.

4. **Concurrency within one consumer.** Are the consumer step loop and hydration ever concurrent
   on the same registry/key such that two `repair_gap` calls race? The footgun audit argues the
   monotonic `set_state` makes concurrent seeds idempotent and unable to clobber. Confirm the
   runner is single-flight per partition/key.

5. **Position/`last_pos` on seed.** `set_state`'s vacant arm installs `last_pos = LogCursor::ZERO`.
   For an aligned (revision-gated) aggregate this is irrelevant (gating is by revision). Confirm
   no aligned-path code consults `last_pos` such that a ZERO seed misbehaves.

6. **The tail-loop's `repair_outcome.gaps.is_empty()` advance-gate is too broad — FIXED by change
   (b), not merely declared unreachable.** The old gate advances *B's* watermark only when the
   just-applied tail event produced **no gaps in any aggregator** (`apply_event` runs *all*
   matching aggregators; only `Action::Gap` pushes to `outcome.gaps` — `aggregator.rs:592`;
   `Skipped` pushes nothing — `:571`). On a "foreign-to-B / meaningful-to-C" stream, if peer C is
   *behind* when B repairs, `apply_event(c)` emits C's gap → B's advance suppressed → B bails
   despite the seed. Constructed trace: stream `[c@0, d@1(foreign-to-all), c@2, b@3]` delivered to
   a cold registry with C never pre-folded — B's repair hits C's gap at rev2 every round and bails.

   **Reachability (why this is latent, not live):** today's runners cannot deliver in that order —
   `step` folds **every** event via `fold_event` in position order (`multi_projector.rs:216-221`);
   `ensure_hydrated` replays from `LogCursor::ZERO` for the **whole registry**
   (`multi_projector.rs:291`); within a stream position order = revision order; and `fold_event`
   self-repairs each aggregate when its own event is processed. So a peer is never behind during a
   repair, and the old global gate happens to be correct *given that external invariant*.

   **We are fixing it anyway (change (b)), because we will not ship a gate we have proven is too
   broad and whose correctness rests on a caller-side invariant.** The fix is to gate the advance
   on whether **this aggregate's own key** is in `repair_outcome.gaps`
   (filter by `FoldGap.aggregate_type`+`id`, which it carries — `:593-597`), not on the set being
   globally empty. This is strictly safe — advancing over a *peer's* gap is an identity fold for us;
   a gap on *our own* key (the concurrent restore/fold the comment cares about) is still suppressed —
   and it makes `repair_gap` correct independent of how callers sequence folds. It also literally
   restores the gate's existing comment, which already scopes the concern to "**this entry**." The
   alternative we rejected — a tripwire comment documenting the global gate's fragility — is exactly
   the "leave a known bug, hope the caller keeps holding it right" pattern this review set out to
   eliminate. Regression guard: test 6b (bails under the old gate, converges under (b)).

---

## 9. Operational recovery (corrected per OpenAI review)

**The wedged consumers self-heal on redeploy — do NOT advance checkpoints by default.**

The supervisor (`engine.rs:2708`) treats the bail as a fault and backs off, **retrying forever
without ever advancing the cursor**. So the stuck consumer's checkpoint still sits *before* the
poison position. Deploying the fixed `causal` binary is sufficient: on the next retry the
consumer re-attempts the *same* poison position, `repair_gap` now converges (seed → advance →
fold), the projection runs, and the checkpoint advances normally. No manual data surgery.

**Manually advancing `causal_checkpoints` past the poison position is an *exceptional* runbook
step, not the default.** It is correct *only* if the event itself is genuinely corrupt or is one
you have decided to intentionally skip — because advancing the checkpoint **skips the
projection** for that event, which is read-model data loss. The original analysis's note #2
("advance checkpoints past the poison position once the new build is deployed") was too strong
and is superseded by this section.

Recovery order:
1. Deploy the fixed `causal` build. Verify the previously-wedged consumers resume and their
   checkpoints advance (watch `settle()` / consumer health stop reporting the failure).
2. Only if a consumer is still stuck *after* redeploy: investigate whether the lead event is
   actually corrupt (genuine missing-rev0, §4/§5). Manual checkpoint advance is the last resort
   and means accepting the skipped projection.

---

## 10. Release sequencing

Per the established causal→rootsignal flow:
1. Land fix + tests in `causal-rs`; `cargo test` green workspace-wide.
2. Bump **every** workspace crate to the aligned version `0.16.1` (no version skew), even
   unchanged crates.
3. Publish `causal`.
4. Verify rootsignal usage statically against published `causal`; then bump rootsignal's
   dependency. Do not patch rootsignal to test against an unpublished causal.
5. Apply the operational checkpoint-advance (§9) to the wedged consumers.
6. Optional: rootsignal defense-in-depth (no-op fold arm or namespace change).
