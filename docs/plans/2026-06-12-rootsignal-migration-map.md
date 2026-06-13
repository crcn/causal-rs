# rootsignal migration map (0.10 step 7)

Static verification of rootsignal (`~/Developer/fourthplaces/rootsignal`,
pinned `causal = "0.8.0"`) against the 0.10 surface — produced WITHOUT
patching rootsignal, per the release flow: verify statically → publish
causal 0.10 → bump rootsignal → migrate.

Consuming crates: `rootsignal-api`, `rootsignal-graph`,
`rootsignal-scout`, `rootsignal-scout-supervisor`.

## Inventory (grep counts, 2026-06-12)

| 0.8 surface | hits | 0.10 replacement |
|---|---|---|
| `GROUP_NAME` | 77 | `NAME` (mechanical) |
| `correlation_id` | 177 | `workflow_id` domain-side; SQL/storage columns KEEP `correlation_id` (the boundary rule — see the live-gate drift this repo just fixed in its own backends; rootsignal must not sweep its SQL) |
| `stream_id` | 56 | `subject_id` (mechanical; beware kurrentdb client API calls — `rec.stream_id()` is THEIR vocabulary, not ours) |
| `aggregate_of` (ctx) | 42 | `ctx.state_of(id).await?` — now async + Result; in reactors it's a position-bounded fold-on-read |
| `CATEGORY` / `STREAM_CATEGORY` | 28 / 14 | `NAME` / `SUBJECT` (names become flat + exact; see vocabulary note below) |
| `engine.snapshot` | 15 | `engine.state_of(id).await?` |
| `load_aggregate` | 4 | `engine.state_of` |
| `on_dlq` / `DlqInfo` | 1 / 3 | `on_terminal_failure` / `TerminalFailure` (now carries `subject`/`subject_id`/`class`) |
| `#[causal::event(prefix = ..)]` enums | 4 sites | **the big one** — see below |
| `for_type_with_id_fn` (fan-in aggregates) | 2 | still legal in projectors; `ctx.state_of` on them in a REACTOR now errors (teaching) — move those reads to projector-maintained read models |
| `Uuid::new_v4` / `Utc::now` | 646 / 321 | determinism audit: in consumer bodies → `ctx.derive_id(label)` / `ctx.time()`; at boundaries they stay legal. Adopt the clippy `disallowed-methods` config from `causal::contexts` module docs |

## The four enum facts (retracted form)

All four macro sites are family enums with the retracted shape:

- `domains/coalescing/events.rs` — `prefix = "coalescing", ephemeral`
- `domains/cluster_weaving/events.rs` — `prefix = "cluster_weaving", ephemeral`
- `domains/supervisor/events.rs` — `prefix = "supervisor", ephemeral`
- `domains/curiosity/events.rs` — `prefix = "curiosity"` (**durable** — this
  is the curiosity restream)

Each variant becomes a struct fact: `name` (flat, exact, REQUIRED),
`subject_id = "<field>"` (shape-gated), `subject = "<kind>"` where
variants co-locate in one subject history. The three ephemeral enums
migrate code-only. **Curiosity is durable**: historical events carry the
old composed `event_type`s, so the restream (replay old → emit new
kinds, or a consumer-side kind-mapping during a deprecation window) must
be planned with the data — this was already a standing item before 0.10.

## Semantics changes that need eyes, not seds

1. **Reactors run concurrently** (per-subject partitions by default).
   Any reactor relying on serial total order needs an `ORDERING`
   declaration (`per_workflow` for run pipelines) — review each of the
   ~69 `ctx.*` reactor bodies against the BLOCKING-4 memo.
2. **No-mapper reactors now park after 3 attempts** (built-in
   `causal:reaction_failed` on the trigger's subject) instead of
   retrying forever. Classify errors at call sites:
   `.map_err(causal::transient)` for Neo4j/HTTP/LLM-infra calls is the
   S3-wedge fix. Completion folds MUST fold terminal facts as
   completion-with-error or runs leak "running".
3. **Settle is per-workflow** — `settled()` no longer waits for
   unrelated consumers. The 52 settle call sites mostly get FASTER, but
   any test that used settle as a global barrier needs `settle_tree`
   (test-only) or explicit waits.
4. **Slow external labor → workflow roots**: enrichment-style facts
   declare `workflow_id = "<field>"` (value via `ctx.derive_id`) so
   they stop holding their parent's settle hostage. Candidates: the
   media-enrichment dispatch path discussed in the design doc.
5. **Side-effecting reactors → `ctx.effect(label, ..)`** (the old
   `ctx.remember(NAME, ..)` shape is gone; multiple effects per
   reaction now key by label). Wire `with_effect_store` +
   (optionally) a PG `EffectStore` impl — note the trait gained
   `remove()` for floor-GC.
6. **Macro surface**: new consumers can use `#[causal::reactors]` /
   `#[causal::projectors]` modules instead of trait impls — adopt
   opportunistically, not as part of the rename pass.

## Order of operations

1. Publish causal 0.10 (after this repo's release gates: live
   conformance ✅ 2026-06-12, checkpoint conformance ✅, version bump
   pending).
2. Bump rootsignal pins; let the compiler drive the mechanical renames
   (GROUP_NAME/NAME, stream_id/subject_id, aggregate_of/state_of,
   snapshot/state_of, DlqInfo/TerminalFailure, correlation_id —
   domain-side only).
3. Struct-ify the three EPHEMERAL enums (code-only).
4. Error classification pass (`transient`/`poison`/`domain`) + terminal
   completion folds — this is the behavior-critical step; deploy gate.
5. ORDERING review per reactor; declare roots for slow labor.
6. Curiosity restream (durable data; its own plan).
7. Determinism audit (`derive_id`/`time`/`effect`) + clippy guardrail
   in rootsignal's workspace.
