//! Effect builder functions and types.

use std::any::{Any, TypeId};
use std::future::Future;
use std::marker::PhantomData;
use std::sync::Arc;

use anyhow::Result;

use super::context::EffectContext;
use super::types::{AnyEvent, Effect};

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
///
/// Type parameters:
/// - `EventType`: Either `Typed<E>` for specific event types or `Untyped` for all events
/// - `Filter`: Either `NoFilter` or `WithFilter<F>`
/// - `Trans`: Either `NoTransition` or `WithTransition<S, P>`
/// - `Started`: Either `NoStarted` or `WithStarted<S, D, St>`
pub struct EffectBuilder<EventType, Filter, Trans, Started> {
    filter: Filter,
    transition: Trans,
    started: Started,
    _marker: PhantomData<EventType>,
}

// =============================================================================
// on::<E>() - Single typed event entry point
// =============================================================================

/// Create an effect that handles a specific event type.
///
/// # Example
///
/// ```ignore
/// effect::on::<MyEvent>().run(|event, ctx| async move {
///     // event is Arc<MyEvent>
///     Ok(())
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

// =============================================================================
// on_any() - All events entry point
// =============================================================================

/// Create an effect that handles all events (type-erased).
///
/// # Example
///
/// ```ignore
/// effect::on_any().run(|event, ctx| async move {
///     // event is AnyEvent { value, type_id }
///     if let Some(my_event) = event.downcast::<MyEvent>() {
///         // handle
///     }
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
// Builder Methods - Typed Events
// =============================================================================

impl<E, Trans, Started> EffectBuilder<Typed<E>, NoFilter, Trans, Started>
where
    E: Send + Sync + 'static,
{
    /// Add a filter predicate that must pass for the handler to run.
    /// Only available for typed event effects.
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
}

// =============================================================================
// Builder Methods - Transition (Both Typed and Untyped)
// =============================================================================

impl<EventType, Filter, Started> EffectBuilder<EventType, Filter, NoTransition, Started> {
    /// Add a state transition predicate.
    /// Handler only runs when the predicate returns true for prev/next state.
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
// run() - Typed Events, No Filter, No Transition, No Started
// =============================================================================

impl<E> EffectBuilder<Typed<E>, NoFilter, NoTransition, NoStarted>
where
    E: Send + Sync + 'static,
{
    /// Set the handler (terminal operation).
    pub fn run<S, D, H, Fut>(self, handler: H) -> Effect<S, D>
    where
        S: Clone + Send + Sync + 'static,
        D: Send + Sync + 'static,
        H: Fn(Arc<E>, EffectContext<S, D>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        let target = TypeId::of::<E>();
        Effect {
            can_handle: Arc::new(move |t| t == target),
            started: None,
            handler: Arc::new(move |value, _, ctx| {
                let typed = value.downcast::<E>().expect("type checked by can_handle");
                Box::pin(handler(typed, ctx))
            }),
        }
    }
}

// =============================================================================
// run() - Typed Events, With Filter, No Transition, No Started
// =============================================================================

impl<E, F> EffectBuilder<Typed<E>, WithFilter<F>, NoTransition, NoStarted>
where
    E: Send + Sync + 'static,
    F: Fn(&E) -> bool + Send + Sync + 'static,
{
    /// Set the handler (terminal operation).
    pub fn run<S, D, H, Fut>(self, handler: H) -> Effect<S, D>
    where
        S: Clone + Send + Sync + 'static,
        D: Send + Sync + 'static,
        H: Fn(Arc<E>, EffectContext<S, D>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        let target = TypeId::of::<E>();
        let filter = self.filter.0;
        Effect {
            can_handle: Arc::new(move |t| t == target),
            started: None,
            handler: Arc::new(move |value, _, ctx| {
                let typed = value.downcast::<E>().expect("type checked by can_handle");
                if !filter(&typed) {
                    return Box::pin(async { Ok(()) });
                }
                Box::pin(handler(typed, ctx))
            }),
        }
    }
}

// =============================================================================
// run() - Typed Events, No Filter, With Transition, No Started
// =============================================================================

impl<E, S, P> EffectBuilder<Typed<E>, NoFilter, WithTransition<S, P>, NoStarted>
where
    E: Send + Sync + 'static,
    S: Clone + Send + Sync + 'static,
    P: Fn(&S, &S) -> bool + Send + Sync + 'static,
{
    /// Set the handler (terminal operation).
    pub fn run<D, H, Fut>(self, handler: H) -> Effect<S, D>
    where
        D: Send + Sync + 'static,
        H: Fn(Arc<E>, EffectContext<S, D>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        let target = TypeId::of::<E>();
        let transition = self.transition.0;
        Effect {
            can_handle: Arc::new(move |t| t == target),
            started: None,
            handler: Arc::new(move |value, _, ctx| {
                if !transition(ctx.prev_state(), ctx.next_state()) {
                    return Box::pin(async { Ok(()) });
                }
                let typed = value.downcast::<E>().expect("type checked by can_handle");
                Box::pin(handler(typed, ctx))
            }),
        }
    }
}

// =============================================================================
// run() - Typed Events, With Filter, With Transition, No Started
// =============================================================================

impl<E, S, F, P> EffectBuilder<Typed<E>, WithFilter<F>, WithTransition<S, P>, NoStarted>
where
    E: Send + Sync + 'static,
    S: Clone + Send + Sync + 'static,
    F: Fn(&E) -> bool + Send + Sync + 'static,
    P: Fn(&S, &S) -> bool + Send + Sync + 'static,
{
    /// Set the handler (terminal operation).
    pub fn run<D, H, Fut>(self, handler: H) -> Effect<S, D>
    where
        D: Send + Sync + 'static,
        H: Fn(Arc<E>, EffectContext<S, D>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        let target = TypeId::of::<E>();
        let filter = self.filter.0;
        let transition = self.transition.0;
        Effect {
            can_handle: Arc::new(move |t| t == target),
            started: None,
            handler: Arc::new(move |value, _, ctx| {
                if !transition(ctx.prev_state(), ctx.next_state()) {
                    return Box::pin(async { Ok(()) });
                }
                let typed = value.downcast::<E>().expect("type checked by can_handle");
                if !filter(&typed) {
                    return Box::pin(async { Ok(()) });
                }
                Box::pin(handler(typed, ctx))
            }),
        }
    }
}

// =============================================================================
// run() - Typed Events, No Filter, No Transition, With Started
// =============================================================================

impl<E, S, D, St, StFut> EffectBuilder<Typed<E>, NoFilter, NoTransition, WithStarted<S, D, St>>
where
    E: Send + Sync + 'static,
    S: Clone + Send + Sync + 'static,
    D: Send + Sync + 'static,
    St: Fn(EffectContext<S, D>) -> StFut + Send + Sync + 'static,
    StFut: Future<Output = Result<()>> + Send + 'static,
{
    /// Set the handler (terminal operation).
    pub fn run<H, Fut>(self, handler: H) -> Effect<S, D>
    where
        H: Fn(Arc<E>, EffectContext<S, D>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        let target = TypeId::of::<E>();
        let started = self.started.0;
        Effect {
            can_handle: Arc::new(move |t| t == target),
            started: Some(Arc::new(move |ctx| Box::pin(started(ctx)))),
            handler: Arc::new(move |value, _, ctx| {
                let typed = value.downcast::<E>().expect("type checked by can_handle");
                Box::pin(handler(typed, ctx))
            }),
        }
    }
}

// =============================================================================
// run() - Untyped Events (on_any), No Transition, No Started
// =============================================================================

impl EffectBuilder<Untyped, NoFilter, NoTransition, NoStarted> {
    /// Set the handler (terminal operation).
    pub fn run<S, D, H, Fut>(self, handler: H) -> Effect<S, D>
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
                Box::pin(handler(event, ctx))
            }),
        }
    }
}

