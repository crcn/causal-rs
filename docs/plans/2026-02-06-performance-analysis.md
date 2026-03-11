# Performance Analysis - Async-First Architecture

**Date:** 2026-02-06

## Executive Summary

**OLD Architecture:** Throughput limited by slowest reactor (1 event per reactor duration)
**NEW Architecture:** Throughput limited by database write speed + queue backend

**Key Metrics:**
- Dispatch latency: <10ms (p99)
- PostgresQueue: ~1,000 events/sec
- RedisQueue: ~10,000 events/sec
- KafkaQueue: ~100,000+ events/sec
- Reactor execution: Unlimited duration, scales horizontally

---

## Part 1: Current (Broken) Performance

### Throughput Calculation

**Formula:** `Throughput = 1 / (dispatch_time + slowest_inline_handler_time)`

**Example 1: Fast reactors**
```
Slowest reactor: 100ms
Throughput: 1 / 0.1s = 10 events/sec per worker

With 10 workers: ~100 events/sec max
```

**Example 2: Slow reactors (typical)**
```
Slowest reactor: 2 seconds (external API call)
Throughput: 1 / 2s = 0.5 events/sec per worker

With 10 workers: ~5 events/sec max
With 100 workers: ~50 events/sec max (but hitting DB connection limits!)
```

**Example 3: Very slow reactors (realistic)**
```
Slowest reactor: 60 seconds (AI processing)
Throughput: 1 / 60s = 0.016 events/sec per worker

With 10 workers: ~0.16 events/sec = 576 events/hour
With 100 workers: ~1.6 events/sec = 5,760 events/hour

Problem: 100 workers × 60s = holding 100 DB connections for 1 minute each!
```

### The Blocking Problem

**Scenario:** Processing 1,000 OrderPlaced events with one slow reactor (2 seconds)

```
Worker 1: Event #1   [TX held for 2s] → Done
Worker 2: Event #2   [TX held for 2s] → Done
Worker 3: Event #3   [TX held for 2s] → Done
...

With 10 workers:
  Time to process 1,000 events = 1,000 / 10 = 100 batches
  100 batches × 2s each = 200 seconds (3.3 minutes)

With 100 workers:
  Time to process 1,000 events = 1,000 / 100 = 10 batches
  10 batches × 2s each = 20 seconds
  BUT: 100 workers × 2s = 100 connections held for 2 seconds
```

**Database Connection Exhaustion:**
```
Postgres connection pool: 100 connections (typical)
Workers: 100
Reactor duration: 2 seconds

Scenario:
  T=0s:   100 events arrive
  T=0s:   All 100 workers claim events
  T=0s:   All 100 DB connections in use
  T=0.1s: New events arrive → NO CONNECTIONS AVAILABLE
  T=2s:   Workers finish, connections released
```

### Scalability Limits (Current)

| Workers | Reactor Duration | Max Throughput | DB Connections | Feasible? |
|---------|-----------------|----------------|----------------|-----------|
| 10 | 100ms | 100 events/sec | 10 | ✅ Yes |
| 10 | 2s | 5 events/sec | 10 | ⚠️ Slow |
| 100 | 2s | 50 events/sec | 100 | ⚠️ Pool exhausted |
| 100 | 60s | 1.6 events/sec | 100 | ❌ Unusable |
| 1000 | 2s | 500 events/sec | 1000 | ❌ Too many connections |

**Conclusion:** Can't scale horizontally without exhausting database connections.

---

## Part 2: New Architecture Performance

### Throughput Calculation

**Dispatch Throughput = Database Write Throughput**

**Components:**
1. Event insert: ~3ms
2. Reactor intent inserts (N reactors): ~2ms per reactor
3. Queue notification: ~1ms (Postgres) / ~0.1ms (Redis) / ~0.01ms (Kafka)
4. Transaction commit: ~2ms

