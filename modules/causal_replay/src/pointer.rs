//! Pointer store for tracking projection position.

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};

use causal::LogCursor;

/// Position tracking for projection streams.
///
/// Two columns: `active` (promoted, used in live mode) and `staged`
/// (written during replay, promoted on success).
#[async_trait]
pub trait PointerStore: Send + Sync {
    /// Current position — the promoted `active` log cursor.
    ///
    /// Use at boot to derive the database name (e.g., `neo4j.v{position}`).
    async fn position(&self) -> Result<Option<LogCursor>>;

    /// Save position directly to `active`.
    async fn save(&self, position: LogCursor) -> Result<()>;

    /// Write position to `staged` column only.
    async fn stage(&self, position: LogCursor) -> Result<()>;

    /// Promote `staged` → `active`. Returns the promoted position.
    async fn promote(&self) -> Result<LogCursor>;

    /// Force-set the `active` position directly.
    async fn set(&self, position: LogCursor) -> Result<()>;

    /// Read current pointer status.
    async fn status(&self) -> Result<PointerStatus>;
}

/// Current state of the replay pointer.
#[derive(Debug, Clone)]
pub struct PointerStatus {
    pub active: LogCursor,
    pub staged: Option<LogCursor>,
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
                "CREATE TABLE IF NOT EXISTS causal_replay_pointer (
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
                "INSERT INTO causal_replay_pointer (id, active, updated_at)
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
        async fn position(&self) -> Result<Option<LogCursor>> {
            let row: Option<(i64,)> =
                sqlx::query_as("SELECT active FROM causal_replay_pointer WHERE id = 1")
                    .fetch_optional(&self.db)
                    .await?;
            Ok(row.map(|(v,)| LogCursor::from_raw(v as u64)))
        }

        async fn save(&self, position: LogCursor) -> Result<()> {
            sqlx::query(
                "UPDATE causal_replay_pointer
                 SET active = $1, updated_at = now()
                 WHERE id = 1",
            )
            .bind(position.raw() as i64)
            .execute(&self.db)
            .await?;
            Ok(())
        }

        async fn stage(&self, position: LogCursor) -> Result<()> {
            sqlx::query(
                "UPDATE causal_replay_pointer
                 SET staged = $1, updated_at = now()
                 WHERE id = 1",
            )
            .bind(position.raw() as i64)
            .execute(&self.db)
            .await?;
            Ok(())
        }

        async fn promote(&self) -> Result<LogCursor> {
            let row: (i64,) = sqlx::query_as(
                "UPDATE causal_replay_pointer
                 SET active = staged, staged = NULL, updated_at = now()
                 WHERE id = 1 AND staged IS NOT NULL
                 RETURNING active",
            )
            .fetch_one(&self.db)
            .await?;
            Ok(LogCursor::from_raw(row.0 as u64))
        }

        async fn set(&self, position: LogCursor) -> Result<()> {
            sqlx::query(
                "UPDATE causal_replay_pointer
                 SET active = $1, updated_at = now()
                 WHERE id = 1",
            )
            .bind(position.raw() as i64)
            .execute(&self.db)
            .await?;
            Ok(())
        }

        async fn status(&self) -> Result<PointerStatus> {
            let row: (i64, Option<i64>, DateTime<Utc>) = sqlx::query_as(
                "SELECT active, staged, updated_at FROM causal_replay_pointer WHERE id = 1",
            )
            .fetch_one(&self.db)
            .await?;
            Ok(PointerStatus {
                active: LogCursor::from_raw(row.0 as u64),
                staged: row.1.map(|v| LogCursor::from_raw(v as u64)),
                updated_at: row.2,
            })
        }
    }
}

#[cfg(feature = "postgres")]
pub use pg::PgPointerStore;
