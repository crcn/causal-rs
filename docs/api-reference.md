# Seesaw Queue-Backed API Reference

**Version**: v0.8 (Queue-Backed Architecture)
**Status**: Design Locked
**Last Updated**: 2026-02-05

This is the complete API surface after semantic decisions are locked.

---

## Core Types

```rust
use seesaw::{Engine, Runtime, Effect, Event, EffectContext};
use std::sync::Arc;
use uuid::Uuid;
use chrono::{DateTime, Utc, Duration};

/// Events must be Clone + Send + Sync + 'static
pub trait Event: Clone + Send + Sync + 'static {}

/// Auto-implemented for all qualifying types
impl<T> Event for T where T: Clone + Send + Sync + 'static {}
```

---

## 1. Engine API (Entry Point)

The engine is the main entry point for processing external events.

```rust
pub struct Engine<S, D> {
    // Internal fields private
}

impl<S, D> Engine<S, D>
where
    S: Clone + Send + Sync + 'static,
    D: Send + Sync + 'static,
{
    /// Create a new engine with dependencies
    pub fn new(deps: D) -> Self;

    /// Register an effect handler
    pub fn with_effect(self, effect: Effect<S, D>) -> Self;

    /// Register a reducer (pure state transformation)
    pub fn with_reducer(self, reducer: Reducer<S>) -> Self;

    /// Attach a queue (for durable execution)
    pub fn with_queue(self, queue: Arc<PostgresQueue>) -> Self;

    // ============================================================
    // EXTERNAL EVENT PROCESSING (Public API)
    // ============================================================

    /// Process an external event (new saga, generates event_id)
    ///
    /// Use this for: user actions, cron jobs, manual triggers
    /// Returns: saga_id for the new workflow
    ///
    /// # Example
    /// ```rust
    /// let saga_id = engine.process(OrderPlaced {
    ///     order_id: "12345",
    ///     user_id: "user_42",
    ///     amount: 99.99,
    /// }).await?;
    ///
    /// println!("Started saga: {}", saga_id);
    /// ```
    pub async fn process(&self, event: impl Event) -> Result<Uuid>;

    /// Process with explicit event_id (for webhook idempotency)
    ///
    /// Use this for: webhooks, external APIs with idempotency keys
    /// Returns: saga_id for the workflow
    ///
    /// # Example
    /// ```rust
    /// // Stripe webhook with idempotency
    /// let webhook_id = Uuid::parse_str(&stripe_event.id)?;
    /// engine.process_with_id(webhook_id, PaymentReceived {
    ///     order_id: payload.order_id,
    ///     amount: payload.amount,
    /// }).await?;
    /// ```
    pub async fn process_with_id(
        &self,
        event_id: Uuid,
        event: impl Event
    ) -> Result<Uuid>;

    /// Graceful shutdown - wait for workers to drain
    ///
    /// # Example
    /// ```rust
    /// tokio::select! {
    ///     _ = tokio::signal::ctrl_c() => {
    ///         info!("Shutting down...");
    ///         engine.shutdown().await?;
    ///     }
    /// }
    /// ```
    pub async fn shutdown(&self) -> Result<()>;

    // ============================================================
    // INTERNAL (Called by Workers - Not Public API)
    // ============================================================

    /// Process an event from the queue (called by workers)
    /// Returns next events to publish
    async fn process_event(
        &self,
        envelope: EventEnvelope
    ) -> Result<Vec<Event>>;
}
```

---

## 2. Effect API (Type-State Builder)

Effects are built using a type-state pattern that enforces `.id()` at compile time.

```rust
/// Effect builder - starts with NoId type state
pub struct EffectBuilder<E, State> {
    // Internal fields private
}

/// Type states for compile-time enforcement
pub struct NoId;
pub struct HasId;

// ============================================================
// STEP 1: Start Building (NoId state)
// ============================================================

