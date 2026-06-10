//! End-to-end smoke test for the Kurrent-log + Postgres-checkpoint
//! hybrid deployment.
//!
//! This is the configuration `rootsignal` plans to ship: KurrentDB
//! as the durable event log (per-aggregate streams, `$all`,
//! server-side projections) and Postgres as the reactor/projection
//! cursor + DLQ-attempt store (`PgReactorCheckpoint`). The roadmap
//! calls this "Option B (hybrid)" — see
//! `docs/plans/2026-03-06-kurrentdb-integration-roadmap.md`.
//!
//! The compile-time smoke check below verifies that the type wiring
//! works without a live backend. The `#[ignore]`'d round-trip test
//! requires both Kurrent and Postgres running locally — run with:
//!
//!   cargo test -p causal_replay --features postgres,kurrent \
//!     --test kurrent_pg_hybrid_test -- --ignored --nocapture

#![cfg(all(feature = "postgres", feature = "kurrent"))]

use std::sync::Arc;

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use uuid::Uuid;

use causal::{
    CheckpointStore, EngineBuilder, EventLogBackend, Event, ReactorCheckpoint,
};
use causal_replay::{KurrentEventLogBackend, PgReactorCheckpoint};

// ──────────────────────────────────────────────────────────────────
// Compile-time wiring check — no backend connection required
// ──────────────────────────────────────────────────────────────────

/// Trait-objects compose into the engine builder. If `EventLogBackend`
/// gains a non-object-safe method, this test fails to compile and
/// catches the regression before rootsignal does.
///
/// Doesn't run an engine — just type-checks the construction. The
/// `let _ = ...` discards the values so they're never dropped at
/// runtime via this path.
#[test]
#[allow(unused_variables)]
fn hybrid_construction_types_compose() {
    // We never actually connect; these closures only need to exist
    // for the type-checker to verify the trait objects line up.
    async fn _build_with(
        kurrent: KurrentEventLogBackend,
        pg: PgReactorCheckpoint,
    ) -> impl Sized {
        let pg = Arc::new(pg);
        EngineBuilder::new(
            Arc::new(kurrent) as Arc<dyn EventLogBackend>,
            pg.clone() as Arc<dyn CheckpointStore>,
            pg as Arc<dyn ReactorCheckpoint>,
        )
        .build()
        .await
        .unwrap()
    }
}

// ──────────────────────────────────────────────────────────────────
// Live round-trip test — requires Docker'd Kurrent + Postgres
// ──────────────────────────────────────────────────────────────────

#[derive(Clone, Serialize, Deserialize)]
struct OrderPlaced {
    order_id: Uuid,
    occurred_at: DateTime<Utc>,
}

impl Event for OrderPlaced {
    const CATEGORY: &'static str = "order";
    fn event_type(&self) -> &str {
        "placed"
    }
    fn stream_id(&self) -> Uuid {
        self.order_id
    }
    fn occurred_at(&self) -> Option<DateTime<Utc>> {
        Some(self.occurred_at)
    }
}

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

async fn connect_pg() -> PgPool {
    load_env();
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL required");
    assert!(
        url.contains("localhost") || url.contains("127.0.0.1"),
        "refusing non-local PG: {url}"
    );
    PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .expect("PG connect")
}

fn kurrent_url() -> String {
    std::env::var("KURRENT_URL")
        .unwrap_or_else(|_| "kurrentdb://localhost:2113?tls=false".to_string())
}

#[tokio::test]
#[ignore = "requires running Kurrent on KURRENT_URL + local DATABASE_URL"]
async fn engine_appends_to_kurrent_with_pg_checkpoint() -> Result<()> {
    let kurrent = KurrentEventLogBackend::connect(&kurrent_url())?;
    let pg_pool = connect_pg().await;
    let pg = Arc::new(PgReactorCheckpoint::new(pg_pool.clone()));
    let kurrent_arc: Arc<dyn EventLogBackend> = Arc::new(kurrent);

    let engine = EngineBuilder::new(
        kurrent_arc.clone(),
        pg.clone() as Arc<dyn CheckpointStore>,
        pg.clone() as Arc<dyn ReactorCheckpoint>,
    )
    .build().await.unwrap();

    let order_id = Uuid::new_v4();
    let fact = OrderPlaced {
        order_id,
        occurred_at: Utc::now(),
    };
    engine.emit(fact).await?;

    // Verify the fact landed in Kurrent.
    let stream = kurrent_arc
        .read_stream("order", order_id, None)
        .await?;
    assert_eq!(stream.len(), 1, "exactly the one fact we emitted");
    assert_eq!(stream[0].event_type, "order:placed");
    assert_eq!(stream[0].category, "order");
    assert_eq!(stream[0].stream_id, order_id);

    engine.shutdown().await?;
    Ok(())
}
