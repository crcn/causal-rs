# Distributed Production Readiness Checklist

**Date:** 2026-02-06

## Status Overview

| Category | Status | Priority |
|----------|--------|----------|
| Core Architecture | 🟡 Planned | Critical |
| Idempotency | 🟢 Exists | Critical |
| Worker Coordination | 🟢 Exists | Critical |
| Dead Letter Queue | 🔴 Missing | Critical |
| Graceful Shutdown | 🔴 Missing | Critical |
| Observability | 🔴 Missing | High |
| Backpressure | 🔴 Missing | High |
| Event Versioning | 🔴 Missing | Medium |
| Testing Utilities | 🔴 Missing | High |
| Configuration | 🟡 Partial | High |

---

## Part 1: Critical Items (Must Have for Production)

### 1. Dead Letter Queue 🔴 NEED TO IMPLEMENT

**Problem:** When a handler fails after max retries, what happens to the event?

**Current behavior:**
```rust
#[handler(on = PaymentRequested, retry = 3)]
async fn charge_payment(...) -> Result<PaymentCharged> {
    ctx.deps().stripe.charge(&event).await?;  // Fails 3 times
    Ok(PaymentCharged { ... })
}

// After 3 failures:
// - Intent marked as "failed"
// - Event stuck in limbo
// - No way to recover
// - No visibility into what failed
```

**Solution: Dead Letter Queue**

```sql
-- New table for failed intents with explicit lifecycle
CREATE TYPE causal_dlq_status AS ENUM ('open', 'retrying', 'replayed', 'resolved');

CREATE TABLE causal_dead_letter_queue (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    event_id UUID NOT NULL REFERENCES causal_events(id),
    handler_id TEXT NOT NULL,
    intent_id UUID NOT NULL UNIQUE,
    error_message TEXT NOT NULL,
    error_details JSONB,
    retry_count INTEGER NOT NULL,
    first_failed_at TIMESTAMPTZ NOT NULL,
    last_failed_at TIMESTAMPTZ NOT NULL,
    event_payload JSONB NOT NULL,
    status causal_dlq_status NOT NULL DEFAULT 'open',
    retry_attempts INTEGER NOT NULL DEFAULT 0,
    last_retry_at TIMESTAMPTZ,
    resolved_at TIMESTAMPTZ,
    resolution_note TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_dlq_handler_open
    ON causal_dead_letter_queue(handler_id, created_at DESC)
    WHERE status IN ('open', 'retrying');
```

**Implementation:**
```rust
// After max retries exhausted
async fn handle_permanent_failure(
    intent: &HandlerIntent,
    error: &Error,
    store: &impl Store,
) -> Result<()> {
    let mut tx = store.pool().begin().await?;

    // Insert (or refresh) DLQ record
    sqlx::query(
        "INSERT INTO causal_dead_letter_queue
         (event_id, handler_id, intent_id, error_message, error_details,
          retry_count, first_failed_at, last_failed_at, event_payload, status)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 'open')
         ON CONFLICT (intent_id) DO UPDATE SET
           error_message = EXCLUDED.error_message,
           error_details = EXCLUDED.error_details,
           retry_count = EXCLUDED.retry_count,
           last_failed_at = EXCLUDED.last_failed_at,
           status = 'open'"
    )
    .bind(intent.event_id)
    .bind(&intent.handler_id)
    .bind(intent.id)
    .bind(error.to_string())
    .bind(json!({"error": format!("{:?}", error)}))
    .bind(intent.retry_count)
    .bind(intent.first_attempted_at)
    .bind(Utc::now())
    .bind(&intent.event_payload)
    .execute(&mut *tx)
    .await?;

    // Mark intent as permanently failed
    sqlx::query(
        "UPDATE causal_handler_intents
         SET status = 'dead_letter', updated_at = NOW()
         WHERE id = $1"
    )
    .bind(intent.id)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    // Emit DLQ event for monitoring (outside transaction)
    store.dispatch(HandlerFailedPermanently {
        event_id: intent.event_id,
        handler_id: intent.handler_id.clone(),
        error: error.to_string(),
    }).await?;

    Ok(())
}
```

