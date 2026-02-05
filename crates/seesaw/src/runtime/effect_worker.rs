//! Effect Worker - polls and executes queued effects

use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{sleep, timeout};
use tracing::{error, info, warn};

use crate::effect_registry::EffectRegistry;
use crate::Store;

/// Effect worker configuration
#[derive(Debug, Clone)]
pub struct EffectWorkerConfig {
    /// Polling interval when no effects available
    pub poll_interval: Duration,
    /// Default timeout for effect execution
    pub default_timeout: Duration,
}

impl Default for EffectWorkerConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_millis(100),
            default_timeout: Duration::from_secs(30),
        }
    }
}

/// Effect worker - polls and executes queued effects
pub struct EffectWorker<S, D, St>
where
    S: Clone + Send + Sync + 'static,
    D: Send + Sync + 'static,
    St: Store,
{
    store: Arc<St>,
    deps: Arc<D>,
    effects: Arc<EffectRegistry<S, D>>,
    config: EffectWorkerConfig,
}

impl<S, D, St> EffectWorker<S, D, St>
where
    S: Clone + Send + Sync + 'static,
    D: Send + Sync + 'static,
    St: Store,
{
    pub fn new(
        store: Arc<St>,
        deps: Arc<D>,
        effects: Arc<EffectRegistry<S, D>>,
        config: EffectWorkerConfig,
    ) -> Self {
        Self {
            store,
            deps,
            effects,
            config,
        }
    }

    /// Run worker loop
    pub async fn run(self) -> Result<()> {
        info!("Effect worker started");

        loop {
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
            "Processing effect: effect_id={}, saga={}, priority={}, attempt={}/{}",
            execution.effect_id,
            execution.saga_id,
            execution.priority,
            execution.attempts,
            execution.max_attempts
        );

        // Find effect handler
        let effect = self
            .effects
            .all()
            .find(|e| e.id() == execution.effect_id)
            .ok_or_else(|| anyhow::anyhow!("Effect not found: {}", execution.effect_id))?;

        // Execute effect with timeout
        let timeout_duration = Duration::from_secs(execution.timeout_seconds as u64);

        let result = timeout(timeout_duration, async {
            // TODO: Execute effect handler with proper context
            // For now, return success
            Ok::<serde_json::Value, anyhow::Error>(serde_json::json!({"status": "ok"}))
        })
        .await;

        match result {
            Ok(Ok(value)) => {
                // Success - mark as completed
                info!("Effect completed successfully: {}", execution.effect_id);

                self.store
                    .complete_effect(execution.event_id, execution.effect_id.clone(), value)
                    .await?;

                // TODO: Emit next events if handler returned events
            }
            Ok(Err(e)) => {
                // Effect failed
                warn!(
                    "Effect failed: {} (attempt {}/{}): {}",
                    execution.effect_id, execution.attempts, execution.max_attempts, e
                );

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
}
