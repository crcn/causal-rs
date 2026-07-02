//! The deterministic `Ctx` (0.10 step 4): `derive_id` / `time` /
//! `effect`, effect-store floor-GC with the parked exception, and
//! calendar-clock injection.

use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use causal::checkpoint_store::{CheckpointStore, ReactorCheckpoint};
use causal::contexts::Ctx;
use causal::effect_store::{EffectKey, EffectStore, InMemoryEffectStore};
use causal::event::Event;
use causal::event_log::EventLogBackend;
use causal::memory_store::MemoryStore;
use causal::reactor::{Events, Reactor};
use causal::{EngineBuilder, FixedClock};

fn backend(store: &Arc<MemoryStore>) -> EngineBuilder {
    EngineBuilder::new(
        store.clone() as Arc<dyn EventLogBackend>,
        store.clone() as Arc<dyn CheckpointStore>,
        store.clone() as Arc<dyn ReactorCheckpoint>,
    )
    .allow_in_memory_decision_store_for_tests()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JobOpened { job_id: Uuid, occurred_at: DateTime<Utc> }
impl Event for JobOpened {
    const NAME: &'static str = "det_job_opened";
    fn subject_id(&self) -> Uuid { self.job_id }
    fn occurred_at(&self) -> Option<DateTime<Utc>> { Some(self.occurred_at) }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EnrichmentQueued { enrichment_id: Uuid }
impl Event for EnrichmentQueued {
    const NAME: &'static str = "det_enrichment_queued";
    fn subject_id(&self) -> Uuid { self.enrichment_id }
}

/// Mints a child identity with `ctx.derive_id` and memoizes an
/// "external call" with `ctx.effect`.
struct Enricher { external_calls: Arc<AtomicU32> }
#[async_trait]
impl Reactor for Enricher {
    type Trigger = JobOpened;
    const NAME: &'static str = "det.enricher";
    async fn react(&self, _t: &JobOpened, ctx: Ctx<'_>) -> Result<Events> {
        let enrichment_id = ctx.derive_id("enrichment")?;
        let calls = self.external_calls.clone();
        let _score: i64 = ctx.effect("score", || async move {
            calls.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(7)
        }).await?;
        Ok(causal::events![EnrichmentQueued { enrichment_id }])
    }
}

#[tokio::test]
async fn derive_id_and_effect_are_stable_under_redelivery() {
    // The determinism triple in action: redelivery re-runs react(),
    // derive_id mints the SAME child identity, the effect replays its
    // memo instead of re-calling, and the byte-identical output dedups.
    let store = Arc::new(MemoryStore::new());
    let effects = Arc::new(InMemoryEffectStore::new());
    let calls = Arc::new(AtomicU32::new(0));
    let engine = backend(&store)
        .with_effect_store(effects.clone() as Arc<dyn EffectStore>)
        .with_reactor(Enricher { external_calls: calls.clone() })
        .build().await.unwrap();

    engine.emit(JobOpened { job_id: Uuid::new_v4(), occurred_at: Utc::now() })
        .settled().await.unwrap();

    let first_output = EventLogBackend::read_all(store.as_ref(), causal::LogCursor::ZERO, 10)
        .await.unwrap()
        .into_iter()
        .find(|e| e.event_type == "det_enrichment_queued")
        .expect("output landed");

    // Crash-redeliver: rewind the reactor cursor; the supervisor
    // re-runs react() on the same trigger.
    CheckpointStore::set(store.as_ref(), "det.enricher", causal::LogCursor::ZERO)
        .await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;

    let all = EventLogBackend::read_all(store.as_ref(), causal::LogCursor::ZERO, 10)
        .await.unwrap();
    let outputs: Vec<_> = all.iter()
        .filter(|e| e.event_type == "det_enrichment_queued")
        .collect();
    assert_eq!(outputs.len(), 1, "redelivery deduped — no second enrichment");
    assert_eq!(outputs[0].subject_id, first_output.subject_id,
               "derive_id minted the SAME identity on redelivery");
    assert_eq!(calls.load(AtomicOrdering::SeqCst), 1,
               "the external call ran exactly once across redeliveries");
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn effect_entries_are_gced_once_the_floor_passes() {
    // An effect memo exists only to make redelivery deterministic —
    // once the durable ack-floor passes its trigger, it's dead weight.
    let store = Arc::new(MemoryStore::new());
    let effects = Arc::new(InMemoryEffectStore::new());
    let engine = backend(&store)
        .with_effect_store(effects.clone() as Arc<dyn EffectStore>)
        .with_reactor(Enricher { external_calls: Arc::new(AtomicU32::new(0)) })
        .build().await.unwrap();

    engine.emit(JobOpened { job_id: Uuid::new_v4(), occurred_at: Utc::now() })
        .settled().await.unwrap();

    let trigger_id = EventLogBackend::read_all(store.as_ref(), causal::LogCursor::ZERO, 10)
        .await.unwrap()
        .into_iter()
        .find(|e| e.event_type == "det_job_opened")
        .unwrap().event_id;
    let key = EffectKey::new("det.enricher", trigger_id, "score");

    // The floor persists + GC runs in the dispatcher's reap; give the
    // supervisor a few polls.
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        if effects.get(&key).await.unwrap().is_none() { break; }
        assert!(std::time::Instant::now() < deadline,
                "effect entry survived the floor — GC never ran");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn parked_trigger_keeps_its_effect_entries() {
    // The floor-GC exception: failure replay happens after the floor
    // has passed, so a parked trigger's effect memos must survive —
    // otherwise an operator's replay re-runs effects fresh and the
    // replay path poisons itself by default.
    struct EffectThenFail { external_calls: Arc<AtomicU32> }
    #[async_trait]
    impl Reactor for EffectThenFail {
        type Trigger = JobOpened;
        const NAME: &'static str = "det.park";
        async fn react(&self, _t: &JobOpened, ctx: Ctx<'_>) -> Result<Events> {
            let calls = self.external_calls.clone();
            let _: String = ctx.effect("fetch", || async move {
                calls.fetch_add(1, AtomicOrdering::SeqCst);
                Ok("page".to_string())
            }).await?;
            anyhow::bail!("downstream step broken") // unclassified → parks after 3
        }
    }

    let store = Arc::new(MemoryStore::new());
    let effects = Arc::new(InMemoryEffectStore::new());
    let calls = Arc::new(AtomicU32::new(0));
    let engine = backend(&store)
        .with_effect_store(effects.clone() as Arc<dyn EffectStore>)
        .with_reactor(EffectThenFail { external_calls: calls.clone() })
        .build().await.unwrap();

    engine.emit(JobOpened { job_id: Uuid::new_v4(), occurred_at: Utc::now() })
        .settled().await.unwrap(); // settles once parked

    let all = EventLogBackend::read_all(store.as_ref(), causal::LogCursor::ZERO, 10)
        .await.unwrap();
    let trigger_id = all.iter().find(|e| e.event_type == "det_job_opened").unwrap().event_id;
    assert!(all.iter().any(|e| e.event_type == "causal:reaction_failed"),
            "the trigger parked");
    assert_eq!(calls.load(AtomicOrdering::SeqCst), 1,
               "retries replayed the memo instead of re-calling");

    // Give GC every chance to (wrongly) fire, then assert retention.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let key = EffectKey::new("det.park", trigger_id, "fetch");
    assert!(effects.get(&key).await.unwrap().is_some(),
            "parked trigger's effect memo survives floor-GC (failure replay needs it)");
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn duplicate_effect_label_parks_as_domain() {
    // Reusing a label in one invocation would silently replay the
    // first call's result as the second's — a silent-correctness bug,
    // surfaced as a domain-class error.
    struct DupLabels;
    #[async_trait]
    impl Reactor for DupLabels {
        type Trigger = JobOpened;
        const NAME: &'static str = "det.dup";
        async fn react(&self, _t: &JobOpened, ctx: Ctx<'_>) -> Result<Events> {
            let _: i64 = ctx.effect("call", || async { Ok(1) }).await?;
            let _: i64 = ctx.effect("call", || async { Ok(2) }).await?; // duplicate
            Ok(Events::new())
        }
    }

    let store = Arc::new(MemoryStore::new());
    let effects = Arc::new(InMemoryEffectStore::new());
    let seen_class = Arc::new(parking_lot::Mutex::new(None));
    let seen_class_c = seen_class.clone();
    let engine = backend(&store)
        .with_effect_store(effects.clone() as Arc<dyn EffectStore>)
        .with_reactor(DupLabels)
        .on_terminal_failure(move |info: causal::TerminalFailure| -> Option<JobOpened> {
            *seen_class_c.lock() = Some((info.class, info.error.clone()));
            None
        })
        .build().await.unwrap();

    engine.emit(JobOpened { job_id: Uuid::new_v4(), occurred_at: Utc::now() })
        .settled().await.unwrap();

    let (class, error) = seen_class.lock().clone().expect("parked");
    assert_eq!(class, causal::FailureClass::Domain,
               "duplicate label is a domain-class error");
    assert!(error.contains("duplicate"), "the error teaches: {error}");
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn injected_clock_stamps_created_at() {
    // A fact that declares no occurred_at gets created_at from the
    // INJECTED clock — pinned in tests, reproducible envelopes.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct Timeless { id: Uuid }
    impl Event for Timeless {
        const NAME: &'static str = "det_timeless";
        fn subject_id(&self) -> Uuid { self.id }
    }

    let pinned = Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap();
    let store = Arc::new(MemoryStore::new());
    let engine = backend(&store)
        .with_clock(Arc::new(FixedClock(pinned)))
        .build().await.unwrap();

    engine.emit(Timeless { id: Uuid::new_v4() }).await.unwrap();

    let all = EventLogBackend::read_all(store.as_ref(), causal::LogCursor::ZERO, 10)
        .await.unwrap();
    assert_eq!(all[0].created_at, pinned,
               "framework-stamped created_at comes from the injected clock");
    engine.shutdown().await.unwrap();
}
