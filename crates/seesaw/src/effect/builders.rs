//! Effect builder functions and types.

use std::any::{Any, TypeId};
use std::future::Future;
use std::marker::PhantomData;
use std::sync::Arc;

use anyhow::Result;

use super::context::EffectContext;
use super::types::{AnyEvent, BoxFuture, Effect, EventOutput};

// =============================================================================
// Type-State Markers
// =============================================================================

/// Marker for typed event effects (on::<E>()).
pub struct Typed<E>(PhantomData<E>);

/// Marker for any event effects (on_any()).
pub struct Untyped;

/// Marker for no filter.
pub struct NoFilter;

/// Marker for having a filter predicate.
pub struct WithFilter<F>(F);

/// Marker for having a filter_map predicate.
pub struct WithFilterMap<F, T>(F, PhantomData<T>);

/// Marker for no transition predicate.
pub struct NoTransition;

/// Marker for having a transition predicate.
pub struct WithTransition<S, P>(P, PhantomData<S>);

/// Marker for no started handler.
pub struct NoStarted;

/// Marker for having a started handler.
pub struct WithStarted<S, D, St>(St, PhantomData<(S, D)>);

// =============================================================================
// Unified EffectBuilder
// =============================================================================

/// A unified builder for effects using the type-state pattern.
pub struct EffectBuilder<EventType, Filter, Trans, Started> {
    filter: Filter,
    transition: Trans,
    started: Started,
    _marker: PhantomData<EventType>,
}

// =============================================================================
// Entry Points
// =============================================================================

/// Create an effect that handles a specific event type.
///
/// # Example
///
/// ```ignore
/// effect::on::<MyEvent>().then(|event, ctx| async move {
///     Ok(NextEvent { data: event.data })
/// })
/// ```
pub fn on<E: Send + Sync + 'static>() -> EffectBuilder<Typed<E>, NoFilter, NoTransition, NoStarted>
{
    EffectBuilder {
        filter: NoFilter,
        transition: NoTransition,
        started: NoStarted,
        _marker: PhantomData,
    }
}

/// Create an effect that handles all events (observer pattern).
///
/// # Example
///
/// ```ignore
/// effect::on_any().then(|event, ctx| async move {
///     ctx.deps().metrics.increment("events");
///     Ok(())
/// })
/// ```
pub fn on_any() -> EffectBuilder<Untyped, NoFilter, NoTransition, NoStarted> {
    EffectBuilder {
        filter: NoFilter,
        transition: NoTransition,
        started: NoStarted,
        _marker: PhantomData,
    }
}

// =============================================================================
// Builder Methods - Filter/FilterMap (Typed only)
// =============================================================================

impl<E, Trans, Started> EffectBuilder<Typed<E>, NoFilter, Trans, Started>
where
    E: Send + Sync + 'static,
{
    /// Add a filter predicate that must pass for the handler to run.
    pub fn filter<F>(self, predicate: F) -> EffectBuilder<Typed<E>, WithFilter<F>, Trans, Started>
    where
        F: Fn(&E) -> bool + Send + Sync + 'static,
    {
        EffectBuilder {
            filter: WithFilter(predicate),
            transition: self.transition,
            started: self.started,
            _marker: PhantomData,
        }
    }

    /// Extract data from events. Handler receives the extracted value.
    ///
    /// # Example
    ///
    /// ```ignore
    /// on::<MyEvent>()
    ///     .extract(|e| match e {
    ///         MyEvent::Variant { id, data } => Some((*id, data.clone())),
    ///         _ => None
    ///     })
    ///     .then(|(id, data), ctx| async move {
    ///         Ok(MyEvent::Processed { id, data })
    ///     })
    /// ```
    pub fn extract<F, T>(
        self,
        extractor: F,
    ) -> EffectBuilder<Typed<E>, WithFilterMap<F, T>, Trans, Started>
    where
        F: Fn(&E) -> Option<T> + Send + Sync + 'static,
        T: Clone + Send + Sync + 'static,
    {
        EffectBuilder {
            filter: WithFilterMap(extractor, PhantomData),
            transition: self.transition,
            started: self.started,
            _marker: PhantomData,
        }
    }
}

// =============================================================================
// Builder Methods - Transition (Both Typed and Untyped)
// =============================================================================

