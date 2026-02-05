# Seesaw Architecture Guidelines

**Mental Model**: Events are signals. Effects react and return new events. That's it.

## Quick Start - v0.7.0 API

### Stateless Engine Pattern

```rust
// 1. Define engine once (stateless, reusable)
let engine = Engine::new()
    .with_effect(effect::on::<OrderPlaced>().then(|event, ctx| async move {
        ctx.deps().mailer.send_confirmation(&event).await?;
        Ok(EmailSent { order_id: event.id })  // Return event to dispatch
    }))
    .with_reducer(reducer::on::<OrderPlaced>().run(|state, event| {
        State { order_count: state.order_count + 1, ..state }
    }));

// 2. Activate with initial state (per-execution)
let handle = engine.activate(State::default());

// 3. Process your logic - return event to dispatch, waits for effects
handle.process(|_ctx| async { Ok(OrderPlaced { id: 123, total: 99.99 }) }).await?;
```

### Edge Function Pattern

```rust
// Engine is stateless - define once, use many times
let engine = Engine::new()
    .with_effect(...)
    .with_reducer(...);

// Edge function returns event to dispatch
fn process_webhook(payload: Webhook) -> OrderPlaced {
    OrderPlaced::from(payload)
}

// Execute per-request
let handle = engine.activate(State::default());
handle.process(|_ctx| async { Ok(process_webhook(payload)) }).await?;
```

### Key Differences from v0.6

- **Effects return events**: Use `.then()` and return `Ok(Event)` instead of `ctx.emit()`
- **`handle.process()` for dispatch**: Async method that dispatches event and waits for effects
- **`ctx.emit()` removed from public API**: Effects and edge functions return events
- **Observer pattern**: Return `Ok(())` to dispatch nothing

---

## What Seesaw Is

Seesaw is an **event-driven runtime** for building reactive systems.

**Core flow**: Event → Reducer → Effect → Event

- **Events** are signals (facts that already happened)
- **Reducers** transform state before effects (pure functions)
- **Effects** react to events, perform IO, return new events

Simple, direct, no ceremony.

### Key Properties

- **Is**: Event-driven runtime
- **Is**: Direct event → effect → event flows
- **Is Not**: Event sourcing, distributed actors, retry engine, saga orchestrator, workflow engine, job queue

## Core Primitives

### Event

A **signal**. Something that already happened. Immutable. Past-tense by convention.

```rust
#[derive(Debug, Clone)]
enum ScrapeEvent {
    SourceRequested { source_id: Uuid },
    SourceScraped { source_id: Uuid, data: String },
    NeedsExtracted { source_id: Uuid },
    AuthorizationDenied { source_id: Uuid },
}
// Event is auto-implemented for Clone + Send + Sync + 'static
```

Events may come from:
- User actions
- Background jobs
- External systems
- Failed attempts
- Other effects

Events are the only signals in the system.

### Effect

Handlers that react to events and return new events.

```rust
// Simple effect - reacts to one event type, returns new event
let scrape_effect = effect::on::<SourceRequested>().then(|event, ctx| async move {
    let data = ctx.deps().scraper.scrape(event.source_id).await?;
    Ok(SourceScraped {
        source_id: event.source_id,
        data
    })
});

// Effect with filter_map - extract data and return event
let priority_effect = effect::on::<OrderPlaced>()
    .filter_map(|event| {
        if event.priority > 5 { Some(event.clone()) } else { None }
    })
    .then(|event, ctx| async move {
        ctx.deps().notify_urgent(&event).await?;
        Ok(UrgentNotified { order_id: event.id })
    });

// Effect with state transition
let status_effect = effect::on::<StatusChanged>()
    .transition(|prev, next| prev.status != next.status)
    .then(|event, ctx| async move {
        ctx.deps().notify_status_change(&event).await?;
        Ok(StatusNotified { id: event.id })
    });

// Observer effect - returns () to dispatch nothing
let logger_effect = effect::on::<OrderPlaced>().then(|event, ctx| async move {
    ctx.deps().logger.log(&event);
    Ok(())  // No event dispatched
});

// Observe ALL events (for logging, metrics, debugging)
let observer_effect = effect::on_any().then(|event, ctx| async move {
    ctx.deps().logger.log(event.type_id);
    if let Some(order) = event.downcast::<OrderPlaced>() {
        ctx.deps().analytics.track("order_placed", order);
    }
    Ok(())
});
```

