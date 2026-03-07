//! Handler builder functions and types.

use std::any::TypeId;
use std::future::Future;
use std::marker::PhantomData;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;

use super::context::Context;
use super::types::{AnyEvent, BoxFuture, DlqTerminalInfo, Events, Handler, Projection};
use crate::event_codec::EventCodec;

#[track_caller]
fn default_handler_id(prefix: &str) -> String {
    let location = std::panic::Location::caller();
    format!(
        "{prefix}@{}:{}:{}",
        location.file(),
        location.line(),
        location.column()
    )
}

/// Marker for typed event handlers (`on::<E>()`).
pub struct Typed<E>(PhantomData<E>);

/// Marker for any event handlers (`on_any()`).
pub struct Untyped;

/// Marker for no filter.
pub struct NoFilter;

/// Builder for handlers with a context-aware filter predicate.
///
/// Created by calling `.filter(predicate)` on a `HandlerBuilder`.
/// The predicate receives both the event and the handler [`Context`],
/// giving access to aggregate state, deps, etc.
pub struct FilteredHandlerBuilder<E, Started, D: Send + Sync + 'static, G> {
    inner: HandlerBuilder<Typed<E>, NoFilter, Started>,
    filter_fn: G,
    describe_fn: Option<Arc<dyn Fn(&super::context::Context<D>) -> serde_json::Value + Send + Sync>>,
    _deps: PhantomData<D>,
}

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

/// A unified builder for handlers using a compile-time type-phase pattern.
pub struct HandlerBuilder<EventType, Filter, Started> {
    filter: Filter,
    started: Started,
    id: Option<String>,
    queued: bool,
    delay: Option<Duration>,
    timeout: Option<Duration>,
    max_attempts: u32,
    backoff: Option<Duration>,
    priority: Option<i32>,
    codec: Option<Arc<EventCodec>>,
    dlq_terminal_mapper: Option<super::types::DlqTerminalMapper>,
    _marker: PhantomData<EventType>,
}

/// Create a handler for a specific event type.
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

        max_attempts: 1,
        backoff: None,
        priority: None,
        codec: Some(typed_event_codec::<E>()),
        dlq_terminal_mapper: None,
        _marker: PhantomData,
    }
}

/// Create a handler for all events (observer pattern).
pub fn on_any() -> HandlerBuilder<Untyped, NoFilter, NoStarted> {
    HandlerBuilder {
        filter: NoFilter,
        started: NoStarted,
        id: None,
        queued: false,
        delay: None,
        timeout: None,

        max_attempts: 1,
        backoff: None,
        priority: None,
        codec: None,
        dlq_terminal_mapper: None,
        _marker: PhantomData,
    }
}

/// Create a projection that receives all events.
///
/// Projections run sequentially before all other handlers, ensuring
/// read models are up-to-date when regular handlers execute.
///
/// ```ignore
/// project("audit_log").then(|event: AnyEvent, ctx| async move {
///     if let Some(order) = event.downcast_ref::<OrderPlaced>() {
///         // update read model
///     }
///     Ok(())
/// })
/// ```
pub fn project(id: impl Into<String>) -> ProjectionBuilder {
    ProjectionBuilder {
        id: id.into(),
        priority: None,
    }
}

/// Builder for projection handlers.
pub struct ProjectionBuilder {
    id: String,
    priority: Option<i32>,
}

impl ProjectionBuilder {
    /// Set execution priority (lower = runs first).
    pub fn priority(mut self, priority: i32) -> Self {
        self.priority = Some(priority);
        self
    }

    /// Set the projection handler (terminal operation).
    pub fn then<D, H, Fut>(self, handler: H) -> Projection<D>
    where
        D: Send + Sync + 'static,
        H: Fn(AnyEvent, Context<D>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        Projection {
            id: self.id,
            handler: Arc::new(move |event, ctx| Box::pin(handler(event, ctx))),
            priority: self.priority,
        }
    }
}

