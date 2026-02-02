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
use tokio::task::JoinHandle;

use crate::bus::EventBus;
use crate::core::{CorrelationId, Event, EventEnvelope};
use crate::dispatch::Dispatcher;
use crate::edge::Edge;
use crate::effect_impl::Effect;
use crate::runtime::Runtime;

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
pub struct Engine<D, S = ()> {
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

        tokio::time::timeout(timeout_duration, async {
            let mut receiver = self.bus.subscribe();

            loop {
                match receiver.try_recv() {
                    Ok(envelope) if envelope.cid == cid => {
                        // Process this event through effects
                        self.dispatcher
                            .dispatch_event(envelope, state.clone(), Some(&self.inflight))
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

            Ok::<(), anyhow::Error>(())
        })
        .await??;

        // Read final result from edge
        Ok(edge.read(&state))
    }

    /// Get access to the dependencies.
    pub fn deps(&self) -> &D {
        &self.deps
    }

    /// Get access to the event bus.
    pub fn bus(&self) -> &EventBus {
        &self.bus
    }

    /// Start the engine in background mode.
    ///
    /// Returns a handle for emitting events.
    pub fn start(self) -> EngineHandle<D, S> {
        let bus = self.bus.clone();
        let deps = self.deps.clone();
        let inflight = self.inflight.clone();

        // Spawn runtime in background
        let runtime = Runtime::new(self.dispatcher, self.bus.clone())
            .with_inflight(inflight.clone());

        let handle: JoinHandle<Result<()>> = tokio::spawn(async move {
            runtime.run().await
        });

        EngineHandle {
            deps,
            bus,
            inflight,
            runtime_handle: Some(handle),
            _phantom: std::marker::PhantomData,
        }
    }
}

// Note: Engine doesn't implement Clone because Dispatcher can't be cloned
// (effects are stateful and wrapped in Arc<Mutex>)

/// Handle for interacting with a running engine.
pub struct EngineHandle<D, S = ()> {
    deps: Arc<D>,
    bus: EventBus,
    inflight: Arc<InflightTracker>,
    runtime_handle: Option<JoinHandle<Result<()>>>,
    _phantom: std::marker::PhantomData<S>,
}

impl<D, S> EngineHandle<D, S> {
    /// Emit an event (fire-and-forget).
    pub fn emit<E: Event>(&self, event: E) {
        self.bus.emit(event);
    }

    /// Emit an event and wait for all cascading work to complete.
    pub async fn emit_and_await<E: Event>(&self, event: E) -> Result<()> {
        let cid = CorrelationId::new();
        self.inflight.inc(cid, 1);
        self.bus.emit_with_correlation(event, cid);
        self.inflight.wait_for(cid, Duration::from_secs(30)).await
    }

    /// Get access to the dependencies.
    pub fn deps(&self) -> &D {
        &self.deps
    }

    /// Get access to the event bus.
    pub fn bus(&self) -> &EventBus {
        &self.bus
    }

    /// Stop the runtime (if running).
    pub async fn stop(mut self) -> Result<()> {
        if let Some(handle) = self.runtime_handle.take() {
            handle.abort();
            Ok(())
        } else {
            Ok(())
        }
    }
}

// =============================================================================
// Builder
// =============================================================================

/// Builder for constructing an engine.
pub struct EngineBuilder<D, S = ()> {
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

    /// Register an effect handler.
    pub fn with_effect<E, Eff>(mut self, effect: Eff) -> Self
    where
        E: Event + Clone,
        Eff: Effect<E, D, S>,
    {
        self.dispatcher = self.dispatcher.with_effect::<E, Eff>(effect);
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
    use crate::core::Event;
    use crate::edge::{Edge, EdgeContext};
    use crate::effect_impl::{Effect, EffectContext};
    use std::sync::atomic::{AtomicUsize, Ordering};

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

    impl Event for TestEvent {
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    struct TestEdge {
        initial: i32,
    }

    impl Edge<TestState> for TestEdge {
        type Data = i32;

        fn execute(&self, _ctx: &EdgeContext<TestState>) -> Option<Box<dyn Event>> {
            Some(Box::new(TestEvent {
                value: self.initial,
            }))
        }

        fn read(&self, state: &TestState) -> Option<i32> {
            state.result
        }
    }

    struct TestEffect;

    #[async_trait::async_trait]
    impl Effect<TestEvent, TestDeps, TestState> for TestEffect {
        type Event = TestEvent;

        async fn handle(
            &mut self,
            event: TestEvent,
            mut ctx: EffectContext<TestDeps, TestState>,
        ) -> Result<Option<TestEvent>> {
            // Update state (this is a hack for testing - normally reducers would do this)
            // In real usage, we'd use reducers to transform state
            Ok(None) // Don't emit more events
        }
    }

    #[tokio::test]
    async fn test_engine_builder() {
        let engine = EngineBuilder::new(TestDeps { value: 42 })
            .with_effect::<TestEvent, _>(TestEffect)
            .build();

        assert_eq!(engine.deps().value, 42);
    }

    #[tokio::test]
    async fn test_engine_handle_emit() {
        let engine = EngineBuilder::new(TestDeps { value: 42 })
            .with_effect::<TestEvent, _>(TestEffect)
            .build();

        let handle = engine.start();
        handle.emit(TestEvent { value: 100 });

        // Clean up
        handle.stop().await.unwrap();
    }
}
