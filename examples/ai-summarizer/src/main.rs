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
    // The fact's kind — flat, exact, matched by equality (0.10).
    const NAME: &'static str = "summarize_requested";
    fn subject_id(&self) -> Uuid { self.task_id }
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
    const NAME: &'static str = "summarized";
    // Co-locate both outcomes in one subject history per task.
    const SUBJECT: &'static str = "summary_result";
    fn subject_id(&self) -> Uuid { self.task_id }
    fn occurred_at(&self) -> Option<DateTime<Utc>> { Some(self.occurred_at) }
}

#[derive(Clone, Serialize, Deserialize)]
struct SummaryFailed {
    task_id:     Uuid,
    reason:      String,
    occurred_at: DateTime<Utc>,
}

impl Event for SummaryFailed {
    const NAME: &'static str = "summary_failed";
    const SUBJECT: &'static str = "summary_result";
    fn subject_id(&self) -> Uuid { self.task_id }
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

/// The memoized outcome of one LLM call — `ctx.effect` stores it, so a
/// crash-redelivery replays the SAME summary instead of paying for (and
/// emitting) a different one.
#[derive(Serialize, Deserialize)]
enum SummaryOutcome {
    Done { summary: String, tokens_used: u32 },
    Failed { reason: String },
}

#[async_trait]
impl Reactor for SummarizeReactor {
    type Trigger = SummarizeRequested;
    const NAME: &'static str = "summarize";

    async fn react(&self, trigger: &SummarizeRequested, ctx: Ctx<'_>) -> Result<Events> {
        let request = AnthropicRequest {
            model:      "claude-sonnet-4-20250514".to_string(),
            max_tokens: 1024,
            messages:   vec![Message {
                role:    "user".to_string(),
                content: format!("Summarize this text in 2-3 sentences:\n\n{}", trigger.text),
            }],
        };

        // The LLM call goes through the deterministic door: memoized
        // under (consumer, trigger, label), it runs once per reaction
        // no matter how many times the trigger redelivers.
        let client = self.http_client.clone();
        let api_key = self.api_key.clone();
        let outcome: SummaryOutcome = ctx
            .effect("summarize", || async move {
                Ok(match call_anthropic(&client, &api_key, request).await {
                    Ok(response) => {
                        let summary = response
                            .content
                            .first()
                            .and_then(|c| c.text.clone())
                            .unwrap_or_default();
                        let tokens_used =
                            response.usage.input_tokens + response.usage.output_tokens;
                        SummaryOutcome::Done { summary, tokens_used }
                    }
                    Err(e) => SummaryOutcome::Failed { reason: e.to_string() },
                })
            })
            .await?;

        // Timestamps come from the trigger (`ctx.time()`), never the
        // wall clock — replay reproduces the payload byte-identically.
        match outcome {
            SummaryOutcome::Done { summary, tokens_used } => {
                println!("summary ({} tokens): {}", tokens_used, summary);
                Ok(causal::events![Summarized {
                    task_id: trigger.task_id,
                    summary,
                    tokens_used,
                    occurred_at: ctx.time(),
                }])
            }
            SummaryOutcome::Failed { reason } => {
                eprintln!("summary failed: {reason}");
                Ok(causal::events![SummaryFailed {
                    task_id: trigger.task_id,
                    reason,
                    occurred_at: ctx.time(),
                }])
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
    .with_effect_store(Arc::new(causal::InMemoryEffectStore::new()))
    .with_reactor(SummarizeReactor { http_client, api_key })
    .build()
    .await?;

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
