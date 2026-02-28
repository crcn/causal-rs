# Simplified Architecture (Post-PostgreSQL)

## Current Complexity

```
┌─────────────────────────────────────────────────────────┐
│                    User Code                            │
│  Engine::new(deps, backend).with_handler(...)           │
└──────────────────────────┬──────────────────────────────┘
                           │
           ┌───────────────┼───────────────┐
           │               │               │
           ▼               ▼               ▼
    ┌──────────┐   ┌─────────────┐  ┌──────────┐
    │  Memory  │   │  PostgreSQL │  │  Kafka   │
    │ Backend  │   │   Backend   │  │ Backend  │
    └──────────┘   └──────┬──────┘  └────┬─────┘
                          │                │
                          ▼                ▼
                   ┌──────────────┐  ┌──────────────┐
                   │ PostgreSQL   │  │ Kafka        │
                   │ • Events     │  │ • Events     │
                   │ • Handlers   │  │              │
                   │ • State      │  │              │
                   │ • Idempotency│  │              │
                   │ • Joins      │  │              │
                   │ • DLQ        │  │              │
                   └──────────────┘  └──────┬───────┘
                                            │
                                            ▼
                                     ┌──────────────┐
                                     │ PostgreSQL   │
                                     │ • Idempotency│
                                     │ • Handlers   │
                                     │ • State      │
                                     │ • Joins      │
                                     │ • DLQ        │
                                     └──────────────┘

Problem: 3 backends, 2 require PostgreSQL, Kafka adds no value
```

## Proposed Simplicity

```
┌─────────────────────────────────────────────────────────┐
│                    User Code                            │
│  Engine::new(deps, backend).with_handler(...)           │
└──────────────────────────┬──────────────────────────────┘
                           │
           ┌───────────────┼───────────────┐
           │               │               │
           ▼               ▼               ▼
    ┌──────────┐   ┌─────────────┐  ┌──────────┐
    │  Memory  │   │  Restate    │  │  Legacy  │
    │ Backend  │   │  Backend    │  │ (Postgres)│
    │          │   │             │  │ DEPRECATED│
    │          │   │             │  │           │
    └──────────┘   └──────┬──────┘  └──────────┘
                          │
                          ▼
                   ┌──────────────────────────┐
                   │ Restate Runtime          │
                   │ • Event Log (built-in)   │
                   │ • Durable Execution      │
                   │ • Partition-local State  │
                   │ • Idempotency            │
                   │ • Joins (object state)   │
                   │ • Error Handling         │
                   │ • Horizontal Scaling     │
                   └──────────────────────────┘

Solution: 2 backends (Memory + Restate), no external dependencies
```

## Backend Decision Matrix

### Before (Complex)

| Use Case | Backend | Dependencies | Throughput | Ops Complexity |
|----------|---------|--------------|------------|----------------|
| Dev/Test | Memory | None | Unlimited | ⭐ Simple |
| Small Prod | PostgreSQL | PostgreSQL | 1-5k/sec | ⭐⭐ Medium |
| Medium Prod | Kafka | Kafka + PostgreSQL | 10-30k/sec | ⭐⭐⭐ High |
| Large Prod | ??? | ??? | ??? | ??? |

**Problem:** Medium prod hits complexity wall, large prod has no solution.

### After (Simple)

| Use Case | Backend | Dependencies | Throughput | Ops Complexity |
|----------|---------|--------------|------------|----------------|
| Dev/Test | Memory | None | Unlimited | ⭐ Simple |
| All Prod | Restate | Restate | 100k+/sec | ⭐⭐ Medium |

**Solution:** Two backends. Memory for dev, Restate for prod. Scales from zero to millions.

## Code Comparison

### Current: Choose Backend (Confusing)

```rust
// Development
let backend = MemoryBackend::new();

// Small production (< 5k/sec)
let pool = PgPoolOptions::new().connect(db_url).await?;
let store = PostgresStore::new(pool);
let backend = PostgresBackend::new(store);

// Medium production (< 30k/sec)
let pool = PgPoolOptions::new().connect(db_url).await?;
let store = PostgresStore::new(pool);
let kafka_config = KafkaBackendConfig::new(brokers)
    .with_topic_events("seesaw.events")
    .with_num_partitions(16);
let backend = KafkaBackend::new(kafka_config, store)?;

// Large production (> 30k/sec)
// ??? No solution ???
```

### Proposed: Simple Decision

```rust
// Development
let backend = MemoryBackend::new();

// Production (any scale)
let backend = RestateBackend::new("http://restate:8080").await?;

// That's it!
```

## Operational Complexity

### Current: Kafka Backend

