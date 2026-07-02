//! Postgres-backed `EffectStore` implementation.
//!
//! Persists memoized reactor side-effect results across process restarts,
//! making crash recovery production-safe. The schema is created by
//! migration `20260614_causal_effect_store.sql`.
//!
//! ## First-write-wins atomicity
//!
//! `put()` uses a CTE so the INSERT and the canonical-value fetch are a
//! single round-trip with no TOCTOU window:
//!
//! ```sql
//! WITH ins AS (
//!     INSERT INTO causal_effect_store … ON CONFLICT … DO NOTHING
//!     RETURNING value
//! )
//! SELECT value FROM ins
//! UNION ALL
//! SELECT value FROM causal_effect_store WHERE … LIMIT 1
//! ```
//!
//! Either the INSERT wins (RETURNING gives us our value) or DO NOTHING
//! fires (the UNION ALL SELECT gives us the winner's value). Exactly one
//! row is always returned.

#[cfg(feature = "postgres")]
mod pg {
    use anyhow::Result;
    use async_trait::async_trait;
    use sqlx::PgPool;

    use causal::effect_store::{EffectKey, EffectStore};

    /// Postgres-backed [`EffectStore`].
    ///
    /// Construct with a `PgPool` already wired to a database where
    /// migration `20260614_causal_effect_store.sql` has been applied.
    /// The backend does NOT auto-create its table.
    pub struct PgEffectStore {
        pool: PgPool,
    }

    impl PgEffectStore {
        pub fn new(pool: PgPool) -> Self {
            Self { pool }
        }
    }

    #[async_trait]
    impl EffectStore for PgEffectStore {
        async fn get(&self, key: &EffectKey) -> Result<Option<serde_json::Value>> {
            let row: Option<(serde_json::Value,)> = sqlx::query_as(
                "SELECT value FROM causal_effect_store \
                 WHERE consumer = $1 AND trigger_event_id = $2 AND label = $3",
            )
            .bind(&key.consumer)
            .bind(key.trigger_event_id)
            .bind(&key.label)
            .fetch_optional(&self.pool)
            .await?;
            Ok(row.map(|(v,)| v))
        }

        async fn put(
            &self,
            key: &EffectKey,
            value: serde_json::Value,
        ) -> Result<serde_json::Value> {
            // Atomic first-write-wins via a CTE: one round-trip, no TOCTOU.
            // If INSERT wins → RETURNING yields our value;
            // if DO NOTHING fires → the UNION ALL SELECT yields the winner's.
            // Exactly one row is always returned.
            let row: (serde_json::Value,) = sqlx::query_as(
                "WITH ins AS ( \
                     INSERT INTO causal_effect_store \
                         (consumer, trigger_event_id, label, value) \
                     VALUES ($1, $2, $3, $4) \
                     ON CONFLICT (consumer, trigger_event_id, label) DO NOTHING \
                     RETURNING value \
                 ) \
                 SELECT value FROM ins \
                 UNION ALL \
                 SELECT value FROM causal_effect_store \
                 WHERE consumer = $1 AND trigger_event_id = $2 AND label = $3 \
                 LIMIT 1",
            )
            .bind(&key.consumer)
            .bind(key.trigger_event_id)
            .bind(&key.label)
            .bind(&value)
            .fetch_one(&self.pool)
            .await?;
            Ok(row.0)
        }

        async fn remove(&self, key: &EffectKey) -> Result<()> {
            sqlx::query(
                "DELETE FROM causal_effect_store \
                 WHERE consumer = $1 AND trigger_event_id = $2 AND label = $3",
            )
            .bind(&key.consumer)
            .bind(key.trigger_event_id)
            .bind(&key.label)
            .execute(&self.pool)
            .await?;
            Ok(())
        }

        async fn list_consumers(&self) -> Result<Vec<String>> {
            let rows: Vec<(String,)> =
                sqlx::query_as("SELECT DISTINCT consumer FROM causal_effect_store")
                    .fetch_all(&self.pool)
                    .await?;
            Ok(rows.into_iter().map(|(c,)| c).collect())
        }
    }

}

#[cfg(feature = "postgres")]
pub use pg::PgEffectStore;
