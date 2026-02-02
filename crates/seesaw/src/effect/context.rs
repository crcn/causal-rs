//! Effect context and related types.

use std::any::{Any, TypeId};
use std::sync::Arc;

use anyhow::Result;
use uuid::Uuid;

use crate::task_group::TaskGroup;

/// Trait for emitting events of any type.
pub trait EventEmitter<S, D>: Send + Sync + 'static
where
    S: Send + Sync + 'static,
    D: Send + Sync + 'static,
{
    fn emit(&self, type_id: TypeId, event: Arc<dyn Any + Send + Sync>, ctx: EffectContext<S, D>);
}

/// A boxed event emitter.
pub type BoxedEmitter<S, D> = Arc<dyn EventEmitter<S, D>>;

/// Envelope wrapping an event with its type and context.
pub struct EventEnvelope<S, D>
where
    S: Send + Sync + 'static,
    D: Send + Sync + 'static,
{
    pub event: Arc<dyn Any + Send + Sync>,
    pub event_type: TypeId,
    pub ctx: EffectContext<S, D>,
    /// Origin stream ID for echo prevention (0 = internal, will be stamped by stream)
    pub origin: u64,
}

/// Context passed to effect handlers.
pub struct EffectContext<S, D>
where
    S: Send + Sync + 'static,
    D: Send + Sync + 'static,
{
    pub(crate) prev_state: Arc<S>,
    pub(crate) state: Arc<S>,
    pub(crate) live_state: Arc<parking_lot::RwLock<S>>,
    pub(crate) deps: Arc<D>,
    pub(crate) emitter: BoxedEmitter<S, D>,
    pub(crate) tasks: Arc<TaskGroup>,
    /// Current event ID for causation tracking in flow visualization
    pub(crate) current_event_id: Option<Uuid>,
}

impl<S, D> Clone for EffectContext<S, D>
where
    S: Send + Sync + 'static,
    D: Send + Sync + 'static,
{
    fn clone(&self) -> Self {
        Self {
            prev_state: self.prev_state.clone(),
            state: self.state.clone(),
            live_state: self.live_state.clone(),
            deps: self.deps.clone(),
            emitter: self.emitter.clone(),
            tasks: self.tasks.clone(),
            current_event_id: self.current_event_id,
        }
    }
}

