//! Graceful shutdown for Seesaw workers
//!
//! Handles SIGTERM/SIGINT signals and ensures in-flight tasks complete before exit.

use anyhow::{anyhow, Result};
use std::sync::Arc;
use std::time::Duration;
#[cfg(unix)]
use tokio::signal::unix::{signal, SignalKind};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

/// Graceful shutdown coordinator
pub struct GracefulShutdown {
    /// Cancellation token shared across all workers
    token: CancellationToken,
    /// Maximum time to wait for in-flight tasks
    drain_timeout: Duration,
}

impl GracefulShutdown {
    /// Create new graceful shutdown coordinator
    pub fn new(drain_timeout: Duration) -> Self {
        Self {
            token: CancellationToken::new(),
            drain_timeout,
        }
    }

    /// Get a cancellation token for workers to monitor
    pub fn token(&self) -> CancellationToken {
        self.token.clone()
    }

    /// Install signal handlers and wait for shutdown signal
    #[cfg(unix)]
    pub async fn wait_for_signal(&self) {
        let token = self.token.clone();

        tokio::spawn(async move {
            let mut sigterm = match signal(SignalKind::terminate()) {
                Ok(sig) => sig,
                Err(e) => {
                    error!("Failed to register SIGTERM handler: {}", e);
                    return;
                }
            };

            let mut sigint = match signal(SignalKind::interrupt()) {
                Ok(sig) => sig,
                Err(e) => {
                    error!("Failed to register SIGINT handler: {}", e);
                    return;
                }
            };

            tokio::select! {
                _ = sigterm.recv() => {
                    info!("SIGTERM received, initiating graceful shutdown");
                }
                _ = sigint.recv() => {
                    info!("SIGINT received, initiating graceful shutdown");
                }
            }

            token.cancel();
        });
    }

    /// Install signal handlers and wait for shutdown signal (non-Unix fallback)
    #[cfg(not(unix))]
    pub async fn wait_for_signal(&self) {
        // On non-Unix systems, just wait for the token to be cancelled externally
        self.token.cancelled().await;
    }

    /// Drain remaining tasks with timeout
    pub async fn drain_tasks<T: 'static>(&self, mut tasks: JoinSet<Result<T>>) -> Result<()> {
        if tasks.is_empty() {
            return Ok(());
        }

        let count = tasks.len();
        info!(
            "Draining {} in-flight tasks (timeout: {:?})",
            count, self.drain_timeout
        );

        let drain = async {
            let mut completed = 0;
            let mut errors = 0;

            while let Some(joined) = tasks.join_next().await {
                match joined {
                    Ok(Ok(_)) => {
                        completed += 1;
                    }
                    Ok(Err(e)) => {
                        error!("Task failed during drain: {}", e);
                        errors += 1;
                    }
                    Err(e) => {
                        error!("Task panicked during drain: {}", e);
                        errors += 1;
                    }
                }
            }

            info!(
                "Drain complete: {}/{} tasks completed, {} errors",
                completed, count, errors
            );

            if errors > 0 {
                Err(anyhow!("{} tasks failed during drain", errors))
            } else {
                Ok(())
            }
        };

        match tokio::time::timeout(self.drain_timeout, drain).await {
            Ok(result) => result,
            Err(_) => {
                let remaining = tasks.len();
                error!("Drain timeout exceeded, {} tasks abandoned", remaining);
                tasks.abort_all();
                Err(anyhow!(
                    "Graceful shutdown timeout exceeded ({} tasks abandoned)",
                    remaining
                ))
            }
        }
    }

    /// Check if shutdown has been requested
    pub fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }
}

impl Default for GracefulShutdown {
    fn default() -> Self {
        Self::new(Duration::from_secs(30))
    }
}

/// Worker with graceful shutdown support
pub struct ShutdownAwareWorker {
    shutdown: Arc<GracefulShutdown>,
    max_in_flight: usize,
}

impl ShutdownAwareWorker {
    pub fn new(shutdown: Arc<GracefulShutdown>, max_in_flight: usize) -> Self {
        Self {
            shutdown,
            max_in_flight,
        }
    }

    /// Run worker loop with graceful shutdown
    pub async fn run<F, Fut, T>(&self, mut poll_work: F) -> Result<()>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<Option<T>>>,
        T: Send + 'static,
    {
        let mut tasks: JoinSet<Result<()>> = JoinSet::new();
        let token = self.shutdown.token();

        loop {
            tokio::select! {
                // Shutdown signal received
                _ = token.cancelled() => {
                    info!("Shutdown signal received, stopping new work");
                    break;
                }

                // Task completed
                Some(joined) = tasks.join_next(), if !tasks.is_empty() => {
                    match joined {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => {
                            error!("Task failed: {}", e);
                        }
                        Err(e) => {
                            error!("Task panicked: {}", e);
                        }
                    }
                }

                // Poll for new work (if capacity available)
                polled = poll_work(), if tasks.len() < self.max_in_flight && !self.shutdown.is_cancelled() => {
                    match polled {
                        Ok(Some(work)) => {
                            // Spawn task (work is the actual closure to execute)
                            // Note: caller must pass a closure that executes the work
                            // This is a simplified example - actual implementation
                            // would need to be more specific to the work type
                            info!("Work received, spawning task");
                        }
                        Ok(None) => {
                            // No work available, sleep briefly
                            tokio::time::sleep(Duration::from_millis(50)).await;
                        }
                        Err(e) => {
                            error!("Error polling work: {}", e);
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }
                    }
                }
            }
        }

        // Drain remaining tasks
        self.shutdown.drain_tasks(tasks).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_graceful_shutdown_completes_tasks() {
        let shutdown = Arc::new(GracefulShutdown::new(Duration::from_secs(5)));

        let mut tasks = JoinSet::new();
        for i in 0..10 {
            tasks.spawn(async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                Ok::<_, anyhow::Error>(())
            });
        }

        // Trigger shutdown
        shutdown.token().cancel();

        // Drain should complete all tasks
        let result = shutdown.drain_tasks(tasks).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_graceful_shutdown_timeout() {
        let shutdown = Arc::new(GracefulShutdown::new(Duration::from_millis(100)));

        let mut tasks = JoinSet::new();
        tasks.spawn(async {
            // Task that takes too long
            tokio::time::sleep(Duration::from_secs(10)).await;
            Ok::<_, anyhow::Error>(())
        });

        // Trigger shutdown
        shutdown.token().cancel();

        // Drain should timeout
        let result = shutdown.drain_tasks(tasks).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_graceful_shutdown_handles_errors() {
        let shutdown = Arc::new(GracefulShutdown::new(Duration::from_secs(5)));

        let mut tasks = JoinSet::new();
        tasks.spawn(async { Err::<(), _>(anyhow!("task failed")) });

        // Trigger shutdown
        shutdown.token().cancel();

        // Drain should report errors
        let result = shutdown.drain_tasks(tasks).await;
        assert!(result.is_err());
    }
}
