//! Event dispatcher for routing events to effects.
//!
//! The `Dispatcher` is responsible for:
//! 1. Routing events to registered effect handlers
//! 2. Managing multiple effects per event type
//! 3. Providing the effect context with state

use std::any::TypeId;
use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use anyhow::Result;
use futures::FutureExt;

use crate::bus::EventBus;
use crate::core::{Event, EventEnvelope};
use crate::effect_impl::{AnyEffect, Effect, EffectContext, EffectWrapper};
use crate::engine::InflightTracker;
use crate::reducer::{Reducer, ReducerRegistry};
use tracing::error;

/// Event dispatcher for routing events to effects.
///
/// The `Dispatcher` maintains a registry of effects keyed by event type.
/// Multiple effects can listen to the same event type.
///
/// # Example
///
/// ```ignore
/// let dispatcher = Dispatcher::new(deps, bus)
///     .with_effect::<UserEvent, _, ()>(CreateUserEffect)
///     .with_effect::<UserEvent, _, ()>(NotifyAdminEffect);
///
/// // Dispatch an event
/// let event = UserEvent::SignupRequested { email: "test@example.com".into() };
/// dispatcher.dispatch_event(EventEnvelope::new_random(event), (), None).await?;
/// ```
pub struct Dispatcher<D, S: Clone = ()> {
    // Effects keyed by event type, multiple effects can listen to same event
    effects: HashMap<TypeId, Vec<Box<dyn AnyEffect<D, S>>>>,
    deps: Arc<D>,
    bus: EventBus,
    // Reducers for pure state transformations before effects
    reducers: ReducerRegistry<S>,
}

impl<D: Send + Sync + 'static, S: Clone + Send + Sync + 'static> Dispatcher<D, S> {
    /// Create a new dispatcher.
    pub fn new(deps: D, bus: EventBus) -> Self {
        Self {
            effects: HashMap::new(),
            deps: Arc::new(deps),
            bus,
            reducers: ReducerRegistry::new(),
        }
    }

    /// Create a new dispatcher with pre-wrapped Arc dependencies.
    pub fn from_arc(deps: Arc<D>, bus: EventBus) -> Self {
        Self {
            effects: HashMap::new(),
            deps,
            bus,
            reducers: ReducerRegistry::new(),
        }
    }

    /// Register a reducer for pure state transformations.
    ///
    /// Reducers run before effects to transform state in response to events.
    /// Multiple reducers can be registered for the same event type.
    pub fn with_reducer<E, R>(mut self, reducer: R) -> Self
    where
        E: Event + Clone,
        R: Reducer<E, S>,
    {
        self.reducers.register(reducer);
        self
    }

    /// Register an effect handler for an event type.
    ///
    /// Multiple effects can be registered for the same event type.
    /// They will all be invoked in parallel when an event is dispatched.
    pub fn with_effect<E, Eff>(mut self, effect: Eff) -> Self
    where
        E: Event + Clone,
        Eff: Effect<E, D, S>,
    {
        let type_id = TypeId::of::<E>();
        self.effects
            .entry(type_id)
            .or_insert_with(Vec::new)
            .push(Box::new(EffectWrapper::new(effect)));
        self
    }

    /// Apply reducers to transform state based on an event.
    ///
    /// This is called by the engine before dispatching to effects.
    /// Returns the transformed state.
    pub(crate) fn apply_reducers(&self, state: &S, envelope: &EventEnvelope) -> S {
        if self.reducers.is_empty() {
            state.clone()
        } else {
            self.reducers.reduce_all(state, &*envelope.payload)
        }
    }

