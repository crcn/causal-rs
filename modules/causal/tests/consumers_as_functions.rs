//! Primitive 7 at runtime: `#[causal::reactors]` / `#[causal::projectors]`
//! modules wired through a real engine — the macro removes trait
//! ceremony only; everything load-bearing behaves exactly like the
//! hand-written impls it expands to. Plus `Reactor::MAX_IN_FLIGHT`
//! enforcement (the knob the attribute carries).

use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use causal::checkpoint_store::{CheckpointStore, ReactorCheckpoint};
use causal::contexts::Ctx;
use causal::event::Event;
use causal::event_log::EventLogBackend;
use causal::memory_store::MemoryStore;
use causal::reactor::{Events, Reactor};
use causal::types::RecordedEvent;
use causal::EngineBuilder;

fn backend(store: &Arc<MemoryStore>) -> EngineBuilder {
    EngineBuilder::new(
        store.clone() as Arc<dyn EventLogBackend>,
        store.clone() as Arc<dyn CheckpointStore>,
        store.clone() as Arc<dyn ReactorCheckpoint>,
    )
}

#[causal::event(name = "fnc_job_opened", subject_id = "job_id")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobOpened { pub job_id: Uuid }

#[causal::event(name = "fnc_job_done", subject_id = "job_id")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobDone { pub job_id: Uuid, pub tag: String }

#[causal::reactors(deps = JobDeps)]
mod job_reactors {
    use super::*;

    pub struct JobDeps {
        pub tag: String,
    }

    #[reactor(name = "fnc.finish")]
    async fn finish(deps: &JobDeps, t: &JobOpened, ctx: Ctx<'_>) -> Result<Events> {
        // The Primitive-2 body, untouched: deterministic doors only.
        let _id = ctx.derive_id("artifact")?;
        Ok(causal::events![JobDone { job_id: t.job_id, tag: deps.tag.clone() }])
    }
}

#[causal::projectors(deps = Sink)]
mod job_projectors {
    use super::*;

    #[derive(Clone)]
    pub struct Sink {
        pub typed: Arc<parking_lot::Mutex<Vec<Uuid>>>,
        pub raw: Arc<parking_lot::Mutex<Vec<String>>>,
    }

    #[projector(name = "fnc.rows")]
    async fn rows(deps: &Sink, e: &JobOpened, _ctx: Ctx<'_>) -> Result<()> {
        deps.typed.lock().push(e.job_id);
        Ok(())
    }

    #[projector(name = "fnc.audit", kinds = ["fnc_job_opened", "fnc_job_done"])]
    async fn audit(deps: &Sink, e: &RecordedEvent, _ctx: Ctx<'_>) -> Result<()> {
        deps.raw.lock().push(e.event_type.clone());
        Ok(())
    }
}

#[tokio::test]
async fn macro_consumers_run_like_handwritten_ones() {
    let store = Arc::new(MemoryStore::new());
    let sink = job_projectors::Sink {
        typed: Arc::new(parking_lot::Mutex::new(Vec::new())),
        raw: Arc::new(parking_lot::Mutex::new(Vec::new())),
    };
    let (typed, raw) = (sink.typed.clone(), sink.raw.clone());

    let engine = backend(&store)
        .with_reactors(job_reactors::reactors(job_reactors::JobDeps { tag: "macro".into() }))
        .with_projectors(job_projectors::projectors(sink.clone()))
        .with_multi_projectors(job_projectors::multi_projectors(sink))
        .build().await.unwrap();

    let job = Uuid::new_v4();
    engine.emit(JobOpened { job_id: job }).settled().await.unwrap();

    // The reactor ran with its deps threaded through.
    let all = EventLogBackend::read_all(store.as_ref(), causal::LogCursor::ZERO, 10)
        .await.unwrap();
    let done = all.iter().find(|e| e.event_type == "fnc_job_done")
        .expect("macro reactor produced its output");
    assert_eq!(done.payload["tag"], "macro", "deps reached the body");
    assert_eq!(done.subject_id, job);

    // Typed projector saw the typed fact; cross-kind one saw both kinds.
    assert_eq!(*typed.lock(), vec![job]);
    let mut kinds = raw.lock().clone();
    kinds.sort();
    assert_eq!(kinds, vec!["fnc_job_done", "fnc_job_opened"],
               "the declared kinds list routed both facts");
    engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn max_in_flight_caps_concurrent_reactions() {
    // Two subjects = two partitions = would overlap (proven by the
    // step-2 acceptance suite). MAX_IN_FLIGHT = 1 must serialize them
    // anyway — the external-resource knob beats partition fan-out.
    struct SerialDwell { inside: Arc<AtomicU64>, peak: Arc<AtomicU64> }
    #[async_trait]
    impl Reactor for SerialDwell {
        type Trigger = JobOpened;
        const NAME: &'static str = "fnc.serial";
        const MAX_IN_FLIGHT: usize = 1;
        async fn react(&self, _t: &JobOpened, _: Ctx<'_>) -> Result<Events> {
            let now = self.inside.fetch_add(1, AtomicOrdering::SeqCst) + 1;
            self.peak.fetch_max(now, AtomicOrdering::SeqCst);
            tokio::time::sleep(Duration::from_millis(30)).await; // dwell
            self.inside.fetch_sub(1, AtomicOrdering::SeqCst);
            Ok(Events::new())
        }
    }

    let store = Arc::new(MemoryStore::new());
    let inside = Arc::new(AtomicU64::new(0));
    let peak = Arc::new(AtomicU64::new(0));
    let engine = backend(&store)
        .with_reactor(SerialDwell { inside: inside.clone(), peak: peak.clone() })
        .build().await.unwrap();

    let a = engine.emit(JobOpened { job_id: Uuid::new_v4() }).settled();
    let b = engine.emit(JobOpened { job_id: Uuid::new_v4() }).settled();
    let c = engine.emit(JobOpened { job_id: Uuid::new_v4() }).settled();
    let (ra, rb, rc) = tokio::join!(a, b, c);
    ra.unwrap(); rb.unwrap(); rc.unwrap();

    assert_eq!(peak.load(AtomicOrdering::SeqCst), 1,
               "MAX_IN_FLIGHT = 1 serialized reactions across partitions");
    engine.shutdown().await.unwrap();
}
