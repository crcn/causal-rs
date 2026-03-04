//! Core effect types.

use std::any::{Any, TypeId};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Error, Result};
use uuid::Uuid;

use super::context::Context;
use crate::event_codec::EventCodec;

/// Error handler called when an effect returns an error.
///
/// The handler receives the error, event type that caused it, and context.
/// It can emit failure events or log the error. The chain continues regardless.
pub type ErrorHandler<D> = Arc<dyn Fn(Error, TypeId, Context<D>) -> BoxFuture<()> + Send + Sync>;

/// Metadata passed to DLQ terminal mappers when an effect exhausts retries.
#[derive(Debug, Clone)]
pub struct DlqTerminalInfo {
    pub handler_id: String,
    pub source_event_type: String,
    pub source_event_id: Uuid,
    pub error: String,
    pub reason: String,
    pub attempts: i32,
    pub max_attempts: i32,
}

/// Global DLQ mapper that converts any handler DLQ into a domain event.
pub type GlobalDlqMapper = Arc<
    dyn Fn(DlqTerminalInfo) -> Result<crate::EmittedEvent> + Send + Sync,
>;

/// Alias for DLQ terminal mapper metadata used by macro APIs.
pub type ErrorContext = DlqTerminalInfo;

/// Optional mapper that converts exhausted effect failures into terminal events.
pub type DlqTerminalMapper = Arc<
    dyn Fn(Arc<dyn Any + Send + Sync>, TypeId, DlqTerminalInfo) -> Result<crate::EmittedEvent>
        + Send
        + Sync,
>;

/// A boxed future for async effect handlers.
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// Event emission result from effect handlers.
#[derive(Debug, Clone)]
pub enum Emit<E> {
    None,
    One(E),
    Batch(Vec<E>),
}

impl<E> Emit<E> {
    pub fn into_vec(self) -> Vec<E> {
        match self {
            Emit::None => Vec::new(),
            Emit::One(event) => vec![event],
            Emit::Batch(events) => events,
        }
    }
}

impl<E> From<E> for Emit<E> {
    fn from(value: E) -> Self {
        Emit::One(value)
    }
}

impl<E> From<Option<E>> for Emit<E> {
    fn from(value: Option<E>) -> Self {
        match value {
            Some(event) => Emit::One(event),
            None => Emit::None,
        }
    }
}

/// Join mode for queued batch fan-in effects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinMode {
    SameBatch,
}

/// Output from an effect handler that returns an event.
///
/// The payload is eagerly serialized at creation time, making `EventOutput`
/// trivially journalable for durable execution.
#[derive(Clone)]
pub struct EventOutput {
    /// The TypeId of the event type.
    pub type_id: TypeId,
    /// Fully-qualified Rust type name of the event.
    pub event_type: String,
    /// Eagerly-serialized event payload.
    pub payload: serde_json::Value,
    /// Codec for automatic registration (None for replayed/reconstructed outputs).
    pub(crate) codec: Option<Arc<EventCodec>>,
}

impl EventOutput {
    /// Create a new EventOutput from a typed event.
    pub fn new<E: Send + Sync + serde::Serialize + serde::de::DeserializeOwned + 'static>(event: E) -> Self {
        let payload = serde_json::to_value(&event).expect("Event must be serializable");
        Self {
            type_id: TypeId::of::<E>(),
            event_type: std::any::type_name::<E>().to_string(),
            payload,
            codec: Some(Arc::new(EventCodec {
                event_type: std::any::type_name::<E>().to_string(),
                type_id: TypeId::of::<E>(),
                decode: Arc::new(|payload| {
                    let event: E = serde_json::from_value(payload.clone())?;
                    Ok(Arc::new(event))
                }),
            })),
        }
    }

    /// Reconstruct an EventOutput from a serialized form (e.g. from a durable execution journal).
    ///
    /// The reconstructed output has no codec — codec registration is only needed on first
    /// execution, not on replay.
    pub fn from_serialized(event_type: String, payload: serde_json::Value) -> Self {
        Self {
            type_id: TypeId::of::<()>(),
            event_type,
            payload,
            codec: None,
        }
    }
}

/// The universal return type for all handlers.
///
/// Use the [`events!`](crate::events) macro:
///
/// ```ignore
/// // Single event
/// Ok(events![OrderShipped { order_id }])
///
/// // Multiple heterogeneous events
/// Ok(events![ScrapeEvent { data }, LifecycleEvent::PhaseCompleted])
///
/// // Fan-out batch
/// Ok(events![..items])
///
/// // Nothing
/// Ok(events![])
/// ```
#[derive(Clone, Default)]
pub struct Events {
    pub(crate) outputs: Vec<EventOutput>,
}

