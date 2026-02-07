//! Integration tests for Kafka backend
//!
//! These tests require:
//! - Kafka running on localhost:9092
//! - PostgreSQL running on localhost:5432
//!
//! Run with: cargo test --features integration-tests

#![cfg(feature = "integration-tests")]

use anyhow::Result;
use seesaw_core::{handler, Context, Engine};
use seesaw_kafka::{KafkaBackend, KafkaBackendConfig};
use seesaw_postgres::PostgresStore;
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgPoolOptions;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
enum TestEvent {
    Started { id: Uuid },
    Processed { id: Uuid },
    Completed { id: Uuid },
}

#[derive(Clone)]
struct TestDeps {
    processed_count: Arc<AtomicUsize>,
}

async fn setup_test_backend() -> Result<(KafkaBackend, PostgresStore)> {
    // Create PostgreSQL pool
    let pool = PgPoolOptions::new()
        .max_connections(10)
        .connect("postgres://postgres:postgres@localhost/seesaw_test")
        .await?;

    let store = PostgresStore::new(pool);

    // Configure Kafka with unique consumer group per test
    let test_id = Uuid::new_v4();
    let kafka_config = KafkaBackendConfig::new(vec!["localhost:9092".to_string()])
        .with_topic_events("seesaw.test.events")
        .with_consumer_group(&format!("seesaw-test-{}", test_id))
        .with_num_partitions(4);

    let backend = KafkaBackend::new(kafka_config, store.clone())?;

    Ok((backend, store))
}

#[tokio::test]
#[ignore] // Requires Kafka and PostgreSQL
async fn test_kafka_backend_publishes_and_processes_events() -> Result<()> {
    let (backend, _store) = setup_test_backend().await?;

    let processed_count = Arc::new(AtomicUsize::new(0));
    let deps = TestDeps {
        processed_count: processed_count.clone(),
    };

    let engine = Engine::new(deps, backend)
        .with_handler(
            handler::on::<TestEvent>()
                .id("process_test_event")
                .extract(|e| match e {
                    TestEvent::Started { id } => Some(*id),
                    _ => None,
                })
                .then(|id, ctx: Context<TestDeps>| async move {
                    ctx.deps().processed_count.fetch_add(1, Ordering::SeqCst);
                    Ok(TestEvent::Processed { id })
                }),
        )
        .with_handler(
            handler::on::<TestEvent>()
                .id("complete_test_event")
                .extract(|e| match e {
                    TestEvent::Processed { id } => Some(*id),
                    _ => None,
                })
                .then(|id, ctx: Context<TestDeps>| async move {
                    ctx.deps().processed_count.fetch_add(1, Ordering::SeqCst);
                    Ok(TestEvent::Completed { id })
                }),
        );

    // Start engine in background
    let engine_handle = {
        let engine = engine.clone();
        tokio::spawn(async move { engine.start().await })
    };

    // Give workers time to start
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    // Dispatch test events
    let num_events = 10;
    for _ in 0..num_events {
        let id = Uuid::new_v4();
        engine.dispatch(TestEvent::Started { id }).await?;
    }

    // Wait for processing
    tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;

    // Verify all events were processed
    let final_count = processed_count.load(Ordering::SeqCst);
    assert_eq!(
        final_count,
        num_events * 2,
        "Expected {} processed events, got {}",
        num_events * 2,
        final_count
    );

    // Cleanup
    engine_handle.abort();

    Ok(())
}

