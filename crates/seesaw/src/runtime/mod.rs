//! Runtime - worker management for Backend-backed engine.

pub mod event_worker;
// TODO: Re-enable once tokio signal feature is added
// pub mod graceful_shutdown;
pub mod handler_worker;

use anyhow::Result;
use std::sync::Arc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::backend::{Backend, BackendServeConfig};
use crate::backend::job_executor::JobExecutor;

/// Runtime - manages backend workers via spawned task.
pub struct Runtime {
    handles: Vec<JoinHandle<Result<()>>>,
    shutdown: CancellationToken,
}

impl Runtime {
    /// Start runtime with engine (non-blocking).
    ///
    /// Spawns backend.serve() in background and returns handle for shutdown.
    pub async fn start<D, B>(
        engine: crate::engine_v2::Engine<D, B>,
        config: BackendServeConfig,
    ) -> Result<Self>
    where
        D: Send + Sync + 'static,
        B: Backend,
    {
        info!(
            "Starting runtime: {} event workers, {} handler workers",
            config.event_workers, config.handler_workers
        );

        let shutdown = CancellationToken::new();
        let executor = Arc::new(JobExecutor::new(
            engine.deps().clone(),
            engine.effects().clone(),
        ));

        // Run startup handlers before spawning workers
        executor.run_startup_handlers().await?;

        // Spawn backend.serve() in background
        let backend = engine.backend().clone();
        let shutdown_clone = shutdown.clone();
        let handle = tokio::spawn(async move {
            backend.serve(executor, config, shutdown_clone).await
        });

        Ok(Self {
            handles: vec![handle],
            shutdown,
        })
    }

    /// Shutdown runtime (cancels token and waits for workers).
    pub async fn shutdown(self) -> Result<()> {
        info!("Shutting down runtime...");

        self.shutdown.cancel();

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

    /// Get number of running workers (always 1 - the backend.serve() task).
    pub fn worker_count(&self) -> usize {
        self.handles.len()
    }
}
