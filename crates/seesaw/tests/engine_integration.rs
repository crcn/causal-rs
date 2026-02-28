//! Integration tests for Engine settle loop.

use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::Result;
use parking_lot::Mutex;
use seesaw_core::insight::InsightEvent;
use seesaw_core::{emit, handler, Context, Engine, Events};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone)]
struct Deps;

// -- Event types --

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Ping {
    msg: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EventA {
    value: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EventB {
    value: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FailEvent {
    attempt: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FailedTerminal {
    error: String,
    attempts: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BatchItem {
    index: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BatchResult {
    count: usize,
}

// -- Tests --

#[tokio::test]
async fn basic_handler_fires() -> Result<()> {
    let counter = Arc::new(AtomicUsize::new(0));
    let counter_clone = counter.clone();

    let engine = Engine::new(Deps).with_handler(
        handler::on::<Ping>().then(move |_event: Arc<Ping>, _ctx: Context<Deps>| {
            let c = counter_clone.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok(emit![])
            }
        }),
    );

    engine
        .dispatch(Ping {
            msg: "hello".into(),
        })
        .settled()
        .await?;

    assert_eq!(counter.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn handler_emits_chain() -> Result<()> {
    let b_counter = Arc::new(AtomicUsize::new(0));
    let b_counter_clone = b_counter.clone();

    let engine = Engine::new(Deps)
        .with_handler(handler::on::<EventA>().then(
            |event: Arc<EventA>, _ctx: Context<Deps>| async move {
                Ok(emit![EventB {
                    value: event.value + 1,
                }])
            },
        ))
        .with_handler(handler::on::<EventB>().then(
            move |event: Arc<EventB>, _ctx: Context<Deps>| {
                let c = b_counter_clone.clone();
                async move {
                    c.fetch_add(event.value as usize, Ordering::SeqCst);
                    Ok(emit![])
                }
            },
        ));

    engine.dispatch(EventA { value: 10 }).settled().await?;

    assert_eq!(b_counter.load(Ordering::SeqCst), 11);
    Ok(())
}

#[tokio::test]
async fn multiple_handlers_same_event() -> Result<()> {
    let counter_a = Arc::new(AtomicUsize::new(0));
    let counter_b = Arc::new(AtomicUsize::new(0));
    let ca = counter_a.clone();
    let cb = counter_b.clone();

    let engine = Engine::new(Deps)
        .with_handler(
            handler::on::<Ping>()
                .id("ping_handler_a")
                .then(move |_event: Arc<Ping>, _ctx: Context<Deps>| {
                    let c = ca.clone();
                    async move {
                        c.fetch_add(1, Ordering::SeqCst);
                        Ok(emit![])
                    }
                }),
        )
        .with_handler(
            handler::on::<Ping>()
                .id("ping_handler_b")
                .then(move |_event: Arc<Ping>, _ctx: Context<Deps>| {
                    let c = cb.clone();
                    async move {
                        c.fetch_add(10, Ordering::SeqCst);
                        Ok(emit![])
                    }
                }),
        );

    engine
        .dispatch(Ping {
            msg: "hello".into(),
        })
        .settled()
        .await?;

    assert_eq!(counter_a.load(Ordering::SeqCst), 1);
    assert_eq!(counter_b.load(Ordering::SeqCst), 10);
    Ok(())
}

#[tokio::test]
async fn queued_handler_executes() -> Result<()> {
    let counter = Arc::new(AtomicUsize::new(0));
    let counter_clone = counter.clone();

    let engine = Engine::new(Deps).with_handler(
        handler::on::<Ping>()
            .id("queued_ping")
            .queued()
            .then(move |_event: Arc<Ping>, _ctx: Context<Deps>| {
                let c = counter_clone.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Ok(emit![])
                }
            }),
    );

    engine
        .dispatch(Ping {
            msg: "hello".into(),
        })
        .settled()
        .await?;

    assert_eq!(counter.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn dispatch_requires_settled() -> Result<()> {
    let counter = Arc::new(AtomicUsize::new(0));
    let counter_clone = counter.clone();

    let engine = Engine::new(Deps).with_handler(
        handler::on::<Ping>()
            .id("queued_fire_forget")
            .queued()
            .then(move |_event: Arc<Ping>, _ctx: Context<Deps>| {
                let c = counter_clone.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Ok(emit![])
                }
            }),
    );

    // Fire-and-forget: dispatch without settled
    let _handle = engine
        .dispatch(Ping {
            msg: "hello".into(),
        })
        .await?;

    // Queued handler has not run yet (no settle)
    assert_eq!(counter.load(Ordering::SeqCst), 0);

    // Now settle
    engine.settle().await?;
    assert_eq!(counter.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn handler_returns_nothing() -> Result<()> {
    let engine = Engine::new(Deps).with_handler(
        handler::on::<Ping>().then(|_event: Arc<Ping>, _ctx: Context<Deps>| async move {
            Ok(emit![])
        }),
    );

    engine
        .dispatch(Ping {
            msg: "hello".into(),
        })
        .settled()
        .await?;

    Ok(())
}

#[tokio::test]
async fn retry_succeeds_on_second_attempt() -> Result<()> {
    let attempt_counter = Arc::new(AtomicI32::new(0));
    let success_counter = Arc::new(AtomicUsize::new(0));
    let ac = attempt_counter.clone();
    let sc = success_counter.clone();

    let engine = Engine::new(Deps).with_handler(
        handler::on::<FailEvent>()
            .id("retry_handler")
            .queued()
            .retry(3)
            .then(move |_event: Arc<FailEvent>, _ctx: Context<Deps>| {
                let ac = ac.clone();
                let sc = sc.clone();
                async move {
                    let attempt = ac.fetch_add(1, Ordering::SeqCst);
                    if attempt == 0 {
                        anyhow::bail!("first attempt fails");
                    }
                    sc.fetch_add(1, Ordering::SeqCst);
                    Ok(emit![])
                }
            }),
    );

    engine
        .dispatch(FailEvent { attempt: 0 })
        .settled()
        .await?;

    assert!(
        attempt_counter.load(Ordering::SeqCst) >= 2,
        "should have been called at least twice"
    );
    assert_eq!(
        success_counter.load(Ordering::SeqCst),
        1,
        "should succeed on retry"
    );
    Ok(())
}

#[tokio::test]
async fn dlq_terminal_event_published() -> Result<()> {
    let terminal_counter = Arc::new(AtomicUsize::new(0));
    let tc = terminal_counter.clone();

    let engine = Engine::new(Deps)
        .with_handler(
            handler::on::<FailEvent>()
                .id("always_fail")
                .queued()
                .retry(1)
                .on_failure(|_event: Arc<FailEvent>, info: seesaw_core::ErrorContext| {
                    FailedTerminal {
                        error: info.error,
                        attempts: info.attempts,
                    }
                })
                .then(|_event: Arc<FailEvent>, _ctx: Context<Deps>| async move {
                    Err::<Events, _>(anyhow::anyhow!("always fails"))
                }),
        )
        .with_handler(handler::on::<FailedTerminal>().then(
            move |_event: Arc<FailedTerminal>, _ctx: Context<Deps>| {
                let c = tc.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Ok(emit![])
                }
            },
        ));

    engine
        .dispatch(FailEvent { attempt: 0 })
        .settled()
        .await?;

    assert_eq!(
        terminal_counter.load(Ordering::SeqCst),
        1,
        "terminal event from on_failure should be published and handled"
    );
    Ok(())
}

#[tokio::test]
async fn accumulate_batch() -> Result<()> {
    let result_counter = Arc::new(AtomicUsize::new(0));
    let batch_size_seen = Arc::new(AtomicUsize::new(0));
    let rc = result_counter.clone();
    let bs = batch_size_seen.clone();

    // An inline handler emits a batch which sets batch metadata automatically.
    // The accumulate handler collects all items in the batch.
    let engine = Engine::new(Deps)
        .with_handler(handler::on::<Ping>().then(
            |_event: Arc<Ping>, _ctx: Context<Deps>| async move {
                Ok(emit![..vec![
                    BatchItem { index: 0 },
                    BatchItem { index: 1 },
                    BatchItem { index: 2 },
                ]])
            },
        ))
        .with_handler(
            handler::on::<BatchItem>()
                .id("batch_accumulator")
                .accumulate()
                .then(move |batch: Vec<BatchItem>, _ctx: Context<Deps>| {
                    let rc = rc.clone();
                    let bs = bs.clone();
                    async move {
                        bs.store(batch.len(), Ordering::SeqCst);
                        rc.fetch_add(1, Ordering::SeqCst);
                        Ok(emit![BatchResult { count: batch.len() }])
                    }
                }),
        );

    engine
        .dispatch(Ping {
            msg: "trigger".into(),
        })
        .settled()
        .await?;

    assert_eq!(
        result_counter.load(Ordering::SeqCst),
        1,
        "batch handler should fire exactly once"
    );
    assert_eq!(
        batch_size_seen.load(Ordering::SeqCst),
        3,
        "batch should contain all 3 items"
    );
    Ok(())
}

#[tokio::test]
async fn insight_callback_fires() -> Result<()> {
    let insight_count = Arc::new(AtomicUsize::new(0));
    let ic = insight_count.clone();

    let engine = Engine::new(Deps)
        .with_on_insight(move |_event| {
            ic.fetch_add(1, Ordering::SeqCst);
        })
        .with_handler(handler::on::<Ping>().then(
            |_event: Arc<Ping>, _ctx: Context<Deps>| async move { Ok(emit![]) },
        ));

    engine
        .dispatch(Ping {
            msg: "hello".into(),
        })
        .settled()
        .await?;

    assert!(
        insight_count.load(Ordering::SeqCst) >= 1,
        "insight callback should fire at least once (for EventDispatched)"
    );
    Ok(())
}

#[tokio::test]
async fn correlation_preserved_through_queued_chain() -> Result<()> {
    let seen_correlation: Arc<Mutex<Option<Uuid>>> = Arc::new(Mutex::new(None));
    let sc = seen_correlation.clone();

    let engine = Engine::new(Deps)
        .with_handler(
            handler::on::<EventA>()
                .id("emit_b_queued")
                .queued()
                .then(|event: Arc<EventA>, _ctx: Context<Deps>| async move {
                    Ok(emit![EventB {
                        value: event.value + 1,
                    }])
                }),
        )
        .with_handler(handler::on::<EventB>().then(
            move |_event: Arc<EventB>, ctx: Context<Deps>| {
                let sc = sc.clone();
                async move {
                    *sc.lock() = Some(ctx.correlation_id);
                    Ok(emit![])
                }
            },
        ));

    let handle = engine.dispatch(EventA { value: 1 }).settled().await?;

    let seen = seen_correlation.lock().expect("EventB handler should have run");
    assert_eq!(
        seen, handle.correlation_id,
        "EventB emitted by queued handler must carry the original correlation_id"
    );
    Ok(())
}

#[tokio::test]
async fn dlq_terminal_preserves_correlation() -> Result<()> {
    let seen_correlation: Arc<Mutex<Option<Uuid>>> = Arc::new(Mutex::new(None));
    let sc = seen_correlation.clone();

    let engine = Engine::new(Deps)
        .with_handler(
            handler::on::<FailEvent>()
                .id("always_fail_corr")
                .queued()
                .retry(1)
                .on_failure(|_event: Arc<FailEvent>, info: seesaw_core::ErrorContext| {
                    FailedTerminal {
                        error: info.error,
                        attempts: info.attempts,
                    }
                })
                .then(|_event: Arc<FailEvent>, _ctx: Context<Deps>| async move {
                    Err::<Events, _>(anyhow::anyhow!("always fails"))
                }),
        )
        .with_handler(handler::on::<FailedTerminal>().then(
            move |_event: Arc<FailedTerminal>, ctx: Context<Deps>| {
                let sc = sc.clone();
                async move {
                    *sc.lock() = Some(ctx.correlation_id);
                    Ok(emit![])
                }
            },
        ));

    let handle = engine
        .dispatch(FailEvent { attempt: 0 })
        .settled()
        .await?;

    let seen = seen_correlation
        .lock()
        .expect("FailedTerminal handler should have run");
    assert_eq!(
        seen, handle.correlation_id,
        "DLQ terminal event must carry the original correlation_id"
    );
    Ok(())
}

#[tokio::test]
async fn insight_seq_monotonically_increases() -> Result<()> {
    let collected: Arc<Mutex<Vec<InsightEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let cc = collected.clone();

    let engine = Engine::new(Deps)
        .with_on_insight(move |event| {
            cc.lock().push(event);
        })
        .with_handler(
            handler::on::<EventA>()
                .id("queued_for_insight")
                .queued()
                .then(|_event: Arc<EventA>, _ctx: Context<Deps>| async move { Ok(emit![]) }),
        );

    engine.dispatch(EventA { value: 42 }).settled().await?;

    let events = collected.lock();
    assert!(
        events.len() >= 2,
        "should have at least EventDispatched + EffectCompleted, got {}",
        events.len()
    );

    for window in events.windows(2) {
        assert!(
            window[1].seq > window[0].seq,
            "insight seq must be strictly increasing: {} should be > {}",
            window[1].seq,
            window[0].seq
        );
    }

    // All seq values should be > 0
    for event in events.iter() {
        assert!(event.seq > 0, "insight seq should never be 0, got 0");
    }

    Ok(())
}
