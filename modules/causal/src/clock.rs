//! Calendar-clock injection — one half of Primitive 6's two-clock rule.
//!
//! **Calendar time** (this trait) is what the framework *stamps*:
//! `created_at` on appended events, terminal-failure record times.
//! Injected so tests pin it and replay reproduces byte-identical
//! envelopes. Stated precisely: replay is byte-stable only when all
//! three hold — injected clock, effects memoized (`ctx.effect`),
//! canonical payload serialization. Any one alone is not the property.
//!
//! **Liveness time** is tokio time and never comes through here: every
//! framework wait and duration comparison (settle polls, worker
//! backoff, the transient ceiling, partition eviction) runs on
//! `tokio::time` primitives, which virtualize under
//! `#[tokio::test(start_paused = true)]`. Chrono never participates in
//! a liveness decision — a wall-clock wait would be testable only in
//! production.

use chrono::{DateTime, Utc};

/// Source of calendar timestamps the framework stamps onto envelopes.
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

/// The default: real wall-clock time.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// A pinned clock for tests: every framework-stamped timestamp becomes
/// the given instant, making appended envelopes reproducible.
#[derive(Debug, Clone, Copy)]
pub struct FixedClock(pub DateTime<Utc>);

impl Clock for FixedClock {
    fn now(&self) -> DateTime<Utc> {
        self.0
    }
}
