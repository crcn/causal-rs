use anyhow::Result;
use chrono::Utc;
use futures::StreamExt;
use seesaw_core::{
    EmittedEvent, EventProcessingCommit, InlineEffectFailure, QueuedEffectIntent, QueuedEvent,
    Store, NAMESPACE_SEESAW,
};
use seesaw_postgres::PostgresStore;
use sqlx::PgPool;
use std::collections::HashSet;
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
        retry_count: 0,
        batch_id: None,
        batch_index: None,
        batch_size: None,
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
        retry_count: 0,
        batch_id: None,
        batch_index: None,
        batch_size: None,
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
async fn test_per_workflow_fifo_ordering() -> Result<()> {
    let db = TestDb::new().await?;

    let correlation_id = Uuid::new_v4();

    // Publish 3 events for same workflow
    for i in 0..3 {
        let event = QueuedEvent {
            id: 0,
            event_id: Uuid::new_v4(),
            parent_id: None,
            correlation_id,
            event_type: format!("Event{}", i),
            payload: serde_json::json!({"order": i}),
            hops: 0,
            retry_count: 0,
            batch_id: None,
            batch_index: None,
            batch_size: None,
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
                retry_count: 0,
                batch_id: None,
                batch_index: None,
                batch_size: None,
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
        retry_count: 0,
        batch_id: None,
        batch_index: None,
        batch_size: None,
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
async fn test_commit_event_processing_persists_all_side_effects() -> Result<()> {
    let db = TestDb::new().await?;
    let correlation_id = Uuid::new_v4();
    let source_event_id = Uuid::new_v4();

    db.store
        .publish(QueuedEvent {
            id: 0,
            event_id: source_event_id,
            parent_id: None,
            correlation_id,
            event_type: "SourceEvent".to_string(),
            payload: serde_json::json!({ "value": 1 }),
            hops: 0,
            retry_count: 0,
            batch_id: None,
            batch_index: None,
            batch_size: None,
            created_at: Utc::now(),
        })
        .await?;

    let claimed = db
        .store
        .poll_next()
        .await?
        .expect("source event should exist");

    let emitted_event_id = Uuid::new_v4();
    db.store
        .commit_event_processing(EventProcessingCommit {
            event_row_id: claimed.id,
            event_id: claimed.event_id,
            correlation_id: claimed.correlation_id,
            event_type: claimed.event_type.clone(),
            event_payload: claimed.payload.clone(),
            queued_effect_intents: vec![QueuedEffectIntent {
                effect_id: "queued_commit_effect".to_string(),
                parent_event_id: Some(claimed.event_id),
                batch_id: None,
                batch_index: None,
                batch_size: None,
                execute_at: Utc::now(),
                timeout_seconds: 30,
                max_attempts: 3,
                priority: 10,
            }],
            inline_effect_failures: vec![InlineEffectFailure {
                effect_id: "inline_failure".to_string(),
                error: "boom".to_string(),
                reason: "inline_failed".to_string(),
                attempts: 1,
            }],
            emitted_events: vec![QueuedEvent {
                id: 0,
                event_id: emitted_event_id,
                parent_id: Some(claimed.event_id),
                correlation_id: claimed.correlation_id,
                event_type: "EmittedEvent".to_string(),
                payload: serde_json::json!({ "ok": true }),
                hops: claimed.hops + 1,
                retry_count: 0,
                batch_id: None,
                batch_index: None,
                batch_size: None,
                created_at: Utc::now(),
            }],
        })
        .await?;

    let effect_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)
         FROM seesaw_effect_executions
         WHERE event_id = $1 AND effect_id = $2",
    )
    .bind(source_event_id)
    .bind("queued_commit_effect")
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(effect_count, 1);

    let emitted_row: Option<(Uuid, Option<Uuid>, String)> = sqlx::query_as(
        "SELECT event_id, parent_id, event_type
         FROM seesaw_events
         WHERE event_id = $1",
    )
    .bind(emitted_event_id)
    .fetch_optional(&db.pool)
    .await?;
    let emitted_row = emitted_row.expect("emitted event should be persisted");
    assert_eq!(emitted_row.0, emitted_event_id);
    assert_eq!(emitted_row.1, Some(source_event_id));
    assert_eq!(emitted_row.2, "EmittedEvent");

    let dlq_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)
         FROM seesaw_dlq
         WHERE event_id = $1 AND effect_id = $2",
    )
    .bind(source_event_id)
    .bind("inline_failure")
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(dlq_count, 1);

    let processed_at_is_set: bool = sqlx::query_scalar(
        "SELECT processed_at IS NOT NULL
         FROM seesaw_events
         WHERE id = $1",
    )
    .bind(claimed.id)
    .fetch_one(&db.pool)
    .await?;
    assert!(processed_at_is_set, "source event should be acked");

    Ok(())
}

