//! Core effect types.

use std::any::{Any, TypeId};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::Result;

use super::context::EffectContext;

/// A boxed future for async effect handlers.
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// Payload for effects that receive all events (type-erased).
#[derive(Clone)]
pub struct AnyEvent {
    /// The type-erased event value.
    pub value: Arc<dyn Any + Send + Sync>,
    /// The TypeId of the original event.
    pub type_id: TypeId,
}

impl AnyEvent {
    /// Attempt to downcast to a specific event type.
    pub fn downcast<T: Send + Sync + 'static>(&self) -> Option<Arc<T>> {
        self.value.clone().downcast::<T>().ok()
    }

    /// Attempt to get a reference to a specific event type.
    pub fn downcast_ref<T: Send + Sync + 'static>(&self) -> Option<&T> {
        self.value.downcast_ref::<T>()
    }

    /// Check if this event is of a specific type.
    pub fn is<T: 'static>(&self) -> bool {
        self.type_id == TypeId::of::<T>()
    }
}

/// An effect handler - no traits, just data with closures.
///
/// Effects react to events and can perform side effects like I/O,
/// emit new events, or spawn background tasks.
pub struct Effect<S, D>
where
    S: Clone + Send + Sync + 'static,
    D: Send + Sync + 'static,
{
    /// Determines if this effect handles the given event type.
    /// Returns true if this effect should receive events of the given TypeId.
    pub(crate) can_handle: Arc<dyn Fn(TypeId) -> bool + Send + Sync>,

    /// Called once when the store is activated.
    /// Used for setup, spawning background tasks, etc.
    pub(crate) started: Option<Arc<dyn Fn(EffectContext<S, D>) -> BoxFuture<Result<()>> + Send + Sync>>,

    /// Called for each event that passes `can_handle`.
    /// Receives the type-erased event, its TypeId, and the effect context.
    pub(crate) handler: Arc<
        dyn Fn(Arc<dyn Any + Send + Sync>, TypeId, EffectContext<S, D>) -> BoxFuture<Result<()>>
            + Send
            + Sync,
    >,
}

impl<S, D> Clone for Effect<S, D>
where
    S: Clone + Send + Sync + 'static,
    D: Send + Sync + 'static,
{
    fn clone(&self) -> Self {
        Self {
            can_handle: self.can_handle.clone(),
            started: self.started.clone(),
            handler: self.handler.clone(),
        }
    }
}

impl<S, D> Effect<S, D>
where
    S: Clone + Send + Sync + 'static,
    D: Send + Sync + 'static,
{
    /// Check if this effect handles the given event type.
    pub fn can_handle(&self, type_id: TypeId) -> bool {
        (self.can_handle)(type_id)
    }

    /// Call the started handler if present.
    pub async fn call_started(&self, ctx: EffectContext<S, D>) -> Result<()> {
        if let Some(ref started) = self.started {
            started(ctx).await
        } else {
            Ok(())
        }
    }

    /// Call the event handler.
    pub async fn call_handler(
        &self,
        value: Arc<dyn Any + Send + Sync>,
        type_id: TypeId,
        ctx: EffectContext<S, D>,
    ) -> Result<()> {
        (self.handler)(value, type_id, ctx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_any_event_downcast() {
        let value: Arc<dyn Any + Send + Sync> = Arc::new(42i32);
        let event = AnyEvent {
            value,
            type_id: TypeId::of::<i32>(),
        };

        assert!(event.is::<i32>());
        assert!(!event.is::<String>());

        let downcasted = event.downcast::<i32>();
        assert!(downcasted.is_some());
        assert_eq!(*downcasted.unwrap(), 42);

        let wrong_type = event.downcast::<String>();
        assert!(wrong_type.is_none());
    }

    #[test]
    fn test_any_event_downcast_ref() {
        let value: Arc<dyn Any + Send + Sync> = Arc::new("hello".to_string());
        let event = AnyEvent {
            value,
            type_id: TypeId::of::<String>(),
        };

        let downcasted = event.downcast_ref::<String>();
        assert!(downcasted.is_some());
        assert_eq!(downcasted.unwrap(), "hello");
    }
}
