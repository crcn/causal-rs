//! Command modules for dev-cli.

pub mod examples;
pub mod stack;
pub mod test;

/// Live-test infrastructure (separate from any example stack).
pub const DEV_COMPOSE: &str = "dev/docker-compose.yml";
pub const DEV_PG_URL: &str = "postgres://causal:causal@localhost:5433/causal_dev";
pub const DEV_KURRENT_URL: &str = "kurrentdb://localhost:2114?tls=false";
