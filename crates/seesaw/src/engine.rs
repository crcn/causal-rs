//! Seesaw Engine - unified orchestration for edges, effects, and event bus.
//!
//! The Engine is the central coordinator for the seesaw architecture:
//!
//! ```text
//! Edge → Events → Effects → Events → Effects → ... (until settled)
//! ```
//!
//! # Usage
//!
//! ```ignore
//! use seesaw::{EngineBuilder, EventBus, Edge};
//!
//! let engine = EngineBuilder::new(deps)
//!     .with_effect::<MyEvent, _, ()>(MyEffect)
//!     .build();
//!
//! // Run an edge with state
//! let result = engine.run(MyEdge { data }, initial_state).await?;
//! ```

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use dashmap::DashMap;
use tokio::sync::Notify;

use crate::bus::EventBus;
use crate::core::{CorrelationId, Event, EventEnvelope};
use crate::dispatch::Dispatcher;
use crate::edge::Edge;
use crate::effect_impl::Effect;
use crate::tap::EventTap;

// =============================================================================
// Inflight Tracking
// =============================================================================

/// Entry tracking inflight work for a single correlation ID.
pub(crate) struct InflightEntry {
    /// Count of pending work items
    count: AtomicUsize,
    /// Number of tasks waiting for completion
    waiters: AtomicUsize,
    /// Notifier for waiters when count hits zero
    notify: Notify,
    /// First error encountered (if any)
    first_error: Mutex<Option<anyhow::Error>>,
}

impl InflightEntry {
    fn new() -> Self {
        Self {
            count: AtomicUsize::new(0),
            waiters: AtomicUsize::new(0),
            notify: Notify::new(),
            first_error: Mutex::new(None),
        }
    }
}

/// Tracks inflight work per correlation ID.
#[derive(Default)]
pub struct InflightTracker {
    entries: DashMap<CorrelationId, Arc<InflightEntry>>,
}

impl InflightTracker {
    /// Create a new inflight tracker.
    pub fn new() -> Self {
        Self {
            entries: DashMap::new(),
        }
    }

    /// Get or create an entry for the given correlation ID.
    pub(crate) fn get_or_create(&self, cid: CorrelationId) -> Arc<InflightEntry> {
        self.entries
            .entry(cid)
            .or_insert_with(|| Arc::new(InflightEntry::new()))
            .clone()
    }

    /// Increment the inflight count for a correlation ID.
    pub fn inc(&self, cid: CorrelationId, n: usize) {
        let entry = self.get_or_create(cid);
        entry.count.fetch_add(n, Ordering::AcqRel);
    }

    /// Decrement the inflight count for a correlation ID.
    pub fn dec(&self, cid: CorrelationId, n: usize) {
        let entry = self.get_or_create(cid);
        let prev = entry.count.fetch_sub(n, Ordering::AcqRel);

        if prev == n {
            // Count reached zero, notify waiters
            entry.notify.notify_waiters();

            // Clean up entry if no waiters
            if entry.waiters.load(Ordering::Acquire) == 0 {
                self.entries.remove(&cid);
            }
        }
    }

    /// Record an error for a correlation ID.
    pub fn record_error(&self, cid: CorrelationId, error: anyhow::Error) {
        let entry = self.get_or_create(cid);
        let mut first_error = entry.first_error.lock().unwrap();
        if first_error.is_none() {
            *first_error = Some(error);
        }
    }

    /// Check if there is any inflight work.
    pub fn has_inflight(&self) -> bool {
        !self.entries.is_empty()
    }

    /// Wait for correlation ID to complete.
    pub async fn wait_for(&self, cid: CorrelationId, timeout: Duration) -> Result<()> {
        let entry = self.get_or_create(cid);
        entry.waiters.fetch_add(1, Ordering::AcqRel);

        let result = tokio::time::timeout(timeout, async {
            loop {
                if entry.count.load(Ordering::Acquire) == 0 {
                    break;
                }
                entry.notify.notified().await;
            }
        })
        .await;

        entry.waiters.fetch_sub(1, Ordering::AcqRel);

        // Clean up if no more waiters and count is zero
        if entry.count.load(Ordering::Acquire) == 0
            && entry.waiters.load(Ordering::Acquire) == 0
        {
            self.entries.remove(&cid);
        }

        match result {
            Ok(_) => {
                // Check for errors
                let first_error = entry.first_error.lock().unwrap();
                if let Some(ref error) = *first_error {
                    return Err(anyhow::anyhow!("{}", error));
                }
                Ok(())
            }
            Err(_) => Err(anyhow::anyhow!("timeout waiting for correlation")),
        }
    }
}

// =============================================================================
// Engine
// =============================================================================

