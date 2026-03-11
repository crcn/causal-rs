# Causal Examples

Practical examples showing how to use standard Rust libraries directly in Causal effects.

## Philosophy

**No adapters. No ceremony. Just clean code.**

These examples show that you don't need special "Causal adapters" to integrate with external services. Just use the standard libraries in your effects:

- Want HTTP? Use `reqwest` directly
- Want AI? Call the API with `reqwest + serde`
- Want database? Use `sqlx` directly
- Want caching? Use `redis` directly

**The pattern is simple:**
1. Put the library client in your `Deps`
2. Use it in your effect via `ctx.deps()`
3. Return events describing outcomes

## Examples

### 1. HTTP Fetcher

**Shows:** Using `reqwest` directly to fetch URLs

```bash
cd examples/http-fetcher
cargo run
```

**Key takeaway:** No adapter needed. Just use `ctx.deps().http_client.get(url).send().await?`

### 2. AI Summarizer

**Shows:** Calling Anthropic API directly with `reqwest + serde`

```bash
cd examples/ai-summarizer
export ANTHROPIC_API_KEY=your-key-here
cargo run
```

**Key takeaway:** No SDK needed. Just make HTTP requests and parse JSON.

### 3. Research Assistant (Coming Soon)

**Shows:** Combining multiple patterns in one application

- HTTP fetching for web scraping
- AI API calls for analysis
- Multiple machines coordinating via events

## Common Patterns

### Pattern 1: HTTP Requests

```rust
struct Deps {
    http_client: reqwest::Client,
}

#[async_trait]
impl Effect<MyCommand, Deps> for MyEffect {
    type Event = MyEvent;

    async fn execute(&self, cmd: MyCommand, ctx: EffectContext<Deps>)
        -> Result<MyEvent>
    {
        let response = ctx.deps().http_client
            .get(&url)
            .send()
            .await?;

        match response.status().is_success() {
            true => Ok(MyEvent::Success { content: response.text().await? }),
            false => Ok(MyEvent::Failed { status: response.status().as_u16() }),
        }
    }
}
```

### Pattern 2: External API Calls

```rust
#[derive(Serialize)]
struct ApiRequest {
    // Your request fields
}

#[derive(Deserialize)]
struct ApiResponse {
    // Response fields
}

async fn call_api(client: &reqwest::Client, request: ApiRequest) -> Result<ApiResponse> {
    let response = client
        .post("https://api.example.com/endpoint")
        .header("authorization", "Bearer token")
        .json(&request)
        .send()
        .await?;

    Ok(response.json().await?)
}

// Use in effect:
let result = call_api(&ctx.deps().http_client, request).await?;
```

### Pattern 3: Failure Modeling

```rust
// Model failures as events, not errors
match external_call().await {
    Ok(result) => Ok(MyEvent::Success { result }),
    Err(e) => Ok(MyEvent::Failed { reason: e.to_string() }),
}

// Let the machine decide what to do about failures
impl Machine for MyMachine {
    fn decide(&mut self, event: &MyEvent) -> Option<MyCommand> {
        match event {
            MyEvent::Failed { reason } => {
                // Retry? Give up? User intervention?
                Some(MyCommand::Retry)
            }
            _ => None,
        }
    }
}
```

### Pattern 4: Rate Limiting

```rust
use governor::{Quota, RateLimiter};
use std::num::NonZeroU32;

struct Deps {
    http_client: reqwest::Client,
    rate_limiter: RateLimiter</* ... */>,
}

// In effect:
ctx.deps().rate_limiter.until_ready().await;
let response = ctx.deps().http_client.get(url).send().await?;
```

### Pattern 5: Retries with Backoff

```rust
use tokio::time::{sleep, Duration};

async fn fetch_with_retry(url: &str, client: &reqwest::Client) -> Result<String> {
    let mut attempts = 0;
    let max_attempts = 3;

    loop {
        match client.get(url).send().await {
            Ok(response) if response.status().is_success() => {
                return Ok(response.text().await?);
            }
            Err(_) if attempts < max_attempts => {
                attempts += 1;
                let delay = Duration::from_secs(2u64.pow(attempts));
                sleep(delay).await;
            }
            Err(e) => return Err(e.into()),
            Ok(response) => {
                bail!("HTTP {}", response.status());
            }
        }
    }
}
```

## When to Build an Adapter

**Don't build adapters speculatively.** Only build one when:

1. ✅ You have the same pattern in 3+ places
2. ✅ Users are asking for it
3. ✅ It provides real value beyond a thin wrapper

**Most of the time, just use the library directly.**

## Running Examples

All examples use the workspace dependencies, so you can run them from the workspace root:

```bash
# Run specific example
cargo run --bin http-fetcher-example

# Or cd into the example directory
cd examples/http-fetcher
cargo run
```

## Adding Your Own Examples

Create a new directory under `examples/` with:
- `Cargo.toml` - Dependencies
- `src/main.rs` - Your example code
- Optional: `README.md` - Specific instructions

Follow the existing examples' structure for consistency.