**Total dispatch time:**
```
PostgresQueue:  3 + (2 × N) + 1 + 2 = 6 + 2N ms
RedisQueue:     3 + (2 × N) + 0.1 + 2 = 5.1 + 2N ms
KafkaQueue:     3 + (2 × N) + 0.01 + 2 = 5.01 + 2N ms
```

**Example with 5 reactors:**
```
PostgresQueue:  6 + 10 = 16ms per event → ~62 events/sec per dispatcher
RedisQueue:     5.1 + 10 = 15.1ms per event → ~66 events/sec per dispatcher
KafkaQueue:     5.01 + 10 = 15.01ms per event → ~66 events/sec per dispatcher
```

**With 10 dispatcher workers:**
```
PostgresQueue:  ~620 events/sec
RedisQueue:     ~660 events/sec
KafkaQueue:     ~660 events/sec
```

### Reactor Execution Throughput (Independent!)

**Reactor execution is now SEPARATE from dispatch:**

```
Dispatch workers (10):  Handle event persistence
  ↓
Queue (PostgresQueue/Redis/Kafka): Distribute intents
  ↓
Reactor workers (100+): Execute reactors asynchronously
```

**Reactor throughput = Number of reactor workers × (1 / handler_duration)**

**Example: 2-second reactor, 100 reactor workers**
```
Throughput: 100 workers × (1 / 2s) = 50 reactors/sec
```

**Key insight:** Can scale reactor workers independently of dispatch workers!

### Scalability Comparison

**Scenario:** 1,000 events/sec, reactor takes 2 seconds

**OLD Architecture:**
```
Need: 1,000 events/sec × 2s = 2,000 worker-seconds
Workers needed: 2,000

Problem:
  - 2,000 database connections needed
  - Impossible on most databases
  - Each worker holds transaction for 2s
```

**NEW Architecture:**
```
Dispatch:
  Need: 1,000 events/sec ÷ 62 events/sec per dispatcher = 17 dispatchers
  DB connections: 17 (one per dispatcher)

Reactors:
  Need: 1,000 events/sec × 2s = 2,000 worker-seconds
  Workers needed: 2,000
  DB connections: 0 during execution! (only during result persistence)

Total DB connections: 17 dispatchers + ~100 for reactor results = ~120
```

**Result:**
- ✅ Feasible with standard DB connection pool (100-200 connections)
- ✅ Dispatch workers handle persistence (fast, < 10ms)
- ✅ Reactor workers scale independently (no DB connection during execution)

---

## Part 3: Queue Backend Comparison

### PostgresQueue (pg_notify)

**Throughput:**
```
Dispatch: ~620 events/sec (limited by DB write speed)
Notification: ~1,000 events/sec (pg_notify limit)
Worker polling: ~1,000 events/sec (LISTEN/NOTIFY)

Bottleneck: pg_notify (~1,000 events/sec)
```

**Characteristics:**
- ✅ Built-in (no extra infrastructure)
- ✅ Reliable (transactional)
- ✅ Simple setup
- ❌ Limited throughput (~1k events/sec)
- ❌ Not suitable for high-scale production

**Best for:**
- Development/staging
- Low-traffic production (<500 events/sec)
- Simple deployments

**Scaling limits:**
```
Single Postgres instance: ~1,000 events/sec
Multiple instances: Cannot scale horizontally (pg_notify is per-instance)
```

### RedisQueue

**Throughput:**
```
Dispatch: ~620 events/sec (limited by Postgres writes for events)
Notification: ~50,000 messages/sec (Redis pub/sub)
Worker polling: ~10,000 events/sec (limited by Postgres intent claims)

Bottleneck: Postgres intent claims (~10k/sec)
```

**Characteristics:**
- ✅ High throughput (~10k events/sec)
- ✅ Low latency (<1ms notification)
- ✅ Horizontal scaling (Redis Cluster)
- ⚠️ Fire-and-forget (no delivery guarantees)
- ⚠️ Requires Redis infrastructure

