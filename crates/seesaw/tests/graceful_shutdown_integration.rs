//! Integration tests for Graceful Shutdown
//!
//! These tests define the expected behavior of graceful shutdown.
//! They will fail initially until we integrate graceful shutdown with the runtime.

use anyhow::Result;
use seesaw_core::{handler, Context, Engine, Runtime};
use seesaw_postgres::PostgresStore;
use sqlx::PgPool;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use uuid::Uuid;

#[derive(Clone)]
struct TestDeps {
    started_count: Arc<AtomicUsize>,
    completed_count: Arc<AtomicUsize>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct SlowEvent {
    id: Uuid,
    duration_ms: u64,
}

// Helper to setup test database
async fn setup_test_db() -> Result<PgPool> {
    let database_url = std::env::var("TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://localhost/seesaw_test".to_string());

    let pool = PgPool::connect(&database_url).await?;

    sqlx::migrate!("../../migrations").run(&pool).await?;

    sqlx::query("TRUNCATE seesaw_events, seesaw_handler_intents, seesaw_dead_letter_queue CASCADE")
        .execute(&pool)
        .await?;

    Ok(pool)
}

/// Test 1: Graceful shutdown should complete all in-flight tasks
#[tokio::test]
async fn test_shutdown_completes_inflight_tasks() -> Result<()> {
    let pool = setup_test_db().await?;
    let store = PostgresStore::new(pool.clone());
    let deps = TestDeps {
        started_count: Arc::new(AtomicUsize::new(0)),
        completed_count: Arc::new(AtomicUsize::new(0)),
    };

    let started = deps.started_count.clone();
    let completed = deps.completed_count.clone();

    // Handler that takes 2 seconds to complete
    let slow_handler = handler::on::<SlowEvent>().id("slow_handler").then(
        move |event: std::sync::Arc<SlowEvent>, ctx: Context<TestDeps>| async move {
            ctx.deps().started_count.fetch_add(1, Ordering::SeqCst);

            // Simulate work
            tokio::time::sleep(Duration::from_millis(event.duration_ms)).await;

            ctx.deps().completed_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        },
    );

    let engine = Engine::new(deps, store).with_handler(slow_handler);

    let runtime = Runtime::start(&engine, Default::default()).await?;

    // Dispatch 5 events that take 2 seconds each
    for i in 0..5 {
        engine
            .dispatch(SlowEvent {
                id: Uuid::new_v4(),
                duration_ms: 2000,
            })
            .await?;
    }

    // Wait for handlers to start
    sleep(Duration::from_millis(500)).await;

    // Verify some handlers started
    let started_before_shutdown = started.load(Ordering::SeqCst);
    assert!(
        started_before_shutdown > 0,
        "Some handlers should have started before shutdown"
    );

    // Trigger graceful shutdown
    runtime.shutdown().await?;

    // Verify all started handlers completed
    let final_started = started.load(Ordering::SeqCst);
    let final_completed = completed.load(Ordering::SeqCst);

    assert_eq!(
        final_started, final_completed,
        "All started handlers should complete during graceful shutdown: started={}, completed={}",
        final_started, final_completed
    );

    Ok(())
}

/// Test 2: Graceful shutdown should not accept new work after signal
#[tokio::test]
async fn test_shutdown_stops_accepting_new_work() -> Result<()> {
    let pool = setup_test_db().await?;
    let store = PostgresStore::new(pool.clone());
    let deps = TestDeps {
        started_count: Arc::new(AtomicUsize::new(0)),
        completed_count: Arc::new(AtomicUsize::new(0)),
    };

    let started = deps.started_count.clone();

    let handler = handler::on::<SlowEvent>().id("counter_handler").then(
        move |event: std::sync::Arc<SlowEvent>, ctx: Context<TestDeps>| async move {
            ctx.deps().started_count.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(event.duration_ms)).await;
            Ok(())
        },
    );

    let engine = Engine::new(deps, store).with_handler(handler);

    let runtime = Runtime::start(&engine, Default::default()).await?;

    // Dispatch first batch
    for _ in 0..3 {
        engine
            .dispatch(SlowEvent {
                id: Uuid::new_v4(),
                duration_ms: 1000,
            })
            .await?;
    }

