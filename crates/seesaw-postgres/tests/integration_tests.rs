use anyhow::Result;
use chrono::Utc;
use seesaw_core::{EmittedEvent, QueuedEvent, Store, NAMESPACE_SEESAW};
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
        let container = Postgres::default().with_tag("16-alpine").start().await?;

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
        correlation_id: Uuid::new_v4(),
        event_type: "TestEvent".to_string(),
        payload: serde_json::json!({"data": "test"}),
        hops: 0,
        created_at: Utc::now(),
    };

    // Publish event
    db.store.publish(event.clone()).await?;

    // Poll event
    let polled = db
        .store
        .poll_next()
        .await?
        .expect("Event should be available");

    assert_eq!(polled.event_id, event.event_id);
    assert_eq!(polled.correlation_id, event.correlation_id);
    assert_eq!(polled.event_type, event.event_type);
    assert_eq!(polled.payload, event.payload);

    Ok(())
}

#[tokio::test]
async fn test_idempotent_publish() -> Result<()> {
    let db = TestDb::new().await?;

    let event_id = Uuid::new_v4();
    let correlation_id = Uuid::new_v4();

    let event = QueuedEvent {
        id: 0,
        event_id,
        parent_id: None,
        correlation_id,
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
    assert_eq!(first.event_id, event_id);

    // Mark as processed so it won't be polled again
    db.store.ack(first.id).await?;

    let second = db.store.poll_next().await?;
    assert!(second.is_none(), "Should not have duplicate event");

    Ok(())
}

#[tokio::test]
async fn test_per_saga_fifo_ordering() -> Result<()> {
    let db = TestDb::new().await?;

    let correlation_id = Uuid::new_v4();

    // Publish 3 events for same saga
    for i in 0..3 {
        let event = QueuedEvent {
            id: 0,
            event_id: Uuid::new_v4(),
            parent_id: None,
            correlation_id,
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

    // Publish 10 events with DIFFERENT correlation_ids so they can be processed concurrently
    for i in 0..10 {
        db.store
            .publish(QueuedEvent {
                id: 0,
                event_id: Uuid::new_v4(),
                parent_id: None,
                correlation_id: Uuid::new_v4(), // Different correlation_id for each event
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
        correlation_id: Uuid::new_v4(),
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

#[tokio::test]
async fn test_poll_next_claims_event_until_ack_or_nack() -> Result<()> {
    let db = TestDb::new().await?;

    let event = QueuedEvent {
        id: 0,
        event_id: Uuid::new_v4(),
        parent_id: None,
        correlation_id: Uuid::new_v4(),
        event_type: "ClaimedEvent".to_string(),
        payload: serde_json::json!({}),
        hops: 0,
        created_at: Utc::now(),
    };

    db.store.publish(event.clone()).await?;

    let first = db
        .store
        .poll_next()
        .await?
        .expect("first poll should claim the event");

    // Without ack/nack, same event must not be re-delivered.
    let second = db.store.poll_next().await?;
    assert!(
        second.is_none(),
        "claimed event should not be re-delivered before ack/nack"
    );

    // Nack releases claim and should make event available again.
    db.store.nack(first.id, 0).await?;
    let retried = db
        .store
        .poll_next()
        .await?
        .expect("nacked event should become available again");
    assert_eq!(retried.event_id, first.event_id);

    Ok(())
}

#[tokio::test]
async fn test_publish_deduplicates_event_id_even_with_different_timestamps() -> Result<()> {
    let db = TestDb::new().await?;

    let event_id = Uuid::new_v4();
    let correlation_id = Uuid::new_v4();
    let base_time = Utc::now();

    db.store
        .publish(QueuedEvent {
            id: 0,
            event_id,
            parent_id: None,
            correlation_id,
            event_type: "WebhookReceived".to_string(),
            payload: serde_json::json!({ "attempt": 1 }),
            hops: 0,
            created_at: base_time,
        })
        .await?;

    db.store
        .publish(QueuedEvent {
            id: 0,
            event_id,
            parent_id: None,
            correlation_id,
            event_type: "WebhookReceived".to_string(),
            payload: serde_json::json!({ "attempt": 2 }),
            hops: 0,
            created_at: base_time + chrono::Duration::seconds(1),
        })
        .await?;

    let duplicate_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM seesaw_events WHERE event_id = $1")
            .bind(event_id)
            .fetch_one(&db.pool)
            .await?;

    assert_eq!(
        duplicate_count, 1,
        "event_id idempotency must hold even when created_at differs"
    );

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

    let correlation_id = Uuid::new_v4();
    let state = TestState {
        count: 42,
        name: "test".to_string(),
    };

    // Save state
    let version = db.store.save_state(correlation_id, &state, 0).await?;
    assert_eq!(version, 1);

    // Load state
    let (loaded, loaded_version): (TestState, i32) = db.store.load_state(correlation_id).await?.unwrap();

    assert_eq!(loaded, state);
    assert_eq!(loaded_version, 1);

    Ok(())
}

#[tokio::test]
async fn test_optimistic_locking() -> Result<()> {
    let db = TestDb::new().await?;

    let correlation_id = Uuid::new_v4();
    let state = TestState {
        count: 1,
        name: "initial".to_string(),
    };

    // Initial save
    db.store.save_state(correlation_id, &state, 0).await?;

    // Update with correct version
    let new_state = TestState {
        count: 2,
        name: "updated".to_string(),
    };
    let v2 = db.store.save_state(correlation_id, &new_state, 1).await?;
    assert_eq!(v2, 2);

    // Try to update with stale version (should fail)
    let stale_state = TestState {
        count: 3,
        name: "stale".to_string(),
    };
    let result = db.store.save_state(correlation_id, &stale_state, 1).await;

    assert!(result.is_err(), "Should fail with version conflict");
    assert!(result.unwrap_err().to_string().contains("Version conflict"));

    Ok(())
}

#[tokio::test]
async fn test_load_nonexistent_state() -> Result<()> {
    let db = TestDb::new().await?;

    let correlation_id = Uuid::new_v4();
    let result: Option<(TestState, i32)> = db.store.load_state(correlation_id).await?;

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
    let correlation_id = Uuid::new_v4();

    // Insert effect intent
    db.store
        .insert_effect_intent(
            event_id,
            "test_effect".to_string(),
            correlation_id,
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
    assert_eq!(effect.correlation_id, correlation_id);
    assert_eq!(effect.attempts, 1); // Should increment on poll

    Ok(())
}

#[tokio::test]
async fn test_effect_priority_ordering() -> Result<()> {
    let db = TestDb::new().await?;

    let correlation_id = Uuid::new_v4();

    // Insert effects with different priorities
    for (i, priority) in [(0, 5), (1, 1), (2, 10)].iter() {
        db.store
            .insert_effect_intent(
                Uuid::new_v4(),
                format!("effect_{}", i),
                correlation_id,
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

    let correlation_id = Uuid::new_v4();
    let event_id = Uuid::new_v4();

    // 1. Publish event
    db.store
        .publish(QueuedEvent {
            id: 0,
            event_id,
            parent_id: None,
            correlation_id,
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
    db.store.save_state(correlation_id, &state, 0).await?;

    // 4. Insert effect intent
    db.store
        .insert_effect_intent(
            event_id,
            "send_email".to_string(),
            correlation_id,
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
    let (final_state, version): (TestState, i32) = db.store.load_state(correlation_id).await?.unwrap();
    assert_eq!(final_state.count, 1);
    assert_eq!(version, 1);

    Ok(())
}

#[tokio::test]
async fn test_concurrent_saga_processing() -> Result<()> {
    let db = TestDb::new().await?;

    // Create 3 different sagas, each with 3 events
    let correlation_ids: Vec<Uuid> = (0..3).map(|_| Uuid::new_v4()).collect();

    for correlation_id in &correlation_ids {
        for i in 0..3 {
            db.store
                .publish(QueuedEvent {
                    id: 0,
                    event_id: Uuid::new_v4(),
                    parent_id: None,
                    correlation_id: *correlation_id,
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

/// Test deterministic event ID generation on effect completion
/// Verifies crash+retry safety: same effect execution generates same event IDs
#[tokio::test]
async fn test_deterministic_event_emission() -> Result<()> {
    let db = TestDb::new().await?;
    let correlation_id = Uuid::new_v4();
    let event_id = Uuid::new_v4();

    // Publish initial event
    let parent_created_at = Utc::now();
    db.store
        .publish(QueuedEvent {
            id: 0,
            event_id,
            parent_id: None,
            correlation_id,
            event_type: "OrderPlaced".to_string(),
            payload: serde_json::json!({ "order_id": 123 }),
            hops: 0,
            created_at: parent_created_at,
        })
        .await?;

    // Insert effect execution
    db.store
        .insert_effect_intent(
            event_id,
            "charge_payment".to_string(),
            correlation_id,
            "OrderPlaced".to_string(),
            serde_json::json!({ "order_id": 123 }),
            None,
            Utc::now(),
            30,
            3,
            10,
        )
        .await?;

    // Complete effect with emitted events
    let emitted_events = vec![
        EmittedEvent {
            event_type: "PaymentCharged".to_string(),
            payload: serde_json::json!({ "order_id": 123, "amount": 99.99 }),
        },
        EmittedEvent {
            event_type: "EmailQueued".to_string(),
            payload: serde_json::json!({ "order_id": 123, "template": "receipt" }),
        },
    ];

    // Ack the initial OrderPlaced event BEFORE completing the effect
    // (emitted events use midnight timestamp which sorts before parent's Utc::now())
    let initial_event = db
        .store
        .poll_next()
        .await?
        .expect("Initial event should exist");
    assert_eq!(initial_event.event_id, event_id);
    db.store.ack(initial_event.id).await?;

    // Now complete the effect and emit new events
    db.store
        .complete_effect_with_events(
            event_id,
            "charge_payment".to_string(),
            serde_json::json!({ "status": "success" }),
            emitted_events.clone(),
        )
        .await?;

    // Compute expected deterministic IDs (same logic as PostgresStore)
    let expected_payment_id = Uuid::new_v5(
        &NAMESPACE_SEESAW,
        format!("{}-{}-{}", event_id, "charge_payment", "PaymentCharged").as_bytes(),
    );
    let expected_email_id = Uuid::new_v5(
        &NAMESPACE_SEESAW,
        format!("{}-{}-{}", event_id, "charge_payment", "EmailQueued").as_bytes(),
    );

    // Compute expected timestamp (midnight UTC today)
    let _expected_timestamp = Utc::now()
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc();

    // Poll emitted events (order may vary, so collect both)
    let event1 = db
        .store
        .poll_next()
        .await?
        .expect("First emitted event should exist");
    db.store.ack(event1.id).await?;

    let event2 = db
        .store
        .poll_next()
        .await?
        .expect("Second emitted event should exist");
    db.store.ack(event2.id).await?;

    // Debug: Show polled events
    println!("Event 1: {} ({})", event1.event_id, event1.event_type);
    println!("Event 2: {} ({})", event2.event_id, event2.event_type);
    println!("Expected PaymentCharged ID: {}", expected_payment_id);
    println!("Expected EmailQueued ID: {}", expected_email_id);

    // Collect events by type (order-independent verification)
    let events_by_type: std::collections::HashMap<String, _> = [
        (event1.event_type.clone(), event1),
        (event2.event_type.clone(), event2),
    ]
    .into_iter()
    .collect();

    let payment_event = events_by_type
        .get("PaymentCharged")
        .expect("PaymentCharged event should exist");

    assert_eq!(
        payment_event.event_id, expected_payment_id,
        "Deterministic ID mismatch for PaymentCharged"
    );
    assert_eq!(payment_event.parent_id, Some(event_id));
    assert_eq!(payment_event.hops, 1);
    assert_eq!(
        payment_event.created_at.date_naive(),
        parent_created_at.date_naive(),
        "emitted event should stay in parent partition day"
    );

    let email_event = events_by_type
        .get("EmailQueued")
        .expect("EmailQueued event should exist");
    assert_eq!(email_event.event_id, expected_email_id);
    assert_eq!(email_event.parent_id, Some(event_id));
    assert_eq!(email_event.hops, 1);
    assert_eq!(
        email_event.created_at.date_naive(),
        parent_created_at.date_naive(),
        "emitted event should stay in parent partition day"
    );

    // Verify effect is marked complete
    let execution_status: Option<String> = sqlx::query_scalar(
        "SELECT status FROM seesaw_effect_executions
         WHERE event_id = $1 AND effect_id = $2",
    )
    .bind(event_id)
    .bind("charge_payment")
    .fetch_optional(&db.pool)
    .await?;

    assert_eq!(
        execution_status,
        Some("completed".to_string()),
        "Effect should be marked as completed"
    );

    // Verify no additional events in queue
    let remaining_events = db.store.poll_next().await?;
    assert!(
        remaining_events.is_none(),
        "All events should have been processed"
    );

    Ok(())
}
