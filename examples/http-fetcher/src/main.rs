//! HTTP Fetcher example: KurrentDB event log + Postgres reactor checkpoint.
//!
//! Run the stack first (Kurrent + Postgres):
//!
//!     docker compose up -d
//!
//! Then:
//!
//!     cargo run
//!
//! With the PG checkpoint store, the reactor cursor survives restarts — re-running
//! `cargo run` skips events from prior runs and only processes the new
//! `FetchRequested` emissions.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use causal::{
    CheckpointStore, Ctx, EngineBuilder, Event, EventLogBackend, Events,
    InMemoryEffectStore, Reactor, ReactorCheckpoint,
};
use causal_replay::{KurrentEventLogBackend, PgReactorCheckpoint};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

#[derive(Clone, Serialize, Deserialize)]
struct FetchRequested {
    request_id:  Uuid,
    url:         String,
    occurred_at: DateTime<Utc>,
}

impl Event for FetchRequested {
    // The fact's kind — flat, exact, matched by equality (0.10).
    const NAME: &'static str = "fetch_requested";
    fn subject_id(&self) -> Uuid { self.request_id }
    fn occurred_at(&self) -> Option<DateTime<Utc>> { Some(self.occurred_at) }
}

#[derive(Clone, Serialize, Deserialize)]
struct Fetched {
    request_id:  Uuid,
    url:         String,
    status:      u16,
    occurred_at: DateTime<Utc>,
}

impl Event for Fetched {
    const NAME: &'static str = "fetched";
    // Co-locate both outcomes in one subject history per request.
    const SUBJECT: &'static str = "fetch_result";
    fn subject_id(&self) -> Uuid { self.request_id }
    fn occurred_at(&self) -> Option<DateTime<Utc>> { Some(self.occurred_at) }
}

#[derive(Clone, Serialize, Deserialize)]
struct FetchFailed {
    request_id:  Uuid,
    url:         String,
    reason:      String,
    occurred_at: DateTime<Utc>,
}

impl Event for FetchFailed {
    const NAME: &'static str = "fetch_failed";
    const SUBJECT: &'static str = "fetch_result";
    fn subject_id(&self) -> Uuid { self.request_id }
    fn occurred_at(&self) -> Option<DateTime<Utc>> { Some(self.occurred_at) }
}

struct FetchReactor {
    http_client: reqwest::Client,
}

/// The memoized outcome of one HTTP call — `ctx.effect` stores this, so
/// a crash-redelivery replays the SAME outcome instead of re-fetching
/// (the determinism that lets the log dedup the reactor's output).
#[derive(Serialize, Deserialize)]
enum FetchOutcome {
    Status(u16),
    TransportError(String),
}

#[async_trait]
impl Reactor for FetchReactor {
    type Trigger = FetchRequested;
    const NAME: &'static str = "fetch_url";

    async fn react(&self, trigger: &FetchRequested, ctx: Ctx<'_>) -> Result<Events> {
        // The external call goes through the one deterministic door:
        // memoized under (consumer, trigger, label), it runs once per
        // reaction no matter how many times the trigger redelivers.
        let client = self.http_client.clone();
        let url = trigger.url.clone();
        let outcome: FetchOutcome = ctx
            .effect("fetch", || async move {
                Ok(match client.get(&url).send().await {
                    Ok(response) => FetchOutcome::Status(response.status().as_u16()),
                    Err(error) => FetchOutcome::TransportError(error.to_string()),
                })
            })
            .await?;

        // Timestamps come from the trigger (`ctx.time()`), never the
        // wall clock — replay reproduces the payload byte-identically.
        match outcome {
            FetchOutcome::Status(status) => {
                println!("fetched {} → {}", trigger.url, status);
                Ok(causal::events![Fetched {
                    request_id: trigger.request_id,
                    url: trigger.url.clone(),
                    status,
                    occurred_at: ctx.time(),
                }])
            }
            FetchOutcome::TransportError(reason) => {
                println!("failed  {} → {}", trigger.url, reason);
                Ok(causal::events![FetchFailed {
                    request_id: trigger.request_id,
                    url: trigger.url.clone(),
                    reason,
                    occurred_at: ctx.time(),
                }])
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let kurrent_url = std::env::var("KURRENT_URL")
        .unwrap_or_else(|_| "kurrentdb://localhost:2113?tls=false".to_string());
    let database_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://causal:causal@localhost:54320/causal".to_string());

    let kurrent = match KurrentEventLogBackend::connect(&kurrent_url) {
        Ok(client) => client,
        Err(e) => {
            eprintln!("could not connect to KurrentDB at {kurrent_url}: {e}");
            eprintln!();
            eprintln!("start the stack with:  docker compose up -d");
            std::process::exit(1);
        }
    };

    let pg_pool = match PgPoolOptions::new().max_connections(4).connect(&database_url).await {
        Ok(pool) => pool,
        Err(e) => {
            eprintln!("could not connect to Postgres at {database_url}: {e}");
            eprintln!();
            eprintln!("start the stack with:  docker compose up -d");
            std::process::exit(1);
        }
    };

    let pg = Arc::new(PgReactorCheckpoint::new(pg_pool));
    let http_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;

    let engine = EngineBuilder::new(
        Arc::new(kurrent) as Arc<dyn EventLogBackend>,
        pg.clone() as Arc<dyn CheckpointStore>,
        pg as Arc<dyn ReactorCheckpoint>,
    )
    // In-memory effect memos are fine for a demo; production wires a
    // durable EffectStore so redelivery stays deterministic across
    // restarts too.
    .with_effect_store(Arc::new(InMemoryEffectStore::new()))
    .with_reactor(FetchReactor { http_client })
    .build()
    .await?;

    let urls = [
        "https://example.com",
        "https://httpbin.org/status/200",
        "https://httpbin.org/status/404",
    ];

    let now = Utc::now();
    let requests: Vec<FetchRequested> = urls
        .into_iter()
        .map(|url| FetchRequested {
            request_id:  Uuid::new_v4(),
            url:         url.to_string(),
            occurred_at: now,
        })
        .collect();

    engine.emit(requests).settled().await?;

    println!();
    println!("all fetches drained. event log → {kurrent_url}");
    Ok(())
}
