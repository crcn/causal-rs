//! Span collection via async channels (P1 - Critical for non-blocking)
//!
//! This module implements span collection off the hot path using tokio mpsc channels.
//! Event spans are sent to a background task that builds the graph without blocking
//! the Seesaw engine.

use crate::formatter::{FormatterError, StateFormatter};
use crate::sampling::SamplingStrategy;
use crate::types::{EventSpan, SpanGraph};
use chrono::Utc;
use std::any::TypeId;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use uuid::Uuid;

/// Configuration for span collection
#[derive(Debug, Clone)]
pub struct CollectorConfig {
    /// Buffer size for the async channel
    pub buffer_size: usize,

    /// Whether to collect state snapshots
    pub collect_state: bool,

    /// Whether to compute state diffs
    pub compute_diffs: bool,

    /// Maximum spans to keep in memory
    pub max_spans: Option<usize>,
}

impl Default for CollectorConfig {
    fn default() -> Self {
        Self {
            buffer_size: 1000,
            collect_state: true,
            compute_diffs: true,
            max_spans: Some(10_000),
        }
    }
}

/// Async span collector that builds the span graph off the hot path
pub struct SpanCollector<S> {
    /// Configuration
    config: CollectorConfig,

    /// Sender for span commands
    tx: mpsc::Sender<SpanCommand>,

    /// Shared graph (read-only for queries)
    graph: Arc<RwLock<SpanGraph>>,

    /// Sampling strategy
    sampler: Arc<dyn SamplingStrategy>,

    /// Phantom data to keep S type parameter
    _phantom: std::marker::PhantomData<S>,
}

impl<S> SpanCollector<S>
where
    S: Send + 'static,
{
    /// Create a new span collector with default config
    pub fn new(buffer_size: usize) -> Self {
        Self::with_config(CollectorConfig {
            buffer_size,
            ..Default::default()
        })
    }

    /// Create a span collector with custom config
    pub fn with_config(config: CollectorConfig) -> Self {
        let (tx, rx) = mpsc::channel(config.buffer_size);
        let graph = Arc::new(RwLock::new(SpanGraph::default()));

        // Spawn background processor
        let processor = SpanProcessor {
            rx,
            graph: Arc::clone(&graph),
            config: config.clone(),
        };

        tokio::spawn(async move {
            processor.run().await;
        });

        Self {
            config,
            tx,
            graph,
            sampler: Arc::new(crate::sampling::AlwaysSample), // Default: sample everything
            _phantom: std::marker::PhantomData,
        }
    }

    /// Set a custom sampling strategy
    pub fn with_sampler(mut self, sampler: impl SamplingStrategy + 'static) -> Self {
        self.sampler = Arc::new(sampler);
        self
    }

    /// Create an observer effect for Seesaw's on_any()
    ///
    /// This returns a closure that can be used in:
    /// ```rust,ignore
    /// effect::on_any().then(collector.create_observer())
    /// ```
    pub fn create_observer<F>(&self, formatter: F) -> SpanObserver<S, F>
    where
        F: StateFormatter<S> + Clone + Send + 'static,
    {
        SpanObserver {
            tx: self.tx.clone(),
            formatter,
            sampler: Arc::clone(&self.sampler),
            _phantom: std::marker::PhantomData,
        }
    }

    /// Start a new span (manual API)
    pub async fn start_span(&self, event_id: Uuid, event_type: String) -> Result<Uuid, CollectorError> {
        let span_id = Uuid::new_v4();

        if !self.sampler.should_sample(&event_type, &HashMap::new()) {
            return Ok(span_id); // Skip sampling
        }

        self.tx
            .send(SpanCommand::Start {
                span_id,
                event_id,
                event_type,
                event_type_id: TypeId::of::<()>(), // Default
                parent_event_id: None,
                module_path: None,
                effect_name: None,
            })
            .await
            .map_err(|_| CollectorError::ChannelClosed)?;

        Ok(span_id)
    }

    /// Complete a span with success
    pub async fn complete_span(&self, span_id: Uuid) -> Result<(), CollectorError> {
        self.tx
            .send(SpanCommand::Complete {
                span_id,
                success: true,
                error: None,
            })
            .await
            .map_err(|_| CollectorError::ChannelClosed)
    }

    /// Complete a span with error
    pub async fn fail_span(&self, span_id: Uuid, error: String) -> Result<(), CollectorError> {
        self.tx
            .send(SpanCommand::Complete {
                span_id,
                success: false,
                error: Some(error),
            })
            .await
            .map_err(|_| CollectorError::ChannelClosed)
    }

    /// Get a snapshot of the current span graph
    pub async fn graph(&self) -> SpanGraph {
        self.graph.read().await.clone()
    }

    /// Get statistics about collected spans
    pub async fn stats(&self) -> CollectorStats {
        let graph = self.graph.read().await;
        CollectorStats {
            total_spans: graph.spans.len(),
            root_spans: graph.roots.len(),
            complete_spans: graph.spans.values().filter(|s| s.is_complete()).count(),
            failed_spans: graph.spans.values().filter(|s| !s.success).count(),
        }
    }

    /// Clear all collected spans
    pub async fn clear(&self) -> Result<(), CollectorError> {
        self.tx
            .send(SpanCommand::Clear)
            .await
            .map_err(|_| CollectorError::ChannelClosed)
    }
}

