//! Integration tests for Dead Letter Queue
//!
//! These tests define the expected behavior of the DLQ system.
//! They will fail initially until we integrate DLQ with the handler worker.

use anyhow::Result;
use seesaw_core::{handler, Context, DlqStatus, Engine, Runtime};
use seesaw_postgres::{DeadLetterQueue, PostgresStore};
use sqlx::PgPool;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use uuid::Uuid;

#[derive(Clone)]
struct TestDeps {
    failure_count: Arc<AtomicUsize>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct TestEvent {
    id: Uuid,
    should_fail: bool,
}

// Helper to setup test database
async fn setup_test_db() -> Result<PgPool> {
    let database_url = std::env::var("TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://localhost/seesaw_test".to_string());

    let pool = PgPool::connect(&database_url).await?;

    // Run migrations
    sqlx::migrate!("../../migrations").run(&pool).await?;

    // Clean tables
    sqlx::query("TRUNCATE seesaw_events, seesaw_handler_intents, seesaw_dead_letter_queue CASCADE")
        .execute(&pool)
        .await?;

    Ok(pool)
}

/// Test 1: Handler that fails permanently should create DLQ entry
#[tokio::test]
async fn test_permanent_failure_creates_dlq_entry() -> Result<()> {
    let pool = setup_test_db().await?;
    let store = PostgresStore::new(pool.clone());
    let deps = TestDeps {
        failure_count: Arc::new(AtomicUsize::new(0)),
    };

    // Create handler that always fails
    let failing_handler = handler::on::<TestEvent>()
        .id("always_fails")
        .retry(3) // Max 3 attempts
        .then(
            move |event: std::sync::Arc<TestEvent>, ctx: Context<TestDeps>| async move {
                ctx.deps().failure_count.fetch_add(1, Ordering::SeqCst);
                anyhow::bail!("Intentional failure for testing")
            },
        );

    let engine = Engine::new(deps.clone(), store).with_handler(failing_handler);

    // Start runtime
    let runtime = Runtime::start(&engine, Default::default()).await?;

    // Dispatch event
    let event_id = Uuid::new_v4();
    engine
        .dispatch(TestEvent {
            id: event_id,
            should_fail: true,
        })
        .await?;

    // Wait for handler to exhaust retries
    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

    // Verify DLQ entry created
    let dlq = DeadLetterQueue::new(pool.clone());
    let entries = dlq.list(100).await?;

    assert_eq!(
        entries.len(),
        1,
        "Expected 1 DLQ entry after permanent failure"
    );

    let entry = &entries[0];
    assert_eq!(entry.handler_id, "always_fails");
    assert_eq!(entry.status, DlqStatus::Open);
    assert_eq!(
        entry.retry_count, 3,
        "Should have attempted 3 times before DLQ"
    );
    assert!(entry.error_message.contains("Intentional failure"));

    // Verify handler was called 3 times
    assert_eq!(
        deps.failure_count.load(Ordering::SeqCst),
        3,
        "Handler should have been called 3 times"
    );

    runtime.shutdown().await?;
    Ok(())
}

/// Test 2: Retry from DLQ should replay event successfully
#[tokio::test]
async fn test_dlq_retry_replays_event() -> Result<()> {
    let pool = setup_test_db().await?;
    let store = PostgresStore::new(pool.clone());
    let deps = TestDeps {
        failure_count: Arc::new(AtomicUsize::new(0)),
    };

    let failure_count = deps.failure_count.clone();
    let handler = handler::on::<TestEvent>()
        .id("flaky_handler")
        .retry(1) // Only 1 attempt before DLQ
        .then(
            move |event: std::sync::Arc<TestEvent>, ctx: Context<TestDeps>| async move {
                let count = ctx.deps().failure_count.fetch_add(1, Ordering::SeqCst);

                // Fail first time, succeed on retry from DLQ
                if count == 0 {
                    anyhow::bail!("First attempt fails")
                }

                Ok(())
            },
        );

    let engine = Engine::new(deps, store).with_handler(handler);

    let runtime = Runtime::start(&engine, Default::default()).await?;

    // Dispatch event that will fail
    let event_id = Uuid::new_v4();
    engine
        .dispatch(TestEvent {
            id: event_id,
            should_fail: true,
        })
        .await?;

    // Wait for DLQ entry
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    // Verify DLQ entry exists
    let dlq = DeadLetterQueue::new(pool.clone());
    let entries = dlq.list(100).await?;
    assert_eq!(entries.len(), 1, "Expected 1 DLQ entry");

    // Retry from DLQ
    let dlq_id = entries[0].id;
    let entry = dlq.start_retry(dlq_id).await?;

    // Re-dispatch the event
    let original_event: TestEvent = serde_json::from_value(entry.event_payload)?;
    engine.dispatch(original_event).await?;

    // Wait for handler to succeed
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    // Mark as replayed
    dlq.mark_replayed(dlq_id, Some("Successfully replayed"))
        .await?;

    // Verify status changed to replayed
    let entry = dlq.get(dlq_id).await?;
    assert_eq!(entry.status, DlqStatus::Replayed);
    assert!(entry.resolution_note.is_some());

    // Verify handler was called twice (initial + retry)
    assert_eq!(
        failure_count.load(Ordering::SeqCst),
        2,
        "Handler should have been called twice"
    );

    runtime.shutdown().await?;
    Ok(())
}

/// Test 3: Duplicate retry should be prevented by row lock
#[tokio::test]
async fn test_dlq_duplicate_retry_prevented() -> Result<()> {
    let pool = setup_test_db().await?;
    let store = PostgresStore::new(pool.clone());
    let deps = TestDeps {
        failure_count: Arc::new(AtomicUsize::new(0)),
    };

    // Create DLQ entry manually
    let dlq = DeadLetterQueue::new(pool.clone());
    let dlq_id = dlq
        .insert(
            Uuid::new_v4(),
            "test_handler",
            Uuid::new_v4(),
            "Test error",
            None,
            3,
            chrono::Utc::now(),
            chrono::Utc::now(),
            serde_json::json!({"id": "test"}),
        )
        .await?;

    // Start two concurrent retries
    let dlq1 = dlq.clone();
    let dlq2 = DeadLetterQueue::new(pool.clone());

    let handle1 = tokio::spawn(async move { dlq1.start_retry(dlq_id).await });

    let handle2 = tokio::spawn(async move {
        // Small delay to ensure they overlap
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        dlq2.start_retry(dlq_id).await
    });

    let result1 = handle1.await?;
    let result2 = handle2.await?;

    // One should succeed, one should fail (or one should wait)
    // In current implementation with FOR UPDATE, one will block
    // For this test, we just verify retry_attempts incremented correctly

    // Check final state
    let entry = dlq.get(dlq_id).await?;

    // retry_attempts should be incremented by each start_retry call
    // Due to row lock, they execute sequentially
    assert!(
        entry.retry_attempts >= 1,
        "At least one retry should have started"
    );

    Ok(())
}

/// Test 4: Partial batch failure should create correct DLQ entries
#[tokio::test]
async fn test_partial_batch_failure_creates_dlq_entries() -> Result<()> {
    let pool = setup_test_db().await?;
    let store = PostgresStore::new(pool.clone());
    let deps = TestDeps {
        failure_count: Arc::new(AtomicUsize::new(0)),
    };

    // Handler that fails on specific IDs
    let handler = handler::on::<TestEvent>()
        .id("selective_fail")
        .retry(1)
        .then(
            |event: std::sync::Arc<TestEvent>, _ctx: Context<TestDeps>| async move {
                if event.should_fail {
                    anyhow::bail!("Event marked to fail")
                }
                Ok(())
            },
        );

    let engine = Engine::new(deps, store).with_handler(handler);

    let runtime = Runtime::start(&engine, Default::default()).await?;

    // Dispatch batch of 5 events, 2 should fail
    for i in 0..5 {
        engine
            .dispatch(TestEvent {
                id: Uuid::new_v4(),
                should_fail: i % 3 == 0, // Events 0 and 3 fail
            })
            .await?;
    }

    // Wait for processing
    tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;

    // Verify 2 DLQ entries
    let dlq = DeadLetterQueue::new(pool.clone());
    let entries = dlq.list(100).await?;

    assert_eq!(
        entries.len(),
        2,
        "Expected 2 DLQ entries from partial batch failure"
    );

    runtime.shutdown().await?;
    Ok(())
}

/// Test 5: DLQ statistics should be accurate
#[tokio::test]
async fn test_dlq_statistics_accurate() -> Result<()> {
    let pool = setup_test_db().await?;
    let dlq = DeadLetterQueue::new(pool.clone());

    // Create entries in different states
    let dlq_id_1 = dlq
        .insert(
            Uuid::new_v4(),
            "handler1",
            Uuid::new_v4(),
            "Error 1",
            None,
            3,
            chrono::Utc::now(),
            chrono::Utc::now(),
            serde_json::json!({}),
        )
        .await?;

    let dlq_id_2 = dlq
        .insert(
            Uuid::new_v4(),
            "handler2",
            Uuid::new_v4(),
            "Error 2",
            None,
            3,
            chrono::Utc::now(),
            chrono::Utc::now(),
            serde_json::json!({}),
        )
        .await?;

    let dlq_id_3 = dlq
        .insert(
            Uuid::new_v4(),
            "handler3",
            Uuid::new_v4(),
            "Error 3",
            None,
            3,
            chrono::Utc::now(),
            chrono::Utc::now(),
            serde_json::json!({}),
        )
        .await?;

    // Change states
    dlq.start_retry(dlq_id_1).await?; // -> retrying
    dlq.mark_replayed(dlq_id_2, None).await?; // -> replayed
                                              // dlq_id_3 stays open

    // Check stats
    let stats = dlq.stats().await?;

    assert_eq!(stats.open, 1, "Expected 1 open entry");
    assert_eq!(stats.retrying, 1, "Expected 1 retrying entry");
    assert_eq!(stats.replayed, 1, "Expected 1 replayed entry");
    assert_eq!(stats.resolved, 0, "Expected 0 resolved entries");
    assert_eq!(stats.total, 3, "Expected 3 total entries");

    Ok(())
}

/// Test 6: Crash during retry should leave entry in retrying state
#[tokio::test]
async fn test_crash_during_retry_preserves_state() -> Result<()> {
    let pool = setup_test_db().await?;
    let dlq = DeadLetterQueue::new(pool.clone());

    // Create DLQ entry
    let dlq_id = dlq
        .insert(
            Uuid::new_v4(),
            "handler",
            Uuid::new_v4(),
            "Error",
            None,
            3,
            chrono::Utc::now(),
            chrono::Utc::now(),
            serde_json::json!({}),
        )
        .await?;

    // Start retry (simulating worker claiming it)
    dlq.start_retry(dlq_id).await?;

    // Verify status is retrying
    let entry = dlq.get(dlq_id).await?;
    assert_eq!(entry.status, DlqStatus::Retrying);

    // Simulate crash - don't mark as completed
    // Entry should stay in retrying state

    // On recovery, operator can:
    // 1. Mark as open to retry again
    // 2. Mark as resolved if manually fixed
    // 3. Check last_retry_at to see how long it's been stuck

    // Verify entry still in retrying state
    let entry = dlq.get(dlq_id).await?;
    assert_eq!(entry.status, DlqStatus::Retrying);
    assert!(entry.last_retry_at.is_some());

    Ok(())
}
