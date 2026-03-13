//! Inspector Demo — a content ingestion pipeline that showcases branching,
//! convergence, and phase tracking with singleton aggregates.
//!
//! The pipeline:
//!   ArticleSubmitted
//!     ├── extract_metadata → MetadataExtracted
//!     ├── analyze_sentiment → SentimentAnalyzed
//!     └── check_plagiarism → PlagiarismChecked
//!           └── (all three converge) → enrich_article → ArticleEnriched
//!                 ├── generate_summary → SummaryGenerated
//!                 └── tag_categories → CategoriesTagged
//!                       └── (both converge) → publish_article → ArticlePublished
//!                             └── notify_subscribers → SubscribersNotified
//!
//! ```
//! cargo run -p inspector-demo
//! ```
//!
//! Then open:
//!   - http://localhost:4000/causal — Inspector UI
//!   - http://localhost:4000 — GraphQL playground

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Result;
use axum::routing::get;
use tower_http::services::ServeDir;
use tokio::sync::broadcast;

use causal::{
    event, events, reactor, Aggregate, Apply, Context, Engine, MemoryStore,
};
use causal_inspector::{
    InspectorReadModel, StoredEvent,
    display::EventDisplay,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ── Event display ────────────────────────────────────────────────

#[derive(Clone)]
struct DemoEventDisplay;

impl EventDisplay for DemoEventDisplay {
    fn display_name(&self, event_type: &str, _payload: &serde_json::Value) -> String {
        event_type
            .split('_')
            .map(|w| {
                let mut c = w.chars();
                match c.next() {
                    None => String::new(),
                    Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn summary(&self, event_type: &str, payload: &serde_json::Value) -> Option<String> {
        match event_type {
            "article_submitted" => {
                let title = payload.get("title")?.as_str()?;
                Some(title.to_string())
            }
            "metadata_extracted" => {
                let word_count = payload.get("word_count")?.as_u64()?;
                Some(format!("{word_count} words"))
            }
            "sentiment_analyzed" => {
                let score = payload.get("score")?.as_f64()?;
                let label = if score > 0.6 { "positive" } else if score < 0.4 { "negative" } else { "neutral" };
                Some(format!("{label} ({score:.2})"))
            }
            "plagiarism_checked" => {
                let similarity = payload.get("similarity")?.as_f64()?;
                Some(format!("{:.0}% original", (1.0 - similarity) * 100.0))
            }
            "article_enriched" => Some("All analyses complete".to_string()),
            "summary_generated" => {
                let summary = payload.get("summary")?.as_str()?;
                Some(summary.chars().take(60).collect::<String>() + "...")
            }
            "categories_tagged" => {
                let tags = payload.get("categories")?.as_array()?;
                let names: Vec<_> = tags.iter().filter_map(|t| t.as_str()).collect();
                Some(names.join(", "))
            }
            "article_published" => {
                let slug = payload.get("slug")?.as_str()?;
                Some(slug.to_string())
            }
            "subscribers_notified" => {
                let count = payload.get("count")?.as_u64()?;
                Some(format!("{count} subscribers"))
            }
            _ => None,
        }
    }
}

// ── Events ──────────────────────────────────────────────────────

#[event]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ArticleSubmitted {
    article_id: Uuid,
    title: String,
    author: String,
    body: String,
}

#[event]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct MetadataExtracted {
    article_id: Uuid,
    word_count: u32,
    reading_time_mins: u32,
    language: String,
}

#[event]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SentimentAnalyzed {
    article_id: Uuid,
    score: f64,
    dominant_emotion: String,
}

#[event]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PlagiarismChecked {
    article_id: Uuid,
    similarity: f64,
    is_original: bool,
}

#[event]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ArticleEnriched {
    article_id: Uuid,
}

#[event]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SummaryGenerated {
    article_id: Uuid,
    summary: String,
}

#[event]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CategoriesTagged {
    article_id: Uuid,
    categories: Vec<String>,
}

#[event]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ArticlePublished {
    article_id: Uuid,
    slug: String,
}

#[event]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SubscribersNotified {
    article_id: Uuid,
    count: u32,
}

// ── Singleton aggregate: pipeline phase tracking ────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PipelineState {
    /// Which analysis phases have completed for each article
    completed_analyses: HashSet<String>,
    /// Which publishing phases have completed for each article
    completed_publishing: HashSet<String>,
}

