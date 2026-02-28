//! Queue-backed Engine implementation with Backend trait
//!
//! Simplified Engine that publishes to Backend instead of Store.
//! Workers are managed by Backend.serve().

use std::sync::Arc;
use uuid::Uuid;

use crate::backend::{Backend, BackendServeConfig};
use crate::backend::capability::*;
use crate::backend::job_executor::JobExecutor;
use crate::handler::Handler;
use crate::handler_registry::HandlerRegistry;
use crate::process::{DispatchFuture, ProcessFuture, SettleDriver};
use crate::runtime::Runtime;
use anyhow::Result;
use tokio_util::sync::CancellationToken;

/// Queue-backed Engine with Backend trait.
///
/// Publishes events to Backend, which handles worker management.
pub struct Engine<D, B>
where
    D: Send + Sync + 'static,
    B: Backend,
{
    backend: Arc<B>,
    deps: Arc<D>,
    effects: Arc<HandlerRegistry<D>>,
}

impl<D, B> Engine<D, B>
where
    D: Send + Sync + 'static,
    B: Backend,
{
    /// Create new engine with dependencies and backend.
    pub fn new(deps: D, backend: B) -> Self {
        Self {
            backend: Arc::new(backend),
            deps: Arc::new(deps),
            effects: Arc::new(HandlerRegistry::new()),
        }
    }

    /// Register a handler.
    pub fn with_handler(mut self, handler: Handler<D>) -> Self {
        Arc::get_mut(&mut self.effects)
            .expect("Cannot add handler after cloning")
            .register(handler);
        self
    }

    /// Register multiple handlers.
    pub fn with_handlers<I>(mut self, handlers: I) -> Self
    where
        I: IntoIterator<Item = Handler<D>>,
    {
        let registry = Arc::get_mut(&mut self.effects).expect("Cannot add handlers after cloning");
        for handler in handlers {
            registry.register(handler);
        }
        self
    }

    /// Backend name for diagnostics.
    pub fn backend_name(&self) -> &'static str {
        self.backend.name()
    }

    /// Dispatch event (returns lazy future).
    ///
    /// Event is serialized and published to backend when future is polled.
    pub fn dispatch<E>(&self, event: E) -> ProcessFuture<B>
    where
        E: Clone + Send + Sync + serde::Serialize + 'static,
    {
        let event_id = Uuid::new_v4();
        let correlation_id = Uuid::new_v4();
        let event_type = std::any::type_name::<E>().to_string();
        let payload = serde_json::to_value(&event).expect("Event must be serializable");

        ProcessFuture::new(
            self.backend.clone(),
            event_id,
            correlation_id,
            None, // parent_id will be set by workers
            event_type,
            payload,
            0, // Initial hop count
        )
    }

    /// Convenience for worker-only processes (blocking).
    ///
    /// Runs backend workers until shutdown token is cancelled.
    pub async fn serve(self, shutdown: CancellationToken) -> Result<()> {
        let executor = Arc::new(JobExecutor::new(self.deps.clone(), self.effects.clone()));
        let config = BackendServeConfig::default();
        self.backend.serve(executor, config, shutdown).await
    }

    /// Non-blocking handle (for HTTP servers).
    ///
    /// Spawns backend workers in background and returns handle for shutdown.
    pub async fn start(self, config: BackendServeConfig) -> Result<Runtime> {
        Runtime::start(self, config).await
    }

    /// Get backend reference (for Runtime).
    pub(crate) fn backend(&self) -> &Arc<B> {
        &self.backend
    }

    /// Get deps reference (for Runtime).
    pub(crate) fn deps(&self) -> &Arc<D> {
        &self.deps
    }

    /// Get effects (for Runtime).
    pub(crate) fn effects(&self) -> &Arc<HandlerRegistry<D>> {
        &self.effects
    }
}

impl<D, B> Clone for Engine<D, B>
where
    D: Send + Sync + 'static,
    B: Backend,
{
    fn clone(&self) -> Self {
        Self {
            backend: self.backend.clone(),
            deps: self.deps.clone(),
            effects: self.effects.clone(),
        }
    }
}

// Capability-based methods - only available when backend supports them

impl<D, B> Engine<D, B>
where
    D: Send + Sync + 'static,
    B: WorkflowStatusBackend,
{
    /// Get workflow status (requires WorkflowStatusBackend).
    pub async fn get_workflow_status(&self, correlation_id: Uuid) -> Result<crate::WorkflowStatus> {
        self.backend.get_workflow_status(correlation_id).await
    }
}

impl<D, B> Engine<D, B>
where
    D: Send + Sync + 'static,
    B: WorkflowSubscriptionBackend,
{
    /// Subscribe to workflow events (requires WorkflowSubscriptionBackend).
    pub async fn subscribe_workflow(
        &self,
        correlation_id: Uuid,
    ) -> Result<Box<dyn futures::Stream<Item = crate::WorkflowEvent> + Send + Unpin>> {
        self.backend.subscribe_workflow_events(correlation_id).await
    }
}

impl<D, B> Engine<D, B>
where
    D: Send + Sync + 'static,
    B: DeadLetterQueueBackend,
{
    /// List dead letter queue entries (requires DeadLetterQueueBackend).
    pub async fn list_dlq(&self, filters: DlqFilters) -> Result<Vec<DeadLetter>> {
        self.backend.list_dlq(filters).await
    }

    /// Retry a dead letter (requires DeadLetterQueueBackend).
    pub async fn retry_dlq(&self, event_id: Uuid, handler_id: String) -> Result<()> {
        self.backend.retry_dlq(event_id, handler_id).await
    }
}

impl<D, B> Engine<D, B>
where
    D: Send + Sync + 'static,
    B: SettleableBackend,
{
    /// Dispatch event with settlement support.
    ///
    /// Returns a `DispatchFuture` that can be:
    /// - Awaited directly (fire-and-forget, same as `dispatch()`)
    /// - Chained with `.settled()` to drive the entire causal tree to completion
    ///
    /// ```ignore
    /// // Fire-and-forget
    /// engine.process(event).await?;
    ///
    /// // Synchronous settlement
    /// engine.process(event).settled().await?;
    /// ```
    pub fn process<E>(&self, event: E) -> DispatchFuture<B>
    where
        E: Clone + Send + Sync + serde::Serialize + 'static,
    {
        let inner = self.dispatch(event);

        let backend = self.backend.clone();
        let deps = self.deps.clone();
        let effects = self.effects.clone();
        let settle_driver: SettleDriver = Arc::new(move |correlation_id| {
            let executor = Arc::new(JobExecutor::new(deps.clone(), effects.clone()));
            let backend = backend.clone();
            Box::pin(async move { backend.settle(&executor, correlation_id).await })
        });

        DispatchFuture::new(inner, Some(settle_driver))
    }
}

impl<D, B> Engine<D, B>
where
    D: Send + Sync + 'static,
    B: InsightBackend,
{
    /// Get workflow execution tree (requires InsightBackend).
    pub async fn get_workflow_tree(&self, correlation_id: Uuid) -> Result<WorkflowTree> {
        self.backend.get_workflow_tree(correlation_id).await
    }

    /// Get aggregate workflow statistics (requires InsightBackend).
    pub async fn get_insight_stats(&self) -> Result<InsightStats> {
        self.backend.get_insight_stats().await
    }
}
