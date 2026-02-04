//! Engine implementation with TypeId-based multi-event dispatch.

use std::any::{Any, TypeId};
use std::sync::Arc;

use anyhow::Result;
use parking_lot::RwLock;
use tracing::info_span;
use uuid::Uuid;

use std::marker::PhantomData;

use crate::effect::{BoxedEmitter, Effect, EffectContext, EventEmitter, EventEnvelope};
use crate::effect_registry::EffectRegistry;
use crate::reducer::Reducer;
use crate::reducer_registry::ReducerRegistry;
use crate::task_group::TaskGroup;
use pipedream::{Relay, RelaySender, RelayReceiver, WeakSender};

struct EngineEmitter<S, D>
where
    S: Clone + Send + Sync + 'static,
    D: Send + Sync + 'static,
{
    state: Arc<RwLock<S>>,
    reducers: Arc<ReducerRegistry<S>>,
    /// Event sender for sending EventEnvelope to effects
    event_sender: WeakSender,
    _marker: PhantomData<D>,
}

impl<S, D> EventEmitter<S, D> for EngineEmitter<S, D>
where
    S: Clone + Send + Sync + 'static,
    D: Send + Sync + 'static,
{
    fn emit(&self, type_id: TypeId, event: Arc<dyn Any + Send + Sync>, ctx: EffectContext<S, D>) {
        // Generate event ID and extract type name for tracing
        let event_id = Uuid::new_v4();
        let event_type_name = std::any::type_name_of_val(event.as_ref());
        let parent_event_id = ctx.current_event_id();

        // Create tracing span for this event
        let span = info_span!(
            "seesaw.event",
            event_type = %event_type_name,
            event_id = %event_id,
            parent_event_id = ?parent_event_id,
        );
        let _guard = span.enter();

        let mut state = self.state.write();

        let prev_state = Arc::new(state.clone());
        let new_state = self
            .reducers
            .apply((*prev_state).clone(), type_id, event.as_ref());

        *state = new_state.clone();

        drop(state); // important

        // Create fresh context with updated states and the new event_id for causation tracking
        let fresh_ctx = ctx.with_states_and_event_id(prev_state, Arc::new(new_state), event_id);

        // Send envelope to effects - they subscribe to EventEnvelope<S, D>
        // origin: 0 means internal event, stream will stamp with its ID
        // visited starts empty because this is a NEW event (not a forwarded one)
        let envelope = EventEnvelope {
            event,
            event_type: type_id,
            ctx: fresh_ctx,
            origin: 0,
            visited: std::collections::HashSet::new(),
        };

        // Clone for the spawned task
        let event_sender = self.event_sender.clone();

        // Spawn a tracked task to await the send.
        // send() waits for tracked subscribers (effect loops) to spawn handlers,
        // ensuring handlers are in TaskGroup before this task completes.
        ctx.within(move |_| async move {
            let _ = event_sender
                .send_any(
                    Arc::new(envelope) as Arc<dyn Any + Send + Sync>,
                    TypeId::of::<EventEnvelope<S, D>>(),
                )
                .await;

            Ok(())
        });
    }
}

impl<S, D> Clone for EngineEmitter<S, D>
where
    S: Clone + Send + Sync + 'static,
    D: Send + Sync + 'static,
{
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
            reducers: self.reducers.clone(),
            event_sender: self.event_sender.clone(),
            _marker: PhantomData,
        }
    }
}

/// A handle to a running store with active effects.
pub struct Handle<S, D>
where
    S: Send + Sync + 'static,
    D: Send + Sync + 'static,
{
    tasks: Arc<TaskGroup>,
    /// Context passed to effects
    pub context: EffectContext<S, D>,
    /// Event sender for external events (writable side)
    pub event_sender: WeakSender,
    /// Event receiver for effect-emitted events (readable side)
    pub event_receiver: RelayReceiver,
    /// Keep internal sender alive to prevent channel closure
    _internal_tx: Arc<RelaySender>,
    /// Keep external sender alive
    _external_tx: RelaySender,
}

impl<S, D> Handle<S, D>
where
    S: Send + Sync + 'static,
    D: Send + Sync + 'static,
{
    /// Execute an action that returns an event to dispatch.
    ///
    /// The returned event (if not `()`) will be dispatched to trigger effects.
    /// Use `settled()` to wait for all effects to complete.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let handle = engine.activate(State::default());
    /// handle.run(|ctx| Ok(OrderPlaced { id: 123, total: 99.99 }))?;
    /// handle.settled().await?;
    /// ```
    pub fn run<F, E>(&self, f: F) -> Result<()>
    where
        F: FnOnce(&EffectContext<S, D>) -> Result<E>,
        E: Send + Sync + 'static,
    {
        let event = f(&self.context)?;
        // Dispatch the event if it's not ()
        if std::any::TypeId::of::<E>() != std::any::TypeId::of::<()>() {
            self.context.emit(event);
        }
        Ok(())
    }

    /// Process an event to completion.
    ///
    /// This is a convenience method that combines `run()` and `settled()`.
    /// The returned event will be dispatched and all cascading effects
    /// will complete before this method returns.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let handle = engine.activate(State::default());
    /// handle.process(|ctx| async move {
    ///     let data = fetch_data().await?;
    ///     Ok(OrderPlaced { id: 123, data })
    /// }).await?;
    /// println!("All effects completed");
    /// ```
    pub async fn process<'a, F, Fut, E>(&'a self, f: F) -> Result<()>
    where
        F: FnOnce(&'a EffectContext<S, D>) -> Fut,
        Fut: std::future::Future<Output = Result<E>> + Send + 'a,
        E: Send + Sync + 'static,
    {
        let event = f(&self.context).await?;
        // Dispatch the event if it's not ()
        if std::any::TypeId::of::<E>() != std::any::TypeId::of::<()>() {
            self.context.emit(event);
        }
        self.tasks.settled().await?;
        Ok(())
    }

    /// Wait for all tasks to complete.
    pub async fn settled(&self) -> Result<()> {
        self.tasks.settled().await
    }

    /// Cancel all tasks.
    pub fn cancel(&self) {
        self.tasks.cancel();
    }

    /// Get an owned future that resolves when all tasks complete.
    /// Useful for spawning into a separate task.
    pub fn settled_owned(&self) -> impl std::future::Future<Output = Result<()>> + Send + 'static {
        let tasks = self.tasks.clone();
        async move { tasks.settled().await }
    }
}

impl<S, D> Drop for Handle<S, D>
where
    S: Send + Sync + 'static,
    D: Send + Sync + 'static,
{
    fn drop(&mut self) {
        self.tasks.cancel();
    }
}

/// An event-driven runtime with TypeId-based multi-event dispatch.
///
/// Engine holds effect and reducer registries but no state.
/// State is passed per-activation via `activate(initial_state)`.
pub struct Engine<S, D = ()>
where
    S: Clone + Send + Sync + 'static,
    D: Send + Sync + 'static,
{
    reducers: Arc<ReducerRegistry<S>>,
    deps: Arc<D>,
    effect_registry: Arc<EffectRegistry<S, D>>,
}

impl<S> Engine<S, ()>
where
    S: Clone + Send + Sync + 'static,
{
    /// Create a new Engine with no dependencies.
    pub fn new() -> Self {
        Self::with_deps(())
    }
}

impl<S, D> Engine<S, D>
where
    S: Clone + Send + Sync + 'static,
    D: Send + Sync + 'static,
{
    /// Create a new Engine with dependencies.
    pub fn with_deps(deps: D) -> Self {
        Engine {
            reducers: Arc::new(ReducerRegistry::new()),
            deps: Arc::new(deps),
            effect_registry: Arc::new(EffectRegistry::new()),
        }
    }

    /// Register a reducer.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use seesaw::reducer;
    ///
    /// store.with_reducer(reducer::on::<MyEvent>().run(|state, event| {
    ///     State { count: state.count + event.amount, ..state }
    /// }))
    /// ```
    pub fn with_reducer(self, reducer: Reducer<S>) -> Self {
        self.reducers.register(reducer);
        self
    }

    /// Register an effect.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use seesaw::effect;
    ///
    /// store.with_effect(effect::on::<MyEvent>().then(handle_my_event))
    /// ```
    pub fn with_effect(self, effect: Effect<S, D>) -> Self {
        self.effect_registry.register(effect);
        self
    }

    /// Use an existing effect registry.
    pub fn with_effect_registry(mut self, registry: Arc<EffectRegistry<S, D>>) -> Self {
        self.effect_registry = registry;
        self
    }

    /// Activate effects with initial state and return a handle for lifecycle management.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let handle = engine.activate(State::default());
    /// let result = handle.run(|ctx| process_order(order, ctx))?;
    /// handle.settled().await?;
    /// ```
    pub fn activate(&self, initial_state: S) -> Handle<S, D> {
        let state = Arc::new(RwLock::new(initial_state));
        let tasks = TaskGroup::new();

        // Architecture (simplified with origin tracking):
        // - ONE event_stream carries EventEnvelope<S, D>
        // - Effects subscribe to event_stream
        // - Emitter sends to event_stream (origin: 0, stamped by stream)
        // - External interface converts raw ↔ EventEnvelope with origin preservation
        // - Origin tracking prevents echo: external events not sent back to external

        // Internal event relay - carries EventEnvelope<S, D>
        let (internal_tx, internal_rx) = Relay::channel();

        let emitter = EngineEmitter {
            state: state.clone(),
            reducers: self.reducers.clone(),
            event_sender: internal_tx.weak(),
            _marker: PhantomData,
        };

        // Snapshot current state
        let state_snapshot = Arc::new(state.read().clone());

        let context = EffectContext::new(
            state_snapshot.clone(),
            state_snapshot,
            state.clone(),
            self.deps.clone(),
            Arc::new(emitter) as BoxedEmitter<S, D>,
            tasks.clone(),
        );

        // Effects subscribe to EventEnvelope<S, D>
        self.effect_registry.start_effects(&internal_rx, &context);

        // External interface: raw events with origin tracking for echo prevention
        let (external_tx, external_rx) = Relay::channel();
        let external_id = external_rx.id();

        // Get internal ID for origin tracking
        let internal_id = internal_rx.id();

        // External → internal: raw events become EventEnvelope with preserved origin
        forward_raw_to_envelope::<S, D>(
            &external_rx,
            internal_tx.weak(),
            state.clone(),
            self.reducers.clone(),
            context.clone(),
            internal_id,  // Use internal_id to mark forwarded events
        );

        // Internal → external: EventEnvelope becomes raw, origin tracking prevents echo
        forward_envelope_to_raw::<S, D>(&internal_rx, external_tx.weak(), internal_id, external_id);

        // Store internal sender to keep channel alive
        let internal_tx = Arc::new(internal_tx);

        Handle {
            tasks,
            context,
            event_sender: external_tx.weak(),
            event_receiver: external_rx,
            _internal_tx: internal_tx,
            _external_tx: external_tx,
        }
    }
}

/// Forward raw events from external to internal event_stream as EventEnvelope.
///
/// Uses chain-based origin tracking to skip events that have already visited
/// internal (prevents internal→external→internal echo loops).
fn forward_raw_to_envelope<S, D>(
    external_rx: &RelayReceiver,
    event_stream_tx: WeakSender,
    state: Arc<RwLock<S>>,
    reducers: Arc<ReducerRegistry<S>>,
    ctx: EffectContext<S, D>,
    internal_id: u64,  // ID of internal relay to detect echoes
) where
    S: Clone + Send + Sync + 'static,
    D: Send + Sync + 'static,
{
    let external_id = external_rx.id();
    let mut sub = external_rx.subscribe_all();

    tokio::spawn(async move {
        while let Some(envelope) = sub.recv().await {
            // Skip events that were forwarded from internal (origin=internal_id)
            // This prevents internal→external→internal feedback loops
            if envelope.origin() == internal_id {
                continue;
            }

            let type_id = envelope.type_id();
            let event = envelope.value.clone();

            // Generate event ID for tracing
            let event_id = Uuid::new_v4();

            // Run reducers (block scope ensures guard is dropped before await)
            let (prev_state, new_state) = {
                let mut state_guard = state.write();
                let prev_state = Arc::new(state_guard.clone());
                let new_state = reducers.apply((*prev_state).clone(), type_id, event.as_ref());
                *state_guard = new_state.clone();
                (prev_state, new_state)
            };

            // Build visited set: this event came from external, so mark external as visited
            let mut visited = std::collections::HashSet::new();
            visited.insert(external_id);
            let visited = Arc::new(visited);

            // Create fresh context with updated states and visited set
            let fresh_ctx = ctx
                .with_states_and_event_id(prev_state, Arc::new(new_state), event_id)
                .with_visited(visited.clone());

            // Create EventEnvelope with external origin and visited chain
            let event_envelope = EventEnvelope {
                event,
                event_type: type_id,
                ctx: fresh_ctx,
                origin: external_id,
                visited: (*visited).clone(),
            };

            // Send with external origin preserved
            let _ = event_stream_tx
                .send_any_with_origin(
                    Arc::new(event_envelope) as Arc<dyn Any + Send + Sync>,
                    TypeId::of::<EventEnvelope<S, D>>(),
                    external_id,
                )
                .await;
        }
    });
}

/// Forward EventEnvelope from internal to external as raw events.
///
/// Uses chain-based origin tracking to prevent echo: events that have already
/// visited external are not sent back to external.
fn forward_envelope_to_raw<S, D>(
    event_stream_rx: &RelayReceiver,
    external_tx: WeakSender,
    forward_origin: u64,     // Origin to stamp on forwarded events
    external_id: u64,        // External relay ID to check for echo
)
where
    S: Send + Sync + 'static,
    D: Send + Sync + 'static,
{
    let mut sub = event_stream_rx.subscribe::<EventEnvelope<S, D>>();

    tokio::spawn(async move {
        while let Some(envelope) = sub.recv().await {
            // Echo prevention: skip events that have already visited external
            if envelope.visited.contains(&external_id) {
                continue;
            }

            // Send raw event to external with forward_origin
            let _ = external_tx
                .send_any_with_origin(envelope.event.clone(), envelope.event_type, forward_origin)
                .await;
        }
    });
}

