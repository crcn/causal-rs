use anyhow::Result;
use async_trait::async_trait;
use uuid::Uuid;

use crate::{QueuedHandlerExecution, Store};

/// Queue backend abstraction for queued effect dispatch.
///
/// By default Seesaw uses the backing [`Store`] as the queue transport.
/// Alternate backends (Redis/Kafka/etc.) can implement this trait to
/// receive queued intent notifications and provide work to effect workers.
///
/// Contract:
/// - The store remains the durable source of truth for effect intents.
/// - `on_effect_intent_inserted` is advisory; worker progress must not depend on it.
/// - Effect workers should fall back to `Store::poll_next_effect` when backend
///   polling fails or returns no work.
#[async_trait]
pub trait QueueBackend<St: Store>: Send + Sync + 'static {
    /// Human-readable backend name for diagnostics.
    fn name(&self) -> &'static str {
        "custom"
    }

    /// Called after an effect intent is persisted in the store.
    ///
    /// Custom backends can use this hook to enqueue the intent ID in an
    /// external transport. Default implementation is a no-op.
    async fn on_effect_intent_inserted(
        &self,
        _store: &St,
        _event_id: Uuid,
        _effect_id: &str,
    ) -> Result<()> {
        Ok(())
    }

    /// Fetch the next queued effect execution to process.
    ///
    /// Default implementation delegates to the store-backed queue.
    async fn poll_next_effect(&self, store: &St) -> Result<Option<QueuedHandlerExecution>> {
        store.poll_next_effect().await
    }
}

/// Default queue backend that uses the store for polling and dispatch.
#[derive(Debug, Default, Clone, Copy)]
pub struct StoreQueueBackend;

#[async_trait]
impl<St: Store> QueueBackend<St> for StoreQueueBackend {
    fn name(&self) -> &'static str {
        "store"
    }
}