**DLQ Management API:**
```rust
// Inspect DLQ
async fn list_dead_letters(store: &impl Store) -> Result<Vec<DeadLetter>> {
    sqlx::query_as(
        "SELECT * FROM causal_dead_letter_queue
         WHERE status IN ('open', 'retrying')
         ORDER BY created_at DESC
         LIMIT 100"
    )
    .fetch_all(&store.pool())
    .await
}

// Retry from DLQ
async fn retry_dead_letter(
    dlq_id: Uuid,
    engine: &Engine,
    store: &impl Store,
) -> Result<()> {
    // Lock row so only one operator retries this entry at a time
    let mut tx = store.pool().begin().await?;
    let dlq: DeadLetter = sqlx::query_as(
        "SELECT * FROM causal_dead_letter_queue
         WHERE id = $1
         FOR UPDATE"
    )
    .bind(dlq_id)
    .fetch_one(&mut *tx)
    .await?;

    sqlx::query(
        "UPDATE causal_dead_letter_queue
         SET status = 'retrying',
             retry_attempts = retry_attempts + 1,
             last_retry_at = NOW()
         WHERE id = $1"
    )
    .bind(dlq_id)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    // Re-dispatch event
    let event: Event = serde_json::from_value(dlq.event_payload.clone())?;
    match engine.dispatch(event).await {
        Ok(()) => {
            sqlx::query(
                "UPDATE causal_dead_letter_queue
                 SET status = 'replayed',
                     resolved_at = NOW(),
                     resolution_note = 're-dispatched from DLQ'
                 WHERE id = $1"
            )
            .bind(dlq_id)
            .execute(&store.pool())
            .await?;
            Ok(())
        }
        Err(err) => {
            // Preserve record for future retry and postmortem
            sqlx::query(
                "UPDATE causal_dead_letter_queue
                 SET status = 'open',
                     last_failed_at = NOW(),
                     error_message = $2,
                     error_details = $3
                 WHERE id = $1"
            )
            .bind(dlq_id)
            .bind(err.to_string())
            .bind(json!({"retry_error": format!("{:?}", err)}))
            .execute(&store.pool())
            .await?;
            Err(err)
        }
    }
}

// Bulk retry with bounded batch size
async fn retry_all_by_handler(
    handler_id: &str,
    engine: &Engine,
    store: &impl Store,
) -> Result<RetrySummary> {
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id
         FROM causal_dead_letter_queue
         WHERE handler_id = $1 AND status = 'open'
         ORDER BY created_at
         LIMIT 100"
    )
    .bind(handler_id)
    .fetch_all(&store.pool())
    .await?;

    let mut succeeded = 0usize;
    let mut failed = 0usize;
    for id in ids {
        if retry_dead_letter(id, engine, store).await.is_ok() {
            succeeded += 1;
        } else {
            failed += 1;
        }
    }

    Ok(RetrySummary { succeeded, failed })
}
```

**Priority:** 🔴 **CRITICAL** - Without DLQ, failed events are lost forever

---

### 2. Graceful Shutdown 🔴 NEED TO IMPLEMENT

**Problem:** When deploying new code, workers must finish current tasks before exiting.

**Current behavior:**
```
SIGTERM received → Worker killed immediately → In-flight handlers aborted
```

**Solution: Graceful Shutdown**

