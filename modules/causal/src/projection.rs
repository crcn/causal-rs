//! Projection configuration types and persistence trait.
//!
//! This module defines the public types needed to configure a projection
//! and the persistence trait backends implement to support per-projection
//! cursors. The runtime (`ProjectionRunner`, engine integration, sync-vs-
//! async dispatch) is not yet implemented — see
//! `docs/plans/2026-05-04-feat-async-projections-plan.md` for the design
//! these types belong to.
//!
//! The types are `pub` and stable as part of the 0.3 API surface; backend
//! implementers (e.g. a Postgres `ProjectionStore`) can develop against
//! them in parallel with the engine-side runtime work. The trait has
//! default `unimplemented!()` impls to keep early consumers from
//! accidentally depending on un-finished behavior.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::types::LogCursor;

// ─────────────────────────────────────────────────────────────────────────
// Configuration types (D1, D4, D5 from the plan)
// ─────────────────────────────────────────────────────────────────────────

/// Whether a projection runs inline with dispatch (`Sync`) or in an
/// independent runner with its own cursor (`Async`).
///
/// **D1 (no default).** Every `register_projection` call site declares
/// its mode explicitly. Both extremes — defaulting Sync (preserves the
/// pre-0.2.0 silent-coupling bug for new projections) and defaulting
/// Async (silently breaks read-your-writes consumers) — are wrong; the
/// API refuses to choose so the caller has to.
///
/// See `register_sync_projection` / `register_async_projection` on
/// `Engine` for ergonomic shorthand once the runtime lands.
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
#[derive(Debug, Clone, Copy)]
pub enum Backoff {
    /// No delay between attempts.
    None,
    /// Constant delay between attempts.
    Linear { base: Duration },
    /// `min(max, base * 2^attempt)` with optional jitter. The default
    /// for async projections.
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
/// **D4.** Default `BlockUntilFixed`. Reasoning: preserves the
/// event-sourcing invariant (projection state == fold(log)). Operator
/// pauses, fixes, resumes for permanent failures. The escape hatch
/// (`AdvanceAfter`) is real — some consumers prefer skip-and-continue
/// over stuck-and-block — but it's a deliberate opt-in, not a default.
///
/// Switching `BlockUntilFixed → AdvanceAfter` is reversible at runtime;
/// switching the other direction doesn't recover lost events. Default
/// to the recoverable choice.
#[derive(Debug, Clone, Copy)]
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
#[derive(Debug, Clone, Default)]
pub struct RetryPolicy {
    pub backoff: Backoff,
    pub failure: FailureBehavior,
}

/// Where an async projection should start when its runner first comes
/// up.
///
/// **D5 (no default).** Refusing to default forces a per-site decision
/// the same way mode does. The "obvious" defaults are both wrong in some
/// case: `ResumeOrLatest` silently makes historical events invisible to
/// a freshly-added projection; `Zero` is catastrophically expensive for
/// redeploys of long-running projections.
#[derive(Debug, Clone, Copy)]
pub enum StartPosition {
    /// Use the existing cursor if one is persisted for this
    /// `projection_id`; otherwise start at the current
    /// `latest_position()`. The pragmatic production choice for
    /// projections that already have history.
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
// Persistence trait (cursor + per-projection ops)
// ─────────────────────────────────────────────────────────────────────────

/// Status snapshot of an async projection.
///
/// Returned by `ProjectionStore::projection_status` and
/// `Engine::projection_status`. Used by the inspector UI and operational
/// tooling.
#[derive(Debug, Clone)]
pub struct ProjectionStatus {
    pub projection_id: String,
    pub cursor: LogCursor,
    pub paused: bool,
    pub last_error: Option<String>,
    pub last_attempt_at: Option<DateTime<Utc>>,
    pub consecutive_failures: u32,
}

/// Persistence backend for async projection cursors.
///
/// Implemented by storage backends (Postgres, KurrentDB, in-memory).
/// The engine's `ProjectionRunner` calls these methods to claim the
/// next batch of events, advance the cursor on success, and record
/// failures on persistent error.
///
/// **Forward-compatibility:** the underlying schema (in Postgres
/// backends, a `causal_projection_cursors` table) reserves
/// `leased_by`, `leased_until`, and `fencing_token` columns for the
/// future multi-process leader election story (D3). They are unused
/// by 0.3 — per-batch `SELECT ... FOR UPDATE SKIP LOCKED` is the only
/// concurrency primitive at this stage. Adding lease semantics later
/// is a column-population change, not a schema redesign.
///
/// Default method bodies are `unimplemented!()` rather than `Ok(())`
/// or sensible defaults so that early backend skeletons fail loudly
/// rather than silently no-op.
#[async_trait]
pub trait ProjectionStore: Send + Sync {
    /// Initialize a cursor for a projection if one does not exist.
    ///
    /// `start` is the resolved starting position (after the engine has
    /// resolved `StartPosition` against the current log state). Idempotent:
    /// if the projection_id already has a cursor, this method is a no-op
    /// and the existing cursor is preserved.
    async fn init_projection_cursor(
        &self,
        _projection_id: &str,
        _start: LogCursor,
    ) -> Result<()> {
        unimplemented!("ProjectionStore::init_projection_cursor")
    }

