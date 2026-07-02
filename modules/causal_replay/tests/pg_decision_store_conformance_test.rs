//! DecisionStore conformance suite run against `PgDecisionStore`.
//!
//! All tests are `#[ignore]`'d by default; run against a local Postgres
//! with the causal_decisions schema applied:
//!
//!     DATABASE_URL=postgres://localhost/causal cargo test -p causal_replay \
//!         --features postgres --test pg_decision_store_conformance_test -- --ignored
#![cfg(feature = "postgres")]

use anyhow::Result;
use causal_replay::conformance;
use causal_replay::decision_store::PgDecisionStore;
use sqlx::postgres::PgPoolOptions;

async fn connect_local() -> sqlx::PgPool {
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

async fn backend() -> PgDecisionStore {
    let pool = connect_local().await;
    // Idempotent — safe to run every test; keeps the suite self-provisioning.
    PgDecisionStore::ensure_schema(&pool).await.expect("ensure decisions schema");
    PgDecisionStore::new(pool)
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL"]
async fn decision_store_get_miss() -> Result<()> {
    conformance::decision_store_get_miss(&backend().await).await
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL"]
async fn decision_store_seal_then_get_round_trips() -> Result<()> {
    conformance::decision_store_seal_then_get_round_trips(&backend().await).await
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL"]
async fn decision_store_seal_is_first_write_wins() -> Result<()> {
    conformance::decision_store_seal_is_first_write_wins(&backend().await).await
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL"]
async fn decision_store_empty_record_is_present() -> Result<()> {
    conformance::decision_store_empty_record_is_present(&backend().await).await
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL"]
async fn decision_store_remove_makes_record_absent() -> Result<()> {
    conformance::decision_store_remove_makes_record_absent(&backend().await).await
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL"]
async fn decision_store_remove_is_idempotent() -> Result<()> {
    conformance::decision_store_remove_is_idempotent(&backend().await).await
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL"]
async fn decision_store_isolated_by_consumer() -> Result<()> {
    conformance::decision_store_isolated_by_consumer(&backend().await).await
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL"]
async fn decision_store_isolated_by_trigger() -> Result<()> {
    conformance::decision_store_isolated_by_trigger(&backend().await).await
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL"]
async fn decision_store_strips_nul_bytes() -> Result<()> {
    conformance::decision_store_strips_nul_bytes(&backend().await).await
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL"]
async fn decision_store_retention_gc_by_age() -> Result<()> {
    conformance::decision_store_retention_gc_by_age(&backend().await).await
}

// ── Crash scenario ────────────────────────────────────────────────────

/// A sealed decision survives a process restart — the property that lets
/// redelivery replay the record instead of re-running the reactor body.
#[tokio::test]
#[ignore = "requires local DATABASE_URL"]
async fn decision_store_survives_process_restart() -> Result<()> {
    use causal::{DecisionRecord, DecisionStore, EventData};
    use chrono::Utc;
    use uuid::Uuid;

    let trigger = Uuid::new_v4();
    let output = EventData {
        event_id:     Uuid::new_v4(),
        causation_id: Some(trigger),
        workflow_id:  Uuid::new_v4(),
        event_type:   "Out".to_string(),
        payload:      serde_json::json!({"sealed_by": "process-a"}),
        created_at:   Utc::now(),
        category:     Some("Out".to_string()),
        subject_id:   Some(Uuid::new_v4()),
        metadata:     serde_json::Map::new(),
        ephemeral:    None,
        persistent:   true,
    };
    let out_id = output.event_id;

    // "Process A" seals, then dies (store dropped).
    {
        let store_a = backend().await;
        store_a
            .seal(DecisionRecord::new("crash-restart", trigger, vec![output], Utc::now()))
            .await?;
    }

    // "Process B" — new store on the same database.
    let store_b = backend().await;
    let got = store_b.get("crash-restart", trigger).await?;
    let got = got.expect("decision must survive process restart");
    assert_eq!(got.outputs.len(), 1);
    assert_eq!(got.outputs[0].event_id, out_id);

    store_b.remove("crash-restart", trigger).await?;
    Ok(())
}
