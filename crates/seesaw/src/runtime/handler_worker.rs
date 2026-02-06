//! Effect Worker - polls and executes queued effects.

use anyhow::Result;
use chrono::Utc;
use std::any::{Any, TypeId};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{sleep, timeout};
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::handler::{Context, DlqTerminalInfo, JoinMode};
use crate::handler_registry::HandlerRegistry;
use crate::queue_backend::{QueueBackend, StoreQueueBackend};
use crate::{EmittedEvent, Store, NAMESPACE_SEESAW};

/// Handler worker configuration.
#[derive(Debug, Clone)]
pub struct HandlerWorkerConfig {
    /// Polling interval when no effects available.
    pub poll_interval: Duration,
    /// Default timeout for effect execution.
    pub default_timeout: Duration,
    /// Maximum number of events an effect may emit in one batch.
    pub max_batch_size: usize,
}

impl Default for HandlerWorkerConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_millis(100),
            default_timeout: Duration::from_secs(30),
            max_batch_size: 10_000,
        }
    }
}

/// Handler worker - polls and executes queued effects.
pub struct HandlerWorker<D, St>
where
    D: Send + Sync + 'static,
    St: Store,
{
    store: Arc<St>,
    deps: Arc<D>,
    effects: Arc<HandlerRegistry<D>>,
    queue_backend: Arc<dyn QueueBackend<St>>,
    config: HandlerWorkerConfig,
    shutdown: Arc<AtomicBool>,
}