impl<EventType, Filter, Started> EffectBuilder<EventType, Filter, NoTransition, Started> {
    /// Add a state transition predicate. Handler only runs when predicate returns true.
    pub fn transition<S, P>(
        self,
        predicate: P,
    ) -> EffectBuilder<EventType, Filter, WithTransition<S, P>, Started>
    where
        S: Clone + Send + Sync + 'static,
        P: Fn(&S, &S) -> bool + Send + Sync + 'static,
    {
        EffectBuilder {
            filter: self.filter,
            transition: WithTransition(predicate, PhantomData),
            started: self.started,
            _marker: PhantomData,
        }
    }
}

// =============================================================================
// Builder Methods - Started (Both Typed and Untyped)
// =============================================================================

impl<EventType, Filter, Trans> EffectBuilder<EventType, Filter, Trans, NoStarted> {
    /// Add a started handler that runs when the store is activated.
    pub fn started<S, D, St, StFut>(
        self,
        started: St,
    ) -> EffectBuilder<EventType, Filter, Trans, WithStarted<S, D, St>>
    where
        S: Clone + Send + Sync + 'static,
        D: Send + Sync + 'static,
        St: Fn(EffectContext<S, D>) -> StFut + Send + Sync + 'static,
        StFut: Future<Output = Result<()>> + Send + 'static,
    {
        EffectBuilder {
            filter: self.filter,
            transition: self.transition,
            started: WithStarted(started, PhantomData),
            _marker: PhantomData,
        }
    }
}

// =============================================================================
// Extraction Trait - Abstracts filter/filter_map logic
// =============================================================================

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

// =============================================================================
// Transition Trait - Abstracts transition checking
// =============================================================================

/// Trait for checking state transitions.
trait TransitionChecker<S>: Send + Sync + 'static {
    fn should_run(&self, prev: &S, next: &S) -> bool;
}

impl<S> TransitionChecker<S> for NoTransition {
    fn should_run(&self, _prev: &S, _next: &S) -> bool {
        true
    }
}

impl<S, P> TransitionChecker<S> for WithTransition<S, P>
where
    S: Clone + Send + Sync + 'static,
    P: Fn(&S, &S) -> bool + Send + Sync + 'static,
{
    fn should_run(&self, prev: &S, next: &S) -> bool {
        (self.0)(prev, next)
    }
}

// =============================================================================
// Started Trait - Abstracts started handler
// =============================================================================

/// Trait for optional started handlers.
trait StartedHandler<S, D>: Send + Sync + 'static
where
    S: Send + Sync + 'static,
    D: Send + Sync + 'static,
{
    fn into_started(self) -> Option<Arc<dyn Fn(EffectContext<S, D>) -> BoxFuture<Result<()>> + Send + Sync>>;
}

impl<S, D> StartedHandler<S, D> for NoStarted
where
    S: Send + Sync + 'static,
    D: Send + Sync + 'static,
{
    fn into_started(self) -> Option<Arc<dyn Fn(EffectContext<S, D>) -> BoxFuture<Result<()>> + Send + Sync>> {
        None
    }
}

impl<S, D, St, StFut> StartedHandler<S, D> for WithStarted<S, D, St>
where
    S: Clone + Send + Sync + 'static,
    D: Send + Sync + 'static,
    St: Fn(EffectContext<S, D>) -> StFut + Send + Sync + 'static,
    StFut: Future<Output = Result<()>> + Send + 'static,
{
    fn into_started(self) -> Option<Arc<dyn Fn(EffectContext<S, D>) -> BoxFuture<Result<()>> + Send + Sync>> {
        let started = self.0;
        Some(Arc::new(move |ctx| Box::pin(started(ctx))))
    }
}

// =============================================================================
// then() - Unified Implementation for Typed Events
// =============================================================================

