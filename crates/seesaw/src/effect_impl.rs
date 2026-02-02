//! Effect trait and context for event handling.
//!
//! Effects are event handlers that execute IO and emit new events.
//!
//! # Key Properties
//!
//! - **Event → Effect → Event**: Simple, direct flow
//! - **Can be stateful or stateless**: Your choice with `&mut self`
//! - **Return events**: Effects return `Option<Event>` (Runtime emits)
//! - **State flow**: Effects receive per-execution state via `EffectContext<D, S>`

use std::any::Any;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use crate::bus::EventBus;
use crate::core::{CorrelationId, Event, EventEnvelope};
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
/// 3. Return events (Runtime emits)
/// 4. Optionally signal UI progress via `signal()`
///
/// Effects do NOT have access to:
/// - The raw EventBus (removed)
/// - Spawning their own emitters
///
/// # Correlation Propagation
///
/// When an effect returns an event, the correlation ID from the original
/// event is automatically propagated. This enables `emit_and_await`
/// to track all cascading work.
///
/// # Example
///
/// ```ignore
/// async fn handle(&mut self, event: UserEvent, ctx: EffectContext<MyDeps, RequestState>) -> Result<Option<UserEvent>> {
///     match event {
///         UserEvent::SignupRequested { email, name } => {
///             // Access state
///             let visitor_id = ctx.state().visitor_id;
///
///             // Access dependencies
///             let user = ctx.deps().db.create_user(&email, &name, visitor_id).await?;
///
///             // Return event (Runtime emits with correlation ID)
///             Ok(Some(UserEvent::SignedUp { user_id: user.id, email }))
///         }
///         _ => Ok(None)
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

    /// Fire-and-forget signal for UI observability.
    ///
    /// Signals are NOT fact events - they are transient UI updates
    /// like typing indicators and progress notifications.
    ///
    /// # Constraints (Frozen)
    ///
    /// - **Not persisted** - signals are ephemeral
    /// - **Not replayed** - excluded from event streams
    /// - **Not observed by machines** - machines only react to fact events
    /// - **Not used for correctness** - system behavior unchanged if dropped
    /// - **Cannot trigger commands** - directly or indirectly (no "reactive UI machines")
    ///
    /// # Example
    ///
    /// ```ignore
    /// async fn execute(&self, cmd: ProcessCommand, ctx: EffectContext<Deps>) -> Result<ProcessEvent> {
    ///     // Signal progress started (UI-only, not a fact)
    ///     ctx.signal(ProgressSignal { task_id: cmd.task_id, percent: 0 });
    ///
    ///     // Do the actual work...
    ///     let result = process_data(...).await?;
    ///
    ///     // Return the fact event
    ///     Ok(ProcessEvent::Completed { task_id: cmd.task_id, result })
    /// }
    /// ```
    pub fn signal<E: Event>(&self, event: E) {
        // Signals use random correlation (fire-and-forget, no tracking)
        self.bus.emit(event);
    }
}

/// Context for interactive tool execution.
///
/// This is the blessed way to provide context to agent tools that need
/// to perform interactive operations (calling `dispatch_request`, etc.).
///
/// # When to Use
///
/// Use `ToolContext` when:
/// - Agent tools need to call `dispatch_request` for interactive actions
/// - Tools need access to the kernel/dependencies AND the event bus
///
/// # Example
///
/// ```ignore
/// let tool_ctx = ctx.tool_context();
/// let agent_config = registry
///     .select_for_container(&container, agent, tool_ctx.kernel.clone(), tool_ctx.bus.clone())
///     .await?;
/// ```
pub struct ToolContext<D> {
    /// Shared dependencies (kernel, database, etc.)
    pub deps: Arc<D>,
    /// Event bus for interactive dispatch_request calls.
    pub bus: EventBus,
}

impl<D> Clone for ToolContext<D> {
    fn clone(&self) -> Self {
        Self {
            deps: self.deps.clone(),
            bus: self.bus.clone(),
        }
    }
}

