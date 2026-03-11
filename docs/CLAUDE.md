# Causal Architecture Guidelines

**Mental Model**: Events are signals. Reactors react and return new events. That's it.

## Quick Start - v0.9.0 API

### Simple Pattern - Reactors Only

```rust
// Define engine once with dependencies and store
let engine = Engine::new(deps, store)
    .with_reactor(reactor::on::<OrderPlaced>().then(|event, ctx| async move {
        ctx.deps().mailer.send_confirmation(&event).await?;
        Ok(EmailSent { order_id: event.id })  // Return event to dispatch
    }));

// Dispatch events directly
engine.dispatch(OrderPlaced { id: 123, total: 99.99 }).await?;
```

### Edge Function Pattern

```rust
// Engine is stateless - define once, use many times
let engine = Engine::new(deps, store)
    .with_reactor(reactor::on::<OrderPlaced>().then(|event, ctx| async move {
        ctx.deps().ship(event.order_id).await?;
        Ok(OrderShipped { order_id: event.order_id })
    }));

// Edge function returns event to dispatch
fn process_webhook(payload: Webhook) -> OrderPlaced {
    OrderPlaced::from(payload)
}

// Dispatch per-request
engine.dispatch(process_webhook(payload)).await?;
```

### Key Differences from v0.6

- **Reactors return events**: Use `.then()` and return `Ok(Event)` instead of `ctx.emit()`
- **`engine.dispatch()` for dispatch**: Async method that dispatches event and waits for reactors
- **`ctx.emit()` removed from public API**: Reactors and edge functions return events
- **Observer pattern**: Return `Ok(())` to dispatch nothing
- **Reducers removed**: No more `.with_reducer()`, state lives in events or dependencies

### What's New in v0.10

- **Renamed `effect` → `reactor`**: Clearer terminology, no confusion with React/FP effects
- **Renamed methods**: `.process()` → `.dispatch()`, `.id()` → `.name()`, `.delayed()` → `.delay()`, `.queued()` → `.background()`, `.join()` → `.accumulate()`
- **`Emit<E>` type**: Reactors return `Emit::One(e)`, `Emit::Batch(vec![...])`, or `Emit::None`
- **Event batching**: Emit multiple events atomically with `Ok(Emit::Batch(events))`
- **Accumulate pattern**: Accumulate batched events with `.accumulate()` for bulk operations
- **Batch metadata**: Events track `batch_id`, `batch_index`, `batch_size`
- **Backward compatible**: `Ok(event)` and `Ok(Some(event))` auto-convert to `Emit`
- **Reducers removed**: Reactors-only architecture, state lives in events or dependencies
- **Simplified HandlerContext**: Only `deps()` method, removed state accessors

---

## What Causal Is

Causal is an **event-driven runtime** for building reactive systems.

**Core flow**: Event → Reactor → Event

- **Events** are signals (facts that already happened)
- **Reactors** react to events, perform IO, return new events
- **State** lives in events or shared dependencies

Simple, direct, no ceremony.

### Key Properties

- **Is**: Event-driven runtime
- **Is**: Direct event → effect → event flows
- **Is Not**: Event sourcing, distributed actors, retry engine, workflow orchestrator, workflow engine, job queue

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

### Reactor

Reactors react to events and return new events.

```rust
// Simple reactor - reacts to one event type, returns new event
let scrape_handler = reactor::on::<SourceRequested>().then(|event, ctx| async move {
    let data = ctx.deps().scraper.scrape(event.source_id).await?;
    Ok(SourceScraped {
        source_id: event.source_id,
        data
    })
});

// Reactor with extract - extract data and return event
let priority_handler = reactor::on::<OrderPlaced>()
    .extract(|event| {
        if event.priority > 5 { Some(event.clone()) } else { None }
    })
    .then(|event, ctx| async move {
        ctx.deps().notify_urgent(&event).await?;
        Ok(UrgentNotified { order_id: event.id })
    });

// Reactor with state transition
let status_handler = reactor::on::<StatusChanged>()
    .transition(|prev, next| prev.status != next.status)
    .then(|event, ctx| async move {
        ctx.deps().notify_status_change(&event).await?;
        Ok(StatusNotified { id: event.id })
    });

// Observer reactor - returns () to dispatch nothing
let logger_handler = reactor::on::<OrderPlaced>().then(|event, ctx| async move {
    ctx.deps().logger.log(&event);
    Ok(())  // No event dispatched
});

// Observe ALL events (for logging, metrics, debugging)
let observer_handler = reactor::on_any().then(|event, ctx| async move {
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
use causal::on;

// Match-like syntax with Event::Variant patterns
let reactors = on! {
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

// Returns Vec<Reactor<D>> - add to engine
let engine = reactors.into_iter().fold(Engine::new(deps, store), |e, h| e.with_reactor(h));
```