**Components to manage:**
1. Kafka cluster (3-5 brokers)
2. Zookeeper/KRaft (3-5 nodes)
3. PostgreSQL (1 primary + replicas)
4. Connection pools
5. Schema migrations
6. Kafka topics management
7. Consumer group monitoring
8. PostgreSQL vacuum/analyze
9. Index maintenance
10. Backup strategies for both systems

**Failure modes:**
- Kafka broker fails
- Zookeeper loses quorum
- PostgreSQL crashes
- PostgreSQL runs out of connections
- Kafka consumer lag
- PostgreSQL lock contention
- Network partition between Kafka and PostgreSQL
- Schema migration fails
- Disk fills on Kafka
- Disk fills on PostgreSQL
- Connection pool exhaustion
- SKIP LOCKED contention
- etc.

### Proposed: Restate Backend

**Components to manage:**
1. Restate cluster

**Failure modes:**
- Restate node fails (auto-recovers)
- Network issues (auto-recovers)

**That's it.**

## Migration Path

### Phase 1: Deprecation (Now)

```rust
// Mark old backends as deprecated
#[deprecated(since = "0.12.0", note = "Use RestateBackend")]
pub struct PostgresBackend { ... }

#[deprecated(since = "0.12.0", note = "Use RestateBackend")]
pub struct KafkaBackend { ... }
```

### Phase 2: Transition (1-6 months)

Users can choose:
- MemoryBackend (dev/test)
- PostgresBackend (legacy, deprecated)
- KafkaBackend (legacy, deprecated)
- RestateBackend (new, recommended)

### Phase 3: Removal (6-12 months)

Remove:
- `crates/seesaw-postgres` (entire crate)
- `crates/seesaw-kafka` (entire crate)
- All PostgreSQL/Kafka examples
- PostgreSQL dependencies from core

Keep:
- `crates/seesaw` (core)
- `crates/seesaw-memory` (dev/test)
- `crates/seesaw-restate` (production)

### Phase 4: Simplicity (12+ months)

Final architecture:
```
seesaw-rs/
├── crates/
│   ├── seesaw/           # Core
│   ├── seesaw-memory/    # Dev/test
│   └── seesaw-restate/   # Production
└── examples/             # All using Restate
```

**Three crates. Simple.**

## Documentation Simplification

### Before

```markdown
# Choosing a Backend

## Development
Use MemoryBackend for fast iteration...

## Small Production (< 5k events/sec)
Use PostgresBackend with proper tuning...
- Set shared_buffers
- Configure connection pooling
- Run migrations
- Set up monitoring
- Configure backups
...10 more pages...

## Medium Production (< 30k events/sec)
Use KafkaBackend for better throughput...
- Set up Kafka cluster
- Configure Zookeeper/KRaft
- Still need PostgreSQL
- Configure both systems
- Monitor both systems
...20 more pages...

## Large Production (> 30k events/sec)
¯\_(ツ)_/¯
```

### After

```markdown
# Choosing a Backend

## Development
Use MemoryBackend:
```rust
let backend = MemoryBackend::new();
```

## Production
Use RestateBackend:
```rust
let backend = RestateBackend::new("http://restate:8080").await?;
```

Done.
```

## Performance Characteristics

### Current Architecture

```
Memory Backend:     100k+ evt/sec (no persistence)
PostgreSQL Backend: 1-5k evt/sec (bottleneck)
Kafka Backend:      10-30k evt/sec (PostgreSQL still bottleneck)
```

**Observation:** Adding Kafka gives 2-6x improvement, but hits wall at 30k.

### Proposed Architecture

```
Memory Backend:     100k+ evt/sec (no persistence)
Restate Backend:    100k-500k+ evt/sec (horizontally scalable)
```

**Observation:** Linear scaling by adding partitions.

## Cost Analysis

### Current: Kafka + PostgreSQL

**AWS Example (30k events/sec):**
- Kafka (MSK): 3 brokers × m5.large = $~500/month
- PostgreSQL (RDS): db.r5.2xlarge = $~800/month
- **Total: ~$1,300/month**

### Proposed: Restate

**Self-hosted (30k events/sec):**
- Restate cluster: 3 nodes × c5.2xlarge = $~600/month
- **Total: ~$600/month**

**Or Restate Cloud:**
- Usage-based pricing
- No infrastructure management

**Savings: ~50% + less operational overhead**

## Conclusion

**Ditching PostgreSQL means:**
- ✅ Simpler architecture
- ✅ Lower operational complexity
- ✅ Better performance
- ✅ Horizontal scaling
- ✅ Lower costs
- ✅ Clearer documentation
- ✅ Easier onboarding

**The only downside is change itself. But change is necessary for growth.**
