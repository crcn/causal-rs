//! Conformance suite run against `causal::MemoryStore`.
//!
//! `MemoryStore` is the reference implementation — these tests must
//! always pass. If they don't, the conformance suite itself is
//! probably wrong (or the trait contract drifted).

use anyhow::Result;
use causal::MemoryStore;
use causal_replay::conformance;

fn b() -> MemoryStore {
    MemoryStore::new()
}

#[tokio::test]
async fn append_is_idempotent_on_event_id() -> Result<()> {
    conformance::append_is_idempotent_on_event_id(&b()).await
}

#[tokio::test]
async fn fresh_stream_first_event_lands_at_revision_zero() -> Result<()> {
    conformance::fresh_stream_first_event_lands_at_revision_zero(&b()).await
}

#[tokio::test]
async fn revision_is_monotonic_within_stream() -> Result<()> {
    conformance::revision_is_monotonic_within_stream(&b()).await
}

#[tokio::test]
async fn append_to_stream_rejects_stale_expected() -> Result<()> {
    conformance::append_to_stream_rejects_stale_expected(&b()).await
}

#[tokio::test]
async fn append_to_stream_idempotent_on_event_id_retry() -> Result<()> {
    conformance::append_to_stream_idempotent_on_event_id_retry(&b()).await
}

#[tokio::test]
async fn append_to_stream_batch_lands_atomically() -> Result<()> {
    conformance::append_to_stream_batch_lands_atomically(&b()).await
}

#[tokio::test]
async fn append_to_stream_batch_idempotent_on_replay() -> Result<()> {
    conformance::append_to_stream_batch_idempotent_on_replay(&b()).await
}

#[tokio::test]
async fn expected_revision_ahead_of_head_is_rejected() -> Result<()> {
    conformance::expected_revision_ahead_of_head_is_rejected(&b()).await
}

#[tokio::test]
async fn cas_redelivery_after_foreign_write_returns_original_result() -> Result<()> {
    conformance::cas_redelivery_after_foreign_write_returns_original_result(&b()).await
}

#[tokio::test]
async fn any_redelivery_after_foreign_write_is_idempotent() -> Result<()> {
    conformance::any_redelivery_after_foreign_write_is_idempotent(&b()).await
}

#[tokio::test]
async fn cas_partial_overlap_batch_is_rejected_loudly() -> Result<()> {
    conformance::cas_partial_overlap_batch_is_rejected_loudly(&b()).await
}

#[tokio::test]
async fn read_stream_partitions_by_aggregate_id() -> Result<()> {
    conformance::read_stream_partitions_by_aggregate_id(&b()).await
}

#[tokio::test]
async fn read_stream_after_revision_is_strict() -> Result<()> {
    conformance::read_stream_after_revision_is_strict(&b()).await
}

#[tokio::test]
async fn read_stream_returns_empty_for_missing_stream() -> Result<()> {
    conformance::read_stream_returns_empty_for_missing_stream(&b()).await
}

#[tokio::test]
async fn read_all_returns_events_strictly_after_cursor() -> Result<()> {
    conformance::read_all_returns_events_strictly_after_cursor(&b()).await
}

#[tokio::test]
async fn latest_position_reflects_committed_writes() -> Result<()> {
    conformance::latest_position_reflects_committed_writes(&b()).await
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_appends_are_tailable_without_loss() -> Result<()> {
    conformance::concurrent_appends_are_tailable_without_loss(&b()).await
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_any_appends_all_succeed() -> Result<()> {
    conformance::concurrent_any_appends_all_succeed(&b()).await
}