#### Reactor Execution Configuration (v0.8.0+)

Reactors can be configured for retry, timeout, delay, priority, and background execution:

```rust
// Background reactor with retry and timeout
reactor::on::<PaymentRequested>()
    .name("charge_payment")          // Custom name for tracing/debugging
    .retry(5)                        // Retry up to 5 times on failure
    .timeout(Duration::from_secs(30))  // 30 second timeout
    .priority(1)                     // Higher priority (lower number = higher priority)
    .then(|event, ctx| async move {
        ctx.deps().stripe.charge(&event).await?;
        Ok(PaymentCharged { order_id: event.order_id })
    });

// Delayed execution
reactor::on::<OrderPlaced>()
    .delay(Duration::from_secs(3600))  // Run 1 hour later
    .then(|event, ctx| async move {
        ctx.deps().send_followup_email(&event).await?;
        Ok(FollowupSent { order_id: event.order_id })
    });

// Force background execution (even without retry/delay/timeout)
reactor::on::<AnalyticsEvent>()
    .background()                    // Execute in background worker
    .then(|event, ctx| async move {
        ctx.deps().analytics.track(&event).await?;
        Ok(())
    });

// Chaining works in any order
reactor::on::<OrderPlaced>()
    .filter(|e| e.total > 100.0)     // Filter first
    .retry(3)                        // Then config
    .name("large_orders")
    .priority(1)
    .then(|event, ctx| async move {
        Ok(LargeOrderProcessed { id: event.id })
    });
```

**Execution Modes:**
- **Inline** (default): Runs immediately in same transaction, no retry
- **Background**: Triggered by `.background()`, `.delay()`, `.timeout()`, `.retry() > 1`, or `.priority()`

**Configuration Methods:**
- `.name(String)` - Custom name for tracing/debugging
- `.retry(u32)` - Max retry attempts (default: 1 = no retry)
- `.timeout(Duration)` - Execution timeout
- `.delay(Duration)` - Delay before execution
- `.priority(i32)` - Priority (lower = higher priority)
- `.background()` - Force background execution

Each arm generates a reactor equivalent to:
```rust
reactor::on::<CrawlEvent>()
    .extract(|e| match e {
        CrawlEvent::WebsiteIngested { website_id, job_id, .. } => Some((website_id.clone(), job_id.clone())),
        _ => None,
    })
    .then(|(website_id, job_id), ctx| async move { ... })
```

Reactors can:
- Do IO (DB queries, API calls, etc.)
- Make decisions
- Branch on conditions
- Be pure or impure (your choice)
- Return events to dispatch, or `Ok(())` to dispatch nothing
- Filter events with `.extract()` (formerly `.extract()`)
- React to state transitions with `.transition()`
- Use `on!` macro for ergonomic multi-variant matching

HandlerContext provides:
- `deps()` — shared dependencies
- `reactor_id()` — reactor identifier for tracing
- `idempotency_key()` — deterministic key for external APIs
- `correlation_id` — groups related events in workflow
- `event_id` — current event's unique identifier

### Event Batching

**v0.9+** Reactors can emit multiple events atomically using `Emit<E>`:

```rust
pub enum Emit<E> {
    None,           // Observer pattern, no events dispatched
    One(E),         // Single event (most common)
    Batch(Vec<E>),  // Multiple events atomically
}
```

#### Emitting Batches

Use `Emit::Batch` when processing collections that need to emit one event per item:

```rust
// Parse CSV and emit batch of row events
reactor::on::<FileUploaded>().then(|event, ctx| async move {
    let rows = ctx.deps().parse_csv(&event.path).await?;

    // Emit all row events atomically with same batch_id
    let events: Vec<_> = rows.into_iter()
        .map(|row| RowParsed { row })
        .collect();

    Ok(Emit::Batch(events))  // All events get same batch_id
})
```

