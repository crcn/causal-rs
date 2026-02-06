//! Runtime - worker management for queue-backed engine

pub mod effect_worker;
pub mod event_worker;

use anyhow::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::task::JoinHandle;
use tracing::info;

use crate::Store;
use effect_worker::{EffectWorker, EffectWorkerConfig};
use event_worker::{EventWorker, EventWorkerConfig};

/// Runtime configuration
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    /// Number of event workers to spawn
    pub event_workers: usize,
    /// Number of effect workers to spawn
    pub effect_workers: usize,
    /// Event worker configuration
    pub event_worker_config: EventWorkerConfig,
    /// Effect worker configuration
    pub effect_worker_config: EffectWorkerConfig,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            event_workers: 2,
            effect_workers: 4,
            event_worker_config: EventWorkerConfig::default(),
            effect_worker_config: EffectWorkerConfig::default(),
        }
    }
}

/// Runtime - manages event and effect workers
pub struct Runtime {
    handles: Vec<JoinHandle<Result<()>>>,
    shutdown: Arc<AtomicBool>,
}

impl Runtime {
    /// Start runtime with engine
    pub fn start<S, D, St>(
        engine: &crate::engine_v2::Engine<S, D, St>,
        config: RuntimeConfig,
    ) -> Self
    where
        S: Clone + Send + Sync + serde::Serialize + serde::de::DeserializeOwned + Default + 'static,
        D: Send + Sync + 'static,
        St: Store,
    {
        info!(
            "Starting runtime: {} event workers, {} effect workers",
            config.event_workers, config.effect_workers
        );

        let mut handles = Vec::new();
        let shutdown = Arc::new(AtomicBool::new(false));

        // Spawn event workers
        for i in 0..config.event_workers {
            let worker = EventWorker::new(
                engine.store().clone(),
                engine.deps().clone(),
                engine.reducers().clone(),
                engine.effects().clone(),
                config.event_worker_config.clone(),
            )
            .with_shutdown(shutdown.clone());

            let handle = tokio::spawn(async move {
                info!("Event worker {} started", i);
                worker.run().await
            });

            handles.push(handle);
        }

        // Spawn effect workers
        for i in 0..config.effect_workers {
            let worker = EffectWorker::new(
                engine.store().clone(),
                engine.deps().clone(),
                engine.reducers().clone(),
                engine.effects().clone(),
                config.effect_worker_config.clone(),
            )
            .with_shutdown(shutdown.clone());

            let handle = tokio::spawn(async move {
                info!("Effect worker {} started", i);
                worker.run().await
            });

            handles.push(handle);
        }

        Self { handles, shutdown }
    }

    /// Shutdown runtime (waits for all workers to complete)
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

    /// Get number of running workers
    pub fn worker_count(&self) -> usize {
        self.handles.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use futures::Stream;
    use std::time::Duration;
    use uuid::Uuid;

    #[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
    struct TestState;

    #[derive(Clone, Default)]
    struct TestDeps;

    #[derive(Clone)]
    struct TestStore;

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
    async fn runtime_shutdown_stops_workers_cooperatively() {
        let engine = crate::Engine::<TestState, TestDeps, TestStore>::new(TestDeps, TestStore);
        let runtime = Runtime::start(
            &engine,
            RuntimeConfig {
                event_workers: 1,
                effect_workers: 1,
                event_worker_config: event_worker::EventWorkerConfig {
                    poll_interval: Duration::from_millis(10),
                    ..Default::default()
                },
                effect_worker_config: effect_worker::EffectWorkerConfig {
                    poll_interval: Duration::from_millis(10),
                    ..Default::default()
                },
            },
        );

        tokio::time::sleep(Duration::from_millis(30)).await;
        runtime.shutdown().await.expect("runtime should shut down");
    }
}
