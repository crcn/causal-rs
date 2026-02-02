//! OpenTelemetry integration for seesaw.
//!
//! This module provides helpers for initializing OpenTelemetry tracing.
//! Enable the `otel` feature to use this module.
//!
//! # Example
//!
//! ```rust,ignore
//! use seesaw::otel::init_otel_tracing;
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     init_otel_tracing("my-service")?;
//!     // Your application code here
//!     Ok(())
//! }
//! ```
//!
//! # Environment Variables
//!
//! - `OTEL_EXPORTER_OTLP_ENDPOINT`: The OTLP endpoint (default: `http://localhost:4317`)
//! - `OTEL_SERVICE_NAME`: Override the service name
//! - `RUST_LOG`: Log level filter (default: `info`)

use opentelemetry::trace::TracerProvider;
use opentelemetry::KeyValue;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::{runtime, trace as sdktrace, Resource};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Initialize OpenTelemetry tracing with OTLP exporter.
///
/// This sets up:
/// - OTLP span exporter (gRPC via tonic)
/// - Tracing subscriber with env-filter
/// - Console output layer for local debugging
///
/// # Arguments
///
/// * `service_name` - The name of your service for OTEL resource identification
///
/// # Returns
///
/// Returns `Ok(())` on success, or an error if initialization fails.
///
/// # Panics
///
/// This function will panic if called more than once (tracing subscriber can only be set once).
pub fn init_otel_tracing(service_name: &str) -> anyhow::Result<()> {
    let endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
        .unwrap_or_else(|_| "http://localhost:4317".to_string());

    let exporter = opentelemetry_otlp::new_exporter()
        .tonic()
        .with_endpoint(&endpoint);

    let tracer_provider = opentelemetry_otlp::new_pipeline()
        .tracing()
        .with_exporter(exporter)
        .with_trace_config(
            sdktrace::Config::default().with_resource(Resource::new(vec![KeyValue::new(
                "service.name",
                service_name.to_string(),
            )])),
        )
        .install_batch(runtime::Tokio)?;

    let tracer = tracer_provider.tracer(service_name.to_string());
    let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .with(otel_layer)
        .init();

    Ok(())
}

/// Initialize basic tracing without OpenTelemetry.
///
/// Use this when you want tracing support but don't need OTEL export.
/// This is useful for local development or when the `otel` feature is disabled.
pub fn init_basic_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .init();
}
