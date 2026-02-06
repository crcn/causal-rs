//! Effect context and related types.

use std::sync::Arc;

use uuid::Uuid;

/// Context passed to effect handlers.
pub struct EffectContext<D>
where
    D: Send + Sync + 'static,
{
    /// Human-readable identifier of the effect being executed.
    pub effect_id: String,
    /// Deterministic idempotency key for external API calls.
    pub idempotency_key: String,
    /// Correlation ID from event envelope - groups related events together.
    pub correlation_id: Uuid,
    /// Current event's unique ID from envelope.
    pub event_id: Uuid,
    pub(crate) deps: Arc<D>,
}

impl<D> Clone for EffectContext<D>
where
    D: Send + Sync + 'static,
{
    fn clone(&self) -> Self {
        Self {
            effect_id: self.effect_id.clone(),
            idempotency_key: self.idempotency_key.clone(),
            correlation_id: self.correlation_id,
            event_id: self.event_id,
            deps: self.deps.clone(),
        }
    }
}

impl<D> EffectContext<D>
where
    D: Send + Sync + 'static,
{
    pub(crate) fn new(
        effect_id: String,
        idempotency_key: String,
        correlation_id: Uuid,
        event_id: Uuid,
        deps: Arc<D>,
    ) -> Self {
        Self {
            effect_id,
            idempotency_key,
            correlation_id,
            event_id,
            deps,
        }
    }

    /// Get the effect ID (human-readable identifier).
    pub fn effect_id(&self) -> &str {
        &self.effect_id
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug, Default)]
    struct TestDeps {
        multiplier: i32,
    }

    fn create_test_context() -> EffectContext<TestDeps> {
        let deps = Arc::new(TestDeps { multiplier: 2 });
        EffectContext::new(
            "test_effect".to_string(),
            "test_idempotency_key".to_string(),
            Uuid::nil(),
            Uuid::nil(),
            deps,
        )
    }

    #[tokio::test]
    async fn test_effect_context_accessors() {
        let context = create_test_context();

        assert_eq!(context.effect_id(), "test_effect");
        assert_eq!(context.idempotency_key(), "test_idempotency_key");
        assert_eq!(context.deps().multiplier, 2);
    }

    #[tokio::test]
    async fn test_effect_context_clone() {
        let context = create_test_context();
        let cloned = context.clone();

        assert_eq!(cloned.effect_id(), "test_effect");
        assert_eq!(cloned.deps().multiplier, 2);
    }
}
