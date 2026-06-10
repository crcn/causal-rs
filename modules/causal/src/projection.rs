//! Projection / consumer start-position configuration.
//!
//! This module once carried a larger operational surface
//! (`ProjectionMode`, `RetryPolicy`, `FailureBehavior`, `ProjectionOps`)
//! that was exported but consumed by nothing — removed pre-1.0 rather
//! than shipped as dead configuration (2026-06-10 audit remediation,
//! Phase 1). The Postgres schema's `causal_projection_cursors` /
//! `causal_projection_failures` tables remain reserved for a future,
//! wired re-introduction.

use crate::types::LogCursor;

/// Where a consumer should start when its runner first comes up.
///
/// Wired for **reactors** (side-effecting consumers): the runner seeds an
/// absent cursor according to this policy. Projections always start from
/// `LogCursor::ZERO` — read models want full history; side effects must
/// not replay it. The defaults differ on purpose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartPosition {
    /// Use the existing cursor if one is persisted for this group;
    /// otherwise start at the current `latest_position()`. The default
    /// for reactors: a freshly-deployed reactor must not re-fire side
    /// effects for history, and a restarted one must resume where it
    /// left off.
    ResumeOrLatest,
    /// Always start at the current `latest_position()`, **ignoring any
    /// persisted cursor** — the backlog is skipped on every restart.
    /// Almost never what a reactor wants; exists for ops escape hatches.
    Latest,
    /// Always start at `LogCursor::ZERO`. Forces the reactor to process
    /// the entire log from the beginning.
    ///
    /// # ⚠️ Replay hazard
    ///
    /// `Zero` (and `Specific` for any position behind a downstream
    /// consumer's view) re-delivers every event the consumer has seen
    /// before. For a reactor that means **re-firing side effects**
    /// (re-sent emails, re-charged cards) unless the bodies are
    /// idempotent. Treat as a footgun on any non-trivial deployment;
    /// the safe rebuild pattern is blue/green via
    /// `causal_replay::ProjectionStream` in replay mode.
    Zero,
    /// Specific position. Manual rewind for debugging or partial
    /// replay. **Same replay hazard as [`StartPosition::Zero`]** when
    /// the position is behind a downstream consumer's view.
    Specific(LogCursor),
}
