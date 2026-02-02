# Seesaw Architecture Guidelines

**Mental Model**: Events are signals. Effects react and emit new events. That's it.

## What Seesaw Is

Seesaw is an **event-driven runtime** for building reactive systems.

**Core flow**: Event → Effect → Event

- **Events** are signals (facts that already happened)
- **Effects** react to events, perform IO, emit new events

Simple, direct, no ceremony.

### Key Properties

- **Is**: Event-driven runtime
- **Is**: Direct event → effect → event flows
- **Is Not**: Event sourcing, distributed actors, retry engine, saga orchestrator, workflow engine

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

Handlers that react to events. Execute IO, emit new events.

```rust
#[async_trait]
impl Effect<ScrapeEvent, Deps> for ScrapeEffect {
    type Event = ScrapeEvent;

    async fn execute(&self, event: ScrapeEvent, ctx: EffectContext<Deps>) -> Result<ScrapeEvent> {
        match event {
            ScrapeEvent::SourceRequested { source_id } => {
                let data = ctx.deps().scraper.scrape(source_id).await?;
                Ok(ScrapeEvent::SourceScraped { source_id, data })
            }
            ScrapeEvent::SourceScraped { source_id, data } => {
                let items = ctx.deps().extractor.extract(&data).await?;
                Ok(ScrapeEvent::NeedsExtracted { source_id })
            }
            _ => bail!("unhandled event")
        }
    }
}
```

Effects can:
- Do IO (DB queries, API calls, etc.)
- Hold state (if needed)
- Make decisions
- Branch on conditions
- Be pure or impure (your choice)

EffectContext provides:
- `deps()` — shared dependencies
- `signal(event)` — fire-and-forget UI notifications
- `tool_context()` — context for interactive tool execution
- `outbox_correlation_id()` — for outbox writes
- `correlation_id()` — get correlation ID for this execution

### EventTap

Observe committed facts after effects. No decisions, no mutations, no emit.

```rust
#[async_trait]
impl EventTap<ScrapeEvent> for ScrapeTap {
    async fn on_event(&self, event: &ScrapeEvent, ctx: &TapContext) -> Result<()> {
        // Publish to NATS, webhooks, metrics, audit logging
        match event {
            ScrapeEvent::SourceScraped { source_id, .. } => {
                ctx.nats.publish("scrapes", source_id).await?;
            }
            _ => {}
        }
        Ok(())
    }
}
```

## Execution Model

Simple and direct:

```
Event emitted
  ↓
All Effects listening to this event
  ↓
Execute (IO, decisions, state checks)
  ↓
Emit new Event(s)
  ↓
Repeat
```

**Example**: Scraping pipeline

```rust
ScrapeEvent::SourceRequested
  → ScrapeEffect.execute()
    → scrapes URL
    → emits ScrapeEvent::SourceScraped { data }
  → ExtractEffect.execute()
    → extracts items
    → emits ScrapeEvent::NeedsExtracted { needs }
  → SyncEffect.execute()
    → syncs to DB
    → emits ScrapeEvent::SyncComplete
```

Multiple effects can listen to the same event and run in parallel.

**Example**: Parallel notifications

```rust
UserEvent::SignedUp
  ↓
  ├─→ EmailEffect → sends welcome email → EmailSent
  ├─→ SlackEffect → posts to Slack → SlackPosted
  └─→ AnalyticsEffect → tracks event → AnalyticsRecorded
```

All three effects run concurrently when `SignedUp` is emitted.

## Examples

### Example 1: Scraping pipeline

