use anyhow::Result;
use chrono::Utc;
use seesaw_core::{QueuedEffectExecution, QueuedEvent, Store};
use seesaw_postgres::PostgresStore;
use sqlx::PgPool;
use testcontainers::{runners::AsyncRunner, ContainerAsync, ImageExt};
use testcontainers_modules::postgres::Postgres;
use uuid::Uuid;

/// Test harness with Postgres container
struct TestDb {
    #[allow(dead_code)]
    container: ContainerAsync<Postgres>,
    pool: PgPool,
    store: PostgresStore,
}

impl TestDb {
    async fn new() -> Result<Self> {
        // Initialize tracing for tests
        let _ = tracing_subscriber::fmt()
            .with_test_writer()
            .with_max_level(tracing::Level::DEBUG)
            .try_init();

        // Start Postgres container
        let container = Postgres::default()
            .with_tag("16-alpine")
            .start()
            .await?;

        let host = container.get_host().await?;
        let port = container.get_host_port_ipv4(5432).await?;

        // Connect to database
        let database_url = format!("postgres://postgres:postgres@{}:{}/postgres", host, port);
        let pool = PgPool::connect(&database_url).await?;

        // Apply schema
        let schema = include_str!("../../../docs/schema.sql");
        sqlx::raw_sql(schema).execute(&pool).await?;

        let store = PostgresStore::new(pool.clone());

        Ok(Self {
            container,
            pool,
            store,
        })
    }
}

// ============================================================================
// Queue Operations Tests
// ============================================================================

#[tokio::test]
async fn test_publish_and_poll() -> Result<()> {
    let db = TestDb::new().await?;

    let event = QueuedEvent {
        id: 0,
        event_id: Uuid::new_v4(),
        parent_id: None,
        saga_id: Uuid::new_v4(),
        event_type: "TestEvent".to_string(),
        payload: serde_json::json!({"data": "test"}),
        hops: 0,
        created_at: Utc::now(),
    };

    // Publish event
    db.store.publish(event.clone()).await?;

    // Poll event
    let polled = db.store.poll_next().await?.expect("Event should be available");

    assert_eq!(polled.event_id, event.event_id);
    assert_eq!(polled.saga_id, event.saga_id);
    assert_eq!(polled.event_type, event.event_type);
    assert_eq!(polled.payload, event.payload);

    Ok(())
}

#[tokio::test]
async fn test_idempotent_publish() -> Result<()> {
    let db = TestDb::new().await?;

    let event_id = Uuid::new_v4();
    let saga_id = Uuid::new_v4();

    let event = QueuedEvent {
        id: 0,
        event_id,
        parent_id: None,
        saga_id,
        event_type: "TestEvent".to_string(),
        payload: serde_json::json!({"data": "test"}),
        hops: 0,
        created_at: Utc::now(),
    };

    // Publish same event twice
    db.store.publish(event.clone()).await?;
    db.store.publish(event.clone()).await?; // Should not error (idempotent)

    // Should only have one event
    let first = db.store.poll_next().await?.expect("Should have event");
    let second = db.store.poll_next().await?;

    assert_eq!(first.event_id, event_id);
    assert!(second.is_none(), "Should not have duplicate event");

    Ok(())
}

#[tokio::test]
async fn test_per_saga_fifo_ordering() -> Result<()> {
    let db = TestDb::new().await?;

    let saga_id = Uuid::new_v4();

    // Publish 3 events for same saga
    for i in 0..3 {
        let event = QueuedEvent {
            id: 0,
            event_id: Uuid::new_v4(),
            parent_id: None,
            saga_id,
            event_type: format!("Event{}", i),
            payload: serde_json::json!({"order": i}),
            hops: 0,
            created_at: Utc::now(),
        };
        db.store.publish(event).await?;
        // Small delay to ensure ordering
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    }

    // Poll should return events in order
    for i in 0..3 {
        let polled = db.store.poll_next().await?.expect("Event should exist");
        assert_eq!(polled.event_type, format!("Event{}", i));
        db.store.ack(polled.id).await?;
    }

    Ok(())
}

#[tokio::test]
async fn test_skip_locked_concurrent_workers() -> Result<()> {
    let db = TestDb::new().await?;

    let saga_id = Uuid::new_v4();

    // Publish 10 events
    for i in 0..10 {
        db.store
            .publish(QueuedEvent {
                id: 0,
                event_id: Uuid::new_v4(),
                parent_id: None,
                saga_id,
                event_type: format!("Event{}", i),
                payload: serde_json::json!({}),
                hops: 0,
                created_at: Utc::now(),
            })
            .await?;
    }

    // Simulate concurrent workers polling
    let store1 = db.store.clone();
    let store2 = db.store.clone();

    let handle1 = tokio::spawn(async move {
        let mut count = 0;
        for _ in 0..5 {
            if let Some(event) = store1.poll_next().await.unwrap() {
                count += 1;
                store1.ack(event.id).await.unwrap();
            }
        }
        count
    });

    let handle2 = tokio::spawn(async move {
        let mut count = 0;
        for _ in 0..5 {
            if let Some(event) = store2.poll_next().await.unwrap() {
                count += 1;
                store2.ack(event.id).await.unwrap();
            }
        }
        count
    });

    let count1 = handle1.await?;
    let count2 = handle2.await?;

    // Both workers should have processed some events (SKIP LOCKED working)
    assert!(count1 > 0);
    assert!(count2 > 0);
    assert_eq!(count1 + count2, 10, "All events should be processed once");

    Ok(())
}