impl<E> EffectBuilder<E, NoId>
where
    E: Event,
{
    /// Start building an effect for a specific event type
    ///
    /// # Example
    /// ```rust
    /// use seesaw::effect;
    ///
    /// let eff = effect::on::<OrderPlaced>()
    ///     .id("send_confirmation")  // ← REQUIRED
    ///     .then(|event, ctx| async move { ... });
    /// ```
    pub fn on<E>() -> EffectBuilder<E, NoId>;

    /// Set effect ID (REQUIRED - moves to HasId state)
    ///
    /// Effect IDs must be:
    /// - Unique per event type
    /// - Stable across deploys
    /// - Descriptive (e.g., "send_email", "charge_payment")
    pub fn id(self, id: &str) -> EffectBuilder<E, HasId>;

    /// Extract data from enum events (optional filter)
    ///
    /// # Example
    /// ```rust
    /// effect::on::<OrderEvent>()
    ///     .extract(|e| match e {
    ///         OrderEvent::Placed { order_id, .. } => Some(order_id.clone()),
    ///         _ => None,
    ///     })
    ///     .id("send_email")
    ///     .then(|order_id, ctx| async move { ... });
    /// ```
    pub fn extract<T, F>(self, extractor: F) -> EffectBuilder<T, NoId>
    where
        F: Fn(&E) -> Option<T> + Send + Sync + 'static;

    /// Filter events before processing
    ///
    /// # Example
    /// ```rust
    /// effect::on::<OrderPlaced>()
    ///     .filter(|e| e.amount > 100.0)  // Only high-value orders
    ///     .id("fraud_check")
    ///     .then(|event, ctx| async move { ... });
    /// ```
    pub fn filter<F>(self, predicate: F) -> EffectBuilder<E, NoId>
    where
        F: Fn(&E) -> bool + Send + Sync + 'static;
}

// ============================================================
// STEP 2: Configure Execution (HasId state)
// ============================================================