```rust
#[derive(Debug, Clone)]
enum ScrapeEvent {
    SourceRequested { source_id: Uuid },
    SourceScraped { source_id: Uuid, data: String },
    DataExtracted { source_id: Uuid, items: Vec<Item> },
}

// Effect 1: Scrape on request
#[async_trait]
impl Effect<ScrapeEvent, Deps> for ScraperEffect {
    type Event = ScrapeEvent;

    async fn execute(&self, event: ScrapeEvent, ctx: EffectContext<Deps>) -> Result<ScrapeEvent> {
        match event {
            ScrapeEvent::SourceRequested { source_id } => {
                let data = ctx.deps().scraper.scrape(source_id).await?;
                Ok(ScrapeEvent::SourceScraped { source_id, data })
            }
            _ => bail!("unhandled")
        }
    }
}

// Effect 2: Extract on scrape
#[async_trait]
impl Effect<ScrapeEvent, Deps> for ExtractorEffect {
    type Event = ScrapeEvent;

    async fn execute(&self, event: ScrapeEvent, ctx: EffectContext<Deps>) -> Result<ScrapeEvent> {
        match event {
            ScrapeEvent::SourceScraped { source_id, data } => {
                let items = ctx.deps().extractor.extract(&data).await?;
                Ok(ScrapeEvent::DataExtracted { source_id, items })
            }
            _ => bail!("unhandled")
        }
    }
}

let engine = EngineBuilder::new(deps)
    .with_effect::<ScrapeEvent, _>(ScraperEffect)
    .with_effect::<ScrapeEvent, _>(ExtractorEffect)
    .build();
```

### Example 2: Notification dispatch

```rust
#[derive(Debug, Clone)]
enum NotificationEvent {
    UserSignedUp { user_id: Uuid, email: String },
    EmailSent { user_id: Uuid, email_id: Uuid },
    SlackPosted { user_id: Uuid, message_id: String },
}

#[async_trait]
impl Effect<NotificationEvent, Deps> for EmailEffect {
    type Event = NotificationEvent;

    async fn execute(&self, event: NotificationEvent, ctx: EffectContext<Deps>) -> Result<NotificationEvent> {
        match event {
            NotificationEvent::UserSignedUp { user_id, email } => {
                let email_id = ctx.deps().mailer.send_welcome(email).await?;
                Ok(NotificationEvent::EmailSent { user_id, email_id })
            }
            _ => bail!("unhandled")
        }
    }
}

#[async_trait]
impl Effect<NotificationEvent, Deps> for SlackEffect {
    type Event = NotificationEvent;

    async fn execute(&self, event: NotificationEvent, ctx: EffectContext<Deps>) -> Result<NotificationEvent> {
        match event {
            NotificationEvent::UserSignedUp { user_id, .. } => {
                let msg_id = ctx.deps().slack.post("New signup!").await?;
                Ok(NotificationEvent::SlackPosted { user_id, message_id: msg_id })
            }
            _ => bail!("unhandled")
        }
    }
}
```

Both effects run in parallel when `UserSignedUp` is emitted. No coordination needed.

### Example 3: Effect with guards and state

```rust
struct RateLimitedScraperEffect {
    pending: RwLock<HashSet<Uuid>>,
}

#[async_trait]
impl Effect<ScrapeEvent, Deps> for RateLimitedScraperEffect {
    type Event = ScrapeEvent;

    async fn execute(&self, event: ScrapeEvent, ctx: EffectContext<Deps>) -> Result<ScrapeEvent> {
        match event {
            ScrapeEvent::SourceRequested { source_id } => {
                // Guard: check if already pending
                if !self.pending.write().await.insert(source_id) {
                    bail!("already pending");
                }

                // Guard: check rate limit
                if !ctx.deps().rate_limiter.check().await? {
                    self.pending.write().await.remove(&source_id);
                    bail!("rate limited");
                }

                // Do the work
                let data = ctx.deps().scraper.scrape(source_id).await?;
                self.pending.write().await.remove(&source_id);

                Ok(ScrapeEvent::SourceScraped { source_id, data })
            }
            _ => bail!("unhandled")
        }
    }
}
```

Effects can hold state and make decisions. You choose whether to separate pure logic or keep it together.

## Design Guidelines

### Events close loops

Every long-running workflow should have terminal events:
- Success events (e.g., `DataPublished`, `JobComplete`)
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

Effects can listen to events from any domain.

