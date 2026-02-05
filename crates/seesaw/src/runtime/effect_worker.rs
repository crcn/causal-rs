//! Effect Worker - polls and executes queued effects

use anyhow::Result;
use parking_lot::{Mutex, RwLock};
use std::any::{Any, TypeId};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{sleep, timeout};
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::effect::{EffectContext, EventEmitter};
use crate::effect_registry::EffectRegistry;
use crate::reducer_registry::ReducerRegistry;
use crate::task_group::TaskGroup;
use crate::{EmittedEvent, Store, NAMESPACE_SEESAW};

struct BufferedEmitter<S, D>
where
    S: Clone + Send + Sync + 'static,
    D: Send + Sync + 'static,
{
    emissions: Arc<Mutex<Vec<(TypeId, Arc<dyn Any + Send + Sync>)>>>,
    _marker: std::marker::PhantomData<(S, D)>,
}

impl<S, D> EventEmitter<S, D> for BufferedEmitter<S, D>
where
    S: Clone + Send + Sync + 'static,
    D: Send + Sync + 'static,
{
    fn emit(&self, type_id: TypeId, event: Arc<dyn Any + Send + Sync>, _ctx: EffectContext<S, D>) {
        self.emissions.lock().push((type_id, event));
    }
}

/// Effect worker configuration
#[derive(Debug, Clone)]
pub struct EffectWorkerConfig {
    /// Polling interval when no effects available
    pub poll_interval: Duration,
    /// Default timeout for effect execution
    pub default_timeout: Duration,
}

impl Default for EffectWorkerConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_millis(100),
            default_timeout: Duration::from_secs(30),
        }
    }
}

/// Effect worker - polls and executes queued effects
pub struct EffectWorker<S, D, St>
where
    S: Clone + Send + Sync + serde::Serialize + serde::de::DeserializeOwned + Default + 'static,
    D: Send + Sync + 'static,
    St: Store,
{
    store: Arc<St>,
    deps: Arc<D>,
    reducers: Arc<ReducerRegistry<S>>,
    effects: Arc<EffectRegistry<S, D>>,
    config: EffectWorkerConfig,
    shutdown: Arc<AtomicBool>,
}