impl<E> EffectBuilder<E, HasId>
where
    E: Event,
{
    // -------------------------
    // Timing Control
    // -------------------------

    /// Delay effect execution
    ///
    /// # Example
    /// ```rust
    /// effect::on::<OrderPlaced>()
    ///     .id("send_reminder")
    ///     .delayed(Duration::from_days(7))  // ← Run in 7 days
    ///     .then(|event, ctx| async move { ... });
    /// ```
    pub fn delayed(self, duration: Duration) -> Self;

    /// Schedule at specific time
    ///
    /// # Example
    /// ```rust
    /// use chrono::Utc;
    ///
    /// let midnight = Utc::today().and_hms(0, 0, 0);
    /// effect::on::<DailyReport>()
    ///     .id("generate_report")
    ///     .scheduled_at(midnight)
    ///     .then(|event, ctx| async move { ... });
    /// ```
    pub fn scheduled_at(self, time: DateTime<Utc>) -> Self;

    // -------------------------
    // Execution Constraints
    // -------------------------

    /// Set timeout (default: 30s)
    ///
    /// # Example
    /// ```rust
    /// effect::on::<LongRunningJob>()
    ///     .id("process_video")
    ///     .timeout(Duration::from_secs(300))  // 5 minutes
    ///     .then(|event, ctx| async move { ... });
    /// ```
    pub fn timeout(self, duration: Duration) -> Self;

    /// Set max retry attempts (default: 3)
    ///
    /// # Example
    /// ```rust
    /// effect::on::<PaymentCharge>()
    ///     .id("charge_stripe")
    ///     .retry(5)  // Try up to 5 times
    ///     .then(|event, ctx| async move { ... });
    /// ```
    pub fn retry(self, attempts: u32) -> Self;

    /// Set priority (lower = higher priority, default: 10)
    ///
    /// Priority affects effect worker polling order, not event order.
    ///
    /// # Example
    /// ```rust
    /// // High priority - urgent alerts
    /// effect::on::<SystemAlert>()
    ///     .id("send_pagerduty")
    ///     .priority(1)  // ← Execute first
    ///     .then(|event, ctx| async move { ... });
    ///
    /// // Normal priority
    /// effect::on::<UserSignedUp>()
    ///     .id("send_welcome")
    ///     .priority(10)  // ← Default
    ///     .then(|event, ctx| async move { ... });
    ///
    /// // Low priority - background work
    /// effect::on::<DataExport>()
    ///     .id("export_csv")
    ///     .priority(20)  // ← Execute last
    ///     .then(|event, ctx| async move { ... });
    /// ```
    pub fn priority(self, priority: i32) -> Self;

    /// Mark effect as stateless (skip state loading)
    ///
    /// Use for: logging, metrics, observers that don't need state
    ///
    /// # Example
    /// ```rust
    /// effect::on::<OrderPlaced>()
    ///     .id("log_order")
    ///     .stateless(true)  // ← Skip state load (performance)
    ///     .then(|event, ctx| async move {
    ///         tracing::info!("Order placed: {}", event.order_id);
    ///         Ok(())
    ///     });
    /// ```
    pub fn stateless(self, stateless: bool) -> Self;

    // -------------------------
    // Execute Handler
    // -------------------------

    /// Attach the effect handler (final step)
    ///
    /// Handler signature: `|event, ctx| async move { ... }`
    /// - Returns: `Result<impl Event>` to dispatch next event
    /// - Returns: `Result<()>` to complete without emitting
    ///
    /// # Example
    /// ```rust
    /// effect::on::<OrderPlaced>()
    ///     .id("send_confirmation")
    ///     .then(|event, ctx| async move {
    ///         ctx.deps().mailer.send_confirmation(&event).await?;
    ///         Ok(EmailSent { order_id: event.order_id })
    ///     });
    /// ```
    pub fn then<H, Fut, O>(self, handler: H) -> Effect<S, D>
    where
        H: Fn(E, EffectContext<S, D>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<O>> + Send + 'static,
        O: Into<Option<Event>> + Send + 'static;
}
```

---

## 3. EffectContext API (What Effects See)

Effects receive a context with dependencies, state, and metadata.

```rust
pub struct EffectContext<S, D> {
    // Private fields
}

impl<S, D> EffectContext<S, D>
where
    S: Clone + Send + Sync + 'static,
    D: Send + Sync + 'static,
{
    // ============================================================
    // State Access (Latest, Not Snapshot)
    // ============================================================

    /// Get current state (loaded fresh from DB)
    ///
    /// **Semantic Contract**: Effects see LATEST state, not snapshot.
    /// If state changed between event creation and effect execution,
    /// you get the updated state.
    ///
    /// # Example
    /// ```rust
    /// effect::on::<OrderPlaced>()
    ///     .id("send_email")
    ///     .then(|event, ctx| async move {
    ///         let state = ctx.state();  // ← Fresh from DB
    ///         ctx.deps().mailer.send(state.user_email).await?;
    ///         Ok(())
    ///     });
    /// ```
    pub fn state(&self) -> &S;

    // ============================================================
    // Dependencies
    // ============================================================

    /// Get shared dependencies (DB, HTTP clients, etc.)
    ///
    /// # Example
    /// ```rust
    /// effect::on::<PaymentCharge>()
    ///     .id("charge_stripe")
    ///     .then(|event, ctx| async move {
    ///         ctx.deps().stripe.charge(
    ///             event.amount,
    ///             &ctx.idempotency_key()
    ///         ).await?;
    ///         Ok(PaymentCharged { order_id: event.order_id })
    ///     });
    /// ```
    pub fn deps(&self) -> &Arc<D>;

    // ============================================================
    // Envelope Metadata (From Queue)
    // ============================================================

    /// Get saga ID (workflow identifier)
    ///
    /// All events emitted by this effect inherit this saga_id.
    ///
    /// # Example
    /// ```rust
    /// tracing::info!("Processing saga {}", ctx.saga_id());
    /// ```
    pub fn saga_id(&self) -> Uuid;

    /// Get event ID (unique event identifier)
    ///
    /// # Example
    /// ```rust
    /// tracing::info!("Processing event {}", ctx.event_id());
    /// ```
    pub fn event_id(&self) -> Uuid;

    /// Get idempotency key (for external APIs)
    ///
    /// Generated as: `UUID v5(event_id + effect_id)`
    ///
    /// Use this for Stripe, Twilio, etc. to ensure exactly-once delivery.
    ///
    /// # Example
    /// ```rust
    /// effect::on::<OrderPlaced>()
    ///     .id("charge_payment")
    ///     .then(|event, ctx| async move {
    ///         // Stripe won't double-charge if effect retries
    ///         ctx.deps().stripe.charge(
    ///             event.amount,
    ///             &ctx.idempotency_key()  // ← Framework provides this
    ///         ).await?;
    ///         Ok(PaymentCharged { order_id: event.order_id })
    ///     });
    /// ```
    pub fn idempotency_key(&self) -> String;

    /// Get effect name (for logging/debugging)
    ///
    /// # Example
    /// ```rust
    /// tracing::info!("Running effect: {}", ctx.effect_name());
    /// ```
    pub fn effect_name(&self) -> &str;

    // ============================================================
    // Concurrency Control
    // ============================================================

    /// Spawn tracked sub-tasks (for parallel IO)
    ///
    /// # Example
    /// ```rust
    /// effect::on::<BatchProcess>()
    ///     .id("process_batch")
    ///     .then(|event, ctx| async move {
    ///         let mut handles = vec![];
    ///
    ///         for item in event.items {
    ///             let handle = ctx.within(|| async move {
    ///                 process_item(item).await
    ///             });
    ///             handles.push(handle);
    ///         }
    ///
    ///         futures::future::join_all(handles).await;
    ///         Ok(BatchComplete)
    ///     });
    /// ```
    pub fn within<F, Fut, T>(&self, f: F) -> JoinHandle<T>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = T> + Send + 'static,
        T: Send + 'static;
}
```

---

## 4. Reducer API (Pure State Transitions)

Reducers transform state before effects run. They are pure functions.

```rust
pub struct Reducer<S> {
    // Internal fields private
}

impl<S> Reducer<S>
where
    S: Clone + Send + Sync + 'static,
{
    /// Create a reducer for a specific event type
    ///
    /// # Example
    /// ```rust
    /// use seesaw::reducer;
    ///
    /// let reducer = reducer::on::<OrderPlaced>()
    ///     .run(|state, event| {
    ///         State {
    ///             order_count: state.order_count + 1,
    ///             total_revenue: state.total_revenue + event.amount,
    ///             ..state
    ///         }
    ///     });
    /// ```
    pub fn on<E>() -> ReducerBuilder<S, E>
    where
        E: Event;
}

pub struct ReducerBuilder<S, E> {
    // Internal fields private
}

impl<S, E> ReducerBuilder<S, E>
where
    S: Clone + Send + Sync + 'static,
    E: Event,
{
    /// Attach the reducer function (pure state transformation)
    ///
    /// # Example
    /// ```rust
    /// reducer::on::<OrderPlaced>()
    ///     .run(|state, event| {
    ///         State {
    ///             order_count: state.order_count + 1,
    ///             ..state
    ///         }
    ///     });
    /// ```
    pub fn run<F>(self, reducer: F) -> Reducer<S>
    where
        F: Fn(&S, &E) -> S + Send + Sync + 'static;
}
```

---

## 5. PostgresQueue API (Infrastructure)

The queue manages durable event storage and worker orchestration.

```rust
pub struct PostgresQueue {
    // Internal fields private
}

impl PostgresQueue {
    /// Create a new queue
    pub fn new(pool: PgPool) -> Self;

    /// Publish an event to the queue
    ///
    /// # Example
    /// ```rust
    /// queue.publish(EventEnvelope {
    ///     event_id: Uuid::new_v4(),
    ///     saga_id,
    ///     event_type: "OrderPlaced",
    ///     payload: json!(event),
    ///     priority: 10,
    ///     hops: 0,
    /// }).await?;
    /// ```
    pub async fn publish(&self, envelope: EventEnvelope) -> Result<()>;

    // ============================================================
    // DLQ Management (Operational, Not Domain Logic)
    // ============================================================

    /// List failed effects in dead letter queue
    ///
    /// # Example
    /// ```rust
    /// let failed = queue.list_dlq().await?;
    /// for entry in failed {
    ///     println!("Failed: {:?} - {}", entry.effect_id, entry.error);
    /// }
    /// ```
    pub async fn list_dlq(&self) -> Result<Vec<DlqEntry>>;

    /// Retry a failed effect from DLQ
    ///
    /// Moves effect from DLQ back to effect_executions table.
    ///
    /// # Example
    /// ```rust
    /// queue.retry_from_dlq(event_id, "send_email").await?;
    /// ```
    pub async fn retry_from_dlq(
        &self,
        event_id: Uuid,
        effect_id: &str
    ) -> Result<()>;

    /// Mark DLQ entry as resolved (delete without retry)
    ///
    /// # Example
    /// ```rust
    /// queue.resolve_dlq(event_id, "send_email").await?;
    /// ```
    pub async fn resolve_dlq(
        &self,
        event_id: Uuid,
        effect_id: &str
    ) -> Result<()>;

    // ============================================================
    // Saga Introspection (Progress Tracking)
    // ============================================================

    /// Get current state for a saga
    ///
    /// # Example
    /// ```rust
    /// let state: CrawlState = queue.get_saga_state(saga_id).await?
    ///     .ok_or_else(|| anyhow!("Saga not found"))?;
    /// println!("Pages crawled: {}/{}", state.pages_crawled, state.pages_total);
    /// ```
    pub async fn get_saga_state<S>(&self, saga_id: Uuid) -> Result<Option<S>>
    where
        S: serde::de::DeserializeOwned;

    /// Get all effect executions for a saga
    ///
    /// Returns detailed information about each effect (status, attempts, errors).
    ///
    /// # Example
    /// ```rust
    /// let effects = queue.list_saga_effects(saga_id).await?;
    /// for effect in effects {
    ///     println!("{}: {} (attempts: {})",
    ///         effect.effect_id,
    ///         effect.status,
    ///         effect.attempts
    ///     );
    /// }
    /// ```
    pub async fn list_saga_effects(&self, saga_id: Uuid) -> Result<Vec<EffectExecution>>;

    /// Get saga summary (effect counts by status)
    ///
    /// Efficient single query that returns counts for pending, executing,
    /// completed, and failed effects.
    ///
    /// # Example
    /// ```rust
    /// let summary = queue.get_saga_summary(saga_id).await?;
    /// let progress = summary.completed as f64 /
    ///     (summary.completed + summary.pending + summary.executing) as f64;
    /// println!("Progress: {:.0}%", progress * 100.0);
    /// ```
    pub async fn get_saga_summary(&self, saga_id: Uuid) -> Result<SagaSummary>;

    /// List all active sagas (have pending/executing effects)
    ///
    /// Useful for admin dashboards and monitoring.
    ///
    /// # Example
    /// ```rust
    /// let sagas = queue.list_active_sagas().await?;
    /// println!("Active sagas: {}", sagas.len());
    /// for saga in sagas {
    ///     println!("  {} - {} effects", saga.saga_id, saga.total_effects);
    /// }
    /// ```
    pub async fn list_active_sagas(&self) -> Result<Vec<SagaInfo>>;

    // ============================================================
    // Health Monitoring
    // ============================================================

    /// Check reaper health (for load balancer)
    ///
    /// Returns 200 if reaper ran in last 3 minutes, 503 otherwise.
    ///
    /// # Example
    /// ```rust
    /// // GET /health/reaper
    /// pub async fn health_check(queue: &PostgresQueue) -> StatusCode {
    ///     queue.reaper_health().await
    /// }
    /// ```
    pub async fn reaper_health(&self) -> Result<ReaperHealth>;
}

pub struct DlqEntry {
    pub event_id: Uuid,
    pub effect_id: String,
    pub saga_id: Uuid,
    pub error: String,
    pub payload: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

pub struct ReaperHealth {
    pub last_run: DateTime<Utc>,
    pub seconds_since_last_run: i64,
    pub is_healthy: bool,  // false if > 180 seconds
}
```

---

## 6. Runtime API (Orchestration)

The runtime owns the engine, queue, and workers.

```rust
pub struct Runtime<S, D> {
    // Internal fields private
}

impl<S, D> Runtime<S, D>
where
    S: Clone + Send + Sync + 'static,
    D: Send + Sync + 'static,
{
    /// Create a new runtime
    ///
    /// # Example
    /// ```rust
    /// let engine = Engine::new(deps)
    ///     .with_effect(send_email_effect)
    ///     .with_reducer(order_reducer);
    ///
    /// let queue = PostgresQueue::new(pool);
    ///
    /// let runtime = Runtime::new(engine, queue);
    /// ```
    pub fn new(engine: Engine<S, D>, queue: Arc<PostgresQueue>) -> Self;

    /// Spawn worker pools
    ///
    /// # Arguments
    /// - `event_workers`: Number of event workers (state transitions)
    /// - `effect_workers`: Number of effect workers (side effects)
    ///
    /// # Example
    /// ```rust
    /// runtime.spawn_workers(
    ///     2,   // 2 event workers (CPU-bound)
    ///     20,  // 20 effect workers (IO-bound)
    /// ).await?;
    /// ```
    pub async fn spawn_workers(
        &mut self,
        event_workers: usize,
        effect_workers: usize,
    ) -> Result<()>;

    /// Graceful shutdown - wait for workers to drain
    ///
    /// # Example
    /// ```rust
    /// tokio::select! {
    ///     _ = tokio::signal::ctrl_c() => {
    ///         runtime.shutdown().await?;
    ///     }
    /// }
    /// ```
    pub async fn shutdown(self) -> Result<()>;

    /// Get engine reference (for processing external events)
    ///
    /// # Example
    /// ```rust
    /// runtime.engine().process(OrderPlaced { ... }).await?;
    /// ```
    pub fn engine(&self) -> &Engine<S, D>;

    /// Get queue reference (for DLQ management)
    ///
    /// # Example
    /// ```rust
    /// let dlq = runtime.queue().list_dlq().await?;
    /// ```
    pub fn queue(&self) -> &Arc<PostgresQueue>;
}
```

---

## 7. Complete Usage Example

```rust
use seesaw::{Engine, Runtime, PostgresQueue, effect, reducer};
use sqlx::PgPool;
use anyhow::Result;

// ============================================================
// 1. Define Events
// ============================================================

#[derive(Debug, Clone)]
enum OrderEvent {
    Placed { order_id: String, amount: f64 },
    PaymentCharged { order_id: String },
    Fulfilled { order_id: String },
}

// ============================================================
// 2. Define State
// ============================================================

#[derive(Debug, Clone)]
struct State {
    order_count: u64,
    total_revenue: f64,
}

// ============================================================
// 3. Define Dependencies
// ============================================================

struct Deps {
    db: PgPool,
    stripe: StripeClient,
    mailer: MailerClient,
}

// ============================================================
// 4. Build Engine
// ============================================================

fn build_engine(deps: Deps) -> Engine<State, Deps> {
    Engine::new(deps)
        // Reducer: pure state transformation
        .with_reducer(
            reducer::on::<OrderEvent>()
                .run(|state, event| match event {
                    OrderEvent::Placed { amount, .. } => State {
                        order_count: state.order_count + 1,
                        total_revenue: state.total_revenue + amount,
                    },
                    _ => state.clone(),
                })
        )

        // Effect: charge payment
        .with_effect(
            effect::on::<OrderEvent>()
                .extract(|e| match e {
                    OrderEvent::Placed { order_id, amount } => {
                        Some((order_id.clone(), *amount))
                    },
                    _ => None,
                })
                .id("charge_payment")
                .timeout(Duration::from_secs(30))
                .retry(5)
                .priority(5)  // High priority
                .then(|(order_id, amount), ctx| async move {
                    ctx.deps().stripe.charge(
                        amount,
                        &ctx.idempotency_key()
                    ).await?;

                    Ok(OrderEvent::PaymentCharged { order_id })
                })
        )

        // Effect: send confirmation email
        .with_effect(
            effect::on::<OrderEvent>()
                .extract(|e| match e {
                    OrderEvent::PaymentCharged { order_id } => {
                        Some(order_id.clone())
                    },
                    _ => None,
                })
                .id("send_confirmation")
                .then(|order_id, ctx| async move {
                    let state = ctx.state();
                    ctx.deps().mailer.send_confirmation(
                        &state.user_email,
                        &order_id
                    ).await?;

                    Ok(OrderEvent::Fulfilled { order_id })
                })
        )
}

// ============================================================
// 5. Setup Runtime
// ============================================================

#[tokio::main]
async fn main() -> Result<()> {
    let pool = PgPool::connect(&env::var("DATABASE_URL")?).await?;

    let deps = Deps {
        db: pool.clone(),
        stripe: StripeClient::new(),
        mailer: MailerClient::new(),
    };

    let engine = build_engine(deps);
    let queue = Arc::new(PostgresQueue::new(pool));

    let mut runtime = Runtime::new(engine, queue);

    // Spawn workers
    runtime.spawn_workers(
        2,   // 2 event workers
        20,  // 20 effect workers
    ).await?;

    // Handle graceful shutdown
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("Shutting down...");
            runtime.shutdown().await?;
        }
    }

    Ok(())
}

