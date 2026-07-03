//! AUDIT VERIFIER (finding #13): FoldOnReadCache degenerates the
//! (prev, curr) transition on a retry attempt of the SAME trigger.
//!
//! Shape: PingCount folds Ping (SUBJECT "ping"). ThresholdReactor is
//! triggered by Ping and gates on the transition its own trigger caused:
//! `prev.n < 3 && curr.n >= 3` -> emit Threshold. On the trigger where the
//! gate first fires (ping #3), the body fails ONCE with a transient error
//! AFTER calling ctx.state_of (so the worker-local fold cache is already
//! warmed to folded_to == trigger position). The runner retries the same
//! trigger with the same cache.
//!
//! CORRECT behavior: the retry attempt sees the same (prev=2, curr=3)
//! transition attempt 1 saw, the gate fires, Threshold is emitted, and
//! ThresholdSeen.seen == true.
//!
//! DEFECT behavior: the retry's fold_bounded hits the cache with
//! folded_to == bound, reads an empty tail, never captures `prev`, and
//! falls back to prev == curr (3, 3). The gate is false, the body returns
//! zero outputs, an EMPTY decision record seals (seal_empty_decisions
//! defaults true), and the Threshold output is lost permanently.

use anyhow::Result;
use async_trait::async_trait;
use causal::aggregate::{Aggregate, Apply};
use causal::{
    Aggregator, Ctx, EngineBuilder, Event, Events, Ordering, Reactor, RetryPolicy,
};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrd};
use std::sync::Mutex;
use uuid::Uuid;

// ── Facts ──

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Ping {
    id: Uuid,
}
impl Event for Ping {
    const NAME: &'static str = "ping";
    const SUBJECT: &'static str = "ping";
    fn subject_id(&self) -> Uuid {
        self.id
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Threshold {
    id: Uuid,
}
impl Event for Threshold {
    const NAME: &'static str = "threshold";
    const SUBJECT: &'static str = "thresh";
    fn subject_id(&self) -> Uuid {
        self.id
    }
}

// ── Aggregates ──

#[derive(Default, Clone, Debug, Serialize, Deserialize)]
struct PingCount {
    n: u32,
}
impl Aggregate for PingCount {
    const NAME: &'static str = "PingCount";
    const SUBJECT: &'static str = "ping";
}
impl Apply<Ping> for PingCount {
    fn apply(&mut self, _: &Ping) {
        self.n += 1;
    }
}

/// Witness for the reactor's output: seen == true iff Threshold was
/// ever appended to the log.
#[derive(Default, Clone, Debug, Serialize, Deserialize)]
struct ThresholdSeen {
    seen: bool,
}
impl Aggregate for ThresholdSeen {
    const NAME: &'static str = "ThresholdSeen";
    const SUBJECT: &'static str = "thresh";
}
impl Apply<Threshold> for ThresholdSeen {
    fn apply(&mut self, _: &Threshold) {
        self.seen = true;
    }
}

// ── Reactor: transition-gated, one injected transient failure ──

static FAILED_ONCE: AtomicBool = AtomicBool::new(false);
/// (prev.n, curr.n) per attempt, for the failure diagnostic.
static OBSERVED: Mutex<Vec<(u32, u32)>> = Mutex::new(Vec::new());

struct ThresholdReactor;
#[async_trait]
impl Reactor for ThresholdReactor {
    type Trigger = Ping;
    const NAME: &'static str = "threshold.reactor";
    const ORDERING: Ordering = Ordering::PerSubject;
    fn retry_policy(&self) -> Option<RetryPolicy> {
        // 1ms fixed backoff so the transient retry happens in test time.
        Some(RetryPolicy::fixed(10, 1))
    }
    async fn react(&self, t: &Ping, ctx: Ctx<'_>) -> Result<Events> {
        let st = ctx
            .state_of::<PingCount>(t.id)
            .await
            .map_err(causal::transient)?;
        let (p, c) = (st.prev.n, st.curr.n);
        OBSERVED.lock().unwrap().push((p, c));
        let crossed = p < 3 && c >= 3;
        // Inject exactly one transient failure, AFTER the state read, on
        // the first attempt where the gate fires — models any flaky await
        // (HTTP, LLM, projection query) between state_of and return.
        if crossed && !FAILED_ONCE.swap(true, AtomicOrd::SeqCst) {
            return Err(causal::transient(anyhow::anyhow!(
                "injected transient failure after state read"
            )));
        }
        let mut out = Events::new();
        if crossed {
            out.push(Threshold { id: t.id });
        }
        Ok(out)
    }
}

#[tokio::test]
async fn transition_gate_survives_transient_retry() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("causal=warn")
        .with_test_writer()
        .try_init();

    let engine = EngineBuilder::memory()
        .with_aggregators(vec![
            Aggregator::for_type::<PingCount, Ping>(),
            Aggregator::for_type::<ThresholdSeen, Threshold>(),
        ])
        .with_reactor(ThresholdReactor)
        .build()
        .await
        .unwrap();

    let id = Uuid::new_v4();
    for i in 0..3 {
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            engine.emit(Ping { id }).settled(),
        )
        .await
        .unwrap_or_else(|_| panic!("settle of ping #{} timed out", i + 1))
        .unwrap();
    }

    let observed: Vec<(u32, u32)> = OBSERVED.lock().unwrap().clone();
    let seen = engine
        .state_of::<ThresholdSeen>(id)
        .await
        .unwrap()
        .map(|s| s.seen)
        .unwrap_or(false);
    engine.shutdown().await.unwrap();

    assert!(
        FAILED_ONCE.load(AtomicOrd::SeqCst),
        "test harness bug: the injected failure never fired; observed = {observed:?}"
    );
    // CORRECT behavior: the retry re-observes the (2, 3) transition and
    // emits Threshold. DEFECT: the retry observes the degenerate (3, 3),
    // the gate stays closed, and an empty decision seals permanently.
    assert!(
        seen,
        "Threshold output LOST: (prev, curr) pairs per attempt were {observed:?} — \
         the retry attempt saw a degenerate prev == curr instead of the (2, 3) \
         transition, decided differently, and sealed that empty decision"
    );
}
