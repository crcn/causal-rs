//! `MirroringEventLogBackend` — wraps two backends and writes to both.
//!
//! Dual-write enabler for the v0.3 transition. During Phase 4f, the
//! legacy `events` store remains the source of truth; v0.3 `causal_log`
//! accumulates a parallel record. Migrated consumers read from
//! `causal_log`; un-migrated ones still read from the legacy store.
//!
//! ## Atomicity model
//!
//! `append` is **sequential, idempotent at-least-once** — NOT 2PC over a
//! shared transaction. The two child writes happen one after the other.
//! If the first succeeds and the second fails:
//!
//!   - The legacy store has the row; the v0.3 mirror does not.
//!   - On retry of the same `event_id`, the legacy store's idempotent
//!     `append` is a no-op (returns the existing position) and the v0.3
//!     mirror gets its missing write.
//!
//! This is the standard dual-write pattern: at-least-once delivery to
//! idempotent sinks. It DOES NOT prevent observable inconsistency
//! during the failure window — a reader of the v0.3 mirror briefly
//! sees N events while the legacy store has N+1. Acceptable in 4f
//! because:
//!
//!   1. The legacy store remains the source of truth — readers that
//!      need consistency read from it.
//!   2. The mirror catches up on retry, bounded by the producer's
//!      retry policy.
//!   3. Parity validation (4f.4) runs continuously and surfaces drift.
//!
//! Strict tx-shared atomicity would require either a trait extension
//! for `append_in_tx` (option a in the 4f plan) or bypassing the trait
//! (option b). Both are feasible; both add cost. The plan defers them
//! to 4f.2 if the parity audit reveals real divergence under load.
//!
//! ## Read paths
//!
//! `read_all`, `read_stream`, `latest_position` delegate to the legacy
//! child. The legacy store is the source of truth during transition;
//! the v0.3 mirror is a write-only secondary.

use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;
use uuid::Uuid;

use causal::types::{
    WriteResult, LogCursor, EventData, RecordedEvent, StreamRevision,
};
use causal::EventLogBackend;

pub struct MirroringEventLogBackend {
    legacy: Arc<dyn EventLogBackend>,
    v03: Arc<dyn EventLogBackend>,
}

impl MirroringEventLogBackend {
    /// Construct a wrapper that writes every emit to both `legacy` and
    /// `v03`. `legacy` is the source of truth — its `WriteResult` is
    /// what callers see. `v03` is the mirror.
    pub fn new(
        legacy: Arc<dyn EventLogBackend>,
        v03: Arc<dyn EventLogBackend>,
    ) -> Self {
        Self { legacy, v03 }
    }

    /// Diagnostic: did the most recent write to `legacy` make it to
    /// `v03` too? This is a logical check — looks up the same
    /// event_id in the v0.3 mirror via `read_stream` heuristics or a
    /// full `read_all` scan. For continuous parity monitoring see
    /// 4f.4 in the migration plan.
    ///
    /// Returns `Ok(true)` if both stores contain the event, `Ok(false)`
    /// if only the legacy contains it, `Err(_)` on backend errors.
    pub async fn parity_check_event(&self, event_id: Uuid) -> Result<bool> {
        // Both backends scanned via read_all from position 0. Cheap
        // for tests, expensive for production traffic — the production
        // version of this check should use a backend-specific query
        // (`SELECT EXISTS(... WHERE event_id = $1)`). The trait doesn't
        // expose that; runners that need it can downcast to the
        // concrete child type.
        let legacy_events = self.legacy.read_all(LogCursor::ZERO, 100_000).await?;
        let v03_events = self.v03.read_all(LogCursor::ZERO, 100_000).await?;
        let in_legacy = legacy_events.iter().any(|e| e.event_id == event_id);
        let in_v03 = v03_events.iter().any(|e| e.event_id == event_id);
        Ok(in_legacy && in_v03)
    }
}

#[async_trait]
impl EventLogBackend for MirroringEventLogBackend {
    async fn append_to_stream(
        &self,
        category: &str,
        stream_id: Uuid,
        expected: causal::types::StreamState,
        events: Vec<EventData>,
    ) -> Result<WriteResult> {
        // OCC semantics flow from the legacy store — it's the source
        // of truth and the only one that can reject on stale expected.
        // The mirror writes whatever the legacy write produced.
        let legacy_result = self
            .legacy
            .append_to_stream(category, stream_id, expected, events.clone())
            .await?;
        // Mirror writes with `Any` (no independent OCC) — its only job is
        // to accumulate the same event_id stream. Letting it perform OCC
        // separately would risk diverging from legacy under concurrent
        // writers.
        let _ = self
            .v03
            .append_to_stream(category, stream_id, causal::types::StreamState::Any, events)
            .await?;
        Ok(legacy_result)
    }

    async fn read_all(
        &self,
        after: LogCursor,
        limit: usize,
    ) -> Result<Vec<RecordedEvent>> {
        // Source of truth = legacy during transition.
        self.legacy.read_all(after, limit).await
    }

    async fn read_stream(
        &self,
        category: &str,
        stream_id: Uuid,
        after: Option<StreamRevision>,
    ) -> Result<Vec<RecordedEvent>> {
        self.legacy
            .read_stream(category, stream_id, after)
            .await
    }

    async fn latest_position(&self) -> Result<LogCursor> {
        self.legacy.latest_position().await
    }
}
