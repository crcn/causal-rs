//! Handler context and related types.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use anyhow::Result;
use uuid::Uuid;

use parking_lot::Mutex;

use crate::aggregator::AggregatorRegistry;
use crate::store::Store;
use crate::types::{JournalEntry, LogEntry, LogLevel};

/// Journaling state for `ctx.run()` replay.
pub(crate) struct JournalState {
    store: Arc<dyn Store>,
    handler_id: String,
    event_id: Uuid,
    /// Preloaded journal entries keyed by sequence number.
    entries: HashMap<u32, serde_json::Value>,
    /// Next sequence number to assign.
    next_seq: AtomicU32,
}

/// Structured logger for capturing log entries during handler execution.
///
/// Entries are drained by the engine after execution and attached to the
/// `HandlerCompletion` for Store implementations to persist.
#[derive(Clone)]
pub struct Logger {
    entries: Arc<Mutex<Vec<LogEntry>>>,
}

impl Logger {
    fn new() -> Self {
        Self {
            entries: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Log a debug message.
    pub fn debug(&self, message: impl Into<String>) {
        self.push(LogLevel::Debug, message.into(), None);
    }

    /// Log a debug message with structured data.
    pub fn debug_with(&self, message: impl Into<String>, data: &impl serde::Serialize) {
        self.push(LogLevel::Debug, message.into(), to_value_opt(data));
    }

    /// Log an info message.
    pub fn info(&self, message: impl Into<String>) {
        self.push(LogLevel::Info, message.into(), None);
    }

    /// Log an info message with structured data.
    pub fn info_with(&self, message: impl Into<String>, data: &impl serde::Serialize) {
        self.push(LogLevel::Info, message.into(), to_value_opt(data));
    }

    /// Log a warning message.
    pub fn warn(&self, message: impl Into<String>) {
        self.push(LogLevel::Warn, message.into(), None);
    }

    /// Log a warning message with structured data.
    pub fn warn_with(&self, message: impl Into<String>, data: &impl serde::Serialize) {
        self.push(LogLevel::Warn, message.into(), to_value_opt(data));
    }

    fn push(&self, level: LogLevel, message: String, data: Option<serde_json::Value>) {
        self.entries.lock().push(LogEntry {
            level,
            message,
            data,
            timestamp: chrono::Utc::now(),
        });
    }

    /// Drain all captured entries (called by the engine after execution).
    pub(crate) fn drain(&self) -> Vec<LogEntry> {
        std::mem::take(&mut *self.entries.lock())
    }
}

fn to_value_opt(data: &impl serde::Serialize) -> Option<serde_json::Value> {
    serde_json::to_value(data).ok()
}

/// Context passed to handlers.
pub struct Context<D>
where
    D: Send + Sync + 'static,
{
    /// Human-readable identifier of the handler being executed.
    pub handler_id: String,
    /// Deterministic idempotency key for external API calls.
    pub idempotency_key: String,
    /// Correlation ID from event envelope - groups related events together.
    pub correlation_id: Uuid,
    /// Current event's unique ID from envelope.
    pub event_id: Uuid,
    /// Parent event ID for causal tracking.
    pub parent_event_id: Option<Uuid>,
    pub(crate) deps: Arc<D>,
    /// Structured logger — entries are drained into `HandlerCompletion` after execution.
    pub logger: Logger,
    /// Aggregator registry for transition guard replay.
    pub(crate) aggregator_registry: Option<Arc<AggregatorRegistry>>,
    /// Journal state for `ctx.run()` replay (None = passthrough).
    pub(crate) journal: Option<Arc<JournalState>>,
}

impl<D> Clone for Context<D>
where
    D: Send + Sync + 'static,
{
    fn clone(&self) -> Self {
        Self {
            handler_id: self.handler_id.clone(),
            idempotency_key: self.idempotency_key.clone(),
            correlation_id: self.correlation_id,
            event_id: self.event_id,
            parent_event_id: self.parent_event_id,
            deps: self.deps.clone(),
            logger: self.logger.clone(),
            aggregator_registry: self.aggregator_registry.clone(),
            journal: self.journal.clone(),
        }
    }
}

impl<D> Context<D>
where
    D: Send + Sync + 'static,
{
    pub(crate) fn new(
        handler_id: String,
        idempotency_key: String,
        correlation_id: Uuid,
        event_id: Uuid,
        parent_event_id: Option<Uuid>,
        deps: Arc<D>,
    ) -> Self {
        Self {
            handler_id,
            idempotency_key,
            correlation_id,
            event_id,
            parent_event_id,
            deps,
            logger: Logger::new(),
            aggregator_registry: None,
            journal: None,
        }
    }

    /// Attach journal state for `ctx.run()` replay.
    pub(crate) fn with_journal(
        mut self,
        store: Arc<dyn Store>,
        entries: Vec<JournalEntry>,
    ) -> Self {
        let map = entries.into_iter().map(|e| (e.seq, e.value)).collect();
        self.journal = Some(Arc::new(JournalState {
            store,
            handler_id: self.handler_id.clone(),
            event_id: self.event_id,
            entries: map,
            next_seq: AtomicU32::new(0),
        }));
        self
    }

    /// Attach an aggregator registry (used by the engine for transition guards).
    pub(crate) fn with_aggregator_registry(
        mut self,
        registry: Arc<AggregatorRegistry>,
    ) -> Self {
        self.aggregator_registry = Some(registry);
        self
    }

    /// Get the aggregator registry (if set).
    pub fn aggregator_registry(&self) -> Option<&AggregatorRegistry> {
        self.aggregator_registry.as_deref()
    }

    /// Get the (prev, next) transition for an aggregate by ID.
    ///
    /// Returns `(Arc::new(A::default()), Arc::new(A::default()))` if no state exists.
    pub fn aggregate<A>(&self, id: Uuid) -> (Arc<A>, Arc<A>)
    where
        A: crate::Aggregate + 'static,
    {
        self.aggregator_registry
            .as_ref()
            .expect("aggregate() requires an aggregator registry")
            .get_transition_arc::<A>(id)
    }

    /// Get the (prev, next) transition for a singleton aggregate.
    ///
    /// Returns `(Arc::new(A::default()), Arc::new(A::default()))` if no state exists.
    pub fn singleton<A>(&self) -> (Arc<A>, Arc<A>)
    where
        A: crate::Aggregate + 'static,
    {
        self.aggregator_registry
            .as_ref()
            .expect("singleton() requires an aggregator registry")
            .get_singleton_arc::<A>()
    }

    /// Get the handler ID (human-readable identifier).
    pub fn handler_id(&self) -> &str {
        &self.handler_id
    }

    /// Get the idempotency key for external API calls.
    pub fn idempotency_key(&self) -> &str {
        &self.idempotency_key
    }

    /// Get shared dependencies.
    pub fn deps(&self) -> &D {
        &self.deps
    }

    /// Get the current event ID for causation tracking.
    pub fn current_event_id(&self) -> Uuid {
        self.event_id
    }

    /// Get the parent event ID for causal tracking.
    pub fn parent_event_id(&self) -> Option<Uuid> {
        self.parent_event_id
    }

    /// Execute a side-effect closure with journaling.
    ///
    /// When a journal is wired (durable store), the result is persisted
    /// so that retried effects can skip already-completed steps. Without
    /// a journal the closure executes directly (passthrough).
    ///
    /// ```rust,ignore
    /// let tracking_id: String = ctx.run(|| async {
    ///     ctx.deps().shipping_api.ship(order_id).await
    /// }).await?;
    /// ```
    pub async fn run<F, Fut, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Result<T>> + Send + 'static,
        T: serde::Serialize + serde::de::DeserializeOwned + Send + 'static,
    {
        let Some(journal) = &self.journal else {
            return f().await;
        };

        let seq = journal.next_seq.fetch_add(1, Ordering::SeqCst);

        // Replay from journal if entry exists at this sequence
        if let Some(value) = journal.entries.get(&seq) {
            return Ok(serde_json::from_value(value.clone())?);
        }

        // Execute + persist. Note: if append_journal fails after the closure
        // succeeds, the side effect happened but isn't journaled. On retry the
        // closure will re-execute. Use idempotency keys for external APIs.
        let result = f().await?;
        journal
            .store
            .append_journal(
                &journal.handler_id,
                journal.event_id,
                seq,
                serde_json::to_value(&result)?,
            )
            .await?;

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicI32;

    use crate::memory_store::MemoryStore;

    #[derive(Clone, Debug, Default)]
    struct TestDeps {
        multiplier: i32,
    }

    fn create_test_context() -> Context<TestDeps> {
        let deps = Arc::new(TestDeps { multiplier: 2 });
        Context::new(
            "test_effect".to_string(),
            "test_idempotency_key".to_string(),
            Uuid::nil(),
            Uuid::nil(),
            None,
            deps,
        )
    }

    fn create_journaled_context(entries: Vec<JournalEntry>) -> Context<TestDeps> {
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        create_test_context().with_journal(store, entries)
    }

    #[tokio::test]
    async fn test_effect_context_accessors() {
        let context = create_test_context();

        assert_eq!(context.handler_id(), "test_effect");
        assert_eq!(context.idempotency_key(), "test_idempotency_key");
        assert_eq!(context.deps().multiplier, 2);
    }

    #[tokio::test]
    async fn test_effect_context_clone() {
        let context = create_test_context();
        let cloned = context.clone();

        assert_eq!(cloned.handler_id(), "test_effect");
        assert_eq!(cloned.deps().multiplier, 2);
    }

    #[tokio::test]
    async fn ctx_run_executes_closure_and_returns_result() {
        let ctx = create_test_context();
        let result: String = ctx.run(|| async { Ok("hello".to_string()) }).await.unwrap();
        assert_eq!(result, "hello");
    }

    #[tokio::test]
    async fn ctx_run_propagates_errors() {
        let ctx = create_test_context();
        let result: Result<String> = ctx.run(|| async { Err(anyhow::anyhow!("boom")) }).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn ctx_run_unit_return() {
        let ctx = create_test_context();
        ctx.run(|| async { Ok(()) }).await.unwrap();
    }

    #[tokio::test]
    async fn ctx_run_multiple_calls_return_independent_results() {
        let ctx = create_test_context();
        let a: i32 = ctx.run(|| async { Ok(1) }).await.unwrap();
        let b: String = ctx.run(|| async { Ok("two".into()) }).await.unwrap();
        assert_eq!(a, 1);
        assert_eq!(b, "two");
    }

    // -- Journal replay tests --

    #[tokio::test]
    async fn journal_replays_single_entry_without_executing() {
        let call_count = Arc::new(AtomicI32::new(0));
        let cc = call_count.clone();

        let ctx = create_journaled_context(vec![JournalEntry {
            seq: 0,
            value: serde_json::json!("cached"),
        }]);

        let result: String = ctx
            .run(move || {
                cc.fetch_add(1, Ordering::SeqCst);
                async { Ok("should not run".to_string()) }
            })
            .await
            .unwrap();

        assert_eq!(result, "cached");
        assert_eq!(call_count.load(Ordering::SeqCst), 0, "closure should not execute on replay");
    }

    #[tokio::test]
    async fn journal_replays_multiple_entries_in_sequence() {
        let ctx = create_journaled_context(vec![
            JournalEntry { seq: 0, value: serde_json::json!(10) },
            JournalEntry { seq: 1, value: serde_json::json!("two") },
        ]);

        let a: i32 = ctx.run(|| async { Ok(999) }).await.unwrap();
        let b: String = ctx.run(|| async { Ok("nope".into()) }).await.unwrap();

        assert_eq!(a, 10);
        assert_eq!(b, "two");
    }

    #[tokio::test]
    async fn journal_replays_then_executes_fresh_after_journal_exhausted() {
        let call_count = Arc::new(AtomicI32::new(0));
        let cc = call_count.clone();

        let ctx = create_journaled_context(vec![JournalEntry {
            seq: 0,
            value: serde_json::json!("cached"),
        }]);

        // First call replays
        let a: String = ctx.run(|| async { Ok("nope".into()) }).await.unwrap();
        assert_eq!(a, "cached");

        // Second call executes fresh (no journal entry at seq=1)
        let b: String = ctx
            .run(move || {
                cc.fetch_add(1, Ordering::SeqCst);
                async { Ok("fresh".into()) }
            })
            .await
            .unwrap();

        assert_eq!(b, "fresh");
        assert_eq!(call_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn journal_persists_new_entries_to_store() {
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        let ctx = create_test_context().with_journal(store.clone(), vec![]);

        let _: String = ctx.run(|| async { Ok("first".into()) }).await.unwrap();
        let _: i32 = ctx.run(|| async { Ok(42) }).await.unwrap();

        let entries = store.load_journal("test_effect", Uuid::nil()).await.unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].seq, 0);
        assert_eq!(entries[0].value, serde_json::json!("first"));
        assert_eq!(entries[1].seq, 1);
        assert_eq!(entries[1].value, serde_json::json!(42));
    }

    #[tokio::test]
    async fn journal_sorts_unsorted_entries_on_load() {
        // Entries arrive out of order — with_journal should sort them
        let ctx = create_journaled_context(vec![
            JournalEntry { seq: 1, value: serde_json::json!("second") },
            JournalEntry { seq: 0, value: serde_json::json!("first") },
        ]);

        let a: String = ctx.run(|| async { Ok("nope".into()) }).await.unwrap();
        let b: String = ctx.run(|| async { Ok("nope".into()) }).await.unwrap();

        assert_eq!(a, "first");
        assert_eq!(b, "second");
    }

    #[tokio::test]
    async fn journal_error_not_journaled_allows_retry() {
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        let ctx = create_test_context().with_journal(store.clone(), vec![]);

        // First run succeeds
        let _: String = ctx.run(|| async { Ok("ok".into()) }).await.unwrap();

        // Second run fails — error should NOT be journaled
        let result: Result<String> = ctx.run(|| async { Err(anyhow::anyhow!("boom")) }).await;
        assert!(result.is_err());

        // Only the successful entry should be in the journal
        let entries = store.load_journal("test_effect", Uuid::nil()).await.unwrap();
        assert_eq!(entries.len(), 1, "only successful run() should be journaled");
    }

    // -- Adversarial / edge case tests --

    #[tokio::test]
    async fn journal_type_mismatch_on_replay_returns_error() {
        // Journal has a string, but we try to deserialize as i32
        let ctx = create_journaled_context(vec![JournalEntry {
            seq: 0,
            value: serde_json::json!("not_a_number"),
        }]);

        let result: Result<i32> = ctx.run(|| async { Ok(42) }).await;
        assert!(result.is_err(), "type mismatch should produce a deserialization error");
    }

    #[tokio::test]
    async fn journal_replay_with_complex_struct() {
        #[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
        struct Shipment {
            tracking_id: String,
            items: Vec<String>,
            weight_kg: f64,
        }

        let original = Shipment {
            tracking_id: "TRK-123".into(),
            items: vec!["widget".into(), "gadget".into()],
            weight_kg: 2.5,
        };

        let ctx = create_journaled_context(vec![JournalEntry {
            seq: 0,
            value: serde_json::to_value(&original).unwrap(),
        }]);

        let replayed: Shipment = ctx.run(|| async { panic!("should not execute") }).await.unwrap();
        assert_eq!(replayed, original);
    }

    #[tokio::test]
    async fn journal_replay_with_option_none() {
        let ctx = create_journaled_context(vec![JournalEntry {
            seq: 0,
            value: serde_json::json!(null),
        }]);

        let result: Option<String> = ctx.run(|| async { Ok(Some("nope".into())) }).await.unwrap();
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn journal_replay_with_option_some() {
        let ctx = create_journaled_context(vec![JournalEntry {
            seq: 0,
            value: serde_json::json!("hello"),
        }]);

        let result: Option<String> = ctx.run(|| async { Ok(None) }).await.unwrap();
        assert_eq!(result, Some("hello".into()));
    }

    #[tokio::test]
    async fn journal_replay_unit_type() {
        let call_count = Arc::new(AtomicI32::new(0));
        let cc = call_count.clone();

        let ctx = create_journaled_context(vec![JournalEntry {
            seq: 0,
            value: serde_json::json!(null),
        }]);

        ctx.run(move || {
            cc.fetch_add(1, Ordering::SeqCst);
            async { Ok(()) }
        })
        .await
        .unwrap();

        assert_eq!(call_count.load(Ordering::SeqCst), 0, "unit replay should skip closure");
    }

    #[tokio::test]
    async fn journal_replay_empty_vec() {
        let ctx = create_journaled_context(vec![JournalEntry {
            seq: 0,
            value: serde_json::json!([]),
        }]);

        let result: Vec<String> = ctx.run(|| async { Ok(vec!["nope".into()]) }).await.unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn journal_crash_retry_simulation() {
        // Simulate: effect runs 2 of 3 steps, then "crashes".
        // On retry, load journal from store, first 2 replay, third executes fresh.
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());

        let call_counts = Arc::new([AtomicI32::new(0), AtomicI32::new(0), AtomicI32::new(0)]);

        // --- First attempt: run 2 steps, then "crash" before step 3 ---
        {
            let ctx = create_test_context().with_journal(store.clone(), vec![]);
            let cc = call_counts.clone();

            let _: String = ctx
                .run(move || {
                    cc[0].fetch_add(1, Ordering::SeqCst);
                    async { Ok("step1".into()) }
                })
                .await
                .unwrap();

            let cc = call_counts.clone();
            let _: i32 = ctx
                .run(move || {
                    cc[1].fetch_add(1, Ordering::SeqCst);
                    async { Ok(42) }
                })
                .await
                .unwrap();

            // "crash" here — ctx dropped, step 3 never runs
        }

        assert_eq!(call_counts[0].load(Ordering::SeqCst), 1);
        assert_eq!(call_counts[1].load(Ordering::SeqCst), 1);
        assert_eq!(call_counts[2].load(Ordering::SeqCst), 0);

        // --- Retry: load journal from store, resume ---
        let entries = store.load_journal("test_effect", Uuid::nil()).await.unwrap();
        assert_eq!(entries.len(), 2, "two steps should be journaled");

        let ctx = create_test_context().with_journal(store.clone(), entries);

        let cc = call_counts.clone();
        let a: String = ctx
            .run(move || {
                cc[0].fetch_add(1, Ordering::SeqCst);
                async { Ok("should replay".into()) }
            })
            .await
            .unwrap();
        assert_eq!(a, "step1", "step 1 should replay from journal");

        let cc = call_counts.clone();
        let b: i32 = ctx
            .run(move || {
                cc[1].fetch_add(1, Ordering::SeqCst);
                async { Ok(999) }
            })
            .await
            .unwrap();
        assert_eq!(b, 42, "step 2 should replay from journal");

        let cc = call_counts.clone();
        let c: String = ctx
            .run(move || {
                cc[2].fetch_add(1, Ordering::SeqCst);
                async { Ok("step3".into()) }
            })
            .await
            .unwrap();
        assert_eq!(c, "step3", "step 3 should execute fresh");

        // Steps 1 and 2 should NOT have re-executed
        assert_eq!(call_counts[0].load(Ordering::SeqCst), 1, "step 1 should not re-execute");
        assert_eq!(call_counts[1].load(Ordering::SeqCst), 1, "step 2 should not re-execute");
        assert_eq!(call_counts[2].load(Ordering::SeqCst), 1, "step 3 should execute once");

        // Store should now have 3 entries
        let entries = store.load_journal("test_effect", Uuid::nil()).await.unwrap();
        assert_eq!(entries.len(), 3);
    }

    #[tokio::test]
    async fn journal_isolation_by_handler_id() {
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());

        // Handler A writes to journal
        let ctx_a = Context::new(
            "handler_a".into(),
            "key".into(),
            Uuid::nil(),
            Uuid::nil(),
            None,
            Arc::new(TestDeps::default()),
        )
        .with_journal(store.clone(), vec![]);

        let _: String = ctx_a.run(|| async { Ok("from_a".into()) }).await.unwrap();

        // Handler B should have empty journal
        let entries_b = store.load_journal("handler_b", Uuid::nil()).await.unwrap();
        assert!(entries_b.is_empty(), "handler_b journal should be empty");

        // Handler A should have its entry
        let entries_a = store.load_journal("handler_a", Uuid::nil()).await.unwrap();
        assert_eq!(entries_a.len(), 1);
        assert_eq!(entries_a[0].value, serde_json::json!("from_a"));
    }

    #[tokio::test]
    async fn journal_isolation_by_event_id() {
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        let event_1 = Uuid::from_u128(1);
        let event_2 = Uuid::from_u128(2);

        // Same handler, different events
        let ctx_1 = Context::new(
            "handler".into(),
            "key".into(),
            Uuid::nil(),
            event_1,
            None,
            Arc::new(TestDeps::default()),
        )
        .with_journal(store.clone(), vec![]);

        let _: String = ctx_1.run(|| async { Ok("event1".into()) }).await.unwrap();

        // Event 2 should have empty journal
        let entries_2 = store.load_journal("handler", event_2).await.unwrap();
        assert!(entries_2.is_empty());

        // Event 1 should have its entry
        let entries_1 = store.load_journal("handler", event_1).await.unwrap();
        assert_eq!(entries_1.len(), 1);
    }

    #[tokio::test]
    async fn journal_cloned_context_shares_sequence_counter() {
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        let ctx = create_test_context().with_journal(store.clone(), vec![]);
        let cloned = ctx.clone();

        // Call run on original — gets seq=0
        let _: String = ctx.run(|| async { Ok("first".into()) }).await.unwrap();

        // Call run on clone — should get seq=1 (shared counter)
        let _: String = cloned.run(|| async { Ok("second".into()) }).await.unwrap();

        let entries = store.load_journal("test_effect", Uuid::nil()).await.unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].seq, 0);
        assert_eq!(entries[1].seq, 1);
    }

    #[tokio::test]
    async fn journal_many_sequential_runs() {
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        let ctx = create_test_context().with_journal(store.clone(), vec![]);

        for i in 0..100u32 {
            let _: u32 = ctx.run(move || async move { Ok(i) }).await.unwrap();
        }

        let entries = store.load_journal("test_effect", Uuid::nil()).await.unwrap();
        assert_eq!(entries.len(), 100);
        for (i, entry) in entries.iter().enumerate() {
            assert_eq!(entry.seq, i as u32);
            assert_eq!(entry.value, serde_json::json!(i as u32));
        }
    }

    #[tokio::test]
    async fn journal_replay_many_then_continue() {
        // Pre-load 50 entries, then execute 50 more fresh
        let entries: Vec<JournalEntry> = (0..50)
            .map(|i| JournalEntry {
                seq: i,
                value: serde_json::json!(i * 10),
            })
            .collect();

        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        let ctx = create_test_context().with_journal(store.clone(), entries);

        let call_count = Arc::new(AtomicI32::new(0));

        // First 50 should replay
        for i in 0..50u32 {
            let cc = call_count.clone();
            let result: u32 = ctx
                .run(move || {
                    cc.fetch_add(1, Ordering::SeqCst);
                    async move { Ok(999) }
                })
                .await
                .unwrap();
            assert_eq!(result, i * 10, "entry {} should replay", i);
        }
        assert_eq!(call_count.load(Ordering::SeqCst), 0, "no closures should execute during replay");

        // Next 50 should execute fresh
        for i in 50..100u32 {
            let cc = call_count.clone();
            let result: u32 = ctx
                .run(move || {
                    cc.fetch_add(1, Ordering::SeqCst);
                    async move { Ok(i) }
                })
                .await
                .unwrap();
            assert_eq!(result, i);
        }
        assert_eq!(call_count.load(Ordering::SeqCst), 50);
    }

    #[tokio::test]
    async fn journal_error_midway_preserves_prior_entries() {
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        let ctx = create_test_context().with_journal(store.clone(), vec![]);

        let _: String = ctx.run(|| async { Ok("ok1".into()) }).await.unwrap();
        let _: i32 = ctx.run(|| async { Ok(42) }).await.unwrap();

        // Third call fails
        let result: Result<String> = ctx.run(|| async { Err(anyhow::anyhow!("crash")) }).await;
        assert!(result.is_err());

        // Fourth call also fails
        let result: Result<String> = ctx.run(|| async { Err(anyhow::anyhow!("crash2")) }).await;
        assert!(result.is_err());

        // Only the two successful entries should be journaled
        let entries = store.load_journal("test_effect", Uuid::nil()).await.unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].value, serde_json::json!("ok1"));
        assert_eq!(entries[1].value, serde_json::json!(42));
    }

