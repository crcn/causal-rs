//! `EffectStore` — make side-effecting reactors safe under
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
//! [`EffectKey`] = `(consumer, trigger event_id, label)`. The first
//! execution caches its result; every redelivery returns the **cached**
//! value instead of re-calling the external service. That makes the
//! reactor replayable/deterministic — after which the deterministic
//! [`EffectKey::output_event_id`] lets Kurrent's append-dedup collapse
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

/// Identifies one memoized effect: consumer × trigger × label. The
/// label distinguishes multiple external calls within one reaction
/// (`ctx.effect("ocr", ..)` and `ctx.effect("embed", ..)` memoize
/// independently); duplicate labels in one invocation are a runtime
/// error caught by the runner.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EffectKey {
    /// The reacting consumer — `Reactor::NAME`.
    pub consumer: String,
    /// The triggering event's id (the reaction's `causation_id`).
    pub trigger_event_id: Uuid,
    /// The call-site label passed to `ctx.effect(label, ..)`.
    pub label: String,
}

impl EffectKey {
    pub fn new(
        consumer: impl Into<String>,
        trigger_event_id: Uuid,
        label: impl Into<String>,
    ) -> Self {
        Self { consumer: consumer.into(), trigger_event_id, label: label.into() }
    }

    /// Deterministic event_id for an emitted output of this reaction,
    /// identity-keyed (kind + subject + nth-of-that-pair). Stable
    /// across retries/restarts AND across deploys that reorder or
    /// insert outputs, so the log collapses duplicate emits on
    /// redelivery. Shares the exact derivation the reactor runner uses
    /// ([`derive_output_event_id`]).
    pub fn output_event_id(&self, kind: &str, subject_id: Uuid, nth: u32) -> Uuid {
        derive_output_event_id(&self.consumer, self.trigger_event_id, kind, subject_id, nth)
    }
}

/// Durable memo of a reactor's side-effecting result, keyed by
/// [`EffectKey`]. See module docs.
#[async_trait]
pub trait EffectStore: Send + Sync {
    /// Cached result for `key`, if a prior execution stored one.
    async fn get(&self, key: &EffectKey) -> Result<Option<serde_json::Value>>;

    /// Store `value` for `key`, **first-write-wins**: an existing entry
    /// MUST NOT be overwritten (a redelivery that raced past `get` must
    /// not clobber the canonical result). Returns the value now in the
    /// cache — the pre-existing one if present, else `value`.
    async fn put(
        &self,
        key: &EffectKey,
        value: serde_json::Value,
    ) -> Result<serde_json::Value>;

    /// Delete the entry for `key` (idempotent — absent is fine).
    ///
    /// Called by the runner's floor-GC: an effect entry exists only to
    /// make *redelivery* deterministic, so once the durable ack-floor
    /// has passed its trigger the entry is dead — without this the
    /// cache grows to the size of the log. Triggers that PARK as
    /// terminal failures are exempted by the runner (their entries
    /// must survive for failure replay).
    async fn remove(&self, key: &EffectKey) -> Result<()>;
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
    key: &EffectKey,
    compute: Compute,
) -> Result<T>
where
    Cache: EffectStore + ?Sized,
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

/// In-memory [`EffectStore`] for tests, examples, and single-process
/// use. No durability across restarts.
#[derive(Default)]
pub struct InMemoryEffectStore {
    inner: Mutex<HashMap<EffectKey, serde_json::Value>>,
}

impl InMemoryEffectStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Scan all entries whose trigger matches `trigger_event_id`.
    ///
    /// Not on the `EffectStore` trait — this is a memory-layer-only
    /// operation used by `MemoryInspectorReadModel::effects_for_event`.
    pub fn scan_by_trigger(&self, trigger_event_id: Uuid) -> Vec<(EffectKey, serde_json::Value)> {
        self.inner
            .lock()
            .unwrap()
            .iter()
            .filter(|(k, _)| k.trigger_event_id == trigger_event_id)
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }
}

#[async_trait]
impl EffectStore for InMemoryEffectStore {
    async fn get(&self, key: &EffectKey) -> Result<Option<serde_json::Value>> {
        Ok(self.inner.lock().unwrap().get(key).cloned())
    }

    async fn put(
        &self,
        key: &EffectKey,
        value: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let mut map = self.inner.lock().unwrap();
        let canonical = map.entry(key.clone()).or_insert(value).clone();
        Ok(canonical)
    }

    async fn remove(&self, key: &EffectKey) -> Result<()> {
        self.inner.lock().unwrap().remove(key);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    #[test]
    fn output_event_id_is_deterministic_and_matches_runner() {
        let key = EffectKey::new("welcome_reactor", Uuid::nil(), "main");
        // Stable across calls.
        assert_eq!(key.output_event_id("k", Uuid::nil(), 0), key.output_event_id("k", Uuid::nil(), 0));
        // Distinct per index.
        assert_ne!(key.output_event_id("k", Uuid::nil(), 0), key.output_event_id("k", Uuid::nil(), 1));
        // Matches the reactor runner's derivation exactly.
        assert_eq!(
            key.output_event_id("welcome_queued", Uuid::nil(), 3),
            derive_output_event_id("welcome_reactor", Uuid::nil(), "welcome_queued", Uuid::nil(), 3),
        );
    }

    #[tokio::test]
    async fn remember_computes_once_then_replays() {
        let cache = InMemoryEffectStore::new();
        let key = EffectKey::new("r", Uuid::new_v4(), "call");
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
        let cache = InMemoryEffectStore::new();
        let key = EffectKey::new("r", Uuid::new_v4(), "call");

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
