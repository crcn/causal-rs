//! Service layer for action execution on top of seesaw Engine.
//!
//! Provides a higher-level abstraction for domain-driven design patterns.

pub mod action;
mod error;
pub mod into_action;

use std::future::Future;
use std::sync::Arc;

use anyhow::Result;

use crate::effect::Effect;
use crate::effect_registry::EffectRegistry;
use crate::{EffectContext, Engine};

pub use action::{ActionResult, EmptyResult, GenericActionResult};
pub use error::{Result as ServiceResult, ServiceError};
pub use into_action::{ActionWithOpts, IntoAction};

/// Service provides action execution using seesaw Engine.
///
/// # Example
///
/// ```ignore
/// use seesaw::{Service, effect, GenericActionResult, IntoAction};
///
/// let service: Service<MyState, MyKernel> = Service::new(kernel)
///     .with_effect(effect::on::<MyEvent>().run(handle_my_event));
///
/// let result = service.run(MyState::default(), my_action.with(opts)).await?;
/// ```
pub struct Service<State, Kernel>
where
    State: Clone + Send + Sync + 'static,
    Kernel: Clone + Send + Sync + 'static,
{
    kernel: Kernel,
    registry: Arc<EffectRegistry<State, Kernel>>,
}

impl<State, Kernel> Service<State, Kernel>
where
    State: Clone + Send + Sync + 'static,
    Kernel: Clone + Send + Sync + 'static,
{
    /// Create a new service with kernel.
    pub fn new(kernel: Kernel) -> Self {
        Self {
            kernel,
            registry: Arc::new(EffectRegistry::new()),
        }
    }

    /// Add an effect handler.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use seesaw::effect;
    ///
    /// service.with_effect(effect::on::<MyEvent>().run(handle_my_event))
    /// ```
    pub fn with_effect(self, effect: Effect<State, Kernel>) -> Self {
        self.registry.register(effect);
        self
    }

    /// Execute an action using the `.with()` pattern.
    pub async fn run<F, Fut, Opts, Ret, Data>(
        &self,
        initial_state: State,
        action: ActionWithOpts<F, Opts>,
    ) -> Result<Data>
    where
        F: FnOnce(Opts, EffectContext<State, Kernel>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<Ret>> + Send + 'static,
        Opts: Send + 'static,
        Ret: ActionResult<Data> + Send + Sync + 'static,
        Data: Send + Sync + 'static,
    {
        let store = Engine::with_deps(self.kernel.clone())
            .with_effect_registry(self.registry.clone());

        let session = store.activate(initial_state);
        let ret = (action.action)(action.opts, session.context.clone()).await?;
        session.settled().await?;
        ret.read().await
    }
}

impl<State, Kernel> Clone for Service<State, Kernel>
where
    State: Clone + Send + Sync + 'static,
    Kernel: Clone + Send + Sync + 'static,
{
    fn clone(&self) -> Self {
        Self {
            kernel: self.kernel.clone(),
            registry: self.registry.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effect;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone, Default)]
    struct TestState {
        #[allow(dead_code)]
        value: i32,
    }

    #[derive(Clone, Default)]
    struct TestKernel;

    // =========================================================================
    // SERVICE ERROR TESTS
    // =========================================================================

    #[tokio::test]
    async fn action_function_error_propagates() {
        // Test: Error returned from action function propagates through run()
        let service: Service<TestState, TestKernel> = Service::new(TestKernel);

        async fn failing_action(
            _opts: (),
            _ctx: EffectContext<TestState, TestKernel>,
        ) -> Result<EmptyResult> {
            Err(anyhow::anyhow!("action function error"))
        }

        let result = service
            .run(TestState::default(), failing_action.with(()))
            .await;

        assert!(result.is_err(), "Action error should propagate");
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("action function error"),
            "Error should be from action function"
        );
    }

    #[tokio::test]
    async fn action_result_read_error_propagates() {
        // Test: Error from ActionResult::read() propagates through run()
        struct FailingResult;

        #[async_trait::async_trait]
        impl ActionResult<String> for FailingResult {
            async fn read(&self) -> Result<String> {
                Err(anyhow::anyhow!("read error"))
            }
        }

        let service: Service<TestState, TestKernel> = Service::new(TestKernel);

        async fn action_with_failing_result(
            _opts: (),
            _ctx: EffectContext<TestState, TestKernel>,
        ) -> Result<FailingResult> {
            Ok(FailingResult)
        }

        let result = service
            .run(TestState::default(), action_with_failing_result.with(()))
            .await;

        assert!(result.is_err(), "Read error should propagate");
        assert!(
            result.unwrap_err().to_string().contains("read error"),
            "Error should be from read()"
        );
    }

    #[tokio::test]
    async fn effect_error_during_action_propagates() {
        // Test: Error from effect during action execution propagates through run()
        #[derive(Clone, Debug)]
        struct TriggerEvent;

        let service: Service<TestState, TestKernel> =
            Service::new(TestKernel).with_effect(effect::on::<TriggerEvent>().run(|_, _| async {
                Err(anyhow::anyhow!("effect error during action"))
            }));

        async fn action_that_emits(
            _opts: (),
            ctx: EffectContext<TestState, TestKernel>,
        ) -> Result<EmptyResult> {
            ctx.emit(TriggerEvent);
            Ok(EmptyResult::default())
        }

        let result = service
            .run(TestState::default(), action_that_emits.with(()))
            .await;

        assert!(result.is_err(), "Effect error should propagate");
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("effect error during action"),
            "Error should be from effect"
        );
    }

    #[tokio::test]
    async fn action_success_with_effect_success() {
        // Test: Successful action with successful effect works correctly
        #[derive(Clone, Debug)]
        struct SuccessEvent;

        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = counter.clone();

        let service: Service<TestState, TestKernel> =
            Service::new(TestKernel).with_effect(effect::on::<SuccessEvent>().run(move |_, _| {
                let c = counter_clone.clone();
                async move {
                    c.fetch_add(1, Ordering::Relaxed);
                    Ok(())
                }
            }));

        async fn successful_action(
            _opts: (),
            ctx: EffectContext<TestState, TestKernel>,
        ) -> Result<GenericActionResult<i32>> {
            ctx.emit(SuccessEvent);
            Ok(GenericActionResult::new(42))
        }

        let result = service
            .run(TestState::default(), successful_action.with(()))
            .await;

        assert!(result.is_ok(), "Should succeed");
        assert_eq!(result.unwrap(), 42, "Should return action result");
        assert_eq!(counter.load(Ordering::Relaxed), 1, "Effect should have run");
    }

    #[tokio::test]
    async fn multiple_effects_one_fails_during_action() {
        // Test: One failing effect among multiple effects propagates error
        #[derive(Clone, Debug)]
        struct MultiEvent;

        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = counter.clone();

        let service: Service<TestState, TestKernel> = Service::new(TestKernel)
            .with_effect(effect::on::<MultiEvent>().run(move |_, _| {
                let c = counter_clone.clone();
                async move {
                    c.fetch_add(1, Ordering::Relaxed);
                    Ok(())
                }
            }))
            .with_effect(
                effect::on::<MultiEvent>()
                    .run(|_, _| async { Err(anyhow::anyhow!("one effect fails")) }),
            );

        async fn emit_multi(
            _opts: (),
            ctx: EffectContext<TestState, TestKernel>,
        ) -> Result<EmptyResult> {
            ctx.emit(MultiEvent);
            Ok(EmptyResult::default())
        }

        let result = service.run(TestState::default(), emit_multi.with(())).await;

        assert!(result.is_err(), "Should fail due to one effect failing");
        // Both effects should have been called
        assert_eq!(
            counter.load(Ordering::Relaxed),
            1,
            "Successful effect should have run"
        );
    }

    #[tokio::test]
    async fn action_error_prevents_effect_processing() {
        // Test: If action function fails immediately, effects may not process
        #[derive(Clone, Debug)]
        struct NeverEmittedEvent;

        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = counter.clone();

        let service: Service<TestState, TestKernel> = Service::new(TestKernel).with_effect(
            effect::on::<NeverEmittedEvent>().run(move |_, _| {
                let c = counter_clone.clone();
                async move {
                    c.fetch_add(1, Ordering::Relaxed);
                    Ok(())
                }
            }),
        );

        async fn immediate_fail(
            _opts: (),
            _ctx: EffectContext<TestState, TestKernel>,
        ) -> Result<EmptyResult> {
            Err(anyhow::anyhow!("immediate failure"))
        }

        let result = service
            .run(TestState::default(), immediate_fail.with(()))
            .await;

        assert!(result.is_err(), "Should fail immediately");
        assert_eq!(
            counter.load(Ordering::Relaxed),
            0,
            "Effect should not have been called"
        );
    }
}