#[tokio::test]
async fn test_ack_and_nack() -> Result<()> {
    let db = TestDb::new().await?;

    let event = QueuedEvent {
        id: 0,
        event_id: Uuid::new_v4(),
        parent_id: None,
        saga_id: Uuid::new_v4(),
        event_type: "TestEvent".to_string(),
        payload: serde_json::json!({}),
        hops: 0,
        created_at: Utc::now(),
    };

    db.store.publish(event).await?;

    // Poll and nack (retry)
    let polled = db.store.poll_next().await?.unwrap();
    db.store.nack(polled.id, 1).await?;

    // Should not be available immediately (locked)
    let should_be_none = db.store.poll_next().await?;
    assert!(should_be_none.is_none(), "Event should be locked");

    // Wait for lock to expire
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    // Now should be available
    let retried = db.store.poll_next().await?.unwrap();
    assert_eq!(retried.event_id, polled.event_id);

    // Ack it
    db.store.ack(retried.id).await?;

    // Should not be available anymore
    let final_check = db.store.poll_next().await?;
    assert!(final_check.is_none(), "Event should be acked");

    Ok(())
}

// ============================================================================
// State Operations Tests
// ============================================================================

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
struct TestState {
    count: i32,
    name: String,
}

#[tokio::test]
async fn test_save_and_load_state() -> Result<()> {
    let db = TestDb::new().await?;

    let saga_id = Uuid::new_v4();
    let state = TestState {
        count: 42,
        name: "test".to_string(),
    };

    // Save state
    let version = db.store.save_state(saga_id, &state, 0).await?;
    assert_eq!(version, 1);

    // Load state
    let (loaded, loaded_version): (TestState, i32) =
        db.store.load_state(saga_id).await?.unwrap();

    assert_eq!(loaded, state);
    assert_eq!(loaded_version, 1);

    Ok(())
}

#[tokio::test]
async fn test_optimistic_locking() -> Result<()> {
    let db = TestDb::new().await?;

    let saga_id = Uuid::new_v4();
    let state = TestState {
        count: 1,
        name: "initial".to_string(),
    };

    // Initial save
    db.store.save_state(saga_id, &state, 0).await?;

    // Update with correct version
    let new_state = TestState {
        count: 2,
        name: "updated".to_string(),
    };
    let v2 = db.store.save_state(saga_id, &new_state, 1).await?;
    assert_eq!(v2, 2);

    // Try to update with stale version (should fail)
    let stale_state = TestState {
        count: 3,
        name: "stale".to_string(),
    };
    let result = db.store.save_state(saga_id, &stale_state, 1).await;

    assert!(result.is_err(), "Should fail with version conflict");
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("Version conflict"));

    Ok(())
}

#[tokio::test]
async fn test_load_nonexistent_state() -> Result<()> {
    let db = TestDb::new().await?;

    let saga_id = Uuid::new_v4();
    let result: Option<(TestState, i32)> = db.store.load_state(saga_id).await?;

    assert!(result.is_none(), "Should return None for nonexistent state");

    Ok(())
}

// ============================================================================
// Effect Execution Tests
// ============================================================================

#[tokio::test]
async fn test_insert_and_poll_effect() -> Result<()> {
    let db = TestDb::new().await?;

    let event_id = Uuid::new_v4();
    let saga_id = Uuid::new_v4();

    // Insert effect intent
    db.store
        .insert_effect_intent(
            event_id,
            "test_effect".to_string(),
            saga_id,
            "TestEvent".to_string(),
            serde_json::json!({"data": "test"}),
            None,
            Utc::now(),
            30,
            3,
            10,
        )
        .await?;

    // Poll effect
    let effect = db.store.poll_next_effect().await?.unwrap();

    assert_eq!(effect.event_id, event_id);
    assert_eq!(effect.effect_id, "test_effect");
    assert_eq!(effect.saga_id, saga_id);
    assert_eq!(effect.attempts, 1); // Should increment on poll

    Ok(())
}

#[tokio::test]
async fn test_effect_priority_ordering() -> Result<()> {
    let db = TestDb::new().await?;

    let saga_id = Uuid::new_v4();

    // Insert effects with different priorities
    for (i, priority) in [(0, 5), (1, 1), (2, 10)].iter() {
        db.store
            .insert_effect_intent(
                Uuid::new_v4(),
                format!("effect_{}", i),
                saga_id,
                "TestEvent".to_string(),
                serde_json::json!({}),
                None,
                Utc::now(),
                30,
                3,
                *priority,
            )
            .await?;
    }

    // Should poll highest priority first (lower number = higher priority)
    let first = db.store.poll_next_effect().await?.unwrap();
    assert_eq!(first.effect_id, "effect_1"); // Priority 1

    Ok(())
}