// ============================================================
// 6. Process External Events
// ============================================================

async fn handle_webhook(
    runtime: &Runtime<State, Deps>,
    payload: StripeWebhook,
) -> Result<()> {
    // Use webhook ID for idempotency
    let webhook_id = Uuid::parse_str(&payload.id)?;

    runtime.engine().process_with_id(
        webhook_id,
        OrderEvent::Placed {
            order_id: payload.order_id,
            amount: payload.amount,
        }
    ).await?;

    Ok(())
}
```

---

## 8. Macro Syntax (Coming Soon)

For cleaner multi-variant event matching:

```rust
use seesaw::on;

let effects = on! {
    // Match multiple variants
    OrderEvent::Placed { order_id, .. } |
    OrderEvent::Updated { order_id, .. } => {
        #[effect(id = "sync", priority = 10)]
        |order_id, ctx| async move {
            ctx.deps().db.sync(order_id).await?;
            Ok(())
        }
    },

    // Single variant with config
    OrderEvent::PaymentCharged { order_id } => {
        #[effect(id = "fulfill", delayed = 1hour, retry = 5)]
        |order_id, ctx| async move {
            ctx.deps().fulfillment.ship(order_id).await?;
            Ok(OrderEvent::Fulfilled { order_id })
        }
    },
};