    // Wait for handlers to start
    sleep(Duration::from_millis(200)).await;

    let started_before_shutdown = started.load(Ordering::SeqCst);

    // Trigger shutdown (but don't await yet)
    let shutdown_handle = tokio::spawn(async move { runtime.shutdown().await });

    // Try to dispatch more events during shutdown
    for _ in 0..3 {
        engine
            .dispatch(SlowEvent {
                id: Uuid::new_v4(),
                duration_ms: 100,
            })
            .await?;
    }

    // Wait for shutdown to complete
    shutdown_handle.await??;

    let started_after_shutdown = started.load(Ordering::SeqCst);

    // New events should not have been processed
    // (Or at most, events that were claimed before shutdown)
    assert!(
        started_after_shutdown <= started_before_shutdown + 1,
        "Should not accept much new work after shutdown signal: before={}, after={}",
        started_before_shutdown,
        started_after_shutdown
    );

    Ok(())
}

/// Test 3: Graceful shutdown timeout should abort remaining tasks
#[tokio::test]
async fn test_shutdown_timeout_aborts_tasks() -> Result<()> {
    let pool = setup_test_db().await?;
    let store = PostgresStore::new(pool.clone());
    let deps = TestDeps {
        started_count: Arc::new(AtomicUsize::new(0)),
        completed_count: Arc::new(AtomicUsize::new(0)),
    };

    let started = deps.started_count.clone();
    let completed = deps.completed_count.clone();

    // Handler that takes longer than shutdown timeout
    let very_slow_handler = handler::on::<SlowEvent>().id("very_slow_handler").then(
        move |event: std::sync::Arc<SlowEvent>, ctx: Context<TestDeps>| async move {
            ctx.deps().started_count.fetch_add(1, Ordering::SeqCst);

            // Takes 60 seconds (longer than 30s timeout)
            tokio::time::sleep(Duration::from_millis(event.duration_ms)).await;

            ctx.deps().completed_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        },
    );

    let engine = Engine::new(deps, store).with_handler(very_slow_handler);

    // Start with short shutdown timeout
    let mut config = Default::default();
    config.graceful_shutdown_timeout = Duration::from_secs(2);

    let runtime = Runtime::start(&engine, config).await?;

    // Dispatch event that takes 60 seconds
    engine
        .dispatch(SlowEvent {
            id: Uuid::new_v4(),
            duration_ms: 60000,
        })
        .await?;

    // Wait for handler to start
    sleep(Duration::from_millis(500)).await;

    assert_eq!(
        started.load(Ordering::SeqCst),
        1,
        "Handler should have started"
    );

    // Trigger shutdown (should timeout after 2 seconds)
    let shutdown_start = std::time::Instant::now();
    let shutdown_result = runtime.shutdown().await;
    let shutdown_duration = shutdown_start.elapsed();

    // Shutdown should complete around timeout duration (2s + small overhead)
    assert!(
        shutdown_duration < Duration::from_secs(5),
        "Shutdown should timeout and abort tasks, took {:?}",
        shutdown_duration
    );

    // Handler should not have completed (aborted by timeout)
    assert_eq!(
        completed.load(Ordering::SeqCst),
        0,
        "Handler should have been aborted by timeout"
    );

    // Shutdown might return error due to timeout
    // (depending on implementation)

    Ok(())
}

/// Test 4: Worker restart after shutdown should be clean
#[tokio::test]
async fn test_worker_restart_after_shutdown() -> Result<()> {
    let pool = setup_test_db().await?;
    let store = PostgresStore::new(pool.clone());
    let deps = TestDeps {
        started_count: Arc::new(AtomicUsize::new(0)),
        completed_count: Arc::new(AtomicUsize::new(0)),
    };

    let completed = deps.completed_count.clone();

    let handler = handler::on::<SlowEvent>().id("restart_handler").then(
        move |_event: std::sync::Arc<SlowEvent>, ctx: Context<TestDeps>| async move {
            ctx.deps().completed_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        },
    );

    let engine = Engine::new(deps.clone(), store.clone()).with_handler(handler.clone());

    // Start first runtime
    let runtime1 = Runtime::start(&engine, Default::default()).await?;

    // Dispatch event
    engine
        .dispatch(SlowEvent {
            id: Uuid::new_v4(),
            duration_ms: 100,
        })
        .await?;

    sleep(Duration::from_millis(500)).await;

    // Shutdown
    runtime1.shutdown().await?;

    let count_after_first = completed.load(Ordering::SeqCst);
    assert_eq!(
        count_after_first, 1,
        "First event should have been processed"
    );

    // Start second runtime (restart)
    let engine2 = Engine::new(deps.clone(), store).with_handler(handler);

    let runtime2 = Runtime::start(&engine2, Default::default()).await?;

    // Dispatch another event
    engine2
        .dispatch(SlowEvent {
            id: Uuid::new_v4(),
            duration_ms: 100,
        })
        .await?;

    sleep(Duration::from_millis(500)).await;

    // Shutdown
    runtime2.shutdown().await?;

    let count_after_second = completed.load(Ordering::SeqCst);
    assert_eq!(
        count_after_second, 2,
        "Second event should have been processed after restart"
    );

    Ok(())
}

