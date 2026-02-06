//! Effect context and related types.

use std::any::{Any, TypeId};
use std::collections::HashSet;
use std::sync::Arc;

use uuid::Uuid;

/// Trait for emitting events of any type.
pub trait EventEmitter<S, D>: Send + Sync + 'static
where
    S: Send + Sync + 'static,
    D: Send + Sync + 'static,
{
    fn emit(&self, type_id: TypeId, event: Arc<dyn Any + Send + Sync>, ctx: EffectContext<S, D>);
}

/// A boxed event emitter.
pub type BoxedEmitter<S, D> = Arc<dyn EventEmitter<S, D>>;

/// Context passed to effect handlers.
pub struct EffectContext<S, D>
where
    S: Send + Sync + 'static,
    D: Send + Sync + 'static,
{
    /// Human-readable identifier of the effect being executed
    pub effect_id: String,
    /// Deterministic idempotency key for external API calls
    /// Generated from event_id + effect_id, stable across retries
    pub idempotency_key: String,
    /// Correlation ID from event envelope - groups related events together
    pub correlation_id: Uuid,
    /// Current event's unique ID from envelope
    pub event_id: Uuid,
    pub(crate) prev_state: Arc<S>,
    pub(crate) state: Arc<S>,
    pub(crate) live_state: Arc<parking_lot::RwLock<S>>,
    pub(crate) deps: Arc<D>,
    pub(crate) emitter: BoxedEmitter<S, D>,
    /// Chain of relay IDs visited (inherited by emitted events for echo prevention)
    pub(crate) visited: Arc<HashSet<u64>>,
}

impl<S, D> Clone for EffectContext<S, D>
where
    S: Send + Sync + 'static,
    D: Send + Sync + 'static,
{
    fn clone(&self) -> Self {
        Self {
            effect_id: self.effect_id.clone(),
            idempotency_key: self.idempotency_key.clone(),
            correlation_id: self.correlation_id,
            event_id: self.event_id,
            prev_state: self.prev_state.clone(),
            state: self.state.clone(),
            live_state: self.live_state.clone(),
            deps: self.deps.clone(),
            emitter: self.emitter.clone(),
            visited: self.visited.clone(),
        }
    }
}

impl<S, D> EffectContext<S, D>
where
    S: Send + Sync + 'static,
    D: Send + Sync + 'static,
{
    pub(crate) fn new(
        effect_id: String,
        idempotency_key: String,
        correlation_id: Uuid,
        event_id: Uuid,
        prev_state: Arc<S>,
        state: Arc<S>,
        live_state: Arc<parking_lot::RwLock<S>>,
        deps: Arc<D>,
        emitter: BoxedEmitter<S, D>,
    ) -> Self {
        Self {
            effect_id,
            idempotency_key,
            correlation_id,
            event_id,
            prev_state,
            state,
            live_state,
            deps,
            emitter,
            visited: Arc::new(HashSet::new()),
        }
    }

    /// Get the state before the reducer ran.
    pub fn prev_state(&self) -> &S {
        &self.prev_state
    }

    /// Get the state after the reducer ran (snapshot at event dispatch time).
    pub fn next_state(&self) -> &S {
        &self.state
    }

    /// Get the current live state (reads current value from store).
    /// Use this in long-running background tasks that need fresh state.
    pub fn curr_state(&self) -> S
    where
        S: Clone,
    {
        self.live_state.read().clone()
    }

    /// Get the effect ID (human-readable identifier).
    /// Used for debugging, tracing, and the `produced_by` field in event envelopes.
    pub fn effect_id(&self) -> &str {
        &self.effect_id
    }

    /// Get the idempotency key for external API calls.
    /// Deterministic key generated from event_id + effect_id, stable across retries.
    ///
    /// # Example
    /// ```ignore
    /// effect::on::<OrderPlaced>()
    ///     .id("charge_payment")
    ///     .then(|event, ctx| async move {
    ///         // Use with Stripe API for idempotency
    ///         ctx.deps().stripe.charge(
    ///             event.amount,
    ///             ctx.idempotency_key()  // Same key on retry
    ///         ).await?;
    ///         Ok(PaymentCharged { order_id: event.id })
    ///     })
    /// ```
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

    /// Emit a type-erased event to the store.
    /// Used for forwarding events from external sources where the type is not known at compile time.
    pub(crate) fn emit_any(&self, event: Arc<dyn Any + Send + Sync>, type_id: TypeId) {
        self.emitter.emit(type_id, event, self.clone());
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug, Default)]
    struct TestState {
        value: i32,
    }

    #[derive(Clone, Debug, Default)]
    struct TestDeps {
        multiplier: i32,
    }

    struct NoopEmitter;

    impl EventEmitter<TestState, TestDeps> for NoopEmitter {
        fn emit(
            &self,
            _type_id: TypeId,
            _event: Arc<dyn Any + Send + Sync>,
            _ctx: EffectContext<TestState, TestDeps>,
        ) {
        }
    }

    fn create_test_context() -> EffectContext<TestState, TestDeps> {
        let state = Arc::new(TestState { value: 42 });
        let live_state = Arc::new(parking_lot::RwLock::new(TestState { value: 42 }));
        let deps = Arc::new(TestDeps { multiplier: 2 });
        let emitter: BoxedEmitter<TestState, TestDeps> = Arc::new(NoopEmitter);
        EffectContext::new(
            "test_effect".to_string(),
            "test_idempotency_key".to_string(),
            Uuid::nil(),
            Uuid::nil(),
            state.clone(),
            state,
            live_state,
            deps,
            emitter,
        )
    }

    #[tokio::test]
    async fn test_effect_context_state_access() {
        let context = create_test_context();

        assert_eq!(context.next_state().value, 42);
        assert_eq!(context.curr_state().value, 42);
        assert_eq!(context.deps().multiplier, 2);
    }

    #[tokio::test]
    async fn test_effect_context_clone() {
        let context = create_test_context();
        let cloned = context.clone();

        assert_eq!(cloned.next_state().value, 42);
        assert_eq!(cloned.deps().multiplier, 2);
    }
}
