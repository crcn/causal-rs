//! Projection configuration types and persistence trait.
//!
//! This module defines the public API for configuring projections (mode,
//! retry policy, start position) and the persistence trait
//! (`ProjectionStore`) that backends implement to support per-projection
//! cursors. The runtime (`ProjectionRunner`, engine integration,
//! sync-vs-async dispatch) is implemented separately and consumes this
//! trait — see `docs/plans/2026-05-04-feat-async-projections-plan.md`.
//!
//! Backends implement `ProjectionStore` against their own storage. An
//! in-memory implementation is provided on `MemoryStore` for tests and
//! single-process use cases.
//!
//! ## Recommended schema (Postgres backends)
//!
//! ```sql
//! CREATE TABLE causal_projection_cursors (
//!     projection_id        TEXT PRIMARY KEY,
//!     cursor_position      BIGINT NOT NULL,
//!     paused               BOOL NOT NULL DEFAULT FALSE,
//!     last_error           TEXT,
//!     last_attempt_at      TIMESTAMPTZ,
//!     consecutive_failures INT NOT NULL DEFAULT 0,
//!     -- Forward-compat columns for future multi-process leases (D3
//!     -- in the plan). Unused in 0.3; reserved so adding leases later
//!     -- is a column-population change, not a schema migration.
//!     leased_by            TEXT,
//!     leased_until         TIMESTAMPTZ,
//!     fencing_token        BIGINT
//! );
//!
//! CREATE TABLE causal_projection_failures (
//!     projection_id  TEXT NOT NULL,
//!     event_id       UUID NOT NULL,
//!     error          TEXT NOT NULL,
//!     attempts       INT NOT NULL,
//!     failed_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
//!     PRIMARY KEY (projection_id, event_id)
//! );
//! ```
//!
//! The DLQ table's primary key on `(projection_id, event_id)` is
//! load-bearing — it makes `advance_past_failure` idempotent on retry.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::types::LogCursor;

// ─────────────────────────────────────────────────────────────────────────
// Configuration types (D1, D4, D5)
// ─────────────────────────────────────────────────────────────────────────

/// Whether a projection runs inline with dispatch (`Sync`) or in an
/// independent runner with its own cursor (`Async`).
///
/// **D1: no default.** Every `register_projection` call site declares
/// its mode explicitly. Both extremes — defaulting `Sync` (preserves
/// the pre-0.2.0 silent-coupling bug for new projections) and
/// defaulting `Async` (silently breaks read-your-writes consumers) —
/// are wrong; the API refuses to choose so the caller has to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectionMode {
    /// Runs inline in the dispatch loop, before reactors. Failure
    /// blocks the dispatch cursor; retry budget then park applies.
    /// Read-your-writes within the same causal chain.
    Sync,
    /// Runs in an independent `ProjectionRunner` with its own cursor.
    /// Failure does not block dispatch. Lag is observable; recovery
    /// is per-projection.
    Async,
}

/// Backoff policy between retry attempts on projection failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backoff {
    /// No delay between attempts.
    None,
    /// Constant delay between attempts.
    Linear { base: Duration },
    /// `min(max, base * 2^attempt)` with optional jitter. Default for
    /// async projections.
    Exponential {
        base: Duration,
        max: Duration,
        jitter: bool,
    },
}

impl Default for Backoff {
    fn default() -> Self {
        Backoff::Exponential {
            base: Duration::from_millis(100),
            max: Duration::from_secs(30),
            jitter: true,
        }
    }
}

/// What happens when a projection keeps failing.
///
/// **D4: default `BlockUntilFixed`.** Preserves the event-sourcing
/// invariant (projection state == fold(log)). Switching
/// `BlockUntilFixed → AdvanceAfter` is reversible at runtime; switching
/// the other direction doesn't recover lost events. Default to the
/// recoverable choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureBehavior {
    /// Retry forever with backoff. Cursor does not advance past the
    /// failing event until the projection succeeds or an operator
    /// intervenes. Lag grows visibly while stuck.
    BlockUntilFixed,
    /// Retry `max_attempts` times, then advance the cursor past the
    /// failed event and record a per-projection DLQ row. Operator
    /// manually replays via `reset_projection(id, position)` after
    /// fixing the issue.
    AdvanceAfter { max_attempts: u32 },
}

