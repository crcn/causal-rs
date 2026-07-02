# Ctx-only I/O — design doc (target: 0.20)

Companion to `docs/plans/2026-07-02-decision-records-design.md` (D8). Decision
records (0.19) made reactor nondeterminism *non-corrupting* — a body's outputs
are sealed once and replayed, so an un-memoized external call can no longer grow
a chimera. This doc addresses the failure mode records leave behind: **cost and
incoherence**, not corruption.

## The problem this closes

`ctx.effect(label, ..)` is *advisory*. Nothing stops a reactor body from calling
an HTTP client, the wall clock, or an RNG directly — outside any effect scope.
The downstream evidence: 5 reactors used `ctx.effect` correctly and 35 did not
(the D-series rationale). The consequences after records:

- **Wasted retry cost.** An un-memoized call re-runs on every body attempt (and
  on every first delivery that later fails and retries before sealing). Records
  don't help here — the seal happens *after* the body, so a retry before seal
  re-does the raw I/O.
- **Incoherent effect boundaries.** A body that reads `Utc::now()` or
  `Uuid::new_v4()` inline produces values that differ per attempt; records seal
  whichever attempt won, but the *inputs* to that decision were never captured,
  so the decision isn't reconstructible or auditable.
- **The lint (D7) is opt-in and evadable.** `clippy.toml`'s
  `disallowed-methods` catches the named offenders (`Uuid::new_v4`, `Utc::now`)
  but not an arbitrary HTTP client, and any crate can `#[allow]` it.

Records make this non-urgent (no corruption), but 0.20 is the only version where
the *next* reactor can't reintroduce the pattern — the fix is structural, so it
belongs before the API ossifies at 1.0.

## The shape: I/O reachable only through an effect scope

Make un-memoized I/O *unwritable* rather than merely inadvisable. Dependencies
(HTTP client, clock, RNG, graph handle) are reachable **only** through an
effect-scoped accessor that memoizes by construction:

```rust
// 0.19 (advisory): deps are ambient; ctx.effect is a convention.
let page = ctx.effect("fetch", || async { http.get(&url).await }).await?;
let other = reqwest::get(&url2).await?;   // ← compiles; the trap

// 0.20 (structural): deps live behind ctx.io; there is no ambient client.
let page = ctx.io("fetch", |deps| async move { deps.http.get(&url).await }).await?;
let other = reqwest::get(&url2).await?;   // ← still compiles at the language
                                          //   level, but `reqwest` is not a
                                          //   declared dep and has no url/token
                                          //   context — see "Enforcement".
```

`ctx.io(label, |deps| fut)`:
- `label` keys the memo exactly like `ctx.effect` today (`(consumer, trigger,
  label)`), so redelivery/retry replays the cached result.
- `deps` is a per-engine bundle of the I/O capabilities the app registered at
  build time (`EngineBuilder::with_io_deps(...)`). The closure is the *only*
  place a `deps` reference exists.
- The result is `Serialize + DeserializeOwned` (same bound as `remember`), so it
  round-trips through the effect store.

### Enforcement — how "unwritable" is actually achieved

Rust can't forbid a reactor from importing `reqwest` directly. Enforcement is
therefore layered, strongest first:

1. **No ambient handle.** The app's I/O clients are moved into the `deps` bundle
   and are *not* otherwise in scope inside a reactor module (they live behind
   `ctx.io`). A body that wants the shared HTTP client, DB pool, or graph handle
   can only get it through the scope. This kills the accidental case (the 35).
2. **Clock/RNG via `deps`.** `deps.clock` / `deps.rng` replace `Utc::now()` /
   `Uuid::new_v4()`; the D7 lint stays as the backstop for the language-level
   escapes.
3. **A body that constructs its *own* client** (a fresh `reqwest::Client`) is
   still possible — that's a deliberate act, not an accident, and the lint plus
   review catch it. The goal is to make the wrong thing require effort, not to
   make it impossible in a language without capability effects.

## Migration (breaking, real downstream churn)

This is a breaking API change with genuine downstream migration (~35 call
sites). Sequencing:

1. Ship `ctx.io` + `with_io_deps` **additively** alongside `ctx.effect` in a
   0.20-preview: both work, `ctx.effect` is `#[deprecated]` with a note pointing
   at `ctx.io`.
2. Downstream migrates reactor by reactor; the deprecation warning is the
   checklist.
3. Remove the ambient handles from reactor scope once the last body migrates —
   that removal is what turns the trap from "inadvisable" into "unwritable".
4. `ctx.effect` retained one release past the ambient-handle removal as a thin
   shim over `ctx.io` with a default `deps` of `()`, then dropped.

`ctx.effect` and `ctx.io` share the same effect-store key derivation and the
same store, so a trigger mid-migration (sealed under `effect`, replayed under
`io`) resolves to the same memo — no data migration.

## What this does NOT change

- Decision records remain the correctness backbone; `ctx.io` is a
  cost/coherence refinement on top, not a replacement.
- Fold (`apply`) purity is unaffected — folds take no `ctx` and do no I/O by
  construction; the D5 self-check remains their guard.
- The effect store trait is unchanged; `ctx.io` is a new *surface* over the same
  memoization primitive.

## Open questions (resolve before coding 0.20)

- **`deps` typing.** One concrete app-defined struct (simplest, least generic)
  vs. a registry keyed by capability type (more flexible, more machinery).
  Lean concrete: a reactor engine has one I/O bundle.
- **Async closure ergonomics.** `|deps| async move { .. }` capturing `deps` by
  ref across an await needs the bundle to be `Clone`/`Arc` internally; confirm
  the borrow shape before committing the signature.
- **Per-reactor dep subsets.** Whether a reactor declares which capabilities it
  needs (tighter least-privilege) or receives the whole bundle. Defer to a
  follow-up unless it falls out cheaply.
