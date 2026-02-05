//! Core effect types.

use std::any::{Any, TypeId};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Error, Result};

use crate::event_codec::EventCodec;
use super::context::EffectContext;

/// Error handler called when an effect returns an error.
///
/// The handler receives the error, event type that caused it, and context.
/// It can emit failure events or log the error. The chain continues regardless.
pub type ErrorHandler<S, D> =
    Arc<dyn Fn(Error, TypeId, EffectContext<S, D>) -> BoxFuture<()> + Send + Sync>;

/// A boxed future for async effect handlers.
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// Output from an effect handler that returns an event.
///
/// Used by `.then()` effects to return events for dispatch by the engine.
/// The event is type-erased but includes its TypeId for proper routing.
#[derive(Clone)]
pub struct EventOutput {
    /// The TypeId of the event type.
    pub type_id: TypeId,
    /// Fully-qualified Rust type name of the event.
    pub event_type: String,
    /// The type-erased event value.
    pub value: Arc<dyn Any + Send + Sync>,
}

impl EventOutput {
    /// Create a new EventOutput from a typed event.
    pub fn new<E: Send + Sync + 'static>(event: E) -> Self {
        Self {
            type_id: TypeId::of::<E>(),
            event_type: std::any::type_name::<E>().to_string(),
            value: Arc::new(event),
        }
    }
}

impl std::fmt::Debug for EventOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventOutput")
            .field("type_id", &self.type_id)
            .field("event_type", &self.event_type)
            .finish()
    }
}

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
/// and optionally return new events for dispatch.
pub struct Effect<S, D>
where
    S: Clone + Send + Sync + 'static,
    D: Send + Sync + 'static,
{
    /// Human-readable identifier for this effect.
    /// Used for debugging, tracing, and the `produced_by` field in event envelopes.
    pub id: String,

    /// Queue codec metadata for typed event handling/serialization.
    pub(crate) codecs: Vec<std::sync::Arc<EventCodec>>,

    /// Determines if this effect handles the given event type.
    /// Returns true if this effect should receive events of the given TypeId.
    pub(crate) can_handle: Arc<dyn Fn(TypeId) -> bool + Send + Sync>,

    /// Called once when the store is activated.
    /// Used for setup, spawning background tasks, etc.
    pub(crate) started:
        Option<Arc<dyn Fn(EffectContext<S, D>) -> BoxFuture<Result<()>> + Send + Sync>>,

    /// Called for each event that passes `can_handle`.
    /// Receives the type-erased event, its TypeId, and the effect context.
    /// Returns `Option<EventOutput>`:
    /// - `None`: Effect completed, no event to dispatch (observer pattern)
    /// - `Some(EventOutput)`: Effect completed, dispatch this event
    pub(crate) handler: Arc<
        dyn Fn(
                Arc<dyn Any + Send + Sync>,
                TypeId,
                EffectContext<S, D>,
            ) -> BoxFuture<Result<Option<EventOutput>>>
            + Send
            + Sync,
    >,

    // Execution configuration (determines inline vs queued)
    /// Force queued execution (default: false = inline)
    pub(crate) queued: bool,
    /// Delay before execution (triggers queued)
    pub(crate) delay: Option<Duration>,
    /// Execution timeout (triggers queued)
    pub(crate) timeout: Option<Duration>,
    /// Maximum retry attempts (default: 1 = no retry = inline)
    pub(crate) max_attempts: u32,
    /// Execution priority (lower = higher priority, triggers queued)
    pub(crate) priority: Option<i32>,
}

impl<S, D> Clone for Effect<S, D>
where
    S: Clone + Send + Sync + 'static,
    D: Send + Sync + 'static,
{
    fn clone(&self) -> Self {
        Self {
            id: self.id.clone(),
            codecs: self.codecs.clone(),
            can_handle: self.can_handle.clone(),
            started: self.started.clone(),
            handler: self.handler.clone(),
            queued: self.queued,
            delay: self.delay,
            timeout: self.timeout,
            max_attempts: self.max_attempts,
            priority: self.priority,
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
    /// Returns `Option<EventOutput>` - if `Some`, the engine should dispatch the returned event.
    pub async fn call_handler(
        &self,
        value: Arc<dyn Any + Send + Sync>,
        type_id: TypeId,
        ctx: EffectContext<S, D>,
    ) -> Result<Option<EventOutput>> {
        (self.handler)(value, type_id, ctx).await
    }

    /// Internal queue codec metadata.
    pub(crate) fn codecs(&self) -> &[std::sync::Arc<EventCodec>] {
        &self.codecs
    }

    /// Check if this effect should execute inline (in same transaction as event processing)
    ///
    /// Effects are inline by default. They become queued if:
    /// - `.queued()` was explicitly set
    /// - `.delayed()` was set
    /// - `.timeout()` was set
    /// - `.retry()` was set with attempts > 1
    /// - `.priority()` was set
    pub fn is_inline(&self) -> bool {
        !self.queued
            && self.delay.is_none()
            && self.timeout.is_none()
            && self.max_attempts == 1
            && self.priority.is_none()
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
