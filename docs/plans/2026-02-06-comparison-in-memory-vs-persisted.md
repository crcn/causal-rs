# Performance Comparison: In-Memory vs Persisted Handlers

**Date:** 2026-02-06

## TL;DR

| Approach | Throughput | Latency | Durability | Distribution | Use Case |
|----------|-----------|---------|------------|--------------|----------|
| **In-Memory Handlers** | 1M+ events/sec | <1ms | ❌ Lost on crash | ❌ Single process | Real-time, ephemeral |
| **Relay/Forward** | 500k+ events/sec | <5ms | ⚠️ At-most-once | ✅ Multi-process | Event forwarding |
| **Seesaw (Persisted)** | 1k-100k events/sec | 10-50ms | ✅ Durable | ✅ Multi-worker | Critical workflows |
| **Seesaw + In-Memory** | 1M+ events/sec | <1ms | ⚠️ Hybrid | ✅ Multi-worker | Hybrid approach |

**Key insight:** Different approaches for different problems. Seesaw prioritizes correctness and durability, but can adopt in-memory patterns when needed.

---

## Part 1: In-Memory Handlers (No Persistence)

### Architecture

```rust
// Pure in-memory - no database
struct Engine {
    handlers: Vec<Handler>,
}

engine.dispatch(event) {
    // No database writes!
    for handler in handlers {
        if handler.matches(&event) {
            let result = handler.execute(event).await?;
            // Recursively dispatch result events
            self.dispatch(result).await?;
        }
    }
}
```

### Performance Characteristics

**Throughput:**
```
Single thread: ~100,000 events/sec (10μs per event)
4 cores: ~400,000 events/sec
16 cores: ~1,600,000 events/sec

Limited by: CPU and memory bandwidth
```

**Latency:**
```
Handler execution: 1-100μs
Total dispatch: <1ms (no I/O)
```

**Resource usage:**
```
Memory: Proportional to in-flight events
CPU: High (all processing in-process)
Database: None (or read-only)
Network: Only for external APIs
```

### Example: In-Memory Event Bus

```rust
use tokio::sync::mpsc;

struct InMemoryEngine {
    handlers: Vec<Box<dyn Handler>>,
}

impl InMemoryEngine {
    async fn dispatch(&self, event: Event) -> Result<()> {
        // Execute all handlers immediately in memory
        for handler in &self.handlers {
            if handler.matches(&event) {
                // No persistence - just execute
                let result = handler.execute(&event).await?;

                // Recursively dispatch result events
                if let Some(result_event) = result {
                    Box::pin(self.dispatch(result_event)).await?;
                }
            }
        }
        Ok(())
    }
}

// Usage
let engine = InMemoryEngine::new();

engine.dispatch(OrderPlaced { id: 123 }).await?;
// Handlers execute immediately, no database writes
// Result: <1ms total latency
```

### Pros & Cons

**Pros:**
- ✅ **Extremely fast** - 1M+ events/sec, <1ms latency
- ✅ **Simple** - no database, no queues, just function calls
- ✅ **Low resource usage** - no database connections
- ✅ **Predictable** - no I/O variability

**Cons:**
- ❌ **Not durable** - events lost on crash/restart
- ❌ **Not distributed** - single process only (or complex coordination)
- ❌ **No replay** - can't reprocess events
- ❌ **No audit trail** - events disappear after processing
- ❌ **No retry** - failures lost forever
- ❌ **Memory-bound** - high throughput = high memory usage

### When to Use In-Memory

**Good for:**
1. **Real-time analytics** - metrics, counters, gauges
2. **Ephemeral workflows** - UI state management, WebSocket routing
3. **Non-critical events** - logging, debugging, telemetry
4. **Read-only processing** - aggregation, filtering, transformation
5. **High-frequency events** - sensor data, click streams (>10k/sec)

**Bad for:**
1. **Financial transactions** - need durability
2. **Critical workflows** - need retry and audit trail
3. **Long-running processes** - need persistence across restarts
4. **Compliance/audit** - need event history
5. **Distributed systems** - need coordination across processes

---

## Part 2: Relay Patterns

### Pattern 1: Fire-and-Forget Relay

**Architecture:**
```
Service A → Relay → Service B
          ↘
           Service C

Relay receives event, forwards to subscribers, doesn't persist
```