#### `on!` Macro for Multi-Variant Matching

When handling enum events with multiple variants, the `on!` macro provides concise syntax that mirrors Rust's `match`:

```rust
use seesaw::on;

// Match-like syntax with Event::Variant patterns
let effects = on! {
    // Multiple variants with | - same fields required
    CrawlEvent::WebsiteIngested { website_id, job_id, .. } |
    CrawlEvent::WebsitePostsRegenerated { website_id, job_id, .. } => |ctx| async move {
        ctx.deps().jobs.enqueue(ExtractPostsJob {
            website_id,
            parent_job_id: job_id,
        }).await?;
        Ok(CrawlEvent::ExtractJobEnqueued { website_id })
    },

    // Single variant
    CrawlEvent::PostsExtractedFromPages { website_id, posts, .. } => |ctx| async move {
        ctx.deps().jobs.enqueue(SyncPostsJob { website_id, posts }).await?;
        Ok(CrawlEvent::SyncJobEnqueued { website_id })
    },
};

// Returns Vec<Effect<S, D>> - add to engine
let engine = effects.into_iter().fold(Engine::new(), |e, eff| e.with_effect(eff));
```

#### Effect Execution Configuration (v0.7.6+)

Effects can be configured for retry, timeout, delay, priority, and queued execution:

```rust
// Queued effect with retry and timeout
effect::on::<PaymentRequested>()
    .id("charge_payment")          // Custom ID for tracing/debugging
    .retry(5)                       // Retry up to 5 times on failure
    .timeout(Duration::from_secs(30))  // 30 second timeout
    .priority(1)                    // Higher priority (lower number = higher priority)
    .then(|event, ctx| async move {
        ctx.deps().stripe.charge(&event).await?;
        Ok(PaymentCharged { order_id: event.order_id })
    });

// Delayed execution
effect::on::<OrderPlaced>()
    .delayed(Duration::from_secs(3600))  // Run 1 hour later
    .then(|event, ctx| async move {
        ctx.deps().send_followup_email(&event).await?;
        Ok(FollowupSent { order_id: event.order_id })
    });

// Force queued execution (even without retry/delay/timeout)
effect::on::<AnalyticsEvent>()
    .queued()                       // Execute in background worker
    .then(|event, ctx| async move {
        ctx.deps().analytics.track(&event).await?;
        Ok(())
    });

// Chaining works in any order
effect::on::<OrderPlaced>()
    .filter(|e| e.total > 100.0)    // Filter first
    .retry(3)                        // Then config
    .id("large_orders")
    .priority(1)
    .then(|event, ctx| async move {
        Ok(LargeOrderProcessed { id: event.id })
    });
```

**Execution Modes:**
- **Inline** (default): Runs immediately in same transaction, no retry
- **Queued**: Triggered by `.queued()`, `.delayed()`, `.timeout()`, `.retry() > 1`, or `.priority()`

**Configuration Methods:**
- `.id(String)` - Custom identifier for tracing/debugging
- `.retry(u32)` - Max retry attempts (default: 1 = no retry)
- `.timeout(Duration)` - Execution timeout
- `.delayed(Duration)` - Delay before execution
- `.priority(i32)` - Priority (lower = higher priority)
- `.queued()` - Force queued execution

Each arm generates an effect equivalent to:
```rust
effect::on::<CrawlEvent>()
    .extract(|e| match e {
        CrawlEvent::WebsiteIngested { website_id, job_id, .. } => Some((website_id.clone(), job_id.clone())),
        _ => None,
    })
    .then(|(website_id, job_id), ctx| async move { ... })
```

