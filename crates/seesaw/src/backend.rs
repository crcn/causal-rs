//! Backend trait and core abstractions for v0.11.0

pub mod capability;
pub mod job_executor;

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::runtime::event_worker::EventWorkerConfig;
use crate::runtime::handler_worker::HandlerWorkerConfig;

/// Core backend trait - simplified API for v0.11.0.
///
/// Backends implement:
/// - `publish()`: Accept dispatched events
/// - `serve()`: Run workers until shutdown
///
/// Optional capabilities are provided via separate traits (see `capability` module).
#[async_trait]
pub trait Backend: Send + Sync + 'static {
    /// Backend name for diagnostics.
    fn name(&self) -> &'static str {
        "custom"
    }

    /// Publish an event to the backend.
    async fn publish(&self, event: DispatchedEvent) -> Result<()>;

    /// Start serving workers (blocking until shutdown).
    ///
    /// The backend should spawn workers according to `config` and run until
    /// the `shutdown` token is cancelled.
    async fn serve<D>(
        &self,
        executor: Arc<job_executor::JobExecutor<D>>,
        config: BackendServeConfig,
        shutdown: CancellationToken,
    ) -> Result<()>
    where
        D: Send + Sync + 'static;
}

/// Dispatched event sent to backend via `publish()`.
#[derive(Debug, Clone)]
pub struct DispatchedEvent {
    pub event_id: Uuid,
    pub parent_id: Option<Uuid>,
    pub correlation_id: Uuid,
    pub event_type: String,
    pub payload: serde_json::Value,
    pub hops: i32,
    pub retry_count: i32,
    pub batch_id: Option<Uuid>,
    pub batch_index: Option<i32>,
    pub batch_size: Option<i32>,
    pub created_at: DateTime<Utc>,
}

/// Configuration for backend workers.
#[derive(Debug, Clone)]
pub struct BackendServeConfig {
    /// Number of event workers to spawn.
    pub event_workers: usize,
    /// Number of handler workers to spawn.
    pub handler_workers: usize,
    /// Event worker configuration.
    pub event_worker: EventWorkerConfig,
    /// Handler worker configuration.
    pub handler_worker: HandlerWorkerConfig,
    /// Graceful shutdown timeout (time to wait for in-flight work to complete).
    pub graceful_shutdown_timeout: Duration,
}

impl Default for BackendServeConfig {
    fn default() -> Self {
        Self {
            event_workers: 2,
            handler_workers: 4,
            event_worker: EventWorkerConfig::default(),
            handler_worker: HandlerWorkerConfig::default(),
            graceful_shutdown_timeout: Duration::from_secs(30),
        }
    }
}
