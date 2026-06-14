//! EffectStore conformance suite run against `PgEffectStore`.
//!
//! All tests are `#[ignore]`'d by default; run against a local Postgres
//! with the causal_effect_store migration applied:
//!
//!     DATABASE_URL=postgres://localhost/causal cargo test -p causal_replay \
//!         --features postgres --test pg_effect_store_conformance_test -- --ignored
#![cfg(feature = "postgres")]

use anyhow::Result;
use causal_replay::conformance;
use causal_replay::effect_store::PgEffectStore;
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

async fn backend() -> PgEffectStore {
    PgEffectStore::new(connect_local().await)
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL + causal_effect_store table"]
async fn effect_store_get_miss() -> Result<()> {
    conformance::effect_store_get_miss(&backend().await).await
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL + causal_effect_store table"]
async fn effect_store_put_and_get_round_trips() -> Result<()> {
    conformance::effect_store_put_and_get_round_trips(&backend().await).await
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL + causal_effect_store table"]
async fn effect_store_first_write_wins() -> Result<()> {
    conformance::effect_store_first_write_wins(&backend().await).await
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL + causal_effect_store table"]
async fn effect_store_remove_makes_entry_absent() -> Result<()> {
    conformance::effect_store_remove_makes_entry_absent(&backend().await).await
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL + causal_effect_store table"]
async fn effect_store_remove_is_idempotent() -> Result<()> {
    conformance::effect_store_remove_is_idempotent(&backend().await).await
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL + causal_effect_store table"]
async fn effect_store_keys_isolated_by_consumer() -> Result<()> {
    conformance::effect_store_keys_isolated_by_consumer(&backend().await).await
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL + causal_effect_store table"]
async fn effect_store_keys_isolated_by_trigger() -> Result<()> {
    conformance::effect_store_keys_isolated_by_trigger(&backend().await).await
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL + causal_effect_store table"]
async fn effect_store_keys_isolated_by_label() -> Result<()> {
    conformance::effect_store_keys_isolated_by_label(&backend().await).await
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL + causal_effect_store table"]
async fn effect_store_remember_calls_compute_once() -> Result<()> {
    conformance::effect_store_remember_calls_compute_once(&backend().await).await
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL + causal_effect_store table"]
async fn effect_store_remember_replays_cached_on_redelivery() -> Result<()> {
    conformance::effect_store_remember_replays_cached_on_redelivery(&backend().await).await
}
