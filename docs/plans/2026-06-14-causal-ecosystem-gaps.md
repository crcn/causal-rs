# causal ecosystem gaps — 2026-06-14

Three concrete gaps surfaced from a comparative study of Temporal, Restate, Resonate,
Inngest, Hatchet, Azure Durable Functions, and Flink. Each gap is grounded in a
specific file and line number. Sequencing matters: Gap 1 unblocks Gap 3.

---

## Design decision — no `Workflow` trait

Every durable execution system in the ecosystem separates "workflow" from "activity"
(Temporal), "handler" from "step" (Restate), or "function" from "step" (Inngest). The
split exists because those systems have a genuine runtime distinction: a workflow can
**suspend mid-execution** and resume when an external event arrives. An activity runs
to completion. The type boundary enforces the semantic boundary.

causal has no such runtime distinction. A `Reactor` runs to completion in one
invocation. The event chain IS the suspension mechanism — when a reactor needs to wait
for something, it emits a fact and a downstream reactor picks up from there. There is
no mid-body pause.

The proposed `Workflow` trait would have been:

```rust
// What it would look like:
trait Workflow {
    type Trigger: Event;
    const NAME: &'static str;
    async fn run(&self, trigger: &Self::Trigger, ctx: WorkflowCtx<'_>) -> Result<Events>;
}

// With a blanket impl:
impl<W: Workflow> Reactor for W { ... }
```

`WorkflowCtx` would rename `ctx.effect()` → `ctx.step()` and add no other surface.
The blanket impl makes `Workflow` literally identical to `Reactor` at runtime.
`ctx.step()` is `ctx.effect()` with a different name.

**Decision: do not add the trait.** Reasons:

1. **No runtime distinction.** The trait boundary exists in Temporal/Restate to enforce
   a real semantic difference. That difference (mid-execution suspension) does not exist
   in causal. A trait without a semantic distinction is a naming ceremony.

2. **`ctx.effect()` already IS the step primitive.** A reactor that calls
   `ctx.effect("scrape", ...)` then `ctx.effect("dedup", ...)` sequentially is already
   doing exactly what a "workflow step" does in other systems — memoized, exactly-once,
   ordered. No rename needed to unlock that pattern.

3. **Vocabulary import without structural justification.** Calling it `ctx.step()` is
   reasonable in Temporal because `step` refers to an Activity invocation — a discrete
   unit with its own retry policy and timeout. In causal, the memoized primitive is a
   side effect cache entry. `effect` is the precise name.

4. **Flat surface area.** One primitive (`ctx.effect`) for all memoized side-effectful
   work — regardless of whether the reactor is "reactive" (reads aggregate state,
   emits a few facts) or "orchestrating" (calls external services sequentially). The
   author decides the pattern; the framework provides one tool.

**What this means in practice:** a reactor that coordinates a multi-step pipeline
looks like this:

```rust
impl Reactor for SourceScan {
    type Trigger = SourceScanRequested;
    const NAME: &'static str = "source_scan";

    async fn react(&self, req: &SourceScanRequested, ctx: Ctx<'_>) -> Result<Events> {
        let raw      = ctx.effect("scrape",   || scrape(&req.source_id)).await?;
        let signals  = ctx.effect("dedup",    || dedup(raw)).await?;
        let clusters = ctx.effect("coalesce", || coalesce(&signals)).await?;
        Ok(events![SourceScanCompleted { .. }])
    }
}
```

This is a "workflow" in every meaningful sense: sequential, memoized, retryable at
each step. It is also just a `Reactor`. No additional abstraction layer required.

---

## Gap 1 — Mandatory effect store (correctness)

**Severity: correctness.** A reactor that calls `ctx.effect()` with no store wired
panics at runtime instead of failing at configuration time or working silently.

### What the code does today

`contexts.rs:346`:
```rust
let cache = self.effect_store.ok_or_else(|| {
    anyhow::anyhow!(
        "ctx.effect called but no EffectStore was configured \
         (EngineBuilder::with_effect_store)"
    )
})?;
```

`engine.rs:545` in `EngineBuilder::new()`:
```rust
effect_store: None,
```

A reactor body that calls `ctx.effect("fetch", ...)` succeeds in a test that never
configured `with_effect_store` — until it's retried. On the second invocation the
stored effect entry is missing (no store), and the external call runs again. This is
the failure mode the effect store exists to prevent.

Restate and Temporal never have this failure mode: their journal IS the store, always
present. causal's `effect_store` is optional only because backends are injected.
The right default is the in-memory backend, not `None`.