**Best for:**
- Medium-scale production (1k-10k events/sec)
- Low-latency requirements
- Acceptable to lose notifications (workers poll anyway)

**Scaling limits:**
```
Single Redis: ~10,000 events/sec (limited by Postgres writes)
Redis Cluster: Same (Postgres is bottleneck)
```

### KafkaQueue

**Throughput:**
```
Dispatch: ~620 events/sec (limited by Postgres writes for events)
Notification: ~1,000,000 messages/sec (Kafka)
Worker polling: ~100,000 events/sec (Kafka consumer groups)

Bottleneck: Postgres event writes (~620/sec per dispatcher)
```

**Wait, Postgres is still the bottleneck?**

**Solution: Use Kafka for EVERYTHING, not just notifications**

```rust
// Option 1: Postgres for events + Kafka for notifications
// Bottleneck: Postgres writes (~620 events/sec per dispatcher)

// Option 2: Kafka for events + Postgres for intents/state
// Bottleneck: Postgres intent writes (~10k/sec)

// Option 3: Kafka for EVERYTHING (event sourcing)
// Bottleneck: Kafka (~1M events/sec)
```

**With Option 3 (full Kafka):**
```
Events stored in Kafka topics
Reactor intents tracked in Kafka
State snapshots in Postgres (optional)

Throughput: ~100,000+ events/sec
Bottleneck: Kafka consumer processing
```

**Characteristics:**
- ✅ Extremely high throughput (100k+ events/sec)
- ✅ Delivery guarantees (at-least-once)
- ✅ Horizontal scaling (partitions + consumer groups)
- ✅ Replay capability
- ⚠️ Requires Kafka infrastructure
- ⚠️ More operational complexity
- ⚠️ May need to rethink Store trait (use Kafka as event store)

**Best for:**
- High-scale production (10k+ events/sec)
- Systems requiring replay
- Already using Kafka

**Scaling limits:**
```
Single Kafka cluster: ~100,000 events/sec (with Kafka as event store)
Multiple clusters: Can scale further if needed
```

### MemoryQueue

**Throughput:**
```
Dispatch: ~620 events/sec (limited by Postgres writes)
Notification: ~1,000,000 messages/sec (in-memory channels)
Worker polling: ~50,000 events/sec (in-memory)

Bottleneck: Postgres writes (~620/sec per dispatcher)
```

**Characteristics:**
- ✅ Fastest for local/testing
- ✅ No external dependencies
- ❌ Not durable (lost on restart)
- ❌ Not distributed (single process only)

**Best for:**
- Testing
- Local development
- Offline mode

---

## Part 4: Bottleneck Analysis

### Bottleneck 1: Postgres Event Writes

**Current limitation:**
```
Single INSERT into causal_events: ~3ms
N INSERT into causal_handler_intents: ~2ms each

Total: 3 + 2N ms per event

With 5 reactors:
  3 + 10 = 13ms per event
  = ~77 events/sec per dispatcher
```

**Mitigation 1: Batch inserts**
```rust
// Instead of:
for event in events {
    tx.execute("INSERT INTO causal_events ...").await?;
}

// Do:
tx.execute("INSERT INTO causal_events VALUES ($1), ($2), ($3), ...").await?;

// Result: 100 events in ~50ms = 2,000 events/sec
```

**Mitigation 2: Multiple dispatchers**
```
1 dispatcher: ~77 events/sec
10 dispatchers: ~770 events/sec
50 dispatchers: ~3,850 events/sec

Limited by database write throughput
```

**Mitigation 3: Postgres tuning**
```sql
-- Increase shared_buffers
shared_buffers = 8GB

-- Disable synchronous_commit for higher throughput (slightly less durable)
synchronous_commit = off

-- Increase max_wal_size
max_wal_size = 4GB

Result: ~5-10x improvement (5k-10k events/sec)
```