impl Aggregate for PipelineState {
    fn aggregate_type() -> &'static str {
        "pipeline_state"
    }
}

impl Apply<MetadataExtracted> for PipelineState {
    fn apply(&mut self, _event: MetadataExtracted) {
        self.completed_analyses.insert("metadata".to_string());
    }
}

impl Apply<SentimentAnalyzed> for PipelineState {
    fn apply(&mut self, _event: SentimentAnalyzed) {
        self.completed_analyses.insert("sentiment".to_string());
    }
}

impl Apply<PlagiarismChecked> for PipelineState {
    fn apply(&mut self, _event: PlagiarismChecked) {
        self.completed_analyses.insert("plagiarism".to_string());
    }
}

impl Apply<ArticleEnriched> for PipelineState {
    fn apply(&mut self, _event: ArticleEnriched) {
        self.completed_analyses.clear();
        self.completed_publishing.clear();
    }
}

impl Apply<SummaryGenerated> for PipelineState {
    fn apply(&mut self, _event: SummaryGenerated) {
        self.completed_publishing.insert("summary".to_string());
    }
}

impl Apply<CategoriesTagged> for PipelineState {
    fn apply(&mut self, _event: CategoriesTagged) {
        self.completed_publishing.insert("categories".to_string());
    }
}

// ── Dependencies ────────────────────────────────────────────────

#[derive(Clone)]
struct Deps;

// ── GraphiQL UI ─────────────────────────────────────────────────

async fn graphiql() -> axum::response::Html<String> {
    axum::response::Html(
        async_graphql::http::GraphiQLSource::build()
            .endpoint("/")
            .subscription_endpoint("/ws")
            .finish(),
    )
}