**Example: WebSocket Relay**
```rust
struct WebSocketRelay {
    subscribers: Arc<Mutex<HashMap<String, Vec<WebSocket>>>>,
}

impl WebSocketRelay {
    async fn relay(&self, event: Event) -> Result<()> {
        let subscribers = self.subscribers.lock().unwrap();

        // Forward to all subscribers (fire-and-forget)
        for (_, sockets) in subscribers.iter() {
            for socket in sockets {
                // Don't wait for confirmation
                let _ = socket.send(&event).await;
            }
        }

        Ok(())
    }
}
```

**Performance:**
```
Throughput: 500k+ events/sec
Latency: <5ms
Durability: None (fire-and-forget)
```

**Use cases:**
- Real-time notifications (WebSockets, SSE)
- Pub/sub for UI updates
- Live dashboards
- Chat applications

### Pattern 2: Kafka Relay (At-Least-Once)

**Architecture:**
```
Service A → Kafka Topic → Consumer Group (Service B, C, D)

Kafka persists events, consumers poll and process
```

**Example:**
```rust
// Producer (relay into Kafka)
let producer = FutureProducer::new(kafka_config);

async fn relay_to_kafka(event: Event) -> Result<()> {
    let record = FutureRecord::to("events")
        .payload(&serde_json::to_vec(&event)?);

    producer.send(record, Duration::from_secs(0)).await?;
    Ok(())
}

// Consumer (relay out of Kafka)
let consumer = StreamConsumer::new(kafka_config);
consumer.subscribe(&["events"])?;

while let Some(message) = consumer.stream().next().await {
    let event: Event = serde_json::from_slice(message.payload())?;

    // Process event
    handle_event(event).await?;

    // Commit offset (at-least-once)
    consumer.commit_message(&message, CommitMode::Async)?;
}
```

**Performance:**
```
Throughput: 100k+ events/sec
Latency: 5-50ms
Durability: Yes (Kafka retention)
Distribution: Yes (consumer groups)
```

**Use cases:**
- Event streaming between microservices
- Log aggregation
- Change data capture (CDC)
- Event sourcing

### Pattern 3: Redis Pub/Sub Relay (At-Most-Once)

**Architecture:**
```
Service A → Redis Pub/Sub → Subscribers (Service B, C, D)

Redis forwards events to active subscribers only
```

**Example:**
```rust
// Publisher
let client = redis::Client::open("redis://localhost")?;
let mut conn = client.get_async_connection().await?;

async fn relay_to_redis(event: Event) -> Result<()> {
    redis::cmd("PUBLISH")
        .arg("events")
        .arg(serde_json::to_string(&event)?)
        .query_async(&mut conn)
        .await?;
    Ok(())
}

// Subscriber
let mut pubsub = conn.into_pubsub();
pubsub.subscribe("events").await?;

while let Some(msg) = pubsub.on_message().next().await {
    let event: Event = serde_json::from_str(&msg.get_payload::<String>()?)?;
    handle_event(event).await?;
}
```

**Performance:**
```
Throughput: 50k+ events/sec
Latency: <5ms
Durability: No (pub/sub is fire-and-forget)
Distribution: Yes (multiple subscribers)
```

**Use cases:**
- Cache invalidation
- Real-time notifications
- UI state sync
- Session management

### Pattern 4: NATS Relay (Various Guarantees)

**Architecture:**
```
Service A → NATS → Subscribers

NATS supports:
- Core NATS: At-most-once (fire-and-forget)
- JetStream: At-least-once (persisted)
```

**Example:**
```rust
// Publisher
let nc = nats::connect("nats://localhost")?;

async fn relay_to_nats(event: Event) -> Result<()> {
    nc.publish("events", &serde_json::to_vec(&event)?)?;
    Ok(())
}

// Subscriber
let sub = nc.subscribe("events")?;
for msg in sub.messages() {
    let event: Event = serde_json::from_slice(&msg.data)?;
    handle_event(event).await?;
}
```

**Performance:**
```
Core NATS:
  Throughput: 500k+ events/sec
  Latency: <1ms
  Durability: No

JetStream:
  Throughput: 100k+ events/sec
  Latency: 5-10ms
  Durability: Yes
```

**Use cases:**
- Microservice communication
- Request/reply patterns
- Service discovery
- Distributed systems

---

## Part 3: Seesaw vs In-Memory/Relay

### Architectural Comparison

