//! Workflow roots (0.10 step 5): which workflow a fact belongs to is
//! part of what the fact IS — declared on the type, stamped from the
//! payload, at every emit site, from any emitter. Plus `settle_tree`
//! (test/ops transitive settle over causation links) and boundary
//! handles (origin-attributed exogenous emits).

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use causal::checkpoint_store::{CheckpointStore, ReactorCheckpoint};
use causal::contexts::Ctx;
use causal::event::Event;
use causal::event_log::EventLogBackend;
use causal::memory_store::MemoryStore;
use causal::reactor::{Events, Reactor};
use causal::EngineBuilder;

fn backend(store: &Arc<MemoryStore>) -> EngineBuilder {
    EngineBuilder::new(
        store.clone() as Arc<dyn EventLogBackend>,
        store.clone() as Arc<dyn CheckpointStore>,
        store.clone() as Arc<dyn ReactorCheckpoint>,
    )
    .allow_in_memory_effect_store_for_tests()
}

// ── facts ────────────────────────────────────────────────────────────

/// Parent-chain trigger.
#[causal::event(name = "wf_file_archived", subject_id = "file_id")]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct FileArchived { file_id: Uuid, occurred_at: DateTime<Utc> }

/// Workflow ROOT: its own lifecycle, named by its own field.
#[causal::event(name = "wf_job_opened", subject_id = "job_id", subject = "wf_job",
                workflow_id = "job_id")]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct JobOpened { job_id: Uuid, file_id: Uuid }

/// Chain member inside the JOB workflow.
#[causal::event(name = "wf_job_enriched", subject_id = "job_id", subject = "wf_job")]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct JobEnriched { job_id: Uuid }

// ── reactors ─────────────────────────────────────────────────────────

/// Parent-chain reactor: archives spawn enrichment JOBS — slow external
/// labor gets a workflow-root fact, outside the parent's settle scope.
struct OpenJob;
#[async_trait]
impl Reactor for OpenJob {
    type Trigger = FileArchived;
    const NAME: &'static str = "wf.open_job";
    async fn react(&self, t: &FileArchived, ctx: Ctx<'_>) -> Result<Events> {
        let job_id = ctx.derive_id("job")?; // derived, never new_v4
        Ok(causal::events![JobOpened { job_id, file_id: t.file_id }])
    }
}

/// Child-chain reactor: works INSIDE the job workflow.
struct Enrich;
#[async_trait]
impl Reactor for Enrich {
    type Trigger = JobOpened;
    const NAME: &'static str = "wf.enrich";
    async fn react(&self, t: &JobOpened, _ctx: Ctx<'_>) -> Result<Events> {
        Ok(causal::events![JobEnriched { job_id: t.job_id }])
    }
}

// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_reactor_emitted_root_starts_its_own_workflow() {
    let store = Arc::new(MemoryStore::new());
    let engine = backend(&store)
        .with_reactor(OpenJob)
        .with_reactor(Enrich)
        .build().await.unwrap();

    let parent = engine
        .emit(FileArchived { file_id: Uuid::new_v4(), occurred_at: Utc::now() })
        .settled().await.unwrap();
    // settle_tree so the child chain is fully in the log for asserting.
    engine.settle_tree(parent).await.unwrap();

    let all = EventLogBackend::read_all(store.as_ref(), causal::LogCursor::ZERO, 20)
        .await.unwrap();
    let archived = all.iter().find(|e| e.event_type == "wf_file_archived").unwrap();
    let opened = all.iter().find(|e| e.event_type == "wf_job_opened").unwrap();
    let enriched = all.iter().find(|e| e.event_type == "wf_job_enriched").unwrap();

    // One source of truth: envelope workflow == the payload field.
    assert_eq!(opened.workflow_id, opened.subject_id,
               "the root's envelope workflow is stamped FROM its payload field");
    assert_ne!(opened.workflow_id, archived.workflow_id,
               "the job roots its OWN workflow — the 0.8 always-inherit hardcode is gone");
    // Causation still crosses the boundary (settle_tree's discovery edge).
    assert_eq!(opened.causation_id, Some(archived.event_id));
    // The member inherits the JOB's workflow, not the parent's.
    assert_eq!(enriched.workflow_id, opened.workflow_id,
               "chain members inherit their trigger's workflow");
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn builder_override_against_a_declaring_type_is_a_loud_error() {
    let store = Arc::new(MemoryStore::new());
    let engine = backend(&store).build().await.unwrap();

    let err = engine
        .emit(JobOpened { job_id: Uuid::new_v4(), file_id: Uuid::new_v4() })
        .workflow_id(Uuid::new_v4()) // conflicts with the declaration
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("single source of truth"),
            "the error teaches: {err:#}");

    // Nothing was appended — the conflict is detected before any write.
    assert!(EventLogBackend::read_all(store.as_ref(), causal::LogCursor::ZERO, 5)
        .await.unwrap().is_empty());
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_batch_cannot_mix_roots_and_members() {
    let store = Arc::new(MemoryStore::new());
    let engine = backend(&store).build().await.unwrap();

    // Two roots of DIFFERENT workflows in one batch: rejected.
    let err = engine
        .emit(vec![
            JobOpened { job_id: Uuid::new_v4(), file_id: Uuid::new_v4() },
            JobOpened { job_id: Uuid::new_v4(), file_id: Uuid::new_v4() },
        ])
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("emit roots separately"),
            "the error teaches: {err:#}");

    // Two roots of the SAME workflow: fine (one workflow, one batch).
    let job = Uuid::new_v4();
    engine
        .emit(vec![
            JobOpened { job_id: job, file_id: Uuid::new_v4() },
            JobOpened { job_id: job, file_id: Uuid::new_v4() },
        ])
        .await
        .expect("same-workflow roots batch cleanly");
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn emitting_a_root_directly_settles_its_declared_workflow() {
    // A root emitted at the boundary (no parent): EmitResult carries
    // the DECLARED workflow, so settled() waits the right chain.
    let store = Arc::new(MemoryStore::new());
    let engine = backend(&store)
        .with_reactor(Enrich)
        .build().await.unwrap();

    let job = Uuid::new_v4();
    let result = engine
        .emit(JobOpened { job_id: job, file_id: Uuid::new_v4() })
        .settled().await.unwrap();
    assert_eq!(result.workflow_id, job, "EmitResult carries the declared workflow");

    // settled() waited the job chain: the enrichment is already durable.
    let all = EventLogBackend::read_all(store.as_ref(), causal::LogCursor::ZERO, 10)
        .await.unwrap();
    assert!(all.iter().any(|e| e.event_type == "wf_job_enriched"),
            "the declared workflow's chain drained under settled()");
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn settle_tree_drains_the_whole_causal_tree() {
    let store = Arc::new(MemoryStore::new());
    let engine = backend(&store)
        .with_reactor(OpenJob)
        .with_reactor(Enrich)
        .build().await.unwrap();

    let parent = engine
        .emit(FileArchived { file_id: Uuid::new_v4(), occurred_at: Utc::now() })
        .await.unwrap();

    engine.settle_tree(parent).await.unwrap();

    // Parent chain AND the rooted child chain are fully durable.
    let all = EventLogBackend::read_all(store.as_ref(), causal::LogCursor::ZERO, 20)
        .await.unwrap();
    for kind in ["wf_file_archived", "wf_job_opened", "wf_job_enriched"] {
        assert!(all.iter().any(|e| e.event_type == kind),
                "{kind} present after settle_tree");
    }
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn boundary_emits_stamp_origin_attribution() {
    let store = Arc::new(MemoryStore::new());
    let engine = backend(&store).build().await.unwrap();

    engine.boundary("due_sweep")
        .emit(FileArchived { file_id: Uuid::new_v4(), occurred_at: Utc::now() })
        .await.unwrap();

    let all = EventLogBackend::read_all(store.as_ref(), causal::LogCursor::ZERO, 5)
        .await.unwrap();
    assert_eq!(
        all[0].metadata.get("_origin").and_then(|v| v.as_str()),
        Some("due_sweep"),
        "boundary emits carry origin attribution in the envelope",
    );
    engine.shutdown().await.unwrap();
}
