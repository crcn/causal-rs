//! Inspector Demo — a content ingestion pipeline that showcases branching,
//! convergence, retry/recovery, and phase tracking, wired to the causal
//! inspector UI.
//!
//! The pipeline (one article in flight at a time):
//!   ArticleSubmitted
//!     ├── extract_metadata   → MetadataExtracted
//!     ├── analyze_sentiment  → SentimentAnalyzed
//!     └── check_plagiarism   → PlagiarismChecked   (fails once, recovers on retry)
//!           └── (all three converge) → enrich_article → ArticleEnriched
//!                 ├── generate_summary → SummaryGenerated
//!                 └── tag_categories   → CategoriesTagged
//!                       └── (both converge) → publish_article → ArticlePublished
//!                             └── notify_subscribers → SubscribersNotified
//!
//! ```
//! (cd ../../modules/causal-inspector-ui && npm install && npm run build)
//! (cd ui && npm install && npm run build)        # build the inspector UI once
//! docker compose up -d                            # start KurrentDB + Postgres
//! cargo run                                       # or: ../../dev.sh example run inspector-demo
//! ```
//!
//! Then open:
//!   - http://localhost:4000/causal — Inspector UI
//!   - http://localhost:4000        — GraphQL playground
//!
//! KurrentDB is the durable source of truth. Postgres is a best-effort
//! observability store: `PgEventProjector` mirrors the event log into PG
//! `causal_log`, and `PgReactorObserver` records reactor execution, logs,
//! descriptions, and aggregate snapshots. The inspector reads only from PG
//! (`PgInspectorReadModel`), so any box in a fleet can serve it.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use axum::routing::get;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::broadcast;
use tower_http::services::{ServeDir, ServeFile};
use uuid::Uuid;

use causal::aggregate::{Aggregate, Apply};
use causal::aggregator::Aggregator;
use causal::types::LogLevel;
use causal::{
    CheckpointStore, Ctx, EngineBuilder, Event, EventLogBackend, Events, Reactor,
    ReactorCheckpoint,
};
use causal_inspector::{router, EventDisplay, InspectorReadModel, StoredEvent};
use causal_replay::{
    KurrentEventLogBackend, PgEventProjector, PgInspectorReadModel, PgReactorCheckpoint,
    PgReactorObserver,
};
use sqlx::postgres::PgPoolOptions;

// ── Events ──────────────────────────────────────────────────────────
//
// Each fact declares its kind (flat, exact — the wire event_type) and
// which article it is about. A single multi-event aggregate
// (`PipelineState`, keyed by article_id) folds all of them.

macro_rules! pipeline_event {
    ($name:ident, $kind:literal, { $($field:ident : $ty:ty),* $(,)? }) => {
        #[derive(Debug, Clone, Serialize, Deserialize)]
        struct $name {
            article_id: Uuid,
            $($field: $ty),*
        }
        impl Event for $name {
            const NAME: &'static str = $kind;
            fn subject_id(&self) -> Uuid { self.article_id }
        }
    };
}

pipeline_event!(ArticleSubmitted, "article_submitted", { title: String, author: String, body: String });
pipeline_event!(MetadataExtracted, "metadata_extracted", { word_count: u32, reading_time_mins: u32, language: String });
pipeline_event!(SentimentAnalyzed, "sentiment_analyzed", { score: f64, dominant_emotion: String });
pipeline_event!(PlagiarismChecked, "plagiarism_checked", { similarity: f64, is_original: bool });
pipeline_event!(ArticleEnriched, "article_enriched", {});
pipeline_event!(SummaryGenerated, "summary_generated", { summary: String });
pipeline_event!(CategoriesTagged, "categories_tagged", { categories: Vec<String> });
pipeline_event!(ArticlePublished, "article_published", { slug: String });
pipeline_event!(SubscribersNotified, "subscribers_notified", { count: u32 });

// ── Per-article aggregate: pipeline phase tracking ──────────────────
//
// Keyed by article_id — each article folds its own phase state from its
// own subject histories. (The pre-0.10 version used a nil-keyed global
// singleton; that fan-in shape is exactly the lying default 0.10
// removed, and it would have raced once articles overlapped.)

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PipelineState {
    completed_analyses: HashSet<String>,
    completed_publishing: HashSet<String>,
}

impl Aggregate for PipelineState {
    const NAME: &'static str = "PipelineState";
}

