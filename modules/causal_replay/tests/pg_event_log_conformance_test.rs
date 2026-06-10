//! Conformance suite run against `PgEventLogBackend`.
//!
//! `#[ignore]`'d by default; run against a local Postgres with
//! migration 054 applied:
//!
//!   cargo test -p causal_replay --features postgres \
//!     --test pg_event_log_conformance_test -- --ignored --nocapture

#![cfg(feature = "postgres")]

use anyhow::Result;
use causal_replay::{conformance, PgEventLogBackend};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

fn load_env() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join(".env");
    if let Ok(content) = std::fs::read_to_string(&path) {
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((key, value)) = line.split_once('=') {
                if std::env::var(key.trim()).is_err() {
                    std::env::set_var(key.trim(), value.trim());
                }
            }
        }
    }
}

async fn connect_local() -> PgPool {
    load_env();
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL required");
    assert!(
        url.contains("localhost") || url.contains("127.0.0.1"),
        "Refusing to run integration tests against non-local Postgres: {url}"
    );
    PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .expect("connect local postgres")
}

async fn backend() -> PgEventLogBackend {
    PgEventLogBackend::new(connect_local().await)
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL + migration 054"]
async fn append_is_idempotent_on_event_id() -> Result<()> {
    conformance::append_is_idempotent_on_event_id(&backend().await).await
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL + migration 054"]
async fn fresh_stream_first_event_lands_at_revision_zero() -> Result<()> {
    conformance::fresh_stream_first_event_lands_at_revision_zero(&backend().await).await
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL + migration 054"]
async fn revision_is_monotonic_within_stream() -> Result<()> {
    conformance::revision_is_monotonic_within_stream(&backend().await).await
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL + migration 054"]
async fn append_to_stream_rejects_stale_expected() -> Result<()> {
    conformance::append_to_stream_rejects_stale_expected(&backend().await).await
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL + migration 054"]
async fn append_to_stream_idempotent_on_event_id_retry() -> Result<()> {
    conformance::append_to_stream_idempotent_on_event_id_retry(&backend().await).await
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL + migration 054"]
async fn append_to_stream_batch_lands_atomically() -> Result<()> {
    conformance::append_to_stream_batch_lands_atomically(&backend().await).await
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL + migration 054"]
async fn append_to_stream_batch_idempotent_on_replay() -> Result<()> {
    conformance::append_to_stream_batch_idempotent_on_replay(&backend().await).await
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL + migration 054"]
async fn expected_revision_ahead_of_head_is_rejected() -> Result<()> {
    conformance::expected_revision_ahead_of_head_is_rejected(&backend().await).await
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL + migration 054"]
async fn cas_redelivery_after_foreign_write_returns_original_result() -> Result<()> {
    conformance::cas_redelivery_after_foreign_write_returns_original_result(&backend().await)
        .await
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL + migration 054"]
async fn any_redelivery_after_foreign_write_is_idempotent() -> Result<()> {
    conformance::any_redelivery_after_foreign_write_is_idempotent(&backend().await)
        .await
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL + migration 054"]
async fn cas_partial_overlap_batch_is_rejected_loudly() -> Result<()> {
    conformance::cas_partial_overlap_batch_is_rejected_loudly(&backend().await).await
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL + migration 054"]
async fn read_stream_partitions_by_aggregate_id() -> Result<()> {
    conformance::read_stream_partitions_by_aggregate_id(&backend().await).await
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL + migration 054"]
async fn read_stream_after_revision_is_strict() -> Result<()> {
    conformance::read_stream_after_revision_is_strict(&backend().await).await
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL + migration 054"]
async fn read_stream_returns_empty_for_missing_stream() -> Result<()> {
    conformance::read_stream_returns_empty_for_missing_stream(&backend().await).await
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL + migration 054"]
async fn read_all_returns_events_strictly_after_cursor() -> Result<()> {
    conformance::read_all_returns_events_strictly_after_cursor(&backend().await).await
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL + migration 054"]
async fn latest_position_reflects_committed_writes() -> Result<()> {
    conformance::latest_position_reflects_committed_writes(&backend().await).await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires local DATABASE_URL + migration 054"]
async fn concurrent_appends_are_tailable_without_loss() -> Result<()> {
    conformance::concurrent_appends_are_tailable_without_loss(&backend().await).await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires local DATABASE_URL + migration 054"]
async fn concurrent_any_appends_all_succeed() -> Result<()> {
    conformance::concurrent_any_appends_all_succeed(&backend().await).await
}