// =============================================================================
// run() - Untyped Events (on_any), With Transition, No Started
// Note: Handler receives only ctx, not the event!
// =============================================================================

impl<S, P> EffectBuilder<Untyped, NoFilter, WithTransition<S, P>, NoStarted>
where
    S: Clone + Send + Sync + 'static,
    P: Fn(&S, &S) -> bool + Send + Sync + 'static,
{
    /// Set the handler (terminal operation).
    /// Note: Handler receives only the context, not the event,
    /// since transitions are about state changes.
    pub fn run<D, H, Fut>(self, handler: H) -> Effect<S, D>
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
                    return Box::pin(async { Ok(()) });
                }
                Box::pin(handler(ctx))
            }),
        }
    }
}

// =============================================================================
// run() - Untyped Events (on_any), No Transition, With Started
// =============================================================================

impl<S, D, St, StFut> EffectBuilder<Untyped, NoFilter, NoTransition, WithStarted<S, D, St>>
where
    S: Clone + Send + Sync + 'static,
    D: Send + Sync + 'static,
    St: Fn(EffectContext<S, D>) -> StFut + Send + Sync + 'static,
    StFut: Future<Output = Result<()>> + Send + 'static,
{
    /// Set the handler (terminal operation).
    pub fn run<H, Fut>(self, handler: H) -> Effect<S, D>
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
                Box::pin(handler(event, ctx))
            }),
        }
    }
}

// =============================================================================
// run() - Untyped Events (on_any), With Transition, With Started
// Note: Handler receives only ctx, not the event!
// =============================================================================

impl<S, D, St, StFut, P> EffectBuilder<Untyped, NoFilter, WithTransition<S, P>, WithStarted<S, D, St>>
where
    S: Clone + Send + Sync + 'static,
    D: Send + Sync + 'static,
    St: Fn(EffectContext<S, D>) -> StFut + Send + Sync + 'static,
    StFut: Future<Output = Result<()>> + Send + 'static,
    P: Fn(&S, &S) -> bool + Send + Sync + 'static,
{
    /// Set the handler (terminal operation).
    pub fn run<H, Fut>(self, handler: H) -> Effect<S, D>
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
                    return Box::pin(async { Ok(()) });
                }
                Box::pin(handler(ctx))
            }),
        }
    }
}

// =============================================================================
// Type aliases for backwards compatibility (if needed externally)
// =============================================================================

/// Builder for typed event effects (alias for backward compatibility).
pub type OnBuilder<E> = EffectBuilder<Typed<E>, NoFilter, NoTransition, NoStarted>;

/// Builder for effects with a filter (alias for backward compatibility).
pub type OnFilteredBuilder<E, F> = EffectBuilder<Typed<E>, WithFilter<F>, NoTransition, NoStarted>;

/// Builder for effects with a transition predicate (alias for backward compatibility).
pub type OnTransitionBuilder<E, S, P> =
    EffectBuilder<Typed<E>, NoFilter, WithTransition<S, P>, NoStarted>;

