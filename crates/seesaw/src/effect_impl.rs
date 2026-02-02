//! Effect trait and context for event handling.
//!
//! Effects are event handlers that execute IO and emit new events.
//!
//! # Key Properties
//!
//! - **Event → Effect → Event**: Simple, direct flow
//! - **Can be stateful or stateless**: Your choice with `&mut self`
//! - **Emit events**: Effects emit via `ctx.emit()` (not returned)
//! - **State flow**: Effects receive per-execution state via `EffectContext<D, S>`

use std::any::Any;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use crate::bus::EventBus;
use crate::core::{CorrelationId, Event};
use crate::engine::InflightTracker;

/// Context passed to effect handlers.
///
/// # State Flow
///
/// `EffectContext<D, S>` carries per-execution state that flows through the system:
/// - Initial state comes from `engine.run(edge, state)`
/// - Reducers transform state before effects
/// - Effects access state via `ctx.state()`
///
/// # Immutability Invariant (CRITICAL)
///
/// `EffectContext` is **immutable and cheap to clone**.
/// Clones MUST be semantically identical.
///
/// This invariant is critical for batch operations where `ctx.clone()` is used:
/// ```ignore
/// for event in events {
///     results.push(self.handle(event, ctx.clone()).await?);
/// }
/// ```
///
/// **DO NOT add to EffectContext:**
/// - Counters or timers
/// - "First event" flags
/// - Retry metadata
/// - Any per-event mutable state
///
/// Such additions would silently break batch semantics.
///
/// # Narrow API
///
/// `EffectContext` is intentionally narrow to prevent effects from
/// accumulating too much power. Effects should only:
/// 1. Access dependencies via `deps()`
/// 2. Access state via `state()`
/// 3. Emit events via `emit()`
///
/// Effects do NOT have access to:
/// - The raw EventBus (removed)
/// - Spawning their own emitters
///
/// # Correlation Propagation
///
/// When an effect emits an event via `ctx.emit()`, the correlation ID from
/// the original event is automatically propagated. This enables `emit_and_await`
/// to track all cascading work.
///
/// # Example
///
/// ```ignore
/// async fn handle(&mut self, event: UserEvent, ctx: EffectContext<MyDeps, RequestState>) -> Result<()> {
///     match event {
///         UserEvent::SignupRequested { email, name } => {
///             // Access state
///             let visitor_id = ctx.state().visitor_id;
///
///             // Access dependencies
///             let user = ctx.deps().db.create_user(&email, &name, visitor_id).await?;
///
///             // Emit event (with correlation ID)
///             ctx.emit(UserEvent::SignedUp { user_id: user.id, email });
///             Ok(())
///         }
///         _ => Ok(())
///     }
/// }
/// ```
pub struct EffectContext<D, S = ()> {
    deps: Arc<D>,
    state: S,
    bus: EventBus,
    /// Correlation ID to propagate on emit (None for fire-and-forget)
    cid: Option<CorrelationId>,
    /// Inflight tracker for increment-before-emit
    inflight: Option<Arc<InflightTracker>>,
}

impl<D, S> EffectContext<D, S> {
    /// Create a new effect context without correlation (fire-and-forget).
    ///
    /// Use this for edge functions and other contexts outside the seesaw
    /// dispatch loop where correlation tracking is not needed.
    pub fn new(deps: Arc<D>, state: S, bus: EventBus) -> Self {
        Self {
            deps,
            state,
            bus,
            cid: None,
            inflight: None,
        }
    }

    /// Create a new effect context with correlation tracking.
    ///
    /// Events emitted will carry the same correlation ID, enabling
    /// `emit_and_await` to track cascading work.
    pub(crate) fn with_correlation(
        deps: Arc<D>,
        state: S,
        bus: EventBus,
        cid: CorrelationId,
        inflight: Option<Arc<InflightTracker>>,
    ) -> Self {
        Self {
            deps,
            state,
            bus,
            cid: Some(cid),
            inflight,
        }
    }

