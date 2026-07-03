//! Single `Ctx` type passed to all consumer bodies. Three separate
//! context types would add no safety beyond what the runtime already
//! enforces.
//!
//! ## The deterministic `Ctx` (0.10 Primitive 2)
//!
//! `react()` gets no ambient world. The `Ctx` is the only door, and
//! every door is deterministic-or-memoized:
//!
//! ```ignore
//! async fn react(&self, t: &Trigger, ctx: Ctx<'_>) -> Result<Events> {
//!     let id   = ctx.derive_id("group")?;   // v5(consumer ∥ trigger ∥ label); never new_v4
//!     let when = ctx.time();                // the TRIGGER's recorded time
//!     let text: String = ctx.effect("ocr", || async {
//!         claude.ocr(&t.url).await          // memoized by construction
//!     }).await?;
//!     ...
//! }
//! ```
//!
//! No wall-clock accessor exists. `ctx.time()` returns the fact's
//! logical `occurred_at`, set at emit by the producer — append-time
//! `created_at` stays envelope-only; wall clock must never enter a
//! payload. Replay reproduces byte-identical state because
//! deterministic time is the only reachable time. Migration-boundary
//! tagging belongs in `ctx.metadata`, not a dedicated enum.
//!
//! ## The guardrail layer
//!
//! Rust cannot make nondeterminism inexpressible — nothing *stops*
//! `Uuid::new_v4()` inside `react()`. Three layers, none sufficient
//! alone: this deterministic `Ctx` is the paved road; a clippy
//! `disallowed-methods` lint in application crates is the guardrail;
//! the runtime divergence check is the backstop. Recommended
//! `clippy.toml` for crates that hold reactor bodies:
//!
//! ```toml
//! [[disallowed-methods]]
//! path = "uuid::Uuid::new_v4"
//! reason = "in consumer bodies, mint ids with ctx.derive_id(label)"
//! [[disallowed-methods]]
//! path = "chrono::Utc::now"
//! reason = "in consumer bodies, use ctx.time(); wall clock breaks replay"
//! ```
//! (Allow at non-consumer call sites with
//! `#[allow(clippy::disallowed_methods)]` — the boundary that *emits*
//! facts legitimately mints fresh identities.)

use std::sync::Arc;

use anyhow::Result;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use uuid::Uuid;

use crate::aggregate::Aggregate;
use crate::aggregator::{AggregatorRegistry, FoldOnReadCache};
use crate::types::{LogCursor, LogEntry, LogLevel};

/// Namespace UUID for `ctx.derive_id` — distinct from the reactor
/// runner's output-id namespace so a derived identity can never
/// collide with an output event_id.
const NS_DERIVED_ID: Uuid = Uuid::from_bytes([
    0x9c, 0x1a, 0x2b, 0x77, 0x41, 0x5d, 0x4e, 0x02,
    0x8f, 0x66, 0x0d, 0xe9, 0x5a, 0x33, 0x70, 0x1b,
]);