```rust
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

pub struct Worker {
    engine: Arc<Engine>,
    shutdown: CancellationToken,
    max_in_flight: usize,
}

impl Worker {
    pub async fn start(&self) -> Result<()> {
        // Convert SIGTERM/SIGINT into cancellation
        let token = self.shutdown.clone();
        tokio::spawn(async move {
            let mut sigterm = signal(SignalKind::terminate())?;
            let mut sigint = signal(SignalKind::interrupt())?;

            tokio::select! {
                _ = sigterm.recv() => {
                    println!("SIGTERM received, starting graceful shutdown...");
                }
                _ = sigint.recv() => {
                    println!("SIGINT received, starting graceful shutdown...");
                }
            }

            token.cancel();
            Ok::<_, anyhow::Error>(())
        });

        let mut tasks: JoinSet<Result<()>> = JoinSet::new();

        // Main worker loop
        loop {
            tokio::select! {
                _ = self.shutdown.cancelled() => {
                    println!("Shutdown requested, draining in-flight tasks...");
                    break;
                }
                Some(joined) = tasks.join_next(), if !tasks.is_empty() => {
                    joined??;
                }
                polled = self.poll_intent(), if tasks.len() < self.max_in_flight => {
                    if let Some(intent) = polled? {
                        let engine = Arc::clone(&self.engine);
                        tasks.spawn(async move {
                            engine.execute_handler(&intent).await
                        });
                    } else {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                }
            }
        }

        self.drain_with_timeout(tasks, Duration::from_secs(30)).await
    }

    async fn drain_with_timeout(
        &self,
        mut tasks: JoinSet<Result<()>>,
        timeout: Duration,
    ) -> Result<()> {
        let drain = async {
            while let Some(joined) = tasks.join_next().await {
                joined??;
            }
            Ok::<(), anyhow::Error>(())
        };

        tokio::time::timeout(timeout, drain)
            .await
            .map_err(|_| anyhow!("graceful shutdown timeout exceeded"))??;

        Ok(())
    }

    async fn poll_intent(&self) -> Result<Option<HandlerIntent>> {
        if self.shutdown.is_cancelled() {
            // Don't claim new work once shutdown starts
            return Ok(None);
        }

        self.engine.store.claim_intent().await
    }
}
```

**Key behavior:** stop claiming new intents immediately, then drain claimed tasks with a hard timeout and explicit join handling.

**Priority:** 🔴 **CRITICAL** - Without graceful shutdown, deployments lose in-flight work

---

### 3. Idempotency (Already Exists!) ✅

**Current implementation:**
```rust
impl HandlerContext {
    pub fn idempotency_key(&self) -> String {
        // Deterministic key based on event_id + handler_id
        format!("{}-{}", self.event_id, self.handler_id)
    }
}
```

**Usage:**
```rust
#[handler(on = PaymentRequested, retry = 3)]
async fn charge_payment(event: PaymentRequested, ctx: Ctx) -> Result<PaymentCharged> {
    // Use idempotency key for external API
    let result = ctx.deps().stripe
        .charge(&event)
        .idempotency_key(&ctx.idempotency_key())
        .send()
        .await?;

    Ok(PaymentCharged {
        order_id: event.order_id,
        charge_id: result.id,
    })
}
```

**Status:** 🟢 **EXISTS** - Already implemented in HandlerContext

---

### 4. Worker Coordination (Already Exists!) ✅

**Current implementation:**
```sql
-- Workers claim intents using SKIP LOCKED
SELECT * FROM causal_handler_intents
WHERE status = 'pending'
ORDER BY created_at
FOR UPDATE SKIP LOCKED
LIMIT 10;
```

**How it works:**
- Worker 1 claims intent → Row locked
- Worker 2 tries to claim same intent → SKIP LOCKED skips it
- Worker 2 claims next available intent
- No duplicate execution

**Status:** 🟢 **EXISTS** - Already using Postgres row-level locks

---

## Part 2: High Priority Items (Should Have)

### 5. Observability 🔴 NEED TO IMPLEMENT

**Problem:** Can't see what's happening in distributed system.

**Solution: Structured Logging + Metrics + Tracing**