**Mitigation 4: Use Kafka as event store**
```
Events stored in Kafka (not Postgres)
Postgres only for intent claims and state

Result: ~100k+ events/sec
```

### Bottleneck 2: Reactor Intent Claims

**Current limitation:**
```sql
-- Each worker polls:
SELECT * FROM causal_handler_intents
WHERE status = 'pending'
ORDER BY priority, created_at
FOR UPDATE SKIP LOCKED
LIMIT 10;

-- With 100 workers polling every 100ms:
100 workers × 10 queries/sec = 1,000 queries/sec

Each query: ~5ms
= ~5,000ms of database time per second
= Saturated at ~200 workers
```

**Mitigation 1: Longer polling intervals**
```
Poll every 1 second instead of 100ms
= 100 workers × 1 query/sec = 100 queries/sec
= Can scale to 2,000 workers
```

**Mitigation 2: Event-driven polling (queue notifications)**
```
Workers only poll when notified by queue
No polling when no work available

Result: Query load proportional to work, not workers
```

**Mitigation 3: Partition intents by event type**
```sql
-- Separate table per event type (or partitioned table)
causal_handler_intents_OrderPlaced
causal_handler_intents_PaymentRequested
...

Workers specialized per event type query only relevant partition

Result: Less lock contention, higher throughput
```

### Bottleneck 3: Queue Throughput

**PostgresQueue:**
```
pg_notify limit: ~1,000 notifications/sec

Mitigation: Batch notifications
  Instead of 1 NOTIFY per intent
  Group into: NOTIFY per event (multiple intents)

Result: ~5,000 intents/sec (5 reactors per event × 1k events/sec)
```

**RedisQueue:**
```
Redis pub/sub: ~50,000 messages/sec

Not a bottleneck until very high scale
```

**KafkaQueue:**
```
Kafka: ~1,000,000 messages/sec

Not a bottleneck
```

---

## Part 5: Load Testing Scenarios

### Scenario 1: Moderate Load (1,000 events/sec)

**Setup:**
- 1,000 OrderPlaced events/sec
- 5 reactors per event (3 fast, 2 slow)
- Reactor durations: 10ms, 50ms, 100ms, 2s, 5s

**OLD Architecture:**
```
Slowest reactor: 5s
Throughput: 1 / 5s = 0.2 events/sec per worker

Workers needed: 1,000 / 0.2 = 5,000 workers
DB connections: 5,000

Result: ❌ Impossible (DB connection exhaustion)
```

**NEW Architecture (PostgresQueue):**
```
Dispatch:
  1,000 events/sec ÷ 62 events/sec per dispatcher = 17 dispatchers
  DB connections: 17

Reactors:
  Fast reactors (10ms): 1,000 events/sec × 3 reactors = 3,000 executions/sec
    Workers needed: 3,000 × 0.01s = 30 workers

  Fast reactors (50ms): 1,000 × 0.05s = 50 workers

  Medium reactors (100ms): 1,000 × 0.1s = 100 workers

  Slow reactors (2s): 1,000 × 2s = 2,000 workers

  Very slow reactors (5s): 1,000 × 5s = 5,000 workers

Total reactor workers: 30 + 50 + 100 + 2,000 + 5,000 = 7,180 workers
DB connections during execution: 0
DB connections for results: ~100 (burst)

Total DB connections: 17 + 100 = 117

Result: ✅ Feasible!

But wait... PostgresQueue limit: 1,000 events/sec
Actual throughput: 1,000 events/sec (at queue limit)
```

**NEW Architecture (RedisQueue):**
```
Same as above, but:
Queue limit: 10,000 events/sec
Actual throughput: 1,000 events/sec (under limit)

Result: ✅ Plenty of headroom
```

### Scenario 2: High Load (10,000 events/sec)