/// Per-invocation label registry for `ctx.derive_id` / `ctx.effect`
/// duplicate detection. The runner owns one per attempt; duplicate
/// labels silently yielding the same id/result would be a silent-
/// correctness bug, so reuse is a runtime error (domain class).
///
/// Ordered Vec (not HashSet): insertion order is preserved so the
/// reactor runner can compare effect label sequences across retry
/// attempts to detect control-flow divergence.
pub(crate) type LabelSet = Mutex<Vec<(&'static str, String)>>;

/// Where `ctx.state_of` reads aggregate state from.
///
/// Serial consumers (projectors, multi-projectors) read the shared
/// per-consumer registry, folded in scan order before the body runs —
/// today's semantics, unchanged. Partitioned reactors fold on read:
/// the subject history from the log, bounded at the trigger's
/// position, so the answer is deterministic regardless of what other
/// partitions are doing (BLOCKING-1).
#[derive(Clone, Copy)]
pub(crate) enum StateSource<'a> {
    /// No aggregators wired.
    None,
    /// Shared per-consumer registry (serial consumers).
    Registry(&'a Arc<AggregatorRegistry>),
    /// Position-bounded fold from the log (partitioned reactors).
    FoldOnRead {
        /// Fold-function table (the registry's state is unused here).
        registry: &'a Arc<AggregatorRegistry>,
        log: &'a dyn crate::event_log::EventLogBackend,
        /// The trigger's position — reads answer "as of this event".
        bound: LogCursor,
        /// Worker-local incremental cache; dies with the partition.
        cache: &'a FoldOnReadCache,
    },
}

/// Aggregate state snapshot pair returned by [`Ctx::state_of`]. `prev` is the state before the current
/// event was folded; `curr` is the state after. Both are `Arc<A>`
/// so reads are zero-clone.
pub struct AggregateState<A> {
    pub prev: Arc<A>,
    pub curr: Arc<A>,
}

/// Per-event metadata carried through the log envelope. Application
/// code may stamp arbitrary keys at emit time (`_run_id`,
/// `_schema_v`, `_phase` for migration boundaries); consumers read
/// them via `ctx.metadata`.
pub type Metadata = serde_json::Map<String, serde_json::Value>;

/// Context passed to every consumer body
/// (`Projector::project`, `Reactor::react`, `MultiProjector::project`).
///
/// Deliberately absent: any wall-clock accessor. `ctx.time()` returns
/// `occurred_at`, never the system clock.
#[derive(Clone, Copy)]
pub struct Ctx<'a> {
    pub event_id:       Uuid,
    pub log_position:   LogCursor,
    pub occurred_at:    DateTime<Utc>,
    pub workflow_id: Uuid,
    pub metadata:       &'a Metadata,
    /// The consuming `Reactor::NAME` / `Projector::NAME` — keys
    /// `derive_id` and `effect` derivations. Empty string when the Ctx
    /// is hand-constructed outside a runner (tests).
    pub(crate) consumer: &'a str,
    /// Per-invocation duplicate-label registry for `derive_id` /
    /// `effect`. `None` outside a reactor attempt (duplicate detection
    /// is then skipped).
    pub(crate) labels: Option<&'a LabelSet>,
    /// Read-side access to aggregate state — see [`StateSource`].
    /// Query via [`Ctx::state_of`].
    pub(crate) state: StateSource<'a>,
    /// Per-attempt sink for `ctx.log(...)` entries. The runner owns
    /// the underlying Vec and drains it after `react()` returns,
    /// routing the entries through the configured
    /// [`ReactorObserver`](crate::reactor_observer::ReactorObserver).
    /// `None` when not running inside a reactor body (engine emit
    /// path, projector body, tests that construct Ctx by hand).
    pub(crate) logs: Option<&'a Mutex<Vec<LogEntry>>>,
    /// Optional reaction-result cache (Phase 4). Lets a side-effecting
    /// reactor memoize its external call under its [`EffectKey`] so
    /// redelivery / retry runs the call effectively once. `None` unless
    /// the engine was built with `EngineBuilder::with_effect_store`.
    pub(crate) effect_store:
        Option<&'a Arc<dyn crate::effect_store::EffectStore>>,
    /// Shared cancel fence from the engine. `Some` inside reactor bodies;
    /// `None` in projector bodies and hand-constructed Ctx values.
    pub(crate) cancelled_workflows:
        Option<&'a crate::engine::CancelledWorkflows>,
    /// Set by [`is_workflow_cancelled`](Ctx::is_workflow_cancelled) so the
    /// runner knows the body CONSULTED the fence. Fence-consulted emptiness
    /// is fence-dependent, not deterministic — the runner must seal it even
    /// under `.seal_empty_decisions(false)`, or a redelivery where the fence
    /// reads differently re-decides a different outcome. `None` outside
    /// reactor bodies.
    pub(crate) fence_consulted:
        Option<&'a std::sync::atomic::AtomicBool>,
}

impl<'a> std::fmt::Debug for Ctx<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ctx")
            .field("event_id", &self.event_id)
            .field("log_position", &self.log_position)
            .field("occurred_at", &self.occurred_at)
            .field("workflow_id", &self.workflow_id)
            .field("metadata", &self.metadata)
            .field(
                "state_source",
                &match self.state {
                    StateSource::None => "none",
                    StateSource::Registry(_) => "registry",
                    StateSource::FoldOnRead { .. } => "fold-on-read",
                },
            )
            .field("has_effect_store", &self.effect_store.is_some())
            .finish()
    }
}

