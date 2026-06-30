//! Per-consumer cursor storage + reactor retry-tracking traits.
//!
//! - [`CheckpointStore`] — minimal cursor read/write. Required by
//!   `Projector` / `MultiProjector` / `Reactor` runners.
//! - [`ReactorCheckpoint`] — extends `CheckpointStore` with the failure store
//!   attempt-counter surface `ReactorRunner` uses to track retries
//!   across step boundaries.
//!
//! Reactor outputs append **directly** to the log (no outbox); these
//! traits carry only the per-consumer cursor and the failure store-attempt counters.

use anyhow::Result;
use async_trait::async_trait;
use uuid::Uuid;

use crate::types::LogCursor;

/// Minimal cursor read/write surface.
#[async_trait]
pub trait CheckpointStore: Send + Sync {
    async fn get(&self, consumer_id: &str) -> Result<Option<LogCursor>>;

    /// **Absolute** write — installs `pos` verbatim, even backwards. The
    /// authoritative setter used by lifecycle wiring that legitimately moves a
    /// cursor *down*: build-time seeding (e.g. `StartPosition::Zero` resets to
    /// 0; `Specific` to an arbitrary point) and the downward
    /// [`clamp_ahead_of`](Self::clamp_ahead_of) heal. **Not** for the
    /// per-event hot path — use [`advance`](Self::advance) there, which is
    /// monotonic.
    async fn set(&self, consumer_id: &str, pos: LogCursor) -> Result<()>;

    /// **Monotonic** advance — moves a cursor forward only; a `pos` at or
    /// behind the stored value is a no-op. This is the per-event hot-path
    /// writer (projector/multi-projector cursor, reactor ack-floor, PG mirror
    /// tailer).
    ///
    /// Why monotonic and not [`set`](Self::set): a consumer's single-writer
    /// guarantee rests on the (opt-in) `ConsumerLeasor`. Without a leasor, a
    /// two-node deployment can run two live workers for one consumer; an
    /// absolute `set` lets a *lagging* worker overwrite a more-advanced cursor
    /// **backwards**, replaying already-processed events (and, for a
    /// nondeterministic reactor, triggering a divergence storm). A monotonic
    /// advance makes the lagging write a no-op, so checkpoint correctness no
    /// longer depends on remembering the lease.
    ///
    /// The default is a non-atomic get→compare→set, correct for a single
    /// writer; **concurrent-safe backends MUST override** with an atomic
    /// maximum (e.g. SQL `GREATEST`) so two racing advances can't interleave
    /// into a regression.
    async fn advance(&self, consumer_id: &str, pos: LogCursor) -> Result<()> {
        let current = self.get(consumer_id).await?;
        if current.map_or(true, |c| pos > c) {
            self.set(consumer_id, pos).await?;
        }
        Ok(())
    }

    /// Clamp every stored checkpoint whose position is strictly **ahead of**
    /// `tip` down to `tip`, returning how many were clamped.
    ///
    /// A crash-recovery primitive for the case where a consumer's durable
    /// cursor ran *past* the event log's tip — e.g. the event store was
    /// restored to an earlier point, so positions beyond `tip` no longer
    /// exist. The log is append-only, so events at or below `tip` are
    /// byte-identical to what the consumer already processed; only events
    /// beyond it are gone. **Clamping to `tip`** lets each affected consumer
    /// resume exactly there and process only genuinely-new events.
    ///
    /// Prefer this over resetting such cursors to zero: a reset forces a full
    /// replay that re-runs every reactor over all history, and any
    /// nondeterministic reactor then re-emits divergently on every historical
    /// output — a divergence storm. Clamping re-processes nothing.
    ///
    /// The default is a **no-op** returning `0`; durable backends override it.
    /// (A generic default can't enumerate consumers through this trait, and a
    /// silent no-op is the safe degradation for stores that don't support it.)
    async fn clamp_ahead_of(&self, tip: LogCursor) -> Result<u64> {
        let _ = tip;
        Ok(0)
    }
}

// ─────────────────────────────────────────────────────────────────────
// ReactorCheckpoint — checkpoint + terminal-failure retry-attempt tracking
// ─────────────────────────────────────────────────────────────────────

/// Extends [`CheckpointStore`] with reactor retry-attempt tracking for
/// the terminal-failure path. Required only for engines hosting reactors.
#[async_trait]
pub trait ReactorCheckpoint: CheckpointStore {
    /// Increment the attempt counter for a `(consumer_id,
    /// trigger_id)` pair and return the new count. Called by
    /// `ReactorRunner` on every `react()` failure to track retries
    /// for the terminal-failure path. The returned value is the count INCLUDING this
    /// attempt (first failure returns 1).
    ///
    /// This is an in-process retry budget, not durable state. Both
    /// current backends (MemoryStore, Postgres) hold it in memory; a
    /// process restart resets it, which is fine — a restart re-runs the
    /// uncommitted reactor work from its cursor and re-counts toward the
    /// cap before parking. A backend MAY persist it (a future schema
    /// migration could) but is not required to.
    async fn record_reactor_attempt(
        &self,
        consumer_id: &str,
        trigger_id: Uuid,
    ) -> Result<u32>;

    /// Clear the attempt counter for a `(consumer_id,
    /// trigger_id)` pair. Called on successful `react()` (the
    /// next failure should start fresh) and after the terminal-failure mapper
    /// has fired. Idempotent.
    async fn clear_reactor_attempts(
        &self,
        consumer_id: &str,
        trigger_id: Uuid,
    ) -> Result<()>;
}