#[tokio::test]
async fn test_complete_effect() -> Result<()> {
    let db = TestDb::new().await?;

    let event_id = Uuid::new_v4();
    let effect_id = "test_effect".to_string();

    // Insert and poll
    db.store
        .insert_effect_intent(
            event_id,
            effect_id.clone(),
            Uuid::new_v4(),
            "TestEvent".to_string(),
            serde_json::json!({}),
            None,
            Utc::now(),
            30,
            3,
            10,
        )
        .await?;

    db.store.poll_next_effect().await?.unwrap();

    // Complete it
    let result = serde_json::json!({"status": "success"});
    db.store
        .complete_effect(event_id, effect_id.clone(), result)
        .await?;

    // Should not be pollable anymore
    let should_be_none = db.store.poll_next_effect().await?;
    assert!(should_be_none.is_none());

    Ok(())
}

#[tokio::test]
async fn test_fail_and_dlq_effect() -> Result<()> {
    let db = TestDb::new().await?;

    let event_id = Uuid::new_v4();
    let effect_id = "test_effect".to_string();

    // Insert effect with max_attempts=1
    db.store
        .insert_effect_intent(
            event_id,
            effect_id.clone(),
            Uuid::new_v4(),
            "TestEvent".to_string(),
            serde_json::json!({}),
            None,
            Utc::now(),
            30,
            1, // max_attempts
            10,
        )
        .await?;

    // Poll and fail
    db.store.poll_next_effect().await?.unwrap();

    // Move to DLQ
    db.store
        .dlq_effect(
            event_id,
            effect_id.clone(),
            "Test failure".to_string(),
            "failed".to_string(),
            1,
        )
        .await?;

    // Should not be in executions table
    let should_be_none = db.store.poll_next_effect().await?;
    assert!(should_be_none.is_none());

    // Verify it's in DLQ
    let dlq_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM seesaw_dlq WHERE event_id = $1")
        .bind(event_id)
        .fetch_one(&db.pool)
        .await?;

    assert_eq!(dlq_count, 1, "Effect should be in DLQ");

    Ok(())
}

// ============================================================================
// End-to-End Workflow Tests
// ============================================================================

#[tokio::test]
async fn test_complete_event_workflow() -> Result<()> {
    let db = TestDb::new().await?;

    let saga_id = Uuid::new_v4();
    let event_id = Uuid::new_v4();

    // 1. Publish event
    db.store
        .publish(QueuedEvent {
            id: 0,
            event_id,
            parent_id: None,
            saga_id,
            event_type: "OrderPlaced".to_string(),
            payload: serde_json::json!({"order_id": 123}),
            hops: 0,
            created_at: Utc::now(),
        })
        .await?;

    // 2. Poll event
    let event = db.store.poll_next().await?.unwrap();
    assert_eq!(event.event_id, event_id);

    // 3. Save state (simulating reducer)
    let state = TestState {
        count: 1,
        name: "order_placed".to_string(),
    };
    db.store.save_state(saga_id, &state, 0).await?;

    // 4. Insert effect intent
    db.store
        .insert_effect_intent(
            event_id,
            "send_email".to_string(),
            saga_id,
            "OrderPlaced".to_string(),
            event.payload.clone(),
            None,
            Utc::now(),
            30,
            3,
            10,
        )
        .await?;

    // 5. Ack event
    db.store.ack(event.id).await?;

    // 6. Poll effect
    let effect = db.store.poll_next_effect().await?.unwrap();
    assert_eq!(effect.effect_id, "send_email");

    // 7. Complete effect
    db.store
        .complete_effect(
            effect.event_id,
            effect.effect_id,
            serde_json::json!({"sent": true}),
        )
        .await?;

    // 8. Verify final state
    let (final_state, version): (TestState, i32) = db.store.load_state(saga_id).await?.unwrap();
    assert_eq!(final_state.count, 1);
    assert_eq!(version, 1);

    Ok(())
}

#[tokio::test]
async fn test_concurrent_saga_processing() -> Result<()> {
    let db = TestDb::new().await?;

    // Create 3 different sagas, each with 3 events
    let saga_ids: Vec<Uuid> = (0..3).map(|_| Uuid::new_v4()).collect();

    for saga_id in &saga_ids {
        for i in 0..3 {
            db.store
                .publish(QueuedEvent {
                    id: 0,
                    event_id: Uuid::new_v4(),
                    parent_id: None,
                    saga_id: *saga_id,
                    event_type: format!("Event{}", i),
                    payload: serde_json::json!({}),
                    hops: 0,
                    created_at: Utc::now(),
                })
                .await?;
        }
    }

    // Process all events (simulating workers)
    let mut processed = 0;
    while let Some(event) = db.store.poll_next().await? {
        db.store.ack(event.id).await?;
        processed += 1;
    }

    assert_eq!(processed, 9, "All events should be processed");

    Ok(())
}
