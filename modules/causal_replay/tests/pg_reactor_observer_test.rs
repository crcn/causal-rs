//! Fail-soft + schema-ownership tests for `PgReactorObserver`.
//!
//! - A missing inspector table (e.g. `causal_reactor_divergences`, whose
//!   migration shipped after 0.16) must NOT poison co-batched execution/log
//!   rows: the writer falls back to per-record writes, drops only the record
//!   for the missing table, and warns once.
//! - `ensure_schema` provisions every table the observer writes and is
//!   idempotent.
//!
//! Each test runs inside a throwaway Postgres schema (via `search_path`) so it
//! can DROP tables without touching the shared dev schema, and cleans up after
//! itself. Requires a local Postgres + DATABASE_URL (./dev.sh test pg).
#![cfg(feature = "postgres")]

use std::str::FromStr;
use std::time::Duration;

use chrono::Utc;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::PgPool;
use uuid::Uuid;

use causal::reactor_observer::ReactorObserver;
use causal::types::{LogEntry, LogLevel};
use causal_replay::PgReactorObserver;

fn load_env() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join(".env"));
    if let Some(p) = path {
        if let Ok(content) = std::fs::read_to_string(p) {
            for line in content.lines() {
                if let Some((k, v)) = line.trim().split_once('=') {
                    if std::env::var(k.trim()).is_err() {
                        std::env::set_var(k.trim(), v.trim());
                    }
                }
            }
        }
    }
}

fn database_url() -> String {
    load_env();
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL required");
    assert!(
        url.contains("localhost") || url.contains("127.0.0.1"),
        "local only"
    );
    url
}

/// Create a fresh throwaway schema and return a pool whose `search_path`
/// points at it exclusively, so all unqualified DDL/DML is isolated there.
async fn scratch_schema() -> (PgPool, String) {
    let url = database_url();
    // NB: Postgres reserves the `pg_` prefix for system schemas.
    let schema = format!("obs_test_{}", Uuid::new_v4().simple());

    // Bootstrap on the default schema to create ours.
    let bootstrap = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .unwrap();
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&bootstrap)
        .await
        .unwrap();
    bootstrap.close().await;

    let opts = PgConnectOptions::from_str(&url)
        .unwrap()
        .options([("search_path", schema.as_str())]);
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect_with(opts)
        .await
        .unwrap();
    (pool, schema)
}

async fn drop_schema(schema: &str) {
    // Fresh connection on the default schema — the scratch pool is dropped by
    // the time we tear down.
    if let Ok(pool) = PgPoolOptions::new()
        .max_connections(1)
        .connect(&database_url())
        .await
    {
        let _ = sqlx::query(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE"))
            .execute(&pool)
            .await;
        pool.close().await;
    }
}

/// Poll `count_query` until it reaches `target` or the deadline elapses.
async fn wait_for_count(pool: &PgPool, count_query: &str, target: i64) -> i64 {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let (n,): (i64,) = sqlx::query_as(count_query).fetch_one(pool).await.unwrap();
        if n >= target || std::time::Instant::now() >= deadline {
            return n;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn one_log() -> Vec<LogEntry> {
    vec![LogEntry {
        level: LogLevel::Info,
        message: "hello".into(),
        data: None,
        timestamp: Utc::now(),
    }]
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL + migrations applied"]
async fn divergence_table_missing_does_not_poison_batch() {
    let (pool, schema) = scratch_schema().await;
    PgReactorObserver::ensure_schema(&pool).await.unwrap();
    // Simulate a consumer whose DB predates the divergences migration.
    sqlx::query("DROP TABLE causal_reactor_divergences")
        .execute(&pool)
        .await
        .unwrap();

    let obs = PgReactorObserver::new(pool.clone());
    let wf = Uuid::new_v4();
    let now = Utc::now();

    // A batch mixing execution/log records with a divergence record for the
    // missing table. The divergence must not take the executions down with it.
    let e1 = Uuid::new_v4();
    obs.reactor_started(e1, "r.a", wf, 1, now);
    obs.reactor_completed(e1, "r.a", wf, 1, now, now, &one_log());
    obs.reactor_divergence(Uuid::new_v4(), "r.a", wf, "payload mismatch");
    let e2 = Uuid::new_v4();
    obs.reactor_started(e2, "r.b", wf, 1, now);
    obs.reactor_completed(e2, "r.b", wf, 1, now, now, &one_log());

    // Executions + logs still land despite the missing divergences table.
    let execs = wait_for_count(&pool, "SELECT count(*) FROM causal_reactor_executions", 2).await;
    assert_eq!(execs, 2, "both executions landed past the poison record");
    let logs = wait_for_count(&pool, "SELECT count(*) FROM causal_reactor_logs", 2).await;
    assert_eq!(logs, 2, "log rows landed too");

    // The writer loop survived and keeps draining a subsequent round.
    let e3 = Uuid::new_v4();
    obs.reactor_started(e3, "r.c", wf, 1, now);
    let after = wait_for_count(&pool, "SELECT count(*) FROM causal_reactor_executions", 3).await;
    assert_eq!(after, 3, "writer loop still draining after the failure");

    drop(obs);
    pool.close().await;
    drop_schema(&schema).await;
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL + migrations applied"]
async fn ensure_schema_idempotent_and_sufficient() {
    let (pool, schema) = scratch_schema().await;
    // Idempotent: running twice must not error.
    PgReactorObserver::ensure_schema(&pool).await.unwrap();
    PgReactorObserver::ensure_schema(&pool).await.unwrap();

    let obs = PgReactorObserver::new(pool.clone());
    let wf = Uuid::new_v4();
    let now = Utc::now();
    let e = Uuid::new_v4();

    // One record of every class → all five tables must accept it.
    obs.reactor_started(e, "r", wf, 1, now);
    obs.reactor_completed(e, "r", wf, 1, now, now, &one_log());
    obs.reactor_divergence(e, "r", wf, "diff");
    obs.aggregate_folded(
        wf,
        causal::types::LogCursor::ZERO,
        e,
        "agg",
        serde_json::json!({"n": 1}),
    );
    obs.reactor_description(
        wf,
        causal::types::LogCursor::ZERO,
        e,
        "r",
        serde_json::json!({"intent": "x"}),
    );

    for (table, target) in [
        ("causal_reactor_executions", 1),
        ("causal_reactor_logs", 1),
        ("causal_reactor_divergences", 1),
        ("causal_aggregate_snapshots", 1),
        ("causal_reactor_descriptions", 1),
    ] {
        let n = wait_for_count(&pool, &format!("SELECT count(*) FROM {table}"), target).await;
        assert_eq!(n, target, "table `{table}` did not receive its record");
    }

    drop(obs);
    pool.close().await;
    drop_schema(&schema).await;
}
