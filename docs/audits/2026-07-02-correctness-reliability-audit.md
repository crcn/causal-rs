# causal-rs correctness & reliability audit — 2026-07-02

Adversarial pre-release audit of the five runtime promises (at-least-once + idempotent appends;
one decision per trigger; quiescence; replay determinism; opt-in OCC), focused on the 0.19
decision-records machinery and its interactions with everything older.

**Method.** Two multi-agent passes over the working tree at `81b1b5a` + the then-uncommitted
event-id-registry work: 8 invariant-focused finders → dedup (41 raw → 25 canonical) → one
adversarial verifier per finding, required to *refute first* and to prove in-memory-reachable
claims with a failing test against `MemoryStore` (baseline suite green: 274 passed / 0 failed) →
completeness critic → 4 targeted round-2 hunts (15 more canonical findings) → verify again.
Trace-confirmed (non-executable PG/Kurrent) findings got a second, independent hostile refuter.
Because `dev` advanced during the audit (merge `905e13a` + fixes), **every confirmed finding was
then re-verified against `dev@1721079`** by replaying its recorded failing test (or re-tracing).

**Result.** 40 canonical findings: **29 confirmed with failing tests**, **9 confirmed by trace**,
2 refuted by the audit itself. Of the 38 confirmed, **37 are still present at `dev@1721079`**;
one (the retention-GC floor bound) was fixed mid-audit by `7290dd1` and its fix re-verified as
complete. The 29 recorded failing tests live in
[`2026-07-02-audit-tests/`](2026-07-02-audit-tests/) — each is RED against HEAD and is the
regression test for its fix.

Severity legend, worst first: `silent_corruption` > `loss_or_duplication` > `nondeterminism` >
`wedge_or_livelock` > `incorrect_result` > `minor`. Findings below are grouped by theme and
ranked by blast radius within each group.

---

## Update — fixed in `feat/decision-protocol-side-doors` (2026-07-02)

