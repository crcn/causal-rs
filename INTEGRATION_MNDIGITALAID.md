# Integrating Seesaw v0.8.1 + Insight into mndigitalaid

## Current State (v0.7.8)

mndigitalaid is currently using:
- **Seesaw v0.7.8** - In-memory engine
- **seesaw-viz** - Mermaid diagram visualization
- **Domain-driven architecture** - events/effects for various domains

## Upgrade to v0.8.1 + Insight

### Benefits

1. **Durability**: Events survive crashes/restarts
2. **Distributed**: Multiple workers can process from queue
3. **Observability**: Real-time monitoring with tree visualization
4. **Retry/Timeout**: Built-in retry logic for effects
5. **Priority Queues**: Control effect execution order
6. **Status Tracking**: Query workflow progress at any time

### Step 1: Update Dependencies

**`packages/server/Cargo.toml`:**
```toml
[dependencies]
# Upgrade from 0.7.8 to 0.8.1
seesaw_core = "0.8.1"  # Was 0.7.8

# Add queue-backed support
seesaw-postgres = "0.8.1"

# Optional: Keep viz or replace with insight
# seesaw-viz = { version = "0.1", features = ["web-viewer"] }  # Old approach
seesaw-insight = "0.8.1"  # New real-time visualization
```

**Root `Cargo.toml` workspace:**
```toml
[workspace]
members = [
    "packages/server",
    "packages/twilio-rs",
    "packages/extraction",
    "packages/seesaw-insight",  # Add if running embedded
]
```

### Step 2: Apply Database Schema

```bash
cd ~/Developer/fourthplaces/mndigitalaid

# Apply Seesaw schema
psql $DATABASE_URL < ~/Developer/crcn/seesaw-rs/docs/schema.sql

# Or copy schema to your migrations
cp ~/Developer/crcn/seesaw-rs/docs/schema.sql data/migrations/009_seesaw_queue.sql
```

The schema adds:
- `seesaw_events` - Queue table (partitioned)
- `seesaw_state` - Per-workflow state
- `seesaw_effect_executions` - Effect tracking
- `seesaw_stream` - Observability stream
- `seesaw_dlq` - Dead letter queue

### Step 3: Update Engine Creation

**Before (v0.7.8 - in-memory):**
```rust
// packages/server/src/server/app.rs
use seesaw_core::{effect, Engine};

let engine = Engine::with_deps(server_deps)
    .with_effect(auth_effect())
    .with_effect(website_effect())
    .with_effect(post_composite_effect())
    // ... more effects
```

**After (v0.8.1 - queue-backed):**
```rust
// packages/server/src/server/app.rs
use seesaw_core::{effect, reducer, QueueEngine};
use seesaw_postgres::PostgresStore;

pub async fn create_queue_engine(
    pool: PgPool,
    deps: ServerDeps,
) -> Result<QueueEngine<WorkflowState, ServerDeps, PostgresStore>> {
    let store = PostgresStore::new(pool);

    let engine = QueueEngine::new(deps, store)
        // Add reducer if you need state tracking
        .with_reducer(
            reducer::fold::<DomainEvent>().into_queue(|state, event| {
                // Update state based on events
                state  // Or modified state
            })
        )
        // Convert effects to queue-backed
        .with_effect(auth_effect())
        .with_effect(website_effect())
        .with_effect(post_composite_effect())
        .with_effect(member_effect())
        .with_effect(chat_effect())
        .with_effect(provider_effect())
        .with_effect(website_approval_effect())
        .with_effect(mark_no_listings_effect());

    Ok(engine)
}
```

### Step 4: Update Effect Definitions

Your existing effects need minimal changes. Add queue codec registration:

