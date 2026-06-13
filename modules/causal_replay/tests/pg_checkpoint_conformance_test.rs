//! Checkpoint / reactor-cursor conformance against `PgReactorCheckpoint`
//! (release gate). `#[ignore]`'d by default; run against a local
//! Postgres with migration 054 applied:
//!
//!     DATABASE_URL=postgres://localhost/causal cargo test -p causal_replay \
//!         --features postgres --test pg_checkpoint_conformance_test -- --ignored
#![cfg(feature = "postgres")]

use anyhow::Result;
use causal_replay::conformance;
use causal_replay::reactor_checkpoint::PgReactorCheckpoint;
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

async fn backend() -> PgReactorCheckpoint {
    PgReactorCheckpoint::new(connect_local().await)
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL + migration 054"]
async fn checkpoint_unknown_consumer_is_none() -> Result<()> {
    conformance::checkpoint_unknown_consumer_is_none(&backend().await).await
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL + migration 054"]
async fn checkpoint_set_then_get_round_trips() -> Result<()> {
    conformance::checkpoint_set_then_get_round_trips(&backend().await).await
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL + migration 054"]
async fn checkpoint_consumers_are_isolated() -> Result<()> {
    conformance::checkpoint_consumers_are_isolated(&backend().await).await
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL + migration 054"]
async fn reactor_attempts_count_monotonically() -> Result<()> {
    conformance::reactor_attempts_count_monotonically(&backend().await).await
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL + migration 054"]
async fn reactor_attempts_are_isolated() -> Result<()> {
    conformance::reactor_attempts_are_isolated(&backend().await).await
}

#[tokio::test]
#[ignore = "requires local DATABASE_URL + migration 054"]
async fn reactor_attempts_clear_resets() -> Result<()> {
    conformance::reactor_attempts_clear_resets(&backend().await).await
}
