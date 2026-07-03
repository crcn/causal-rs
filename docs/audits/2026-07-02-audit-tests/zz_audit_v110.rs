//! Audit v110: macro attribute parsing silently drops natural spellings.
//!
//! Each test asserts the semantics the source DECLARES. A correct macro
//! would either honor the declaration or reject it at compile time
//! (per the crate's own "wrongness must be loud" contract). If these
//! tests COMPILE and then FAIL, the declaration was silently dropped.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use causal::contexts::Ctx;
use causal::event::Event;
use causal::reactor::{Events, Ordering, Reactor, RetryPolicy};

// ── Case A: #[event] workflow_id with an UNQUOTED value (Expr::Path,
//    the grammar #[reactor] itself uses for `ordering = per_workflow`).
#[causal::event(name = "audit110_run_started", subject_id = "run_id", workflow_id = run_id)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunStartedUnquoted {
    pub run_id: Uuid,
}

// ── Case B: #[event] with a typo'd key (`workflow` for `workflow_id`).
#[causal::event(name = "audit110_run_started2", subject_id = "run_id", workflow = "run_id")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunStartedTypoKey {
    pub run_id: Uuid,
}

// ── Cases C/D/E: #[reactor] value-shape fall-throughs.
#[causal::reactors]
mod audit_reactors {
    use super::*;

    // C: ordering as a QUOTED string — the value shape every neighboring
    // string attr (name, subject_id, kinds entries) uses.
    #[reactor(name = "audit110.quoted_ordering", ordering = "per_workflow")]
    async fn quoted_ordering(_t: &RunStartedUnquoted, _ctx: Ctx<'_>) -> Result<Events> {
        Ok(causal::events![])
    }

    // D: ordering as a multi-segment path (the enum's real name).
    #[reactor(name = "audit110.path_ordering", ordering = Ordering::PerWorkflow)]
    async fn path_ordering(_t: &RunStartedUnquoted, _ctx: Ctx<'_>) -> Result<Events> {
        Ok(causal::events![])
    }

    // E: backoff_multiplier as an INT literal (2 instead of 2.0).
    #[reactor(name = "audit110.int_backoff", backoff_multiplier = 2)]
    async fn int_backoff(_t: &RunStartedUnquoted, _ctx: Ctx<'_>) -> Result<Events> {
        Ok(causal::events![])
    }
}

#[test]
fn event_workflow_id_unquoted_is_honored_or_rejected() {
    let run_id = Uuid::new_v4();
    let e = RunStartedUnquoted { run_id };
    assert_eq!(
        e.declared_workflow_id(),
        Some(run_id),
        "workflow_id = run_id (unquoted) compiled but was silently dropped: \
         this fact is a chain member, so every top-level emit mints a fresh \
         v4 workflow (engine.rs:2377)"
    );
}

#[test]
fn event_workflow_typo_key_is_rejected_or_honored() {
    let run_id = Uuid::new_v4();
    let e = RunStartedTypoKey { run_id };
    assert_eq!(
        e.declared_workflow_id(),
        Some(run_id),
        "typo'd key `workflow = \"run_id\"` compiled clean and was silently \
         swallowed by the `_ => {{}}` arm — no unknown-argument diagnostic"
    );
}

#[test]
fn reactor_quoted_ordering_is_honored_or_rejected() {
    assert_eq!(
        <audit_reactors::__causal_reactor_quoted_ordering as Reactor>::ORDERING,
        Ordering::PerWorkflow,
        "ordering = \"per_workflow\" (quoted) compiled but was silently \
         dropped: reactor runs with the PerSubject default"
    );
}

#[test]
fn reactor_path_ordering_is_honored_or_rejected() {
    assert_eq!(
        <audit_reactors::__causal_reactor_path_ordering as Reactor>::ORDERING,
        Ordering::PerWorkflow,
        "ordering = Ordering::PerWorkflow (multi-segment path) compiled but \
         was silently dropped: reactor runs with the PerSubject default"
    );
}

#[test]
fn reactor_int_backoff_multiplier_is_honored_or_rejected() {
    let r = audit_reactors::__causal_reactor_int_backoff;
    assert_eq!(
        r.retry_policy(),
        Some(RetryPolicy {
            max_attempts: 3,
            initial_backoff_ms: 25,
            backoff_multiplier: 2.0,
            max_backoff_ms: 5_000,
        }),
        "backoff_multiplier = 2 (int literal) compiled but was silently \
         dropped: since it was the ONLY retry param, retry_policy() was not \
         generated at all — trait default None"
    );
}
