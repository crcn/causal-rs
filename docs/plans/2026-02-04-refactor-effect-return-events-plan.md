---
title: "refactor: Deprecate ctx.emit() in favor of returned events from effects"
type: refactor
date: 2026-02-04
---

# Deprecate ctx.emit() in Favor of Returned Events

## Overview

Redesign the effect API to return events instead of calling `ctx.emit()`. This is a more opinionated but safer approach that encourages single-responsibility effects ("one function = one fact") and makes event flow explicit in type signatures.

**Current API:**
```rust
effect::on::<OrderPlaced>().run(|event, ctx| async move {
    ctx.deps().mailer.send(&event).await?;
    ctx.emit(EmailSent { order_id: event.id });  // Side effect emission
    Ok(())
})
```

**Proposed API:**
```rust
effect::on::<OrderPlaced>().then(|event, ctx| async move {
    ctx.deps().mailer.send(&event).await?;
    Ok(EmailSent { order_id: event.id })  // Return the event
})
```

Reads as: *"On OrderPlaced, then EmailSent"*

## Problem Statement / Motivation

### Why Change?

1. **Implicit event flow**: With `ctx.emit()`, event causation is hidden in implementation details. You can't see what events an effect produces without reading its body.

2. **Multiple emissions encourage god-effects**: Effects can emit any number of events anywhere in their body, leading to effects that do too much.

3. **Type safety gap**: `ctx.emit()` accepts any event type with no compile-time relationship to the input event. The type system doesn't help reason about event chains.

4. **Testing friction**: Testing what events an effect produces requires capturing side effects rather than asserting on return values.

### Benefits of Return-Based Design

1. **Explicit contracts**: Effect return types declare exactly what events flow through.

2. **Single responsibility**: Returning one event naturally guides developers toward focused effects that do one thing.

3. **Chain visibility**: Event chains become visible in type signatures, making the system easier to reason about.

4. **Simpler testing**: Assert on return values instead of capturing side effects.

5. **Safer refactoring**: Compiler catches when effect chains break.

## Proposed Solution

### Core API Change

Effects use `.then()` and return the next event. Output type is inferred:

```rust
// Basic effect - input type explicit, output inferred from return
effect::on::<OrderPlaced>().then(|event, ctx| async move {
    ctx.deps().mailer.send(&event).await?;
    Ok(EmailSent { order_id: event.id })
})
```

### Design Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Return type | `anyhow::Result<Event>` | Supports `?` with any error type via `Into<anyhow::Error>` |
| Method name | `.then()` | Reads naturally: "on A, then B" |
| Generic syntax | `on::<E>()` single generic | Output type inferred from `.then()` return |
| Errors as events | Failure variants in event enums | Errors are facts too, flow through system |
| No "no event" case | Terminal events have no effect | If no event needed, don't register an effect |
| Conditional outputs | Return different enum variants | Domain event enums include success AND failure |
| Observer effects | `on_any().run()` returns `anyhow::Result<()>` | Observers observe, they don't produce |
| Infrastructure errors | Use `?` operator, returns `Err` | Rare, for true panics (DB down, OOM) |
| Context | Keep `EffectContext` | `deps()` and `state()` still needed |

### Key Insight: `anyhow::Result` Accepts Any Error

Using `anyhow::Result<Event>` means the `?` operator works with any error type that implements `std::error::Error`:

```rust
effect::on::<CrawlEvent>()
    .filter_map(...)
    .then(|(website_id, job_id), ctx| async move {
        // All these work with ? - any error type
        let data = ctx.deps().db.fetch(website_id).await?;      // sqlx::Error
        let parsed = serde_json::from_str(&data)?;               // serde_json::Error
        let response = ctx.deps().http.get(url).await?;          // reqwest::Error

        Ok(CrawlEvent::Processed { ... })
    })
```

### Key Insight: Errors Are Events

With thiserror, failure becomes a first-class event:

