//! Pointer store for tracking projection position.

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};

/// Position tracking for projection streams.
///
/// Two columns: `active` (promoted, used in live mode) and `staged`
/// (written during replay, promoted on success).
#[async_trait]
pub trait PointerStore: Send + Sync {
    /// Load the current position.
    ///
    /// In replay mode (when `SEESAW_REPLAY_VERSION` env var is set),
    /// returns the env var value. Otherwise returns the `active` position
    /// from the database.
    async fn load(&self) -> Result<Option<u64>>;

    /// Save position. In replay mode, writes to `staged`.
    /// In live mode, writes to `active`.
    async fn save(&self, position: u64) -> Result<()>;

    /// Write position to `staged` column only.
    async fn stage(&self, position: u64) -> Result<()>;

    /// Promote `staged` → `active`. Returns the promoted position.
    async fn promote(&self) -> Result<u64>;

    /// Force-set the `active` position directly.
    async fn set(&self, position: u64) -> Result<()>;

    /// Read current pointer status.
    async fn status(&self) -> Result<PointerStatus>;
}

/// Current state of the replay pointer.
#[derive(Debug, Clone)]
pub struct PointerStatus {
    pub active: u64,
    pub staged: Option<u64>,
    pub updated_at: DateTime<Utc>,
}

// ── Postgres implementation ─────────────────────────────────────────

#[cfg(feature = "postgres")]
mod pg {
    use super::*;
    use sqlx::PgPool;

    /// Postgres-backed pointer store with auto-created table.
    pub struct PgPointerStore {
        db: PgPool,
    }

    impl PgPointerStore {
        /// Create a new pointer store, auto-creating the table if needed.
        pub async fn new(db: PgPool) -> Result<Self> {
            sqlx::query(
                "CREATE TABLE IF NOT EXISTS seesaw_replay_pointer (
                    id INTEGER PRIMARY KEY DEFAULT 1 CHECK (id = 1),
                    active BIGINT NOT NULL DEFAULT 0,
                    staged BIGINT,
                    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
                )",
            )
            .execute(&db)
            .await?;

            // Ensure the singleton row exists.
            sqlx::query(
                "INSERT INTO seesaw_replay_pointer (id, active, updated_at)
                 VALUES (1, 0, now())
                 ON CONFLICT (id) DO NOTHING",
            )
            .execute(&db)
            .await?;

            Ok(Self { db })
        }
    }

    #[async_trait]
    impl PointerStore for PgPointerStore {
        async fn load(&self) -> Result<Option<u64>> {
            // Replay mode: use env var if set.
            if let Ok(v) = std::env::var("SEESAW_REPLAY_VERSION") {
                return Ok(v.parse().ok());
            }
            // Live mode: read active from database.
            let row: Option<(i64,)> =
                sqlx::query_as("SELECT active FROM seesaw_replay_pointer WHERE id = 1")
                    .fetch_optional(&self.db)
                    .await?;
            Ok(row.map(|(v,)| v as u64))
        }

        async fn save(&self, position: u64) -> Result<()> {
            if std::env::var("REPLAY").is_ok() {
                return self.stage(position).await;
            }
            sqlx::query(
                "UPDATE seesaw_replay_pointer
                 SET active = $1, updated_at = now()
                 WHERE id = 1",
            )
            .bind(position as i64)
            .execute(&self.db)
            .await?;
            Ok(())
        }

        async fn stage(&self, position: u64) -> Result<()> {
            sqlx::query(
                "UPDATE seesaw_replay_pointer
                 SET staged = $1, updated_at = now()
                 WHERE id = 1",
            )
            .bind(position as i64)
            .execute(&self.db)
            .await?;
            Ok(())
        }

        async fn promote(&self) -> Result<u64> {
            let row: (i64,) = sqlx::query_as(
                "UPDATE seesaw_replay_pointer
                 SET active = staged, staged = NULL, updated_at = now()
                 WHERE id = 1 AND staged IS NOT NULL
                 RETURNING active",
            )
            .fetch_one(&self.db)
            .await?;
            Ok(row.0 as u64)
        }

        async fn set(&self, position: u64) -> Result<()> {
            sqlx::query(
                "UPDATE seesaw_replay_pointer
                 SET active = $1, updated_at = now()
                 WHERE id = 1",
            )
            .bind(position as i64)
            .execute(&self.db)
            .await?;
            Ok(())
        }

        async fn status(&self) -> Result<PointerStatus> {
            let row: (i64, Option<i64>, DateTime<Utc>) = sqlx::query_as(
                "SELECT active, staged, updated_at FROM seesaw_replay_pointer WHERE id = 1",
            )
            .fetch_one(&self.db)
            .await?;
            Ok(PointerStatus {
                active: row.0 as u64,
                staged: row.1.map(|v| v as u64),
                updated_at: row.2,
            })
        }
    }
}

#[cfg(feature = "postgres")]
pub use pg::PgPointerStore;