    #[tokio::test]
    async fn journal_error_then_retry_replays_and_continues() {
        // Simulate: 2 succeed, 3rd fails. Retry: 2 replay, 3rd succeeds.
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());

        // --- First attempt ---
        {
            let ctx = create_test_context().with_journal(store.clone(), vec![]);
            let _: String = ctx.run(|| async { Ok("a".into()) }).await.unwrap();
            let _: String = ctx.run(|| async { Ok("b".into()) }).await.unwrap();
            let _: Result<String> = ctx.run(|| async { Err(anyhow::anyhow!("fail")) }).await;
        }

        // --- Retry ---
        let entries = store.load_journal("test_effect", Uuid::nil()).await.unwrap();
        assert_eq!(entries.len(), 2);

        let ctx = create_test_context().with_journal(store.clone(), entries);
        let call_count = Arc::new(AtomicI32::new(0));

        let cc = call_count.clone();
        let a: String = ctx
            .run(move || {
                cc.fetch_add(1, Ordering::SeqCst);
                async { Ok("nope".into()) }
            })
            .await
            .unwrap();
        assert_eq!(a, "a");

        let cc = call_count.clone();
        let b: String = ctx
            .run(move || {
                cc.fetch_add(1, Ordering::SeqCst);
                async { Ok("nope".into()) }
            })
            .await
            .unwrap();
        assert_eq!(b, "b");