### Fix

1. In `engine.rs`, change `EngineBuilder::new()` to default `effect_store` to
   `Arc::new(InMemoryEffectStore::new())` (already exported from `effect_store.rs:133`).
2. Remove the `ok_or_else` error branch from `contexts.rs:346`. Replace with a plain
   `.unwrap()` — the field is never `None` after this change.
3. `with_effect_store()` remains the override path for production callers who want
   durability (Postgres-backed store, etc.). Calling it replaces the default.

**Breaking change:** none. The runtime panic becomes correct behavior. Any test that
was accidentally relying on the panic being absent (i.e., a test that called
`ctx.effect()` with no store and got away with it on the first invocation) will now
work correctly instead.

**Docs addition:** update `EngineBuilder::with_effect_store` rustdoc to say
"overrides the default InMemoryEffectStore; required in production for cross-restart
durability."

---

## Gap 2 — Divergence detection

**Severity: correctness backstop.** The framework has no way to detect when a reactor
body takes a different branching path on a retry than it did on the first attempt. The
data structure for detection was laid, but the comparison was never wired.

### What the code does today

`contexts.rs:72`:
```rust
pub(crate) type LabelSet = Mutex<std::collections::HashSet<(&'static str, String)>>;
```

`reactor_runner.rs:745-751` (after each attempt):
```rust
used_effects.extend(
    labels
        .into_inner()
        .into_iter()
        .filter(|(kind, _)| *kind == "effect")
        .map(|(_, label)| label),
);
```

`used_effects` is a `Vec<String>` that accumulates all effect labels across retries.
The data exists but nothing compares attempt N's labels against attempt N-1's labels.
An unordered `HashSet` can't detect ordering changes anyway.

Temporal detects this via journal replay: a diverged sequence causes an explicit
`NonDeterministicError`. Restate detects it similarly. causal is the only layer where
reactor authors have a deterministic `Ctx` — but without detection, a branching bug
(e.g., `if rand::random()` inside a reactor) produces silently wrong results under retry.

### Fix

**Step 1 — ordered LabelSet.** Change the type alias:
```rust
// before:
pub(crate) type LabelSet = Mutex<std::collections::HashSet<(&'static str, String)>>;

// after (preserves insertion order for comparison):
pub(crate) type LabelSet = Mutex<Vec<(&'static str, String)>>;
```

`claim_label` in `contexts.rs` currently inserts into a `HashSet` for O(1) duplicate
detection. With a `Vec`, duplicate detection becomes an O(N) linear scan — acceptable
since N is small (a typical reactor claims 2–5 labels). Alternatively, use
`IndexSet` from the `indexmap` crate for O(1) duplicate detection with preserved
insertion order.

**Step 2 — per-attempt capture.** Instead of extending one flat `used_effects` across
attempts, capture each attempt's label sequence separately:
```rust
// before:
let mut used_effects: Vec<String> = Vec::new();
// ...
used_effects.extend(labels...);

// after:
let mut attempt_labels: Vec<Vec<String>> = Vec::new();
// ...
let this_attempt: Vec<String> = labels.into_inner()
    .into_iter()
    .filter(|(kind, _)| *kind == "effect")
    .map(|(_, label)| label)
    .collect();
attempt_labels.push(this_attempt);
```

**Step 3 — compare.** After pushing, if `attempt_labels.len() > 1`, compare the
last attempt's sequence against the first attempt's sequence:
```rust
if let (Some(first), Some(last)) = (attempt_labels.first(), attempt_labels.last()) {
    if first != last {
        tracing::error!(
            consumer = self.consumer_id,
            event_id = %event.event_id,
            first = ?first,
            last = ?last,
            "reactor diverged across retries: effect label sequence changed. \
             This indicates nondeterminism inside react() — a likely bug.",
        );
    }
}
```

On divergence: log an error and continue (do not panic, do not park — the body may
still produce correct output; the log is the alert for investigation). A future
`with_strict_divergence()` builder option could escalate to domain-error for teams
that want hard enforcement.

**Breaking change:** none at the API level. Internal type alias change only.
`IndexMap` is a new optional dependency.

---

## Gap 3 — `ctx.effect_all()` parallel combinator

**Severity: ergonomics / performance.** Running N side effects concurrently inside
a reactor today requires wrapping them all in a single `ctx.effect("combined", ...)`,
losing per-effect label granularity and caching. The only alternative is manual
`tokio::join!`, which bypasses the memoization layer entirely.