impl<S, D> Clone for Engine<S, D>
where
    S: Clone + Send + Sync + 'static,
    D: Send + Sync + 'static,
{
    fn clone(&self) -> Self {
        Self {
            reducers: self.reducers.clone(),
            deps: self.deps.clone(),
            effect_registry: self.effect_registry.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{effect, reducer};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone, Debug, Default, PartialEq)]
    struct Counter {
        value: i32,
    }

    #[derive(Clone, Debug)]
    struct Increment {
        amount: i32,
    }

    #[derive(Clone, Debug)]
    struct Decrement {
        amount: i32,
    }

    #[derive(Clone, Debug)]
    struct Reset;

    // Helper function to forward events from receiver to sender (replaces old .forward() API)
    fn forward_events(rx: &RelayReceiver, tx: WeakSender) {
        let mut sub = rx.subscribe_all();
        tokio::spawn(async move {
            while let Some(envelope) = sub.recv().await {
                let type_id = envelope.type_id();
                let value = envelope.value;
                let _ = tx.send_any(value, type_id).await;
            }
        });
    }

    #[tokio::test]
    async fn store_returns_initial_state() {
        let store: Engine<Counter> = Engine::new();
        let result = store.activate(Counter { value: 42 });
        assert_eq!(result.context.curr_state(), Counter { value: 42 });
    }

    #[tokio::test]
    async fn store_with_reducer_applies_on_emit() {
        let store: Engine<Counter> = Engine::new()
            .with_reducer(reducer::on::<Increment>().run(|state: Counter, event| Counter {
                value: state.value + event.amount,
            }))
            .with_reducer(reducer::on::<Decrement>().run(|state: Counter, event| Counter {
                value: state.value - event.amount,
            }));

        let result = store.activate(Counter::default());
        result.context.emit(Increment { amount: 5 });
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        assert_eq!(result.context.curr_state().value, 5);

        result.context.emit(Decrement { amount: 2 });
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        assert_eq!(result.context.curr_state().value, 3);
    }

    #[tokio::test]
    async fn store_with_handler_receives_events() {
        let handler_calls = Arc::new(AtomicUsize::new(0));
        let counter = handler_calls.clone();
        let store: Engine<Counter> = Engine::new().with_effect(
            effect::on::<Increment>().then(move |_event, _ctx| {
                let c = counter.clone();
                async move {
                    c.fetch_add(1, Ordering::Relaxed);
                    Ok(())
                }
            }),
        );

        let result = store.activate(Counter::default());
        result.context.emit(Increment { amount: 1 });
        result.context.emit(Increment { amount: 2 });

        result.settled().await.unwrap();
        assert_eq!(handler_calls.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn process_runs_and_settles() {
        let handler_calls = Arc::new(AtomicUsize::new(0));
        let counter = handler_calls.clone();
        let store: Engine<Counter> = Engine::new()
            .with_reducer(reducer::on::<Increment>().run(|state: Counter, event| Counter {
                value: state.value + event.amount,
            }))
            .with_effect(
                effect::on::<Increment>().then(move |_event, _ctx| {
                    let c = counter.clone();
                    async move {
                        c.fetch_add(1, Ordering::Relaxed);
                        Ok(())
                    }
                }),
            );

        let handle = store.activate(Counter::default());

        // Use run() to dispatch events, then settled() to wait
        handle.run(|_ctx| Ok(Increment { amount: 10 })).unwrap();
        handle.run(|_ctx| Ok(Increment { amount: 5 })).unwrap();

        // Wait for effects
        handle.settled().await.unwrap();

        // Effects should have completed
        assert_eq!(handler_calls.load(Ordering::Relaxed), 2);
        // State should be updated
        assert_eq!(handle.context.curr_state().value, 15);
    }

    #[tokio::test]
    async fn store_reducer_and_handler_together() {
        let log = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let log_clone = log.clone();
        let store: Engine<Counter> = Engine::new()
            .with_reducer(reducer::on::<Increment>().run(|state: Counter, event| Counter {
                value: state.value + event.amount,
            }))
            .with_effect(effect::on::<Increment>().then(move |event, _ctx| {
                let l = log_clone.clone();
                async move {
                    l.lock().push(event.amount);
                    Ok(())
                }
            }));

        let result = store.activate(Counter::default());
        result.context.emit(Increment { amount: 10 });
        result.context.emit(Increment { amount: 5 });

        result.settled().await.unwrap();

        let logged = log.lock().clone();
        assert!(logged.contains(&10));
        assert!(logged.contains(&5));
        assert_eq!(result.context.curr_state().value, 15);
    }

    #[tokio::test]
    async fn store_multiple_event_types() {
        let store: Engine<Counter> = Engine::new()
            .with_reducer(reducer::on::<Increment>().run(|state: Counter, event| Counter {
                value: state.value + event.amount,
            }))
            .with_reducer(reducer::on::<Decrement>().run(|state: Counter, event| Counter {
                value: state.value - event.amount,
            }))
            .with_reducer(reducer::on::<Reset>().run(|_: Counter, _| Counter { value: 0 }));

        let result = store.activate(Counter::default());

        result.context.emit(Increment { amount: 100 });
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        assert_eq!(result.context.curr_state().value, 100);

        result.context.emit(Decrement { amount: 30 });
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        assert_eq!(result.context.curr_state().value, 70);

        result.context.emit(Reset);
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        assert_eq!(result.context.curr_state().value, 0);
    }

    #[tokio::test]
    async fn store_with_deps() {
        #[derive(Clone)]
        struct Deps {
            multiplier: i32,
        }

        #[derive(Clone, Debug)]
        struct Bonus {
            amount: i32,
        }

        let store: Engine<Counter, Deps> =
            Engine::with_deps(Deps { multiplier: 3 })
                .with_reducer(reducer::on::<Increment>().run(|state: Counter, event| Counter {
                    value: state.value + event.amount,
                }))
                .with_reducer(reducer::on::<Bonus>().run(|state: Counter, event| Counter {
                    value: state.value + event.amount,
                }))
                .with_effect(effect::on::<Increment>().then(|event, ctx: EffectContext<Counter, Deps>| async move {
                    let bonus = event.amount * (ctx.deps().multiplier - 1);
                    ctx.emit(Bonus { amount: bonus });
                    Ok(())
                }));

        let result = store.activate(Counter::default());
        result.context.emit(Increment { amount: 10 });

        result.settled().await.unwrap();
        assert_eq!(result.context.curr_state().value, 30);
    }

    #[tokio::test]
    async fn store_clones_handlers_per_context() {
        // With the new builder API, effects are created via closures that are cloned
        // when the store is activated. This test verifies that effects are cloned
        // for each activation.
        static CALL_COUNT: AtomicUsize = AtomicUsize::new(0);

        let store: Engine<Counter> = Engine::new()
            .with_effect(effect::on::<Increment>().then(|_event, _ctx| async move {
                CALL_COUNT.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }));

        // Create two contexts
        let result1 = store.activate(Counter::default());
        let result2 = store.activate(Counter::default());

        // Emit to both
        result1.context.emit(Increment { amount: 1 });
        result2.context.emit(Increment { amount: 1 });

        result1.settled().await.unwrap();
        result2.settled().await.unwrap();

        // Both activations should have handled the event
        assert_eq!(CALL_COUNT.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn started_effect_emits_events_handled_on_foreground() {
        // This tests that events emitted from started() effects are
        // handled on the foreground task group, so settled() waits for them.

        #[derive(Clone, Debug)]
        struct Started;

        let handled = Arc::new(AtomicUsize::new(0));
        let handled_clone = handled.clone();

        let store: Engine<Counter> = Engine::new()
            .with_effect(
                effect::on_any()
                    .started(|ctx| async move {
                        ctx.emit(Started);
                        Ok(())
                    })
                    .run(|_, _| async { Ok(()) }),
            )
            .with_effect(effect::on::<Started>().then(move |_event, _ctx| {
                let h = handled_clone.clone();
                async move {
                    // Small delay to ensure this is actually waited on
                    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
                    h.fetch_add(1, Ordering::Relaxed);
                    Ok(())
                }
            }));

        let session = store.activate(Counter::default());
        session.settled().await.unwrap();

        assert_eq!(
            handled.load(Ordering::Relaxed),
            1,
            "Handler should have been called - events from started() should be handled on foreground"
        );
    }

    // ==================== CASCADING EFFECT TESTS ====================

    #[tokio::test]
    async fn cascading_effect_chain() {
        // Test: Event A triggers Effect A which emits Event B, triggering Effect B
        #[derive(Clone, Debug)]
        struct EventA;
        #[derive(Clone, Debug)]
        struct EventB;
        #[derive(Clone, Debug)]
        struct EventC;

        #[derive(Clone, Default, Debug)]
        struct ChainState {
            a_count: i32,
            b_count: i32,
            c_count: i32,
        }

        let c_counter = Arc::new(AtomicUsize::new(0));
        let c_counter_clone = c_counter.clone();

        let store: Engine<ChainState> = Engine::new()
            .with_reducer(reducer::on::<EventA>().run(|state: ChainState, _| ChainState {
                a_count: state.a_count + 1,
                ..state
            }))
            .with_reducer(reducer::on::<EventB>().run(|state: ChainState, _| ChainState {
                b_count: state.b_count + 1,
                ..state
            }))
            .with_reducer(reducer::on::<EventC>().run(|state: ChainState, _| ChainState {
                c_count: state.c_count + 1,
                ..state
            }))
            .with_effect(effect::on::<EventA>().then(|_event, ctx| async move {
                ctx.emit(EventB);
                Ok(())
            }))
            .with_effect(effect::on::<EventB>().then(|_event, ctx| async move {
                ctx.emit(EventC);
                Ok(())
            }))
            .with_effect(effect::on::<EventC>().then(move |_event, _ctx| {
                let counter = c_counter_clone.clone();
                async move {
                    counter.fetch_add(1, Ordering::Relaxed);
                    Ok(())
                }
            }));

        let handle = store.activate(ChainState::default());
        handle.context.emit(EventA);
        handle.settled().await.unwrap();

        // All three events should have been processed
        assert_eq!(handle.context.curr_state().a_count, 1, "EventA should be reduced");
        assert_eq!(
            handle.context.curr_state().b_count,
            1,
            "EventB should be reduced (cascaded from A)"
        );
        assert_eq!(
            handle.context.curr_state().c_count,
            1,
            "EventC should be reduced (cascaded from B)"
        );
        assert_eq!(
            c_counter.load(Ordering::Relaxed),
            1,
            "EffectC handler should have been called"
        );
    }

    #[tokio::test]
    async fn deep_cascading_chain() {
        // Test: 10-level deep cascade: Event0 -> Event1 -> ... -> Event9
        #[derive(Clone, Default, Debug)]
        struct DeepState {
            depth_reached: i32,
        }

        #[derive(Clone, Debug)]
        struct DepthEvent(i32);

        const MAX_DEPTH: i32 = 10;

        let store: Engine<DeepState> = Engine::new()
            .with_reducer(reducer::on::<DepthEvent>().run(|state: DeepState, event| DeepState {
                depth_reached: state.depth_reached.max(event.0),
            }))
            .with_effect(effect::on::<DepthEvent>().then(move |event, ctx| async move {
                if event.0 < MAX_DEPTH {
                    ctx.emit(DepthEvent(event.0 + 1));
                }
                Ok(())
            }));

        let handle = store.activate(DeepState::default());
        handle.context.emit(DepthEvent(0));
        handle.settled().await.unwrap();

        assert_eq!(
            handle.context.curr_state().depth_reached,
            MAX_DEPTH,
            "Should reach depth {MAX_DEPTH} through cascading"
        );
    }

    #[tokio::test]
    async fn multiple_effects_same_event() {
        // Test: Multiple effects registered for same event type all get called
        #[derive(Clone, Debug)]
        struct SharedEvent;

        let counter = Arc::new(AtomicUsize::new(0));
        let c1 = counter.clone();
        let c2 = counter.clone();
        let c3 = counter.clone();

        let store: Engine<Counter> = Engine::new()
            .with_effect(effect::on::<SharedEvent>().then(move |_event, _ctx| {
                let c = c1.clone();
                async move {
                    c.fetch_add(1, Ordering::Relaxed);
                    Ok(())
                }
            }))
            .with_effect(effect::on::<SharedEvent>().then(move |_event, _ctx| {
                let c = c2.clone();
                async move {
                    c.fetch_add(1, Ordering::Relaxed);
                    Ok(())
                }
            }))
            .with_effect(effect::on::<SharedEvent>().then(move |_event, _ctx| {
                let c = c3.clone();
                async move {
                    c.fetch_add(1, Ordering::Relaxed);
                    Ok(())
                }
            }));

        let handle = store.activate(Counter::default());
        handle.context.emit(SharedEvent);
        handle.settled().await.unwrap();

        assert_eq!(
            counter.load(Ordering::Relaxed),
            3,
            "All 3 effects should have handled the event"
        );
    }

    #[tokio::test]
    async fn effect_observes_state_transition() {
        // Test: Effect can observe prev_state vs next_state
        #[derive(Clone, Debug)]
        struct SetValue(i32);

        let transitions = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let transitions_clone = transitions.clone();

        let store: Engine<Counter> = Engine::new()
            .with_reducer(reducer::on::<SetValue>().run(|_: Counter, event| Counter { value: event.0 }))
            .with_effect(effect::on::<SetValue>().then(move |_event, ctx: EffectContext<Counter, ()>| {
                let t = transitions_clone.clone();
                async move {
                    let prev = ctx.prev_state().value;
                    let next = ctx.next_state().value;
                    t.lock().push((prev, next));
                    Ok(())
                }
            }));

        let handle = store.activate(Counter::default());
        handle.context.emit(SetValue(10));
        handle.context.emit(SetValue(20));
        handle.context.emit(SetValue(30));
        handle.settled().await.unwrap();

        let recorded = transitions.lock().clone();
        assert_eq!(recorded.len(), 3);
        assert_eq!(recorded[0], (0, 10), "First transition: 0 -> 10");
        assert_eq!(recorded[1], (10, 20), "Second transition: 10 -> 20");
        assert_eq!(recorded[2], (20, 30), "Third transition: 20 -> 30");
    }

    #[tokio::test]
    async fn effect_conditionally_emits_based_on_state() {
        // Test: Effect only emits when state crosses a threshold
        #[derive(Clone, Debug)]
        struct AddPoints(i32);
        #[derive(Clone, Debug)]
        struct ThresholdReached;

        #[derive(Clone, Default, Debug)]
        struct PointsState {
            points: i32,
            threshold_events: i32,
        }

        const THRESHOLD: i32 = 100;

        let store: Engine<PointsState> = Engine::new()
            .with_reducer(reducer::on::<AddPoints>().run(|state: PointsState, event| PointsState {
                points: state.points + event.0,
                ..state
            }))
            .with_reducer(reducer::on::<ThresholdReached>().run(|state: PointsState, _| PointsState {
                threshold_events: state.threshold_events + 1,
                ..state
            }))
            .with_effect(effect::on::<AddPoints>().then(move |_event, ctx: EffectContext<PointsState, ()>| async move {
                let prev = ctx.prev_state().points;
                let next = ctx.next_state().points;
                // Only emit if we crossed the threshold
                if prev < THRESHOLD && next >= THRESHOLD {
                    ctx.emit(ThresholdReached);
                }
                Ok(())
            }));

        let handle = store.activate(PointsState::default());
        handle.context.emit(AddPoints(30)); // 30, below threshold
        handle.context.emit(AddPoints(40)); // 70, below threshold
        handle.context.emit(AddPoints(50)); // 120, crosses threshold!
        handle.context.emit(AddPoints(10)); // 130, already above
        handle.settled().await.unwrap();

        assert_eq!(handle.context.curr_state().points, 130);
        assert_eq!(
            handle.context.curr_state().threshold_events,
            1,
            "Threshold should be crossed exactly once"
        );
    }

    #[tokio::test]
    async fn fan_out_single_event_triggers_multiple_cascades() {
        // Test: One event triggers multiple effects that each emit different events
        #[derive(Clone, Debug)]
        struct TriggerEvent;
        #[derive(Clone, Debug)]
        struct BranchA;
        #[derive(Clone, Debug)]
        struct BranchB;
        #[derive(Clone, Debug)]
        struct BranchC;

        #[derive(Clone, Default, Debug)]
        struct FanOutState {
            trigger_count: i32,
            branch_a_count: i32,
            branch_b_count: i32,
            branch_c_count: i32,
        }

        let store: Engine<FanOutState> = Engine::new()
            .with_reducer(reducer::on::<TriggerEvent>().run(|state: FanOutState, _| FanOutState {
                trigger_count: state.trigger_count + 1,
                ..state
            }))
            .with_reducer(reducer::on::<BranchA>().run(|state: FanOutState, _| FanOutState {
                branch_a_count: state.branch_a_count + 1,
                ..state
            }))
            .with_reducer(reducer::on::<BranchB>().run(|state: FanOutState, _| FanOutState {
                branch_b_count: state.branch_b_count + 1,
                ..state
            }))
            .with_reducer(reducer::on::<BranchC>().run(|state: FanOutState, _| FanOutState {
                branch_c_count: state.branch_c_count + 1,
                ..state
            }))
            .with_effect(effect::on::<TriggerEvent>().then(|_, ctx| async move {
                ctx.emit(BranchA);
                Ok(())
            }))
            .with_effect(effect::on::<TriggerEvent>().then(|_, ctx| async move {
                ctx.emit(BranchB);
                Ok(())
            }))
            .with_effect(effect::on::<TriggerEvent>().then(|_, ctx| async move {
                ctx.emit(BranchC);
                Ok(())
            }));

        let handle = store.activate(FanOutState::default());
        handle.context.emit(TriggerEvent);
        handle.settled().await.unwrap();

        assert_eq!(handle.context.curr_state().trigger_count, 1);
        assert_eq!(handle.context.curr_state().branch_a_count, 1, "BranchA should be emitted");
        assert_eq!(handle.context.curr_state().branch_b_count, 1, "BranchB should be emitted");
        assert_eq!(handle.context.curr_state().branch_c_count, 1, "BranchC should be emitted");
    }

    // ==================== STRESS TESTS ====================

    #[tokio::test]
    async fn stress_many_rapid_events() {
        // Test: Rapid emission of many events
        let store: Engine<Counter> = Engine::new()
            .with_reducer(reducer::on::<Increment>().run(|state: Counter, event| Counter {
                value: state.value + event.amount,
            }));

        let handle = store.activate(Counter::default());

        const EVENT_COUNT: i32 = 1000;
        for _ in 0..EVENT_COUNT {
            handle.context.emit(Increment { amount: 1 });
        }

        handle.settled().await.unwrap();
        assert_eq!(handle.context.curr_state().value, EVENT_COUNT);
    }

    #[tokio::test]
    async fn stress_cascading_with_many_events() {
        // Test: Many events each triggering a cascade
        // Note: This test emits many events rapidly, so we need to be mindful of
        // the async nature - settled() waits for all tracked tasks
        #[derive(Clone, Debug)]
        struct Source(i32);
        #[derive(Clone, Debug)]
        struct Derived(i32);

        #[derive(Clone, Default, Debug)]
        struct CascadeState {
            source_sum: i32,
            derived_sum: i32,
        }

        // Use smaller event count to avoid overwhelming the async task queue
        let store: Engine<CascadeState> = Engine::new()
            .with_reducer(reducer::on::<Source>().run(|state: CascadeState, event| CascadeState {
                source_sum: state.source_sum + event.0,
                ..state
            }))
            .with_reducer(reducer::on::<Derived>().run(|state: CascadeState, event| CascadeState {
                derived_sum: state.derived_sum + event.0,
                ..state
            }))
            .with_effect(effect::on::<Source>().then(|event, ctx| async move {
                ctx.emit(Derived(event.0 * 2));
                Ok(())
            }));

        let handle = store.activate(CascadeState::default());

        const EVENT_COUNT: i32 = 20;
        for i in 1..=EVENT_COUNT {
            handle.context.emit(Source(i));
        }

        handle.settled().await.unwrap();

        let expected_source = (EVENT_COUNT * (EVENT_COUNT + 1)) / 2; // sum 1..20 = 210
        let expected_derived = expected_source * 2; // 420

        assert_eq!(handle.context.curr_state().source_sum, expected_source);
        assert_eq!(handle.context.curr_state().derived_sum, expected_derived);
    }

    #[tokio::test]
    async fn effect_error_propagates_to_settled() {
        // Test: Effect returning error propagates to settled()
        #[derive(Clone, Debug)]
        struct FailEvent;

        let store: Engine<Counter> = Engine::new().with_effect(
            effect::on::<FailEvent>()
                .then(|_event, _ctx| async move { Err::<(), _>(anyhow::anyhow!("intentional failure")) }),
        );

        let handle = store.activate(Counter::default());
        handle.context.emit(FailEvent);

        let result = handle.settled().await;
        assert!(result.is_err(), "settled() should return error from effect");
    }

    #[tokio::test]
    async fn effect_curr_state_returns_live_state() {
        // Test: curr_state() returns the actual current state, not a snapshot
        #[derive(Clone, Debug)]
        struct UpdateValue(i32);
        #[derive(Clone, Debug)]
        struct CheckState;

        let observed = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let observed_clone = observed.clone();

        let store: Engine<Counter> = Engine::new()
            .with_reducer(reducer::on::<UpdateValue>().run(|_: Counter, event| Counter { value: event.0 }))
            .with_effect(effect::on::<CheckState>().then(move |_event, ctx: EffectContext<Counter, ()>| {
                let obs = observed_clone.clone();
                async move {
                    // Small delay to let other events process
                    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
                    // curr_state should return the latest value
                    let live = ctx.curr_state();
                    obs.lock().push(live.value);
                    Ok(())
                }
            }));

        let handle = store.activate(Counter::default());
        // Emit check first, then updates - the check should see the updated values
        handle.context.emit(CheckState);
        handle.context.emit(UpdateValue(100));
        handle.context.emit(UpdateValue(200));

        handle.settled().await.unwrap();

        let values = observed.lock().clone();
        assert_eq!(values.len(), 1);
        // curr_state() should see a value >= 100 (at least some updates processed)
        assert!(
            values[0] >= 100,
            "curr_state should see live updates, got {}",
            values[0]
        );
    }

    #[tokio::test]
    async fn started_effect_runs_before_events() {
        // Test: started() effects run before any events are processed
        #[derive(Clone, Debug)]
        struct RegularEvent;

        let order = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let order_started = order.clone();
        let order_handle = order.clone();

        let store: Engine<Counter> = Engine::new().with_effect(
            effect::on::<RegularEvent>()
                .started(move |_ctx| {
                    let o = order_started.clone();
                    async move {
                        o.lock().push("started");
                        Ok(())
                    }
                })
                .then(move |_event, _ctx| {
                    let o = order_handle.clone();
                    async move {
                        o.lock().push("handle");
                        Ok(())
                    }
                }),
        );

        let handle = store.activate(Counter::default());
        handle.context.emit(RegularEvent);
        handle.settled().await.unwrap();

        let recorded = order.lock().clone();
        assert_eq!(recorded, vec!["started", "handle"]);
    }

    #[tokio::test]
    async fn cancel_stops_effect_processing() {
        // Test: cancel() stops effect processing
        #[derive(Clone, Debug)]
        struct LongEvent;

        let started = Arc::new(AtomicUsize::new(0));
        let completed = Arc::new(AtomicUsize::new(0));
        let started_clone = started.clone();
        let completed_clone = completed.clone();

        let store: Engine<Counter> =
            Engine::new().with_effect(effect::on::<LongEvent>().then(
                move |_event, ctx| {
                    let s = started_clone.clone();
                    let c = completed_clone.clone();
                    async move {
                        s.fetch_add(1, Ordering::Relaxed);
                        // Check cancellation in a loop
                        for _ in 0..100 {
                            if ctx.is_cancelled() {
                                return Ok(());
                            }
                            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
                        }
                        c.fetch_add(1, Ordering::Relaxed);
                        Ok(())
                    }
                },
            ));

        let handle = store.activate(Counter::default());
        handle.context.emit(LongEvent);

        // Give it time to start
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Cancel
        handle.cancel();

        // Effect should have started but not completed
        assert!(
            started.load(Ordering::Relaxed) >= 1,
            "Effect should have started"
        );
        assert_eq!(
            completed.load(Ordering::Relaxed),
            0,
            "Effect should not have completed due to cancellation"
        );
    }

    // ==================== EVENT STREAM DOWNCAST TESTS ====================

    #[tokio::test]
    async fn event_stream_receives_downcasted_events() {
        // Test: External subscribers can receive raw events via event_stream
        let store: Engine<Counter> = Engine::new()
            .with_reducer(reducer::on::<Increment>().run(|state: Counter, event| Counter {
                value: state.value + event.amount,
            }));

        let handle = store.activate(Counter::default());

        // Subscribe to raw Increment events on the event_stream
        let mut sub = handle.event_receiver.subscribe::<Increment>();

        // Emit an event
        handle.context.emit(Increment { amount: 42 });

        // Should receive the raw event (not wrapped in EventEnvelope)
        let received = tokio::time::timeout(tokio::time::Duration::from_millis(100), sub.recv())
            .await
            .expect("timeout waiting for event")
            .expect("no event received");

        assert_eq!(received.amount, 42);
    }

    #[tokio::test]
    async fn event_stream_receives_multiple_event_types() {
        // Test: Multiple event types are properly downcasted
        let store: Engine<Counter> = Engine::new()
            .with_reducer(reducer::on::<Increment>().run(|state: Counter, event| Counter {
                value: state.value + event.amount,
            }))
            .with_reducer(reducer::on::<Decrement>().run(|state: Counter, event| Counter {
                value: state.value - event.amount,
            }));

        let handle = store.activate(Counter::default());

        let mut inc_sub = handle.event_receiver.subscribe::<Increment>();
        let mut dec_sub = handle.event_receiver.subscribe::<Decrement>();

        handle.context.emit(Increment { amount: 10 });
        handle.context.emit(Decrement { amount: 3 });
        handle.context.emit(Increment { amount: 5 });

        // Collect Increment events
        let inc1 = tokio::time::timeout(tokio::time::Duration::from_millis(100), inc_sub.recv())
            .await
            .unwrap()
            .unwrap();
        let inc2 = tokio::time::timeout(tokio::time::Duration::from_millis(100), inc_sub.recv())
            .await
            .unwrap()
            .unwrap();

        // Collect Decrement events
        let dec1 = tokio::time::timeout(tokio::time::Duration::from_millis(100), dec_sub.recv())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(inc1.amount, 10);
        assert_eq!(inc2.amount, 5);
        assert_eq!(dec1.amount, 3);
    }

    #[tokio::test]
    async fn event_stream_receives_cascaded_events() {
        // Test: Cascaded events (emitted by effects) are also downcasted
        #[derive(Clone, Debug)]
        struct TriggerEvent;
        #[derive(Clone, Debug)]
        struct CascadedEvent {
            value: i32,
        }

        let store: Engine<Counter> = Engine::new().with_effect(
            effect::on::<TriggerEvent>().then(|_event, ctx| async move {
                ctx.emit(CascadedEvent { value: 123 });
                Ok(())
            }),
        );

        let handle = store.activate(Counter::default());

        let mut trigger_sub = handle.event_receiver.subscribe::<TriggerEvent>();
        let mut cascaded_sub = handle.event_receiver.subscribe::<CascadedEvent>();

        handle.context.emit(TriggerEvent);
        handle.settled().await.unwrap();

        // Should receive both the trigger and the cascaded event
        let trigger =
            tokio::time::timeout(tokio::time::Duration::from_millis(100), trigger_sub.recv())
                .await
                .expect("timeout waiting for trigger event");
        assert!(trigger.is_some(), "should receive TriggerEvent");

        let cascaded =
            tokio::time::timeout(tokio::time::Duration::from_millis(100), cascaded_sub.recv())
                .await
                .expect("timeout waiting for cascaded event")
                .expect("no cascaded event received");

        assert_eq!(cascaded.value, 123);
    }

    #[tokio::test]
    async fn event_stream_multiple_subscribers_same_type() {
        // Test: Multiple subscribers to the same event type all receive events
        let store: Engine<Counter> = Engine::new();

        let handle = store.activate(Counter::default());

        let mut sub1 = handle.event_receiver.subscribe::<Increment>();
        let mut sub2 = handle.event_receiver.subscribe::<Increment>();
        let mut sub3 = handle.event_receiver.subscribe::<Increment>();

        handle.context.emit(Increment { amount: 77 });

        let msg1 = tokio::time::timeout(tokio::time::Duration::from_millis(100), sub1.recv())
            .await
            .unwrap()
            .unwrap();
        let msg2 = tokio::time::timeout(tokio::time::Duration::from_millis(100), sub2.recv())
            .await
            .unwrap()
            .unwrap();
        let msg3 = tokio::time::timeout(tokio::time::Duration::from_millis(100), sub3.recv())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(msg1.amount, 77);
        assert_eq!(msg2.amount, 77);
        assert_eq!(msg3.amount, 77);
    }

    #[tokio::test]
    async fn event_stream_rapid_events() {
        // Test: Rapid event emission works correctly with downcasting
        let store: Engine<Counter> = Engine::new();

        let handle = store.activate(Counter::default());
        let mut sub = handle.event_receiver.subscribe::<Increment>();

        const EVENT_COUNT: i32 = 100;
        for i in 0..EVENT_COUNT {
            handle.context.emit(Increment { amount: i });
        }

        handle.settled().await.unwrap();

        // Collect all events
        let mut received = Vec::new();
        for _ in 0..EVENT_COUNT {
            match tokio::time::timeout(tokio::time::Duration::from_millis(100), sub.recv()).await {
                Ok(Some(msg)) => received.push(msg.amount),
                _ => break,
            }
        }

        assert_eq!(
            received.len(),
            EVENT_COUNT as usize,
            "should receive all {} events",
            EVENT_COUNT
        );

        // Verify order is preserved
        for (i, amount) in received.iter().enumerate() {
            assert_eq!(*amount, i as i32, "event {} should have amount {}", i, i);
        }
    }

    // Disabled: filter() method no longer exists in pipedream 0.2.0
    // #[tokio::test]
    // async fn event_stream_filter_works() {
    //     // Test: Pipedream filter operations work on the event_stream
    //     let store: Engine<Counter> = Engine::new();
    //     let handle = store.activate(Counter::default());
    //     // Filter to only Increment events with amount > 50
    //     let filtered = handle.event_stream.filter::<Increment, _>(|e| e.amount > 50);
    //     let mut sub = filtered.subscribe::<Increment>();
    //     handle.context.emit(Increment { amount: 30 }); // filtered out
    //     handle.context.emit(Increment { amount: 70 }); // passes
    //     handle.context.emit(Increment { amount: 10 }); // filtered out
    //     handle.context.emit(Increment { amount: 90 }); // passes
    //     handle.settled().await.unwrap();
    //     let msg1 = tokio::time::timeout(tokio::time::Duration::from_millis(100), sub.recv())
    //         .await.unwrap().unwrap();
    //     let msg2 = tokio::time::timeout(tokio::time::Duration::from_millis(100), sub.recv())
    //         .await.unwrap().unwrap();
    //     assert_eq!(msg1.amount, 70);
    //     assert_eq!(msg2.amount, 90);
    // }

    // Disabled: map() method no longer exists in pipedream 0.2.0
    // #[tokio::test]
    // async fn event_stream_map_works() {
    //     // Test: Pipedream map operations work on the event_stream
    //     #[derive(Clone, Debug, PartialEq)]
    //     struct IncrementAmount(i32);
    //     let store: Engine<Counter> = Engine::new();
    //     let handle = store.activate(Counter::default());
    //     // Map Increment events to just their amounts
    //     let mapped = handle.event_stream.map::<Increment, IncrementAmount, _>(|e| IncrementAmount(e.amount));
    //     let mut sub = mapped.subscribe::<IncrementAmount>();
    //     handle.context.emit(Increment { amount: 42 });
    //     handle.context.emit(Increment { amount: 99 });
    //     handle.settled().await.unwrap();
    //     let msg1 = tokio::time::timeout(tokio::time::Duration::from_millis(100), sub.recv())
    //         .await.unwrap().unwrap();
    //     let msg2 = tokio::time::timeout(tokio::time::Duration::from_millis(100), sub.recv())
    //         .await.unwrap().unwrap();
    //     assert_eq!(*msg1, IncrementAmount(42));
    //     assert_eq!(*msg2, IncrementAmount(99));
    // }

    // ==================== TOKIO::SPAWN EMIT TESTS ====================

    #[tokio::test]
    async fn emit_from_tokio_spawn_is_received_by_effects() {
        // Test: When an effect spawns a tokio::spawn and emits from within,
        // other effects should still receive the event.
        // This simulates the job scheduler pattern.

        #[derive(Clone, Debug)]
        struct TriggerEvent;
        #[derive(Clone, Debug)]
        struct SpawnedEvent {
            value: i32,
        }

        let received = Arc::new(AtomicUsize::new(0));
        let received_clone = received.clone();

        let store: Engine<Counter> = Engine::new()
            .with_effect(effect::on::<TriggerEvent>().then(|_event, ctx| async move {
                // Spawn a background task (like the scheduler does)
                tokio::spawn(async move {
                    // Small delay to simulate scheduler interval
                    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
                    // Emit from within the spawned task
                    ctx.emit(SpawnedEvent { value: 42 });
                });
                Ok(())
            }))
            .with_effect(effect::on::<SpawnedEvent>().then(move |event, _ctx| {
                let r = received_clone.clone();
                async move {
                    r.fetch_add(event.value as usize, Ordering::Relaxed);
                    Ok(())
                }
            }));

        let handle = store.activate(Counter::default());
        handle.context.emit(TriggerEvent);

        // Wait for the spawned task to emit and the receiver to process
        // Note: We can't use settled() here because the tokio::spawn is not tracked
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        assert_eq!(
            received.load(Ordering::Relaxed),
            42,
            "Receiver effect should have received the event from tokio::spawn"
        );
    }

    #[tokio::test]
    async fn emit_from_tokio_spawn_updates_state() {
        // Test: Events emitted from tokio::spawn should still update state via reducers

        #[derive(Clone, Debug)]
        struct StartScheduler;
        #[derive(Clone, Debug)]
        struct ScheduledIncrement {
            amount: i32,
        }

        let store: Engine<Counter> = Engine::new()
            .with_reducer(reducer::on::<ScheduledIncrement>().run(|state: Counter, event| Counter {
                value: state.value + event.amount,
            }))
            .with_effect(effect::on::<StartScheduler>().then(|_event, ctx| async move {
                // Spawn like the job scheduler does
                tokio::spawn(async move {
                    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
                    ctx.emit(ScheduledIncrement { amount: 100 });
                });
                Ok(())
            }));

        let handle = store.activate(Counter::default());
        handle.context.emit(StartScheduler);

        // Wait for spawned task
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        assert_eq!(
            handle.context.curr_state().value,
            100,
            "State should be updated by event from tokio::spawn"
        );
    }

    #[tokio::test]
    async fn emit_from_tokio_spawn_cascades_to_other_effects() {
        // Test: Events emitted from tokio::spawn should cascade to other effects

        #[derive(Clone, Debug)]
        struct StartEvent;
        #[derive(Clone, Debug)]
        struct MiddleEvent {
            value: i32,
        }
        #[derive(Clone, Debug)]
        struct FinalEvent {
            value: i32,
        }

        let final_received = Arc::new(AtomicUsize::new(0));
        let final_received_clone = final_received.clone();

        let store: Engine<Counter> = Engine::new()
            .with_effect(effect::on::<StartEvent>().then(|_event, ctx| async move {
                tokio::spawn(async move {
                    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
                    ctx.emit(MiddleEvent { value: 10 });
                });
                Ok(())
            }))
            .with_effect(effect::on::<MiddleEvent>().then(|event, ctx| async move {
                // This effect is triggered by the spawned event and emits another
                ctx.emit(FinalEvent {
                    value: event.value * 2,
                });
                Ok(())
            }))
            .with_effect(effect::on::<FinalEvent>().then(move |event, _ctx| {
                let r = final_received_clone.clone();
                async move {
                    r.fetch_add(event.value as usize, Ordering::Relaxed);
                    Ok(())
                }
            }));

        let handle = store.activate(Counter::default());
        handle.context.emit(StartEvent);

        // Wait for cascade
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        assert_eq!(
            final_received.load(Ordering::Relaxed),
            20,
            "Final effect should receive cascaded event (10 * 2 = 20)"
        );
    }

    // =========================================================================
    // ERROR HANDLING TESTS
    // =========================================================================

    #[tokio::test]
    async fn task_panic_converts_to_error() {
        // Test: A panic in a spawned task is caught and converted to an error
        #[derive(Clone, Debug)]
        struct PanicEvent;

        let store: Engine<Counter> = Engine::new().with_effect(
            effect::on::<PanicEvent>().then(|_event, ctx| async move {
                ctx.within(|_| async move {
                    panic!("intentional panic for testing");
                });
                Ok(())
            }),
        );

        let handle = store.activate(Counter::default());
        handle.context.emit(PanicEvent);

        let result = handle.settled().await;
        assert!(result.is_err(), "settled() should return error from panic");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("panic") || err_msg.contains("intentional"),
            "Error should mention panic: {}",
            err_msg
        );
    }

    #[tokio::test]
    async fn multiple_effects_same_event_first_error_wins() {
        // Test: When multiple effects fail for the same event, first error is captured
        #[derive(Clone, Debug)]
        struct MultiFailEvent;

        let order = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let order1 = order.clone();
        let order2 = order.clone();

        let store: Engine<Counter> = Engine::new()
            .with_effect(effect::on::<MultiFailEvent>().then(move |_event, _ctx| {
                let o = order1.clone();
                async move {
                    o.lock().push("first");
                    Err::<(), _>(anyhow::anyhow!("first error"))
                }
            }))
            .with_effect(effect::on::<MultiFailEvent>().then(move |_event, _ctx| {
                let o = order2.clone();
                async move {
                    o.lock().push("second");
                    Err::<(), _>(anyhow::anyhow!("second error"))
                }
            }));

        let handle = store.activate(Counter::default());
        handle.context.emit(MultiFailEvent);

        let result = handle.settled().await;
        assert!(result.is_err(), "settled() should return error");

        // Both effects were called
        let recorded = order.lock().clone();
        assert_eq!(recorded.len(), 2, "Both effects should have run");

        // Only first error is captured (implementation detail, but important behavior)
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("first error") || err_msg.contains("second error"),
            "Error message should be from one of the effects: {}",
            err_msg
        );
    }

    #[tokio::test]
    async fn started_hook_error_propagates() {
        // Test: Error in started() hook propagates to settled()
        #[derive(Clone, Debug)]
        struct AnyEvent;

        let store: Engine<Counter> = Engine::new().with_effect(
            effect::on::<AnyEvent>()
                .started(|_ctx| async move { Err::<(), _>(anyhow::anyhow!("started hook failure")) })
                .then(|_event, _ctx| async move { Ok(()) }),
        );

        let handle = store.activate(Counter::default());
        // Don't even emit any events - started() should still fail

        let result = handle.settled().await;
        assert!(
            result.is_err(),
            "settled() should return error from started() hook"
        );
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("started hook failure"),
            "Error should be from started hook"
        );
    }

    #[tokio::test]
    async fn cascading_effect_error_propagates() {
        // Test: Error in a cascaded effect propagates to settled()
        #[derive(Clone, Debug)]
        struct TriggerEvent;

        #[derive(Clone, Debug)]
        struct CascadedEvent;

        let store: Engine<Counter> = Engine::new()
            .with_effect(effect::on::<TriggerEvent>().then(|_event, ctx| async move {
                ctx.emit(CascadedEvent);
                Ok(())
            }))
            .with_effect(effect::on::<CascadedEvent>().then(|_event, _ctx| async move {
                Err::<(), _>(anyhow::anyhow!("cascade failure"))
            }));

        let handle = store.activate(Counter::default());
        handle.context.emit(TriggerEvent);

        let result = handle.settled().await;
        assert!(
            result.is_err(),
            "settled() should return error from cascaded effect"
        );
        assert!(
            result.unwrap_err().to_string().contains("cascade failure"),
            "Error should be from cascaded effect"
        );
    }

    #[tokio::test]
    async fn spawned_subtask_error_propagates() {
        // Test: Error in ctx.within() subtask propagates to settled()
        #[derive(Clone, Debug)]
        struct SubtaskEvent;

        let store: Engine<Counter> = Engine::new().with_effect(
            effect::on::<SubtaskEvent>().then(|_event, ctx| async move {
                ctx.within(|_| async move { Err::<(), _>(anyhow::anyhow!("subtask failure")) });
                Ok(())
            }),
        );

        let handle = store.activate(Counter::default());
        handle.context.emit(SubtaskEvent);

        let result = handle.settled().await;
        assert!(
            result.is_err(),
            "settled() should return error from subtask"
        );
        assert!(
            result.unwrap_err().to_string().contains("subtask failure"),
            "Error should be from subtask"
        );
    }

    #[tokio::test]
    async fn nested_subtask_error_propagates() {
        // Test: Error in nested ctx.within() calls propagates correctly
        #[derive(Clone, Debug)]
        struct NestedEvent;

        let store: Engine<Counter> = Engine::new().with_effect(
            effect::on::<NestedEvent>().then(|_event, ctx| async move {
                ctx.within(|ctx1| async move {
                    ctx1.within(|ctx2| async move {
                        ctx2.within(|_ctx3| async move {
                            Err::<(), _>(anyhow::anyhow!("deeply nested failure"))
                        });
                        Ok(())
                    });
                    Ok(())
                });
                Ok(())
            }),
        );

        let handle = store.activate(Counter::default());
        handle.context.emit(NestedEvent);

        let result = handle.settled().await;
        assert!(
            result.is_err(),
            "settled() should return error from nested subtask"
        );
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("deeply nested failure"),
            "Error should be from deeply nested subtask"
        );
    }

    // =========================================================================
    // ERROR RECOVERY TESTS
    // =========================================================================

    #[tokio::test]
    async fn transient_task_error_does_not_block_settlement() {
        // Test: Errors in transient tasks don't prevent head tasks from settling
        #[derive(Clone, Debug)]
        struct TransientEvent;

        let head_completed = Arc::new(AtomicUsize::new(0));
        let head_completed_clone = head_completed.clone();

        let store: Engine<Counter> =
            Engine::new().with_effect(effect::on::<TransientEvent>().then(
                move |_event, ctx| {
                    let completed = head_completed_clone.clone();
                    async move {
                        // Spawn a failing task on transient context
                        let transient_ctx = ctx.transient();
                        transient_ctx.within(|_| async move {
                            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                            Err::<(), _>(anyhow::anyhow!("transient task failure"))
                        });

                        // Record that the head effect completed
                        completed.fetch_add(1, Ordering::Relaxed);
                        Ok(())
                    }
                },
            ));

        let handle = store.activate(Counter::default());
        handle.context.emit(TransientEvent);

        // Head should settle successfully even though transient will fail
        let result = handle.settled().await;
        assert!(
            result.is_ok(),
            "settled() should succeed - transient errors don't block: {:?}",
            result
        );
        assert_eq!(
            head_completed.load(Ordering::Relaxed),
            1,
            "Head effect should have completed"
        );
    }

    #[tokio::test]
    async fn error_in_one_event_allows_other_events_to_process() {
        // Test: An error processing one event doesn't prevent other events from being processed
        #[derive(Clone, Debug)]
        struct FailingEventType;

        #[derive(Clone, Debug)]
        struct SuccessEventType;

        let success_count = Arc::new(AtomicUsize::new(0));
        let success_count_clone = success_count.clone();

        let store: Engine<Counter> = Engine::new()
            .with_effect(effect::on::<FailingEventType>().then(|_event, _ctx| async move {
                Err::<(), _>(anyhow::anyhow!("failing event error"))
            }))
            .with_effect(effect::on::<SuccessEventType>().then(move |_event, _ctx| {
                let sc = success_count_clone.clone();
                async move {
                    sc.fetch_add(1, Ordering::Relaxed);
                    Ok(())
                }
            }));

        let handle = store.activate(Counter::default());

        // Emit multiple events - some will fail, some will succeed
        handle.context.emit(SuccessEventType);
        handle.context.emit(FailingEventType);
        handle.context.emit(SuccessEventType);
        handle.context.emit(SuccessEventType);

        let result = handle.settled().await;
        // Should have an error from the failing event
        assert!(result.is_err(), "Should have error from failing event");

        // But success events should still have been processed
        assert!(
            success_count.load(Ordering::Relaxed) >= 1,
            "At least some success events should have been processed"
        );
    }

    #[tokio::test]
    async fn state_updates_continue_despite_effect_error() {
        // Test: Reducers continue to update state even when effects fail
        #[derive(Clone, Debug)]
        struct IncrementWithEffect {
            amount: i32,
        }

        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count_clone = call_count.clone();

        let store: Engine<Counter> = Engine::new()
            .with_reducer(reducer::on::<IncrementWithEffect>().run(|state: Counter, event| Counter {
                value: state.value + event.amount,
            }))
            .with_effect(effect::on::<IncrementWithEffect>().then(move |_event, _ctx| {
                let cc = call_count_clone.clone();
                async move {
                    let count = cc.fetch_add(1, Ordering::Relaxed);
                    if count == 1 {
                        // Fail on the second call
                        Err::<(), _>(anyhow::anyhow!("effect failed on second call"))
                    } else {
                        Ok(())
                    }
                }
            }));

        let handle = store.activate(Counter::default());

        // Emit 3 events - the second one's effect will fail
        handle.context.emit(IncrementWithEffect { amount: 10 });
        handle.context.emit(IncrementWithEffect { amount: 20 });
        handle.context.emit(IncrementWithEffect { amount: 30 });

        let _ = handle.settled().await; // Ignore error

        // State should have been updated by all reducers (60 total)
        // Reducers run synchronously before effects
        assert_eq!(
            handle.context.curr_state().value,
            60,
            "All reducers should have run despite effect failure"
        );
    }

    #[tokio::test]
    async fn cancel_prevents_further_errors() {
        // Test: Cancelling stops task processing, preventing cascading errors
        #[derive(Clone, Debug)]
        struct SlowEvent;

        let started = Arc::new(AtomicUsize::new(0));
        let started_clone = started.clone();

        let store: Engine<Counter> =
            Engine::new().with_effect(effect::on::<SlowEvent>().then(
                move |_event, ctx| {
                    let s = started_clone.clone();
                    async move {
                        s.fetch_add(1, Ordering::Relaxed);

                        // Check cancellation before doing expensive work
                        if ctx.is_cancelled() {
                            return Ok(());
                        }

                        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

                        // This error won't happen if cancelled
                        Err::<(), _>(anyhow::anyhow!("slow failure"))
                    }
                },
            ));

        let handle = store.activate(Counter::default());
        handle.context.emit(SlowEvent);

        // Give it a moment to start
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        // Cancel before the slow effect completes
        handle.cancel();

        // The effect started but was cancelled
        assert!(
            started.load(Ordering::Relaxed) >= 1,
            "Effect should have started"
        );
    }

    #[tokio::test]
    async fn error_preserves_state_snapshot() {
        // Test: prev_state and next_state are still valid even when effect errors
        #[derive(Clone, Debug)]
        struct StatefulFailEvent {
            new_value: i32,
        }

        let observed_prev = Arc::new(parking_lot::Mutex::new(None));
        let observed_next = Arc::new(parking_lot::Mutex::new(None));
        let observed_prev_clone = observed_prev.clone();
        let observed_next_clone = observed_next.clone();

        let store: Engine<Counter> = Engine::new()
            .with_reducer(reducer::on::<StatefulFailEvent>().run(|_state: Counter, event| Counter {
                value: event.new_value,
            }))
            .with_effect(effect::on::<StatefulFailEvent>().then(move |_event, ctx: EffectContext<Counter, ()>| {
                let prev = observed_prev_clone.clone();
                let next = observed_next_clone.clone();
                async move {
                    // Record state before failing
                    *prev.lock() = Some(ctx.prev_state().value);
                    *next.lock() = Some(ctx.next_state().value);

                    Err::<(), _>(anyhow::anyhow!("intentional failure after state check"))
                }
            }));

        let handle = store.activate(Counter { value: 100 });
        handle.context.emit(StatefulFailEvent { new_value: 500 });

        let _ = handle.settled().await; // Ignore error

        assert_eq!(
            *observed_prev.lock(),
            Some(100),
            "prev_state should be captured before error"
        );
        assert_eq!(
            *observed_next.lock(),
            Some(500),
            "next_state should be captured before error"
        );
    }

    #[tokio::test]
    async fn multiple_errors_only_first_propagates() {
        // Test: When multiple independent tasks fail, only the first error propagates
        #[derive(Clone, Debug)]
        struct MultiErrorEvent;

        let store: Engine<Counter> = Engine::new().with_effect(
            effect::on::<MultiErrorEvent>().then(|_event, ctx| async move {
                // Spawn multiple failing tasks
                for i in 0..5 {
                    let delay = i * 10;
                    ctx.within(move |_| async move {
                        tokio::time::sleep(tokio::time::Duration::from_millis(delay)).await;
                        Err::<(), _>(anyhow::anyhow!("error from task {}", i))
                    });
                }
                Ok(())
            }),
        );

        let handle = store.activate(Counter::default());
        handle.context.emit(MultiErrorEvent);

        let result = handle.settled().await;
        assert!(result.is_err(), "Should have an error");

        // Error should be from task 0 (first to fail due to no delay)
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("error from task 0"),
            "First error should win: {}",
            err_msg
        );
    }

    #[tokio::test]
    async fn error_after_successful_cascade_propagates() {
        // Test: A successful cascade followed by a failing effect still propagates the error
        #[derive(Clone, Debug)]
        struct StartCascade;

        #[derive(Clone, Debug)]
        struct MiddleCascade;

        #[derive(Clone, Debug)]
        struct FinalCascade;

        let reached = Arc::new(AtomicUsize::new(0));
        let reached_clone = reached.clone();

        let store: Engine<Counter> = Engine::new()
            .with_effect(effect::on::<StartCascade>().then(|_event, ctx| async move {
                ctx.emit(MiddleCascade);
                Ok(())
            }))
            .with_effect(effect::on::<MiddleCascade>().then(move |_event, ctx| {
                let r = reached_clone.clone();
                async move {
                    r.fetch_add(1, Ordering::Relaxed);
                    ctx.emit(FinalCascade);
                    Ok(())
                }
            }))
            .with_effect(effect::on::<FinalCascade>().then(|_event, _ctx| async move {
                Err::<(), _>(anyhow::anyhow!("final cascade failure"))
            }));

        let handle = store.activate(Counter::default());
        handle.context.emit(StartCascade);

        let result = handle.settled().await;

        // Middle effect should have run successfully
        assert_eq!(
            reached.load(Ordering::Relaxed),
            1,
            "Middle effect should have completed"
        );

        // But final cascade failure should propagate
        assert!(result.is_err(), "Final cascade error should propagate");
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("final cascade failure"),
            "Error should be from final cascade"
        );
    }

    #[tokio::test]
    async fn delayed_error_still_propagates() {
        // Test: An error that occurs after a delay still propagates to settled()
        #[derive(Clone, Debug)]
        struct DelayedErrorEvent;

        let store: Engine<Counter> = Engine::new()
            .with_effect(effect::on::<DelayedErrorEvent>().then(|_event, _ctx| async move {
                tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                Err::<(), _>(anyhow::anyhow!("delayed error"))
            }));

        let handle = store.activate(Counter::default());
        handle.context.emit(DelayedErrorEvent);

        let result = handle.settled().await;
        assert!(result.is_err(), "Delayed error should propagate");
        assert!(
            result.unwrap_err().to_string().contains("delayed error"),
            "Should be the delayed error"
        );
    }

    // =========================================================================
    // RACE CONDITION TESTS
    // =========================================================================

    #[tokio::test]
    async fn concurrent_emits_from_multiple_tasks() {
        // Test: Multiple tokio tasks emitting events concurrently
        // This tests the reducer lock contention and event ordering
        let store: Engine<Counter> = Engine::new()
            .with_reducer(reducer::on::<Increment>().run(|state: Counter, event| Counter {
                value: state.value + event.amount,
            }));

        let handle = store.activate(Counter::default());

        const TASK_COUNT: usize = 10;
        const EVENTS_PER_TASK: i32 = 100;

        let mut handles = Vec::new();
        for _ in 0..TASK_COUNT {
            let ctx = handle.context.clone();
            handles.push(tokio::spawn(async move {
                for _ in 0..EVENTS_PER_TASK {
                    ctx.emit(Increment { amount: 1 });
                }
            }));
        }

        // Wait for all spawn tasks to complete
        for h in handles {
            h.await.unwrap();
        }

        handle.settled().await.unwrap();

        assert_eq!(
            handle.context.curr_state().value,
            (TASK_COUNT as i32) * EVENTS_PER_TASK,
            "All events from concurrent tasks should be processed"
        );
    }

    #[tokio::test]
    async fn concurrent_emits_with_effects() {
        // Test: Concurrent emissions where effects also emit events
        #[derive(Clone, Debug)]
        struct Source(#[allow(dead_code)] i32);
        #[derive(Clone, Debug)]
        struct Derived(#[allow(dead_code)] i32);

        #[derive(Clone, Default, Debug)]
        struct ConcurrentState {
            source_count: i32,
            derived_count: i32,
        }

        let store: Engine<ConcurrentState> = Engine::new()
            .with_reducer(reducer::on::<Source>().run(|state: ConcurrentState, _| ConcurrentState {
                source_count: state.source_count + 1,
                ..state
            }))
            .with_reducer(reducer::on::<Derived>().run(|state: ConcurrentState, _| ConcurrentState {
                derived_count: state.derived_count + 1,
                ..state
            }))
            .with_effect(effect::on::<Source>().then(|event, ctx| async move {
                ctx.emit(Derived(event.0));
                Ok(())
            }));

        let handle = store.activate(ConcurrentState::default());

        const TASK_COUNT: usize = 5;
        const EVENTS_PER_TASK: i32 = 50;

        let mut handles = Vec::new();
        for task_id in 0..TASK_COUNT {
            let ctx = handle.context.clone();
            handles.push(tokio::spawn(async move {
                for i in 0..EVENTS_PER_TASK {
                    ctx.emit(Source((task_id as i32) * 1000 + i));
                }
            }));
        }

        for h in handles {
            h.await.unwrap();
        }

        handle.settled().await.unwrap();

        let expected = (TASK_COUNT as i32) * EVENTS_PER_TASK;
        assert_eq!(handle.context.curr_state().source_count, expected);
        assert_eq!(handle.context.curr_state().derived_count, expected);
    }

    #[tokio::test]
    async fn rapid_emit_with_slow_effect() {
        // Test: Rapid event emission while effects are slow
        // This ensures the event queue handles backpressure correctly
        #[derive(Clone, Debug)]
        struct FastEvent(#[allow(dead_code)] i32);

        let processed = Arc::new(AtomicUsize::new(0));
        let processed_clone = processed.clone();
        let store: Engine<Counter> = Engine::new()
            .with_reducer(reducer::on::<FastEvent>().run(|state: Counter, _| Counter {
                value: state.value + 1,
            }))
            .with_effect(effect::on::<FastEvent>().then(move |_event, _ctx| {
                let p = processed_clone.clone();
                async move {
                    // Simulate slow processing
                    tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;
                    p.fetch_add(1, Ordering::Relaxed);
                    Ok(())
                }
            }));

        let handle = store.activate(Counter::default());

        const EVENT_COUNT: i32 = 100;
        for i in 0..EVENT_COUNT {
            handle.context.emit(FastEvent(i));
        }

        handle.settled().await.unwrap();

        assert_eq!(handle.context.curr_state().value, EVENT_COUNT);
        assert_eq!(processed.load(Ordering::Relaxed), EVENT_COUNT as usize);
    }

    #[tokio::test]
    async fn effect_ordering_under_concurrent_load() {
        // Test: Effects maintain proper state observation under concurrent load
        #[derive(Clone, Debug)]
        struct OrderedEvent(i32);

        let observations = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let observations_clone = observations.clone();
        let store: Engine<Counter> = Engine::new()
            .with_reducer(reducer::on::<OrderedEvent>().run(|state: Counter, event| Counter {
                value: state.value + event.0,
            }))
            .with_effect(effect::on::<OrderedEvent>().then(move |_event, ctx: EffectContext<Counter, ()>| {
                let obs = observations_clone.clone();
                async move {
                    let prev = ctx.prev_state().value;
                    let next = ctx.next_state().value;
                    obs.lock().push((prev, next));
                    Ok(())
                }
            }));

        let handle = store.activate(Counter::default());

        // Emit from multiple tasks
        let mut handles = Vec::new();
        for _ in 0..5 {
            let ctx = handle.context.clone();
            handles.push(tokio::spawn(async move {
                for _ in 0..10 {
                    ctx.emit(OrderedEvent(1));
                }
            }));
        }

        for h in handles {
            h.await.unwrap();
        }

        handle.settled().await.unwrap();

        let obs = observations.lock().clone();

        // Each observation should show state increasing
        for (prev, next) in &obs {
            assert!(
                next > prev,
                "State should always increase: prev={}, next={}",
                prev,
                next
            );
        }

        // Total observations should match event count
        assert_eq!(obs.len(), 50);
    }

    #[tokio::test]
    async fn interleaved_emit_and_cancel() {
        // Test: Emitting while another task is cancelling
        #[derive(Clone, Debug)]
        struct InterleaveEvent;

        let started = Arc::new(AtomicUsize::new(0));
        let started_clone = started.clone();
        let store: Engine<Counter> = Engine::new()
            .with_effect(effect::on::<InterleaveEvent>().then(move |_event, ctx| {
                let s = started_clone.clone();
                async move {
                    s.fetch_add(1, Ordering::Relaxed);
                    // Check cancellation during processing
                    for _ in 0..10 {
                        if ctx.is_cancelled() {
                            return Ok(());
                        }
                        tokio::time::sleep(tokio::time::Duration::from_millis(5)).await;
                    }
                    Ok(())
                }
            }));

        let handle = store.activate(Counter::default());

        // Start emitting events
        let ctx = handle.context.clone();
        let emit_handle = tokio::spawn(async move {
            for _ in 0..20 {
                ctx.emit(InterleaveEvent);
                tokio::time::sleep(tokio::time::Duration::from_millis(2)).await;
            }
        });

        // Cancel after a short delay
        tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;
        handle.cancel();

        // Wait for emit task to finish
        let _ = emit_handle.await;

        // Some effects should have started
        assert!(
            started.load(Ordering::Relaxed) >= 1,
            "At least some effects should have started"
        );
    }

    #[tokio::test]
    async fn multiple_activations_independent() {
        // Test: Multiple activations of the same store are independent
        let store: Engine<Counter> = Engine::new()
            .with_reducer(reducer::on::<Increment>().run(|state: Counter, event| Counter {
                value: state.value + event.amount,
            }));

        let handle1 = store.activate(Counter::default());
        let handle2 = store.activate(Counter::default());

        // Emit to both handles
        handle1.context.emit(Increment { amount: 10 });
        handle2.context.emit(Increment { amount: 100 });

        handle1.settled().await.unwrap();
        handle2.settled().await.unwrap();

        // Each activation has independent state
        assert_eq!(
            handle1.context.curr_state().value,
            10,
            "First activation should have value 10"
        );
        assert_eq!(
            handle2.context.curr_state().value,
            100,
            "Second activation should have value 100 (independent state)"
        );
    }

    // =========================================================================
    // ERROR EDGE CASE TESTS
    // =========================================================================

    #[tokio::test]
    async fn error_in_effect_with_concurrent_success() {
        // Test: One effect errors while others succeed concurrently
        #[derive(Clone, Debug)]
        struct MixedEvent(i32);

        let success_count = Arc::new(AtomicUsize::new(0));
        let success_count_clone = success_count.clone();
        let store: Engine<Counter> = Engine::new()
            .with_effect(effect::on::<MixedEvent>().then(move |event, _ctx| {
                let sc = success_count_clone.clone();
                async move {
                    if event.0 == 5 {
                        Err::<(), _>(anyhow::anyhow!("conditional failure at 5"))
                    } else {
                        sc.fetch_add(1, Ordering::Relaxed);
                        Ok(())
                    }
                }
            }));

        let handle = store.activate(Counter::default());

        // Emit events concurrently
        let mut handles = Vec::new();
        for i in 0..10 {
            let ctx = handle.context.clone();
            handles.push(tokio::spawn(async move {
                ctx.emit(MixedEvent(i));
            }));
        }

        for h in handles {
            h.await.unwrap();
        }

        let result = handle.settled().await;
        assert!(result.is_err(), "Should have error from event 5");

        // Some events should have succeeded
        assert!(
            success_count.load(Ordering::Relaxed) >= 1,
            "At least some events should succeed"
        );
    }

    #[tokio::test]
    async fn error_does_not_prevent_state_reads() {
        // Test: State can still be read after an error
        #[derive(Clone, Debug)]
        struct FailAfterUpdate {
            value: i32,
        }

        let store: Engine<Counter> = Engine::new()
            .with_reducer(reducer::on::<FailAfterUpdate>().run(|state: Counter, event| Counter {
                value: state.value + event.value,
            }))
            .with_effect(effect::on::<FailAfterUpdate>().then(|event, _ctx| async move {
                if event.value > 50 {
                    Err::<(), _>(anyhow::anyhow!("value too high"))
                } else {
                    Ok(())
                }
            }));

        let handle = store.activate(Counter::default());

        handle.context.emit(FailAfterUpdate { value: 30 });
        handle.context.emit(FailAfterUpdate { value: 60 }); // Will fail effect
        handle.context.emit(FailAfterUpdate { value: 10 });

        let _ = handle.settled().await;

        // State should still be accessible and updated by reducers
        assert_eq!(
            handle.context.curr_state().value,
            100,
            "State should reflect all reducer updates"
        );
    }

    #[tokio::test]
    async fn cascading_error_does_not_affect_sibling_cascades() {
        // Test: Error in one cascade branch doesn't affect sibling branches
        #[derive(Clone, Debug)]
        struct TriggerFanOut;
        #[derive(Clone, Debug)]
        struct BranchA;
        #[derive(Clone, Debug)]
        struct BranchB;
        #[derive(Clone, Debug)]
        struct BranchAChild;
        #[derive(Clone, Debug)]
        struct BranchBChild;

        #[derive(Clone, Default, Debug)]
        struct FanOutState {
            branch_a_child: bool,
            branch_b_child: bool,
        }

        let store: Engine<FanOutState> = Engine::new()
            .with_reducer(reducer::on::<BranchAChild>().run(|state, _| FanOutState {
                branch_a_child: true,
                ..state
            }))
            .with_reducer(reducer::on::<BranchBChild>().run(|state, _| FanOutState {
                branch_b_child: true,
                ..state
            }))
            .with_effect(effect::on::<TriggerFanOut>().then(|_, ctx| async move {
                ctx.emit(BranchA);
                ctx.emit(BranchB);
                Ok(())
            }))
            .with_effect(effect::on::<BranchA>().then(|_, ctx| async move {
                ctx.emit(BranchAChild);
                Err::<(), _>(anyhow::anyhow!("branch A fails"))
            }))
            .with_effect(effect::on::<BranchB>().then(|_, ctx| async move {
                ctx.emit(BranchBChild);
                Ok(())
            }));

        let handle = store.activate(FanOutState::default());
        handle.context.emit(TriggerFanOut);

        let result = handle.settled().await;
        assert!(result.is_err(), "Should have error from branch A");

        // Branch B should still have processed
        let state = handle.context.curr_state();
        assert!(
            state.branch_a_child,
            "Branch A child should have been emitted before error"
        );
        assert!(
            state.branch_b_child,
            "Branch B should complete despite branch A error"
        );
    }

    #[tokio::test]
    async fn error_with_empty_message() {
        // Test: Error with empty message is handled gracefully
        #[derive(Clone, Debug)]
        struct EmptyErrorEvent;

        let store: Engine<Counter> = Engine::new()
            .with_effect(effect::on::<EmptyErrorEvent>().then(|_event, _ctx| async move {
                Err::<(), _>(anyhow::anyhow!(""))
            }));

        let handle = store.activate(Counter::default());
        handle.context.emit(EmptyErrorEvent);

        let result = handle.settled().await;
        assert!(result.is_err(), "Should have error");
    }

    #[tokio::test]
    async fn error_chain_preserved() {
        // Test: Error context/chain is preserved through propagation
        #[derive(Clone, Debug)]
        struct ChainedErrorEvent;

        let store: Engine<Counter> = Engine::new()
            .with_effect(effect::on::<ChainedErrorEvent>().then(|_event, _ctx| async move {
                let inner = anyhow::anyhow!("inner error");
                Err::<(), _>(inner.context("outer context"))
            }));

        let handle = store.activate(Counter::default());
        handle.context.emit(ChainedErrorEvent);

        let result = handle.settled().await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        let err_string = format!("{:?}", err);
        assert!(
            err_string.contains("inner error") || err_string.contains("outer context"),
            "Error chain should be preserved: {}",
            err_string
        );
    }

    // =========================================================================
    // PANIC HANDLING TESTS
    // =========================================================================

    #[tokio::test]
    async fn panic_in_effect_handle_captured() {
        // Test: Panic directly in effect handle is captured
        #[derive(Clone, Debug)]
        struct DirectPanicEvent;

        let store: Engine<Counter> = Engine::new()
            .with_effect(effect::on::<DirectPanicEvent>().then(|_event, _ctx| async move {
                panic!("direct panic in effect handle");
                #[allow(unreachable_code)]
                Ok(())
            }));

        let handle = store.activate(Counter::default());
        handle.context.emit(DirectPanicEvent);

        let result = handle.settled().await;
        assert!(result.is_err(), "Panic should be captured as error");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("panic") || err_msg.contains("direct panic"),
            "Error should mention panic: {}",
            err_msg
        );
    }

    #[tokio::test]
    async fn panic_in_nested_within() {
        // Test: Panic in deeply nested ctx.within() calls
        #[derive(Clone, Debug)]
        struct NestedPanicEvent;

        let store: Engine<Counter> = Engine::new()
            .with_effect(effect::on::<NestedPanicEvent>().then(|_event, ctx| async move {
                ctx.within(|ctx1| async move {
                    ctx1.within(|ctx2| async move {
                        ctx2.within(|_ctx3| async move {
                            panic!("panic at level 3");
                        });
                        Ok(())
                    });
                    Ok(())
                });
                Ok(())
            }));

        let handle = store.activate(Counter::default());
        handle.context.emit(NestedPanicEvent);

        let result = handle.settled().await;
        assert!(result.is_err(), "Nested panic should be captured");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("panic") && err_msg.contains("level 3"),
            "Should capture nested panic message: {}",
            err_msg
        );
    }

    #[tokio::test]
    async fn panic_with_non_string_payload() {
        // Test: Panic with non-string payload (like integers)
        #[derive(Clone, Debug)]
        struct NonStringPanicEvent;

        let store: Engine<Counter> = Engine::new()
            .with_effect(effect::on::<NonStringPanicEvent>().then(|_event, ctx| async move {
                ctx.within(|_| async move {
                    std::panic::panic_any(42i32);
                });
                Ok(())
            }));

        let handle = store.activate(Counter::default());
        handle.context.emit(NonStringPanicEvent);

        let result = handle.settled().await;
        assert!(result.is_err(), "Non-string panic should be captured");
        // Should have some error message even for non-string panics
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("panic"),
            "Should identify as panic: {}",
            err_msg
        );
    }

    #[tokio::test]
    async fn panic_does_not_prevent_other_effects() {
        // Test: Panic in one effect doesn't prevent other effects from running
        #[derive(Clone, Debug)]
        struct MultiEffectPanicEvent;

        let ran = Arc::new(AtomicUsize::new(0));
        let ran_clone = ran.clone();
        let store: Engine<Counter> = Engine::new()
            .with_effect(effect::on::<MultiEffectPanicEvent>().then(|_event, ctx| async move {
                ctx.within(|_| async move {
                    panic!("effect panics");
                });
                Ok(())
            }))
            .with_effect(effect::on::<MultiEffectPanicEvent>().then(move |_event, _ctx| {
                let r = ran_clone.clone();
                async move {
                    r.fetch_add(1, Ordering::Relaxed);
                    Ok(())
                }
            }));

        let handle = store.activate(Counter::default());
        handle.context.emit(MultiEffectPanicEvent);

        let result = handle.settled().await;
        assert!(result.is_err(), "Should have panic error");

        // The other effect should have run
        assert_eq!(
            ran.load(Ordering::Relaxed),
            1,
            "Non-panicking effect should still run"
        );
    }

    #[tokio::test]
    async fn panic_in_started_hook() {
        // Test: Panic in started() hook is captured
        #[derive(Clone, Debug)]
        struct AnyEvent;

        let store: Engine<Counter> = Engine::new()
            .with_effect(
                effect::on::<AnyEvent>()
                    .started(|_ctx| async move {
                        panic!("panic in started hook");
                    })
                    .then(|_event, _ctx| async move { Ok(()) }),
            );

        let handle = store.activate(Counter::default());

        let result = handle.settled().await;
        assert!(result.is_err(), "Panic in started should be captured");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("panic") || err_msg.contains("started hook"),
            "Should identify started hook panic: {}",
            err_msg
        );
    }

    #[tokio::test]
    async fn multiple_panics_first_wins() {
        // Test: When multiple tasks panic, only the first is captured
        #[derive(Clone, Debug)]
        struct MultiPanicEvent;

        let store: Engine<Counter> = Engine::new()
            .with_effect(effect::on::<MultiPanicEvent>().then(|_event, ctx| async move {
                // Spawn multiple panicking tasks with different delays
                for i in 0..5 {
                    let delay = i * 20;
                    ctx.within(move |_| async move {
                        tokio::time::sleep(tokio::time::Duration::from_millis(delay)).await;
                        panic!("panic from task {}", i);
                    });
                }
                Ok(())
            }));

        let handle = store.activate(Counter::default());
        handle.context.emit(MultiPanicEvent);

        let result = handle.settled().await;
        assert!(result.is_err(), "Should have panic error");

        let err_msg = result.unwrap_err().to_string();
        // First panic (task 0) should win
        assert!(
            err_msg.contains("task 0"),
            "First panic should win: {}",
            err_msg
        );
    }

    #[tokio::test]
    async fn panic_and_error_first_wins() {
        // Test: When both panic and error occur, first one wins
        #[derive(Clone, Debug)]
        struct PanicAndErrorEvent;

        let store: Engine<Counter> = Engine::new()
            .with_effect(effect::on::<PanicAndErrorEvent>().then(|_event, ctx| async move {
                // Error task - no delay
                ctx.within(|_| async { Err::<(), _>(anyhow::anyhow!("error before panic")) });

                // Panic task - small delay
                ctx.within(|_| async {
                    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                    panic!("panic after error");
                });

                Ok(())
            }));

        let handle = store.activate(Counter::default());
        handle.context.emit(PanicAndErrorEvent);

        let result = handle.settled().await;
        assert!(result.is_err());

        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("error before panic"),
            "Error should win over later panic: {}",
            err_msg
        );
    }

    #[tokio::test]
    async fn transient_panic_does_not_affect_head() {
        // Test: Panic in transient context doesn't affect head settlement
        #[derive(Clone, Debug)]
        struct TransientPanicEvent;

        let head_completed = Arc::new(AtomicUsize::new(0));
        let head_completed_clone = head_completed.clone();
        let store: Engine<Counter> = Engine::new()
            .with_effect(effect::on::<TransientPanicEvent>().then(move |_event, ctx| {
                let completed = head_completed_clone.clone();
                async move {
                    // Spawn panicking task on transient
                    let transient = ctx.transient();
                    transient.within(|_| async move {
                        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                        panic!("transient panic");
                    });

                    // Record head completion
                    completed.fetch_add(1, Ordering::Relaxed);
                    Ok(())
                }
            }));

        let handle = store.activate(Counter::default());
        handle.context.emit(TransientPanicEvent);

        let result = handle.settled().await;
        assert!(
            result.is_ok(),
            "Transient panic should not affect head: {:?}",
            result
        );
        assert_eq!(head_completed.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn state_consistent_after_panic() {
        // Test: State remains consistent after a panic
        #[derive(Clone, Debug)]
        struct IncrementThenPanic {
            amount: i32,
            should_panic: bool,
        }

        let store: Engine<Counter> = Engine::new()
            .with_reducer(reducer::on::<IncrementThenPanic>().run(|state: Counter, event| Counter {
                value: state.value + event.amount,
            }))
            .with_effect(effect::on::<IncrementThenPanic>().then(|event, ctx| async move {
                if event.should_panic {
                    ctx.within(|_| async move {
                        panic!("intentional panic");
                    });
                }
                Ok(())
            }));

        let handle = store.activate(Counter::default());

        handle.context.emit(IncrementThenPanic {
            amount: 10,
            should_panic: false,
        });
        handle.context.emit(IncrementThenPanic {
            amount: 20,
            should_panic: true, // This will panic
        });
        handle.context.emit(IncrementThenPanic {
            amount: 30,
            should_panic: false,
        });

        let _ = handle.settled().await;

        // State should be consistent - reducers run before effects
        assert_eq!(
            handle.context.curr_state().value,
            60,
            "State should be updated by all reducers despite panic"
        );
    }

    // ============================================================================
    // PIPE_TO RAW EVENT TESTS
    // ============================================================================

    #[tokio::test]
    async fn pipe_to_forwards_raw_events_to_external_subscriber() {
        // Test: When an effect emits an event, external subscribers via pipe_to
        // should receive the raw event type (via downcast_envelope), not EventEnvelope.
        //
        // This is critical for integrations like valet where the LLM agent
        // subscribes to InputAudio directly.

        #[derive(Clone, Debug, PartialEq)]
        struct InputAudio(Vec<u8>);

        #[derive(Clone, Debug)]
        struct AudioReceived;

        let store: Engine<Counter> = Engine::new()
            .with_effect(effect::on::<AudioReceived>().then(|_event, ctx| async move {
                // Emit InputAudio when AudioReceived is processed
                ctx.emit(InputAudio(vec![1, 2, 3, 4]));
                Ok(())
            }));

        let handle = store.activate(Counter::default());

        // Subscribe to InputAudio on the handle's event receiver BEFORE emitting
        let mut audio_sub = handle.event_receiver.subscribe::<InputAudio>();

        // Emit AudioReceived which triggers the effect to emit InputAudio
        handle.context.emit(AudioReceived);

        // Wait for the event to propagate
        let received =
            tokio::time::timeout(tokio::time::Duration::from_millis(100), audio_sub.recv()).await;

        assert!(received.is_ok(), "Should receive InputAudio within timeout");
        let audio = received.unwrap();
        assert!(audio.is_some(), "Should receive InputAudio event");
        assert_eq!(
            audio.unwrap().0,
            vec![1, 2, 3, 4],
            "InputAudio should contain correct data"
        );
    }

    #[tokio::test]
    async fn pipe_to_forwards_multiple_event_types() {
        // Test: Multiple different event types emitted by effects should all
        // be available to external subscribers via downcast_envelope.

        #[derive(Clone, Debug, PartialEq)]
        struct TextChunk(String);

        #[derive(Clone, Debug, PartialEq)]
        struct AudioChunk(Vec<u8>);

        #[derive(Clone, Debug)]
        struct Trigger;

        let store: Engine<Counter> = Engine::new()
            .with_effect(effect::on::<Trigger>().then(|_event, ctx| async move {
                ctx.emit(TextChunk("hello".to_string()));
                ctx.emit(AudioChunk(vec![5, 6, 7, 8]));
                Ok(())
            }));

        let handle = store.activate(Counter::default());

        // Create external channel and forward from handle's receiver
        let (external_tx, external_rx) = Relay::channel();
        forward_events(&handle.event_receiver, external_tx.weak());

        // Give forward task time to set up subscription
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        // Subscribe to both event types
        let mut text_sub = external_rx.subscribe::<TextChunk>();
        let mut audio_sub = external_rx.subscribe::<AudioChunk>();

        // Emit trigger
        handle.context.emit(Trigger);

        // Wait for both events
        let text_result =
            tokio::time::timeout(tokio::time::Duration::from_millis(100), text_sub.recv()).await;

        let audio_result =
            tokio::time::timeout(tokio::time::Duration::from_millis(100), audio_sub.recv()).await;

        assert!(
            text_result.is_ok(),
            "Should receive TextChunk within timeout"
        );
        assert!(
            audio_result.is_ok(),
            "Should receive AudioChunk within timeout"
        );

        let text = text_result.unwrap().unwrap();
        let audio = audio_result.unwrap().unwrap();

        assert_eq!(text.0, "hello");
        assert_eq!(audio.0, vec![5, 6, 7, 8]);
    }

    // ============================================================================
    // PIPE_TO + PIPE_FROM FEEDBACK LOOP TESTS
    // ============================================================================

    #[tokio::test]
    async fn pipe_to_and_pipe_from_same_stream_prevents_feedback() {
        // When Handle::pipe_to and pipe_from use the same stream, we should not
        // re-emit events back to the external stream (prevent feedback loops).
        //
        // Flow:
        // 1. Effect emits InputAudio
        // 2. InputAudio wrapped in EventEnvelope, sent to internal stream
        // 3. downcast_envelope extracts InputAudio, sends to external_stream
        // 4. pipe_from receives InputAudio and re-emits it into the store
        // 5. External-origin events are NOT forwarded back out

        #[derive(Clone, Debug, PartialEq)]
        struct InputAudio(Vec<u8>);

        #[derive(Clone, Debug)]
        struct Trigger;

        let store: Engine<Counter> = Engine::new()
            .with_effect(effect::on::<Trigger>().then(|_event, ctx| async move {
                ctx.emit(InputAudio(vec![1, 2, 3]));
                Ok(())
            }));

        let handle = store.activate(Counter::default());

        // The handle already provides bidirectional communication:
        // - handle.event_receiver: receives events from effects (output)
        // - handle.event_sender: sends events to engine (input)
        // No additional forwarding needed!

        let mut audio_sub = handle.event_receiver.subscribe::<InputAudio>();

        // Emit trigger
        handle.context.emit(Trigger);

        // Should receive InputAudio
        let result1 =
            tokio::time::timeout(std::time::Duration::from_millis(100), audio_sub.recv()).await;

        assert!(result1.is_ok(), "Should receive InputAudio");
        let audio = result1.unwrap().unwrap();
        assert_eq!(audio.0, vec![1, 2, 3]);

        // We should not receive the same event again due to feedback loop
        let result2 =
            tokio::time::timeout(std::time::Duration::from_millis(100), audio_sub.recv()).await;

        if let Ok(Some(audio2)) = result2 {
            // This indicates a feedback loop
            panic!(
                "FEEDBACK LOOP: Received duplicate InputAudio {:?}. \
                 Events sent via pipe_to were forwarded back out.",
                audio2.0
            );
        }
    }

    #[tokio::test]
    async fn pipe_from_wraps_events_for_effects() {
        // pipe_from should wrap external events in EventEnvelope so effects receive them.

        #[derive(Clone, Debug, PartialEq)]
        struct ExternalEvent(String);

        // Count how many times the effect receives the event
        let recv_count = Arc::new(AtomicUsize::new(0));
        let rc = recv_count.clone();

        let store: Engine<Counter> = Engine::new()
            .with_effect(effect::on::<ExternalEvent>().then(move |_event, _ctx| {
                let c = rc.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }
            }));

        let handle = store.activate(Counter::default());

        // Give time for forwarding tasks to be ready
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // Use handle's built-in bidirectional interface
        // Send event to engine via handle.event_sender
        handle.event_sender
            .send(ExternalEvent("from external".to_string()))
            .await
            .unwrap();

        // Wait for processing
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let count = recv_count.load(Ordering::SeqCst);
        assert_eq!(
            count, 1,
            "Effect should receive external events routed through pipe_from"
        );
    }

    #[tokio::test]
    async fn pipe_from_routes_external_events_through_reducers_and_effects() {
        // External events should update state and trigger effects, just like internal emits.

        #[derive(Clone, Debug)]
        struct ExternalIncrement {
            amount: i32,
        }

        let count = Arc::new(AtomicUsize::new(0));
        let count_clone = count.clone();
        let store: Engine<Counter> = Engine::new()
            .with_reducer(reducer::on::<ExternalIncrement>().run(|state: Counter, event| Counter {
                value: state.value + event.amount,
            }))
            .with_effect(effect::on::<ExternalIncrement>().then(move |_event, _ctx| {
                let c = count_clone.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }
            }));

        let handle = store.activate(Counter::default());

        // Give time for forwarding tasks to be ready
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // Use handle's built-in bidirectional interface
        handle.event_sender
            .send(ExternalIncrement { amount: 5 })
            .await
            .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        assert_eq!(
            handle.context.curr_state().value,
            5,
            "Reducer should process external event"
        );
        assert_eq!(
            count.load(Ordering::SeqCst),
            1,
            "Effect should receive external event"
        );
    }

    #[tokio::test]
    async fn pipe_from_then_pipe_to_same_stream_does_not_echo_external_messages() {
        // External messages piped into the store should not be echoed back out
        // when using the same stream for pipe_from and pipe_to.

        #[derive(Clone, Debug, PartialEq)]
        struct ExternalPing(u8);

        let store: Engine<Counter> = Engine::new();
        let handle = store.activate(Counter::default());

        // Use handle's built-in bidirectional interface
        let mut sub = handle.event_receiver.subscribe::<ExternalPing>();

        handle.event_sender.send(ExternalPing(1)).await.unwrap();

        let first = tokio::time::timeout(std::time::Duration::from_millis(100), sub.recv())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(first.0, 1);

        let second = tokio::time::timeout(std::time::Duration::from_millis(100), sub.recv()).await;

        if let Ok(Some(ping)) = second {
            panic!(
                "Echo detected: received ExternalPing {:?} twice via bidirectional piping",
                ping.0
            );
        }
    }

    // ============================================================================
    // DOWNCAST_ENVELOPE EDGE CASES
    // ============================================================================

    #[tokio::test]
    async fn downcast_envelope_forwarder_closes_when_source_closes() {
        // When the source stream closes, downcast_envelope forwarder should stop

        let store: Engine<Counter> = Engine::new();

        let (external_tx, external_rx) = Relay::channel();

        let handle = store.activate(Counter::default());
        // Spawn forward as background task
        forward_events(&handle.event_receiver, external_tx.weak());

        let mut sub = external_rx.subscribe::<String>();

        // Drop handle (closes event_stream)
        drop(handle);

        // Wait for closure to propagate
        let result = tokio::time::timeout(std::time::Duration::from_millis(500), sub.recv()).await;

        // Should either timeout or receive None (closed)
        match result {
            Ok(None) => (), // Expected: subscription closed
            Err(_) => (),   // Also acceptable: timeout
            Ok(Some(msg)) => panic!("Unexpected message after handle dropped: {:?}", msg),
        }
    }

    #[tokio::test]
    async fn downcast_envelope_handles_rapid_events() {
        // Test that downcast_envelope can handle rapid event emission

        #[derive(Clone, Debug, PartialEq)]
        struct RapidEvent(i32);

        #[derive(Clone, Debug)]
        struct StartRapid;

        let store: Engine<Counter> = Engine::new()
            .with_effect(effect::on::<StartRapid>().then(|_event, ctx| async move {
                // Emit many events rapidly
                for i in 0..100 {
                    ctx.emit(RapidEvent(i));
                }
                Ok(())
            }));

        let handle = store.activate(Counter::default());

        let (external_tx, external_rx) = Relay::channel_with_size(256);
        // Spawn forward as background task
        forward_events(&handle.event_receiver, external_tx.weak());

        let mut sub = external_rx.subscribe::<RapidEvent>();

        // Trigger rapid emission
        handle.context.emit(StartRapid);

        // Count received events
        let mut count = 0;
        loop {
            match tokio::time::timeout(std::time::Duration::from_millis(100), sub.recv()).await {
                Ok(Some(_)) => count += 1,
                _ => break,
            }
        }

        assert!(
            count >= 90, // Allow for some timing variance
            "Should receive most rapid events, got {}",
            count
        );
    }

    // =========================================================================
    // STATE ACCESS TESTS
    // =========================================================================

    #[tokio::test]
    async fn store_state_updates_between_events() {
        // Test: store.state() returns real-time state updates
        let store: Engine<Counter> = Engine::new()
            .with_reducer(reducer::on::<Increment>().run(|state: Counter, event| Counter {
                value: state.value + event.amount,
            }));

        let handle = store.activate(Counter::default());

        // Emit multiple events
        handle.context.emit(Increment { amount: 10 });
        handle.context.emit(Increment { amount: 20 });

        handle.settled().await.unwrap();

        // handle.context.curr_state() should reflect all updates
        assert_eq!(handle.context.curr_state().value, 30);

        // Emit more
        handle.context.emit(Increment { amount: 5 });
        handle.settled().await.unwrap();

        assert_eq!(handle.context.curr_state().value, 35);
    }

    // ==================== EXTERNAL PIPE TESTS ====================

    #[tokio::test]
    async fn external_pipe_bidirectional() {
        // Test: external.forward(&handle) and handle.forward(&external)
        // Events sent to external should flow through the store (reducer + effects)
        // Events emitted by effects should flow back out to external

        #[derive(Clone, Debug)]
        struct InputEvent {
            value: i32,
        }

        #[derive(Clone, Debug)]
        struct OutputEvent {
            result: i32,
        }

        #[derive(Clone, Default, Debug, PartialEq)]
        struct PipeState {
            total: i32,
        }

        let store: Engine<PipeState> = Engine::new()
            .with_reducer(reducer::on::<InputEvent>().run(|state: PipeState, event| PipeState {
                total: state.total + event.value,
            }))
            .with_effect(effect::on::<InputEvent>().then(|event, ctx| async move {
                // Emit output with doubled value
                ctx.emit(OutputEvent {
                    result: event.value * 2,
                });
                Ok(())
            }));

        let handle = store.activate(PipeState::default());

        // Use handle's built-in bidirectional interface
        // Subscribe to output events from effects
        let mut output_sub = handle.event_receiver.subscribe::<OutputEvent>();

        // Give time for forwarding tasks to be ready
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        // Send input event to engine
        handle.event_sender.send(InputEvent { value: 10 }).await.ok();

        // Give time for processing
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Wait a bit for processing
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // State should be updated by reducer
        assert_eq!(handle.context.curr_state().total, 10);

        // Should receive output event on external
        let output =
            tokio::time::timeout(tokio::time::Duration::from_millis(100), output_sub.recv())
                .await
                .expect("timeout waiting for output")
                .expect("no output received");

        assert_eq!(output.result, 20);
    }

    #[tokio::test]
    async fn external_pipe_no_echo() {
        // Test: Events sent from external should not echo back through the store
        // We verify this by counting how many times an event is received
        // If there's an echo, we'd receive it twice (once from send, once from echo)

        #[derive(Clone, Debug, PartialEq)]
        struct PingEvent {
            id: i32,
        }

        #[derive(Clone, Default, Debug)]
        struct EchoState {
            ping_count: i32,
        }

        let store: Engine<EchoState> = Engine::new()
            .with_reducer(reducer::on::<PingEvent>().run(|state: EchoState, _event| EchoState {
                ping_count: state.ping_count + 1,
            }));

        let handle = store.activate(EchoState::default());

        // Use handle's built-in bidirectional interface
        // Count how many times we receive the ping event
        let receive_count = Arc::new(AtomicUsize::new(0));
        let receive_count_clone = receive_count.clone();

        let mut ping_sub = handle.event_receiver.subscribe::<PingEvent>();
        tokio::spawn(async move {
            while let Some(_) = ping_sub.recv().await {
                receive_count_clone.fetch_add(1, Ordering::Relaxed);
            }
        });

        // Small delay for subscription to be ready
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        // Send ping to engine
        handle.event_sender.send(PingEvent { id: 1 }).await.ok();

        // Wait for processing
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // State should be updated (reducer ran)
        assert_eq!(handle.context.curr_state().ping_count, 1);

        // Should receive exactly once (from send), not twice (would indicate echo)
        assert_eq!(
            receive_count.load(Ordering::Relaxed),
            1,
            "Should receive event exactly once, not echoed back"
        );
    }

    #[tokio::test]
    async fn external_pipe_cleanup_on_drop() {
        // Test: When handle is dropped, external subscriptions should end

        #[derive(Clone, Debug)]
        struct TestEvent;

        #[derive(Clone, Default, Debug)]
        struct DropState;

        let store: Engine<DropState> = Engine::new();

        let (external_tx, external_rx) = Relay::channel();

        {
            let handle = store.activate(DropState::default());
            // Spawn forward calls as background tasks
            forward_events(&external_rx, handle.event_sender.clone());
            forward_events(&handle.event_receiver, external_tx.weak());

            // Send an event to verify connection works
            external_tx.send(TestEvent).await.ok();
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

            // handle dropped here
        }

        // Give time for cleanup
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // External channel is still open (sender still exists)
        // The test verifies cleanup happens without panicking
    }

    #[tokio::test]
    async fn multi_hop_relay_chain_prevents_echo() {
        // Test: Events flowing through 3+ relays should not create echo loops
        // Chain: Relay A → Handle 1 → Relay B → Handle 2 → Relay C
        // Event sent to A should flow through to C, but not echo back

        use std::sync::atomic::{AtomicUsize, Ordering};

        #[derive(Clone, Debug, PartialEq)]
        struct ChainEvent {
            value: i32,
        }

        #[derive(Clone, Default, Debug)]
        struct ChainState {
            count: i32,
        }

        // Create two engines
        let engine1: Engine<ChainState> = Engine::new()
            .with_reducer(reducer::on::<ChainEvent>().run(|state: ChainState, event| {
                ChainState {
                    count: state.count + event.value,
                }
            }));

        let engine2: Engine<ChainState> = Engine::new()
            .with_reducer(reducer::on::<ChainEvent>().run(|state: ChainState, event| {
                ChainState {
                    count: state.count + event.value * 10,
                }
            }));

        // Create relay chain: A → Handle1 → B → Handle2 → C
        let (relay_a_tx, relay_a_rx) = Relay::channel();
        let (relay_b_tx, relay_b_rx) = Relay::channel();
        let (relay_c_tx, relay_c_rx) = Relay::channel();

        let handle1 = engine1.activate(ChainState::default());
        let handle2 = engine2.activate(ChainState::default());

        // Connect A → Handle1 → B
        forward_events(&relay_a_rx, handle1.event_sender.clone());
        forward_events(&handle1.event_receiver, relay_b_tx.weak());

        // Connect B → Handle2 → C
        forward_events(&relay_b_rx, handle2.event_sender.clone());
        forward_events(&handle2.event_receiver, relay_c_tx.weak());

        // Subscribe to relay C to receive final events
        let mut relay_c_sub = relay_c_rx.subscribe::<ChainEvent>();

        // Count how many times we receive events on relay A (to detect echoes)
        let relay_a_count = Arc::new(AtomicUsize::new(0));
        let counter = relay_a_count.clone();
        let mut relay_a_sub = relay_a_rx.subscribe::<ChainEvent>();
        tokio::spawn(async move {
            while relay_a_sub.recv().await.is_some() {
                counter.fetch_add(1, Ordering::SeqCst);
            }
        });

        // Give time for forwarding tasks to be ready
        tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;

        // Send event to relay A
        relay_a_tx.send(ChainEvent { value: 5 }).await.ok();

        // Give time for processing through the chain
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Should receive event on relay C (end of chain)
        let output = tokio::time::timeout(
            tokio::time::Duration::from_millis(100),
            relay_c_sub.recv(),
        )
        .await
        .expect("timeout waiting for output on relay C")
        .expect("no output received on relay C");

        assert_eq!(output.value, 5);

        // Verify state was updated in both handles
        assert_eq!(handle1.context.curr_state().count, 5);
        assert_eq!(handle2.context.curr_state().count, 50);

        // Critical check: Event should only appear once on relay A (the original send)
        // If there's an echo loop, it would appear multiple times
        assert_eq!(
            relay_a_count.load(Ordering::SeqCst),
            1,
            "Event echoed back to relay A - visited chain tracking failed"
        );

        handle1.cancel();
        handle2.cancel();
    }

    #[tokio::test]
    async fn visited_chain_tracks_multiple_relay_ids() {
        // Test: Verify that the visited HashSet correctly tracks multiple relay IDs
        // This is a unit-style test for the visited chain mechanism

        #[derive(Clone, Debug)]
        struct TestEvent {
            value: i32,
        }

        #[derive(Clone, Default, Debug)]
        struct TestState;

        let engine: Engine<TestState> = Engine::new();
        let handle = engine.activate(TestState::default());

        // Create multiple external relays
        let (relay1_tx, relay1_rx) = Relay::channel();
        let (relay2_tx, relay2_rx) = Relay::channel();
        let (_relay3_tx, relay3_rx) = Relay::channel();

        let relay1_id = relay1_rx.id();
        let relay2_id = relay2_rx.id();
        let relay3_id = relay3_rx.id();

        // Verify all IDs are unique
        assert_ne!(relay1_id, relay2_id);
        assert_ne!(relay2_id, relay3_id);
        assert_ne!(relay1_id, relay3_id);

        // Forward relay1 → handle → relay2
        forward_events(&relay1_rx, handle.event_sender.clone());
        forward_events(&handle.event_receiver, relay2_tx.weak());

        // Subscribe to relay2
        let mut relay2_sub = relay2_rx.subscribe::<TestEvent>();

        // Give time for forwarding tasks
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        // Send event from relay1
        relay1_tx.send(TestEvent { value: 42 }).await.ok();

        // Should receive on relay2
        let output = tokio::time::timeout(
            tokio::time::Duration::from_millis(50),
            relay2_sub.recv(),
        )
        .await
        .expect("timeout waiting for event on relay2")
        .expect("no event received on relay2");

        assert_eq!(output.value, 42);

        // Now connect relay2 back to relay1 (creating potential loop)
        forward_events(&relay2_rx, relay1_tx.weak());

        // Count events on relay1 to detect if the event echoes back
        let mut relay1_sub = relay1_rx.subscribe::<TestEvent>();
        let relay1_count = Arc::new(AtomicUsize::new(0));
        let counter = relay1_count.clone();

        tokio::spawn(async move {
            while relay1_sub.recv().await.is_some() {
                counter.fetch_add(1, Ordering::SeqCst);
            }
        });

        // Wait to see if event echoes back
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Should not echo back to relay1 because relay1_id is in visited chain
        assert_eq!(
            relay1_count.load(Ordering::SeqCst),
            0,
            "Event echoed back to origin relay - visited chain failed"
        );

        handle.cancel();
    }
}

// NOTE: Tests below use the old API and need updating
// The core implementation above is complete and functional
// Tests can be updated incrementally as needed
