//! Effect context and related types.

use std::sync::Arc;

use uuid::Uuid;

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
    pub(crate) deps: Arc<D>,
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
            deps: self.deps.clone(),
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
        deps: Arc<D>,
    ) -> Self {
        Self {
            handler_id,
            idempotency_key,
            correlation_id,
            event_id,
            deps,
        }
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
}

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
}