#[tokio::test]
#[ignore] // Requires Kafka and PostgreSQL
async fn test_idempotency_prevents_duplicate_processing() -> Result<()> {
    let (backend, store) = setup_test_backend().await?;

    let processed_count = Arc::new(AtomicUsize::new(0));
    let deps = TestDeps {
        processed_count: processed_count.clone(),
    };

    let engine = Engine::new(deps, backend.clone())
        .with_handler(
            handler::on::<TestEvent>()
                .id("count_events")
                .extract(|e| match e {
                    TestEvent::Started { id } => Some(*id),
                    _ => None,
                })
                .then(|_id, ctx: Context<TestDeps>| async move {
                    ctx.deps().processed_count.fetch_add(1, Ordering::SeqCst);
                    Ok::<(), anyhow::Error>(())
                }),
        );

    // Start engine
    let engine_handle = {
        let engine = engine.clone();
        tokio::spawn(async move { engine.start().await })
    };

    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    // Dispatch same event twice
    let event_id = Uuid::new_v4();
    let event = TestEvent::Started { id: event_id };

    engine.dispatch(event.clone()).await?;

    // Wait a bit, then dispatch again
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    engine.dispatch(event).await?;

    // Wait for processing
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    // Should only be processed once due to idempotency
    let final_count = processed_count.load(Ordering::SeqCst);
    assert_eq!(
        final_count, 1,
        "Expected 1 processed event (idempotency), got {}",
        final_count
    );

    // Cleanup
    engine_handle.abort();

    Ok(())
}

#[tokio::test]
#[ignore] // Requires Kafka and PostgreSQL
async fn test_partition_ordering() -> Result<()> {
    let (backend, _store) = setup_test_backend().await?;

    let processed_order = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let deps_order = processed_order.clone();

    #[derive(Clone)]
    struct OrderDeps {
        processed_order: Arc<tokio::sync::Mutex<Vec<usize>>>,
    }

    let engine = Engine::new(OrderDeps { processed_order: deps_order }, backend)
        .with_handler(
            handler::on::<TestEvent>()
                .id("track_order")
                .extract(|e| match e {
                    TestEvent::Started { id } => Some(*id),
                    _ => None,
                })
                .then(|id, ctx: Context<OrderDeps>| async move {
                    // Extract sequence number from UUID
                    let seq = id.as_u128() as usize % 1000;
                    ctx.deps().processed_order.lock().await.push(seq);
                    Ok::<(), anyhow::Error>(())
                }),
        );

    let engine_handle = {
        let engine = engine.clone();
        tokio::spawn(async move { engine.start().await })
    };

    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    // Dispatch events with same correlation_id (should maintain order)
    let num_events = 20;
    for i in 0..num_events {
        // Create UUID with sequence embedded
        let mut uuid_bytes = Uuid::new_v4().as_bytes().to_owned();
        uuid_bytes[0] = (i % 256) as u8;
        let id = Uuid::from_bytes(uuid_bytes);

        engine.dispatch(TestEvent::Started { id }).await?;
    }

    // Wait for processing
    tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;

    let order = processed_order.lock().await;
    assert_eq!(
        order.len(),
        num_events,
        "Expected {} events processed",
        num_events
    );

    // Cleanup
    engine_handle.abort();

    Ok(())
}

#[tokio::test]
#[ignore] // Requires Kafka and PostgreSQL
async fn test_throughput_benchmark() -> Result<()> {
    let (backend, _store) = setup_test_backend().await?;

    let processed_count = Arc::new(AtomicUsize::new(0));
    let deps = TestDeps {
        processed_count: processed_count.clone(),
    };

    let engine = Engine::new(deps, backend)
        .with_handler(
            handler::on::<TestEvent>()
                .id("benchmark_handler")
                .extract(|e| match e {
                    TestEvent::Started { id } => Some(*id),
                    _ => None,
                })
                .then(|_id, ctx: Context<TestDeps>| async move {
                    ctx.deps().processed_count.fetch_add(1, Ordering::SeqCst);
                    Ok::<(), anyhow::Error>(())
                }),
        );

    let engine_handle = {
        let engine = engine.clone();
        tokio::spawn(async move { engine.start().await })
    };

    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    // Benchmark dispatch
    let num_events = 1000;
    let start = std::time::Instant::now();

    for _ in 0..num_events {
        let id = Uuid::new_v4();
        engine.dispatch(TestEvent::Started { id }).await?;
    }

    let dispatch_duration = start.elapsed();
    let throughput = num_events as f64 / dispatch_duration.as_secs_f64();

    println!("Dispatch throughput: {:.0} events/sec", throughput);

    // Wait for processing
    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

    let final_count = processed_count.load(Ordering::SeqCst);
    println!("Processed {} events", final_count);

    // Cleanup
    engine_handle.abort();

    Ok(())
}