#[tokio::test]
async fn test_commit_event_processing_rolls_back_on_mid_commit_failure() -> Result<()> {
    let db = TestDb::new().await?;
    let correlation_id = Uuid::new_v4();
    let source_event_id = Uuid::new_v4();

    db.store
        .publish(QueuedEvent {
            id: 0,
            event_id: source_event_id,
            parent_id: None,
            correlation_id,
            event_type: "SourceEvent".to_string(),
            payload: serde_json::json!({ "value": 1 }),
            hops: 0,
            retry_count: 0,
            batch_id: None,
            batch_index: None,
            batch_size: None,
            created_at: Utc::now(),
        })
        .await?;

    let claimed = db
        .store
        .poll_next()
        .await?
        .expect("source event should exist");
    let emitted_event_id = Uuid::new_v4();

    let error = db
        .store
        .commit_event_processing(EventProcessingCommit {
            event_row_id: claimed.id,
            event_id: claimed.event_id,
            correlation_id: claimed.correlation_id,
            event_type: claimed.event_type.clone(),
            event_payload: claimed.payload.clone(),
            queued_effect_intents: vec![QueuedEffectIntent {
                effect_id: "queued_commit_effect".to_string(),
                parent_event_id: Some(claimed.event_id),
                batch_id: None,
                batch_index: None,
                batch_size: None,
                execute_at: Utc::now(),
                timeout_seconds: 30,
                max_attempts: 3,
                priority: 10,
            }],
            inline_effect_failures: vec![
                InlineEffectFailure {
                    effect_id: "dup_inline_failure".to_string(),
                    error: "boom".to_string(),
                    reason: "inline_failed".to_string(),
                    attempts: 1,
                },
                InlineEffectFailure {
                    effect_id: "dup_inline_failure".to_string(),
                    error: "boom again".to_string(),
                    reason: "inline_failed".to_string(),
                    attempts: 1,
                },
            ],
            emitted_events: vec![QueuedEvent {
                id: 0,
                event_id: emitted_event_id,
                parent_id: Some(claimed.event_id),
                correlation_id: claimed.correlation_id,
                event_type: "EmittedEvent".to_string(),
                payload: serde_json::json!({ "ok": true }),
                hops: claimed.hops + 1,
                retry_count: 0,
                batch_id: None,
                batch_index: None,
                batch_size: None,
                created_at: Utc::now(),
            }],
        })
        .await
        .expect_err("duplicate DLQ inserts should fail and rollback whole commit");
    assert!(
        error
            .to_string()
            .contains("duplicate key value violates unique constraint"),
        "unexpected error: {error}"
    );

    let effect_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)
         FROM seesaw_effect_executions
         WHERE event_id = $1 AND effect_id = $2",
    )
    .bind(source_event_id)
    .bind("queued_commit_effect")
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(effect_count, 0, "queued intent should roll back");

    let emitted_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM seesaw_events WHERE event_id = $1")
            .bind(emitted_event_id)
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(emitted_count, 0, "emitted events should roll back");

    let dlq_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)
         FROM seesaw_dlq
         WHERE event_id = $1 AND effect_id = $2",
    )
    .bind(source_event_id)
    .bind("dup_inline_failure")
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(dlq_count, 0, "DLQ writes should roll back");

    let processed_at_is_set: bool = sqlx::query_scalar(
        "SELECT processed_at IS NOT NULL
         FROM seesaw_events
         WHERE id = $1",
    )
    .bind(claimed.id)
    .fetch_one(&db.pool)
    .await?;
    assert!(
        !processed_at_is_set,
        "source event ack should roll back when commit fails"
    );

    Ok(())
}

