# Fix Inline Reactor Blocking Footgun - Complete Analysis

**Date:** 2026-02-06
**Status:** Planning
**Priority:** CRITICAL

## Executive Summary

**Problem:** Inline reactors execute in the same database transaction as event insertion. Slow reactors (1+ minute) hold transactions open, blocking all other events and causing cascading failures.

**Solution:** ALL reactors execute asynchronously in workers. Separate queue streams per event type eliminates priority decisions and enables natural parallelism.

**Impact:** Enables reactors of any duration, scales to ultra-high throughput, eliminates transaction blocking footgun.

---

## Part 1: The Problem (With Concrete Examples)

### Example 1: Slow Inline Reactor Blocks Everything

**Code:**
```rust
// Reactor that calls external API
#[reactor(on = OrderPlaced)]
async fn charge_payment(event: OrderPlaced, ctx: Ctx) -> Result<PaymentCharged> {
    // Stripe API call takes 2 seconds
    ctx.deps().stripe.charge(&event).await?;
    Ok(PaymentCharged { order_id: event.order_id })
}

// User dispatches event
engine.dispatch(OrderPlaced { id: 123, total: 99.99 }).await?;
```

**What Actually Happens:**

```
T=0ms:   [Transaction begins]
T=0ms:     Worker 1 acquires transaction lock
T=1ms:     Insert OrderPlaced event into causal_events
T=1ms:     Execute inline reactor → call Stripe API
T=2001ms:  Stripe returns success
T=2002ms:  Insert PaymentCharged event
T=2003ms: [Transaction commits]
```

**The Disaster:**

While Worker 1 holds the transaction for **2 seconds**:

```
Worker 2 (T=100ms):  Tries to dispatch OrderPlaced { id: 456 }
                     → [Transaction begins]
                     → INSERT INTO causal_events ...
                     → BLOCKED! (Waiting for Worker 1's locks)

Worker 3 (T=200ms):  Tries to dispatch UserSignedUp { ... }
                     → [Transaction begins]
                     → INSERT INTO causal_events ...
                     → BLOCKED! (Waiting for Worker 1's locks)

Worker 4 (T=300ms):  Tries to dispatch EmailSent { ... }
                     → BLOCKED!

... All workers blocked waiting for Worker 1 ...
```

**Postgres Lock Situation:**
```sql
SELECT * FROM pg_locks WHERE NOT granted;
-- Shows dozens of workers waiting for row/table locks
-- All blocked on causal_events table
-- Connection pool exhausted
-- Query timeout errors start appearing
```

**User Impact:**
- API requests timeout
- Events pile up in application memory
- Database connection pool exhausted
- System grinds to a halt
- Requires restart to recover

### Example 2: One Slow Reactor Ruins Everything

**Scenario:** You have 99 fast reactors and 1 slow reactor:

```rust
// Reactor 1-99: Fast database writes (10ms each)
#[reactor(on = OrderPlaced)]
async fn save_to_orders_table(...) { /* 10ms */ }

#[reactor(on = OrderPlaced)]
async fn save_to_analytics_table(...) { /* 10ms */ }

// ... 97 more fast reactors ...

// Reactor 100: Slow AI processing (60 seconds)
#[reactor(on = OrderPlaced)]
async fn generate_product_recommendations(...) {
    // OpenAI API call - 60 seconds
    let recommendations = ctx.deps().openai.generate(...).await?;
    Ok(RecommendationsGenerated { recommendations })
}
```

**What Happens:**

```
User dispatches OrderPlaced:
  [Transaction begins]
    Execute reactor 1  (10ms)  ✅
    Execute reactor 2  (10ms)  ✅
    Execute reactor 3  (10ms)  ✅
    ...
    Execute reactor 99 (10ms)  ✅
    Execute reactor 100 (60 seconds) ⏰ ← Transaction held for 1 minute!
  [Transaction commits after 60+ seconds]
```

**Result:** One slow reactor blocks the entire system for 60 seconds per event.

### Example 3: Under Load - Complete Meltdown

**Scenario:** 10 events/second, 1 reactor takes 2 seconds

```
T=0s:    OrderPlaced #1 dispatched → Reactor runs (2s) → Transaction held
T=0.1s:  OrderPlaced #2 dispatched → BLOCKED (waiting for #1)
T=0.2s:  OrderPlaced #3 dispatched → BLOCKED (waiting for #1)
T=0.3s:  OrderPlaced #4 dispatched → BLOCKED (waiting for #1)
...
T=2.0s:  OrderPlaced #1 completes → #2 can start
T=2.1s:  OrderPlaced #21 dispatched → BLOCKED (waiting for #2)
...

After 1 minute:
- 600 events dispatched
- Only 30 completed (2s each)
- 570 events blocked in queue
- Connection pool: 570/100 connections used (exhausted!)
- New requests: Connection timeout errors
```