**Key properties:**
- All events in batch inserted in single transaction
- All events share same `batch_id` (auto-generated)
- Events have sequential `batch_index` (0, 1, 2, ...)
- Atomic: rollback discards entire batch

**Return types:**
```rust
// Auto-conversion works when return type is consistent:
Ok(event)                     // → Emit::One(event) when always returning events
Ok(vec![e1, e2])             // → Emit::Batch([e1, e2]) when always returning vecs

// Use explicit Emit when mixing return types in same reactor:
Ok(Emit::One(event))         // Single event
Ok(Emit::Batch(vec![...]))   // Multiple events atomically
Ok(Emit::None)               // Observer pattern, no event
```

**Rule**: If all code paths return the same type (`Event`, `Vec<Event>`, or `()`), auto-conversion works. If mixing types (some paths return events, others return nothing), use explicit `Emit` to avoid type inference ambiguity.

#### Accumulating Batched Events

Use `.accumulate()` to accumulate events from the same batch before processing:

```rust
// Accumulate all RowParsed events from same batch
reactor::on::<RowParsed>()
    .accumulate()  // Enable batch accumulation
    .then(|batch: Vec<RowParsed>, ctx| async move {
        // Reactor receives Vec<Event> instead of single Event

        // Bulk insert all rows at once
        ctx.deps().db.bulk_insert(&batch).await?;

        Ok(Emit::One(BatchInserted { count: batch.len() }))
    })
```

**How accumulate works:**
1. Events with same `batch_id` are accumulated in `causal_join_entries` table
2. Per-event reactor is skipped (no-op)
3. When all events in batch arrive (based on `batch_size`), accumulate reactor fires
4. Accumulate reactor receives `Vec<Event>` with all accumulated events
5. Window marked complete in `causal_join_windows` table

**Accumulate properties:**
- **Durable**: Accumulate state persisted in database, survives restarts
- **Deterministic**: Window closes when `batch_size` events received
- **Ordered**: Events in `Vec` maintain `batch_index` order
- **Always background**: Accumulate reactors execute in background workers

#### Batch Flow Example

Complete flow showing batch emission → join accumulation → bulk operation:

```rust
// Event types
enum ImportEvent {
    FileUploaded { path: String },
    RowParsed { row: String },
    RowValidated { row: String },
    BatchInserted { count: usize },
}

// Reactor 1: Parse file, emit batch
reactor::on::<ImportEvent>()
    .extract(|e| match e {
        ImportEvent::FileUploaded { path } => Some(path.clone()),
        _ => None,
    })
    .then(|path, ctx| async move {
        let rows = ctx.deps().parse_csv(&path).await?;
        let events = rows.into_iter()
            .map(|row| ImportEvent::RowParsed { row })
            .collect();
        Ok(Emit::Batch(events))  // 1000 events with same batch_id
    })

// Reactor 2: Validate each row (runs 1000 times)
reactor::on::<ImportEvent>()
    .extract(|e| match e {
        ImportEvent::RowParsed { row } => Some(row.clone()),
        _ => None,
    })
    .then(|row, ctx| async move {
        ctx.deps().validate_row(&row).await?;
        Ok(Emit::One(ImportEvent::RowValidated { row }))
    })

// Reactor 3: Accumulate validated rows, bulk insert (runs once per batch)
reactor::on::<ImportEvent>()
    .extract(|e| match e {
        ImportEvent::RowValidated { row } => Some(row.clone()),
        _ => None,
    })
    .accumulate()
    .then(|batch, ctx| async move {
        // batch contains all 1000 validated rows
        ctx.deps().db.bulk_insert(&batch).await?;
        Ok(Emit::One(ImportEvent::BatchInserted { count: batch.len() }))
    })
```

**Execution trace:**
```
1. FileUploaded dispatched
2. Parse reactor runs → Emit::Batch([Row1, Row2, ..., Row1000])
3. All 1000 RowParsed events inserted (same batch_id, sequential batch_index)
4. Validate reactor runs 1000 times (once per RowParsed)
5. Each validation emits RowValidated (new batch_id per event)
6. Accumulate reactor accumulates all RowValidated in causal_join_entries
7. When all events arrived, accumulate reactor fires with Vec[Row1...Row1000]
8. Bulk insert runs once
```

#### Error Handling in Batches