Effects can:
- Do IO (DB queries, API calls, etc.)
- Make decisions
- Branch on conditions
- Be pure or impure (your choice)
- Return events to dispatch, or `Ok(())` to dispatch nothing
- Filter events with `.extract()` (formerly `.filter_map()`)
- React to state transitions with `.transition()`
- Use `on!` macro for ergonomic multi-variant matching

EffectContext provides:
- `deps()` — shared dependencies
- `prev_state()` — state before reducer ran
- `next_state()` — state after reducer ran
- `curr_state()` — current live state
- `within(closure)` — spawn tracked sub-tasks

### Reducer

Pure state transformations that run before effects.

```rust
// Simple reducer
let scrape_reducer = reducer::on::<SourceScraped>().run(|state, event| {
    State {
        last_scrape: Some(event.data.clone()),
        scrape_count: state.scrape_count + 1,
        ..state
    }
});

// Multiple reducers for different events
let order_reducer = reducer::on::<OrderPlaced>().run(|state, event| {
    State {
        order_count: state.order_count + 1,
        total_revenue: state.total_revenue + event.amount,
        ..state
    }
});

// Reducer that resets state
let reset_reducer = reducer::on::<Reset>().run(|_state, _event| {
    State::default()
});
```

Reducers:
- Are pure functions (no side effects)
- Transform state based on events
- Run before effects see the event
- Multiple reducers chain together
- Provide updated state to effects via `ctx.state()`

## Execution Model

Simple and direct:

```
Event dispatched
  ↓
Reducers transform state
  ↓
All Effects listening to this event
  ↓
Execute (IO, decisions, state checks)
  ↓
Return new Event (or () for none)
  ↓
Repeat
```

**Example**: Scraping pipeline

```rust
ScrapeEvent::SourceRequested
  → ScrapeEffect → scrapes URL → returns SourceScraped { data }
  → ExtractEffect → extracts items → returns DataExtracted { items }
  → SyncEffect → syncs to DB → returns SyncComplete
```

Multiple effects can listen to the same event and run in parallel.

**Example**: Parallel notifications

```rust
UserEvent::SignedUp
  ↓
  ├─→ EmailEffect → sends welcome email → returns EmailSent
  ├─→ SlackEffect → posts to Slack → returns SlackPosted
  └─→ AnalyticsEffect → tracks event → returns ()
```

All three effects run concurrently when `SignedUp` is dispatched.

## Examples

### Example 1: Scraping pipeline

```rust
#[derive(Debug, Clone)]
enum ScrapeEvent {
    SourceRequested { source_id: Uuid },
    SourceScraped { source_id: Uuid, data: String },
    DataExtracted { source_id: Uuid, items: Vec<Item> },
}

let engine = Engine::with_deps(deps)
    // Effect 1: Scrape on request
    .with_effect(
        effect::on::<ScrapeEvent>()
            .filter_map(|e| match e {
                ScrapeEvent::SourceRequested { source_id } => Some(*source_id),
                _ => None,
            })
            .then(|source_id, ctx| async move {
                let data = ctx.deps().scraper.scrape(source_id).await?;
                Ok(ScrapeEvent::SourceScraped { source_id, data })
            })
    )
    // Effect 2: Extract on scrape
    .with_effect(
        effect::on::<ScrapeEvent>()
            .filter_map(|e| match e {
                ScrapeEvent::SourceScraped { source_id, data } => Some((*source_id, data.clone())),
                _ => None,
            })
            .then(|(source_id, data), ctx| async move {
                let items = ctx.deps().extractor.extract(&data).await?;
                Ok(ScrapeEvent::DataExtracted { source_id, items })
            })
    );
```

### Example 2: Notification dispatch