**A. Structured Logging**
```rust
use tracing::{info, warn, error, instrument};

#[instrument(skip(ctx), fields(
    event_id = %ctx.event_id(),
    handler_id = %ctx.handler_id(),
    correlation_id = %ctx.correlation_id
))]
async fn execute_handler(
    handler: &Handler,
    event: Event,
    ctx: HandlerContext,
) -> Result<Vec<Event>> {
    info!("Executing handler");

    let start = Instant::now();
    let result = handler.execute(event, ctx).await;
    let duration = start.elapsed();

    match &result {
        Ok(events) => {
            info!(
                events_emitted = events.len(),
                duration_ms = duration.as_millis(),
                "Handler succeeded"
            );
        }
        Err(e) => {
            error!(
                error = %e,
                duration_ms = duration.as_millis(),
                "Handler failed"
            );
        }
    }

    result
}
```

**B. Metrics**
```rust
use prometheus::{Counter, Histogram, Registry};

pub struct Metrics {
    events_dispatched: Counter,
    handlers_executed: Counter,
    handlers_failed: Counter,
    handler_duration: Histogram,
    queue_depth: Gauge,
}

impl Engine {
    async fn dispatch(&self, event: Event) -> Result<()> {
        self.metrics.events_dispatched.inc();

        // ... dispatch logic ...

        Ok(())
    }

    async fn execute_handler(&self, intent: &HandlerIntent) -> Result<()> {
        let start = Instant::now();
        self.metrics.handlers_executed.inc();

        let result = /* ... execute ... */;

        let duration = start.elapsed();
        self.metrics.handler_duration.observe(duration.as_secs_f64());

        if result.is_err() {
            self.metrics.handlers_failed.inc();
        }

        result
    }

    async fn queue_depth(&self) -> Result<u64> {
        let depth = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM causal_handler_intents WHERE status = 'pending'"
        )
        .fetch_one(&self.store.pool())
        .await?;

        self.metrics.queue_depth.set(depth as f64);
        Ok(depth as u64)
    }
}
```

**C. Distributed Tracing**
```rust
use opentelemetry::trace::{Tracer, Span};

async fn dispatch(&self, event: Event) -> Result<()> {
    let tracer = global::tracer("causal");
    let mut span = tracer.start("dispatch_event");
    span.set_attribute("event.type", event.type_name());

    // Persist event
    let event_id = self.store.insert_event(&event).await?;
    span.set_attribute("event.id", event_id.to_string());

    // Insert intents
    for handler in &self.handlers {
        let mut intent_span = tracer.start("create_intent");
        intent_span.set_attribute("handler.id", handler.id());

        self.store.insert_intent(event_id, handler.id()).await?;

        intent_span.end();
    }

    span.end();
    Ok(())
}
```

**Priority:** 🔴 **HIGH** - Can't operate distributed system without visibility

---

### 6. Backpressure 🔴 NEED TO IMPLEMENT

**Problem:** What happens when workers can't keep up with event rate?

**Scenarios:**
1. Queue fills up → Database storage grows unbounded
2. Workers fall behind → Latency increases
3. System overload → Everything slows down

**Solution A: Dispatch-Side Admission Control (Primary)**
```rust
pub struct BackpressureConfig {
    soft_limit: usize,
    hard_limit: usize,
    soft_delay: Duration,
}

impl Engine {
    pub async fn dispatch_with_backpressure(&self, event: Event) -> Result<()> {
        let depth = self.queue_depth().await?;

        if depth >= self.backpressure.hard_limit as u64 {
            // Caller can retry with exponential backoff + jitter.
            return Err(anyhow!("queue overloaded: depth={} limit={}", depth, self.backpressure.hard_limit));
        }

        if depth >= self.backpressure.soft_limit as u64 {
            // Slow producers before the system hits hard overload.
            tokio::time::sleep(self.backpressure.soft_delay).await;
        }

        self.dispatch(event).await
    }
}
```

**Solution B: Worker Drain Mode (Do Not Sleep While Backlogged)**
```rust
impl Worker {
    async fn run(&self) -> Result<()> {
        loop {
            let depth = self.engine.queue_depth().await?;

            // When backlog is high, increase drain aggressiveness.
            let batch_size = if depth > self.high_watermark as u64 { 100 } else { 20 };
            let poll_delay = if depth > self.high_watermark as u64 {
                Duration::from_millis(5)
            } else {
                Duration::from_millis(100)
            };

            let intents = self.engine.store.claim_intent_batch(batch_size).await?;
            if intents.is_empty() {
                tokio::time::sleep(poll_delay).await;
                continue;
            }

            for intent in intents {
                self.process_intent(intent).await?;
            }
        }
    }
}
```