// Add to engine
let engine = effects.into_iter()
    .fold(Engine::new(deps), |e, eff| e.with_effect(eff));
```

---

## 9. Type-State Enforcement

The API uses type-states to prevent invalid usage at compile time:

```rust
// ❌ COMPILE ERROR: .id() required
effect::on::<OrderPlaced>()
    .then(|event, ctx| async move { ... });
//  ^^^^ Error: no method `then` found for type `EffectBuilder<OrderPlaced, NoId>`

// ✅ COMPILES: .id() called
effect::on::<OrderPlaced>()
    .id("send_email")  // ← Moves to HasId state
    .then(|event, ctx| async move { ... });
```

---

## Summary

**Clean Boundaries**:
- Engine: Entry point for external events (`process`, `process_with_id`)
- Effects: Side-effect handlers with type-safe builder
- Queue: Infrastructure (DLQ, health checks)
- Runtime: Orchestration (owns workers)

**No More**:
- ❌ `process_with_priority()` - priority is on effects
- ❌ `process_saga_with_priority()` - removed
- ❌ Engine DLQ methods - moved to Queue
- ❌ `ctx.prev_state()` / `ctx.next_state()` - only latest state

**Invariants Enforced**:
- ✅ Effect ID required at compile time
- ✅ Latest state semantics (no snapshots)
- ✅ Idempotency via deterministic event IDs
- ✅ Clear separation of concerns