    /// Get shared dependencies.
    ///
    /// Dependencies typically include:
    /// - Database connection pool
    /// - External API clients
    /// - Configuration
    pub fn deps(&self) -> &D {
        &self.deps
    }

    /// Get the per-execution state.
    ///
    /// State flows through the system:
    /// - Initial state from `engine.run(edge, state)`
    /// - Transformed by reducers
    /// - Accessible to effects
    pub fn state(&self) -> &S {
        &self.state
    }

    /// Get the correlation ID for outbox writes.
    ///
    /// Returns the `CorrelationId` suitable for use with `OutboxWriter::write_event`.
    /// If no correlation ID is set, returns `CorrelationId::NONE`.
    ///
    /// # Example
    ///
    /// ```ignore
    /// async fn execute(&self, cmd: CreateCmd, ctx: EffectContext<Deps>) -> Result<MyEvent> {
    ///     let mut tx = ctx.deps().db.begin().await?;
    ///
    ///     // Business write
    ///     let entity = Entity::create(&cmd, &mut tx).await?;
    ///
    ///     // Outbox write with correlation
    ///     let mut writer = PgOutboxWriter::new(&mut tx);
    ///     writer.write_event(&EntityCreated { id: entity.id }, ctx.outbox_correlation_id()).await?;
    ///
    ///     tx.commit().await?;
    ///     Ok(MyEvent::EntityCreated { id: entity.id })
    /// }
    /// ```
    pub fn outbox_correlation_id(&self) -> CorrelationId {
        self.cid.unwrap_or(CorrelationId::NONE)
    }

    /// Get the correlation ID.
    ///
    /// Returns the correlation ID for this effect execution.
    /// If no correlation ID is set, returns `CorrelationId::NONE`.
    pub fn correlation_id(&self) -> CorrelationId {
        self.cid.unwrap_or(CorrelationId::NONE)
    }

    /// Emit an event with correlation tracking.
    ///
    /// Events are emitted with the same correlation ID as the triggering event,
    /// enabling `emit_and_await` to track cascading work.
    ///
    /// # Example
    ///
    /// ```ignore
    /// async fn handle(&mut self, event: UserEvent, ctx: EffectContext<Deps, State>) -> Result<()> {
    ///     match event {
    ///         UserEvent::SignupRequested { email, name } => {
    ///             let user = ctx.deps().db.create_user(&email, &name).await?;
    ///             ctx.emit(UserEvent::SignedUp { user_id: user.id, email });
    ///             Ok(())
    ///         }
    ///         _ => Ok(())
    ///     }
    /// }
    /// ```
    pub fn emit<E: Event>(&self, event: E) {
        if let Some(cid) = self.cid {
            // Increment inflight before emitting (for emit_and_await tracking)
            if let Some(tracker) = &self.inflight {
                tracker.inc(cid, 1);
            }
            self.bus.emit_with_correlation(event, cid);
        } else {
            // No correlation - fire-and-forget
            self.bus.emit(event);
        }
    }
}


impl<D, S: Clone> Clone for EffectContext<D, S> {
    fn clone(&self) -> Self {
        Self {
            deps: self.deps.clone(),
            state: self.state.clone(),
            bus: self.bus.clone(),
            cid: self.cid,
            inflight: self.inflight.clone(),
        }
    }
}

impl<D, S> std::fmt::Debug for EffectContext<D, S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EffectContext").finish_non_exhaustive()
    }
}

