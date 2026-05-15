//! Integration tests for `MirroringEventLogBackend` (Phase 4f.1).
//!
//! Tests the wrapper's dual-write logic against two `MemoryStore`
//! children. The integration with the actual rootsignal legacy store
//! is 4f.2.

use anyhow::Result;
use async_trait::async_trait;
use causal::types::{WriteResult, LogCursor, EventData, RecordedEvent, StreamRevision, StreamState};
use causal::{EventLogBackend, MemoryStore};
use causal_replay::MirroringEventLogBackend;
use chrono::Utc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use uuid::Uuid;

fn make_event(correlation: Uuid, event_type: &str) -> EventData {
    EventData {
        event_id: Uuid::new_v4(),
        causation_id: None,
        correlation_id: correlation,
        event_type: event_type.to_string(),
        payload: serde_json::json!({}),
        created_at: Utc::now(),
        aggregate_type: None,
        aggregate_id: None,
        metadata: serde_json::Map::new(),
        ephemeral: None,
        persistent: true,
    }
}

#[tokio::test]
async fn append_writes_to_both_children() -> Result<()> {
    let legacy: Arc<dyn EventLogBackend> = Arc::new(MemoryStore::new());
    let v03: Arc<dyn EventLogBackend> = Arc::new(MemoryStore::new());
    let mirror = MirroringEventLogBackend::new(legacy.clone(), v03.clone());

    let correlation = Uuid::new_v4();
    let event = make_event(correlation, "test:dual");
    let event_id = event.event_id;

    mirror.append(event).await?;

    let legacy_events = legacy.read_all(LogCursor::ZERO, 100).await?;
    let v03_events = v03.read_all(LogCursor::ZERO, 100).await?;

    assert_eq!(legacy_events.len(), 1, "legacy must have the row");
    assert_eq!(v03_events.len(), 1, "v0.3 mirror must have the row");
    assert_eq!(legacy_events[0].event_id, event_id);
    assert_eq!(v03_events[0].event_id, event_id);
    Ok(())
}

#[tokio::test]
async fn read_paths_delegate_to_legacy() -> Result<()> {
    // The wrapper's `read_all`, `read_stream`, `latest_position` all
    // read from legacy. To verify, write to legacy directly (bypassing
    // the wrapper) and confirm `mirror.read_all` sees it; then write
    // to v03 directly and confirm `mirror.read_all` does NOT see it.
    let legacy: Arc<dyn EventLogBackend> = Arc::new(MemoryStore::new());
    let v03: Arc<dyn EventLogBackend> = Arc::new(MemoryStore::new());
    let mirror = MirroringEventLogBackend::new(legacy.clone(), v03.clone());

    legacy
        .append(make_event(Uuid::new_v4(), "legacy:only"))
        .await?;

    let from_mirror = mirror.read_all(LogCursor::ZERO, 100).await?;
    assert_eq!(from_mirror.len(), 1);
    assert_eq!(from_mirror[0].event_type, "legacy:only");

    // Now write to v03 only — mirror should NOT see it via read_all.
    v03.append(make_event(Uuid::new_v4(), "v03:only")).await?;
    let from_mirror_again = mirror.read_all(LogCursor::ZERO, 100).await?;
    assert_eq!(
        from_mirror_again.len(),
        1,
        "mirror.read_all should still only see the legacy row"
    );
    assert_eq!(from_mirror_again[0].event_type, "legacy:only");
    Ok(())
}

#[tokio::test]
async fn legacy_failure_propagates_and_skips_v03_write() -> Result<()> {
    // If the legacy write fails, the mirror call returns Err and the
    // v0.3 write is never attempted. This is the expected sequencing.
    let legacy: Arc<dyn EventLogBackend> = Arc::new(FailingBackend {
        fail: AtomicBool::new(true),
    });
    let v03 = Arc::new(MemoryStore::new());
    let mirror = MirroringEventLogBackend::new(legacy, v03.clone());

    let result = mirror
        .append(make_event(Uuid::new_v4(), "test:fail"))
        .await;
    assert!(result.is_err(), "wrapper must propagate legacy failure");

    let v03_events = v03.read_all(LogCursor::ZERO, 100).await?;
    assert!(
        v03_events.is_empty(),
        "v0.3 mirror must NOT have the row after legacy failure"
    );
    Ok(())
}

#[tokio::test]
async fn v03_failure_after_legacy_success_leaves_legacy_committed() -> Result<()> {
    // Sequential dual-write: if legacy succeeds and v0.3 fails, legacy
    // is already committed (orphan). Idempotent retry recovers — same
    // event_id retried later is a no-op on legacy and completes v0.3.
    let legacy: Arc<dyn EventLogBackend> = Arc::new(MemoryStore::new());
    let v03 = Arc::new(FailingBackend {
        fail: AtomicBool::new(true),
    });
    let mirror = MirroringEventLogBackend::new(legacy.clone(), v03.clone());

    let event = make_event(Uuid::new_v4(), "test:partial");
    let event_id = event.event_id;
    let result = mirror.append(event.clone()).await;
    assert!(result.is_err(), "wrapper must propagate v0.3 failure");

    // Legacy has the row — observable inconsistency until retry.
    let legacy_events = legacy.read_all(LogCursor::ZERO, 100).await?;
    assert_eq!(legacy_events.len(), 1);
    assert_eq!(legacy_events[0].event_id, event_id);

    // Recovery: v0.3 stops failing; retry with same event_id; both
    // converge.
    v03.fail.store(false, Ordering::SeqCst);
    mirror.append(event).await?;

    // Legacy still has just the one row (idempotent on event_id).
    let legacy_events_after = legacy.read_all(LogCursor::ZERO, 100).await?;
    assert_eq!(legacy_events_after.len(), 1);

    // v0.3 mirror now ALSO has it. Verify via read_all on the
    // FailingBackend's "succeed" mode (its read_all returns empty
    // since FailingBackend is a stub — for a real verification we'd
    // need to use MemoryStore on the v0.3 side; what this test
    // demonstrates is that the wrapper's retry semantics work, not
    // that FailingBackend persists).
    Ok(())
}

