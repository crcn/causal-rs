//! # AI Summarizer Example
//!
//! Shows how to call the Anthropic API directly in Seesaw effects with `.then()`.
//! Effects return events directly - success or failure variants.

use anyhow::{bail, Result};
use seesaw_core::{effect, reducer, Engine, EffectContext};
use serde::{Deserialize, Serialize};
use std::env;
use uuid::Uuid;

// ============================================================================
// Events (Facts)
// ============================================================================

#[derive(Debug, Clone)]
enum SummaryEvent {
    /// User requested text to be summarized
    SummarizeRequested { task_id: Uuid, text: String },

    /// Summary generated
    Summarized { task_id: Uuid, summary: String, tokens_used: u32 },

    /// Summary failed
    SummaryFailed { task_id: Uuid, reason: String },
}

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
// Anthropic API Types
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

#[derive(Clone)]
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

    // Define engine with closure-based effects and reducers
    let engine = Engine::with_deps(deps)
        // Reducer - pure state transformation
        .with_reducer(reducer::on::<SummaryEvent>().run(|state: SummaryState, event| {
            match event {
                SummaryEvent::Summarized { summary, tokens_used, .. } => SummaryState {
                    summary: Some(summary.clone()),
                    tokens_used: *tokens_used,
                    error: None,
                },
                SummaryEvent::SummaryFailed { reason, .. } => SummaryState {
                    summary: None,
                    tokens_used: 0,
                    error: Some(reason.clone()),
                },
                _ => state,
            }
        }))
        // Effect - call Anthropic API on request, return Summarized or SummaryFailed
        .with_effect(
            effect::on::<SummaryEvent>()
                .filter_map(|e| match e {
                    SummaryEvent::SummarizeRequested { task_id, text } => {
                        Some((task_id.clone(), text.clone()))
                    }
                    _ => None,
                })
                .then(|(task_id, text), ctx: EffectContext<SummaryState, Deps>| async move {
                    println!("Summarizing text...");

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

                            Ok(SummaryEvent::Summarized {
                                task_id,
                                summary,
                                tokens_used: tokens,
                            })
                        }
                        Err(e) => {
                            println!("✗ Failed: {}", e);

                            Ok(SummaryEvent::SummaryFailed {
                                task_id,
                                reason: e.to_string(),
                            })
                        }
                    }
                })
        );

    // Summarize some text
    let text = r#"
        Rust is a multi-paradigm, general-purpose programming language that emphasizes
        performance, type safety, and concurrency. It enforces memory safety—meaning that
        all references point to valid memory—without a garbage collector. To simultaneously
        enforce memory safety and prevent data races, its "borrow checker" tracks the object
        lifetime of all references in a program during compilation.
    "#;

    // Activate with initial state
    let handle = engine.activate(SummaryState::default());

    // Run with closure that returns initial event
    let task_id = Uuid::new_v4();
    handle.run(|_ctx| {
        Ok(SummaryEvent::SummarizeRequested {
            task_id,
            text: text.to_string(),
        })
    })?;

    // Wait for all effects to complete
    handle.settled().await?;

    println!("\nDone!");

    Ok(())
}