impl<'a> Ctx<'a> {
    /// True if the engine's cancel fence contains this trigger's
    /// `workflow_id` — i.e., `Engine::cancel_workflow` was called for
    /// this workflow before or after the trigger was dispatched.
    ///
    /// **O(1)** — hashset lookup against the shared fence. **Replayable**
    /// — the fence is rebuilt from the durable `causal:control` stream
    /// during engine startup, so replay produces the same answer as the
    /// original run for any trigger dispatched after the cancel marker.
    ///
    /// Typical use: early-exit at the top of a reactor body so already-
    /// dispatched triggers do no real work after the workflow is cancelled.
    ///
    /// ```ignore
    /// async fn react(&self, evt: &MyEvent, ctx: Ctx<'_>) -> Result<Events> {
    ///     if ctx.is_workflow_cancelled() { return Ok(Events::new()); }
    ///     // ...
    /// }
    /// ```
    ///
    /// **Interaction with `seal_empty_decisions(false)`:** calling this method
    /// marks the reaction as fence-consulted, which forces its empty decision
    /// to seal even when empty-seal elision is enabled — the cancel outcome
    /// must be durable so a redelivery (where the fence may read differently)
    /// replays it instead of re-deciding. A body using the early-exit above
    /// therefore seals every no-op decision; elision only benefits bodies that
    /// never consult the fence.
    pub fn is_workflow_cancelled(&self) -> bool {
        // Record the consult itself (not just a `true` answer): a body that
        // BRANCHES on the fence produces fence-dependent output either way —
        // `false` here with an empty return elided from the decision store
        // would let a fence-`true` redelivery decide a different batch.
        if let Some(flag) = self.fence_consulted {
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        self.cancelled_workflows
            .map(|f| f.lock().unwrap().contains(&self.workflow_id))
            .unwrap_or(false)
    }

    /// The TRIGGER's recorded time — the fact's logical `occurred_at`.
    /// Consumers that need a timestamp use this, never wall-clock, so
    /// replay reproduces state byte-identically. (Append-time
    /// `created_at` stays envelope-only; wall clock must never enter a
    /// payload.)
    #[inline]
    pub fn time(&self) -> DateTime<Utc> { self.occurred_at }

    /// Mint a deterministic identity for something this reaction
    /// creates: `v5(consumer ∥ trigger event_id ∥ label)`. The same
    /// trigger redelivered derives the same id, so the entity the
    /// reaction creates exists once — never `Uuid::new_v4()` in a
    /// consumer body.
    ///
    /// The label names *which* identity (a reaction may mint several);
    /// it's the one argument with genuinely nothing else to derive
    /// from. Reusing a label within one invocation would silently
    /// yield the same id — a silent-correctness bug — so it errors
    /// (domain class: bounded retries, then parks).
    pub fn derive_id(&self, label: &str) -> Result<Uuid> {
        self.claim_label("derive_id", label)?;
        let mut key: Vec<u8> =
            Vec::with_capacity(self.consumer.len() + 16 + label.len() + 2);
        key.extend_from_slice(self.consumer.as_bytes());
        key.push(0);
        key.extend_from_slice(self.event_id.as_bytes());
        key.push(0);
        key.extend_from_slice(label.as_bytes());
        Ok(Uuid::new_v5(&NS_DERIVED_ID, &key))
    }

    fn claim_label(&self, kind: &'static str, label: &str) -> Result<()> {
        if let Some(labels) = self.labels {
            let mut lock = labels.lock();
            if lock.iter().any(|(k, l)| *k == kind && l == label) {
                return Err(crate::failure::domain(anyhow::anyhow!(
                    "duplicate ctx.{kind}(\"{label}\") in one reaction — each \
                     call would silently yield the same result; give each \
                     identity/effect its own label",
                )));
            }
            lock.push((kind, label.to_string()));
        }
        Ok(())
    }

    /// Append a log entry to this attempt's per-event log. Captured
    /// by the `ReactorObserver` and surfaced in the inspector's
    /// reactor-log pane. No-op when called outside a reactor body
    /// (e.g. from a projector or hand-constructed Ctx).
    pub fn log(&self, level: LogLevel, message: impl Into<String>) {
        self.log_with_data(level, message, None);
    }

    /// Append a log entry with attached structured data.
    pub fn log_with_data(
        &self,
        level: LogLevel,
        message: impl Into<String>,
        data: Option<serde_json::Value>,
    ) {
        if let Some(sink) = self.logs {
            sink.lock().push(LogEntry {
                level,
                message: message.into(),
                data,
                timestamp: Utc::now(),
            });
        }
    }

    /// Read a subject's folded state — `(prev, curr)` around the
    /// current event. `curr` reflects state INCLUDING the current
    /// event; `prev` excludes it.
    ///
    /// In projector bodies this reads the per-consumer registry the
    /// runner folded before invoking you (today's semantics). In
    /// reactor bodies it is a **position-bounded fold-on-read**: the
    /// subject's history from the log, bounded at your trigger's
    /// position — deterministic regardless of how other partitions
    /// interleave, and exclusive over your own subject under the
    /// default `Ordering::PerSubject` (nothing can advance your
    /// subject's history mid-reaction within this consumer).
    ///
    /// The no-arg singleton read (`ctx.aggregate()`) was deleted in the
    /// 0.10 step-1 rename: a magic nil-keyed global was a lying default
    /// (keying invisible at the call site). State keyed to "the whole
    /// system" is still expressible — explicitly, with `Uuid::nil()`.
    ///
    /// # Errors
    /// Log-read failures (fold-on-read path) propagate. An aggregator
    /// registered with a custom `id_fn` (cross-subject fan-in) cannot
    /// be folded from one subject history and errors with a teaching
    /// message in reactor bodies.
    ///
    /// # Panics
    /// Panics if no aggregator for `A` was registered with the engine
    /// via `EngineBuilder::with_aggregators(...)` — a configuration
    /// bug, surfaced loudly at the offending call site.
    pub async fn state_of<A>(&self, id: Uuid) -> Result<AggregateState<A>>
    where
        A: Aggregate,
    {
        match self.state {
            StateSource::None => panic!(
                "ctx.state_of::<{}>(id) called but no aggregators were \
                 registered with EngineBuilder::with_aggregators(...)",
                std::any::type_name::<A>(),
            ),
            StateSource::Registry(reg) => {
                let (prev, curr) = reg.get_transition_arc::<A>(id);
                Ok(AggregateState { prev, curr })
            }
            StateSource::FoldOnRead { registry, log, bound, cache } => {
                let (prev, curr) = crate::aggregator::fold_bounded(
                    registry,
                    log,
                    <A as Aggregate>::NAME,
                    id,
                    bound,
                    cache,
                )
                .await?;
                let downcast = |b: Box<dyn std::any::Any + Send + Sync>| -> Arc<A> {
                    Arc::new(*b.downcast::<A>().expect(
                        "fold_bounded returned a state of the wrong type \
                         (aggregate NAME registered against a different type?)",
                    ))
                };
                Ok(AggregateState { prev: downcast(prev), curr: downcast(curr) })
            }
        }
    }

    /// The reaction-result cache, if the engine was built with one via
    /// `EngineBuilder::with_effect_store`. Most code wants
    /// [`Ctx::effect`] instead — this accessor exists for tooling that
    /// inspects the store directly.
    pub fn effect_store(&self) -> Option<&Arc<dyn crate::effect_store::EffectStore>> {
        self.effect_store
    }

    /// Memoize a side-effecting computation under
    /// `(consumer, trigger event_id, label)`. `compute` runs at most
    /// once per reaction — retry / redelivery returns the cached
    /// result, so the expensive external call (LLM, HTTP, graph)
    /// effectively runs once and the reaction is deterministic by
    /// construction. Projection-store queries (similarity search,
    /// "hottest ungrouped") are effects too: nondeterministic under
    /// redelivery, so they go under the same door.
    ///
    /// ```ignore
    /// let summary: String = ctx.effect("summarize", || async {
    ///     anthropic.summarize(&doc).await   // runs once per reaction
    /// }).await?;
    /// ```
    ///
    /// The label distinguishes multiple effects in one reaction;
    /// reusing one within an invocation errors (domain class), since
    /// the second call would silently replay the first call's result.
    ///
    /// Entries are garbage-collected once the consumer's durable
    /// ack-floor passes the trigger (they exist only to make
    /// redelivery deterministic); parked terminal failures keep theirs
    /// for failure replay.
    ///
    /// The default store is [`InMemoryEffectStore`](crate::effect_store::InMemoryEffectStore),
    /// which handles retries within a single process lifetime. For cross-restart
    /// durability, override via [`EngineBuilder::with_effect_store`].
    pub async fn effect<F, Fut, T>(&self, label: &str, compute: F) -> Result<T>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
        T: serde::Serialize + serde::de::DeserializeOwned,
    {
        let cache = self.effect_store
            .expect(
                "ctx.effect() is only available in reactor bodies — \
                 projectors are side-effect-free by contract \
                 (effect_store is None in projector Ctx)"
            );
        self.claim_label("effect", label)?;
        let key =
            crate::effect_store::EffectKey::new(self.consumer, self.event_id, label);
        crate::effect_store::remember(&**cache, &key, compute).await
    }

    /// Run N side-effecting computations **concurrently**, each
    /// memoized under its own label. All labels are claimed before any
    /// I/O starts — a duplicate label errors immediately. Results are
    /// returned in **input order** regardless of completion order.
    ///
    /// **Prefer `tokio::join!` with individual [`Ctx::effect`] calls** —
    /// it's equally memoized, handles heterogeneous return types, and
    /// reads better at known N:
    ///
    /// ```ignore
    /// let (html, meta) = tokio::join!(
    ///     ctx.effect("fetch_html", || async { scrape_html(&url).await }),
    ///     ctx.effect("fetch_meta", || async { scrape_meta(&url).await }),
    /// );
    /// let (html, meta) = (html?, meta?);
    /// ```
    ///
    /// Use `effect_all` only when N is dynamic (a runtime-length `Vec`
    /// of homogeneous futures). All effects must share the same return
    /// type `T`. The `Box::pin` dance is required to erase the future type:
    ///
    /// ```ignore
    /// let results = ctx.effect_all(vec![
    ///     ("fetch_html", Box::new(|| Box::pin(async { scrape_html(&url).await }))),
    ///     ("fetch_meta", Box::new(|| Box::pin(async { scrape_meta(&url).await }))),
    /// ]).await?;
    /// ```
    pub async fn effect_all<T>(
        &self,
        effects: Vec<(
            &str,
            Box<dyn FnOnce() -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<T>>>>>,
        )>,
    ) -> Result<Vec<T>>
    where
        T: serde::Serialize + serde::de::DeserializeOwned,
    {
        let cache = self.effect_store
            .expect(
                "ctx.effect() is only available in reactor bodies — \
                 projectors are side-effect-free by contract \
                 (effect_store is None in projector Ctx)"
            );
        // Claim all labels atomically upfront — fail fast before I/O.
        for (label, _) in &effects {
            self.claim_label("effect", label)?;
        }
        // Each future owns its own Arc clone and EffectKey so no
        // lifetime constraint links them back to this stack frame.
        let futs: Vec<_> = effects.into_iter()
            .map(|(label, compute)| {
                let store = Arc::clone(cache);
                let key = crate::effect_store::EffectKey::new(
                    self.consumer, self.event_id, label,
                );
                async move {
                    crate::effect_store::remember(
                        store.as_ref(), &key, move || compute(),
                    ).await
                }
            })
            .collect();
        // try_join_all preserves input order.
        futures::future::try_join_all(futs).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_meta() -> Metadata {
        Metadata::new()
    }

    fn test_ctx<'a>(
        meta: &'a Metadata,
        labels: Option<&'a LabelSet>,
        occurred: DateTime<Utc>,
    ) -> Ctx<'a> {
        Ctx {
            event_id:       Uuid::nil(),
            log_position:   LogCursor::ZERO,
            occurred_at:    occurred,
            workflow_id: Uuid::nil(),
            metadata:       meta,
            consumer:       "test.consumer",
            labels,
            state:    StateSource::None,
            logs:           None,
            effect_store: None,
            cancelled_workflows: None,
            fence_consulted: None,
        }
    }