**Solution C: Dynamic Worker Scaling with Cooldown**
```rust
pub struct AutoScaler {
    min_workers: usize,
    max_workers: usize,
    scale_up_threshold: f64,    // Queue depth / worker count
    scale_down_threshold: f64,
    cooldown: Duration,
    last_scaled_at: Instant,
}

impl AutoScaler {
    async fn run(&mut self, engine: Arc<Engine>) -> Result<()> {
        loop {
            let queue_depth = engine.queue_depth().await?;
            let worker_count = engine.active_workers().await?;

            let ratio = queue_depth as f64 / worker_count.max(1) as f64;

            if self.last_scaled_at.elapsed() < self.cooldown {
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }

            if ratio > self.scale_up_threshold && worker_count < self.max_workers {
                // Scale up
                engine.spawn_worker().await?;
                self.last_scaled_at = Instant::now();
                info!("Scaled up to {} workers", worker_count + 1);
            } else if ratio < self.scale_down_threshold && worker_count > self.min_workers {
                // Scale down
                engine.stop_worker().await?;
                self.last_scaled_at = Instant::now();
                info!("Scaled down to {} workers", worker_count - 1);
            }

            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    }
}
```

**Operator guidance:** apply backpressure at dispatch/admission first; workers should prioritize draining backlog, not pausing when depth is high.

**Priority:** 🔴 **HIGH** - Without backpressure, system can be overwhelmed

---

### 7. Configuration Management 🔴 NEED TO IMPLEMENT

**Problem:** Workers need consistent configuration across deployments.

**Solution: Centralized Configuration (Fail Fast)**
```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerConfig {
    // Worker settings
    pub concurrency: usize,
    pub poll_interval: Duration,
    pub graceful_shutdown_timeout: Duration,

    // Queue settings
    pub max_queue_depth: usize,
    pub event_types: Option<Vec<String>>,  // None = subscribe to all

    // Retry settings
    pub default_max_retries: u32,
    pub retry_backoff: RetryBackoff,

    // Observability
    pub enable_tracing: bool,
    pub enable_metrics: bool,
    pub metrics_port: u16,

    // Resource limits
    pub max_memory_mb: Option<usize>,
    pub max_cpu_percent: Option<f64>,
}

impl WorkerConfig {
    pub fn load(config_path: Option<&Path>) -> Result<Self> {
        let mut builder = config::Config::builder();

        if let Some(path) = config_path {
            builder = builder.add_source(config::File::from(path.to_path_buf()).required(true));
        }

        builder = builder.add_source(
            config::Environment::with_prefix("CAUSAL")
                .separator("__")
        );

        let cfg: WorkerConfig = builder.build()?.try_deserialize()?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(self.concurrency > 0, "concurrency must be > 0");
        ensure!(self.metrics_port > 0, "metrics_port must be set");
        ensure!(self.default_max_retries <= 20, "default_max_retries too high");
        Ok(())
    }
}

// Usage: invalid values should fail startup, not silently fall back.
let config = WorkerConfig::load(args.config.as_deref())
    .context("invalid worker config; fix env/file values before startup")?;

let worker = Worker::builder()
    .config(config)
    .engine(engine)
    .build();
```

**Optional runtime overrides (explicit):**
```rust
pub async fn apply_runtime_overrides(base: WorkerConfig, db: &PgPool) -> Result<WorkerConfig> {
    let overrides = sqlx::query_as::<_, RuntimeConfigRow>(
        "SELECT * FROM worker_config WHERE id = 'default'"
    )
    .fetch_optional(db)
    .await?;

    let merged = merge(base, overrides);
    merged.validate()?;
    Ok(merged)
}
```

**Priority:** 🔴 **HIGH** - Consistent configuration critical for distributed systems

