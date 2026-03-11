use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use causal::event_log::EventLog;
use causal::types::NewEvent;
use causal::MemoryStore;
use causal_replay::{Mode, PointerStatus, PointerStore, ProjectionStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;

// ── In-memory PointerStore for tests ────────────────────────────────

struct MemoryPointerStore {
    active: Mutex<u64>,
    staged: Mutex<Option<u64>>,
}

impl MemoryPointerStore {
    fn new() -> Self {
        Self {
            active: Mutex::new(0),
            staged: Mutex::new(None),
        }
    }

    async fn active(&self) -> u64 {
        *self.active.lock().await
    }

    async fn staged_value(&self) -> Option<u64> {
        *self.staged.lock().await
    }
}

#[async_trait]
impl PointerStore for MemoryPointerStore {
    async fn version(&self) -> Result<Option<u64>> {
        let v = *self.active.lock().await;
        Ok(Some(v))
    }

    async fn save(&self, position: u64) -> Result<()> {
        *self.active.lock().await = position;
        Ok(())
    }

    async fn stage(&self, position: u64) -> Result<()> {
        *self.staged.lock().await = Some(position);
        Ok(())
    }

    async fn promote(&self) -> Result<u64> {
        let staged = self
            .staged
            .lock()
            .await
            .ok_or_else(|| anyhow::anyhow!("nothing staged"))?;
        *self.active.lock().await = staged;
        *self.staged.lock().await = None;
        Ok(staged)
    }

    async fn set(&self, position: u64) -> Result<()> {
        *self.active.lock().await = position;
        Ok(())
    }

    async fn status(&self) -> Result<PointerStatus> {
        Ok(PointerStatus {
            active: *self.active.lock().await,
            staged: *self.staged.lock().await,
            updated_at: Utc::now(),
        })
    }
}

// ── Helpers ─────────────────────────────────────────────────────────

fn make_event(i: usize) -> NewEvent {
    NewEvent {
        event_id: Uuid::new_v4(),
        parent_id: None,
        correlation_id: Uuid::new_v4(),
        event_type: format!("test:event_{i}"),
        payload: serde_json::json!({ "index": i }),
        created_at: Utc::now(),
        aggregate_type: None,
        aggregate_id: None,
        metadata: serde_json::Map::new(),
        ephemeral: None,
        persistent: true,
    }
}

async fn append_events(store: &MemoryStore, count: usize) {
    for i in 0..count {
        store.append(make_event(i)).await.unwrap();
    }
}

// ── Replay mode tests ───────────────────────────────────────────────

#[tokio::test]
async fn replay_processes_all_events_and_promotes() {
    let log = MemoryStore::new();
    let pointer = MemoryPointerStore::new();
    append_events(&log, 50).await;

    let count = AtomicUsize::new(0);

    ProjectionStream::new(&log, &pointer)
        .mode(Mode::Replay)
        .run(|_event| {
            count.fetch_add(1, Ordering::SeqCst);
            async { Ok(()) }
        })
        .await
        .unwrap();

    assert_eq!(count.load(Ordering::SeqCst), 50);
    // MemoryStore positions start at 1, so last position = 50.
    assert_eq!(pointer.active().await, 50);
    // Staged should be cleared after promote.
    assert_eq!(pointer.staged_value().await, None);
}

#[tokio::test]
async fn replay_empty_log_still_promotes() {
    let log = MemoryStore::new();
    let pointer = MemoryPointerStore::new();

    ProjectionStream::new(&log, &pointer)
        .mode(Mode::Replay)
        .run(|_event| async { Ok(()) })
        .await
        .unwrap();

    // Position 0 staged and promoted (no events processed).
    assert_eq!(pointer.active().await, 0);
}

#[tokio::test]
async fn replay_fail_fast_on_apply_error() {
    let log = MemoryStore::new();
    let pointer = MemoryPointerStore::new();
    append_events(&log, 10).await;

    let count = AtomicUsize::new(0);

    let result = ProjectionStream::new(&log, &pointer)
        .mode(Mode::Replay)
        .run(|_event| {
            let n = count.fetch_add(1, Ordering::SeqCst);
            async move {
                if n == 3 {
                    anyhow::bail!("projection failed on event 3");
                }
                Ok(())
            }
        })
        .await;

    assert!(result.is_err());
    assert_eq!(count.load(Ordering::SeqCst), 4); // processed 0,1,2,3 then stopped
    // Should NOT have promoted.
    assert_eq!(pointer.active().await, 0);
}

#[tokio::test]
async fn replay_checkpoints_during_progress() {
    let log = MemoryStore::new();
    let pointer = MemoryPointerStore::new();
    append_events(&log, 25).await;

    ProjectionStream::new(&log, &pointer)
        .mode(Mode::Replay)
        .checkpoint_interval(10)
        .run(|_event| async { Ok(()) })
        .await
        .unwrap();

    // After replay, active should be final position (promoted).
    assert_eq!(pointer.active().await, 25);
}

#[tokio::test]
async fn replay_promote_if_gate_passes() {
    let log = MemoryStore::new();
    let pointer = MemoryPointerStore::new();
    append_events(&log, 5).await;

    ProjectionStream::new(&log, &pointer)
        .mode(Mode::Replay)
        .promote_if(|| async { Ok(true) })
        .run(|_event| async { Ok(()) })
        .await
        .unwrap();

    assert_eq!(pointer.active().await, 5);
}

#[tokio::test]
async fn replay_promote_if_gate_fails_stays_staged() {
    let log = MemoryStore::new();
    let pointer = MemoryPointerStore::new();
    append_events(&log, 5).await;

    let result = ProjectionStream::new(&log, &pointer)
        .mode(Mode::Replay)
        .promote_if(|| async { Ok(false) })
        .run(|_event| async { Ok(()) })
        .await;

    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("promotion gate failed"));
    // Active should NOT have changed.
    assert_eq!(pointer.active().await, 0);
    // But staged should have the final position.
    assert_eq!(pointer.staged_value().await, Some(5));
}