#[tokio::test]
async fn test_commit_event_processing_fails_when_source_ack_row_missing() -> Result<()> {
    let db = TestDb::new().await?;
    let correlation_id = Uuid::new_v4();
    let source_event_id = Uuid::new_v4();

    db.store
        .publish(QueuedEvent {
            id: 0,
            event_id: source_event_id,
            parent_id: None,
            correlation_id,
            event_type: "SourceEvent".to_string(),
            payload: serde_json::json!({ "value": 1 }),
            hops: 0,
            retry_count: 0,
            batch_id: None,
            batch_index: None,
            batch_size: None,
            created_at: Utc::now(),
        })
        .await?;

    let claimed = db
        .store
        .poll_next()
        .await?
        .expect("source event should exist");

    let error = db
        .store
        .commit_event_processing(EventProcessingCommit {
            event_row_id: claimed.id + 999_999,
            event_id: claimed.event_id,
            correlation_id: claimed.correlation_id,
            event_type: claimed.event_type.clone(),
            event_payload: claimed.payload.clone(),
            queued_effect_intents: vec![QueuedEffectIntent {
                effect_id: "queued_commit_effect".to_string(),
                parent_event_id: Some(claimed.event_id),
                batch_id: None,
                batch_index: None,
                batch_size: None,
                execute_at: Utc::now(),
                timeout_seconds: 30,
                max_attempts: 3,
                priority: 10,
            }],
            inline_effect_failures: vec![],
            emitted_events: vec![],
        })
        .await
        .expect_err("invalid source row id should fail atomic commit");
    assert!(
        error
            .to_string()
            .contains("atomic event commit failed to ack source event row"),
        "unexpected error: {error}"
    );

    let effect_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)
         FROM seesaw_effect_executions
         WHERE event_id = $1 AND effect_id = $2",
    )
    .bind(source_event_id)
    .bind("queued_commit_effect")
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(effect_count, 0, "intent insert should roll back");

    let processed_at_is_set: bool = sqlx::query_scalar(
        "SELECT processed_at IS NOT NULL
         FROM seesaw_events
         WHERE id = $1",
    )
    .bind(claimed.id)
    .fetch_one(&db.pool)
    .await?;
    assert!(!processed_at_is_set, "source event should remain unacked");

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
        retry_count: 0,
        batch_id: None,
        batch_index: None,
        batch_size: None,
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
            retry_count: 0,
            batch_id: None,
            batch_index: None,
            batch_size: None,
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
            retry_count: 0,
            batch_id: None,
            batch_index: None,
            batch_size: None,
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
            None,
            None,
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
                None,
                None,
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
async fn test_insert_effect_intent_is_idempotent() -> Result<()> {
    let db = TestDb::new().await?;

    let event_id = Uuid::new_v4();
    let effect_id = "idempotent_effect".to_string();
    let correlation_id = Uuid::new_v4();

    db.store
        .insert_effect_intent(
            event_id,
            effect_id.clone(),
            correlation_id,
            "TestEvent".to_string(),
            serde_json::json!({ "payload": "same" }),
            None,
            None,
            None,
            None,
            Utc::now(),
            30,
            3,
            10,
        )
        .await?;

    // Duplicate delivery should not error and should not create another row.
    db.store
        .insert_effect_intent(
            event_id,
            effect_id.clone(),
            correlation_id,
            "TestEvent".to_string(),
            serde_json::json!({ "payload": "same" }),
            None,
            None,
            None,
            None,
            Utc::now(),
            30,
            3,
            10,
        )
        .await?;

    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)
         FROM seesaw_effect_executions
         WHERE event_id = $1 AND effect_id = $2",
    )
    .bind(event_id)
    .bind(&effect_id)
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(count, 1, "duplicate insert should be a no-op");

    let polled = db
        .store
        .poll_next_effect()
        .await?
        .expect("single effect should be available");
    assert_eq!(polled.event_id, event_id);
    assert_eq!(polled.effect_id, effect_id);

    db.store
        .complete_effect(
            polled.event_id,
            polled.effect_id.clone(),
            serde_json::json!({ "ok": true }),
        )
        .await?;

    let next = db.store.poll_next_effect().await?;
    assert!(next.is_none(), "no duplicate execution should exist");

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
            None,
            None,
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
async fn test_fail_effect_schedules_retry() -> Result<()> {
    let db = TestDb::new().await?;

    let event_id = Uuid::new_v4();
    let effect_id = "retry_effect".to_string();
    let correlation_id = Uuid::new_v4();

    db.store
        .insert_effect_intent(
            event_id,
            effect_id.clone(),
            correlation_id,
            "TestEvent".to_string(),
            serde_json::json!({}),
            None,
            None,
            None,
            None,
            Utc::now(),
            30,
            3,
            10,
        )
        .await?;

    let first = db
        .store
        .poll_next_effect()
        .await?
        .expect("effect should be polled");
    assert_eq!(first.attempts, 1);

    db.store
        .fail_effect(
            first.event_id,
            first.effect_id.clone(),
            "transient failure".to_string(),
            first.attempts,
        )
        .await?;

    let status = db.store.get_workflow_status(correlation_id).await?;
    assert_eq!(status.pending_effects, 1);
    assert!(!status.is_settled);

    let immediate_retry = db.store.poll_next_effect().await?;
    assert!(immediate_retry.is_none(), "retry should respect backoff");

    tokio::time::sleep(tokio::time::Duration::from_millis(1100)).await;
    let retried = db
        .store
        .poll_next_effect()
        .await?
        .expect("failed effect should be retried");
    assert_eq!(retried.event_id, event_id);
    assert_eq!(retried.effect_id, effect_id);
    assert_eq!(retried.attempts, 2);

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
            None,
            None,
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

#[tokio::test]
async fn test_dlq_effect_without_execution_row_uses_parent_event_payload() -> Result<()> {
    let db = TestDb::new().await?;

    let event_id = Uuid::new_v4();
    let correlation_id = Uuid::new_v4();
    let effect_id = "__inline_failed__".to_string();
    let event_type = "InlineOnlyEvent".to_string();
    let payload = serde_json::json!({ "request_id": "abc123" });

    db.store
        .publish(QueuedEvent {
            id: 0,
            event_id,
            parent_id: None,
            correlation_id,
            event_type: event_type.clone(),
            payload: payload.clone(),
            hops: 0,
            retry_count: 0,
            batch_id: None,
            batch_index: None,
            batch_size: None,
            created_at: Utc::now(),
        })
        .await?;

    // No insert_effect_intent call on purpose: this mirrors inline/synthetic failures.
    db.store
        .dlq_effect(
            event_id,
            effect_id.clone(),
            "inline failed".to_string(),
            "inline_failed".to_string(),
            1,
        )
        .await?;

    let dlq_row: (Uuid, String, serde_json::Value, String, i32) = sqlx::query_as(
        "SELECT correlation_id, event_type, event_payload, reason, attempts
         FROM seesaw_dlq
         WHERE event_id = $1 AND effect_id = $2",
    )
    .bind(event_id)
    .bind(&effect_id)
    .fetch_one(&db.pool)
    .await?;

    assert_eq!(dlq_row.0, correlation_id);
    assert_eq!(dlq_row.1, event_type);
    assert_eq!(dlq_row.2, payload);
    assert_eq!(dlq_row.3, "inline_failed");
    assert_eq!(dlq_row.4, 1);

    let execution_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)
         FROM seesaw_effect_executions
         WHERE event_id = $1 AND effect_id = $2",
    )
    .bind(event_id)
    .bind(&effect_id)
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(execution_count, 0);

    Ok(())
}

