//! Integration tests for `PgReactorOutbox` (Phase 4e.3).
//!
//! Asserts the C12 atomicity contract — `commit_reactor_batch` writes
//! N outbox rows AND advances a cursor in one transaction. Plus the
//! supporting properties: dedup on (reactor_id, source_event_id, idx),
//! oldest-first drain ordering, idempotent delete, cursor-doesn't-go-
//! backwards on the CAS path.
//!
//! Run with:
//!   cargo test -p causal_replay --features postgres --test pg_reactor_outbox_test -- --ignored --nocapture

#![cfg(feature = "postgres")]

use anyhow::Result;
use causal::checkpoint_store::{
    CheckpointStore, InsertableOutboxRow, ReactorOutbox,
};
use causal::types::LogCursor;
use causal_replay::PgReactorOutbox;
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

fn make_row(reactor_id: &str, source: Uuid, idx: u32) -> InsertableOutboxRow {
    InsertableOutboxRow {
        reactor_id: reactor_id.into(),
        source_event_id: source,
        output_index: idx,
        event_id: Uuid::new_v4(),
        event_type: "test:fact".into(),
        fact_payload: serde_json::json!({"idx": idx}),
        correlation_id: Uuid::new_v4(),
    }
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL + migration 054 applied"]
async fn checkpoint_get_and_set_round_trip() -> Result<()> {
    let pool = connect_local().await;
    let store = PgReactorOutbox::new(pool.clone());

    let id = format!("test-{}", Uuid::new_v4());
    assert!(store.get(&id).await?.is_none());

    store.set(&id, LogCursor::from_raw(42)).await?;
    let got = store.get(&id).await?.unwrap();
    assert_eq!(got, LogCursor::from_raw(42));

    sqlx::query("DELETE FROM causal_checkpoints WHERE consumer_id = $1")
        .bind(&id)
        .execute(&pool)
        .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL + migration 054 applied"]
async fn commit_reactor_batch_writes_rows_and_cursor_atomically() -> Result<()> {
    let pool = connect_local().await;
    let store = PgReactorOutbox::new(pool.clone());

    let consumer = format!("reactor-{}", Uuid::new_v4());
    let source_event = Uuid::new_v4();

    let rows = vec![
        make_row(&consumer, source_event, 0),
        make_row(&consumer, source_event, 1),
        make_row(&consumer, source_event, 2),
    ];

    store
        .commit_reactor_batch(rows, Some((consumer.clone(), LogCursor::from_raw(100))))
        .await?;

    // Cursor advanced.
    assert_eq!(
        store.get(&consumer).await?,
        Some(LogCursor::from_raw(100))
    );

    // Rows present.
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM causal_outbox WHERE reactor_id = $1 AND source_event_id = $2",
    )
    .bind(&consumer)
    .bind(source_event)
    .fetch_one(&pool)
    .await?;
    assert_eq!(count, 3);

    sqlx::query("DELETE FROM causal_outbox WHERE reactor_id = $1")
        .bind(&consumer)
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM causal_checkpoints WHERE consumer_id = $1")
        .bind(&consumer)
        .execute(&pool)
        .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL + migration 054 applied"]
async fn commit_reactor_batch_idempotent_on_retry() -> Result<()> {
    // Same batch committed twice → second call is a no-op for rows
    // (UNIQUE on the dedup triple) and re-asserts the cursor (which
    // is already at that position, so cursor write is also a no-op).
    let pool = connect_local().await;
    let store = PgReactorOutbox::new(pool.clone());

    let consumer = format!("retry-{}", Uuid::new_v4());
    let source_event = Uuid::new_v4();
    let rows = vec![
        make_row(&consumer, source_event, 0),
        make_row(&consumer, source_event, 1),
    ];

    store
        .commit_reactor_batch(rows.clone(), Some((consumer.clone(), LogCursor::from_raw(50))))
        .await?;

    // Replay the same batch — should not error, should not duplicate.
    store
        .commit_reactor_batch(rows, Some((consumer.clone(), LogCursor::from_raw(50))))
        .await?;

    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM causal_outbox WHERE reactor_id = $1 AND source_event_id = $2",
    )
    .bind(&consumer)
    .bind(source_event)
    .fetch_one(&pool)
    .await?;
    assert_eq!(count, 2, "duplicate commit must NOT create duplicate rows");

    sqlx::query("DELETE FROM causal_outbox WHERE reactor_id = $1")
        .bind(&consumer)
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM causal_checkpoints WHERE consumer_id = $1")
        .bind(&consumer)
        .execute(&pool)
        .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL + migration 054 applied"]
async fn cursor_does_not_go_backwards_via_commit_reactor_batch() -> Result<()> {
    // C12 safety: a stale concurrent writer attempting to advance the
    // cursor to a SMALLER position must be a no-op. Otherwise we could
    // "uncomplete" delivered work — direct violation of at-least-once.
    let pool = connect_local().await;
    let store = PgReactorOutbox::new(pool.clone());

    let consumer = format!("cas-{}", Uuid::new_v4());

    // Initial advance to 100.
    store
        .commit_reactor_batch(Vec::new(), Some((consumer.clone(), LogCursor::from_raw(100))))
        .await?;
    assert_eq!(store.get(&consumer).await?, Some(LogCursor::from_raw(100)));

    // Stale writer at 50 — must be ignored.
    store
        .commit_reactor_batch(Vec::new(), Some((consumer.clone(), LogCursor::from_raw(50))))
        .await?;
    assert_eq!(
        store.get(&consumer).await?,
        Some(LogCursor::from_raw(100)),
        "cursor must not go backwards under CAS guard"
    );

    // Forward write to 200 — should succeed.
    store
        .commit_reactor_batch(Vec::new(), Some((consumer.clone(), LogCursor::from_raw(200))))
        .await?;
    assert_eq!(store.get(&consumer).await?, Some(LogCursor::from_raw(200)));

    sqlx::query("DELETE FROM causal_checkpoints WHERE consumer_id = $1")
        .bind(&consumer)
        .execute(&pool)
        .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL + migration 054 applied"]
async fn outbox_pending_returns_oldest_first() -> Result<()> {
    let pool = connect_local().await;
    let store = PgReactorOutbox::new(pool.clone());

    let consumer = format!("drain-{}", Uuid::new_v4());
    let source = Uuid::new_v4();

    // Insert 5 rows with same created_at (microsecond ties on real clock);
    // the secondary sort by id should disambiguate.
    let rows = (0..5)
        .map(|i| make_row(&consumer, source, i))
        .collect::<Vec<_>>();
    store.commit_reactor_batch(rows, None).await?;

    let pending = store.outbox_pending(100).await?;
    let ours: Vec<_> = pending
        .iter()
        .filter(|r| r.reactor_id == consumer)
        .collect();
    assert_eq!(ours.len(), 5);

    // Output indices preserved in insert order (which matches
    // (created_at, id) order under the sort).
    for (i, row) in ours.iter().enumerate() {
        assert_eq!(row.output_index, i as u32);
    }

    sqlx::query("DELETE FROM causal_outbox WHERE reactor_id = $1")
        .bind(&consumer)
        .execute(&pool)
        .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL + migration 054 applied"]
async fn outbox_delete_is_idempotent_on_missing_id() -> Result<()> {
    // Per the trait contract: delete on already-deleted id MUST succeed.
    // Relay loops may retry after a partial crash.
    let pool = connect_local().await;
    let store = PgReactorOutbox::new(pool.clone());

    let consumer = format!("delete-{}", Uuid::new_v4());
    let source = Uuid::new_v4();
    store
        .commit_reactor_batch(vec![make_row(&consumer, source, 0)], None)
        .await?;

    let id: i64 = sqlx::query_scalar(
        "SELECT id FROM causal_outbox WHERE reactor_id = $1",
    )
    .bind(&consumer)
    .fetch_one(&pool)
    .await?;

    store.outbox_delete(id).await?;
    // Second delete on the same id — must succeed.
    store.outbox_delete(id).await?;
    // Delete a non-existent id — must succeed.
    store.outbox_delete(9_999_999_999).await?;

    Ok(())
}