impl Events {
    /// Create an empty event collection.
    pub fn new() -> Self {
        Self {
            outputs: Vec::new(),
        }
    }

    /// Add a single event to the collection (builder-style, chainable).
    pub fn add<E: Send + Sync + serde::Serialize + serde::de::DeserializeOwned + 'static>(mut self, event: E) -> Self {
        self.outputs.push(EventOutput::new(event));
        self
    }

    /// Add a single event to the collection (Vec-style, in-place).
    pub fn push<E: Send + Sync + serde::Serialize + serde::de::DeserializeOwned + 'static>(&mut self, event: E) {
        self.outputs.push(EventOutput::new(event));
    }

    /// Append all events from another `Events` collection.
    pub fn extend(&mut self, other: Events) {
        self.outputs.extend(other.outputs);
    }

    /// Returns the number of events in the collection.
    pub fn len(&self) -> usize {
        self.outputs.len()
    }

    /// Returns true if the collection contains no events.
    pub fn is_empty(&self) -> bool {
        self.outputs.is_empty()
    }

    /// Add all items from an iterator as individual events (fan-out).
    pub fn batch<E: Send + Sync + serde::Serialize + serde::de::DeserializeOwned + 'static>(items: impl IntoIterator<Item = E>) -> Self {
        Self {
            outputs: items.into_iter().map(EventOutput::new).collect(),
        }
    }

    /// Convert into the internal `Vec<EventOutput>`.
    pub fn into_outputs(self) -> Vec<EventOutput> {
        self.outputs
    }
}

/// Trait for converting handler return values into [`Events`].
///
/// The blanket impl converts any `T: Send + Sync + 'static` into a single-event `Events`.
/// Specialized impls exist for `Events` (pass-through), `()` (empty), `Emit<T>` (legacy).
///
/// Used by the proc macro to wrap user function return values.
pub trait IntoEvents {
    fn into_events(self) -> Events;
}

impl IntoEvents for Events {
    fn into_events(self) -> Events {
        self
    }
}

impl IntoEvents for () {
    fn into_events(self) -> Events {
        Events::new()
    }
}