#[tokio::test]
async fn test_get_workflow_status_uses_correlation_id_columns() -> Result<()> {
    let db = TestDb::new().await?;

    let correlation_id = Uuid::new_v4();
    db.store
        .publish(QueuedEvent {
            id: 0,
            event_id: Uuid::new_v4(),
            parent_id: None,
            correlation_id,
            event_type: "StatusEvent".to_string(),
            payload: serde_json::json!({}),
            hops: 0,
            retry_count: 0,
            batch_id: None,
            batch_index: None,
            batch_size: None,
            created_at: Utc::now(),
        })
        .await?;

    let status = db.store.get_workflow_status(correlation_id).await?;
    assert_eq!(status.correlation_id, correlation_id);
    assert_eq!(status.pending_effects, 0);
    assert_eq!(status.last_event, Some("StatusEvent".to_string()));

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
            retry_count: 0,
            batch_id: None,
            batch_index: None,
            batch_size: None,
            created_at: Utc::now(),
        })
        .await?;

    // 2. Poll event
    let event = db.store.poll_next().await?.unwrap();
    assert_eq!(event.event_id, event_id);

    // 3. Insert effect intent
    db.store
        .insert_effect_intent(
            event_id,
            "send_email".to_string(),
            correlation_id,
            "OrderPlaced".to_string(),
            event.payload.clone(),
            None,
            None,
            None,
            None,
            Utc::now(),
            30,
            3,
            10,
        )
        .await?;

    // 4. Ack event
    db.store.ack(event.id).await?;

    // 5. Poll effect
    let effect = db.store.poll_next_effect().await?.unwrap();
    assert_eq!(effect.effect_id, "send_email");

    // 6. Complete effect
    db.store
        .complete_effect(
            effect.event_id,
            effect.effect_id,
            serde_json::json!({"sent": true}),
        )
        .await?;

    Ok(())
}

