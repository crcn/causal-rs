//! Runtime - Event processing loop
//!
//! The runtime processes events through reducers and effects:
//! 1. Event emitted
//! 2. Reducers transform state
//! 3. Effects handle event with new state
//! 4. Effects emit new events
//! 5. Repeat until settled

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::time::timeout;

use crate::bus::EventBus;
use crate::dispatch::Dispatcher;
use crate::engine::InflightTracker;

/// Runtime for processing events.
///
/// The runtime runs a simple event loop:
/// - Subscribe to event bus
/// - For each event:
///   - Apply reducers (state transformation)
///   - Dispatch to effects (which may emit new events)
/// - Continue until no more events
///
/// # Example
///
/// ```ignore
/// let runtime = Runtime::new(dispatcher, bus);
/// runtime.run().await?;
/// ```
pub struct Runtime<D, S> {
    dispatcher: Dispatcher<D, S>,
    bus: EventBus,
    inflight: Option<Arc<InflightTracker>>,
}

impl<D: Send + Sync + 'static, S: Clone + Send + Sync + 'static + Default> Runtime<D, S> {
    /// Create a new runtime.
    pub fn new(dispatcher: Dispatcher<D, S>, bus: EventBus) -> Self {
        Self {
            dispatcher,
            bus,
            inflight: None,
        }
    }

    /// Create a new runtime with inflight tracking.
    pub fn with_inflight(mut self, inflight: Arc<InflightTracker>) -> Self {
        self.inflight = Some(inflight);
        self
    }

    /// Run the event processing loop.
    ///
    /// Subscribes to the event bus and processes events until:
    /// - The timeout is reached (default 30s)
    /// - No more events are available
    /// - An error occurs
    pub async fn run(self) -> Result<()> {
        self.run_with_timeout(Duration::from_secs(30)).await
    }

