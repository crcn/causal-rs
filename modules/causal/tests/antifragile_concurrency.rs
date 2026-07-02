//! Adversarial concurrency suite for the revision-gated fold machinery
//! (idempotent folds + gap repair) and reactor cursor seeding.
//!
//! Master invariant (checked after every attack): for every aggregate,
//! `engine.state_of::<A>(id).await.unwrap()` == an independent from-scratch fold of the
//! aggregate's own stream read directly from the log.
//!
//! Run: `cargo test -p causal --test antifragile_concurrency -- --test-threads=8`
//! (loop ≥20× to shake out flaky interleavings).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::time::timeout;
use uuid::Uuid;

use causal::aggregate::{Aggregate, Apply};
use causal::checkpoint_store::{CheckpointStore, ReactorCheckpoint};
use causal::contexts::Ctx;
use causal::event::Event;
use causal::event_log::EventLogBackend;
use causal::memory_store::MemoryStore;
use causal::reactor::{Events, Reactor};
use causal::reactor_runner::derive_output_event_id;
use causal::types::LogCursor;
use causal::{Aggregator, Engine, EngineBuilder, SnapshotStore};

// ─────────────────────────────────────────────────────────────────────
// Harness
// ─────────────────────────────────────────────────────────────────────

fn backend(store: &Arc<MemoryStore>) -> EngineBuilder {
    EngineBuilder::new(
        store.clone() as Arc<dyn EventLogBackend>,
        store.clone() as Arc<dyn CheckpointStore>,
        store.clone() as Arc<dyn ReactorCheckpoint>,
    )
    .allow_in_memory_effect_store_for_tests()
            .allow_in_memory_decision_store_for_tests()
}

/// Independent from-scratch fold of `A`'s own stream straight from the log.
/// Foreign (non-`F`) events co-located in the stream are skipped, exactly
/// like the registry's identity fold.
async fn replay_one<A, F>(store: &MemoryStore, id: Uuid) -> (A, usize)
where
    A: Aggregate + Apply<F>,
    F: Event,
{
    let events = EventLogBackend::read_stream(store, A::SUBJECT, id, None)
        .await
        .unwrap();
    let mut agg = A::default();
    let mut applied = 0usize;
    for e in &events {
        if e.event_type == F::NAME {
            let fact: F = serde_json::from_value(e.payload.clone()).unwrap();
            agg.apply(&fact);
            applied += 1;
        }
    }
    (agg, applied)
}

/// THE invariant: engine snapshot == from-scratch fold of the log.
async fn assert_invariant<A, F>(engine: &Engine, store: &MemoryStore, id: Uuid, ctx: &str)
where
    A: Aggregate + Apply<F> + Clone + PartialEq + std::fmt::Debug + serde::de::DeserializeOwned,
    F: Event,
{
    let (expected, n) = replay_one::<A, F>(store, id).await;
    match engine.state_of::<A>(id).await.unwrap() {
        None if n == 0 => {}
        None => panic!(
            "[{ctx}] INVARIANT BROKEN: log has {n} folding events for {id} \
             but engine registry has no state"
        ),
        Some(got) => assert_eq!(
            got, expected,
            "[{ctx}] INVARIANT BROKEN: engine state != from-scratch fold for {id} ({n} events)"
        ),
    }
}

/// Blue/green variant: an engine registry only tracks streams it has
/// touched (eager read-your-write state, not a projection). Present ⇒
/// must be exact; absent is legal. Returns whether state was present.
async fn assert_invariant_if_present<A, F>(
    engine: &Engine,
    store: &MemoryStore,
    id: Uuid,
    ctx: &str,
) -> bool
where
    A: Aggregate + Apply<F> + Clone + PartialEq + std::fmt::Debug + serde::de::DeserializeOwned,
    F: Event,
{
    let (expected, _) = replay_one::<A, F>(store, id).await;
    match engine.state_of::<A>(id).await.unwrap() {
        None => false,
        Some(got) => {
            assert_eq!(
                got, expected,
                "[{ctx}] INVARIANT BROKEN: present engine state != from-scratch fold for {id}"
            );
            true
        }
    }
}

// ─────────────────────────────────────────────────────────────────────
// Attack 2 — N tasks emit to the SAME stream-aligned aggregate stream.
// Folds race; gap repair must order them; nothing may be lost.
// ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Hit {
    stream: Uuid,
    amount: i64,
}
impl Event for Hit {
    const NAME: &'static str = "af_hit";
    fn subject_id(&self) -> Uuid {
        self.stream
    }
}

