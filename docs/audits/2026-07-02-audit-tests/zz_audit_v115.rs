//! Audit verifier #115: Ctx::is_workflow_cancelled inside projector bodies.
//!
//! Claim: ProjectionRunner / MultiProjectorRunner construct Ctx with
//! `cancelled_workflows: None`, and `is_workflow_cancelled()` maps None to
//! `false` — so a projector body consulting the fence is silently told the
//! workflow is NOT cancelled even when it durably is.
//!
//! This test asserts the CORRECT (fence-aware) behavior: the workflow is
//! cancelled BEFORE the trigger is appended, so a truthful Ctx would report
//! `true`. If the defect is real, the assertion fails with `false`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::time::timeout;
use uuid::Uuid;

use causal::checkpoint_store::{CheckpointStore, ReactorCheckpoint};
use causal::contexts::Ctx;
use causal::event::Event;
use causal::event_log::EventLogBackend;
use causal::memory_store::MemoryStore;
use causal::projector::Projector;
use causal::EngineBuilder;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Ping {
    id: Uuid,
    occurred_at: DateTime<Utc>,
}
impl Event for Ping {
    const NAME: &'static str = "audit115_ping";
    fn subject_id(&self) -> Uuid { self.id }
    fn occurred_at(&self) -> Option<DateTime<Utc>> { Some(self.occurred_at) }
}

/// Projector that records what `ctx.is_workflow_cancelled()` returned for
/// every event it processes — the canonical "stop mirroring cancelled
/// workflows to the external system" guard.
#[derive(Clone)]
struct FenceProbe {
    observed: Arc<Mutex<Vec<bool>>>,
}

#[async_trait]
impl Projector for FenceProbe {
    type Event = Ping;
    const NAME: &'static str = "audit115-fence-probe";
    async fn project(&self, _p: &Ping, ctx: Ctx<'_>) -> Result<()> {
        self.observed
            .lock()
            .unwrap()
            .push(ctx.is_workflow_cancelled());
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn projector_ctx_reports_cancel_fence() -> Result<()> {
    let store = Arc::new(MemoryStore::new());
    let observed = Arc::new(Mutex::new(Vec::new()));

    let engine = EngineBuilder::new(
        store.clone() as Arc<dyn EventLogBackend>,
        store.clone() as Arc<dyn CheckpointStore>,
        store.clone() as Arc<dyn ReactorCheckpoint>,
    )
    .with_projector(FenceProbe { observed: observed.clone() })
    .allow_in_memory_effect_store_for_tests()
    .allow_in_memory_decision_store_for_tests()
    .build()
    .await?;

    let wf = Uuid::new_v4();

    // Cancel FIRST: the fence contains `wf` durably (control-stream
    // marker) and in-memory before the trigger even exists. No race.
    timeout(Duration::from_secs(5), engine.cancel_workflow(wf)).await??;

    // Emit into the cancelled workflow. Reactors would be fence-acked;
    // projectors still observe the event (read models keep folding).
    timeout(
        Duration::from_secs(10),
        engine
            .emit(Ping { id: Uuid::new_v4(), occurred_at: Utc::now() })
            .workflow_id(wf)
            .settled(),
    )
    .await??;

    let seen = observed.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "projector must have processed exactly the one event");
    assert!(
        seen[0],
        "Ctx::is_workflow_cancelled inside a projector body returned false \
         for workflow {wf}, which was cancelled BEFORE the trigger was \
         appended — the projector Ctx is wired with cancelled_workflows: \
         None, so the method is a silent constant false",
    );
    Ok(())
}