impl Apply<MetadataExtracted> for PipelineState {
    fn apply(&mut self, _: &MetadataExtracted) {
        self.completed_analyses.insert("metadata".into());
    }
}
impl Apply<SentimentAnalyzed> for PipelineState {
    fn apply(&mut self, _: &SentimentAnalyzed) {
        self.completed_analyses.insert("sentiment".into());
    }
}
impl Apply<PlagiarismChecked> for PipelineState {
    fn apply(&mut self, _: &PlagiarismChecked) {
        self.completed_analyses.insert("plagiarism".into());
    }
}
impl Apply<ArticleEnriched> for PipelineState {
    fn apply(&mut self, _: &ArticleEnriched) {
        // Enrichment converges the analysis phase and opens publishing.
        self.completed_analyses.clear();
        self.completed_publishing.clear();
    }
}
impl Apply<SummaryGenerated> for PipelineState {
    fn apply(&mut self, _: &SummaryGenerated) {
        self.completed_publishing.insert("summary".into());
    }
}
impl Apply<CategoriesTagged> for PipelineState {
    fn apply(&mut self, _: &CategoriesTagged) {
        self.completed_publishing.insert("categories".into());
    }
}

// In a reactor, `ctx.state_of` is a fold of the article's histories
// BOUNDED AT THE TRIGGER — deterministic under any concurrency. The
// convergence gates below lean on exactly that: only the reactor whose
// trigger IS the last-arriving analysis sees all-complete, so exactly
// one emits (and the deterministic output id makes a redelivered
// duplicate a no-op anyway).
async fn analyses_complete(ctx: &Ctx<'_>, article_id: Uuid) -> Result<bool> {
    let a = ctx.state_of::<PipelineState>(article_id).await?;
    let done = &a.curr.completed_analyses;
    Ok(done.contains("metadata") && done.contains("sentiment") && done.contains("plagiarism"))
}
async fn publishing_complete(ctx: &Ctx<'_>, article_id: Uuid) -> Result<bool> {
    let p = ctx.state_of::<PipelineState>(article_id).await?;
    let done = &p.curr.completed_publishing;
    Ok(done.contains("summary") && done.contains("categories"))
}

// ── Reactors ────────────────────────────────────────────────────────
//
// Phase 1: three parallel analyses fan out from ArticleSubmitted.

struct ExtractMetadata;
#[async_trait]
impl Reactor for ExtractMetadata {
    type Trigger = ArticleSubmitted;
    const NAME: &'static str = "extract_metadata";
    async fn react(&self, t: &ArticleSubmitted, ctx: Ctx<'_>) -> Result<Events> {
        ctx.log(LogLevel::Info, "Extracting metadata");
        tokio::time::sleep(Duration::from_millis(80)).await;
        let words = t.body.split_whitespace().count() as u32;
        ctx.log_with_data(
            LogLevel::Info,
            "Metadata extracted",
            Some(json!({ "word_count": words })),
        );
        Ok(Events::new().add(MetadataExtracted {
            article_id: t.article_id,
            word_count: words,
            reading_time_mins: (words / 200).max(1),
            language: "en".into(),
        }))
    }
}

struct AnalyzeSentiment;
#[async_trait]
impl Reactor for AnalyzeSentiment {
    type Trigger = ArticleSubmitted;
    const NAME: &'static str = "analyze_sentiment";
    async fn react(&self, t: &ArticleSubmitted, ctx: Ctx<'_>) -> Result<Events> {
        ctx.log(LogLevel::Info, "Analyzing sentiment");
        tokio::time::sleep(Duration::from_millis(150)).await;
        let score = (t.title.len() as f64 * 0.07).min(0.95);
        Ok(Events::new().add(SentimentAnalyzed {
            article_id: t.article_id,
            score,
            dominant_emotion: if score > 0.6 { "enthusiasm" } else { "neutral" }.into(),
        }))
    }
}