        // Third call succeeds this time
        let cc = call_count.clone();
        let c: String = ctx
            .run(move || {
                cc.fetch_add(1, Ordering::SeqCst);
                async { Ok("c".into()) }
            })
            .await
            .unwrap();
        assert_eq!(c, "c");

        assert_eq!(call_count.load(Ordering::SeqCst), 1, "only step 3 should execute");

        let entries = store.load_journal("test_effect", Uuid::nil()).await.unwrap();
        assert_eq!(entries.len(), 3);
    }

    #[tokio::test]
    async fn journal_seq_counter_advances_even_on_error() {
        // If run() at seq=1 fails, the next run() should be seq=2, not seq=1
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        let ctx = create_test_context().with_journal(store.clone(), vec![]);

        let _: String = ctx.run(|| async { Ok("seq0".into()) }).await.unwrap();
        let _: Result<String> = ctx.run(|| async { Err(anyhow::anyhow!("seq1 fails")) }).await;
        let _: String = ctx.run(|| async { Ok("seq2".into()) }).await.unwrap();

        let entries = store.load_journal("test_effect", Uuid::nil()).await.unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].seq, 0);
        assert_eq!(entries[1].seq, 2, "seq should skip over the failed entry");
    }

    #[tokio::test]
    async fn journal_replay_with_gap_in_seq_from_prior_error() {
        // Simulate journal from a prior attempt that had an error at seq=1:
        // entries at seq=0 and seq=2 (gap at 1)
        let ctx = create_journaled_context(vec![
            JournalEntry { seq: 0, value: serde_json::json!("a") },
            JournalEntry { seq: 2, value: serde_json::json!("c") },
        ]);

        // seq=0 replays
        let a: String = ctx.run(|| async { Ok("nope".into()) }).await.unwrap();
        assert_eq!(a, "a");

        // seq=1 has no journal entry (error last time) — executes fresh
        let b: String = ctx.run(|| async { Ok("fresh_b".into()) }).await.unwrap();
        assert_eq!(b, "fresh_b");

        // seq=2 replays from journal (HashMap lookup handles the gap)
        let c: String = ctx.run(|| async { Ok("nope".into()) }).await.unwrap();
        assert_eq!(c, "c");
    }

    #[tokio::test]
    async fn journal_clear_on_resolve_complete() {
        let store = Arc::new(MemoryStore::new());

        // Manually seed journal entries
        store
            .append_journal("handler_x", Uuid::from_u128(1), 0, serde_json::json!("a"))
            .await
            .unwrap();
        store
            .append_journal("handler_x", Uuid::from_u128(1), 1, serde_json::json!("b"))
            .await
            .unwrap();

        let entries = store
            .load_journal("handler_x", Uuid::from_u128(1))
            .await
            .unwrap();
        assert_eq!(entries.len(), 2);

        // resolve_handler(Complete) should clear journal atomically
        use crate::types::{HandlerCompletion, HandlerResolution};
        store
            .resolve_handler(HandlerResolution::Complete(HandlerCompletion {
                event_id: Uuid::from_u128(1),
                handler_id: "handler_x".into(),
                result: serde_json::json!({}),
                events_to_publish: vec![],
                log_entries: vec![],
            }))
            .await
            .unwrap();

        let entries = store
            .load_journal("handler_x", Uuid::from_u128(1))
            .await
            .unwrap();
        assert!(entries.is_empty(), "journal should be cleared on complete");
    }

    #[tokio::test]
    async fn journal_preserved_on_retry_resolution() {
        let store = Arc::new(MemoryStore::new());

        // Seed journal and an in-flight handler
        store
            .append_journal("handler_y", Uuid::from_u128(2), 0, serde_json::json!("a"))
            .await
            .unwrap();

        // We need an in-flight handler for retry to work in MemoryStore
        // Poll it first to put it in the in-flight map
        use crate::types::{HandlerResolution, QueuedHandler};
        let queued = QueuedHandler {
            event_id: Uuid::from_u128(2),
            handler_id: "handler_y".into(),
            correlation_id: Uuid::nil(),
            event_type: "Test".into(),
            event_payload: serde_json::json!({}),
            parent_event_id: None,
            batch_id: None,
            batch_index: None,
            batch_size: None,
            execute_at: chrono::Utc::now(),
            timeout_seconds: 30,
            max_attempts: 3,
            priority: 10,
            hops: 0,
            attempts: 1,
            join_window_timeout_seconds: None,
        };
        // Insert directly into handler queue, then poll to put in-flight
        store.publish_handler_for_test(queued).await;
        let _ = store.poll_next_handler().await.unwrap();

        store
            .resolve_handler(HandlerResolution::Retry {
                event_id: Uuid::from_u128(2),
                handler_id: "handler_y".into(),
                error: "transient".into(),
                new_attempts: 2,
                next_execute_at: chrono::Utc::now(),
            })
            .await
            .unwrap();

        // Journal should still be there after retry
        let entries = store
            .load_journal("handler_y", Uuid::from_u128(2))
            .await
            .unwrap();
        assert_eq!(entries.len(), 1, "journal should survive retry resolution");
    }
}
