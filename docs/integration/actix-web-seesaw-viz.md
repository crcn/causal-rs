# Integrating seesaw-viz with Actix-web

This guide shows how to integrate the `seesaw-viz` live viewer with any Actix-web + Seesaw application.

> **Note**: This is a generic integration guide. Adapt the module paths, types, and structure to match your project's organization.

## Architecture

```
Seesaw DomainService
  └─> on_any() observer
        └─> SpanCollector (async channel)
              └─> Background processor builds graph
                    └─> Exposed via /seesaw HTTP endpoint
```

## Step 1: Add Dependencies

Add to your `Cargo.toml`:

```toml
[dependencies]
seesaw-viz = { path = "<path-to-seesaw-viz>", features = ["web-viewer"] }
# Or from crates.io once published:
# seesaw-viz = { version = "0.1", features = ["web-viewer"] }
```

## Step 2: Create Visualization Module

Create a module for visualization handlers (e.g., `src/server/seesaw_viz.rs` or wherever your handlers live):

```rust
// NOTE: Replace `YourStateType` with your actual Seesaw state type throughout this module

use actix_web::{web, HttpResponse, Responder};
use seesaw_viz::{
    SpanCollector, SpanObserver, JsonDiffFormatter, MermaidRenderer, RenderOptions,
};
use std::sync::Arc;

/// Global span collector (initialized once at startup)
/// Replace `YourStateType` with your actual state type
pub static SPAN_COLLECTOR: once_cell::sync::Lazy<Arc<SpanCollector<YourStateType>>> =
    once_cell::sync::Lazy::new(|| {
        Arc::new(SpanCollector::new(1000)) // 1000 event buffer
    });

/// Create the observer effect for Seesaw's on_any()
pub fn create_seesaw_viz_observer() -> SpanObserver<YourStateType, JsonDiffFormatter> {
    SPAN_COLLECTOR.create_observer(JsonDiffFormatter)
}

/// GET /seesaw - Live workflow visualization page
pub async fn seesaw_viz_handler() -> impl Responder {
    HttpResponse::Ok()
        .content_type("text/html")
        .body(include_str!("<path-to>/seesaw-viz/static/index.html"))
    // Alternatively, copy static/index.html to your project and use:
    // .body(include_str!("../static/seesaw_viz.html"))
}

/// GET /seesaw/api/stats - Span collection statistics
pub async fn seesaw_stats_handler() -> impl Responder {
    let stats = SPAN_COLLECTOR.stats().await;
    web::Json(stats)
}

/// GET /seesaw/api/graph - Full span graph (JSON)
pub async fn seesaw_graph_handler() -> impl Responder {
    let graph = SPAN_COLLECTOR.graph().await;
    web::Json(graph)
}

/// GET /seesaw/api/diagram - Mermaid diagram
pub async fn seesaw_diagram_handler() -> impl Responder {
    let graph = SPAN_COLLECTOR.graph().await;

    let renderer = MermaidRenderer::new(RenderOptions {
        group_by_component: true, // P2: Component subgraphs
        show_timings: true,
        show_state_diffs: false, // Too verbose for live view
        direction: "TD".into(),
        ..Default::default()
    });

    let diagram = renderer.render(&graph);

    web::Json(serde_json::json!({
        "diagram": diagram,
        "stats": SPAN_COLLECTOR.stats().await,
    }))
}

/// POST /seesaw/api/clear - Clear all collected spans
pub async fn seesaw_clear_handler() -> impl Responder {
    SPAN_COLLECTOR.clear().await.ok();
    HttpResponse::Ok().body("Spans cleared")
}
```

## Step 3: Attach Observer to Seesaw

In your Seesaw engine setup (e.g., where you create your `Engine` or `DomainService`):

```rust
use your_module::seesaw_viz;

// Create the observer once
let viz_observer = seesaw_viz::create_seesaw_viz_observer();

// Add to your engine
let engine = Engine::with_deps(deps)
    // ... your existing effects ...

    // Add visualization observer (P1: non-blocking async channel)
    .with_handler(
        effect::on_any().then(move |event, ctx| {
            let observer = viz_observer.clone();
            async move {
                // Record span off the hot path
                observer.record(
                    event.id,
                    event.type_name(),
                    event.type_id(),
                    event.parent_event_id,
                    Some(module_path!().to_string()), // P2: Component grouping
                    ctx.effect_name(), // Current effect name
                    Some(ctx.prev_state().clone()),
                    Some(ctx.next_state().clone()),
                ).await.ok();

                Ok(()) // Observer never fails the effect
            }
        })
    );
```

## Step 4: Add Routes to HTTP Server

In your Actix-web server setup:

```rust
use actix_web::{web, App, HttpServer};
use your_module::seesaw_viz;

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    HttpServer::new(move || {
        App::new()
            // ... your existing routes ...

            // Seesaw visualization routes
            .service(web::resource("/seesaw")
                .route(web::get().to(seesaw_viz::seesaw_viz_handler)))
            .service(web::resource("/seesaw/api/stats")
                .route(web::get().to(seesaw_viz::seesaw_stats_handler)))
            .service(web::resource("/seesaw/api/graph")
                .route(web::get().to(seesaw_viz::seesaw_graph_handler)))
            .service(web::resource("/seesaw/api/diagram")
                .route(web::get().to(seesaw_viz::seesaw_diagram_handler)))
            .service(web::resource("/seesaw/api/clear")
                .route(web::post().to(seesaw_viz::seesaw_clear_handler)))
    })
    .bind(("127.0.0.1", 8080))?
    .run()
    .await
}
```

## Step 5: Test the Integration

1. Start your server:
   ```bash
   cargo run --bin <your-binary-name>
   ```

2. Open the visualization:
   ```
   http://localhost:<your-port>/seesaw
   ```

3. Trigger some events by using your API (GraphQL mutations, WebSocket messages, etc.)

4. Watch the live diagram update!

## Performance Characteristics

- **Zero engine impact**: Span collection happens via async channel (P1)
- **Sampling available**: Use `with_sampler(RateSample::new(10))` for high-traffic systems
- **Memory bounded**: Set `max_spans: Some(10_000)` in config
- **Component grouping**: Mermaid subgraphs prevent spaghetti (P2)

## Troubleshooting

### No events showing up?

Check that `ctx.effect_name()` is being set properly. You may need to pass effect names explicitly if they're not available in context.

### Diagram too large?

Use sampling:
```rust
SPAN_COLLECTOR.with_sampler(EventTypeSampler::new(vec![
    "Order".into(),
    "Payment".into(),
]))
```

### Need delayed event visualization?

For timer-triggered events, the causality chain may show temporal gaps. The diagram will show these as normal edges - dashed edges for temporal breaks are a future enhancement.

## Next Steps

- Add WebSocket endpoint for real-time updates (no page refresh)
- Add filtering controls (by event type, time range)
- Add trace context propagation display
- Export diagrams as SVG/PNG

## Compatibility

✅ **Actix-web**: 4.x
✅ **Seesaw**: v0.7.x+
✅ **Tokio**: 1.x
✅ **Requires**: `module_path!()` for component grouping (optional but recommended)
