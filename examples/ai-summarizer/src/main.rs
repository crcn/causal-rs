//! AI Summarizer Example

use anyhow::{bail, Result};
use causal_core::{event, events, handler, Context, Engine};
use serde::{Deserialize, Serialize};
use std::env;
use uuid::Uuid;

#[event(prefix = "summary")]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
enum SummaryEvent {
    SummarizeRequested {
        task_id: Uuid,
        text: String,
    },
    Summarized {
        task_id: Uuid,
        summary: String,
        tokens_used: u32,
    },
    SummaryFailed {
        task_id: Uuid,
        reason: String,
    },
}

#[derive(Debug, Serialize)]
struct AnthropicRequest {
    model: String,
    max_tokens: u32,
    messages: Vec<Message>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Message {
    role: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct AnthropicResponse {
    content: Vec<ContentBlock>,
    usage: Usage,
}

#[derive(Debug, Deserialize)]
struct ContentBlock {
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Usage {
    input_tokens: u32,
    output_tokens: u32,
}

async fn call_anthropic(
    client: &reqwest::Client,
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
        bail!("API error {}: {}", status, body);
    }

    Ok(response.json().await?)
}

#[derive(Clone)]
struct Deps {
    http_client: reqwest::Client,
    api_key: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let api_key =
        env::var("ANTHROPIC_API_KEY").expect("ANTHROPIC_API_KEY environment variable required");

    let deps = Deps {
        http_client: reqwest::Client::new(),
        api_key,
    };

    let engine = Engine::in_memory(deps).with_handler(
        handler::on::<SummaryEvent>()
            .id("summarize")
            .extract(|e| match e {
                SummaryEvent::SummarizeRequested { task_id, text } => {
                    Some((*task_id, text.clone()))
                }
                _ => None,
            })
            .then(|(task_id, text), ctx: Context<Deps>| async move {
                let request = AnthropicRequest {
                    model: "claude-sonnet-4-20250514".to_string(),
                    max_tokens: 1024,
                    messages: vec![Message {
                        role: "user".to_string(),
                        content: format!("Summarize this text in 2-3 sentences:\n\n{}", text),
                    }],
                };

                match call_anthropic(&ctx.deps().http_client, &ctx.deps().api_key, request).await {
                    Ok(response) => {
                        let summary = response
                            .content
                            .first()
                            .and_then(|c| c.text.clone())
                            .unwrap_or_default();

                        let tokens = response.usage.input_tokens + response.usage.output_tokens;

                        Ok(events![SummaryEvent::Summarized {
                            task_id,
                            summary,
                            tokens_used: tokens,
                        }])
                    }
                    Err(e) => Ok(events![SummaryEvent::SummaryFailed {
                        task_id,
                        reason: e.to_string(),
                    }]),
                }
            }),
    );

    let task_id = Uuid::new_v4();
    let text = r#"
        Rust is a multi-paradigm, general-purpose programming language that emphasizes
        performance, type safety, and concurrency.
    "#
    .to_string();

    engine
        .emit(SummaryEvent::SummarizeRequested { task_id, text })
        .settled()
        .await?;

    Ok(())
}