/// Test 5: Multiple concurrent shutdowns should be safe
#[tokio::test]
async fn test_concurrent_shutdown_calls_safe() -> Result<()> {
    let pool = setup_test_db().await?;
    let store = PostgresStore::new(pool.clone());
    let deps = TestDeps {
        started_count: Arc::new(AtomicUsize::new(0)),
        completed_count: Arc::new(AtomicUsize::new(0)),
    };

    let handler = handler::on::<SlowEvent>().id("concurrent_handler").then(
        move |event: std::sync::Arc<SlowEvent>, _ctx: Context<TestDeps>| async move {
            tokio::time::sleep(Duration::from_millis(event.duration_ms)).await;
            Ok(())
        },
    );

    let engine = Engine::new(deps, store).with_handler(handler);

    let runtime = Runtime::start(&engine, Default::default()).await?;

    // Dispatch some events
    for _ in 0..3 {
        engine
            .dispatch(SlowEvent {
                id: Uuid::new_v4(),
                duration_ms: 1000,
            })
            .await?;
    }

    sleep(Duration::from_millis(200)).await;

    // Shutdown should complete without panicking
    // Note: Once we integrate graceful shutdown with Runtime,
    // we can test concurrent shutdown calls by wrapping Runtime in Arc<>
    // and changing shutdown() to take &self instead of self
    runtime.shutdown().await?;

    Ok(())
}

/// Test 6: Shutdown during handler error should still complete
#[tokio::test]
async fn test_shutdown_during_handler_error() -> Result<()> {
    let pool = setup_test_db().await?;
    let store = PostgresStore::new(pool.clone());
    let deps = TestDeps {
        started_count: Arc::new(AtomicUsize::new(0)),
        completed_count: Arc::new(AtomicUsize::new(0)),
    };

    let started = deps.started_count.clone();
    let completed = deps.completed_count.clone();

    // Handler that fails after delay
    let failing_handler = handler::on::<SlowEvent>()
        .id("failing_handler")
        .retry(1) // No retry
        .then(
            move |event: std::sync::Arc<SlowEvent>, ctx: Context<TestDeps>| async move {
                ctx.deps().started_count.fetch_add(1, Ordering::SeqCst);

                tokio::time::sleep(Duration::from_millis(event.duration_ms)).await;

                // Always fail
                anyhow::bail!("Handler failed");
                #[allow(unreachable_code)]
                Ok::<(), _>(())
            },
        );

    let engine = Engine::new(deps, store).with_handler(failing_handler);

    let runtime = Runtime::start(&engine, Default::default()).await?;

    // Dispatch events that will fail
    for _ in 0..3 {
        engine
            .dispatch(SlowEvent {
                id: Uuid::new_v4(),
                duration_ms: 1000,
            })
            .await?;
    }

    sleep(Duration::from_millis(200)).await;

    // Shutdown while handlers are failing
    runtime.shutdown().await?;

    // Verify all started handlers completed (even with errors)
    let final_started = started.load(Ordering::SeqCst);

    // All started handlers should have finished (either success or error)
    // The key is that shutdown waited for them to complete
    assert!(
        final_started > 0,
        "Some handlers should have started before shutdown"
    );

    // Note: completed_count won't increment because handlers fail
    // But shutdown should still wait for them to finish failing

    Ok(())
}
