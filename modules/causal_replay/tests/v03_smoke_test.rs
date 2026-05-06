//! Phase 4e.7 — End-to-end smoke test: v0.3 Engine against Postgres.
//!
//! The closing gate for Phase 4e. Constructs a real `engine_v3::Engine`
//! with all three Postgres-backed backends, registers an
//! `AnyMaterializer`, appends events directly via the EventLogBackend,
//! and verifies the materializer's supervisor processes them.
//!
//! Doesn't go through `Engine::emit` (which requires a typed `Fact`
//! impl) — the smoke test asserts the *integration* between backends
//! and the engine runtime, not the `Fact` macro. That's the right scope
//! for "is the v0.3 engine production-deployable against Postgres?"
//!
//! Run with:
//!   cargo test -p causal_replay --features postgres --test v03_smoke_test -- --ignored --nocapture

#![cfg(feature = "postgres")]

use anyhow::Result;
use async_trait::async_trait;
use causal::any_materializer::AnyMaterializer;
use causal::checkpoint_store::{CheckpointStore, ReactorOutbox};
use causal::contexts::Ctx;
use causal::event_log::EventLogBackend;
use causal::engine_v3::EngineBuilder;
use causal::types::{NewEvent, PersistedEvent};
use causal_replay::{PgEventLogBackend, PgReactorOutbox, PgSnapshotStore};
use chrono::Utc;
use parking_lot::Mutex;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use std::sync::Arc;
use std::time::Duration;
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
        "Refusing to run smoke test against non-local Postgres: {url}"
    );
    PgPoolOptions::new()
        .max_connections(8)
        .connect(&url)
        .await
        .expect("connect")
}

/// Records every (event_id, event_type) it sees so the smoke test can
/// assert the engine's supervisor delivered our events.
struct RecordingMaterializer {
    seen: Arc<Mutex<Vec<(Uuid, String)>>>,
    /// Filter by correlation_id so the test isolates from any other
    /// events in the log.
    filter_correlation: Uuid,
}

#[async_trait]
impl AnyMaterializer for RecordingMaterializer {
    async fn materialize(&self, event: &PersistedEvent, _ctx: Ctx<'_>) -> Result<()> {
        if event.correlation_id == self.filter_correlation {
            self.seen.lock().push((event.event_id, event.event_type.clone()));
        }
        Ok(())
    }
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL + migrations 054 + 055 applied"]
async fn v03_engine_drives_materializer_end_to_end_against_postgres() -> Result<()> {
    let pool = connect_local().await;

    // Backends. PgReactorOutbox impls both CheckpointStore and
    // ReactorOutbox so the same Arc fills both slots in EngineBuilder.
    let log: Arc<dyn EventLogBackend> =
        Arc::new(PgEventLogBackend::new(pool.clone()));
    let outbox_arc = Arc::new(PgReactorOutbox::new(pool.clone()));
    let checkpoint: Arc<dyn CheckpointStore> = outbox_arc.clone();
    let outbox: Arc<dyn ReactorOutbox> = outbox_arc;
    // PgSnapshotStore exists for completeness even though EngineBuilder
    // doesn't currently take it as a parameter (per the v0.3 API trim,
    // snapshots are deferred). Construct it so the dep is exercised.
    let _snapshot = Arc::new(PgSnapshotStore::new(pool.clone()));

    let correlation = Uuid::new_v4();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let materializer = RecordingMaterializer {
        seen: seen.clone(),
        filter_correlation: correlation,
    };

    // Build the engine with the materializer registered. Use a unique
    // consumer_id so this test's checkpoint doesn't collide with
    // anything else in the DB.
    let consumer_id = format!("smoke-{}", Uuid::new_v4());
    let engine = EngineBuilder::new(log.clone(), checkpoint, outbox)
        .with_any_materializer(materializer, &consumer_id)
        .build();

    // Append 3 events directly via the EventLogBackend. The engine's
    // supervisor task tails the same backend (shared Arc), sees the
    // new events via load_from, and delivers them to the materializer.
    let mut event_ids = Vec::new();
    for i in 0..3 {
        let event = NewEvent {
            event_id: Uuid::new_v4(),
            parent_id: None,
            correlation_id: correlation,
            event_type: format!("smoke:fact:{}", i),
            payload: serde_json::json!({"index": i}),
            created_at: Utc::now(),
            aggregate_type: None,
            aggregate_id: None,
            metadata: serde_json::Map::new(),
            ephemeral: None,
            persistent: true,
        };
        event_ids.push(event.event_id);
        log.append(event).await?;
    }

    // Wait for the materializer to catch up. Bounded — the engine
    // supervisor polls load_from on a short interval, plus LISTEN/NOTIFY
    // wakes it sooner. 5s is generous.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if seen.lock().len() == 3 {
            break;
        }
        if std::time::Instant::now() > deadline {
            let observed = seen.lock().clone();
            panic!(
                "materializer did not see all 3 events within 5s; observed {observed:?}"
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let observed = seen.lock().clone();
    assert_eq!(observed.len(), 3);
    let observed_ids: Vec<Uuid> = observed.iter().map(|(id, _)| *id).collect();
    for id in &event_ids {
        assert!(
            observed_ids.contains(id),
            "materializer must have seen event {id}"
        );
    }
    let observed_types: Vec<&str> =
        observed.iter().map(|(_, t)| t.as_str()).collect();
    assert!(observed_types.contains(&"smoke:fact:0"));
    assert!(observed_types.contains(&"smoke:fact:1"));
    assert!(observed_types.contains(&"smoke:fact:2"));

    engine.shutdown().await?;

    // Cleanup: remove our test events + checkpoint.
    sqlx::query("DELETE FROM causal_log WHERE correlation_id = $1")
        .bind(correlation)
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM causal_checkpoints WHERE consumer_id = $1")
        .bind(&consumer_id)
        .execute(&pool)
        .await?;

    Ok(())
}
