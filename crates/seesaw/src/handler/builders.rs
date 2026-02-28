//! Effect builder functions and types.

use std::any::TypeId;
use std::future::Future;
use std::marker::PhantomData;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;

use super::context::Context;
use super::types::{AnyEvent, BoxFuture, DlqTerminalInfo, Emit, EventOutput, Handler, JoinMode};
use crate::event_codec::EventCodec;

#[track_caller]
fn default_effect_id(prefix: &str) -> String {
    let location = std::panic::Location::caller();
    format!(
        "{prefix}@{}:{}:{}",
        location.file(),
        location.line(),
        location.column()
    )
}

fn emit_to_outputs<E: Send + Sync + 'static>(emit: Emit<E>) -> Vec<EventOutput> {
    if TypeId::of::<E>() == TypeId::of::<()>() {
        return Vec::new();
    }

    emit.into_vec()
        .into_iter()
        .map(EventOutput::new)
        .collect::<Vec<_>>()
}

/// Marker for typed event effects (`on::<E>()`).
pub struct Typed<E>(PhantomData<E>);

/// Marker for any event effects (`on_any()`).
pub struct Untyped;

/// Marker for no filter.
pub struct NoFilter;

/// Marker for having a filter predicate.
pub struct WithFilter<F>(F);

/// Marker for having a filter_map predicate.
pub struct WithFilterMap<F, T>(F, PhantomData<T>);

/// Marker for no started handler.
pub struct NoStarted;

/// Marker for having a started handler.
pub struct WithStarted<D, St>(St, PhantomData<D>);

fn typed_event_codec<E>() -> Arc<EventCodec>
where
    E: Clone + Send + Sync + 'static + serde::Serialize + serde::de::DeserializeOwned,
{
    Arc::new(EventCodec {
        event_type: std::any::type_name::<E>().to_string(),
        type_id: TypeId::of::<E>(),
        decode: Arc::new(|payload| {
            let event: E = serde_json::from_value(payload.clone())?;
            Ok(Arc::new(event))
        }),
        encode: Arc::new(|event_any| {
            event_any
                .downcast_ref::<E>()
                .and_then(|event| serde_json::to_value(event).ok())
        }),
    })
}

trait QueueCodecProvider {
    fn queue_codec() -> Option<Arc<EventCodec>>;
}

impl QueueCodecProvider for Untyped {
    fn queue_codec() -> Option<Arc<EventCodec>> {
        None
    }
}

impl<E> QueueCodecProvider for Typed<E>
where
    E: Clone + Send + Sync + 'static + serde::Serialize + serde::de::DeserializeOwned,
{
    fn queue_codec() -> Option<Arc<EventCodec>> {
        Some(typed_event_codec::<E>())
    }
}

/// A unified builder for effects using a compile-time type-phase pattern.
pub struct HandlerBuilder<EventType, Filter, Started> {
    filter: Filter,
    started: Started,
    id: Option<String>,
    queued: bool,
    delay: Option<Duration>,
    timeout: Option<Duration>,
    join_window: Option<Duration>,
    max_attempts: u32,
    priority: Option<i32>,
    codec: Option<Arc<EventCodec>>,
    dlq_terminal_mapper: Option<super::types::DlqTerminalMapper>,
    _marker: PhantomData<EventType>,
}

/// Create an effect that handles a specific event type.
pub fn on<E>() -> HandlerBuilder<Typed<E>, NoFilter, NoStarted>
where
    E: Clone + Send + Sync + 'static + serde::Serialize + serde::de::DeserializeOwned,
{
    HandlerBuilder {
        filter: NoFilter,
        started: NoStarted,
        id: None,
        queued: false,
        delay: None,
        timeout: None,
        join_window: None,
        max_attempts: 1,
        priority: None,
        codec: Some(typed_event_codec::<E>()),
        dlq_terminal_mapper: None,
        _marker: PhantomData,
    }
}

/// Create an effect that handles all events (observer pattern).
pub fn on_any() -> HandlerBuilder<Untyped, NoFilter, NoStarted> {
    HandlerBuilder {
        filter: NoFilter,
        started: NoStarted,
        id: None,
        queued: false,
        delay: None,
        timeout: None,
        join_window: None,
        max_attempts: 1,
        priority: None,
        codec: None,
        dlq_terminal_mapper: None,
        _marker: PhantomData,
    }
}