**NEW Architecture (PostgresQueue):**
```
Queue limit: 1,000 events/sec
Actual throughput: 1,000 events/sec

Result: ❌ Queue bottleneck
```

**NEW Architecture (RedisQueue):**
```
Dispatch:
  10,000 events/sec ÷ 62 events/sec = 162 dispatchers
  DB connections: 162

Reactors:
  7,180 workers × 10 = 71,800 workers needed
  DB connections: 162 + 1,000 = ~1,200

Result: ⚠️ DB connection pressure
Mitigation: Batch result inserts, tune Postgres
```

**NEW Architecture (KafkaQueue + Kafka Event Store):**
```
Dispatch:
  Events written to Kafka: ~100,000 events/sec capacity
  Actual: 10,000 events/sec

Reactors:
  71,800 workers
  DB connections: ~100 (only for state snapshots)

Result: ✅ Easily handles load
```

### Scenario 3: Burst Load (100 events → 10,000 events)

**OLD Architecture:**
```
Steady: 100 events/sec → 500 workers (manageable)
Burst: 10,000 events/sec → 50,000 workers needed

Problem: Can't spin up 50,000 workers instantly
Result: Queue backs up, timeouts, failures
```

**NEW Architecture:**
```
Dispatch:
  Steady: 100 events/sec → 2 dispatchers
  Burst: 10,000 events/sec → 162 dispatchers

Can spin up dispatchers quickly (just DB writers)

Reactors:
  Queue absorbs burst
  Workers process at steady rate
  Backlog clears over time

Result: ✅ Graceful degradation
```

---

## Part 6: Scaling Strategies

### Horizontal Scaling (Add Workers)

**Dispatch Workers:**
```
1 dispatcher ≈ 60 events/sec
10 dispatchers ≈ 600 events/sec
100 dispatchers ≈ 6,000 events/sec

Limited by database write throughput
```

**Reactor Workers:**
```
Unlimited scaling (no shared resources during execution)

100 reactor workers
1,000 reactor workers
10,000 reactor workers

Only limited by:
  - Queue throughput
  - Database for result persistence
  - Cost
```

### Vertical Scaling (Bigger Database)

**Postgres tuning:**
```
shared_buffers: 8GB → 32GB
max_connections: 100 → 500
synchronous_commit: off (10x throughput)
max_wal_size: 4GB → 16GB

Result: 5-10x dispatch throughput
```

**Postgres hardware:**
```
4 cores, 16GB RAM: ~1,000 events/sec
16 cores, 64GB RAM: ~5,000 events/sec
64 cores, 256GB RAM: ~20,000 events/sec
```

### Queue Backend Scaling

**PostgresQueue → RedisQueue:**
```
Throughput: 1,000 → 10,000 events/sec
Change: Update queue backend, restart workers
Effort: 1 line of code
```

**RedisQueue → KafkaQueue:**
```
Throughput: 10,000 → 100,000+ events/sec
Change: Update queue backend, restart workers
Effort: 1 line of code + Kafka infrastructure
```

### Separate Streams Scaling

**Scale specific event types:**
```
# Allocate more workers to slow event types
OrderPlaced (slow): 1,000 workers
EmailSent (fast): 100 workers
PaymentRequested (medium): 200 workers

# Workers specialized by event type
$ causal-worker --event-types OrderPlaced,PaymentRequested --workers 100
$ causal-worker --event-types EmailSent --workers 1000
```

---

## Part 7: Performance Summary

### Dispatch Performance

| Architecture | Dispatch Latency | Throughput (1 dispatcher) | Throughput (10 dispatchers) |
|--------------|-----------------|--------------------------|----------------------------|
| **OLD (inline)** | 100ms - 60s+ | 0.01 - 10 events/sec | 0.1 - 100 events/sec |
| **NEW (PostgresQueue)** | 6-16ms | ~62 events/sec | ~620 events/sec |
| **NEW (RedisQueue)** | 5-15ms | ~66 events/sec | ~660 events/sec |
| **NEW (KafkaQueue)** | 5-15ms | ~66 events/sec | ~660 events/sec |

