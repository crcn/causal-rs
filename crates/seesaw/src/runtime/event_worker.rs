//! Event Worker - polls events and executes reducers + inline effects

use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::effect_registry::EffectRegistry;
use crate::reducer_registry::ReducerRegistry;
use crate::Store;

/// Event worker configuration
#[derive(Debug, Clone)]
pub struct EventWorkerConfig {
    /// Polling interval when no events available
    pub poll_interval: Duration,
    /// Maximum hop count before DLQ (infinite loop detection)
    pub max_hops: i32,
}

impl Default for EventWorkerConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_millis(100),
            max_hops: 50,
        }
    }
}

/// Event worker - polls and processes events
pub struct EventWorker<S, D, St>
where
    S: Clone + Send + Sync + 'static,
    D: Send + Sync + 'static,
    St: Store,
{
    store: Arc<St>,
    deps: Arc<D>,
    reducers: Arc<ReducerRegistry<S>>,
    effects: Arc<EffectRegistry<S, D>>,
    config: EventWorkerConfig,
}

impl<S, D, St> EventWorker<S, D, St>
where
    S: Clone + Send + Sync + 'static,
    D: Send + Sync + 'static,
    St: Store,
{
    pub fn new(
        store: Arc<St>,
        deps: Arc<D>,
        reducers: Arc<ReducerRegistry<S>>,
        effects: Arc<EffectRegistry<S, D>>,
        config: EventWorkerConfig,
    ) -> Self {
        Self {
            store,
            deps,
            reducers,
            effects,
            config,
        }
    }

    /// Run worker loop (polls events and processes them)
    pub async fn run(self) -> Result<()> {
        info!("Event worker started");

        loop {
            match self.process_next_event().await {
                Ok(processed) => {
                    if !processed {
                        // No events available, sleep briefly
                        sleep(self.config.poll_interval).await;
                    }
                }
                Err(e) => {
                    error!("Error processing event: {}", e);
                    sleep(self.config.poll_interval).await;
                }
            }
        }
    }

    /// Process next available event
    ///
    /// Returns true if event was processed, false if no events available
    async fn process_next_event(&self) -> Result<bool> {
        // Poll next event (per-saga FIFO with advisory lock)
        let Some(event) = self.store.poll_next().await? else {
            return Ok(false);
        };

        info!(
            "Processing event: type={}, saga={}, hops={}",
            event.event_type, event.saga_id, event.hops
        );

        // Check for infinite loops
        if event.hops >= self.config.max_hops {
            warn!(
                "Event exceeded max hops ({}), sending to DLQ: event_id={}",
                self.config.max_hops, event.event_id
            );
            // TODO: Send to DLQ
            self.store.ack(event.id).await?;
            return Ok(true);
        }

        // Load current state
        let (mut state, version): (S, i32) = self
            .store
            .load_state(event.saga_id)
            .await?
            .unwrap_or_else(|| {
                // No state yet, use Default
                (S::default(), 0)
            });

        // Run reducers (pure state transformations)
        let prev_state = state.clone();
        for reducer in self.reducers.all() {
            // TODO: Type matching for reducers
            // For now, skip reducer execution
        }

        // Save updated state (optimistic locking)
        let new_version = self
            .store
            .save_state(event.saga_id, &state, version)
            .await?;

        info!(
            "State updated: saga={}, version={} -> {}",
            event.saga_id, version, new_version
        );

        // Execute effects (branch on inline vs queued)
        for effect in self.effects.all() {
            // TODO: Check if effect matches event type

            // TODO: Determine if effect is inline or queued
            let is_inline = false; // Default to queued for now

            if is_inline {
                // Inline effect: Execute NOW in this transaction
                info!("Executing inline effect: {}", effect.id());

                // TODO: Execute effect handler
                // TODO: Record execution in seesaw_effect_executions (completed)
                // TODO: Emit next events (same transaction - atomicity!)
            } else {
                // Queued effect: Insert intent for effect worker
                info!("Queueing effect for async execution: {}", effect.id());

                self.store
                    .insert_effect_intent(
                        event.event_id,
                        effect.id().to_string(),
                        event.saga_id,
                        event.event_type.clone(),
                        event.payload.clone(),
                        event.parent_id,
                        chrono::Utc::now(), // Execute immediately
                        30,                 // Default timeout
                        3,                  // Default max attempts
                        10,                 // Default priority
                    )
                    .await?;
            }
        }

        // Mark event as processed
        self.store.ack(event.id).await?;

        info!("Event processed successfully: event_id={}", event.event_id);

        Ok(true)
    }
}