```rust
#[derive(Debug, Clone)]
enum NotificationEvent {
    UserSignedUp { user_id: Uuid, email: String },
    EmailSent { user_id: Uuid, email_id: Uuid },
    SlackPosted { user_id: Uuid, message_id: String },
}

let engine = Engine::with_deps(deps)
    // Email effect
    .with_effect(
        effect::on::<NotificationEvent>()
            .filter_map(|e| match e {
                NotificationEvent::UserSignedUp { user_id, email } => Some((*user_id, email.clone())),
                _ => None,
            })
            .then(|(user_id, email), ctx| async move {
                let email_id = ctx.deps().mailer.send_welcome(email).await?;
                Ok(NotificationEvent::EmailSent { user_id, email_id })
            })
    )
    // Slack effect
    .with_effect(
        effect::on::<NotificationEvent>()
            .filter_map(|e| match e {
                NotificationEvent::UserSignedUp { user_id, .. } => Some(*user_id),
                _ => None,
            })
            .then(|user_id, ctx| async move {
                let msg_id = ctx.deps().slack.post("New signup!").await?;
                Ok(NotificationEvent::SlackPosted { user_id, message_id: msg_id })
            })
    );
```

Both effects run in parallel when `UserSignedUp` is dispatched. No coordination needed.

### Example 3: Conditional event dispatch

```rust
// Effect that conditionally returns different events
effect::on::<ScrapeEvent>()
    .filter_map(|e| match e {
        ScrapeEvent::SourceRequested { source_id } => Some(*source_id),
        _ => None,
    })
    .then(|source_id, ctx| async move {
        // Check rate limit
        if !ctx.deps().rate_limiter.check().await? {
            return Ok(ScrapeEvent::RateLimited { source_id });
        }

        // Do the work
        match ctx.deps().scraper.scrape(source_id).await {
            Ok(data) => Ok(ScrapeEvent::SourceScraped { source_id, data }),
            Err(e) => Ok(ScrapeEvent::ScrapeFailed { source_id, reason: e.to_string() }),
        }
    })
```

Effects return events based on outcomes - success, failure, or rate-limited all flow as events.

## Design Guidelines

### Events close loops

Every long-running workflow should have terminal events:
- Success events (e.g., `DataPublished`, `WorkflowComplete`)
- Failure events (e.g., `ScrapeFailed`, `RateLimited`)

Otherwise you get:
- Permanent "in-flight" state
- Silent deadlocks
- Ghost workflows

### Effects can do anything

Effects are unconstrained. They can:
- Do IO or be pure
- Hold state or be stateless
- Make complex decisions or be simple transforms
- Branch on time, randomness, config

You decide based on your needs.

### Cross-domain listening

Effects can listen to events from any domain and return events from another.

```rust
// Effect listening to WebsiteEvent, returning CrawlEvent
effect::on::<WebsiteEvent>()
    .filter_map(|e| match e {
        WebsiteEvent::WebsiteApproved { website_id } => Some(*website_id),
        _ => None,
    })
    .then(|website_id, ctx| async move {
        ctx.deps().crawler.start(website_id).await?;
        Ok(CrawlEvent::CrawlStarted { website_id })
    })
```

This is normal and correct. Cross-domain coordination happens via events.

## Error Handling Pattern

Effects can handle errors in two ways:

### Preferred: Explicit Failure Events

```rust
// Define explicit failure events
#[derive(Clone)]
enum OrderEvent {
    OrderPlaced { order_id: Uuid, total: f64 },
    PaymentCharged { order_id: Uuid },
    PaymentChargeFailed { order_id: Uuid, reason: String },
    OrderCancelled { order_id: Uuid, reason: String },
}

// Handle failures explicitly
effect::on::<OrderEvent>()
    .extract(|e| match e {
        OrderEvent::OrderPlaced { order_id, total, .. } => Some((*order_id, *total)),
        _ => None,
    })
    .then(|(order_id, total), ctx| async move {
        match ctx.deps().payment.charge(total).await {
            Ok(_) => Ok(OrderEvent::PaymentCharged { order_id }),
            Err(e) => Ok(OrderEvent::PaymentChargeFailed {
                order_id,
                reason: e.to_string(),
            })
        }
    });

// Compensation
effect::on::<OrderEvent>()
    .extract(|e| match e {
        OrderEvent::PaymentChargeFailed { order_id, reason, .. } => Some((*order_id, reason.clone())),
        _ => None,
    })
    .then(|(order_id, reason), ctx| async move {
        if reason.contains("network") {
            // Retry
            Ok(OrderEvent::PaymentRetryScheduled { order_id })
        } else {
            // Give up
            Ok(OrderEvent::OrderCancelled { order_id, reason })
        }
    });
```