impl<S, D> EffectContext<S, D>
where
    S: Send + Sync + 'static,
    D: Send + Sync + 'static,
{
    pub(crate) fn new(
        prev_state: Arc<S>,
        state: Arc<S>,
        live_state: Arc<parking_lot::RwLock<S>>,
        deps: Arc<D>,
        emitter: BoxedEmitter<S, D>,
        tasks: Arc<TaskGroup>,
    ) -> Self {
        Self {
            prev_state,
            state,
            live_state,
            deps,
            emitter,
            tasks,
            current_event_id: None,
        }
    }

    /// Get the state before the reducer ran.
    pub fn prev_state(&self) -> &S {
        &self.prev_state
    }

    /// Get the state after the reducer ran (snapshot at event dispatch time).
    pub fn next_state(&self) -> &S {
        &self.state
    }

    /// Get the current live state (reads current value from store).
    /// Use this in long-running background tasks that need fresh state.
    pub fn curr_state(&self) -> S
    where
        S: Clone,
    {
        self.live_state.read().clone()
    }

    /// Get shared dependencies.
    pub fn deps(&self) -> &D {
        &self.deps
    }

    /// Create a new context with updated state snapshots and a new event ID for causation tracking.
    pub(crate) fn with_states_and_event_id(
        &self,
        prev_state: Arc<S>,
        state: Arc<S>,
        event_id: Uuid,
    ) -> Self {
        Self {
            prev_state,
            state,
            live_state: self.live_state.clone(),
            deps: self.deps.clone(),
            emitter: self.emitter.clone(),
            tasks: self.tasks.clone(),
            current_event_id: Some(event_id),
        }
    }

    /// Get the current event ID for causation tracking.
    pub fn current_event_id(&self) -> Option<Uuid> {
        self.current_event_id
    }

    /// Emit a new event to the store.
    /// Events are always handled on the head (foreground) task group,
    /// even when emit() is called from a background context.
    pub fn emit<E: Send + Sync + 'static>(&self, event: E) {
        let type_id = TypeId::of::<E>();
        let event_arc: Arc<dyn Any + Send + Sync> = Arc::new(event);
        let emitter = self.emitter.clone();

        self.with_head_task().within(move |ctx| async move {
            emitter.emit(type_id, event_arc, ctx);
            Ok(())
        });
    }

    /// Emit a type-erased event to the store.
    /// Used for forwarding events from external sources where the type is not known at compile time.
    pub fn emit_any(&self, event: Arc<dyn Any + Send + Sync>, type_id: TypeId) {
        let emitter = self.emitter.clone();

        self.with_head_task().within(move |ctx| async move {
            emitter.emit(type_id, event, ctx);
            Ok(())
        });
    }

    /// Create a context that spawns on the head (foreground) task group.
    fn with_head_task(&self) -> Self {
        Self {
            prev_state: self.prev_state.clone(),
            state: self.state.clone(),
            live_state: self.live_state.clone(),
            deps: self.deps.clone(),
            emitter: self.emitter.clone(),
            tasks: self.tasks.head_or_self(),
            current_event_id: self.current_event_id,
        }
    }

    /// Spawn a tracked sub-task.
    pub fn within<F, Fut>(&self, f: F)
    where
        F: FnOnce(Self) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<()>> + Send + 'static,
    {
        let child_ctx = self.clone();
        self.tasks.spawn(async move { f(child_ctx).await });
    }

    /// Check if the session has been cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.tasks.is_cancelled()
    }

    /// Wait until the session is cancelled.
    /// Returns immediately if already cancelled.
    pub async fn cancelled(&self) {
        self.tasks.cancelled().await
    }

    /// Create a transient context.
    ///
    /// Tasks spawned in the transient context do NOT count toward the
    /// session's `settled()`. Transient tasks are automatically cancelled
    /// when the session settles or is dropped.
    pub fn transient(&self) -> Self {
        Self {
            prev_state: self.prev_state.clone(),
            state: self.state.clone(),
            live_state: self.live_state.clone(),
            deps: self.deps.clone(),
            emitter: self.emitter.clone(),
            tasks: self.tasks.transient(),
            current_event_id: self.current_event_id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_group::TaskGroup;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone, Debug, Default)]
    struct TestState {
        value: i32,
    }

    #[derive(Clone, Debug, Default)]
    struct TestDeps {
        multiplier: i32,
    }

    struct NoopEmitter;

    impl EventEmitter<TestState, TestDeps> for NoopEmitter {
        fn emit(
            &self,
            _type_id: TypeId,
            _event: Arc<dyn Any + Send + Sync>,
            _ctx: EffectContext<TestState, TestDeps>,
        ) {
        }
    }

    struct TestEffectResult {
        tasks: Arc<TaskGroup>,
        context: EffectContext<TestState, TestDeps>,
    }

    impl TestEffectResult {
        async fn settled(&self) -> anyhow::Result<()> {
            self.tasks.settled().await
        }
    }

    fn create_test_context() -> TestEffectResult {
        let state = Arc::new(TestState { value: 42 });
        let live_state = Arc::new(parking_lot::RwLock::new(TestState { value: 42 }));
        let deps = Arc::new(TestDeps { multiplier: 2 });
        let emitter: BoxedEmitter<TestState, TestDeps> = Arc::new(NoopEmitter);
        let tasks = TaskGroup::new();
        let context = EffectContext::new(
            state.clone(),
            state,
            live_state,
            deps,
            emitter,
            tasks.clone(),
        );

        TestEffectResult { tasks, context }
    }

    #[tokio::test]
    async fn test_effect_context_state_access() {
        let result = create_test_context();

        assert_eq!(result.context.next_state().value, 42);
        assert_eq!(result.context.curr_state().value, 42);
        assert_eq!(result.context.deps().multiplier, 2);
    }

    #[tokio::test]
    async fn test_effect_context_clone() {
        let result = create_test_context();
        let cloned = result.context.clone();

        assert_eq!(cloned.next_state().value, 42);
        assert_eq!(cloned.deps().multiplier, 2);
    }

    #[tokio::test]
    async fn test_effect_context_within() {
        let result = create_test_context();
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = counter.clone();

        result.context.within(move |_child_ctx| async move {
            counter_clone.fetch_add(1, Ordering::Relaxed);
            Ok(())
        });

        result.settled().await.unwrap();
        assert_eq!(counter.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn test_effect_context_error_propagation() {
        let result = create_test_context();

        result
            .context
            .within(|_| async { Err::<(), _>(anyhow::anyhow!("test error")) });

        let err = result.settled().await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn test_effect_context_is_cancelled() {
        let result = create_test_context();

        assert!(
            !result.context.is_cancelled(),
            "Should not be cancelled initially"
        );

        result.tasks.cancel();

        assert!(
            result.context.is_cancelled(),
            "Should be cancelled after cancel()"
        );
    }
}
