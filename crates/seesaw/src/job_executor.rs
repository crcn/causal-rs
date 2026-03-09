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

use crate::aggregator::AggregatorRegistry;
use crate::event_store::event_type_short_name;
use crate::handler::{Context, DlqTerminalInfo, EventOutput, GlobalDlqMapper, Handler};
use crate::handler_queue::HandlerQueue;
use crate::handler_registry::HandlerRegistry;
use crate::types::{
    EmittedEvent, EventWorkerConfig, HandlerIntent, HandlerWorkerConfig,
    IntentCommit, PersistedEvent, ProjectionFailure, QueuedHandler, NAMESPACE_SEESAW,
};
use crate::upcaster::UpcasterRegistry;

/// Extracted execution logic for events and handlers.
///
/// This struct consolidates the pure execution logic that was previously
/// scattered across EventWorker and HandlerWorker implementations.
pub struct JobExecutor<D>
where
    D: Send + Sync + 'static,
{
    deps: Arc<D>,
    queue: Arc<dyn HandlerQueue>,
    handlers: Arc<HandlerRegistry<D>>,
    aggregator_registry: Arc<AggregatorRegistry>,
    upcasters: Arc<UpcasterRegistry>,
    global_dlq_mapper: Option<GlobalDlqMapper>,
}