#[tokio::test]
async fn replay_promote_if_gate_error_propagates() {
    let log = MemoryStore::new();
    let pointer = MemoryPointerStore::new();
    append_events(&log, 5).await;

    let result = ProjectionStream::new(&log, &pointer)
        .mode(Mode::Replay)
        .promote_if(|| async { anyhow::bail!("health check exploded") })
        .run(|_event| async { Ok(()) })
        .await;

    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("health check exploded"));
    assert_eq!(pointer.active().await, 0);
}

#[tokio::test]
async fn replay_respects_batch_size() {
    let log = MemoryStore::new();
    let pointer = MemoryPointerStore::new();
    append_events(&log, 25).await;

    let count = AtomicUsize::new(0);

    // Small batch size forces multiple load_from calls.
    ProjectionStream::new(&log, &pointer)
        .mode(Mode::Replay)
        .batch_size(7)
        .run(|_event| {
            count.fetch_add(1, Ordering::SeqCst);
            async { Ok(()) }
        })
        .await
        .unwrap();

    assert_eq!(count.load(Ordering::SeqCst), 25);
    assert_eq!(pointer.active().await, 25);
}

// ── Live mode tests ─────────────────────────────────────────────────

#[tokio::test]
async fn live_catches_up_from_pointer() {
    let log = MemoryStore::new();
    let pointer = MemoryPointerStore::new();
    append_events(&log, 20).await;

    // Set pointer to position 10 — should only process events after 10.
    pointer.set(10).await.unwrap();

    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen_clone = seen.clone();

    let result = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        ProjectionStream::new(&log, &pointer)
            .mode(Mode::Live)
            .poll_interval(std::time::Duration::from_millis(50))
            .run(|event| {
                let seen = seen_clone.clone();
                let pos = event.position;
                async move {
                    seen.lock().await.push(pos);
                    Ok(())
                }
            }),
    )
    .await;

    // Should timeout (live mode tails forever).
    assert!(result.is_err());

    let positions = seen.lock().await;
    // Positions 11..=20 (after position 10). MemoryStore starts at 1.
    assert_eq!(positions.len(), 10);
    assert_eq!(positions[0], 11);
    assert_eq!(positions[positions.len() - 1], 20);
}