```rust
#[derive(Debug, Clone)]
pub enum CrawlEvent {
    // Success facts
    PostsExtracted { website_id: WebsiteId, posts: Vec<Post> },
    PostsSynced { website_id: WebsiteId, count: usize },

    // Failure facts (these are ALSO events!)
    ExtractionFailed { website_id: WebsiteId, reason: String },
    SyncFailed { website_id: WebsiteId, reason: String },
}

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),

    #[error("Rate limited after {attempts} attempts")]
    RateLimited { attempts: u32 },
}

// Effect always produces an event - success OR failure
effect::on::<CrawlEvent>()
    .filter_map(|e| match e {
        CrawlEvent::PostsExtracted { website_id, posts } => Some((*website_id, posts.clone())),
        _ => None,
    })
    .then(|(website_id, posts), ctx| async move {
        match sync_posts(website_id, &posts, ctx.deps()).await {
            Ok(result) => Ok(CrawlEvent::PostsSynced {
                website_id,
                count: result.count
            }),
            Err(e) => Ok(CrawlEvent::SyncFailed {
                website_id,
                reason: e.to_string()  // thiserror provides nice message
            }),
        }
    })
```

### Real-World Validation

Validated against two production codebases (mntogether, shay):

**Finding 1: Input and output are usually the SAME domain enum**
```rust
// Effects filter to variant, return different variant of same enum
effect::on::<WebsiteApprovalEvent>()
    .filter_map(...)
    .then(|data, ctx| async move { Ok(WebsiteApprovalEvent::NextStep { ... }) })
```

**Finding 2: Terminal events simply have no effect**
```rust
// WebsiteAssessmentCompleted is terminal - no effect listens to it
// Nothing to write! The event flows in and stops.
```

**Finding 3: Conditional branching returns different variants**
```rust
// Agent loop - every code path produces an event
Ok(if let Some(tool) = state.next_pending_tool() {
    AgentEvent::ToolRequested { ... }
} else if state.done {
    AgentEvent::Completed { ... }
} else {
    AgentEvent::Waiting { turn: state.turn }  // Previously implicit "no event"
})
```

### API Patterns

#### Pattern 1: Basic Effect with `.then()`

```rust
effect::on::<OrderPlaced>().then(|event, ctx| async move {
    ctx.deps().mailer.send(&event).await?;
    Ok(EmailSent { order_id: event.id })
})
```

#### Pattern 2: Domain Event Cascade (Most Common)

Real-world effects typically use a single domain event enum for both input and output:

```rust
// Website approval cascade - filter to variant, return different variant
effect::on::<WebsiteApprovalEvent>()
    .filter_map(|e| match e {
        WebsiteApprovalEvent::ResearchCreated { research_id, website_id, job_id, .. } => {
            Some((*research_id, *website_id, *job_id))
        }
        _ => None,
    })
    .then(|(research_id, website_id, job_id), ctx| async move {
        let result = conduct_searches(research_id, website_id, ctx.deps()).await?;
        Ok(WebsiteApprovalEvent::SearchesCompleted {
            research_id,
            website_id,
            job_id,
            total_queries: result.total_queries,
        })
    })
```

#### Pattern 3: Terminal Events (No Effect)

```rust
// Terminal events have NO effect registered - they just flow through and stop
// WebsiteAssessmentCompleted, PostsSynced, etc. - nothing to write!
```

#### Pattern 4: Success/Failure Branching

```rust
// Every code path returns an event - success OR failure
effect::on::<PaymentAttempted>().then(|event, ctx| async move {
    match ctx.deps().stripe.charge(&event).await {
        Ok(receipt) => Ok(PaymentSucceeded {
            order_id: event.order_id,
            receipt_id: receipt.id
        }),
        Err(e) => Ok(PaymentFailed {
            order_id: event.order_id,
            reason: e.to_string()
        }),
    }
})
```

#### Pattern 5: Conditional State-Based Branching

