//! Conformance suite run against `KurrentEventLogBackend`.
//!
//! `#[ignore]`'d by default; run against a live KurrentDB on
//! `KURRENT_URL` (default `kurrentdb://localhost:2113?tls=false`):
//!
//!   cargo test -p causal_replay --features kurrent \
//!     --test kurrent_event_log_conformance_test -- --ignored --nocapture

#![cfg(feature = "kurrent")]

use anyhow::Result;
use causal_replay::{conformance, KurrentEventLogBackend};

fn connection_string() -> String {
    std::env::var("KURRENT_URL")
        .unwrap_or_else(|_| "kurrentdb://localhost:2113?tls=false".to_string())
}

fn backend() -> KurrentEventLogBackend {
    KurrentEventLogBackend::connect(&connection_string())
        .expect("Kurrent connect failed — is the server up at KURRENT_URL?")
}

#[tokio::test]
#[ignore = "requires running Kurrent on KURRENT_URL"]
async fn append_is_idempotent_on_event_id() -> Result<()> {
    conformance::append_is_idempotent_on_event_id(&backend()).await
}

#[tokio::test]
#[ignore = "requires running Kurrent on KURRENT_URL"]
async fn fresh_stream_first_event_lands_at_revision_zero() -> Result<()> {
    conformance::fresh_stream_first_event_lands_at_revision_zero(&backend()).await
}

#[tokio::test]
#[ignore = "requires running Kurrent on KURRENT_URL"]
async fn revision_is_monotonic_within_stream() -> Result<()> {
    conformance::revision_is_monotonic_within_stream(&backend()).await
}

#[tokio::test]
#[ignore = "requires running Kurrent on KURRENT_URL"]
async fn append_to_stream_rejects_stale_expected() -> Result<()> {
    conformance::append_to_stream_rejects_stale_expected(&backend()).await
}

#[tokio::test]
#[ignore = "requires running Kurrent on KURRENT_URL"]
async fn append_to_stream_idempotent_on_event_id_retry() -> Result<()> {
    conformance::append_to_stream_idempotent_on_event_id_retry(&backend()).await
}

#[tokio::test]
#[ignore = "requires running Kurrent on KURRENT_URL"]
async fn append_to_stream_batch_lands_atomically() -> Result<()> {
    conformance::append_to_stream_batch_lands_atomically(&backend()).await
}

#[tokio::test]
#[ignore = "requires running Kurrent on KURRENT_URL"]
async fn append_to_stream_batch_idempotent_on_replay() -> Result<()> {
    conformance::append_to_stream_batch_idempotent_on_replay(&backend()).await
}

#[tokio::test]
#[ignore = "requires running Kurrent on KURRENT_URL"]
async fn expected_revision_ahead_of_head_is_rejected() -> Result<()> {
    conformance::expected_revision_ahead_of_head_is_rejected(&backend()).await
}

#[tokio::test]
#[ignore = "requires running Kurrent on KURRENT_URL"]
async fn cas_redelivery_after_foreign_write_returns_original_result() -> Result<()> {
    conformance::cas_redelivery_after_foreign_write_returns_original_result(&backend()).await
}

#[tokio::test]
#[ignore = "requires running Kurrent on KURRENT_URL"]
async fn any_redelivery_after_foreign_write_is_idempotent() -> Result<()> {
    conformance::any_redelivery_after_foreign_write_is_idempotent(&backend()).await
}

#[tokio::test]
#[ignore = "requires running Kurrent on KURRENT_URL"]
async fn cas_partial_overlap_batch_is_rejected_loudly() -> Result<()> {
    conformance::cas_partial_overlap_batch_is_rejected_loudly(&backend()).await
}

#[tokio::test]
#[ignore = "requires running Kurrent on KURRENT_URL"]
async fn read_stream_partitions_by_aggregate_id() -> Result<()> {
    conformance::read_stream_partitions_by_aggregate_id(&backend()).await
}

#[tokio::test]
#[ignore = "requires running Kurrent on KURRENT_URL"]
async fn read_stream_after_revision_is_strict() -> Result<()> {
    conformance::read_stream_after_revision_is_strict(&backend()).await
}

#[tokio::test]
#[ignore = "requires running Kurrent on KURRENT_URL"]
async fn read_stream_returns_empty_for_missing_stream() -> Result<()> {
    conformance::read_stream_returns_empty_for_missing_stream(&backend()).await
}

#[tokio::test]
#[ignore = "requires running Kurrent on KURRENT_URL"]
async fn read_all_returns_events_strictly_after_cursor() -> Result<()> {
    conformance::read_all_returns_events_strictly_after_cursor(&backend()).await
}

#[tokio::test]
#[ignore = "requires running Kurrent on KURRENT_URL"]
async fn latest_position_reflects_committed_writes() -> Result<()> {
    conformance::latest_position_reflects_committed_writes(&backend()).await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires running Kurrent on KURRENT_URL"]
async fn concurrent_appends_are_tailable_without_loss() -> Result<()> {
    conformance::concurrent_appends_are_tailable_without_loss(&backend()).await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires running Kurrent on KURRENT_URL"]
async fn concurrent_any_appends_all_succeed() -> Result<()> {
    conformance::concurrent_any_appends_all_succeed(&backend()).await
}
