//! EventIdRegistry conformance run against `PgEventIdRegistry`.
//!
//!     DATABASE_URL=postgres://localhost/causal cargo test -p causal_replay \
//!         --features postgres --test pg_event_id_registry_conformance_test -- --ignored
#![cfg(feature = "postgres")]

use anyhow::Result;
use causal_replay::conformance;
use causal_replay::event_id_registry::PgEventIdRegistry;
use sqlx::postgres::PgPoolOptions;

async fn connect_local() -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL required");
    assert!(
        url.contains("localhost") || url.contains("127.0.0.1"),
        "Refusing to run integration tests against non-local Postgres: {url}"
    );
    PgPoolOptions::new().max_connections(4).connect(&url).await.expect("connect")
}

async fn backend() -> PgEventIdRegistry {
    let pool = connect_local().await;
    PgEventIdRegistry::ensure_schema(&pool).await.expect("ensure registry schema");
    PgEventIdRegistry::new(pool)
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL"]
async fn event_id_registry_absent_batch() -> Result<()> {
    conformance::event_id_registry_absent_batch(&backend().await).await
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL"]
async fn event_id_registry_redelivery_after_register() -> Result<()> {
    conformance::event_id_registry_redelivery_after_register(&backend().await).await
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL"]
async fn event_id_registry_partial_overlap() -> Result<()> {
    conformance::event_id_registry_partial_overlap(&backend().await).await
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL"]
async fn event_id_registry_register_first_write_wins() -> Result<()> {
    conformance::event_id_registry_register_first_write_wins(&backend().await).await
}
