//! Postgres-backed `ConsumerLeasor` implementation.
//!
//! Uses Postgres session-level advisory locks so the OS reclaims the lock
//! automatically on process crash — no separate heartbeat or TTL needed.
//! The advisory-lock key is derived from the consumer_id via FNV-1a so
//! it fits in an i32 (stable, collision-negligible for < 1000 consumers).
//!
//! ## Lock namespace
//!
//! Two-arg `pg_advisory_lock(classid int4, objid int4)` form, namespaced
//! under `0xCA05` ("CAuSal"). The consumer-lease objid range starts at
//! `0xC1_0000` to keep it distinct from other causal advisory locks
//! (e.g., append-serialization at `0xA1`).
//!
//! ## Connection lifecycle
//!
//! Each acquired `PgLeaseGuard` holds a **dedicated** connection (not from
//! the pool). Session advisory locks are tied to the connection's session;
//! returning the connection to the pool would release the lock. Dropping
//! the guard drops the connection — Postgres then releases all its
//! session advisory locks automatically.

#[cfg(feature = "postgres")]
mod pg {
    use anyhow::Result;
    use async_trait::async_trait;
    use sqlx::Connection as _;

    use causal::consumer_lease::{ConsumerLeasor, LeaseGuard};

    /// Advisory-lock class for all consumer-lease locks.
    /// `0xCA05` = "CAuSal", matching the append-serialization namespace.
    const ADVISORY_CLASS: i32 = 0xCA05_u32 as i32;

    /// Derive a stable i32 objid for `consumer_id` via FNV-1a 32-bit.
    /// Collisions are negligible for < 1000 unique consumer names.
    fn fnv1a_32(s: &str) -> i32 {
        let mut hash: u32 = 2_166_136_261;
        for byte in s.bytes() {
            hash ^= u32::from(byte);
            hash = hash.wrapping_mul(16_777_619);
        }
        // Shift into the consumer-lease objid range (high bit set = negative
        // as i32, which is fine — Postgres just treats it as the signed value).
        hash as i32
    }

    /// Holds the dedicated Postgres connection that owns the advisory lock.
    /// Dropping this struct drops the connection, which releases the lock.
    pub struct PgLeaseGuard {
        // The connection is kept alive (not returned to pool) so the
        // session advisory lock survives for the guard's lifetime.
        // We hold it by value; dropping it closes the connection and
        // Postgres releases all session advisory locks automatically.
        _conn: sqlx::PgConnection,
    }

    impl LeaseGuard for PgLeaseGuard {}

    /// Postgres-backed [`ConsumerLeasor`].
    ///
    /// Construct with a `database_url` string. Each `acquire()` call
    /// opens a dedicated connection (not from a shared pool) so the
    /// session advisory lock is tied to exactly one TCP connection.
    pub struct PgConsumerLeasor {
        url: String,
    }

    impl PgConsumerLeasor {
        pub fn new(database_url: impl Into<String>) -> Self {
            Self { url: database_url.into() }
        }
    }

    #[async_trait]
    impl ConsumerLeasor for PgConsumerLeasor {
        async fn acquire(&self, consumer_id: &str) -> Result<Box<dyn LeaseGuard>> {
            let mut conn = sqlx::PgConnection::connect(&self.url).await?;
            let key = fnv1a_32(consumer_id);
            // pg_advisory_lock blocks until acquired. Session-level (NOT
            // xact-level) so it survives COMMIT/ROLLBACK on this connection
            // and is released only when the connection closes.
            sqlx::query("SELECT pg_advisory_lock($1, $2)")
                .bind(ADVISORY_CLASS)
                .bind(key)
                .execute(&mut conn)
                .await?;
            Ok(Box::new(PgLeaseGuard { _conn: conn }))
        }
    }
}

#[cfg(feature = "postgres")]
pub use pg::PgConsumerLeasor;