**Pattern 1: Per-item error events** (recommended)
```rust
reactor::on::<RowParsed>().then(|event, ctx| async move {
    match ctx.deps().validate_row(&event.row).await {
        Ok(_) => Ok(Emit::One(RowValidated { row: event.row })),
        Err(e) => Ok(Emit::One(RowRejected { row: event.row, reason: e.to_string() })),
    }
})
```

**Pattern 2: Collect successes and failures**
```rust
reactor::on::<RowParsed>()
    .accumulate()
    .then(|batch, ctx| async move {
        let mut results = Vec::new();
        for row in batch {
            match ctx.deps().process(&row).await {
                Ok(_) => results.push(RowSucceeded { row }),
                Err(e) => results.push(RowFailed { row, error: e.to_string() }),
            }
        }
        Ok(Emit::Batch(results))  // Emit both successes and failures
    })
```

**Pattern 3: Retry entire batch** (for idempotent operations)
```rust
reactor::on::<RowValidated>()
    .accumulate()
    .retry(3)  // Retry whole batch on failure
    .then(|batch, ctx| async move {
        // Must be idempotent - may run multiple times
        ctx.deps().bulk_insert(&batch).await?;
        Ok(Emit::One(BatchInserted { count: batch.len() }))
    })
```

#### When to Use Batching

**Use `Emit::Batch` when:**
- ✅ Processing collections with hundreds/thousands of items
- ✅ Need atomic emission of related events
- ✅ Avoiding N separate effect executions
- ✅ Fan-out: dispatch notification to many users

**Use `.accumulate()` when:**
- ✅ Bulk database operations (inserts, updates)
- ✅ Rate limiting: accumulate, send in bursts
- ✅ Aggregation: combine multiple events into summary
- ✅ Performance: reduce transaction overhead

**Don't use batching when:**
- ❌ <10 events (overhead not worth it)
- ❌ Events are unrelated (prefer sequential)
- ❌ Need per-event traceability immediately
- ❌ Partial results should emit incrementally

#### Batch Limitations

**pg_notify 8KB limit:**
- Notifications send metadata only (event_id, type), not payload
- Listeners fetch full events from database
- No size limit on event payloads themselves

**Max batch size:**
- Recommend batches <10,000 events
- For larger datasets, use pagination:
  ```rust
  for chunk in rows.chunks(1000) {
      Ok(Emit::Batch(chunk.to_vec()))  // Emit 1000-item batches
  }
  ```

**Join completion:**
- Window closes when all `batch_size` events received
- If some events fail to emit, window stays open indefinitely
- Ensure batch emission is atomic (all-or-nothing)

#### Complete Example

See `examples/batch-processor/` for full working example demonstrating:
- CSV parsing with `Emit::Batch`
- Per-row validation
- Batch accumulation with `.accumulate()`
- Bulk database insert
- Error handling patterns

### State Management Without Reducers

Causal uses **reactors-only** architecture. State is managed through four patterns:

#### Pattern 1: Event-Threaded State (Pure, Auditable) - ✅ Distributed-Safe
State flows as event fields. Each event carries accumulated state forward.

```rust
#[derive(Clone, Serialize, Deserialize)]
enum OrderEvent {
    Processing {
        order_id: Uuid,
        items_processed: usize,  // Accumulated state
        items_remaining: Vec<Item>,
    },
    Complete {
        order_id: Uuid,
        total_items: usize,  // Final state
    },
}

reactor::on::<OrderEvent>()
    .extract(|e| match e {
        OrderEvent::Processing { order_id, items_processed, items_remaining } =>
            Some((*order_id, *items_processed, items_remaining.clone())),
        _ => None,
    })
    .then(|(order_id, processed, remaining), ctx| async move {
        if let Some((item, rest)) = remaining.split_first() {
            ctx.deps().process_item(item).await?;
            Ok(OrderEvent::Processing {
                order_id,
                items_processed: processed + 1,
                items_remaining: rest.to_vec(),
            })
        } else {
            Ok(OrderEvent::Complete {
                order_id,
                total_items: processed,
            })
        }
    })
```

**When to use:** Deterministic workflows, auditability requirements, replay scenarios, multi-worker deployments

#### Pattern 2: External Persistence (DB/Redis) - ✅ Distributed-Safe
State stored in external systems that are shared across all workers.

