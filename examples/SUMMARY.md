# Examples Summary

## What We Built

Instead of building adapter crates, we created practical examples showing how to use standard Rust libraries directly in Causal effects.

### ✅ Created Examples

1. **http-fetcher** (`examples/http-fetcher/`)
   - Shows how to use `reqwest` directly in effects
   - Demonstrates failure modeling (Success vs Failed events)
   - No adapter needed - just use the library

2. **ai-summarizer** (`examples/ai-summarizer/`)
   - Shows how to call Anthropic API directly with `reqwest + serde`
   - No special SDK needed - just HTTP + JSON
   - Demonstrates structured API calls in effects

### ✅ Documentation

- `examples/README.md` - Overview and common patterns
- `examples/EXAMPLES_PHILOSOPHY.md` - Why no adapters, what to do instead
- Each example has inline documentation

## Why No Adapters?

We explored building `causal-http` and `causal-anthropic` adapter crates but decided against it because:

1. **Pointless indirection** - Wrappers add ceremony without value
2. **One size doesn't fit all** - Different use cases need different features
3. **Maintenance burden** - Every API change requires adapter updates
4. **Fights abstraction** - Users forced to work around limitations
5. **Complicates Causal** - Adds concepts without architectural benefit

## The Pattern

**Causal is about architecture, not libraries.**

```rust
// Just use libraries directly in your effects
struct MyEffect;

#[async_trait]
impl Effect<MyCommand, Deps> for MyEffect {
    type Event = MyEvent;

    async fn execute(&self, cmd: MyCommand, ctx: EffectContext<Deps>)
        -> Result<MyEvent>
    {
        // Use any library you want
        let result = ctx.deps().http_client
            .get(&url)
            .send()
            .await?;

        // Return events describing outcomes
        Ok(MyEvent::Fetched { content: result.text().await? })
    }
}
```

## What's Next?

### For Users

1. Copy the patterns from examples
2. Use whatever libraries you want
3. Model failures as events
4. Keep machines pure

### For Contributors

Only build an adapter if:
1. ✅ The same pattern appears in 3+ places
2. ✅ Multiple users are asking for it
3. ✅ It provides genuine value beyond a thin wrapper

Otherwise: just show examples.

## Testing the Examples

```bash
# HTTP Fetcher
cargo run --bin http-fetcher-example

# AI Summarizer (requires API key)
export ANTHROPIC_API_KEY=your-key
cargo run --bin ai-summarizer-example
```

Both examples compile and run successfully.

## Key Takeaway

**Keep Causal simple. Use libraries directly. Show patterns, not abstractions.**