    #[test]
    fn ctx_time_returns_occurred_at_not_wall_clock() {
        let occurred = DateTime::parse_from_rfc3339("2026-01-01T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let meta = fixed_meta();
        let ctx = test_ctx(&meta, None, occurred);
        assert_eq!(ctx.time(), occurred);
    }

    #[test]
    fn metadata_is_readable() {
        let mut meta = fixed_meta();
        meta.insert("_phase".into(), serde_json::json!("pre_migration"));
        meta.insert("_schema_v".into(), serde_json::json!(2));

        let ctx = test_ctx(&meta, None, Utc::now());
        assert_eq!(
            ctx.metadata.get("_phase").and_then(|v| v.as_str()),
            Some("pre_migration"),
        );
        assert_eq!(
            ctx.metadata.get("_schema_v").and_then(|v| v.as_i64()),
            Some(2),
        );
    }

    #[test]
    fn derive_id_is_deterministic_and_label_scoped() {
        let meta = fixed_meta();
        let labels_a = LabelSet::default();
        let ctx_a = test_ctx(&meta, Some(&labels_a), Utc::now());
        let labels_b = LabelSet::default();
        let ctx_b = test_ctx(&meta, Some(&labels_b), Utc::now());

        // Same consumer + trigger + label ⇒ same id across invocations
        // (redelivery mints the same identity).
        let a = ctx_a.derive_id("group").unwrap();
        let b = ctx_b.derive_id("group").unwrap();
        assert_eq!(a, b, "redelivery derives the same identity");

        // Different labels mint different identities.
        let c = ctx_a.derive_id("other").unwrap();
        assert_ne!(a, c);
    }

    #[test]
    fn duplicate_derive_id_label_is_a_domain_error() {
        let meta = fixed_meta();
        let labels = LabelSet::default();
        let ctx = test_ctx(&meta, Some(&labels), Utc::now());
        ctx.derive_id("group").unwrap();
        let err = ctx.derive_id("group").unwrap_err();
        assert_eq!(
            crate::failure::classify(&err),
            Some(crate::failure::ErrorClass::Domain),
            "duplicate label errors with domain class",
        );
        assert!(format!("{err:#}").contains("duplicate"), "the error teaches");
        // Same label under a DIFFERENT kind (effect) is not a duplicate.
        assert!(
            !labels.lock().iter().any(|(k, l)| *k == "effect" && l == "group"),
            "effect:group should not yet be claimed",
        );
        labels.lock().push(("effect", "group".into()));
    }

    #[tokio::test]
    async fn effect_all_runs_concurrently_and_returns_in_order() {
        use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};

        let owner = crate::testing::TestCtx::new().with_effects();
        let ctx = owner.ctx();

        let calls = Arc::new(AtomicUsize::new(0));
        let c1 = calls.clone();
        let c2 = calls.clone();
        let results: Vec<String> = ctx.effect_all(vec![
            ("first",  Box::new(move || Box::pin(async move {
                c1.fetch_add(1, Ordering::SeqCst);
                Ok::<String, anyhow::Error>("hello".into())
            }))),
            ("second", Box::new(move || Box::pin(async move {
                c2.fetch_add(1, Ordering::SeqCst);
                Ok::<String, anyhow::Error>("world".into())
            }))),
        ]).await.unwrap();

        assert_eq!(results[0], "hello");
        assert_eq!(results[1], "world");
        assert_eq!(calls.load(Ordering::SeqCst), 2, "both effects ran");
        assert!(
            owner.labels.lock().iter().any(|(k, l)| *k == "effect" && l == "first"),
            "first label claimed",
        );
        assert!(
            owner.labels.lock().iter().any(|(k, l)| *k == "effect" && l == "second"),
            "second label claimed",
        );
    }

    #[tokio::test]
    async fn effect_all_duplicate_label_errors_before_any_io() {
        use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};

        let owner = crate::testing::TestCtx::new().with_effects();
        let ctx = owner.ctx();

        // Pre-claim "fetch" so effect_all sees a duplicate.
        ctx.effect("fetch", || async { Ok::<String, anyhow::Error>("cached".into()) }).await.unwrap();

        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let err = ctx.effect_all(vec![
            ("fetch",  Box::new(move || Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok::<String, anyhow::Error>("should not run".into())
            }))),
            ("other",  Box::new(|| Box::pin(async {
                Ok::<String, anyhow::Error>("fine".into())
            }))),
        ]).await.unwrap_err();

        assert_eq!(calls.load(Ordering::SeqCst), 0, "compute never ran — error before I/O");
        assert!(format!("{err:#}").contains("duplicate"), "error message is instructive");
    }
}