    /// Read the current cursor for a projection.
    ///
    /// Returns `None` if `init_projection_cursor` was never called for
    /// this `projection_id`.
    async fn get_projection_cursor(
        &self,
        _projection_id: &str,
    ) -> Result<Option<LogCursor>> {
        unimplemented!("ProjectionStore::get_projection_cursor")
    }

    /// Atomically advance a projection's cursor.
    ///
    /// `to` is the new cursor position (post-batch). Backends should
    /// claim the row via `SELECT ... FOR UPDATE SKIP LOCKED` so two
    /// engines never advance the same cursor concurrently.
    async fn advance_projection_cursor(
        &self,
        _projection_id: &str,
        _to: LogCursor,
    ) -> Result<()> {
        unimplemented!("ProjectionStore::advance_projection_cursor")
    }

    /// Record a per-projection failure event for `AdvanceAfter` mode.
    ///
    /// Writes a per-projection DLQ row capturing the event_id, error
    /// message, and attempt count. Called only when
    /// `FailureBehavior::AdvanceAfter` decides to skip the failing
    /// event after exhausting `max_attempts`.
    async fn record_projection_failure(
        &self,
        _projection_id: &str,
        _event_id: Uuid,
        _error: &str,
        _attempts: u32,
    ) -> Result<()> {
        unimplemented!("ProjectionStore::record_projection_failure")
    }

    /// Update the `last_error` / `last_attempt_at` /
    /// `consecutive_failures` columns for `BlockUntilFixed` mode.
    ///
    /// Unlike `record_projection_failure`, this does NOT advance the
    /// cursor — it just surfaces the current failure state via
    /// `projection_status` so operators can see why the projection is
    /// stuck.
    async fn record_projection_attempt_error(
        &self,
        _projection_id: &str,
        _error: &str,
        _consecutive_failures: u32,
    ) -> Result<()> {
        unimplemented!("ProjectionStore::record_projection_attempt_error")
    }

    /// Clear the failure-state columns after a successful apply.
    async fn clear_projection_attempt_error(
        &self,
        _projection_id: &str,
    ) -> Result<()> {
        unimplemented!("ProjectionStore::clear_projection_attempt_error")
    }

    /// Return the operational status of a projection.
    async fn projection_status(
        &self,
        _projection_id: &str,
    ) -> Result<Option<ProjectionStatus>> {
        unimplemented!("ProjectionStore::projection_status")
    }

    /// Pause the projection runner. Subsequent
    /// `advance_projection_cursor` calls from the runner are skipped
    /// until `resume_projection` is called. Backends should write a
    /// `paused = true` flag to the cursor row.
    async fn pause_projection(
        &self,
        _projection_id: &str,
    ) -> Result<()> {
        unimplemented!("ProjectionStore::pause_projection")
    }

    /// Resume a paused projection.
    async fn resume_projection(
        &self,
        _projection_id: &str,
    ) -> Result<()> {
        unimplemented!("ProjectionStore::resume_projection")
    }

    /// Reset a projection's cursor to a specific position. Forces
    /// backfill or rewind from the new position. Operator-initiated
    /// only — runners do not call this.
    async fn reset_projection(
        &self,
        _projection_id: &str,
        _to: LogCursor,
    ) -> Result<()> {
        unimplemented!("ProjectionStore::reset_projection")
    }
}