/// Fails the *first* attempt for ~1/3 of articles, then recovers on retry —
/// shows the inspector's reactor-attempt / error panes without stalling.
#[derive(Default)]
struct CheckPlagiarism {
    failed_once: Mutex<HashSet<Uuid>>,
}
#[async_trait]
impl Reactor for CheckPlagiarism {
    type Trigger = ArticleSubmitted;
    const NAME: &'static str = "check_plagiarism";
    async fn react(&self, t: &ArticleSubmitted, ctx: Ctx<'_>) -> Result<Events> {
        ctx.log(LogLevel::Info, "Checking plagiarism");
        tokio::time::sleep(Duration::from_millis(200)).await;

        let unlucky = t.article_id.as_bytes()[0] % 3 == 0;
        if unlucky {
            let first_time = self.failed_once.lock().unwrap().insert(t.article_id);
            if first_time {
                ctx.log(LogLevel::Warn, "[simulated] plagiarism API 503 — retrying");
                // Deliberate first-attempt failure to demonstrate the
                // inspector's retry / outcome panes; the next attempt
                // succeeds. A 503 is TRANSIENT — declared, so the
                // runner waits it out instead of spending the bounded
                // domain budget (0.10 retry taxonomy).
                return Err(causal::transient(anyhow::anyhow!(
                    "[simulated] plagiarism 503 for article {} (demo retry — succeeds next attempt)",
                    &t.article_id.to_string()[..8]
                )));
            }
        }

        let similarity = 0.05 + (t.body.len() as f64 * 0.001).min(0.15);
        ctx.log_with_data(
            LogLevel::Info,
            "Plagiarism check complete",
            Some(json!({ "similarity": similarity, "is_original": similarity < 0.3 })),
        );
        Ok(Events::new().add(PlagiarismChecked {
            article_id: t.article_id,
            similarity,
            is_original: similarity < 0.3,
        }))
    }
}

// Convergence gate: enrich once all three analyses are done. One reactor per
// analysis event type; only the last-arriving one sees all-complete and emits.
// `ArticleEnriched`'s deterministic id makes a duplicate a no-op (dedup on append).

async fn enrich(t_article_id: Uuid, ctx: &Ctx<'_>) -> Result<Events> {
    if analyses_complete(ctx, t_article_id).await? {
        ctx.log(LogLevel::Info, "All analyses complete — enriching article");
        Ok(Events::new().add(ArticleEnriched { article_id: t_article_id }))
    } else {
        Ok(Events::new())
    }
}

struct EnrichOnMetadata;
#[async_trait]
impl Reactor for EnrichOnMetadata {
    type Trigger = MetadataExtracted;
    const NAME: &'static str = "enrich_article:on_metadata";
    async fn react(&self, t: &MetadataExtracted, ctx: Ctx<'_>) -> Result<Events> {
        enrich(t.article_id, &ctx).await
    }
}
struct EnrichOnSentiment;
#[async_trait]
impl Reactor for EnrichOnSentiment {
    type Trigger = SentimentAnalyzed;
    const NAME: &'static str = "enrich_article:on_sentiment";
    async fn react(&self, t: &SentimentAnalyzed, ctx: Ctx<'_>) -> Result<Events> {
        enrich(t.article_id, &ctx).await
    }
}
struct EnrichOnPlagiarism;
#[async_trait]
impl Reactor for EnrichOnPlagiarism {
    type Trigger = PlagiarismChecked;
    const NAME: &'static str = "enrich_article:on_plagiarism";
    async fn react(&self, t: &PlagiarismChecked, ctx: Ctx<'_>) -> Result<Events> {
        enrich(t.article_id, &ctx).await
    }
}

// Phase 2: two parallel publishing steps fan out from ArticleEnriched.

struct GenerateSummary;
#[async_trait]
impl Reactor for GenerateSummary {
    type Trigger = ArticleEnriched;
    const NAME: &'static str = "generate_summary";
    async fn react(&self, t: &ArticleEnriched, ctx: Ctx<'_>) -> Result<Events> {
        ctx.log(LogLevel::Info, "Generating summary");
        tokio::time::sleep(Duration::from_millis(120)).await;
        Ok(Events::new().add(SummaryGenerated {
            article_id: t.article_id,
            summary: "An insightful article exploring the latest developments \
                      with practical takeaways for practitioners"
                .into(),
        }))
    }
}

struct TagCategories;
#[async_trait]
impl Reactor for TagCategories {
    type Trigger = ArticleEnriched;
    const NAME: &'static str = "tag_categories";
    async fn react(&self, t: &ArticleEnriched, ctx: Ctx<'_>) -> Result<Events> {
        ctx.log(LogLevel::Info, "Tagging categories");
        tokio::time::sleep(Duration::from_millis(90)).await;
        Ok(Events::new().add(CategoriesTagged {
            article_id: t.article_id,
            categories: vec!["Technology".into(), "Engineering".into(), "Best Practices".into()],
        }))
    }
}

// Convergence gate: publish once summary + categories are both done.

async fn publish(article_id: Uuid, ctx: &Ctx<'_>) -> Result<Events> {
    if publishing_complete(ctx, article_id).await? {
        let slug = format!("article-{}", &article_id.to_string()[..8]);
        ctx.log_with_data(LogLevel::Info, "Publishing article", Some(json!({ "slug": slug })));
        Ok(Events::new().add(ArticlePublished { article_id, slug }))
    } else {
        Ok(Events::new())
    }
}