```rust
#[derive(Clone)]
struct Deps {
    db: PgPool,           // ✅ Shared across workers
    redis: RedisClient,   // ✅ Shared across workers
}

reactor::on::<OrderEvent>()
    .then(|event, ctx| async move {
        // State lives in database - all workers see same data
        sqlx::query("UPDATE orders SET status = 'shipped' WHERE id = $1")
            .bind(event.order_id)
            .execute(&ctx.deps().db)
            .await?;
        Ok(OrderShipped { order_id: event.order_id })
    })
```

**When to use:** Complex queryable state, shared across workers, need strong consistency

#### Pattern 3: Implicit State (Event Sequence) - ✅ Distributed-Safe
State is implicit in "which events have fired".

```rust
// State is: OrderPlaced → state "placed"
//          OrderShipped → state "shipped"
//          OrderDelivered → state "delivered"

reactor::on::<OrderPlaced>().then(|event, ctx| async move {
    ctx.deps().ship(event.order_id).await?;
    Ok(OrderShipped { order_id: event.order_id })
})
```

**When to use:** Simple workflows where event types represent state transitions

#### Pattern 4: In-Memory State (`Arc<Mutex>`) - ⚠️ SINGLE-PROCESS ONLY

**🚨 CRITICAL WARNING: This pattern BREAKS with multiple workers!**

```rust
// ❌ DO NOT USE IN PRODUCTION WITH MULTIPLE WORKERS
#[derive(Clone)]
struct Deps {
    order_status: Arc<Mutex<HashMap<Uuid, OrderStatus>>>,
}

reactor::on::<OrderEvent>()
    .then(|event, ctx| async move {
        let mut status = ctx.deps().order_status.lock().unwrap();
        status.insert(event.order_id, OrderStatus::Shipped);
        Ok(OrderShipped { order_id: event.order_id })
    })
```

**Why this breaks:**
```
Worker 1 starts with:   cache = {}
Worker 2 starts with:   cache = {}

Worker 1: cache[order_123] = "Shipped"  → Worker 1 cache = { order_123: "Shipped" }
Worker 2: cache[order_123] = "Pending"  → Worker 2 cache = { order_123: "Pending" }

Workers have diverged! They'll never see each other's updates. ☠️
```

**When to use:**
- ✅ Development/testing with single worker
- ✅ Demos and prototypes
- ❌ **NEVER in production with multiple workers**

**If you need in-memory caching:**
- Use Pattern 2 with Redis/Memcached (shared cache)
- Use Pattern 1 with event-threaded state
- Accept that cache is worker-local (eventual consistency)

#### Choosing a Pattern

| Pattern | Deterministic | Auditable | Multi-Worker | Complexity |
|---------|---------------|-----------|--------------|------------|
| Event-Threaded | ✅ | ✅ | ✅ | Low-Medium |
| External Storage | ⚠️ | ⚠️ | ✅ | Medium |
| Implicit | ✅ | ✅ | ✅ | Low |
| Arc<Mutex> | ❌ | ❌ | ❌ | Low |

**For production systems:** Use Patterns 1, 2, or 3. Avoid Pattern 4 unless you have exactly one worker.

### Reducer (Removed in v0.9)

**Reducers have been removed** from Causal. State is now managed by reactors through:
- Event-threaded state (state flows through event fields)
- Shared dependency state (Arc<Mutex<T>> in deps)
- Implicit state (event sequences represent state)

## Execution Model

Simple and direct:

```
Event dispatched
  ↓
All Reactors listening to this event execute
  ↓
Reactors perform IO, make decisions, query state from:
  - Event payload (state-in-events pattern)
  - Shared dependencies (Arc<Mutex<T>> pattern)
  - Event history (implicit state pattern)
  ↓
Reactors return new Event (or () for observer pattern)
  ↓
Repeat
```

**Example**: Scraping pipeline

```rust
ScrapeEvent::SourceRequested
  → ScrapeHandler → scrapes URL → returns SourceScraped { data }
  → ExtractHandler → extracts items → returns DataExtracted { items }
  → SyncHandler → syncs to DB → returns SyncComplete
```

Multiple reactors can listen to the same event and run in parallel.

**Example**: Parallel notifications

```rust
UserEvent::SignedUp
  ↓
  ├─→ EmailHandler → sends welcome email → returns EmailSent
  ├─→ SlackHandler → posts to Slack → returns SlackPosted
  └─→ AnalyticsHandler → tracks event → returns ()
```