impl<E, Started> HandlerBuilder<Typed<E>, NoFilter, Started>
where
    E: Send + Sync + 'static,
{
    /// Add a filter predicate that must pass for the handler to run.
    pub fn filter<F>(self, predicate: F) -> HandlerBuilder<Typed<E>, WithFilter<F>, Started>
    where
        F: Fn(&E) -> bool + Send + Sync + 'static,
    {
        HandlerBuilder {
            filter: WithFilter(predicate),
            started: self.started,
            id: self.id,
            queued: self.queued,
            delay: self.delay,
            timeout: self.timeout,
            join_window: self.join_window,
            max_attempts: self.max_attempts,
            priority: self.priority,
            codec: self.codec,
            dlq_terminal_mapper: self.dlq_terminal_mapper,
            _marker: PhantomData,
        }
    }

    /// Extract data from events. Handler receives the extracted value.
    pub fn extract<F, T>(
        self,
        extractor: F,
    ) -> HandlerBuilder<Typed<E>, WithFilterMap<F, T>, Started>
    where
        F: Fn(&E) -> Option<T> + Send + Sync + 'static,
        T: Clone + Send + Sync + 'static,
    {
        HandlerBuilder {
            filter: WithFilterMap(extractor, PhantomData),
            started: self.started,
            id: self.id,
            queued: self.queued,
            delay: self.delay,
            timeout: self.timeout,
            join_window: self.join_window,
            max_attempts: self.max_attempts,
            priority: self.priority,
            codec: self.codec,
            dlq_terminal_mapper: self.dlq_terminal_mapper,
            _marker: PhantomData,
        }
    }
}

impl<EventType, Filter> HandlerBuilder<EventType, Filter, NoStarted> {
    /// Add an init handler that runs when the store is activated.
    pub fn init<D, St, StFut>(
        self,
        started: St,
    ) -> HandlerBuilder<EventType, Filter, WithStarted<D, St>>
    where
        D: Send + Sync + 'static,
        St: Fn(Context<D>) -> StFut + Send + Sync + 'static,
        StFut: Future<Output = Result<()>> + Send + 'static,
    {
        HandlerBuilder {
            filter: self.filter,
            started: WithStarted(started, PhantomData),
            id: self.id,
            queued: self.queued,
            delay: self.delay,
            timeout: self.timeout,
            join_window: self.join_window,
            max_attempts: self.max_attempts,
            priority: self.priority,
            codec: self.codec,
            dlq_terminal_mapper: self.dlq_terminal_mapper,
            _marker: PhantomData,
        }
    }
}

impl<EventType, Filter, Started> HandlerBuilder<EventType, Filter, Started> {
    /// Set a custom ID for this effect (default: auto-generated).
    pub fn id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }
}

impl<E, Filter, Started> HandlerBuilder<Typed<E>, Filter, Started>
where
    E: Clone + Send + Sync + 'static,
{
    /// Map exhausted retries/timeouts to a failure event.
    pub fn on_failure<O, M>(mut self, mapper: M) -> Self
    where
        O: serde::Serialize + Send + Sync + 'static,
        M: Fn(Arc<E>, DlqTerminalInfo) -> O + Send + Sync + 'static,
    {
        let output_type = std::any::type_name::<O>().to_string();
        self.dlq_terminal_mapper = Some(Arc::new(move |source_any, source_type, info| {
            if source_type != TypeId::of::<E>() {
                anyhow::bail!(
                    "on_failure source type mismatch: expected {}",
                    std::any::type_name::<E>()
                );
            }

            let typed = source_any.downcast::<E>().map_err(|_| {
                anyhow::anyhow!(
                    "on_failure source downcast failed for {}",
                    std::any::type_name::<E>()
                )
            })?;

            Ok(crate::EmittedEvent {
                event_type: output_type.clone(),
                payload: serde_json::to_value(mapper(typed, info))?,
                batch_id: None,
                batch_index: None,
                batch_size: None,
            })
        }));
        self
    }
}