#[tokio::test]
async fn live_log_and_continue_on_error() {
    let log = MemoryStore::new();
    let pointer = MemoryPointerStore::new();
    append_events(&log, 10).await;

    let count = AtomicUsize::new(0);

    let _ = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        ProjectionStream::new(&log, &pointer)
            .mode(Mode::Live)
            .poll_interval(std::time::Duration::from_millis(50))
            .run(|_event| {
                let n = count.fetch_add(1, Ordering::SeqCst);
                async move {
                    if n == 3 || n == 7 {
                        anyhow::bail!("projection error");
                    }
                    Ok(())
                }
            }),
    )
    .await;

    // All 10 events processed despite errors on 3 and 7.
    assert_eq!(count.load(Ordering::SeqCst), 10);
    // Pointer advanced past all events.
    assert_eq!(pointer.active().await, 10);
}

#[tokio::test]
async fn live_saves_pointer_after_catchup_batch() {
    let log = MemoryStore::new();
    let pointer = MemoryPointerStore::new();
    append_events(&log, 5).await;

    let _ = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        ProjectionStream::new(&log, &pointer)
            .mode(Mode::Live)
            .poll_interval(std::time::Duration::from_millis(50))
            .run(|_event| async { Ok(()) }),
    )
    .await;

    assert_eq!(pointer.active().await, 5);
}

#[tokio::test]
async fn live_empty_log_enters_tail() {
    let log = MemoryStore::new();
    let pointer = MemoryPointerStore::new();

    let result = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        ProjectionStream::new(&log, &pointer)
            .mode(Mode::Live)
            .poll_interval(std::time::Duration::from_millis(50))
            .run(|_event| async { Ok(()) }),
    )
    .await;

    assert!(result.is_err()); // timeout = expected (tailing forever)
    assert_eq!(pointer.active().await, 0);
}

// ── Edge cases ──────────────────────────────────────────────────────

#[tokio::test]
async fn replay_single_event() {
    let log = MemoryStore::new();
    let pointer = MemoryPointerStore::new();
    append_events(&log, 1).await;

    let count = AtomicUsize::new(0);

    ProjectionStream::new(&log, &pointer)
        .mode(Mode::Replay)
        .run(|_event| {
            count.fetch_add(1, Ordering::SeqCst);
            async { Ok(()) }
        })
        .await
        .unwrap();

    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert_eq!(pointer.active().await, 1);
}

#[tokio::test]
async fn replay_error_on_first_event_processes_nothing() {
    let log = MemoryStore::new();
    let pointer = MemoryPointerStore::new();
    append_events(&log, 5).await;

    let result = ProjectionStream::new(&log, &pointer)
        .mode(Mode::Replay)
        .run(|_event| async { anyhow::bail!("immediate failure") })
        .await;

    assert!(result.is_err());
    // Active should not have changed (never promoted).
    assert_eq!(pointer.active().await, 0);
    // Staged should reflect the error on the first event (position 0, never staged).
    assert_eq!(pointer.staged_value().await, None);
}

#[tokio::test]
async fn replay_checkpoint_interval_stages_periodically() {
    let log = MemoryStore::new();
    append_events(&log, 15).await;

    let staged_positions = Arc::new(Mutex::new(Vec::new()));
    let staged_clone = staged_positions.clone();

    struct SpyPointer {
        inner: MemoryPointerStore,
        staged_log: Arc<Mutex<Vec<u64>>>,
    }

    #[async_trait]
    impl PointerStore for SpyPointer {
        async fn version(&self) -> Result<Option<u64>> {
            self.inner.version().await
        }
        async fn save(&self, position: u64) -> Result<()> {
            self.inner.save(position).await
        }
        async fn stage(&self, position: u64) -> Result<()> {
            self.staged_log.lock().await.push(position);
            self.inner.stage(position).await
        }
        async fn promote(&self) -> Result<u64> {
            self.inner.promote().await
        }
        async fn set(&self, position: u64) -> Result<()> {
            self.inner.set(position).await
        }
        async fn status(&self) -> Result<PointerStatus> {
            self.inner.status().await
        }
    }

    let spy = SpyPointer {
        inner: MemoryPointerStore::new(),
        staged_log: staged_clone,
    };

    ProjectionStream::new(&log, &spy)
        .mode(Mode::Replay)
        .checkpoint_interval(5)
        .run(|_event| async { Ok(()) })
        .await
        .unwrap();

    let stages = staged_positions.lock().await;
    // MemoryStore positions start at 1.
    // Checkpoint at count 5 (position 5), count 10 (position 10), count 15 (position 15),
    // plus final stage save (also position 15).
    assert!(stages.contains(&5), "stages: {:?}", *stages);
    assert!(stages.contains(&10), "stages: {:?}", *stages);
    assert!(stages.contains(&15), "stages: {:?}", *stages);
}

