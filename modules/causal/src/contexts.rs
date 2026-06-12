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
use crate::aggregator::AggregatorRegistry;
use crate::types::{LogCursor, LogEntry, LogLevel};

/// Aggregate state snapshot pair returned by [`Ctx::aggregate`] and
/// [`Ctx::aggregate_of`]. `prev` is the state before the current
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
    pub correlation_id: Uuid,
    pub metadata:       &'a Metadata,
    /// Optional read-side access to in-process aggregator state folded
    /// from events. `None` if the runner wasn't configured with an
    /// aggregator registry. Use [`Ctx::aggregate`] to query.
    pub(crate) aggregators: Option<&'a Arc<AggregatorRegistry>>,
    /// Per-attempt sink for `ctx.log(...)` entries. The runner owns
    /// the underlying Vec and drains it after `react()` returns,
    /// routing the entries through the configured
    /// [`ReactorObserver`](crate::reactor_observer::ReactorObserver).
    /// `None` when not running inside a reactor body (engine emit
    /// path, projector body, tests that construct Ctx by hand).
    pub(crate) logs: Option<&'a Mutex<Vec<LogEntry>>>,
    /// Optional reaction-result cache (Phase 4). Lets a side-effecting
    /// reactor memoize its external call under its [`ReactionKey`] so
    /// redelivery / retry runs the call effectively once. `None` unless
    /// the engine was built with `EngineBuilder::with_reaction_cache`.
    pub(crate) reaction_cache:
        Option<&'a Arc<dyn crate::reaction_cache::ReactionCache>>,
}

impl<'a> std::fmt::Debug for Ctx<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ctx")
            .field("event_id", &self.event_id)
            .field("log_position", &self.log_position)
            .field("occurred_at", &self.occurred_at)
            .field("correlation_id", &self.correlation_id)
            .field("metadata", &self.metadata)
            .field("has_aggregators", &self.aggregators.is_some())
            .field("has_reaction_cache", &self.reaction_cache.is_some())
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

    /// Read singleton aggregate state — `(prev, curr)` snapshots
    /// captured by the runner around folding the current event.
    /// `curr` reflects state INCLUDING the current event because the
    /// runner folds before invoking the consumer body.
    ///
    /// Used for incrementally-built read-only state shared across
    /// reactors / projectors (saga-style PipelineState pattern).
    ///
    /// # Panics
    /// Panics if no aggregators were registered with the engine via
    /// `EngineBuilder::with_aggregators(...)`. Calling `aggregate()`
    /// in a body that has no aggregator wiring is a configuration bug
    /// — the panic surfaces it loudly at the offending call site.
    pub fn aggregate<A>(&self) -> AggregateState<A>
    where
        A: Aggregate,
    {
        let reg = self.aggregators.expect(
            "ctx.aggregate::<A>() called but no aggregators were registered \
             with EngineBuilder::with_aggregators(...)",
        );
        let (prev, curr) = reg.get_singleton_arc::<A>();
        AggregateState { prev, curr }
    }

    /// Read aggregate state for a specific aggregate id (non-singleton
    /// aggregates). Same semantics as [`Self::aggregate`] but takes an
    /// id parameter.
    ///
    /// # Panics
    /// Panics if no aggregators were registered. See [`Self::aggregate`].
    pub fn aggregate_of<A>(&self, id: Uuid) -> AggregateState<A>
    where
        A: Aggregate,
    {
        let reg = self.aggregators.expect(
            "ctx.aggregate_of::<A>(id) called but no aggregators were registered \
             with EngineBuilder::with_aggregators(...)",
        );
        let (prev, curr) = reg.get_transition_arc::<A>(id);
        AggregateState { prev, curr }
    }

    /// The reaction-result cache, if the engine was built with one via
    /// `EngineBuilder::with_reaction_cache`. Combine with
    /// [`Ctx::reaction_key`] + [`crate::remember`] to make a
    /// side-effecting reactor idempotent under redelivery / retry:
    ///
    /// ```ignore
    /// let key = ctx.reaction_key(Self::NAME);
    /// let out = causal::remember(ctx.reaction_cache().unwrap(), &key, || async {
    ///     expensive_external_call().await   // runs once per reaction
    /// }).await?;
    /// ```
    pub fn reaction_cache(&self) -> Option<&Arc<dyn crate::reaction_cache::ReactionCache>> {
        self.reaction_cache
    }

    /// Build the [`ReactionKey`](crate::reaction_cache::ReactionKey) for
    /// this reaction — `(group, this trigger's event_id)`. Pass your
    /// `Reactor::NAME`.
    pub fn reaction_key(&self, group: &str) -> crate::reaction_cache::ReactionKey {
        crate::reaction_cache::ReactionKey::new(group, self.event_id)
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
    /// (`EngineBuilder::with_reaction_cache`).
    pub async fn remember<F, Fut, T>(&self, group: &str, compute: F) -> Result<T>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
        T: serde::Serialize + serde::de::DeserializeOwned,
    {
        let cache = self.reaction_cache.ok_or_else(|| {
            anyhow::anyhow!(
                "ctx.remember called but no ReactionCache was configured \
                 (EngineBuilder::with_reaction_cache)"
            )
        })?;
        let key = crate::reaction_cache::ReactionKey::new(group, self.event_id);
        crate::reaction_cache::remember(&**cache, &key, compute).await
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
            correlation_id: Uuid::nil(),
            metadata:       &meta,
            aggregators:    None,
            logs:           None,
            reaction_cache: None,
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
            correlation_id: Uuid::nil(),
            metadata:       &meta,
            aggregators:    None,
            logs:           None,
            reaction_cache: None,
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