| Aspect | In-Memory | Relay (Fire-and-Forget) | Relay (Kafka) | Seesaw |
|--------|-----------|------------------------|---------------|--------|
| **Event Persistence** | ❌ No | ❌ No | ✅ Yes (Kafka) | ✅ Yes (Postgres) |
| **Handler Persistence** | ❌ No | ❌ No | ⚠️ Manual | ✅ Yes (intent tracking) |
| **At-Least-Once** | ❌ No | ❌ No | ✅ Yes | ✅ Yes |
| **Exactly-Once** | ⚠️ Maybe | ❌ No | ⚠️ Complex | ⚠️ Idempotency |
| **Retry Logic** | ❌ Manual | ❌ Manual | ❌ Manual | ✅ Built-in |
| **Audit Trail** | ❌ No | ❌ No | ✅ Yes | ✅ Yes |
| **Distribution** | ⚠️ Complex | ✅ Yes | ✅ Yes | ✅ Yes |
| **Replay** | ❌ No | ❌ No | ✅ Yes | ✅ Yes |

### Performance Comparison

**Throughput:**
```
In-Memory:           1,000,000 events/sec   (pure compute)
Redis Pub/Sub:         500,000 events/sec   (network + pub/sub)
NATS:                  500,000 events/sec   (network + routing)
Kafka:                 100,000 events/sec   (network + persistence)
Seesaw + Kafka:        100,000 events/sec   (persistence + intent tracking)
Seesaw + Redis:         10,000 events/sec   (Postgres writes)
Seesaw + Postgres:       1,000 events/sec   (pg_notify limit)
```

**Latency:**
```
In-Memory:           <1ms      (function call)
Redis Pub/Sub:       1-5ms     (network + pub/sub)
NATS:                1-5ms     (network + routing)
Kafka:               5-50ms    (network + disk + replication)
Seesaw + Kafka:      10-50ms   (Postgres + Kafka + intent tracking)
Seesaw + Redis:      10-30ms   (Postgres writes + Redis)
Seesaw + Postgres:   10-30ms   (Postgres writes + pg_notify)
```

### Use Case Comparison

**Scenario 1: Real-Time Analytics Dashboard**