```rust
// Agent loop - every code path produces an event
effect::on::<AgentEvent>()
    .filter_map(|e| match e {
        AgentEvent::LlmResponded { .. } => Some(()),
        _ => None,
    })
    .then(|_, ctx| async move {
        let state = ctx.next_state();

        Ok(if let Some(tool) = state.next_pending_tool() {
            AgentEvent::ToolRequested {
                tool_call_id: tool.id.clone(),
                tool_name: tool.function.name.clone(),
                arguments: tool.function.arguments.clone(),
            }
        } else if state.done {
            AgentEvent::Completed {
                text: state.last_response_text.clone().unwrap_or_default(),
            }
        } else {
            AgentEvent::Waiting { turn: state.turn }
        })
    })
```

#### Pattern 6: Observer Effects (No Output)

```rust
// on_any() uses .run() and returns () - observers don't produce events
effect::on_any().run(|event, ctx| async move {
    ctx.deps().metrics.increment("events_processed");
    Ok(())  // anyhow::Result<()>
})
```

#### Pattern 7: With Filter

```rust
effect::on::<OrderEvent>()
    .filter_map(|e| match e {
        OrderEvent::Placed { order_id, total, .. } if *total > 10_000.0 => {
            Some((*order_id, *total))
        }
        _ => None,
    })
    .then(|(order_id, total), ctx| async move {
        ctx.deps().alerts.send_high_value_order(order_id, total).await?;
        Ok(OrderEvent::PriorityAlertSent { order_id })
    })
```

#### Pattern 8: With State Transition

```rust
effect::on::<StatusEvent>()
    .filter_map(|e| match e {
        StatusEvent::Changed { entity_id, .. } => Some(*entity_id),
        _ => None,
    })
    .transition(|prev, next| prev.status != next.status)
    .then(|entity_id, ctx| async move {
        ctx.deps().notify.send_status_change(entity_id).await?;
        Ok(StatusEvent::NotificationSent { entity_id })
    })
```

#### Pattern 9: Infrastructure Errors (Rare)

```rust
// Use ? for true infrastructure failures - these break the chain intentionally
effect::on::<CrawlEvent>()
    .filter_map(...)
    .then(|(website_id, job_id), ctx| async move {
        // ? propagates infrastructure errors (DB down, network failure)
        let data = ctx.deps().db.fetch(website_id).await?;

        // Business failures become events, not errors
        match process(data).await {
            Ok(result) => Ok(CrawlEvent::Processed { ... }),
            Err(e) => Ok(CrawlEvent::ProcessingFailed { reason: e.to_string() }),
        }
    })
```

## Technical Approach

### Architecture

#### Current Internal Structure

```
Effect<S, D> {
    can_handle: Fn(TypeId) -> bool,
    handler: Fn(Arc<dyn Any>, TypeId, EffectContext<S, D>) -> BoxFuture<Result<()>>,
}
```

#### Proposed Internal Structure

```
Effect<S, D> {
    input_type: TypeId,
    output_type: TypeId,  // NEW: Track output type
    can_handle: Fn(TypeId) -> bool,
    handler: Fn(Arc<dyn Any>, TypeId, EffectContext<S, D>) -> BoxFuture<Result<EventOutput>>,
}

struct EventOutput {
    type_id: TypeId,
    value: Arc<dyn Any + Send + Sync>,
}
```

#### Event Flow Changes

**Current:**
```
Event → Reducer → Effect.handle() → [ctx.emit() called N times] → Events dispatched during execution
```

**Proposed:**
```
Event → Reducer → Effect.then() → Result<Event> → Engine dispatches returned event
```

**Key difference:** Effects always produce exactly one event (success or failure). No "no event" case - terminal events simply have no effect registered.

### Implementation Phases

#### Phase 1: Foundation (Core Types)

**Tasks:**
- [ ] Add `EventOutput` struct to `effect/types.rs`
- [ ] Create `Event` marker trait (if not exists) for output type bounds
- [ ] Add `output_type: TypeId` field to `Effect<S, D>` struct
- [ ] Keep `EffectContext` but plan to deprecate `emit()` method

**Files:**
- `crates/seesaw/src/effect/types.rs`
- `crates/seesaw/src/effect/context.rs`