/// Effect handler for reacting to events.
///
/// Effects are the reactive layer of seesaw. They:
/// 1. Receive events (signals about what happened)
/// 2. Execute database transactions, API calls, make decisions, etc.
/// 3. Emit new events via `ctx.emit()`
///
/// # Key Principles
///
/// - Effects have `&mut self` and can maintain state across invocations
/// - Effects return `Result<()>`. Emit events via `ctx.emit()`.
/// - Effects can be pure or impure, stateful or stateless - your choice.
/// - Effects can make decisions, branch on conditions, hold state.
/// - Effects access per-execution state via `ctx.state()`
///
/// # Transaction Boundaries
///
/// Each effect execution should use exactly one database transaction.
/// If multiple writes must be atomic, do them in the same transaction
/// within the effect.
///
/// # Example
///
/// ```ignore
/// struct SignupEffect;
///
/// #[async_trait]
/// impl Effect<UserEvent, MyDeps, RequestState> for SignupEffect {
///     type Event = UserEvent;
///
///     async fn handle(&mut self, event: UserEvent, ctx: EffectContext<MyDeps, RequestState>) -> Result<()> {
///         match event {
///             UserEvent::SignupRequested { email, name } => {
///                 let visitor_id = ctx.state().visitor_id;
///                 let user = ctx.deps().db.create_user(&email, &name, visitor_id).await?;
///                 ctx.emit(UserEvent::SignedUp { user_id: user.id, email });
///                 Ok(())
///             }
///             _ => Ok(())
///         }
///     }
/// }
/// ```
#[async_trait]
pub trait Effect<E: Event + Clone, D, S = ()>: Send + Sync + 'static
where
    S: Send,
{
    /// The event type this effect produces.
    type Event: Event;

    /// Handle an event and emit new events via `ctx.emit()`.
    ///
    /// Effects emit events via `ctx.emit()` instead of returning them.
    /// This allows effects to emit multiple events or skip events cleanly.
    ///
    /// # Errors
    ///
    /// Return `Err` if the event handling fails. Errors will propagate
    /// and stop the event flow.
    async fn handle(&mut self, event: E, ctx: EffectContext<D, S>) -> Result<()>;

    /// Handle multiple events of the same type.
    ///
    /// The default implementation calls `handle` sequentially with early return on error.
    ///
    /// Override this method to optimize batch operations:
    /// - Bulk database inserts
    /// - Single transaction for multiple operations
    /// - Batch API calls
    ///
    /// # Semantics
    ///
    /// - Events are processed in order (no reordering)
    /// - Default is fail-fast (first error stops processing)
    ///
    /// # Example
    ///
    /// ```ignore
    /// async fn handle_batch(&mut self, events: Vec<E>, ctx: EffectContext<D, S>) -> Result<()> {
    ///     // Single transaction for all events
    ///     let ids: Vec<_> = events.iter().map(|e| e.id).collect();
    ///     ctx.deps().db.bulk_insert(&events).await?;
    ///     // Emit events for all inserts
    ///     for id in ids {
    ///         ctx.emit(MyEvent::Created { id });
    ///     }
    ///     Ok(())
    /// }
    /// ```
    async fn handle_batch(
        &mut self,
        events: Vec<E>,
        ctx: EffectContext<D, S>,
    ) -> Result<()>
    where
        D: Send + Sync + 'static,
        S: Clone + Send + 'static,
    {
        for event in events {
            self.handle(event, ctx.clone()).await?;
        }
        Ok(())
    }
}

/// Type-erased effect trait for internal use.
#[async_trait]
pub(crate) trait AnyEffect<D, S>: Send + Sync {
    /// Handle a type-erased event.
    ///
    /// Effects emit events via ctx.emit() during execution.
    async fn handle_any(
        &mut self,
        event: Arc<dyn Any + Send + Sync>,
        ctx: EffectContext<D, S>,
    ) -> Result<()>;

    /// Handle a batch of type-erased events.
    ///
    /// Effects emit events via ctx.emit() during execution.
    async fn handle_any_batch(
        &mut self,
        events: Vec<Arc<dyn Any + Send + Sync>>,
        ctx: EffectContext<D, S>,
    ) -> Result<()>;
}

/// Wrapper to make concrete effects implement AnyEffect.
pub(crate) struct EffectWrapper<Eff, E, D, S = ()>
where
    Eff: Effect<E, D, S>,
    E: Event + Clone,
    D: Send + Sync + 'static,
    S: Send + 'static,
{
    effect: Arc<tokio::sync::Mutex<Eff>>,
    _phantom: std::marker::PhantomData<(E, D, S)>,
}