// ── Main ────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_target(false)
        .with_level(true)
        .init();

    // 1. Create store with broadcast
    let (store, _rx) = MemoryStore::with_broadcast(1024);
    let store = Arc::new(store);

    // 2. Bridge MemoryStore broadcast → inspector StoredEvent
    let (inspector_tx, _) = broadcast::channel::<StoredEvent>(1024);
    let bridge_tx = inspector_tx.clone();
    let mut store_rx = store.subscribe().unwrap();
    tokio::spawn(async move {
        loop {
            match store_rx.recv().await {
                Ok(persisted) => {
                    let stored = StoredEvent {
                        seq: persisted.position.raw() as i64,
                        ts: persisted.created_at,
                        event_type: persisted.event_type.clone(),
                        payload: persisted.payload.clone(),
                        id: Some(persisted.event_id),
                        parent_id: persisted.parent_id,
                        correlation_id: Some(persisted.correlation_id),
                        reactor_id: persisted
                            .metadata
                            .get("reactor_id")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string()),
                        aggregate_type: persisted
                            .metadata
                            .get("aggregate_type")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string()),
                        aggregate_id: persisted
                            .metadata
                            .get("aggregate_id")
                            .and_then(|v| v.as_str())
                            .and_then(|s| uuid::Uuid::parse_str(s).ok()),
                        stream_version: persisted
                            .metadata
                            .get("stream_version")
                            .and_then(|v| v.as_u64()),
                    };
                    let _ = bridge_tx.send(stored);
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(missed = n, "Inspector bridge lagged");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    // 3. Build the engine with branching + convergence pipeline
    let engine = Engine::new(Deps)
        .with_store(store.clone())
        // Register singleton aggregator for phase tracking
        .with_aggregator::<MetadataExtracted, PipelineState, _>(|_| Uuid::nil())
        .with_aggregator::<SentimentAnalyzed, PipelineState, _>(|_| Uuid::nil())
        .with_aggregator::<PlagiarismChecked, PipelineState, _>(|_| Uuid::nil())
        .with_aggregator::<ArticleEnriched, PipelineState, _>(|_| Uuid::nil())
        .with_aggregator::<SummaryGenerated, PipelineState, _>(|_| Uuid::nil())
        .with_aggregator::<CategoriesTagged, PipelineState, _>(|_| Uuid::nil())
        // ── Phase 1: Three parallel analysis branches ──
        .with_reactor(
            reactor::on::<ArticleSubmitted>()
                .id("extract_metadata")
                .then(|event, ctx: Context<Deps>| async move {
                    ctx.logger.info("Extracting metadata");
                    tokio::time::sleep(tokio::time::Duration::from_millis(80)).await;
                    let words: Vec<&str> = event.body.split_whitespace().collect();
                    ctx.logger.info_with("Metadata extracted", &serde_json::json!({
                        "word_count": words.len(),
                        "reading_time_mins": (words.len() / 200).max(1),
                    }));
                    Ok(events![MetadataExtracted {
                        article_id: event.article_id,
                        word_count: words.len() as u32,
                        reading_time_mins: (words.len() as u32 / 200).max(1),
                        language: "en".to_string(),
                    }])
                }),
        )
        .with_reactor(
            reactor::on::<ArticleSubmitted>()
                .id("analyze_sentiment")
                .then(|event, ctx: Context<Deps>| async move {
                    ctx.logger.info("Analyzing sentiment");
                    tokio::time::sleep(tokio::time::Duration::from_millis(150)).await;
                    let score = (event.title.len() as f64 * 0.07).min(0.95);
                    ctx.logger.info_with("Sentiment analyzed", &serde_json::json!({
                        "score": score,
                        "label": if score > 0.6 { "positive" } else if score < 0.4 { "negative" } else { "neutral" },
                    }));
                    Ok(events![SentimentAnalyzed {
                        article_id: event.article_id,
                        score,
                        dominant_emotion: if score > 0.6 { "enthusiasm" } else { "neutral" }.to_string(),
                    }])
                }),
        )
        .with_reactor(
            reactor::on::<ArticleSubmitted>()
                .id("check_plagiarism")
                .retry(3)
                .then(|event, ctx: Context<Deps>| async move {
                    ctx.logger.info("Checking plagiarism");
                    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

                    // ~30% chance of failure to demonstrate error handling
                    let hash = event.article_id.as_bytes()[0];
                    if hash % 3 == 0 {
                        ctx.logger.warn("Plagiarism API returned 503");
                        anyhow::bail!(
                            "Plagiarism service unavailable (upstream 503 for article {})",
                            &event.article_id.to_string()[..8]
                        );
                    }

                    let similarity = 0.05 + (event.body.len() as f64 * 0.001).min(0.15);
                    ctx.logger.info_with("Plagiarism check complete", &serde_json::json!({
                        "similarity": similarity,
                        "is_original": similarity < 0.3,
                    }));
                    Ok(events![PlagiarismChecked {
                        article_id: event.article_id,
                        similarity,
                        is_original: similarity < 0.3,
                    }])
                }),
        )
        // ── Convergence gate: fires only when all 3 analyses are done ──
        //    Each listener has a unique ID; the filter ensures only the
        //    last-arriving analysis actually triggers enrichment.
        .with_reactor(
            reactor::on::<MetadataExtracted>()
                .id("enrich_article:on_metadata")
                .filter(|_event, ctx: &Context<Deps>| {
                    let state = ctx.aggregate::<PipelineState>();
                    let analyses = &state.curr.completed_analyses;
                    analyses.contains("metadata")
                        && analyses.contains("sentiment")
                        && analyses.contains("plagiarism")
                })
                .describe(|ctx: &Context<Deps>| {
                    let state = ctx.aggregate::<PipelineState>();
                    let analyses = &state.curr.completed_analyses;
                    serde_json::json!([{
                        "type": "checklist",
                        "label": "Analyses",
                        "items": [
                            { "text": "metadata", "done": analyses.contains("metadata") },
                            { "text": "sentiment", "done": analyses.contains("sentiment") },
                            { "text": "plagiarism", "done": analyses.contains("plagiarism") }
                        ]
                    }])
                })
                .then(|event, ctx: Context<Deps>| async move {
                    ctx.logger.info("All analyses complete — enriching article");
                    Ok(events![ArticleEnriched {
                        article_id: event.article_id,
                    }])
                }),
        )
        .with_reactor(
            reactor::on::<SentimentAnalyzed>()
                .id("enrich_article:on_sentiment")
                .filter(|_event, ctx: &Context<Deps>| {
                    let state = ctx.aggregate::<PipelineState>();
                    let analyses = &state.curr.completed_analyses;
                    analyses.contains("metadata")
                        && analyses.contains("sentiment")
                        && analyses.contains("plagiarism")
                })
                .describe(|ctx: &Context<Deps>| {
                    let state = ctx.aggregate::<PipelineState>();
                    let analyses = &state.curr.completed_analyses;
                    serde_json::json!([{
                        "type": "checklist",
                        "label": "Analyses",
                        "items": [
                            { "text": "metadata", "done": analyses.contains("metadata") },
                            { "text": "sentiment", "done": analyses.contains("sentiment") },
                            { "text": "plagiarism", "done": analyses.contains("plagiarism") }
                        ]
                    }])
                })
                .then(|event, ctx: Context<Deps>| async move {
                    ctx.logger.info("All analyses complete — enriching article");
                    Ok(events![ArticleEnriched {
                        article_id: event.article_id,
                    }])
                }),
        )
        .with_reactor(
            reactor::on::<PlagiarismChecked>()
                .id("enrich_article:on_plagiarism")
                .filter(|_event, ctx: &Context<Deps>| {
                    let state = ctx.aggregate::<PipelineState>();
                    let analyses = &state.curr.completed_analyses;
                    analyses.contains("metadata")
                        && analyses.contains("sentiment")
                        && analyses.contains("plagiarism")
                })
                .describe(|ctx: &Context<Deps>| {
                    let state = ctx.aggregate::<PipelineState>();
                    let analyses = &state.curr.completed_analyses;
                    serde_json::json!([{
                        "type": "checklist",
                        "label": "Analyses",
                        "items": [
                            { "text": "metadata", "done": analyses.contains("metadata") },
                            { "text": "sentiment", "done": analyses.contains("sentiment") },
                            { "text": "plagiarism", "done": analyses.contains("plagiarism") }
                        ]
                    }])
                })
                .then(|event, ctx: Context<Deps>| async move {
                    ctx.logger.info("All analyses complete — enriching article");
                    Ok(events![ArticleEnriched {
                        article_id: event.article_id,
                    }])
                }),
        )
        // ── Phase 2: Two parallel publishing branches ──
        .with_reactor(
            reactor::on::<ArticleEnriched>()
                .id("generate_summary")
                .then(|event, ctx: Context<Deps>| async move {
                    ctx.logger.info("Generating summary");
                    tokio::time::sleep(tokio::time::Duration::from_millis(120)).await;
                    ctx.logger.debug("Summary generation complete");
                    Ok(events![SummaryGenerated {
                        article_id: event.article_id,
                        summary: "An insightful article exploring the latest developments in the field with practical takeaways for practitioners".to_string(),
                    }])
                }),
        )
        .with_reactor(
            reactor::on::<ArticleEnriched>()
                .id("tag_categories")
                .then(|event, ctx: Context<Deps>| async move {
                    ctx.logger.info("Tagging categories");
                    tokio::time::sleep(tokio::time::Duration::from_millis(90)).await;
                    ctx.logger.info_with("Categories assigned", &serde_json::json!({
                        "count": 3,
                    }));
                    Ok(events![CategoriesTagged {
                        article_id: event.article_id,
                        categories: vec!["Technology".to_string(), "Engineering".to_string(), "Best Practices".to_string()],
                    }])
                }),
        )
        // ── Convergence gate: fires when summary + categories are done ──
        .with_reactor(
            reactor::on::<SummaryGenerated>()
                .id("publish_article:on_summary")
                .filter(|_event, ctx: &Context<Deps>| {
                    let state = ctx.aggregate::<PipelineState>();
                    let publishing = &state.curr.completed_publishing;
                    publishing.contains("summary") && publishing.contains("categories")
                })
                .describe(|ctx: &Context<Deps>| {
                    let state = ctx.aggregate::<PipelineState>();
                    let publishing = &state.curr.completed_publishing;
                    serde_json::json!([{
                        "type": "checklist",
                        "label": "Publishing",
                        "items": [
                            { "text": "summary", "done": publishing.contains("summary") },
                            { "text": "categories", "done": publishing.contains("categories") }
                        ]
                    }])
                })
                .then(|event, ctx: Context<Deps>| async move {
                    let slug = format!("article-{}", &event.article_id.to_string()[..8]);
                    ctx.logger.info_with("Publishing article", &serde_json::json!({
                        "slug": &slug,
                    }));
                    Ok(events![ArticlePublished {
                        article_id: event.article_id,
                        slug,
                    }])
                }),
        )
        .with_reactor(
            reactor::on::<CategoriesTagged>()
                .id("publish_article:on_categories")
                .filter(|_event, ctx: &Context<Deps>| {
                    let state = ctx.aggregate::<PipelineState>();
                    let publishing = &state.curr.completed_publishing;
                    publishing.contains("summary") && publishing.contains("categories")
                })
                .describe(|ctx: &Context<Deps>| {
                    let state = ctx.aggregate::<PipelineState>();
                    let publishing = &state.curr.completed_publishing;
                    serde_json::json!([{
                        "type": "checklist",
                        "label": "Publishing",
                        "items": [
                            { "text": "summary", "done": publishing.contains("summary") },
                            { "text": "categories", "done": publishing.contains("categories") }
                        ]
                    }])
                })
                .then(|event, ctx: Context<Deps>| async move {
                    let slug = format!("article-{}", &event.article_id.to_string()[..8]);
                    ctx.logger.info_with("Publishing article", &serde_json::json!({
                        "slug": &slug,
                    }));
                    Ok(events![ArticlePublished {
                        article_id: event.article_id,
                        slug,
                    }])
                }),
        )
        // ── Final: notify subscribers ──
        .with_reactor(
            reactor::on::<ArticlePublished>()
                .id("notify_subscribers")
                .then(|event, ctx: Context<Deps>| async move {
                    ctx.logger.info("Notifying subscribers");
                    tokio::time::sleep(tokio::time::Duration::from_millis(60)).await;
                    ctx.logger.info_with("Subscribers notified", &serde_json::json!({
                        "count": 42,
                    }));
                    Ok(events![SubscribersNotified {
                        article_id: event.article_id,
                        count: 42,
                    }])
                }),
        );

    // 4. Build inspector router (GraphQL + WebSocket)
    let inspector = causal_inspector::router(
        store.clone() as Arc<dyn InspectorReadModel>,
        DemoEventDisplay,
        inspector_tx,
    );

    // 5. Start HTTP server
    let ui_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("ui/dist");
    let app = inspector
        .route("/", get(graphiql))
        .nest_service(
            "/causal",
            ServeDir::new(&ui_dir)
                .fallback(tower_http::services::ServeFile::new(ui_dir.join("index.html"))),
        );

    // 6. Spawn article generation
    let engine = Arc::new(engine);
    let engine_clone = engine.clone();
    tokio::spawn(async move {
        let articles = [
            ("The Future of Event-Driven Architecture", "Alice Chen", "Event-driven systems are transforming how we build scalable applications. This article explores patterns for reactive systems including event sourcing, CQRS, and causal consistency models that enable teams to build resilient distributed systems."),
            ("Rust for Production Systems", "Bob Martinez", "Why Rust is becoming the language of choice for high-performance backend services. We examine memory safety guarantees, zero-cost abstractions, and the ecosystem maturity that makes Rust production-ready."),
            ("Observability Beyond Logging", "Charlie Park", "Modern observability requires more than log aggregation. We dive into distributed tracing, metrics correlation, and the emerging practice of causal analysis for understanding complex system behaviors."),
            ("Building Resilient Pipelines", "Diana Okafor", "Data pipelines fail. The question is how gracefully. This guide covers retry strategies, dead letter queues, circuit breakers, and convergence gates for building pipelines that self-heal."),
            ("The Art of System Design", "Eve Nakamura", "Good system design balances simplicity with scalability. We explore event-driven architectures, aggregate patterns, and the principle that state should be derived, not stored."),
        ];
        let mut i = 0;
        loop {
            let (title, author, body) = articles[i % articles.len()];
            let article_id = Uuid::new_v4();

            tracing::info!(article_id = %article_id, title, "Submitting article");

            if let Err(e) = engine_clone
                .emit(ArticleSubmitted {
                    article_id,
                    title: title.to_string(),
                    author: author.to_string(),
                    body: body.to_string(),
                })
                .settled()
                .await
            {
                tracing::error!(error = %e, "Failed to process article");
            }

            i += 1;
            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
        }
    });

    let addr = "0.0.0.0:4000";
    tracing::info!("Inspector UI at http://localhost:4000/causal");
    tracing::info!("GraphQL playground at http://localhost:4000");
    tracing::info!("WebSocket subscriptions at ws://localhost:4000/ws");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
