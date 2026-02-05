//! Runtime - worker management for queue-backed engine

pub mod event_worker;
pub mod effect_worker;

use anyhow::Result;
use std::sync::Arc;
use tokio::task::JoinHandle;
use tracing::info;

use crate::Store;
use event_worker::{EventWorker, EventWorkerConfig};
use effect_worker::{EffectWorker, EffectWorkerConfig};

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
}

impl Runtime {
    /// Start runtime with engine
    pub fn start<S, D, St>(
        engine: &crate::engine_v2::Engine<S, D, St>,
        config: RuntimeConfig,
    ) -> Self
    where
        S: Clone + Send + Sync + 'static,
        D: Send + Sync + 'static,
        St: Store,
    {
        info!(
            "Starting runtime: {} event workers, {} effect workers",
            config.event_workers, config.effect_workers
        );

        let mut handles = Vec::new();

        // Spawn event workers
        for i in 0..config.event_workers {
            let worker = EventWorker::new(
                engine.store().clone(),
                engine.deps().clone(),
                engine.reducers().clone(),
                engine.effects().clone(),
                config.event_worker_config.clone(),
            );

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
                engine.effects().clone(),
                config.effect_worker_config.clone(),
            );

            let handle = tokio::spawn(async move {
                info!("Effect worker {} started", i);
                worker.run().await
            });

            handles.push(handle);
        }

        Self { handles }
    }

    /// Shutdown runtime (waits for all workers to complete)
    pub async fn shutdown(self) -> Result<()> {
        info!("Shutting down runtime...");

        for handle in self.handles {
            handle.abort();
        }

        info!("Runtime shutdown complete");
        Ok(())
    }

    /// Get number of running workers
    pub fn worker_count(&self) -> usize {
        self.handles.len()
    }
}