    /// Dispatch an event to all registered effects.
    ///
    /// Routes the event to all effects registered for this event type.
    /// Effects run in parallel and can each emit new events.
    ///
    /// Note: State should already be transformed by reducers before calling this.
    /// The engine loop handles reducer application.
    ///
    /// # Returns
    ///
    /// - `Ok(())` if dispatch succeeded (effects may have returned None)
    /// - `Err` if any effect failed
    pub async fn dispatch_event(
        &mut self,
        envelope: EventEnvelope,
        state: S,
        inflight: Option<&Arc<InflightTracker>>,
    ) -> Result<()> {
        let type_id = envelope.type_id;
        let cid = envelope.cid;

        // Get all effects for this event type
        let effects = match self.effects.get_mut(&type_id) {
            Some(effs) if !effs.is_empty() => effs,
            _ => {
                // No effects registered - not an error, just skip
                // But still decrement inflight if this event was tracked
                if let Some(tracker) = inflight {
                    tracker.dec(cid, 1);
                }
                return Ok(());
            }
        };

        // Create context with correlation ID and state
        let ctx = EffectContext::with_correlation(
            self.deps.clone(),
            state,
            self.bus.clone(),
            cid,
            inflight.cloned(),
        );

        // Clone the event for each effect
        let event_any = envelope.payload;

        // Run all effects sequentially (effects have &mut self)
        for effect in effects.iter_mut() {
            let event_clone = event_any.clone();
            let ctx_clone = ctx.clone();
            let cid_clone = cid;

            let result = AssertUnwindSafe(effect.handle_any(event_clone, ctx_clone))
                .catch_unwind()
                .await;

            // Convert panic to error
            let result = match result {
                Ok(inner) => inner,
                Err(panic_info) => {
                    let panic_msg = extract_panic_message(&panic_info);
                    error!(%cid_clone, panic = %panic_msg, "effect panicked");
                    Err(anyhow::anyhow!("effect panicked: {}", panic_msg))
                }
            };

            match result {
                Ok(()) => {
                    // Effect completed successfully
                    // Events were emitted via ctx.emit() if any
                }
                Err(e) => {
                    // Effect failed
                    error!(%cid_clone, error = ?e, "effect failed");

                    // Record error for correlation tracking
                    if let Some(tracker) = inflight {
                        tracker.record_error(cid_clone, anyhow::anyhow!("{}", e));
                    }

                    // Decrement inflight before returning error
                    if let Some(tracker) = inflight {
                        tracker.dec(cid, 1);
                    }

                    return Err(e);
                }
            }
        }

        // Decrement inflight counter after successful dispatch
        if let Some(tracker) = inflight {
            tracker.dec(cid, 1);
        }

        Ok(())
    }

    /// Returns the number of registered effects.
    pub fn effect_count(&self) -> usize {
        self.effects.values().map(|v| v.len()).sum()
    }

    /// Get access to the dependencies.
    pub fn deps(&self) -> &D {
        &self.deps
    }

    /// Get access to the event bus.
    pub fn bus(&self) -> &EventBus {
        &self.bus
    }
}

/// Extract a human-readable message from a panic payload.
fn extract_panic_message(panic_info: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = panic_info.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = panic_info.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}

