//! Single `Ctx` type passed to all consumer bodies. Three separate
//! context types would add no safety beyond what the runtime already
//! enforces.
//!
//! Critical property — no wall-clock accessor. `ctx.now()` returns
//! the fact's logical `occurred_at`, set at emit by the producer.
//! Replay reproduces byte-identical state because deterministic time
//! is the only reachable time. Migration-boundary tagging belongs in
//! `ctx.metadata`, not a dedicated enum.

use std::sync::Arc;

use anyhow::Result;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use uuid::Uuid;

use crate::aggregate::Aggregate;
use crate::aggregator::{AggregatorRegistry, FoldOnReadCache};
use crate::types::{LogCursor, LogEntry, LogLevel};

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
/// Deliberately absent: any wall-clock accessor. `ctx.now()` returns
/// `occurred_at`, never the system clock.
#[derive(Clone, Copy)]
pub struct Ctx<'a> {
    pub event_id:       Uuid,
    pub log_position:   LogCursor,
    pub occurred_at:    DateTime<Utc>,
    pub workflow_id: Uuid,
    pub metadata:       &'a Metadata,
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
    /// The fact's logical occurrence time. Consumers that need a
    /// timestamp use this — never wall-clock — so replay reproduces
    /// state byte-identically.
    #[inline]
    pub fn now(&self) -> DateTime<Utc> { self.occurred_at }

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
    /// `EngineBuilder::with_effect_store`. Combine with
    /// [`Ctx::effect_key`] + [`crate::remember`] to make a
    /// side-effecting reactor idempotent under redelivery / retry:
    ///
    /// ```ignore
    /// let key = ctx.effect_key(Self::NAME);
    /// let out = causal::remember(ctx.effect_store().unwrap(), &key, || async {
    ///     expensive_external_call().await   // runs once per reaction
    /// }).await?;
    /// ```
    pub fn effect_store(&self) -> Option<&Arc<dyn crate::effect_store::EffectStore>> {
        self.effect_store
    }

    /// Build the [`EffectKey`](crate::effect_store::EffectKey) for
    /// this reaction — `(group, this trigger's event_id)`. Pass your
    /// `Reactor::NAME`.
    pub fn effect_key(&self, group: &str) -> crate::effect_store::EffectKey {
        crate::effect_store::EffectKey::new(group, self.event_id)
    }

    /// Memoize a side-effecting computation under this reaction's key.
    /// `compute` runs at most once per reaction — retry / redelivery
    /// returns the cached result, so the expensive external call (LLM,
    /// HTTP, graph) effectively runs once. Pass your `Reactor::NAME`.
    ///
    /// ```ignore
    /// let summary: String = ctx.remember(Self::NAME, || async {
    ///     anthropic.summarize(&doc).await   // runs once per reaction
    /// }).await?;
    /// ```
    ///
    /// Errors if no cache was configured
    /// (`EngineBuilder::with_effect_store`).
    pub async fn remember<F, Fut, T>(&self, group: &str, compute: F) -> Result<T>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
        T: serde::Serialize + serde::de::DeserializeOwned,
    {
        let cache = self.effect_store.ok_or_else(|| {
            anyhow::anyhow!(
                "ctx.remember called but no EffectStore was configured \
                 (EngineBuilder::with_effect_store)"
            )
        })?;
        let key = crate::effect_store::EffectKey::new(group, self.event_id);
        crate::effect_store::remember(&**cache, &key, compute).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_meta() -> Metadata {
        Metadata::new()
    }

    #[test]
    fn ctx_now_returns_occurred_at_not_wall_clock() {
        let occurred = DateTime::parse_from_rfc3339("2026-01-01T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let meta = fixed_meta();
        let ctx = Ctx {
            event_id:       Uuid::nil(),
            log_position:   LogCursor::ZERO,
            occurred_at:    occurred,
            workflow_id: Uuid::nil(),
            metadata:       &meta,
            state:    crate::contexts::StateSource::None,
            logs:           None,
            effect_store: None,
        };
        assert_eq!(ctx.now(), occurred);
    }

    #[test]
    fn metadata_is_readable() {
        let mut meta = fixed_meta();
        meta.insert("_phase".into(), serde_json::json!("pre_migration"));
        meta.insert("_schema_v".into(), serde_json::json!(2));

        let ctx = Ctx {
            event_id:       Uuid::nil(),
            log_position:   LogCursor::ZERO,
            occurred_at:    Utc::now(),
            workflow_id: Uuid::nil(),
            metadata:       &meta,
            state:    crate::contexts::StateSource::None,
            logs:           None,
            effect_store: None,
        };
        assert_eq!(
            ctx.metadata.get("_phase").and_then(|v| v.as_str()),
            Some("pre_migration"),
        );
        assert_eq!(
            ctx.metadata.get("_schema_v").and_then(|v| v.as_i64()),
            Some(2),
        );
    }
}