    /// Run the event processing loop with a custom timeout.
    pub async fn run_with_timeout(mut self, duration: Duration) -> Result<()> {
        let mut receiver = self.bus.subscribe();

        let result = timeout(duration, async {
            loop {
                match receiver.try_recv() {
                    Ok(envelope) => {
                        // TODO: Apply reducers here before dispatching
                        // For now, just dispatch with empty/default state

                        // This is a placeholder - the actual state management
                        // will be handled by the engine's run() method
                        if let Err(e) = self
                            .dispatcher
                            .dispatch_event(envelope, S::default(), self.inflight.as_ref())
                            .await
                        {
                            return Err(e);
                        }
                    }
                    Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {
                        // No more events, check if inflight work is done
                        if let Some(tracker) = &self.inflight {
                            if !tracker.has_inflight() {
                                break;
                            }
                        } else {
                            // No inflight tracking, just break
                            break;
                        }

                        // Wait a bit for more events
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {
                        // Missed some events due to slow processing
                        // Continue processing remaining events
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::TryRecvError::Closed) => {
                        // Channel closed, stop processing
                        break;
                    }
                }
            }

            Ok(())
        })
        .await;

        match result {
            Ok(inner) => inner,
            Err(_) => Err(anyhow::anyhow!("runtime timeout exceeded")),
        }
    }

    /// Get access to the dependencies.
    pub fn deps(&self) -> &D {
        self.dispatcher.deps()
    }

    /// Get access to the event bus.
    pub fn bus(&self) -> &EventBus {
        &self.bus
    }
}

/// Builder for configuring the runtime.
pub struct RuntimeBuilder<D, S> {
    dispatcher: Dispatcher<D, S>,
    bus: EventBus,
    inflight: Option<Arc<InflightTracker>>,
}

impl<D: Send + Sync + 'static, S: Clone + Send + Sync + 'static + Default>
    RuntimeBuilder<D, S>
{
    /// Create a new runtime builder.
    pub fn new(dispatcher: Dispatcher<D, S>, bus: EventBus) -> Self {
        Self {
            dispatcher,
            bus,
            inflight: None,
        }
    }

    /// Enable inflight tracking.
    pub fn with_inflight(mut self, inflight: Arc<InflightTracker>) -> Self {
        self.inflight = Some(inflight);
        self
    }

    /// Build the runtime.
    pub fn build(self) -> Runtime<D, S> {
        let mut runtime = Runtime::new(self.dispatcher, self.bus);
        if let Some(inflight) = self.inflight {
            runtime = runtime.with_inflight(inflight);
        }
        runtime
    }
}

impl<D, S> std::fmt::Debug for Runtime<D, S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runtime").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::Event;
    use crate::effect_impl::{Effect, EffectContext};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone)]
    struct TestDeps {
        value: i32,
    }

    #[derive(Clone, Default)]
    struct TestState {
        counter: i32,
    }

    #[derive(Debug, Clone)]
    struct TestEvent {
        action: String,
    }

    // Event is auto-implemented via blanket impl

    #[derive(Debug, Clone)]
    struct ResultEvent {
        result: String,
    }

    // Event is auto-implemented via blanket impl

    struct TestEffect {
        call_count: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl Effect<TestEvent, TestDeps, TestState> for TestEffect {
        type Event = ResultEvent;

        async fn handle(
            &mut self,
            event: TestEvent,
            _ctx: EffectContext<TestDeps, TestState>,
        ) -> Result<Option<ResultEvent>> {
            self.call_count.fetch_add(1, Ordering::Relaxed);
            Ok(Some(ResultEvent {
                result: format!("processed {}", event.action),
            }))
        }
    }

    #[tokio::test]
    async fn test_runtime_processes_events() {
        use crate::engine::InflightTracker;
        use crate::core::{CorrelationId, EventEnvelope};

        let call_count = Arc::new(AtomicUsize::new(0));
        let bus = EventBus::new();
        let deps = TestDeps { value: 42 };

        let dispatcher = Dispatcher::new(deps, bus.clone())
            .with_effect::<TestEvent, _>(TestEffect {
                call_count: call_count.clone(),
            });

        let inflight = Arc::new(InflightTracker::new());

        // Increment inflight BEFORE starting runtime
        let cid = CorrelationId::new();
        inflight.inc(cid, 1);

        let runtime = Runtime::new(dispatcher, bus.clone())
            .with_inflight(inflight.clone());

        // Spawn runtime in background
        let runtime_handle = tokio::spawn(async move {
            runtime.run_with_timeout(Duration::from_secs(1)).await
        });

        // Give runtime time to subscribe
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Now emit event
        bus.emit_with_correlation(TestEvent {
            action: "test".to_string(),
        }, cid);

        // Wait for runtime to complete
        runtime_handle.await.unwrap().unwrap();

        assert_eq!(call_count.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn test_runtime_processes_multiple_events() {
        use crate::engine::InflightTracker;
        use crate::core::CorrelationId;

        let call_count = Arc::new(AtomicUsize::new(0));
        let bus = EventBus::new();
        let deps = TestDeps { value: 42 };

        let dispatcher = Dispatcher::new(deps, bus.clone())
            .with_effect::<TestEvent, _>(TestEffect {
                call_count: call_count.clone(),
            });

        let inflight = Arc::new(InflightTracker::new());

        // Increment inflight BEFORE starting runtime
        let cid = CorrelationId::new();
        inflight.inc(cid, 3);

        let runtime = Runtime::new(dispatcher, bus.clone())
            .with_inflight(inflight.clone());

        // Spawn runtime in background
        let runtime_handle = tokio::spawn(async move {
            runtime.run_with_timeout(Duration::from_secs(1)).await
        });

        // Give runtime time to subscribe
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Emit multiple events
        bus.emit_with_correlation(TestEvent {
            action: "first".to_string(),
        }, cid);
        bus.emit_with_correlation(TestEvent {
            action: "second".to_string(),
        }, cid);
        bus.emit_with_correlation(TestEvent {
            action: "third".to_string(),
        }, cid);

        // Wait for processing
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Wait for runtime to complete
        runtime_handle.await.unwrap().unwrap();

        assert_eq!(call_count.load(Ordering::Relaxed), 3);
    }

    #[tokio::test]
    async fn test_runtime_builder() {
        use crate::engine::InflightTracker;
        use crate::core::CorrelationId;

        let call_count = Arc::new(AtomicUsize::new(0));
        let bus = EventBus::new();
        let deps = TestDeps { value: 42 };

        let dispatcher = Dispatcher::new(deps, bus.clone())
            .with_effect::<TestEvent, _>(TestEffect {
                call_count: call_count.clone(),
            });

        let inflight = Arc::new(InflightTracker::new());

        // Increment inflight BEFORE starting runtime
        let cid = CorrelationId::new();
        inflight.inc(cid, 1);

        let runtime = RuntimeBuilder::new(dispatcher, bus.clone())
            .with_inflight(inflight.clone())
            .build();

        // Spawn runtime in background
        let runtime_handle = tokio::spawn(async move {
            runtime.run_with_timeout(Duration::from_secs(1)).await
        });

        // Give runtime time to subscribe
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Emit event
        bus.emit_with_correlation(TestEvent {
            action: "test".to_string(),
        }, cid);

        // Wait for processing
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Wait for runtime to complete
        runtime_handle.await.unwrap().unwrap();

        assert_eq!(call_count.load(Ordering::Relaxed), 1);
    }
}