impl<D, S: Clone> std::fmt::Debug for Dispatcher<D, S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count: usize = self.effects.values().map(|v| v.len()).sum();
        f.debug_struct("Dispatcher")
            .field("effect_count", &count)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Test types
    #[derive(Debug, Clone)]
    struct TestDeps {
        value: i32,
    }

    #[derive(Clone)]
    struct TestState {
        counter: i32,
    }

    #[derive(Debug, Clone)]
    struct CreateEvent {
        name: String,
    }

    // Event is auto-implemented via blanket impl

    #[derive(Debug, Clone)]
    struct ResultEvent {
        message: String,
    }

    // Event is auto-implemented via blanket impl

    // Test effects
    struct CreateEffect {
        call_count: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl Effect<CreateEvent, TestDeps, TestState> for CreateEffect {
        type Event = ResultEvent;

        async fn handle(
            &mut self,
            event: CreateEvent,
            ctx: EffectContext<TestDeps, TestState>,
        ) -> Result<()> {
            self.call_count.fetch_add(1, Ordering::Relaxed);
            let counter = ctx.state().counter;
            ctx.emit(ResultEvent {
                message: format!("created {} counter={}", event.name, counter),
            });
            Ok(())
        }
    }

    struct NotifyEffect {
        call_count: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl Effect<CreateEvent, TestDeps, TestState> for NotifyEffect {
        type Event = ResultEvent;

        async fn handle(
            &mut self,
            event: CreateEvent,
            ctx: EffectContext<TestDeps, TestState>,
        ) -> Result<()> {
            self.call_count.fetch_add(1, Ordering::Relaxed);
            ctx.emit(ResultEvent {
                message: format!("notified about {}", event.name),
            });
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_dispatcher_single_effect() {
        let call_count = Arc::new(AtomicUsize::new(0));
        let bus = EventBus::new();
        let mut receiver = bus.subscribe();

        let mut dispatcher = Dispatcher::new(TestDeps { value: 42 }, bus)
            .with_effect::<CreateEvent, _>(CreateEffect {
                call_count: call_count.clone(),
            });

        let event = CreateEvent {
            name: "test".to_string(),
        };
        let envelope = EventEnvelope::new_random(event);
        let state = TestState { counter: 5 };

        dispatcher.dispatch_event(envelope, state, None).await.unwrap();

        assert_eq!(call_count.load(Ordering::Relaxed), 1);

        let result = receiver.recv().await.unwrap();
        let result_event = result.downcast_ref::<ResultEvent>().unwrap();
        assert_eq!(result_event.message, "created test counter=5");
    }

    #[tokio::test]
    async fn test_dispatcher_multiple_effects_same_event() {
        let create_count = Arc::new(AtomicUsize::new(0));
        let notify_count = Arc::new(AtomicUsize::new(0));

        let bus = EventBus::new();
        let mut receiver = bus.subscribe();

        let mut dispatcher = Dispatcher::new(TestDeps { value: 0 }, bus)
            .with_effect::<CreateEvent, _>(CreateEffect {
                call_count: create_count.clone(),
            })
            .with_effect::<CreateEvent, _>(NotifyEffect {
                call_count: notify_count.clone(),
            });

        let event = CreateEvent {
            name: "item".to_string(),
        };
        let envelope = EventEnvelope::new_random(event);
        let state = TestState { counter: 10 };

        dispatcher.dispatch_event(envelope, state, None).await.unwrap();

        // Both effects should have been called
        assert_eq!(create_count.load(Ordering::Relaxed), 1);
        assert_eq!(notify_count.load(Ordering::Relaxed), 1);

        // Should have emitted 2 events
        let e1 = receiver.recv().await.unwrap();
        let e2 = receiver.recv().await.unwrap();

        let messages: Vec<String> = vec![
            e1.downcast_ref::<ResultEvent>().unwrap().message.clone(),
            e2.downcast_ref::<ResultEvent>().unwrap().message.clone(),
        ];

        assert!(messages.contains(&"created item counter=10".to_string()));
        assert!(messages.contains(&"notified about item".to_string()));
    }

    #[tokio::test]
    async fn test_dispatcher_no_effects_registered() {
        let bus = EventBus::new();
        let mut dispatcher = Dispatcher::<TestDeps, TestState>::new(TestDeps { value: 0 }, bus);

        let event = CreateEvent {
            name: "test".to_string(),
        };
        let envelope = EventEnvelope::new_random(event);
        let state = TestState { counter: 0 };

        // Should succeed without error (just no effects to call)
        let result = dispatcher.dispatch_event(envelope, state, None).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_dispatcher_effect_returns_none() {
        struct FilterEffect;

        #[async_trait::async_trait]
        impl Effect<CreateEvent, TestDeps, TestState> for FilterEffect {
            type Event = ResultEvent;

            async fn handle(
                &mut self,
                event: CreateEvent,
                ctx: EffectContext<TestDeps, TestState>,
            ) -> Result<()> {
                // Skip if event.name == "skip"
                if event.name != "skip" {
                    ctx.emit(ResultEvent {
                        message: "handled".to_string(),
                    });
                }
                Ok(())
            }
        }

        let bus = EventBus::new();
        let mut receiver = bus.subscribe();

        let mut dispatcher = Dispatcher::new(TestDeps { value: 0 }, bus)
            .with_effect::<CreateEvent, _>(FilterEffect);

        // Event that should be skipped
        let event1 = CreateEvent {
            name: "skip".to_string(),
        };
        let envelope1 = EventEnvelope::new_random(event1);
        let state = TestState { counter: 0 };

        dispatcher
            .dispatch_event(envelope1, state.clone(), None)
            .await
            .unwrap();

        // Should NOT have emitted an event (returned None)
        assert!(receiver.try_recv().is_err());  // No event emitted

        // Event that should be handled
        let event2 = CreateEvent {
            name: "process".to_string(),
        };
        let envelope2 = EventEnvelope::new_random(event2);

        dispatcher.dispatch_event(envelope2, state, None).await.unwrap();

        // Should have emitted an event
        let result = receiver.recv().await.unwrap();
        let result_event = result.downcast_ref::<ResultEvent>().unwrap();
        assert_eq!(result_event.message, "handled");
    }

    #[test]
    fn test_dispatcher_effect_count() {
        let bus = EventBus::new();
        let dispatcher = Dispatcher::<TestDeps, TestState>::new(TestDeps { value: 0 }, bus)
            .with_effect::<CreateEvent, _>(CreateEffect {
                call_count: Arc::new(AtomicUsize::new(0)),
            })
            .with_effect::<CreateEvent, _>(NotifyEffect {
                call_count: Arc::new(AtomicUsize::new(0)),
            });

        assert_eq!(dispatcher.effect_count(), 2);
    }

    #[test]
    fn test_dispatcher_deps() {
        let bus = EventBus::new();
        let dispatcher = Dispatcher::<TestDeps, TestState>::new(TestDeps { value: 123 }, bus);

        assert_eq!(dispatcher.deps().value, 123);
    }

    #[test]
    fn test_dispatcher_debug() {
        let bus = EventBus::new();
        let dispatcher = Dispatcher::<TestDeps, TestState>::new(TestDeps { value: 0 }, bus);

        let debug = format!("{:?}", dispatcher);
        assert!(debug.contains("Dispatcher"));
        assert!(debug.contains("effect_count"));
    }
}