**Before:**
```rust
// domains/website_approval/effects/mod.rs
pub fn website_approval_effect<S, D>() -> Effect<S, D>
where
    S: Clone + Send + Sync + 'static,
    D: ServerDepsProvider,
{
    effect::on::<WebsiteApprovalEvent>()
        .extract(|e| match e {
            WebsiteApprovalEvent::WebsiteApproved { website_id } => Some(*website_id),
            _ => None,
        })
        .then(|website_id, ctx| async move {
            // ... logic
            Ok(CrawlEvent::CrawlStarted { website_id })
        })
}
```

**After (with retry/timeout/queue):**
```rust
// domains/website_approval/effects/mod.rs
pub fn website_approval_effect<S, D>() -> Effect<S, D>
where
    S: Clone + Send + Sync + serde::Serialize + serde::de::DeserializeOwned + 'static,
    D: ServerDepsProvider,
{
    effect::on::<WebsiteApprovalEvent>()
        .extract(|e| match e {
            WebsiteApprovalEvent::WebsiteApproved { website_id } => Some(*website_id),
            _ => None,
        })
        .id("start_crawl")          // For debugging
        .retry(3)                    // Retry on failure
        .timeout(Duration::from_secs(30))
        .priority(1)                 // Higher priority
        .then_queue(|website_id, ctx| async move {
            ctx.deps().crawler.start(website_id).await?;
            Ok(CrawlEvent::CrawlStarted { website_id })
        })
}
```

**Key Changes:**
- Use `.then_queue()` instead of `.then()` for queue-backed effects
- Add `.id()`, `.retry()`, `.timeout()`, `.priority()` as needed
- Add serde bounds if using state: `S: serde::Serialize + serde::de::DeserializeOwned`

### Step 5: Register Event Codecs

For queue-backed execution, events must be serializable:

```rust
// Add to your event types
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum WebsiteApprovalEvent {
    WebsiteApproved { website_id: Uuid },
    WebsiteRejected { website_id: Uuid, reason: String },
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum CrawlEvent {
    CrawlStarted { website_id: Uuid },
    PagesCrawled { website_id: Uuid, page_count: usize },
    CrawlCompleted { website_id: Uuid },
}
```

Register codecs (happens automatically with `.then_queue()`):
```rust
// Reducer registration includes codec
.with_reducer(
    reducer::fold::<WebsiteApprovalEvent>()
        .into_queue(|state, event| {
            // State updates
            state
        })
)

// Effect registration includes codec
.with_effect(
    effect::on::<WebsiteApprovalEvent>()
        .then_queue(|event, ctx| async move {
            // Effect logic
            Ok(CrawlEvent::CrawlStarted { website_id: event.website_id })
        })
)
```

### Step 6: Update GraphQL Mutations

Update your mutations to use queue-backed execution:

**Before:**
```rust
// Returns immediately
async fn approve_website(ctx: &Context, website_id: Uuid) -> Result<Website> {
    let engine = ctx.engine();
    engine.run(|_| Ok(WebsiteApprovalEvent::WebsiteApproved { website_id })).await?;
    // ... fetch and return
}
```

**After (Fire & Forget):**
```rust
async fn approve_website(ctx: &Context, website_id: Uuid) -> Result<ApprovalResponse> {
    let engine = ctx.engine();

    // Pattern 1: Fire and forget - return workflow_id
    let handle = engine
        .process(WebsiteApprovalEvent::WebsiteApproved { website_id })
        .await?;

    Ok(ApprovalResponse {
        workflow_id: handle.correlation_id,  // Client can track this
        message: "Website approval workflow started"
    })
}
```

**After (Wait for Completion):**
```rust
async fn approve_website_sync(ctx: &Context, website_id: Uuid) -> Result<Website> {
    let engine = ctx.engine();

    // Pattern 2: Wait for terminal event
    engine
        .process(WebsiteApprovalEvent::WebsiteApproved { website_id })
        .wait(|event| {
            if let Some(saga_event) = event.downcast_ref::<SagaEvent>() {
                match saga_event.event_type.as_str() {
                    "CrawlCompleted" => Some(Ok(())),
                    "CrawlFailed" => Some(Err(anyhow!("Crawl failed"))),
                    _ => None  // Keep waiting
                }
            } else {
                None
            }
        })
        .timeout(Duration::from_secs(300))  // 5 minute timeout
        .await?;

    // Fetch and return website
    let website = ctx.website_repo().find_by_id(website_id).await?;
    Ok(website)
}
```

