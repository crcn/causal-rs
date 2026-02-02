//! # HTTP Fetcher Example
//!
//! Shows how to use `reqwest` directly in Seesaw effects.
//! No adapters, no ceremony - just standard library usage.

use anyhow::Result;
use async_trait::async_trait;
use seesaw_core::{Edge, EdgeContext, Effect, EffectContext, EngineBuilder, Event};
use std::sync::Arc;
use uuid::Uuid;

// ============================================================================
// Events (Facts)
// ============================================================================

#[derive(Debug, Clone)]
enum FetchEvent {
    /// User requested URLs to be fetched
    FetchRequested {
        urls: Vec<String>,
    },

    /// Fetch succeeded
    Fetched {
        url: String,
        content: String,
        status: u16,
    },

    /// Fetch failed
    FetchFailed {
        url: String,
        reason: String,
    },

    /// All fetches complete
    AllComplete {
        success_count: usize,
        failure_count: usize,
    },
}

// Event is auto-implemented via blanket impl for Clone + Send + Sync + 'static

// ============================================================================
// State
// ============================================================================

#[derive(Clone, Default)]
struct FetchState {
    urls_to_fetch: Vec<String>,
    success_count: usize,
    failure_count: usize,
    complete: bool,
}

// ============================================================================
// Effect (React to events, execute IO, emit new events)
// ============================================================================

struct FetchEffect;

#[async_trait]
impl Effect<FetchEvent, Deps, FetchState> for FetchEffect {
    type Event = FetchEvent;

    async fn handle(
        &mut self,
        event: FetchEvent,
        ctx: EffectContext<Deps, FetchState>,
    ) -> Result<Option<FetchEvent>> {
        match event {
            FetchEvent::FetchRequested { urls } => {
                // Fetch first URL
                if let Some(url) = urls.first() {
                    let url = url.clone();
                    println!("Fetching: {}", url);

                    match ctx.deps().http_client.get(&url).send().await {
                        Ok(response) => {
                            let status = response.status().as_u16();

                            if response.status().is_success() {
                                let content = response.text().await?;
                                println!("✓ Fetched {} ({} bytes)", url, content.len());

                                Ok(Some(FetchEvent::Fetched {
                                    url,
                                    content,
                                    status,
                                }))
                            } else {
                                println!("✗ Failed {} (HTTP {})", url, status);

                                Ok(Some(FetchEvent::FetchFailed {
                                    url,
                                    reason: format!("HTTP {}", status),
                                }))
                            }
                        }
                        Err(e) => {
                            println!("✗ Failed {}: {}", url, e);

                            Ok(Some(FetchEvent::FetchFailed {
                                url,
                                reason: e.to_string(),
                            }))
                        }
                    }
                } else {
                    // No URLs to fetch
                    let state = ctx.state();
                    Ok(Some(FetchEvent::AllComplete {
                        success_count: state.success_count,
                        failure_count: state.failure_count,
                    }))
                }
            }
            FetchEvent::Fetched { .. } => {
                // Check if more URLs to fetch
                let state = ctx.state();
                if !state.urls_to_fetch.is_empty() {
                    Ok(Some(FetchEvent::FetchRequested {
                        urls: state.urls_to_fetch.clone(),
                    }))
                } else {
                    Ok(Some(FetchEvent::AllComplete {
                        success_count: state.success_count,
                        failure_count: state.failure_count,
                    }))
                }
            }
            FetchEvent::FetchFailed { .. } => {
                // Check if more URLs to fetch
                let state = ctx.state();
                if !state.urls_to_fetch.is_empty() {
                    Ok(Some(FetchEvent::FetchRequested {
                        urls: state.urls_to_fetch.clone(),
                    }))
                } else {
                    Ok(Some(FetchEvent::AllComplete {
                        success_count: state.success_count,
                        failure_count: state.failure_count,
                    }))
                }
            }
            _ => Ok(None), // Skip other events
        }
    }
}

// ============================================================================
// Reducer (Pure state transformation)
// ============================================================================

struct FetchReducer;

impl seesaw_core::Reducer<FetchEvent, FetchState> for FetchReducer {
    fn reduce(&self, state: &FetchState, event: &FetchEvent) -> FetchState {
        match event {
            FetchEvent::FetchRequested { urls } => {
                let mut new_state = state.clone();
                if new_state.urls_to_fetch.is_empty() {
                    // Initial request - set all URLs
                    new_state.urls_to_fetch = urls.clone();
                }
                new_state
            }
            FetchEvent::Fetched { .. } => {
                let mut new_state = state.clone();
                new_state.success_count += 1;
                if !new_state.urls_to_fetch.is_empty() {
                    new_state.urls_to_fetch.remove(0);
                }
                new_state
            }
            FetchEvent::FetchFailed { .. } => {
                let mut new_state = state.clone();
                new_state.failure_count += 1;
                if !new_state.urls_to_fetch.is_empty() {
                    new_state.urls_to_fetch.remove(0);
                }
                new_state
            }
            FetchEvent::AllComplete { .. } => {
                let mut new_state = state.clone();
                new_state.complete = true;
                new_state
            }
        }
    }
}

// ============================================================================
// Edge (Entry point)
// ============================================================================

struct FetchEdge {
    urls: Vec<String>,
}

impl Edge<FetchState> for FetchEdge {
    type Event = FetchEvent;
    type Data = (usize, usize); // (success_count, failure_count)

    fn execute(&self, _ctx: &EdgeContext<FetchState>) -> Option<FetchEvent> {
        Some(FetchEvent::FetchRequested {
            urls: self.urls.clone(),
        })
    }

    fn read(&self, state: &FetchState) -> Option<Self::Data> {
        if state.complete {
            Some((state.success_count, state.failure_count))
        } else {
            None
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

    let mut engine = EngineBuilder::new(deps)
        .with_effect::<FetchEvent, _>(FetchEffect)
        .build();

    let urls = vec![
        "https://example.com".to_string(),
        "https://httpbin.org/status/200".to_string(),
        "https://httpbin.org/status/404".to_string(),
    ];

    let (success, failure) = engine
        .run(FetchEdge { urls }, FetchState::default())
        .await?
        .unwrap_or((0, 0));

    println!(
        "\nAll fetches complete! Success: {}, Failed: {}",
        success, failure
    );

    Ok(())
}
