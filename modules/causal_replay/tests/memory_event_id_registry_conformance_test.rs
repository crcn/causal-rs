//! EventIdRegistry conformance run against `causal::InMemoryEventIdRegistry`.

use anyhow::Result;
use causal::InMemoryEventIdRegistry;
use causal_replay::conformance;

fn r() -> InMemoryEventIdRegistry {
    InMemoryEventIdRegistry::new()
}

#[tokio::test]
async fn event_id_registry_absent_batch() -> Result<()> {
    conformance::event_id_registry_absent_batch(&r()).await
}

#[tokio::test]
async fn event_id_registry_redelivery_after_register() -> Result<()> {
    conformance::event_id_registry_redelivery_after_register(&r()).await
}

#[tokio::test]
async fn event_id_registry_partial_overlap() -> Result<()> {
    conformance::event_id_registry_partial_overlap(&r()).await
}

#[tokio::test]
async fn event_id_registry_register_first_write_wins() -> Result<()> {
    conformance::event_id_registry_register_first_write_wins(&r()).await
}
