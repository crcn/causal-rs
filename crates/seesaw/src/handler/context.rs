//! Effect context and related types.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::Result;
use uuid::Uuid;

use crate::aggregator::AggregatorRegistry;

/// Executes side-effect closures with optional journaling.
///
/// Implementations determine how results are captured:
/// - [`DirectRunner`]: executes inline, no journaling
/// - Restate backend: journals result, replays from journal
pub trait SideEffectRunner: Send + Sync {
    /// Run a side effect, optionally journaling the result.
    ///
    /// The future produces a serialized `serde_json::Value` on success.
    /// Implementations may execute the future directly (DirectRunner) or
    /// journal the result for replay (durable runtimes).
    fn run_side_effect(
        &self,
        f: Pin<Box<dyn Future<Output = Result<serde_json::Value>> + Send>>,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value>> + Send>>;
}

/// Default runner — executes side effects directly with no journaling.
pub struct DirectRunner;

impl SideEffectRunner for DirectRunner {
    fn run_side_effect(
        &self,
        f: Pin<Box<dyn Future<Output = Result<serde_json::Value>> + Send>>,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value>> + Send>> {
        f
    }
}

/// Trait for handler context types.
///
/// This trait allows different backend implementations to provide
/// their own context types while maintaining a common interface.
///
/// The default implementation is `Context<D>`, but backends like
/// Restate can provide enhanced contexts with additional operations.
pub trait HandlerContext<D>: Clone + Send + Sync + 'static
where
    D: Send + Sync + 'static,
{
    /// Get the handler ID (human-readable identifier).
    fn handler_id(&self) -> &str;

    /// Get the idempotency key for external API calls.
    fn idempotency_key(&self) -> &str;

    /// Get the correlation ID for workflow grouping.
    fn correlation_id(&self) -> Uuid;

    /// Get the current event ID.
    fn event_id(&self) -> Uuid;

    /// Get the parent event ID (for causal tracking).
    fn parent_event_id(&self) -> Option<Uuid>;

    /// Whether this handler is being invoked during event replay.
    #[deprecated(since = "0.16.0", note = "Use ctx.run() for replay-safe side effects")]
    fn is_replay(&self) -> bool;

    /// Get shared dependencies.
    fn deps(&self) -> &D;
}

/// Context passed to effect handlers.
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
    /// Whether this handler is being invoked during event replay.
    pub(crate) is_replay: bool,
    /// Aggregator registry for transition guard replay.
    pub(crate) aggregator_registry: Option<Arc<AggregatorRegistry>>,
    /// Side-effect runner for journaled execution.
    pub(crate) side_effect_runner: Arc<dyn SideEffectRunner>,
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
            is_replay: self.is_replay,
            aggregator_registry: self.aggregator_registry.clone(),
            side_effect_runner: self.side_effect_runner.clone(),
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
            is_replay: false,
            aggregator_registry: None,
            side_effect_runner: Arc::new(DirectRunner),
        }
    }

    /// Attach an aggregator registry (used by the engine for transition guards).
    pub(crate) fn with_aggregator_registry(
        mut self,
        registry: Arc<AggregatorRegistry>,
    ) -> Self {
        self.aggregator_registry = Some(registry);
        self
    }

    /// Set the replay flag (used during event replay to suppress side effects).
    #[allow(dead_code)]
    pub(crate) fn with_replay(mut self, is_replay: bool) -> Self {
        self.is_replay = is_replay;
        self
    }

    /// Attach a side-effect runner for journaled execution.
    pub(crate) fn with_side_effect_runner(
        mut self,
        runner: Arc<dyn SideEffectRunner>,
    ) -> Self {
        self.side_effect_runner = runner;
        self
    }

    /// Get the aggregator registry (if set).
    pub fn aggregator_registry(&self) -> Option<&AggregatorRegistry> {
        self.aggregator_registry.as_deref()
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

    /// Whether this handler is being invoked during event replay.
    ///
    /// When `true`, handlers should skip side effects (API calls, emails, etc.)
    /// and only perform state reconstruction.
    #[deprecated(since = "0.16.0", note = "Use ctx.run() for replay-safe side effects")]
    pub fn is_replay(&self) -> bool {
        self.is_replay
    }

    /// Execute a side-effect closure with replay safety.
    ///
    /// - **DirectRunner (default):** Executes inline, returns result directly.
    /// - **Durable runtime (Restate):** Journals the result. On replay,
    ///   skips execution and returns the journaled value.
    ///
    /// The return type must implement `Serialize + DeserializeOwned` so
    /// durable runtimes can persist the result.
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
        let fut = async move {
            let result = f().await?;
            let json = serde_json::to_value(&result)?;
            Ok(json)
        };

        let json = self
            .side_effect_runner
            .run_side_effect(Box::pin(fut))
            .await?;

        let result: T = serde_json::from_value(json)?;
        Ok(result)
    }
}

#[allow(deprecated)]
impl<D> HandlerContext<D> for Context<D>
where
    D: Send + Sync + 'static,
{
    fn handler_id(&self) -> &str {
        &self.handler_id
    }

    fn idempotency_key(&self) -> &str {
        &self.idempotency_key
    }

    fn correlation_id(&self) -> Uuid {
        self.correlation_id
    }

    fn event_id(&self) -> Uuid {
        self.event_id
    }

    fn parent_event_id(&self) -> Option<Uuid> {
        self.parent_event_id
    }

    fn is_replay(&self) -> bool {
        self.is_replay
    }

    fn deps(&self) -> &D {
        &self.deps
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