impl Default for FailureBehavior {
    fn default() -> Self {
        FailureBehavior::BlockUntilFixed
    }
}

/// Combined retry policy for an async projection.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RetryPolicy {
    pub backoff: Backoff,
    pub failure: FailureBehavior,
}

/// Where an async projection should start when its runner first comes
/// up.
///
/// **D5: no default.** Refusing to default forces a per-site decision
/// the same way mode does. The "obvious" defaults are both wrong in
/// some case: `ResumeOrLatest` silently makes historical events
/// invisible to a freshly-added projection; `Zero` is catastrophically
/// expensive for redeploys of long-running projections.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartPosition {
    /// Use the existing cursor if one is persisted for this
    /// `projection_id`; otherwise start at the current
    /// `latest_position()`. Pragmatic production choice for projections
    /// that already have history.
    ResumeOrLatest,
    /// Always start at the current `latest_position()`, ignoring any
    /// persisted cursor. New projection that should NOT backfill.
    Latest,
    /// Always start at `LogCursor::ZERO`. Force backfill from the
    /// beginning of the event log. Search-index rebuild, analytics
    /// reset, etc.
    Zero,
    /// Specific position. Manual rewind for debugging or partial
    /// replay.
    Specific(LogCursor),
}

// ─────────────────────────────────────────────────────────────────────────
// Status + DLQ row types
// ─────────────────────────────────────────────────────────────────────────

/// Status snapshot of a projection.
///
/// Returned by `ProjectionStore::projection_status` and
/// `Engine::projection_status`. Used by the inspector UI and
/// operational tooling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionStatus {
    pub projection_id: String,
    pub cursor: LogCursor,
    pub paused: bool,
    pub last_error: Option<String>,
    pub last_attempt_at: Option<DateTime<Utc>>,
    pub consecutive_failures: u32,
}

/// One DLQ row recorded by `advance_past_failure` when an async
/// projection in `FailureBehavior::AdvanceAfter` mode skips an event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionFailure {
    pub projection_id: String,
    pub event_id: Uuid,
    pub error: String,
    pub attempts: u32,
    pub failed_at: DateTime<Utc>,
}

// ─────────────────────────────────────────────────────────────────────────
// Persistence trait
// ─────────────────────────────────────────────────────────────────────────

/// Persistence backend for async projection cursors and DLQ.
///
/// The engine's `ProjectionRunner` calls these methods to claim the
/// next batch of events, advance the cursor on success, or skip the
/// failing event on terminal failure.
///
/// ## Concurrency contract
///
/// The cursor-advancing methods (`advance_projection_cursor`,
/// `advance_past_failure`) take an `expected_from: LogCursor`
/// parameter and return `Result<bool>` — `true` if the update happened,
/// `false` if `expected_from` did not match the persisted cursor (i.e.,
/// another runner advanced it). Backends implement this as
/// optimistic-concurrency CAS: `UPDATE ... WHERE cursor_position =
/// $expected_from RETURNING cursor_position`. Postgres backends should
/// wrap this in a `SELECT ... FOR UPDATE SKIP LOCKED` claim so the
/// `false` return distinguishes "row was locked by another engine"
/// from "row had a different cursor."
///
/// ## Atomicity contract
///
/// `advance_past_failure` writes a DLQ row and advances the cursor
/// **atomically** (single transaction in backends with transactions).
/// A runner crash between the two writes is impossible by construction
/// — either both happened or neither did.
///
/// **Ordering is part of the contract:** the CAS check on
/// `expected_from` happens *before* any DLQ write. If the CAS check
/// fails, no DLQ row is written. Backends MUST NOT write the DLQ row
/// first — doing so would leave a permanent failure record for an
/// event whose cursor never advanced, which the runner will retry and
/// possibly succeed at, producing a misleading DLQ entry. Reference
/// implementation order:
///
/// ```sql
/// BEGIN;
///   SELECT cursor_position FROM causal_projection_cursors
///     WHERE projection_id = $1 FOR UPDATE SKIP LOCKED;
///   -- if cursor_position != $expected_from, ROLLBACK and return false
///   INSERT INTO causal_projection_failures (...)
///     ON CONFLICT (projection_id, event_id) DO NOTHING;
///   UPDATE causal_projection_cursors SET cursor_position = $to ...;
/// COMMIT;
/// ```
///
/// `record_projection_failure` is NOT a separate method; the failure
/// write is bundled into `advance_past_failure` precisely to enforce
/// this atomicity and ordering.
///
/// ## Idempotency contract
///
/// The DLQ primary key `(projection_id, event_id)` ensures
/// `advance_past_failure` is idempotent on retry-of-retry: if the
/// cursor advance succeeded but the runner died before the next read,
/// a redundant call writes the same DLQ row (`ON CONFLICT DO NOTHING`)
/// and the second cursor advance fails the CAS check (returning
/// `false`). No duplicate DLQ rows.
///
/// ## Forward-compat
///
/// The recommended schema reserves `leased_by` / `leased_until` /
/// `fencing_token` columns for future multi-process leader election
/// (deferred per D3). 0.3 does not use them; adding lease semantics
/// later is a column-population change, not a schema redesign.
///
/// ## No defaults
///
/// All methods are required (no default bodies). Incomplete backend
/// impls fail at compile time.
#[async_trait]
pub trait ProjectionStore: Send + Sync {
    /// Initialize a cursor for a projection if one does not exist.
    ///
    /// Returns `true` if a new cursor row was created, `false` if a
    /// cursor already existed (in which case the existing cursor is
    /// preserved unchanged). Used by `ProjectionMode::Async`
    /// registration with `StartPosition::ResumeOrLatest`.
    async fn init_projection_cursor(
        &self,
        projection_id: &str,
        start: LogCursor,
    ) -> Result<bool>;

