//! Compile-time safety for distributed worker deployments.
//!
//! The `DistributedSafe` trait marks types that are safe to use as dependencies
//! in multi-worker deployments where each worker is a separate process.
//!
//! # Safety
//!
//! Types implementing `DistributedSafe` MUST NOT contain:
//! - `Arc<Mutex<T>>` or other in-memory shared state
//! - File handles or OS resources
//! - Thread-local storage
//! - Any state that won't be synchronized across separate processes
//!
//! Safe types include:
//! - Database connection pools (e.g., `sqlx::PgPool`)
//! - HTTP clients (e.g., `reqwest::Client`)
//! - Redis clients
//! - Any stateless service or external resource
//!
//! # Example
//!
//! ```rust,ignore
//! use seesaw_core::DistributedSafe;
//!
//! // Safe: all fields are shared external resources
//! #[derive(Clone, DistributedSafe)]
//! struct Deps {
//!     db: sqlx::PgPool,      // ✅ Shared across workers
//!     http: reqwest::Client, // ✅ Stateless
//! }
//!
//! // Unsafe: contains in-memory state
//! #[derive(Clone)]
//! struct UnsafeDeps {
//!     cache: Arc<Mutex<HashMap<K, V>>>,  // ❌ Each worker has separate copy!
//! }
//! ```
//!
//! # Opt-out
//!
//! For development or single-worker deployments, you can opt out:
//!
//! ```rust,ignore
//! #[derive(Clone, DistributedSafe)]
//! struct Deps {
//!     db: sqlx::PgPool,
//!     #[allow_non_distributed]  // ⚠️ Explicit opt-out with warning
//!     cache: Arc<Mutex<HashMap>>,
//! }
//! ```

/// Sealed trait to prevent external implementations
#[doc(hidden)]
pub mod sealed {
    pub trait Sealed {}
}

/// Marker trait for types safe to share across distributed workers.
///
/// This trait is **sealed** and can only be implemented:
/// 1. Automatically for known-safe types (database pools, HTTP clients, etc.)
/// 2. Via the `#[derive(DistributedSafe)]` macro with field validation
/// 3. Explicitly with `unsafe impl` (use with caution!)
///
/// # Safety Contract
///
/// Implementing this trait asserts that:
/// - The type does not contain in-memory state (`Arc<Mutex<T>>`, `RwLock`, etc.)
/// - All state is stored in external systems (databases, caches, etc.)
/// - The type behaves correctly when cloned across separate processes
///
/// Violating this contract leads to silent data divergence across workers.
pub trait DistributedSafe: Clone + Send + Sync + sealed::Sealed + 'static {}

// Implement for common safe types from standard library
impl sealed::Sealed for String {}
impl DistributedSafe for String {}

impl sealed::Sealed for std::sync::Arc<str> {}
impl DistributedSafe for std::sync::Arc<str> {}

// Tuples up to 4 elements
impl<A: DistributedSafe> sealed::Sealed for (A,) {}
impl<A: DistributedSafe> DistributedSafe for (A,) {}

impl<A: DistributedSafe, B: DistributedSafe> sealed::Sealed for (A, B) {}
impl<A: DistributedSafe, B: DistributedSafe> DistributedSafe for (A, B) {}

impl<A: DistributedSafe, B: DistributedSafe, C: DistributedSafe> sealed::Sealed for (A, B, C) {}
impl<A: DistributedSafe, B: DistributedSafe, C: DistributedSafe> DistributedSafe for (A, B, C) {}

impl<A: DistributedSafe, B: DistributedSafe, C: DistributedSafe, D: DistributedSafe> sealed::Sealed
    for (A, B, C, D)
{
}
impl<A: DistributedSafe, B: DistributedSafe, C: DistributedSafe, D: DistributedSafe> DistributedSafe
    for (A, B, C, D)
{
}

// Note: Arc<Mutex<T>> is intentionally NOT implemented
// This prevents accidentally using in-memory state in distributed deployments

// Implementations for external crates (feature-gated)

#[cfg(feature = "sqlx")]
mod sqlx_impls {
    use super::*;

    impl sealed::Sealed for sqlx::PgPool {}
    impl DistributedSafe for sqlx::PgPool {}
}

#[cfg(feature = "reqwest")]
mod reqwest_impls {
    use super::*;

    impl sealed::Sealed for reqwest::Client {}
    impl DistributedSafe for reqwest::Client {}
}

#[cfg(feature = "redis")]
mod redis_impls {
    use super::*;

    impl sealed::Sealed for redis::Client {}
    impl DistributedSafe for redis::Client {}

    impl sealed::Sealed for redis::aio::ConnectionManager {}
    impl DistributedSafe for redis::aio::ConnectionManager {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_string_is_distributed_safe() {
        fn assert_distributed_safe<T: DistributedSafe>() {}
        assert_distributed_safe::<String>();
    }

    #[test]
    fn test_tuple_is_distributed_safe() {
        fn assert_distributed_safe<T: DistributedSafe>() {}
        assert_distributed_safe::<(String, String)>();
    }
}