impl<D, St> HandlerWorker<D, St>
where
    D: Send + Sync + 'static,
    St: Store,
{
    pub(crate) fn new(
        store: Arc<St>,
        deps: Arc<D>,
        effects: Arc<HandlerRegistry<D>>,
        config: HandlerWorkerConfig,
    ) -> Self {
        Self {
            store,
            deps,
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

    /// Run worker loop.
    pub async fn run(self) -> Result<()> {
        info!("Handler worker started");

        while !self.shutdown.load(Ordering::SeqCst) {
            match self.process_next_effect().await {
                Ok(processed) => {
                    if !processed {
                        sleep(self.config.poll_interval).await;
                    }
                }
                Err(e) => {
                    error!("Error processing effect: {}", e);
                    sleep(self.config.poll_interval).await;
                }
            }
        }

        info!("Handler worker stopped");
        Ok(())
    }

    /// Process next available effect.
    ///
    /// Returns true if effect was processed, false if no effects available.
    async fn process_next_effect(&self) -> Result<bool> {
        self.expire_accumulation_windows().await?;

        let execution = match self.queue_backend.poll_next_effect(&*self.store).await {
            Ok(Some(execution)) => Some(execution),
            Ok(None) => self.store.poll_next_effect().await?,
            Err(error) => {
                warn!(
                    "Queue backend poll failed, falling back to store polling: backend={}, error={}",
                    self.queue_backend.name(),
                    error
                );
                self.store.poll_next_effect().await?
            }
        };
        let Some(execution) = execution else {
            return Ok(false);
        };

        info!(
            "Processing effect: effect_id={}, workflow={}, priority={}, attempt={}/{}",
            execution.handler_id,
            execution.correlation_id,
            execution.priority,
            execution.attempts,
            execution.max_attempts
        );

        let Some(effect) = self.effects.find_by_id(&execution.handler_id) else {
            let error = format!(
                "No effect handler registered for id '{}'",
                execution.handler_id
            );
            warn!("{}", error);
            if execution.attempts >= execution.max_attempts {
                self.store
                    .dlq_effect(
                        execution.event_id,
                        execution.handler_id,
                        error,
                        "missing_handler".to_string(),
                        execution.attempts,
                    )
                    .await?;
            } else {
                self.store
                    .fail_effect(
                        execution.event_id,
                        execution.handler_id,
                        error,
                        execution.attempts,
                    )
                    .await?;
            }
            return Ok(true);
        };

        let (typed_event, type_id) =
            self.decode_event(&execution.event_type, &execution.event_payload)?;
        let source_event_for_dlq = typed_event.clone();
        let idempotency_key = Uuid::new_v5(
            &NAMESPACE_SEESAW,
            format!("{}-{}", execution.event_id, execution.handler_id).as_bytes(),
        )
        .to_string();
        let ctx = Context::new(
            effect.id.clone(),
            idempotency_key,
            execution.correlation_id,
            execution.event_id,
            self.deps.clone(),
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
                            execution.handler_id.clone(),
                            execution.correlation_id,
                            execution.event_id,
                            execution.event_type.clone(),
                            execution.event_payload.clone(),
                            Utc::now(),
                            batch_id,
                            batch_index,
                            batch_size,
                            execution.join_window_timeout_seconds,
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
                            execution.handler_id,
                            execution.attempts,
                            execution.max_attempts,
                            message
                        );
                        self.fail_or_dlq_effect(
                            &execution,
                            Some(&effect),
                            source_event_for_dlq.clone(),
                            type_id,
                            "failed",
                            message,
                        )
                        .await?;
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
                    execution.handler_id.clone(),
                    serde_json::json!({ "status": "join_waiting" }),
                )
                .await?;
            return Ok(true);
        }

        let claimed_batch_id = join_claim
            .as_ref()
            .and_then(|entries| entries.first().map(|entry| entry.batch_id));

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

            Ok::<Vec<(TypeId, Arc<dyn Any + Send + Sync>)>, anyhow::Error>(emitted)
        })
        .await;

        match result {
            Ok(Ok(emitted_raw)) => {
                let emitted_events = match self.serialize_emitted_events(emitted_raw, &execution) {
                    Ok(events) => events,
                    Err(error) => {
                        warn!(
                            "Handler output serialization failed: {} (attempt {}/{}): {}",
                            execution.handler_id, execution.attempts, execution.max_attempts, error
                        );

                        if let Some(batch_id) = claimed_batch_id {
                            if let Err(release_error) = self
                                .store
                                .join_same_batch_release(
                                    execution.handler_id.clone(),
                                    execution.correlation_id,
                                    batch_id,
                                    error.to_string(),
                                )
                                .await
                            {
                                error!(
                                    "Failed to release join claim for {}: {}",
                                    execution.handler_id, release_error
                                );
                            }
                        }

                        self.fail_or_dlq_effect(
                            &execution,
                            Some(&effect),
                            source_event_for_dlq.clone(),
                            type_id,
                            "failed",
                            error.to_string(),
                        )
                        .await?;
                        return Ok(true);
                    }
                };

                info!("Handler completed successfully: {}", execution.handler_id);
                let result_value = serde_json::json!({ "status": "ok" });

                if emitted_events.is_empty() {
                    self.store
                        .complete_effect(
                            execution.event_id,
                            execution.handler_id.clone(),
                            result_value,
                        )
                        .await?;
                } else {
                    self.store
                        .complete_effect_with_events(
                            execution.event_id,
                            execution.handler_id.clone(),
                            result_value,
                            emitted_events,
                        )
                        .await?;
                }

                if let Some(batch_id) = claimed_batch_id {
                    self.store
                        .join_same_batch_complete(
                            execution.handler_id.clone(),
                            execution.correlation_id,
                            batch_id,
                        )
                        .await?;
                }
            }
            Ok(Err(e)) => {
                warn!(
                    "Handler failed: {} (attempt {}/{}): {}",
                    execution.handler_id, execution.attempts, execution.max_attempts, e
                );

                if let Some(batch_id) = claimed_batch_id {
                    if let Err(release_error) = self
                        .store
                        .join_same_batch_release(
                            execution.handler_id.clone(),
                            execution.correlation_id,
                            batch_id,
                            e.to_string(),
                        )
                        .await
                    {
                        error!(
                            "Failed to release join claim for {}: {}",
                            execution.handler_id, release_error
                        );
                    }
                }

                self.fail_or_dlq_effect(
                    &execution,
                    Some(&effect),
                    source_event_for_dlq.clone(),
                    type_id,
                    "failed",
                    e.to_string(),
                )
                .await?;
            }
            Err(_) => {
                warn!("Handler timed out: {}", execution.handler_id);

                if let Some(batch_id) = claimed_batch_id {
                    if let Err(release_error) = self
                        .store
                        .join_same_batch_release(
                            execution.handler_id.clone(),
                            execution.correlation_id,
                            batch_id,
                            "Handler execution timed out".to_string(),
                        )
                        .await
                    {
                        error!(
                            "Failed to release join claim for {} after timeout: {}",
                            execution.handler_id, release_error
                        );
                    }
                }

                self.fail_or_dlq_effect(
                    &execution,
                    Some(&effect),
                    source_event_for_dlq.clone(),
                    type_id,
                    "timeout",
                    "Handler execution timed out".to_string(),
                )
                .await?;
            }
        }

        Ok(true)
    }

    async fn expire_accumulation_windows(&self) -> Result<()> {
        let expired_windows = self.store.expire_same_batch_windows(Utc::now()).await?;
        for expired in expired_windows {
            let error = format!(
                "accumulation window timed out: correlation_id={}, batch_id={}",
                expired.correlation_id, expired.batch_id
            );
            for source_event_id in expired.source_event_ids {
                self.store
                    .dlq_effect(
                        source_event_id,
                        expired.join_handler_id.clone(),
                        error.clone(),
                        "accumulate_timeout".to_string(),
                        0,
                    )
                    .await?;
            }
        }
        Ok(())
    }

    fn decode_event(
        &self,
        event_type: &str,
        payload: &serde_json::Value,
    ) -> Result<(Arc<dyn Any + Send + Sync>, TypeId)> {
        let codec = self.effects.find_codec_by_event_type(event_type);

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
        execution: &crate::QueuedHandlerExecution,
    ) -> Result<Vec<EmittedEvent>> {
        let emitted_count = emitted.len();
        if emitted_count > self.config.max_batch_size {
            anyhow::bail!(
                "effect '{}' emitted {} events, exceeding max_batch_size {}",
                execution.handler_id,
                emitted_count,
                self.config.max_batch_size
            );
        }
        if emitted_count > i32::MAX as usize {
            anyhow::bail!(
                "effect '{}' emitted {} events, exceeding i32 batch metadata capacity",
                execution.handler_id,
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
                format!("{}-{}-batch", execution.event_id, execution.handler_id).as_bytes(),
            ))
        } else {
            None
        };

        let mut result = Vec::with_capacity(emitted_count);
        for (emitted_index, (type_id, event_any)) in emitted.into_iter().enumerate() {
            let codec = self.effects.find_codec_by_type_id(type_id).ok_or_else(|| {
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

    fn build_dlq_terminal_event(
        &self,
        effect: &crate::handler::Handler<D>,
        source_event: Arc<dyn Any + Send + Sync>,
        source_type_id: TypeId,
        execution: &crate::QueuedHandlerExecution,
        reason: &str,
        error: String,
    ) -> Result<Option<EmittedEvent>> {
        let Some(mapper) = effect.dlq_terminal_mapper.as_ref() else {
            return Ok(None);
        };

        let mut emitted = mapper(
            source_event,
            source_type_id,
            DlqTerminalInfo {
                error,
                reason: reason.to_string(),
                attempts: execution.attempts,
                max_attempts: execution.max_attempts,
            },
        )?;

        if emitted.batch_id.is_none()
            && emitted.batch_index.is_none()
            && emitted.batch_size.is_none()
            && execution.batch_id.is_some()
            && execution.batch_index.is_some()
            && execution.batch_size.is_some()
        {
            emitted.batch_id = execution.batch_id;
            emitted.batch_index = execution.batch_index;
            emitted.batch_size = execution.batch_size;
        }

        Ok(Some(emitted))
    }

    async fn fail_or_dlq_effect(
        &self,
        execution: &crate::QueuedHandlerExecution,
        effect: Option<&crate::handler::Handler<D>>,
        source_event: Arc<dyn Any + Send + Sync>,
        source_type_id: TypeId,
        reason: &str,
        error: String,
    ) -> Result<()> {
        if execution.attempts < execution.max_attempts {
            self.store
                .fail_effect(
                    execution.event_id,
                    execution.handler_id.clone(),
                    error,
                    execution.attempts,
                )
                .await?;
            return Ok(());
        }

        if let Some(effect) = effect {
            match self.build_dlq_terminal_event(
                effect,
                source_event,
                source_type_id,
                execution,
                reason,
                error.clone(),
            ) {
                Ok(Some(emitted)) => {
                    let dlq_result = self
                        .store
                        .dlq_effect_with_events(
                            execution.event_id,
                            execution.handler_id.clone(),
                            error.clone(),
                            reason.to_string(),
                            execution.attempts,
                            vec![emitted],
                        )
                        .await;
                    if dlq_result.is_ok() {
                        return Ok(());
                    }

                    if let Err(store_error) = dlq_result {
                        warn!(
                            "dlq_effect_with_events failed for {}: {}. Falling back to dlq_effect",
                            execution.handler_id, store_error
                        );
                    }
                }
                Ok(None) => {}
                Err(mapper_error) => {
                    warn!(
                        "dlq_terminal mapper failed for {}: {}. Falling back to dlq_effect",
                        execution.handler_id, mapper_error
                    );
                }
            }
        }

        self.store
            .dlq_effect(
                execution.event_id,
                execution.handler_id.clone(),
                error,
                reason.to_string(),
                execution.attempts,
            )
            .await?;

        Ok(())
    }
}
