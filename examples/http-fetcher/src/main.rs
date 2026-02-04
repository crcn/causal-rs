//! # HTTP Fetcher Example
//!
//! Shows how to use `reqwest` directly in Seesaw effects with `.then()`.
//! Effects return events directly - no ctx.emit() needed.

use anyhow::Result;
use seesaw_core::{effect, reducer, Engine, EffectContext};

// ============================================================================
// Events (Facts)
// ============================================================================

#[derive(Debug, Clone)]
enum FetchEvent {
    /// User requested URLs to be fetched
    FetchRequested { urls: Vec<String> },

    /// Fetch succeeded
    Fetched { url: String, content: String, status: u16 },

    /// Fetch failed
    FetchFailed { url: String, reason: String },

    /// All fetches complete
    AllComplete { success_count: usize, failure_count: usize },
}

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
        .with_reducer(reducer::fold::<FetchEvent>().into(|state: FetchState, event| {
            match event {
                FetchEvent::FetchRequested { urls } => {
                    let mut new_state = state.clone();
                    if new_state.urls_to_fetch.is_empty() {
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
        // Effect 1 - Fetch URLs when requested
        .with_effect(
            effect::on::<FetchEvent>()
                .extract(|e| match e {
                    FetchEvent::FetchRequested { urls } => Some(urls.clone()),
                    _ => None,
                })
                .then(|urls, ctx: EffectContext<FetchState, Deps>| async move {
                    if let Some(url) = urls.first() {
                        let url = url.clone();
                        println!("Fetching: {}", url);

                        match ctx.deps().http_client.get(&url).send().await {
                            Ok(response) => {
                                let status = response.status().as_u16();

                                if response.status().is_success() {
                                    let content = response.text().await?;
                                    println!("✓ Fetched {} ({} bytes)", url, content.len());

                                    Ok(FetchEvent::Fetched { url, content, status })
                                } else {
                                    println!("✗ Failed {} (HTTP {})", url, status);
                                    Ok(FetchEvent::FetchFailed {
                                        url,
                                        reason: format!("HTTP {}", status),
                                    })
                                }
                            }
                            Err(e) => {
                                println!("✗ Failed {}: {}", url, e);
                                Ok(FetchEvent::FetchFailed {
                                    url,
                                    reason: e.to_string(),
                                })
                            }
                        }
                    } else {
                        // No URLs to fetch - complete
                        let state = ctx.curr_state();
                        Ok(FetchEvent::AllComplete {
                            success_count: state.success_count,
                            failure_count: state.failure_count,
                        })
                    }
                })
        )
        // Effect 2 - Continue fetching after success/failure
        .with_effect(
            effect::on::<FetchEvent>()
                .extract(|e| match e {
                    FetchEvent::Fetched { .. } | FetchEvent::FetchFailed { .. } => Some(()),
                    _ => None,
                })
                .then(|_, ctx: EffectContext<FetchState, Deps>| async move {
                    let state = ctx.curr_state();
                    if !state.urls_to_fetch.is_empty() {
                        Ok(FetchEvent::FetchRequested {
                            urls: state.urls_to_fetch.clone(),
                        })
                    } else {
                        Ok(FetchEvent::AllComplete {
                            success_count: state.success_count,
                            failure_count: state.failure_count,
                        })
                    }
                })
        );

    let urls = vec![
        "https://example.com".to_string(),
        "https://httpbin.org/status/200".to_string(),
        "https://httpbin.org/status/404".to_string(),
    ];

    // Activate with initial state
    let handle = engine.activate(FetchState::default());

    // Run with closure that returns initial event
    handle.run(|_ctx| {
        Ok(FetchEvent::FetchRequested { urls: urls.clone() })
    })?;

    // Wait for all effects to complete
    handle.settled().await?;

    println!("\nAll fetches complete!");

    Ok(())
}
