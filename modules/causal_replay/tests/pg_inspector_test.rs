//! Round-trip test: drive a reactor whose observability flows through
//! `PgReactorObserver`, then read it back through `PgInspectorReadModel`.
//! Validates the SQL that compile-checking can't.
//!
//! Requires a local Postgres with the schema applied (./dev.sh test pg, or
//! apply migrations/) and DATABASE_URL set.
#![cfg(feature = "postgres")]

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use uuid::Uuid;

use causal::aggregate::{Aggregate, Apply};
use causal::aggregator::Aggregator;
use causal::types::LogLevel;
use causal::{
    CheckpointStore, Ctx, EngineBuilder, Event, EventLogBackend, Events, Reactor, ReactorCheckpoint,
};
use causal_inspector::read_model::InspectorReadModel;
use causal_replay::{PgEventLogBackend, PgInspectorReadModel, PgReactorCheckpoint, PgReactorObserver};

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

async fn connect() -> PgPool {
    load_env();
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL required");
    assert!(url.contains("localhost") || url.contains("127.0.0.1"), "local only");
    PgPoolOptions::new().max_connections(4).connect(&url).await.unwrap()
}

// ── fixtures ──────────────────────────────────────────────────────────
#[derive(Clone, Serialize, Deserialize)]
struct Pinged { id: Uuid, occurred_at: DateTime<Utc> }
impl Event for Pinged {
    const CATEGORY: &'static str = "insp_ping";
    fn event_type(&self) -> &str { "v1" }
    fn subject_id(&self) -> Uuid { self.id }
    fn occurred_at(&self) -> Option<DateTime<Utc>> { Some(self.occurred_at) }
}

#[derive(Clone, Serialize, Deserialize)]
struct Ponged { id: Uuid, occurred_at: DateTime<Utc> }
impl Event for Ponged {
    const CATEGORY: &'static str = "insp_pong";
    fn event_type(&self) -> &str { "v1" }
    fn subject_id(&self) -> Uuid { self.id }
    fn occurred_at(&self) -> Option<DateTime<Utc>> { Some(self.occurred_at) }
}

#[derive(Default, Clone, Serialize, Deserialize)]
struct Pings { count: u32 }
impl Aggregate for Pings { const NAME: &'static str = "InspPings"; }
impl Apply<Pinged> for Pings { fn apply(&mut self, _: &Pinged) { self.count += 1; } }

// Fails its first attempt, succeeds on retry — exercises the terminal-status /
// terminal-error / attempt-history paths in the read model.
#[derive(Default)]
struct PongReactor {
    failed_once: std::sync::Mutex<bool>,
}
#[async_trait]
impl Reactor for PongReactor {
    type Trigger = Pinged;
    const NAME: &'static str = "insp_pong_reactor";
    async fn react(&self, t: &Pinged, ctx: Ctx<'_>) -> Result<Events> {
        ctx.log(LogLevel::Info, "pong reacting");
        let first = {
            let mut failed = self.failed_once.lock().unwrap();
            let was = *failed;
            *failed = true;
            !was
        };
        if first {
            anyhow::bail!("simulated pong failure (recovers on retry)");
        }
        Ok(Events::new().add(Ponged { id: t.id, occurred_at: Utc::now() }))
    }
    fn describe(&self, _t: &Pinged) -> Option<serde_json::Value> {
        Some(serde_json::json!([{ "type": "text", "text": "emit pong" }]))
    }
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL + migrations applied"]
async fn pg_inspector_round_trip() -> Result<()> {
    let pool = connect().await;
    let store = Arc::new(PgEventLogBackend::new(pool.clone()));
    let checkpoint = Arc::new(PgReactorCheckpoint::new(pool.clone()));
    let observer = Arc::new(PgReactorObserver::new(pool.clone()));

    let engine = EngineBuilder::new(
        store.clone() as Arc<dyn EventLogBackend>,
        checkpoint.clone() as Arc<dyn CheckpointStore>,
        checkpoint as Arc<dyn ReactorCheckpoint>,
    )
    .with_observer(observer)
    .with_aggregators(vec![
        Aggregator::for_type::<Pings, Pinged>(),
    ])
    .with_reactor(PongReactor::default())
    .build().await.unwrap();

    let id = Uuid::new_v4();
    let result = engine
        .emit(Pinged { id, occurred_at: Utc::now() })
        .settled()
        .await?;
    let corr = result.workflow_id.to_string();

    // Read side. Poll for the async observer flush.
    let read = PgInspectorReadModel::new(pool.clone());
    let mut outcomes = vec![];
    for _ in 0..40 {
        outcomes = read.reactor_outcomes(&corr).await?;
        if !outcomes.is_empty() { break; }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Events: Pinged + Ponged in this workflow.
    let flow = read.causal_flow(&corr).await?;
    assert!(flow.len() >= 2, "expected Pinged + Ponged, got {}", flow.len());
    assert!(flow.iter().any(|e| e.event_type == "insp_ping:v1"));
    assert!(flow.iter().any(|e| e.event_type == "insp_pong:v1"));

    // Reactor observability. Terminal status: a reactor that failed then
    // recovered must read as `completed` (not `failed`), with no terminal error.
    let pong = outcomes
        .iter()
        .find(|o| o.reactor_id == "insp_pong_reactor")
        .expect("expected pong reactor outcome");
    assert_eq!(pong.status, "completed", "recovered reactor must be completed, got {pong:?}");
    assert!(pong.error.is_none(), "completed reactor must have no terminal error, got {:?}", pong.error);
    assert!(pong.attempts >= 2, "expected a retry (>=2 attempts), got {}", pong.attempts);

    let logs = read.reactor_logs_by_workflow(&corr).await?;
    assert!(logs.iter().any(|l| l.message == "pong reacting"), "expected reactor log line");

    // Attempt history: closed attempts only, showing both the failed attempt
    // and the recovering completed attempt.
    let attempts = read.reactor_attempt_history(&corr).await?;
    let pong_attempts: Vec<_> =
        attempts.iter().filter(|a| a.reactor_id == "insp_pong_reactor").collect();
    assert!(!pong_attempts.is_empty(), "expected attempt rows");
    assert!(
        pong_attempts.iter().any(|a| a.status == "failed"),
        "expected a failed attempt row, got {pong_attempts:?}"
    );
    assert!(
        pong_attempts.iter().any(|a| a.status == "completed"),
        "expected a completed attempt row, got {pong_attempts:?}"
    );

    // Aggregate snapshot timeline (Pings folded on Pinged).
    let timeline = read.aggregate_state_timeline(&corr).await?;
    assert!(
        timeline.iter().any(|s| s.aggregate_key.starts_with("InspPings")),
        "expected Pings aggregate snapshot, got {timeline:?}"
    );

    // Reactor description captured.
    let descs = read.reactor_descriptions(&corr).await?;
    assert!(descs.iter().any(|d| d.reactor_id == "insp_pong_reactor"), "expected description");

    Ok(())
}