#[tokio::test]
async fn test_concurrent_workflow_processing() -> Result<()> {
    let db = TestDb::new().await?;

    // Create 3 different workflows, each with 3 events
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
                    retry_count: 0,
                    batch_id: None,
                    batch_index: None,
                    batch_size: None,
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
            retry_count: 0,
            batch_id: None,
            batch_index: None,
            batch_size: None,
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
            None,
            None,
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
            batch_id: None,
            batch_index: None,
            batch_size: None,
        },
        EmittedEvent {
            event_type: "EmailQueued".to_string(),
            payload: serde_json::json!({ "order_id": 123, "template": "receipt" }),
            batch_id: None,
            batch_index: None,
            batch_size: None,
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

#[tokio::test]
async fn test_workflow_subscription_coalesced_notify_stress_drains_all_events() -> Result<()> {
    let db = TestDb::new().await?;
    let correlation_id = Uuid::new_v4();
    let parent_event_id = Uuid::new_v4();
    let effect_id = "notify_stress_effect".to_string();

    db.store
        .publish(QueuedEvent {
            id: 0,
            event_id: parent_event_id,
            parent_id: None,
            correlation_id,
            event_type: "FanOutRoot".to_string(),
            payload: serde_json::json!({ "root": true }),
            hops: 0,
            retry_count: 0,
            batch_id: None,
            batch_index: None,
            batch_size: None,
            created_at: Utc::now(),
        })
        .await?;

    db.store
        .insert_effect_intent(
            parent_event_id,
            effect_id.clone(),
            correlation_id,
            "FanOutRoot".to_string(),
            serde_json::json!({ "root": true }),
            None,
            None,
            None,
            None,
            Utc::now(),
            30,
            3,
            10,
        )
        .await?;

    // Start subscription after existing rows are present. The stream should
    // emit only events created after this subscribe call.
    let mut stream = db.store.subscribe_workflow_events(correlation_id).await?;

    // Ack the root event so the queue remains focused on emitted stress events.
    let root_event = db
        .store
        .poll_next()
        .await?
        .expect("root event should exist");
    assert_eq!(root_event.event_id, parent_event_id);
    db.store.ack(root_event.id).await?;

    const FAN_OUT_SIZE: usize = 700; // > paging size to exercise cursor paging.
    let emitted_events = (0..FAN_OUT_SIZE)
        .map(|index| EmittedEvent {
            event_type: "StressItem".to_string(),
            payload: serde_json::json!({ "index": index }),
            batch_id: None,
            batch_index: None,
            batch_size: None,
        })
        .collect::<Vec<_>>();

    db.store
        .complete_effect_with_events(
            parent_event_id,
            effect_id,
            serde_json::json!({ "status": "ok" }),
            emitted_events,
        )
        .await?;

    let mut received_indexes = HashSet::with_capacity(FAN_OUT_SIZE);
    while received_indexes.len() < FAN_OUT_SIZE {
        let maybe_event = tokio::time::timeout(tokio::time::Duration::from_secs(10), stream.next())
            .await
            .expect("timed out waiting for stress workflow event");
        let event = maybe_event.expect("workflow stream ended unexpectedly");

        if event.event_type != "StressItem" {
            continue;
        }

        let index = event.payload["index"]
            .as_u64()
            .expect("StressItem payload should contain numeric index") as usize;
        assert!(
            received_indexes.insert(index),
            "duplicate StressItem index received: {}",
            index
        );
    }

    assert_eq!(received_indexes.len(), FAN_OUT_SIZE);
    Ok(())
}
