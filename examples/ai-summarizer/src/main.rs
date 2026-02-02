//! # AI Summarizer Example
//!
//! Shows how to call the Anthropic API directly in Seesaw effects.
//! No special adapter - just reqwest + serde.

use anyhow::{bail, Result};
use async_trait::async_trait;
use seesaw_core::{Edge, EdgeContext, Effect, EffectContext, EngineBuilder, Event};
use serde::{Deserialize, Serialize};
use std::env;
use uuid::Uuid;

// ============================================================================
// Events (Facts)
// ============================================================================

#[derive(Debug, Clone)]
enum SummaryEvent {
    /// User requested text to be summarized
    SummarizeRequested {
        task_id: Uuid,
        text: String,
    },

    /// Summary generated
    Summarized {
        task_id: Uuid,
        summary: String,
        tokens_used: u32,
    },

    /// Summary failed
    SummaryFailed {
        task_id: Uuid,
        reason: String,
    },
}

// Event is auto-implemented via blanket impl for Clone + Send + Sync + 'static

// ============================================================================
// State
// ============================================================================

#[derive(Clone, Default)]
struct SummaryState {
    summary: Option<String>,
    tokens_used: u32,
    error: Option<String>,
}

// ============================================================================
// Effect (React to events, call Anthropic API, emit new events)
// ============================================================================

struct SummarizeEffect;

#[async_trait]
impl Effect<SummaryEvent, Deps, SummaryState> for SummarizeEffect {
    type Event = SummaryEvent;

    async fn handle(
        &mut self,
        event: SummaryEvent,
        ctx: EffectContext<Deps, SummaryState>,
    ) -> Result<Option<SummaryEvent>> {
        match event {
            SummaryEvent::SummarizeRequested { task_id, text } => {
                println!("Summarizing text...");

                // Call Anthropic API directly using reqwest
                let request = AnthropicRequest {
                    model: "claude-3-5-sonnet-20241022".to_string(),
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

                        println!("\n✓ Summary: {}", summary);
                        println!("  Tokens used: {}", tokens);

                        Ok(Some(SummaryEvent::Summarized {
                            task_id,
                            summary,
                            tokens_used: tokens,
                        }))
                    }
                    Err(e) => {
                        println!("✗ Failed: {}", e);

                        Ok(Some(SummaryEvent::SummaryFailed {
                            task_id,
                            reason: e.to_string(),
                        }))
                    }
                }
            }
            _ => Ok(None), // Skip other events
        }
    }
}

// ============================================================================
// Reducer (Pure state transformation)
// ============================================================================

struct SummaryReducer;

impl seesaw_core::Reducer<SummaryEvent, SummaryState> for SummaryReducer {
    fn reduce(&self, state: &SummaryState, event: &SummaryEvent) -> SummaryState {
        match event {
            SummaryEvent::Summarized {
                summary,
                tokens_used,
                ..
            } => SummaryState {
                summary: Some(summary.clone()),
                tokens_used: *tokens_used,
                error: None,
            },
            SummaryEvent::SummaryFailed { reason, .. } => SummaryState {
                summary: None,
                tokens_used: 0,
                error: Some(reason.clone()),
            },
            _ => state.clone(),
        }
    }
}

// ============================================================================
// Edge (Entry point)
// ============================================================================

struct SummarizeEdge {
    text: String,
}

impl Edge<SummaryState> for SummarizeEdge {
    type Event = SummaryEvent;
    type Data = String; // The summary

    fn execute(&self, _ctx: &EdgeContext<SummaryState>) -> Option<SummaryEvent> {
        Some(SummaryEvent::SummarizeRequested {
            task_id: Uuid::new_v4(),
            text: self.text.clone(),
        })
    }

    fn read(&self, state: &SummaryState) -> Option<Self::Data> {
        state.summary.clone()
    }
}

// ============================================================================
// Anthropic API Types (Just plain structs)
// ============================================================================

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
    #[serde(rename = "type")]
    block_type: String,
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Usage {
    input_tokens: u32,
    output_tokens: u32,
}

/// Call Anthropic API - just a plain function
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

// ============================================================================
// Dependencies
// ============================================================================

struct Deps {
    http_client: reqwest::Client,
    api_key: String,
}

// ============================================================================
// Main
// ============================================================================

#[tokio::main]
async fn main() -> Result<()> {
    let api_key = env::var("ANTHROPIC_API_KEY")
        .expect("ANTHROPIC_API_KEY environment variable required");

    let deps = Deps {
        http_client: reqwest::Client::new(),
        api_key,
    };

    let mut engine = EngineBuilder::new(deps)
        .with_effect::<SummaryEvent, _>(SummarizeEffect)
        .build();

    // Summarize some text
    let text = r#"
        Rust is a multi-paradigm, general-purpose programming language that emphasizes
        performance, type safety, and concurrency. It enforces memory safety—meaning that
        all references point to valid memory—without a garbage collector. To simultaneously
        enforce memory safety and prevent data races, its "borrow checker" tracks the object
        lifetime of all references in a program during compilation.
    "#;

    let summary = engine
        .run(
            SummarizeEdge {
                text: text.to_string(),
            },
            SummaryState::default(),
        )
        .await?;

    if let Some(summary) = summary {
        println!("\nFinal summary: {}", summary);
    } else {
        println!("\nFailed to generate summary");
    }

    println!("\nDone!");

    Ok(())
}