/// Builder for effects with both filter and transition (alias for backward compatibility).
pub type OnFilteredTransitionBuilder<E, S, F, P> =
    EffectBuilder<Typed<E>, WithFilter<F>, WithTransition<S, P>, NoStarted>;

/// Builder for effects with a started handler (alias for backward compatibility).
pub type OnStartedBuilder<E, S, D, St> =
    EffectBuilder<Typed<E>, NoFilter, NoTransition, WithStarted<S, D, St>>;

/// Builder for effects that handle all events (alias for backward compatibility).
pub type OnAnyBuilder = EffectBuilder<Untyped, NoFilter, NoTransition, NoStarted>;

/// Builder for on_any effects with a started handler (alias for backward compatibility).
pub type OnAnyStartedBuilder<S, D, St> =
    EffectBuilder<Untyped, NoFilter, NoTransition, WithStarted<S, D, St>>;

/// Builder for on_any effects with a transition predicate (alias for backward compatibility).
pub type OnAnyTransitionBuilder<S, P> =
    EffectBuilder<Untyped, NoFilter, WithTransition<S, P>, NoStarted>;

/// Builder for on_any effects with both started and transition (alias for backward compatibility).
pub type OnAnyStartedTransitionBuilder<S, D, St, P> =
    EffectBuilder<Untyped, NoFilter, WithTransition<S, P>, WithStarted<S, D, St>>;

// =============================================================================
// group() - Compose multiple effects
// =============================================================================

/// Compose multiple effects into a single effect.
///
/// # Example
///
/// ```ignore
/// effect::group([
///     effect::on::<EventA>().run(handle_a),
///     effect::on::<EventB>().run(handle_b),
///     effect::on::<EventC>().run(handle_c),
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
                        for effect in effects.iter() {
                            if (effect.can_handle)(type_id) {
                                (effect.handler)(value.clone(), type_id, ctx.clone()).await?;
                            }
                        }
                        Ok(())
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
/// Use this for background tasks that don't need to handle events,
/// like websocket connections, polling loops, or cleanup tasks.
///
/// The task keeps the store alive until it completes (via `settled()`).
///
/// # Example
///
/// ```ignore
/// store.with_effect(effect::task(|ctx| async move {
///     while let Some(msg) = ws.recv().await {
///         if ctx.is_cancelled() {
///             break;
///         }
///         ctx.emit(MessageReceived(msg));
///     }
///     Ok(())
/// }));
/// ```
pub fn task<S, D, F, Fut>(f: F) -> Effect<S, D>
where
    S: Clone + Send + Sync + 'static,
    D: Send + Sync + 'static,
    F: Fn(EffectContext<S, D>) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<()>> + Send + 'static,
{
    Effect {
        can_handle: Arc::new(|_| false), // Never handles events
        started: Some(Arc::new(move |ctx| Box::pin(f(ctx)))),
        handler: Arc::new(|_, _, _| Box::pin(async { Ok(()) })), // No-op
    }
}

// =============================================================================
// bridge() - Bidirectional relay connection
// =============================================================================