impl<D> JobExecutor<D>
where
    D: Send + Sync + 'static,
{
    /// Create a new job executor.
    pub fn new(
        deps: Arc<D>,
        queue: Arc<dyn HandlerQueue>,
        handlers: Arc<HandlerRegistry<D>>,
        aggregator_registry: Arc<AggregatorRegistry>,
        upcasters: Arc<UpcasterRegistry>,
        global_dlq_mapper: Option<GlobalDlqMapper>,
    ) -> Self {
        Self {
            deps,
            queue,
            handlers,
            aggregator_registry,
            upcasters,
            global_dlq_mapper,
        }
    }

    /// Process a persisted event: create handler intents and run projections.
    ///
    /// All matching handlers become queued handler intents. Only projections
    /// (observers) run inline during event processing.
    ///
    /// Returns an [`IntentCommit`] that the caller enqueues via `HandlerQueue::enqueue`.
    pub async fn process_event(
        &self,
        event: &PersistedEvent,
        _config: &EventWorkerConfig,
    ) -> Result<IntentCommit> {
        info!(
            "Processing event: type={}, correlation={}, position={}",
            event.event_type, event.correlation_id, event.position
        );

        // 1. Decode event via codec (prefer ephemeral sidecar if present)
        let (typed_event, event_type_id) = self.decode_event(&event.event_type, &event.payload, event.ephemeral.as_ref())?;

        // 2. Route to matching handlers
        let matching_handlers: Vec<_> = self
            .handlers
            .all()
            .into_iter()
            .filter(|h| h.can_handle(event_type_id))
            .collect();

        // 3. Call describe() on ALL handlers that have it (not just matching ones).
        //
        // Every event in a correlation may update aggregate state, so we
        // re-run describe for every handler with a describe closure. This
        // ensures that when handler A emits EventB (which updates aggregates),
        // handler A's description is refreshed when EventB is processed —
        // even though handler A doesn't match EventB.
        let mut handler_descriptions = std::collections::HashMap::new();
        for handler in self.handlers.all() {
            if handler.has_describe() {
                let ctx = self.make_context(
                    handler.id.clone(),
                    format!("describe::{}", handler.id),
                    event.correlation_id,
                    event.event_id,
                    event.parent_id,
                );
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    handler.call_describe(&ctx)
                })) {
                    Ok(Some(value)) => {
                        handler_descriptions.insert(handler.id.clone(), value);
                    }
                    Ok(None) => {}
                    Err(_) => {
                        tracing::warn!(
                            handler_id = %handler.id,
                            "describe() panicked, skipping"
                        );
                    }
                }
            }
        }

        // 4. Create queued handler intents for ALL matching handlers
        let hops = event.metadata.get("_hops")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as i32;
        let mut handler_intents = Vec::new();
        for handler in &matching_handlers {
            let execute_at = match handler.delay {
                Some(delay) => {
                    chrono::Utc::now()
                        + chrono::Duration::from_std(delay)
                            .map_err(|_| anyhow::anyhow!("invalid handler delay"))?
                }
                None => chrono::Utc::now(),
            };
            let timeout_seconds = handler
                .timeout
                .map(|d| d.as_secs() as i32)
                .unwrap_or(900)
                .max(1);
            handler_intents.push(HandlerIntent {
                handler_id: handler.id.clone(),
                parent_event_id: event.parent_id,
                execute_at,
                timeout_seconds,
                max_attempts: handler.max_attempts as i32,
                priority: handler.priority.unwrap_or(10),
                hops,
            });
        }

        // 5. Execute projections sequentially (projections are observers, not handlers)
        let mut projection_failures = Vec::new();

        let projections = self.handlers.projections();
        for projection in &projections {
            let any_event = crate::handler::AnyEvent {
                value: typed_event.clone(),
                type_id: event_type_id,
            };
            let idempotency_key = Uuid::new_v5(
                &NAMESPACE_SEESAW,
                format!("{}-{}", event.event_id, projection.id).as_bytes(),
            )
            .to_string();
            let ctx = self.make_context(
                projection.id.clone(),
                idempotency_key,
                event.correlation_id,
                event.event_id,
                event.parent_id,
            );

            if let Err(error) = (projection.handler)(any_event, ctx).await {
                let error_string = error.to_string();
                warn!(
                    "Projection handler failed: event_id={}, projection_id={}, error={}",
                    event.event_id, projection.id, error_string
                );
                projection_failures.push(ProjectionFailure {
                    handler_id: projection.id.clone(),
                    error: error_string,
                    reason: "projection_failed".to_string(),
                    attempts: 1,
                });
            }
        }

        // 6. Return IntentCommit
        Ok(IntentCommit {
            event_id: event.event_id,
            correlation_id: event.correlation_id,
            event_type: event.event_type.clone(),
            event_payload: event.payload.clone(),
            checkpoint: event.position,
            intents: handler_intents,
            projection_failures,
            handler_descriptions,
            park: None,
        })
    }

    /// Execute a queued handler.
    pub async fn execute_handler(
        &self,
        execution: QueuedHandler,
        config: &HandlerWorkerConfig,
    ) -> Result<HandlerResult> {
        info!(
            "Processing handler: handler_id={}, workflow={}, priority={}, attempt={}/{}",
            execution.handler_id,
            execution.correlation_id,
            execution.priority,
            execution.attempts,
            execution.max_attempts
        );

        // 1. Find handler by ID
        let Some(handler) = self.handlers.find_by_id(&execution.handler_id) else {
            let error = format!(
                "No handler registered for id '{}'",
                execution.handler_id
            );
            warn!("{}", error);
            return Ok(HandlerResult {
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

                log_entries: Vec::new(),
            });
        };

        let (typed_event, type_id) =
            self.decode_event(&execution.event_type, &execution.event_payload, execution.ephemeral.as_ref())?;

        let idempotency_key = Uuid::new_v5(
            &NAMESPACE_SEESAW,
            format!("{}-{}", execution.event_id, execution.handler_id).as_bytes(),
        )
        .to_string();

        let journal_entries = self
            .queue
            .load_journal(&handler.id, execution.event_id)
            .await?;

        let ctx = self
            .make_context(
                handler.id.clone(),
                idempotency_key,
                execution.correlation_id,
                execution.event_id,
                execution.parent_event_id,
            )
            .with_journal(self.queue.clone(), journal_entries);

        // 2. Execute with timeout
        let timeout_duration = if execution.timeout_seconds > 0 {
            std::time::Duration::from_secs(execution.timeout_seconds as u64)
        } else {
            config.default_timeout
        };

        let handler_fut = handler.make_handler_future(typed_event.clone(), type_id, ctx.clone());
        let result = timeout(timeout_duration, handler_fut)
        .await;

        // 4. Handle execution result
        match result {
            Ok(Ok(emitted_raw)) => {
                // 5. Serialize emitted events
                let emitted_events = self.serialize_emitted_events(
                    emitted_raw,
                    &execution,
                )?;

                info!("Handler completed successfully: {}", execution.handler_id);
                Ok(HandlerResult {
                    status: HandlerStatus::Success,
                    emitted_events,
                    result: serde_json::json!({ "status": "ok" }),
    
                    log_entries: ctx.logger.drain(),
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
                    if execution.attempts >= execution.max_attempts
                        && (handler.dlq_terminal_mapper.is_some() || self.global_dlq_mapper.is_some())
                    {
                        self.build_dlq_terminal_event(
                            &handler,
                            typed_event,
                            type_id,
                            &execution,
                            "failed",
                            e.to_string(),
                        )?
                    } else {
                        Vec::new()
                    };

                Ok(HandlerResult {
                    status,
                    emitted_events,
                    result: serde_json::json!({}),
    
                    log_entries: ctx.logger.drain(),
                })
            }
            Err(_) => {
                warn!("Handler timed out: {}", execution.handler_id);

                let timeout_error = "Handler execution timed out".to_string();

                let status = if execution.attempts >= execution.max_attempts {
                    HandlerStatus::Failed {
                        error: timeout_error.clone(),
                        attempts: execution.attempts,
                    }
                } else {
                    HandlerStatus::Retry {
                        error: timeout_error.clone(),
                        attempts: execution.attempts,
                    }
                };

                // Try to build DLQ terminal event for timeout
                let emitted_events = if execution.attempts >= execution.max_attempts
                    && (handler.dlq_terminal_mapper.is_some() || self.global_dlq_mapper.is_some())
                {
                    self.build_dlq_terminal_event(
                        &handler,
                        typed_event,
                        type_id,
                        &execution,
                        "timeout",
                        timeout_error,
                    )?
                } else {
                    Vec::new()
                };

                Ok(HandlerResult {
                    status,
                    emitted_events,
                    result: serde_json::json!({}),
    
                    log_entries: ctx.logger.drain(),
                })
            }
        }
    }

    /// Run startup handlers.
    pub async fn run_startup_handlers(&self) -> Result<()> {
        for h in self.handlers.all() {
            if h.started.is_none() {
                continue;
            }

            let ctx = self.make_context(
                h.id.clone(),
                format!("startup::{}", h.id),
                Uuid::nil(),
                Uuid::nil(),
                None,
            );

            h.call_started(ctx)
                .await
                .map_err(|e| anyhow::anyhow!("startup handler '{}' failed: {}", h.id, e))?;
        }
        Ok(())
    }

    /// Get handler registry reference.
    pub fn handler_registry(&self) -> &Arc<HandlerRegistry<D>> {
        &self.handlers
    }

    // --- Private helpers ---

    fn make_context(
        &self,
        handler_id: String,
        idempotency_key: String,
        correlation_id: Uuid,
        event_id: Uuid,
        parent_event_id: Option<Uuid>,
    ) -> Context<D> {
        Context::new(
            handler_id,
            idempotency_key,
            correlation_id,
            event_id,
            parent_event_id,
            self.deps.clone(),
        )
        .with_aggregator_registry(self.aggregator_registry.clone())
    }

    fn decode_event(
        &self,
        event_type: &str,
        payload: &serde_json::Value,
        ephemeral: Option<&Arc<dyn Any + Send + Sync>>,
    ) -> Result<(Arc<dyn Any + Send + Sync>, TypeId)> {
        // Fast path: if the ephemeral sidecar is present and a codec is registered,
        // use the original typed event directly (preserves #[serde(skip)] fields).
        // Skip when upcasters exist — the ephemeral holds the pre-upcasted shape.
        if let Some(typed) = ephemeral {
            if self.upcasters.is_empty() {
                if let Some(codec) = self.handlers.find_codec_by_event_type(event_type) {
                    if (**typed).type_id() == codec.type_id {
                        return Ok((Arc::clone(typed), codec.type_id));
                    }
                }
            }
        }

        // Slow path: deserialize from JSON (replay, hydration, or no ephemeral).
        // Apply upcasters before decoding (schema_version=0 for now — events
        // without a persisted version get the full upcaster chain as a no-op
        // when no upcasters are registered).
        let short_name = event_type_short_name(event_type);
        let upcasted = self.upcasters.upcast(short_name, 0, payload.clone())?;

        let codec = self.handlers.find_codec_by_event_type(event_type);

        if let Some(codec) = codec {
            let typed = (codec.decode)(&upcasted)?;
            Ok((typed, codec.type_id))
        } else {
            warn!(
                event_type = %event_type,
                "No codec registered for event type — falling back to raw JSON. \
                 If this event was emitted by a queued handler, ensure the \
                 receiving handler is registered with the engine."
            );
            Ok((Arc::new(upcasted), TypeId::of::<serde_json::Value>()))
        }
    }

    pub(crate) fn serialize_emitted_events(
        &self,
        emitted: Vec<EventOutput>,
        execution: &QueuedHandler,
    ) -> Result<Vec<EmittedEvent>> {
        let mut result = Vec::with_capacity(emitted.len());
        for output in emitted {
            // Auto-register codec so the event can be decoded in the next dispatch cycle
            if let Some(codec) = &output.codec {
                self.handlers.register_codec(codec.clone());
            }

            result.push(EmittedEvent {
                event_type: output.event_type,
                payload: output.payload,
                handler_id: Some(execution.handler_id.clone()),
                ephemeral: output.ephemeral,
            });
        }

        Ok(result)
    }

    fn build_dlq_terminal_event(
        &self,
        handler: &Handler<D>,
        source_event: Arc<dyn Any + Send + Sync>,
        source_type_id: TypeId,
        execution: &QueuedHandler,
        reason: &str,
        error: String,
    ) -> Result<Vec<EmittedEvent>> {
        let Some(mapper) = handler.dlq_terminal_mapper.as_ref() else {
            // Global fallback
            if let Some(global) = self.global_dlq_mapper.as_ref() {
                let mut emitted = global(DlqTerminalInfo {
                    handler_id: execution.handler_id.clone(),
                    source_event_type: event_type_short_name(&execution.event_type).to_string(),
                    source_event_id: execution.event_id,
                    error,
                    reason: reason.to_string(),
                    attempts: execution.attempts,
                    max_attempts: execution.max_attempts,
                })?;
                if emitted.handler_id.is_none() {
                    emitted.handler_id = Some(execution.handler_id.clone());
                }
                return Ok(vec![emitted]);
            }
            return Ok(Vec::new());
        };

        let mut emitted = mapper(
            source_event,
            source_type_id,
            DlqTerminalInfo {
                handler_id: execution.handler_id.clone(),
                source_event_type: event_type_short_name(&execution.event_type).to_string(),
                source_event_id: execution.event_id,
                error,
                reason: reason.to_string(),
                attempts: execution.attempts,
                max_attempts: execution.max_attempts,
            },
        )?;

        // Ensure handler_id is set for causal tracking
        if emitted.handler_id.is_none() {
            emitted.handler_id = Some(execution.handler_id.clone());
        }

        Ok(vec![emitted])
    }
}

/// Result of handler execution.
#[derive(Debug)]
pub struct HandlerResult {
    pub status: HandlerStatus,
    pub emitted_events: Vec<EmittedEvent>,
    pub result: serde_json::Value,
    /// Log entries captured during handler execution.
    pub log_entries: Vec<crate::types::LogEntry>,
}

/// Handler execution status.
#[derive(Debug)]
pub enum HandlerStatus {
    Success,
    Failed { error: String, attempts: i32 },
    Retry { error: String, attempts: i32 },
    Timeout,
}