#[tokio::test]
async fn retry_recovers_when_v03_recovers() -> Result<()> {
    // Tighter version of the previous test using MemoryStore on the
    // v0.3 side via a "drop first call, succeed thereafter" wrapper.
    let legacy: Arc<dyn EventLogBackend> = Arc::new(MemoryStore::new());
    let v03_inner = Arc::new(MemoryStore::new());
    let v03_gated: Arc<dyn EventLogBackend> = Arc::new(GatedBackend {
        inner: v03_inner.clone(),
        fail_first_n: AtomicBool::new(true),
    });
    let mirror = MirroringEventLogBackend::new(legacy.clone(), v03_gated);

    let event = make_event(Uuid::new_v4(), "test:retry");
    let event_id = event.event_id;

    // First call: legacy succeeds, v0.3 fails.
    let r = mirror.append(event.clone()).await;
    assert!(r.is_err());

    // Retry: v0.3 should now succeed; legacy is idempotent.
    let r2 = mirror.append(event).await;
    assert!(r2.is_ok(), "retry must succeed once v0.3 recovers");

    // Both have it.
    let legacy_events = legacy.read_all(LogCursor::ZERO, 100).await?;
    assert_eq!(legacy_events.len(), 1);
    let v03_events = v03_inner.read_all(LogCursor::ZERO, 100).await?;
    assert_eq!(v03_events.len(), 1);
    assert_eq!(legacy_events[0].event_id, event_id);
    assert_eq!(v03_events[0].event_id, event_id);
    Ok(())
}

/// Backend that fails the first append, then delegates to inner.
struct GatedBackend {
    inner: Arc<MemoryStore>,
    fail_first_n: AtomicBool,
}

#[async_trait]
impl EventLogBackend for GatedBackend {
    async fn append(&self, event: EventData) -> Result<WriteResult> {
        if self.fail_first_n.swap(false, Ordering::SeqCst) {
            Err(anyhow::anyhow!("gated: failing first call"))
        } else {
            self.inner.append(event).await
        }
    }
    async fn read_all(&self, after: LogCursor, limit: usize) -> Result<Vec<RecordedEvent>> {
        self.inner.read_all(after, limit).await
    }
    async fn read_stream(
        &self,
        a: &str,
        b: Uuid,
        c: Option<StreamRevision>,
    ) -> Result<Vec<RecordedEvent>> {
        self.inner.read_stream(a, b, c).await
    }
    async fn latest_position(&self) -> Result<LogCursor> {
        self.inner.latest_position().await
    }
    async fn append_to_stream(
        &self,
        a: &str,
        b: Uuid,
        c: StreamState,
        event: EventData,
    ) -> Result<WriteResult> {
        self.inner.append_to_stream(a, b, c, event).await
    }
}

#[tokio::test]
async fn parity_check_returns_true_when_both_have_event() -> Result<()> {
    let legacy: Arc<dyn EventLogBackend> = Arc::new(MemoryStore::new());
    let v03: Arc<dyn EventLogBackend> = Arc::new(MemoryStore::new());
    let mirror = MirroringEventLogBackend::new(legacy, v03);

    let event = make_event(Uuid::new_v4(), "test:parity");
    let event_id = event.event_id;
    mirror.append(event).await?;

    assert!(
        mirror.parity_check_event(event_id).await?,
        "both stores should have the row after a successful dual-write"
    );

    // An event_id we never wrote.
    assert!(
        !mirror.parity_check_event(Uuid::new_v4()).await?,
        "parity_check_event should return false for unknown event_id"
    );
    Ok(())
}

// ─── Test helpers ───────────────────────────────────────────────────

/// Always-fails (or toggles) backend for testing failure propagation.
struct FailingBackend {
    fail: AtomicBool,
}

#[async_trait]
impl EventLogBackend for FailingBackend {
    async fn append(&self, _event: EventData) -> Result<WriteResult> {
        if self.fail.load(Ordering::SeqCst) {
            Err(anyhow::anyhow!("FailingBackend: configured to fail"))
        } else {
            Ok(WriteResult {
                position: LogCursor::from_raw(1),
                revision: None,
            })
        }
    }

    async fn read_all(&self, _: LogCursor, _: usize) -> Result<Vec<RecordedEvent>> {
        Ok(Vec::new())
    }

    async fn read_stream(
        &self,
        _: &str,
        _: Uuid,
        _: Option<StreamRevision>,
    ) -> Result<Vec<RecordedEvent>> {
        Ok(Vec::new())
    }

    async fn latest_position(&self) -> Result<LogCursor> {
        Ok(LogCursor::ZERO)
    }

    async fn append_to_stream(
        &self,
        _: &str,
        _: Uuid,
        _: StreamState,
        _: EventData,
    ) -> Result<WriteResult> {
        Err(anyhow::anyhow!("FailingBackend: configured to fail"))
    }
}
