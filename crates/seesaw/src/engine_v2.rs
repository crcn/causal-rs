//! Queue-backed Engine implementation
//!
//! Simplified Engine that publishes to Store instead of in-memory dispatch.
//! Workers (EventWorker, EffectWorker) poll from store and execute.

use anyhow::Result;
use std::any::TypeId;
use std::marker::PhantomData;
use std::sync::Arc;
use uuid::Uuid;

use crate::effect_registry::EffectRegistry;
use crate::process::ProcessFuture;
use crate::reducer_registry::ReducerRegistry;
use crate::{Effect, Reducer, Store};

/// Queue-backed Engine
///
/// Publishes events to Store, workers poll and execute.
pub struct Engine<S, D, St>
where
    S: Clone + Send + Sync + 'static,
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
    S: Clone + Send + Sync + 'static,
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
    pub fn with_reducer(mut self, reducer: impl Reducer<S> + 'static) -> Self {
        Arc::get_mut(&mut self.reducers)
            .expect("Cannot add reducer after cloning")
            .register(reducer);
        self
    }

    /// Register an effect
    pub fn with_effect(mut self, effect: impl Effect<S, D> + 'static) -> Self {
        Arc::get_mut(&mut self.effects)
            .expect("Cannot add effect after cloning")
            .register(effect);
        self
    }

    /// Process event (returns lazy future)
    ///
    /// Event is serialized and published to store when future is polled.
    pub fn process<E>(&self, event: E) -> ProcessFuture<St>
    where
        E: Clone + Send + Sync + 'static,
    {
        self.process_saga(Uuid::new_v4(), event)
    }

    /// Process event with saga ID
    pub fn process_saga<E>(&self, saga_id: Uuid, event: E) -> ProcessFuture<St>
    where
        E: Clone + Send + Sync + 'static,
    {
        self.process_saga_with_id(Uuid::new_v4(), saga_id, event)
    }

    /// Process event with event ID and saga ID (for idempotency)
    pub fn process_saga_with_id<E>(
        &self,
        event_id: Uuid,
        saga_id: Uuid,
        event: E,
    ) -> ProcessFuture<St>
    where
        E: Clone + Send + Sync + 'static,
    {
        let event_type = std::any::type_name::<E>().to_string();
        let payload = serde_json::to_value(&event)
            .expect("Event must be serializable");

        ProcessFuture::new(
            self.store.clone(),
            event_id,
            saga_id,
            None, // parent_id will be set by workers
            event_type,
            payload,
            0, // Initial hop count
        )
    }

    /// Get store reference (for workers)
    pub fn store(&self) -> &Arc<St> {
        &self.store
    }

    /// Get deps reference (for workers)
    pub fn deps(&self) -> &Arc<D> {
        &self.deps
    }

    /// Get reducers (for event workers)
    pub fn reducers(&self) -> &Arc<ReducerRegistry<S>> {
        &self.reducers
    }

    /// Get effects (for event workers)
    pub fn effects(&self) -> &Arc<EffectRegistry<S, D>> {
        &self.effects
    }
}

impl<S, D, St> Clone for Engine<S, D, St>
where
    S: Clone + Send + Sync + 'static,
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
