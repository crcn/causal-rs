//! Workflow visualization and observability for Seesaw
//!
//! This crate provides tools for visualizing event flows, tracking causality,
//! and generating diagrams from Seesaw runtime execution.
//!
//! # Design Principles
//!
//! 1. **Non-blocking**: Span collection happens off the hot path via async channels
//! 2. **Causality-aware**: Tracks parent-child relationships via `parent_event_id`
//! 3. **Component-grouped**: Uses `module_path!()` to organize visualizations
//! 4. **Sampling-capable**: Configurable sampling to reduce overhead
//!
//! # Example
//!
//! ```rust,ignore
//! use seesaw_viz::{SpanCollector, MermaidRenderer, StateFormatter};
//!
//! // Create collector with async channel
//! let collector = SpanCollector::new(1000); // Buffer size
//!
//! // Attach to Seesaw engine via on_any() observer
//! let observer = collector.create_observer(seesaw_viz::JsonDiffFormatter);
//!
//! // Generate diagram
//! // let graph = collector.graph().await;
//! // let diagram = MermaidRenderer::new().render(&graph)?;
//! ```

pub mod collector;
pub mod formatter;
pub mod renderer;
pub mod sampling;
pub mod types;

#[cfg(feature = "web-viewer")]
pub mod web;

pub use collector::{SpanCollector, SpanObserver};
pub use formatter::{JsonDiffFormatter, StateFormatter};
pub use renderer::{MermaidRenderer, RenderOptions};
pub use sampling::{AlwaysSample, RateSample, SamplingStrategy};
pub use types::{CausalChain, EventSpan, SpanGraph};

/// Re-export for convenience
pub use chrono::{DateTime, Utc};
pub use uuid::Uuid;
