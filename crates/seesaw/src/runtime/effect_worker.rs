//! Effect Worker - polls and executes queued effects

use anyhow::Result;
use chrono::Utc;
use parking_lot::{Mutex, RwLock};
use std::any::{Any, TypeId};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{sleep, timeout};
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::effect::{EffectContext, EventEmitter, JoinMode};
use crate::effect_registry::EffectRegistry;
use crate::reducer_registry::ReducerRegistry;
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
    /// Maximum number of events an effect may emit in one batch
    pub max_batch_size: usize,
}

impl Default for EffectWorkerConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_millis(100),
            default_timeout: Duration::from_secs(30),
            max_batch_size: 10_000,
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
            "Processing effect: effect_id={}, workflow={}, priority={}, attempt={}/{}",
            execution.effect_id,
            execution.correlation_id,
            execution.priority,
            execution.attempts,
            execution.max_attempts
        );

        // Find effect handler by stable ID
        let Some(effect) = self.effects.find_by_id(&execution.effect_id) else {
            let error = format!(
                "No effect handler registered for id '{}'",
                execution.effect_id
            );
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
        );

        let join_claim = match effect.join_mode {
            Some(JoinMode::SameBatch) => {
                let join_claim_result = async {
                    let (Some(batch_id), Some(batch_index), Some(batch_size)) =
                        (execution.batch_id, execution.batch_index, execution.batch_size)
                    else {
                        anyhow::bail!(
                            "join().same_batch() requires batch_id, batch_index, and batch_size metadata"
                        );
                    };
                    if batch_size <= 0 || batch_index < 0 || batch_index >= batch_size {
                        anyhow::bail!(
                            "join().same_batch() received invalid batch metadata: index={} size={}",
                            batch_index,
                            batch_size
                        );
                    }
                    if batch_size as usize > self.config.max_batch_size {
                        anyhow::bail!(
                            "join().same_batch() batch_size {} exceeds max_batch_size {}",
                            batch_size,
                            self.config.max_batch_size
                        );
                    }

                    self.store
                        .join_same_batch_append_and_maybe_claim(
                            execution.effect_id.clone(),
                            execution.correlation_id,
                            execution.event_id,
                            execution.event_type.clone(),
                            execution.event_payload.clone(),
                            Utc::now(),
                            batch_id,
                            batch_index,
                            batch_size,
                        )
                        .await
                }
                .await;

                match join_claim_result {
                    Ok(claim) => claim,
                    Err(error) => {
                        let message = error.to_string();
                        warn!(
                            "Join append failed: {} (attempt {}/{}): {}",
                            execution.effect_id, execution.attempts, execution.max_attempts, message
                        );
                        if execution.attempts >= execution.max_attempts {
                            self.store
                                .dlq_effect(
                                    execution.event_id,
                                    execution.effect_id.clone(),
                                    message,
                                    "failed".to_string(),
                                    execution.attempts,
                                )
                                .await?;
                        } else {
                            self.store
                                .fail_effect(
                                    execution.event_id,
                                    execution.effect_id.clone(),
                                    message,
                                    execution.attempts,
                                )
                                .await?;
                        }
                        return Ok(true);
                    }
                }
            }
            None => None,
        };

        if effect.join_mode.is_some() && join_claim.is_none() {
            self.store
                .complete_effect(
                    execution.event_id,
                    execution.effect_id.clone(),
                    serde_json::json!({ "status": "join_waiting" }),
                )
                .await?;
            return Ok(true);
        }

        let claimed_batch_id = join_claim
            .as_ref()
            .and_then(|entries| entries.first().map(|entry| entry.batch_id));

        // Execute effect with timeout
        let timeout_duration = if execution.timeout_seconds > 0 {
            Duration::from_secs(execution.timeout_seconds as u64)
        } else {
            self.config.default_timeout
        };

        let result = timeout(timeout_duration, async {
            let mut emitted = Vec::new();
            if let Some(entries) = join_claim.as_ref() {
                let mut values = Vec::with_capacity(entries.len());
                for entry in entries {
                    let (typed, _value_type_id) =
                        self.decode_event(&entry.event_type, &entry.payload)?;
                    values.push(typed);
                }

                for output in effect.call_join_batch_handler(values, ctx.clone()).await? {
                    emitted.push((output.type_id, output.value));
                }
            } else {
                for output in effect
                    .call_handler(typed_event, type_id, ctx.clone())
                    .await?
                {
                    emitted.push((output.type_id, output.value));
                }
            }

            // tasks.wait_pending().await; // TODO: Re-enable when tasks tracking is implemented
            emitted.extend(emissions.lock().drain(..));
            Ok::<Vec<(TypeId, Arc<dyn Any + Send + Sync>)>, anyhow::Error>(emitted)
        })
        .await;

        match result {
            Ok(Ok(emitted_raw)) => {
                let emitted_events = match self.serialize_emitted_events(emitted_raw, &execution) {
                    Ok(events) => events,
                    Err(error) => {
                        warn!(
                            "Effect output serialization failed: {} (attempt {}/{}): {}",
                            execution.effect_id,
                            execution.attempts,
                            execution.max_attempts,
                            error
                        );

                        if let Some(batch_id) = claimed_batch_id {
                            if let Err(release_error) = self
                                .store
                                .join_same_batch_release(
                                    execution.effect_id.clone(),
                                    execution.correlation_id,
                                    batch_id,
                                    error.to_string(),
                                )
                                .await
                            {
                                error!(
                                    "Failed to release join claim for {}: {}",
                                    execution.effect_id, release_error
                                );
                            }
                        }

                        if execution.attempts >= execution.max_attempts {
                            self.store
                                .dlq_effect(
                                    execution.event_id,
                                    execution.effect_id.clone(),
                                    error.to_string(),
                                    "failed".to_string(),
                                    execution.attempts,
                                )
                                .await?;
                        } else {
                            self.store
                                .fail_effect(
                                    execution.event_id,
                                    execution.effect_id.clone(),
                                    error.to_string(),
                                    execution.attempts,
                                )
                                .await?;
                        }
                        return Ok(true);
                    }
                };

                // Success - mark as completed
                info!("Effect completed successfully: {}", execution.effect_id);
                let result_value = serde_json::json!({ "status": "ok" });

                if emitted_events.is_empty() {
                    self.store
                        .complete_effect(
                            execution.event_id,
                            execution.effect_id.clone(),
                            result_value,
                        )
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

                if let Some(batch_id) = claimed_batch_id {
                    self.store
                        .join_same_batch_complete(
                            execution.effect_id.clone(),
                            execution.correlation_id,
                            batch_id,
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

                if let Some(batch_id) = claimed_batch_id {
                    if let Err(release_error) = self
                        .store
                        .join_same_batch_release(
                            execution.effect_id.clone(),
                            execution.correlation_id,
                            batch_id,
                            e.to_string(),
                        )
                        .await
                    {
                        error!(
                            "Failed to release join claim for {}: {}",
                            execution.effect_id, release_error
                        );
                    }
                }

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

                if let Some(batch_id) = claimed_batch_id {
                    if let Err(release_error) = self
                        .store
                        .join_same_batch_release(
                            execution.effect_id.clone(),
                            execution.correlation_id,
                            batch_id,
                            "Effect execution timed out".to_string(),
                        )
                        .await
                    {
                        error!(
                            "Failed to release join claim for {} after timeout: {}",
                            execution.effect_id, release_error
                        );
                    }
                }

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
        execution: &crate::QueuedEffectExecution,
    ) -> Result<Vec<EmittedEvent>> {
        let emitted_count = emitted.len();
        if emitted_count > self.config.max_batch_size {
            anyhow::bail!(
                "effect '{}' emitted {} events, exceeding max_batch_size {}",
                execution.effect_id,
                emitted_count,
                self.config.max_batch_size
            );
        }
        if emitted_count > i32::MAX as usize {
            anyhow::bail!(
                "effect '{}' emitted {} events, exceeding i32 batch metadata capacity",
                execution.effect_id,
                emitted_count
            );
        }
        let inherited_batch = if emitted_count == 1 {
            match (
                execution.batch_id,
                execution.batch_index,
                execution.batch_size,
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
                format!("{}-{}-batch", execution.event_id, execution.effect_id).as_bytes(),
            ))
        } else {
            None
        };

        let mut result = Vec::with_capacity(emitted_count);
        for (emitted_index, (type_id, event_any)) in emitted.into_iter().enumerate() {
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
            result.push(EmittedEvent {
                event_type: codec.event_type.clone(),
                payload,
                batch_id,
                batch_index,
                batch_size,
            });
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use chrono::{DateTime, Utc};
    use futures::Stream;
    use parking_lot::Mutex;
    use std::collections::{HashMap, HashSet, VecDeque};
    use std::sync::atomic::{AtomicUsize, Ordering};
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

    #[derive(Clone, serde::Serialize, serde::Deserialize)]
    struct BatchItemResult {
        index: i32,
        ok: bool,
    }

    #[derive(Clone, serde::Serialize, serde::Deserialize)]
    struct BatchJoinSummary {
        total: usize,
        failed: usize,
    }

    #[derive(Clone)]
    struct JoinWindow {
        target_count: i32,
        status: JoinStatus,
        source_ids: HashSet<Uuid>,
        entries_by_index: HashMap<i32, crate::JoinEntry>,
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum JoinStatus {
        Open,
        Processing,
        Completed,
    }

    struct TestStore {
        queued_effects: Mutex<VecDeque<crate::QueuedEffectExecution>>,
        completed: Mutex<Vec<(Uuid, String)>>,
        completed_with_events: Mutex<Vec<(Uuid, String, Vec<crate::EmittedEvent>)>>,
        failed: Mutex<Vec<(Uuid, String)>>,
        dlqed: Mutex<Vec<(Uuid, String)>>,
        join_windows: Mutex<HashMap<(String, Uuid, Uuid), JoinWindow>>,
        join_complete_calls: Mutex<Vec<(String, Uuid, Uuid)>>,
        join_release_calls: Mutex<Vec<(String, Uuid, Uuid, String)>>,
    }

    impl TestStore {
        fn new(effects: Vec<crate::QueuedEffectExecution>) -> Self {
            Self {
                queued_effects: Mutex::new(effects.into()),
                completed: Mutex::new(Vec::new()),
                completed_with_events: Mutex::new(Vec::new()),
                failed: Mutex::new(Vec::new()),
                dlqed: Mutex::new(Vec::new()),
                join_windows: Mutex::new(HashMap::new()),
                join_complete_calls: Mutex::new(Vec::new()),
                join_release_calls: Mutex::new(Vec::new()),
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
            _batch_id: Option<Uuid>,
            _batch_index: Option<i32>,
            _batch_size: Option<i32>,
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

        async fn join_same_batch_append_and_maybe_claim(
            &self,
            join_effect_id: String,
            correlation_id: Uuid,
            source_event_id: Uuid,
            source_event_type: String,
            source_payload: serde_json::Value,
            source_created_at: DateTime<Utc>,
            batch_id: Uuid,
            batch_index: i32,
            batch_size: i32,
        ) -> Result<Option<Vec<crate::JoinEntry>>> {
            let key = (join_effect_id.clone(), correlation_id, batch_id);
            let mut windows = self.join_windows.lock();
            let window = windows.entry(key).or_insert_with(|| JoinWindow {
                target_count: batch_size,
                status: JoinStatus::Open,
                source_ids: HashSet::new(),
                entries_by_index: HashMap::new(),
            });

            if window.status == JoinStatus::Completed {
                return Ok(None);
            }
            window.target_count = batch_size;

            if window.source_ids.insert(source_event_id) {
                window
                    .entries_by_index
                    .entry(batch_index)
                    .or_insert_with(|| crate::JoinEntry {
                        source_event_id,
                        event_type: source_event_type,
                        payload: source_payload,
                        batch_id,
                        batch_index,
                        batch_size,
                        created_at: source_created_at,
                    });
            }

            let ready = window.entries_by_index.len() as i32 >= window.target_count;
            if ready && window.status == JoinStatus::Open {
                window.status = JoinStatus::Processing;
                let mut entries = window
                    .entries_by_index
                    .values()
                    .cloned()
                    .collect::<Vec<_>>();
                entries.sort_by_key(|entry| entry.batch_index);
                return Ok(Some(entries));
            }

            Ok(None)
        }

        async fn join_same_batch_complete(
            &self,
            join_effect_id: String,
            correlation_id: Uuid,
            batch_id: Uuid,
        ) -> Result<()> {
            self.join_complete_calls
                .lock()
                .push((join_effect_id.clone(), correlation_id, batch_id));
            if let Some(window) = self
                .join_windows
                .lock()
                .get_mut(&(join_effect_id, correlation_id, batch_id))
            {
                window.status = JoinStatus::Completed;
            }
            Ok(())
        }

        async fn join_same_batch_release(
            &self,
            join_effect_id: String,
            correlation_id: Uuid,
            batch_id: Uuid,
            error: String,
        ) -> Result<()> {
            self.join_release_calls
                .lock()
                .push((join_effect_id.clone(), correlation_id, batch_id, error));
            if let Some(window) = self
                .join_windows
                .lock()
                .get_mut(&(join_effect_id, correlation_id, batch_id))
            {
                if window.status == JoinStatus::Processing {
                    window.status = JoinStatus::Open;
                }
            }
            Ok(())
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
            batch_id: None,
            batch_index: None,
            batch_size: None,
            execute_at: chrono::Utc::now(),
            timeout_seconds: 1,
            max_attempts: 3,
            priority: 10,
            attempts: 1,
        }
    }

    fn queued_batch_execution(
        effect_id: String,
        correlation_id: Uuid,
        batch_id: Uuid,
        index: i32,
        batch_size: i32,
        attempts: i32,
        max_attempts: i32,
        ok: bool,
    ) -> crate::QueuedEffectExecution {
        crate::QueuedEffectExecution {
            event_id: Uuid::new_v4(),
            effect_id,
            correlation_id,
            event_type: std::any::type_name::<BatchItemResult>().to_string(),
            event_payload: serde_json::json!(BatchItemResult { index, ok }),
            parent_event_id: None,
            batch_id: Some(batch_id),
            batch_index: Some(index),
            batch_size: Some(batch_size),
            execute_at: chrono::Utc::now(),
            timeout_seconds: 5,
            max_attempts,
            priority: 10,
            attempts,
        }
    }

    #[tokio::test]
    async fn effect_worker_executes_and_completes_with_emitted_events() {
        let reducers = Arc::new(ReducerRegistry::new());
        reducers.register(
            crate::reducer::fold::<Incremented>().into_queue(|state: TestState, _event| state),
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

    #[tokio::test]
    async fn effect_worker_marks_failed_when_handler_missing_and_attempts_remaining() {
        let reducers = Arc::new(ReducerRegistry::new());
        let effects = Arc::new(EffectRegistry::<TestState, TestDeps>::new());
        let mut execution = queued_execution("missing_effect_id".to_string());
        execution.attempts = 1;
        execution.max_attempts = 3;

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
        assert_eq!(store.failed.lock().len(), 1);
        assert!(store.dlqed.lock().is_empty());
    }

    #[tokio::test]
    async fn effect_worker_join_same_batch_executes_once_on_closure() {
        let reducers = Arc::new(ReducerRegistry::new());
        reducers.register(
            crate::reducer::fold::<BatchJoinSummary>().into_queue(|state: TestState, _| state),
        );

        let handler_calls = Arc::new(AtomicUsize::new(0));
        let handler_calls_clone = handler_calls.clone();
        let effect = crate::effect::on::<BatchItemResult>()
            .join()
            .same_batch()
            .then(
                move |items: Vec<BatchItemResult>, _ctx: EffectContext<TestState, TestDeps>| {
                    let handler_calls = handler_calls_clone.clone();
                    async move {
                        handler_calls.fetch_add(1, Ordering::SeqCst);
                        let failed = items.iter().filter(|item| !item.ok).count();
                        Ok(BatchJoinSummary {
                            total: items.len(),
                            failed,
                        })
                    }
                },
            );
        let effect_id = effect.id.clone();

        let effects = Arc::new(EffectRegistry::new());
        effects.register(effect);

        let correlation_id = Uuid::new_v4();
        let batch_id = Uuid::new_v4();
        let executions = vec![
            queued_batch_execution(
                effect_id.clone(),
                correlation_id,
                batch_id,
                0,
                3,
                1,
                3,
                true,
            ),
            queued_batch_execution(
                effect_id.clone(),
                correlation_id,
                batch_id,
                1,
                3,
                1,
                3,
                false,
            ),
            queued_batch_execution(
                effect_id.clone(),
                correlation_id,
                batch_id,
                2,
                3,
                1,
                3,
                true,
            ),
        ];

        let store = Arc::new(TestStore::new(executions));
        let worker = EffectWorker::new(
            store.clone(),
            Arc::new(TestDeps),
            reducers,
            effects,
            EffectWorkerConfig::default(),
        );

        while worker.process_next_effect().await.expect("process should succeed") {}

        assert_eq!(handler_calls.load(Ordering::SeqCst), 1);
        assert_eq!(store.completed.lock().len(), 2);
        assert_eq!(store.completed_with_events.lock().len(), 1);
        assert_eq!(store.failed.lock().len(), 0);
        assert_eq!(store.dlqed.lock().len(), 0);
        assert_eq!(store.join_complete_calls.lock().len(), 1);
        assert_eq!(store.join_release_calls.lock().len(), 0);

        let emitted = &store.completed_with_events.lock()[0].2;
        assert_eq!(emitted.len(), 1);
        let payload = &emitted[0].payload;
        assert_eq!(payload["total"], serde_json::json!(3));
        assert_eq!(payload["failed"], serde_json::json!(1));
    }

    #[tokio::test]
    async fn effect_worker_join_same_batch_releases_and_retries_after_handler_error() {
        let reducers = Arc::new(ReducerRegistry::new());
        reducers.register(
            crate::reducer::fold::<BatchJoinSummary>().into_queue(|state: TestState, _| state),
        );

        let handler_calls = Arc::new(AtomicUsize::new(0));
        let handler_calls_clone = handler_calls.clone();
        let effect = crate::effect::on::<BatchItemResult>()
            .join()
            .same_batch()
            .then(
                move |items: Vec<BatchItemResult>, _ctx: EffectContext<TestState, TestDeps>| {
                    let handler_calls = handler_calls_clone.clone();
                    async move {
                        let call = handler_calls.fetch_add(1, Ordering::SeqCst);
                        if call == 0 {
                            anyhow::bail!("synthetic join failure");
                        }
                        Ok(BatchJoinSummary {
                            total: items.len(),
                            failed: items.iter().filter(|item| !item.ok).count(),
                        })
                    }
                },
            );
        let effect_id = effect.id.clone();

        let effects = Arc::new(EffectRegistry::new());
        effects.register(effect);

        let correlation_id = Uuid::new_v4();
        let batch_id = Uuid::new_v4();
        let first_item =
            queued_batch_execution(effect_id.clone(), correlation_id, batch_id, 0, 2, 1, 2, true);
        let retrying_item =
            queued_batch_execution(effect_id.clone(), correlation_id, batch_id, 1, 2, 1, 2, false);
        let mut retry_attempt = retrying_item.clone();
        retry_attempt.attempts = 2;
        retry_attempt.max_attempts = 2;

        let store = Arc::new(TestStore::new(vec![first_item, retrying_item, retry_attempt]));
        let worker = EffectWorker::new(
            store.clone(),
            Arc::new(TestDeps),
            reducers,
            effects,
            EffectWorkerConfig::default(),
        );

        while worker.process_next_effect().await.expect("process should succeed") {}

        assert_eq!(handler_calls.load(Ordering::SeqCst), 2);
        assert_eq!(store.failed.lock().len(), 1);
        assert_eq!(store.dlqed.lock().len(), 0);
        assert_eq!(store.join_release_calls.lock().len(), 1);
        assert_eq!(store.join_complete_calls.lock().len(), 1);
        assert_eq!(store.completed_with_events.lock().len(), 1);

        let payload = &store.completed_with_events.lock()[0].2[0].payload;
        assert_eq!(payload["total"], serde_json::json!(2));
        assert_eq!(payload["failed"], serde_json::json!(1));
    }

    #[tokio::test]
    async fn effect_worker_join_same_batch_invalid_metadata_is_failed() {
        let reducers = Arc::new(ReducerRegistry::new());
        let effect = crate::effect::on::<BatchItemResult>()
            .join()
            .same_batch()
            .then(
                |_items: Vec<BatchItemResult>, _ctx: EffectContext<TestState, TestDeps>| async move {
                    Ok(BatchJoinSummary {
                        total: 0,
                        failed: 0,
                    })
                },
            );
        let effect_id = effect.id.clone();
        let effects = Arc::new(EffectRegistry::new());
        effects.register(effect);

        let mut execution = queued_execution(effect_id);
        execution.event_type = std::any::type_name::<BatchItemResult>().to_string();
        execution.event_payload = serde_json::json!(BatchItemResult {
            index: 0,
            ok: true,
        });
        execution.attempts = 1;
        execution.max_attempts = 3;
        execution.batch_id = None;
        execution.batch_index = Some(0);
        execution.batch_size = Some(1);

        let store = Arc::new(TestStore::new(vec![execution]));
        let worker = EffectWorker::new(
            store.clone(),
            Arc::new(TestDeps),
            reducers,
            effects,
            EffectWorkerConfig::default(),
        );

        let processed = worker.process_next_effect().await.expect("process should succeed");
        assert!(processed);
        assert_eq!(store.failed.lock().len(), 1);
        assert_eq!(store.dlqed.lock().len(), 0);
        assert_eq!(store.join_release_calls.lock().len(), 0);
        assert_eq!(store.join_complete_calls.lock().len(), 0);
    }

    #[tokio::test]
    async fn effect_worker_join_same_batch_stress_large_batch_closes_once() {
        let reducers = Arc::new(ReducerRegistry::new());
        reducers.register(
            crate::reducer::fold::<BatchJoinSummary>().into_queue(|state: TestState, _| state),
        );

        let handler_calls = Arc::new(AtomicUsize::new(0));
        let handler_calls_clone = handler_calls.clone();
        let effect = crate::effect::on::<BatchItemResult>()
            .join()
            .same_batch()
            .then(
                move |items: Vec<BatchItemResult>, _ctx: EffectContext<TestState, TestDeps>| {
                    let handler_calls = handler_calls_clone.clone();
                    async move {
                        handler_calls.fetch_add(1, Ordering::SeqCst);
                        Ok(BatchJoinSummary {
                            total: items.len(),
                            failed: items.iter().filter(|item| !item.ok).count(),
                        })
                    }
                },
            );
        let effect_id = effect.id.clone();
        let effects = Arc::new(EffectRegistry::new());
        effects.register(effect);

        let correlation_id = Uuid::new_v4();
        let batch_id = Uuid::new_v4();
        let batch_size = 256i32;
        let mut executions = Vec::with_capacity(batch_size as usize);
        for index in 0..batch_size {
            executions.push(queued_batch_execution(
                effect_id.clone(),
                correlation_id,
                batch_id,
                index,
                batch_size,
                1,
                3,
                index % 4 != 0,
            ));
        }

        let store = Arc::new(TestStore::new(executions));
        let worker = EffectWorker::new(
            store.clone(),
            Arc::new(TestDeps),
            reducers,
            effects,
            EffectWorkerConfig::default(),
        );

        let mut iterations = 0usize;
        while worker.process_next_effect().await.expect("process should succeed") {
            iterations += 1;
        }
        assert_eq!(iterations, batch_size as usize);
        assert_eq!(handler_calls.load(Ordering::SeqCst), 1);
        assert_eq!(store.completed.lock().len(), (batch_size - 1) as usize);
        assert_eq!(store.completed_with_events.lock().len(), 1);
        assert_eq!(store.join_complete_calls.lock().len(), 1);

        let payload = &store.completed_with_events.lock()[0].2[0].payload;
        assert_eq!(payload["total"], serde_json::json!(batch_size));
        assert_eq!(payload["failed"], serde_json::json!(batch_size / 4));
    }

    #[tokio::test]
    async fn effect_worker_emitted_batch_over_limit_is_failed() {
        let reducers = Arc::new(ReducerRegistry::new());
        reducers.register(
            crate::reducer::fold::<Incremented>().into_queue(|state: TestState, _event| state),
        );

        let effect = crate::effect::on::<Increment>().then_queue::<
            TestState,
            TestDeps,
            Arc<Increment>,
            _,
            _,
            Vec<Incremented>,
            Incremented,
        >(
            |_event: Arc<Increment>, _ctx: EffectContext<TestState, TestDeps>| async move {
                Ok((0..256)
                    .map(|idx| Incremented { amount: idx })
                    .collect::<Vec<_>>())
            },
        );
        let effect_id = effect.id.clone();
        let effects = Arc::new(EffectRegistry::new());
        effects.register(effect);

        let execution = queued_execution(effect_id.clone());
        let store = Arc::new(TestStore::new(vec![execution]));
        let worker = EffectWorker::new(
            store.clone(),
            Arc::new(TestDeps),
            reducers,
            effects,
            EffectWorkerConfig {
                max_batch_size: 32,
                ..EffectWorkerConfig::default()
            },
        );

        let processed = worker
            .process_next_effect()
            .await
            .expect("process should succeed");
        assert!(processed);
        assert_eq!(store.failed.lock().len(), 1);
        assert!(store.dlqed.lock().is_empty());
        assert!(store.completed.lock().is_empty());
        assert!(store.completed_with_events.lock().is_empty());
    }

    #[tokio::test]
    async fn effect_worker_join_same_batch_over_limit_is_failed() {
        let reducers = Arc::new(ReducerRegistry::new());
        let effect = crate::effect::on::<BatchItemResult>()
            .join()
            .same_batch()
            .then(
                |_items: Vec<BatchItemResult>, _ctx: EffectContext<TestState, TestDeps>| async move {
                    Ok(BatchJoinSummary {
                        total: 0,
                        failed: 0,
                    })
                },
            );
        let effect_id = effect.id.clone();
        let effects = Arc::new(EffectRegistry::new());
        effects.register(effect);

        let execution = queued_batch_execution(
            effect_id,
            Uuid::new_v4(),
            Uuid::new_v4(),
            0,
            256,
            1,
            3,
            true,
        );
        let store = Arc::new(TestStore::new(vec![execution]));
        let worker = EffectWorker::new(
            store.clone(),
            Arc::new(TestDeps),
            reducers,
            effects,
            EffectWorkerConfig {
                max_batch_size: 64,
                ..EffectWorkerConfig::default()
            },
        );

        let processed = worker
            .process_next_effect()
            .await
            .expect("process should succeed");
        assert!(processed);
        assert_eq!(store.failed.lock().len(), 1);
        assert!(store.dlqed.lock().is_empty());
        assert!(store.join_complete_calls.lock().is_empty());
        assert!(store.join_release_calls.lock().is_empty());
    }
}
