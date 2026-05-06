//! Integration tests for `PgSnapshotStore` (Phase 4e.4).
//!
//! Run with:
//!   cargo test -p causal_replay --features postgres --test pg_snapshot_store_test -- --ignored --nocapture

#![cfg(feature = "postgres")]

use anyhow::Result;
use causal::snapshot_store::SnapshotStore;
use causal::types::{Snapshot, StreamVersion};
use causal_replay::PgSnapshotStore;
use chrono::Utc;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use uuid::Uuid;

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
        "Refusing to run against non-local Postgres: {url}"
    );
    PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .expect("connect")
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL + migrations applied"]
async fn save_and_load_round_trips_state_and_version() -> Result<()> {
    let pool = connect_local().await;
    let store = PgSnapshotStore::new(pool.clone());

    let aggregate_id = Uuid::new_v4();
    let snapshot = Snapshot {
        aggregate_type: "order".to_string(),
        aggregate_id,
        version: StreamVersion::from_raw(42),
        state: serde_json::json!({"total": 100, "items": ["apple", "banana"]}),
        created_at: Utc::now(),
    };

    store.save_snapshot(snapshot.clone()).await?;
    let loaded = store.load_snapshot("order", aggregate_id).await?.unwrap();

    assert_eq!(loaded.aggregate_type, "order");
    assert_eq!(loaded.aggregate_id, aggregate_id);
    assert_eq!(loaded.version, StreamVersion::from_raw(42));
    assert_eq!(loaded.state, snapshot.state);

    store.delete_snapshot("order", aggregate_id).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL + migrations applied"]
async fn load_returns_latest_version_when_multiple_exist() -> Result<()> {
    let pool = connect_local().await;
    let store = PgSnapshotStore::new(pool.clone());

    let aggregate_id = Uuid::new_v4();
    for v in [1, 5, 10, 7] {
        store
            .save_snapshot(Snapshot {
                aggregate_type: "order".to_string(),
                aggregate_id,
                version: StreamVersion::from_raw(v),
                state: serde_json::json!({"v": v}),
                created_at: Utc::now(),
            })
            .await?;
    }

    let loaded = store.load_snapshot("order", aggregate_id).await?.unwrap();
    assert_eq!(
        loaded.version,
        StreamVersion::from_raw(10),
        "load_snapshot must return the highest-version snapshot"
    );
    assert_eq!(loaded.state, serde_json::json!({"v": 10}));

    store.delete_snapshot("order", aggregate_id).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL + migrations applied"]
async fn load_returns_none_for_unknown_aggregate() -> Result<()> {
    let pool = connect_local().await;
    let store = PgSnapshotStore::new(pool.clone());
    let loaded = store.load_snapshot("order", Uuid::new_v4()).await?;
    assert!(loaded.is_none());
    Ok(())
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL + migrations applied"]
async fn save_is_idempotent_on_duplicate_key_version() -> Result<()> {
    // Trait + schema contract: same (aggregate, version) saved twice is
    // a no-op. State for a given (aggregate, version) is deterministic
    // (fold of events up to that version), so duplicate writes are
    // guaranteed-equal.
    let pool = connect_local().await;
    let store = PgSnapshotStore::new(pool.clone());
    let aggregate_id = Uuid::new_v4();
    let snapshot = Snapshot {
        aggregate_type: "order".to_string(),
        aggregate_id,
        version: StreamVersion::from_raw(1),
        state: serde_json::json!({"original": true}),
        created_at: Utc::now(),
    };

    store.save_snapshot(snapshot.clone()).await?;

    let mut second = snapshot.clone();
    second.state = serde_json::json!({"this_should_not": "overwrite"});
    store.save_snapshot(second).await?;

    let loaded = store.load_snapshot("order", aggregate_id).await?.unwrap();
    assert_eq!(
        loaded.state,
        serde_json::json!({"original": true}),
        "duplicate save must NOT overwrite — first write wins"
    );

    store.delete_snapshot("order", aggregate_id).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL + migrations applied"]
async fn delete_removes_all_versions_for_aggregate() -> Result<()> {
    let pool = connect_local().await;
    let store = PgSnapshotStore::new(pool.clone());
    let aggregate_id = Uuid::new_v4();

    for v in [1, 2, 3] {
        store
            .save_snapshot(Snapshot {
                aggregate_type: "order".to_string(),
                aggregate_id,
                version: StreamVersion::from_raw(v),
                state: serde_json::json!({"v": v}),
                created_at: Utc::now(),
            })
            .await?;
    }

    store.delete_snapshot("order", aggregate_id).await?;

    assert!(
        store.load_snapshot("order", aggregate_id).await?.is_none(),
        "delete must remove all versions for the aggregate"
    );

    // Idempotent — second delete on the now-empty key must succeed.
    store.delete_snapshot("order", aggregate_id).await?;
    Ok(())
}