#[tokio::test]
async fn replay_apply_receives_correct_event_data() {
    let log = MemoryStore::new();
    let pointer = MemoryPointerStore::new();

    for i in 0..3 {
        let mut event = make_event(i);
        event.event_type = format!("test:type_{i}");
        event.payload = serde_json::json!({ "value": i * 10 });
        log.append(event).await.unwrap();
    }

    let types = Arc::new(Mutex::new(Vec::new()));
    let types_clone = types.clone();

    ProjectionStream::new(&log, &pointer)
        .mode(Mode::Replay)
        .run(|event| {
            let types = types_clone.clone();
            let et = event.event_type.clone();
            async move {
                types.lock().await.push(et);
                Ok(())
            }
        })
        .await
        .unwrap();

    let collected = types.lock().await;
    assert_eq!(
        *collected,
        vec!["test:type_0", "test:type_1", "test:type_2"]
    );
}

// ── stream.version() tests ──────────────────────────────────────────

#[tokio::test]
async fn version_live_returns_active_position() {
    let log = MemoryStore::new();
    let pointer = MemoryPointerStore::new();
    append_events(&log, 50).await;

    // Simulate a previous replay that promoted to position 50.
    pointer.set(50).await.unwrap();

    let stream = ProjectionStream::new(&log, &pointer).mode(Mode::Live);
    let version = stream.version().await.unwrap();

    assert_eq!(version, 50);
}

#[tokio::test]
async fn version_live_returns_zero_when_no_prior_replay() {
    let log = MemoryStore::new();
    let pointer = MemoryPointerStore::new();

    let stream = ProjectionStream::new(&log, &pointer).mode(Mode::Live);
    let version = stream.version().await.unwrap();

    assert_eq!(version, 0);
}

#[tokio::test]
async fn version_replay_snapshots_latest_position() {
    let log = MemoryStore::new();
    let pointer = MemoryPointerStore::new();
    append_events(&log, 30).await;

    let stream = ProjectionStream::new(&log, &pointer).mode(Mode::Replay);
    let version = stream.version().await.unwrap();

    // Should return latest_position() from event log, not active.
    assert_eq!(version, 30);
}

#[tokio::test]
async fn version_replay_stages_the_target() {
    let log = MemoryStore::new();
    let pointer = MemoryPointerStore::new();
    append_events(&log, 25).await;

    let stream = ProjectionStream::new(&log, &pointer).mode(Mode::Replay);
    let version = stream.version().await.unwrap();

    assert_eq!(version, 25);
    // Staged should hold the snapshot.
    assert_eq!(pointer.staged_value().await, Some(25));
    // Active should NOT have changed.
    assert_eq!(pointer.active().await, 0);
}

#[tokio::test]
async fn version_replay_empty_log_returns_zero() {
    let log = MemoryStore::new();
    let pointer = MemoryPointerStore::new();

    let stream = ProjectionStream::new(&log, &pointer).mode(Mode::Replay);
    let version = stream.version().await.unwrap();

    assert_eq!(version, 0);
    assert_eq!(pointer.staged_value().await, Some(0));
}

#[tokio::test]
async fn version_replay_matches_promoted_active_after_run() {
    let log = MemoryStore::new();
    let pointer = MemoryPointerStore::new();
    append_events(&log, 40).await;

    let stream = ProjectionStream::new(&log, &pointer).mode(Mode::Replay);
    let version = stream.version().await.unwrap();

    // Run the replay — should process all events and promote.
    stream
        .run(|_event| async { Ok(()) })
        .await
        .unwrap();

    // After promotion, active == the version we got before run().
    assert_eq!(pointer.active().await, version);
    assert_eq!(version, 40);
}