impl<E: Send + Sync + serde::Serialize + serde::de::DeserializeOwned + 'static> IntoEvents for Emit<E> {
    fn into_events(self) -> Events {
        if TypeId::of::<E>() == TypeId::of::<()>() {
            return Events::new();
        }
        Events {
            outputs: self
                .into_vec()
                .into_iter()
                .map(EventOutput::new)
                .collect(),
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

/// A projection handler — receives ALL events, returns `Result<()>`, runs sequentially.
///
/// Projections are fundamentally different from effects:
/// - They receive all events (no type routing)
/// - They return `Result<()>` (no emitted events)
/// - They run sequentially before other handlers
/// - They have no retry/timeout/delay/join semantics
pub struct Projection<D>
where
    D: Send + Sync + 'static,
{
    /// Human-readable identifier.
    pub id: String,
    /// The projection handler — receives ALL events as AnyEvent.
    pub(crate) handler: Arc<dyn Fn(AnyEvent, Context<D>) -> BoxFuture<Result<()>> + Send + Sync>,
    /// Execution priority (lower = runs first).
    pub(crate) priority: Option<i32>,
}

impl<D> Clone for Projection<D>
where
    D: Send + Sync + 'static,
{
    fn clone(&self) -> Self {
        Self {
            id: self.id.clone(),
            handler: self.handler.clone(),
            priority: self.priority,
        }
    }
}

/// An effect handler - no traits, just data with closures.
pub struct Handler<D>
where
    D: Send + Sync + 'static,
{
    /// Human-readable identifier for this effect.
    pub id: String,

    /// Queue codec metadata for typed event handling/serialization.
    pub(crate) codecs: Vec<std::sync::Arc<EventCodec>>,

    /// Determines if this effect handles the given event type.
    pub(crate) can_handle: Arc<dyn Fn(TypeId) -> bool + Send + Sync>,

    /// Called once when the store is activated.
    pub(crate) started: Option<Arc<dyn Fn(Context<D>) -> BoxFuture<Result<()>> + Send + Sync>>,

    /// Called for each event that passes `can_handle`.
    pub(crate) handler: Arc<
        dyn Fn(
                Arc<dyn Any + Send + Sync>,
                TypeId,
                Context<D>,
            ) -> BoxFuture<Result<Vec<EventOutput>>>
            + Send
            + Sync,
    >,

    /// Optional join mode - when set, effect executions are accumulated and
    /// flushed in durable windows before invoking `join_batch_handler`.
    pub(crate) join_mode: Option<JoinMode>,

    /// Optional batch handler for join modes.
    pub(crate) join_batch_handler: Option<
        Arc<
            dyn Fn(
                    Vec<Arc<dyn Any + Send + Sync>>,
                    Context<D>,
                ) -> BoxFuture<Result<Vec<EventOutput>>>
                + Send
                + Sync,
        >,
    >,

    /// Optional timeout for same-batch accumulation windows.
    pub(crate) join_window_timeout: Option<Duration>,

    /// Optional mapper for creating terminal events when an execution moves to DLQ.
    pub(crate) dlq_terminal_mapper: Option<DlqTerminalMapper>,

    // Execution configuration (determines inline vs queued)
    /// Force queued execution (default: false = inline)
    pub(crate) queued: bool,
    /// Delay before execution (triggers queued)
    pub(crate) delay: Option<Duration>,
    /// Execution timeout (triggers queued)
    pub(crate) timeout: Option<Duration>,
    /// Maximum retry attempts (default: 1 = no retry = inline)
    pub(crate) max_attempts: u32,
    /// Exponential backoff base duration for retries.
    pub(crate) backoff: Option<Duration>,
    /// Execution priority (lower = higher priority).
    pub(crate) priority: Option<i32>,
}

impl<D> Clone for Handler<D>
where
    D: Send + Sync + 'static,
{
    fn clone(&self) -> Self {
        Self {
            id: self.id.clone(),
            codecs: self.codecs.clone(),
            can_handle: self.can_handle.clone(),
            started: self.started.clone(),
            handler: self.handler.clone(),
            join_mode: self.join_mode,
            join_batch_handler: self.join_batch_handler.clone(),
            join_window_timeout: self.join_window_timeout,
            dlq_terminal_mapper: self.dlq_terminal_mapper.clone(),
            queued: self.queued,
            delay: self.delay,
            timeout: self.timeout,
            max_attempts: self.max_attempts,
            backoff: self.backoff,
            priority: self.priority,
        }
    }
}

impl<D> Handler<D>
where
    D: Send + Sync + 'static,
{
    /// Check if this effect handles the given event type.
    pub fn can_handle(&self, type_id: TypeId) -> bool {
        (self.can_handle)(type_id)
    }

    /// Call the started handler if present.
    pub async fn call_started(&self, ctx: Context<D>) -> Result<()> {
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
        ctx: Context<D>,
    ) -> Result<Vec<EventOutput>> {
        (self.handler)(value, type_id, ctx).await
    }

    /// Create the handler future without awaiting it.
    ///
    /// Returns a `'static` future suitable for direct execution.
    pub fn make_handler_future(
        &self,
        value: Arc<dyn Any + Send + Sync>,
        type_id: TypeId,
        ctx: Context<D>,
    ) -> BoxFuture<Result<Vec<EventOutput>>> {
        (self.handler)(value, type_id, ctx)
    }

    /// Call the join batch handler if configured.
    pub async fn call_join_batch_handler(
        &self,
        values: Vec<Arc<dyn Any + Send + Sync>>,
        ctx: Context<D>,
    ) -> Result<Vec<EventOutput>> {
        if let Some(handler) = &self.join_batch_handler {
            handler(values, ctx).await
        } else {
            Ok(Vec::new())
        }
    }

    /// Internal queue codec metadata.
    pub(crate) fn codecs(&self) -> &[std::sync::Arc<EventCodec>] {
        &self.codecs
    }

    /// Check if this effect should execute inline.
    pub fn is_inline(&self) -> bool {
        !self.queued
            && self.delay.is_none()
            && self.timeout.is_none()
            && self.max_attempts == 1
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

        let downcasted = event.downcast::<i32>();
        assert!(downcasted.is_some());
        assert_eq!(*downcasted.unwrap(), 42);
        assert!(event.is::<i32>());
        assert!(!event.is::<String>());
    }

    #[test]
    fn test_emit_conversions() {
        let none: Emit<i32> = Emit::None;
        assert_eq!(none.into_vec().len(), 0);

        let one: Emit<i32> = 42.into();
        assert_eq!(one.into_vec(), vec![42]);

        let opt_some: Emit<i32> = Some(42).into();
        assert_eq!(opt_some.into_vec(), vec![42]);

        let opt_none: Emit<i32> = Option::<i32>::None.into();
        assert_eq!(opt_none.into_vec().len(), 0);

        let vec_emit: Emit<i32> = Emit::Batch(vec![1, 2, 3]);
        assert_eq!(vec_emit.into_vec(), vec![1, 2, 3]);
    }

    #[test]
    fn test_event_output_debug() {
        let output = EventOutput::new(42i32);
        let debug_str = format!("{:?}", output);
        assert!(debug_str.contains("EventOutput"));
        assert!(debug_str.contains("event_type"));
    }
}
