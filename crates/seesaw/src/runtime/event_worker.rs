//! Event Worker - polls events and executes reducers + inline effects

use anyhow::Result;
use parking_lot::{Mutex, RwLock};
use std::any::{Any, TypeId};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::effect::{EffectContext, EventEmitter};
use crate::effect_registry::EffectRegistry;
use crate::queue_backend::{QueueBackend, StoreQueueBackend};
use crate::reducer_registry::ReducerRegistry;
use crate::{
    EventProcessingCommit, InlineEffectFailure, QueuedEffectIntent, QueuedEvent, Store,
    NAMESPACE_SEESAW,
};

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

/// Event worker configuration
#[derive(Debug, Clone)]
pub struct EventWorkerConfig {
    /// Polling interval when no events available
    pub poll_interval: Duration,
    /// Maximum hop count before DLQ (infinite loop detection)
    pub max_hops: i32,
    /// Maximum number of events an effect may emit in one batch
    pub max_batch_size: usize,
    /// Maximum retry count for event-level failures in inline processing path
    pub max_inline_retry_attempts: i32,
}

impl Default for EventWorkerConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_millis(100),
            max_hops: 50,
            max_batch_size: 10_000,
            max_inline_retry_attempts: 3,
        }
    }
}

/// Event worker - polls and processes events
pub struct EventWorker<S, D, St>
where
    S: Clone + Send + Sync + serde::Serialize + serde::de::DeserializeOwned + Default + 'static,
    D: Send + Sync + 'static,
    St: Store,
{
    store: Arc<St>,
    deps: Arc<D>,
    reducers: Arc<ReducerRegistry<S>>,
    effects: Arc<EffectRegistry<S, D>>,
    queue_backend: Arc<dyn QueueBackend<St>>,
    config: EventWorkerConfig,
    shutdown: Arc<AtomicBool>,
}

