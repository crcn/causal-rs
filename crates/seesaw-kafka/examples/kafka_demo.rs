//! Kafka Backend Demo - High-throughput event processing
//!
//! This example demonstrates using the Kafka backend for high-throughput
//! event processing with horizontal scaling capabilities.
//!
//! # Prerequisites
//!
//! 1. Start Kafka (Docker):
//!    ```bash
//!    docker run -d -p 9092:9092 apache/kafka:latest
//!    ```
//!
//! 2. Start PostgreSQL (Docker):
//!    ```bash
//!    docker run -d -p 5432:5432 -e POSTGRES_PASSWORD=postgres postgres:latest
//!    ```
//!
//! 3. Run migrations:
//!    ```bash
//!    cd crates/seesaw-postgres
//!    cargo run --example migrate
//!    ```
//!
//! 4. Create Kafka topic (optional - will auto-create):
//!    ```bash
//!    kafka-topics --bootstrap-server localhost:9092 --create \
//!      --topic seesaw.events --partitions 16 --replication-factor 1
//!    ```
//!
//! # Run the example
//!
//! ```bash
//! cargo run --example kafka_demo
//! ```

use anyhow::Result;
use seesaw_core::backend::BackendServeConfig;
use seesaw_core::{handler, Context, Engine};
use seesaw_kafka::{KafkaBackend, KafkaBackendConfig};
use seesaw_postgres::PostgresStore;
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgPoolOptions;
use std::time::Instant;
use tracing_subscriber;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
enum OrderEvent {
    Placed { order_id: Uuid, total: f64 },
    PaymentProcessed { order_id: Uuid },
    Shipped { order_id: Uuid },
    Delivered { order_id: Uuid },
}

#[derive(Clone)]
struct Deps {
    payment_enabled: bool,
    shipping_enabled: bool,
}

impl Deps {
    async fn process_payment(&self, order_id: Uuid, total: f64) -> Result<()> {
        if self.payment_enabled {
            println!("💳 Processing payment for order {} (${:.2})", order_id, total);
            // Simulate payment processing
            tokio::time::sleep(tokio::time::Duration::from_millis(5)).await;
        }
        Ok(())
    }

    async fn ship(&self, order_id: Uuid) -> Result<()> {
        if self.shipping_enabled {
            println!("📦 Shipping order {}", order_id);
            tokio::time::sleep(tokio::time::Duration::from_millis(5)).await;
        }
        Ok(())
    }

    async fn notify(&self, order_id: Uuid, message: &str) -> Result<()> {
        println!("📧 Notify order {}: {}", order_id, message);
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_target(false)
        .with_level(true)
        .init();

    println!("🚀 Kafka Backend Demo - High-throughput Event Processing\n");

    // Create PostgreSQL pool
    println!("📊 Connecting to PostgreSQL...");
    let pool = PgPoolOptions::new()
        .max_connections(20)
        .connect("postgres://postgres:postgres@localhost/seesaw")
        .await?;

    let store = PostgresStore::new(pool);

    // Configure Kafka backend
    println!("🎯 Configuring Kafka backend...");
    let kafka_config = KafkaBackendConfig::new(vec!["localhost:9092".to_string()])
        .with_topic_events("seesaw.events")
        .with_consumer_group("seesaw-demo-workers")
        .with_num_partitions(16);

    // Create Kafka backend
    println!("🔗 Creating Kafka backend...");
    let backend = KafkaBackend::new(kafka_config, store)?;

    // Create dependencies
    let deps = Deps {
        payment_enabled: true,
        shipping_enabled: true,
    };

    // Build engine with handlers
    println!("⚙️  Building engine with handlers...");
    let engine = Engine::new(deps, backend)
        .with_handler(
            handler::on::<OrderEvent>()
                .id("process_payment")
                .extract(|e| match e {
                    OrderEvent::Placed { order_id, total } => Some((*order_id, *total)),
                    _ => None,
                })
                .then(|(order_id, total), ctx: Context<Deps>| async move {
                    ctx.deps().process_payment(order_id, total).await?;
                    Ok(OrderEvent::PaymentProcessed { order_id })
                }),
        )
        .with_handler(
            handler::on::<OrderEvent>()
                .id("ship_order")
                .extract(|e| match e {
                    OrderEvent::PaymentProcessed { order_id } => Some(*order_id),
                    _ => None,
                })
                .then(|order_id, ctx: Context<Deps>| async move {
                    ctx.deps().ship(order_id).await?;
                    Ok(OrderEvent::Shipped { order_id })
                }),
        )
        .with_handler(
            handler::on::<OrderEvent>()
                .id("notify_delivered")
                .extract(|e| match e {
                    OrderEvent::Shipped { order_id } => Some(*order_id),
                    _ => None,
                })
                .then(|order_id, ctx: Context<Deps>| async move {
                    ctx.deps()
                        .notify(order_id, "Your order has been delivered!")
                        .await?;
                    Ok(OrderEvent::Delivered { order_id })
                }),
        );

    println!("🎬 Starting engine...\n");

    // Start engine in background with custom config
    let config = BackendServeConfig {
        event_workers: 4,
        handler_workers: 8,
        ..Default::default()
    };

    let runtime = engine.clone().start(config).await?;

    // Give workers time to start
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    // Dispatch orders
    let num_orders = 100;
    println!("📤 Dispatching {} orders...", num_orders);
    let start = Instant::now();

    for i in 1..=num_orders {
        let order_id = Uuid::new_v4();
        let total = 99.99 * i as f64;

        engine
            .dispatch(OrderEvent::Placed { order_id, total })
            .await?;

        if i % 10 == 0 {
            println!("  ✓ Dispatched {} orders", i);
        }
    }

    let dispatch_duration = start.elapsed();
    let throughput = num_orders as f64 / dispatch_duration.as_secs_f64();

    println!("\n📊 Performance Metrics:");
    println!("  • Orders dispatched: {}", num_orders);
    println!("  • Time taken: {:.2}s", dispatch_duration.as_secs_f64());
    println!("  • Throughput: {:.0} events/sec", throughput);

    // Wait for processing to complete
    println!("\n⏳ Waiting for processing to complete...");
    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

    println!("\n✅ Demo completed!");
    println!("\n💡 Tips:");
    println!("  • Check Kafka topic: kafka-console-consumer --topic seesaw.events --from-beginning");
    println!("  • Scale horizontally by running multiple instances");
    println!("  • Events are partitioned by correlation_id for FIFO ordering");
    println!("  • Exactly-once semantics via Kafka offsets + PostgreSQL idempotency");

    // Shutdown engine
    runtime.shutdown().await?;

    Ok(())
}