impl<D, S> EffectContext<D, S> {
    /// Get a context suitable for interactive tool execution.
    ///
    /// This provides the deps and bus needed by agent tools that call
    /// `dispatch_request` during their execution.
    pub fn tool_context(&self) -> ToolContext<D> {
        ToolContext {
            deps: self.deps.clone(),
            bus: self.bus.clone(),
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
/// 3. Return `Option<Event>` describing outcomes (Runtime emits them)
///
/// # Key Principles
///
/// - Effects have `&mut self` and can maintain state across invocations
/// - Effects return `Result<Option<Self::Event>>`. Return `None` if the event doesn't apply.
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
///     async fn handle(&mut self, event: UserEvent, ctx: EffectContext<MyDeps, RequestState>) -> Result<Option<UserEvent>> {
///         match event {
///             UserEvent::SignupRequested { email, name } => {
///                 let visitor_id = ctx.state().visitor_id;
///                 let user = ctx.deps().db.create_user(&email, &name, visitor_id).await?;
///                 Ok(Some(UserEvent::SignedUp { user_id: user.id, email }))
///             }
///             _ => Ok(None)
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

    /// Handle an event and optionally return a new event.
    ///
    /// The Runtime wraps the returned event in an EventEnvelope and emits it.
    /// Effects do not emit directly - they return events.
    ///
    /// Return:
    /// - `Ok(Some(event))` to emit a new event
    /// - `Ok(None)` if this event doesn't apply to this effect
    /// - `Err` if the event handling fails
    ///
    /// # Errors
    ///
    /// Return `Err` if the event handling fails. Errors will propagate
    /// and stop the event flow.
    async fn handle(&mut self, event: E, ctx: EffectContext<D, S>) -> Result<Option<Self::Event>>;

    /// Handle multiple events of the same type.
    ///
    /// Returns `Vec<Option<Self::Event>>` - one result per input event.
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
    /// - Each input event produces exactly one output (Some event or None)
    ///
    /// # Example
    ///
    /// ```ignore
    /// async fn handle_batch(&mut self, events: Vec<E>, ctx: EffectContext<D, S>) -> Result<Vec<Option<Self::Event>>> {
    ///     // Single transaction for all events
    ///     let ids: Vec<_> = events.iter().map(|e| e.id).collect();
    ///     ctx.deps().db.bulk_insert(&events).await?;
    ///     Ok(ids.into_iter().map(|id| Some(MyEvent::Created { id })).collect())
    /// }
    /// ```
    async fn handle_batch(
        &mut self,
        events: Vec<E>,
        ctx: EffectContext<D, S>,
    ) -> Result<Vec<Option<Self::Event>>>
    where
        D: Send + Sync + 'static,
        S: Clone + Send + 'static,
    {
        let mut output_events = Vec::with_capacity(events.len());
        for event in events {
            output_events.push(self.handle(event, ctx.clone()).await?);
        }
        Ok(output_events)
    }
}

/// Type-erased effect trait for internal use.
///
/// Returns `Option<EventEnvelope>` so the Runtime can emit events.
#[async_trait]
pub(crate) trait AnyEffect<D, S>: Send + Sync {
    /// Handle a type-erased event and optionally return an event envelope.
    ///
    /// The Runtime emits the returned envelope - effects never emit directly.
    async fn handle_any(
        &mut self,
        event: Arc<dyn Any + Send + Sync>,
        ctx: EffectContext<D, S>,
    ) -> Result<Option<EventEnvelope>>;

    /// Handle a batch of type-erased events and return event envelopes.
    ///
    /// Returns one optional envelope per input event. The Runtime emits all non-None results.
    async fn handle_any_batch(
        &mut self,
        events: Vec<Arc<dyn Any + Send + Sync>>,
        ctx: EffectContext<D, S>,
    ) -> Result<Vec<Option<EventEnvelope>>>;
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
    ) -> Result<Option<EventEnvelope>> {
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

        let cid = ctx.correlation_id();
        let mut effect = self.effect.lock().await;
        let output_event = effect.handle(input_event, ctx).await?;
        Ok(output_event.map(|e| EventEnvelope::new(cid, e)))
    }

    async fn handle_any_batch(
        &mut self,
        events: Vec<Arc<dyn Any + Send + Sync>>,
        ctx: EffectContext<D, S>,
    ) -> Result<Vec<Option<EventEnvelope>>> {
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
        let cid = ctx.correlation_id();
        let mut effect = self.effect.lock().await;
        let output_events = effect.handle_batch(typed?, ctx).await?;
        Ok(output_events
            .into_iter()
            .map(|opt| opt.map(|e| EventEnvelope::new(cid, e)))
            .collect())
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

    impl Event for TestEvent {
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[derive(Debug, Clone, PartialEq)]
    struct OutputEvent {
        result: String,
    }

    impl Event for OutputEvent {
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

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
        ) -> Result<Option<OutputEvent>> {
            self.call_count.fetch_add(1, Ordering::Relaxed);

            // Access deps and state
            let value = ctx.deps().value;
            let counter = ctx.state().counter;

            // Return event (Runtime emits)
            Ok(Some(OutputEvent {
                result: format!("{} with value {} counter {}", event.action, value, counter),
            }))
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

        let ctx = EffectContext::new(deps, state, bus);

        let event = effect
            .handle(
                TestEvent {
                    action: "test".to_string(),
                },
                ctx,
            )
            .await
            .unwrap();

        assert_eq!(call_count.load(Ordering::Relaxed), 1);
        assert_eq!(
            event.unwrap().result,
            "test with value 100 counter 5"
        );
    }

    #[tokio::test]
    async fn test_effect_handle_returns_none() {
        struct FilterEffect;

        #[async_trait]
        impl Effect<TestEvent, TestDeps, TestState> for FilterEffect {
            type Event = OutputEvent;

            async fn handle(
                &mut self,
                event: TestEvent,
                _ctx: EffectContext<TestDeps, TestState>,
            ) -> Result<Option<OutputEvent>> {
                if event.action == "skip" {
                    Ok(None)
                } else {
                    Ok(Some(OutputEvent {
                        result: "handled".to_string(),
                    }))
                }
            }
        }

        let mut effect = FilterEffect;
        let deps = Arc::new(TestDeps { value: 0 });
        let state = TestState { counter: 0 };
        let bus = EventBus::new();
        let ctx = EffectContext::new(deps, state, bus);

        let result1 = effect
            .handle(TestEvent { action: "skip".to_string() }, ctx.clone())
            .await
            .unwrap();
        assert!(result1.is_none());

        let result2 = effect
            .handle(TestEvent { action: "process".to_string() }, ctx)
            .await
            .unwrap();
        assert_eq!(result2.unwrap().result, "handled");
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
        let ctx = EffectContext::new(deps, state, bus);

        let event: Box<dyn Any + Send + Sync> = Box::new(TestEvent {
            action: "wrapped".to_string(),
        });

        let envelope = wrapper.handle_any(event, ctx).await.unwrap();

        assert_eq!(call_count.load(Ordering::Relaxed), 1);
        assert!(envelope.is_some());
        let event = envelope.unwrap().downcast_ref::<OutputEvent>().unwrap();
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
        #[derive(Debug)]
        struct WrongEvent;
        impl Event for WrongEvent {
            fn as_any(&self) -> &dyn Any {
                self
            }
        }

        let event: Box<dyn Any + Send + Sync> = Box::new(WrongEvent);

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
        ) -> Result<Option<OutputEvent>> {
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
            _ctx: EffectContext<TestDeps, TestState>,
        ) -> Result<Option<OutputEvent>> {
            self.individual_calls.fetch_add(1, Ordering::Relaxed);
            Ok(Some(OutputEvent {
                result: format!("individual: {}", event.action),
            }))
        }

        async fn handle_batch(
            &mut self,
            events: Vec<TestEvent>,
            _ctx: EffectContext<TestDeps, TestState>,
        ) -> Result<Vec<Option<OutputEvent>>> {
            self.batch_calls.fetch_add(1, Ordering::Relaxed);

            // Simulated bulk operation - return one event for the batch
            let actions: Vec<_> = events.iter().map(|e| e.action.as_str()).collect();
            Ok(vec![Some(OutputEvent {
                result: format!("batch: [{}]", actions.join(", ")),
            })])
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

        let results = effect.handle_batch(events, ctx).await.unwrap();

        // Should use batch path, not individual
        assert_eq!(individual_calls.load(Ordering::Relaxed), 0);
        assert_eq!(batch_calls.load(Ordering::Relaxed), 1);

        // Should return single batch event (custom batch can consolidate)
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].as_ref().unwrap().result, "batch: [a, b, c]");
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

        let ctx = EffectContext::new(deps, state, bus);

        let events = vec![
            TestEvent {
                action: "x".to_string(),
            },
            TestEvent {
                action: "y".to_string(),
            },
        ];

        let results = effect.handle_batch(events, ctx).await.unwrap();

        // Default batch impl calls handle for each
        assert_eq!(call_count.load(Ordering::Relaxed), 2);

        // Should return 2 individual events
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].as_ref().unwrap().result, "x with value 100 counter 2");
        assert_eq!(results[1].as_ref().unwrap().result, "y with value 100 counter 2");
    }
}
