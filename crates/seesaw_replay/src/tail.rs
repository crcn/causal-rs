//! Tail source trait for pluggable event tailing.

use anyhow::Result;
use async_trait::async_trait;
use std::time::Duration;

/// A source of wake-up signals for tailing new events.
///
/// Implementations provide a `wait()` method that blocks until new events
/// may be available. The caller always reads from `EventLog::load_from()`
/// after waking — the tail source is just a signal, not a delivery mechanism.
#[async_trait]
pub trait TailSource: Send + Sync {
    /// Wait for a signal that new events may be available.
    async fn wait(&self) -> Result<()>;
}

/// Poll-based tail source that sleeps for a fixed interval.
pub struct PollTailSource {
    interval: Duration,
}

impl PollTailSource {
    pub fn new(interval: Duration) -> Self {
        Self { interval }
    }
}

#[async_trait]
impl TailSource for PollTailSource {
    async fn wait(&self) -> Result<()> {
        tokio::time::sleep(self.interval).await;
        Ok(())
    }
}

// ── PG NOTIFY implementation ────────────────────────────────────────

#[cfg(feature = "postgres")]
mod pg {
    use super::*;
    use sqlx::PgPool;
    use tokio::sync::Mutex;

    /// PG NOTIFY-based tail source for low-latency event tailing.
    ///
    /// Uses `LISTEN` to receive wake-up signals. Always use with a poll
    /// fallback (handled by `ProjectionStream`) to catch missed notifications.
    pub struct PgNotifyTailSource {
        listener: Mutex<sqlx::postgres::PgListener>,
    }

    impl PgNotifyTailSource {
        /// Create and subscribe to a PG NOTIFY channel.
        ///
        /// Call this **before** catch-up to prevent the race condition gap
        /// between catching up and starting to listen.
        pub async fn new(pool: &PgPool, channel: &str) -> Result<Self> {
            let mut listener = sqlx::postgres::PgListener::connect_with(pool).await?;
            listener.listen(channel).await?;
            Ok(Self {
                listener: Mutex::new(listener),
            })
        }
    }

    #[async_trait]
    impl TailSource for PgNotifyTailSource {
        async fn wait(&self) -> Result<()> {
            let mut listener = self.listener.lock().await;
            let _ = listener.recv().await?;
            Ok(())
        }
    }
}

#[cfg(feature = "postgres")]
pub use pg::PgNotifyTailSource;