impl<S, D, St> EffectWorker<S, D, St>
where
    S: Clone + Send + Sync + serde::Serialize + serde::de::DeserializeOwned + Default + 'static,
    D: Send + Sync + 'static,
    St: Store,
{
    pub(crate) fn new(
        store: Arc<St>,
        deps: Arc<D>,
        reducers: Arc<ReducerRegistry<S>>,
        effects: Arc<EffectRegistry<S, D>>,
        config: EffectWorkerConfig,
    ) -> Self {
        Self {
            store,
            deps,
            reducers,
            effects,
            config,
            shutdown: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn with_shutdown(self, shutdown: Arc<AtomicBool>) -> Self {
        Self { shutdown, ..self }
    }

    /// Run worker loop
    pub async fn run(self) -> Result<()> {
        info!("Effect worker started");

        while !self.shutdown.load(Ordering::SeqCst) {
            match self.process_next_effect().await {
                Ok(processed) => {
                    if !processed {
                        // No effects available, sleep briefly
                        sleep(self.config.poll_interval).await;
                    }
                }
                Err(e) => {
                    error!("Error processing effect: {}", e);
                    sleep(self.config.poll_interval).await;
                }
            }
        }

        info!("Effect worker stopped");
        Ok(())
    }

    /// Process next available effect
    ///
    /// Returns true if effect was processed, false if no effects available
    async fn process_next_effect(&self) -> Result<bool> {
        // Poll next ready effect (priority-based)
        let Some(execution) = self.store.poll_next_effect().await? else {
            return Ok(false);
        };

        info!(
            "Processing effect: effect_id={}, saga={}, priority={}, attempt={}/{}",
            execution.effect_id,
            execution.correlation_id,
            execution.priority,
            execution.attempts,
            execution.max_attempts
        );

        // Find effect handler by stable ID
        let Some(effect) = self.effects.find_by_id(&execution.effect_id) else {
            let error = format!("No effect handler registered for id '{}'", execution.effect_id);
            warn!("{}", error);
            if execution.attempts >= execution.max_attempts {
                self.store
                    .dlq_effect(
                        execution.event_id,
                        execution.effect_id,
                        error,
                        "missing_handler".to_string(),
                        execution.attempts,
                    )
                    .await?;
            } else {
                self.store
                    .fail_effect(
                        execution.event_id,
                        execution.effect_id,
                        error,
                        execution.attempts,
                    )
                    .await?;
            }
            return Ok(true);
        };

        let (typed_event, type_id) =
            self.decode_event(&execution.event_type, &execution.event_payload)?;
        let state: S = self
            .store
            .load_state(execution.correlation_id)
            .await?
            .map(|(state, _version)| state)
            .unwrap_or_default();
        let emissions: Arc<Mutex<Vec<(TypeId, Arc<dyn Any + Send + Sync>)>>> =
            Arc::new(Mutex::new(Vec::new()));
        let tasks = TaskGroup::new();
        let emitter = Arc::new(BufferedEmitter::<S, D> {
            emissions: emissions.clone(),
            _marker: std::marker::PhantomData,
        });
        let idempotency_key = Uuid::new_v5(
            &NAMESPACE_SEESAW,
            format!("{}-{}", execution.event_id, execution.effect_id).as_bytes(),
        )
        .to_string();
        let live_state = Arc::new(RwLock::new(state.clone()));
        let ctx = EffectContext::new(
            effect.id.clone(),
            idempotency_key,
            execution.correlation_id,
            execution.event_id,
            Arc::new(state.clone()),
            Arc::new(state),
            live_state,
            self.deps.clone(),
            emitter,
            tasks.clone(),
        );

        // Execute effect with timeout
        let timeout_duration = if execution.timeout_seconds > 0 {
            Duration::from_secs(execution.timeout_seconds as u64)
        } else {
            self.config.default_timeout
        };

        let result = timeout(timeout_duration, async {
            let mut emitted = Vec::new();
            if let Some(output) = effect
                .call_handler(typed_event, type_id, ctx.clone())
                .await?
            {
                emitted.push((output.type_id, output.value));
            }

            tasks.wait_pending().await;
            emitted.extend(emissions.lock().drain(..));
            Ok::<Vec<(TypeId, Arc<dyn Any + Send + Sync>)>, anyhow::Error>(emitted)
        })
        .await;

        match result {
            Ok(Ok(emitted_raw)) => {
                // Success - mark as completed
                info!("Effect completed successfully: {}", execution.effect_id);

                let emitted_events = self.serialize_emitted_events(emitted_raw)?;
                let result_value = serde_json::json!({ "status": "ok" });

                if emitted_events.is_empty() {
                    self.store
                        .complete_effect(execution.event_id, execution.effect_id.clone(), result_value)
                        .await?;
                } else {
                    self.store
                        .complete_effect_with_events(
                            execution.event_id,
                            execution.effect_id.clone(),
                            result_value,
                            emitted_events,
                        )
                        .await?;
                }
            }
            Ok(Err(e)) => {
                // Effect failed
                warn!(
                    "Effect failed: {} (attempt {}/{}): {}",
                    execution.effect_id, execution.attempts, execution.max_attempts, e
                );

                if execution.attempts >= execution.max_attempts {
                    // Permanently failed - move to DLQ
                    error!(
                        "Effect exceeded max attempts, moving to DLQ: {}",
                        execution.effect_id
                    );

                    self.store
                        .dlq_effect(
                            execution.event_id,
                            execution.effect_id,
                            e.to_string(),
                            "failed".to_string(),
                            execution.attempts,
                        )
                        .await?;
                } else {
                    // Mark as failed, will be retried
                    self.store
                        .fail_effect(
                            execution.event_id,
                            execution.effect_id,
                            e.to_string(),
                            execution.attempts,
                        )
                        .await?;
                }
            }
            Err(_) => {
                // Timeout
                warn!("Effect timed out: {}", execution.effect_id);

                if execution.attempts >= execution.max_attempts {
                    // Permanently failed - move to DLQ
                    self.store
                        .dlq_effect(
                            execution.event_id,
                            execution.effect_id,
                            "Effect execution timed out".to_string(),
                            "timeout".to_string(),
                            execution.attempts,
                        )
                        .await?;
                } else {
                    // Mark as failed, will be retried
                    self.store
                        .fail_effect(
                            execution.event_id,
                            execution.effect_id,
                            "Timeout".to_string(),
                            execution.attempts,
                        )
                        .await?;
                }
            }
        }

        Ok(true)
    }

    fn decode_event(
        &self,
        event_type: &str,
        payload: &serde_json::Value,
    ) -> Result<(Arc<dyn Any + Send + Sync>, TypeId)> {
        let codec = self
            .effects
            .find_codec_by_event_type(event_type)
            .or_else(|| self.reducers.find_codec_by_event_type(event_type));

        if let Some(codec) = codec {
            let typed = (codec.decode)(payload)?;
            Ok((typed, codec.type_id))
        } else {
            Ok((Arc::new(payload.clone()), TypeId::of::<serde_json::Value>()))
        }
    }

    fn serialize_emitted_events(
        &self,
        emitted: Vec<(TypeId, Arc<dyn Any + Send + Sync>)>,
    ) -> Result<Vec<EmittedEvent>> {
        let mut result = Vec::with_capacity(emitted.len());
        for (type_id, event_any) in emitted {
            let codec = self
                .effects
                .find_codec_by_type_id(type_id)
                .or_else(|| self.reducers.find_codec_by_type_id(type_id))
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "No queue codec registered for emitted event TypeId {:?}",
                        type_id
                    )
                })?;
            let payload = (codec.encode)(event_any.as_ref()).ok_or_else(|| {
                anyhow::anyhow!("Failed to serialize emitted event {}", codec.event_type)
            })?;
            result.push(EmittedEvent {
                event_type: codec.event_type.clone(),
                payload,
            });
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use futures::Stream;
    use parking_lot::Mutex;
    use std::collections::VecDeque;
    use std::sync::Arc;

    #[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
    struct TestState;

    #[derive(Clone, Default)]
    struct TestDeps;

    #[derive(Clone, serde::Serialize, serde::Deserialize)]
    struct Increment {
        amount: i32,
    }

    #[derive(Clone, serde::Serialize, serde::Deserialize)]
    struct Incremented {
        amount: i32,
    }

    struct TestStore {
        queued_effects: Mutex<VecDeque<crate::QueuedEffectExecution>>,
        completed: Mutex<Vec<(Uuid, String)>>,
        completed_with_events: Mutex<Vec<(Uuid, String, Vec<crate::EmittedEvent>)>>,
        failed: Mutex<Vec<(Uuid, String)>>,
        dlqed: Mutex<Vec<(Uuid, String)>>,
    }

    impl TestStore {
        fn new(effects: Vec<crate::QueuedEffectExecution>) -> Self {
            Self {
                queued_effects: Mutex::new(effects.into()),
                completed: Mutex::new(Vec::new()),
                completed_with_events: Mutex::new(Vec::new()),
                failed: Mutex::new(Vec::new()),
                dlqed: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl crate::Store for TestStore {
        async fn publish(&self, _event: crate::QueuedEvent) -> Result<()> {
            Ok(())
        }

        async fn poll_next(&self) -> Result<Option<crate::QueuedEvent>> {
            Ok(None)
        }

        async fn ack(&self, _id: i64) -> Result<()> {
            Ok(())
        }

        async fn nack(&self, _id: i64, _retry_after_secs: u64) -> Result<()> {
            Ok(())
        }

        async fn load_state<S>(&self, _correlation_id: Uuid) -> Result<Option<(S, i32)>>
        where
            S: for<'de> serde::Deserialize<'de> + Send,
        {
            Ok(None)
        }

        async fn save_state<S>(
            &self,
            _correlation_id: Uuid,
            _state: &S,
            _expected_version: i32,
        ) -> Result<i32>
        where
            S: serde::Serialize + Send + Sync,
        {
            Ok(1)
        }

        async fn insert_effect_intent(
            &self,
            _event_id: Uuid,
            _effect_id: String,
            _correlation_id: Uuid,
            _event_type: String,
            _event_payload: serde_json::Value,
            _parent_event_id: Option<Uuid>,
            _execute_at: chrono::DateTime<chrono::Utc>,
            _timeout_seconds: i32,
            _max_attempts: i32,
            _priority: i32,
        ) -> Result<()> {
            Ok(())
        }

        async fn poll_next_effect(&self) -> Result<Option<crate::QueuedEffectExecution>> {
            Ok(self.queued_effects.lock().pop_front())
        }

        async fn complete_effect(
            &self,
            event_id: Uuid,
            effect_id: String,
            _result: serde_json::Value,
        ) -> Result<()> {
            self.completed.lock().push((event_id, effect_id));
            Ok(())
        }

        async fn complete_effect_with_events(
            &self,
            event_id: Uuid,
            effect_id: String,
            _result: serde_json::Value,
            emitted_events: Vec<crate::EmittedEvent>,
        ) -> Result<()> {
            self.completed_with_events
                .lock()
                .push((event_id, effect_id, emitted_events));
            Ok(())
        }

        async fn fail_effect(
            &self,
            event_id: Uuid,
            effect_id: String,
            _error: String,
            _attempts: i32,
        ) -> Result<()> {
            self.failed.lock().push((event_id, effect_id));
            Ok(())
        }

        async fn dlq_effect(
            &self,
            event_id: Uuid,
            effect_id: String,
            _error: String,
            _reason: String,
            _attempts: i32,
        ) -> Result<()> {
            self.dlqed.lock().push((event_id, effect_id));
            Ok(())
        }

        async fn subscribe_saga_events(
            &self,
            _correlation_id: Uuid,
        ) -> Result<Box<dyn Stream<Item = crate::SagaEvent> + Send + Unpin>> {
            Ok(Box::new(futures::stream::empty::<crate::SagaEvent>()))
        }
    }

    fn queued_execution(effect_id: String) -> crate::QueuedEffectExecution {
        crate::QueuedEffectExecution {
            event_id: Uuid::new_v4(),
            effect_id,
            correlation_id: Uuid::new_v4(),
            event_type: std::any::type_name::<Increment>().to_string(),
            event_payload: serde_json::json!({ "amount": 2 }),
            parent_event_id: None,
            execute_at: chrono::Utc::now(),
            timeout_seconds: 1,
            max_attempts: 3,
            priority: 10,
            attempts: 1,
        }
    }

    #[tokio::test]
    async fn effect_worker_executes_and_completes_with_emitted_events() {
        let reducers = Arc::new(ReducerRegistry::new());
        reducers.register(
            crate::reducer::fold::<Incremented>()
                .into_queue(|state: TestState, _event| state),
        );

        let effect = crate::effect::on::<Increment>().then_queue(
            |event: Arc<Increment>, _ctx: EffectContext<TestState, TestDeps>| async move {
                Ok(Incremented {
                    amount: event.amount + 1,
                })
            },
        );
        let effect_id = effect.id.clone();

        let effects = Arc::new(EffectRegistry::new());
        effects.register(effect);

        let execution = queued_execution(effect_id.clone());
        let store = Arc::new(TestStore::new(vec![execution.clone()]));

        let worker = EffectWorker::new(
            store.clone(),
            Arc::new(TestDeps),
            reducers,
            effects,
            EffectWorkerConfig::default(),
        );

        let processed = worker.process_next_effect().await.expect("should process");
        assert!(processed);

        let completed_with_events = store.completed_with_events.lock();
        assert_eq!(completed_with_events.len(), 1);
        let (_event_id, completed_effect_id, emitted) = &completed_with_events[0];
        assert_eq!(completed_effect_id, &effect_id);
        assert_eq!(emitted.len(), 1);
        assert_eq!(
            emitted[0].event_type,
            std::any::type_name::<Incremented>().to_string()
        );
    }

    #[tokio::test]
    async fn effect_worker_dlqs_when_handler_missing_and_attempts_exhausted() {
        let reducers = Arc::new(ReducerRegistry::new());
        let effects = Arc::new(EffectRegistry::<TestState, TestDeps>::new());
        let mut execution = queued_execution("missing_effect_id".to_string());
        execution.attempts = execution.max_attempts;

        let store = Arc::new(TestStore::new(vec![execution]));
        let worker = EffectWorker::new(
            store.clone(),
            Arc::new(TestDeps),
            reducers,
            effects,
            EffectWorkerConfig::default(),
        );

        let processed = worker.process_next_effect().await.expect("should process");
        assert!(processed);
        assert_eq!(store.dlqed.lock().len(), 1);
        assert!(store.failed.lock().is_empty());
    }
}