**System state:**
```
Database connections: 100/100 (EXHAUSTED)
Blocked queries: 570
Oldest blocked query: 58 seconds
Application memory: 2.3 GB (events backed up)
Error rate: 95% (timeouts)
```

### Root Cause Analysis

**The Fundamental Problem:**

Mixing two concerns in one execution model:
1. **Event persistence** (fast: ~5-10ms)
2. **Reactor execution** (variable: 1ms to 60+ seconds)

**Current Design:**
```
Dispatch = Persist + Execute Inline Reactors (in ONE transaction)
```

If ANY reactor is slow → ENTIRE dispatch is slow → Transaction held → Locks block others

**Why This Is Broken:**

1. **No isolation**: Fast reactors blocked by slow reactors
2. **Resource exhaustion**: Long transactions hold DB connections
3. **Cascading failures**: One slow reactor cascades to all workers
4. **Unpredictable**: Adding new reactor can break existing system
5. **No escape hatch**: Can't fix without rewriting reactors

---

## Part 2: The Solution (Separate Streams Per Event Type)

### Core Principle

**Event persistence and reactor execution are separate concerns and should use separate transactions.**

```
OLD (Broken):
  Dispatch = [TX: Persist + Execute] ← Single transaction

NEW (Fixed):
  Dispatch = [TX: Persist + Queue Intents] ← Fast transaction (<10ms)
  Workers  = [TX: Execute Reactor] ← Separate transaction per reactor
```

### Architecture: Separate Streams Per Event Type

