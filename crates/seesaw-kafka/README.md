# seesaw-kafka

Kafka backend for Seesaw event-driven orchestration.

## Overview

`seesaw-kafka` provides a high-throughput, horizontally scalable backend for Seesaw that combines:

- **Kafka** for event streaming (high throughput, partitioning, replay)
- **PostgreSQL** for coordination state (idempotency, handler executions, joins, DLQ)

## Features

- **High throughput**: 50k-100k events/sec (10-20x improvement over PostgreSQL-only)
- **Horizontal scaling**: Scale via Kafka partitions and consumer groups
- **Event replay**: Replay events from Kafka topic for debugging/analysis
- **Exactly-once semantics**: Kafka offset commits + PostgreSQL idempotency
- **Cross-platform integration**: Other systems can consume from Kafka
- **Same API**: Drop-in replacement for PostgresBackend

## Architecture

```
KafkaBackend
├── Kafka (event streaming)
│   ├── seesaw.events topic (partitioned by correlation_id)
│   ├── Producer (publish events)
│   └── Consumer Groups (event workers)
└── PostgresStore (coordination state)
    ├── Idempotency ledger (seesaw_processed)
    ├── Handler executions (seesaw_handler_executions)
    ├── Join windows (seesaw_join_windows, seesaw_join_entries)
    └── DLQ (seesaw_dlq)
```

## Installation

Add to your `Cargo.toml`:

```toml
[dependencies]
seesaw_core = "0.10.3"
seesaw-postgres = "0.10.3"
seesaw-kafka = "0.11.0"
```

## Quick Start

```rust
use seesaw_core::{Engine, handler, Context};
use seesaw_kafka::{KafkaBackend, KafkaBackendConfig};
use seesaw_postgres::PostgresStore;
use sqlx::postgres::PgPoolOptions;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Create PostgreSQL pool
    let pool = PgPoolOptions::new()
        .max_connections(20)
        .connect("postgres://localhost/seesaw")
        .await?;

    let store = PostgresStore::new(pool);

    // Configure Kafka
    let kafka_config = KafkaBackendConfig::new(vec!["localhost:9092".to_string()])
        .with_topic_events("seesaw.events")
        .with_consumer_group("seesaw-workers")
        .with_num_partitions(16);

    // Create Kafka backend
    let backend = KafkaBackend::new(kafka_config, store)?;

    // Create engine with handlers (same API as PostgresBackend)
    let engine = Engine::new(deps, backend)
        .with_handler(handler::on::<OrderPlaced>().then(...))
        .start()
        .await?;

    Ok(())
}
```

## Prerequisites

### Kafka

Start Kafka using Docker:

```bash
docker run -d -p 9092:9092 apache/kafka:latest
```

Or use a managed Kafka service (Confluent Cloud, AWS MSK, etc.).

### PostgreSQL

Start PostgreSQL using Docker:

```bash
docker run -d -p 5432:5432 -e POSTGRES_PASSWORD=postgres postgres:latest
```

Run migrations:

```bash
cd crates/seesaw-postgres
cargo run --example migrate
```

### Create Kafka Topic (Optional)

Topic will auto-create, but you can pre-create it:

```bash
kafka-topics --bootstrap-server localhost:9092 --create \
  --topic seesaw.events --partitions 16 --replication-factor 1
```

## Configuration

```rust
let kafka_config = KafkaBackendConfig::new(vec!["localhost:9092".to_string()])
    .with_topic_events("seesaw.events")           // Topic name
    .with_consumer_group("seesaw-workers")        // Consumer group
    .with_num_partitions(16)                      // Partition count
    .with_idempotent_producer(true);              // Exactly-once producer
```

### Tuning

- **Partitions**: More partitions = more parallelism (typical: 16-32)
- **Event workers**: Number of Kafka consumers (typical: 2-8)
- **Handler workers**: Number of PostgreSQL pollers (typical: 4-16)

```rust
use seesaw_core::backend::BackendServeConfig;

let config = BackendServeConfig {
    event_workers: 4,      // Kafka consumers
    handler_workers: 8,    // PostgreSQL pollers
    ..Default::default()
};

engine.start(config).await?;
```

## Guarantees

- **Per-workflow FIFO ordering**: Events for same `correlation_id` always go to same partition
- **Exactly-once processing**: Idempotency checks prevent duplicate processing on redelivery
- **Crash recovery**: Kafka redelivers uncommitted messages; idempotency prevents duplicates
- **Atomic commits**: PostgreSQL transaction includes all side effects

## Performance

| Backend       | Throughput      | Scaling        |
|---------------|-----------------|----------------|
| PostgreSQL    | ~1-5k evt/sec   | Vertical only  |
| Kafka         | ~50-100k evt/sec| Horizontal     |
| **Improvement** | **10-20x**    | **Partitions** |

## Monitoring

### Kafka

```bash
# Check topic
kafka-topics --bootstrap-server localhost:9092 --describe --topic seesaw.events

# Consumer lag
kafka-consumer-groups --bootstrap-server localhost:9092 \
  --describe --group seesaw-workers

# View events
kafka-console-consumer --bootstrap-server localhost:9092 \
  --topic seesaw.events --from-beginning
```

### PostgreSQL

```sql
-- Check idempotency ledger
SELECT COUNT(*) FROM seesaw_processed;

-- Check pending handlers
SELECT COUNT(*) FROM seesaw_handler_executions WHERE status = 'pending';

-- Check DLQ
SELECT * FROM seesaw_dlq ORDER BY created_at DESC LIMIT 10;
```

## Examples

See [`examples/kafka_demo.rs`](examples/kafka_demo.rs) for a complete working example.

```bash
cargo run --example kafka_demo
```

## Trade-offs

### Pros
- ✅ 10-20x throughput improvement
- ✅ Horizontal scaling via Kafka partitions
- ✅ Event replay capabilities
- ✅ Cross-platform integration
- ✅ No breaking changes to Seesaw API

### Cons
- ❌ Operational complexity (requires Kafka cluster)
- ❌ Two systems to manage (Kafka + PostgreSQL)
- ❌ Join operations still require PostgreSQL
- ❌ Not pure Kafka (hybrid architecture)

### When to Use

**Use Kafka backend when:**
- You need >5k events/sec throughput
- You want horizontal scaling
- You need event replay/time-travel
- Other systems need to consume Seesaw events

**Use PostgreSQL backend when:**
- You have <5k events/sec
- You want simplicity (single system)
- You don't need horizontal scaling
- Operational complexity is a concern

## License

MIT
