//! JobExecutor - extracted reactor execution logic from workers
//!
//! This consolidates the event processing and reactor execution logic
//! that was previously embedded in EventWorker and HandlerWorker.

use anyhow::Result;
use std::any::{Any, TypeId};
use std::sync::Arc;
use tokio::time::timeout;
use tracing::{info, warn};
use uuid::Uuid;

use crate::aggregator::AggregatorRegistry;
use crate::reactor::{Context, DlqTerminalInfo, EventOutput, GlobalDlqMapper, Reactor};
use crate::reactor_queue::ReactorQueue;
use crate::reactor_registry::ReactorRegistry;
use crate::types::{
    EmittedEvent, EventWorkerConfig, ReactorIntent, ReactorWorkerConfig,
    IntentCommit, PersistedEvent, ProjectionFailure, QueuedReactor, NAMESPACE_CAUSAL,
};
use crate::upcaster::UpcasterRegistry;

/// Extracted execution logic for events and reactors.
///
/// This struct consolidates the pure execution logic that was previously
/// scattered across EventWorker and HandlerWorker implementations.
pub struct JobExecutor<D>
where
    D: Send + Sync + 'static,
{
    deps: Arc<D>,
    queue: Arc<dyn ReactorQueue>,
    reactors: Arc<ReactorRegistry<D>>,
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
        queue: Arc<dyn ReactorQueue>,
        reactors: Arc<ReactorRegistry<D>>,
        aggregator_registry: Arc<AggregatorRegistry>,
        upcasters: Arc<UpcasterRegistry>,
        global_dlq_mapper: Option<GlobalDlqMapper>,
    ) -> Self {
        Self {
            deps,
            queue,
            reactors,
            aggregator_registry,
            upcasters,
            global_dlq_mapper,
        }
    }

    /// Process a persisted event: create reactor intents and run projections.
    ///
    /// All matching reactors become queued reactor intents. Only projections
    /// (observers) run inline during event processing.
    ///
    /// Returns an [`IntentCommit`] that the caller enqueues via `ReactorQueue::enqueue`.
    /// Process an event: decode, route to reactors, build intents, run projections.
    ///
    /// When `skip_projections` is true, projections are not executed. This is
    /// used for ephemeral events which route through reactors but skip
    /// persistence, aggregators, and projections.
    pub async fn process_event(
        &self,
        event: &PersistedEvent,
        _config: &EventWorkerConfig,
    ) -> Result<IntentCommit> {
        self.process_event_inner(event, _config, false).await
    }

    pub async fn process_event_inner(
        &self,
        event: &PersistedEvent,
        _config: &EventWorkerConfig,
        skip_projections: bool,
    ) -> Result<IntentCommit> {
        info!(
            "Processing event: type={}, correlation={}, position={}",
            event.event_type, event.correlation_id, event.position
        );

        // 1. Decode event via codec (prefer ephemeral sidecar if present)
        let (typed_event, event_type_id) = self.decode_event(&event.event_type, &event.payload, event.ephemeral.as_ref())?;

        // 2. Route to matching reactors
        let matching_handlers: Vec<_> = self
            .reactors
            .all()
            .into_iter()
            .filter(|h| h.can_handle(event_type_id))
            .collect();

        // 3. Call describe() on ALL reactors that have it (not just matching ones).
        //
        // Every event in a correlation may update aggregate state, so we
        // re-run describe for every reactor with a describe closure. This
        // ensures that when reactor A emits EventB (which updates aggregates),
        // reactor A's description is refreshed when EventB is processed —
        // even though reactor A doesn't match EventB.
        let mut reactor_descriptions = std::collections::HashMap::new();
        for reactor in self.reactors.all() {
            if reactor.has_describe() {
                let ctx = self.make_context(
                    reactor.id.clone(),
                    format!("describe::{}", reactor.id),
                    event.correlation_id,
                    event.event_id,
                    event.parent_id,
                );
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    reactor.call_describe(&ctx)
                })) {
                    Ok(Some(value)) => {
                        reactor_descriptions.insert(reactor.id.clone(), value);
                    }
                    Ok(None) => {}
                    Err(_) => {
                        tracing::warn!(
                            reactor_id = %reactor.id,
                            "describe() panicked, skipping"
                        );
                    }
                }
            }
        }

        // 4. Create queued reactor intents for ALL matching reactors
        let hops = event.metadata.get("_hops")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as i32;
        let mut handler_intents = Vec::new();
        for reactor in &matching_handlers {
            let execute_at = match reactor.delay {
                Some(delay) => {
                    chrono::Utc::now()
                        + chrono::Duration::from_std(delay)
                            .map_err(|_| anyhow::anyhow!("invalid reactor delay"))?
                }
                None => chrono::Utc::now(),
            };
            let timeout_seconds = reactor
                .timeout
                .map(|d| d.as_secs() as i32)
                .unwrap_or(900)
                .max(1);
            handler_intents.push(ReactorIntent {
                reactor_id: reactor.id.clone(),
                parent_event_id: event.parent_id,
                execute_at,
                timeout_seconds,
                max_attempts: reactor.max_attempts as i32,
                priority: reactor.priority.unwrap_or(10),
                hops,
            });
        }

        // 5. Execute projections sequentially (projections are observers, not reactors)
        //    Skipped for ephemeral events.
        let mut projection_failures = Vec::new();

        let projections = if skip_projections { Vec::new() } else { self.reactors.projections() };
        for projection in &projections {
            let any_event = crate::reactor::AnyEvent {
                value: typed_event.clone(),
                type_id: event_type_id,
            };
            let idempotency_key = Uuid::new_v5(
                &NAMESPACE_CAUSAL,
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

            if let Err(error) = (projection.reactor)(any_event, ctx).await {
                let error_string = error.to_string();
                warn!(
                    "Projection reactor failed: event_id={}, projection_id={}, error={}",
                    event.event_id, projection.id, error_string
                );
                projection_failures.push(ProjectionFailure {
                    reactor_id: projection.id.clone(),
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
            reactor_descriptions,
            park: None,
        })
    }

    /// Execute a queued reactor.
    pub async fn execute_reactor(
        &self,
        execution: QueuedReactor,
        config: &ReactorWorkerConfig,
    ) -> Result<ReactorResult> {
        info!(
            "Processing reactor: reactor_id={}, workflow={}, priority={}, attempt={}/{}",
            execution.reactor_id,
            execution.correlation_id,
            execution.priority,
            execution.attempts,
            execution.max_attempts
        );

        // 1. Find reactor by ID
        let Some(reactor) = self.reactors.find_by_id(&execution.reactor_id) else {
            let error = format!(
                "No reactor registered for id '{}'",
                execution.reactor_id
            );
            warn!("{}", error);
            return Ok(ReactorResult {
                status: if execution.attempts >= execution.max_attempts {
                    ReactorStatus::Failed {
                        error: error.clone(),
                        attempts: execution.attempts,
                    }
                } else {
                    ReactorStatus::Retry {
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
            &NAMESPACE_CAUSAL,
            format!("{}-{}", execution.event_id, execution.reactor_id).as_bytes(),
        )
        .to_string();

        let journal_entries = self
            .queue
            .load_journal(&reactor.id, execution.event_id)
            .await?;

        let ctx = self
            .make_context(
                reactor.id.clone(),
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

        let handler_fut = reactor.make_handler_future(typed_event.clone(), type_id, ctx.clone());
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

                info!("Reactor completed successfully: {}", execution.reactor_id);
                Ok(ReactorResult {
                    status: ReactorStatus::Success,
                    emitted_events,
                    result: serde_json::json!({ "status": "ok" }),
    
                    log_entries: ctx.logger.drain(),
                })
            }
            Ok(Err(e)) => {
                warn!(
                    "Reactor failed: {} (attempt {}/{}): {}",
                    execution.reactor_id, execution.attempts, execution.max_attempts, e
                );

                let status = if execution.attempts >= execution.max_attempts {
                    ReactorStatus::Failed {
                        error: e.to_string(),
                        attempts: execution.attempts,
                    }
                } else {
                    ReactorStatus::Retry {
                        error: e.to_string(),
                        attempts: execution.attempts,
                    }
                };

                // Try to build DLQ terminal event
                let emitted_events =
                    if execution.attempts >= execution.max_attempts
                        && (reactor.dlq_terminal_mapper.is_some() || self.global_dlq_mapper.is_some())
                    {
                        self.build_dlq_terminal_event(
                            &reactor,
                            typed_event,
                            type_id,
                            &execution,
                            "failed",
                            e.to_string(),
                        )?
                    } else {
                        Vec::new()
                    };

                Ok(ReactorResult {
                    status,
                    emitted_events,
                    result: serde_json::json!({}),
    
                    log_entries: ctx.logger.drain(),
                })
            }
            Err(_) => {
                warn!("Reactor timed out: {}", execution.reactor_id);

                let timeout_error = "Reactor execution timed out".to_string();

                let status = if execution.attempts >= execution.max_attempts {
                    ReactorStatus::Failed {
                        error: timeout_error.clone(),
                        attempts: execution.attempts,
                    }
                } else {
                    ReactorStatus::Retry {
                        error: timeout_error.clone(),
                        attempts: execution.attempts,
                    }
                };

                // Try to build DLQ terminal event for timeout
                let emitted_events = if execution.attempts >= execution.max_attempts
                    && (reactor.dlq_terminal_mapper.is_some() || self.global_dlq_mapper.is_some())
                {
                    self.build_dlq_terminal_event(
                        &reactor,
                        typed_event,
                        type_id,
                        &execution,
                        "timeout",
                        timeout_error,
                    )?
                } else {
                    Vec::new()
                };

                Ok(ReactorResult {
                    status,
                    emitted_events,
                    result: serde_json::json!({}),
    
                    log_entries: ctx.logger.drain(),
                })
            }
        }
    }

    /// Run startup reactors.
    pub async fn run_startup_reactors(&self) -> Result<()> {
        for h in self.reactors.all() {
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
                .map_err(|e| anyhow::anyhow!("startup reactor '{}' failed: {}", h.id, e))?;
        }
        Ok(())
    }

    /// Get reactor registry reference.
    pub fn reactor_registry(&self) -> &Arc<ReactorRegistry<D>> {
        &self.reactors
    }

    // --- Private helpers ---

    fn make_context(
        &self,
        reactor_id: String,
        idempotency_key: String,
        correlation_id: Uuid,
        event_id: Uuid,
        parent_event_id: Option<Uuid>,
    ) -> Context<D> {
        Context::new(
            reactor_id,
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
                if let Some(codec) = self.reactors.find_codec_by_durable_name(event_type) {
                    if (**typed).type_id() == codec.type_id {
                        return Ok((Arc::clone(typed), codec.type_id));
                    }
                }
            }
        }

        // Slow path: deserialize from JSON (replay, hydration, or no ephemeral).
        // Apply upcasters before decoding.
        let upcasted = self.upcasters.upcast(event_type, 0, payload.clone())?;

        let codec = self.reactors.find_codec_by_durable_name(event_type);

        if let Some(codec) = codec {
            let typed = (codec.decode)(&upcasted)?;
            Ok((typed, codec.type_id))
        } else {
            warn!(
                event_type = %event_type,
                "No codec registered for event type — falling back to raw JSON. \
                 If this event was emitted by a queued reactor, ensure the \
                 receiving reactor is registered with the engine."
            );
            Ok((Arc::new(upcasted), TypeId::of::<serde_json::Value>()))
        }
    }

    pub(crate) fn serialize_emitted_events(
        &self,
        emitted: Vec<EventOutput>,
        execution: &QueuedReactor,
    ) -> Result<Vec<EmittedEvent>> {
        let mut result = Vec::with_capacity(emitted.len());
        for output in emitted {
            // Auto-register codec so the event can be decoded in the next dispatch cycle
            if let Some(codec) = &output.codec {
                self.reactors.register_codec(codec.clone());
            }

            result.push(EmittedEvent {
                durable_name: output.durable_name,
                event_prefix: output.event_prefix,
                persistent: output.persistent,
                payload: output.payload,
                reactor_id: Some(execution.reactor_id.clone()),
                ephemeral: output.ephemeral,
            });
        }

        Ok(result)
    }

    fn build_dlq_terminal_event(
        &self,
        reactor: &Reactor<D>,
        source_event: Arc<dyn Any + Send + Sync>,
        source_type_id: TypeId,
        execution: &QueuedReactor,
        reason: &str,
        error: String,
    ) -> Result<Vec<EmittedEvent>> {
        let Some(mapper) = reactor.dlq_terminal_mapper.as_ref() else {
            // Global fallback
            if let Some(global) = self.global_dlq_mapper.as_ref() {
                let mut emitted = global(DlqTerminalInfo {
                    reactor_id: execution.reactor_id.clone(),
                    source_event_type: execution.event_type.clone(),
                    source_event_id: execution.event_id,
                    error,
                    reason: reason.to_string(),
                    attempts: execution.attempts,
                    max_attempts: execution.max_attempts,
                })?;
                if emitted.reactor_id.is_none() {
                    emitted.reactor_id = Some(execution.reactor_id.clone());
                }
                return Ok(vec![emitted]);
            }
            return Ok(Vec::new());
        };

        let mut emitted = mapper(
            source_event,
            source_type_id,
            DlqTerminalInfo {
                reactor_id: execution.reactor_id.clone(),
                source_event_type: execution.event_type.clone(),
                source_event_id: execution.event_id,
                error,
                reason: reason.to_string(),
                attempts: execution.attempts,
                max_attempts: execution.max_attempts,
            },
        )?;

        // Ensure reactor_id is set for causal tracking
        if emitted.reactor_id.is_none() {
            emitted.reactor_id = Some(execution.reactor_id.clone());
        }

        Ok(vec![emitted])
    }
}

/// Result of reactor execution.
#[derive(Debug)]
pub struct ReactorResult {
    pub status: ReactorStatus,
    pub emitted_events: Vec<EmittedEvent>,
    pub result: serde_json::Value,
    /// Log entries captured during reactor execution.
    pub log_entries: Vec<crate::types::LogEntry>,
}

/// Reactor execution status.
#[derive(Debug)]
pub enum ReactorStatus {
    Success,
    Failed { error: String, attempts: i32 },
    Retry { error: String, attempts: i32 },
    Timeout,
}