impl<S, D, St> EventWorker<S, D, St>
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
        config: EventWorkerConfig,
    ) -> Self {
        Self {
            store,
            deps,
            reducers,
            effects,
            queue_backend: Arc::new(StoreQueueBackend),
            config,
            shutdown: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn with_queue_backend(self, queue_backend: Arc<dyn QueueBackend<St>>) -> Self {
        Self {
            queue_backend,
            ..self
        }
    }

    pub(crate) fn with_shutdown(self, shutdown: Arc<AtomicBool>) -> Self {
        Self { shutdown, ..self }
    }

    /// Run worker loop (polls events and processes them)
    pub async fn run(self) -> Result<()> {
        info!("Event worker started");

        while !self.shutdown.load(Ordering::SeqCst) {
            match self.process_next_event().await {
                Ok(processed) => {
                    if !processed {
                        // No events available, sleep briefly
                        sleep(self.config.poll_interval).await;
                    }
                }
                Err(e) => {
                    error!("Error processing event: {}", e);
                    sleep(self.config.poll_interval).await;
                }
            }
        }

        info!("Event worker stopped");
        Ok(())
    }

    /// Process next available event
    ///
    /// Returns true if event was processed, false if no events available
    async fn process_next_event(&self) -> Result<bool> {
        // Poll next event (per-workflow FIFO with advisory lock)
        let Some(event) = self.store.poll_next().await? else {
            return Ok(false);
        };

        if let Err(error) = self.process_claimed_event(&event).await {
            warn!(
                "Event processing failed, nacking for retry: event_id={}, error={}",
                event.event_id, error
            );
            let _ = self.store.nack(event.id, 1).await;
            return Err(error);
        }

        Ok(true)
    }

    async fn process_claimed_event(&self, event: &QueuedEvent) -> Result<()> {
        let (typed_event, event_type_id) = self.decode_event(&event.event_type, &event.payload)?;

        info!(
            "Processing event: type={}, workflow={}, hops={}",
            event.event_type, event.correlation_id, event.hops
        );

        // Check for infinite loops
        if event.hops >= self.config.max_hops {
            warn!(
                "Event exceeded max hops ({}), sending to DLQ: event_id={}",
                self.config.max_hops, event.event_id
            );
            // DLQ the event using a synthetic effect_id to record the failure
            let error = format!(
                "Event exceeded maximum hop count ({}) - infinite loop detected",
                self.config.max_hops
            );
            self.store
                .dlq_effect(
                    event.event_id,
                    "__event_max_hops__".to_string(),
                    error,
                    "infinite_loop".to_string(),
                    event.hops,
                )
                .await?;
            self.store.ack(event.id).await?;
            return Ok(());
        }

        if event.retry_count >= self.config.max_inline_retry_attempts {
            warn!(
                "Event exceeded max retry attempts ({}), sending to DLQ: event_id={}",
                self.config.max_inline_retry_attempts, event.event_id
            );
            let error = format!("Event failed after {} retry attempts", event.retry_count);
            self.store
                .dlq_effect(
                    event.event_id,
                    "__inline_effect_retry_exhausted__".to_string(),
                    error,
                    "max_retries_exceeded".to_string(),
                    event.retry_count,
                )
                .await?;
            self.store.ack(event.id).await?;
            return Ok(());
        }

        // Load current state
        let (state, version): (S, i32) = self
            .store
            .load_state(event.correlation_id)
            .await?
            .unwrap_or_else(|| {
                // No state yet, use Default
                (S::default(), 0)
            });

        // Run reducers (pure state transformations)
        let prev_state = state.clone();
        let next_state = self
            .reducers
            .apply(state, event_type_id, typed_event.as_ref());

        // Gather effects once so we can run in two phases:
        // 1) insert all queued intents, then
        // 2) execute inline effects. This avoids registration-order coupling where an
        // early inline failure could prevent queued intent insertion.
        let matching_effects: Vec<_> = self
            .effects
            .all()
            .into_iter()
            .filter(|effect| effect.can_handle(event_type_id))
            .collect();

        // Phase 1: build all queued effect intents.
        let mut queued_effect_intents = Vec::new();
        for effect in matching_effects.iter().filter(|effect| !effect.is_inline()) {
            let execute_at = match effect.delay {
                Some(delay) => {
                    chrono::Utc::now()
                        + chrono::Duration::from_std(delay)
                            .map_err(|_| anyhow::anyhow!("invalid queued effect delay"))?
                }
                None => chrono::Utc::now(),
            };
            let timeout_seconds = effect
                .timeout
                .map(|d| d.as_secs() as i32)
                .unwrap_or(30)
                .max(1);
            queued_effect_intents.push(QueuedEffectIntent {
                effect_id: effect.id.clone(),
                parent_event_id: Some(event.event_id),
                batch_id: event.batch_id,
                batch_index: event.batch_index,
                batch_size: event.batch_size,
                execute_at,
                timeout_seconds,
                max_attempts: effect.max_attempts as i32,
                priority: effect.priority.unwrap_or(10),
            });
        }

        // Phase 2: execute inline effects and collect outputs/failures to persist atomically.
        let mut inline_effect_failures = Vec::new();
        let mut emitted_events = Vec::new();
        for effect in matching_effects.iter().filter(|effect| effect.is_inline()) {
            match self
                .run_inline_effect(
                    effect,
                    event,
                    typed_event.clone(),
                    event_type_id,
                    prev_state.clone(),
                    next_state.clone(),
                )
                .await
            {
                Ok(mut emitted) => emitted_events.append(&mut emitted),
                Err(error) => {
                    let error_string = error.to_string();
                    warn!(
                        "Inline effect failed and will be persisted to DLQ: event_id={}, effect_id={}, error={}",
                        event.event_id, effect.id, error_string
                    );
                    inline_effect_failures.push(InlineEffectFailure {
                        effect_id: effect.id.clone(),
                        error: error_string,
                        reason: "inline_failed".to_string(),
                        attempts: event.retry_count.saturating_add(1),
                    });
                }
            }
        }

        let inline_failure_count = inline_effect_failures.len();

        let queued_effect_ids = queued_effect_intents
            .iter()
            .map(|intent| intent.effect_id.clone())
            .collect::<Vec<_>>();

        let new_version = self
            .store
            .commit_event_processing(EventProcessingCommit {
                event_row_id: event.id,
                event_id: event.event_id,
                correlation_id: event.correlation_id,
                event_type: event.event_type.clone(),
                event_payload: event.payload.clone(),
                state: next_state,
                expected_state_version: version,
                queued_effect_intents,
                inline_effect_failures,
                emitted_events,
            })
            .await?;

        info!(
            "State updated: workflow={}, version={} -> {}",
            event.correlation_id, version, new_version
        );

        // Backend notification is best-effort. The source of truth remains the store,
        // and effect workers can always fall back to store polling.
        for effect_id in queued_effect_ids {
            if let Err(error) = self
                .queue_backend
                .on_effect_intent_inserted(&*self.store, event.event_id, &effect_id)
                .await
            {
                warn!(
                    "Queue backend notification failed; relying on store polling fallback: event_id={}, effect_id={}, error={}",
                    event.event_id, effect_id, error
                );
            }
        }

        if inline_failure_count > 0 {
            warn!(
                "Event processed with inline failures moved to DLQ: event_id={}, inline_failures={}",
                event.event_id, inline_failure_count
            );
        } else {
            info!("Event processed successfully: event_id={}", event.event_id);
        }

        Ok(())
    }

    fn decode_event(
        &self,
        event_type: &str,
        payload: &serde_json::Value,
    ) -> Result<(Arc<dyn Any + Send + Sync>, TypeId)> {
        let codec = self
            .reducers
            .find_codec_by_event_type(event_type)
            .or_else(|| self.effects.find_codec_by_event_type(event_type));

        if let Some(codec) = codec {
            let typed = (codec.decode)(payload)?;
            Ok((typed, codec.type_id))
        } else {
            // Fallback for queue-unaware handlers: expose raw JSON payload.
            Ok((Arc::new(payload.clone()), TypeId::of::<serde_json::Value>()))
        }
    }

    async fn run_inline_effect(
        &self,
        effect: &crate::effect::Effect<S, D>,
        source_event: &QueuedEvent,
        typed_event: Arc<dyn Any + Send + Sync>,
        event_type_id: TypeId,
        prev_state: S,
        next_state: S,
    ) -> Result<Vec<QueuedEvent>> {
        let emissions: Arc<Mutex<Vec<(TypeId, Arc<dyn Any + Send + Sync>)>>> =
            Arc::new(Mutex::new(Vec::new()));
        let emitter = Arc::new(BufferedEmitter::<S, D> {
            emissions: emissions.clone(),
            _marker: std::marker::PhantomData,
        });
        let live_state = Arc::new(RwLock::new(next_state.clone()));
        let idempotency_key = Uuid::new_v5(
            &NAMESPACE_SEESAW,
            format!("{}-{}", source_event.event_id, effect.id).as_bytes(),
        )
        .to_string();
        let ctx = EffectContext::new(
            effect.id.clone(),
            idempotency_key,
            source_event.correlation_id,
            source_event.event_id,
            Arc::new(prev_state),
            Arc::new(next_state),
            live_state,
            self.deps.clone(),
            emitter,
        );

        for output in effect
            .call_handler(typed_event, event_type_id, ctx.clone())
            .await?
        {
            emissions.lock().push((output.type_id, output.value));
        }

        // tasks.wait_pending().await; // TODO: Re-enable when tasks tracking is implemented
        let drained = emissions.lock().drain(..).collect::<Vec<_>>();
        let emitted_count = drained.len();
        if emitted_count > self.config.max_batch_size {
            anyhow::bail!(
                "inline effect '{}' emitted {} events, exceeding max_batch_size {}",
                effect.id,
                emitted_count,
                self.config.max_batch_size
            );
        }
        if emitted_count > i32::MAX as usize {
            anyhow::bail!(
                "inline effect '{}' emitted {} events, exceeding i32 batch metadata capacity",
                effect.id,
                emitted_count
            );
        }
        let inherited_batch = if emitted_count == 1 {
            match (
                source_event.batch_id,
                source_event.batch_index,
                source_event.batch_size,
            ) {
                (Some(batch_id), Some(batch_index), Some(batch_size)) => {
                    if batch_size <= 0
                        || batch_index < 0
                        || batch_index >= batch_size
                        || batch_size as usize > self.config.max_batch_size
                    {
                        anyhow::bail!(
                            "invalid inherited batch metadata: id={} index={} size={} max_batch_size={}",
                            batch_id,
                            batch_index,
                            batch_size,
                            self.config.max_batch_size
                        );
                    }
                    Some((batch_id, batch_index, batch_size))
                }
                _ => None,
            }
        } else {
            None
        };
        let emitted_batch_id = if emitted_count > 1 {
            Some(Uuid::new_v5(
                &NAMESPACE_SEESAW,
                format!("{}-{}-batch", source_event.event_id, effect.id).as_bytes(),
            ))
        } else {
            None
        };

        let mut emitted_events = Vec::with_capacity(emitted_count);

        for (emitted_index, (type_id, event_any)) in drained.into_iter().enumerate() {
            let codec = self
                .reducers
                .find_codec_by_type_id(type_id)
                .or_else(|| self.effects.find_codec_by_type_id(type_id))
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "No queue codec registered for emitted event TypeId {:?}",
                        type_id
                    )
                })?;
            let payload = (codec.encode)(event_any.as_ref()).ok_or_else(|| {
                anyhow::anyhow!("Failed to serialize emitted event {}", codec.event_type)
            })?;
            let event_id = Uuid::new_v5(
                &NAMESPACE_SEESAW,
                format!(
                    "{}-{}-{}-{}",
                    source_event.event_id, effect.id, codec.event_type, emitted_index
                )
                .as_bytes(),
            );
            let created_at = source_event
                .created_at
                .date_naive()
                .and_hms_opt(0, 0, 0)
                .expect("midnight UTC should always be valid")
                .and_utc();
            let (batch_id, batch_index, batch_size) = if let Some(inherited) = inherited_batch {
                (Some(inherited.0), Some(inherited.1), Some(inherited.2))
            } else if emitted_count > 1 {
                (
                    emitted_batch_id,
                    Some(emitted_index as i32),
                    Some(emitted_count as i32),
                )
            } else {
                (None, None, None)
            };

            emitted_events.push(QueuedEvent {
                id: 0,
                event_id,
                parent_id: Some(source_event.event_id),
                correlation_id: source_event.correlation_id,
                event_type: codec.event_type.clone(),
                payload,
                hops: source_event.hops + 1,
                retry_count: 0,
                batch_id,
                batch_index,
                batch_size,
                created_at,
            });
        }

        Ok(emitted_events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use futures::Stream;
    use parking_lot::Mutex;
    use std::collections::{HashSet, VecDeque};
    use std::sync::Arc;

    #[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
    struct TestState {
        count: i32,
    }

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

    #[derive(Clone, serde::Serialize, serde::Deserialize)]
    struct EffectOnlyEvent {
        id: i32,
    }

    #[derive(Clone, serde::Serialize, serde::Deserialize)]
    struct FanOut {
        count: usize,
    }

    #[derive(Clone, serde::Serialize, serde::Deserialize)]
    struct FanOutItem {
        index: i32,
    }

    #[derive(Clone, serde::Serialize, serde::Deserialize)]
    struct BatchCarry {
        marker: String,
    }

    struct TestStore {
        queued_events: Mutex<VecDeque<QueuedEvent>>,
        acked_ids: Mutex<Vec<i64>>,
        nacked_ids: Mutex<Vec<i64>>,
        dlqed: Mutex<Vec<(Uuid, String, String, String, i32)>>,
        saved_states: Mutex<Vec<serde_json::Value>>,
        effect_intents: Mutex<Vec<(String, Option<Uuid>, Option<i32>, Option<i32>)>>,
        published_events: Mutex<Vec<QueuedEvent>>,
    }

    impl TestStore {
        fn new(queued_events: Vec<QueuedEvent>) -> Self {
            Self {
                queued_events: Mutex::new(queued_events.into()),
                acked_ids: Mutex::new(Vec::new()),
                nacked_ids: Mutex::new(Vec::new()),
                dlqed: Mutex::new(Vec::new()),
                saved_states: Mutex::new(Vec::new()),
                effect_intents: Mutex::new(Vec::new()),
                published_events: Mutex::new(Vec::new()),
            }
        }
    }

    #[derive(Default)]
    struct RecordingQueueBackend {
        inserted_intents: Mutex<Vec<(Uuid, String)>>,
    }

    #[async_trait]
    impl crate::queue_backend::QueueBackend<TestStore> for RecordingQueueBackend {
        async fn on_effect_intent_inserted(
            &self,
            _store: &TestStore,
            event_id: Uuid,
            effect_id: &str,
        ) -> Result<()> {
            self.inserted_intents
                .lock()
                .push((event_id, effect_id.to_string()));
            Ok(())
        }
    }

    struct FailingQueueBackend;

    #[async_trait]
    impl crate::queue_backend::QueueBackend<TestStore> for FailingQueueBackend {
        async fn on_effect_intent_inserted(
            &self,
            _store: &TestStore,
            _event_id: Uuid,
            _effect_id: &str,
        ) -> Result<()> {
            anyhow::bail!("queue backend insert hook failed")
        }
    }

    #[async_trait]
    impl crate::Store for TestStore {
        async fn publish(&self, event: QueuedEvent) -> Result<()> {
            self.published_events.lock().push(event);
            Ok(())
        }

        async fn poll_next(&self) -> Result<Option<QueuedEvent>> {
            Ok(self.queued_events.lock().pop_front())
        }

        async fn ack(&self, id: i64) -> Result<()> {
            self.acked_ids.lock().push(id);
            Ok(())
        }

        async fn nack(&self, id: i64, _retry_after_secs: u64) -> Result<()> {
            self.nacked_ids.lock().push(id);
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
            state: &S,
            _expected_version: i32,
        ) -> Result<i32>
        where
            S: serde::Serialize + Send + Sync,
        {
            self.saved_states.lock().push(serde_json::to_value(state)?);
            Ok(1)
        }

        async fn insert_effect_intent(
            &self,
            _event_id: Uuid,
            effect_id: String,
            _correlation_id: Uuid,
            _event_type: String,
            _event_payload: serde_json::Value,
            _parent_event_id: Option<Uuid>,
            batch_id: Option<Uuid>,
            batch_index: Option<i32>,
            batch_size: Option<i32>,
            _execute_at: chrono::DateTime<chrono::Utc>,
            _timeout_seconds: i32,
            _max_attempts: i32,
            _priority: i32,
        ) -> Result<()> {
            self.effect_intents
                .lock()
                .push((effect_id, batch_id, batch_index, batch_size));
            Ok(())
        }

        async fn poll_next_effect(&self) -> Result<Option<crate::QueuedEffectExecution>> {
            Ok(None)
        }

        async fn complete_effect(
            &self,
            _event_id: Uuid,
            _effect_id: String,
            _result: serde_json::Value,
        ) -> Result<()> {
            Ok(())
        }

        async fn complete_effect_with_events(
            &self,
            _event_id: Uuid,
            _effect_id: String,
            _result: serde_json::Value,
            _emitted_events: Vec<crate::EmittedEvent>,
        ) -> Result<()> {
            Ok(())
        }

        async fn fail_effect(
            &self,
            _event_id: Uuid,
            _effect_id: String,
            _error: String,
            _attempts: i32,
        ) -> Result<()> {
            Ok(())
        }

        async fn dlq_effect(
            &self,
            event_id: Uuid,
            effect_id: String,
            error: String,
            reason: String,
            attempts: i32,
        ) -> Result<()> {
            self.dlqed
                .lock()
                .push((event_id, effect_id, error, reason, attempts));
            Ok(())
        }

        async fn get_workflow_status(
            &self,
            _correlation_id: Uuid,
        ) -> Result<crate::WorkflowStatus> {
            Ok(crate::WorkflowStatus {
                correlation_id: _correlation_id,
                state: None,
                pending_effects: 0,
                is_settled: true,
                last_event: None,
            })
        }

        async fn subscribe_workflow_events(
            &self,
            _correlation_id: Uuid,
        ) -> Result<Box<dyn Stream<Item = crate::WorkflowEvent> + Send + Unpin>> {
            Ok(Box::new(futures::stream::empty::<crate::WorkflowEvent>()))
        }
    }

    fn queued_increment_event() -> QueuedEvent {
        QueuedEvent {
            id: 7,
            event_id: Uuid::new_v4(),
            parent_id: None,
            correlation_id: Uuid::new_v4(),
            event_type: std::any::type_name::<Increment>().to_string(),
            payload: serde_json::json!({ "amount": 2 }),
            hops: 0,
            retry_count: 0,
            batch_id: None,
            batch_index: None,
            batch_size: None,
            created_at: chrono::Utc::now(),
        }
    }

    fn queued_effect_only_event() -> QueuedEvent {
        QueuedEvent {
            id: 8,
            event_id: Uuid::new_v4(),
            parent_id: None,
            correlation_id: Uuid::new_v4(),
            event_type: std::any::type_name::<EffectOnlyEvent>().to_string(),
            payload: serde_json::json!({ "id": 42 }),
            hops: 0,
            retry_count: 0,
            batch_id: None,
            batch_index: None,
            batch_size: None,
            created_at: chrono::Utc::now(),
        }
    }

    struct AtomicCommitStore {
        queued_events: Mutex<VecDeque<QueuedEvent>>,
        commit_calls: Mutex<Vec<(Uuid, Uuid)>>,
        nacked_ids: Mutex<Vec<i64>>,
    }

    impl AtomicCommitStore {
        fn new(event: QueuedEvent) -> Self {
            Self {
                queued_events: Mutex::new(vec![event].into()),
                commit_calls: Mutex::new(Vec::new()),
                nacked_ids: Mutex::new(Vec::new()),
            }
        }
    }

    #[tokio::test]
    async fn event_worker_applies_reducer_and_queues_non_inline_effect() {
        let event = queued_increment_event();
        let store = Arc::new(TestStore::new(vec![event.clone()]));

        let reducers = Arc::new(ReducerRegistry::new());
        reducers.register(crate::reducer::fold::<Increment>().into_queue(
            |state: TestState, event| TestState {
                count: state.count + event.amount,
            },
        ));

        let effects = Arc::new(EffectRegistry::new());
        let queued_effect = crate::effect::on::<Increment>()
            .id("queued_increment_effect")
            .queued()
            .then_queue(
                |_event: Arc<Increment>, _ctx: EffectContext<TestState, TestDeps>| async { Ok(()) },
            );
        effects.register(queued_effect);

        let worker = EventWorker::new(
            store.clone(),
            Arc::new(TestDeps),
            reducers,
            effects,
            EventWorkerConfig::default(),
        );

        let processed = worker
            .process_next_event()
            .await
            .expect("process should succeed");
        assert!(processed);

        let saved = store.saved_states.lock();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0]["count"], serde_json::json!(2));
        assert_eq!(store.effect_intents.lock().len(), 1);
        assert_eq!(*store.acked_ids.lock(), vec![event.id]);
    }

    #[async_trait]
    impl crate::Store for AtomicCommitStore {
        async fn publish(&self, _event: QueuedEvent) -> Result<()> {
            panic!("publish should not be called directly; event worker must use commit_event_processing");
        }

        async fn poll_next(&self) -> Result<Option<QueuedEvent>> {
            Ok(self.queued_events.lock().pop_front())
        }

        async fn ack(&self, _id: i64) -> Result<()> {
            panic!(
                "ack should not be called directly; event worker must use commit_event_processing"
            );
        }

        async fn nack(&self, id: i64, _retry_after_secs: u64) -> Result<()> {
            self.nacked_ids.lock().push(id);
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
            panic!(
                "save_state should not be called directly; event worker must use commit_event_processing"
            );
        }

        async fn commit_event_processing<S>(&self, commit: EventProcessingCommit<S>) -> Result<i32>
        where
            S: serde::Serialize + Send + Sync,
        {
            self.commit_calls
                .lock()
                .push((commit.event_id, commit.correlation_id));
            Ok(commit.expected_state_version + 1)
        }

        async fn insert_effect_intent(
            &self,
            _event_id: Uuid,
            _effect_id: String,
            _correlation_id: Uuid,
            _event_type: String,
            _event_payload: serde_json::Value,
            _parent_event_id: Option<Uuid>,
            _batch_id: Option<Uuid>,
            _batch_index: Option<i32>,
            _batch_size: Option<i32>,
            _execute_at: chrono::DateTime<chrono::Utc>,
            _timeout_seconds: i32,
            _max_attempts: i32,
            _priority: i32,
        ) -> Result<()> {
            panic!(
                "insert_effect_intent should not be called directly; event worker must use commit_event_processing"
            );
        }

        async fn poll_next_effect(&self) -> Result<Option<crate::QueuedEffectExecution>> {
            Ok(None)
        }

        async fn complete_effect(
            &self,
            _event_id: Uuid,
            _effect_id: String,
            _result: serde_json::Value,
        ) -> Result<()> {
            Ok(())
        }

        async fn complete_effect_with_events(
            &self,
            _event_id: Uuid,
            _effect_id: String,
            _result: serde_json::Value,
            _emitted_events: Vec<crate::EmittedEvent>,
        ) -> Result<()> {
            Ok(())
        }

        async fn fail_effect(
            &self,
            _event_id: Uuid,
            _effect_id: String,
            _error: String,
            _attempts: i32,
        ) -> Result<()> {
            Ok(())
        }

        async fn dlq_effect(
            &self,
            _event_id: Uuid,
            _effect_id: String,
            _error: String,
            _reason: String,
            _attempts: i32,
        ) -> Result<()> {
            Ok(())
        }

        async fn get_workflow_status(
            &self,
            _correlation_id: Uuid,
        ) -> Result<crate::WorkflowStatus> {
            Ok(crate::WorkflowStatus {
                correlation_id: _correlation_id,
                state: None,
                pending_effects: 0,
                is_settled: true,
                last_event: None,
            })
        }

        async fn subscribe_workflow_events(
            &self,
            _correlation_id: Uuid,
        ) -> Result<Box<dyn Stream<Item = crate::WorkflowEvent> + Send + Unpin>> {
            Ok(Box::new(futures::stream::empty::<crate::WorkflowEvent>()))
        }
    }

    #[tokio::test]
    async fn event_worker_uses_atomic_commit_path() {
        let event = queued_increment_event();
        let store = Arc::new(AtomicCommitStore::new(event.clone()));

        let reducers = Arc::new(ReducerRegistry::new());
        reducers.register(crate::reducer::fold::<Increment>().into_queue(
            |state: TestState, event| TestState {
                count: state.count + event.amount,
            },
        ));

        let effects = Arc::new(EffectRegistry::<TestState, TestDeps>::new());
        let worker = EventWorker::new(
            store.clone(),
            Arc::new(TestDeps),
            reducers,
            effects,
            EventWorkerConfig::default(),
        );

        let processed = worker
            .process_next_event()
            .await
            .expect("process should succeed");
        assert!(processed);
        assert_eq!(store.commit_calls.lock().len(), 1);
        assert!(store.nacked_ids.lock().is_empty());
    }

    #[tokio::test]
    async fn event_worker_notifies_queue_backend_for_queued_intents() {
        let event = queued_effect_only_event();
        let store = Arc::new(TestStore::new(vec![event.clone()]));
        let reducers = Arc::new(ReducerRegistry::<TestState>::new());

        let effects = Arc::new(EffectRegistry::new());
        effects.register(
            crate::effect::on::<EffectOnlyEvent>()
                .id("queued_effect")
                .queued()
                .then_queue(
                    |_event: Arc<EffectOnlyEvent>, _ctx: EffectContext<TestState, TestDeps>| async {
                        Ok(())
                    },
                ),
        );

        let queue_backend = Arc::new(RecordingQueueBackend::default());
        let worker = EventWorker::new(
            store,
            Arc::new(TestDeps),
            reducers,
            effects,
            EventWorkerConfig::default(),
        )
        .with_queue_backend(queue_backend.clone());

        let processed = worker
            .process_next_event()
            .await
            .expect("process should succeed");
        assert!(processed);

        let inserted = queue_backend.inserted_intents.lock();
        assert_eq!(inserted.len(), 1);
        assert_eq!(inserted[0].0, event.event_id);
        assert_eq!(inserted[0].1, "queued_effect");
    }

    #[tokio::test]
    async fn event_worker_continues_when_queue_backend_insert_hook_fails() {
        let event = queued_effect_only_event();
        let store = Arc::new(TestStore::new(vec![event.clone()]));
        let reducers = Arc::new(ReducerRegistry::<TestState>::new());
        let effects = Arc::new(EffectRegistry::new());
        effects.register(
            crate::effect::on::<EffectOnlyEvent>()
                .id("queued_effect")
                .queued()
                .then_queue(
                    |_event: Arc<EffectOnlyEvent>, _ctx: EffectContext<TestState, TestDeps>| async {
                        Ok(())
                    },
                ),
        );

        let worker = EventWorker::new(
            store.clone(),
            Arc::new(TestDeps),
            reducers,
            effects,
            EventWorkerConfig::default(),
        )
        .with_queue_backend(Arc::new(FailingQueueBackend));

        let processed = worker
            .process_next_event()
            .await
            .expect("queue backend notification failure should not fail event processing");
        assert!(processed);
        assert_eq!(*store.acked_ids.lock(), vec![event.id]);
        assert!(store.nacked_ids.lock().is_empty());
        assert_eq!(store.effect_intents.lock().len(), 1);
    }

    #[tokio::test]
    async fn event_worker_runs_inline_effect_and_publishes_output_event() {
        let source = queued_increment_event();
        let store = Arc::new(TestStore::new(vec![source.clone()]));

        let reducers = Arc::new(ReducerRegistry::new());
        reducers.register(crate::reducer::fold::<Increment>().into_queue(
            |state: TestState, event| TestState {
                count: state.count + event.amount,
            },
        ));
        // Register codec for emitted event type so inline serialization can succeed.
        reducers.register(
            crate::reducer::fold::<Incremented>().into_queue(|state: TestState, _| state),
        );

        let effects = Arc::new(EffectRegistry::new());
        let inline_effect = crate::effect::on::<Increment>().then_queue(
            |event: Arc<Increment>, _ctx: EffectContext<TestState, TestDeps>| async move {
                Ok(Incremented {
                    amount: event.amount + 1,
                })
            },
        );
        effects.register(inline_effect);

        let worker = EventWorker::new(
            store.clone(),
            Arc::new(TestDeps),
            reducers,
            effects,
            EventWorkerConfig::default(),
        );

        let processed = worker
            .process_next_event()
            .await
            .expect("process should succeed");
        assert!(processed);

        let published = store.published_events.lock();
        assert_eq!(published.len(), 1);
        let emitted = &published[0];
        assert_eq!(emitted.parent_id, Some(source.event_id));
        assert_eq!(emitted.correlation_id, source.correlation_id);
        assert_eq!(emitted.hops, source.hops + 1);
        assert_eq!(
            emitted.event_type,
            std::any::type_name::<Incremented>().to_string()
        );
    }

    #[tokio::test]
    async fn event_worker_queues_typed_then_without_reducer_codec() {
        let event = queued_effect_only_event();
        let store = Arc::new(TestStore::new(vec![event.clone()]));

        // No reducer for EffectOnlyEvent on purpose:
        // this verifies queued().then() contributes the codec needed for decode.
        let reducers = Arc::new(ReducerRegistry::new());

        let effects = Arc::new(EffectRegistry::new());
        effects.register(
            crate::effect::on::<EffectOnlyEvent>()
                .id("effect_only_event_queued")
                .queued()
                .then(
                    |_event: Arc<EffectOnlyEvent>, _ctx: EffectContext<TestState, TestDeps>| async {
                        Ok(())
                    },
                ),
        );

        let worker = EventWorker::new(
            store.clone(),
            Arc::new(TestDeps),
            reducers,
            effects,
            EventWorkerConfig::default(),
        );

        let processed = worker
            .process_next_event()
            .await
            .expect("process should succeed");
        assert!(processed);
        assert_eq!(store.effect_intents.lock().len(), 1);
        assert_eq!(*store.acked_ids.lock(), vec![event.id]);
    }

    #[tokio::test]
    async fn event_worker_dlqs_when_inline_retry_limit_is_reached() {
        let mut event = queued_effect_only_event();
        event.retry_count = 3;
        let store = Arc::new(TestStore::new(vec![event.clone()]));
        let reducers = Arc::new(ReducerRegistry::<TestState>::new());
        let effects = Arc::new(EffectRegistry::<TestState, TestDeps>::new());

        let worker = EventWorker::new(
            store.clone(),
            Arc::new(TestDeps),
            reducers,
            effects,
            EventWorkerConfig {
                max_inline_retry_attempts: 3,
                ..EventWorkerConfig::default()
            },
        );

        let processed = worker
            .process_next_event()
            .await
            .expect("process should succeed");
        assert!(processed);
        assert_eq!(*store.acked_ids.lock(), vec![event.id]);
        assert!(store.nacked_ids.lock().is_empty());

        let dlqed = store.dlqed.lock();
        assert_eq!(dlqed.len(), 1);
        assert_eq!(dlqed[0].0, event.event_id);
        assert_eq!(dlqed[0].1, "__inline_effect_retry_exhausted__");
        assert_eq!(dlqed[0].3, "max_retries_exceeded");
        assert_eq!(dlqed[0].4, 3);
        assert!(dlqed[0].2.contains("failed after 3 retry attempts"));
    }

    #[tokio::test]
    async fn event_worker_inline_failure_does_not_block_queued_effects() {
        let event = queued_effect_only_event();
        let store = Arc::new(TestStore::new(vec![event.clone()]));
        let reducers = Arc::new(ReducerRegistry::<TestState>::new());

        let effects = Arc::new(EffectRegistry::new());
        effects.register(
            crate::effect::on::<EffectOnlyEvent>()
                .id("inline_failing_effect")
                .then_queue(
                    |_event: Arc<EffectOnlyEvent>, _ctx: EffectContext<TestState, TestDeps>| async move {
                        Err::<(), _>(anyhow::anyhow!("inline effect exploded"))
                    },
                ),
        );
        effects.register(
            crate::effect::on::<EffectOnlyEvent>()
                .id("queued_effect")
                .retry(3)
                .then_queue(
                    |_event: Arc<EffectOnlyEvent>, _ctx: EffectContext<TestState, TestDeps>| async {
                        Ok(())
                    },
                ),
        );

        let worker = EventWorker::new(
            store.clone(),
            Arc::new(TestDeps),
            reducers,
            effects,
            EventWorkerConfig::default(),
        );

        let processed = worker
            .process_next_event()
            .await
            .expect("process should succeed");
        assert!(processed);
        assert_eq!(*store.acked_ids.lock(), vec![event.id]);
        assert!(store.nacked_ids.lock().is_empty());

        let dlqed = store.dlqed.lock();
        assert_eq!(dlqed.len(), 1);
        assert_eq!(dlqed[0].0, event.event_id);
        assert_eq!(dlqed[0].1, "inline_failing_effect");
        assert_eq!(dlqed[0].3, "inline_failed");

        let intents = store.effect_intents.lock();
        assert_eq!(intents.len(), 1);
        assert_eq!(intents[0].0, "queued_effect");
    }

    #[tokio::test]
    async fn multiple_inline_effects_run_independently() {
        // Verify that when multiple inline effects listen to the same event,
        // one effect failing does NOT prevent other effects from running.

        let event = queued_effect_only_event();
        let store = Arc::new(TestStore::new(vec![event.clone()]));
        let reducers = Arc::new(ReducerRegistry::<TestState>::new());

        // Use a counter to track which effects actually ran
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        #[derive(Clone)]
        struct TestDepsWithCounter {
            counter: Arc<std::sync::atomic::AtomicUsize>,
        }

        let effects = Arc::new(EffectRegistry::new());

        // Effect 1: Inline, increments counter, then fails
        let counter_clone = counter.clone();
        effects.register(
            crate::effect::on::<EffectOnlyEvent>()
                .id("failing_inline")
                .then_queue(
                    move |_event: Arc<EffectOnlyEvent>,
                          _ctx: EffectContext<TestState, TestDepsWithCounter>| {
                        let counter = counter_clone.clone();
                        async move {
                            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            Err::<(), _>(anyhow::anyhow!("boom"))
                        }
                    },
                ),
        );

        // Effect 2: Inline, increments counter, succeeds
        let counter_clone = counter.clone();
        effects.register(
            crate::effect::on::<EffectOnlyEvent>()
                .id("succeeding_inline")
                .then_queue(
                    move |_event: Arc<EffectOnlyEvent>,
                          _ctx: EffectContext<TestState, TestDepsWithCounter>| {
                        let counter = counter_clone.clone();
                        async move {
                            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            Ok(())
                        }
                    },
                ),
        );

        let worker = EventWorker::new(
            store.clone(),
            Arc::new(TestDepsWithCounter {
                counter: counter.clone(),
            }),
            reducers,
            effects,
            EventWorkerConfig::default(),
        );

        let processed = worker
            .process_next_event()
            .await
            .expect("process should succeed");

        assert!(processed);

        // Both effects should have run (counter should be 2)
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "Both inline effects should have run"
        );

        // Event should be ACK'd despite one effect failing
        assert_eq!(*store.acked_ids.lock(), vec![event.id]);
        assert!(store.nacked_ids.lock().is_empty());

        // Failed effect should be in DLQ
        let dlqed = store.dlqed.lock();
        assert_eq!(dlqed.len(), 1);
        assert_eq!(dlqed[0].0, event.event_id);
        assert_eq!(dlqed[0].1, "failing_inline");
        assert_eq!(dlqed[0].3, "inline_failed");
    }

    #[tokio::test]
    async fn multiple_inline_effects_run_but_event_retries_when_dlq_commit_fails() {
        // Verify that all inline effects run, but event commit fails atomically
        // if DLQ persistence fails.

        let event = queued_effect_only_event();

        // Create a store that fails on DLQ operations
        struct FailingDlqStore {
            inner: TestStore,
        }

        #[async_trait]
        impl crate::Store for FailingDlqStore {
            async fn publish(&self, event: QueuedEvent) -> Result<()> {
                self.inner.publish(event).await
            }

            async fn poll_next(&self) -> Result<Option<QueuedEvent>> {
                self.inner.poll_next().await
            }

            async fn ack(&self, id: i64) -> Result<()> {
                self.inner.ack(id).await
            }

            async fn nack(&self, id: i64, retry_after_secs: u64) -> Result<()> {
                self.inner.nack(id, retry_after_secs).await
            }

            async fn load_state<S>(&self, correlation_id: Uuid) -> Result<Option<(S, i32)>>
            where
                S: for<'de> serde::Deserialize<'de> + Send,
            {
                self.inner.load_state(correlation_id).await
            }

            async fn save_state<S>(
                &self,
                correlation_id: Uuid,
                state: &S,
                expected_version: i32,
            ) -> Result<i32>
            where
                S: serde::Serialize + Send + Sync,
            {
                self.inner
                    .save_state(correlation_id, state, expected_version)
                    .await
            }

            async fn insert_effect_intent(
                &self,
                event_id: Uuid,
                effect_id: String,
                correlation_id: Uuid,
                event_type: String,
                event_payload: serde_json::Value,
                parent_event_id: Option<Uuid>,
                batch_id: Option<Uuid>,
                batch_index: Option<i32>,
                batch_size: Option<i32>,
                execute_at: chrono::DateTime<chrono::Utc>,
                timeout_seconds: i32,
                max_attempts: i32,
                priority: i32,
            ) -> Result<()> {
                self.inner
                    .insert_effect_intent(
                        event_id,
                        effect_id,
                        correlation_id,
                        event_type,
                        event_payload,
                        parent_event_id,
                        batch_id,
                        batch_index,
                        batch_size,
                        execute_at,
                        timeout_seconds,
                        max_attempts,
                        priority,
                    )
                    .await
            }

            async fn poll_next_effect(&self) -> Result<Option<crate::QueuedEffectExecution>> {
                self.inner.poll_next_effect().await
            }

            async fn complete_effect(
                &self,
                event_id: Uuid,
                effect_id: String,
                result: serde_json::Value,
            ) -> Result<()> {
                self.inner
                    .complete_effect(event_id, effect_id, result)
                    .await
            }

            async fn complete_effect_with_events(
                &self,
                event_id: Uuid,
                effect_id: String,
                result: serde_json::Value,
                emitted_events: Vec<crate::EmittedEvent>,
            ) -> Result<()> {
                self.inner
                    .complete_effect_with_events(event_id, effect_id, result, emitted_events)
                    .await
            }

            async fn fail_effect(
                &self,
                event_id: Uuid,
                effect_id: String,
                error: String,
                attempts: i32,
            ) -> Result<()> {
                self.inner
                    .fail_effect(event_id, effect_id, error, attempts)
                    .await
            }

            async fn dlq_effect(
                &self,
                _event_id: Uuid,
                _effect_id: String,
                _error: String,
                _reason: String,
                _attempts: i32,
            ) -> Result<()> {
                // Always fail
                anyhow::bail!("DLQ operation failed")
            }

            async fn get_workflow_status(
                &self,
                correlation_id: Uuid,
            ) -> Result<crate::WorkflowStatus> {
                self.inner.get_workflow_status(correlation_id).await
            }

            async fn subscribe_workflow_events(
                &self,
                correlation_id: Uuid,
            ) -> Result<Box<dyn futures::Stream<Item = crate::WorkflowEvent> + Send + Unpin>>
            {
                self.inner.subscribe_workflow_events(correlation_id).await
            }
        }

        let store = Arc::new(FailingDlqStore {
            inner: TestStore::new(vec![event.clone()]),
        });
        let reducers = Arc::new(ReducerRegistry::<TestState>::new());

        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        #[derive(Clone)]
        struct TestDepsWithCounter {
            counter: Arc<std::sync::atomic::AtomicUsize>,
        }

        let effects = Arc::new(EffectRegistry::new());

        // Effect 1: Inline, increments counter, then fails
        let counter_clone = counter.clone();
        effects.register(
            crate::effect::on::<EffectOnlyEvent>()
                .id("failing_inline_1")
                .then_queue(
                    move |_event: Arc<EffectOnlyEvent>,
                          _ctx: EffectContext<TestState, TestDepsWithCounter>| {
                        let counter = counter_clone.clone();
                        async move {
                            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            Err::<(), _>(anyhow::anyhow!("boom"))
                        }
                    },
                ),
        );

        // Effect 2: Inline, increments counter, also fails
        let counter_clone = counter.clone();
        effects.register(
            crate::effect::on::<EffectOnlyEvent>()
                .id("failing_inline_2")
                .then_queue(
                    move |_event: Arc<EffectOnlyEvent>,
                          _ctx: EffectContext<TestState, TestDepsWithCounter>| {
                        let counter = counter_clone.clone();
                        async move {
                            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            Err::<(), _>(anyhow::anyhow!("boom again"))
                        }
                    },
                ),
        );

        // Effect 3: Inline, increments counter, succeeds
        let counter_clone = counter.clone();
        effects.register(
            crate::effect::on::<EffectOnlyEvent>()
                .id("succeeding_inline")
                .then_queue(
                    move |_event: Arc<EffectOnlyEvent>,
                          _ctx: EffectContext<TestState, TestDepsWithCounter>| {
                        let counter = counter_clone.clone();
                        async move {
                            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            Ok(())
                        }
                    },
                ),
        );

        let worker = EventWorker::new(
            store.clone(),
            Arc::new(TestDepsWithCounter {
                counter: counter.clone(),
            }),
            reducers,
            effects,
            EventWorkerConfig::default(),
        );

        let error = worker
            .process_next_event()
            .await
            .expect_err("DLQ persistence failure should fail the atomic commit");
        assert!(
            error.to_string().contains("DLQ operation failed"),
            "unexpected error: {error}"
        );

        // All three effects should have run (counter should be 3)
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "All three inline effects should have run despite DLQ failures"
        );

        // Commit failed, so event should be nacked for retry.
        assert!(store.inner.acked_ids.lock().is_empty());
        assert_eq!(*store.inner.nacked_ids.lock(), vec![event.id]);

        // DLQ should be empty because all DLQ operations failed
        assert_eq!(store.inner.dlqed.lock().len(), 0);
    }

    #[tokio::test]
    async fn event_worker_inline_batch_emit_stress_generates_unique_ids_and_batch_metadata() {
        let source = QueuedEvent {
            id: 99,
            event_id: Uuid::new_v4(),
            parent_id: None,
            correlation_id: Uuid::new_v4(),
            event_type: std::any::type_name::<FanOut>().to_string(),
            payload: serde_json::json!(FanOut { count: 256 }),
            hops: 0,
            retry_count: 0,
            batch_id: None,
            batch_index: None,
            batch_size: None,
            created_at: chrono::Utc::now(),
        };
        let store = Arc::new(TestStore::new(vec![source.clone()]));

        let reducers = Arc::new(ReducerRegistry::new());
        reducers.register(
            crate::reducer::fold::<FanOut>().into_queue(|state: TestState, _event| state),
        );
        reducers.register(
            crate::reducer::fold::<FanOutItem>().into_queue(|state: TestState, _event| state),
        );

        let effects = Arc::new(EffectRegistry::new());
        effects.register(
            crate::effect::on::<FanOut>()
                .then_queue::<TestState, TestDeps, Arc<FanOut>, _, _, Vec<FanOutItem>, FanOutItem>(
                    |event: Arc<FanOut>, _ctx: EffectContext<TestState, TestDeps>| async move {
                        Ok((0..event.count)
                            .map(|index| FanOutItem {
                                index: index as i32,
                            })
                            .collect::<Vec<_>>())
                    },
                ),
        );

        let worker = EventWorker::new(
            store.clone(),
            Arc::new(TestDeps),
            reducers,
            effects,
            EventWorkerConfig::default(),
        );

        let processed = worker
            .process_next_event()
            .await
            .expect("process should succeed");
        assert!(processed);

        let published = store.published_events.lock();
        assert_eq!(published.len(), 256);
        let batch_id = published[0].batch_id.expect("batch_id should be set");
        let mut ids = HashSet::new();
        let mut indexes = HashSet::new();
        for event in published.iter() {
            assert!(ids.insert(event.event_id), "event IDs should be unique");
            assert_eq!(event.batch_id, Some(batch_id));
            assert_eq!(event.batch_size, Some(256));
            let index = event.batch_index.expect("batch_index should be present");
            assert!(indexes.insert(index), "batch indexes should be unique");
            assert!(index >= 0 && index < 256);
        }
        assert_eq!(indexes.len(), 256);
        assert_eq!(*store.acked_ids.lock(), vec![source.id]);
    }

    #[tokio::test]
    async fn event_worker_inline_batch_emit_over_limit_is_dlqd_without_retry() {
        let source = QueuedEvent {
            id: 102,
            event_id: Uuid::new_v4(),
            parent_id: None,
            correlation_id: Uuid::new_v4(),
            event_type: std::any::type_name::<FanOut>().to_string(),
            payload: serde_json::json!(FanOut { count: 256 }),
            hops: 0,
            retry_count: 0,
            batch_id: None,
            batch_index: None,
            batch_size: None,
            created_at: chrono::Utc::now(),
        };
        let store = Arc::new(TestStore::new(vec![source.clone()]));

        let reducers = Arc::new(ReducerRegistry::new());
        reducers.register(
            crate::reducer::fold::<FanOut>().into_queue(|state: TestState, _event| state),
        );
        reducers.register(
            crate::reducer::fold::<FanOutItem>().into_queue(|state: TestState, _event| state),
        );

        let effects = Arc::new(EffectRegistry::new());
        effects.register(
            crate::effect::on::<FanOut>()
                .id("oversized_batch_effect")
                .then_queue::<TestState, TestDeps, Arc<FanOut>, _, _, Vec<FanOutItem>, FanOutItem>(
                    |event: Arc<FanOut>, _ctx: EffectContext<TestState, TestDeps>| async move {
                        Ok((0..event.count)
                            .map(|index| FanOutItem {
                                index: index as i32,
                            })
                            .collect::<Vec<_>>())
                    },
                ),
        );

        let worker = EventWorker::new(
            store.clone(),
            Arc::new(TestDeps),
            reducers,
            effects,
            EventWorkerConfig {
                max_batch_size: 32,
                ..EventWorkerConfig::default()
            },
        );

        let processed = worker
            .process_next_event()
            .await
            .expect("event should be acked while failed inline effect is DLQ'd");
        assert!(processed);
        assert!(store.nacked_ids.lock().is_empty());
        assert_eq!(*store.acked_ids.lock(), vec![source.id]);
        assert!(store.published_events.lock().is_empty());

        let dlqed = store.dlqed.lock();
        assert_eq!(dlqed.len(), 1);
        assert_eq!(dlqed[0].0, source.event_id);
        assert_eq!(dlqed[0].1, "oversized_batch_effect");
        assert_eq!(dlqed[0].3, "inline_failed");
        assert_eq!(dlqed[0].4, 1);
        assert!(
            dlqed[0].2.contains("max_batch_size"),
            "DLQ error should mention max_batch_size, got: {}",
            dlqed[0].2
        );
    }

    #[tokio::test]
    async fn event_worker_inline_single_emit_inherits_incoming_batch_metadata() {
        let incoming_batch_id = Uuid::new_v4();
        let source = QueuedEvent {
            id: 100,
            event_id: Uuid::new_v4(),
            parent_id: None,
            correlation_id: Uuid::new_v4(),
            event_type: std::any::type_name::<BatchCarry>().to_string(),
            payload: serde_json::json!(BatchCarry {
                marker: "in".to_string(),
            }),
            hops: 0,
            retry_count: 0,
            batch_id: Some(incoming_batch_id),
            batch_index: Some(7),
            batch_size: Some(42),
            created_at: chrono::Utc::now(),
        };
        let store = Arc::new(TestStore::new(vec![source.clone()]));

        let reducers = Arc::new(ReducerRegistry::new());
        reducers.register(
            crate::reducer::fold::<BatchCarry>().into_queue(|state: TestState, _event| state),
        );

        let effects = Arc::new(EffectRegistry::new());
        effects.register(crate::effect::on::<BatchCarry>().then_queue(
            |_event: Arc<BatchCarry>, _ctx: EffectContext<TestState, TestDeps>| async move {
                Ok(BatchCarry {
                    marker: "out".to_string(),
                })
            },
        ));

        let worker = EventWorker::new(
            store.clone(),
            Arc::new(TestDeps),
            reducers,
            effects,
            EventWorkerConfig::default(),
        );

        let processed = worker
            .process_next_event()
            .await
            .expect("process should succeed");
        assert!(processed);

        let published = store.published_events.lock();
        assert_eq!(published.len(), 1);
        let emitted = &published[0];
        assert_eq!(emitted.batch_id, Some(incoming_batch_id));
        assert_eq!(emitted.batch_index, Some(7));
        assert_eq!(emitted.batch_size, Some(42));
    }

    #[tokio::test]
    async fn event_worker_passes_batch_metadata_to_queued_effect_intent() {
        let event = QueuedEvent {
            id: 101,
            event_id: Uuid::new_v4(),
            parent_id: None,
            correlation_id: Uuid::new_v4(),
            event_type: std::any::type_name::<Increment>().to_string(),
            payload: serde_json::json!({ "amount": 1 }),
            hops: 0,
            retry_count: 0,
            batch_id: Some(Uuid::new_v4()),
            batch_index: Some(3),
            batch_size: Some(10),
            created_at: chrono::Utc::now(),
        };
        let store = Arc::new(TestStore::new(vec![event.clone()]));

        let reducers = Arc::new(ReducerRegistry::new());
        reducers.register(crate::reducer::fold::<Increment>().into_queue(
            |state: TestState, event| TestState {
                count: state.count + event.amount,
            },
        ));

        let effects = Arc::new(EffectRegistry::new());
        let queued_effect = crate::effect::on::<Increment>()
            .id("queued_increment_batch_metadata")
            .queued()
            .then_queue(
                |_event: Arc<Increment>, _ctx: EffectContext<TestState, TestDeps>| async { Ok(()) },
            );
        effects.register(queued_effect);

        let worker = EventWorker::new(
            store.clone(),
            Arc::new(TestDeps),
            reducers,
            effects,
            EventWorkerConfig::default(),
        );
        let processed = worker
            .process_next_event()
            .await
            .expect("process should succeed");
        assert!(processed);

        let intents = store.effect_intents.lock();
        assert_eq!(intents.len(), 1);
        assert_eq!(intents[0].1, event.batch_id);
        assert_eq!(intents[0].2, event.batch_index);
        assert_eq!(intents[0].3, event.batch_size);
    }
}
