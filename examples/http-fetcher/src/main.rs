//! # HTTP Fetcher Example
//!
//! Shows how to use `reqwest` directly in Seesaw effects.
//! No adapters, no ceremony - just standard library usage.

use anyhow::Result;
use seesaw_core::{effect, reducer, Engine, EffectContext};

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
// Effects and Reducers (Closure-based)
// ============================================================================

// No struct definitions needed - we use closures directly

// ============================================================================
// Dependencies
// ============================================================================

#[derive(Clone)]
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

    // Define engine with closure-based effects and reducers
    let engine = Engine::with_deps(deps)
        // Reducer - pure state transformation
        .with_reducer(reducer::on::<FetchEvent>().run(|state: FetchState, event| {
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
        }))
        // Effect 1 - Fetch URLs
        .with_effect(effect::on::<FetchEvent>().run(|event, ctx: EffectContext<FetchState, Deps>| async move {
            match event.as_ref() {
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

                                    ctx.emit(FetchEvent::Fetched {
                                        url,
                                        content,
                                        status,
                                    });
                                } else {
                                    println!("✗ Failed {} (HTTP {})", url, status);

                                    ctx.emit(FetchEvent::FetchFailed {
                                        url,
                                        reason: format!("HTTP {}", status),
                                    });
                                }
                            }
                            Err(e) => {
                                println!("✗ Failed {}: {}", url, e);

                                ctx.emit(FetchEvent::FetchFailed {
                                    url,
                                    reason: e.to_string(),
                                });
                            }
                        }
                    } else {
                        // No URLs to fetch
                        let state = ctx.curr_state();
                        ctx.emit(FetchEvent::AllComplete {
                            success_count: state.success_count,
                            failure_count: state.failure_count,
                        });
                    }
                }
                _ => {} // Skip other events
            }
            Ok(())
        }))
        // Effect 2 - Continue fetching after success/failure
        .with_effect(effect::on::<FetchEvent>().run(|event, ctx: EffectContext<FetchState, Deps>| async move {
            match event.as_ref() {
                FetchEvent::Fetched { .. } | FetchEvent::FetchFailed { .. } => {
                    // Check if more URLs to fetch
                    let state = ctx.curr_state();
                    if !state.urls_to_fetch.is_empty() {
                        ctx.emit(FetchEvent::FetchRequested {
                            urls: state.urls_to_fetch.clone(),
                        });
                    } else {
                        ctx.emit(FetchEvent::AllComplete {
                            success_count: state.success_count,
                            failure_count: state.failure_count,
                        });
                    }
                }
                _ => {} // Skip other events
            }
            Ok(())
        }));

    let urls = vec![
        "https://example.com".to_string(),
        "https://httpbin.org/status/200".to_string(),
        "https://httpbin.org/status/404".to_string(),
    ];

    // Activate with initial state
    let handle = engine.activate(FetchState::default());

    // Run with closure that emits initial event
    handle.run(|ctx| {
        ctx.emit(FetchEvent::FetchRequested {
            urls: urls.clone(),
        });
        Ok(())
    })?;

    // Wait for all effects to complete
    handle.settled().await?;

    println!("\nAll fetches complete!");

    Ok(())
}