All three reactors run concurrently when `SignedUp` is dispatched.

## Transaction Boundaries and Execution Modes

Understanding when reactors run in which transaction is critical for correctness.

### Inline Reactors (Default)

Reactors without `.retry() > 1`, `.delay()`, `.timeout()`, or `.priority()` run **inline**:

```rust
// Inline reactor - runs immediately
reactor::on::<OrderPlaced>()
    .then(|event, ctx| async move {
        ctx.deps().db.insert_order(&event).await?;
        Ok(OrderSaved { order_id: event.order_id })
    })
```

**Execution flow:**
```
engine.process(OrderPlaced { id: 123 }).await?;
  ↓ [Transaction begins in EventWorker]
  ↓ Insert OrderPlaced into causal_events
  ↓ Reactor executes (same transaction)
  ↓ Insert OrderSaved into causal_events
  ↓ Mark OrderPlaced as processed
  ↓ [Transaction commits - atomic]
  ↓ pg_notify sends events to workers
```

**Guarantees:**
- ⚡ **Fast** - no queue overhead, runs immediately
- 🔒 **Atomic** - all-or-nothing with event dispatch
- 🎯 **Synchronous** - `.await` waits for completion
- ✅ **Same transaction** - event + reactor + emitted events all atomic

**Use when:**
- Fast operations (<100ms)
- Must be atomic with event dispatch
- Need synchronous confirmation
- Database updates that must commit together

### Background Reactors (Queued)

Reactors with `.retry() > 1`, `.delay()`, `.timeout()`, or `.priority()` run in **background workers**:

```rust
// Background reactor - queued for workers
reactor::on::<PaymentRequested>()
    .retry(3)
    .timeout(Duration::from_secs(30))
    .then(|event, ctx| async move {
        ctx.deps().stripe.charge(&event).await?;
        Ok(PaymentCharged { order_id: event.order_id })
    })
```

**Execution flow:**
```
1. Event inserted into causal_events (Transaction A)
2. Reactor intent inserted into causal_handler_intents (Transaction A)
3. [Transaction A commits]
4. pg_notify alerts workers
5. Worker picks up reactor intent
6. [Transaction B begins in HandlerWorker]
7. Reactor executes
8. Insert PaymentCharged into causal_events
9. Mark reactor complete
10. [Transaction B commits]
11. pg_notify sends new events
```

**Guarantees:**
- ⏱️ **Async** - `.await` only waits for queue insertion, not execution
- 🔄 **Retryable** - survives failures and restarts
- 📈 **Scalable** - distributes across multiple workers
- ⚠️ **Separate transaction** - not atomic with event dispatch

**Use when:**
- Slow operations (API calls, external services)
- Need retry on failure
- Can be asynchronous (eventual consistency okay)
- Want to distribute load across workers

### Key Differences

| Aspect | Inline | Background |
|--------|--------|------------|
| **Transaction** | Same as dispatch | Separate |
| **Speed** | Immediate | Queued (eventually) |
| **Atomicity** | With dispatch | No |
| **Retry** | No (failure = rollback) | Yes (configurable) |
| **Workers** | Any worker | Dedicated reactor workers |
| **Latency** | Low (~ms) | Higher (~100ms+) |
| **Use case** | DB updates | External APIs |

### Common Patterns

**Pattern 1: Inline for DB, Background for External**
```rust
// Inline - update local database atomically
reactor::on::<OrderPlaced>()
    .then(|event, ctx| async move {
        sqlx::query("INSERT INTO orders ...").execute(&ctx.deps().db).await?;
        Ok(OrderSaved { order_id: event.order_id })
    })

// Background - call external API with retry
reactor::on::<OrderSaved>()
    .retry(3)
    .then(|event, ctx| async move {
        ctx.deps().stripe.charge(&event).await?;
        Ok(PaymentCharged { order_id: event.order_id })
    })
```

**Pattern 2: Chain via Events**
```rust
// Inline reactor emits event for background processing
reactor::on::<WebhookReceived>()
    .then(|event, ctx| async move {
        // Fast validation and persistence
        ctx.deps().db.insert_webhook(&event).await?;
        // Emit event for slow processing
        Ok(WebhookValidated { webhook_id: event.id })
    })

// Background reactor does slow work
reactor::on::<WebhookValidated>()
    .retry(5)
    .timeout(Duration::from_secs(60))
    .then(|event, ctx| async move {
        // Slow processing with retry
        ctx.deps().process_webhook(&event).await?;
        Ok(WebhookProcessed { webhook_id: event.webhook_id })
    })
```