    /// Read the current cursor for a projection. Returns `None` if
    /// `init_projection_cursor` was never called for this id (or the
    /// projection was deleted).
    async fn get_projection_cursor(
        &self,
        projection_id: &str,
    ) -> Result<Option<LogCursor>>;

    /// CAS-advance a projection's cursor on successful batch apply.
    ///
    /// Returns `true` if the cursor was updated, `false` if
    /// `expected_from` did not match the persisted value (i.e.,
    /// another runner advanced it concurrently, or this runner's read
    /// is stale). Caller should re-read and retry on `false`.
    ///
    /// Also clears `last_error` and `consecutive_failures` on success.
    async fn advance_projection_cursor(
        &self,
        projection_id: &str,
        expected_from: LogCursor,
        to: LogCursor,
    ) -> Result<bool>;

    /// CAS-advance a projection's cursor past a failing event,
    /// recording a DLQ row in the same transaction.
    ///
    /// Used only by `FailureBehavior::AdvanceAfter` after the retry
    /// budget exhausts. The DLQ write and cursor advance are atomic;
    /// the DLQ row is idempotent on `(projection_id, event_id)`.
    ///
    /// Returns `true` if both writes happened, `false` if
    /// `expected_from` did not match.
    ///
    /// Also clears `last_error` and `consecutive_failures` on
    /// successful skip.
    async fn advance_past_failure(
        &self,
        projection_id: &str,
        expected_from: LogCursor,
        to: LogCursor,
        event_id: Uuid,
        error: &str,
        attempts: u32,
    ) -> Result<bool>;

    /// Update the live failure-state fields without advancing the
    /// cursor. Called by the runner after each failed attempt
    /// (regardless of `FailureBehavior` mode) to surface live status.
    ///
    /// Pass `Some(error)` after a failure; pass `None` after a
    /// successful apply to clear. Note that
    /// `advance_projection_cursor` and `advance_past_failure` already
    /// clear the error state on success — this method exists for the
    /// in-flight case where the runner wants to update status without
    /// committing a cursor advance.
    async fn set_projection_error(
        &self,
        projection_id: &str,
        error: Option<&str>,
        consecutive_failures: u32,
    ) -> Result<()>;

    /// Return the operational status of a projection. Returns `None`
    /// if no cursor exists for this id.
    async fn projection_status(
        &self,
        projection_id: &str,
    ) -> Result<Option<ProjectionStatus>>;

    /// Return statuses for all known projections. Used by ops
    /// dashboards. Order is unspecified; callers sort if they need
    /// stable ordering.
    async fn list_projections(&self) -> Result<Vec<ProjectionStatus>>;