/// Engine for running edges and coordinating events/effects.
pub struct Engine<D, S: Clone = ()> {
    deps: Arc<D>,
    bus: EventBus,
    dispatcher: Dispatcher<D, S>,
    inflight: Arc<InflightTracker>,
}

impl<D: Send + Sync + 'static, S: Clone + Send + Sync + 'static + Default> Engine<D, S> {
    /// Run an edge with the given state.
    ///
    /// This is the main entry point for executing event flows:
    /// 1. Execute edge to get initial event
    /// 2. Process events through effects until settled
    /// 3. Read final result from edge
    ///
    /// # Example
    ///
    /// ```ignore
    /// let user = engine.run(SignupEdge { email, name }, request_state).await?
    ///     .ok_or_else(|| anyhow!("signup failed"))?;
    /// ```
    pub async fn run<E>(&mut self, edge: E, state: S) -> Result<Option<E::Data>>
    where
        E: Edge<S>,
    {
        // Create correlation ID for tracking
        let cid = CorrelationId::new();

        // Execute edge to get initial event (if any)
        let ctx = crate::edge::EdgeContext::new(Arc::new(state.clone()));
        if let Some(event) = edge.execute(&ctx) {
            // Emit initial event with correlation
            self.inflight.inc(cid, 1);
            let envelope = EventEnvelope::new(cid, event);
            self.bus.emit_envelope(envelope);
        }

        // Process events until settled (with timeout)
        let timeout_duration = Duration::from_secs(30);

        let final_state = tokio::time::timeout(timeout_duration, async {
            let mut receiver = self.bus.subscribe();
            let mut current_state = state;

            loop {
                match receiver.try_recv() {
                    Ok(envelope) if envelope.cid == cid => {
                        // Apply reducers to get new state
                        current_state = self.dispatcher.apply_reducers(&current_state, &envelope);

                        // Process this event through effects
                        self.dispatcher
                            .dispatch_event(envelope, current_state.clone(), Some(&self.inflight))
                            .await?;
                    }
                    Ok(_) => {
                        // Different correlation ID, skip
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {
                        // Check if correlation is settled
                        if !self.inflight.has_inflight() {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::TryRecvError::Closed) => {
                        break;
                    }
                }
            }

            Ok::<S, anyhow::Error>(current_state)
        })
        .await??;

        // Read final result from edge using the final transformed state
        Ok(edge.read(&final_state))
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

// Note: Engine doesn't implement Clone because Dispatcher can't be cloned
// (effects are stateful and wrapped in Arc<Mutex>)

// =============================================================================
// Builder
// =============================================================================

/// Builder for constructing an engine.
pub struct EngineBuilder<D, S: Clone = ()> {
    deps: Arc<D>,
    bus: EventBus,
    dispatcher: Dispatcher<D, S>,
    inflight: Option<Arc<InflightTracker>>,
}

impl<D: Send + Sync + 'static, S: Clone + Send + Sync + 'static + Default>
    EngineBuilder<D, S>
{
    /// Create a new engine builder.
    pub fn new(deps: D) -> Self {
        let bus = EventBus::new();
        let deps_arc = Arc::new(deps);
        let dispatcher = Dispatcher::from_arc(deps_arc.clone(), bus.clone());

        Self {
            deps: deps_arc,
            bus,
            dispatcher,
            inflight: None,
        }
    }

    /// Create a new engine builder with an existing event bus.
    pub fn with_bus(deps: D, bus: EventBus) -> Self {
        let deps_arc = Arc::new(deps);
        let dispatcher = Dispatcher::from_arc(deps_arc.clone(), bus.clone());

        Self {
            deps: deps_arc,
            bus,
            dispatcher,
            inflight: None,
        }
    }

    /// Create a new engine builder with pre-wrapped Arc dependencies.
    pub fn with_arc(deps: Arc<D>) -> Self {
        let bus = EventBus::new();
        let dispatcher = Dispatcher::from_arc(deps.clone(), bus.clone());

        Self {
            deps,
            bus,
            dispatcher,
            inflight: None,
        }
    }

    /// Register a reducer for pure state transformations.
    ///
    /// Reducers run before effects to transform state in response to events.
    pub fn with_reducer<E, R>(mut self, reducer: R) -> Self
    where
        E: Event + Clone,
        R: crate::reducer::Reducer<E, S>,
    {
        self.dispatcher = self.dispatcher.with_reducer::<E, R>(reducer);
        self
    }

    /// Register an effect handler.
    pub fn with_effect<E, Eff>(mut self, effect: Eff) -> Self
    where
        E: Event + Clone,
        Eff: Effect<E, D, S>,
    {
        self.dispatcher = self.dispatcher.with_effect::<E, Eff>(effect);
        self
    }

    /// Register an event tap for observing events.
    ///
    /// Taps are called after effects complete and events are emitted.
    /// They run fire-and-forget and cannot emit new events.
    pub fn with_event_tap<E, T>(mut self, tap: T) -> Self
    where
        E: Event + Clone,
        T: EventTap<E>,
    {
        self.dispatcher = self.dispatcher.with_event_tap::<E, T>(tap);
        self
    }

    /// Enable inflight tracking.
    pub fn with_inflight(mut self, inflight: Arc<InflightTracker>) -> Self {
        self.inflight = Some(inflight);
        self
    }

    /// Build the engine.
    pub fn build(self) -> Engine<D, S> {
        Engine {
            deps: self.deps,
            bus: self.bus,
            dispatcher: self.dispatcher,
            inflight: self.inflight.unwrap_or_else(|| Arc::new(InflightTracker::new())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edge::{Edge, EdgeContext};
    use crate::effect_impl::{Effect, EffectContext};

    #[derive(Clone)]
    struct TestDeps {
        value: i32,
    }

    #[derive(Clone, Default)]
    struct TestState {
        result: Option<i32>,
    }

    #[derive(Debug, Clone)]
    struct TestEvent {
        value: i32,
    }

    #[derive(Debug, Clone)]
    struct TerminalEvent;

    // Events are auto-implemented via blanket impl

    struct TestEdge {
        initial: i32,
    }

    impl Edge<TestState> for TestEdge {
        type Event = TestEvent;
        type Data = i32;

        fn execute(&self, _ctx: &EdgeContext<TestState>) -> Option<TestEvent> {
            Some(TestEvent {
                value: self.initial,
            })
        }

        fn read(&self, state: &TestState) -> Option<i32> {
            state.result
        }
    }

    struct TestEffect;

    #[async_trait::async_trait]
    impl Effect<TestEvent, TestDeps, TestState> for TestEffect {
        type Event = TerminalEvent;

        async fn handle(
            &mut self,
            _event: TestEvent,
            _ctx: EffectContext<TestDeps, TestState>,
        ) -> Result<TerminalEvent> {
            // Emit terminal event - no effect handles this, so flow terminates
            Ok(TerminalEvent)
        }
    }

    // No effect registered for TerminalEvent, so the flow naturally terminates

    #[tokio::test]
    async fn test_engine_builder() {
        let engine = EngineBuilder::new(TestDeps { value: 42 })
            .with_effect::<TestEvent, _>(TestEffect)
            .build();

        assert_eq!(engine.deps().value, 42);
    }

    // Test reducer integration
    struct TestReducer;

    impl crate::reducer::Reducer<TestEvent, TestState> for TestReducer {
        fn reduce(&self, _state: &TestState, event: &TestEvent) -> TestState {
            TestState {
                result: Some(event.value * 2), // Double the value
            }
        }
    }

    #[test]
    fn test_engine_builder_with_reducer() {
        // Test that EngineBuilder accepts reducers
        let _engine = EngineBuilder::new(TestDeps { value: 42 })
            .with_reducer::<TestEvent, _>(TestReducer)
            .with_effect::<TestEvent, _>(TestEffect)
            .build();

        // If this compiles, the API works correctly
    }

    #[test]
    fn test_reducer_applied_in_dispatcher() {
        use crate::bus::EventBus;
        use crate::dispatch::Dispatcher;

        let bus = EventBus::new();
        let dispatcher = Dispatcher::new(TestDeps { value: 42 }, bus)
            .with_reducer::<TestEvent, _>(TestReducer);

        // Test that reducer is registered
        let state = TestState { result: None };
        let event = TestEvent { value: 10 };
        let envelope = crate::core::EventEnvelope::new_random(event);

        // Apply reducers
        let new_state = dispatcher.apply_reducers(&state, &envelope);

        // Reducer should have doubled the value
        assert_eq!(new_state.result, Some(20));
    }

    #[test]
    fn test_multiple_reducers_chain() {
        use crate::bus::EventBus;
        use crate::dispatch::Dispatcher;

        struct AddReducer {
            amount: i32,
        }

        impl crate::reducer::Reducer<TestEvent, TestState> for AddReducer {
            fn reduce(&self, state: &TestState, _event: &TestEvent) -> TestState {
                TestState {
                    result: Some(state.result.unwrap_or(0) + self.amount),
                }
            }
        }

        let bus = EventBus::new();
        let dispatcher = Dispatcher::new(TestDeps { value: 42 }, bus)
            .with_reducer::<TestEvent, _>(AddReducer { amount: 5 })
            .with_reducer::<TestEvent, _>(AddReducer { amount: 10 });

        let state = TestState { result: Some(0) };
        let event = TestEvent { value: 100 };
        let envelope = crate::core::EventEnvelope::new_random(event);

        let new_state = dispatcher.apply_reducers(&state, &envelope);

        // First reducer adds 5, second adds 10, so result should be 15
        assert_eq!(new_state.result, Some(15));
    }

}
