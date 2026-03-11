# Examples Philosophy

## Why No Adapters?

We considered building `causal-http`, `causal-anthropic`, and other adapter crates.

**We killed those ideas. Here's why:**

### Problem 1: Pointless Indirection

```rust
// With adapter:
ctx.deps().http.fetch(&url).await?

// Without adapter:
ctx.deps().http_client.get(&url).send().await?
```

We saved 4 characters. Not worth a crate.

### Problem 2: One Size Doesn't Fit All

Every HTTP use case is different:
- Scraping needs JS rendering, cookies, proxies
- APIs need OAuth, pagination, GraphQL
- Each needs different rate limiting

No single adapter can handle all of this.

### Problem 3: Maintenance Burden

Every external API change requires:
- Adapter update
- Testing
- Documentation
- Version coordination

Meanwhile, users of the raw SDK get new features immediately.

### Problem 4: Abstractions You Can't Escape

The moment the adapter doesn't do exactly what you need:
- Fork it (maintenance nightmare)
- Work around it (fighting the abstraction)
- Abandon it (wasted effort)

### Problem 5: It Complicates Causal

Causal's value is conceptual clarity:
- Events (facts)
- Commands (intent)
- Machines (decisions)
- Effects (execution)

Adding "adapter ecosystem with reflexive effects and tool registries" makes it complicated.

## What We Do Instead

**Show patterns, not abstractions.**

### Pattern: HTTP Requests

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
        match ctx.deps().http_client.get(&url).send().await {
            Ok(response) => Ok(MyEvent::Success { content: response.text().await? }),
            Err(e) => Ok(MyEvent::Failed { reason: e.to_string() }),
        }
    }
}
```

That's it. No adapter needed.

### Pattern: External APIs

```rust
async fn call_api(client: &reqwest::Client, request: ApiRequest) -> Result<ApiResponse> {
    client
        .post("https://api.example.com")
        .json(&request)
        .send()
        .await?
        .json()
        .await
}

// Use in effect:
let result = call_api(&ctx.deps().http_client, request).await?;
```

No SDK needed for most APIs. Just HTTP + JSON.

### Pattern: Rate Limiting (If You Need It)

```rust
use governor::{Quota, RateLimiter};

struct Deps {
    http_client: reqwest::Client,
    rate_limiter: RateLimiter</* ... */>,
}

// In effect:
ctx.deps().rate_limiter.until_ready().await;
ctx.deps().http_client.get(url).send().await?;
```

Add it when you need it, not before.

### Pattern: Retries (If You Need Them)

```rust
async fn fetch_with_retry(url: &str, client: &reqwest::Client) -> Result<String> {
    for attempt in 1..=3 {
        match client.get(url).send().await {
            Ok(r) if r.status().is_success() => return Ok(r.text().await?),
            Err(_) if attempt < 3 => {
                tokio::time::sleep(Duration::from_secs(2u64.pow(attempt))).await;
            }
            Err(e) => return Err(e.into()),
            Ok(r) => bail!("HTTP {}", r.status()),
        }
    }
    unreachable!()
}
```

Write it when you need it. Don't cargo-cult it.

## When to Build an Adapter

Only build an adapter when:

1. ✅ The same pattern appears in 3+ places
2. ✅ Multiple users are asking for it
3. ✅ It provides genuine value beyond a thin wrapper
4. ✅ The abstraction won't be limiting

**Most of the time: don't build an adapter.**

## The Real Pattern

**Causal is about architecture, not libraries.**

The architecture is:
```
Events → Machine → Commands → Effect → Events
```

What libraries you use in your effects? **That's your choice.**

- Want `reqwest`? Use it.
- Want `hyper`? Use it.
- Want `surf`? Use it.

**Causal doesn't care.** It just provides the architecture.

## Examples Structure

Each example shows:
1. How to use a standard library
2. Common patterns (retry, rate limiting, etc.)
3. How to model failures as events
4. How to keep machines pure

No ceremony. No special adapters. Just clean code.

---

**Keep Causal simple. Use libraries directly.**