### Fallback: EffectError for Ergonomic `?` Usage

```rust
// Use ? for ergonomics
effect::on::<OrderPlaced>().then(|order, ctx| async move {
    ctx.deps().payment.charge(order.total).await?;  // Propagates error
    Ok(PaymentCharged { order_id: order.id })
})

// Generic EffectError handler
effect::on::<EffectError>()
    .filter(|err| {
        // Explicit retry logic
        err.source_event_type == TypeId::of::<OrderPlaced>() &&
        err.error.to_string().contains("timeout")
    })
    .then(|err, ctx| async move {
        // Custom retry logic
        tokio::time::sleep(Duration::from_secs(1)).await;
        // Re-emit original event or emit retry event
        Ok(RetryScheduled)
    });

// Typed error handling
effect::on::<EffectError>()
    .extract(|err| {
        // Filter by source event + error type
        if err.source_event_type == TypeId::of::<OrderPlaced>() {
            err.downcast::<PaymentError>()
                .map(|pe| (err.source_event.downcast_ref::<OrderPlaced>().unwrap().clone(), pe.clone()))
        } else {
            None
        }
    })
    .then(|(order, payment_err), ctx| async move {
        // Domain-specific compensation based on error type
        if payment_err.is_retryable() {
            Ok(PaymentRetryScheduled { order_id: order.id })
        } else {
            Ok(OrderCancelled {
                order_id: order.id,
                reason: payment_err.to_string(),
            })
        }
    });
```

### Guidelines

1. **Use explicit failure events** for critical flows where you need fine-grained control
2. **Use EffectError** for convenience when you just want to use `?`
3. **Write explicit logic** - no magic helpers for "transient" or "should_retry"
4. **Compensation effects should be infallible** - catch errors internally, don't propagate

## Role Matrix

| Role    | React? | Mutate? | Returns | Listen To | Pure? |
| ------- | ------ | ------- | ------- | --------- | ----- |
| Reducer | No     | No      | State   | Events    | Yes   |
| Effect  | Yes    | Yes     | Event   | Events    | No    |

Reducers transform state. Effects do the work and return events.

## What Seesaw Is Not

### ❌ Not a workflow engine
- No DAGs
- No BPMN
- No retries
- No timers

Workflows **emerge** from event sequences.

### ❌ Not CQRS (exactly)

It overlaps, but:
- Seesaw doesn't require read models
- It doesn't enforce command/event segregation at the system level

It's closer to **event-driven decision modeling**.

### ❌ Not a state machine in the classical sense

There are no explicit "states".

State is:
- Implicit
- Derived
- Reconstructable

You don't "enter" a state. You observe that certain events have occurred.

### ❌ Not a framework that forces patterns

Seesaw doesn't force you to use machines or commands.

If your flow is simple, use events and effects.
If you need guards or branching, add machines.

The runtime supports both without special casing.

## Common Pitfalls

### 1. Smuggling volatile data through events

❌ **Bad**:
```rust
Event::UserRequested { user_email: String }  // Email might change!
```

✅ **Better**:
```rust
Event::UserRequested { user_id: Uuid }  // Immutable reference
```

Events should reference facts, not embed data that might change.

### 2. Effects that know too much

If your effect:
- Has dozens of fields
- Mirrors database rows
- Holds authoritative data

You're putting the source of truth in the wrong place. Effects should query deps, not store domain data.