#[allow(private_bounds)]
impl<EventType, Filter, Started> HandlerBuilder<EventType, Filter, Started>
where
    EventType: QueueCodecProvider,
{
    /// Set a timeout for accumulation windows.
    pub fn window(mut self, duration: Duration) -> Self {
        self.join_window = Some(duration);
        self
    }

    /// Force queued execution (default: inline).
    pub fn queued(mut self) -> Self {
        self.queued = true;
        self.codec = EventType::queue_codec();
        self
    }

    /// Add a delay before execution (triggers queued execution).
    pub fn delayed(mut self, duration: Duration) -> Self {
        self.delay = Some(duration);
        self.codec = EventType::queue_codec();
        self
    }

    /// Set execution timeout (triggers queued execution).
    pub fn timeout(mut self, duration: Duration) -> Self {
        self.timeout = Some(duration);
        self.codec = EventType::queue_codec();
        self
    }

    /// Set maximum retry attempts (default: 1 = no retry).
    pub fn retry(mut self, attempts: u32) -> Self {
        self.max_attempts = attempts;
        if attempts > 1 {
            self.codec = EventType::queue_codec();
        }
        self
    }

    /// Set execution priority (lower = higher priority).
    ///
    /// For inline handlers, lower priority runs first.
    /// For queued handlers, lower priority is polled first.
    pub fn priority(mut self, level: i32) -> Self {
        self.priority = Some(level);
        self
    }
}

/// Builder for durable same-batch join effects.
pub struct JoinHandlerBuilder<E, Started> {
    inner: HandlerBuilder<Typed<E>, NoFilter, Started>,
    mode: JoinMode,
}

impl<E, Started> HandlerBuilder<Typed<E>, NoFilter, Started>
where
    E: Clone + Send + Sync + 'static,
{
    /// Configure this effect as a durable accumulation effect.
    pub fn accumulate(self) -> JoinHandlerBuilder<E, Started> {
        JoinHandlerBuilder {
            inner: self,
            mode: JoinMode::SameBatch,
        }
    }
}

impl<E, Started> JoinHandlerBuilder<E, Started>
where
    E: Clone + Send + Sync + 'static + serde::Serialize + serde::de::DeserializeOwned,
{
    /// Join terminal events that share the same `(correlation_id, batch_id)`.
    pub fn same_batch(mut self) -> Self {
        self.mode = JoinMode::SameBatch;
        self
    }

    /// Set a timeout for this accumulation window.
    pub fn window(mut self, duration: Duration) -> Self {
        self.inner.join_window = Some(duration);
        self
    }

    /// Set the handler for joined batch execution.
    #[track_caller]
    #[allow(private_bounds)]
    pub fn then<D, H, Fut, R, O>(mut self, handler: H) -> Handler<D>
    where
        D: Send + Sync + 'static,
        Started: StartedHandler<D>,
        H: Fn(Vec<E>, Context<D>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<R>> + Send + 'static,
        R: Into<Emit<O>> + Send + 'static,
        O: Clone + Send + Sync + 'static + serde::Serialize + serde::de::DeserializeOwned,
    {
        let target = TypeId::of::<E>();
        let id = self
            .inner
            .id
            .take()
            .unwrap_or_else(|| default_effect_id(std::any::type_name::<E>()));
        let join_mode = self.mode;
        let input_codec = self
            .inner
            .codec
            .take()
            .unwrap_or_else(typed_event_codec::<E>);
        let output_codec = typed_event_codec::<O>();

        let mut codecs = vec![input_codec];
        // Register output codec if it differs from the input
        if TypeId::of::<O>() != TypeId::of::<E>() && TypeId::of::<O>() != TypeId::of::<()>() {
            codecs.push(output_codec);
        }

        Handler {
            id,
            codecs,
            can_handle: Arc::new(move |t| t == target),
            started: self.inner.started.into_started(),
            handler: Arc::new(move |_, _, _| Box::pin(async { Ok(Vec::new()) })),
            join_mode: Some(join_mode),
            join_batch_handler: Some(Arc::new(move |values, ctx| {
                let mut typed = Vec::with_capacity(values.len());
                for value in values {
                    let Ok(downcasted) = value.downcast::<E>() else {
                        return Box::pin(async {
                            Err(anyhow::anyhow!("join batch item downcast failed"))
                        });
                    };
                    typed.push((*downcasted).clone());
                }

                let fut = handler(typed, ctx);
                Box::pin(async move {
                    let output: R = fut.await?;
                    let emit: Emit<O> = output.into();
                    Ok(emit_to_outputs(emit))
                })
            })),
            join_window_timeout: self.inner.join_window,
            dlq_terminal_mapper: self.inner.dlq_terminal_mapper.take(),
            queued: true,
            delay: self.inner.delay,
            timeout: self.inner.timeout,
            max_attempts: self.inner.max_attempts.max(1),
            priority: self.inner.priority,
        }
    }
}

/// Trait for extracting a value from an event (handles filter/filter_map).
trait Extractor<E, T>: Send + Sync + 'static {
    fn extract(&self, event: &E) -> Option<T>;
}