impl<E, Started> HandlerBuilder<Typed<E>, NoFilter, Started>
where
    E: Send + Sync + 'static,
{
    /// Add a context-aware filter predicate that must pass for the handler to run.
    ///
    /// Unlike `.extract()` (which transforms the event), `.filter()` gates
    /// execution based on both the event and the handler [`Context`] — giving
    /// access to aggregate state, deps, etc.
    ///
    /// ```ignore
    /// on::<ScrapeRoleCompleted>()
    ///     .id("actor_extraction")
    ///     .retry(3)
    ///     .filter(|event, ctx: &Context<Deps>| {
    ///         let (_, state) = ctx.singleton::<PipelineState>();
    ///         state.completed_scrape_roles.is_superset(&response_roles())
    ///     })
    ///     .then(|event, ctx| async move { Ok(events![]) })
    /// ```
    pub fn filter<D, F>(self, predicate: F) -> FilteredHandlerBuilder<E, Started, D, F>
    where
        D: Send + Sync + 'static,
        F: Fn(&E, &Context<D>) -> bool + Send + Sync + 'static,
    {
        FilteredHandlerBuilder {
            inner: self,
            filter_fn: predicate,
            describe_fn: None,
            _deps: PhantomData,
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

            max_attempts: self.max_attempts,
            backoff: self.backoff,
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

            max_attempts: self.max_attempts,
            backoff: self.backoff,
            priority: self.priority,
            codec: self.codec,
            dlq_terminal_mapper: self.dlq_terminal_mapper,
            _marker: PhantomData,
        }
    }
}

impl<EventType, Filter, Started> HandlerBuilder<EventType, Filter, Started> {
    /// Set a custom ID for this handler (default: auto-generated).
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
                handler_id: None,
                ephemeral: None,
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
    /// Add a delay before execution.
    pub fn delayed(mut self, duration: Duration) -> Self {
        self.delay = Some(duration);
        self.queued = true;
        self.codec = EventType::queue_codec();
        self
    }

    /// Set execution timeout.
    pub fn timeout(mut self, duration: Duration) -> Self {
        self.timeout = Some(duration);
        self.queued = true;
        self.codec = EventType::queue_codec();
        self
    }

    /// Set maximum retry attempts (default: 1 = no retry).
    ///
    /// Use `.retry(1)` to explicitly mark a handler as background
    /// without retries.
    pub fn retry(mut self, attempts: u32) -> Self {
        self.max_attempts = attempts;
        self.queued = true;
        self.codec = EventType::queue_codec();
        self
    }

    /// Set exponential backoff base duration for retries.
    ///
    /// Each retry waits `base * 2^(attempt-1)`. For example, with
    /// `backoff(Duration::from_secs(1))` and `retry(4)`:
    /// - Attempt 1: immediate
    /// - Attempt 2: 1s delay
    /// - Attempt 3: 2s delay
    /// - Attempt 4: 4s delay
    pub fn backoff(mut self, base: Duration) -> Self {
        self.backoff = Some(base);
        self
    }

    /// Set execution priority (lower = higher priority).
    ///
    /// Handlers with lower priority are polled first.
    pub fn priority(mut self, level: i32) -> Self {
        self.priority = Some(level);
        self
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
    /// Set the handler (terminal operation). Return `events![]` from the handler.
    #[track_caller]
    #[allow(private_bounds)]
    pub fn then<D, T, H, Fut>(self, handler: H) -> Handler<D>
    where
        D: Send + Sync + 'static,
        T: Clone + Send + 'static,
        Filter: Extractor<E, T>,
        Started: StartedHandler<D>,
        H: Fn(T, Context<D>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Events>> + Send + 'static,
    {
        let target = TypeId::of::<E>();
        let filter = self.filter;
        // Always register input codec so Engine::decode_event works for all handlers
        let input_codec = self.codec.unwrap_or_else(typed_event_codec::<E>);
        let dlq_terminal_mapper = self.dlq_terminal_mapper;

        let id = self
            .id
            .unwrap_or_else(|| default_handler_id(std::any::type_name::<E>()));

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
                            let events: Events = fut.await?;
                            Ok(events.into_outputs())
                        })
                    }
                    None => Box::pin(async { Ok(Vec::new()) }),
                }
            }),

            dlq_terminal_mapper,
            queued: self.queued,
            delay: self.delay,
            timeout: self.timeout,
            max_attempts: self.max_attempts,
            backoff: self.backoff,
            priority: self.priority,
            describe: None,
        }
    }
}