**After (Check Status):**
```rust
async fn workflow_status(ctx: &Context, workflow_id: Uuid) -> Result<WorkflowStatus> {
    let store = ctx.store();
    let status = store.get_workflow_status(workflow_id).await?;
    Ok(status)
}
```

### Step 7: Start Background Workers

Queue-backed execution requires runtime workers:

**`packages/server/src/main.rs`:**
```rust
use seesaw_core::runtime::{Runtime, RuntimeConfig};

#[tokio::main]
async fn main() -> Result<()> {
    // ... existing setup

    let pool = create_pool(&config).await?;
    let deps = ServerDeps::new(/* ... */);
    let engine = create_queue_engine(pool.clone(), deps).await?;

    // Start Seesaw runtime (event + effect workers)
    let runtime = Runtime::start(
        &engine,
        RuntimeConfig {
            event_workers: 2,   // 2 event workers
            effect_workers: 4,  // 4 effect workers
            ..Default::default()
        }
    ).await?;

    // Start web server
    let app = create_app(pool, engine).await?;
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await?;

    info!("Server listening on http://0.0.0.0:3000");

    // Run both concurrently
    tokio::select! {
        result = axum::serve(listener, app) => {
            result?;
        }
        _ = tokio::signal::ctrl_c() => {
            info!("Shutting down gracefully...");
            runtime.shutdown().await;
        }
    }

    Ok(())
}
```

### Step 8: Add Seesaw Insight

**Option A: Separate Service (Recommended)**

```bash
cd ~/Developer/crcn/seesaw-rs

# Build insight binary
cargo build --release -p seesaw-insight

# Run it
export DATABASE_URL="postgres://user:pass@host/mndigitalaid"
./target/release/seesaw-insight

# Access at http://localhost:3000
```

**Option B: Embedded in Server**

```rust
// packages/server/src/main.rs
use seesaw_insight;

#[tokio::main]
async fn main() -> Result<()> {
    // ... existing setup

    // Start insight server on different port
    let insight_pool = pool.clone();
    tokio::spawn(async move {
        let static_dir = Some("./packages/seesaw-insight/static");
        seesaw_insight::serve(insight_pool, "127.0.0.1:3001", static_dir)
            .await
            .expect("Insight server failed");
    });

    info!("Seesaw Insight: http://localhost:3001");

    // ... rest of server startup
}
```

### Step 9: Update Frontend to Show Workflow Status

**GraphQL Schema Addition:**
```graphql
type WorkflowStatus {
  correlationId: ID!
  pendingEffects: Int!
  isSettled: Boolean!
  lastEvent: String
}

type ApprovalResponse {
  workflowId: ID!
  message: String!
}

type Mutation {
  approveWebsite(websiteId: ID!): ApprovalResponse!
  workflowStatus(workflowId: ID!): WorkflowStatus!
}
```