impl<E: Clone + Send + Sync + 'static> Extractor<E, Arc<E>> for NoFilter {
    fn extract(&self, event: &E) -> Option<Arc<E>> {
        Some(Arc::new(event.clone()))
    }
}

impl<E, F> Extractor<E, Arc<E>> for WithFilter<F>
where
    E: Clone + Send + Sync + 'static,
    F: Fn(&E) -> bool + Send + Sync + 'static,
{
    fn extract(&self, event: &E) -> Option<Arc<E>> {
        if (self.0)(event) {
            Some(Arc::new(event.clone()))
        } else {
            None
        }
    }
}

impl<E, F, T> Extractor<E, T> for WithFilterMap<F, T>
where
    E: Send + Sync + 'static,
    F: Fn(&E) -> Option<T> + Send + Sync + 'static,
    T: Clone + Send + Sync + 'static,
{
    fn extract(&self, event: &E) -> Option<T> {
        (self.0)(event)
    }
}

/// Trait for optional started handlers.
trait StartedHandler<D>: Send + Sync + 'static
where
    D: Send + Sync + 'static,
{
    fn into_started(self)
        -> Option<Arc<dyn Fn(Context<D>) -> BoxFuture<Result<()>> + Send + Sync>>;
}

impl<D> StartedHandler<D> for NoStarted
where
    D: Send + Sync + 'static,
{
    fn into_started(
        self,
    ) -> Option<Arc<dyn Fn(Context<D>) -> BoxFuture<Result<()>> + Send + Sync>> {
        None
    }
}

impl<D, St, StFut> StartedHandler<D> for WithStarted<D, St>
where
    D: Send + Sync + 'static,
    St: Fn(Context<D>) -> StFut + Send + Sync + 'static,
    StFut: Future<Output = Result<()>> + Send + 'static,
{
    fn into_started(
        self,
    ) -> Option<Arc<dyn Fn(Context<D>) -> BoxFuture<Result<()>> + Send + Sync>> {
        let started = self.0;
        Some(Arc::new(move |ctx| Box::pin(started(ctx))))
    }
}

