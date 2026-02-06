//! Runtime - worker management for queue-backed engine.

pub mod event_worker;
pub mod handler_worker;

use anyhow::{Context, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::task::JoinHandle;
use tracing::info;
use uuid::Uuid;

use crate::handler::HandlerContext;
use crate::Store;
use event_worker::{EventWorker, EventWorkerConfig};
use handler_worker::{HandlerWorker, HandlerWorkerConfig};

/// Runtime configuration.
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    /// Number of event workers to spawn.
    pub event_workers: usize,
    /// Number of effect workers to spawn.
    pub handler_workers: usize,
    /// Event worker configuration.
    pub event_worker_config: EventWorkerConfig,
    /// Handler worker configuration.
    pub handler_worker_config: HandlerWorkerConfig,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            event_workers: 2,
            handler_workers: 4,
            event_worker_config: EventWorkerConfig::default(),
            handler_worker_config: HandlerWorkerConfig::default(),
        }
    }
}

/// Runtime - manages event and effect workers.
pub struct Runtime {
    handles: Vec<JoinHandle<Result<()>>>,
    shutdown: Arc<AtomicBool>,
}

impl Runtime {
    /// Start runtime with engine.
    pub async fn start<D, St>(
        engine: &crate::engine_v2::Engine<D, St>,
        config: RuntimeConfig,
    ) -> Result<Self>
    where
        D: Send + Sync + 'static,
        St: Store,
    {
        info!(
            "Starting runtime: {} event workers, {} effect workers",
            config.event_workers, config.handler_workers
        );

        let mut handles = Vec::new();
        let shutdown = Arc::new(AtomicBool::new(false));
        let queue_backend = engine.queue_backend();

        for effect in engine.effects().all() {
            if effect.started.is_none() {
                continue;
            }

            let deps = engine.deps().clone();
            let effect_id = effect.id.clone();
            let ctx = HandlerContext::new(
                effect_id.clone(),
                format!("startup::{}", effect_id),
                Uuid::nil(),
                Uuid::nil(),
                deps,
            );

            effect
                .call_started(ctx)
                .await
                .with_context(|| format!("startup handler for effect '{}' failed", effect_id))?;
        }

        for i in 0..config.event_workers {
            let worker = EventWorker::new(
                engine.store().clone(),
                engine.deps().clone(),
                engine.effects().clone(),
                config.event_worker_config.clone(),
            )
            .with_queue_backend(queue_backend.clone())
            .with_shutdown(shutdown.clone());

            let handle = tokio::spawn(async move {
                info!("Event worker {} started", i);
                worker.run().await
            });

            handles.push(handle);
        }

        for i in 0..config.handler_workers {
            let worker = HandlerWorker::new(
                engine.store().clone(),
                engine.deps().clone(),
                engine.effects().clone(),
                config.handler_worker_config.clone(),
            )
            .with_queue_backend(queue_backend.clone())
            .with_shutdown(shutdown.clone());

            let handle = tokio::spawn(async move {
                info!("Handler worker {} started", i);
                worker.run().await
            });

            handles.push(handle);
        }

        Ok(Self { handles, shutdown })
    }

    /// Shutdown runtime (waits for all workers to complete).
    pub async fn shutdown(self) -> Result<()> {
        info!("Shutting down runtime...");

        self.shutdown.store(true, Ordering::SeqCst);

        for handle in self.handles {
            match handle.await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    info!("Worker exited with error during shutdown: {}", error);
                }
                Err(join_error) => {
                    info!("Worker task join error during shutdown: {}", join_error);
                }
            }
        }

        info!("Runtime shutdown complete");
        Ok(())
    }

    /// Get number of running workers.
    pub fn worker_count(&self) -> usize {
        self.handles.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::{on_any, HandlerContext};
    use crate::{
        EmittedEvent, QueuedEvent, QueuedHandlerExecution, Store, WorkflowEvent, WorkflowStatus,
    };
    use anyhow::Result;
    use async_trait::async_trait;
    use chrono::{DateTime, Utc};
    use futures::stream;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use uuid::Uuid;

    #[derive(Clone)]
    struct TestDeps;

    struct NoopStore;

    #[async_trait]
    impl Store for NoopStore {
        async fn publish(&self, _event: QueuedEvent) -> Result<()> {
            Ok(())
        }

        async fn poll_next(&self) -> Result<Option<QueuedEvent>> {
            Ok(None)
        }

        async fn ack(&self, _id: i64) -> Result<()> {
            Ok(())
        }

        async fn nack(&self, _id: i64, _retry_after_secs: u64) -> Result<()> {
            Ok(())
        }

        async fn commit_event_processing(
            &self,
            _commit: crate::EventProcessingCommit,
        ) -> Result<()> {
            Ok(())
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
            _execute_at: DateTime<Utc>,
            _timeout_seconds: i32,
            _max_attempts: i32,
            _priority: i32,
            _join_window_timeout_seconds: Option<i32>,
        ) -> Result<()> {
            Ok(())
        }

        async fn poll_next_effect(&self) -> Result<Option<QueuedHandlerExecution>> {
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
            _emitted_events: Vec<EmittedEvent>,
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

        async fn subscribe_workflow_events(
            &self,
            _correlation_id: Uuid,
        ) -> Result<Box<dyn futures::Stream<Item = WorkflowEvent> + Send + Unpin>> {
            Ok(Box::new(Box::pin(stream::empty())))
        }

        async fn get_workflow_status(&self, correlation_id: Uuid) -> Result<WorkflowStatus> {
            Ok(WorkflowStatus {
                correlation_id,
                pending_effects: 0,
                is_settled: true,
                last_event: None,
            })
        }
    }

    #[tokio::test]
    async fn runtime_start_runs_started_handlers_once() {
        let started_calls = Arc::new(AtomicUsize::new(0));
        let started_calls_clone = started_calls.clone();

        let engine = crate::Engine::new(TestDeps, NoopStore).with_handler(
            on_any()
                .id("startup_probe")
                .started(move |_ctx: HandlerContext<TestDeps>| {
                    let started_calls = started_calls_clone.clone();
                    async move {
                        started_calls.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    }
                })
                .then(|_, _| async move { Ok(()) }),
        );

        let runtime = Runtime::start(
            &engine,
            RuntimeConfig {
                event_workers: 0,
                handler_workers: 0,
                ..Default::default()
            },
        )
        .await
        .expect("runtime start should succeed");

        assert_eq!(
            started_calls.load(Ordering::SeqCst),
            1,
            "expected started handler to run exactly once on runtime start"
        );

        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn runtime_start_fails_when_started_handler_fails() {
        let engine = crate::Engine::new(TestDeps, NoopStore).with_handler(
            on_any()
                .id("startup_fail")
                .started(|_ctx: HandlerContext<TestDeps>| async move {
                    Err(anyhow::anyhow!("startup failed"))
                })
                .then(|_, _| async move { Ok(()) }),
        );

        let startup_result = Runtime::start(
            &engine,
            RuntimeConfig {
                event_workers: 0,
                handler_workers: 0,
                ..Default::default()
            },
        )
        .await;

        let error = match startup_result {
            Ok(_) => panic!("runtime startup should fail when a started hook errors"),
            Err(error) => error,
        };

        assert!(
            error.to_string().contains("startup_fail"),
            "startup error should include failing effect id: {error}"
        );
    }
}
