//! Queue-backed Engine implementation
//!
//! Simplified Engine that publishes to Store instead of in-memory dispatch.
//! Workers (EventWorker, EffectWorker) poll from store and execute.

use std::marker::PhantomData;
use std::sync::Arc;
use uuid::Uuid;

use crate::effect::Effect;
use crate::effect_registry::EffectRegistry;
use crate::process::ProcessFuture;
use crate::reducer::Reducer;
use crate::reducer_registry::ReducerRegistry;
use crate::Store;

/// Queue-backed Engine
///
/// Publishes events to Store, workers poll and execute.
pub struct Engine<S, D, St>
where
    S: Clone + Send + Sync + serde::Serialize + serde::de::DeserializeOwned + Default + 'static,
    D: Send + Sync + 'static,
    St: Store,
{
    store: Arc<St>,
    deps: Arc<D>,
    reducers: Arc<ReducerRegistry<S>>,
    effects: Arc<EffectRegistry<S, D>>,
    _marker: PhantomData<S>,
}

impl<S, D, St> Engine<S, D, St>
where
    S: Clone + Send + Sync + serde::Serialize + serde::de::DeserializeOwned + Default + 'static,
    D: Send + Sync + 'static,
    St: Store,
{
    /// Create new engine with dependencies and store
    pub fn new(deps: D, store: St) -> Self {
        Self {
            store: Arc::new(store),
            deps: Arc::new(deps),
            reducers: Arc::new(ReducerRegistry::new()),
            effects: Arc::new(EffectRegistry::new()),
            _marker: PhantomData,
        }
    }

    /// Register a reducer
    pub fn with_reducer(mut self, reducer: Reducer<S>) -> Self {
        Arc::get_mut(&mut self.reducers)
            .expect("Cannot add reducer after cloning")
            .register(reducer);
        self
    }

    /// Register multiple reducers.
    pub fn with_reducers<I>(mut self, reducers: I) -> Self
    where
        I: IntoIterator<Item = Reducer<S>>,
    {
        let registry = Arc::get_mut(&mut self.reducers).expect("Cannot add reducers after cloning");
        for reducer in reducers {
            registry.register(reducer);
        }
        self
    }

    /// Register an effect
    pub fn with_effect(mut self, effect: Effect<S, D>) -> Self {
        Arc::get_mut(&mut self.effects)
            .expect("Cannot add effect after cloning")
            .register(effect);
        self
    }

    /// Register multiple effects.
    pub fn with_effects<I>(mut self, effects: I) -> Self
    where
        I: IntoIterator<Item = Effect<S, D>>,
    {
        let registry = Arc::get_mut(&mut self.effects).expect("Cannot add effects after cloning");
        for effect in effects {
            registry.register(effect);
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
    ///
    /// Uses provided event_id for idempotency (e.g., Stripe webhook ID).
    /// Generates a new correlation_id.
    ///
    /// # Example
    /// ```ignore
    /// // Webhook handler
    /// async fn handle_stripe_webhook(payload: StripeWebhook) -> Result<()> {
    ///     engine.process_with_id(
    ///         Uuid::parse_str(&payload.id)?,  // Stripe's idempotency key
    ///         OrderPlaced { ... }
    ///     ).await?;
    ///     Ok(())
    /// }
    /// ```
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

    /// Get deps reference (for workers)
    pub(crate) fn deps(&self) -> &Arc<D> {
        &self.deps
    }

    /// Get reducers (for event workers)
    pub(crate) fn reducers(&self) -> &Arc<ReducerRegistry<S>> {
        &self.reducers
    }

    /// Get effects (for event workers)
    pub(crate) fn effects(&self) -> &Arc<EffectRegistry<S, D>> {
        &self.effects
    }
}

impl<S, D, St> Clone for Engine<S, D, St>
where
    S: Clone + Send + Sync + serde::Serialize + serde::de::DeserializeOwned + Default + 'static,
    D: Send + Sync + 'static,
    St: Store,
{
    fn clone(&self) -> Self {
        Self {
            store: self.store.clone(),
            deps: self.deps.clone(),
            reducers: self.reducers.clone(),
            effects: self.effects.clone(),
            _marker: PhantomData,
        }
    }
}