#[tokio::test]
async fn version_live_then_run_catches_up_from_version() {
    let log = MemoryStore::new();
    let pointer = MemoryPointerStore::new();
    append_events(&log, 20).await;

    // Simulate prior replay promoted to position 10.
    pointer.set(10).await.unwrap();

    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen_clone = seen.clone();

    let stream = ProjectionStream::new(&log, &pointer)
        .mode(Mode::Live)
        .poll_interval(std::time::Duration::from_millis(50));

    let version = stream.version().await.unwrap();
    assert_eq!(version, 10);

    // Live mode catches up from version (position 10).
    let _ = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        stream.run(|event| {
            let seen = seen_clone.clone();
            let pos = event.position;
            async move {
                seen.lock().await.push(pos);
                Ok(())
            }
        }),
    )
    .await;

    let positions = seen.lock().await;
    // Should have processed events 11..=20 (after position 10).
    assert_eq!(positions.len(), 10);
    assert_eq!(positions[0], 11);
    assert_eq!(positions[positions.len() - 1], 20);
}

// ── run_batch tests ──────────────────────────────────────────────────

#[tokio::test]
async fn run_batch_replay_processes_all_events_and_promotes() {
    let log = MemoryStore::new();
    let pointer = MemoryPointerStore::new();
    append_events(&log, 50).await;

    let count = AtomicUsize::new(0);

    ProjectionStream::new(&log, &pointer)
        .mode(Mode::Replay)
        .run_batch(|batch| {
            count.fetch_add(batch.len(), Ordering::SeqCst);
            async { Ok(()) }
        })
        .await
        .unwrap();

    assert_eq!(count.load(Ordering::SeqCst), 50);
    assert_eq!(pointer.active().await, 50);
    assert_eq!(pointer.staged_value().await, None);
}

#[tokio::test]
async fn run_batch_replay_callback_receives_whole_batch() {
    let log = MemoryStore::new();
    let pointer = MemoryPointerStore::new();
    append_events(&log, 25).await;

    let batch_sizes = Arc::new(Mutex::new(Vec::new()));
    let batch_sizes_clone = batch_sizes.clone();

    ProjectionStream::new(&log, &pointer)
        .mode(Mode::Replay)
        .batch_size(7)
        .run_batch(|batch| {
            let sizes = batch_sizes_clone.clone();
            let len = batch.len();
            async move {
                sizes.lock().await.push(len);
                Ok(())
            }
        })
        .await
        .unwrap();

    let sizes = batch_sizes.lock().await;
    // 25 events with batch_size 7: batches of 7, 7, 7, 4
    assert_eq!(*sizes, vec![7, 7, 7, 4]);
    assert_eq!(pointer.active().await, 25);
}

#[tokio::test]
async fn run_batch_replay_fail_fast_on_batch_error() {
    let log = MemoryStore::new();
    let pointer = MemoryPointerStore::new();
    append_events(&log, 20).await;

    let call_count = AtomicUsize::new(0);

    let result = ProjectionStream::new(&log, &pointer)
        .mode(Mode::Replay)
        .batch_size(5)
        .run_batch(|_batch| {
            let n = call_count.fetch_add(1, Ordering::SeqCst);
            async move {
                if n == 2 {
                    anyhow::bail!("batch 2 failed");
                }
                Ok(())
            }
        })
        .await;

    assert!(result.is_err());
    // 3 batches attempted (0, 1, 2) — failed on third.
    assert_eq!(call_count.load(Ordering::SeqCst), 3);
    assert_eq!(pointer.active().await, 0); // not promoted
}

