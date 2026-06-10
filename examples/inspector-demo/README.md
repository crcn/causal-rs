# inspector-demo

A content-ingestion pipeline wired to the **causal inspector** — a GraphQL API
plus a React UI that visualizes the event flow live, on `:4000`.

**KurrentDB** is the durable event log and the source of truth. **Postgres** is
a best-effort observability store that backs the inspector: `PgEventProjector`
mirrors the Kurrent `$all` log into PG `causal_log`, and `PgReactorObserver`
records reactor executions, logs, descriptions, and aggregate snapshots. The
inspector reads only from Postgres (`PgInspectorReadModel`), so any box in a
fleet can serve it. This is the same KurrentDB + Postgres production shape as
the other examples, with the inspector attached.

## Run

Build the inspector UI once (the React component library, then this app's
bundle), then start KurrentDB + Postgres + the backend:

```bash
# 1. inspector UI → dist/  (one-time)
(cd ../../modules/causal-inspector-ui && npm install && npm run build)
(cd ui && npm install && npm run build)

# 2. KurrentDB + Postgres + the pipeline + inspector server
./dev.sh example run inspector-demo     # from the repo root: brings up the stack, then runs
#   ── or manually ──
# docker compose up -d                  # KurrentDB on :2113, Postgres on :54330
#                                       # (compose applies docs/schema.sql to PG on init)
# cargo run
```

Then open:
- **http://localhost:4000/causal** — the Inspector UI
- http://localhost:4000 — GraphQL playground (`/ws` for live subscriptions)

(The backend runs without the UI built — `/causal` just 404s until you build it;
`./dev.sh` warns you about this.)

## What it shows

The pipeline processes one article at a time:

```
ArticleSubmitted
  ├── extract_metadata   → MetadataExtracted
  ├── analyze_sentiment  → SentimentAnalyzed
  └── check_plagiarism   → PlagiarismChecked   (fails once for ~1/3, recovers on retry)
        └── (all three converge) → enrich_article → ArticleEnriched
              ├── generate_summary → SummaryGenerated
              └── tag_categories   → CategoriesTagged
                    └── (both converge) → publish_article → ArticlePublished
                          └── notify_subscribers → SubscribersNotified
```

- **Branching** — one event fans out to parallel reactors.
- **Convergence gates** — enrichment fires only when all three analyses are
  done; publish only when summary + categories are done. A singleton
  `PipelineState` aggregate tracks completion; each gate reads it via
  `ctx.aggregate::<PipelineState>()` and emits only when ready (the convergent
  event's deterministic id makes a duplicate a no-op).
- **Retry / recovery** — plagiarism returns a transient `503` on the first
  attempt for some articles, then succeeds on retry — visible in the inspector's
  reactor-attempt / log panes.
