//! AI Summarizer example backed by KurrentDB.
//!
//! Run KurrentDB first:
//!
//!     docker compose up -d
//!
//! Then:
//!
//!     export ANTHROPIC_API_KEY=your-key
//!     cargo run

use std::sync::Arc;

use anyhow::{bail, Result};
use async_trait::async_trait;
use causal::{
    CheckpointStore, Ctx, EngineBuilder, Event, EventLogBackend, Events,
    MemoryStore, Reactor, ReactorCheckpoint,
};
use causal_replay::KurrentEventLogBackend;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Serialize, Deserialize)]
struct SummarizeRequested {
    task_id:     Uuid,
    text:        String,
    occurred_at: DateTime<Utc>,
}

impl Event for SummarizeRequested {
    const CATEGORY: &'static str = "summary_request";
    fn event_type(&self) -> &str { "requested" }
    fn stream_id(&self) -> Uuid { self.task_id }
    fn occurred_at(&self) -> Option<DateTime<Utc>> { Some(self.occurred_at) }
}

#[derive(Clone, Serialize, Deserialize)]
struct Summarized {
    task_id:     Uuid,
    summary:     String,
    tokens_used: u32,
    occurred_at: DateTime<Utc>,
}

impl Event for Summarized {
    const CATEGORY: &'static str = "summary_result";
    fn event_type(&self) -> &str { "summarized" }
    fn stream_id(&self) -> Uuid { self.task_id }
    fn occurred_at(&self) -> Option<DateTime<Utc>> { Some(self.occurred_at) }
}

#[derive(Clone, Serialize, Deserialize)]
struct SummaryFailed {
    task_id:     Uuid,
    reason:      String,
    occurred_at: DateTime<Utc>,
}

impl Event for SummaryFailed {
    const CATEGORY: &'static str = "summary_result";
    fn event_type(&self) -> &str { "failed" }
    fn stream_id(&self) -> Uuid { self.task_id }
    fn occurred_at(&self) -> Option<DateTime<Utc>> { Some(self.occurred_at) }
}

#[derive(Serialize)]
struct AnthropicRequest {
    model:      String,
    max_tokens: u32,
    messages:   Vec<Message>,
}

#[derive(Serialize, Deserialize)]
struct Message {
    role:    String,
    content: String,
}

#[derive(Deserialize)]
struct AnthropicResponse {
    content: Vec<ContentBlock>,
    usage:   Usage,
}

#[derive(Deserialize)]
struct ContentBlock {
    text: Option<String>,
}

#[derive(Deserialize)]
struct Usage {
    input_tokens:  u32,
    output_tokens: u32,
}

async fn call_anthropic(
    client:  &reqwest::Client,
    api_key: &str,
    request: AnthropicRequest,
) -> Result<AnthropicResponse> {
    let response = client
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .json(&request)
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await?;
        bail!("Anthropic API error {}: {}", status, body);
    }

    Ok(response.json().await?)
}

struct SummarizeReactor {
    http_client: reqwest::Client,
    api_key:     String,
}

#[async_trait]
impl Reactor for SummarizeReactor {
    type Trigger = SummarizeRequested;
    const GROUP_NAME: &'static str = "summarize";

    async fn react(&self, trigger: &SummarizeRequested, _ctx: Ctx<'_>) -> Result<Events> {
        let request = AnthropicRequest {
            model:      "claude-sonnet-4-20250514".to_string(),
            max_tokens: 1024,
            messages:   vec![Message {
                role:    "user".to_string(),
                content: format!("Summarize this text in 2-3 sentences:\n\n{}", trigger.text),
            }],
        };

        let occurred_at = Utc::now();
        match call_anthropic(&self.http_client, &self.api_key, request).await {
            Ok(response) => {
                let summary = response
                    .content
                    .first()
                    .and_then(|c| c.text.clone())
                    .unwrap_or_default();
                let tokens_used = response.usage.input_tokens + response.usage.output_tokens;
                println!("summary ({} tokens): {}", tokens_used, summary);
                Ok(Events::new().add(Summarized {
                    task_id: trigger.task_id,
                    summary,
                    tokens_used,
                    occurred_at,
                }))
            }
            Err(e) => {
                let reason = e.to_string();
                eprintln!("summary failed: {reason}");
                Ok(Events::new().add(SummaryFailed {
                    task_id: trigger.task_id,
                    reason,
                    occurred_at,
                }))
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let api_key = match std::env::var("ANTHROPIC_API_KEY") {
        Ok(k) => k,
        Err(_) => {
            eprintln!("ANTHROPIC_API_KEY not set.");
            eprintln!();
            eprintln!("export it and re-run:");
            eprintln!("    export ANTHROPIC_API_KEY=sk-ant-...");
            std::process::exit(1);
        }
    };

    let kurrent_url = std::env::var("KURRENT_URL")
        .unwrap_or_else(|_| "kurrentdb://localhost:2113?tls=false".to_string());

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
    let http_client = reqwest::Client::new();

    let engine = EngineBuilder::new(
        Arc::new(kurrent) as Arc<dyn EventLogBackend>,
        mem.clone() as Arc<dyn CheckpointStore>,
        mem as Arc<dyn ReactorCheckpoint>,
    )
    .with_reactor(SummarizeReactor { http_client, api_key })
    .build();

    let text = "Rust is a multi-paradigm, general-purpose programming language that emphasizes \
                performance, type safety, and concurrency.";

    engine
        .emit(SummarizeRequested {
            task_id:     Uuid::new_v4(),
            text:        text.to_string(),
            occurred_at: Utc::now(),
        })
        .settled()
        .await?;

    println!();
    println!("event log → {kurrent_url}");
    Ok(())
}