#[derive(Default, Clone, Debug, PartialEq, Serialize, Deserialize)]
struct HitTotal {
    n: u64,
    sum: i64,
}
impl Aggregate for HitTotal {
    const NAME: &'static str = "HitTotal";
    const SUBJECT: &'static str = "af_hit";
}
impl Apply<Hit> for HitTotal {
    fn apply(&mut self, f: &Hit) {
        self.n += 1;
        self.sum += f.amount;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn same_stream_emit_storm_no_fold_lost() {
    let store = Arc::new(MemoryStore::new());
    let engine = Arc::new(
        backend(&store)
            .with_aggregators([Aggregator::for_type::<HitTotal, Hit>()])
            .build()
            .await
            .unwrap(),
    );
    let id = Uuid::new_v4();
    const TASKS: u64 = 8;
    const PER: u64 = 25;

    let mut handles = Vec::new();
    for t in 0..TASKS {
        let engine = engine.clone();
        handles.push(tokio::spawn(async move {
            for k in 0..PER {
                let amount = (t * PER + k) as i64;
                timeout(Duration::from_secs(20), async {
                    engine.emit(Hit { stream: id, amount }).settled().await
                })
                .await
                .expect("settle hung on same-stream storm")
                .expect("emit failed");
            }
        }));
    }
    for h in handles {
        h.await.unwrap();
    }

    // No fold lost, no double-count.
    let snap = engine.state_of::<HitTotal>(id).await.unwrap().expect("state present");
    assert_eq!(snap.n, TASKS * PER, "every concurrent emit folded exactly once");
    let expected_sum: i64 = (0..(TASKS * PER) as i64).sum();
    assert_eq!(snap.sum, expected_sum, "fold content intact, not just count");

    // Dense revisions in the stream (the property gap repair relies on).
    let evs = EventLogBackend::read_stream(store.as_ref(), "af_hit", id, None)
        .await
        .unwrap();
    assert_eq!(evs.len() as u64, TASKS * PER);
    for (i, e) in evs.iter().enumerate() {
        assert_eq!(e.revision.raw(), i as u64, "dense per-stream revisions");
    }

    assert_invariant::<HitTotal, Hit>(&engine, &store, id, "same-stream storm").await;
}

// ─────────────────────────────────────────────────────────────────────
// Attack 9a — first-fold burst on FRESH aggregates. Targets the
// vacant-entry read-through restore inside gap repair: `restore_aggregate`
// checks `has_state` then later `set_state`s unconditionally (TOCTOU) —
// a concurrent direct fold landing in that window could be clobbered by
// a stale tail read.
// ─────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn first_fold_burst_on_fresh_aggregates() {
    let store = Arc::new(MemoryStore::new());
    let engine = Arc::new(
        backend(&store)
            .with_aggregators([Aggregator::for_type::<HitTotal, Hit>()])
            .build()
            .await
            .unwrap(),
    );

    const IDS: usize = 24;
    const BURST: i64 = 5;
    let ids: Vec<Uuid> = (0..IDS).map(|_| Uuid::new_v4()).collect();

    let mut handles = Vec::new();
    for &id in &ids {
        for j in 0..BURST {
            let engine = engine.clone();
            handles.push(tokio::spawn(async move {
                engine
                    .emit(Hit { stream: id, amount: j })
                    .await
                    .expect("emit failed");
            }));
        }
    }
    for h in handles {
        h.await.unwrap();
    }

    for &id in &ids {
        let snap = engine.state_of::<HitTotal>(id).await.unwrap().expect("state present");
        assert_eq!(
            snap.n, BURST as u64,
            "fresh-aggregate burst lost a fold for {id} (vacant-entry restore race)"
        );
        assert_invariant::<HitTotal, Hit>(&engine, &store, id, "fresh-aggregate burst").await;
    }
}

// ─────────────────────────────────────────────────────────────────────
// Attack 3 — N emitters on different streams + M reactors fanning out
// into yet other streams. Per-workflow settle must return, chains
// must not interfere, all three aggregates must satisfy the invariant.
// ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Job {
    id: Uuid,
}
impl Event for Job {
    const NAME: &'static str = "af_job";
    fn subject_id(&self) -> Uuid {
        self.id
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EchoA {
    id: Uuid,
}
impl Event for EchoA {
    const NAME: &'static str = "af_echo_a";
    fn subject_id(&self) -> Uuid {
        self.id
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EchoB {
    id: Uuid,
}
impl Event for EchoB {
    const NAME: &'static str = "af_echo_b";
    fn subject_id(&self) -> Uuid {
        self.id
    }
}

macro_rules! count_aggregate {
    ($name:ident, $fact:ty, $stream:literal) => {
        #[derive(Default, Clone, Debug, PartialEq, Serialize, Deserialize)]
        struct $name {
            n: u64,
        }
        impl Aggregate for $name {
            const NAME: &'static str = stringify!($name);
            const SUBJECT: &'static str = $stream;
        }
        impl Apply<$fact> for $name {
            fn apply(&mut self, _: &$fact) {
                self.n += 1;
            }
        }
    };
}

count_aggregate!(JobTotal, Job, "af_job");
count_aggregate!(EchoATotal, EchoA, "af_echo_a");
count_aggregate!(EchoBTotal, EchoB, "af_echo_b");

struct ReactA;
#[async_trait]
impl Reactor for ReactA {
    type Trigger = Job;
    const NAME: &'static str = "af.echo-a";
    async fn react(&self, t: &Job, _ctx: Ctx<'_>) -> Result<Events> {
        Ok(causal::events![EchoA { id: t.id }])
    }
}

struct ReactB;
#[async_trait]
impl Reactor for ReactB {
    type Trigger = Job;
    const NAME: &'static str = "af.echo-b";
    async fn react(&self, t: &Job, _ctx: Ctx<'_>) -> Result<Events> {
        Ok(causal::events![EchoB { id: t.id }])
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn fanout_reactors_no_cross_chain_interference() {
    let store = Arc::new(MemoryStore::new());
    let engine = Arc::new(
        backend(&store)
            .with_aggregators([
                Aggregator::for_type::<JobTotal, Job>(),
                Aggregator::for_type::<EchoATotal, EchoA>(),
                Aggregator::for_type::<EchoBTotal, EchoB>(),
            ])
            .with_reactor(ReactA)
            .with_reactor(ReactB)
            .build()
            .await
            .unwrap(),
    );

    const TASKS: usize = 6;
    const PER: usize = 6;
    let mut handles = Vec::new();
    for _ in 0..TASKS {
        let engine = engine.clone();
        handles.push(tokio::spawn(async move {
            let mut results = Vec::new();
            for _ in 0..PER {
                let id = Uuid::new_v4();
                let r = timeout(Duration::from_secs(20), async {
                    engine.emit(Job { id }).settled().await
                })
                .await
                .expect("per-workflow settle hung")
                .expect("emit failed");
                results.push((id, r));
            }
            results
        }));
    }

    let mut all_results = Vec::new();
    for h in handles {
        all_results.extend(h.await.unwrap());
    }

    let log = EventLogBackend::read_all(store.as_ref(), LogCursor::ZERO, 1_000_000)
        .await
        .unwrap();

    for (id, result) in &all_results {
        // Exactly this chain: 1 trigger + 1 EchoA + 1 EchoB — no cross-chain
        // contamination, no duplicated outputs.
        let chain: Vec<_> = log
            .iter()
            .filter(|e| e.workflow_id == result.workflow_id)
            .collect();
        assert_eq!(
            chain.len(),
            3,
            "workflow {} should own exactly trigger + 2 outputs, got {:?}",
            result.workflow_id,
            chain.iter().map(|e| e.event_type.as_str()).collect::<Vec<_>>()
        );

        assert_invariant::<JobTotal, Job>(&engine, &store, *id, "fanout: job").await;
        assert_invariant::<EchoATotal, EchoA>(&engine, &store, *id, "fanout: echo-a").await;
        assert_invariant::<EchoBTotal, EchoB>(&engine, &store, *id, "fanout: echo-b").await;
    }
}

// ─────────────────────────────────────────────────────────────────────
// Attack 4 — blue/green: two Engines, one MemoryStore, SAME reactor
// NAME (shared cursor). Outputs must land exactly once
// (deterministic event_ids), both registries must stay exact for any
// state they hold.
// ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BgPing {
    id: Uuid,
}
impl Event for BgPing {
    const NAME: &'static str = "af_bgping";
    fn subject_id(&self) -> Uuid {
        self.id
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BgPong {
    id: Uuid,
}
impl Event for BgPong {
    const NAME: &'static str = "af_bgpong";
    fn subject_id(&self) -> Uuid {
        self.id
    }
}

count_aggregate!(BgPingTotal, BgPing, "af_bgping");
count_aggregate!(BgPongTotal, BgPong, "af_bgpong");

struct BgEcho;
#[async_trait]
impl Reactor for BgEcho {
    type Trigger = BgPing;
    const NAME: &'static str = "af.bg-echo";
    async fn react(&self, t: &BgPing, _ctx: Ctx<'_>) -> Result<Events> {
        Ok(causal::events![BgPong { id: t.id }])
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn blue_green_shared_cursor_outputs_exactly_once() {
    let store = Arc::new(MemoryStore::new());
    let build = || async {
        Arc::new(
            backend(&store)
                .with_aggregators([
                    Aggregator::for_type::<BgPingTotal, BgPing>(),
                    Aggregator::for_type::<BgPongTotal, BgPong>(),
                ])
                .with_reactor(BgEcho)
                .build()
                .await
                .unwrap(),
        )
    };
    let blue = build().await;
    let green = build().await;

    const PER_ENGINE: usize = 8;
    let mut handles = Vec::new();
    for engine in [blue.clone(), green.clone()] {
        for _ in 0..PER_ENGINE {
            let engine = engine.clone();
            handles.push(tokio::spawn(async move {
                let id = Uuid::new_v4();
                timeout(Duration::from_secs(20), async {
                    engine.emit(BgPing { id }).settled().await
                })
                .await
                .expect("blue/green settle hung")
                .expect("emit failed");
                id
            }));
        }
    }
    let mut ids = Vec::new();
    for h in handles {
        ids.push(h.await.unwrap());
    }

    // Wait for every trigger's deterministic output to land (both runners
    // race the shared cursor; dedup-by-event_id must collapse them).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let triggers_and_outputs = loop {
        let log = EventLogBackend::read_all(store.as_ref(), LogCursor::ZERO, 1_000_000)
            .await
            .unwrap();
        let triggers: Vec<_> = log
            .iter()
            .filter(|e| e.event_type == "af_bgping")
            .cloned()
            .collect();
        let all_present = triggers.iter().all(|t| {
            let want = derive_output_event_id(BgEcho::NAME, t.event_id, "af_bgpong", t.subject_id, 0);
            log.iter().any(|e| e.event_id == want)
        });
        if triggers.len() == ids.len() && all_present {
            break log;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "blue/green never produced all outputs — lost reaction"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };

    // Exactly-once: one pong per trigger, no extras (a duplicated reaction
    // with a non-deterministic id would show up as a surplus pong).
    let pongs: Vec<_> = triggers_and_outputs
        .iter()
        .filter(|e| e.event_type == "af_bgpong")
        .collect();
    assert_eq!(
        pongs.len(),
        ids.len(),
        "exactly one output per trigger across BOTH engines"
    );
    for t in triggers_and_outputs
        .iter()
        .filter(|e| e.event_type == "af_bgping")
    {
        let want = derive_output_event_id(BgEcho::NAME, t.event_id, "af_bgpong", t.subject_id, 0);
        assert_eq!(
            triggers_and_outputs
                .iter()
                .filter(|e| e.event_id == want)
                .count(),
            1,
            "deterministic output id present exactly once"
        );
    }

    // Registry exactness on both engines: any state held must equal the
    // from-scratch fold; every pong stream must be tracked by at least
    // one engine (the one whose runner appended/deduped it).
    for &id in &ids {
        let blue_ping =
            assert_invariant_if_present::<BgPingTotal, BgPing>(&blue, &store, id, "bg ping/blue")
                .await;
        let green_ping =
            assert_invariant_if_present::<BgPingTotal, BgPing>(&green, &store, id, "bg ping/green")
                .await;
        assert!(
            blue_ping || green_ping,
            "the emitting engine must hold ping state for {id}"
        );
        let blue_pong =
            assert_invariant_if_present::<BgPongTotal, BgPong>(&blue, &store, id, "bg pong/blue")
                .await;
        let green_pong =
            assert_invariant_if_present::<BgPongTotal, BgPong>(&green, &store, id, "bg pong/green")
                .await;
        assert!(
            blue_pong || green_pong,
            "at least one engine must have folded the pong for {id}"
        );
    }
}

// ─────────────────────────────────────────────────────────────────────
// Attack 5 — Engine::append (OCC) under 8-way contention with 3-fact
// atomic decisions. Dense revisions, no skipped folds, budget holds.
// ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OccFact {
    counter: Uuid,
    by: i64,
}
impl Event for OccFact {
    const NAME: &'static str = "af_occ";
    fn subject_id(&self) -> Uuid {
        self.counter
    }
}

#[derive(Default, Clone, Debug, PartialEq, Serialize, Deserialize)]
struct OccTotal {
    applied: u64,
    sum: i64,
}
impl Aggregate for OccTotal {
    const NAME: &'static str = "OccTotal";
    const SUBJECT: &'static str = "af_occ";
    const INVARIANT: bool = true;   // OCC-fenced: append-only door
}
impl Apply<OccFact> for OccTotal {
    fn apply(&mut self, f: &OccFact) {
        self.applied += 1;
        self.sum += f.by;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn occ_append_8way_multi_fact_contention() {
    let store = Arc::new(MemoryStore::new());
    let engine = Arc::new(
        backend(&store)
            .with_aggregators([Aggregator::for_type::<OccTotal, OccFact>()])
            .build()
            .await
            .unwrap(),
    );
    let id = Uuid::new_v4();

    const TASKS: u64 = 8;
    const APPENDS: u64 = 4;
    let mut handles = Vec::new();
    for _ in 0..TASKS {
        let engine = engine.clone();
        handles.push(tokio::spawn(async move {
            for _ in 0..APPENDS {
                // Multi-fact decision: one atomic 3-event batch under OCC.
                engine
                    .append::<OccTotal, OccFact, _>(id, |_state| {
                        Ok(vec![
                            OccFact { counter: id, by: 1 },
                            OccFact { counter: id, by: 2 },
                            OccFact { counter: id, by: 3 },
                        ])
                    })
                    .await
                    .expect("OCC retry budget exhausted under 8-way contention");
            }
        }));
    }
    for h in handles {
        h.await.unwrap();
    }

    let total_events = TASKS * APPENDS * 3;

    // Dense revisions; batches contiguous (3k, 3k+1, 3k+2 share a decision).
    let evs = EventLogBackend::read_stream(store.as_ref(), "af_occ", id, None)
        .await
        .unwrap();
    assert_eq!(evs.len() as u64, total_events, "all decisions landed");
    for (i, e) in evs.iter().enumerate() {
        assert_eq!(e.revision.raw(), i as u64, "dense revisions under OCC");
    }
    for batch in evs.chunks(3) {
        assert_eq!(
            batch[0].workflow_id, batch[2].workflow_id,
            "atomic decision batches must land contiguously"
        );
    }

    // engine.load (fresh fold from log) and the live registry agree.
    let (loaded, rev) = engine.load::<OccTotal, OccFact>(id).await.unwrap();
    assert_eq!(loaded.applied, total_events);
    assert_eq!(loaded.sum as u64, total_events * 2); // avg by = 2
    assert_eq!(rev.raw(), total_events - 1);

    let snap = engine.state_of::<OccTotal>(id).await.unwrap().expect("registry state present");
    assert_eq!(snap, loaded, "no skipped folds: registry == log fold");
    assert_invariant::<OccTotal, OccFact>(&engine, &store, id, "occ 8-way").await;
}

// ─────────────────────────────────────────────────────────────────────
// Attack 6 — seeding race: build() a new engine (reactor cursor seeded
// at latest) while a second already-running engine floods emits into the
// same store. The reacted set must be a clean suffix of the log (no
// holes), and every trigger emitted via the NEW engine after build()
// returns must fire.
// ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SeedPing {
    id: Uuid,
}
impl Event for SeedPing {
    const NAME: &'static str = "af_seed";
    fn subject_id(&self) -> Uuid {
        self.id
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SeedPong {
    id: Uuid,
}
impl Event for SeedPong {
    const NAME: &'static str = "af_seedpong";
    fn subject_id(&self) -> Uuid {
        self.id
    }
}

struct SeedEcho;
#[async_trait]
impl Reactor for SeedEcho {
    type Trigger = SeedPing;
    const NAME: &'static str = "af.seed-echo";
    async fn react(&self, t: &SeedPing, _ctx: Ctx<'_>) -> Result<Events> {
        Ok(causal::events![SeedPong { id: t.id }])
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn seeding_race_reacted_set_is_clean_suffix() {
    let store = Arc::new(MemoryStore::new());
    // Already-running engine: pure emitter, no consumers.
    let flooder = Arc::new(backend(&store).build().await.unwrap());

    let stop = Arc::new(AtomicBool::new(false));
    let flood = {
        let flooder = flooder.clone();
        let stop = stop.clone();
        tokio::spawn(async move {
            let mut n = 0u32;
            while !stop.load(Ordering::Relaxed) && n < 1200 {
                flooder
                    .emit(SeedPing { id: Uuid::new_v4() })
                    .await
                    .expect("flood emit failed");
                n += 1;
                tokio::task::yield_now().await;
            }
        })
    };

    // Race build() (cursor seeding reads latest_position) against the flood.
    tokio::time::sleep(Duration::from_millis(2)).await;
    let fresh = Arc::new(
        backend(&store)
            .with_reactor(SeedEcho)
            .build()
            .await
            .unwrap(),
    );

    // Everything emitted via the NEW engine after build() returns MUST fire.
    let mut my_positions = Vec::new();
    for _ in 0..20 {
        let r = fresh
            .emit(SeedPing { id: Uuid::new_v4() })
            .await
            .expect("post-build emit failed");
        my_positions.push(r.position);
    }

    stop.store(true, Ordering::Relaxed);
    flood.await.unwrap();

    // Quiesce: the reactor cursor catches the (now stable) log tail.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let latest = EventLogBackend::latest_position(store.as_ref())
            .await
            .unwrap();
        let cur = CheckpointStore::get(store.as_ref(), SeedEcho::NAME)
            .await
            .unwrap();
        if cur.map_or(false, |c| c >= latest) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "reactor never quiesced after seeding race"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let log = EventLogBackend::read_all(store.as_ref(), LogCursor::ZERO, 1_000_000)
        .await
        .unwrap();
    let triggers: Vec<_> = log
        .iter()
        .filter(|e| e.event_type == "af_seed")
        .collect();
    let reacted: Vec<bool> = triggers
        .iter()
        .map(|t| {
            let want = derive_output_event_id(SeedEcho::NAME, t.event_id, "af_seedpong", t.subject_id, 0);
            log.iter().any(|e| e.event_id == want)
        })
        .collect();

    // Clean suffix: once the first reacted trigger appears (in log order),
    // every later trigger must have reacted too — no holes, no torn window.
    if let Some(first) = reacted.iter().position(|&b| b) {
        for (offset, flag) in reacted[first..].iter().enumerate() {
            assert!(
                *flag,
                "torn seeding: trigger at log index {} skipped AFTER index {} reacted",
                first + offset,
                first
            );
        }
    }

    // All post-build emits via the new engine reacted.
    for pos in &my_positions {
        let (idx, _t) = triggers
            .iter()
            .enumerate()
            .find(|(_, t)| t.position == *pos)
            .expect("post-build trigger present in log");
        assert!(
            reacted[idx],
            "trigger emitted via the NEW engine after build() at {pos:?} did not fire"
        );
    }
}

// ─────────────────────────────────────────────────────────────────────
// Attack 7 — settle() must stay scoped: a perpetual flood of unrelated
// workflows must not delay (or hang) another run's settle.
// ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Work {
    id: Uuid,
}
impl Event for Work {
    const NAME: &'static str = "af_work";
    fn subject_id(&self) -> Uuid {
        self.id
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Done {
    id: Uuid,
}
impl Event for Done {
    const NAME: &'static str = "af_done";
    fn subject_id(&self) -> Uuid {
        self.id
    }
}

struct WorkEcho;
#[async_trait]
impl Reactor for WorkEcho {
    type Trigger = Work;
    const NAME: &'static str = "af.work-echo";
    async fn react(&self, t: &Work, _ctx: Ctx<'_>) -> Result<Events> {
        Ok(causal::events![Done { id: t.id }])
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn settle_returns_promptly_under_unrelated_flood() {
    let store = Arc::new(MemoryStore::new());
    let engine = Arc::new(backend(&store).with_reactor(WorkEcho).build().await.unwrap());

    let stop = Arc::new(AtomicBool::new(false));
    let flood = {
        let engine = engine.clone();
        let stop = stop.clone();
        tokio::spawn(async move {
            let mut n = 0u32;
            // Unrelated workflows, fire-and-forget, perpetual until stop.
            while !stop.load(Ordering::Relaxed) && n < 6000 {
                engine
                    .emit(Work { id: Uuid::new_v4() })
                    .await
                    .expect("flood emit failed");
                n += 1;
                tokio::task::yield_now().await;
            }
        })
    };

    for i in 0..12 {
        timeout(Duration::from_secs(10), async {
            engine.emit(Work { id: Uuid::new_v4() }).settled().await
        })
        .await
        .unwrap_or_else(|_| panic!("settle hung under unrelated flood (iteration {i})"))
        .expect("settled emit failed");
    }

    stop.store(true, Ordering::Relaxed);
    flood.await.unwrap();
}

// ─────────────────────────────────────────────────────────────────────
// Attack 8 — gap-repair convergence with foreign (non-folding) events
// interleaved in the SAME co-located stream, under concurrent emits.
// A foreign event leaves the watermark behind; the next folding event
// must repair via the identity fold (advance_watermark) and converge
// (fold_event errors loudly on non-convergence, which would surface as
// an emit Err here).
//
// HISTORY: this test used to fail intermittently (~1/20 runs) by losing
// a fold — the vacant-entry restore TOCTOU pinned by
// `vacant_registry_restore_race_loses_folds` below (the racing first
// folds of this stream go through the same restore path, with foreign
// events widening the read-tail/replay window). FIXED by the 2026-06-10
// monotonic-`set_state` remediation (see attack 9c's header); verified
// 2026-06-12 with 0 failures in 300 runs (200 release + 100 debug).
// ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Dep {
    acct: Uuid,
    amt: i64,
}
impl Event for Dep {
    const NAME: &'static str = "af_dep";
    const SUBJECT: &'static str = "af_acct";
    fn subject_id(&self) -> Uuid {
        self.acct
    }
}

/// Lives in the same `af_acct` stream but matches NO aggregator.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ForeignNote {
    acct: Uuid,
}
impl Event for ForeignNote {
    const NAME: &'static str = "af_noise";
    const SUBJECT: &'static str = "af_acct";
    fn subject_id(&self) -> Uuid {
        self.acct
    }
}

#[derive(Default, Clone, Debug, PartialEq, Serialize, Deserialize)]
struct AcctBalance {
    n: u64,
    sum: i64,
}
impl Aggregate for AcctBalance {
    const NAME: &'static str = "AcctBalance";
    const SUBJECT: &'static str = "af_acct";
}
impl Apply<Dep> for AcctBalance {
    fn apply(&mut self, f: &Dep) {
        self.n += 1;
        self.sum += f.amt;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn gap_repair_converges_through_foreign_events_in_stream() {
    let store = Arc::new(MemoryStore::new());
    let engine = Arc::new(
        backend(&store)
            .with_aggregators([Aggregator::for_type::<AcctBalance, Dep>()])
            .build()
            .await
            .unwrap(),
    );
    let acct = Uuid::new_v4();

    const TASKS: u64 = 6;
    const PAIRS: u64 = 10;
    let mut handles = Vec::new();
    for t in 0..TASKS {
        let engine = engine.clone();
        handles.push(tokio::spawn(async move {
            for k in 0..PAIRS {
                // Alternate foreign / folding so watermarks constantly trail.
                engine
                    .emit(ForeignNote { acct })
                    .await
                    .expect("foreign emit failed");
                engine
                    .emit(Dep {
                        acct,
                        amt: (t * PAIRS + k) as i64,
                    })
                    .await
                    .expect("folding emit failed — gap repair did not converge?");
            }
        }));
    }
    for h in handles {
        h.await.unwrap();
    }

    let snap = engine.state_of::<AcctBalance>(acct).await.unwrap().expect("state present");
    assert_eq!(snap.n, TASKS * PAIRS, "every Dep folded exactly once");
    assert_invariant::<AcctBalance, Dep>(&engine, &store, acct, "foreign-interleaved stream").await;

    // The stream really is mixed (the attack premise holds).
    let evs = EventLogBackend::read_stream(store.as_ref(), "af_acct", acct, None)
        .await
        .unwrap();
    assert_eq!(evs.len() as u64, TASKS * PAIRS * 2);
}

// ─────────────────────────────────────────────────────────────────────
// Attack 9b — snapshot_every=1 under concurrent emits, then durable
// restore on a fresh engine. Targets the non-atomic (version, state)
// read in maybe_save_snapshots: a torn snapshot claims revision r but
// contains folds past r, so restore double-applies the tail.
// ─────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn snapshot_every_one_storm_then_durable_restore() {
    for round in 0..4 {
        let store = Arc::new(MemoryStore::new());
        let engine = Arc::new(
            backend(&store)
                .with_aggregators([Aggregator::for_type::<HitTotal, Hit>()])
                .with_snapshot_store(store.clone() as Arc<dyn SnapshotStore>)
                .with_snapshot_every(1)
                .build()
                .await
                .unwrap(),
        );
        let id = Uuid::new_v4();

        const TASKS: u64 = 8;
        const PER: u64 = 15;
        let mut handles = Vec::new();
        for _ in 0..TASKS {
            let engine = engine.clone();
            handles.push(tokio::spawn(async move {
                for _ in 0..PER {
                    engine
                        .emit(Hit { stream: id, amount: 1 })
                        .await
                        .expect("emit failed");
                }
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        assert_invariant::<HitTotal, Hit>(&engine, &store, id, "snapshot=1 storm (live)").await;

        // Direct snapshot-consistency audit: a snapshot at revision r MUST
        // contain exactly r+1 folds of this count-by-one stream.
        if let Some(snap) = SnapshotStore::load_snapshot(store.as_ref(), "HitTotal", id)
            .await
            .unwrap()
        {
            let state: HitTotal = serde_json::from_value(snap.state.clone()).unwrap();
            assert_eq!(
                state.n,
                snap.revision.raw() + 1,
                "round {round}: TORN SNAPSHOT — claims revision {} but contains {} folds",
                snap.revision.raw(),
                state.n,
            );
        }

        // Restart: read-through restore (snapshot + tail) must equal the
        // from-scratch fold.
        let engine2 = backend(&store)
            .with_aggregators([Aggregator::for_type::<HitTotal, Hit>()])
            .with_snapshot_store(store.clone() as Arc<dyn SnapshotStore>)
            .build()
            .await
            .unwrap();
        let restored = engine2
            .state_of::<HitTotal>(id)
            .await
            .unwrap()
            .expect("restorable aggregate");
        let (expected, _) = replay_one::<HitTotal, Hit>(&store, id).await;
        assert_eq!(
            restored, expected,
            "round {round}: durable restore diverged from log fold (torn snapshot replayed)"
        );
    }
}

// ─────────────────────────────────────────────────────────────────────
// Attack 9c — REGRESSION GUARD for a real TOCTOU fold-loss bug found by
// the adversarial concurrency pass (originally failed ~every run at
// commit 1e91b90; FIXED 2026-06-10).
//
// Shape: a stream with history (30 events), a fresh engine whose
// registry is vacant for it, and a handful of CONCURRENT emits.
// No state_of, no snapshot store, no reactors required.
//
// The bug (aggregator.rs): each concurrent emit's `fold_event`
// (unbounded) saw a vacant entry at revision > 0 → `FoldGap` →
// `repair_gap` → `restore_aggregate`, which read the stream tail
// asynchronously then `set_state`'d UNCONDITIONALLY. A restore that
// suspended between `read_stream` and `set_state` clobbered an entry
// that concurrent restores/folds had already advanced further,
// regressing both state and the `version` watermark (and, when the
// regression tripped a concurrent `repair_gap` loop, advancing the
// watermark past never-folded events → permanent desync).
//
// The fix: `set_state` is now monotonic (installs only when strictly
// newer, under the DashMap entry guard — restore is a read-through
// cache fill and must never regress), and `repair_gap` no longer
// advances the watermark when `apply_event` reports a gap (a
// concurrent writer is mid-flight; `fold_event`'s outer loop
// re-repairs). The master invariant below now holds under contention.
// ─────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn vacant_registry_restore_race_loses_folds() {
    const HISTORY: u64 = 30;
    const CONCURRENT: u64 = 6;
    for round in 0..10 {
        let store = Arc::new(MemoryStore::new());

        // Seed history through a throwaway engine.
        let id = Uuid::new_v4();
        {
            let seeder = backend(&store)
                .with_aggregators([Aggregator::for_type::<HitTotal, Hit>()])
                .build()
                .await
                .unwrap();
            for _ in 0..HISTORY {
                seeder
                    .emit(Hit { stream: id, amount: 1 })
                    .await
                    .expect("seed emit failed");
            }
        }

        // Fresh engine — vacant registry entry for `id`.
        let engine = Arc::new(
            backend(&store)
                .with_aggregators([Aggregator::for_type::<HitTotal, Hit>()])
                .build()
                .await
                .unwrap(),
        );

        let mut handles = Vec::new();
        for _ in 0..CONCURRENT {
            let engine = engine.clone();
            handles.push(tokio::spawn(async move {
                engine
                    .emit(Hit { stream: id, amount: 1 })
                    .await
                    .expect("emit failed");
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        let snap = engine.state_of::<HitTotal>(id).await.unwrap().expect("state present");
        assert_eq!(
            snap.n,
            HISTORY + CONCURRENT,
            "round {round}: vacant-entry restore TOCTOU lost folds \
             (got {}, want {})",
            snap.n,
            HISTORY + CONCURRENT,
        );
        assert_invariant::<HitTotal, Hit>(&engine, &store, id, "vacant-registry restore race")
            .await;
    }
}