---

## Part 3: Medium Priority Items (Nice to Have)

### 8. Event Versioning ✅ CONSIDER

**Problem:** Events evolve over time, need to handle old versions.

**Solution A: Version Field**
```rust
#[derive(Clone, Serialize, Deserialize)]
struct OrderPlaced {
    #[serde(default = "default_version")]
    version: u32,  // Start at 1

    order_id: Uuid,
    user_id: Uuid,

    // v2 added this field
    #[serde(default)]
    items: Vec<Item>,
}

fn default_version() -> u32 { 1 }

// Handler handles multiple versions
#[handler(on = OrderPlaced)]
async fn handle_order(event: OrderPlaced, ctx: Ctx) -> Result<()> {
    match event.version {
        1 => {
            // Handle v1 (no items field)
            // Fetch items from elsewhere
        }
        2 => {
            // Handle v2 (has items field)
            // Use event.items directly
        }
        _ => bail!("Unsupported event version: {}", event.version),
    }

    Ok(())
}
```

**Solution B: Upcasting**
```rust
trait EventUpcast {
    fn upcast(payload: Value) -> Result<Self> where Self: Sized;
}

impl EventUpcast for OrderPlaced {
    fn upcast(mut payload: Value) -> Result<Self> {
        // Check version
        let version = payload["version"].as_u64().unwrap_or(1);

        match version {
            1 => {
                // Upcast v1 to v2
                payload["items"] = json!([]);  // Default empty items
                payload["version"] = json!(2);
            }
            2 => {
                // Already v2
            }
            _ => bail!("Unknown version: {}", version),
        }

        // Deserialize as latest version
        serde_json::from_value(payload)
    }
}
```

**Priority:** 🟡 **MEDIUM** - Can defer until schema changes needed

---

### 9. Testing Utilities 🔴 NEED TO IMPLEMENT

**Problem:** Hard to test distributed behavior locally.

**Solution: Test Helpers**

```rust
// Test helper for deterministic execution
pub struct TestEngine {
    engine: Engine,
    clock: Arc<Mutex<FakeClock>>,
}

impl TestEngine {
    pub async fn dispatch_and_wait(&self, event: Event) -> Result<Vec<Event>> {
        // Dispatch event
        self.engine.dispatch(event).await?;

        // Run all handlers synchronously (for testing)
        let mut results = Vec::new();

        while let Some(intent) = self.engine.store.claim_intent().await? {
            let events = self.engine.execute_handler(&intent).await?;
            results.extend(events);

            // Dispatch result events
            for event in &results {
                self.engine.dispatch(event.clone()).await?;
            }
        }

        Ok(results)
    }

    pub async fn advance_time(&self, duration: Duration) {
        self.clock.lock().unwrap().advance(duration);

        // Process delayed handlers
        self.engine.process_delayed_handlers().await.unwrap();
    }
}

// Usage in tests
#[tokio::test]
async fn test_order_workflow() {
    let engine = TestEngine::new().await;

    // Dispatch event
    let events = engine.dispatch_and_wait(OrderPlaced {
        order_id: Uuid::new_v4(),
        user_id: Uuid::new_v4(),
    }).await?;

    // Assert results
    assert_eq!(events.len(), 1);
    assert!(matches!(events[0], OrderSaved { .. }));
}

#[tokio::test]
async fn test_delayed_handler() {
    let engine = TestEngine::new().await;

    engine.dispatch(OrderPlaced { .. }).await?;

    // Advance time by 1 hour
    engine.advance_time(Duration::from_secs(3600)).await;

    // Delayed handler should have executed
    let events = engine.emitted_events().await?;
    assert!(events.iter().any(|e| matches!(e, FollowupSent { .. })));
}
```

**Priority:** 🟡 **MEDIUM** - Essential for testing but can build incrementally

---

## Part 4: Implementation Roadmap (v2)