impl<E, Filter, Started> HandlerBuilder<Typed<E>, Filter, Started>
where
    E: Clone + Send + Sync + 'static + serde::Serialize + serde::de::DeserializeOwned,
{
    /// Set the handler that returns the next event (terminal operation).
    #[track_caller]
    #[allow(private_bounds)]
    pub fn then<D, T, H, Fut, R, O>(self, handler: H) -> Handler<D>
    where
        D: Send + Sync + 'static,
        T: Clone + Send + 'static,
        Filter: Extractor<E, T>,
        Started: StartedHandler<D>,
        H: Fn(T, Context<D>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<R>> + Send + 'static,
        R: Into<Emit<O>> + Send + 'static,
        O: Send + Sync + 'static,
    {
        let target = TypeId::of::<E>();
        let filter = self.filter;
        // Always register input codec so Engine::decode_event works for both inline and queued handlers
        let input_codec = self.codec.unwrap_or_else(typed_event_codec::<E>);
        let dlq_terminal_mapper = self.dlq_terminal_mapper;

        let id = self
            .id
            .unwrap_or_else(|| default_effect_id(std::any::type_name::<E>()));

        Handler {
            id,
            codecs: vec![input_codec],
            can_handle: Arc::new(move |t| t == target),
            started: self.started.into_started(),
            handler: Arc::new(move |value, _, ctx| {
                let typed = value.downcast::<E>().expect("type checked by can_handle");

                match filter.extract(&typed) {
                    Some(extracted) => {
                        let fut = handler(extracted, ctx);
                        Box::pin(async move {
                            let output: R = fut.await?;
                            let emit: Emit<O> = output.into();
                            Ok(emit_to_outputs(emit))
                        })
                    }
                    None => Box::pin(async { Ok(Vec::new()) }),
                }
            }),
            join_mode: None,
            join_batch_handler: None,
            join_window_timeout: self.join_window,
            dlq_terminal_mapper,
            queued: self.queued,
            delay: self.delay,
            timeout: self.timeout,
            max_attempts: self.max_attempts,
            priority: self.priority,
        }
    }
}

impl HandlerBuilder<Untyped, NoFilter, NoStarted> {
    /// Set the handler for observing all events.
    #[track_caller]
    pub fn then<D, H, Fut>(self, handler: H) -> Handler<D>
    where
        D: Send + Sync + 'static,
        H: Fn(AnyEvent, Context<D>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        let id = self.id.unwrap_or_else(|| default_effect_id("effect_any"));

        Handler {
            id,
            codecs: Vec::new(),
            can_handle: Arc::new(|_| true),
            started: None,
            handler: Arc::new(move |value, type_id, ctx| {
                let event = AnyEvent { value, type_id };
                let fut = handler(event, ctx);
                Box::pin(async move {
                    fut.await?;
                    Ok(Vec::new())
                })
            }),
            join_mode: None,
            join_batch_handler: None,
            join_window_timeout: self.join_window,
            dlq_terminal_mapper: None,
            queued: self.queued,
            delay: self.delay,
            timeout: self.timeout,
            max_attempts: self.max_attempts,
            priority: self.priority,
        }
    }
}

impl<D, St, StFut> HandlerBuilder<Untyped, NoFilter, WithStarted<D, St>>
where
    D: Send + Sync + 'static,
    St: Fn(Context<D>) -> StFut + Send + Sync + 'static,
    StFut: Future<Output = Result<()>> + Send + 'static,
{
    /// Set the handler for observing all events with started hook.
    #[track_caller]
    pub fn then<H, Fut>(self, handler: H) -> Handler<D>
    where
        H: Fn(AnyEvent, Context<D>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        let id = self
            .id
            .unwrap_or_else(|| default_effect_id("effect_any_started"));
        let started = self.started.0;

        Handler {
            id,
            codecs: Vec::new(),
            can_handle: Arc::new(|_| true),
            started: Some(Arc::new(move |ctx| Box::pin(started(ctx)))),
            handler: Arc::new(move |value, type_id, ctx| {
                let event = AnyEvent { value, type_id };
                let fut = handler(event, ctx);
                Box::pin(async move {
                    fut.await?;
                    Ok(Vec::new())
                })
            }),
            join_mode: None,
            join_batch_handler: None,
            join_window_timeout: self.join_window,
            dlq_terminal_mapper: None,
            queued: self.queued,
            delay: self.delay,
            timeout: self.timeout,
            max_attempts: self.max_attempts,
            priority: self.priority,
        }
    }
}

// ── Transition guard builder ─────────────────────────────────────────────

use crate::aggregator::Aggregate;

/// Builder for handlers guarded by aggregate state transitions.
///
/// Created by calling `.transition::<A>(guard)` after `.extract()` on a
/// `HandlerBuilder`. The guard function receives `(&prev_state, &next_state)`
/// and returns `true` to fire the handler.
///
/// Aggregate state is replayed using the engine's aggregator registry —
/// no extra configuration needed on the guard.
pub struct TransitionHandlerBuilder<E, A, G, Started> {
    inner: HandlerBuilder<Typed<E>, NoFilter, Started>,
    extractor: Box<dyn Fn(&E) -> Option<uuid::Uuid> + Send + Sync>,
    guard: G,
    _aggregate: PhantomData<A>,
}

impl<E, F, Started> HandlerBuilder<Typed<E>, WithFilterMap<F, uuid::Uuid>, Started>
where
    E: Clone + Send + Sync + 'static,
    F: Fn(&E) -> Option<uuid::Uuid> + Send + Sync + 'static,
    Started: Send + Sync + 'static,
{
    /// Add a transition guard. The handler fires only when the aggregate
    /// state transitions in the way described by `guard(prev, next)`.
    ///
    /// The `.extract()` must return `Option<Uuid>` (the aggregate ID).
    /// Aggregate state is replayed using the engine's aggregator registry.
    pub fn transition<A, G>(self, guard: G) -> TransitionHandlerBuilder<E, A, G, Started>
    where
        A: Aggregate,
        G: Fn(&A, &A) -> bool + Send + Sync + 'static,
    {
        TransitionHandlerBuilder {
            inner: HandlerBuilder {
                filter: NoFilter,
                started: self.started,
                id: self.id,
                queued: self.queued,
                delay: self.delay,
                timeout: self.timeout,
                join_window: self.join_window,
                max_attempts: self.max_attempts,
                priority: self.priority,
                codec: self.codec,
                dlq_terminal_mapper: self.dlq_terminal_mapper,
                _marker: PhantomData,
            },
            extractor: Box::new(self.filter.0),
            guard,
            _aggregate: PhantomData,
        }
    }
}

impl<E, A, G, Started> TransitionHandlerBuilder<E, A, G, Started>
where
    E: Clone + Send + Sync + 'static,
    A: Aggregate,
    G: Fn(&A, &A) -> bool + Send + Sync + 'static,
{
    /// Set a custom ID for this handler.
    pub fn id(mut self, id: impl Into<String>) -> Self {
        self.inner.id = Some(id.into());
        self
    }

    /// Set the handler that runs when the transition guard passes.
    ///
    /// The engine's aggregator registry provides live `(prev, next)` state.
    #[track_caller]
    #[allow(private_bounds)]
    pub fn then<D, H, Fut, R, O>(self, handler: H) -> Handler<D>
    where
        D: Send + Sync + 'static,
        Started: StartedHandler<D>,
        H: Fn(uuid::Uuid, Context<D>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<R>> + Send + 'static,
        R: Into<Emit<O>> + Send + 'static,
        O: Send + Sync + 'static,
    {
        let target = TypeId::of::<E>();
        let extractor: Arc<dyn Fn(&E) -> Option<uuid::Uuid> + Send + Sync> =
            Arc::from(self.extractor);
        let guard = Arc::new(self.guard);
        let handler = Arc::new(handler);

        let id = self
            .inner
            .id
            .unwrap_or_else(|| default_effect_id(std::any::type_name::<E>()));

        let codec = self.inner.codec;

        Handler {
            id,
            codecs: codec.into_iter().collect(),
            can_handle: Arc::new(move |t| t == target),
            started: self.inner.started.into_started(),
            handler: Arc::new(move |value, _, ctx: Context<D>| {
                let typed = value.downcast::<E>().expect("type checked by can_handle");
                let extractor = extractor.clone();
                let guard = guard.clone();
                let handler = handler.clone();

                match extractor(&typed) {
                    Some(aggregate_id) => {
                        Box::pin(async move {
                            let registry = ctx.aggregator_registry().ok_or_else(|| {
                                anyhow::anyhow!(
                                    "transition guard requires aggregator registry on context"
                                )
                            })?;
                            let runtime = ctx.runtime().ok_or_else(|| {
                                anyhow::anyhow!(
                                    "transition guard requires runtime on context"
                                )
                            })?;

                            let (prev, next) = registry.get_transition::<A>(aggregate_id, runtime);

                            if !guard(&prev, &next) {
                                return Ok(Vec::new());
                            }

                            let result: R = handler(aggregate_id, ctx).await?;
                            let emit: Emit<O> = result.into();
                            Ok(emit_to_outputs(emit))
                        })
                    }
                    None => Box::pin(async { Ok(Vec::new()) }),
                }
            }),
            join_mode: None,
            join_batch_handler: None,
            join_window_timeout: self.inner.join_window,
            dlq_terminal_mapper: self.inner.dlq_terminal_mapper,
            queued: self.inner.queued,
            delay: self.inner.delay,
            timeout: self.inner.timeout,
            max_attempts: self.inner.max_attempts,
            priority: self.inner.priority,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[derive(Clone, serde::Serialize, serde::Deserialize)]
    struct QueueEvent {
        value: i32,
    }

    #[derive(Clone)]
    struct Deps;

    #[test]
    fn filter_does_not_force_queued_execution() {
        let effect = on::<QueueEvent>()
            .id("filter_probe")
            .filter(|event| event.value > 0)
            .then(|_event: Arc<QueueEvent>, _ctx: Context<Deps>| async move { Ok(()) });

        assert!(
            effect.is_inline(),
            "filter() should not change inline/queued execution mode"
        );
    }
}
