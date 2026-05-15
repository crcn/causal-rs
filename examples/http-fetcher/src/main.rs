//! HTTP Fetcher example backed by KurrentDB.
//!
//! Run KurrentDB first:
//!
//!     docker compose up -d
//!
//! Then:
//!
//!     cargo run

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use causal::{
    CheckpointStore, Ctx, EngineBuilder, Event, EventLogBackend, Events,
    MemoryStore, Reactor, ReactorOutbox,
};
use causal_replay::KurrentEventLogBackend;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Serialize, Deserialize)]
struct FetchRequested {
    request_id:  Uuid,
    url:         String,
    occurred_at: DateTime<Utc>,
}

impl Event for FetchRequested {
    const CATEGORY: &'static str = "fetch_request";
    fn event_type(&self) -> &str { "requested" }
    fn stream_id(&self) -> Uuid { self.request_id }
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
    const CATEGORY: &'static str = "fetch_result";
    fn event_type(&self) -> &str { "fetched" }
    fn stream_id(&self) -> Uuid { self.request_id }
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
    const CATEGORY: &'static str = "fetch_result";
    fn event_type(&self) -> &str { "failed" }
    fn stream_id(&self) -> Uuid { self.request_id }
    fn occurred_at(&self) -> Option<DateTime<Utc>> { Some(self.occurred_at) }
}

struct FetchReactor {
    http_client: reqwest::Client,
}

#[async_trait]
impl Reactor for FetchReactor {
    type Trigger = FetchRequested;
    const GROUP_NAME: &'static str = "fetch_url";

    async fn react(&self, trigger: &FetchRequested, _ctx: Ctx<'_>) -> Result<Events> {
        let occurred_at = Utc::now();
        match self.http_client.get(&trigger.url).send().await {
            Ok(response) => {
                let status = response.status().as_u16();
                println!("fetched {} → {}", trigger.url, status);
                Ok(Events::new().add(Fetched {
                    request_id: trigger.request_id,
                    url: trigger.url.clone(),
                    status,
                    occurred_at,
                }))
            }
            Err(error) => {
                let reason = error.to_string();
                println!("failed  {} → {}", trigger.url, reason);
                Ok(Events::new().add(FetchFailed {
                    request_id: trigger.request_id,
                    url: trigger.url.clone(),
                    reason,
                    occurred_at,
                }))
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let kurrent_url = std::env::var("KURRENT_URL")
        .unwrap_or_else(|_| "esdb://localhost:2113?tls=false".to_string());

    let kurrent = match KurrentEventLogBackend::connect(&kurrent_url) {
        Ok(client) => client,
        Err(e) => {
            eprintln!("could not connect to KurrentDB at {kurrent_url}: {e}");
            eprintln!();
            eprintln!("start one locally with:");
            eprintln!("    docker compose up -d");
            std::process::exit(1);
        }
    };

    let mem = Arc::new(MemoryStore::new());
    let http_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;

    let engine = EngineBuilder::new(
        Arc::new(kurrent) as Arc<dyn EventLogBackend>,
        mem.clone() as Arc<dyn CheckpointStore>,
        mem as Arc<dyn ReactorOutbox>,
    )
    .with_reactor(FetchReactor { http_client })
    .build();

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
