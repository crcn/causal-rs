use anyhow::Result;
use async_trait::async_trait;
use futures::Stream;
use seesaw_core::{Engine, QueueBackend, QueuedEffectExecution, QueuedEvent, Store, WorkflowEvent};
use uuid::Uuid;

#[derive(Clone, Default)]
struct TestDeps;

#[derive(Clone)]
struct TestStore;

#[async_trait]
impl Store for TestStore {
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

    async fn poll_next_effect(&self) -> Result<Option<QueuedEffectExecution>> {
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
        _emitted_events: Vec<seesaw_core::EmittedEvent>,
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
    ) -> Result<Box<dyn Stream<Item = WorkflowEvent> + Send + Unpin>> {
        Ok(Box::new(futures::stream::empty::<WorkflowEvent>()))
    }

    async fn get_workflow_status(
        &self,
        correlation_id: Uuid,
    ) -> Result<seesaw_core::WorkflowStatus> {
        Ok(seesaw_core::WorkflowStatus {
            correlation_id,
            pending_effects: 0,
            is_settled: true,
            last_event: None,
        })
    }
}

#[derive(Default)]
struct CustomQueueBackend;

#[async_trait]
impl QueueBackend<TestStore> for CustomQueueBackend {
    fn name(&self) -> &'static str {
        "custom-test-backend"
    }
}

#[test]
fn engine_new_defaults_to_store_queue_backend() {
    let engine = Engine::<TestDeps, TestStore>::new(TestDeps, TestStore);
    assert_eq!(engine.queue_backend_name(), "store");
}

#[test]
fn engine_builder_allows_custom_queue_backend() {
    let engine = Engine::<TestDeps, TestStore>::builder(TestDeps, TestStore)
        .queue_backend(CustomQueueBackend)
        .build();
    assert_eq!(engine.queue_backend_name(), "custom-test-backend");
}
