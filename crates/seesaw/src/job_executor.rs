//! JobExecutor - extracted handler execution logic from workers
//!
//! This consolidates the event processing and handler execution logic
//! that was previously embedded in EventWorker and HandlerWorker.

use anyhow::Result;
use std::any::{Any, TypeId};
use std::sync::Arc;
use tokio::time::timeout;
use tracing::{info, warn};
use uuid::Uuid;

use crate::handler::{Context, DlqTerminalInfo, Handler, JoinMode};
use crate::handler_registry::HandlerRegistry;
use crate::handler_runner::HandlerRunner;
use crate::types::{
    EmittedEvent, EventWorkerConfig, HandlerWorkerConfig, QueuedEvent,
    QueuedHandlerExecution, QueuedHandlerIntent, NAMESPACE_SEESAW,
};

/// Extracted execution logic for events and handlers.
///
/// This struct consolidates the pure execution logic that was previously
/// scattered across EventWorker and HandlerWorker implementations.
pub struct JobExecutor<D>
where
    D: Send + Sync + 'static,
{
    deps: Arc<D>,
    effects: Arc<HandlerRegistry<D>>,
}

impl<D> JobExecutor<D>
where
    D: Send + Sync + 'static,
{
    /// Create a new job executor.
    pub fn new(deps: Arc<D>, effects: Arc<HandlerRegistry<D>>) -> Self {
        Self { deps, effects }
    }

    /// Execute event processing (inline handlers + queue intents).
    ///
    /// Extracted from EventWorker::process_claimed_event().
    /// Returns commit payload for atomic transaction.
    pub async fn execute_event(
        &self,
        event: &QueuedEvent,
        config: &EventWorkerConfig,
        runner: &dyn HandlerRunner,
    ) -> Result<EventProcessingCommit> {
        info!(
            "Processing event: type={}, workflow={}, hops={}",
            event.event_type, event.correlation_id, event.hops
        );

        // 1. Check max hops (infinite loop detection)
        if event.hops >= config.max_hops {
            warn!(
                "Event exceeded max hops ({}), will DLQ: event_id={}",
                config.max_hops, event.event_id
            );
            return Err(anyhow::anyhow!(
                "Event exceeded maximum hop count ({}) - infinite loop detected",
                config.max_hops
            ));
        }

        // 2. Check retry limit
        if event.retry_count >= config.max_inline_retry_attempts {
            warn!(
                "Event exceeded max retry attempts ({}), will DLQ: event_id={}",
                config.max_inline_retry_attempts, event.event_id
            );
            return Err(anyhow::anyhow!(
                "Event failed after {} retry attempts",
                event.retry_count
            ));
        }

        // 3. Decode event via codec
        let (typed_event, event_type_id) = self.decode_event(&event.event_type, &event.payload)?;

        // 4. Route to matching handlers
        let matching_effects: Vec<_> = self
            .effects
            .all()
            .into_iter()
            .filter(|effect| effect.can_handle(event_type_id))
            .collect();

        // 5. Create queued handler intents (for queued handlers)
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
            queued_effect_intents.push(QueuedHandlerIntent {
                handler_id: effect.id.clone(),
                parent_event_id: Some(event.event_id),
                batch_id: event.batch_id,
                batch_index: event.batch_index,
                batch_size: event.batch_size,
                execute_at,
                timeout_seconds,
                max_attempts: effect.max_attempts as i32,
                priority: effect.priority.unwrap_or(10),
                join_window_timeout_seconds: effect
                    .join_window_timeout
                    .map(|d| d.as_secs() as i32)
                    .map(|seconds| seconds.max(1)),
            });
        }

        // 6. Execute inline handlers (sorted by priority, lower = first)
        let mut inline_effect_failures = Vec::new();
        let mut emitted_events = Vec::new();
        let mut inline_effects: Vec<_> = matching_effects.iter().filter(|effect| effect.is_inline()).collect();
        inline_effects.sort_by_key(|e| e.priority.unwrap_or(i32::MAX));
        for effect in inline_effects {
            match self
                .run_inline_effect(effect, event, typed_event.clone(), event_type_id, config, runner)
                .await
            {
                Ok(mut emitted) => emitted_events.append(&mut emitted),
                Err(error) => {
                    let error_string = error.to_string();
                    warn!(
                        "Inline effect failed and will be persisted to DLQ: event_id={}, effect_id={}, error={}",
                        event.event_id, effect.id, error_string
                    );
                    inline_effect_failures.push(InlineHandlerFailure {
                        handler_id: effect.id.clone(),
                        error: error_string,
                        reason: "inline_failed".to_string(),
                        attempts: event.retry_count.saturating_add(1),
                    });
                }
            }
        }

        // 7. Return commit payload
        Ok(EventProcessingCommit {
            event_row_id: event.id,
            event_id: event.event_id,
            correlation_id: event.correlation_id,
            event_type: event.event_type.clone(),
            event_payload: event.payload.clone(),
            queued_effect_intents,
            inline_effect_failures,
            emitted_events,
        })
    }

    /// Execute handler (queued effect).
    ///
    /// Extracted from HandlerWorker::process_next_effect().
    pub async fn execute_handler(
        &self,
        execution: QueuedHandlerExecution,
        config: &HandlerWorkerConfig,
        runner: &dyn HandlerRunner,
    ) -> Result<HandlerExecutionResult> {
        info!(
            "Processing effect: effect_id={}, workflow={}, priority={}, attempt={}/{}",
            execution.handler_id,
            execution.correlation_id,
            execution.priority,
            execution.attempts,
            execution.max_attempts
        );

        // 1. Find handler by ID
        let Some(effect) = self.effects.find_by_id(&execution.handler_id) else {
            let error = format!(
                "No effect handler registered for id '{}'",
                execution.handler_id
            );
            warn!("{}", error);
            return Ok(HandlerExecutionResult {
                status: if execution.attempts >= execution.max_attempts {
                    HandlerStatus::Failed {
                        error: error.clone(),
                        attempts: execution.attempts,
                    }
                } else {
                    HandlerStatus::Retry {
                        error,
                        attempts: execution.attempts,
                    }
                },
                emitted_events: Vec::new(),
                result: serde_json::json!({}),
                join_claim: None,
            });
        };

        let (typed_event, type_id) =
            self.decode_event(&execution.event_type, &execution.event_payload)?;

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
            execution.parent_event_id,
            self.deps.clone(),
        );

        // 2. Handle join/accumulation (if configured)
        let join_claim = if effect.join_mode == Some(JoinMode::SameBatch) {
            Some(JoinClaim {
                batch_id: execution.batch_id.ok_or_else(|| {
                    anyhow::anyhow!("join().same_batch() requires batch_id metadata")
                })?,
                needs_release: true,
            })
        } else {
            None
        };

        // For now, we'll return a placeholder result that indicates join is needed
        // The actual join logic will be handled by the backend (PostgresBackend)
        if effect.join_mode.is_some() {
            // Return special status indicating join coordination is needed
            return Ok(HandlerExecutionResult {
                status: HandlerStatus::JoinWaiting,
                emitted_events: Vec::new(),
                result: serde_json::json!({ "status": "join_waiting" }),
                join_claim,
            });
        }

        // 3. Execute with timeout
        let timeout_duration = if execution.timeout_seconds > 0 {
            std::time::Duration::from_secs(execution.timeout_seconds as u64)
        } else {
            config.default_timeout
        };

        let handler_fut = effect.make_handler_future(typed_event.clone(), type_id, ctx.clone());
        let result = timeout(timeout_duration, async {
            let outputs = runner.run(&execution.handler_id, Box::pin(handler_fut)).await?;
            let emitted = outputs.into_iter().map(|o| (o.type_id, o.value)).collect();
            Ok::<Vec<(TypeId, Arc<dyn Any + Send + Sync>)>, anyhow::Error>(emitted)
        })
        .await;

        // 4. Handle execution result
        match result {
            Ok(Ok(emitted_raw)) => {
                // 5. Serialize emitted events
                let emitted_events = self.serialize_emitted_events(
                    emitted_raw,
                    &execution,
                    config.max_batch_size,
                )?;

                info!("Handler completed successfully: {}", execution.handler_id);
                Ok(HandlerExecutionResult {
                    status: HandlerStatus::Success,
                    emitted_events,
                    result: serde_json::json!({ "status": "ok" }),
                    join_claim: None,
                })
            }
            Ok(Err(e)) => {
                warn!(
                    "Handler failed: {} (attempt {}/{}): {}",
                    execution.handler_id, execution.attempts, execution.max_attempts, e
                );

                let status = if execution.attempts >= execution.max_attempts {
                    HandlerStatus::Failed {
                        error: e.to_string(),
                        attempts: execution.attempts,
                    }
                } else {
                    HandlerStatus::Retry {
                        error: e.to_string(),
                        attempts: execution.attempts,
                    }
                };

                // Try to build DLQ terminal event
                let emitted_events =
                    if execution.attempts >= execution.max_attempts && effect.dlq_terminal_mapper.is_some() {
                        self.build_dlq_terminal_event(
                            &effect,
                            typed_event,
                            type_id,
                            &execution,
                            "failed",
                            e.to_string(),
                        )?
                    } else {
                        Vec::new()
                    };

                Ok(HandlerExecutionResult {
                    status,
                    emitted_events,
                    result: serde_json::json!({}),
                    join_claim: None,
                })
            }
            Err(_) => {
                warn!("Handler timed out: {}", execution.handler_id);

                let status = if execution.attempts >= execution.max_attempts {
                    HandlerStatus::Failed {
                        error: "Handler execution timed out".to_string(),
                        attempts: execution.attempts,
                    }
                } else {
                    HandlerStatus::Retry {
                        error: "Handler execution timed out".to_string(),
                        attempts: execution.attempts,
                    }
                };

                Ok(HandlerExecutionResult {
                    status,
                    emitted_events: Vec::new(),
                    result: serde_json::json!({}),
                    join_claim: None,
                })
            }
        }
    }

    /// Run startup handlers (from Runtime::start).
    pub async fn run_startup_handlers(&self) -> Result<()> {
        for effect in self.effects.all() {
            if effect.started.is_none() {
                continue;
            }

            let ctx = Context::new(
                effect.id.clone(),
                format!("startup::{}", effect.id),
                Uuid::nil(),
                Uuid::nil(),
                None,
                self.deps.clone(),
            );

            effect
                .call_started(ctx)
                .await
                .map_err(|e| anyhow::anyhow!("startup handler for effect '{}' failed: {}", effect.id, e))?;
        }
        Ok(())
    }

    /// Get effects reference (for Engine accessor).
    pub fn effects(&self) -> &Arc<HandlerRegistry<D>> {
        &self.effects
    }

    // --- Private helpers ---

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

    async fn run_inline_effect(
        &self,
        effect: &Handler<D>,
        source_event: &QueuedEvent,
        typed_event: Arc<dyn Any + Send + Sync>,
        event_type_id: TypeId,
        config: &EventWorkerConfig,
        runner: &dyn HandlerRunner,
    ) -> Result<Vec<QueuedEvent>> {
        let idempotency_key = Uuid::new_v5(
            &NAMESPACE_SEESAW,
            format!("{}-{}", source_event.event_id, effect.id).as_bytes(),
        )
        .to_string();

        let ctx = Context::new(
            effect.id.clone(),
            idempotency_key,
            source_event.correlation_id,
            source_event.event_id,
            source_event.parent_id,
            self.deps.clone(),
        );

        let handler_fut = effect.make_handler_future(typed_event, event_type_id, ctx.clone());
        let drained = runner
            .run(&effect.id, Box::pin(handler_fut))
            .await?
            .into_iter()
            .map(|output| (output.type_id, output.value))
            .collect::<Vec<_>>();

        let emitted_count = drained.len();
        if emitted_count > config.max_batch_size {
            anyhow::bail!(
                "inline effect '{}' emitted {} events, exceeding max_batch_size {}",
                effect.id,
                emitted_count,
                config.max_batch_size
            );
        }

        if emitted_count > i32::MAX as usize {
            anyhow::bail!(
                "inline effect '{}' emitted {} events, exceeding i32 batch metadata capacity",
                effect.id,
                emitted_count
            );
        }

        // Batch metadata logic (same as original)
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
                        || batch_size as usize > config.max_batch_size
                    {
                        anyhow::bail!(
                            "invalid inherited batch metadata: id={} index={} size={} max_batch_size={}",
                            batch_id,
                            batch_index,
                            batch_size,
                            config.max_batch_size
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
            let codec = self.effects.find_codec_by_type_id(type_id).ok_or_else(|| {
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

    fn serialize_emitted_events(
        &self,
        emitted: Vec<(TypeId, Arc<dyn Any + Send + Sync>)>,
        execution: &QueuedHandlerExecution,
        max_batch_size: usize,
    ) -> Result<Vec<EmittedEvent>> {
        let emitted_count = emitted.len();
        if emitted_count > max_batch_size {
            anyhow::bail!(
                "effect '{}' emitted {} events, exceeding max_batch_size {}",
                execution.handler_id,
                emitted_count,
                max_batch_size
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
                        || batch_size as usize > max_batch_size
                    {
                        anyhow::bail!(
                            "invalid inherited batch metadata: id={} index={} size={} max_batch_size={}",
                            batch_id,
                            batch_index,
                            batch_size,
                            max_batch_size
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
        effect: &Handler<D>,
        source_event: Arc<dyn Any + Send + Sync>,
        source_type_id: TypeId,
        execution: &QueuedHandlerExecution,
        reason: &str,
        error: String,
    ) -> Result<Vec<EmittedEvent>> {
        let Some(mapper) = effect.dlq_terminal_mapper.as_ref() else {
            return Ok(Vec::new());
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

        // Inherit batch metadata if not set
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

        Ok(vec![emitted])
    }
}

/// Result of event processing (for atomic commit).
#[derive(Debug)]
pub struct EventProcessingCommit {
    pub event_row_id: i64,
    pub event_id: Uuid,
    pub correlation_id: Uuid,
    pub event_type: String,
    pub event_payload: serde_json::Value,
    pub queued_effect_intents: Vec<QueuedHandlerIntent>,
    pub inline_effect_failures: Vec<InlineHandlerFailure>,
    pub emitted_events: Vec<QueuedEvent>,
}

/// Captured inline effect failure.
#[derive(Debug, Clone)]
pub struct InlineHandlerFailure {
    pub handler_id: String,
    pub error: String,
    pub reason: String,
    pub attempts: i32,
}

/// Result of handler execution.
#[derive(Debug)]
pub struct HandlerExecutionResult {
    pub status: HandlerStatus,
    pub emitted_events: Vec<EmittedEvent>,
    pub result: serde_json::Value,
    pub join_claim: Option<JoinClaim>,
}

/// Handler execution status.
#[derive(Debug)]
pub enum HandlerStatus {
    Success,
    Failed { error: String, attempts: i32 },
    Retry { error: String, attempts: i32 },
    Timeout,
    JoinWaiting,
}

/// Join claim metadata (for release on failure).
#[derive(Debug)]
pub struct JoinClaim {
    pub batch_id: Uuid,
    pub needs_release: bool,
}