impl HandlerBuilder<Untyped, NoFilter, NoStarted> {
    /// Set the handler for observing all events. Return `events![]` from the handler.
    #[track_caller]
    pub fn then<D, H, Fut>(self, handler: H) -> Handler<D>
    where
        D: Send + Sync + 'static,
        H: Fn(AnyEvent, Context<D>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Events>> + Send + 'static,
    {
        let id = self.id.unwrap_or_else(|| default_handler_id("handler_any"));

        Handler {
            id,
            codecs: Vec::new(),
            can_handle: Arc::new(|_| true),
            started: None,
            handler: Arc::new(move |value, type_id, ctx| {
                let event = AnyEvent { value, type_id };
                let fut = handler(event, ctx);
                Box::pin(async move {
                    let events: Events = fut.await?;
                    Ok(events.into_outputs())
                })
            }),

            dlq_terminal_mapper: None,
            queued: self.queued,
            delay: self.delay,
            timeout: self.timeout,
            max_attempts: self.max_attempts,
            backoff: self.backoff,
            priority: self.priority,
            describe: None,
        }
    }
}

impl<D, St, StFut> HandlerBuilder<Untyped, NoFilter, WithStarted<D, St>>
where
    D: Send + Sync + 'static,
    St: Fn(Context<D>) -> StFut + Send + Sync + 'static,
    StFut: Future<Output = Result<()>> + Send + 'static,
{
    /// Set the handler for observing all events with started hook. Return `events![]`.
    #[track_caller]
    pub fn then<H, Fut>(self, handler: H) -> Handler<D>
    where
        H: Fn(AnyEvent, Context<D>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Events>> + Send + 'static,
    {
        let id = self
            .id
            .unwrap_or_else(|| default_handler_id("handler_any_started"));
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
                    let events: Events = fut.await?;
                    Ok(events.into_outputs())
                })
            }),

            dlq_terminal_mapper: None,
            queued: self.queued,
            delay: self.delay,
            timeout: self.timeout,
            max_attempts: self.max_attempts,
            backoff: self.backoff,
            priority: self.priority,
            describe: None,
        }
    }
}

// ── Filtered handler builder ─────────────────────────────────────────────

impl<E, Started, D, G> FilteredHandlerBuilder<E, Started, D, G>
where
    E: Clone + Send + Sync + 'static,
    D: Send + Sync + 'static,
{
    /// Set a custom ID for this handler.
    pub fn id(mut self, id: impl Into<String>) -> Self {
        self.inner.id = Some(id.into());
        self
    }

    /// Add an introspection closure for flow visualization.
    ///
    /// The closure receives the handler [`Context`] and returns a typed struct
    /// that seesaw serializes to JSON. The output is persisted to the Store
    /// per `(correlation_id, handler_id)` so external UIs can render gate progress.
    ///
    /// ```ignore
    /// on::<SynthesisRoleCompleted>()
    ///     .id("lifecycle:finalize")
    ///     .filter(|event, ctx: &Context<Deps>| { /* ... */ })
    ///     .describe(|ctx: &Context<Deps>| FinalizeGate {
    ///         completed: completed_roles,
    ///         remaining: remaining_roles,
    ///     })
    ///     .then(|event, ctx| async move { Ok(events![]) })
    /// ```
    pub fn describe<T, Desc>(mut self, f: Desc) -> Self
    where
        T: serde::Serialize + Send + Sync + 'static,
        Desc: Fn(&Context<D>) -> T + Send + Sync + 'static,
    {
        self.describe_fn = Some(Arc::new(move |ctx| {
            serde_json::to_value(f(ctx)).unwrap_or_else(|e| {
                tracing::warn!("describe serialization failed: {}", e);
                serde_json::Value::Null
            })
        }));
        self
    }
}

