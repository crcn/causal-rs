# Critical Analysis: Ditching PostgreSQL

## TL;DR: Yes, Ditch It 🔥

**With Restate, PostgreSQL becomes redundant and limits your scaling.**

## Current State: PostgreSQL is Everywhere

```
Memory Backend:     No PostgreSQL ✓
PostgreSQL Backend: Pure PostgreSQL
Kafka Backend:      Kafka + PostgreSQL ← Still bottlenecked!
Restate Backend:    Restate only ← Future
```

## What PostgreSQL Actually Does

### 1. Event Queue
```sql
CREATE TABLE seesaw_events (
    id BIGSERIAL PRIMARY KEY,
    event_id UUID UNIQUE,
    -- ... event data
    locked_until TIMESTAMP
);

-- Worker polling
SELECT * FROM seesaw_events
WHERE locked_until < NOW()
ORDER BY created_at
LIMIT 1
FOR UPDATE SKIP LOCKED;
```

**Problem:** SKIP LOCKED contention at scale.

### 2. Handler Execution Queue
```sql
CREATE TABLE seesaw_handler_executions (
    event_id UUID,
    handler_id TEXT,
    status TEXT,
    -- ... execution state
);

-- Worker polling
SELECT * FROM seesaw_handler_executions
WHERE status = 'pending'
ORDER BY priority, execute_at
LIMIT 1
FOR UPDATE SKIP LOCKED;
```

**Problem:** Every handler poll is a database query with lock contention.

### 3. Idempotency
```sql
CREATE TABLE seesaw_processed (
    event_id UUID PRIMARY KEY,
    -- ...
);

-- Every event checks this
SELECT EXISTS(SELECT 1 FROM seesaw_processed WHERE event_id = $1);
```

**Problem:** Every event hits the database, even duplicates.

### 4. Join Windows (Accumulators)
```sql
CREATE TABLE seesaw_join_windows (
    join_handler_id TEXT,
    correlation_id UUID,
    batch_id UUID,
    -- ...
);

CREATE TABLE seesaw_join_entries (
    -- accumulated events
);
```

**Problem:** Cross-partition accumulation requires centralized state.

### 5. DLQ (Dead Letter Queue)
```sql
CREATE TABLE seesaw_dlq (
    event_id UUID,
    handler_id TEXT,
    error TEXT,
    -- ...
);
```

## Architectural Problems

### Problem 1: The Kafka Paradox

You're using Kafka for high-throughput streaming, but:

```rust
// Kafka worker (fast)
loop {
    let msg = kafka_consumer.recv().await;  // ← Kafka (millions/sec)

    // Every event hits PostgreSQL
    if store.is_processed(msg.event_id).await? {  // ← PostgreSQL (thousands/sec)
        continue;
    }

    store.insert_handler_intents(...).await?;  // ← PostgreSQL (thousands/sec)
}

// Handler worker (slow)
loop {
    let execution = store.poll_next_effect().await?;  // ← PostgreSQL SKIP LOCKED
    execute_handler(execution).await?;
    store.complete_effect(...).await?;  // ← PostgreSQL write
}
```

**You're paying for Kafka's complexity but getting PostgreSQL's throughput.**

### Problem 2: Lock Contention at Scale

```sql
-- 16 handler workers competing for rows
SELECT ... FROM seesaw_handler_executions
WHERE status = 'pending'
LIMIT 1
FOR UPDATE SKIP LOCKED;

-- As workers increase:
-- - Lock contention increases
-- - Query latency increases
-- - Throughput plateaus
```

**SKIP LOCKED is not a scaling solution.**

### Problem 3: Centralized Coordination

```
Kafka Partition 0 ──┐
Kafka Partition 1 ──┼─→ PostgreSQL (BOTTLENECK) ──→ Handler Workers
Kafka Partition 2 ──┘
...
Kafka Partition 15 ─┘
```

Kafka provides horizontal scaling, but **PostgreSQL forces all coordination through a single point.**

### Problem 4: Two Systems to Operate

With Kafka+PostgreSQL:
- Kafka cluster (brokers, zookeeper/kraft)
- PostgreSQL server (replication, backups, tuning)
- Connection pooling
- Schema migrations
- Vacuum/analyze
- Index maintenance
- Query optimization

**Operational complexity is HIGH.**

## What Restate Provides Instead

### 1. Event Storage = Log-Based
```rust
// Restate stores events in a distributed log (like Kafka)
// No need for separate Kafka
restate.dispatch(event)  // ← Writes to log, partitioned by correlation_id
```

### 2. Idempotency = Partition-Local State
```rust
// Each partition has its own state store (RocksDB)
// No cross-partition queries needed
ctx.get("processed_events")  // ← Partition-local, no database query
```

### 3. Handler Execution = Durable Functions
```rust
// No polling! Restate automatically resumes handlers
async fn handler(ctx: RestateContext, event: Event) {
    // Step 1 (checkpoint)
    let result1 = ctx.run("step1", || do_work()).await?;

    // Step 2 (checkpoint)
    let result2 = ctx.run("step2", || do_more_work()).await?;

    // If crash happens, Restate resumes from last checkpoint
}
```

