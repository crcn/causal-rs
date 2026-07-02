//! DecisionStore conformance suite run against `causal::InMemoryDecisionStore`.
//!
//! `InMemoryDecisionStore` is the reference implementation — these tests
//! must always pass. If any fail, the conformance scenarios themselves are
//! wrong (or the trait contract drifted).

use anyhow::Result;
use causal::InMemoryDecisionStore;
use causal_replay::conformance;

fn s() -> InMemoryDecisionStore {
    InMemoryDecisionStore::new()
}

#[tokio::test]
async fn decision_store_get_miss() -> Result<()> {
    conformance::decision_store_get_miss(&s()).await
}

#[tokio::test]
async fn decision_store_seal_then_get_round_trips() -> Result<()> {
    conformance::decision_store_seal_then_get_round_trips(&s()).await
}

#[tokio::test]
async fn decision_store_seal_is_first_write_wins() -> Result<()> {
    conformance::decision_store_seal_is_first_write_wins(&s()).await
}

#[tokio::test]
async fn decision_store_empty_record_is_present() -> Result<()> {
    conformance::decision_store_empty_record_is_present(&s()).await
}

#[tokio::test]
async fn decision_store_remove_makes_record_absent() -> Result<()> {
    conformance::decision_store_remove_makes_record_absent(&s()).await
}

#[tokio::test]
async fn decision_store_remove_is_idempotent() -> Result<()> {
    conformance::decision_store_remove_is_idempotent(&s()).await
}

#[tokio::test]
async fn decision_store_isolated_by_consumer() -> Result<()> {
    conformance::decision_store_isolated_by_consumer(&s()).await
}

#[tokio::test]
async fn decision_store_isolated_by_trigger() -> Result<()> {
    conformance::decision_store_isolated_by_trigger(&s()).await
}

#[tokio::test]
async fn decision_store_strips_nul_bytes() -> Result<()> {
    conformance::decision_store_strips_nul_bytes(&s()).await
}

#[tokio::test]
async fn decision_store_retention_gc_by_age() -> Result<()> {
    conformance::decision_store_retention_gc_by_age(&s()).await
}