Inngest solves this natively with `step.run` + `Promise.all`. Restate solves it with
`ctx.run` + Rust's `try_join!`. causal needs the same primitive.

### What is missing

`Ctx` has `effect()` for one effect at a time. There is no combinator for:
```rust
let (html, meta) = ctx.effect_all([
    ("fetch_html",  || async { scrape_html(&url).await  }),
    ("fetch_meta",  || async { scrape_meta(&url).await  }),
]).await?;
```

### Fix

Add `effect_all` to `Ctx`:

```rust
pub async fn effect_all<F, Fut, T, const N: usize>(
    &self,
    effects: [(&str, F); N],
) -> Result<[T; N]>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
    T: serde::Serialize + serde::de::DeserializeOwned + Send,
{
    // 1. Claim all labels atomically upfront — fail fast before any I/O
    //    if a label is already claimed.
    for (label, _) in &effects {
        self.claim_label("effect", label)?;
    }

    // 2. Scatter: for each effect, check cache then run if missing.
    let cache = self.effect_store.as_ref().expect("effect_store always present after Gap 1");
    let futures: Vec<_> = effects.into_iter().map(|(label, compute)| {
        let key = EffectKey::new(self.consumer, self.event_id, label);
        async move {
            remember(&**cache, &key, compute).await
        }
    }).collect();

    // 3. Gather — futures::future::try_join_all preserves input ordering.
    let results: Vec<T> = futures::future::try_join_all(futures).await?;

    // 4. Convert to fixed-size array.
    results.try_into().map_err(|_| anyhow::anyhow!("effect_all: size mismatch"))
}
```

For variable-length inputs (N not known at compile time), a `Vec`-based overload:
```rust
pub async fn effect_vec<F, Fut, T>(
    &self,
    effects: Vec<(&str, F)>,
) -> Result<Vec<T>>
```

**Semantics:**
- Labels are claimed atomically before any effect runs (prevents partial-claim races).
- Cache is checked per-label — effects with warm caches return immediately; only
  cold effects execute concurrently. A partial-warm input is maximally efficient.
- Results are returned in input order, not completion order — deterministic across
  retries regardless of which effects hit cache vs run live.
- A single failed effect fails the whole combinator (same semantics as `try_join_all`).

**Depends on Gap 1** (effect_store is always present).

**Breaking change:** none (additive).

---

## Gap 4 (lower priority) — `ctx.version()` change markers

**Severity: operational.** When a reactor's branching logic changes across deploys, old
cached effects in the `EffectStore` may have been produced by the old code path. Without
version markers, there is no way to detect or invalidate stale cache entries.

Temporal's `GetVersion()` solves this: it returns the current max version and records a
marker in the history, allowing old workflows replaying pre-marker history to use the
old code path. The design fits causal's model.

### Sketch

```rust
// In Ctx:
/// Returns Ok(()) if the recorded version is <= `max_supported`. Returns
/// Err(causal::version_mismatch(...)) if this trigger was produced by a
/// later code version than this worker understands. Stamps the version
/// into the effect store under a sentinel key on first call.
pub async fn version(&self, label: &str, max_supported: u32) -> Result<u32>
```

Callers gate behavior:
```rust
let v = ctx.version("embed_model", 2).await?;
if v >= 2 {
    ctx.effect("embed", || embed_v2(&text)).await?
} else {
    ctx.effect("embed", || embed_v1(&text)).await?
}
```

Implementation stores the version marker in the `EffectStore` under
`(consumer, trigger_id, "_version:{label}")`. On first invocation it writes
`max_supported`; on redelivery it reads back the stored version. If the stored
version exceeds `max_supported`, return a domain error — this worker is too old
to replay the trigger.

**Not required for 0.11.** The clippy `disallowed-methods` lint, effect memoization,
and the divergence detection from Gap 2 collectively make most version-migration cases
safe without explicit markers. Defer until a concrete migration scenario demands it.

---

## Implementation order

1. **Gap 1** — mandatory default `InMemoryEffectStore`. Small, high confidence,
   correctness. Unblocks Gap 3.
2. **Gap 2** — ordered `LabelSet` + divergence comparison. Internal-only change.
3. **Gap 3** — `ctx.effect_all()`. Additive. Depends on Gap 1.
4. **Gap 4** — `ctx.version()`. Defer until a real migration scenario.

Gaps 1–3 are 0.11 material. Gap 4 is 0.12 or later.