struct PublishOnSummary;
#[async_trait]
impl Reactor for PublishOnSummary {
    type Trigger = SummaryGenerated;
    const NAME: &'static str = "publish_article:on_summary";
    async fn react(&self, t: &SummaryGenerated, ctx: Ctx<'_>) -> Result<Events> {
        publish(t.article_id, &ctx).await
    }
}
struct PublishOnCategories;
#[async_trait]
impl Reactor for PublishOnCategories {
    type Trigger = CategoriesTagged;
    const NAME: &'static str = "publish_article:on_categories";
    async fn react(&self, t: &CategoriesTagged, ctx: Ctx<'_>) -> Result<Events> {
        publish(t.article_id, &ctx).await
    }
}

// Final: notify subscribers once published.

struct NotifySubscribers;
#[async_trait]
impl Reactor for NotifySubscribers {
    type Trigger = ArticlePublished;
    const NAME: &'static str = "notify_subscribers";
    async fn react(&self, t: &ArticlePublished, ctx: Ctx<'_>) -> Result<Events> {
        ctx.log(LogLevel::Info, "Notifying subscribers");
        tokio::time::sleep(Duration::from_millis(60)).await;
        Ok(Events::new().add(SubscribersNotified { article_id: t.article_id, count: 42 }))
    }
}

// ── Inspector wiring ────────────────────────────────────────────────

#[derive(Clone)]
struct DemoEventDisplay;

/// Event kinds are flat since 0.10 — the wire `event_type` IS the name.
fn slug(event_type: &str) -> &str {
    event_type
}

impl EventDisplay for DemoEventDisplay {
    fn display_name(&self, event_type: &str, _payload: &serde_json::Value) -> String {
        slug(event_type)
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
        match slug(event_type) {
            "article_submitted" => Some(payload.get("title")?.as_str()?.to_string()),
            "metadata_extracted" => {
                Some(format!("{} words", payload.get("word_count")?.as_u64()?))
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
            "article_enriched" => Some("All analyses complete".into()),
            "summary_generated" => {
                Some(payload.get("summary")?.as_str()?.chars().take(60).collect::<String>() + "…")
            }
            "categories_tagged" => {
                let tags = payload.get("categories")?.as_array()?;
                Some(tags.iter().filter_map(|t| t.as_str()).collect::<Vec<_>>().join(", "))
            }
            "article_published" => Some(payload.get("slug")?.as_str()?.to_string()),
            "subscribers_notified" => {
                Some(format!("{} subscribers", payload.get("count")?.as_u64()?))
            }
            _ => None,
        }
    }
}

async fn graphiql() -> axum::response::Html<String> {
    axum::response::Html(
        async_graphql::http::GraphiQLSource::build()
            .endpoint("/")
            .subscription_endpoint("/ws")
            .finish(),
    )
}

// ── Main ────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_target(false).init();

    // KurrentDB is the durable source of truth. Postgres is a best-effort
    // observability store: a background projector mirrors the event log into
    // `causal_log`, and `PgReactorObserver` records reactor execution/logs/
    // descriptions + aggregate snapshots. The inspector reads only from PG, so
    // any box in a fleet can serve it (`PgInspectorReadModel`).
    let kurrent_url = std::env::var("KURRENT_URL")
        .unwrap_or_else(|_| "kurrentdb://localhost:2113?tls=false".to_string());
    let kurrent = match KurrentEventLogBackend::connect(&kurrent_url) {
        Ok(k) => k,
        Err(e) => {
            eprintln!("could not connect to KurrentDB at {kurrent_url}: {e}");
            eprintln!();
            eprintln!("start it with:  docker compose up -d");
            eprintln!("          (or:  ../../dev.sh example run inspector-demo)");
            std::process::exit(1);
        }
    };
    let log = Arc::new(kurrent) as Arc<dyn EventLogBackend>;