**React Component Example:**
```typescript
// web-next/components/WorkflowTracker.tsx
import { useEffect, useState } from 'react';

export function WorkflowTracker({ workflowId }: { workflowId: string }) {
  const [status, setStatus] = useState(null);
  const [ws, setWs] = useState<WebSocket | null>(null);

  useEffect(() => {
    // Connect to Seesaw Insight WebSocket
    const socket = new WebSocket(
      `ws://localhost:3001/api/ws?correlation_id=${workflowId}`
    );

    socket.onmessage = (event) => {
      const entry = JSON.parse(event.data);

      if (entry.stream_type === 'effect_completed') {
        console.log(`Step ${entry.effect_id} completed`);
      } else if (entry.stream_type === 'effect_failed') {
        console.error(`Step ${entry.effect_id} failed:`, entry.error);
      }

      // Update UI based on events
      setStatus(prev => ({
        ...prev,
        lastEvent: entry.stream_type
      }));
    };

    setWs(socket);
    return () => socket.close();
  }, [workflowId]);

  return (
    <div>
      <h3>Workflow Status</h3>
      <p>Workflow ID: {workflowId}</p>
      {status && <p>Last Event: {status.lastEvent}</p>}

      <a href={`http://localhost:3001/?workflow=${workflowId}`} target="_blank">
        View in Insight Dashboard
      </a>
    </div>
  );
}
```

### Step 10: Migration Strategy

**Gradual Migration:**

1. **Start with new domains** - Use queue-backed for new features
2. **Keep existing in-memory** - Don't break what works
3. **Hybrid approach** - Run both engines side-by-side

```rust
// Keep both engines temporarily
pub struct AppState {
    pub legacy_engine: Arc<Engine<(), ServerDeps>>,  // v0.7.8 in-memory
    pub queue_engine: Arc<QueueEngine<WorkflowState, ServerDeps, PostgresStore>>,  // v0.8.1
}

// Use legacy for existing features
legacy_engine.run(|_| Ok(ExistingEvent)).await?;

// Use queue for new features
queue_engine.process(NewEvent).await?;
```

### Step 11: Testing

**Unit Tests:**
```rust
#[tokio::test]
async fn test_website_approval_workflow() {
    let pool = test_pool().await;
    let store = PostgresStore::new(pool);
    let deps = test_deps();

    let engine = QueueEngine::new(deps, store)
        .with_effect(website_approval_effect());

    let handle = engine
        .process(WebsiteApprovalEvent::WebsiteApproved {
            website_id: Uuid::new_v4()
        })
        .await
        .expect("should process");

    // Check status
    let status = store.get_workflow_status(handle.correlation_id).await?;
    assert_eq!(status.is_settled, true);
}
```

### Step 12: Monitoring in Production

**Prometheus Metrics (add to your server):**
```rust
// Track workflow metrics
prometheus::register_gauge!("seesaw_pending_effects", "Pending effects").set(status.pending_effects as f64);
prometheus::register_gauge!("seesaw_active_workflows", "Active workflows").set(active_count as f64);
```

**Access Insight Dashboard:**
- Development: `http://localhost:3001`
- Production: `https://insight.mndigitalaid.org` (behind auth)

## Summary of Changes

| Component | Before (v0.7.8) | After (v0.8.1) |
|-----------|----------------|----------------|
| Engine | `Engine::with_deps()` | `QueueEngine::new()` |
| Effects | `.then()` | `.then_queue()` with retry/timeout |
| Events | In-memory only | Persisted to Postgres |
| Workers | None (inline) | Runtime workers (event + effect) |
| Visualization | seesaw-viz (static) | seesaw-insight (real-time) |
| State | Transient | Optional persistent state |
| Retry | Manual | Built-in with `.retry(N)` |
| Status | Not trackable | `get_workflow_status()` |

## Migration Checklist

- [ ] Update dependencies to v0.8.1
- [ ] Apply database schema
- [ ] Add `serde` derives to event types
- [ ] Update effects to use `.then_queue()`
- [ ] Create `QueueEngine` with `PostgresStore`
- [ ] Start runtime workers
- [ ] Update GraphQL mutations to return workflow IDs
- [ ] Deploy Insight server (separate or embedded)
- [ ] Update frontend to track workflows
- [ ] Add monitoring/metrics
- [ ] Test thoroughly in staging
- [ ] Gradual rollout to production

## Questions?

- See full docs: `~/Developer/crcn/seesaw-rs/README.md`
- Insight features: `~/Developer/crcn/seesaw-rs/crates/seesaw-insight/README.md`
- Example: `~/Developer/crcn/seesaw-rs/examples/workflow-status`
