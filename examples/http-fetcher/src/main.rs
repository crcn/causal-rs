//! # HTTP Fetcher Example
//!
//! Shows how to use `reqwest` directly in Seesaw effects.
//! No adapters, no ceremony - just standard library usage.

use anyhow::Result;
use async_trait::async_trait;
use seesaw_core::{Effect, EffectContext, EngineBuilder};
use uuid::Uuid;

// ============================================================================
// Events (Facts)
// ============================================================================

#[derive(Debug, Clone)]
enum FetchEvent {
    /// User requested a URL to be fetched
    FetchRequested {
        fetch_id: Uuid,
        url: String,
    },

    /// Fetch succeeded
    Fetched {
        fetch_id: Uuid,
        url: String,
        content: String,
        status: u16,
    },

    /// Fetch failed
    FetchFailed {
        fetch_id: Uuid,
        url: String,
        reason: String,
    },
}

// Event is auto-implemented via blanket impl for Clone + Send + Sync + 'static

// ============================================================================
// Effect (React to events, execute IO, emit new events)
// ============================================================================

struct FetchEffect;

#[async_trait]
impl Effect<FetchEvent, Deps> for FetchEffect {
    type Event = FetchEvent;

    async fn handle(
        &mut self,
        event: FetchEvent,
        ctx: EffectContext<Deps>,
    ) -> Result<Option<FetchEvent>> {
        match event {
            FetchEvent::FetchRequested { fetch_id, url } => {
                println!("Fetching: {}", url);

                // Use reqwest directly - no adapter needed!
                match ctx.deps().http_client.get(&url).send().await {
                    Ok(response) => {
                        let status = response.status().as_u16();

                        if response.status().is_success() {
                            let content = response.text().await?;

                            println!("✓ Fetched {} ({} bytes)", url, content.len());

                            Ok(Some(FetchEvent::Fetched {
                                fetch_id,
                                url,
                                content,
                                status,
                            }))
                        } else {
                            println!("✗ Failed {} (HTTP {})", url, status);

                            Ok(Some(FetchEvent::FetchFailed {
                                fetch_id,
                                url,
                                reason: format!("HTTP {}", status),
                            }))
                        }
                    }
                    Err(e) => {
                        println!("✗ Failed {}: {}", url, e);

                        Ok(Some(FetchEvent::FetchFailed {
                            fetch_id,
                            url,
                            reason: e.to_string(),
                        }))
                    }
                }
            }
            _ => Ok(None), // Event doesn't apply to this effect
        }
    }
}

// ============================================================================
// Dependencies
// ============================================================================

struct Deps {
    http_client: reqwest::Client,
}

// ============================================================================
// Main
// ============================================================================

#[tokio::main]
async fn main() -> Result<()> {
    let deps = Deps {
        http_client: reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()?,
    };

    let engine = EngineBuilder::new(deps)
        .with_effect::<FetchEvent, _>(FetchEffect)
        .build();

    let handle = engine.start();

    // Fetch some URLs
    let urls = vec![
        "https://example.com",
        "https://httpbin.org/status/200",
        "https://httpbin.org/status/404",
    ];

    for url in urls {
        let fetch_id = Uuid::new_v4();

        // Emit event and wait for cascading work to complete
        handle
            .emit_and_await(FetchEvent::FetchRequested {
                fetch_id,
                url: url.to_string(),
            })
            .await?;
    }

    println!("\nAll fetches complete!");

    Ok(())
}