/// Create a bidirectional bridge to a relay channel.
///
/// This effect:
/// - Forwards all store events to the relay via `WeakSender`
/// - Emits relay events back to the store via `RelayReceiver`
/// - Does NOT keep the relay alive (uses weak sender)
/// - Keeps the store alive until the relay closes
/// - Prevents feedback loops using unique origin tracking
///
/// # Example
///
/// ```ignore
/// let (tx, rx) = Relay::channel();
///
/// // Task owns the channel
/// tokio::spawn(async move {
///     let _owner = tx;
///     // ... produce events ...
/// });
///
/// let store = Engine::new(State::default())
///     .with_effect(effect::bridge(tx.weak(), rx.clone()));
///
/// let handle = store.activate(BridgeState::default());
/// // Engine stays alive until channel closes (tx dropped)
/// handle.settled().await;
/// ```
pub fn bridge<S, D>(tx: pipedream::WeakSender, rx: pipedream::RelayReceiver) -> Effect<S, D>
where
    S: Clone + Send + Sync + 'static,
    D: Send + Sync + 'static,
{
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

    // Generate a unique ID for this bridge instance to track our own sends.
    // Start at a high value to avoid collision with relay IDs (which also start at 1).
    // Using the high 32 bits set to ensure no overlap with relay IDs.
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
                // Subscribe once, use in recv loop
                let mut receiver = rx.subscribe_all();

                // Foreground task: keeps store alive until relay closes
                ctx.within(move |ctx| async move {
                    while let Some(envelope) = receiver.recv().await {
                        // Skip events that originated from this bridge instance.
                        // This prevents feedback loops where events we send to the relay
                        // get received back and re-emitted to the store.
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
                    // Use send_any_with_origin to stamp events with this bridge's unique ID
                    tx.send_any_with_origin(value, type_id, bridge_origin_id)
                        .await
                        .map_err(|e| anyhow::anyhow!("{}", e))
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

    struct TestEvent {
        value: i32,
    }

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
            on::<TestEvent>().run(|_, _| async { Ok(()) });

        assert!(effect.can_handle(TypeId::of::<TestEvent>()));
        assert!(!effect.can_handle(TypeId::of::<OtherEvent>()));
    }

    #[test]
    fn test_on_any_can_handle() {
        let effect: Effect<TestState, TestDeps> =
            on_any().run(|_, _| async { Ok(()) });

        assert!(effect.can_handle(TypeId::of::<TestEvent>()));
        assert!(effect.can_handle(TypeId::of::<OtherEvent>()));
        assert!(effect.can_handle(TypeId::of::<String>()));
    }

    #[test]
    fn test_group_can_handle() {
        let effect: Effect<TestState, TestDeps> = group([
            on::<TestEvent>().run(|_, _| async { Ok(()) }),
            on::<OtherEvent>().run(|_, _| async { Ok(()) }),
        ]);

        assert!(effect.can_handle(TypeId::of::<TestEvent>()));
        assert!(effect.can_handle(TypeId::of::<OtherEvent>()));
        assert!(!effect.can_handle(TypeId::of::<String>()));
    }

    #[tokio::test]
    async fn test_on_handler_called() {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = counter.clone();

        let effect: Effect<TestState, TestDeps> = on::<TestEvent>().run(move |_, _| {
            let counter = counter_clone.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        });

        let ctx = create_test_ctx();
        let event: Arc<dyn Any + Send + Sync> = Arc::new(TestEvent { value: 42 });

        effect
            .call_handler(event, TypeId::of::<TestEvent>(), ctx)
            .await
            .unwrap();

        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_filter_skips_non_matching() {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = counter.clone();

        let effect: Effect<TestState, TestDeps> = on::<TestEvent>()
            .filter(|e| e.value > 10)
            .run(move |_, _| {
                let counter = counter_clone.clone();
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }
            });

        let ctx = create_test_ctx();

        // Should skip - value <= 10
        let event1: Arc<dyn Any + Send + Sync> = Arc::new(TestEvent { value: 5 });
        effect
            .call_handler(event1, TypeId::of::<TestEvent>(), ctx.clone())
            .await
            .unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 0);

        // Should run - value > 10
        let event2: Arc<dyn Any + Send + Sync> = Arc::new(TestEvent { value: 15 });
        effect
            .call_handler(event2, TypeId::of::<TestEvent>(), ctx)
            .await
            .unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    // ==================== TASK TESTS ====================

    #[test]
    fn test_task_never_handles_events() {
        let effect: Effect<TestState, TestDeps> = task(|_ctx| async { Ok(()) });

        // task() should never handle any events
        assert!(!effect.can_handle(TypeId::of::<TestEvent>()));
        assert!(!effect.can_handle(TypeId::of::<OtherEvent>()));
        assert!(!effect.can_handle(TypeId::of::<String>()));
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

    // ==================== BRIDGE TESTS ====================

    mod bridge_tests {
        use super::super::bridge;
        use crate::{effect, reducer, Engine};
        use pipedream::Relay;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        #[derive(Clone, Default, Debug)]
        struct BridgeState {
            message_count: i32,
        }

        #[derive(Clone, Debug)]
        struct MessageFromRelay {
            content: String,
        }

        #[derive(Clone, Debug)]
        struct MessageToRelay {
            content: String,
        }

        #[derive(Clone, Debug)]
        struct InternalEvent;

        #[tokio::test]
        async fn bridge_receives_events_from_relay() {
            // Test: Events sent to relay flow into store via bridge
            let received = Arc::new(parking_lot::Mutex::new(Vec::new()));
            let received_clone = received.clone();

            let (tx, rx) = Relay::channel();

            let store: Engine<BridgeState> = Engine::new()
                .with_reducer(
                    reducer::on::<MessageFromRelay>().run(|state: BridgeState, _| BridgeState {
                        message_count: state.message_count + 1,
                        ..state
                    }),
                )
                .with_effect(effect::on::<MessageFromRelay>().run(move |event, _ctx| {
                    let r = received_clone.clone();
                    async move {
                        r.lock().push(event.content.clone());
                        Ok(())
                    }
                }))
                .with_effect(bridge(tx.weak(), rx.clone()));

            let handle = store.activate(BridgeState::default());

            // Give the bridge's within() task time to start polling
            // The bridge subscribes asynchronously via started(), so we need enough
            // time for the task to be polled and the subscription to be created.
            for _ in 0..10 {
                tokio::task::yield_now().await;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;

            // Send events to relay (should flow into store)
            tx.send(MessageFromRelay {
                content: "hello".into(),
            })
            .await
            .unwrap();
            tx.send(MessageFromRelay {
                content: "world".into(),
            })
            .await
            .unwrap();

            // Give time for processing
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

            // Cancel context to trigger settlement
            handle.cancel();
            handle.settled().await.unwrap();

            // Verify events were received
            let messages = received.lock().clone();
            assert_eq!(messages.len(), 2);
            assert!(messages.contains(&"hello".to_string()));
            assert!(messages.contains(&"world".to_string()));
            assert_eq!(handle.context.curr_state().message_count, 2);
        }

        #[tokio::test]
        async fn bridge_sends_events_to_relay() {
            // Test: Events emitted by store effects flow out to relay
            let (tx, rx) = Relay::channel();

            // Subscribe to relay to capture outgoing events
            let mut subscriber = rx.subscribe::<MessageToRelay>();

            let store: Engine<BridgeState> = Engine::new()
                // When we get InternalEvent, emit MessageToRelay
                .with_effect(effect::on::<InternalEvent>().run(|_event, ctx| async move {
                    ctx.emit(MessageToRelay {
                        content: "from store".into(),
                    });
                    Ok(())
                }))
                .with_effect(bridge(tx.weak(), rx.clone()));

            let handle = store.activate(BridgeState::default());

            // Emit an internal event that triggers sending to relay
            handle.context.emit(InternalEvent);

            // Use recv with timeout to wait for the event
            let received = tokio::time::timeout(
                tokio::time::Duration::from_millis(100),
                subscriber.recv(),
            )
            .await;

            // Check that the message was sent to relay
            assert!(
                received.is_ok(),
                "MessageToRelay should have been sent to relay within timeout"
            );
            let msg = received.unwrap();
            assert!(msg.is_some(), "Should have received a message");
            assert_eq!(msg.unwrap().content, "from store");

            // Cancel to trigger settlement
            handle.cancel();
            handle.settled().await.unwrap();
        }

        #[tokio::test]
        async fn bridge_bidirectional_forwarding() {
            // Test: Events flow both ways through bridge
            // When a MessageFromRelay is received, reply with MessageToRelay
            let received_in_store = Arc::new(parking_lot::Mutex::new(Vec::new()));
            let received_clone = received_in_store.clone();

            let (tx, rx) = Relay::channel();

            // Subscribe to relay to capture outgoing events
            let mut outgoing_subscriber = rx.subscribe::<MessageToRelay>();

            let store: Engine<BridgeState> = Engine::new()
                .with_reducer(
                    reducer::on::<MessageFromRelay>().run(|state: BridgeState, _| BridgeState {
                        message_count: state.message_count + 1,
                        ..state
                    }),
                )
                // Capture incoming messages and reply
                .with_effect(effect::on::<MessageFromRelay>().run(move |event, ctx| {
                    let r = received_clone.clone();
                    async move {
                        r.lock().push(event.content.clone());
                        // Reply with a message
                        ctx.emit(MessageToRelay {
                            content: format!("echo: {}", event.content),
                        });
                        Ok(())
                    }
                }))
                .with_effect(bridge(tx.weak(), rx.clone()));

            let handle = store.activate(BridgeState::default());

            // Emit MessageFromRelay directly via context (like bridge_sends_events_to_relay does)
            handle.context.emit(MessageFromRelay {
                content: "ping".into(),
            });

            // Wait for reply with timeout
            let reply = tokio::time::timeout(
                tokio::time::Duration::from_millis(100),
                outgoing_subscriber.recv(),
            )
            .await;

            // Cancel to trigger settlement
            handle.cancel();
            handle.settled().await.unwrap();

            // Verify incoming message was received
            let messages = received_in_store.lock().clone();
            assert_eq!(messages, vec!["ping"]);

            // Verify outgoing reply was sent to relay
            assert!(
                reply.is_ok(),
                "MessageToRelay should have been sent to relay within timeout"
            );
            let reply_msg = reply.unwrap();
            assert!(reply_msg.is_some(), "Should have received reply");
            assert_eq!(reply_msg.unwrap().content, "echo: ping");
        }

        #[tokio::test]
        async fn bridge_keeps_store_alive_until_relay_closes() {
            // Test: Engine settlement blocks until relay is closed
            // The bridge's within() task keeps the store alive as a foreground task

            let effect_started = Arc::new(AtomicUsize::new(0));
            let effect_started_clone = effect_started.clone();

            let (tx, rx) = Relay::channel();
            let _tx = tx; // Keep channel alive

            let store: Engine<BridgeState> = Engine::new()
                .with_effect(
                    effect::on_any()
                        .started(move |_ctx| {
                            let e = effect_started_clone.clone();
                            async move {
                                e.fetch_add(1, Ordering::Relaxed);
                                Ok(())
                            }
                        })
                        .run(|_, _| async { Ok(()) }),
                )
                .with_effect(bridge(_tx.weak(), rx.clone()));

            let handle = store.activate(BridgeState::default());

            // Verify started was called
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
            assert_eq!(effect_started.load(Ordering::Relaxed), 1);

            // Spawn a task to check if settled() is blocked
            let settled_completed = Arc::new(AtomicUsize::new(0));
            let settled_clone = settled_completed.clone();

            let settle_task = {
                let settled_fut = handle.settled_owned();
                tokio::spawn(async move {
                    // settled() may return error due to feedback, but we're testing that it blocks
                    let _ = settled_fut.await;
                    settled_clone.fetch_add(1, Ordering::Relaxed);
                })
            };

            // Wait a bit - settled should NOT complete yet
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
            assert_eq!(
                settled_completed.load(Ordering::Relaxed),
                0,
                "settled() should be blocked while bridge is active"
            );

            // Cancel context - now settled should complete
            handle.cancel();

            // Wait for settle task
            let _ = settle_task.await;

            assert_eq!(
                settled_completed.load(Ordering::Relaxed),
                1,
                "settled() should complete after context is cancelled"
            );
        }

        // =============================================================================
        // Edge Case & Stress Tests
        // =============================================================================

        #[tokio::test]
        async fn bridge_stress_concurrent_senders() {
            // Test: Multiple tasks sending events concurrently shouldn't cause races
            let received = Arc::new(AtomicUsize::new(0));
            let received_clone = received.clone();

            let (tx, rx) = Relay::channel();
            let tx_weak = tx.weak();

            let store: Engine<BridgeState> = Engine::new()
                .with_effect(effect::on::<MessageFromRelay>().run(move |_event, _ctx| {
                    let r = received_clone.clone();
                    async move {
                        r.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    }
                }))
                .with_effect(bridge(tx.weak(), rx.clone()));

            let handle = store.activate(BridgeState::default());

            // Give bridge time to start
            for _ in 0..10 {
                tokio::task::yield_now().await;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;

            // Spawn many concurrent senders
            let num_senders = 10;
            let events_per_sender = 100;
            let mut sender_handles = Vec::new();

            for sender_id in 0..num_senders {
                let tx_weak = tx_weak.clone();
                let task_handle = tokio::spawn(async move {
                    for i in 0..events_per_sender {
                        let _ = tx_weak
                            .send(MessageFromRelay {
                                content: format!("sender-{}-msg-{}", sender_id, i),
                            })
                            .await;
                    }
                });
                sender_handles.push(task_handle);
            }

            // Wait for all senders to complete
            for h in sender_handles {
                let _ = h.await;
            }

            // Give time for all events to process
            tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

            handle.cancel();
            handle.settled().await.unwrap();

            // All events should be received
            let total_expected = num_senders * events_per_sender;
            assert_eq!(
                received.load(Ordering::SeqCst),
                total_expected,
                "All {} events should be received under concurrent load",
                total_expected
            );
        }

        #[tokio::test]
        async fn bridge_cancel_mid_processing() {
            // Test: Cancelling while events are being processed shouldn't panic
            // Note: With aggressive cancellation, tasks are aborted immediately,
            // so we only verify graceful shutdown, not event processing
            let processed = Arc::new(AtomicUsize::new(0));
            let processed_clone = processed.clone();

            let (tx, rx) = Relay::channel();

            let store: Engine<BridgeState> = Engine::new()
                .with_effect(effect::on::<MessageFromRelay>().run(move |_event, _ctx| {
                    let p = processed_clone.clone();
                    async move {
                        // Simulate slow processing
                        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                        p.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    }
                }))
                .with_effect(bridge(tx.weak(), rx.clone()));

            let handle = store.activate(BridgeState::default());

            // Give bridge time to start
            for _ in 0..10 {
                tokio::task::yield_now().await;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;

            // Send several events
            for i in 0..5 {
                let _ = tx
                    .send(MessageFromRelay {
                        content: format!("msg-{}", i),
                    })
                    .await;
            }

            // Cancel immediately while events are still processing
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
            handle.cancel();

            // Engine should settle without panicking
            let result = handle.settled().await;
            assert!(
                result.is_ok(),
                "Engine should settle gracefully when cancelled mid-processing"
            );

            // Processing state is undefined - cancellation may abort before any complete
            // The key assertion is that we don't hang or panic
        }

        #[tokio::test]
        async fn bridge_high_throughput_burst() {
            // Test: Rapid burst of events shouldn't cause backpressure issues
            let received = Arc::new(AtomicUsize::new(0));
            let received_clone = received.clone();

            let (tx, rx) = Relay::channel();
            let tx_weak = tx.weak();

            let store: Engine<BridgeState> = Engine::new()
                .with_effect(effect::on::<MessageFromRelay>().run(move |_event, _ctx| {
                    let r = received_clone.clone();
                    async move {
                        r.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    }
                }))
                .with_effect(bridge(tx.weak(), rx.clone()));

            let handle = store.activate(BridgeState::default());

            // Give bridge time to start
            for _ in 0..10 {
                tokio::task::yield_now().await;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;

            // Send burst of events as fast as possible (no awaiting between sends)
            let burst_size = 1000;
            let send_task = tokio::spawn(async move {
                for i in 0..burst_size {
                    // Fire and forget - don't wait for each send
                    let _ = tx_weak
                        .send(MessageFromRelay {
                            content: format!("burst-{}", i),
                        })
                        .await;
                }
            });

            send_task.await.unwrap();

            // Give time for all events to process
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

            handle.cancel();
            handle.settled().await.unwrap();

            // All burst events should be received
            assert_eq!(
                received.load(Ordering::SeqCst),
                burst_size,
                "All {} burst events should be received",
                burst_size
            );
        }

        #[tokio::test]
        async fn bridge_multiple_bridges_same_store() {
            // Test: Multiple bridges attached to same store should all work
            let received_a = Arc::new(AtomicUsize::new(0));
            let received_b = Arc::new(AtomicUsize::new(0));
            let received_a_clone = received_a.clone();
            let received_b_clone = received_b.clone();

            #[derive(Clone, Debug)]
            struct EventFromRelayA(i32);

            #[derive(Clone, Debug)]
            struct EventFromRelayB(i32);

            let (tx_a, rx_a) = Relay::channel();
            let (tx_b, rx_b) = Relay::channel();

            let store: Engine<BridgeState> = Engine::new()
                .with_effect(effect::on::<EventFromRelayA>().run(move |_event, _ctx| {
                    let r = received_a_clone.clone();
                    async move {
                        r.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    }
                }))
                .with_effect(effect::on::<EventFromRelayB>().run(move |_event, _ctx| {
                    let r = received_b_clone.clone();
                    async move {
                        r.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    }
                }))
                .with_effect(bridge(tx_a.weak(), rx_a.clone()))
                .with_effect(bridge(tx_b.weak(), rx_b.clone()));

            let handle = store.activate(BridgeState::default());

            // Give bridges time to start
            for _ in 0..10 {
                tokio::task::yield_now().await;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;

            // Send events to both relays
            tx_a.send(EventFromRelayA(1)).await.unwrap();
            tx_a.send(EventFromRelayA(2)).await.unwrap();
            tx_b.send(EventFromRelayB(100)).await.unwrap();
            tx_b.send(EventFromRelayB(200)).await.unwrap();
            tx_b.send(EventFromRelayB(300)).await.unwrap();

            // Give time for processing
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

            handle.cancel();
            handle.settled().await.unwrap();

            assert_eq!(
                received_a.load(Ordering::SeqCst),
                2,
                "Should receive 2 events from relay A"
            );
            assert_eq!(
                received_b.load(Ordering::SeqCst),
                3,
                "Should receive 3 events from relay B"
            );
        }

        #[tokio::test]
        async fn bridge_two_bridges_same_relay_creates_feedback() {
            // Test: Two bridges connected to same relay WILL create feedback
            // because each bridge has a unique origin ID, so bridge A's events
            // are accepted by bridge B and vice versa.
            //
            // This is expected behavior - use separate relays if you need isolated stores.
            // This test verifies that cancellation stops the feedback loop cleanly.
            let store1_received = Arc::new(AtomicUsize::new(0));
            let store2_received = Arc::new(AtomicUsize::new(0));
            let store1_clone = store1_received.clone();
            let store2_clone = store2_received.clone();

            let (tx, rx) = Relay::channel();

            // Engine 1: receives events, counts them
            let store1: Engine<BridgeState> = Engine::new()
                .with_effect(effect::on::<MessageFromRelay>().run(move |_event, _ctx| {
                    let r = store1_clone.clone();
                    async move {
                        r.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    }
                }))
                .with_effect(bridge(tx.weak(), rx.clone()));

            // Engine 2: receives events, counts them
            let store2: Engine<BridgeState> = Engine::new()
                .with_effect(effect::on::<MessageFromRelay>().run(move |_event, _ctx| {
                    let r = store2_clone.clone();
                    async move {
                        r.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    }
                }))
                .with_effect(bridge(tx.weak(), rx.clone()));

            let handle1 = store1.activate(BridgeState::default());
            let handle2 = store2.activate(BridgeState::default());

            // Give bridges time to start
            for _ in 0..10 {
                tokio::task::yield_now().await;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;

            // Send event directly to relay - this will trigger feedback loop
            let _ = tx
                .send(MessageFromRelay {
                    content: "trigger".into(),
                })
                .await;

            // Let it run briefly - feedback loop will generate many events
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

            // Cancel both stores - this should stop the feedback loop
            handle1.cancel();
            handle2.cancel();

            // Wait for both to settle (with timeout to prevent hanging)
            let result1 = tokio::time::timeout(
                tokio::time::Duration::from_millis(500),
                handle1.settled(),
            )
            .await;
            let result2 = tokio::time::timeout(
                tokio::time::Duration::from_millis(500),
                handle2.settled(),
            )
            .await;

            // Key assertion: cancellation stops the feedback loop and allows settlement
            assert!(
                result1.is_ok(),
                "Engine 1 should settle within timeout after cancellation"
            );
            assert!(
                result2.is_ok(),
                "Engine 2 should settle within timeout after cancellation"
            );

            // Both stores received multiple events due to feedback (expected behavior)
            let count1 = store1_received.load(Ordering::SeqCst);
            let count2 = store2_received.load(Ordering::SeqCst);
            assert!(
                count1 > 1,
                "Engine 1 should receive multiple events due to feedback loop (got {})",
                count1
            );
            assert!(
                count2 > 1,
                "Engine 2 should receive multiple events due to feedback loop (got {})",
                count2
            );
        }

        #[tokio::test]
        async fn bridge_store_emit_no_self_echo() {
            // Test: Events emitted by store's own effects shouldn't echo back
            // through the bridge
            let effect_runs = Arc::new(AtomicUsize::new(0));
            let effect_runs_clone = effect_runs.clone();

            let (tx, rx) = Relay::channel();
            let _tx = tx; // Keep channel alive

            let store: Engine<BridgeState> = Engine::new()
                .with_effect(effect::on::<MessageToRelay>().run(move |_event, _ctx| {
                    let r = effect_runs_clone.clone();
                    async move {
                        r.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    }
                }))
                .with_effect(bridge(_tx.weak(), rx.clone()));

            let handle = store.activate(BridgeState::default());

            // Give bridge time to start
            for _ in 0..10 {
                tokio::task::yield_now().await;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;

            // Emit event from store - bridge will forward to relay
            // But it should NOT come back and trigger the effect again
            handle.context.emit(MessageToRelay {
                content: "from store".into(),
            });

            // Give time for potential echo
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

            handle.cancel();
            handle.settled().await.unwrap();

            // Effect should run exactly once (from original emit)
            // NOT twice (which would indicate the event echoed back)
            assert_eq!(
                effect_runs.load(Ordering::SeqCst),
                1,
                "Effect should run exactly once - no echo from bridge"
            );
        }

        #[tokio::test]
        async fn bridge_rapid_open_close_cycles() {
            // Test: Rapidly creating and closing bridges shouldn't leak or panic
            // Note: Some cycles may return errors due to timing, which is acceptable
            // The key assertion is that nothing panics
            for cycle in 0..10 {
                let received = Arc::new(AtomicUsize::new(0));
                let received_clone = received.clone();

                let (tx, rx) = Relay::channel();

                let store: Engine<BridgeState> = Engine::new()
                    .with_effect(effect::on::<MessageFromRelay>().run(move |_event, _ctx| {
                        let r = received_clone.clone();
                        async move {
                            r.fetch_add(1, Ordering::SeqCst);
                            Ok(())
                        }
                    }))
                    .with_effect(bridge(tx.weak(), rx.clone()));

                let handle = store.activate(BridgeState::default());

                // Give bridge time to start
                for _ in 0..5 {
                    tokio::task::yield_now().await;
                }

                // Send a few events (ignore errors if cancelled quickly)
                for i in 0..3 {
                    let _ = tx
                        .send(MessageFromRelay {
                            content: format!("cycle-{}-msg-{}", cycle, i),
                        })
                        .await;
                }

                // Cancel context
                handle.cancel();

                // Settle with timeout - shouldn't hang or panic
                let result = tokio::time::timeout(
                    tokio::time::Duration::from_millis(500),
                    handle.settled(),
                )
                .await;

                assert!(
                    result.is_ok(),
                    "Cycle {} should settle within timeout (not hang)",
                    cycle
                );
                // The inner result may be Ok or Err, both are acceptable for rapid cancel
            }
        }

        #[tokio::test]
        async fn bridge_close_before_any_events() {
            // Test: Closing relay before any events are sent should be fine
            let (tx, rx) = Relay::channel();
            let _tx = tx; // Keep channel alive

            let store: Engine<BridgeState> =
                Engine::new().with_effect(bridge(_tx.weak(), rx.clone()));

            let handle = store.activate(BridgeState::default());

            // Give bridge time to start
            for _ in 0..10 {
                tokio::task::yield_now().await;
            }

            // Cancel immediately without sending anything
            handle.cancel();

            let result = handle.settled().await;
            assert!(
                result.is_ok(),
                "Should settle cleanly when cancelled before any events"
            );
        }

        #[tokio::test]
        async fn bridge_cancel_context_closes_relay() {
            // Test: Cancelling the store context should settle cleanly
            let (tx, rx) = Relay::channel();

            let store: Engine<BridgeState> =
                Engine::new().with_effect(bridge(tx.weak(), rx.clone()));

            let handle = store.activate(BridgeState::default());

            // Give bridge time to start
            for _ in 0..10 {
                tokio::task::yield_now().await;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;

            // Cancel the context
            handle.cancel();

            // Wait for settlement with timeout to prevent hanging
            let result = tokio::time::timeout(
                tokio::time::Duration::from_millis(500),
                handle.settled(),
            )
            .await;

            assert!(
                result.is_ok(),
                "Should settle within timeout after cancellation"
            );
            // Inner result may be Ok or Err depending on timing

            // Channel is still open (tx owns it) - sending should work
            let send_result = tx
                .send(MessageFromRelay {
                    content: "after cancel".into(),
                })
                .await;
            // Send succeeds because tx still owns the channel
            assert!(send_result.is_ok());
        }

        #[tokio::test]
        async fn bridge_interleaved_send_receive() {
            // Test: Interleaved sends and receives shouldn't cause ordering issues
            let received_from_relay = Arc::new(parking_lot::Mutex::new(Vec::new()));
            let received_clone = received_from_relay.clone();

            let (tx, rx) = Relay::channel();
            let mut outgoing_subscriber = rx.subscribe::<MessageToRelay>();

            let store: Engine<BridgeState> = Engine::new()
                // When we receive from relay, record it and reply
                .with_effect(effect::on::<MessageFromRelay>().run(move |event, ctx| {
                    let r = received_clone.clone();
                    async move {
                        r.lock().push(event.content.clone());
                        // Reply with acknowledgment
                        ctx.emit(MessageToRelay {
                            content: format!("ack:{}", event.content),
                        });
                        Ok(())
                    }
                }))
                .with_effect(bridge(tx.weak(), rx.clone()));

            let handle = store.activate(BridgeState::default());

            // Give bridge time to start
            for _ in 0..10 {
                tokio::task::yield_now().await;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;

            // Send multiple messages and collect acks
            let mut acks = Vec::new();
            for i in 0..5 {
                tx.send(MessageFromRelay {
                    content: format!("msg-{}", i),
                })
                .await
                .unwrap();

                // Try to receive ack
                if let Ok(Some(ack)) = tokio::time::timeout(
                    tokio::time::Duration::from_millis(100),
                    outgoing_subscriber.recv(),
                )
                .await
                {
                    acks.push(ack.content.clone());
                }
            }

            handle.cancel();
            handle.settled().await.unwrap();

            // Verify all messages were received
            let received = received_from_relay.lock().clone();
            assert_eq!(received.len(), 5, "Should receive all 5 messages");

            // Verify acks were sent (may not receive all due to timing)
            assert!(!acks.is_empty(), "Should receive at least some acks");
            for ack in &acks {
                assert!(ack.starts_with("ack:"), "Ack should have correct format");
            }
        }

        #[tokio::test]
        async fn bridge_large_payload() {
            // Test: Large payloads shouldn't cause issues
            let received = Arc::new(parking_lot::Mutex::new(Vec::new()));
            let received_clone = received.clone();

            let (tx, rx) = Relay::channel();

            let store: Engine<BridgeState> = Engine::new()
                .with_effect(effect::on::<MessageFromRelay>().run(move |event, _ctx| {
                    let r = received_clone.clone();
                    async move {
                        r.lock().push(event.content.len());
                        Ok(())
                    }
                }))
                .with_effect(bridge(tx.weak(), rx.clone()));

            let handle = store.activate(BridgeState::default());

            // Give bridge time to start
            for _ in 0..10 {
                tokio::task::yield_now().await;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;

            // Send messages with increasing payload sizes
            let sizes = vec![1, 100, 1_000, 10_000, 100_000, 1_000_000];
            for size in &sizes {
                let content = "x".repeat(*size);
                tx.send(MessageFromRelay { content })
                    .await
                    .unwrap();
            }

            // Give time for processing
            tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

            handle.cancel();
            handle.settled().await.unwrap();

            let received_sizes = received.lock().clone();
            assert_eq!(
                received_sizes, sizes,
                "All payloads including large ones should be received"
            );
        }
    }
}