/// Observer that can be attached to Seesaw's on_any() effect
#[derive(Clone)]
pub struct SpanObserver<S, F> {
    tx: mpsc::Sender<SpanCommand>,
    formatter: F,
    sampler: Arc<dyn SamplingStrategy>,
    _phantom: std::marker::PhantomData<S>,
}

impl<S, F> SpanObserver<S, F>
where
    S: Clone + Send + 'static,
    F: StateFormatter<S> + Clone + Send + 'static,
{
    /// Record an event (call from on_any effect)
    pub async fn record(
        &self,
        event_id: Uuid,
        event_type: String,
        event_type_id: TypeId,
        parent_event_id: Option<Uuid>,
        module_path: Option<String>,
        effect_name: Option<String>,
        prev_state: Option<S>,
        next_state: Option<S>,
    ) -> Result<(), CollectorError> {
        // Check sampling
        let attributes = HashMap::new();
        if !self.sampler.should_sample(&event_type, &attributes) {
            return Ok(());
        }

        let span_id = Uuid::new_v4();

        // Serialize state if provided
        let (prev_val, next_val, diff) = if let (Some(prev), Some(next)) = (prev_state, next_state) {
            let prev_val = self.formatter.serialize(&prev).ok();
            let next_val = self.formatter.serialize(&next).ok();
            let diff = self.formatter.diff(&prev, &next).ok().flatten();
            (prev_val, next_val, diff)
        } else {
            (None, None, None)
        };

        self.tx
            .send(SpanCommand::Record {
                span_id,
                event_id,
                event_type,
                event_type_id,
                parent_event_id,
                module_path,
                effect_name,
                prev_state: prev_val,
                next_state: next_val,
                state_diff: diff,
            })
            .await
            .map_err(|_| CollectorError::ChannelClosed)
    }
}

/// Commands sent to the background processor
enum SpanCommand {
    Start {
        span_id: Uuid,
        event_id: Uuid,
        event_type: String,
        event_type_id: TypeId,
        parent_event_id: Option<Uuid>,
        module_path: Option<String>,
        effect_name: Option<String>,
    },
    Complete {
        span_id: Uuid,
        success: bool,
        error: Option<String>,
    },
    Record {
        span_id: Uuid,
        event_id: Uuid,
        event_type: String,
        event_type_id: TypeId,
        parent_event_id: Option<Uuid>,
        module_path: Option<String>,
        effect_name: Option<String>,
        prev_state: Option<serde_json::Value>,
        next_state: Option<serde_json::Value>,
        state_diff: Option<serde_json::Value>,
    },
    Clear,
}