The Group A decision-protocol side doors are **resolved** on branch
`feat/decision-protocol-side-doors`, each with a permanent regression test
(the recorded RED test, adapted where the fix changed the intermediate
mechanism it asserted). Protocol changes are written up in
`docs/plans/2026-07-02-decision-records-design.md` (section "Shipped in
feat/decision-protocol-side-doors").

| Finding | Fix | Regression test |
|---|---|---|
| Terminal park outside the decision protocol (#3) | Park seals a `parked` decision; redelivery replays it | `park_decision_protocol_test.rs` |
| Cancel fence acks without consulting the decision store (#12) | Fenced triggers consult the store; sealed batches complete | `cancel_fence_decision_replay_test.rs` |
| Divergence-accept seals a log-contradicting record (#21) | Log-wins reconciliation; `RecordIntegrityError` deleted | `divergence_accept_reconcile_test.rs` |
| `settle`/empty-seal fence-dependence (#113) | Fence-consulted emptiness always seals | `empty_seal_fence_dependence_test.rs` |
| Boot fence rehydration fails open (#111) | Bounded retry then propagate (loud boot failure) | `cancel_fence_rehydration_test.rs` |
| Park re-park livelock (#15) — **bonus** | Divergent re-park append reconciles log-wins | closed by the same park/reconcile change |

The remaining findings below stand as documented (still open). `DecisionStore::seal`
was kept non-breaking (byte-compare won-detection); `DecisionRecord.parked` and
`DivergentRedelivery.canonical` are additive; PG needs migration
`20260703_causal_decisions_parked.sql`.

---

## A. One-decision-per-trigger — escape hatches around the seal/replay protocol

The 0.19 protocol is sound on its main path; every finding here is a side door that produces or destroys an outcome without going through seal→append→replay.

### 1. Terminal park is outside the decision protocol — a crash (or zombie racer) between the park append and the floor persist puts BOTH a terminal-failure fact and success outputs in the log for one trigger

**Severity:** silent_corruption · **Verdict:** confirmed with test · **Status at `dev@1721079`:** still present

**Defect.** park_terminal_failure appends its REACTION_FAILED terminal fact directly to the log without sealing a decision record and before the ack-floor persists, so the 'parked' outcome is not durable as a decision; a redelivery misses the replay gate (failed bodies seal nothing), re-runs the body, and if it now succeeds, seals and appends the full success batch — two contradictory durable outcomes for one trigger, undetectable because the terminal fact (nth=u32::MAX) and the outputs (nth 0..k) have distinct event_ids so no dedup or divergence machinery ever collides.

**Scenario.** Trigger T fails Transient past TRANSIENT_CEILING (external outage) → parks: terminal fact appended (reactor_runner.rs:1847-1850), attempts cleared, DLQ observer fired; kill -9 before the worker's Completion is reaped and checkpoint.advance persists the floor past T (the window spans at least one dispatcher turn). Restart: T redelivered, decisions.get misses (:1156-1160, pinned by body_failure_retries_then_parks_without_sealing at :4344-4362), body re-runs, outage is over, react() SUCCEEDS — decision seals, outputs append (:1392-1423). Downstream completion folds over T's subject see completion-with-error AND the real outputs; the DLQ entry duplicates whatever the outputs trigger. Racer variant needing no crash: a zombie lease holder (advisory lock, no fencing token) seals+appends success while the new holder, attempts exhausted, parks — it never re-checks the decision store between its last BodyFailed and the park append.

**Evidence.** Park appends outside the record machinery: modules/causal/src/reactor_runner.rs:1847-1850 (no seal, no decisions.get); process_trigger's park arm :1072-1112 never consults the decision store; body failure returns BodyFailed without sealing :1303-1326; replay gate only recognizes sealed decisions :1156-1160. Distinct ids prevent collision detection: :1833-1835 (u32::MAX) vs build_output_events nth 0.. (:1490-1510). Floor persists later on a dispatcher step: :1917-1953. Design doc scoping whose premise is false across this window: docs/plans/2026-07-02-decision-records-design.md:121-123 ('Park path unchanged. A body failure means no record exists').

**Verifier corrections/refinements.** Finding is accurate as stated; all cited line numbers check out in the working tree (park append :1847-1850, park arm :1072-1112, replay gate :1156-1160, BodyFailed without seal :1325, u32::MAX id :1833-1835, floor persist :1917-1953, pinning test :4344-4362). Two refinements: (a) the confirmed proof used the Poison class for brevity (parks on first attempt); the TransientExhausted variant the finder leads with shares the identical park arm and is strictly more probable in production — no correction needed, just noting the test exercised Poison. (b) The zombie-racer (no-crash) variant is plausible but was not verified; the finding does not depend on it. Additionally confirmed beyond the claim: after recovery the decision store contains a sealed SUCCESS record for the trigger while the log retains the terminal-failure fact — so even the new 0.19 'canonical decision' machinery now durably contradicts the log's park fact, and the DLQ observer side effect fired for a trigger whose canonical decision is success.

**Proof (failing test output):**
```text
running 1 test
post-recovery: body_calls=2 terminal_facts=1 success_outputs=1 decision_sealed=true

thread 'park_then_crash_then_success_double_outcome' (2970514) panicked at modules/causal/tests/zz_audit_v3.rs:335:5:
CHIMERA: one trigger produced two contradictory durable outcomes — 1 terminal-failure fact(s) AND 1 success output(s)
test park_then_crash_then_success_double_outcome ... FAILED

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s

(All intermediate assertions passed before the final one failed: parked_facts==1 appended pre-crash, floor NOT persisted, ds.get(...)==None i.e. park sealed no decision, DLQ observer fired exactly once. Phase B then re-ran the body, sealed a SUCCESS decision, and appended the output alongside the terminal fact.)
```

Failing test preserved at [`2026-07-02-audit-tests/zz_audit_v3.rs`](2026-07-02-audit-tests/zz_audit_v3.rs) — drop into `modules/causal/tests/` to reproduce (RED today; becomes the regression test for the fix).

Files: `modules/causal/src/reactor_runner.rs`, `docs/plans/2026-07-02-decision-records-design.md`

---

### 2. Cancel fence acks redelivered triggers without consulting the decision store — a sealed decision interrupted mid-append is left as a permanently torn batch

**Severity:** loss_or_duplication · **Verdict:** confirmed with test · **Status at `dev@1721079`:** still present

**Defect.** Both cancel gates (dispatch-gate and worker-level) ack a trigger whose workflow is in the cancelled set without checking decisions.get, so a trigger that sealed a record and crashed mid-append-loop is never replayed if its workflow is cancelled before restart — the log permanently holds a strict subset of a sealed decision's outputs, contradicting the design's explicit rule that 'a sealed record for a workflow cancelled after sealing still appends' and the H6 claim that the append loop is a resumable projection of the record.

**Scenario.** Reactor seals record R with outputs [A, B] for trigger T (workflow W); appends A; kill -9 before B. Operator cancels W (marker persisted, fence rebuilt at build(), engine.rs:1670-1690). Restart: T redelivered but the dispatch gate sees W cancelled and acks at the gate — replay_decision never runs, B is never appended. Consumers that already reacted to A observe a half-decision forever; R sits in the store until age-GC asserting outputs the log will never contain, which also primes a false integrity park after a later checkpoint regression.

**Evidence.** modules/causal/src/reactor_runner.rs:674-687 — fenced triggers get `d.ingest_pos = event.position` and are never enqueued, no decision_store consultation; :880-898 — queued triggers acked with parked:false on fence hit, again without decisions.get. Design doc states the opposite semantics: docs/plans/2026-07-02-decision-records-design.md 'Fence + H7 depth ceiling unchanged' bullet ('A sealed record for a workflow cancelled after sealing still appends — the decision happened pre-cancel'). The property holds in-process (fence checked before seal) but not across crash-redelivery, exactly when partial appends exist.

**Verifier corrections/refinements.** Core claim confirmed as stated, including file/line citations (dispatch gate reactor_runner.rs:674-687, worker gate :880-898, replay gate only at :1156-1159 inside attempt_trigger; design promise at docs/plans/2026-07-02-decision-records-design.md:122-124). Two corrections: (1) The secondary claim that the lingering record "primes a false integrity park after a later checkpoint regression" is wrong — a partial append cannot diverge (already-appended outputs dedup byte-identically; missing ones append fresh), so a replay reached via checkpoint regression would heal the batch, not park it; and the fence, rebuilt from the durable control stream at every build(), keeps such a trigger gate-acked anyway. The record merely sits until age-GC. (2) An aggravation the finder missed: a sealed output may root a NEW workflow via the workflow override (reactor_runner.rs:1502 — out.workflow.unwrap_or(trigger.workflow_id)), so the permanently lost event can belong to a workflow that was never cancelled. Fix direction implied by the design doc: both fence gates must consult decisions.get before gate-acking (hit ⇒ replay_decision/complete the batch, then ack; miss ⇒ ack as today).

**Proof (failing test output):**
```text
running 2 tests

thread 'sealed_decision_interrupted_mid_append_still_completes_when_workflow_cancelled_while_down' (2988816) panicked at modules/causal/tests/zz_audit_v12.rs:204:5:
DEFECT: the sealed decision's second output was never appended — the cancel fence acked the redelivered trigger without consulting the decision store, leaving a permanently torn batch (1 of 2 sealed outputs in the log)
test sealed_decision_interrupted_mid_append_still_completes_when_workflow_cancelled_while_down ... FAILED
test control_sealed_decision_interrupted_mid_append_completes_on_redelivery ... ok

failures:
    sealed_decision_interrupted_mid_append_still_completes_when_workflow_cancelled_while_down

test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.16s
```

Failing test preserved at [`2026-07-02-audit-tests/zz_audit_v12.rs`](2026-07-02-audit-tests/zz_audit_v12.rs) — drop into `modules/causal/tests/` to reproduce (RED today; becomes the regression test for the fix).

Files: `modules/causal/src/reactor_runner.rs`, `docs/plans/2026-07-02-decision-records-design.md`

---

### 3. Divergence-accept on the re-decide path seals a record that permanently contradicts the log and never reconciles it — the next redelivery poison-parks a succeeded trigger as a fake integrity violation

**Severity:** incorrect_result · **Verdict:** confirmed with test · **Status at `dev@1721079`:** still present

**Defect.** attempt_trigger seals the re-decided batch BEFORE appending; when append_outputs then hits DivergentRedelivery on the first-delivery (record-absent, A3 accept-and-advance) path it keeps the log's canonical rows and returns Done but never removes or rebuilds the just-sealed record — the store now durably asserts outputs the log rejected, and any subsequent redelivery takes the replay path (from_record=true), raises RecordIntegrityError, and poison-parks the trigger, appending a spurious REACTION_FAILED terminal fact for work that succeeded (twice). A3's protection works exactly once, then converts checkpoint regressions into the park-storm it was written to prevent.

**Scenario.** T processed at t0: record D1 sealed, outputs P1 appended, acked; retention passes, D1 GC'd (legal). Checkpoint regression (PG restore, operator truncate) redelivers T; get-miss; nondeterministic body (or 0.18→0.19 upgrade with changed reactor code) re-decides P2 with the same identity-keyed event_ids; seal writes D2{P2} (reactor_runner.rs:1392-1417); append diverges; accept-and-advance warn keeps P1, T acks. Store says P2, log says P1, and no code path removes D2 (the runner never calls decision_store.remove; only the age sweep does). Any later redelivery within D2's retention — routine crash between ack and floor persist, deploy overlap, second regression — hits the gate, replays D2, DivergentRedelivery → RecordIntegrityError → poison park + REACTION_FAILED appended into T's subject history where completion folds read completion-with-error. A regression window covering N such triggers produces N integrity parks on the NEXT regression. Additional gaps on the accept path: O2 outputs with NEW event_ids still append (`continue` skips only diverged ids), so the log becomes P1 ∪ P2 — a chimera — while the record claims P2 is canonical; and the `continue` also skips the settle high-water bump and engine fold for the lingering output (small A5 gap).

**Evidence.** Seal before append: modules/causal/src/reactor_runner.rs:1392-1417 (seal at :1412), append with from_record=false at :1423; comment at :1419-1422 acknowledges the GC'd-record divergence case but leaves the contradicting record. Accept branch neither removes nor reconciles: :1574-1603 (`continue; // skip this output; the persisted row stands` at :1602, bypassing the settle bump/fold at :1621-1623; grep confirms no decision-store remove call — only effect-store remove at :1958). Replay branch converts to loud park: :1567-1573 (from_record ⇒ RecordIntegrityError), :1448-1476 (integrity error → poison BodyFailed → park), park appends REACTION_FAILED into the subject history :1811-1850 (module doc :1726-1735). A3 intent violated: docs/plans/2026-07-02-decision-records-design.md:226-237.

**Verifier corrections/refinements.** The finding is accurate as stated, including all line citations (seal :1412, append :1423, accept branch :1574-1603 with `continue` at :1602, replay gate :1567-1572, integrity park :1448-1476, park fact :1811-1850). Two refinements from the reproduction: (1) the reproduction needs no exotic trigger — after the accept-and-advance pass, ANY single routine redelivery within the record's retention window (a crash between ack and floor persist, i.e. the ordinary at-least-once window) suffices to fire the false integrity park, so the exposure is broader than "the NEXT regression"; (2) the poisoned record self-heals only after the retention window (default days) GCs D2, but the spurious causal:reaction_failed fact appended into the subject history is permanent and is stamped class "poison", so completion folds over that subject durably read a failure for work whose outputs are in the log. Also observed (cosmetic): RecordIntegrityError's message is double-wrapped in the park fact's error string.

**Proof (failing test output):**
```text
post-accept sealed record asserts reminder payload={"nonce":1,"order_id":"96abce28-55ae-42b0-887e-dffba238ab47"} — log holds nonce=0
after routine redelivery: body ran 2 time(s) total; terminal_failures=1; 1 causal:reaction_failed fact(s) in the log
terminal error: decision-record integrity violation on replay: payload at `nonce`: decision-record integrity violation on replay: payload at `nonce`
spurious park fact in subject history: {"attempts":1,"class":"poison","consumer":"r.audit21","error":"decision-record integrity violation on replay: payload at `nonce`: decision-record integrity violation on replay: payload at `nonce`","trigger_event_type":"order_placed","trigger_id":"218510a3-fde4-48b6-93f4-96372bc0da16"}

thread 'accept_and_advance_must_not_arm_a_future_integrity_park' (3009012) panicked at modules/causal/tests/zz_audit_v21.rs:321:5:
assertion `left == right` failed: a SUCCEEDED trigger was poison-parked as a fake integrity violation — the accept-and-advance path left a sealed record contradicting the log
  left: 1
 right: 0
test accept_and_advance_must_not_arm_a_future_integrity_park ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.12s
```

Failing test preserved at [`2026-07-02-audit-tests/zz_audit_v21.rs`](2026-07-02-audit-tests/zz_audit_v21.rs) — drop into `modules/causal/tests/` to reproduce (RED today; becomes the regression test for the fix).

Files: `modules/causal/src/reactor_runner.rs`, `modules/causal/src/decision_store.rs`, `docs/plans/2026-07-02-decision-records-design.md`

---

### 4. seal_empty_decisions(false) composed with the library's own documented cancel early-exit makes emptiness fence-dependent and non-durable — redelivery in a fence-lacking context re-decides fully

**Severity:** nondeterminism · **Verdict:** confirmed with test · **Status at `dev@1721079`:** still present

**Defect.** With A6 empty-seal elision enabled, a body returning Ok(Events::new()) because ctx.is_workflow_cancelled() was true seals nothing (reactor_runner.rs:1392-1400 returns Done before the seal), so the ∅ decision has no durable trace; a redelivery anywhere the fence reads false (failed rehydration, lagging overlapped instance) get-misses the replay gate (reactor_runner.rs:1156-1160), re-runs the body, and seals+appends the full batch — two different outcomes for one trigger. The elision flag (engine.rs:1020 default true, wiring 1445) is documented as safe for side-effect-free consumers, but nothing warns that any fence read makes emptiness nondeterministic and therefore elision-unsafe; the two documented patterns (contexts.rs:194-198 early-exit; A6 elision) compose into a chimera.

**Scenario.** Fan-out reactor with .seal_empty_decisions(false) starts with the recommended is_workflow_cancelled early-exit. Cancel lands after dispatch; body early-exits empty; no seal; crash before floor persists. Next boot's fence rehydration fails or an overlapped instance redelivers: fence false → full outputs sealed and appended into the cancelled workflow.

**Evidence.** reactor_runner.rs:1392-1400 (verified: `if outputs.is_empty() && !self.seal_empty_decisions` skip precedes seal); contexts.rs:194-198; docs/plans/2026-07-02-decision-records-design.md:260-270 scopes elision without a fence-interaction guard.

**Verifier corrections/refinements.** The finding is accurate; three refinements. (a) The "failed rehydration" leg is concretely a fail-open in shipped code: engine.rs:1703-1723 swallows control-stream read errors with `if let Ok(markers)` — no warning, fence silently empty — which is what makes "fence reads false on redelivery" reachable after a single boot-time storage blip. (b) The hazard family is broader than the body-path composition the finder describes: the runner's dispatch gate (reactor_runner.rs:674-687) and worker-level fence (reactor_runner.rs:880-898) ack fenced triggers without EVER consulting the decision store, so gate-skipped cancel decisions are non-durable in every configuration including the default seal_empty_decisions(true); A6 elision merely extends this non-durability to the body path, which the default empty seal otherwise protects (proven by the passing control leg). Any fix should cover the gate paths too, or make the fence rehydration fail-closed. (c) Precise mechanics confirmed: elision early-return at reactor_runner.rs:1398-1400 precedes the seal at 1401-1413; replay gate at 1156-1160; elision default/wiring engine.rs:1020/1445; documented early-exit contexts.rs:194-204; cancel ordering engine.rs:2092-2099 (marker durable before fence set — single-instance restart with successful rehydration is safe, which is why the blip/lag legs are the reachable ones).

**Proof (failing test output):**
```text
running 2 tests
AUDIT: outputs_in_log=1 body_calls=2 observed_cancelled=[true, false]

thread 'elided_empty_cancel_decision_must_not_re_decide_on_redelivery' (3110391) panicked at modules/causal/tests/zz_audit_v113.rs:394:5:
assertion `left == right` failed: CHIMERA: first delivery decided ∅ (cancel fence read true), redelivery re-ran the body with an empty fence and appended a FULL batch — two different outcomes for one trigger
  left: 1
 right: 0
test elided_empty_cancel_decision_must_not_re_decide_on_redelivery ... FAILED
AUDIT-CONTROL: outputs_in_log=0 body_calls=1 observed_cancelled=[true]
test sealed_empty_cancel_decision_replays_empty_on_redelivery ... ok

test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 5.33s
```

Failing test preserved at [`2026-07-02-audit-tests/zz_audit_v113.rs`](2026-07-02-audit-tests/zz_audit_v113.rs) — drop into `modules/causal/tests/` to reproduce (RED today; becomes the regression test for the fix).

Files: `modules/causal/src/reactor_runner.rs`, `modules/causal/src/contexts.rs`, `modules/causal/src/engine.rs`

---

### 5. Boot fence rehydration swallows all read errors with `if let Ok` — a transient blip boots the engine with an EMPTY cancel fence, and caught-up consumers never re-learn markers below their cursor

**Severity:** nondeterminism · **Verdict:** confirmed with test · **Status at `dev@1721079`:** still present

**Defect.** EngineBuilder::build rebuilds the cancel fence with `if let Ok(markers) = self.log.read_stream(...)` (engine.rs:1707) so any storage error is indistinguishable from an empty control stream and silently discarded; the engine boots with an empty fence and never repairs it, because runners only learn markers via the read_all scan starting at their durable checkpoint (reactor_runner.rs:666, 712-724, seeded at 803-811) — a marker below every runner's cursor (or any cursor started at StartPosition::Latest) is never re-scanned. Cancellation is resurrected for the process lifetime, contradicting the code's own durability doc at engine.rs:2078-2081. (Also: the fence Mutex guard is held across the read_stream await, engine.rs:1706-1711.)

**Scenario.** Boot 1: cancel marker for wf X at position P durable; trigger T of X fence-acked; crash before floor persists past T. Boot 2: transient read error on causal:control → empty fence → T redelivered, passes the gate, body RUNS and emits into the cancelled workflow. Same trigger gets a cancelled-∅ outcome on one boot and full execution on the next. In-memory repro: MemoryStore wrapper whose read_stream errors once on the control stream.

**Evidence.** engine.rs:1703-1723 (the comment explicitly calls errors 'benign'); reactor_runner.rs:666/712-724/803-811 (scan-only, checkpoint-forward re-learn); fence writes exist only at engine.rs:1718, 2098, and runner scans.

**Verifier corrections/refinements.** Core defect verified as claimed at engine.rs:1703-1723 (swallow), reactor_runner.rs:666/712-724/803-811 (checkpoint-forward-only marker re-learning), reactor_runner.rs:882-899 (worker recheck reads the same never-populated set, so it cannot save it). Two refinements: (1) the simplest reachable interleaving does not need the finder's "fence-acked pre-crash, floor unpersisted" staging — it is enough that any trigger for the cancelled workflow sits above the runner's persisted checkpoint while the marker sits at or below it (my repro: cancel + runner floor persists past the marker, crash, trigger appended, reboot with one transient control-stream read error); (2) the swallow has no legitimate purpose at all — both shipped backends already return Ok(empty) for an absent control stream (MemoryStore memory_store.rs:386-388; Kurrent maps ResourceNotFound to Ok(Vec::new()) at causal_replay/src/kurrent_event_log.rs:~480), so the fix is simply to propagate the error and fail the boot (`?` instead of `if let Ok`). Note the resurrection is permanent: the body run acks the trigger and the floor advances past it, so the outcome can never be corrected by a later healthy boot. The Mutex-across-await note (engine.rs:1706-1711, std::sync::MutexGuard held across the read_stream await) is real but minor/secondary.

**Proof (failing test output):**
```text
running 2 tests
test control_healthy_boot_keeps_cancellation_durable ... ok

thread 'transient_boot_blip_must_not_resurrect_cancelled_workflow' (3102355) panicked at modules/causal/tests/zz_audit_v111.rs:234:5:
assertion `left == right` failed: DEFECT: a transient control-stream read error at boot emptied the cancel fence; the runner resumed above the marker and never re-learned it — the cancelled workflow's trigger reached the reactor body (ran 1 time(s)). Cancellation must be durable across restarts.
  left: 1
 right: 0
test transient_boot_blip_must_not_resurrect_cancelled_workflow ... FAILED

test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.08s
```

Failing test preserved at [`2026-07-02-audit-tests/zz_audit_v111.rs`](2026-07-02-audit-tests/zz_audit_v111.rs) — drop into `modules/causal/tests/` to reproduce (RED today; becomes the regression test for the fix).

Files: `modules/causal/src/engine.rs`, `modules/causal/src/reactor_runner.rs`

---

## B. Event identity & idempotent appends on the production backends

The in-memory reference has unbounded exact dedup; the Kurrent+registry and PG paths each diverge from it in a way that loses or duplicates events.

### 6. Kurrent registry-hit dedup path skips ALL byte-identity and placement verification — DivergentRedelivery, the A3 warn, and the RecordIntegrityError loud-park are structurally dead on the production backend

**Severity:** silent_corruption · **Verdict:** confirmed with test · **Status at `dev@1721079`:** still present

**Defect.** In append_any_idempotent, a BatchPresence::Redelivery hit from the event_id registry returns the stored coordinates immediately without reading the persisted events or calling ensure_redelivery_identical — so a same-id append with a different payload, event_type, or target stream is silently reported as a successful dedup. This violates the EventLogBackend contract (dedup-hits MUST be byte-identical, placement is part of identity) and makes every runner-side mechanism that dispatches on DivergentRedelivery — the record-absent accept-and-warn, the reactor_divergence observer, and the replay-path RecordIntegrityError poison-park (A3) — unreachable on Kurrent-with-registry, where the identical states park loudly on Memory/PG. The registry schema also dropped the `stream` column the A2 design sketched (EventIdEntry stores only position/revision), so placement verification is structurally impossible and conformance C1c is unrunnable on Kurrent.

**Scenario.** (1) Integrity case: a sealed record's outputs disagree with the log (corruption, or the divergence-accept landmine finding) — replay_decision appends from the record, the registry recognizes every id, returns Ok with old coordinates, the runner logs 'decision replayed' and acks; the A3 loud-park never fires. (2) Divergence case: record GC'd, nondeterministic re-decide emits same-identity/different-payload outputs — swallowed silently, no warn, no observer, no divergences row; operator gets zero signal that a re-decide diverged, and the reference (Memory) and production backends now disagree on a specified safety behavior. (3) Placement case: after a deploy changes an output's subject placement, the re-append targets a different stream; the registry hit returns coordinates from the original stream, the new stream permanently lacks the event, and the runner bumps settle with coordinates from an unrelated stream. The window-scan path DOES verify identity, making the two recognition paths of one backend semantically inconsistent.

**Evidence.** modules/causal_replay/src/kurrent_event_log.rs:178-196 — `BatchPresence::Redelivery { last } => return Ok(WriteResult {...})` with no content comparison; contrast the window path at :204-217 calling ensure_redelivery_identical, and PG's ensure_redelivery_identical which also compares placement (modules/causal_replay/src/event_log.rs:145-146). Contract: modules/causal/src/event_log.rs:31-49 and :136-165. Runner machinery that depends on the backend raising the typed error: modules/causal/src/reactor_runner.rs:1567-1606; the A3 test divergent_redelivery_after_records_is_a_loud_integrity_error (reactor_runner.rs:4365-4424) passes only via MemoryStore's payload comparison (memory_store.rs:454-513). No stream identity in the registry: modules/causal/src/event_id_registry.rs:54-59, modules/causal_replay/src/sql/event_id_registry_schema.sql:9-14, migrations/20260702_causal_event_ids.sql:6-11; design sketch had `stream`: docs/plans/2026-07-02-decision-records-design.md:218-222; Kurrent conformance never invokes C1c (tests/kurrent_event_log_conformance_test.rs:27-135).

**Verifier corrections/refinements.** Three refinements, none weakening the finding: (1) "Structurally dead" needs a caveat — DivergentRedelivery can still be raised on Kurrent-with-registry via two narrow paths: the crash-before-register healing case (batch in the tail window but missing from the registry, kurrent_event_log.rs:206) and the CAS/expected-revision reconcile path (line 376, irrelevant to reactor outputs which always use Any). In steady state (batch registered — the normal case) every redelivery takes the unverified registry path, so the runner-side machinery is dead for practical purposes, including for shallow redeliveries the window would previously have verified. (2) Scenario 3 precision: for the event_id to remain stable across a placement change, it must be the output's stream CATEGORY (out.subject → EventData.category, reactor_runner.rs:1518) that changes; subject_id and durable_name feed derive_output_event_id (reactor_runner.rs:1504-1510), so a subject_id change mints a new id and never hits the registry. The scenario stands via category change. (3) Aggravating detail the finder missed: on the swallowed-divergence path, append_outputs proceeds to fold the NEW divergent payload into the shared engine aggregate registry cache (reactor_runner.rs:1628-1651) while the log retains the OLD row — engine.state_of can then serve state derived from a payload that exists nowhere in the log. Also confirmed exactly as claimed: registry entry/schema lack the stream column the design doc A2 sketched (event_id_registry.rs:54-59; sql/event_id_registry_schema.sql:9-14; migrations/20260702_causal_event_ids.sql:6-11 vs design doc line 219), and the Kurrent conformance suite runs C1b only registry-less and never wires C1c.

**Proof (failing test output):**
```text
DEFECT CONFIRMED: divergent same-id append to a DIFFERENT stream returned Ok(position=42, revision=7) — the fabricated registry coordinates — with zero verification and zero network I/O (the server does not exist). DivergentRedelivery is unreachable on this path.
```

Failing test preserved at [`2026-07-02-audit-tests/zz_audit_v2.rs`](2026-07-02-audit-tests/zz_audit_v2.rs) — drop into `modules/causal/tests/` to reproduce (RED today; becomes the regression test for the fix).

Files: `modules/causal_replay/src/kurrent_event_log.rs`, `modules/causal/src/event_id_registry.rs`, `modules/causal/src/reactor_runner.rs`, `modules/causal_replay/src/sql/event_id_registry_schema.sql`, `migrations/20260702_causal_event_ids.sql`

---

### 7. Crash between Kurrent CAS append and register_batch leaves the id unregistered — a deep redelivery duplicates the event AND the registry canonicalizes the duplicate's coordinates

**Severity:** loss_or_duplication · **Verdict:** confirmed by trace · **Status at `dev@1721079`:** still present

**Defect.** The registry protocol is append-then-register across two non-atomic systems with no reservation: a crash between the Kurrent append and register_batch leaves a durable event with no registry row; a later redelivery whose original is buried past the max(4·batch,64)-event tail window classifies Absent, misses the window scan, CAS-appends a second copy — and then REGISTERS the duplicate, so the registry permanently points at the wrong coordinates for all future lookups. The same Absent-then-duplicate mechanism applies to every event appended before the registry existed, since the migration ships no backfill.

**Scenario.** (1) append_outputs appends output O (id E) to hot stream S — Kurrent append succeeds (kurrent_event_log.rs:239) — process killed before register_batch (:249). (2) T is unacked so it redelivers; meanwhile >64 events land on S (hot subject, or delay from a checkpoint regression). (3) replay_decision re-appends O: registry Absent, window scan misses, Reconciliation::Conflict, second copy CAS-appended at head and registered with the duplicate's coordinates (register is ON CONFLICT DO NOTHING first-write-wins, and the original was never registered). (4) E exists twice; downstream folds/reactors consume it twice — promise 1 (never double-emit) and promise 4 (state = pure fold) broken. Post-upgrade deep redeliveries of pre-registry outputs duplicate the same way.

**Evidence.** modules/causal_replay/src/kurrent_event_log.rs:239-253 — append then `self.register_batch(batch_ids, wr).await?` as two separate awaits, no atomicity; register_batch :134-151 is PG-side. Window-only recognition on the redelivery path: :178-194 (Absent falls through), :200-232 (Conflict arm appends at head); window = `(batch_ids.len() * 4).max(64)` at :200 — reactor outputs are single-event batches so 64. First-write-wins registry: modules/causal_replay/src/event_id_registry.rs:90-99. Acknowledged residual: modules/causal/src/event_id_registry.rs:34-37. No backfill: migrations/20260702_causal_event_ids.sql (CREATE TABLE only). Deep-duplicate mechanics proven in-tree: tests/kurrent_event_log_test.rs:410-432 (asserts copies == 2); the crash-before-register interleaving itself is untested.

**Verifier corrections/refinements.** Finder's mechanism, files, and severity are correct. Minor cite corrections: the Conflict/append arm spans kurrent_event_log.rs:226-259 (not :200-232; :200 is the window-size line); the Absent fall-through is :178-196 with the empty Absent arm at :194; register_batch is :134-151 as cited. One clarification the finder implied but didn't state: the redelivery necessarily goes through the decision-record replay gate (reactor_runner.rs:1156-1158 → replay_decision:1433 → append_outputs:1542), because the seal is durable before append_outputs runs — so the duplicating append is the 0.19 completion path itself, appending single-event batches (window floor 64). One scope note: the same crash-before-register hole applies to ANY StreamState::Any append through KurrentEventLogBackend (e.g. Engine::emit crash-retry), not only reactor outputs — the reactor completion path is just the one with unbounded redelivery delay. The 'registry points at the duplicate's coordinates' consequence is real but secondary: it prevents a third copy and the coordinates' consumers (settle high-water bump, returned WriteResult) tolerate the later position; the core violation is the double emission in the log, which no fold-side dedup filters. Fix directions consistent with the tree's own patterns: register a reservation (intent row) BEFORE the Kurrent append and complete it after, or treat a registry row as the commit point — i.e. invert to register-then-append with a status column; plus ship a $all-scan backfill for pre-registry events (the migration currently creates an empty table only).

**Proof (failing test output):**
```text
Not executable in this environment: defect requires live KurrentDB (port 2113 closed, KURRENT_URL unset) plus the PG registry; MemoryStore cannot express it (unbounded event_id dedup). Decisive in-tree evidence for the key step: modules/causal_replay/tests/kurrent_event_log_test.rs:411-433 `deep_redelivery_without_registry_duplicates` asserts `copies == 2` ("without a registry, a deep redelivery duplicates") against live Kurrent — the crash-before-register state is registry-indistinguishable from "no registry row," which classify_batch maps to Absent (causal/src/event_id_registry.rs:98-100).
```

Files: `modules/causal_replay/src/kurrent_event_log.rs`, `modules/causal/src/event_id_registry.rs`, `migrations/20260702_causal_event_ids.sql`

---

### 8. Kurrent event_id registry is opt-in with a silent window-only fallback — 0.19 on Kurrent double-emits on deep redelivery by default, with no builder gate, boot warning, or in-repo wiring

**Severity:** loss_or_duplication · **Verdict:** confirmed by trace · **Status at `dev@1721079`:** still present

**Defect.** A2 declares the global event_id index a prerequisite for the decision-record completion path on Kurrent ('decision records without this fix trade one corruption for another on the production backend'), yet KurrentEventLogBackend::connect/from_client default event_id_registry to None with a silent fallback to the bounded tail-window scan; nothing in EngineBuilder or the backend gates a reactor-bearing engine on Kurrent without a registry — unlike the loud DecisionStore gate — and the only with_event_id_registry caller in the entire repo is one #[ignore]'d test using the InMemory registry.

**Scenario.** A downstream app upgrades to 0.19: the build fails until a DecisionStore is added (the loud gate), but KurrentEventLogBackend::connect(...) compiles unchanged with registry None. The completion path is now active and unsafe-by-default: a trigger's sealed outputs land on a hot shared stream, crash before checkpoint, >64 foreign events land during the outage, redelivery takes the replay path, window scan misses the buried original, and append_any_idempotent CAS-appends a duplicate — the outcome the repo's own deep_redelivery_without_registry_duplicates test asserts. This contradicts the repo's 'No Lying Defaults' gate rule that the DecisionStore got.

**Evidence.** Default None: modules/causal_replay/src/kurrent_event_log.rs:101-114 (connect and from_client), :88-93 ('When None, Any falls back to the window-only scan'); window-only path :178-201. No engine-side gate: modules/causal/src/engine.rs gates decision/effect stores only, no EventIdRegistry reference. Sole with_event_id_registry caller: tests/kurrent_event_log_test.rs:441 (#[ignore]'d, InMemory registry — PgEventIdRegistry never meets Kurrent in any test). Duplicate-by-default proven in-tree: tests/kurrent_event_log_test.rs:410-432. Amendment: docs/plans/2026-07-02-decision-records-design.md:212-224 (A2), :300-307 (D2).

**Verifier corrections/refinements.** Finding is accurate; minor precision fixes: (1) exact default-None lines are kurrent_event_log.rs:107 (connect) and :113 (from_client), doc comment :85-92; the duplicate-producing append itself is :226-250 (the finder's ":178-201" covers only the registry consult + window sizing). (2) "No builder gate" is true but structurally the EngineBuilder CANNOT gate this today — it holds a type-erased Arc<dyn EventLogBackend> with no capability probe — so the actionable gap is a backend-side gate/boot-warn (or an EventLogBackend capability flag the builder checks), not a straight copy of the DecisionStore gate. (3) One aggravating detail the finder missed: the sole with-registry example (test :441) uses InMemoryEventIdRegistry, which is process-local — even copying that wiring into production would still duplicate across restarts; no in-repo code ever pairs PgEventIdRegistry with the Kurrent backend. (4) D2's "test that FAILS until the index lands" was shipped inverted: deep_redelivery_without_registry_duplicates asserts the duplicate as expected behavior, so nothing executable enforces the prerequisite.

Files: `modules/causal_replay/src/kurrent_event_log.rs`, `modules/causal_replay/tests/kurrent_event_log_test.rs`, `modules/causal/src/engine.rs`

---

### 9. Registry/log skew after restore-from-backup: phantom registry rows permanently swallow re-appends while the replay path reports success against a log that holds nothing

**Severity:** loss_or_duplication · **Verdict:** confirmed by trace · **Status at `dev@1721079`:** still present

**Defect.** PgEventIdRegistry and the Kurrent log are separate datastores with no cross-verification, rebuild, or truncation tooling; after any restore that rolls one back relative to the other, the registry either silently drops legitimate appends (phantom rows: id registered, event absent) or permits duplicates (missing rows) — and the phantom direction makes the decision-record recovery flow report success while writing nothing, unrecoverable by design since every future re-append of those ids is swallowed forever.

**Scenario.** Kurrent restored from a backup older than Postgres (or a stream operationally deleted/truncated). Sealed records and causal_event_ids rows reference outputs that no longer exist in the log. Redelivery → replay_decision → append_outputs: the registry classifies Redelivery and returns coordinates of a nonexistent event; the append is skipped, the settle high-water is bumped with the phantom position, and the runner logs 'decision replayed from sealed record'. Strictly worse than pre-registry behavior, where the re-append would have re-materialized the events. Downstream aggregates fold a log that permanently disagrees with the decision store and registry, zero detection. Opposite skew (PG older than Kurrent) erases registry rows for live events, reducing to the crash-window duplicate path. No conformance entry, boot check, or repair utility covers either direction.

**Evidence.** Registry authoritative over the log with no existence check: modules/causal_replay/src/kurrent_event_log.rs:178-195 (immediate return on Redelivery, no read of the target stream). Registry is a plain PG table with no linkage to Kurrent state: modules/causal_replay/src/event_id_registry.rs:50-102; sql/event_id_registry_schema.sql. Completion path trusts the WriteResult: modules/causal/src/reactor_runner.rs:1438-1446, settle bump :1617-1622. Conformance ER1-ER4 never compose registry with a log: modules/causal_replay/src/conformance.rs:1637-1715.

**Verifier corrections/refinements.** Three corrections/refinements to an otherwise accurate finding: (1) "unrecoverable by design" overclaims — recovery is possible by manually deleting/truncating causal_event_ids rows (plain PG table; the window-scan path then re-registers), but no shipped tooling, documentation, boot check, or detection exists, and the system cannot self-heal because the healing register at kurrent_event_log.rs:213-215 is only reachable when the registry classifies Absent. (2) Settle bump cite is reactor_runner.rs:1621-1623 (finder said 1617-1622); registry-hit early return is :178-196 (finder said 178-195) — immaterial. (3) Additional confirmed aggravator the finder missed: the registry-hit path also skips ensure_redelivery_identical, leaving v0.19 plan item "Divergence check keys off the registry" (docs/plans/2026-07-02-v0.19-implementation-plan.md:60) unimplemented — on Kurrent, a same-id different-payload redelivery buried beyond the tail window returns silent success, so the A3 record-gated integrity park can never fire for deep replays on the production backend even WITHOUT restore skew. Caveat on scope: the defect fires only under an external operational event (datastore restore, stream delete/truncation), not under the library's own crash-and-retry fault model — but design amendment A3 explicitly declares "PG restore, operator truncate" in-scope and handles it for checkpoints/records, so the registry's missing story in both skew directions is a genuine gap by the project's own standard. Note also that for intentional Kurrent retention-based truncation ($maxAge/$maxCount), swallowing re-appends is arguably correct behavior; the phantom-row direction is unambiguously a defect only for genuine skew (restore/data loss), where it silently defeats the decision-record healing that pre-registry code performed.

Files: `modules/causal_replay/src/kurrent_event_log.rs`, `modules/causal_replay/src/event_id_registry.rs`, `modules/causal/src/reactor_runner.rs`

---

### 10. PG event log silently drops the new events of a partial-overlap batch when the batch TAIL is already persisted (Any/NoStream dedup path)

**Severity:** loss_or_duplication · **Verdict:** confirmed with test · **Status at `dev@1721079`:** still present

**Defect.** On a causal_log_event_id_key violation, PG's recovery path verifies content identity of only the ids that already exist and then requires only the LAST batch event_id to be present — so a batch [eNew, eOld] whose tail eOld is persisted but whose head eNew is not returns Ok(eOld's coordinates) while the whole INSERT (including eNew) was rolled back: eNew is silently lost despite the contract promising partial overlaps fail loudly. The CAS arm and Kurrent's Any path both error loudly on the same shape; PG's Any path is the only silent one, and the conformance suite pins only the mirrored (tail-new) shape.

**Scenario.** A multi-fact append to one stream is retried with changed composition (e.g., a record-GC'd re-decide emits an additional fact ahead of the previously-persisted one, with derived ids keyed by (kind, subject, nth)): append [eNew, eOld] with StreamState::Any → single INSERT aborts on eOld's unique violation → handler runs ensure_redelivery_identical (eNew has no join row so passes; eOld identical so passes) → SELECT WHERE event_id = last_event_id finds eOld → returns Ok. The caller believes the whole batch landed; eNew never enters the log, no error, warn, or observer fires.

**Evidence.** modules/causal_replay/src/event_log.rs:418-470 — constraint handler treats presence of the LAST id alone as proof the whole batch persisted; ensure_redelivery_identical (:134-147) JOINs causal_log so absent ids contribute no divergence row. Loud path exists only in the StreamRevision arm via reconcile (:266, :299-309); Kurrent errors via PartialOverlap (kurrent_event_log.rs:187-194, :219-225). Conformance pins only tail-NEW: modules/causal_replay/src/conformance.rs:711-784.

**Verifier corrections/refinements.** Finder's mechanism, line references, and cross-backend comparison all check out verbatim. Two corrections/sharpenings. (1) Reachability overclaim: the stated scenario ("record-GC'd re-decide emits an additional fact ahead of the previously-persisted one") cannot produce this batch shape through in-tree code — reactor outputs are appended ONE event per append_to_stream call (reactor_runner.rs:1556-1563 for react() outputs, :1849 for terminal facts), so a re-decide with an extra fact yields a fresh single-event append plus a dedup-hit single-event append, never a mixed multi-event batch; Engine::emit multi-fact batches always mint fresh v4 ids (engine.rs:2363) and reject caller-supplied event_id for multi-fact (engine.rs:2310); Engine::append mints fresh ids per attempt; MirroringEventLogBackend passes batches through unchanged. The defect is therefore a public-API/backend-contract violation (event_log.rs:98-105 promises loud rejection) triggerable by direct backend users or future callers, not by today's engine paths. (2) Scope is slightly WIDER than "Any/NoStream": NoStream actually fails loud-ish (first inserted row trips idx_causal_log_stream → typed ConflictError), but the silent path is reachable from Any, StreamExists, AND StreamRevision(n) when the head equals n while an older batch id sits at revision ≤ n (head validation passes, INSERT trips the event_id constraint, same handler). Cross-stream reuse of eOld is caught (placement divergence in ensure_redelivery_identical), so the silent shape requires same-stream, byte-identical persisted tail. Fix shape: in the causal_log_event_id_key handler, classify the batch with the shared reconcile helper (SELECT which of the batch's event_ids exist, in revision order) instead of testing only the tail's presence; add an Any-path tail-old conformance scenario (the suite pins only the CAS tail-new shape at conformance.rs:711-784).

**Proof (failing test output):**
```text
[memory] seeded eOld c3a4a449-36d9-45f5-b74c-94085399581e at pos=1 rev=0
[memory] appending partial-overlap batch [eNew=c7a9654f-1a2a-436d-a5c7-9826b5682cda, eOld=c3a4a449-36d9-45f5-b74c-94085399581e] via Any
[memory] append result: Err("append_to_stream: partial-overlap batch — the batch tail c3a4a449-36d9-45f5-b74c-94085399581e is persisted but event_id c7a9654f-1a2a-436d-a5c7-9826b5682cda is not (event_ids must be all-new or all-already-persisted)")
[memory] eNew persisted: false; stream len: 1
[postgres] seeded eOld d935bccd-05d6-430c-be35-38fb17032938 at pos=1 rev=0
[postgres] appending partial-overlap batch [eNew=176aaa3c-decd-41f2-a652-34d4ef017103, eOld=d935bccd-05d6-430c-be35-38fb17032938] via Any
[postgres] append result: Ok((1, 0))
[postgres] eNew persisted: false; stream len: 1

thread 'main' (2991700) panicked at src/main.rs:115:5:
DEFECT: PG Any-path returned Ok(Ok((1, 0))) for a tail-old partial-overlap batch while silently dropping the new head event (eNew absent from the stream). MemoryStore result for the same shape: Err("append_to_stream: partial-overlap batch — ...")
EXIT=101
```

Failing test preserved at [`2026-07-02-audit-tests/zz_audit_v11.rs`](2026-07-02-audit-tests/zz_audit_v11.rs) — drop into `modules/causal/tests/` to reproduce (RED today; becomes the regression test for the fix).

Files: `modules/causal_replay/src/event_log.rs`, `modules/causal_replay/src/conformance.rs`

---

## C. Replay determinism — state as a pure fold of the log

Multiple pairs of fold paths (live vs restore, decide vs state_of, attempt 1 vs attempt 2) fold different event sets or different transition pairs for the same log.

### 11. Durable restore (replay_events_onto) and live fold (apply_event) disagree on which events fold — id_fn None-skips are applied on restore, so state after a restart differs from state before it

**Severity:** silent_corruption · **Verdict:** confirmed with test · **Status at `dev@1721079`:** still present

**Defect.** restore_aggregate replays a stream via replay_events_onto, which applies every event matching (aggregate_type, event_prefix) WITHOUT consulting json_extract_id, while every live path skips a fact when extract_id returns None (documented skip semantics) — so the restored fold covers a different event SET than the live fold, and state is no longer a pure fold of the log: it depends on whether the process was up when the event arrived.

**Scenario.** Aggregate A registered via for_type_with_id_fn where id_fn returns None for Draft variants. Live life 1: stream [Placed, Draft, Paid], Draft skipped, state = f(Placed, Paid), possibly snapshotted. Restart: engine.state_of → restore_aggregate → replay_events_onto applies Draft too: state = f(Placed, Draft, Paid) ≠ pre-restart state, no error or log line. Second variant of the same asymmetry: a payload that fails F-deserialization is silently skipped live (extract_id .ok()?) but makes restore ERROR (apply_to's from_value fails), so engine.state_of flips from Ok before restart to Err forever after. Consumer registries and fold-on-read never call replay_events_onto, so serial consumers, partitioned reactors, and the engine registry can each hold a different fold of the same log.

**Evidence.** modules/causal/src/aggregator.rs:899-925 (replay_events_onto filters only aggregate_type + event_prefix, calls apply_to unconditionally) vs :446-450 (apply_event: extract_id None => continue). Restore call sites: aggregator.rs:1149-1156, engine.rs:2560-2569 (state_of read-through), aggregator.rs:1013-1025 (repair_gap fast path used by engine.rs:2482-2519 and reactor_runner.rs:1686-1724). Skip semantics documented at aggregator.rs:131-133, :191-199.

**Verifier corrections/refinements.** Substance fully correct; minor fixes: (a) line numbers in the working tree — replay_events_onto is aggregator.rs:914-936 (not 899-925); the apply_event None-skip is aggregator.rs:459-462 (not 446-450); state_of's restore call is engine.rs:2580-2590; repair_gap's restore fast path is aggregator.rs:1025-1038. (b) Scope caveat the finder understates: engine.state_of only restores when a snapshot store is wired (engine.rs:2580), and the None-skip corruption variant requires for_type_with_id_fn + non-empty A::SUBJECT — default for_type aggregators can only hit the deserialize-failure variant (Ok-before-restart → Err-after-restart), not the silent value divergence. (c) One additional supporting fact: the apply_event doc comment (aggregator.rs:413-415) claiming "live fold and replay agree that a bad payload is fatal" is false — extract_id's `.ok()?` (aggregator.rs:220-223) means a bad payload silently skips live and never reaches the fatal apply path, while restore makes it fatal.

**Proof (failing test output):**
```text
thread 'restore_folds_the_same_event_set_as_live' (2954665) panicked at modules/causal/tests/zz_audit_v5.rs:120:5:
assertion `left == right` failed: replay determinism violated: restore folded a different event set than the live fold (restore must skip exactly what live skipped)
  left: Total { value: 1120 }
 right: Total { value: 120 }
test restore_folds_the_same_event_set_as_live ... FAILED
(The pre-restart assertion `live == Total { value: 120 }` PASSED — the live fold, including gap repair, correctly skips the id_fn-None fact; only the restore path applies it.)
```

Failing test preserved at [`2026-07-02-audit-tests/zz_audit_v5.rs`](2026-07-02-audit-tests/zz_audit_v5.rs) — drop into `modules/causal/tests/` to reproduce (RED today; becomes the regression test for the fix).

Files: `modules/causal/src/aggregator.rs`, `modules/causal/src/engine.rs`

---

### 12. A payload that fails aggregate deserialization is silently excluded from every live fold — json_extract_id swallows the error with .ok()?, contradicting the documented 'bad payload is fatal' contract

**Severity:** silent_corruption · **Verdict:** confirmed with test · **Status at `dev@1721079`:** still present

**Defect.** json_extract_id swallows deserialization failures with .ok()? so apply_event treats an undeserializable payload as 'this aggregator does not apply', and gap-repair advances the watermark past it as an identity fold — the event is permanently, silently excluded from aggregate state, while the module contract says an apply error fails the fold, leaving the projection runners' fold-poison park path dead code for this class.

**Scenario.** Schema evolution: fact F gains a required field in v2; pre-migration events no longer deserialize. After deploy, every consumer registry, hydration replay, and eager engine fold silently skips the old events — counters/dedup-gates miss all pre-migration history with zero telemetry: no PROJECTION_FAILED fact, no warn, cursor advances normally. Meanwhile restore_aggregate (no extract_id gate) hard-errors on the same events, so one event yields 'silently skipped' or 'hard error' depending on code path. The test previously_parked_poison_is_skipped_during_hydration passes only because of this silent skip — the park it claims to exercise never occurs on the fold path.

**Evidence.** modules/causal/src/aggregator.rs:220-223 (`serde_json::from_value(payload.clone()).ok()?` — both for_type and for_type_with_id_fn route through it); :446-450 (None => continue, no outcome recorded); :1073-1088 (repair_gap advances the watermark over the skipped event); :413-415 (contradicted contract). Test masked by the skip: modules/causal/src/projection_runner.rs:1024-1088.

**Verifier corrections/refinements.** Finder is substantively correct; minor fixes. (1) Line offsets in the current working tree: the silent skip (`None => continue`) is at aggregator.rs:459-462 (finder said 446-450); the gap-repair watermark advance is at aggregator.rs:1094-1100 within repair_gap (finder said 1073-1088, which is the surrounding function); .ok()? at 220-223 and the contradicted contract at 413-415 are accurate. (2) Mechanism nuance: on direct delivery of the poison event itself, apply_event does NOT advance any watermark (it leaves the entry untouched); it is the NEXT event on the stream that gates as a Gap, and repair_gap then advances the watermark over the poison as an identity fold (advance_watermark, aggregator.rs:1098-1099). Net effect is as claimed: permanent silent exclusion (proven: PingCount.n = 1 with 2 ping events in the log, cursor advanced, zero park facts). (3) One mitigating guard the finder didn't mention: projection_runner.rs:275-283 does park a bad payload — but only when the event type matches the projector's OWN subscribed fact type. Aggregates folded from facts the consumer doesn't dispatch on (dedup gates, cross-fact counters), plus all reactor_runner (reactor_runner.rs:1695) and engine eager folds (engine.rs:2526), route solely through fold_event -> apply_event and get the silent skip. (4) The fold-vs-restore divergence is confirmed exactly: apply_event returns Ok(applied=false) while replay_events_onto (used by restore_aggregate/state_of) returns Err("missing field `id`") on the identical payload. (5) The masked-test claim is confirmed by trace: in previously_parked_poison_is_skipped_during_hydration the poison "ping" never reaches CollectingProjector (Event=Recorded), no PROJECTION_FAILED fact is ever appended (the test doesn't assert one — only cursor advancement, which happens via the non-matching-event branch at projection_runner.rs:270-273), and ensure_hydrated's poison-skip branch (413-421) never executes because fold_event never errors.

**Proof (failing test output):**
```text
running 2 tests
live fold: Ok(false) (applied would be silent-skip), restore replay: Some("missing field `id`")
thread 'live_fold_and_restore_replay_agree_on_bad_payload' panicked at modules/causal/tests/zz_audit_v7.rs:216:5:
assertion `left == right` failed: the same undeserializable payload must be treated the same on the live fold path and the restore/replay path — one silently skipping while the other hard-errors means aggregate state depends on which code path happened to compute it
  left: false
 right: true
step_res.is_err() = false, parked_facts = 0, PingCount.n = 1, cursor = Some(LogCursor(2)) (log has 2 ping events; poison is revision 0)
thread 'bad_payload_must_be_fatal_not_silently_excluded' panicked at modules/causal/tests/zz_audit_v7.rs:171:5:
documented contract: a payload that fails aggregate deserialization is fatal to the fold — the step must error or park a PROJECTION_FAILED fact. Instead the event was silently excluded from PingCount (n = 1) with zero telemetry and the cursor advanced to Some(LogCursor(2))
test live_fold_and_restore_replay_agree_on_bad_payload ... FAILED
test bad_payload_must_be_fatal_not_silently_excluded ... FAILED
test result: FAILED. 0 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```

Failing test preserved at [`2026-07-02-audit-tests/zz_audit_v7.rs`](2026-07-02-audit-tests/zz_audit_v7.rs) — drop into `modules/causal/tests/` to reproduce (RED today; becomes the regression test for the fix).

Files: `modules/causal/src/aggregator.rs`, `modules/causal/src/projection_runner.rs`

---

### 13. maybe_save_snapshots reads (version, state) non-atomically — a concurrent fold between the two reads persists a snapshot whose revision understates its state, and restore double-applies events

**Severity:** silent_corruption · **Verdict:** confirmed with test · **Status at `dev@1721079`:** still present

**Defect.** maybe_save_snapshots reads the watermark (get_version) and the state blob (get_state) in two separate DashMap reads with no guard spanning both; a concurrent fold on the same key between the reads yields Snapshot{revision: V-1, state-including-revision-V}, and restore_aggregate replays the tail after V-1, re-applying revision V onto state that already contains it.

**Scenario.** The shared engine registry is folded concurrently from every emit (engine.rs:2417-2448), Engine::append (engine.rs:2152-2181), and reactor output append (reactor_runner.rs:1628-1652); both engine.rs:2439 and reactor_runner.rs:1642 call maybe_save_snapshots on it. Worker A folds revision V-1 of key K, crosses snapshot_every, reads version=V; an emit on another runtime thread folds revision V into K (apply_event holds only the per-key entry guard A is not holding); A then reads state@V and saves Snapshot{revision: V-1, state@V}. After restart, restore replays after V-1, re-applying V — a counter over-counts by one silently, set_state installs it as canonical (higher version wins), engine.state_of serves corrupted state, and the corruption survives into the next snapshot.

**Evidence.** modules/causal/src/aggregator.rs:1188-1246 (version read at :1216, state read at :1228, snapshot.revision = version-1 at :1238-1244 — no single StateEntry read, no spanning lock); :430-550 (apply_event mutates state+version under only the per-key guard); :1101-1182 (restore seeds from snapshot, replays tail after snapshot.revision). Concurrent callers: engine.rs:2439 and reactor_runner.rs:1642 target the same shared registry from independent tokio tasks.

**Verifier corrections/refinements.** Finder's details are essentially all correct. Two refinements: (a) the skew is not limited to one revision — a concurrent gap-repair burst can fold many revisions between the two reads, so the persisted snapshot can understate its state by dozens of events (observed revision:146 with state n=186), each of which gets double-applied on restore; (b) an additional latent fix-target: maybe_save_snapshots already receives the fold's consistent (prev, next) TransitionSnapshots pair but discards it (aggregator.rs:1209 binds _prev/_next) and re-reads live state — reading the whole StateEntry once under a single DashMap guard (or persisting the post-state actually paired with its fold revision) closes the race.

**Proof (failing test output):**
```text
running 1 test
total successful emits: 8000
skewed snapshots persisted: 496
  Snapshot{ revision: 180, state: {"n":182} }  (expected n == 181)
  Snapshot{ revision: 146, state: {"n":186} }  (expected n == 147)
  Snapshot{ revision: 184, state: {"n":284} }  (expected n == 185)
  Snapshot{ revision: 344, state: {"n":346} }  (expected n == 345)
  Snapshot{ revision: 389, state: {"n":391} }  (expected n == 390)
restored after crash-at-skewed-snapshot: Some(Count { n: 8001 }); pure fold of the log = 8000

thread 'concurrent_folds_never_persist_skewed_snapshots' (2979426) panicked at modules/causal/tests/zz_audit_v6.rs:188:5:
maybe_save_snapshots persisted 496 snapshot(s) whose revision disagrees with the state blob (first: revision 180, state {"n":182}); restore will re-apply already-folded events — silent corruption
test concurrent_folds_never_persist_skewed_snapshots ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.09s
```

Failing test preserved at [`2026-07-02-audit-tests/zz_audit_v6.rs`](2026-07-02-audit-tests/zz_audit_v6.rs) — drop into `modules/causal/tests/` to reproduce (RED today; becomes the regression test for the fix).

Files: `modules/causal/src/aggregator.rs`, `modules/causal/src/engine.rs`, `modules/causal/src/reactor_runner.rs`

---

### 14. FoldOnReadCache returns a degenerate (prev==curr) transition on retry attempts of the same trigger — a transition-gated reactor decides differently on attempt 2 and seals that wrong (possibly empty) decision permanently

**Severity:** loss_or_duplication · **Verdict:** confirmed with test · **Status at `dev@1721079`:** still present

**Defect.** fold_bounded only captures `prev` when an event at exactly position == bound is present in the freshly-read merged set; on any retry of the same trigger the worker-local FoldOnReadCache already has folded_to == bound (warmed by attempt 1's ctx.state_of), so merged is empty and prev silently falls back to a clone of curr — the runtime feeds different (prev, curr) pairs to different attempts of the same trigger, making the RUNTIME nondeterministic even for a pure body.

**Scenario.** Partitioned reactor gates on the transition its own trigger caused (the documented headline use of ctx.state_of): `if prev.n < 3 && curr.n >= 3 { emit Threshold }`. Attempt 1: ctx.state_of folds to trigger position P, correctly returns prev != curr, caches folded_to = P; a later await fails transiently before seal. Attempt 2 (same process_trigger loop, same cache): cache hit, merged empty, prev falls back to curr; the gate is false; body returns zero outputs; with seal_empty_decisions defaulting true an EMPTY decision record seals, and every future redelivery replays the empty record without re-running the body — the Threshold output is lost forever. The serial Registry path deliberately reconstructs the exact (prev, curr) pair on idempotent-skip retries (Skipped{exact_prev}); fold-on-read has no equivalent.

**Evidence.** modules/causal/src/aggregator.rs:1335-1358 (cache reused when folded_to <= bound), :1360-1371 (merged filters position > folded_to — an already-folded bound yields an empty set), :1373-1384 (prev captured only from merged at position == bound), :1391 (prev.unwrap_or_else -> clone of curr). Cache lifetime spans retries: modules/causal/src/reactor_runner.rs:852-916 (one FoldOnReadCache per worker_loop), :979-1136 (retry loop passes the same cache), :1219-1226 (bound = event.position). Permanence: reactor_runner.rs:1156-1160, :1392-1417, :434 (seal_empty_decisions default true). Contrast: aggregator.rs:579-607 (Skipped{exact_prev}). Promise broken: aggregator.rs:1283-1288, modules/causal/src/contexts.rs:283-289.

**Verifier corrections/refinements.** Finding is accurate. Tiny corrections: the prev fallback is aggregator.rs:1433 (not :1391); the cache-reuse guard is :1352 and the tail filter :1380; cache re-insertion with folded_to == bound is :1434-1438. Also worth noting the defect does not require seal_empty_decisions=true to be harmful: even with it false, the retry still decides on a degenerate (curr, curr) pair, so any transition-gated output (or differently-shaped batch) is wrong on attempt 2 and whatever DOES seal is the wrong decision — empty-seal just makes the loss permanent and invisible in the default config. The fix likely needs the cache entry to retain the pre-bound state (or the (prev,curr) pair keyed by bound) so a re-read at bound == folded_to reproduces the exact transition, mirroring the serial path's Skipped{exact_prev} reconstruction (aggregator.rs:591-618).

**Proof (failing test output):**
```text
[WARN causal::reactor_runner] reactor attempt failed; retrying in partition consumer="threshold.reactor" event_id=4bd972e2-... attempts=1 class=Some(Transient) error=injected transient failure after state read

thread 'transition_gate_survives_transient_retry' panicked at modules/causal/tests/zz_audit_v13.rs:175:5:
Threshold output LOST: (prev, curr) pairs per attempt were [(0, 1), (1, 2), (2, 3), (3, 3)] — the retry attempt saw a degenerate prev == curr instead of the (2, 3) transition, decided differently, and sealed that empty decision

test transition_gate_survives_transient_retry ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.16s
```

Failing test preserved at [`2026-07-02-audit-tests/zz_audit_v13.rs`](2026-07-02-audit-tests/zz_audit_v13.rs) — drop into `modules/causal/tests/` to reproduce (RED today; becomes the regression test for the fix).

Files: `modules/causal/src/aggregator.rs`, `modules/causal/src/reactor_runner.rs`, `modules/causal/src/contexts.rs`

---

### 15. Engine::append/load fold only F-typed events — multi-event co-located aggregates hand decide() partial state, so invariants are enforced against wrong state

**Severity:** silent_corruption · **Verdict:** confirmed with test · **Status at `dev@1721079`:** still present

**Defect.** For the documented restorable layout (all of an aggregate's event types co-located on one stream, mandated by aggregate.rs:58-63), Engine::append::<A,F1> and Engine::load::<A,F1> skip every sibling-type event (`continue; // foreign co-located type`, engine.rs:2129/2188) during the OCC fold, so the state the invariant-enforcing decide closure sees is missing all F2 transitions, while every read path (state_of, ctx.state_of, restore) folds the full multi-type state — two different states for one aggregate depending on API, with the CAS succeeding silently.

**Scenario.** Account aggregate with Apply<Deposited>+Apply<Withdrawn>, both registered via with_aggregators. append Deposited(100), then append::<Account,Withdrawn> with a balance>=100 guard: the fold skips the Deposited event, decide sees balance 0, and the withdrawal is wrongly rejected — or for opposite-polarity invariants, wrongly accepted, durably writing an invariant-violating fact — while state_of simultaneously reports the correct balance of 100.

**Evidence.** Partial-fold skip verified at engine.rs:2186-2191 (append) and 2126-2133 (load). Docs mandating co-location: aggregate.rs:58-63; multi-aggregator registration blessed at engine.rs:1351-1370; INVARIANT makes append the only write door (occ_categories insert per event_prefix, engine.rs:1377). Read paths fold all types: aggregator.rs:455/478/542-559 (shared registry key), 1319-1408 (fold_bounded), 914-936/1162-1168 (restore).

**Verifier corrections/refinements.** All cited lines check out (engine.rs:2126-2133 load skip, 2185-2192 append skip, 1351-1370 multi-aggregator blessing, 1376-1378 OCC fence per event_prefix; aggregate.rs:58-70 SUBJECT co-location mandate; aggregator.rs shared-key full-type folds). Two additions the finder missed: (1) the partial fold is INTENTIONAL and pinned by an existing internal test — engine.rs:4919-4963 `decider_keys_on_subject_and_skips_foreign_types` asserts load::<Balance,Deposited> returns 100 (not the full-fold 70) on a co-located Deposited(100)+Withdrawn(30) stream — so this is a design incoherence to reconcile, not an accidental skip; notably that test never checks state_of, which would have exposed the 100-vs-70 divergence. (2) The public rustdoc of `load` (engine.rs:2102, "Hydrate an aggregate by folding its full stream from the log") affirmatively misleads users; only non-doc inline comments admit the F-only fold, and nothing in docs/ documents the limitation or guards the hazardous configuration (an INVARIANT aggregate with multiple Apply impls on one SUBJECT builds without warning). Severity revised upward from incorrect_result: the wrong-acceptance polarity returns Ok and durably appends a fact violating the declared invariant with zero detection (state_of shows frozen=true while the withdraw fact sits in the stream) — silently corrupted domain state, not just a wrong return value.

**Proof (failing test output):**
```text
running 2 tests
decide observed balance = 0; state_of reports balance = 100
thread 'append_decide_sees_full_state_sufficient_funds_withdrawal_accepted' panicked at modules/causal/tests/zz_audit_v101.rs:166:5:
assertion `left == right` failed: decide and state_of must agree on one aggregate's state (decide saw 0, state_of saw 100)
  left: 0
 right: 100
append on frozen account returned Ok = true; durable withdraw events = 1
thread 'append_decide_sees_frozen_flag_withdrawal_on_frozen_account_rejected' panicked at modules/causal/tests/zz_audit_v101.rs:245:5:
withdrawal on a frozen account must be refused by the invariant guard
test append_decide_sees_full_state_sufficient_funds_withdrawal_accepted ... FAILED
test append_decide_sees_frozen_flag_withdrawal_on_frozen_account_rejected ... FAILED
test result: FAILED. 0 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```

Failing test preserved at [`2026-07-02-audit-tests/zz_audit_v101.rs`](2026-07-02-audit-tests/zz_audit_v101.rs) — drop into `modules/causal/tests/` to reproduce (RED today; becomes the regression test for the fix).

Files: `modules/causal/src/engine.rs`, `modules/causal/src/aggregate.rs`, `modules/causal/src/aggregator.rs`

---

### 16. build() never validates A::SUBJECT == F::SUBJECT for restorable aggregates — a one-string mismatch ships cleanly, then every fold deterministically fails (park storm + state_of returns None forever while appends succeed)

**Severity:** incorrect_result · **Verdict:** confirmed with test · **Status at `dev@1721079`:** still present

**Defect.** Both strings are statically present on the Aggregator struct (subject and event_subject) but EngineBuilder::build() only validates colon-format names, so a SUBJECT mismatch builds cleanly: writes keep landing on {F::SUBJECT}-{id} while every registry fold hits the runtime alignment bail (aggregator.rs:463-476) — projectors park each event of that type (a terminal-fact park storm), engine-side warm folds are swallowed as warns, and state_of restores from the wrong (empty) {A::SUBJECT}-{id} stream and returns Ok(None) forever despite the aggregate having durable events. A static string comparison at build time would catch it.

**Scenario.** impl Aggregate for Order { const SUBJECT = "order" } but the #[event] omits subject= (Event::SUBJECT defaults to NAME "order_placed") or typos "orders". Appends succeed durably; all reads report 'no aggregate'; projection runners park every OrderPlaced with cursor advance; detectable only via logs.

**Evidence.** aggregator.rs:93-95/100-106/204-206 (both strings on the struct); engine.rs:1633-1642 (only validate_name colon checks, no subject comparison anywhere); runtime bail aggregator.rs:463-476; park path projection_runner.rs:229-263/328-356; swallowed warm folds engine.rs:2603-2630, reactor_runner.rs:1687-1723; state_of None path engine.rs:2657-2688, aggregator.rs:1121-1122/1171-1174.

**Verifier corrections/refinements.** Finding is accurate with one refinement: the projection-runner "park storm" is not one park per delivery attempt — the stream-alignment bail is an unclassified error (no ClassifiedError in the chain), so FailureState::on_failure (modules/causal/src/projection_failure.rs:122-129) makes the projector retry the same event max_attempts times (with supervisor backoff) before parking it as class "unclassified" and advancing (projection_runner.rs:256-263 + 328-356). Every event of the mismatched kind burns the full attempt budget then parks — slower and noisier than claimed, same terminal outcome. All other cited mechanics verified as claimed: no subject comparison at build (engine.rs:1637-1642 only colon-validates), both strings statically available on Aggregator (aggregator.rs:95/106, set at 204/206), runtime bail (aggregator.rs:468-476), swallowed engine-side warm fold (engine.rs:2594-2631), state_of restoring from the empty {A::SUBJECT}-{id} stream and returning Ok(None) (engine.rs:2657-2688, aggregator.rs:1121-1122/1171-1174). Also note fold_bounded (ctx.state_of in partitioned reactors) reads event_subject streams and bypasses apply_event, so that one path would still return correct state — deepening the inconsistency rather than mitigating it. Fix is the one the finder suggests: in build(), for each aggregator with non-empty subject, bail if agg.subject != agg.event_subject.

**Proof (failing test output):**
```text
build() ACCEPTED the A::SUBJECT != F::SUBJECT wiring
emit result: Ok("ok")
thread 'subject_mismatch_is_rejected_at_build_or_still_folds' panicked at modules/causal/tests/zz_audit_v103.rs:119:5:
assertion `left == right` failed: DEFECT: emit() committed the fact durably but state_of() sees nothing — the SUBJECT mismatch shipped cleanly through build(), every fold hits the runtime alignment bail (swallowed as a warn), and restore reads the empty account-{id} stream
  left: None
 right: Some(Balance { value: 100 })
thread 'subject_mismatch_restart_restores_nothing' panicked at modules/causal/tests/zz_audit_v103.rs:167:5:
assertion `left == right` failed: DEFECT: durable event exists on deposit-617519a6-... but a restarted engine restores from the empty account-617519a6-... stream and reports no aggregate
  left: None
 right: Some(Balance { value: 42 })
test result: FAILED. 0 passed; 2 failed; 0 ignored
(intermediate assertions passed: durable append present on deposit-{id}; account-{id} stream empty)
```

Failing test preserved at [`2026-07-02-audit-tests/zz_audit_v103.rs`](2026-07-02-audit-tests/zz_audit_v103.rs) — drop into `modules/causal/tests/` to reproduce (RED today; becomes the regression test for the fix).

Files: `modules/causal/src/engine.rs`, `modules/causal/src/aggregator.rs`, `modules/causal/src/projection_runner.rs`

---

### 17. No global uniqueness check on Event::NAME — two distinct Rust types sharing NAME+SUBJECT make append's hard-`?` fold cross-deserialize the sibling's payload, permanently wedging that aggregate id

**Severity:** wedge_or_livelock · **Verdict:** confirmed with test · **Status at `dev@1721079`:** still present

**Defect.** The builder deduplicates only (aggregate_type, event_prefix) pairs (engine.rs:1345-1370) and never detects two DIFFERENT Rust event types (different TypeId, incompatible payloads) registered under the same NAME string; append's OCC fold deserializes every same-NAME stream event with a hard `?` (engine.rs:2186-2191), so one aggregate's fact in the shared stream permanently errors the other aggregate's append AND restore for that id — and both names being OCC-fenced means there is no alternate write door. The Aggregator struct already carries event_type_id (aggregator.rs:89-90) but build() never reads it.

**Scenario.** Two bounded contexts each declare #[event(name="status_changed", subject="job")] with different payload shapes folded by invariant aggregates A and B. append::<B,H>(id) writes H's payload; every later append::<A,F>(id) hard-errors on serde_json::from_value::<F>(H_payload) at every attempt; restore_aggregate/state_of hit the same error via apply_to (aggregator.rs:224-230, 930-931).

**Evidence.** Duplicate-check scope engine.rs:1345-1370; hard-fail fold engine.rs:2186-2191 and 2126-2133; restore hard-fail aggregator.rs:914-936/224-230; event_type_id present but unused in engine.rs. Raw finding was confidence:hypothesis — the mechanism is line-verified but the dual-registration entry path was not compile-checked.

**Verifier corrections/refinements.** Mechanism confirmed as claimed with two refinements. (1) The wedge does not require aggregator registration at all: Engine::append::<A,F> (engine.rs:2160) is driven purely by type parameters, so ANY two Rust event types sharing NAME+SUBJECT cross-deserialize in the fold (hard `?` at engine.rs:2190) and in load (engine.rs:2131). Registration of the two invariant aggregates matters only for the 'no alternate write door' half: both insert the same NAME into occ_categories (engine.rs:1376-1378), fencing emit (engine.rs:2420-2427). (2) The builder dedup check at engine.rs:1357-1370 requires same aggregate_type AND same event_prefix, so different-aggregate/same-NAME registration passes silently — verified by compile+run, closing the finder's 'not compile-checked' gap. Aggregator.event_type_id (aggregator.rs:90) is confirmed write-only (grep: set at aggregator.rs:202/212, never read). Failure surface: append errors 'missing field `status`' permanently (serde error is not ConflictError, so no OCC retry), load errors identically, emit is fenced — no write door and no read for that aggregate id, and the error message names neither colliding type. Additional hazard beyond the finding: when the sibling payloads ARE structurally compatible, the fold silently applies the sibling's facts to the wrong aggregate (silent corruption instead of a wedge).

**Proof (failing test output):**
```text
running 1 test
emit(JobStatusA) => Some("category 'status_changed' is OCC-required (registered via with_aggregate); use Engine::append::<A, F>(id, decide) instead of emit()")
append<LifecycleA, JobStatusA> attempt 1 => Some("missing field `status`")
append<LifecycleA, JobStatusA> attempt 2 => Some("missing field `status`")
load<LifecycleA, JobStatusA> => Some("missing field `status`")

thread 'same_name_sibling_type_must_not_wedge_appends' (3088336) panicked at modules/causal/tests/zz_audit_v104.rs:161:5:
aggregate id b772dd63-2646-4885-831d-0977dce703df is WEDGED for context A by context B's same-NAME fact: append errors permanently (attempt1: Some("missing field `status`"), attempt2: Some("missing field `status`")) and emit is OCC-fenced (Some("category 'status_changed' is OCC-required (registered via with_aggregate); use Engine::append::<A, F>(id, decide) instead of emit()")) so there is no write door at all
test same_name_sibling_type_must_not_wedge_appends ... FAILED

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```

Failing test preserved at [`2026-07-02-audit-tests/zz_audit_v104.rs`](2026-07-02-audit-tests/zz_audit_v104.rs) — drop into `modules/causal/tests/` to reproduce (RED today; becomes the regression test for the fix).

Files: `modules/causal/src/engine.rs`, `modules/causal/src/aggregator.rs`

---

### 18. Uuid type alias defeats #[event] subject-shape inference — the fact silently becomes nil-subject fan-in, collapsing all instances into one stream/aggregate/partition

**Severity:** silent_corruption · **Verdict:** confirmed with test · **Status at `dev@1721079`:** still present

**Defect.** candidate_subject_fields detects candidate subject fields by syntactic type-name match (last path segment == "Uuid", lib.rs:606-614), so a field typed via a domain alias (type OrderId = Uuid) is invisible; the fact compiles with no subject declaration, require_subject_identity treats it as provably subject-less (lib.rs:577-580), and the macro generates subject_id() -> Uuid::nil() (lib.rs:863-874) — re-creating the pre-0.9 '{category}-nil' fan-in the shape gate exists to prevent, violating the macro's own 'inference only where wrongness is impossible' contract.

**Scenario.** type OrderId = Uuid; #[event(name="order_placed")] struct OrderPlaced { order_id: OrderId, ... } — every OrderPlaced for every order lands in one nil stream; a default aggregator folds ALL orders into the single key 'Order:0000...' (aggregator.rs:169-176, 478); PerSubject reactor ordering collapses to one partition. No compile, build, or runtime error.

**Evidence.** lib.rs:571-599/605-641/863-874; aggregator.rs:169-176/478; reactor.rs:111 (default PerSubject).

**Verifier corrections/refinements.** Finding is accurate as stated. Two small refinements: (a) the same bypass also applies to `use uuid::Uuid as SomeName;` renames and to any re-exported alias, not just `type X = Uuid` — anything whose last path segment isn't literally "Uuid"; conversely `uuid::Uuid` fully-qualified is caught. (b) The bypass only bites facts that OMIT subject_id entirely; an explicit `subject_id = "order_id"` on an alias-typed field still works correctly (the generated `self.order_id` is a Uuid through the alias). Also note the alignment guard at aggregator.rs:468 does not help: it only fires for restorable aggregates (non-empty SUBJECT) on key mismatch, and with nil subjects aggregate_id == subject_id == nil, so it passes.

**Proof (failing test output):**
```text
thread 'alias_typed_id_field_must_not_collapse_subjects_to_nil' panicked at modules/causal/tests/zz_audit_v108.rs:75:5:
assertion `left != right` failed: DEFECT: alias-typed id field silently produced the nil subject (shape gate bypassed; pre-0.9 fan-in re-created)
  left: 00000000-0000-0000-0000-000000000000
 right: 00000000-0000-0000-0000-000000000000
test alias_typed_id_field_must_not_collapse_subjects_to_nil ... FAILED

state_of(order_a) = None
state_of(nil)     = Some(OrderTotal { placed: 2, cents: 350 })
thread 'alias_typed_id_field_must_not_fan_in_all_orders_into_one_aggregate' panicked at modules/causal/tests/zz_audit_v108.rs:127:5:
assertion `left == right` failed: DEFECT: per-order aggregate read at the real order id is wrong (events were folded elsewhere)
  left: None
 right: Some(OrderTotal { placed: 1, cents: 100 })
test alias_typed_id_field_must_not_fan_in_all_orders_into_one_aggregate ... FAILED

test result: FAILED. 0 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```

Failing test preserved at [`2026-07-02-audit-tests/zz_audit_v108.rs`](2026-07-02-audit-tests/zz_audit_v108.rs) — drop into `modules/causal/tests/` to reproduce (RED today; becomes the regression test for the fix).

Files: `modules/causal_core_macros/src/lib.rs`, `modules/causal/src/aggregator.rs`

---

## D. Multi-node reality — leases, deploy overlap, blue/green

Everything here assumes two processes touch one store, which the lease layer permits silently.

### 19. PgConsumerLeasor zombie holders are undetectable by construction — empty LeaseGuard trait, never-revalidated idle connection; a dropped PG session silently dual-drives every consumer with no fencing token

**Severity:** silent_corruption · **Verdict:** confirmed by trace · **Status at `dev@1721079`:** still present

**Defect.** The lease guard is a bare struct holding an idle PgConnection with zero liveness machinery — `pub trait LeaseGuard: Send + Sync {}` has no method through which loss could even be surfaced — and the runner stores the guard once at ensure_started and never touches it again; when the session dies server-side, the advisory lock releases, a second engine acquires, and both engines drive the same consumer indefinitely, while with_consumer_leasor's docs claim concurrent drive is impossible.

**Scenario.** Engine A acquires leases at boot; the dedicated lease connections sit idle forever (no query ever issued after acquire). A NAT/LB or pgbouncer reaps the idle TCP session (5-30 min idle timeouts are common); PG releases the session advisory locks; standby engine B's blocked pg_advisory_lock returns and B starts every consumer; A has no way to notice. Damage while both run: (1) projectors have no decision records or output dedup — both engines execute project() side effects, and the monotonic-only (GREATEST) cursor cannot order SINK writes, so a blind-upsert read model regresses permanently while the checkpoint says otherwise; (2) with seal_empty_decisions(false), no-op deliveries leave no record, so B re-runs bodies A already completed — a nondeterministic body produces a second different outcome for the same trigger; (3) A's floor-GC deletes effect-store entries for triggers B is concurrently re-executing, re-firing un-memoized external calls.

**Evidence.** modules/causal/src/consumer_lease.rs:25 — `pub trait LeaseGuard: Send + Sync {}` (empty). modules/causal_replay/src/consumer_lease.rs:50-56, 77-89 — PgLeaseGuard holds `_conn: sqlx::PgConnection`, acquire() runs one pg_advisory_lock and never touches the connection again; no heartbeat, keepalive, or reconnect-and-reacquire. modules/causal/src/reactor_runner.rs:793-812 — guard stored in self.lease, never read again (only lines 392, 466, 801). modules/causal/src/engine.rs:1122-1138 — doc claims safety. Unfenceable sink writes: modules/causal/src/projection_runner.rs:271, 305, 343; modules/causal_replay/src/reactor_checkpoint.rs:80-97. Design doc itself flags the missing fencing token (A1 zombie note) but only the GC consequence was mitigated.

**Verifier corrections/refinements.** Core defect confirmed as stated: empty LeaseGuard trait (modules/causal/src/consumer_lease.rs:25), never-touched idle dedicated PgConnection (modules/causal_replay/src/consumer_lease.rs:50-89), guard stored once and never revalidated (modules/causal/src/reactor_runner.rs:793-812; ensure_started short-circuits on `started` every subsequent step), versus the unqualified exclusivity promise at modules/causal/src/engine.rs:1110-1121. Corrections to damage claims: (a) reactors with a decision store and default seal_empty_decisions=true are largely chimera-proof even under dual-drive — reactor_runner.rs:1383-1423 appends from the sealed first-write-wins batch on both engines, and appends are idempotent — so "dual-drives every consumer" is true for drive but not for reactor outcome divergence; (b) the nondeterministic-second-outcome scenario requires the opt-in .seal_empty_decisions(false); (c) the effect-store floor-GC re-fire (advance_floor, reactor_runner.rs:1917-1966) is real but a narrow per-trigger race window that recurs while dual-drive persists; (d) the unmitigated, always-on corruption surface is projectors/multi-projectors — no decision records, no effect memoization (projection_runner.rs:297 passes effect_store: None), GREATEST-only checkpoint (causal_replay/src/reactor_checkpoint.rs:80-97) — where interleaved project() side effects from two engines can leave a read model permanently regressed relative to its checkpoint if the zombie dies while lagging. Also note the simplest trigger is not exotic NAT reaping: any PG restart/failover releases every session advisory lock at once while all surviving engines continue believing they hold theirs, and a blocked/retrying standby (supervise_one backoff at engine.rs:3186) immediately acquires and dual-drives. Design doc A1 acknowledges the missing fencing token but only the GC consequence was mitigated; the shipped docs still promise full exclusivity.

Files: `modules/causal_replay/src/consumer_lease.rs`, `modules/causal/src/consumer_lease.rs`, `modules/causal/src/reactor_runner.rs`, `modules/causal/src/projection_runner.rs`

---

### 20. EngineBuilder::build() performs unleased cursor surgery against a live peer: stale-tip clamp_ahead_of regresses concurrently-advancing checkpoints (TOCTOU), and StartPosition seeds clobber a running holder's cursor

**Severity:** loss_or_duplication · **Verdict:** confirmed with test · **Status at `dev@1721079`:** still present

**Defect.** build() reads latest_position() and then clamps every checkpoint (both stores, all consumers) above that snapshot, and unconditionally set()s reactor cursors for Latest/Specific starts — all before any lease is acquired (leases attach at each runner's first step) — so during normal deploy overlap the booting engine's stale tip misclassifies a live peer's legitimately-advanced cursors as 'restored past the tip' and rewrites them downward via the absolute-write paths.

**Scenario.** Deploy overlap: engine A (lease holder) processing; engine B boots, build() reads tip T0; before B's two clamp calls run, A appends T0+1..T1 and persists floor T1. B's clamp (`UPDATE causal_checkpoints SET position = $1 WHERE position > $1`) regresses A's cursor T1→T0 with a spurious 'event store restored?' warning. A's in-memory persisted_floor still says T1 and advance_floor only writes when the in-memory floor CHANGES, so the durable regression stands; if A is killed by the deploy, B seeds from T0 and redelivers (T0, T1] — decision records absorb the reactor side, but every projector re-executes project() for that range (duplicate side effects on non-idempotent sinks), repeated per deploy under load. Separately, StartPosition::Specific(c) seeds rewrite the cursor while A holds the lease and is mid-stream; A's next monotonic advance silently undoes the operator's rewind — the seed's effect depends on a race with the incumbent.

**Evidence.** modules/causal/src/engine.rs:1646-1657 — tip read then clamps with no lease and no atomicity between the awaits; StartPosition seeds :1661-1684 via absolute reactor_checkpoint.set. Lease acquisition only at first step: modules/causal/src/reactor_runner.rs:797-802, modules/causal/src/multi_projector.rs:178-186. Absolute downward clamp writes: modules/causal_replay/src/reactor_checkpoint.rs:99-113, modules/causal/src/memory_store.rs:655-665. Non-re-persist window after regression: reactor_runner.rs:1941-1952 (`if d.persisted_floor != Some(d.floor)` gate).

**Verifier corrections/refinements.** Mechanism, files, and consequence chain are as claimed, with three corrections/sharpening: (1) Line numbers in the working tree: tip read + clamps at modules/causal/src/engine.rs:1660-1672, StartPosition seeds at :1675-1700; PG clamp at modules/causal_replay/src/reactor_checkpoint.rs:105-113; memory clamp at modules/causal/src/memory_store.rs:655-665; non-re-persist gate at modules/causal/src/reactor_runner.rs:1941; lease-at-first-step at reactor_runner.rs:793-812 and multi_projector.rs:178-190. (2) Frequency overclaim: the race window is the span between the Kurrent latest_position() RPC return and the two sequential PG clamp UPDATEs — milliseconds; the incumbent must persist a floor advance inside it, so it fires probabilistically per deploy under sustained load, not "repeated per deploy". (3) Stronger than claimed in one respect: the clamp block runs unconditionally even for an engine with zero registered consumers, and the PG UPDATE has no consumer filter — any process that merely build()s against the shared database (an emit-only client, a CLI) can regress cursors of consumers it does not host. Impact is duplication-only (no loss): reactor-side redelivery is absorbed by decision records + idempotent appends, but projectors re-execute project() over the regressed range (duplicate side effects on non-idempotent read-model sinks) and the regression re-delivers acked events, violating the checkpoint trait's own documented no-regress invariant. The Specific(c) seed race is real but secondary: nondeterministic outcome of an operator rewind racing an incumbent's monotonic advance.

**Proof (failing test output):**
```text
thread 'build_clamp_must_not_regress_concurrently_advanced_peer_cursor' (2983646) panicked at modules/causal/tests/zz_audit_v14.rs:132:5:
assertion `left == right` failed: a booting engine's stale-tip clamp regressed a live peer's legitimately-advanced cursor: cursor=LogCursor(5) live_tip=LogCursor(8) (stale snapshot was LogCursor(5)) — redelivers (LogCursor(5), LogCursor(8)] on handover
  left: LogCursor(5)
 right: LogCursor(8)
test build_clamp_must_not_regress_concurrently_advanced_peer_cursor ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```

Failing test preserved at [`2026-07-02-audit-tests/zz_audit_v14.rs`](2026-07-02-audit-tests/zz_audit_v14.rs) — drop into `modules/causal/tests/` to reproduce (RED today; becomes the regression test for the fix).

Files: `modules/causal/src/engine.rs`, `modules/causal_replay/src/reactor_checkpoint.rs`, `modules/causal/src/memory_store.rs`, `modules/causal/src/reactor_runner.rs`

---

### 21. PointerStore's single row conflates blue/green version identity, replay progress, and live cursor; promote() is unfenced — a half-replayed position can be promoted and the documented db-naming pattern breaks after the first live save

**Severity:** silent_corruption · **Verdict:** confirmed by trace · **Status at `dev@1721079`:** still present

**Defect.** The one causal_replay_pointer row (CHECK id = 1, no projection key, pointer.rs:60-66) serves three mutually-clobbering roles with no ownership fencing: (a) the documented `neo4j.v{position}` naming pattern (stream.rs:161-186) breaks because run_live saves raw log-cursor progress into `active` (stream.rs:364/409), so the next boot derives a database name no replay ever created and connects to an empty projection while the pointer says caught-up; (b) staged doubles as mid-replay progress checkpoint (stream.rs:255-257/293) AND promotion candidate, and promote() (pointer.rs:117-127) swaps whatever staged holds with no token/CAS — overlapping replay runs (k8s Job retry) or an operator promote after a mid-replay crash publish a 40%-built projection as active; (c) any second ProjectionStream sharing the database silently shares and clobbers the one row, skipping its own backlog.

**Scenario.** Fast replay run A finishes and stages final_A; slow duplicate run B stages its 40% checkpoint p_B; A's gate passes and A promotes → active = p_B, a half-replayed position published as the completed projection version. Separately, any live deployment using the documented db-naming pattern silently serves an empty read model after its first live batch.

**Evidence.** pointer.rs:60-66/117-127; stream.rs:161-186 (position() doc) vs 364/409/437/473 (live saves into active), 255-257/293 (staged as progress), 305-329 (finish_replay with no fencing). tests/stream_tests.rs covers only single-process single-projection interleavings.

**Verifier corrections/refinements.** Three corrections/precisions. (1) Provenance: pointer.rs and stream.rs are pre-0.19 legacy code (last touched around 0.17.1; git log shows no 0.19-slice commits) — this is not a defect in the new decision-records machinery, and it violates the ProjectionStream/PointerStore module's own documented blue/green contract (causal_replay/src/lib.rs:19-20, pointer.rs:9-12) rather than the five core engine promises. (2) The 'silently serves an empty read model' claim is conditional on the app creating the database if missing; a connect-only client (e.g. Neo4j session against a nonexistent db) fails loudly instead. However the module's unified no-branch design pushes apps toward create-if-missing, since replay mode must create the new versioned db with the same code path, so the silent variant is the natural implementation. (3) The naming break is over-determined beyond the finder's mechanism: even with zero live saves, run_replay keeps reading until read_all is empty, so if events land during replay the promoted `active` (final position) exceeds the staged target used to derive the db name, and the next live boot already connects to a never-created database. Sub-claim (c) (singleton row shared by a second ProjectionStream) is real by construction but is best characterized as an undocumented single-tenant constraint/footgun — mitigable with one PG database per projection — aggravated by lib.rs:59 advertising multi-target replay (REPLAY_TARGETS) with no per-target pointer keying. Minor line-number nit: the CREATE TABLE is pointer.rs:60-65 and the position() doc block is stream.rs:161-174 with the method at 175-186; all citations otherwise accurate, including the extra live-save sites at stream.rs:437/473.

Files: `modules/causal_replay/src/pointer.rs`, `modules/causal_replay/src/stream.rs`

---

### 22. Live-mode batch projection advances the pointer past a FAILED batch — one transient sink error permanently drops up to batch_size (default 1000) events from the read model

**Severity:** loss_or_duplication · **Verdict:** confirmed by trace · **Status at `dev@1721079`:** still present

**Defect.** run_batch_live (catch-up and tail loops) logs a batch apply() error as a warning, then advances position to the last event of the FAILED batch and persists it (stream.rs:430-437, 466-473); the per-event run_live does the same per event (stream.rs:355-364). No retry, no dead-letter, no poison-vs-transient distinction — one connection blip converts at-least-once into at-most-once and the read model silently diverges until a full REPLAY=1 rebuild. Test-pinned as intended behavior (tests/stream_tests.rs:849-879), so this is a deliberate design choice to re-examine, not a regression; PgEventProjector (event_projector.rs:59-72) handles the identical case correctly by leaving the checkpoint untouched.

**Scenario.** During live catch-up, Neo4j has a 2-second blip; apply() for one 1000-event batch errors; the pointer advances past all 1000 events, which are permanently absent from the live read model with no marker.

**Evidence.** stream.rs:430-437/466-473/355-364; counter-pattern event_projector.rs:64-72; intent lock stream_tests.rs:849-879.

**Verifier corrections/refinements.** All cited lines and mechanisms are accurate. Two additions: (1) The finder's counter-pattern list is incomplete in a way that strengthens the finding — the core ProjectionRunner (modules/causal/src/projection_runner.rs) also implements the correct policy (C2: cursor advances only on Ok; transient errors retry; poison events are parked to causal_projection_failures with a durable `causal:projection_failed` marker fact then advanced past). ProjectionStream is thus the only one of THREE projection paths in the repo that advances past failures, and the only one leaving no durable marker. (2) Scope nuance: ProjectionStream is pre-0.19 code (present since at least v0.5.0 per git history), publicly exported from causal_replay (lib.rs:84), so this is legacy surface rather than new 0.19 machinery; the error policy is undocumented on the public API (run/run_batch doc comments say only "the consumer owns atomicity within the batch"). Minor precision: in per-event run_live the pointer is persisted per batch (stream.rs:364), not per event, but the in-memory position variable advances past each failed event regardless, so the loss granularity is per event as claimed.

**Proof (failing test output):**
```text
$ cargo test -p causal_replay --test stream_tests run_batch_live_log_and_continue_on_error -- --nocapture
running 1 test
test run_batch_live_log_and_continue_on_error ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 30 filtered out; finished in 0.20s

(Existing repo test, executed against the working tree: 10 events appended, batch_size=5, first batch's apply() bails; test asserts — and the run confirms — pointer.active() == LogCursor::from_raw(10), i.e. the pointer committed past the 5 dropped events, which read_all(after=10) will never redeliver.)
```

Files: `modules/causal_replay/src/stream.rs`, `modules/causal_replay/src/event_projector.rs`

---

## E. Wedges & livelocks

Each of these freezes a partition, a consumer, or the whole ack floor, most of them invisibly to every existing guard.

### 23. park_terminal_failure appends its terminal fact with a bare `?` and no DivergentRedelivery handling — a re-park with a changed attempts/error payload livelocks the partition forever

**Severity:** wedge_or_livelock · **Verdict:** confirmed with test · **Status at `dev@1721079`:** still present

**Defect.** The terminal fact's event_id is deterministic (consumer, trigger, kind, subject, nth=u32::MAX) but its payload embeds run-varying fields (attempts count, error string); park_terminal_failure appends it with a raw `?` and no DivergentRedelivery downcast (unlike the output loop, which 0.15's 47a279a fixed), so a redelivered park whose payload differs is rejected as divergent, misclassified by process_trigger's park arm as retryable park-infrastructure I/O, and retried forever — each cycle re-running the body (re-firing un-memoized side effects) and re-deriving a payload that can never match the persisted row.

**Scenario.** Trigger T parks: terminal fact appended with payload {"attempts": 8, "error": ...}; crash in the window between the append (reactor_runner.rs:1847-1850) and clear_reactor_attempts (:1870-1872)/ack. Restart: T redelivered, durable attempt counter uncleared, body fails once, record_reactor_attempt returns 9, park re-derives the SAME event_id with payload {"attempts": 9} → MemoryStore/PG raise DivergentRedelivery → park_terminal_failure propagates Err → park arm logs 'terminal-failure park I/O error', sleeps, loops → attempt 10, diverges again — forever. The trigger never acks, the ack-floor freezes below it (compounding the GC-floor finding), ingestion stops at MAX_PENDING, settle() on workflows behind it wedges, and restarts do not heal. TransientExhausted parks (timing-dependent attempt counts, nondeterministic error strings) reach the same wedge without the crash window. The projection park path has the identical shape (deterministic id, payload embeds attempts+error, raw `?`), mitigated only by its in-memory counter.

**Evidence.** Unguarded append: modules/causal/src/reactor_runner.rs:1847-1850 (contrast append_outputs :1567-1606 which downcasts DivergentRedelivery). Run-varying payload under stable id: :1818-1825 (attempts, error, class) with :1829-1835 (id derived without them, nth=u32::MAX). Park Err arm retries the whole attempt: :1072-1136 (note_worker_retry → sleep → loop → body re-runs). Counter cleared only on success or after successful park: :1165-1167, :1298-1300, :1870-1872; process-lifetime transient clock: modules/causal_replay/src/reactor_checkpoint.rs:26-36, 118-127. Divergence raised on payload mismatch: modules/causal/src/memory_store.rs:454-512; modules/causal_replay/src/event_log.rs:116-169 (doc: retrying divergence 'can never succeed'); Kurrent window path kurrent_event_log.rs:206. Projection analogue: modules/causal/src/projection_failure.rs:208-229. Prior art fixing only the output site: commit 47a279a.

**Verifier corrections/refinements.** The finding is correct as stated; two refinements. (1) The precondition is WEAKER than the claimed crash window: no crash is required — any single failure between the terminal-fact append (reactor_runner.rs:1850) and a durable ack, e.g. one transient clear_reactor_attempts error (:1870-1872), livelocks the partition forever within a single process (proven by test: 4314 body re-runs / 4313 DivergentRedelivery in 10 s, floor frozen). (2) One route in the finder's scenario space is actually safe: a crash after clear_reactor_attempts succeeded does NOT wedge Poison-class parks, because the counter restarts at 1 and a deterministic error string reproduces a byte-identical payload (dedup-hit). The wedge requires the append→clear window, or classes whose payload drifts across deliveries (Domain after max_attempts, TransientExhausted, nondeterministic error text). Fix shape: mirror append_outputs' DivergentRedelivery downcast (:1567-1605) in park_terminal_failure — accept the persisted terminal row as canonical and proceed to clear/ack — and/or drop run-varying fields (attempts, error) from the deterministic-id fact payload. The projection park path (projection_failure.rs:208-239) needs the same treatment.

**Proof (failing test output):**
```text
running 1 test
AUDIT15: floor_advanced=false body_runs=4314 divergent_redeliveries=4313 terminal_facts=1 (payload of first: {"attempts":1,"class":"poison","consumer":"r.audit15","error":"deterministic poison: unprocessable trigger: deterministic poison: unprocessable trigger","trigger_event_type":"trigger","trigger_id":"b85da31b-4e32-4fd4-8cd1-d1291e608f4a"})

thread 'transient_blip_after_terminal_fact_append_must_not_livelock_the_park' (2996501) panicked at modules/causal/tests/zz_audit_v15.rs:257:5:
LIVELOCK CONFIRMED: one transient clear_reactor_attempts blip after the terminal-fact append left the trigger permanently unackable — the park loop re-ran the reaction body 4314 times and hit DivergentRedelivery 4313 times (same deterministic event_id, attempts-counter payload drift), never advancing the ack floor
test transient_blip_after_terminal_fact_append_must_not_livelock_the_park ... FAILED

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 10.00s
```

Failing test preserved at [`2026-07-02-audit-tests/zz_audit_v15.rs`](2026-07-02-audit-tests/zz_audit_v15.rs) — drop into `modules/causal/tests/` to reproduce (RED today; becomes the regression test for the fix).

Files: `modules/causal/src/reactor_runner.rs`, `modules/causal/src/memory_store.rs`, `modules/causal_replay/src/event_log.rs`, `modules/causal/src/projection_failure.rs`, `modules/causal_replay/src/reactor_checkpoint.rs`

---

### 24. sanitize_nul skips JSON object KEYS — a NUL in a payload map key fails the PG seal deterministically and the A4 infra-retry policy wedges the partition forever (A4a incomplete)

**Severity:** wedge_or_livelock · **Verdict:** confirmed with test · **Status at `dev@1721079`:** still present

**Defect.** The seal-time sanitizer strips NUL only from string VALUES (it recurses map.values_mut() and array items); Postgres jsonb rejects   anywhere including object keys, so a payload whose map key contains NUL fails PgDecisionStore.seal deterministically on every attempt, and seal errors are by A4 design infra-retried forever — the exact permanent wedge the sanitizer exists to prevent.

**Scenario.** A reactor emits an output whose payload contains a map keyed by scraped web content (the amendment's own threat model), e.g. {"a b": 1}. outputs_to_json sanitizes nothing (the NUL is in a key); PgDecisionStore.seal's INSERT fails 'unsupported Unicode escape sequence' every attempt; attempt_trigger propagates Err, process_trigger's infra arm retries forever — never classify()-parked — so the trigger occupies its partition permanently, the ack-floor freezes below it (compounding the GC-floor finding), and settle for the workflow hangs. InMemoryDecisionStore cannot catch this (serde_json accepts NUL in keys), so cargo test -p causal never exercises the failure.

**Evidence.** modules/causal/src/decision_store.rs:133-144: `serde_json::Value::Object(map) => map.values_mut().for_each(sanitize_nul)` — keys untouched, while the doc comment (:129-132) and module docs (:43-46) claim unsanitized deterministic seal failure is impossible. Seal errors routed to infinite infra-retry: modules/causal/src/reactor_runner.rs:1407-1412 and :1040-1048. Amendment A4a: docs/plans/2026-07-02-decision-records-design.md:245-249.

**Verifier corrections/refinements.** Two refinements to an otherwise accurate finding. (1) "settle for the workflow hangs" is overstated: the infra-retry arm calls note_worker_retry, and the settle wedge guard (SETTLE_WEDGE_FAILURES=10 in engine.rs) surfaces a loud settle error after ~10 consecutive retries (~16s) instead of hanging silently. The permanent effect is the partition wedge + frozen ack floor, plus a persistent settle error for affected workflows. (2) The hole is broader than payload map keys: sanitize_nul runs over the whole serialized envelope array, so metadata VALUES are sanitized but metadata KEYS have the same defect; nested keys at any depth in payload or metadata are affected. Exact locations: sanitize_nul at modules/causal/src/decision_store.rs:137-147 (fn body; the finder's :133-144 was close), the never-fulfilled no-deterministic-seal-failure promise in module docs :43-46 and fn docs :133-136, seal-error propagation at modules/causal/src/reactor_runner.rs (store.seal(rec).await? inside attempt_trigger, ~line 1413, comment block ~1407-1412) and the infinite infra-retry arm in process_trigger (~1040-1048, "None // infra: backoff + retry, never park"). Amendment A4a at docs/plans/2026-07-02-decision-records-design.md:238-249. Fix is one line: also sanitize keys in the Object arm (rebuild the map with NUL-stripped keys, minding potential key collisions after stripping), and extend conformance DS9 to cover keys.

**Proof (failing test output):**
```text
cargo test -p causal --test zz_audit_v16 -- --nocapture:

thread 'nul_in_object_key_must_also_be_stripped' panicked at modules/causal/tests/zz_audit_v16.rs:75:5:
DEFECT: NUL survives in an object KEY of the canonical durable JSON (wire text contains  ). Postgres jsonb rejects   in keys, so PgDecisionStore::seal fails deterministically and the reactor infra-retry arm loops forever. wire = "[{...\"payload\":{\"a\\u0000b\":1},...}]"
test control_nul_in_string_value_is_stripped ... ok
test in_memory_store_cannot_catch_the_key_case ... ok
test nul_in_object_key_must_also_be_stripped ... FAILED
test result: FAILED. 2 passed; 1 failed

Live Postgres 16 (docker, dedicated container), same parser the sqlx binary bind reaches via jsonb_recv:
SELECT '{"a b": 1}'::jsonb;
ERROR:  unsupported Unicode escape sequence
DETAIL:    cannot be converted to text.   <- KEY case: rejected
SELECT '{"a": "x y"}'::jsonb;  -> same error (the case the sanitizer handles)
SELECT '{"ab": 1}'::jsonb;          -> ok (sanitized control)

sqlx-postgres 0.8.6 types/json.rs:61-77: jsonb bind = version byte 1 + serde_json::to_writer text (NUL escaped as  ) — confirming the seal INSERT hits the same rejection.
```

Failing test preserved at [`2026-07-02-audit-tests/zz_audit_v16.rs`](2026-07-02-audit-tests/zz_audit_v16.rs) — drop into `modules/causal/tests/` to reproduce (RED today; becomes the regression test for the fix).

Files: `modules/causal/src/decision_store.rs`, `modules/causal/src/reactor_runner.rs`

---

### 25. A reactor body that never returns wedges settle() forever — invisible to the wedge guard, worker_stall, AND the D3 SettleTimeout liveness ceiling

**Severity:** wedge_or_livelock · **Verdict:** confirmed with test · **Status at `dev@1721079`:** still present

**Defect.** settle's three escape hatches (ConsumerHealth.wedged, worker_stall, SettleTimeout ceiling) all measure failures or supervisor heartbeats; a react() body that simply never completes produces neither — the attempt hasn't failed so note_worker_retry never fires, and the supervisor keeps stepping (reap+ingest unaffected) returning Idle which calls note_progress() every cycle, so idle_for() resets and the ceiling can never trip — so settle polls drained() forever even when the caller configured with_settle_liveness_ceiling expressly to bound settle.

**Scenario.** A reactor with no attempt_timeout (the default: attempt_timeout: None) awaits a hung HTTP/LLM request or deadlocked channel recv. The trigger stays in wf_pending (decremented only on completion reap), so drained(wf, hw) is false forever. worker_stall stays 0 (fires only on FAILED attempts); ConsumerHealth.wedged() stays None and idle_for() refreshes every supervisor poll. emit(...).settled() with with_settle_liveness_ceiling(Some(30s)) hangs indefinitely, silently — exactly the 'silent infinite hang' class D3 was built to convert into loud errors; the SettleTimeout field doc's promise is violated. Only per-reactor with_attempt_timeout covers this, and nothing warns the ceiling is blind to hung workers.

**Evidence.** modules/causal/src/engine.rs:2665-2727 — the polling loop's only exits are wedged() (consecutive step FAILURES), worker_stall >= threshold (:2688-2692), and idle_for() > ceiling (:2713-2726). modules/causal/src/reactor_runner.rs:998 — worker parks on the unresolved attempt; note_worker_retry reachable only after failure (:929-932); wf_pending incremented at enqueue :698, decremented only at completion reap :1898-1903; drained :755. engine.rs:3146-3148 — Idle/WaitOnDep call note_progress(), refreshing last_activity (idle_for engine.rs:188-190). Defaults: reactor_runner.rs:435 attempt_timeout: None; settle_liveness_ceiling default None.

**Verifier corrections/refinements.** Mechanism and severity are correct as claimed; three citation details need fixing. (1) Line numbers drifted: settle's polling loop and its three escape hatches are at engine.rs:2708-2786 (wedged/worker_stall at :2746-2762, ceiling at :2771-2783), not 2665-2727/2688-2692; the Idle/WaitOnDep note_progress is at engine.rs:3205-3212, not 3146-3148 (3146-3148 is inside the skip_gap_on_start diagnostics helper). (2) The hung await site is reactor_runner.rs:1271 (`None => unwrap_panic(react_fut.await)` — the attempt_timeout:None arm), reached from the worker's process_trigger at :998; the finder's phrase 'worker parks on the unresolved attempt' is loose — 'park' is this codebase's term for terminal-failure parking, which never happens here; the worker task simply awaits forever and never sends a Completion. (3) with_settle_liveness_ceiling is a method on Engine (engine.rs:1989, applied post-build), not EngineBuilder. Additional supporting fact: docs/plans/2026-07-02-v0.19-implementation-plan.md Phase 3 explicitly documents this exact hole ('a hung effect hangs even *with* the ceiling — supervisor heartbeats Idle') and prescribes ceiling default ~30s + per-attempt react timeout as the fix; the working tree ships both defaults as None, and the crate's own D3 test only exercises a hung projector (inline in step, engine.rs:3906-3933), never a hung reactor worker — so the acceptance test named in the plan ('never-returning react is interrupted at the attempt timeout') does not exist.

**Proof (failing test output):**
```text
running 1 test
thread 'hung_react_body_is_surfaced_by_settle_liveness_ceiling' (3002462) panicked at modules/causal/tests/zz_audit_v18.rs:77:13:
DEFECT CONFIRMED: settle() hung past 5s despite with_settle_liveness_ceiling(300ms) — a never-returning react() body is invisible to the wedge guard, worker_stall, and the D3 liveness ceiling
test hung_react_body_is_surfaced_by_settle_liveness_ceiling ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 5.01s
```

Failing test preserved at [`2026-07-02-audit-tests/zz_audit_v18.rs`](2026-07-02-audit-tests/zz_audit_v18.rs) — drop into `modules/causal/tests/` to reproduce (RED today; becomes the regression test for the fix).

Files: `modules/causal/src/engine.rs`, `modules/causal/src/reactor_runner.rs`

---

### 26. A projector DEPENDS_ON naming an unregistered/renamed consumer wedges settle() forever — WaitOnDep is counted as progress by every guard

**Severity:** wedge_or_livelock · **Verdict:** confirmed with test · **Status at `dev@1721079`:** still present

**Defect.** ProjectionRunner/MultiProjectorRunner return WaitOnDep when a dependency's checkpoint is behind, and supervise_one maps WaitOnDep to health.note_progress(); a dep id no checkpoint row will ever advance (typo, consumer rename — the D4 trap — or a dep in another process with a separate checkpoint store) freezes the projector's cursor forever after its first batch, and settle() waits on that durable cursor with no guard able to fire; DEPENDS_ON strings are never validated against registered consumer ids (D4 boot-time orphan detection is specced, not implemented).

**Scenario.** M::DEPENDS_ON = ["old_consumer_name"] after a rename. Fresh start: cursor=ZERO, dep=ZERO, fence passes, projector advances its own checkpoint to N. Every later step: dep_cursor = get("old_consumer_name") = None → ZERO < N → WaitOnDep forever. supervise_one calls note_progress() (consecutive_failures stays 0, idle_for resets), projectors have no worker_stall. Any emit(...).settled() whose hw exceeds N polls drained() (durable cursor >= hw) false forever — silent permanent hang, with or without the liveness ceiling. Nothing distinguishes 'dep momentarily behind' from 'dep will never exist'.

**Evidence.** modules/causal/src/projection_runner.rs:200-210 (dep fence, unwrap_or(ZERO)); modules/causal/src/multi_projector.rs:229-235 (same); modules/causal/src/engine.rs:3145-3151 (WaitOnDep => note_progress); engine.rs:78-80, :105-107 (drained = durable cursor >= hw); engine.rs:2665-2727 (settle escapes defeated by note_progress); no DEPENDS_ON validation in engine.rs (grep).

**Verifier corrections/refinements.** One factual error in the finding: 'D4 boot-time orphan detection is specced, not implemented' is wrong — D4 IS implemented (engine.rs:1725-1767, plus EngineBuilder::with_strict_orphan_detection at :1120). The correct statement is that D4 as implemented (and as specced) cannot catch this defect: it scans persisted checkpoint/decision/effect rows whose consumer id is unregistered, but never validates DEPENDS_ON strings. A phantom dep (typo, never-deployed consumer, dep checkpointing in another process's store) produces no row, so nothing is flagged in any mode. Only the rename variant leaves a stale row D4 can see — and by default that is a tracing::warn (invisible without a subscriber); the wedge still occurs unless strict mode is opted in AND the stale row still exists. Also a precision note on the ceiling: with_settle_liveness_ceiling is a method on Engine (engine.rs:1989), not EngineBuilder, and it is defeated because supervise_one's note_progress() on WaitOnDep stamps last_activity every POLL_INTERVAL, keeping idle_for() below any sane ceiling — proven empirically (2s ceiling, 20s hang, no error). Exact line refs in the working tree: projection_runner.rs:200-210 (dep fence, unwrap_or(ZERO)); multi_projector.rs:226-236 (same); engine.rs:3206-3207 (WaitOnDep => note_progress); engine.rs:66 (worker_stall default None for projectors); engine.rs:78-80/105-107 (drained = cursor >= hw); engine.rs:2746-2784 (all three settle escapes).

**Proof (failing test output):**
```text
thread 'missing_dep_wedges_settle_silently_despite_all_guards' (3002756) panicked at modules/causal/tests/zz_audit_v19.rs:96:13:
DEFECT CONFIRMED: settle hung >20s on a projector wedged by DEPENDS_ON naming a nonexistent consumer. WaitOnDep resets the wedge failure counter and the liveness heartbeat every poll, worker_stall is None for projectors — no guard fires.
test missing_dep_wedges_settle_silently_despite_all_guards ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 20.06s

(Note: the FIRST settle in the same test succeeded near-instantly — the fence passes while the projector's own cursor is ZERO — so the hang is specifically the post-first-batch WaitOnDep wedge, with the 2s liveness ceiling configured and never firing.)
```

Failing test preserved at [`2026-07-02-audit-tests/zz_audit_v19.rs`](2026-07-02-audit-tests/zz_audit_v19.rs) — drop into `modules/causal/tests/` to reproduce (RED today; becomes the regression test for the fix).

Files: `modules/causal/src/projection_runner.rs`, `modules/causal/src/multi_projector.rs`, `modules/causal/src/engine.rs`

---

### 27. apply_event's fold-error wrapper flattens the error chain, defeating classify_structural — deterministic fold failures park 'unclassified' in life 1, then wedge the consumer at every boot because ensure_hydrated's poison-skip can never fire

**Severity:** wedge_or_livelock · **Verdict:** confirmed with test · **Status at `dev@1721079`:** still present

**Defect.** apply_event wraps fold errors with anyhow::anyhow!("fold failed …: {e:#}"), rendering the cause into a string and dropping it from the error chain; classify_structural looks for a serde_json::Error IN the chain, so no fold error is ever classified Poison — and ensure_hydrated's 'skip a previously-parked poison during hydration' branch is unreachable for exactly the errors it exists to skip.

**Scenario.** A deterministic non-serde fold error (stream-alignment bail, or 'gap repair did not converge' on a truncated stream). Life 1: step hits it, classify_structural returns None, FailureState retries 3x then parks 'unclassified' and advances the cursor. Life 2 (restart): cursor > 0, ensure_hydrated replays log[0..cursor], re-hits the identical error at a position <= cursor; classification != Poison so it returns Err — the OnceCell stays uninitialized and every subsequent step() re-runs hydration and fails: the consumer is wedged at boot forever, the exact every-boot wedge the skip branch's comment claims was fixed. Secondary: genuinely-poison fold payloads park labeled 'unclassified' after 3 wasted retries, corrupting the failure-class wire contract mass-replay tooling keys on. (The fold-level serde case itself is unreachable because json_extract_id swallows deserialize failures — see the silent-skip finding.)

**Evidence.** modules/causal/src/aggregator.rs:530-535 (map_err rebuilds the error from a format string — chain broken; the intact-chain path at projection_runner.rs:275-283 is the only case tests cover); alignment bail :456-464; convergence bail :985-991. modules/causal/src/failure.rs:158-163 (classify_structural requires typed serde_json::Error in e.chain()). modules/causal/src/projection_runner.rs:328-356, :375-431 and modules/causal/src/multi_projector.rs:370-422 (hydration skip gated on Poison; non-Poison propagates, OnceCell::get_or_try_init retries forever); modules/causal/src/projection_failure.rs:122-131.

**Verifier corrections/refinements.** Primary mechanism correction: the chain-flattening wrapper (aggregator.rs ~543-547, working-tree lines; finder cited 530-535) is real but latent — errors reaching it that would contain a serde_json::Error are unreachable because json_extract_id swallows deserialize failures before apply_to runs. The reachable triggers are the two raw unclassified bails: the stream-alignment bail (aggregator.rs ~468-476) and the gap-repair convergence bail (aggregator.rs ~997-1002). The load-bearing defect is the class asymmetry between the live park policy (FailureState passes the cursor over deterministic failures of ANY class — unclassified/domain after max_attempts, transient after the 6h ceiling) and the hydration skip (projection_runner.rs:413, multi_projector.rs:404), which forgives only Poison — a class no fold-path error can ever carry. Consequence proven in-memory: an event parked `unclassified` in life 1 wedges ensure_hydrated on every subsequent boot (OnceCell::get_or_try_init never initializes; every step() fails). Nuance: the settle wedge-guard surfaces the wedge as an error ("consumer X is wedged — N consecutive failures") within seconds instead of a silent hang, but the consumer never progresses again. Same defect exists verbatim in MultiProjectorRunner::ensure_hydrated.

**Proof (failing test output):**
```text
AUDIT v20: life-1 park recorded with class = "unclassified"

thread 'parked_unclassified_fold_error_must_not_wedge_hydration_after_restart' (3014776) panicked at modules/causal/tests/zz_audit_v20.rs:170:9:
life 2: settled() errored — consumer wedged at boot, re-hitting the parked (class=unclassified) deterministic fold error during hydration: settle: consumer `v20.ok.projector` is wedged — 10 consecutive failures with no progress, so workflow 4799c953-f1fe-4f57-a96b-e61dc2b2cf00 can never drain to high-water LogCursor(4). Last error: aggregator for `V20State` declares subject `v20_other_stream` (restorable) but folded event `v20_fact` from stream `v20_fact-89b3537c-c09d-4495-ab56-f324b213bc3f` with extracted id 89b3537c-c09d-4495-ab56-f324b213bc3f — a restorable aggregate must fold exactly its own stream
test parked_unclassified_fold_error_must_not_wedge_hydration_after_restart ... FAILED

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.39s
```

Failing test preserved at [`2026-07-02-audit-tests/zz_audit_v20.rs`](2026-07-02-audit-tests/zz_audit_v20.rs) — drop into `modules/causal/tests/` to reproduce (RED today; becomes the regression test for the fix).

Files: `modules/causal/src/aggregator.rs`, `modules/causal/src/failure.rs`, `modules/causal/src/projection_runner.rs`, `modules/causal/src/multi_projector.rs`

---

### 28. PgEffectStore retains both A4 hazards the decision store fixed: zero-row CTE race (fetch_one → spurious RowNotFound) and no NUL sanitization (deterministic JSONB failure → park of legitimate work)

**Severity:** loss_or_duplication · **Verdict:** confirmed by trace · **Status at `dev@1721079`:** still present

**Defect.** A4b's zero-row race was fixed in PgDecisionStore::seal (bounded SELECT retry) but PgEffectStore::put still uses fetch_one on the identical CTE the design doc itself says 'can return zero rows under racing first-writes'; and A4a's NUL sanitization exists only in DecisionRecord::outputs_to_json, so an effect value containing   (scraped content — effects memoize raw fetch results) fails deterministically on Postgres JSONB while succeeding on InMemoryEffectStore.

**Scenario.** (a) Race: blue/green overlap — two executions of the same trigger both call ctx.effect("fetch", ...); the loser's INSERT hits ON CONFLICT DO NOTHING but the outer SELECT runs under the pre-commit statement snapshot → zero rows → fetch_one errors RowNotFound → the reaction attempt fails and burns classify retry budget, potentially parking succeeded work. (b) Wedge: a reactor memoizes a page containing   via ctx.effect → PgEffectStore::put raises 'unsupported Unicode escape sequence' on every retry (the external call re-executes each time since the cache write never lands) → deterministic failure → terminal park of legitimate work, decision never seals. Both invisible under cargo test -p causal; no conformance scenario covers races or NUL (ES1-ES10 are sequential and NUL-free).

**Evidence.** modules/causal_replay/src/effect_store.rs:69-93: CTE + `.fetch_one(&self.pool)` with doc 'Exactly one row is always returned' (:72), contradicted by docs/plans/2026-07-02-decision-records-design.md:246-249 (A4b) and by the fix applied only to PgDecisionStore (modules/causal_replay/src/decision_store.rs:119-141). sanitize_nul exists only in modules/causal/src/decision_store.rs:133-144, invoked only from outputs_to_json (:161-167); no sanitization in either effect store (InMemoryEffectStore stores raw Values: modules/causal/src/effect_store.rs:135, :164-173).

**Verifier corrections/refinements.** Both sub-claims verified, with two corrections. (1) Severity of the zero-row race (claim a) is overstated: it is one-shot per key — the loser's next attempt calls get() first and finds the committed winner's row, so it costs one failed attempt + backoff, and parks succeeded work only if retry max_attempts == 1. The doc comment at modules/causal_replay/src/effect_store.rs:72 ("Exactly one row is always returned") is empirically false. (2) The NUL failure (claim b) is not a wedge/livelock: classify() returns None for raw sqlx errors, so reactor_runner.rs:1067-1068 parks the trigger as Unclassified after max_attempts — a bounded terminal park, not an infinite loop, and settle does not wedge. The actual damage: the trigger's legitimate outputs never enter the log (work loss, and replay-from-park re-fails deterministically since the payload still contains NUL), and because the cache write never lands, the external call re-executes on every retry attempt (duplicated side effects for write-style effects — the very thing the effect store exists to prevent). Minor citation drift: InMemoryEffectStore::put is at modules/causal/src/effect_store.rs:170-178, not :135/:164-173. Fix is mechanical and prescribed by the author's own A4 amendment: port the bounded SELECT retry from PgDecisionStore::seal (modules/causal_replay/src/decision_store.rs:119-141) into PgEffectStore::put, and apply (a pub version of) sanitize_nul to the value in remember()/put before binding to JSONB — plus add racing and NUL scenarios to the ES conformance suite (ES1-ES10 in modules/causal_replay/src/conformance.rs are sequential and NUL-free, confirmed). Note for tooling: modules/causal/src/decision_store.rs contains literal NUL bytes in doc comments, so plain grep treats it as binary — an incidental hazard for future audits.

**Proof (failing test output):**
```text
Experiment 1 (NUL → deterministic put failure on PG, succeeds on InMemoryEffectStore):
  ERROR:  unsupported Unicode escape sequence
  DETAIL:    cannot be converted to text.
  CONTEXT:  JSON data, line 1: {"text":"a ...

Experiment 2 (racing first-writes → loser's CTE returns ZERO rows; fetch_one => sqlx RowNotFound):
  Session B (loser, blocked ~3s on A's in-flight insert, ran under pre-commit snapshot):
     value
    -------
    (0 rows)
    Time: 2991.812 ms (00:02.992)
  Session B follow-up plain SELECT (what a retry — the missing A4b fix — would see):
     visible_after
    ---------------
     "A"
    (1 row)
  Session A (winner): RETURNING gave "A"; COMMIT ok.
```

Files: `modules/causal_replay/src/effect_store.rs`, `modules/causal/src/effect_store.rs`, `modules/causal/src/decision_store.rs`

---

### 29. OCC fence is keyed by event NAME while placement is by SUBJECT — un-fenced Any-appends land in an INVARIANT aggregate's stream and starve the command path via spurious CAS conflicts

**Severity:** wedge_or_livelock · **Verdict:** confirmed with test · **Status at `dev@1721079`:** still present

**Defect.** The optimistic-concurrency fence checks fact.name()/out.durable_name (engine.rs:2420, reactor_runner.rs:1335) but stream placement uses fact.subject()/out.subject, so any event type declaring subject = <the invariant aggregate's stream category> passes both guards and Any-appends mid-history into the OCC-protected stream; build() never validates SUBJECT collisions against OCC placement categories. State is not corrupted (the fold is type-filtered) and the invariant is not torn, but every co-located Any-append bumps the stream head, so under sustained foreign traffic Engine::append loses its CAS repeatedly and exhausts MAX_OCC_RETRIES=16, returning ConflictError with zero genuine contention — violating the documented C4/C11 'reactors cannot emit into Aggregate streams' claim at the stream level.

**Scenario.** Invariant aggregate over F (NAME/SUBJECT "order_placed"); a reactor emits G declared #[event(name="audit_note", subject="order_placed", subject_id="order_id")]. Both fences pass (name not in occ set), placement lands G in "order_placed-{id}" with StreamState::Any; one audit note per order event permanently starves the opted-in OCC writer. Reverse direction (fenced NAME, unfenced SUBJECT) has the same hole; sealed pre-registration decision records also replay through append_outputs with no occ re-check.

**Evidence.** engine.rs:2420 (fence by name) vs engine.rs:2432/2446 (placement by subject), Any append at 2464-2466; reactor_runner.rs:1334-1336 vs 1519/1557-1564; occ_categories populated only with event_prefix at engine.rs:1377; event_subject present on Aggregator (aggregator.rs:100-106) but never inserted into the fence or validated at build (engine.rs:1583-1830 validates only colon-format names).

**Verifier corrections/refinements.** Mechanism, line cites, and files are accurate as claimed (engine.rs:2420 vs 2432/2446/2464-2490; reactor_runner.rs:1334-1336 vs 1519/1553-1564; engine.rs:1377; aggregator.rs:100-106; build() validation at engine.rs:1633-1642 checks only colon-format names). Two overclaims corrected: (1) "one audit note per order event permanently starves the opted-in OCC writer" is wrong for that traffic pattern — reactor-emitted notes trail order appends, so each OCC attempt at worst loses once and succeeds on retry; genuine retry exhaustion requires sustained foreign traffic landing inside each of the 16 read→decide→CAS windows (demonstrated with a tight-loop emitter and a 2ms decide body). The realistic default harm is spurious CAS conflicts consuming the bounded retry budget, escalating to ConflictError exhaustion only under sustained per-subject foreign write pressure. (2) The "reverse direction" is not symmetric as stated: a fenced NAME with a foreign SUBJECT is still rejected by the name check; the actual reverse case is an invariant event whose SUBJECT differs from its NAME — foreign kinds co-locating into that SUBJECT stream hit the same hole. The sealed-replay bypass is real but narrower than implied: replay_decision (reactor_runner.rs:1147-1158) skips the fence at 1333, which only matters when occ_categories changed between seal-time and replay-time (e.g. a deploy promotes an aggregate to INVARIANT). Fix surface: add event_subject (aggregator.rs:106) to the fence set keyed by placement category, and/or validate SUBJECT collisions against invariant placement categories in build().

**Proof (failing test output):**
```text
thread 'subject_colliding_emit_is_fenced_out_of_invariant_stream' panicked at modules/causal/tests/zz_audit_v102.rs:127:5:
DEFECT: un-fenced Any-append landed in the INVARIANT aggregate's stream order_placed-c953751e-6c96-4e46-88e6-85cd89882209 via emit (fence keys on NAME, placement on SUBJECT); stream now holds 1 events
test subject_colliding_emit_is_fenced_out_of_invariant_stream ... FAILED

thread 'lone_occ_writer_survives_colocated_any_append_traffic' panicked at modules/causal/tests/zz_audit_v102.rs:201:5:
DEFECT: the ONLY writer of 'order_placed' facts exhausted its OCC retry budget (16 decide attempts) purely on co-located Any-append traffic (9724 foreign audit_note emits landed in the invariant stream) — spurious ConflictError with zero genuine contention: Some(aggregate stream state mismatch: expected StreamRevision(9103), current Some(StreamRevision(9363)))
test lone_occ_writer_survives_colocated_any_append_traffic ... FAILED

test result: FAILED. 0 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.13s
```

Failing test preserved at [`2026-07-02-audit-tests/zz_audit_v102.rs`](2026-07-02-audit-tests/zz_audit_v102.rs) — drop into `modules/causal/tests/` to reproduce (RED today; becomes the regression test for the fix).

Files: `modules/causal/src/engine.rs`, `modules/causal/src/reactor_runner.rs`, `modules/causal/src/aggregator.rs`

---

### 30. #[event(workflow_id)] with a nil field value silently roots the shared NIL workflow — unrelated runs merge, settled() couples across them, cancel fences them collectively

**Severity:** wedge_or_livelock · **Verdict:** confirmed with test · **Status at `dev@1721079`:** still present

**Defect.** The macro generates declared_workflow_id() -> Some(self.field) with no nil check (lib.rs:749-754) and the engine's workflow resolution accepts Some(Uuid::nil()) as a genuine root with no validation anywhere (no is_nil check in engine.rs/reactor_runner.rs/aggregator.rs), so every fact whose root field is nil — trivially produced via #[derive(Default)]/..Default::default() since Uuid::default() IS nil — joins one shared workflow that also carries the control stream's cancel markers (engine.rs:532-541 stamps cancel markers workflow nil).

**Scenario.** A call site builds RunStarted { ..Default::default() } forgetting run_id. Two independent runs share workflow nil: run A's emit().settled() waits on run B's cascading chain and never resolves while nil traffic continues; cancel_workflow(nil) permanently fences every nil-rooted chain across all runs; per_workflow reactors collapse all nil triggers into one serial partition.

**Evidence.** lib.rs:741-754 (comment warns about new_v4 but nothing checks nil); engine.rs:2355-2378 (Some(nil) accepted as root), 2092-2098 (fence insert), 532-541 (control markers on nil), 2738/2824 (settle high-water); reactor_runner.rs:1623/1853 (outputs inherit workflow).

**Verifier corrections/refinements.** The finding is accurate; minor sharpenings only. (1) The cancel-coupling facet is arguably worse than 'fencing': under the merge, cancel_workflow(nil) causes unrelated runs' triggers to be acked WITHOUT processing (reactor_runner.rs ~881-890), i.e., their reactions are permanently and silently dropped even though the trigger facts are durably in the log — a lost-reaction outcome, not just a wedge. (2) The settle coupling violates an explicit documented promise at engine.rs:2693 ('Other runs' concurrent traffic does not delay it') and the engine's own test invariant at engine.rs:6396 ('distinct emits get distinct workflow_ids'). (3) Reachability is even broader than #[derive(Default)]: serde deserialization of a payload missing the root field (with a defaulted field) also yields nil. (4) The fence insert is at engine.rs:2098 (within cancel_workflow at 2092-2099); the finder's 2092-2098 range is essentially right. All other cited lines (lib.rs:749-754, engine.rs:2355-2378, 532-541, 2738/2824, reactor_runner.rs:1623/1853, 1806) verified correct.

**Proof (failing test output):**
```text
running 4 tests

thread 'independent_nil_rooted_runs_must_not_merge_into_one_workflow' panicked at modules/causal/tests/zz_audit_v109.rs:74:13:
assertion `left != right` failed: a run must not root the engine's reserved NIL workflow (the same workflow_id append_workflow_cancelled stamps on control markers)
  left: 00000000-0000-0000-0000-000000000000
 right: 00000000-0000-0000-0000-000000000000
test independent_nil_rooted_runs_must_not_merge_into_one_workflow ... FAILED
test control_distinct_run_ids_do_not_couple ... ok

thread 'cancelling_one_nil_rooted_run_must_not_fence_unrelated_runs' panicked at modules/causal/tests/zz_audit_v109.rs:251:9:
run A's reactor never fired: cancel_workflow(run B's workflow) collaterally fenced unrelated run A because both nil-rooted runs share the NIL workflow (saw reactions: ["run-b"])
test cancelling_one_nil_rooted_run_must_not_fence_unrelated_runs ... FAILED

thread 'settled_must_not_couple_across_unrelated_nil_rooted_runs' panicked at modules/causal/tests/zz_audit_v109.rs:145:9:
settled() for run A hung for 5s: it is coupled to UNRELATED run B's blocked reactor because both nil-rooted runs merged into the single shared NIL workflow
test settled_must_not_couple_across_unrelated_nil_rooted_runs ... FAILED

test result: FAILED. 1 passed; 3 failed; 0 ignored; 0 measured; 0 filtered out; finished in 5.00s
```

Failing test preserved at [`2026-07-02-audit-tests/zz_audit_v109.rs`](2026-07-02-audit-tests/zz_audit_v109.rs) — drop into `modules/causal/tests/` to reproduce (RED today; becomes the regression test for the fix).

Files: `modules/causal_core_macros/src/lib.rs`, `modules/causal/src/engine.rs`, `modules/causal/src/reactor_runner.rs`

---

## F. settle() correctness

Both failure directions of the quiescence promise: returning too early, and failing/hanging when the chain is fine.

### 31. settle() ignores its emit-position floor when a stale tracker entry exists — settled() returns before the just-emitted trigger is even observed

**Severity:** incorrect_result · **Verdict:** confirmed with test · **Status at `dev@1721079`:** still present

**Defect.** Engine::settle computes its high-water as `workflow_hw.get(&wf).unwrap_or(result.position)` — the emit position is used only when the tracker entry is ABSENT, never as a floor via max() — so a pre-existing tracker entry BELOW the new emit's position (left by an earlier fire-and-forget emit into the same workflow; entries are removed only by settle's own forget or cap eviction) makes settle wait only to the stale position and return while the just-emitted trigger is unprocessed, contradicting the code's own doc comment two lines above.

**Scenario.** (1) emit F1 with explicit workflow_id W without .settled(); reactor processes it, bumps tracker[W]=p1; entry persists for the process lifetime. (2) Later emit F2 into W at p2 >> p1 and await .settled(). (3) settle reads hw = p1, not max(p1, p2); every consumer's ingest_pos >= p1 and wf_pending has no W entry yet (settle's first drained() pass beats the supervisor's next scan), so drained(W, p1) is true and the hw re-check returns p1 unchanged → settle returns Ok while F2 has not been scanned. Caller reads projections that don't reflect F2's chain. Also reachable via settle_tree with a child workflow's stale tracker entry.

**Evidence.** modules/causal/src/engine.rs:2654-2659 — `get(&wf).unwrap_or(result.position)` with no max(), despite the adjacent doc ('Floor it at the emit position...') and the settle docstring ('floored at result.position'). Entries created only by reactor bumps (modules/causal/src/reactor_runner.rs:1622, :1852), removed only by settle's forget (engine.rs:2734) or cap eviction (engine.rs:427-437); execute_emit (engine.rs:2244-2477) never touches workflow_hw. drained race: reactor_runner.rs:745-755 vs wf_pending population at :689-699. No test covers fire-and-forget-then-settle on a reused workflow_id.

**Verifier corrections/refinements.** The finding is fully correct in mechanism; only line numbers for the settle internals are shifted (likely from a slightly different tree state). Actual working-tree locations: the un-floored hw computation is engine.rs:2713-2718 (inside settle, which starts at :2698), not :2654-2659; the contradicting doc comment is :2709-2712 and the docstring floor language :2666-2667; settle's forget is :2793 (not :2734); cap eviction is :428-438 (not :427-437); execute_emit spans :2303-2536 (not :2244-2477). The reactor_runner.rs citations (bump at :1622 and :1852, drained at :745-755, wf_pending population at :698) are exact. One nuance the finder slightly understated: the bug does not even require the drained-vs-supervisor race to be won on a knife's edge — the supervisor idles on a 50ms poll (POLL_INTERVAL, engine.rs:38) while settle probes immediately, so the early return is the overwhelmingly common outcome (reproduced 3/3 in ~0.6s). Fix is one line: `let hw = self.workflow_hw.lock().unwrap().get(&wf).map_or(result.position, |t| t.max(result.position));`.

**Proof (failing test output):**
```text
thread 'settled_waits_for_trigger_despite_stale_tracker_entry' (3010557) panicked at modules/causal/tests/zz_audit_v22.rs:124:5:
assertion `left == right` failed: settled() returned before the just-emitted trigger was processed (reactor ran 1 time(s); expected 2) — the stale workflow_hw entry masked the emit-position floor
  left: 1
 right: 2
test settled_waits_for_trigger_despite_stale_tracker_entry ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.62s
[deterministic: reproduced identically on 3 consecutive runs, ~0.6s each — settled() returns near-instantly instead of waiting for the trigger's chain]
```

Failing test preserved at [`2026-07-02-audit-tests/zz_audit_v22.rs`](2026-07-02-audit-tests/zz_audit_v22.rs) — drop into `modules/causal/tests/` to reproduce (RED today; becomes the regression test for the fix).

Files: `modules/causal/src/engine.rs`, `modules/causal/src/reactor_runner.rs`

---

### 32. Empty-batch emit().settled() no longer honors its documented barrier semantics — it waits for scan position only, not pending work of other workflows

**Severity:** minor · **Verdict:** confirmed with test · **Status at `dev@1721079`:** still present

**Defect.** The empty-batch emit path documents that returning latest_position makes a downstream settled() 'wait for any pre-existing pending work to drain', but since the 0.10 workflow-scoped drained() probe, a reactor with in-flight triggers of OTHER workflows reports drained=true the moment its ingest cursor reaches the tip — the fresh random workflow_id matches no pending work by construction, so the barrier returns while reactions are mid-flight and their future outputs are never waited for.

**Scenario.** (1) emit(F1) fire-and-forget in workflow X at pos 10; reactor ingests it (ingest_pos=10, wf_pending[X]=1) and starts a slow react(). (2) Caller uses the flush idiom engine.emit(Vec::new()).settled().await — EmitResult{position: 10, workflow_id: fresh Uuid W}. (3) settle(W): drained(W, 10) = ingest_pos >= 10 && !wf_pending.contains_key(&W) — vacuously true; settled() returns immediately while X's reaction executes and its output lands at pos 11+. Test harnesses using empty-emit as a quiescence barrier get flaky early returns; the doc's promise held under the old global-cursor probe and is silently false now.

**Evidence.** modules/causal/src/engine.rs:2244-2252 (empty-batch comment and `workflow_id: b.workflow_id.unwrap_or_else(Uuid::new_v4)`), :354-360 (EmitResult doc makes the same claim); modules/causal/src/reactor_runner.rs:745-755 (drained checks wf_pending only for the probed workflow); the settle docs note the 0.10 isolation change but the empty-batch emit doc was never reconciled.

**Verifier corrections/refinements.** Mechanism and files are correct; only line numbers drifted slightly in the current working tree: the empty-batch early-return is engine.rs:2303-2312 (comment + `Uuid::new_v4` at :2310), not :2244-2252; the EmitResult doc claim is engine.rs:365-371, not :354-360; the drained probe is reactor_runner.rs:745-756 (finder said 745-755 — essentially exact). One additional supporting fact the finder implied but did not cite: ingest_pos is advanced at enqueue time (reactor_runner.rs:699), before the reaction runs, which is what makes `ingest_pos >= hw` true while work is mid-flight. The existing in-module regression test at engine.rs:6101 masks the bug because it uses a projector (synchronous apply-while-scan), never a reactor with an in-flight reaction.

**Proof (failing test output):**
```text
thread 'empty_emit_settle_waits_for_in_flight_reaction_of_other_workflow' (3006746) panicked at modules/causal/tests/zz_audit_v24.rs:113:5:
DEFECT: settle(empty-emit result) returned while a pre-existing reaction of another workflow was still mid-flight — the documented flush barrier does not hold under the workflow-scoped drained() probe
test empty_emit_settle_waits_for_in_flight_reaction_of_other_workflow ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
```

Failing test preserved at [`2026-07-02-audit-tests/zz_audit_v24.rs`](2026-07-02-audit-tests/zz_audit_v24.rs) — drop into `modules/causal/tests/` to reproduce (RED today; becomes the regression test for the fix).

Files: `modules/causal/src/engine.rs`, `modules/causal/src/reactor_runner.rs`

---

### 33. settle()'s drained() probe does durable I/O and propagates a single transient store blip as an untyped hard error, contradicting the runner's own infra-retry policy

**Severity:** minor · **Verdict:** confirmed with test · **Status at `dev@1721079`:** still present

**Defect.** Engine::settle's polling loop does `if consumer.drained(wf, hw).await? {` with no retry — and ReactorRunner::drained is not a pure in-memory read: it calls reap() → advance_floor() → checkpoint.advance() (durable I/O), and the not-yet-started path calls checkpoint.get() — so one transient Postgres error aborts settle() with an untyped anyhow error while the chain is draining normally, whereas the same blip inside the runner is deliberately infra-retried forever (A4).

**Scenario.** emit(fact).settled() mid-drain; a pool hiccup makes one checkpoint.advance inside the settle-driven reap return Err; settle returns Err immediately. The caller cannot distinguish 'chain failed' from 'probe blipped, work continuing': re-emitting mints fresh event_ids (per the emit docs) and double-emits; reporting failure lies while the workflow actually completes. Unlike the typed wedge/SettleTimeout exits, this exit is accidental.

**Evidence.** modules/causal/src/engine.rs:2666 — `if consumer.drained(wf, hw).await? {` with no retry/backoff, in contrast to the wedge gate (:2686-2703) and typed SettleTimeout (:2712-2724). Durable work in the probe: modules/causal/src/reactor_runner.rs:745-756 (checkpoint.get on !started; reap otherwise), reap persists via advance_floor :1906, :1917-1948. Policy contrast: reactor_runner.rs:1407-1411 and A4 in docs/plans/2026-07-02-decision-records-design.md.

**Verifier corrections/refinements.** The propagation site is engine.rs:2725 (`if consumer.drained(wf, hw).await? {`), not :2666 (which is doc text in the current tree); the wedge gate is at :2746-2762 and the typed SettleTimeout at :2771-2784. Additionally, the exposure is broader than the finder stated: ProjectionRunner::drained (engine.rs:78-80) and MultiProjectorRunner::drained (:105-107) also do durable checkpoint reads behind the same un-retried `?`, so a blip on a projector cursor read aborts settle identically. One mechanism refinement: because advance_floor re-attempts a failed persist on every subsequent call (persisted_floor stays stale, reactor_runner.rs:1941-1948), a blip first absorbed by the supervisor's step is deterministically re-encountered by settle's next probe — the settle abort does not require the blip to land on the settle task's own I/O call.

**Proof (failing test output):**
```text
thread 'settle_survives_transient_checkpoint_blip' (3021258) panicked at modules/causal/tests/zz_audit_v23.rs:160:5:
DEFECT CONFIRMED: settle() aborted on a transient checkpoint blip while the chain drained normally. Propagated error: injected transient checkpoint blip (pool hiccup)
test settle_survives_transient_checkpoint_blip ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.11s

[Notes: the panic is the FINAL assertion — the earlier `second.is_ok()` assertion passed, proving the same workflow settled Ok once the blip cleared, i.e. the chain was draining normally when settle() reported the raw injected error. The error text is the bare injected store error — not the wedge message, not the typed SettleTimeout. Run in an isolated scratchpad clone of branch HEAD (86e036b) because the live working tree was concurrently mutated by another agent (decision_store.rs mid-edit, lib uncompilable); engine.rs/reactor_runner.rs/checkpoint_store.rs were byte-identical to HEAD at test time. Test file deleted from the repo afterward; repo left clean.]
```

Failing test preserved at [`2026-07-02-audit-tests/zz_audit_v23.rs`](2026-07-02-audit-tests/zz_audit_v23.rs) — drop into `modules/causal/tests/` to reproduce (RED today; becomes the regression test for the fix).

Files: `modules/causal/src/engine.rs`, `modules/causal/src/reactor_runner.rs`

---

## G. Macro contracts, observability, CI

Silent-wrongness traps in the declared-loud macro surface, plus the coverage gap that lets all of the above merge green.

### 34. Macro attribute parsing silently drops natural spellings and typo'd keys in BOTH #[reactor] and #[event] — execution semantics and workflow-root declarations vanish with zero diagnostics

**Severity:** nondeterminism · **Verdict:** confirmed with test · **Status at `dev@1721079`:** still present

**Defect.** Two instances of one parser defect class in causal_core_macros. (1) #[reactor]: parse_consumer_fn's arms for ordering/max_in_flight/backoff_multiplier/max_attempts guard only on the key name, so the arm consumes the meta (the unknown-arg error at lib.rs:1062-1067 can never fire) but accepts only one exact value shape — ordering = "per_workflow" (quoted, matching every neighboring string attr) or ordering = Ordering::PerWorkflow (multi-segment path) both fall through silently (lib.rs:974-995), leaving the trait default PerSubject: same-workflow triggers run concurrently, the exact interleaving the declaration existed to serialize; backoff_multiplier = 2 (Lit::Int where Lit::Float is required, lib.rs:1044-1051) is likewise dropped, possibly eliding retry_policy() entirely. (2) #[event]: parse_event_args swallows unknown keys (`_ => {}` at lib.rs:538) and non-string values (lib.rs:489-537), and unlike name/subject_id there is no downstream required-ness backstop for workflow_id/subject/occurred_at_field — so `workflow_id = run_id` (unquoted, consistent with #[reactor]'s own grammar) or `workflow = "run_id"` compiles clean as a chain-member fact, and every top-level emit mints a fresh v4 workflow (engine.rs:2377): at-least-once redelivery mints phantom workflows, splitting one run across workflows and breaking cancel-by-run — the exact hazard the macro's own error text warns about. Both violate the stated 'wrongness must be loud' contract (lib.rs:888-893, 1403-1409); the two macros in one file disagree on strictness.

**Scenario.** #[reactor(name="saga.step", ordering="per_workflow")] compiles and yields PerSubject → nondeterministic saga interleavings with zero diagnostics. #[event(name="run_started", subject_id="run_id", workflow_id = run_id)] compiles as a non-root fact → a job-queue retry after crash-between-emit-and-ack lands under a NEW workflow, so cancel/settle keyed on the run's chain silently miss the retried work.

**Evidence.** lib.rs:974-995 (ordering arm falls through on Expr::Lit and multi-segment paths), 997-1005, 1044-1051, 1062-1067, 1228-1230, 1238-1263; reactor.rs:111; lib.rs:476-487/489-540 (event args parse), 725-756 (workflow_impl only when captured), 704-713 (name backstop that workflow_id lacks); engine.rs:2376-2378.

**Verifier corrections/refinements.** The finding is accurate with one phrasing correction: the claim "the unknown-arg error at lib.rs:1062-1067 can never fire" is true only per-key — that arm DOES fire for genuinely unknown key names in #[reactor] (e.g. `orderin = ...` is a compile error), so #[reactor] is strict on key names but silent on value shapes of known keys; #[event] is silent on both (unknown keys AND value shapes). One additional minor instance in the same class not cited by the finder: parse_event_args's Err(_) branch (lib.rs:476-487) swallows a total attr-parse failure and returns all-None args, which then trips the misleading "needs a `name`" error instead of the actual syntax error. Also `#[reactor(name = 42)]` (non-string name) falls through to the "needs a name" backstop — loud but misdiagnosed. All cited line numbers verified accurate in the working tree: lib.rs:974-995 (ordering), 1044-1051 (backoff_multiplier), 1062-1067 (unknown-arg arm), 489-540 incl. `_ => {}` at 538 (event args), 704-713 (name backstop), 725-756 (workflow_impl); reactor.rs:111 (PerSubject default); engine.rs:2377 (Uuid::new_v4 for undeclared workflow).

**Proof (failing test output):**
```text
Compiling causal v0.18.0 — Finished `test` profile in 2.06s [NO macro diagnostics: all five natural/typo'd spellings compiled clean]

thread 'event_workflow_id_unquoted_is_honored_or_rejected' panicked: assertion `left == right` failed: workflow_id = run_id (unquoted) compiled but was silently dropped... left: None, right: Some(5701a922-8cf2-4ec8-b11c-a15a30c3f26c)
thread 'reactor_int_backoff_multiplier_is_honored_or_rejected' panicked: ... left: None, right: Some(RetryPolicy { max_attempts: 3, initial_backoff_ms: 25, backoff_multiplier: 2.0, max_backoff_ms: 5000 })
thread 'reactor_path_ordering_is_honored_or_rejected' panicked: ... left: PerSubject, right: PerWorkflow
thread 'reactor_quoted_ordering_is_honored_or_rejected' panicked: ... left: PerSubject, right: PerWorkflow
thread 'event_workflow_typo_key_is_rejected_or_honored' panicked: ... left: None, right: Some(073f8753-dfa8-4776-870e-ed760d0ab1f7)

test result: FAILED. 0 passed; 5 failed; 0 ignored
```

Failing test preserved at [`2026-07-02-audit-tests/zz_audit_v110.rs`](2026-07-02-audit-tests/zz_audit_v110.rs) — drop into `modules/causal/tests/` to reproduce (RED today; becomes the regression test for the fix).

Files: `modules/causal_core_macros/src/lib.rs`, `modules/causal/src/reactor.rs`, `modules/causal/src/engine.rs`

---

### 35. Ctx::is_workflow_cancelled silently returns constant false inside projector bodies (minor)

**Severity:** minor · **Verdict:** confirmed with test · **Status at `dev@1721079`:** still present

**Defect.** ProjectionRunner and MultiProjectorRunner construct Ctx with cancelled_workflows: None (projection_runner.rs:298, multi_projector.rs:315) and is_workflow_cancelled() maps None to false (contexts.rs:200-204), so a projector body consulting the fence is silently lied to rather than erroring, and the public doc (contexts.rs:181-199) never states the method is reactor-only.

**Scenario.** A MultiProjector mirroring workflow progress to an external system guards with `if ctx.is_workflow_cancelled() { return Ok(()); }` — the guard never fires and the external system keeps receiving updates for cancelled workflows after all reactors are fenced.

**Evidence.** projection_runner.rs:298; multi_projector.rs:315; contexts.rs:154-155/181-204. (The raw finding also verified a non-issue: DEPENDS_ON cannot wedge on a fenced reactor since fence-acks advance its durable cursor.)

**Proof (failing test output):**
```text
thread 'projector_ctx_reports_cancel_fence' (3100422) panicked at modules/causal/tests/zz_audit_v115.rs:97:5:
Ctx::is_workflow_cancelled inside a projector body returned false for workflow 93dd9c4b-d33f-4d79-a54a-784245fa16a7, which was cancelled BEFORE the trigger was appended — the projector Ctx is wired with cancelled_workflows: None, so the method is a silent constant false
test projector_ctx_reports_cancel_fence ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.06s
```

Failing test preserved at [`2026-07-02-audit-tests/zz_audit_v115.rs`](2026-07-02-audit-tests/zz_audit_v115.rs) — drop into `modules/causal/tests/` to reproduce (RED today; becomes the regression test for the fix).

Files: `modules/causal/src/projection_runner.rs`, `modules/causal/src/multi_projector.rs`, `modules/causal/src/contexts.rs`

---

### 36. PgEventProjector silently resets its source cursor to ZERO on a checkpoint-store read error (minor)

**Severity:** minor · **Verdict:** confirmed by trace · **Status at `dev@1721079`:** still present

**Defect.** The projector loop computes its cursor as checkpoint.get(...).await.ok().flatten().unwrap_or(LogCursor::ZERO) (event_projector.rs:51-56), so a transient PG error is indistinguishable from 'never ran' and restarts the scan of the entire Kurrent $all from zero. Damage is bounded — ON CONFLICT (event_id) DO NOTHING neutralizes inserts and CheckpointStore::advance is monotonic (checkpoint_store.rs:59-65) — so the cost is unbounded re-scan churn and delayed mirroring, not duplication; the error should be logged-and-retried like the read_all arm at :75-78.

**Scenario.** PG blips while Kurrent stays up: every loop iteration during the outage re-reads the whole source log from ZERO in 256-event batches.

**Evidence.** event_projector.rs:51-56 vs :75-78; checkpoint_store.rs:59-65.

**Verifier corrections/refinements.** The scenario 're-reads the whole source log from ZERO in 256-event batches' overstates the shape of the churn: the cursor is recomputed from checkpoint.get at the top of every loop iteration, so while get keeps failing the projector re-reads the SAME first 256-event batch each iteration and can never march past it (insert_batch either fails too, or succeeds as all conflict-no-ops with a monotonic advance that the next failing get ignores). It is constant per-iteration head-of-log churn, not a rolling full scan. Two aggravating details the finder missed: (1) during a full PG outage the path taken is the insert_batch error arm (event_projector.rs:69-71), which has NO sleep — only the read_all-error and empty-batch arms back off — so the loop spins at Kurrent read speed with one tracing::warn per iteration; (2) when PG is up but a single get blips, the redundant insert_batch acquires the global advisory lock (event_projector.rs:92-96) and runs 256 no-op inserts, briefly contending with PgEventLogBackend::append_to_stream. Fix direction as the finder says: treat a get error like the read_all error arm (warn + IDLE_POLL sleep + continue) instead of unwrap_or(LogCursor::ZERO).

Files: `modules/causal_replay/src/event_projector.rs`, `modules/causal/src/checkpoint_store.rs`

---

### 37. Conformance/CI gaps: the entire 0.19 durable surface (PgDecisionStore, PgEventIdRegistry, PgEffectStore) never runs in CI, the live job is still continue-on-error, and no test anywhere runs Kurrent WITH a registry

**Severity:** minor · **Verdict:** confirmed by trace · **Status at `dev@1721079`:** still present

**Defect.** A2 explicitly required un-ignoring the PG/Kurrent conformance suites and removing continue-on-error from that CI job; instead the live job remains continue-on-error: true (in place since 2026-06-10, several releases ago), its Live Postgres step lists only three pre-0.19 test targets (omitting pg_decision_store / pg_effect_store / pg_event_id_registry / pg_checkpoint conformance, pg_consumer_lease_test, pg_crash_recovery_test), and every Kurrent conformance scenario constructs the backend WITHOUT an event_id registry — so the production Kurrent+PgEventIdRegistry configuration is exercised by zero tests and the registry-path contract regressions (C1b/C1c) are invisible to the suite.

**Scenario.** A regression in PgDecisionStore::seal's A4b retry, PgEventIdRegistry's array binding, or the registry-Redelivery semantics merges green: the only blocking job runs memory backends; the live job never executes the new test files, and even the suites it runs cannot redden the build. Wiring conformance::divergent_redelivery_is_rejected (C1b) against a registry-attached Kurrent backend would fail TODAY (the registry byte-identity finding), and the deep-redelivery tests use InMemoryEventIdRegistry, never PgEventIdRegistry.

**Evidence.** .github/workflows/ci.yml:52 `continue-on-error: true` (comment :5-7 'Flip ... once it has been green for a week', dated 2026-06-10); :71-77 PG step lists exactly three --test targets; :78-83 Kurrent step runs two files with `-- --ignored`. modules/causal_replay/tests/kurrent_event_log_conformance_test.rs:19-22 — backend() without with_event_id_registry. tests/kurrent_event_log_test.rs:440-443 — InMemoryEventIdRegistry. pg_decision_store_conformance_test.rs / pg_event_id_registry_conformance_test.rs / pg_effect_store_conformance_test.rs exist, all #[ignore]d, in no CI step. A2: docs/plans/2026-07-02-decision-records-design.md:221-224.

**Verifier corrections/refinements.** Title overstates one point: a test running Kurrent WITH a registry does exist — deep_redelivery_with_registry_dedups at modules/causal_replay/tests/kurrent_event_log_test.rs:438-459, using InMemoryEventIdRegistry, and it is executed by the (non-blocking) CI Kurrent step. The accurate claims are the body's: (1) every Kurrent CONFORMANCE scenario constructs the backend without a registry (kurrent_event_log_conformance_test.rs:19-22), so C1b/C1c never exercise the registry fast-path that skips ensure_redelivery_identical; (2) no test anywhere composes Kurrent with PgEventIdRegistry — the production configuration — since PgEventIdRegistry appears only in pg_event_id_registry_conformance_test.rs (isolated, #[ignore]d, in no CI step) and kurrent_pg_hybrid_test.rs never mentions a registry. Also the omitted-from-CI list can be extended with pg_inspector_test.rs and pg_reactor_observer_test.rs. Release count is five (v0.10.0 through v0.18.0 all contain the 2026-06-10 ci.yml commit; v0.18.0 tagged 2026-07-02).

Files: `.github/workflows/ci.yml`, `modules/causal_replay/tests/kurrent_event_log_conformance_test.rs`, `modules/causal_replay/tests/pg_decision_store_conformance_test.rs`, `modules/causal_replay/tests/pg_event_id_registry_conformance_test.rs`

---

## Fixed during the audit

### F1. Retention GC ignores the ack-floor — A1's mandatory floor-minimum bound is unimplemented at every layer, deleting records for still-redeliverable triggers and reopening the re-decide/chimera

**Severity:** silent_corruption · **Verdict:** confirmed with test · **Status at `dev@1721079`:** fixed at head

**Defect.** Amendment A1 requires GC to remove a decision record only when it is BOTH older than the retention window AND behind the consumer's ack-floor; the shipped sweep is purely age-based — engine passes only now - retention_window, the DecisionStore trait signature remove_sealed_before(cutoff) cannot even express a floor or consumer scope, and both backends delete purely by sealed_at — so records whose triggers will still be redelivered (unacked, wedged, or consumer down > window) get deleted, after which redelivery get-misses the replay gate and the body re-decides.

**Scenario.** Trigger T: body succeeds, record D sealed at t0, then crash (or persistent append/checkpoint infra-retry, or a wedged early trigger in the same partition holding the contiguous floor) keeps the floor below T for longer than the retention window (7d default; with_decision_retention accepts ANY Duration including ZERO with no validation). The age sweep deletes D. Restart/recovery redelivers T; decisions.get misses (reactor_runner.rs:1156-1160); the nondeterministic body re-runs: same-identity/different-payload outputs are swallowed by the accept-and-advance warn, while outputs with different kinds/subjects get fresh event_ids and append cleanly next to D's already-appended rows — a merged chimera batch with NO signal at all. Aggravators from the same omission: remove_sealed_before is unscoped by consumer, so two engines sharing one causal_decisions table silently apply the SHORTEST configured window to everyone; sealed_at uses the sealing node's clock vs the sweeper's clock (skew shortens the window); the sweep-spawn gate is !consumers.is_empty() (any consumer, not just reactors), so a projector-only engine's 7-day default sweeps records a 30-day reactor engine depends on; and a sweep firing mid-infra-retry can delete a record between replay attempts, causing a NEW decision to seal over a half-appended old one.

**Evidence.** Engine sweep age-only: modules/causal/src/engine.rs:1866-1907 (`cutoff = clock.now() - window; store.remove_sealed_before(cutoff)`, gate at :1874 is `!consumers.is_empty()` contradicting its own reactor-gating comment; no window validation at :1091-1094). Trait codifies the omission: modules/causal/src/decision_store.rs:200-209 ('The engine's sweep passes now - retention_window, never a checkpoint position') and memory impl :273-278 (`retain(|_, s| s.sealed_at >= cutoff)`). PG impl: modules/causal_replay/src/decision_store.rs:182-188 (`DELETE FROM causal_decisions WHERE sealed_at < $1` — no consumer scope, no floor). Spec requiring BOTH bounds: docs/plans/2026-07-02-decision-records-design.md:202-211 ('with the floor as a *minimum* bound — never remove a record the floor hasn't passed'); commit 73575bf rewrote this as 'Age-driven, never floor-driven'. Floor is contiguous-prefix so one wedged trigger strands later sealed records: modules/causal/src/reactor_runner.rs:1924-1939; replay gate that re-runs on a GC'd record: reactor_runner.rs:1147-1160; unacked-trigger redeliverability pinned by test at reactor_runner.rs:2660-2729.

**Verifier corrections/refinements.** All substantive claims verified. Small corrections: (1) `with_decision_retention` with no validation is at engine.rs:1098-1101, not :1091-1094. (2) The sweep task body is engine.rs:1867-1907 with the cutoff/delete at :1889-1890 (finder's :1866-1907 range was essentially right). (3) The design doc requiring both bounds is at :202-210 (finder said :202-211). (4) Nuance on "commit 73575bf rewrote this as 'Age-driven, never floor-driven'": the commit rewrote the CODE/trait comments and commit message that way; the design doc itself is uncommitted in the working tree and still mandates the floor-minimum bound — i.e., code contradicts the current spec, strengthening the finding. (5) Nuance on the wedged-trigger vector: the contiguous ack-floor is per-consumer across ALL partitions (reactor_runner.rs:1917-1946), so one wedged trigger strands the records of every later sealed-but-unacked trigger in any partition of that consumer — as the finder said — but note parked (terminal-failure) triggers DO ack and release the floor; only actively-retrying transient/infra failures and genuinely unprocessed triggers pin it. (6) The aggravator list (unscoped multi-engine DELETE → shortest window wins; sealer-vs-sweeper clock skew via sealed_at = sealing node's clock at reactor_runner.rs:1405 vs sweeper's clock.now(); sweep gate `!consumers.is_empty()` at engine.rs:1874 contradicting its own "reactors exist (nothing else seals)" comment) all verified by direct reading; the "sweep firing mid-infra-retry seals a new decision over a half-appended old one" aggravator is plausible by trace but untested. (7) One secondary corruption the finder didn't state: after the re-decide, the newly sealed record and the log disagree forever — the first decision's outputs remain in the log as orphans no record owns, and future replays of the new record never flag them.

**Proof (failing test output):**
```text
DEFECT RUN (cargo test -p causal --test zz_audit_v1 -- --nocapture):
  sweep removed 1 record(s) while the durable floor (None) was still below the trigger — A1's floor-minimum bound is not applied
  after redelivery: body ran 2 time(s); log holds 1 audit_out_a + 1 audit_out_b output(s) for ONE trigger
  re-sealed decision record now says outputs = ["audit_out_b"] — the log disagrees
  thread '...' panicked: assertion `left == right` failed: the reactor body must never re-run for a trigger whose decision was sealed and which was still redeliverable (chimera reopened)
    left: 2
   right: 1
  test gc_must_not_delete_records_the_floor_has_not_passed ... FAILED

CONTROL RUN (AUDIT_SKIP_SWEEP=1, identical except the sweep is skipped):
  sweep removed 0 record(s) while the durable floor (None) was still below the trigger
  after redelivery: body ran 1 time(s); log holds 1 audit_out_a + 0 audit_out_b output(s) for ONE trigger
  test gc_must_not_delete_records_the_floor_has_not_passed ... ok
(test file modules/causal/tests/zz_audit_v1.rs deleted after the runs)
```

Failing test preserved at [`2026-07-02-audit-tests/zz_audit_v1.rs`](2026-07-02-audit-tests/zz_audit_v1.rs) — drop into `modules/causal/tests/` to reproduce (RED today; becomes the regression test for the fix).

**Fixed by:** 7290dd1 fix(decision-store): retention GC honors A1's floor-minimum bound. Commit 7290dd1 is a direct, complete fix for the confirmed defect, at every layer the finding cited. (1) Trait: remove_sealed_before(cutoff) was replaced by remove_reclaimable(consumer, aged_before, floor) in modules/causal/src/decision_store.rs (:212-232), which can now express both A1 bounds plus consumer scope; DecisionRecord gained trigger_position (LogCursor), stamped at seal from event.position (reactor_runner.rs ~:1404). (2) Memory impl (:307-318) keeps a record unless consumer matches AND sealed_at < aged_before AND trigger_position <= floor. (3) PG impl (modules/causal_replay/src/decision_store.rs:186-202) is now `DELETE ... WHERE consumer = $1 AND sealed_at < $2 AND trigger_position <= $3`, with a trigger_position column added to schema + migration. (4) Engine sweep (engine.rs ~:1923-1993) looks up each registered consumer's durable ack-floor and passes it; a missing/unreadable floor is treated conservatively (skip, never reclaim). I recreated the recorded failing test with one minimal adaptation — step 2's sweep now replicates the engine's HEAD formula (floor lookup, None => skip, Some(f) => remove_reclaimable) since the old API no longer exists — and it PASSES: sweep removed 0 records while the floor was below the trigger, record survived, replay gate hit on redelivery, body ran exactly once, log holds exactly one output. The pass is genuine, not an adaptation artifact: the floor lookup IS the fix mechanism, and I additionally exercised the wedged-floor variant the finding described (floor Some(5), trigger at 10, record aged 30d, 7d window) directly against remove_reclaimable — the aged record above the floor survives (removed=0) and is reclaimed only once the floor passes (removed=1). The aggravators are also closed or neutralized: consumer-scoped deletes end the shared-table shortest-window-wins and projector-only-engine cross-deletion vectors; sealer-vs-sweeper clock skew and mid-infra-retry sweeps can no longer delete a redeliverable record because the unacked trigger's floor bound keeps it regardless of age arithmetic; the residual gaps (with_decision_retention still accepts Duration::ZERO unvalidated; sweep gate still !consumers.is_empty(); floor read from `checkpoint` while reactors ack via `reactor_checkpoint` when the two are distinct stores) are all fail-safe now — worst case is over-retention, not deletion of a live record. DS10 conformance (modules/causal_replay/src/conformance.rs:1719-1756) pins the regression guard including "aged record whose trigger the floor has NOT passed survives". Recheck test file deleted after the run.

Files: `modules/causal/src/engine.rs`, `modules/causal/src/decision_store.rs`, `modules/causal_replay/src/decision_store.rs`, `modules/causal/src/reactor_runner.rs`

---

## Investigated and dismissed

### Kurrent read_all cannot address an event at $all commit position 0 — LogCursor::ZERO conflates 'before everything' with 'position 0 occupied'

**Refuted.** The code mechanics are as described (an event at $all commit 0 would be skipped by every ZERO-seeded scan), but the trigger condition is impossible on real KurrentDB: a fresh-instance live check (official image, both --mem-db and disk) shows the first user event lands at commit 1475, the lowest readable record on a fresh server is a system record at commit 548, and bytes 0..548 hold epoch/bootstrap log records that are never events. latest_position()==ZERO is therefore an unambiguous empty sentinel in practice. The only actionable residue is a test-coverage nicety: the Kurrent conformance suite could pin the invariant "first user event position raw > 0 and visible to read_all(ZERO)" against a fresh instance. Severity, if one insists on recording it, is minor (missing defensive assertion/documented invariant), not loss_or_duplication.

```text
Fresh --mem-db instance (port 2199) AND fresh disk-backed instance (port 2198) — identical output:

PROBE latest_position(fresh) = 0 (raw 0)
PROBE first user event: commit position raw = 1475, revision = 0
PROBE first-user-event-commit-position > 0 : true
PROBE read_all(ZERO) returned 1 events
PROBE read_all(ZERO) SEES the first event at position raw 1475
PROBE raw $all record[0]: commit=548 prepare=548 stream=$$$scavenges type=$metadata
PROBE raw $all record[1]: commit=733 prepare=733 stream=$$$connectors-mngt/state-projection type=$metadata
PROBE raw $all record[2]: commit=883 prepare=883 stream=$$$connectors-mngt/state-projection/checkpoints type=$metadata
PROBE raw $all record[3]: commit=1045 prepare=1045 stream=$connectors-ctrl/registry-snapshots type=$conn-ctrl-activated-connectors-snapshot
PROBE raw $all record[4]: commit=1475 prepare=1475 stream=audit-1792612f-... type=audit:first

Decisive: on a completely fresh server the first user event's commit position is 1475 (never 0); the lowest-addressed readable record is a system record at commit 548; read_all(LogCursor::ZERO) sees the first user event. A user event at $all commit 0 cannot occur on stock KurrentDB.
```

### Fence-ack is outside the decision protocol on the WRITE side — no ∅ record is ever sealed, so the cancelled outcome is race-determined across instances, and the floor advance vacuously satisfies the GC bound, letting retention delete a torn decision's only copy

**Refuted.** What is real here belongs to round-1's read-side finding and should amend it, not stand alone: (a) both fence-ack paths (reactor_runner.rs:674-687 and :880-898) bypass decisions.get, so a decision sealed before a cancel can be left permanently half-appended, contradicting the design's explicit "cancelled after sealing still appends — correct, not a leak" (design doc lines 122-124); and (b) as an epilogue to that same hole, the floor passes the fence-acked trigger (:687, :1939-1953) and the retention GC (engine.rs:1949-1996, decision_store.rs:307-319) later deletes the torn record — worth noting in round-1's writeup as "the tear also loses its forensic record", though the deletion itself changes no runtime behavior since the record is already unreachable by then. The write-side ∅-seal proposal is a hardening option (durable suppression when the fence-holder wins the seal race), not a fix for any loss scenario: seal() is first-write-wins in both backends, so at every point where loss occurs a full record already exists and seal-at-gate degenerates into get-and-replay (= round-1's fix). The "later boot gets a get-miss and re-runs the body" claim is wrong: build() rebuilds the fence from the full control stream before runners spawn (engine.rs:1703-1723) and the checkpoint has durably passed the trigger. The residual cross-instance nondeterminism (lagging-fence peer appends a complete batch into a cancelled workflow) requires no-leasor or zombie-lease overlap, is equivalent to cancel-arrived-late, matches documented best-effort cancel semantics (engine.rs:2085-2088), and persists even under the proposed fix.