**Key Insight:** Instead of priority (which developers can't reason about), use **separate queue streams per event type**. This gives natural parallelism without configuration.

**Stream Model:**

```
OrderPlaced events      → "causal.OrderPlaced" stream      → Workers subscribe
PaymentRequested events → "causal.PaymentRequested" stream → Workers subscribe
EmailSent events        → "causal.EmailSent" stream        → Workers subscribe
UserSignedUp events     → "causal.UserSignedUp" stream     → Workers subscribe
```

**Why This Is Better Than Priority:**

| Approach | Developer Decision | Parallelism | Scalability |
|----------|-------------------|-------------|-------------|
| **Priority** | "What priority should this reactor have?" ❌ | Limited (single queue) | Hard to scale |
| **Separate Streams** | None! ✅ | Natural (per event type) | Easy (scale per stream) |

**Benefits:**
1. ✅ **No priority decisions needed** - Developer just registers reactor, system handles the rest
2. ✅ **Natural parallelism** - OrderPlaced doesn't block PaymentRequested
3. ✅ **Independent scaling** - Can add workers for slow event types only
4. ✅ **Clear mental model** - One stream per event type
5. ✅ **Kafka/Redis native** - Topics/channels map directly to event types

### New Architecture Flow

**Dispatch (Fast Path):**

```rust
// User code
engine.dispatch(OrderPlaced { id: 123 }).await?;
// Returns immediately after persistence ✅

// What happens internally:
[Transaction begins]  ← Single fast transaction
  1. Insert event into causal_events
     - event_id: uuid
     - event_type: "OrderPlaced"
     - payload: {...}

  2. Insert handler_intents for ALL reactors listening to OrderPlaced
     - intent_id: uuid
     - event_id: (from step 1)
     - reactor_id: "charge_payment"
     - status: "pending"

  3. Notify queue backend: "causal.OrderPlaced" stream
     - queue.notify("OrderPlaced", intent_id)
[Transaction commits]  ← Always <10ms

// Dispatch returns to user
```

**Worker Execution (Async Path):**

```
Worker Pool:
  Worker 1: Subscribes to ["OrderPlaced", "UserSignedUp"]
  Worker 2: Subscribes to ["PaymentRequested", "EmailSent"]
  Worker 3: Subscribes to all event types

Each worker polls its subscribed streams:

Worker 1 receives notification on "causal.OrderPlaced":
  1. Poll database: SELECT * FROM causal_handler_intents
                    WHERE intent_id = ? FOR UPDATE SKIP LOCKED

  2. If locked by another worker → Skip (another worker handling it)
     If acquired lock → Execute reactor

  3. Execute reactor (NO TRANSACTION HELD DURING EXECUTION)
     - Can take 1 second, 1 minute, 1 hour - doesn't matter!
     - If fails → Retry logic kicks in
     - If succeeds → Continue to step 4

  4. [New Transaction] ← Separate from dispatch!
       Insert result events
       Mark intent as complete
     [Commit]
```

### Why We Still Need `await` on Dispatch

**Question:** If dispatch returns immediately, why `await`?

**Answer:** Because "immediately" means "after database write completes" (~5-10ms), not "synchronously".

```rust
// This is async I/O (database insert)
engine.dispatch(OrderPlaced { id: 123 }).await?;
//                                       ^^^^^^
//                                       Waits for DB write to complete

// Equivalent to:
let future = engine.dispatch(OrderPlaced { id: 123 });
// At this point, nothing has happened yet (lazy)

future.await?;
// Now we wait for:
// 1. Open database transaction
// 2. INSERT INTO causal_events (5ms)
// 3. INSERT INTO causal_handler_intents (3ms)
// 4. NOTIFY queue (1ms)
// 5. Commit transaction (1ms)
// Total: ~10ms
```

**Without `await`:**
```rust
engine.dispatch(OrderPlaced { id: 123 });  // ❌ Compile error
// Error: unused future that must be used
```

**The key difference:**
- **OLD**: `await` waits for reactors to execute (can be 60 seconds)
- **NEW**: `await` waits for event to be persisted + queued (always <10ms)

---

## Part 3: Queue Backend Abstraction

### QueueBackend Trait

```rust
/// Abstraction for notifying workers about reactor intents
pub trait QueueBackend: Send + Sync + 'static {
    /// Notify that reactor intents are available for an event type
    async fn notify(&self, event_type: &str, intent_ids: &[Uuid]) -> Result<()>;

    /// Subscribe to notifications for specific event types
    /// Returns a stream of intent IDs to process
    async fn subscribe(&self, event_types: &[String]) -> Result<Receiver<Uuid>>;
}
```

### Implementation 1: PostgresQueue (pg_notify)

**Good for:** <1,000 events/second, simple deployments

```rust
pub struct PostgresQueue {
    pool: PgPool,
}

impl QueueBackend for PostgresQueue {
    async fn notify(&self, event_type: &str, intent_ids: &[Uuid]) -> Result<()> {
        // Use pg_notify with channel per event type
        let channel = format!("causal.{}", event_type);

        for intent_id in intent_ids {
            self.pool.execute(
                &format!("NOTIFY {}, '{}'", channel, intent_id)
            ).await?;
        }
        Ok(())
    }

    async fn subscribe(&self, event_types: &[String]) -> Result<Receiver<Uuid>> {
        let mut listener = PgListener::connect_with(&self.pool).await?;

        // LISTEN to each event type channel
        for event_type in event_types {
            listener.listen(&format!("causal.{}", event_type)).await?;
        }

        // Return channel that forwards notifications
        let (tx, rx) = mpsc::channel(1000);

        tokio::spawn(async move {
            while let Ok(notification) = listener.recv().await {
                let intent_id = Uuid::parse_str(&notification.payload())?;
                tx.send(intent_id).await?;
            }
        });

        Ok(rx)
    }
}
```

**Characteristics:**
- ✅ Built-in (no extra infrastructure)
- ✅ Simple setup
- ⚠️ pg_notify has 8KB payload limit (fine - we only send intent_id)
- ⚠️ Limited throughput (<1k/sec)
- ❌ Not suitable for high-scale production

### Implementation 2: RedisQueue

**Good for:** <10,000 events/second, moderate scale

```rust
pub struct RedisQueue {
    client: redis::Client,
}

impl QueueBackend for RedisQueue {
    async fn notify(&self, event_type: &str, intent_ids: &[Uuid]) -> Result<()> {
        let mut conn = self.client.get_async_connection().await?;
        let channel = format!("causal.{}", event_type);

        // Publish to Redis channel per event type
        for intent_id in intent_ids {
            redis::cmd("PUBLISH")
                .arg(&channel)
                .arg(intent_id.to_string())
                .query_async(&mut conn)
                .await?;
        }
        Ok(())
    }

    async fn subscribe(&self, event_types: &[String]) -> Result<Receiver<Uuid>> {
        let mut pubsub = self.client.get_async_connection().await?.into_pubsub();

        // Subscribe to each event type channel
        for event_type in event_types {
            pubsub.subscribe(&format!("causal.{}", event_type)).await?;
        }

        let (tx, rx) = mpsc::channel(1000);

        tokio::spawn(async move {
            while let Some(msg) = pubsub.on_message().next().await {
                let payload: String = msg.get_payload()?;
                let intent_id = Uuid::parse_str(&payload)?;
                tx.send(intent_id).await?;
            }
        });

        Ok(rx)
    }
}
```

**Characteristics:**
- ✅ Higher throughput than Postgres
- ✅ Pub/sub model natural fit
- ✅ Good middle ground
- ⚠️ Requires Redis infrastructure
- ⚠️ Fire-and-forget (no delivery guarantees)

### Implementation 3: KafkaQueue

**Good for:** 100,000+ events/second, ultra-high scale

```rust
pub struct KafkaQueue {
    producer: FutureProducer,
    consumer_config: ClientConfig,
}

impl QueueBackend for KafkaQueue {
    async fn notify(&self, event_type: &str, intent_ids: &[Uuid]) -> Result<()> {
        let topic = format!("causal.{}", event_type);

        // Publish to Kafka topic per event type
        for intent_id in intent_ids {
            let record = FutureRecord::to(&topic)
                .key(&intent_id.to_string())
                .payload(&intent_id.to_string());

            self.producer.send(record, Duration::from_secs(0)).await?;
        }
        Ok(())
    }

    async fn subscribe(&self, event_types: &[String]) -> Result<Receiver<Uuid>> {
        let topics: Vec<_> = event_types.iter()
            .map(|t| format!("causal.{}", t))
            .collect();

        let consumer: StreamConsumer = self.consumer_config.create()?;
        consumer.subscribe(&topics.iter().map(|s| s.as_str()).collect::<Vec<_>>())?;

        let (tx, rx) = mpsc::channel(1000);

        tokio::spawn(async move {
            while let Some(result) = consumer.stream().next().await {
                let message = result?;
                let payload = message.payload_view::<str>()?.unwrap();
                let intent_id = Uuid::parse_str(payload)?;
                tx.send(intent_id).await?;
            }
        });

        Ok(rx)
    }
}
```

**Characteristics:**
- ✅ Extremely high throughput (100k+ events/sec)
- ✅ Delivery guarantees (at-least-once)
- ✅ Horizontal scaling built-in (consumer groups)
- ✅ Replay capability (can re-process events)
- ⚠️ Requires Kafka infrastructure
- ⚠️ More operational complexity

### Implementation 4: MemoryQueue

**Good for:** Local development, testing, offline mode

```rust
pub struct MemoryQueue {
    channels: Arc<Mutex<HashMap<String, Vec<Sender<Uuid>>>>>,
}

impl QueueBackend for MemoryQueue {
    async fn notify(&self, event_type: &str, intent_ids: &[Uuid]) -> Result<()> {
        let channels = self.channels.lock().unwrap();

        if let Some(senders) = channels.get(event_type) {
            for sender in senders {
                for intent_id in intent_ids {
                    sender.send(*intent_id).await?;
                }
            }
        }
        Ok(())
    }

    async fn subscribe(&self, event_types: &[String]) -> Result<Receiver<Uuid>> {
        let (tx, rx) = mpsc::channel(1000);

        let mut channels = self.channels.lock().unwrap();
        for event_type in event_types {
            channels.entry(event_type.clone())
                .or_insert_with(Vec::new)
                .push(tx.clone());
        }

        Ok(rx)
    }
}
```

**Characteristics:**
- ✅ No external dependencies
- ✅ Perfect for tests
- ✅ Offline mode (no network required)
- ❌ Not durable (lost on restart)
- ❌ Not distributed (single process only)

### Usage (Pluggable at Runtime)

```rust
// Development: MemoryQueue
let queue = MemoryQueue::new();
let engine = Engine::new(deps, store, queue);

// Small scale: PostgresQueue
let queue = PostgresQueue::new(pool.clone());
let engine = Engine::new(deps, store, queue);

// Medium scale: RedisQueue
let redis_config = /* ... */;
let queue = RedisQueue::new(redis_config);
let engine = Engine::new(deps, store, queue);

// Ultra-high scale: KafkaQueue
let kafka_config = /* ... */;
let queue = KafkaQueue::new(kafka_config);
let engine = Engine::new(deps, store, queue);
```

---

## Part 4: Worker Model

### Worker Pool Architecture

**Workers subscribe to event type streams:**

```rust
// Option 1: Worker listens to ALL event types (most common)
let worker = Worker::builder()
    .engine(engine.clone())
    .concurrency(10)  // Process 10 intents concurrently
    .build();

worker.start().await?;

// Internally subscribes to all event type streams:
// - causal.OrderPlaced
// - causal.PaymentRequested
// - causal.UserSignedUp
// - ... etc
```

```rust
// Option 2: Worker specialized for specific event types (advanced)
let payment_worker = Worker::builder()
    .engine(engine.clone())
    .event_types(&["PaymentRequested", "PaymentCharged", "PaymentFailed"])
    .concurrency(5)
    .build();

let email_worker = Worker::builder()
    .engine(engine.clone())
    .event_types(&["EmailRequested", "EmailSent"])
    .concurrency(20)  // Emails are fast, high concurrency
    .build();

tokio::try_join!(
    payment_worker.start(),
    email_worker.start(),
)?;
```

### Worker Execution Loop

```rust
impl Worker {
    async fn run(&self) -> Result<()> {
        // Subscribe to configured event type streams
        let mut intent_stream = self.engine.queue
            .subscribe(&self.event_types)
            .await?;

        loop {
            // Receive intent ID from queue
            let intent_id = intent_stream.recv().await?;

            // Spawn concurrent task (respects concurrency limit)
            self.semaphore.acquire().await?;

            let engine = self.engine.clone();
            tokio::spawn(async move {
                // Try to claim intent (may fail if another worker claimed it)
                let intent = match engine.store.claim_intent(intent_id).await {
                    Ok(intent) => intent,
                    Err(AlreadyClaimed) => return Ok(()), // Another worker got it
                    Err(e) => return Err(e),
                };

                // Execute reactor (NO TRANSACTION HELD)
                let result = engine.execute_handler(&intent).await;

                // Persist result in separate transaction
                match result {
                    Ok(events) => {
                        engine.store.complete_intent(intent_id, events).await?;
                    }
                    Err(e) if should_retry(&e) => {
                        engine.store.retry_intent(intent_id).await?;
                    }
                    Err(e) => {
                        engine.store.fail_intent(intent_id, &e).await?;
                    }
                }

                Ok(())
            });
        }
    }
}
```

### Why Separate Streams Work

**Scenario:** System processing multiple event types

```
Event Stream: OrderPlaced, UserSignedUp, PaymentRequested, EmailSent, ...

OLD (Single Queue with Priority):
  All events → Single queue → Workers poll by priority
  Problem: OrderPlaced (slow) blocks PaymentRequested (fast)

NEW (Separate Streams):
  OrderPlaced      → "causal.OrderPlaced" stream      → Workers 1-3
  UserSignedUp     → "causal.UserSignedUp" stream     → Workers 1-3
  PaymentRequested → "causal.PaymentRequested" stream → Workers 1-3
  EmailSent        → "causal.EmailSent" stream        → Workers 1-3

  Result: OrderPlaced can be slow - doesn't affect other event types!
```

**Under Load:**

```
T=0s:    100 OrderPlaced events arrive
         → All go to "causal.OrderPlaced" stream
         → Workers start processing (slow reactors, 2s each)

T=0.5s:  100 EmailSent events arrive
         → All go to "causal.EmailSent" stream
         → Workers start processing (fast reactors, 10ms each)
         → Complete in 1 second ✅

Even though OrderPlaced is slow, EmailSent completes quickly!
```

---

## Part 5: API Changes

### Reactor Definition (No More `queued`)

**OLD:**
```rust
// Inline reactor (executes in dispatch transaction)
#[reactor(on = OrderPlaced)]
async fn save_order(event: OrderPlaced, ctx: Ctx) -> Result<OrderSaved> {
    ctx.deps().db.insert_order(&event).await?;
    Ok(OrderSaved { order_id: event.order_id })
}

// Background reactor (executes in worker)
#[reactor(on = PaymentRequested, queued, retry = 3)]
async fn charge_payment(event: PaymentRequested, ctx: Ctx) -> Result<PaymentCharged> {
    ctx.deps().stripe.charge(&event).await?;
    Ok(PaymentCharged { order_id: event.order_id })
}
```

**NEW:**
```rust
// ALL reactors execute in workers - no distinction!
#[reactor(on = OrderPlaced)]
async fn save_order(event: OrderPlaced, ctx: Ctx) -> Result<OrderSaved> {
    ctx.deps().db.insert_order(&event).await?;
    Ok(OrderSaved { order_id: event.order_id })
}

#[reactor(on = PaymentRequested, retry = 3)]
async fn charge_payment(event: PaymentRequested, ctx: Ctx) -> Result<PaymentCharged> {
    // Can take 1 minute - no problem!
    ctx.deps().stripe.charge(&event).await?;
    Ok(PaymentCharged { order_id: event.order_id })
}
```

**Key Change:** Removed `queued` attribute entirely. ALL reactors are queued (execute in workers).

### Dispatch API

**OLD Behavior:**
```rust
// Blocks until inline reactors complete
engine.dispatch(OrderPlaced { id: 123 }).await?;
// After this line, OrderSaved event already persisted
```

**NEW Behavior:**
```rust
// Returns after event persisted + intents queued (fast!)
engine.dispatch(OrderPlaced { id: 123 }).await?;
// After this line, reactor is QUEUED but may not have executed yet
```

### Wait API (New Feature)

For cases where you need synchronous-feeling behavior:

```rust
// Fire and forget (most common)
engine.dispatch(OrderPlaced { id: 123 }).await?;

// Wait for specific event
let result = engine.dispatch(OrderPlaced { id: 123 })
    .wait_for::<PaymentCharged>()
    .timeout(Duration::from_secs(30))
    .await?;

// Wait for any of multiple events
let result = engine.dispatch(OrderPlaced { id: 123 })
    .wait_for_any::<(PaymentCharged, PaymentFailed)>()
    .timeout(Duration::from_secs(30))
    .await?;

match result {
    Either::Left(charged) => println!("Success!"),
    Either::Right(failed) => println!("Failed: {}", failed.reason),
}

// Wait for ALL reactors to complete
engine.dispatch(OrderPlaced { id: 123 }).await?;
engine.settled().await?;  // Waits for all intents to finish
```

**Implementation:**
```rust
pub struct DispatchHandle {
    event_id: Uuid,
    engine: Engine,
}

impl DispatchHandle {
    pub fn wait_for<E: Event>(self) -> WaitFor<E> {
        WaitFor {
            event_id: self.event_id,
            engine: self.engine,
            _phantom: PhantomData,
        }
    }
}

impl<E: Event> WaitFor<E> {
    pub async fn await(self) -> Result<E> {
        // Poll database for event of type E with correlation_id matching event_id
        loop {
            let event = self.engine.store
                .find_event::<E>(self.event_id)
                .await?;

            if let Some(event) = event {
                return Ok(event);
            }

            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    pub fn timeout(self, duration: Duration) -> WaitForWithTimeout<E> {
        WaitForWithTimeout { inner: self, duration }
    }
}
```

---

## Part 6: Migration Path

### Breaking Changes

1. **Reactor execution model**
   - OLD: Inline reactors run in dispatch transaction
   - NEW: All reactors run in workers (async)

2. **Dispatch behavior**
   - OLD: `dispatch()` waits for inline reactors to complete
   - NEW: `dispatch()` returns after event persisted

3. **Attribute changes**
   - OLD: `#[reactor(on = E, queued)]` for background execution
   - NEW: `queued` attribute removed (all reactors are queued)

### Migration Steps

**Step 1: Update reactor attributes**

```rust
// OLD
#[reactor(on = OrderPlaced, queued, retry = 3)]

// NEW (just remove `queued`)
#[reactor(on = OrderPlaced, retry = 3)]
```

**Step 2: Update dispatch call sites**

If you relied on inline reactors completing before `dispatch()` returns:

```rust
// OLD (inline reactor guaranteed to complete)
engine.dispatch(OrderPlaced { id: 123 }).await?;
let order = db.query("SELECT * FROM orders WHERE id = 123").await?;
assert!(order.is_some());  // Always passes

// NEW (reactor might not have completed yet)
engine.dispatch(OrderPlaced { id: 123 }).await?;

// Option A: Use wait_for
engine.dispatch(OrderPlaced { id: 123 })
    .wait_for::<OrderSaved>()
    .await?;
let order = db.query("SELECT * FROM orders WHERE id = 123").await?;
assert!(order.is_some());  // Now passes

// Option B: Poll database until ready
loop {
    let order = db.query("SELECT * FROM orders WHERE id = 123").await?;
    if order.is_some() { break; }
    tokio::time::sleep(Duration::from_millis(100)).await;
}

// Option C: Redesign to be async (best!)
// Don't query immediately - let reactor emit event when done
engine.dispatch(OrderPlaced { id: 123 }).await?;
// Later, OrderSaved event triggers next step
```

**Step 3: Choose queue backend**

```rust
// Development: MemoryQueue
let engine = Engine::new(deps, store, MemoryQueue::new());

// Production: PostgresQueue (default)
let engine = Engine::new(deps, store, PostgresQueue::new(pool));

// High scale: KafkaQueue
let engine = Engine::new(deps, store, KafkaQueue::new(kafka_config));
```

**Step 4: Start workers**

```rust
// Before (workers started automatically)
let engine = Engine::new(deps, store);
// Workers started internally

// After (explicit worker management)
let engine = Engine::new(deps, store, queue);

// Start worker pool
let worker = Worker::builder()
    .engine(engine.clone())
    .concurrency(10)
    .build();

tokio::spawn(async move {
    worker.start().await
});
```

### Backward Compatibility (Optional)

For gradual migration, could add a `#[reactor(inline)]` attribute that errors:

```rust
#[reactor(on = OrderPlaced, inline)]
async fn reactor(...) { }

// Compile error:
// The `inline` attribute is no longer supported.
// All reactors execute asynchronously in workers.
// Remove the `inline` attribute.
// If you need synchronous-feeling behavior, use:
//   engine.dispatch(...).wait_for::<ResultEvent>().await?
```

---

## Part 7: Benefits & Trade-offs

### Benefits

#### Performance
- ✅ **Event dispatch always fast** (<10ms) regardless of reactor complexity
- ✅ **No transaction blocking** - reactors can take minutes without impact
- ✅ **Better resource utilization** - DB connections not held during slow operations
- ✅ **Natural parallelism** - separate streams per event type
- ✅ **Scales horizontally** - add workers without code changes

#### Scalability
- ✅ **Ultra-high throughput** with Kafka backend (100k+ events/sec)
- ✅ **Independent scaling** - scale workers per event type
- ✅ **No artificial limits** - reactors can be arbitrarily slow
- ✅ **Graceful degradation** - slow event types don't affect fast ones

#### Developer Experience
- ✅ **No priority decisions** - separate streams remove cognitive load
- ✅ **Simpler mental model** - event persistence is always fast
- ✅ **Clear failure model** - dispatch success != reactor success
- ✅ **Explicit waits** - when you need synchronous behavior, it's obvious

#### Correctness
- ✅ **No hidden blocking** - slow reactors can't block dispatch
- ✅ **Better observability** - clear separation of dispatch vs execution
- ✅ **Predictable behavior** - dispatch latency always <10ms

### Trade-offs

#### Lost Atomicity
- ❌ **Reactors not atomic with dispatch** - separate transactions
- ⚠️ **Implications:**
  - Dispatch can succeed even if reactor fails
  - Need to handle eventual consistency
  - Can't rely on reactor completing before dispatch returns

- ✅ **Mitigation:**
  - Use `.wait_for()` when you need synchronous behavior
  - Design for idempotency (reactors may retry)
  - Embrace async-first design (better anyway!)

#### Migration Effort
- ❌ **Breaking change** - requires code updates
- ❌ **Tests may need updates** - timing assumptions change
- ⚠️ **Effort estimate:** ~1-2 days for typical codebase

- ✅ **Worth it:**
  - Fixes critical footgun
  - Enables scale beyond toy examples
  - More correct architecture long-term

#### Operational Complexity
- ⚠️ **Workers must be managed** - no longer automatic
- ⚠️ **Need monitoring** - track queue depth, reactor latency

- ✅ **Standard practice:**
  - Similar to any job queue system
  - Tooling can help (worker pools, health checks)
  - Necessary for production systems anyway

---

## Part 8: Implementation Plan

### Phase 1: Core Architecture (Week 1)

**Tasks:**
1. Extract `QueueBackend` trait (already exists in codebase!)
2. Implement `PostgresQueue`, `MemoryQueue`
3. Remove inline execution from `EventWorker`
4. Move all reactor execution to `HandlerWorker`
5. Update `dispatch()` to only persist + queue

**Files to modify:**
- `crates/causal/src/queue_backend.rs` - Define trait
- `crates/causal/src/postgres_queue.rs` - Implement PostgresQueue
- `crates/causal/src/memory_queue.rs` - Implement MemoryQueue
- `crates/causal/src/engine_v2.rs` - Update dispatch logic
- `crates/causal/src/runtime/event_worker.rs` - Remove inline execution
- `crates/causal/src/runtime/handler_worker.rs` - Update to use streams

**Success criteria:**
- `cargo test` passes
- Dispatch always returns in <10ms
- All reactors execute in workers

### Phase 2: Wait API (Week 1)

**Tasks:**
1. Implement `DispatchHandle`
2. Implement `wait_for::<E>()`
3. Implement `wait_for_any::<(E1, E2)>()`
4. Implement `settled()`
5. Add timeout support

**Files to modify:**
- `crates/causal/src/engine_v2.rs` - Add wait API
- `crates/causal/src/wait.rs` - New module for wait logic

**Success criteria:**
- Can wait for specific events
- Timeouts work correctly
- Polls efficiently (no busy-waiting)

### Phase 3: Additional Queue Backends (Week 2)

**Tasks:**
1. Implement `RedisQueue`
2. Implement `KafkaQueue`
3. Add benchmarks comparing backends
4. Documentation for choosing backend

**Files to create:**
- `crates/causal-redis/` - New crate
- `crates/causal-kafka/` - New crate

**Success criteria:**
- Redis backend works
- Kafka backend works
- Benchmarks show expected throughput

### Phase 4: Documentation & Migration (Week 2)

**Tasks:**
1. Update CLAUDE.md
2. Update DISTRIBUTED-SAFETY.md
3. Write migration guide
4. Update all examples
5. Create runbook for operators

**Files to modify:**
- `CLAUDE.md` - Remove inline transaction docs
- `docs/DISTRIBUTED-SAFETY.md` - Update execution model
- `docs/MIGRATION-v0.11.md` - New migration guide
- `examples/**/*.rs` - Update all examples

**Success criteria:**
- Documentation accurate
- Examples work
- Migration guide clear

### Phase 5: Testing & Validation (Week 3)

**Tasks:**
1. Integration tests with all queue backends
2. Load testing (1k, 10k, 100k events/sec)
3. Chaos testing (worker crashes, network failures)
4. Production validation (canary deployment)

**Success criteria:**
- All tests pass
- No regressions
- Performance meets targets
- Production stable

---

## Part 9: Decision Points

### Decision 1: Keep Inline Reactors?

**Option A:** Remove inline reactors entirely (proposed)
- ✅ Simple, one execution model
- ✅ No footguns
- ❌ Breaking change

**Option B:** Keep inline reactors with timeout
- ✅ Less breaking change
- ❌ Still has blocking risk (timeout = 100ms still blocks)
- ❌ More complex (two execution models)

**Recommendation:** Option A (remove inline)

### Decision 2: Priority vs Separate Streams?

**Option A:** Separate streams per event type (proposed)
- ✅ No developer decisions needed
- ✅ Natural parallelism
- ✅ Maps to Kafka/Redis naturally

**Option B:** Single queue with priority
- ❌ Developers must choose priority
- ❌ Single bottleneck
- ❌ Hard to reason about

**Recommendation:** Option A (separate streams)

### Decision 3: Wait API Required?

**Option A:** Include `.wait_for()` API
- ✅ Easier migration
- ✅ Familiar pattern
- ⚠️ More API surface

**Option B:** No wait API, pure async only
- ✅ Simpler API
- ❌ Harder migration
- ❌ Forces async redesign

**Recommendation:** Option A (include wait API)

---

## Part 10: Success Criteria

### Performance Targets

- ✅ Event dispatch latency <10ms (p99)
- ✅ Reactor execution unlimited duration
- ✅ No database connection exhaustion under load
- ✅ PostgresQueue: 1,000 events/sec
- ✅ RedisQueue: 10,000 events/sec
- ✅ KafkaQueue: 100,000+ events/sec

### Correctness

- ✅ No lost events
- ✅ At-least-once reactor execution
- ✅ Idempotent reactor retries
- ✅ Graceful failure handling

### Developer Experience

- ✅ No priority configuration needed
- ✅ Clear error messages
- ✅ Easy migration path
- ✅ Comprehensive documentation

---

## Conclusion

**The current inline reactor execution model is a critical footgun that breaks at any meaningful scale.** Moving to an async-first architecture with separate queue streams per event type fixes this while enabling:

1. Reactors of any duration (seconds, minutes, hours)
2. Natural parallelism without priority decisions
3. Ultra-high throughput with Kafka/Redis backends
4. Clear mental model (dispatch = persist, workers = execute)

**This is a necessary breaking change to make Causal production-ready.**

---

## Next Steps

1. **Review this plan** - Ensure alignment on problem and solution
2. **Approve approach** - Confirm separate streams over priority
3. **Begin Phase 1** - Implement core architecture changes
4. **Iterate** - Refine based on feedback

**Estimated timeline:** 3 weeks to production-ready
**Estimated effort:** ~80 hours engineering time