### Phase 1: Failure Safety (Week 1)
1. [ ] Dead Letter Queue + replay lifecycle
   - Add DLQ status model (`open/retrying/replayed/resolved`)
   - Keep immutable history (no delete-on-retry)
   - Add retry API with row lock (`FOR UPDATE`) and bounded batches
   - Add tests for duplicate retries, partial failure, and crash-recovery paths
   - Exit criteria: every permanent failure is queryable, retryable, and auditable

2. [ ] Graceful shutdown with bounded drain
   - Cancellation token on SIGTERM/SIGINT
   - Stop claiming new intents immediately
   - Track and join spawned tasks explicitly
   - Enforce drain timeout with clear non-zero exit path on timeout
   - Exit criteria: zero lost claimed intents in integration test under forced deploy restart

### Phase 2: Operability Baseline (Week 1)
3. [ ] Structured logs + correlation IDs
   - Include `event_id`, `handler_id`, and correlation metadata on all handler paths
   - Normalize error fields and retry context
   - Exit criteria: can trace a failed event end-to-end from logs alone

4. [ ] Metrics + alerts (minimum set)
   - Counters: dispatched, executed, failed, retried, DLQ entered
   - Gauges: queue depth, in-flight handler count, worker count
   - Histograms: handler duration, dispatch latency
   - Exit criteria: alert on sustained queue growth, failure-rate spike, DLQ growth

### Phase 3: Load Protection (Week 2)
5. [ ] Backpressure with admission control
   - Dispatch-side soft/hard queue limits
   - Exponential backoff + jitter contract for callers
   - Worker drain mode when backlog is high (no pause-on-overload)
   - Exit criteria: under load test, queue growth stabilizes and system recovers after burst

6. [ ] Configuration management (validated, fail-fast)
   - Single loader with explicit precedence rules
   - Startup validation for all operational limits and ports
   - Optional runtime overrides with merge + validate
   - Exit criteria: invalid config prevents boot with actionable error

### Phase 4: Testability & Runbooks (Week 2)
7. [ ] Testing utilities
   - Deterministic `TestEngine` execution helper
   - Failure injection hooks (timeout, transient error, permanent error)
   - Fake clock for retry/backoff and delayed handler tests
   - Exit criteria: critical safety paths covered by deterministic integration tests

8. [ ] Documentation and operations runbook
   - Deployment behavior during graceful shutdown
   - DLQ triage and replay procedures
   - Backpressure and alert playbooks
   - Exit criteria: on-call can execute recovery steps without code changes

### Phase 5: Advanced Features (Week 3-4, as needed)
9. [ ] Event versioning/upcasting
10. [ ] Auto-scaling policy hardening
11. [ ] Full distributed tracing rollout

---

## Part 5: Summary Checklist

### Must Have (Block Production)
- [ ] Dead Letter Queue
- [ ] Graceful Shutdown
- [x] Idempotency (already exists)
- [x] Worker Coordination (already exists)

### Should Have (High Priority)
- [ ] Observability (logging + metrics baseline)
- [ ] Backpressure handling
- [ ] Configuration management
- [ ] Testing utilities

### Nice to Have (Can Defer)
- [ ] Event versioning
- [ ] Auto-scaling
- [ ] Distributed tracing
- [ ] Circuit breakers

---

## Conclusion

**To make Causal production-ready for distributed systems, we need:**

**Critical (blocking):**
1. Dead Letter Queue (handle permanent failures)
2. Graceful Shutdown (safe deployments)

**High Priority (should have):**
3. Observability baseline (logs + metrics)
4. Backpressure (handle overload)
5. Configuration (consistent settings)
6. Testing (validate behavior)

**Good News:**
- ✅ Idempotency already exists
- ✅ Worker coordination already exists (SKIP LOCKED)
- ✅ Retry logic already exists
- ✅ Store/QueueBackend abstractions already exist

**Timeline:**
- Week 1: Dead Letter Queue + Graceful Shutdown + Observability
- Week 2: Backpressure + Configuration + Testing
- Week 3-4: Advanced features as needed

**Total effort:** ~2-3 weeks to production-ready distributed system