### 4. Join Windows = Object State
```rust
// Accumulator state lives in virtual object
ctx.get("accumulated_rows")  // ← Object state, partition-local
```

### 5. DLQ = Error Handlers
```rust
// Restate has built-in error handling
ctx.on_error(|error| {
    // Handle failures
})
```

## Performance Comparison

| Operation | PostgreSQL | Restate |
|-----------|-----------|---------|
| Event publish | INSERT + index | Append to log |
| Idempotency check | SELECT query | State lookup (RocksDB) |
| Handler poll | SKIP LOCKED query | Automatic invocation |
| State update | UPDATE query | RocksDB write |
| Join accumulation | Cross-row queries | Object state |
| **Throughput** | **~10-30k/sec** | **~100k+/sec** |
| **Scaling** | **Vertical** | **Horizontal** |

## The Case for Keeping PostgreSQL (Weak)

### Argument: "Small deployments need simplicity"

**Counter:**
- PostgreSQL isn't simple (vacuum, indexes, connection pools, migrations)
- For small scale, use **MemoryBackend** (already exists)
- For production, use **Restate** (scales from small to large)

### Argument: "Need ACID transactions"

**Counter:**
- Restate provides consistency via event sourcing
- Virtual objects have transactional state
- Most "ACID needs" are actually idempotency needs (which Restate has)

### Argument: "Observability via SQL queries"

**Counter:**
- Need new observability anyway (seesaw-insight currently queries PostgreSQL)
- Restate provides APIs for introspection
- Better: Use OpenTelemetry + Restate metrics

### Argument: "Backwards compatibility"

**Counter:**
- Keep PostgresBackend as **deprecated/legacy**
- Document migration path
- Don't let legacy limit future

## Recommended Architecture

### Phase 1: Current (Before Restate)
```
Development:  MemoryBackend
Production:   PostgresBackend (deprecated) or KafkaBackend (limited)
```

### Phase 2: Restate Available
```
Development:  MemoryBackend
Production:   RestateBackend ← New default
Legacy:       PostgresBackend (deprecated, will be removed)
```

### Phase 3: Future
```
Development:  MemoryBackend
Production:   RestateBackend (only option)
```

## Migration Strategy

### 1. Deprecate PostgreSQL Backend
```rust
#[deprecated(
    since = "0.12.0",
    note = "Use RestateBackend for production deployments. \
            PostgresBackend will be removed in 1.0."
)]
pub struct PostgresBackend { ... }
```

### 2. Remove Kafka Backend
**Why?** Restate already provides:
- Distributed log (like Kafka)
- Event streaming
- Partitioning
- Replay

```rust
#[deprecated(
    since = "0.12.0",
    note = "RestateBackend provides equivalent functionality without Kafka complexity."
)]
pub struct KafkaBackend { ... }
```

### 3. Simplify Backend Choices

**New architecture:**
```rust
// Development
let backend = MemoryBackend::new();

// Production
let backend = RestateBackend::new(endpoint).await?;
```

**Two backends. Simple.**

## What You Lose by Ditching PostgreSQL

Honestly? **Nothing meaningful.**

| Feature | Lost? | Alternative |
|---------|-------|-------------|
| Event storage | No | Restate log |
| Idempotency | No | Restate state |
| Transactional | No | Restate consistency |
| Query flexibility | Yes | Use Restate APIs |
| SQL familiarity | Yes | Learn Restate APIs |
| Observability | Yes | Build new (needed anyway) |

The only real loss is **SQL query flexibility for ad-hoc debugging**. But:
1. You need to rebuild observability anyway (PostgreSQL queries don't scale)
2. Restate provides introspection APIs
3. You can add observability database separately (write-only, doesn't block processing)

## Implementation Plan

### Step 1: Document Deprecation (Now)
- Mark PostgresBackend as deprecated
- Mark KafkaBackend as deprecated
- Update README with migration guidance

### Step 2: Complete RestateBackend (1-2 months)
- Implement backend
- Implement workflow engine
- Integration tests
- Performance benchmarks

### Step 3: Migration Guide (Concurrent)
- Document PostgreSQL → Restate migration
- Provide conversion tools
- Example migrations

### Step 4: Remove PostgreSQL (6-12 months)
- Remove seesaw-postgres crate
- Remove seesaw-kafka crate
- Simplify codebase

## Conclusion

**Ditch PostgreSQL. It's the right move.**

**Why?**
1. PostgreSQL is the bottleneck in both backends
2. Restate provides everything PostgreSQL does, better
3. Operational complexity decreases (one system vs two/three)
4. Scales horizontally from day one
5. Aligns architecture with goals (event-driven, high throughput)

**When?**
- Deprecate now (signal intent)
- Complete Restate backend (2-3 months)
- Remove PostgreSQL (6-12 months)

**Risk?**
- Low: MemoryBackend still exists for dev/test
- Low: PostgreSQL backend can remain as deprecated legacy
- Low: Clear migration path

**The only reason to keep PostgreSQL is inertia. Don't let inertia determine your architecture.**
