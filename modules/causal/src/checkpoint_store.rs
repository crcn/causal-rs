//! Per-consumer cursor storage + reactor outbox traits.
//!
//! Two traits live here:
//!
//! - [`CheckpointStore`] — minimal cursor read/write. Used by
//!   Materializer and View runners that only need "where is consumer X?".
//!   Any `ProjectionStore` automatically satisfies this via blanket
//!   impl, so existing backends keep working unchanged.
//!
//! - [`ReactorOutbox`] — extends `CheckpointStore` with the atomic
//!   commit primitive that codifies C12 (runtime-side outbox + cursor
//!   advance in one transaction). Backends opt in by implementing this
//!   trait; only the Phase 4c `ReactorRunner` requires it.
//!
//! Splitting the surface this way keeps `CheckpointStore` minimal for
//! consumers that don't host reactors (read-only view backends, ops
//! tooling, etc.) while preserving the C12 atomicity guarantee for
//! reactor execution.

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::projection::ProjectionStore;
use crate::types::LogCursor;

/// Minimal cursor read/write surface.
#[async_trait]
pub trait CheckpointStore: Send + Sync {
    async fn get(&self, consumer_id: &str) -> Result<Option<LogCursor>>;
    async fn set(&self, consumer_id: &str, pos: LogCursor) -> Result<()>;
}

#[async_trait]
impl<T: ProjectionStore + ?Sized> CheckpointStore for T {
    async fn get(&self, consumer_id: &str) -> Result<Option<LogCursor>> {
        ProjectionStore::get_projection_cursor(self, consumer_id).await
    }

    /// Upsert via init + reset. `init_projection_cursor` creates the
    /// row if missing (no-op if present); `reset_projection` then
    /// unconditionally overwrites to `pos`. Together this provides the
    /// simple CheckpointStore::set semantic against a backend that
    /// distinguishes "create" from "overwrite" at the ProjectionStore
    /// layer. The Phase 4 runtime uses `advance_projection_cursor`
    /// directly when CAS semantics matter; this trait is for the
    /// simpler get/set use case.
    async fn set(&self, consumer_id: &str, pos: LogCursor) -> Result<()> {
        ProjectionStore::init_projection_cursor(self, consumer_id, pos).await?;
        ProjectionStore::reset_projection(self, consumer_id, pos).await
    }
}

// ─────────────────────────────────────────────────────────────────────
// ReactorOutbox — atomic outbox + cursor advance (C12)
// ─────────────────────────────────────────────────────────────────────

/// Caller-supplied outbox row for `commit_reactor_batch`. Same fields
/// as [`OutboxRow`] minus `id` and `created_at` — those are
/// backend-assigned (BIGSERIAL / atomic counter for `id`; `now()` /
/// `Utc::now()` for `created_at`). Splitting input from read-shape
/// avoids the awkward "pass placeholder values that get overwritten"
/// pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InsertableOutboxRow {
    pub reactor_id:      String,
    pub source_event_id: Uuid,
    pub output_index:    u32,
    pub event_id:        Uuid,
    pub event_type:      String,
    pub fact_payload:    serde_json::Value,
    pub correlation_id:  Uuid,
}

/// Persisted outbox row returned by `outbox_pending`. Includes the
/// backend-assigned `id` and `created_at`.
///
/// `event_id` is derived deterministically by the runtime from
/// `(reactor_id, source_event_id, output_index)` so retried reactor
/// runs collapse into a single log entry via the log's idempotent-
/// append-on-event-id contract (C1).
///
/// `event_type` and `correlation_id` are carried so the relay can
/// reconstruct a `NewEvent` for `EventLogBackend::append` without
/// re-deserializing the payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxRow {
    pub id:              i64,
    pub reactor_id:      String,
    pub source_event_id: Uuid,
    pub output_index:    u32,
    pub event_id:        Uuid,
    pub event_type:      String,
    pub fact_payload:    serde_json::Value,
    pub correlation_id:  Uuid,
    pub created_at:      DateTime<Utc>,
}

/// Reactor outbox storage. Extends `CheckpointStore` with the atomic
/// commit primitive that backs C12.
#[async_trait]
pub trait ReactorOutbox: CheckpointStore {
    /// Atomically: insert N outbox rows AND optionally advance a
    /// cursor. Backends MUST commit both in one transaction; partial
    /// observable state is a contract violation.
    ///
    /// `cursor` is `Some((consumer_id, pos))` for the typical reactor
    /// step (commit a batch of outputs and advance the reactor's
    /// cursor); `None` for backfill scenarios.
    ///
    /// Backend assigns `id` and `created_at` — caller's
    /// [`InsertableOutboxRow`] doesn't carry them.
    async fn commit_reactor_batch(
        &self,
        rows: Vec<InsertableOutboxRow>,
        cursor: Option<(String, LogCursor)>,
    ) -> Result<()>;

    /// Drain candidates for the relay loop. Order: oldest first by
    /// `created_at`, ties broken by `id`. Caller MAY call repeatedly;
    /// the same row may appear in subsequent calls until
    /// `outbox_delete` removes it.
    async fn outbox_pending(&self, limit: usize) -> Result<Vec<OutboxRow>>;

    /// Mark a row delivered. Idempotent: deleting an already-deleted
    /// id MUST succeed (Ok), as the relay may retry after a partial
    /// crash.
    async fn outbox_delete(&self, id: i64) -> Result<()>;
}