### Transaction Safety Rules

1. **Inline reactors must be idempotent** within their transaction - they may be called multiple times if transaction retries
2. **Background reactors must be idempotent** across executions - they WILL be retried on failure
3. **Never assume atomicity across inline → background** - they're separate transactions
4. **Use idempotency_key()** for external API calls to prevent duplicate charges/notifications
5. **Emit events for coordination** - don't rely on shared mutable state

### Debugging Transaction Issues

**Problem: Changes disappear**
```rust
// ❌ Inline reactor that might rollback
reactor::on::<OrderPlaced>()
    .then(|event, ctx| async move {
        ctx.deps().db.insert_order(&event).await?;  // ✅ Inserted
        external_api_call().await?;  // ❌ Fails, entire TX rolls back
        Ok(OrderSaved { order_id: event.order_id })
    })
```

**Solution: Split inline (DB) and background (external)**
```rust
// ✅ Inline - just DB (fast, atomic)
reactor::on::<OrderPlaced>()
    .then(|event, ctx| async move {
        ctx.deps().db.insert_order(&event).await?;
        Ok(OrderSaved { order_id: event.order_id })  // Always succeeds
    })

// ✅ Background - external API (retryable)
reactor::on::<OrderSaved>()
    .retry(3)
    .then(|event, ctx| async move {
        external_api_call().await?;  // Retries on failure
        Ok(ApiCallComplete)
    })
```

## Examples

### Example 1: Scraping pipeline

```rust
#[derive(Debug, Clone)]
enum ScrapeEvent {
    SourceRequested { source_id: Uuid },
    SourceScraped { source_id: Uuid, data: String },
    DataExtracted { source_id: Uuid, items: Vec<Item> },
}

let engine = Engine::new(deps, store)
    // Reactor 1: Scrape on request
    .with_reactor(
        reactor::on::<ScrapeEvent>()
            .extract(|e| match e {
                ScrapeEvent::SourceRequested { source_id } => Some(*source_id),
                _ => None,
            })
            .then(|source_id, ctx| async move {
                let data = ctx.deps().scraper.scrape(source_id).await?;
                Ok(ScrapeEvent::SourceScraped { source_id, data })
            })
    )
    // Reactor 2: Extract on scrape
    .with_reactor(
        reactor::on::<ScrapeEvent>()
            .extract(|e| match e {
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

let engine = Engine::new(deps, store)
    // Email reactor
    .with_reactor(
        reactor::on::<NotificationEvent>()
            .extract(|e| match e {
                NotificationEvent::UserSignedUp { user_id, email } => Some((*user_id, email.clone())),
                _ => None,
            })
            .then(|(user_id, email), ctx| async move {
                let email_id = ctx.deps().mailer.send_welcome(email).await?;
                Ok(NotificationEvent::EmailSent { user_id, email_id })
            })
    )
    // Slack reactor
    .with_reactor(
        reactor::on::<NotificationEvent>()
            .extract(|e| match e {
                NotificationEvent::UserSignedUp { user_id, .. } => Some(*user_id),
                _ => None,
            })
            .then(|user_id, ctx| async move {
                let msg_id = ctx.deps().slack.post("New signup!").await?;
                Ok(NotificationEvent::SlackPosted { user_id, message_id: msg_id })
            })
    );
```

Both reactors run in parallel when `UserSignedUp` is dispatched. No coordination needed.

### Example 3: Conditional event dispatch