impl<Eff, E, D, S> EffectWrapper<Eff, E, D, S>
where
    Eff: Effect<E, D, S>,
    E: Event + Clone,
    D: Send + Sync + 'static,
    S: Send + 'static,
{
    pub fn new(effect: Eff) -> Self {
        Self {
            effect: Arc::new(tokio::sync::Mutex::new(effect)),
            _phantom: std::marker::PhantomData,
        }
    }
}

#[async_trait]
impl<Eff, E, D, S> AnyEffect<D, S> for EffectWrapper<Eff, E, D, S>
where
    Eff: Effect<E, D, S>,
    E: Event + Clone,
    D: Send + Sync + 'static,
    S: Clone + Send + Sync + 'static,
{
    async fn handle_any(
        &mut self,
        event: Arc<dyn Any + Send + Sync>,
        ctx: EffectContext<D, S>,
    ) -> Result<()> {
        // Try to downcast the Arc<dyn Any> to &E
        let input_event = event
            .downcast_ref::<E>()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "event type mismatch: expected {}, got TypeId {:?}",
                    std::any::type_name::<E>(),
                    (*event).type_id()
                )
            })?
            .clone();

        let mut effect = self.effect.lock().await;
        effect.handle(input_event, ctx).await
    }

    async fn handle_any_batch(
        &mut self,
        events: Vec<Arc<dyn Any + Send + Sync>>,
        ctx: EffectContext<D, S>,
    ) -> Result<()> {
        let typed: Result<Vec<E>, _> = events
            .into_iter()
            .map(|e| {
                e.downcast_ref::<E>()
                    .cloned()
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "event type mismatch: expected {}, got TypeId {:?}",
                            std::any::type_name::<E>(),
                            (*e).type_id()
                        )
                    })
            })
            .collect();
        let mut effect = self.effect.lock().await;
        effect.handle_batch(typed?, ctx).await
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

    #[derive(Debug, Clone)]
    struct TestState {
        counter: i32,
    }

    #[derive(Debug, Clone, PartialEq)]
    struct TestEvent {
        action: String,
    }

    // Event is auto-implemented via blanket impl

    #[derive(Debug, Clone, PartialEq)]
    struct OutputEvent {
        result: String,
    }

    // Event is auto-implemented via blanket impl

    // Test effect
    struct TestEffect {
        call_count: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Effect<TestEvent, TestDeps, TestState> for TestEffect {
        type Event = OutputEvent;

        async fn handle(
            &mut self,
            event: TestEvent,
            ctx: EffectContext<TestDeps, TestState>,
        ) -> Result<()> {
            self.call_count.fetch_add(1, Ordering::Relaxed);

            // Access deps and state
            let value = ctx.deps().value;
            let counter = ctx.state().counter;

            // Emit event
            ctx.emit(OutputEvent {
                result: format!("{} with value {} counter {}", event.action, value, counter),
            });
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_effect_context_deps_and_state() {
        let deps = Arc::new(TestDeps { value: 42 });
        let state = TestState { counter: 10 };
        let bus = EventBus::new();
        let ctx = EffectContext::new(deps, state, bus);

        assert_eq!(ctx.deps().value, 42);
        assert_eq!(ctx.state().counter, 10);
    }

    #[tokio::test]
    async fn test_effect_handle() {
        let call_count = Arc::new(AtomicUsize::new(0));
        let mut effect = TestEffect {
            call_count: call_count.clone(),
        };

        let deps = Arc::new(TestDeps { value: 100 });
        let state = TestState { counter: 5 };
        let bus = EventBus::new();

        // Subscribe to receive emitted events
        let mut sub = bus.subscribe();

        let ctx = EffectContext::new(deps, state, bus);

        effect
            .handle(
                TestEvent {
                    action: "test".to_string(),
                },
                ctx,
            )
            .await
            .unwrap();

        assert_eq!(call_count.load(Ordering::Relaxed), 1);

        // Receive emitted event
        let envelope = sub.recv().await.unwrap();
        let event = envelope.payload.downcast_ref::<OutputEvent>().unwrap();
        assert_eq!(
            event.result,
            "test with value 100 counter 5"
        );
    }

    #[tokio::test]
    async fn test_effect_emits_event() {
        struct TransformEffect;

        #[async_trait]
        impl Effect<TestEvent, TestDeps, TestState> for TransformEffect {
            type Event = OutputEvent;

            async fn handle(
                &mut self,
                event: TestEvent,
                ctx: EffectContext<TestDeps, TestState>,
            ) -> Result<()> {
                // Effects emit events
                ctx.emit(OutputEvent {
                    result: format!("transformed: {}", event.action),
                });
                Ok(())
            }
        }

        let mut effect = TransformEffect;
        let deps = Arc::new(TestDeps { value: 0 });
        let state = TestState { counter: 0 };
        let bus = EventBus::new();
        let mut sub = bus.subscribe();
        let ctx = EffectContext::new(deps, state, bus);

        effect
            .handle(TestEvent { action: "test".to_string() }, ctx)
            .await
            .unwrap();

        let envelope = sub.recv().await.unwrap();
        let event = envelope.payload.downcast_ref::<OutputEvent>().unwrap();
        assert_eq!(event.result, "transformed: test");
    }

    #[tokio::test]
    async fn test_effect_wrapper() {
        let call_count = Arc::new(AtomicUsize::new(0));
        let effect = TestEffect {
            call_count: call_count.clone(),
        };
        let mut wrapper = EffectWrapper::new(effect);

        let deps = Arc::new(TestDeps { value: 50 });
        let state = TestState { counter: 3 };
        let bus = EventBus::new();
        let mut sub = bus.subscribe();
        let ctx = EffectContext::new(deps, state, bus);

        let event: Arc<dyn Any + Send + Sync> = Arc::new(TestEvent {
            action: "wrapped".to_string(),
        });

        wrapper.handle_any(event, ctx).await.unwrap();

        assert_eq!(call_count.load(Ordering::Relaxed), 1);
        let envelope = sub.recv().await.unwrap();
        let event = envelope.payload.downcast_ref::<OutputEvent>().unwrap();
        assert_eq!(event.result, "wrapped with value 50 counter 3");
    }

    #[tokio::test]
    async fn test_effect_wrapper_wrong_event_type() {
        let effect = TestEffect {
            call_count: Arc::new(AtomicUsize::new(0)),
        };
        let mut wrapper = EffectWrapper::new(effect);

        let deps = Arc::new(TestDeps { value: 0 });
        let state = TestState { counter: 0 };
        let bus = EventBus::new();
        let ctx = EffectContext::new(deps, state, bus);

        // Wrong event type
        #[derive(Debug, Clone)]
        struct WrongEvent;
        // Event is auto-implemented via blanket impl

        let event: Arc<dyn Any + Send + Sync> = Arc::new(WrongEvent);

        let result = wrapper.handle_any(event, ctx).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("type mismatch"));
    }

    // Test effect that returns error
    struct FailingEffect;

    #[async_trait]
    impl Effect<TestEvent, TestDeps, TestState> for FailingEffect {
        type Event = OutputEvent;

        async fn handle(
            &mut self,
            _event: TestEvent,
            _ctx: EffectContext<TestDeps, TestState>,
        ) -> Result<()> {
            Err(anyhow::anyhow!("effect failed"))
        }
    }

    #[tokio::test]
    async fn test_effect_error() {
        let mut effect = FailingEffect;

        let deps = Arc::new(TestDeps { value: 0 });
        let state = TestState { counter: 0 };
        let bus = EventBus::new();

        let ctx = EffectContext::new(deps, state, bus);

        let result = effect
            .handle(
                TestEvent {
                    action: "fail".to_string(),
                },
                ctx,
            )
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("effect failed"));
    }

    #[tokio::test]
    async fn test_effect_context_clone() {
        let deps = Arc::new(TestDeps { value: 42 });
        let state = TestState { counter: 10 };
        let bus = EventBus::new();
        let ctx1 = EffectContext::new(deps, state, bus);
        let ctx2 = ctx1.clone();

        assert_eq!(ctx1.deps().value, ctx2.deps().value);
        assert_eq!(ctx1.state().counter, ctx2.state().counter);
    }

    // Test effect with custom batch implementation
    struct BatchOptimizedEffect {
        individual_calls: Arc<AtomicUsize>,
        batch_calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Effect<TestEvent, TestDeps, TestState> for BatchOptimizedEffect {
        type Event = OutputEvent;

        async fn handle(
            &mut self,
            event: TestEvent,
            ctx: EffectContext<TestDeps, TestState>,
        ) -> Result<()> {
            self.individual_calls.fetch_add(1, Ordering::Relaxed);
            ctx.emit(OutputEvent {
                result: format!("individual: {}", event.action),
            });
            Ok(())
        }

        async fn handle_batch(
            &mut self,
            events: Vec<TestEvent>,
            ctx: EffectContext<TestDeps, TestState>,
        ) -> Result<()> {
            self.batch_calls.fetch_add(1, Ordering::Relaxed);

            // Simulated bulk operation - emit events for all inputs
            for e in events.iter() {
                ctx.emit(OutputEvent {
                    result: format!("batch: {}", e.action),
                });
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_effect_batch_override() {
        let individual_calls = Arc::new(AtomicUsize::new(0));
        let batch_calls = Arc::new(AtomicUsize::new(0));

        let mut effect = BatchOptimizedEffect {
            individual_calls: individual_calls.clone(),
            batch_calls: batch_calls.clone(),
        };

        let deps = Arc::new(TestDeps { value: 0 });
        let state = TestState { counter: 0 };
        let bus = EventBus::new();
        let mut sub = bus.subscribe();

        let ctx = EffectContext::new(deps, state, bus);

        let events = vec![
            TestEvent {
                action: "a".to_string(),
            },
            TestEvent {
                action: "b".to_string(),
            },
            TestEvent {
                action: "c".to_string(),
            },
        ];

        effect.handle_batch(events, ctx).await.unwrap();

        // Should use batch path, not individual
        assert_eq!(individual_calls.load(Ordering::Relaxed), 0);
        assert_eq!(batch_calls.load(Ordering::Relaxed), 1);

        // Should emit events for all inputs
        let env1 = sub.recv().await.unwrap();
        let env2 = sub.recv().await.unwrap();
        let env3 = sub.recv().await.unwrap();
        let e1 = env1.payload.downcast_ref::<OutputEvent>().unwrap();
        let e2 = env2.payload.downcast_ref::<OutputEvent>().unwrap();
        let e3 = env3.payload.downcast_ref::<OutputEvent>().unwrap();
        assert_eq!(e1.result, "batch: a");
        assert_eq!(e2.result, "batch: b");
        assert_eq!(e3.result, "batch: c");
    }

    #[tokio::test]
    async fn test_effect_default_batch_uses_handle() {
        let call_count = Arc::new(AtomicUsize::new(0));
        let mut effect = TestEffect {
            call_count: call_count.clone(),
        };

        let deps = Arc::new(TestDeps { value: 100 });
        let state = TestState { counter: 2 };
        let bus = EventBus::new();
        let mut sub = bus.subscribe();

        let ctx = EffectContext::new(deps, state, bus);

        let events = vec![
            TestEvent {
                action: "x".to_string(),
            },
            TestEvent {
                action: "y".to_string(),
            },
        ];

        effect.handle_batch(events, ctx).await.unwrap();

        // Default batch impl calls handle for each
        assert_eq!(call_count.load(Ordering::Relaxed), 2);

        // Should emit 2 individual events
        let env1 = sub.recv().await.unwrap();
        let env2 = sub.recv().await.unwrap();
        let e1 = env1.payload.downcast_ref::<OutputEvent>().unwrap();
        let e2 = env2.payload.downcast_ref::<OutputEvent>().unwrap();
        assert_eq!(e1.result, "x with value 100 counter 2");
        assert_eq!(e2.result, "y with value 100 counter 2");
    }
}