/// Background processor that builds the span graph
struct SpanProcessor {
    rx: mpsc::Receiver<SpanCommand>,
    graph: Arc<RwLock<SpanGraph>>,
    config: CollectorConfig,
}

impl SpanProcessor {
    async fn run(mut self) {
        let mut pending_spans: HashMap<Uuid, EventSpan> = HashMap::new();

        while let Some(cmd) = self.rx.recv().await {
            match cmd {
                SpanCommand::Start {
                    span_id,
                    event_id,
                    event_type,
                    event_type_id,
                    parent_event_id,
                    module_path,
                    effect_name,
                } => {
                    let span = EventSpan {
                        span_id,
                        event_id,
                        parent_event_id,
                        event_type,
                        event_type_id,
                        effect_name,
                        module_path,
                        started_at: Utc::now(),
                        completed_at: None,
                        duration_us: None,
                        prev_state: None,
                        next_state: None,
                        state_diff: None,
                        success: false,
                        error: None,
                        attributes: HashMap::new(),
                    };

                    pending_spans.insert(span_id, span);
                }

                SpanCommand::Complete {
                    span_id,
                    success,
                    error,
                } => {
                    if let Some(mut span) = pending_spans.remove(&span_id) {
                        let completed_at = Utc::now();
                        span.completed_at = Some(completed_at);
                        span.duration_us = Some((completed_at - span.started_at).num_microseconds().unwrap_or(0) as u64);
                        span.success = success;
                        span.error = error;

                        let mut graph = self.graph.write().await;
                        graph.add_span(span);

                        // Enforce max spans limit
                        if let Some(max) = self.config.max_spans {
                            if graph.spans.len() > max {
                                // TODO: Implement LRU eviction
                            }
                        }
                    }
                }

                SpanCommand::Record {
                    span_id,
                    event_id,
                    event_type,
                    event_type_id,
                    parent_event_id,
                    module_path,
                    effect_name,
                    prev_state,
                    next_state,
                    state_diff,
                } => {
                    let span = EventSpan {
                        span_id,
                        event_id,
                        parent_event_id,
                        event_type,
                        event_type_id,
                        effect_name,
                        module_path,
                        started_at: Utc::now(),
                        completed_at: Some(Utc::now()),
                        duration_us: Some(0), // Instantaneous
                        prev_state,
                        next_state,
                        state_diff,
                        success: true,
                        error: None,
                        attributes: HashMap::new(),
                    };

                    let mut graph = self.graph.write().await;
                    graph.add_span(span);
                }

                SpanCommand::Clear => {
                    let mut graph = self.graph.write().await;
                    *graph = SpanGraph::default();
                    pending_spans.clear();
                }
            }
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CollectorStats {
    pub total_spans: usize,
    pub root_spans: usize,
    pub complete_spans: usize,
    pub failed_spans: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum CollectorError {
    #[error("Collector channel closed")]
    ChannelClosed,

    #[error("Formatter error: {0}")]
    Formatter(#[from] FormatterError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formatter::JsonDiffFormatter;

    #[tokio::test]
    async fn test_span_collector() {
        let collector = SpanCollector::<()>::new(10);

        let event_id = Uuid::new_v4();
        let span_id = collector
            .start_span(event_id, "TestEvent".into())
            .await
            .unwrap();

        collector.complete_span(span_id).await.unwrap();

        // Give background task time to process
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        let stats = collector.stats().await;
        assert_eq!(stats.total_spans, 1);
        assert_eq!(stats.complete_spans, 1);
    }

    #[tokio::test]
    async fn test_observer() {
        let collector = SpanCollector::<i32>::new(10);
        let observer = collector.create_observer(JsonDiffFormatter);

        observer
            .record(
                Uuid::new_v4(),
                "TestEvent".into(),
                TypeId::of::<()>(),
                None,
                Some("test::module".into()),
                Some("test_effect".into()),
                Some(1),
                Some(2),
            )
            .await
            .unwrap();

        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        let stats = collector.stats().await;
        assert_eq!(stats.total_spans, 1);
    }
}