```rust
// Reactor that conditionally returns different events
reactor::on::<ScrapeEvent>()
    .extract(|e| match e {
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

Reactors return events based on outcomes - success, failure, or rate-limited all flow as events.

## Design Guidelines

### Events close loops

Every long-running workflow should have terminal events:
- Success events (e.g., `DataPublished`, `WorkflowComplete`)
- Failure events (e.g., `ScrapeFailed`, `RateLimited`)

Otherwise you get:
- Permanent "in-flight" state
- Silent deadlocks
- Ghost workflows

### Reactors can do anything

Reactors are unconstrained. They can:
- Do IO or be pure
- Hold state or be stateless
- Make complex decisions or be simple transforms
- Branch on time, randomness, config

You decide based on your needs.

### Cross-domain listening

Reactors can listen to events from any domain and return events from another.

```rust
// Reactor listening to WebsiteEvent, returning CrawlEvent
reactor::on::<WebsiteEvent>()
    .extract(|e| match e {
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

Reactors can handle errors in two ways:

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
reactor::on::<OrderEvent>()
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
reactor::on::<OrderEvent>()
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

### Fallback: HandlerError for Ergonomic `?` Usage

```rust
// Use ? for ergonomics
reactor::on::<OrderPlaced>().then(|order, ctx| async move {
    ctx.deps().payment.charge(order.total).await?;  // Propagates error
    Ok(PaymentCharged { order_id: order.id })
})

// Generic HandlerError reactor
reactor::on::<HandlerError>()
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
reactor::on::<HandlerError>()
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
2. **Use HandlerError** for convenience when you just want to use `?`
3. **Write explicit logic** - no magic helpers for "transient" or "should_retry"
4. **Compensation reactors should be infallible** - catch errors internally, don't propagate

## Reactor Responsibilities

Reactors are the **only** reactive primitive in Causal. They handle:

- ✅ Reacting to events
- ✅ Performing side effects (IO, API calls)
- ✅ Querying state (from events or dependencies)
- ✅ Updating state (by emitting new events or mutating deps)
- ✅ Making decisions and branching logic
- ✅ Returning new events to dispatch

Reactors can be:
- Pure or impure (your choice)
- Stateless or stateful (your choice)
- Synchronous or asynchronous
- Inline or queued

## What Causal Is Not

### ❌ Not a workflow engine
- No DAGs
- No BPMN
- No retries
- No timers

Workflows **emerge** from event sequences.

### ❌ Not CQRS (exactly)

It overlaps, but:
- Causal doesn't require read models
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

Causal doesn't force you to use machines or commands.

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

### 2. Reactors that know too much

If your reactor:
- Has dozens of fields
- Mirrors database rows
- Holds authoritative data

You're putting the source of truth in the wrong place. Reactors should query deps, not store domain data.

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
// Define engine with dependencies and store
let engine = Engine::new(deps, store)
    .with_reactor(reactor::on::<MyEvent>().then(|event, ctx| async move {
        // Query state from deps or event
        let count = ctx.deps().get_count().await?;
        ctx.deps().set_count(count + 1).await?;

        // Or thread state through events
        ctx.deps().notify(&event).await?;
        Ok(NextEvent { id: event.id, count: count + 1 })
    }));

// Dispatch events directly
engine.dispatch(MyEvent::Started { count: 0 }).await?;
```

Builder methods:
- `.with_reactor(reactor)` — Register event reactors (use `.then()` to return events)
- `Engine::new(deps, store)` — Create engine with dependencies and event store

Engine methods:
- `.dispatch(event).await` — Dispatch event and wait for reactors to complete
- `.settled().await` — Wait for all reactors to complete
- `.cancel()` — Cancel all tasks

## Cross-Domain Reactions

Reactors can listen to events from other domains. This is normal and correct.

**Pattern**: Domain A dispatches event → Domain B's reactor reacts → Domain B returns its own events

### Example: Website approval triggers crawling

```rust
// Website domain dispatches events
pub enum WebsiteEvent {
    WebsiteApproved { website_id: Uuid },
}

// Crawl reactor listens to WebsiteEvent, returns CrawlEvent
reactor::on::<WebsiteEvent>()
    .extract(|e| match e {
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

The engine dispatches events. Reactors that care, react and return new events. That's it.

## Request/Response Pattern

For edges that need a response:

```rust
use causal::{dispatch_request, EnvelopeMatch};

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
Engine → Reactor.then(event) → returns Event → Engine
```

Simple and direct.

## Design Principles Summary

1. **Events are the only signals** — Everything flows through events
2. **Reactors are the only reactive primitive** — Handle both state queries/updates AND side effects
3. **State lives in events or dependencies** — Choose pattern based on needs
4. **Reactors are unconstrained** — Can do anything, pure or impure, stateful or stateless
5. **Events are facts, past-tense** — `UserCreated`, not `CreateUser`
6. **Reactors can listen to any domain** — Cross-domain coordination via events
7. **One Reactor execution = One transaction** — Multiple atomic writes belong together
8. **Terminal events close loops** — Every workflow needs success/failure events
