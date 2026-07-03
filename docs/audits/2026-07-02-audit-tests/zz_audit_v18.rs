//! Audit verifier #18: a reactor body that never returns should be
//! surfaced by SOME settle escape hatch when the caller configures
//! with_settle_liveness_ceiling — the D3 knob whose stated purpose is to
//! convert silent infinite settle hangs into loud typed errors.
//!
//! Claim under test: none of the three escape hatches (ConsumerHealth
//! wedge, worker_stall, liveness ceiling) can see a hung react() body,
//! because the supervisor keeps completing Idle cycles (refreshing
//! last_activity) and the worker never fails an attempt (worker_stall
//! stays 0). So settle polls drained() forever.
//!
//! Assertion direction: CORRECT behavior = settle returns (an error)
//! within the 5s outer timeout when the ceiling is 300ms. The defect
//! makes the outer timeout fire -> test FAILS.

use anyhow::Result;
use async_trait::async_trait;
use causal::{Ctx, EngineBuilder, Event, Events, Reactor};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Kick {
    id: Uuid,
}
impl Event for Kick {
    const NAME: &'static str = "audit18_kick";
    const SUBJECT: &'static str = "audit18";
    fn subject_id(&self) -> Uuid {
        self.id
    }
}

/// Reactor whose body hangs forever — models a hung HTTP/LLM call or a
/// deadlocked channel recv. No attempt_timeout configured (the default).
struct HungReactor;
#[async_trait]
impl Reactor for HungReactor {
    type Trigger = Kick;
    const NAME: &'static str = "audit18.hung";
    async fn react(&self, _t: &Kick, _ctx: Ctx<'_>) -> Result<Events> {
        std::future::pending::<()>().await; // never completes
        unreachable!()
    }
}

#[tokio::test]
async fn hung_react_body_is_surfaced_by_settle_liveness_ceiling() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("causal=warn")
        .with_test_writer()
        .try_init();

    const CEILING: Duration = Duration::from_millis(300);

    let engine = EngineBuilder::memory()
        .with_reactor(HungReactor)
        .build()
        .await
        .unwrap()
        .with_settle_liveness_ceiling(Some(CEILING));

    let run = Uuid::new_v4();

    // Outer timeout is >16x the ceiling: if the ceiling (or any other
    // escape hatch) can see the hung body, settle returns an error well
    // within it. If nothing can, settle is still pending at 5s.
    let settled = tokio::time::timeout(
        Duration::from_secs(5),
        engine.emit(Kick { id: run }).workflow_id(run).settled(),
    )
    .await;

    match settled {
        Err(_elapsed) => {
            panic!(
                "DEFECT CONFIRMED: settle() hung past 5s despite \
                 with_settle_liveness_ceiling(300ms) — a never-returning \
                 react() body is invisible to the wedge guard, worker_stall, \
                 and the D3 liveness ceiling",
            );
        }
        Ok(Err(e)) => {
            // Correct behavior: some escape hatch surfaced the hang.
            eprintln!("settle surfaced the hang as expected: {e:#}");
            assert!(
                e.downcast_ref::<causal::SettleTimeout>().is_some()
                    || format!("{e}").contains("wedged"),
                "settle errored but not via a liveness/wedge mechanism: {e:#}",
            );
        }
        Ok(Ok(_result)) => {
            panic!(
                "settle returned Ok while the trigger's react() body was \
                 still hung — quiescence promise violated (worse than the \
                 claimed defect)",
            );
        }
    }
}
