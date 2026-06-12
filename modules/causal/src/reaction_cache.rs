//! `ReactionCache` — make side-effecting reactors safe under
//! at-least-once delivery.
//!
//! Reactors are at-least-once catch-up consumers, so the same trigger can
//! be **redelivered** — a crash between a reactor's output append and its
//! cursor advance re-runs `react()` on restart. A reactor that calls an
//! external service (LLM,
//! HTTP, graph) and emits events from the *result* is non-deterministic:
//! re-running it produces different output, so the log can't dedup it.
//!
//! The fix is to memoize the reaction's result under its
//! [`ReactionKey`] = `(reactor group, trigger event_id)`. The first
//! execution caches its result; every redelivery returns the **cached**
//! value instead of re-calling the external service. That makes the
//! reactor replayable/deterministic — after which the deterministic
//! [`ReactionKey::output_event_id`] lets Kurrent's append-dedup collapse
//! duplicate emits. No separate inbox/outbox: the event store is the
//! ledger; this cache only guards the *side effect*.
//!
//! This is the one coordination primitive the reactor model needs (see
//! `docs/plans/2026-06-07-kurrent-native-consolidation.md`, Decision 4).
//! `causal` owns the trait; backends/apps supply the impl (PG / Redis /
//! the in-memory one below).
//!
//! Exactly-once *external effect* remains impossible — a crash between
//! the external call and caching its result re-runs the call. That
//! window is benign for read-style calls and should be closed with
//! idempotent sinks (e.g. graph `MERGE`) for write-style ones.

use anyhow::Result;
use async_trait::async_trait;
use serde::{de::DeserializeOwned, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;
use uuid::Uuid;

use crate::reactor_runner::derive_output_event_id;

/// Identifies one reaction: reactor `group` processing one trigger
/// event. The unit of idempotency for the reactor path.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ReactionKey {
    /// The reacting consumer group — `Reactor::NAME`.
    pub group: String,
    /// The triggering event's id (the reaction's `causation_id`).
    pub trigger_event_id: Uuid,
}

impl ReactionKey {
    pub fn new(group: impl Into<String>, trigger_event_id: Uuid) -> Self {
        Self { group: group.into(), trigger_event_id }
    }

    /// Deterministic event_id for the `idx`-th emitted output of this
    /// reaction. Stable across retries/restarts, so the log collapses
    /// duplicate emits on redelivery. Shares the exact derivation the
    /// reactor runner uses ([`derive_output_event_id`]).
    pub fn output_event_id(&self, idx: u32) -> Uuid {
        derive_output_event_id(&self.group, self.trigger_event_id, idx)
    }
}

/// Durable memo of a reactor's side-effecting result, keyed by
/// [`ReactionKey`]. See module docs.
#[async_trait]
pub trait ReactionCache: Send + Sync {
    /// Cached result for `key`, if a prior execution stored one.
    async fn get(&self, key: &ReactionKey) -> Result<Option<serde_json::Value>>;

    /// Store `value` for `key`, **first-write-wins**: an existing entry
    /// MUST NOT be overwritten (a redelivery that raced past `get` must
    /// not clobber the canonical result). Returns the value now in the
    /// cache — the pre-existing one if present, else `value`.
    async fn put(
        &self,
        key: &ReactionKey,
        value: serde_json::Value,
    ) -> Result<serde_json::Value>;
}

/// Run `compute` only if `key` has no cached result; otherwise return
/// the cached value. The canonical pattern for a side-effecting reactor:
///
/// ```ignore
/// let result: FetchResult = remember(cache, &key, || async {
///     http_fetch(&url).await           // expensive, runs once per reaction
/// }).await?;
/// ```
pub async fn remember<Cache, Compute, Fut, T>(
    cache: &Cache,
    key: &ReactionKey,
    compute: Compute,
) -> Result<T>
where
    Cache: ReactionCache + ?Sized,
    Compute: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
    T: Serialize + DeserializeOwned,
{
    if let Some(cached) = cache.get(key).await? {
        return Ok(serde_json::from_value(cached)?);
    }
    let result = compute().await?;
    // First-write-wins: if a concurrent reaction stored first, adopt
    // its value so both reactions agree on the canonical result.
    let canonical = cache.put(key, serde_json::to_value(&result)?).await?;
    Ok(serde_json::from_value(canonical)?)
}

/// In-memory [`ReactionCache`] for tests, examples, and single-process
/// use. No durability across restarts.
#[derive(Default)]
pub struct InMemoryReactionCache {
    inner: Mutex<HashMap<ReactionKey, serde_json::Value>>,
}

impl InMemoryReactionCache {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl ReactionCache for InMemoryReactionCache {
    async fn get(&self, key: &ReactionKey) -> Result<Option<serde_json::Value>> {
        Ok(self.inner.lock().unwrap().get(key).cloned())
    }

    async fn put(
        &self,
        key: &ReactionKey,
        value: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let mut map = self.inner.lock().unwrap();
        let canonical = map.entry(key.clone()).or_insert(value).clone();
        Ok(canonical)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    #[test]
    fn output_event_id_is_deterministic_and_matches_runner() {
        let key = ReactionKey::new("welcome_reactor", Uuid::nil());
        // Stable across calls.
        assert_eq!(key.output_event_id(0), key.output_event_id(0));
        // Distinct per index.
        assert_ne!(key.output_event_id(0), key.output_event_id(1));
        // Matches the reactor runner's derivation exactly.
        assert_eq!(
            key.output_event_id(3),
            derive_output_event_id("welcome_reactor", Uuid::nil(), 3),
        );
    }

    #[tokio::test]
    async fn remember_computes_once_then_replays() {
        let cache = InMemoryReactionCache::new();
        let key = ReactionKey::new("r", Uuid::new_v4());
        let calls = Arc::new(AtomicU32::new(0));

        let c1 = calls.clone();
        let first: i64 = remember(&cache, &key, || async move {
            c1.fetch_add(1, Ordering::SeqCst);
            Ok(42)
        })
        .await
        .unwrap();
        assert_eq!(first, 42);

        // Second call with a DIFFERENT compute returns the cached value
        // and never runs compute again.
        let c2 = calls.clone();
        let second: i64 = remember(&cache, &key, || async move {
            c2.fetch_add(1, Ordering::SeqCst);
            Ok(99)
        })
        .await
        .unwrap();
        assert_eq!(second, 42, "redelivery replays the cached result");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "external call ran exactly once");
    }

    #[tokio::test]
    async fn put_is_first_write_wins() {
        let cache = InMemoryReactionCache::new();
        let key = ReactionKey::new("r", Uuid::new_v4());

        let a = cache.put(&key, serde_json::json!("first")).await.unwrap();
        assert_eq!(a, serde_json::json!("first"));

        // A racing second writer adopts the canonical (first) value.
        let b = cache.put(&key, serde_json::json!("second")).await.unwrap();
        assert_eq!(b, serde_json::json!("first"), "first write wins");

        assert_eq!(
            cache.get(&key).await.unwrap(),
            Some(serde_json::json!("first")),
        );
    }
}