### Reactor Performance

| Architecture | Reactor Duration Limit | Horizontal Scaling | DB Connections |
|--------------|------------------------|-------------------|----------------|
| **OLD (inline)** | <100ms (soft), <1s (hard) | ❌ Limited by DB connections | N workers |
| **NEW (async)** | Unlimited | ✅ Yes | 0 during execution |

### Queue Backend Performance

| Backend | Throughput | Latency | Horizontal Scaling | Best For |
|---------|-----------|---------|-------------------|----------|
| **PostgresQueue** | 1k events/sec | ~10ms | ❌ No | Dev/staging |
| **RedisQueue** | 10k events/sec | <1ms | ✅ Yes (Redis Cluster) | Medium scale |
| **KafkaQueue** | 100k+ events/sec | <1ms | ✅ Yes (partitions) | High scale |
| **MemoryQueue** | 50k events/sec | <0.1ms | ❌ No | Testing |

### Scalability Limits

| Load | OLD Architecture | NEW (Postgres) | NEW (Redis) | NEW (Kafka) |
|------|-----------------|----------------|-------------|-------------|
| **100 events/sec** | ✅ Works | ✅ Works | ✅ Works | ✅ Works |
| **1k events/sec** | ⚠️ Slow reactors fail | ✅ Works | ✅ Works | ✅ Works |
| **10k events/sec** | ❌ Impossible | ❌ Queue bottleneck | ✅ Works | ✅ Works |
| **100k events/sec** | ❌ Impossible | ❌ Impossible | ⚠️ DB pressure | ✅ Works |
| **1M events/sec** | ❌ Impossible | ❌ Impossible | ❌ Impossible | ⚠️ Possible with tuning |

---

## Part 8: Recommendations

### For Small/Medium Scale (<1k events/sec)

**Use:**
- PostgresQueue (simplest, built-in)
- 5-10 dispatcher workers
- 50-100 reactor workers

**Expected performance:**
- Dispatch: <10ms (p99)
- Throughput: 500-1,000 events/sec
- Cost: Minimal (just Postgres)

### For Medium/High Scale (1k-10k events/sec)

**Use:**
- RedisQueue
- 10-20 dispatcher workers
- 500-1,000 reactor workers

**Expected performance:**
- Dispatch: <10ms (p99)
- Throughput: 5,000-10,000 events/sec
- Cost: Postgres + Redis

### For Ultra-High Scale (10k+ events/sec)

**Use:**
- KafkaQueue
- Consider Kafka as event store (not just queue)
- 50-100 dispatcher workers
- 1,000-10,000 reactor workers

**Expected performance:**
- Dispatch: <5ms (p99)
- Throughput: 50,000-100,000+ events/sec
- Cost: Postgres + Kafka cluster

### For Slow Reactors (>10s duration)

**Regardless of scale, use:**
- Separate streams per event type ✅
- Scale reactor workers independently
- Monitor queue depth per event type
- Consider batching results

---

## Conclusion

**The new async-first architecture scales dramatically better:**

| Metric | OLD | NEW (Postgres) | NEW (Redis) | NEW (Kafka) |
|--------|-----|----------------|-------------|-------------|
| **Max throughput** | ~100 events/sec | ~1k events/sec | ~10k events/sec | ~100k+ events/sec |
| **Reactor duration** | <100ms | Unlimited | Unlimited | Unlimited |
| **Horizontal scaling** | ❌ No | ⚠️ Limited | ✅ Yes | ✅ Yes |
| **DB connections** | N workers | <200 | <200 | <200 |
| **Cost scaling** | Linear (workers) | Sub-linear | Sub-linear | Sub-linear |

**Key insight:** Separating dispatch from execution enables independent scaling of each concern, eliminating the blocking bottleneck entirely.