**Requirements:**
- 10,000 events/sec (click streams)
- <10ms latency
- Ephemeral (don't need history)

**Best approach: In-Memory**
```rust
// Just aggregate in memory
struct AnalyticsEngine {
    counters: Arc<Mutex<HashMap<String, u64>>>,
}

impl AnalyticsEngine {
    async fn track(&self, event: ClickEvent) {
        let mut counters = self.counters.lock().unwrap();
        *counters.entry(event.page).or_insert(0) += 1;
    }
}

// Result: 1M events/sec, <1ms latency
```

**Why not Seesaw:** Overkill, don't need persistence/retry

---

**Scenario 2: Payment Processing**

**Requirements:**
- 100 payments/sec
- Must be durable
- Must retry failures
- Need audit trail

**Best approach: Seesaw**
```rust
#[handler(on = PaymentRequested, retry = 3)]
async fn charge_payment(event: PaymentRequested, ctx: Ctx) -> Result<PaymentCharged> {
    ctx.deps().stripe.charge(&event).await?;
    Ok(PaymentCharged { order_id: event.order_id })
}

// Result: Durable, retriable, auditable
```

**Why not in-memory:** Payments too critical, need durability

---

**Scenario 3: Microservice Event Bus**

**Requirements:**
- 1,000 events/sec
- Services subscribe to specific events
- Fire-and-forget (don't need durability)
- Low latency

**Best approach: Redis Pub/Sub or NATS**
```rust
// Service A publishes
redis::cmd("PUBLISH")
    .arg("user.created")
    .arg(&event)
    .query_async(&mut conn)
    .await?;

// Service B subscribes
let mut pubsub = conn.into_pubsub();
pubsub.subscribe("user.created").await?;
```

**Why not Seesaw:** Don't need durability, want lower latency

---

**Scenario 4: Long-Running Workflow**

**Requirements:**
- Multi-step process (takes minutes)
- Must survive restarts
- Need retry logic
- Need to track state

**Best approach: Seesaw**
```rust
#[handler(on = WorkflowStarted)]
async fn step1(...) -> Result<Step1Complete> { ... }

#[handler(on = Step1Complete)]
async fn step2(...) -> Result<Step2Complete> { ... }

#[handler(on = Step2Complete)]
async fn step3(...) -> Result<WorkflowComplete> { ... }

// Result: Durable, survives restarts, auditable
```

**Why not in-memory:** Would lose state on restart

---

**Scenario 5: Hybrid - Hot Path + Persistence**

**Requirements:**
- Real-time updates (WebSocket)
- Also persist for later (audit trail)
- 1,000 events/sec

**Best approach: In-Memory + Seesaw**
```rust
// In-memory relay for real-time
struct RealtimeRelay {
    subscribers: Arc<Mutex<Vec<WebSocket>>>,
}

// Seesaw handler for persistence
#[handler(on = OrderPlaced)]
async fn persist_order(event: OrderPlaced, ctx: Ctx) -> Result<OrderSaved> {
    // Persist to database
    ctx.deps().db.insert_order(&event).await?;

    // Also relay to WebSocket clients (in-memory)
    ctx.deps().realtime.broadcast(&event).await?;

    Ok(OrderSaved { order_id: event.order_id })
}

// Result: <10ms WebSocket updates + durable persistence
```

---

## Part 4: Hybrid Approaches

### Approach 1: Seesaw with In-Memory Fast Path

**Architecture:**
```
Event arrives
  ↓
In-Memory Handler (instant, ephemeral)
  ↓
Persisted Handler (durable, slower)
```

**Implementation:**
```rust
struct HybridEngine {
    memory_handlers: Vec<Handler>,   // Execute immediately
    persisted_handlers: Vec<Handler>, // Execute via Seesaw
}

impl HybridEngine {
    async fn dispatch(&self, event: Event) -> Result<()> {
        // Fast path: Execute in-memory handlers immediately
        for handler in &self.memory_handlers {
            tokio::spawn({
                let event = event.clone();
                async move { handler.execute(event).await }
            });
        }

        // Slow path: Persist and queue
        self.seesaw.dispatch(event).await?;

        Ok(())
    }
}
```

**Performance:**
```
In-memory handlers: <1ms (fire-and-forget)
Persisted handlers: 10-50ms (durable)

Throughput: 1M+ events/sec (limited by memory handlers)
Durability: Only persisted handlers
```

**Use case:**
```rust
// Real-time notification (in-memory, ephemeral)
#[in_memory_handler(on = OrderPlaced)]
async fn notify_websocket(event: OrderPlaced, ctx: Ctx) -> Result<()> {
    ctx.deps().websocket.broadcast(&event).await?;
    Ok(())
}

// Audit trail (persisted, durable)
#[handler(on = OrderPlaced)]
async fn save_order(event: OrderPlaced, ctx: Ctx) -> Result<OrderSaved> {
    ctx.deps().db.insert_order(&event).await?;
    Ok(OrderSaved { order_id: event.order_id })
}

// Result: Instant WebSocket updates + durable database record
```

### Approach 2: Relay + Seesaw

**Architecture:**
```
High-volume events → Kafka (relay)
                     ↓
                   Seesaw workers consume and process
                     ↓
                   Results → Postgres (durable state)
```

**Implementation:**
```rust
// Kafka producer (relay in)
async fn ingest_events(kafka: &FutureProducer) {
    loop {
        let event = receive_external_event().await?;

        // Write to Kafka (fast, durable)
        kafka.send(
            FutureRecord::to("events")
                .payload(&serde_json::to_vec(&event)?)
        ).await?;
    }
}

// Seesaw workers (consume from Kafka)
async fn consume_and_process(kafka: &StreamConsumer, engine: &Engine) {
    while let Some(message) = kafka.stream().next().await {
        let event: Event = serde_json::from_slice(message.payload())?;

        // Process through Seesaw (with retry, audit trail)
        engine.dispatch(event).await?;

        // Commit Kafka offset
        kafka.commit_message(&message, CommitMode::Async)?;
    }
}
```

**Performance:**
```
Kafka ingestion: 100k+ events/sec
Seesaw processing: Limited by workers (scales horizontally)
End-to-end latency: 50-100ms
```

**Benefits:**
- ✅ Kafka handles high-volume ingestion
- ✅ Seesaw handles complex workflows with retry
- ✅ Best of both worlds

### Approach 3: Tiered Processing

**Architecture:**
```
Tier 1: In-Memory (fast, ephemeral)
  ↓
Tier 2: Redis Relay (medium, fire-and-forget)
  ↓
Tier 3: Seesaw (slow, durable)
```

**Use case:**
```rust
// Tier 1: Real-time metrics (in-memory)
#[in_memory_handler(on = ApiRequest)]
async fn track_metrics(event: ApiRequest) {
    METRICS.increment("api.requests");
}

// Tier 2: Cache invalidation (Redis pub/sub)
#[redis_handler(on = UserUpdated)]
async fn invalidate_cache(event: UserUpdated, ctx: Ctx) {
    ctx.deps().redis.publish("cache.invalidate", &event).await?;
}

// Tier 3: Database persistence (Seesaw)
#[handler(on = UserUpdated, retry = 3)]
async fn update_database(event: UserUpdated, ctx: Ctx) -> Result<UserSaved> {
    ctx.deps().db.update_user(&event).await?;
    Ok(UserSaved { user_id: event.user_id })
}
```

---

## Part 5: When to Use Each Approach

### Decision Tree

```
Do you need durability? (survive crashes/restarts)
  ↓ No  → In-Memory or Fire-and-Forget Relay
  ↓ Yes → Continue

Do you need at-least-once delivery?
  ↓ No  → Kafka/Redis (best-effort)
  ↓ Yes → Continue

Do you need retry logic?
  ↓ No  → Kafka with manual retry
  ↓ Yes → Continue

Do you need complex workflows? (multi-step, branching)
  ↓ No  → Simple Kafka consumers
  ↓ Yes → Seesaw

Do you need >10k events/sec?
  ↓ No  → Seesaw with PostgresQueue or RedisQueue
  ↓ Yes → Seesaw with KafkaQueue or Hybrid (in-memory + Seesaw)
```

### Summary Table

| Use Case | Approach | Why |
|----------|----------|-----|
| **Real-time analytics** | In-Memory | Speed > durability |
| **UI notifications** | WebSocket Relay | Low latency, ephemeral |
| **Cache invalidation** | Redis Pub/Sub | Fire-and-forget, low latency |
| **Event sourcing** | Kafka | Durable, replayable |
| **Payment processing** | Seesaw | Critical, needs retry + audit |
| **Long workflows** | Seesaw | Multi-step, durable state |
| **Microservice events** | NATS or Redis | Low latency, fire-and-forget |
| **High-volume + critical** | Kafka + Seesaw | Best of both |
| **Mixed requirements** | Hybrid | Combine approaches |

---

## Part 6: Seesaw's Position

### What Seesaw Optimizes For

1. **Correctness** - At-least-once delivery, retry logic
2. **Durability** - Events and handler state persisted
3. **Auditability** - Full event history
4. **Workflow orchestration** - Multi-step processes
5. **Developer experience** - Simple API, fewer decisions

### What Seesaw Sacrifices

1. **Raw throughput** - 1k-100k vs 1M+ for in-memory
2. **Latency** - 10-50ms vs <1ms for in-memory
3. **Operational simplicity** - Requires database + queue

### When Seesaw is the Right Choice

- ✅ Financial transactions
- ✅ Order processing
- ✅ User workflows
- ✅ Compliance/audit requirements
- ✅ Multi-step sagas
- ✅ Need retry logic
- ✅ Need replay capability

### When Seesaw is NOT the Right Choice

- ❌ Real-time analytics (>10k events/sec, ephemeral)
- ❌ UI state management (in-process only)
- ❌ Pub/sub notifications (fire-and-forget)
- ❌ Simple event forwarding (relay is simpler)
- ❌ Extreme latency requirements (<1ms)

### The Hybrid Sweet Spot

**Best of both worlds:**
```rust
// Use in-memory for hot path
#[in_memory_handler(on = OrderPlaced)]
async fn notify_realtime(event: OrderPlaced, ctx: Ctx) {
    ctx.deps().websocket.broadcast(&event).await?;
}

// Use Seesaw for durability
#[handler(on = OrderPlaced, retry = 3)]
async fn persist_order(event: OrderPlaced, ctx: Ctx) -> Result<OrderSaved> {
    ctx.deps().db.insert_order(&event).await?;
    Ok(OrderSaved { order_id: event.order_id })
}
```

**Result:**
- Fast: <1ms WebSocket updates
- Durable: Persisted to database with retry
- Correct: Both happen independently

---

## Conclusion

**Seesaw is not trying to compete with in-memory handlers on raw throughput.** It optimizes for different concerns:

| Concern | In-Memory | Relay | Seesaw |
|---------|-----------|-------|--------|
| **Speed** | ✅ 1M/sec | ✅ 500k/sec | ⚠️ 1k-100k/sec |
| **Durability** | ❌ No | ⚠️ Maybe | ✅ Yes |
| **Correctness** | ⚠️ Manual | ⚠️ Manual | ✅ Built-in |
| **Workflows** | ❌ Manual | ❌ Manual | ✅ Built-in |
| **Audit Trail** | ❌ No | ⚠️ Maybe | ✅ Yes |

**The key insight:** Different tools for different problems. Seesaw can adopt in-memory patterns when needed (hybrid approach), but its core value is durability + correctness + workflow orchestration.

**For most production systems,** the hybrid approach gives you the best of both worlds:
- In-memory for real-time/ephemeral
- Seesaw for critical/durable
- Relay for pub/sub/notification

**The new async-first architecture makes Seesaw scale 10-1000x better,** bringing it into the range where it can handle production workloads while maintaining its correctness guarantees.
