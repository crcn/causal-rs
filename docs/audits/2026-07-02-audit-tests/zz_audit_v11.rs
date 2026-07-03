// Scratchpad harness crate (NOT in repo; deleted after run).
// Cargo.toml deps: causal + causal_replay (path, features=["postgres"]),
// tokio, sqlx 0.8 (runtime-tokio,postgres,chrono,json,uuid), uuid, anyhow,
// serde_json, chrono. Ran against dockerized postgres:16 on :55432 with the
// causal_log DDL from migrations/20260101_create_causal_tables.sql.

use anyhow::Result;
use causal::types::{EventData, StreamState};
use causal::{EventLogBackend, MemoryStore};
use causal_replay::PgEventLogBackend;
use chrono::Utc;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

const DDL: &str = r#"
CREATE TABLE IF NOT EXISTS causal_log (
    position        BIGSERIAL PRIMARY KEY,
    event_id        UUID NOT NULL UNIQUE,
    causation_id    UUID,
    correlation_id  UUID NOT NULL,
    event_type      VARCHAR(255) NOT NULL,
    payload         JSONB NOT NULL,
    aggregate_type  VARCHAR(255),
    aggregate_id    UUID,
    revision        BIGINT,
    metadata        JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    persistent      BOOLEAN NOT NULL DEFAULT TRUE
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_causal_log_stream
    ON causal_log (aggregate_type, aggregate_id, revision)
    WHERE aggregate_type IS NOT NULL;
"#;

fn ev(event_type: &str, category: &str, subject_id: Uuid, n: u64) -> EventData {
    EventData {
        event_id: Uuid::new_v4(),
        causation_id: None,
        workflow_id: Uuid::new_v4(),
        event_type: event_type.to_string(),
        payload: serde_json::json!({ "n": n }),
        created_at: Utc::now(),
        category: Some(category.to_string()),
        subject_id: Some(subject_id),
        metadata: serde_json::Map::new(),
        ephemeral: None,
        persistent: true,
    }
}

/// Tail-old partial-overlap shape against any backend:
///   1. seed [eOld] via Any  (eOld persisted at revision 0)
///   2. append [eNew, eOld] via Any  (head NEW, tail OLD)
async fn tail_old_partial_overlap<B: EventLogBackend>(
    b: &B,
    label: &str,
) -> Result<(std::result::Result<(u64, u64), String>, bool, usize)> {
    let cat = "audit";
    let sid = Uuid::new_v4();

    let e_old = ev("audit:old", cat, sid, 0);
    let seeded = b
        .append_to_stream(cat, sid, StreamState::Any, vec![e_old.clone()])
        .await?;
    println!(
        "[{label}] seeded eOld {} at pos={} rev={}",
        e_old.event_id, seeded.position.raw(), seeded.revision.raw()
    );

    let e_new = ev("audit:new", cat, sid, 1);
    println!("[{label}] appending partial-overlap batch [eNew={}, eOld={}] via Any",
        e_new.event_id, e_old.event_id);
    let res = b
        .append_to_stream(cat, sid, StreamState::Any, vec![e_new.clone(), e_old.clone()])
        .await;

    let stream = b.read_stream(cat, sid, None).await?;
    let has_new = stream.iter().any(|r| r.event_id == e_new.event_id);
    let repr = match &res {
        Ok(w) => Ok((w.position.raw(), w.revision.raw())),
        Err(e) => Err(format!("{e:#}")),
    };
    println!(
        "[{label}] append result: {repr:?}\n[{label}] eNew persisted: {has_new}; stream len: {}",
        stream.len()
    );
    Ok((repr, has_new, stream.len()))
}

#[tokio::main]
async fn main() -> Result<()> {
    // Reference backend: MemoryStore.
    let mem = MemoryStore::new();
    let (mem_res, mem_has_new, _) = tail_old_partial_overlap(&mem, "memory").await?;
    assert!(mem_res.is_err() || mem_has_new, "MemoryStore silently dropped eNew too?!");

    // PG backend.
    let url = "postgres://postgres:audit@localhost:55432/audit";
    let pool = PgPoolOptions::new().max_connections(4).connect(url).await?;
    sqlx::raw_sql(DDL).execute(&pool).await?;
    let pg = PgEventLogBackend::new(pool.clone());
    let (pg_res, pg_has_new, _) = tail_old_partial_overlap(&pg, "postgres").await?;

    // CORRECT behavior passes this; the defect fails it.
    assert!(
        pg_res.is_err() || pg_has_new,
        "DEFECT: PG Any-path returned Ok({:?}) for a tail-old partial-overlap \
         batch while silently dropping the new head event (eNew absent from \
         the stream). MemoryStore result for the same shape: {:?}",
        pg_res, mem_res,
    );
    println!("PASS: PG failed loudly (or persisted the whole batch) — no defect.");
    Ok(())
}
