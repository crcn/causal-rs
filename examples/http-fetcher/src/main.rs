//! HTTP Fetcher Example

use anyhow::Result;
use causal::{event, events, handler, Context, Engine};
use serde::{Deserialize, Serialize};

#[event(prefix = "fetch")]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
enum FetchEvent {
    FetchRequested {
        urls: Vec<String>,
        success_count: usize,
        failure_count: usize,
    },
    Fetched {
        url: String,
        status: u16,
    },
    FetchFailed {
        url: String,
        reason: String,
    },
    AllComplete {
        success_count: usize,
        failure_count: usize,
    },
}

#[derive(Clone)]
struct Deps {
    http_client: reqwest::Client,
}

#[tokio::main]
async fn main() -> Result<()> {
    let deps = Deps {
        http_client: reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()?,
    };

    let engine = Engine::in_memory(deps)
        .with_handler(
            handler::on::<FetchEvent>()
                .id("fetch_url")
                .extract(|e| match e {
                    FetchEvent::FetchRequested {
                        urls,
                        success_count,
                        failure_count,
                    } => Some((urls.clone(), *success_count, *failure_count)),
                    _ => None,
                })
                .then(
                    |(urls, success_count, failure_count), ctx: Context<Deps>| async move {
                        if let Some((url, rest)) = urls.split_first() {
                            let url = url.clone();
                            let rest = rest.to_vec();

                            match ctx.deps().http_client.get(&url).send().await {
                                Ok(response) if response.status().is_success() => {
                                    let status = response.status().as_u16();
                                    let _ = response.text().await?;
                                    Ok(events![
                                        FetchEvent::Fetched { url, status },
                                        FetchEvent::FetchRequested {
                                            urls: rest,
                                            success_count: success_count + 1,
                                            failure_count,
                                        },
                                    ])
                                }
                                Ok(response) => Ok(events![
                                    FetchEvent::FetchFailed {
                                        url,
                                        reason: format!("HTTP {}", response.status().as_u16()),
                                    },
                                    FetchEvent::FetchRequested {
                                        urls: rest,
                                        success_count,
                                        failure_count: failure_count + 1,
                                    },
                                ]),
                                Err(error) => Ok(events![
                                    FetchEvent::FetchFailed {
                                        url,
                                        reason: error.to_string(),
                                    },
                                    FetchEvent::FetchRequested {
                                        urls: rest,
                                        success_count,
                                        failure_count: failure_count + 1,
                                    },
                                ]),
                            }
                        } else {
                            Ok(events![FetchEvent::AllComplete {
                                success_count,
                                failure_count,
                            }])
                        }
                    },
                ),
        )
        .with_handler(
            handler::on::<FetchEvent>()
                .id("all_complete")
                .extract(|e| match e {
                    FetchEvent::AllComplete {
                        success_count,
                        failure_count,
                    } => Some((*success_count, *failure_count)),
                    _ => None,
                })
                .then(|(ok, fail), _ctx: Context<Deps>| async move {
                    println!("all fetches complete: ok={}, fail={}", ok, fail);
                    Ok(events![])
                }),
        );

    let urls = vec![
        "https://example.com".to_string(),
        "https://httpbin.org/status/200".to_string(),
        "https://httpbin.org/status/404".to_string(),
    ];

    engine
        .emit(FetchEvent::FetchRequested {
            urls,
            success_count: 0,
            failure_count: 0,
        })
        .settled()
        .await?;

    Ok(())
}