#[tokio::test]
async fn run_batch_replay_checkpoints_after_each_batch() {
    let log = MemoryStore::new();
    append_events(&log, 15).await;

    let staged_positions = Arc::new(Mutex::new(Vec::new()));
    let staged_clone = staged_positions.clone();

    struct SpyPointer2 {
        inner: MemoryPointerStore,
        staged_log: Arc<Mutex<Vec<u64>>>,
    }

    #[async_trait]
    impl PointerStore for SpyPointer2 {
        async fn version(&self) -> Result<Option<u64>> {
            self.inner.version().await
        }
        async fn save(&self, position: u64) -> Result<()> {
            self.inner.save(position).await
        }
        async fn stage(&self, position: u64) -> Result<()> {
            self.staged_log.lock().await.push(position);
            self.inner.stage(position).await
        }
        async fn promote(&self) -> Result<u64> {
            self.inner.promote().await
        }
        async fn set(&self, position: u64) -> Result<()> {
            self.inner.set(position).await
        }
        async fn status(&self) -> Result<PointerStatus> {
            self.inner.status().await
        }
    }

    let spy = SpyPointer2 {
        inner: MemoryPointerStore::new(),
        staged_log: staged_clone,
    };

    ProjectionStream::new(&log, &spy)
        .mode(Mode::Replay)
        .batch_size(5)
        .run_batch(|_batch| async { Ok(()) })
        .await
        .unwrap();

    let stages = staged_positions.lock().await;
    // 3 batches: position 5, 10, 15, plus final stage (15 again).
    assert!(stages.contains(&5), "stages: {:?}", *stages);
    assert!(stages.contains(&10), "stages: {:?}", *stages);
    assert!(stages.contains(&15), "stages: {:?}", *stages);
}

#[tokio::test]
async fn run_batch_replay_progress_callback_fires() {
    let log = MemoryStore::new();
    let pointer = MemoryPointerStore::new();
    append_events(&log, 20).await;

    let progress_log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let progress_clone = progress_log.clone();

    ProjectionStream::new(&log, &pointer)
        .mode(Mode::Replay)
        .batch_size(7)
        .on_progress(move |p| {
            progress_clone.lock().unwrap().push((p.processed, p.position, p.target));
        })
        .run_batch(|_batch| async { Ok(()) })
        .await
        .unwrap();

    let log = progress_log.lock().unwrap();
    // 3 batches (7, 7, 6) + final = 4 progress calls.
    // Last call is the finish_replay call with processed=20.
    assert!(log.len() >= 3);
    let last = log.last().unwrap();
    assert_eq!(last.0, 20); // processed
    assert_eq!(last.1, 20); // position
}

#[tokio::test]
async fn run_batch_replay_empty_log_still_promotes() {
    let log = MemoryStore::new();
    let pointer = MemoryPointerStore::new();

    ProjectionStream::new(&log, &pointer)
        .mode(Mode::Replay)
        .run_batch(|_batch| async { Ok(()) })
        .await
        .unwrap();

    assert_eq!(pointer.active().await, 0);
}

#[tokio::test]
async fn run_batch_live_catches_up_from_pointer() {
    let log = MemoryStore::new();
    let pointer = MemoryPointerStore::new();
    append_events(&log, 20).await;

    pointer.set(10).await.unwrap();

    let batch_sizes = Arc::new(Mutex::new(Vec::new()));
    let batch_sizes_clone = batch_sizes.clone();

    let _ = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        ProjectionStream::new(&log, &pointer)
            .mode(Mode::Live)
            .poll_interval(std::time::Duration::from_millis(50))
            .run_batch(|batch| {
                let sizes = batch_sizes_clone.clone();
                let len = batch.len();
                async move {
                    sizes.lock().await.push(len);
                    Ok(())
                }
            }),
    )
    .await;

    let sizes = batch_sizes.lock().await;
    let total: usize = sizes.iter().sum();
    assert_eq!(total, 10); // events 11..=20
    assert_eq!(pointer.active().await, 20);
}

#[tokio::test]
async fn run_batch_live_log_and_continue_on_error() {
    let log = MemoryStore::new();
    let pointer = MemoryPointerStore::new();
    append_events(&log, 10).await;

    let call_count = AtomicUsize::new(0);

    let _ = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        ProjectionStream::new(&log, &pointer)
            .mode(Mode::Live)
            .batch_size(5)
            .poll_interval(std::time::Duration::from_millis(50))
            .run_batch(|_batch| {
                let n = call_count.fetch_add(1, Ordering::SeqCst);
                async move {
                    if n == 0 {
                        anyhow::bail!("first batch fails");
                    }
                    Ok(())
                }
            }),
    )
    .await;

    // Both batches processed despite first failing.
    assert!(call_count.load(Ordering::SeqCst) >= 2);
    // Pointer still advances past all events.
    assert_eq!(pointer.active().await, 10);
}