```rust
// CrawlEffect listening to WebsiteEvent
impl Effect<WebsiteEvent, Deps> for CrawlEffect {
    type Event = CrawlEvent;

    async fn execute(&self, event: WebsiteEvent, ctx: EffectContext<Deps>) -> Result<CrawlEvent> {
        match event {
            WebsiteEvent::WebsiteApproved { website_id } => {
                ctx.deps().crawler.start(website_id).await?;
                Ok(CrawlEvent::CrawlStarted { website_id })
            }
            _ => bail!("unhandled")
        }
    }
}
```

This is normal and correct. Cross-domain coordination happens via events.

## Role Matrix

| Role   | React? | Mutate? | Emit?  | Listen To |
| ------ | ------ | ------- | ------ | --------- |
| Effect | Yes    | Yes     | Events | Events    |
| Tap    | No     | No      | No     | Events    |

Effects do the work. Taps observe.

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
JobStarted → ... → (nothing)  // Job stuck "in progress" forever
```

✅ **Better**:
```rust
JobStarted → ... → JobComplete
                ↘ JobFailed
```

Every long-running flow needs success and failure terminal events.

## Engine Usage

```rust
let engine = EngineBuilder::new(deps)
    .with_effect::<MyEvent, _>(MyEventEffect)
    .with_event_tap::<MyEvent, _>(MyTap)  // Optional observers
    .build();

let handle = engine.start();
handle.emit(MyEvent::Started);                    // Fire-and-forget
handle.emit_and_await(MyEvent::Started).await?;   // Wait for completion
```

Other builder methods: `.with_bus()`, `.with_inflight()`, `.with_arc(deps)`

## Cross-Domain Reactions

Effects can listen to events from other domains. This is normal and correct.

**Pattern**: Domain A emits event → Domain B's effect reacts → Domain B emits its own events

### Example: Website approval triggers crawling

```rust
// Website domain emits events
pub enum WebsiteEvent {
    WebsiteApproved { website_id: Uuid },
}

// Crawl effect listens to WebsiteEvent
impl Effect<WebsiteEvent, Deps> for CrawlEffect {
    type Event = CrawlEvent;

    async fn execute(&self, event: WebsiteEvent, ctx: EffectContext<Deps>) -> Result<CrawlEvent> {
        match event {
            WebsiteEvent::WebsiteApproved { website_id } => {
                ctx.deps().crawler.start(website_id).await?;
                Ok(CrawlEvent::CrawlStarted { website_id })
            }
        }
    }
}
```

**Why this is correct**:
- No domain logic leakage
- No tight coupling (depends on event, not internal state)
- Trivially testable
- Explicit and localized

The runtime broadcasts events. Effects that care, react. That's it.

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

## Structural Authorization Pattern

Wrap commands in `Authorize<C>` to enforce auth in the type system:

```
RequestEvent → Machine → Authorize<Cmd> → AuthEffect → Authorized<Cmd> → Forwarder → Cmd → Effect
```

## Workflow Patterns

| Pattern           | Use For                        | State Location    |
| ----------------- | ------------------------------ | ----------------- |
| Enriched Pipeline | Notifications, audit, webhooks | Entity timestamps |
| State Machine     | AI agents, wizards, sessions   | Machine internal  |

## Background Jobs

Commands with `Background`/`Scheduled` need:

- `fn execution_mode() -> ExecutionMode`
- `fn job_spec() -> Option<JobSpec>`
- `fn serialize_to_json() -> Option<serde_json::Value>`

Wire up via `.with_job_queue(queue)` on EngineBuilder or Dispatcher.

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
EventBus → Effect.execute(event) → Runtime → EventBus
                                                 ↓
                                            EventTaps
```

Simple and direct.

## Design Principles Summary

1. **Events are the only signals** — Everything flows through events
2. **Effects react to events** — Do IO, emit new events
3. **Effects are unconstrained** — Can do anything, pure or impure, stateful or stateless
4. **Events are facts, past-tense** — `UserCreated`, not `CreateUser`
5. **Effects can listen to any domain** — Cross-domain coordination via events
6. **One Effect execution = One transaction** — Multiple atomic writes belong together
7. **Terminal events close loops** — Every workflow needs success/failure events