    /// Set the paused flag for a projection. Runners check this
    /// before each batch and skip work while `paused = true`.
    /// User-facing pause/resume is on `Engine`; this is the storage
    /// primitive.
    async fn set_projection_paused(
        &self,
        projection_id: &str,
        paused: bool,
    ) -> Result<()>;

    /// Reset a projection's cursor to a specific position
    /// unconditionally. Forces backfill or rewind. Operator-initiated
    /// only — the runner does not call this.
    ///
    /// Distinct from `advance_projection_cursor` because resets bypass
    /// the optimistic-concurrency check (it's an authoritative
    /// override). Backends should NOT use SKIP LOCKED here — a reset
    /// blocks until it acquires the row lock.
    ///
    /// Also clears `last_error` and `consecutive_failures`.
    async fn reset_projection(
        &self,
        projection_id: &str,
        to: LogCursor,
    ) -> Result<()>;

    /// Delete a projection's cursor row and all its DLQ rows.
    /// Operator cleanup when a projection is removed from the
    /// registration list. Idempotent: succeeds even if the
    /// projection_id doesn't exist.
    async fn delete_projection(
        &self,
        projection_id: &str,
    ) -> Result<()>;

    /// List DLQ rows for a projection, most-recent first, limited to
    /// `limit` rows.
    async fn list_projection_failures(
        &self,
        projection_id: &str,
        limit: usize,
    ) -> Result<Vec<ProjectionFailure>>;

    /// Delete one DLQ row. Used by operator workflows that fix the
    /// underlying bug then remove the failure record before replaying
    /// (typically followed by `reset_projection` to rewind the cursor
    /// past the deletion).
    ///
    /// Returns `true` if a row was deleted, `false` if no row matched.
    async fn delete_projection_failure(
        &self,
        projection_id: &str,
        event_id: Uuid,
    ) -> Result<bool>;
}

// ─────────────────────────────────────────────────────────────────────────
// v0.4 surface — minimal ops trait on top of CheckpointStore
// ─────────────────────────────────────────────────────────────────────────

/// Optional operational extension to [`CheckpointStore`]. Carries the
/// DLQ + pause/resume + status surface needed by ops tooling and the
/// inspector UI. Backends that don't need these — pure read-only
/// projections, throwaway tests — can satisfy the runtime with just
/// `CheckpointStore`.
///
/// Compared to the legacy [`ProjectionStore`]:
/// - Cursor get/set lives on `CheckpointStore` (required base trait).
/// - CAS variants (`advance_projection_cursor`, `advance_past_failure`)
///   are gone — the v0.4 runners use `CheckpointStore::set` and call
///   `record_failure` separately when DLQ-skipping is wanted.
/// - The trait surface drops from ~12 methods to 5.
///
/// During the v0.3 → v0.4 transition this trait is available alongside
/// `ProjectionStore`; P11 (Legacy collapse) deletes `ProjectionStore`
/// and migrates Postgres backends to `ProjectionOps` directly.
#[async_trait]
pub trait ProjectionOps: crate::checkpoint_store::CheckpointStore {
    /// Set the paused flag for a projection. Runners check this
    /// before each batch and skip work while `paused = true`.
    async fn set_paused(&self, group_name: &str, paused: bool) -> Result<()>;

    /// Record a DLQ row for a failed event. Does NOT advance the
    /// cursor — that's `CheckpointStore::set`'s job. Caller decides
    /// the order (skip-after vs block-on). Idempotent on
    /// `(group_name, event_id)` per the underlying primary-key
    /// contract.
    async fn record_failure(
        &self,
        group_name: &str,
        event_id: Uuid,
        error: &str,
        attempts: u32,
    ) -> Result<()>;

    /// List DLQ rows for a projection, most-recent first, limited to
    /// `limit` rows.
    async fn list_failures(
        &self,
        group_name: &str,
        limit: usize,
    ) -> Result<Vec<ProjectionFailure>>;

    /// Return the operational status of a projection. Returns `None`
    /// if no cursor exists for this id.
    async fn status(&self, group_name: &str) -> Result<Option<ProjectionStatus>>;

    /// Delete one DLQ row. Used by operator workflows that fix the
    /// underlying bug then remove the failure record. Returns `true`
    /// if a row was deleted, `false` if no row matched.
    async fn delete_failure(
        &self,
        group_name: &str,
        event_id: Uuid,
    ) -> Result<bool>;
}