impl<E, Started, D, G> FilteredHandlerBuilder<E, Started, D, G>
where
    E: Clone + Send + Sync + 'static + serde::Serialize + serde::de::DeserializeOwned,
    D: Send + Sync + 'static,
    G: Fn(&E, &Context<D>) -> bool + Send + Sync + 'static,
{
    /// Set the handler (terminal operation). Return `events![]` from the handler.
    ///
    /// The filter predicate runs first — if it returns `false`, the handler
    /// body is skipped and an empty `events![]` is returned.
    #[track_caller]
    #[allow(private_bounds)]
    pub fn then<H, Fut>(self, handler: H) -> Handler<D>
    where
        Started: StartedHandler<D>,
        H: Fn(Arc<E>, Context<D>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Events>> + Send + 'static,
    {
        let target = TypeId::of::<E>();
        let filter_fn = Arc::new(self.filter_fn);
        let input_codec = self.inner.codec.unwrap_or_else(typed_event_codec::<E>);
        let dlq_terminal_mapper = self.inner.dlq_terminal_mapper;

        let id = self
            .inner
            .id
            .unwrap_or_else(|| default_handler_id(std::any::type_name::<E>()));

        Handler {
            id,
            codecs: vec![input_codec],
            can_handle: Arc::new(move |t| t == target),
            started: self.inner.started.into_started(),
            handler: Arc::new(move |value, _, ctx| {
                let typed = value.downcast::<E>().expect("type checked by can_handle");

                if !filter_fn(&typed, &ctx) {
                    return Box::pin(async { Ok(Vec::new()) });
                }

                let fut = handler(typed, ctx);
                Box::pin(async move {
                    let events: Events = fut.await?;
                    Ok(events.into_outputs())
                })
            }),

            dlq_terminal_mapper,
            queued: self.inner.queued,
            delay: self.inner.delay,
            timeout: self.inner.timeout,
            max_attempts: self.inner.max_attempts,
            backoff: self.inner.backoff,
            priority: self.inner.priority,
            describe: self.describe_fn,
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
    
                max_attempts: self.max_attempts,
                backoff: self.backoff,
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

    /// Set the handler that runs when the transition guard passes. Return `events![]`.
    #[track_caller]
    #[allow(private_bounds)]
    pub fn then<D, H, Fut>(self, handler: H) -> Handler<D>
    where
        D: Send + Sync + 'static,
        Started: StartedHandler<D>,
        H: Fn(uuid::Uuid, Context<D>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Events>> + Send + 'static,
    {
        let target = TypeId::of::<E>();
        let extractor: Arc<dyn Fn(&E) -> Option<uuid::Uuid> + Send + Sync> =
            Arc::from(self.extractor);
        let guard = Arc::new(self.guard);
        let handler = Arc::new(handler);

        let id = self
            .inner
            .id
            .unwrap_or_else(|| default_handler_id(std::any::type_name::<E>()));

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

                            let (prev, next) = registry.get_transition::<A>(aggregate_id);

                            if !guard(&prev, &next) {
                                return Ok(Vec::new());
                            }

                            let events: Events = handler(aggregate_id, ctx).await?;
                            Ok(events.into_outputs())
                        })
                    }
                    None => Box::pin(async { Ok(Vec::new()) }),
                }
            }),

            dlq_terminal_mapper: self.inner.dlq_terminal_mapper,
            queued: self.inner.queued,
            delay: self.inner.delay,
            timeout: self.inner.timeout,
            max_attempts: self.inner.max_attempts,
            backoff: self.inner.backoff,
            priority: self.inner.priority,
            describe: None,
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
        let handler = on::<QueueEvent>()
            .id("filter_probe")
            .filter(|event, _ctx: &Context<Deps>| event.value > 0)
            .then(|_event: Arc<QueueEvent>, _ctx: Context<Deps>| async move {
                Ok(crate::events![])
            });

        assert!(
            handler.is_default(),
            "filter() should not change handler execution mode"
        );
    }
}