### 3. Missing terminal events

❌ **Bad**:
```rust
WorkflowStarted → ... → (nothing)  // Workflow stuck "in progress" forever
```

✅ **Better**:
```rust
WorkflowStarted → ... → WorkflowComplete
                     ↘ WorkflowFailed
```

Every long-running flow needs success and failure terminal events.

## Engine Usage

```rust
// Define engine once (stateless, reusable)
let engine = Engine::new()
    .with_reducer(reducer::on::<MyEvent>().run(|state, event| {
        // Pure state transformation
        State { count: state.count + 1, ..state }
    }))
    .with_effect(effect::on::<MyEvent>().then(|event, ctx| async move {
        // Side effects - return event to dispatch
        ctx.deps().notify(&event).await?;
        Ok(NextEvent { id: event.id })
    }));

// Execute per-request - return event to dispatch
let handle = engine.activate(State::default());
handle.process(|_ctx| async { Ok(MyEvent::Started { data }) }).await?;
```

Builder methods:
- `.with_reducer(reducer)` — Register pure state transformations
- `.with_effect(effect)` — Register event handlers (use `.then()` to return events)
- `.with_deps(deps)` — Create engine with dependencies
- `.with_effect_registry(registry)` — Use existing effect registry

Handle methods:
- `.process(async_closure).await` — Execute logic, dispatch event, wait for effects to complete
- `.settled().await` — Wait for all effects to complete
- `.cancel()` — Cancel all tasks

## Cross-Domain Reactions

Effects can listen to events from other domains. This is normal and correct.

**Pattern**: Domain A dispatches event → Domain B's effect reacts → Domain B returns its own events

### Example: Website approval triggers crawling

```rust
// Website domain dispatches events
pub enum WebsiteEvent {
    WebsiteApproved { website_id: Uuid },
}

// Crawl effect listens to WebsiteEvent, returns CrawlEvent
effect::on::<WebsiteEvent>()
    .filter_map(|e| match e {
        WebsiteEvent::WebsiteApproved { website_id } => Some(*website_id),
        _ => None,
    })
    .then(|website_id, ctx| async move {
        ctx.deps().crawler.start(website_id).await?;
        Ok(CrawlEvent::CrawlStarted { website_id })
    })
```

**Why this is correct**:
- No domain logic leakage
- No tight coupling (depends on event, not internal state)
- Trivially testable
- Explicit and localized

The runtime broadcasts events. Effects that care, react and return new events. That's it.

## Request/Response Pattern

For edges that need a response:

```rust
use seesaw::{dispatch_request, EnvelopeMatch};

let entry = dispatch_request(
    EntryRequestEvent::Create { ... },
    &bus,
    |m| m.try_match(|e: &EntryEvent| match e {
        EntryEvent::Created { entry } => Some(Ok(entry.clone())),
        _ => None,
    })
    .or_try(|denied: &AuthDenied| Some(Err(anyhow!("denied"))))
    .result()
).await?;
```

## Outbox Pattern

For durable events (external side effects), write to outbox in same transaction:

```rust
let mut tx = ctx.deps().db.begin().await?;
let entity = Entity::create(&cmd, &mut tx).await?;
writer.write_event(&EntityCreated { id }, ctx.outbox_correlation_id()).await?;
tx.commit().await?;
```

## Architecture Flow

```
EventBus → Effect.then(event) → returns Event → EventBus
```

Simple and direct.

## Design Principles Summary

1. **Events are the only signals** — Everything flows through events
2. **Reducers transform state** — Pure functions that run before effects
3. **Effects react to events** — Do IO, return new events
4. **Effects are unconstrained** — Can do anything, pure or impure, stateful or stateless
5. **Events are facts, past-tense** — `UserCreated`, not `CreateUser`
6. **Effects can listen to any domain** — Cross-domain coordination via events
7. **One Effect execution = One transaction** — Multiple atomic writes belong together
8. **Terminal events close loops** — Every workflow needs success/failure events