    let database_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://causal:causal@localhost:54330/causal".to_string());
    let pool = match PgPoolOptions::new().max_connections(8).connect(&database_url).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("could not connect to Postgres at {database_url}: {e}");
            eprintln!();
            eprintln!("start it with:  docker compose up -d");
            eprintln!("          (or:  ../../dev.sh example run inspector-demo)");
            std::process::exit(1);
        }
    };

    let checkpoint = Arc::new(PgReactorCheckpoint::new(pool.clone()));
    let observer = Arc::new(PgReactorObserver::new(pool.clone()));

    // Project KurrentDB's $all into PG `causal_log` (idempotent, off the hot path)
    // so the inspector's event/flow views have data.
    PgEventProjector::spawn(
        log.clone(),
        checkpoint.clone() as Arc<dyn CheckpointStore>,
        pool.clone(),
    );

    // Live event feed for the inspector: poll PG for new events and broadcast.
    let (inspector_tx, _) = broadcast::channel::<StoredEvent>(1024);
    {
        let read = PgInspectorReadModel::new(pool.clone());
        let tx = inspector_tx.clone();
        tokio::spawn(async move {
            let mut next_seq: i64 = 0;
            loop {
                if let Ok(events) = read.events_from_seq(next_seq, 256).await {
                    for e in events {
                        next_seq = e.seq + 1;
                        let _ = tx.send(e);
                    }
                }
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
        });
    }

    // Build the engine: per-article phase-tracking aggregate + the reactor pipeline.
    let engine = EngineBuilder::new(
        log.clone(),
        checkpoint.clone() as Arc<dyn CheckpointStore>,
        checkpoint.clone() as Arc<dyn ReactorCheckpoint>,
    )
    .with_observer(observer)
    .with_aggregators(vec![
        Aggregator::for_type::<PipelineState, MetadataExtracted>(),
        Aggregator::for_type::<PipelineState, SentimentAnalyzed>(),
        Aggregator::for_type::<PipelineState, PlagiarismChecked>(),
        Aggregator::for_type::<PipelineState, ArticleEnriched>(),
        Aggregator::for_type::<PipelineState, SummaryGenerated>(),
        Aggregator::for_type::<PipelineState, CategoriesTagged>(),
    ])
    .with_reactor(ExtractMetadata)
    .with_reactor(AnalyzeSentiment)
    .with_reactor(CheckPlagiarism::default())
    .with_reactor(EnrichOnMetadata)
    .with_reactor(EnrichOnSentiment)
    .with_reactor(EnrichOnPlagiarism)
    .with_reactor(GenerateSummary)
    .with_reactor(TagCategories)
    .with_reactor(PublishOnSummary)
    .with_reactor(PublishOnCategories)
    .with_reactor(NotifySubscribers)
    .build()
    .await?;

    // Inspector router (GraphQL + WS) + the bundled UI under /causal.
    let inspector = router(
        Arc::new(PgInspectorReadModel::new(pool.clone())) as Arc<dyn InspectorReadModel>,
        DemoEventDisplay,
        inspector_tx,
    );
    let ui_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("ui/dist");
    let app = inspector.route("/", get(graphiql)).nest_service(
        "/causal",
        ServeDir::new(&ui_dir).fallback(ServeFile::new(ui_dir.join("index.html"))),
    );

    // Generate articles, one fully-settled chain at a time.
    tokio::spawn(async move {
        let articles = [
            ("The Future of Event-Driven Architecture", "Alice Chen", "Event-driven systems are transforming how we build scalable applications. This article explores patterns for reactive systems including event sourcing, CQRS, and causal consistency models that enable resilient distributed systems."),
            ("Rust for Production Systems", "Bob Martinez", "Why Rust is becoming the language of choice for high-performance backend services. We examine memory safety, zero-cost abstractions, and the ecosystem maturity that makes Rust production-ready."),
            ("Observability Beyond Logging", "Charlie Park", "Modern observability requires more than log aggregation. We dive into distributed tracing, metrics correlation, and causal analysis for understanding complex system behaviors."),
            ("Building Resilient Pipelines", "Diana Okafor", "Data pipelines fail. The question is how gracefully. This guide covers retry strategies, dead letter queues, circuit breakers, and convergence gates for pipelines that self-heal."),
            ("The Art of System Design", "Eve Nakamura", "Good system design balances simplicity with scalability. We explore event-driven architectures, aggregate patterns, and the principle that state should be derived, not stored."),
        ];
        let mut i = 0;
        loop {
            let (title, author, body) = articles[i % articles.len()];
            let article_id = Uuid::new_v4();
            tracing::info!(%article_id, title, "Submitting article");
            if let Err(e) = engine
                .emit(ArticleSubmitted {
                    article_id,
                    title: title.into(),
                    author: author.into(),
                    body: body.into(),
                })
                .settled()
                .await
            {
                tracing::error!(error = %e, "Failed to process article");
            }
            i += 1;
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });

    tracing::info!("Inspector UI   → http://localhost:4000/causal");
    tracing::info!("GraphQL        → http://localhost:4000");
    let listener = tokio::net::TcpListener::bind("0.0.0.0:4000").await?;
    axum::serve(listener, app).await?;
    Ok(())
}