impl<E, Filter, Trans, Started> EffectBuilder<Typed<E>, Filter, Trans, Started>
where
    E: Clone + Send + Sync + 'static,
{
    /// Set the handler that returns the next event (terminal operation).
    ///
    /// The output type is inferred from the closure return type.
    /// - Return `Ok(SomeEvent)` to dispatch that event
    /// - Return `Ok(())` to dispatch nothing (observer pattern for typed events)
    ///
    /// # Example
    ///
    /// ```ignore
    /// // Chain effect - returns next event
    /// effect::on::<OrderPlaced>().then(|event, ctx| async move {
    ///     ctx.deps().mailer.send(&event).await?;
    ///     Ok(EmailSent { order_id: event.id })
    /// })
    ///
    /// // Observer effect - returns () to dispatch nothing
    /// effect::on::<OrderPlaced>().then(|event, ctx| async move {
    ///     println!("Order placed: {:?}", event);
    ///     Ok(())
    /// })
    /// ```
    pub fn then<S, D, T, H, Fut, O>(self, handler: H) -> Effect<S, D>
    where
        S: Clone + Send + Sync + 'static,
        D: Send + Sync + 'static,
        T: Clone + Send + 'static,
        Filter: Extractor<E, T>,
        Trans: TransitionChecker<S>,
        Started: StartedHandler<S, D>,
        H: Fn(T, EffectContext<S, D>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<O>> + Send + 'static,
        O: Send + Sync + 'static,
    {
        let target = TypeId::of::<E>();
        let output_is_unit = TypeId::of::<O>() == TypeId::of::<()>();
        let filter = self.filter;
        let transition = self.transition;

        Effect {
            can_handle: Arc::new(move |t| t == target),
            started: self.started.into_started(),
            handler: Arc::new(move |value, _, ctx| {
                let typed = value.downcast::<E>().expect("type checked by can_handle");

                // Check transition
                if !transition.should_run(ctx.prev_state(), ctx.next_state()) {
                    return Box::pin(async { Ok(None) });
                }

                // Extract/filter
                match filter.extract(&typed) {
                    Some(extracted) => {
                        let fut = handler(extracted, ctx);
                        Box::pin(async move {
                            let output = fut.await?;
                            // Special case: () means no event to dispatch (observer pattern)
                            if output_is_unit {
                                Ok(None)
                            } else {
                                Ok(Some(EventOutput::new(output)))
                            }
                        })
                    }
                    None => Box::pin(async { Ok(None) }),
                }
            }),
        }
    }
}

// =============================================================================
// then() - Untyped Events (on_any) - Observer Pattern
// =============================================================================

impl EffectBuilder<Untyped, NoFilter, NoTransition, NoStarted> {
    /// Set the handler for observing all events (terminal operation).
    /// Returns `Result<()>` - observers don't produce events.
    pub fn then<S, D, H, Fut>(self, handler: H) -> Effect<S, D>
    where
        S: Clone + Send + Sync + 'static,
        D: Send + Sync + 'static,
        H: Fn(AnyEvent, EffectContext<S, D>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        Effect {
            can_handle: Arc::new(|_| true),
            started: None,
            handler: Arc::new(move |value, type_id, ctx| {
                let event = AnyEvent { value, type_id };
                let fut = handler(event, ctx);
                Box::pin(async move {
                    fut.await?;
                    Ok(None)
                })
            }),
        }
    }
}

impl<S, P> EffectBuilder<Untyped, NoFilter, WithTransition<S, P>, NoStarted>
where
    S: Clone + Send + Sync + 'static,
    P: Fn(&S, &S) -> bool + Send + Sync + 'static,
{
    /// Set the handler for state transitions (terminal operation).
    /// Handler receives only context since transitions are about state changes.
    pub fn then<D, H, Fut>(self, handler: H) -> Effect<S, D>
    where
        D: Send + Sync + 'static,
        H: Fn(EffectContext<S, D>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        let transition = self.transition.0;
        Effect {
            can_handle: Arc::new(|_| true),
            started: None,
            handler: Arc::new(move |_, _, ctx| {
                if !transition(ctx.prev_state(), ctx.next_state()) {
                    return Box::pin(async { Ok(None) });
                }
                let fut = handler(ctx);
                Box::pin(async move {
                    fut.await?;
                    Ok(None)
                })
            }),
        }
    }
}

impl<S, D, St, StFut> EffectBuilder<Untyped, NoFilter, NoTransition, WithStarted<S, D, St>>
where
    S: Clone + Send + Sync + 'static,
    D: Send + Sync + 'static,
    St: Fn(EffectContext<S, D>) -> StFut + Send + Sync + 'static,
    StFut: Future<Output = Result<()>> + Send + 'static,
{
    /// Set the handler for observing all events with started (terminal operation).
    pub fn then<H, Fut>(self, handler: H) -> Effect<S, D>
    where
        H: Fn(AnyEvent, EffectContext<S, D>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        let started = self.started.0;
        Effect {
            can_handle: Arc::new(|_| true),
            started: Some(Arc::new(move |ctx| Box::pin(started(ctx)))),
            handler: Arc::new(move |value, type_id, ctx| {
                let event = AnyEvent { value, type_id };
                let fut = handler(event, ctx);
                Box::pin(async move {
                    fut.await?;
                    Ok(None)
                })
            }),
        }
    }
}

impl<S, D, St, StFut, P> EffectBuilder<Untyped, NoFilter, WithTransition<S, P>, WithStarted<S, D, St>>
where
    S: Clone + Send + Sync + 'static,
    D: Send + Sync + 'static,
    St: Fn(EffectContext<S, D>) -> StFut + Send + Sync + 'static,
    StFut: Future<Output = Result<()>> + Send + 'static,
    P: Fn(&S, &S) -> bool + Send + Sync + 'static,
{
    /// Set the handler for state transitions with started (terminal operation).
    pub fn then<H, Fut>(self, handler: H) -> Effect<S, D>
    where
        H: Fn(EffectContext<S, D>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        let started = self.started.0;
        let transition = self.transition.0;
        Effect {
            can_handle: Arc::new(|_| true),
            started: Some(Arc::new(move |ctx| Box::pin(started(ctx)))),
            handler: Arc::new(move |_, _, ctx| {
                if !transition(ctx.prev_state(), ctx.next_state()) {
                    return Box::pin(async { Ok(None) });
                }
                let fut = handler(ctx);
                Box::pin(async move {
                    fut.await?;
                    Ok(None)
                })
            }),
        }
    }
}

// =============================================================================
// =============================================================================
// group() - Compose multiple effects
// =============================================================================

/// Compose multiple effects into a single effect.
///
/// # Example
///
/// ```ignore
/// effect::group([
///     effect::on::<EventA>().then(handle_a),
///     effect::on::<EventB>().then(handle_b),
/// ])
/// ```
pub fn group<S, D>(effects: impl IntoIterator<Item = Effect<S, D>>) -> Effect<S, D>
where
    S: Clone + Send + Sync + 'static,
    D: Send + Sync + 'static,
{
    let effects: Arc<Vec<Effect<S, D>>> = Arc::new(effects.into_iter().collect());

    Effect {
        can_handle: {
            let effects = effects.clone();
            Arc::new(move |type_id| effects.iter().any(|e| (e.can_handle)(type_id)))
        },
        started: {
            let effects = effects.clone();
            Some(Arc::new(move |ctx: EffectContext<S, D>| {
                let effects = effects.clone();
                Box::pin(async move {
                    for effect in effects.iter() {
                        if let Some(ref started) = effect.started {
                            started(ctx.clone()).await?;
                        }
                    }
                    Ok(())
                })
            }))
        },
        handler: {
            let effects = effects.clone();
            Arc::new(
                move |value: Arc<dyn Any + Send + Sync>,
                      type_id: TypeId,
                      ctx: EffectContext<S, D>| {
                    let effects = effects.clone();
                    Box::pin(async move {
                        let mut last_output = None;
                        let mut first_error: Option<anyhow::Error> = None;
                        for effect in effects.iter() {
                            if (effect.can_handle)(type_id) {
                                match (effect.handler)(value.clone(), type_id, ctx.clone()).await {
                                    Ok(Some(output)) => {
                                        last_output = Some(output);
                                    }
                                    Ok(None) => {}
                                    Err(e) => {
                                        // Capture first error but continue processing
                                        if first_error.is_none() {
                                            first_error = Some(e);
                                        }
                                    }
                                }
                            }
                        }
                        // Return first error after all effects have run
                        if let Some(err) = first_error {
                            return Err(err);
                        }
                        Ok(last_output)
                    })
                },
            )
        },
    }
}

// =============================================================================
// task() - Long-running task without event handling
// =============================================================================

/// Create a long-running task that runs on store activation.
///
/// Use for background tasks that don't need to handle events.
///
/// # Example
///
/// ```ignore
/// effect::task(|ctx| async move {
///     while !ctx.is_cancelled() {
///         // background work
///     }
///     Ok(())
/// })
/// ```
pub fn task<S, D, F, Fut>(f: F) -> Effect<S, D>
where
    S: Clone + Send + Sync + 'static,
    D: Send + Sync + 'static,
    F: Fn(EffectContext<S, D>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    Effect {
        can_handle: Arc::new(|_| false),
        started: Some(Arc::new(move |ctx| Box::pin(f(ctx)))),
        handler: Arc::new(|_, _, _| Box::pin(async { Ok(None) })),
    }
}

// =============================================================================
// bridge() - Bidirectional relay connection
// =============================================================================

/// Create a bidirectional bridge to a relay channel.
///
/// Forwards store events to relay and relay events back to store.
/// Prevents feedback loops using unique origin tracking.
pub fn bridge<S, D>(tx: pipedream::WeakSender, rx: pipedream::RelayReceiver) -> Effect<S, D>
where
    S: Clone + Send + Sync + 'static,
    D: Send + Sync + 'static,
{
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

    static BRIDGE_ID_COUNTER: AtomicU64 = AtomicU64::new(1 << 32);
    let bridge_origin_id = BRIDGE_ID_COUNTER.fetch_add(1, AtomicOrdering::Relaxed);

    let tx = Arc::new(tx);
    let rx = Arc::new(rx);

    Effect {
        can_handle: Arc::new(|_| true),
        started: {
            let rx = rx.clone();
            Some(Arc::new(move |ctx: EffectContext<S, D>| {
                let rx = rx.clone();
                let mut receiver = rx.subscribe_all();

                ctx.within(move |ctx| async move {
                    while let Some(envelope) = receiver.recv().await {
                        if envelope.origin() == bridge_origin_id {
                            continue;
                        }
                        let type_id = envelope.type_id();
                        ctx.emit_any(envelope.value, type_id);
                    }
                    Ok(())
                });

                Box::pin(async { Ok(()) })
            }))
        },
        handler: {
            let tx = tx.clone();
            Arc::new(move |value, type_id, _ctx| {
                let tx = tx.clone();
                Box::pin(async move {
                    tx.send_any_with_origin(value, type_id, bridge_origin_id)
                        .await
                        .map_err(|e| anyhow::anyhow!("{:?}", e))?;
                    Ok(None)
                })
            })
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone, Default)]
    struct TestState {
        count: i32,
    }

    #[derive(Clone, Default)]
    struct TestDeps;

    #[derive(Clone)]
    struct TestEvent {
        value: i32,
    }

    #[derive(Clone)]
    struct OutputEvent {
        result: i32,
    }

    #[derive(Clone)]
    struct OtherEvent;

    fn create_test_ctx() -> EffectContext<TestState, TestDeps> {
        use crate::task_group::TaskGroup;
        use parking_lot::RwLock;

        struct NoopEmitter;
        impl super::super::context::EventEmitter<TestState, TestDeps> for NoopEmitter {
            fn emit(
                &self,
                _: TypeId,
                _: Arc<dyn Any + Send + Sync>,
                _: EffectContext<TestState, TestDeps>,
            ) {
            }
        }

        let state = Arc::new(TestState { count: 0 });
        let live_state = Arc::new(RwLock::new(TestState { count: 0 }));
        EffectContext::new(
            state.clone(),
            state,
            live_state,
            Arc::new(TestDeps),
            Arc::new(NoopEmitter),
            TaskGroup::new(),
        )
    }

    #[test]
    fn test_on_can_handle() {
        let effect: Effect<TestState, TestDeps> =
            on::<TestEvent>().then(|_, _| async { Ok(OutputEvent { result: 1 }) });

        assert!(effect.can_handle(TypeId::of::<TestEvent>()));
        assert!(!effect.can_handle(TypeId::of::<OtherEvent>()));
    }

    #[test]
    fn test_on_any_can_handle() {
        let effect: Effect<TestState, TestDeps> = on_any().then(|_, _| async { Ok(()) });

        assert!(effect.can_handle(TypeId::of::<TestEvent>()));
        assert!(effect.can_handle(TypeId::of::<OtherEvent>()));
    }

    #[test]
    fn test_group_can_handle() {
        let effect: Effect<TestState, TestDeps> = group([
            on::<TestEvent>().then(|_, _| async { Ok(OutputEvent { result: 1 }) }),
            on::<OtherEvent>().then(|_, _| async { Ok(OutputEvent { result: 2 }) }),
        ]);

        assert!(effect.can_handle(TypeId::of::<TestEvent>()));
        assert!(effect.can_handle(TypeId::of::<OtherEvent>()));
        assert!(!effect.can_handle(TypeId::of::<String>()));
    }

    #[tokio::test]
    async fn test_then_returns_event() {
        let effect: Effect<TestState, TestDeps> =
            on::<TestEvent>().then(|event, _| async move {
                Ok(OutputEvent { result: event.value * 2 })
            });

        let ctx = create_test_ctx();
        let event: Arc<dyn Any + Send + Sync> = Arc::new(TestEvent { value: 21 });

        let result = effect
            .call_handler(event, TypeId::of::<TestEvent>(), ctx)
            .await
            .unwrap();

        assert!(result.is_some());
        let output = result.unwrap();
        assert_eq!(output.type_id, TypeId::of::<OutputEvent>());
        let downcasted = output.value.downcast::<OutputEvent>().unwrap();
        assert_eq!(downcasted.result, 42);
    }

    #[tokio::test]
    async fn test_filter_skips_non_matching() {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = counter.clone();

        let effect: Effect<TestState, TestDeps> = on::<TestEvent>()
            .filter(|e| e.value > 10)
            .then(move |event, _| {
                let counter = counter_clone.clone();
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok(OutputEvent { result: event.value })
                }
            });

        let ctx = create_test_ctx();

        // Should skip - value <= 10
        let event1: Arc<dyn Any + Send + Sync> = Arc::new(TestEvent { value: 5 });
        let result1 = effect
            .call_handler(event1, TypeId::of::<TestEvent>(), ctx.clone())
            .await
            .unwrap();
        assert!(result1.is_none());
        assert_eq!(counter.load(Ordering::SeqCst), 0);

        // Should run - value > 10
        let event2: Arc<dyn Any + Send + Sync> = Arc::new(TestEvent { value: 15 });
        let result2 = effect
            .call_handler(event2, TypeId::of::<TestEvent>(), ctx)
            .await
            .unwrap();
        assert!(result2.is_some());
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_filter_map_transforms_and_filters() {
        let effect: Effect<TestState, TestDeps> = on::<TestEvent>()
            .extract(|e| {
                if e.value > 10 {
                    Some((e.value, e.value * 2))
                } else {
                    None
                }
            })
            .then(|(original, doubled), _| async move {
                Ok(OutputEvent { result: original + doubled })
            });

        let ctx = create_test_ctx();

        // Should skip - value <= 10
        let event1: Arc<dyn Any + Send + Sync> = Arc::new(TestEvent { value: 5 });
        let result1 = effect
            .call_handler(event1, TypeId::of::<TestEvent>(), ctx.clone())
            .await
            .unwrap();
        assert!(result1.is_none());

        // Should run with transformed value - value > 10
        let event2: Arc<dyn Any + Send + Sync> = Arc::new(TestEvent { value: 15 });
        let result2 = effect
            .call_handler(event2, TypeId::of::<TestEvent>(), ctx)
            .await
            .unwrap();

        assert!(result2.is_some());
        let output = result2.unwrap().value.downcast::<OutputEvent>().unwrap();
        assert_eq!(output.result, 15 + 30); // original + doubled
    }

    #[test]
    fn test_task_never_handles_events() {
        let effect: Effect<TestState, TestDeps> = task(|_ctx| async { Ok(()) });

        assert!(!effect.can_handle(TypeId::of::<TestEvent>()));
        assert!(!effect.can_handle(TypeId::of::<OtherEvent>()));
    }

    #[tokio::test]
    async fn test_task_started_called() {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = counter.clone();

        let effect: Effect<TestState, TestDeps> = task(move |_ctx| {
            let counter = counter_clone.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        });

        let ctx = create_test_ctx();
        effect.call_started(ctx).await.unwrap();

        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }
}
