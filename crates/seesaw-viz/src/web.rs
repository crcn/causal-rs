//! Live web viewer for workflow visualization (P3 - Coming Soon)
//!
//! Provides an Axum-based web interface for real-time visualization of event flows.

use crate::collector::SpanCollector;
use crate::renderer::{MermaidRenderer, RenderOptions};
use axum::{
    extract::State,
    response::{Html, IntoResponse},
    routing::get,
    Router,
};
use std::sync::Arc;
use tower_http::cors::CorsLayer;

/// Start the live web viewer
///
/// # Example
///
/// ```rust,no_run
/// use seesaw_viz::web::start_viewer;
///
/// let collector = SpanCollector::new(1000);
/// start_viewer(collector, "127.0.0.1:3000").await?;
/// # Ok::<(), anyhow::Error>(())
/// ```
pub async fn start_viewer<S>(collector: SpanCollector<S>, addr: &str) -> Result<(), anyhow::Error>
where
    S: Send + Sync + 'static,
{
    let state = Arc::new(ViewerState { collector });

    let app = Router::new()
        .route("/", get(index_handler))
        .route("/api/graph", get(graph_handler::<S>))
        .route("/api/diagram", get(diagram_handler::<S>))
        .layer(CorsLayer::permissive())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    println!("🌐 Seesaw Viz listening on http://{}", addr);

    axum::serve(listener, app).await?;

    Ok(())
}

struct ViewerState<S> {
    collector: SpanCollector<S>,
}

async fn index_handler() -> Html<&'static str> {
    Html(include_str!("../static/index.html"))
}

async fn graph_handler<S: Send + Sync + 'static>(
    State(state): State<Arc<ViewerState<S>>>,
) -> impl IntoResponse {
    let graph = state.collector.graph().await;
    axum::Json(graph)
}

async fn diagram_handler<S: Send + Sync + 'static>(
    State(state): State<Arc<ViewerState<S>>>,
) -> impl IntoResponse {
    let graph = state.collector.graph().await;
    let renderer = MermaidRenderer::new(RenderOptions::default());
    let diagram = renderer.render(&graph);

    axum::Json(serde_json::json!({
        "diagram": diagram,
        "stats": state.collector.stats().await,
    }))
}

// TODO: Add WebSocket endpoint for real-time updates
// TODO: Add static HTML/JS viewer