**Success Criteria:**
- New types compile
- Existing tests still pass (no behavior change yet)

#### Phase 2: Builder API

**Tasks:**
- [ ] Add `.then()` method to `EffectBuilder`
- [ ] `.then()` accepts `Fn(...) -> Future<Output = anyhow::Result<E>>`
- [ ] Output type inferred from closure return (no second generic on `on::`)
- [ ] Deprecate `.run()` on typed effects (keep only for `on_any()` observer pattern)
- [ ] `on_any().run()` returns `anyhow::Result<()>` (observers don't produce events)

**Files:**
- `crates/seesaw/src/effect/builders.rs`
- `crates/seesaw/src/effect/mod.rs`

**New Builder Methods:**
```rust
impl<E, F, T, S> EffectBuilder<Typed<E>, F, T, S> {
    /// Handler returns anyhow::Result<O> - the next event in the chain
    /// Output type O is inferred from the closure return
    /// Reads as: "on Event, then NextEvent"
    pub fn then<H, Fut, O>(self, handler: H) -> Effect<State, Deps>
    where
        H: Fn(ExtractedData, EffectContext<State, Deps>) -> Fut,
        Fut: Future<Output = anyhow::Result<O>>,
        O: Send + Sync + 'static,  // Output type inferred, no explicit generic needed
}
```

**Success Criteria:**
- New builder compiles
- Can create effects with `effect::on::<E>().filter_map(...).then(...)`
- `anyhow::Result<Event>` return type supports `?` with any error type
- Output type inferred from closure - no turbofish needed

#### Phase 3: Engine Integration

**Tasks:**
- [ ] Update `effect_registry.rs` to handle `EventOutput` returns
- [ ] Modify effect loop to dispatch returned events
- [ ] Ensure returned events go through reducers before next effects
- [ ] Update causation/origin tracking for returned events

**Files:**
- `crates/seesaw/src/effect_registry.rs`
- `crates/seesaw/src/engine.rs`

**Key Change in Effect Loop:**
```rust
// Before
while let Some(envelope) = rx.recv().await {
    effect.handler(event, type_id, ctx).await?;  // Returns ()
}

// After
while let Some(envelope) = rx.recv().await {
    let output = effect.handler(event, type_id, ctx).await?;
    engine_emitter.emit(output.type_id, output.value, ctx);
}
```

**Success Criteria:**
- Returned events dispatch correctly
- Event chains work (A → Effect → B → Effect → C)
- Parallel effects all return and dispatch correctly
- Failure events (e.g., `SyncFailed`) flow through system like success events

#### Phase 4: Migration Support

**Tasks:**
- [ ] Deprecate `ctx.emit()` with compiler warning
- [ ] Deprecate `.run()` on typed effects (keep for `on_any()` only)
- [ ] Add migration guide to documentation
- [ ] Update all examples in `CLAUDE.md`
- [ ] Update all tests to new API

**Files:**
- `crates/seesaw/src/effect/context.rs` (deprecation attribute)
- `crates/seesaw/src/effect/builders.rs` (deprecation attribute)
- `CLAUDE.md`
- All test files

**Deprecation Approach:**
```rust
impl<S, D> EffectContext<S, D> {
    #[deprecated(since = "0.7.0", note = "Use effect return values instead of ctx.emit()")]
    pub fn emit<E: Send + Sync + 'static>(&self, event: E) {
        // Existing implementation - still works during migration
    }
}
```

**Success Criteria:**
- Deprecation warnings appear when using old API
- Both APIs work during migration period
- Clear compiler messages guide users to new API

#### Phase 5: Cleanup and Polish

**Tasks:**
- [ ] Remove `ctx.within()` event emission capability (background tasks can't emit)
- [ ] Add comprehensive documentation
- [ ] Performance benchmarks (ensure no regression)
- [ ] Update `API_MIGRATION.md` with v0.7.0 changes

**Files:**
- `crates/seesaw/src/effect/context.rs`
- `API_MIGRATION.md`
- `README.md`

**Success Criteria:**
- Clean API with no legacy methods (except deprecated)
- Documentation complete
- No performance regression

## Alternative Approaches Considered

### Alternative 1: Keep ctx.emit() but Add Return Option

```rust
// Could return OR emit
effect::on::<A>().run(|event, ctx| async move {
    ctx.emit(B);  // Still works
    Ok(())
})

// Or use returns
effect::on::<A>().then(|event, ctx| async move {
    Ok(B)
})
```

**Rejected because:** Doesn't solve the core problem - effects can still emit arbitrarily. Two ways to do the same thing creates confusion.

### Alternative 2: Return Vec<Event>

```rust
effect::on::<A>().then(|event, ctx| async move {
    Ok(vec![B, C, D])  // Return multiple events
})
```

**Rejected because:** Undermines the "one function = one fact" philosophy. Effects that need multiple outputs should be split.

### Alternative 3: Trait-Based Effects with Associated Types

```rust
trait Effect {
    type Input: Event;
    type Output: Event;

    async fn handle(&self, event: Self::Input, ctx: EffectContext) -> anyhow::Result<Self::Output>;
}
```

**Rejected because:** More boilerplate than closures. The current closure-based API was chosen specifically to avoid trait ceremony. Can be added later if needed.

### Alternative 4: Channel-Based Emission

```rust
effect::on::<A>().run(|event, ctx, tx| async move {
    tx.send(B).await;
    tx.send(C).await;
    Ok(())
})
```

**Rejected because:** Reintroduces the problem of arbitrary emission. Also adds complexity with channels.

### Alternative 5: Two-Generic Syntax `on::<Input, Output>()`

```rust
effect::on::<OrderPlaced, EmailSent>().then(|event, ctx| async move {
    Ok(EmailSent { order_id: event.id })
})
```

**Rejected because:** Verbose and redundant. The output type can be inferred from the `.then()` closure return. Single generic `on::<E>()` is cleaner.

## Acceptance Criteria

### Functional Requirements

- [ ] Effects use `.then()` method: `effect::on::<E>().then(...)`
- [ ] Effects return `anyhow::Result<Event>` instead of calling `ctx.emit()`
- [ ] Output type inferred from closure return (single generic `on::<E>()`)
- [ ] `?` operator works with any error type via `Into<anyhow::Error>`
- [ ] Returned events are dispatched through the normal event flow (reducers → effects)
- [ ] Failure events (e.g., `PaymentFailed`) flow through system like success events
- [ ] `on_any().run()` returns `anyhow::Result<()>` and cannot produce events
- [ ] Effects can return different enum variants for conditional outcomes
- [ ] Terminal events simply have no effect registered (nothing to implement)
- [ ] Filtered effects that don't match don't run at all
- [ ] `ctx.deps()` and `ctx.state()` remain available

### Non-Functional Requirements

- [ ] No performance regression (benchmark effect throughput)
- [ ] Output type inference works - no turbofish needed
- [ ] Compiler errors are clear when types don't match
- [ ] Deprecation warnings guide users to new API
- [ ] thiserror integration documented for failure events

### Quality Gates

- [ ] All existing tests pass (or are updated)
- [ ] New tests cover `.then()` effects
- [ ] New tests cover success/failure event branching
- [ ] New tests cover `on_any()` observer pattern
- [ ] New tests cover `?` operator for infrastructure errors
- [ ] Documentation updated (`CLAUDE.md`, `README.md`)
- [ ] Migration guide written

## Success Metrics

1. **API Clarity**: Effect signatures show input→output relationship at a glance
2. **Test Simplicity**: Tests assert on return values instead of capturing side effects
3. **Adoption**: All internal examples converted to new API
4. **Type Safety**: Compiler catches mismatched event chains

## Dependencies & Prerequisites

- No external dependencies
- Requires Rust stable (async traits are stable as of 1.75)
- Should be released as minor version (0.7.0) since old API still works with deprecation

## Risk Analysis & Mitigation

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Breaking existing user code | High | Medium | Deprecation period with both APIs working |
| Performance regression from extra dispatch step | Low | Medium | Benchmark before/after, optimize if needed |
| Type inference issues | Medium | Low | Provide explicit turbofish examples in docs |
| Users work around single-event limit | Medium | Low | Document patterns for splitting effects |
| Complex outcome enums become unwieldy | Low | Low | Document best practices, suggest splitting |

## Resource Requirements

- **Effort**: ~3-5 days of focused work
- **Testing**: Comprehensive test suite updates
- **Documentation**: CLAUDE.md, README, API_MIGRATION.md updates

## Compatibility Notes

### effect::group() Still Works

Shay uses `effect::group([...])` to combine multiple effects into one. This pattern continues to work because:
- Each effect in the group handles different event variants (via `filter_map`)
- Each effect returns its own `Option<Event>`
- The group dispatches whichever effect matches

```rust
// This pattern remains valid
pub fn agent_effects() -> Effect<AgentState, AgentDeps> {
    effect::group([
        input_received_effect(),   // handles InputReceived, returns Option<AgentEvent>
        llm_responded_effect(),    // handles LlmResponded, returns Option<AgentEvent>
        tool_requested_effect(),   // handles ToolRequested, returns Option<AgentEvent>
    ])
}
```

## Future Considerations

1. **Proc macro for outcome enums**: Could generate dispatch logic for user enums
2. **Effect composition**: Chain effects declaratively (future API TBD)
3. **Typed event graphs**: Generate documentation/diagrams from effect signatures
4. **Compile-time chain validation**: Ensure all events have handlers

## Real-World Migration Examples

### Example 1: MNTogether Website Approval Effect

**Before (ctx.emit):**
```rust
// packages/server/src/domains/website_approval/effects/mod.rs
effect::on::<WebsiteApprovalEvent>().run(|event, ctx| async move {
    match event.as_ref() {
        WebsiteApprovalEvent::WebsiteResearchCreated { research_id, website_id, job_id, requested_by, .. } => {
            handle_conduct_searches(*research_id, *website_id, *job_id, *requested_by, &ctx).await
        }
        // Terminal events - no cascade
        WebsiteApprovalEvent::WebsiteAssessmentCompleted { .. } => Ok(()),
    }
})

// packages/server/src/domains/website_approval/effects/search.rs
pub async fn handle_conduct_searches(..., ctx: &EffectContext<...>) -> Result<()> {
    let result = actions::conduct_searches(research_id, website_id, ctx).await?;
    ctx.emit(WebsiteApprovalEvent::ResearchSearchesCompleted { ... });
    Ok(())
}
```

**After (.then):**
```rust
// Cascade effect - returns the next event
effect::on::<WebsiteApprovalEvent>()
    .filter_map(|e| match e {
        WebsiteApprovalEvent::WebsiteResearchCreated { research_id, website_id, job_id, requested_by, .. } => {
            Some((*research_id, *website_id, *job_id, *requested_by))
        }
        _ => None,
    })
    .then(|(research_id, website_id, job_id, requested_by), ctx| async move {
        let result = actions::conduct_searches(research_id, website_id, ctx.deps()).await?;
        Ok(WebsiteApprovalEvent::ResearchSearchesCompleted {
            research_id,
            website_id,
            job_id,
            total_queries: result.total_queries,
            total_results: result.total_results,
            requested_by,
        })
    })

// Terminal events - NO CODE NEEDED! Just don't register an effect.
// WebsiteAssessmentCompleted flows in and stops. Nothing to write.
```

### Example 2: Shay Agent Loop (Conditional Branching)

**Before (ctx.emit with branching):**
```rust
// packages/valet-rs/src/openai/agent_effects_new.rs
effect::on::<AgentEvent>()
    .filter_map(|event| match event {
        AgentEvent::LlmResponded { .. } => Some(()),
        _ => None,
    })
    .run(|_unit, ctx| async move {
        let state = ctx.next_state();

        if let Some(tool) = state.next_pending_tool() {
            ctx.emit(AgentEvent::ToolRequested { ... });
        } else if state.done {
            ctx.emit(AgentEvent::Completed { ... });
        }
        // Implicit: else emit nothing

        Ok(())
    })
```

**After (.then with explicit branches):**
```rust
effect::on::<AgentEvent>()
    .filter_map(|event| match event {
        AgentEvent::LlmResponded { .. } => Some(()),
        _ => None,
    })
    .then(|_, ctx| async move {
        let state = ctx.next_state();

        // Every code path produces an event - no implicit "nothing"
        Ok(if let Some(tool) = state.next_pending_tool() {
            AgentEvent::ToolRequested {
                tool_call_id: tool.id.clone(),
                tool_name: tool.function.name.clone(),
                arguments: serde_json::from_str(&tool.function.arguments).unwrap_or_default(),
            }
        } else if state.done {
            AgentEvent::Completed {
                text: state.last_response_text.clone().unwrap_or_default(),
                turn_id: new_turn_id("openai"),
            }
        } else {
            AgentEvent::Waiting { turn: state.turn }  // New variant - make implicit explicit!
        })
    })
```

### Example 3: Error Handling with thiserror

**Before (errors break the chain):**
```rust
effect::on::<PaymentEvent>().run(|event, ctx| async move {
    match ctx.deps().stripe.charge(&event).await {
        Ok(receipt) => {
            ctx.emit(PaymentEvent::Succeeded { ... });
        }
        Err(e) => {
            // Error breaks the chain - downstream effects don't see it
            return Err(e.into());
        }
    }
    Ok(())
})
```

**After (errors become events):**
```rust
#[derive(Debug, thiserror::Error)]
pub enum PaymentError {
    #[error("Card declined: {reason}")]
    Declined { reason: String },

    #[error("Network timeout after {attempts} attempts")]
    Timeout { attempts: u32 },
}

effect::on::<PaymentEvent>()
    .filter_map(|e| match e {
        PaymentEvent::Attempted { order_id, amount } => Some((*order_id, *amount)),
        _ => None,
    })
    .then(|(order_id, amount), ctx| async move {
        match ctx.deps().stripe.charge(order_id, amount).await {
            Ok(receipt) => Ok(PaymentEvent::Succeeded {
                order_id,
                receipt_id: receipt.id,
            }),
            Err(e) => Ok(PaymentEvent::Failed {
                order_id,
                reason: e.to_string(),  // thiserror provides nice message
            }),
        }
    })
```

### Migration Benefits

1. **No terminal effect code**: Terminal events have no effect - nothing to write
2. **Errors are observable**: `PaymentFailed` flows through system, can trigger notifications
3. **Explicit code paths**: Every branch must return an event - compiler catches missing cases
4. **thiserror integration**: Error types provide nice `.to_string()` for failure event reasons
5. **Simpler functions**: Helpers return events, don't need ctx for emitting

## Documentation Plan

- [ ] Update `CLAUDE.md` Quick Start with new API
- [ ] Update all examples in `CLAUDE.md`
- [ ] Add "Why Return-Based Effects?" section to README
- [ ] Create `API_MIGRATION.md` section for v0.7.0
- [ ] Add inline documentation to new builder methods

## References & Research

### Internal References

- Effect types: `crates/seesaw/src/effect/types.rs`
- Effect builders: `crates/seesaw/src/effect/builders.rs`
- Effect context: `crates/seesaw/src/effect/context.rs`
- Effect registry: `crates/seesaw/src/effect_registry.rs`
- Engine: `crates/seesaw/src/engine.rs`
- Current API migration: `API_MIGRATION.md`

### Design Principles

From `CLAUDE.md`:
> **Mental Model**: Events are signals. Effects react and emit new events. That's it.

This redesign preserves the mental model while making the "emit new events" part explicit in return types rather than side effects.

### Related Patterns

- **Elm Architecture**: Update functions return (model, Cmd) - explicit about side effects
- **Redux Reducers**: Pure functions that return new state
- **Functional Reactive Programming**: Explicit data flow through function composition
