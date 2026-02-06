//! Queue-backed Engine implementation
//!
//! Simplified Engine that publishes to Store instead of in-memory dispatch.
//! Workers (EventWorker, EffectWorker) poll from store and execute.

use std::sync::Arc;
use uuid::Uuid;

use crate::handler::Handler;
use crate::handler_registry::HandlerRegistry;
use crate::process::ProcessFuture;
use crate::queue_backend::{QueueBackend, StoreQueueBackend};
use crate::Store;

/// Adapter trait for Engine backend construction.
///
/// This allows `Engine::new` to accept either:
/// - A plain store (`St`) which uses [`StoreQueueBackend`] by default.
/// - A tuple (`(St, Q)`) for explicit store + queue backend pairing.
pub trait EngineBackend {
    type Store: Store;
    type Queue: QueueBackend<Self::Store>;

    fn into_engine_parts(self) -> (Self::Store, Self::Queue);
}

impl<St> EngineBackend for St
where
    St: Store,
{
    type Store = St;
    type Queue = StoreQueueBackend;

    fn into_engine_parts(self) -> (Self::Store, Self::Queue) {
        (self, StoreQueueBackend)
    }
}

impl<St, Q> EngineBackend for (St, Q)
where
    St: Store,
    Q: QueueBackend<St>,
{
    type Store = St;
    type Queue = Q;

    fn into_engine_parts(self) -> (Self::Store, Self::Queue) {
        self
    }
}

/// Queue-backed Engine
///
/// Publishes events to Store, workers poll and execute.
pub struct Engine<D, St>
where
    D: Send + Sync + 'static,
    St: Store,
{
    store: Arc<St>,
    queue_backend: Arc<dyn QueueBackend<St>>,
    deps: Arc<D>,
    effects: Arc<HandlerRegistry<D>>,
}

impl<D, St> Engine<D, St>
where
    D: Send + Sync + 'static,
    St: Store,
{
    /// Create new engine with dependencies and backend.
    ///
    /// Accepts either:
    /// - `store` (defaults to store-backed queue dispatch)
    /// - `(store, queue_backend)` for explicit queue backend wiring
    pub fn new<B>(deps: D, backend: B) -> Self
    where
        B: EngineBackend<Store = St>,
    {
        let (store, queue_backend) = backend.into_engine_parts();
        Self::from_parts(deps, store, Vec::new(), Arc::new(queue_backend))
    }

    /// Create a builder for engine configuration.
    pub fn builder(deps: D, store: St) -> EngineBuilder<D, St> {
        EngineBuilder {
            store,
            deps,
            effects: Vec::new(),
        }
    }

    /// Queue backend name for diagnostics.
    pub fn queue_backend_name(&self) -> &'static str {
        self.queue_backend.name()
    }

    fn from_parts(
        deps: D,
        store: St,
        effects: Vec<Handler<D>>,
        queue_backend: Arc<dyn QueueBackend<St>>,
    ) -> Self {
        let mut handler_registry = Arc::new(HandlerRegistry::new());

        // Safe because registry was just created and is uniquely owned.
        let effects_target = Arc::get_mut(&mut handler_registry)
            .expect("new effect registry should be uniquely owned");
        for effect in effects {
            effects_target.register(effect);
        }

        Self {
            store: Arc::new(store),
            queue_backend,
            deps: Arc::new(deps),
            effects: handler_registry,
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

    /// Process event (returns lazy future)
    ///
    /// Event is serialized and published to store when future is polled.
    pub fn process<E>(&self, event: E) -> ProcessFuture<St>
    where
        E: Clone + Send + Sync + serde::Serialize + 'static,
    {
        self.process_workflow(Uuid::new_v4(), event)
    }

    /// Process event with workflow ID
    pub fn process_workflow<E>(&self, correlation_id: Uuid, event: E) -> ProcessFuture<St>
    where
        E: Clone + Send + Sync + serde::Serialize + 'static,
    {
        self.process_workflow_with_id(Uuid::new_v4(), correlation_id, event)
    }

    /// Process event with external event ID (webhook idempotency)
    pub fn process_with_id<E>(&self, event_id: Uuid, event: E) -> ProcessFuture<St>
    where
        E: Clone + Send + Sync + serde::Serialize + 'static,
    {
        self.process_workflow_with_id(event_id, Uuid::new_v4(), event)
    }

    /// Process event with event ID and workflow ID (for idempotency)
    pub fn process_workflow_with_id<E>(
        &self,
        event_id: Uuid,
        correlation_id: Uuid,
        event: E,
    ) -> ProcessFuture<St>
    where
        E: Clone + Send + Sync + serde::Serialize + 'static,
    {
        let event_type = std::any::type_name::<E>().to_string();
        let payload = serde_json::to_value(&event).expect("Event must be serializable");

        ProcessFuture::new(
            self.store.clone(),
            event_id,
            correlation_id,
            None, // parent_id will be set by workers
            event_type,
            payload,
            0, // Initial hop count
        )
    }

    /// Get store reference (for workers)
    pub(crate) fn store(&self) -> &Arc<St> {
        &self.store
    }

    /// Get queue backend (for workers)
    pub(crate) fn queue_backend(&self) -> Arc<dyn QueueBackend<St>> {
        self.queue_backend.clone()
    }

    /// Get deps reference (for workers)
    pub(crate) fn deps(&self) -> &Arc<D> {
        &self.deps
    }

    /// Get effects (for event/effect workers)
    pub(crate) fn effects(&self) -> &Arc<HandlerRegistry<D>> {
        &self.effects
    }
}

impl<D, St> Clone for Engine<D, St>
where
    D: Send + Sync + 'static,
    St: Store,
{
    fn clone(&self) -> Self {
        Self {
            store: self.store.clone(),
            queue_backend: self.queue_backend.clone(),
            deps: self.deps.clone(),
            effects: self.effects.clone(),
        }
    }
}

/// Builder for queue-backed [`Engine`].
pub struct EngineBuilder<D, St>
where
    D: Send + Sync + 'static,
    St: Store,
{
    store: St,
    deps: D,
    effects: Vec<Handler<D>>,
}

impl<D, St> EngineBuilder<D, St>
where
    D: Send + Sync + 'static,
    St: Store,
{
    /// Register a handler.
    pub fn with_handler(mut self, handler: Handler<D>) -> Self {
        self.effects.push(handler);
        self
    }

    /// Register multiple handlers.
    pub fn with_handlers<I>(mut self, handlers: I) -> Self
    where
        I: IntoIterator<Item = Handler<D>>,
    {
        self.effects.extend(handlers);
        self
    }

    /// Build engine.
    pub fn build(self) -> Engine<D, St> {
        Engine::from_parts(
            self.deps,
            self.store,
            self.effects,
            Arc::new(StoreQueueBackend),
        )
    }
}
