# Brainstorm: Addressing the Five Critical Gaps

**Date:** 2026-02-27
**Context:** Deep dive into fixing the five gaps identified in our event sourcing design

## Overview of Gaps

1. 🔴 **Replay Tooling** - No way to actually trigger replays
2. 🟡 **Mid-Chain Failure Recovery** - Events 100-103 persisted, 104 fails, no compensation
3. 🔴 **Event Schema Evolution** - Have `schema_v`, need upcaster logic
4. 🟡 **Snapshots** - Full replay from event 0
5. 🔴 **Replay-Aware Gateways** - Non-idempotent side effects trigger on replay

## Gap #1: Replay Tooling (CRITICAL)

### The Problem

Fowler: "Event Sourcing allows you to replay events... correct past errors by reversing and replaying events with corrections."

**Current State:**
- ✅ Have append-only event log
- ✅ Have idempotent MERGE operations
- ✅ Have causal chains (correlation_id)
- ❌ No API to actually trigger a replay
- ❌ No way to tell if we're in replay mode

**Impact:** Biggest missed opportunity. Can't leverage core ES capability.

### Solution Design

#### API: Three Replay Modes

```rust
/// Event store replay capabilities
pub trait EventStore {
    /// Replay all events for a single aggregate (rebuild from scratch)
    async fn replay_aggregate<A>(
        &self,
        aggregate_id: A::Id,
        context: ReplayContext,
    ) -> Result<A>
    where
        A: Aggregate;

    /// Replay events in a time range (fix bugs, test changes)
    async fn replay_range(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        context: ReplayContext,
    ) -> Result<ReplayStats>;

    /// Replay specific events by ID (surgical fixes)
    async fn replay_events(
        &self,
        event_ids: Vec<Uuid>,
        context: ReplayContext,
    ) -> Result<ReplayStats>;
}

#[derive(Default)]
pub struct ReplayStats {
    pub events_processed: usize,
    pub aggregates_rebuilt: usize,
    pub errors: Vec<ReplayError>,
    pub duration: Duration,
}

#[derive(Debug)]
pub struct ReplayError {
    pub event_id: Uuid,
    pub aggregate_id: Uuid,
    pub error: String,
}
```

#### ReplayContext: Track Replay Mode

```rust
/// Context passed through the entire replay operation
#[derive(Clone)]
pub struct ReplayContext {
    /// Are we replaying or executing live?
    pub mode: ReplayMode,

    /// Original timestamp of events (for deterministic external queries)
    pub original_timestamp: Option<DateTime<Utc>>,

    /// Replay strategy
    pub strategy: ReplayStrategy,

    /// Metadata about the replay operation
    pub metadata: ReplayMetadata,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplayMode {
    /// Live execution - all side effects happen
    Live,

    /// Replaying events - side effects suppressed
    Replay,

    /// Dry run - no state changes, just validation
    DryRun,
}

#[derive(Clone, Debug)]
pub enum ReplayStrategy {
    /// Rebuild state from scratch (drop existing state)
    FullRebuild,

    /// Apply events on top of current state
    Incremental,

    /// Validate events can be applied without actually applying
    Validate,
}

#[derive(Clone, Debug, Default)]
pub struct ReplayMetadata {
    pub triggered_by: Option<String>,
    pub reason: Option<String>,
    pub started_at: DateTime<Utc>,
}
```

#### Example Usage

```rust
// Scenario 1: Rebuild an aggregate from scratch
let context = ReplayContext {
    mode: ReplayMode::Replay,
    strategy: ReplayStrategy::FullRebuild,
    ..Default::default()
};

let order: Order = store
    .replay_aggregate(order_id, context)
    .await?;

// Scenario 2: Fix a bug by replaying yesterday's events
let context = ReplayContext {
    mode: ReplayMode::Replay,
    strategy: ReplayStrategy::Incremental,
    metadata: ReplayMetadata {
        triggered_by: Some("ops-team".into()),
        reason: Some("Fix pricing bug in v0.2.1".into()),
        ..Default::default()
    },
    ..Default::default()
};

let stats = store
    .replay_range(
        Utc::now() - Duration::days(1),
        Utc::now(),
        context,
    )
    .await?;

println!("Replayed {} events across {} aggregates",
    stats.events_processed,
    stats.aggregates_rebuilt
);

// Scenario 3: Dry run to test new projection logic
let context = ReplayContext {
    mode: ReplayMode::DryRun,
    strategy: ReplayStrategy::Validate,
    ..Default::default()
};

let stats = store
    .replay_aggregate(order_id, context)
    .await?;

if stats.errors.is_empty() {
    println!("✅ All events apply cleanly with new logic");
} else {
    println!("❌ {} errors found", stats.errors.len());
}
```

#### Implementation for PostgreSQL

```rust
impl<E> EventStore for PostgresEventStore<E>
where
    E: Clone + Serialize + DeserializeOwned + Send + Sync + 'static,
{
    async fn replay_aggregate<A>(
        &self,
        aggregate_id: A::Id,
        context: ReplayContext,
    ) -> Result<A>
    where
        A: Aggregate,
    {
        // Load all events for aggregate
        let events = self.load_events(aggregate_id.clone(), 0).await?;

        // Start with blank state
        let mut aggregate = A::default();

        // Apply each event through reactors
        for stored_event in events {
            // Reactors can check context.mode to suppress side effects
            for reactor in &reactors {
                reactor.apply_with_context(&mut aggregate, &stored_event.data, &context)?;
            }

            aggregate.set_version(stored_event.sequence);
        }

        Ok(aggregate)
    }

    async fn replay_range(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        context: ReplayContext,
    ) -> Result<ReplayStats> {
        let mut stats = ReplayStats::default();
        let start_time = Instant::now();

        // Query all events in time range
        let events = sqlx::query!(
            r#"
            SELECT DISTINCT aggregate_id, aggregate_type
            FROM causal_event_store
            WHERE timestamp >= $1 AND timestamp <= $2
            "#,
            start,
            end,
        )
        .fetch_all(&self.pool)
        .await?;

        // Replay each affected aggregate
        for row in events {
            match self.replay_aggregate_by_id(row.aggregate_id, context.clone()).await {
                Ok(_) => {
                    stats.aggregates_rebuilt += 1;
                }
                Err(e) => {
                    stats.errors.push(ReplayError {
                        event_id: Uuid::nil(), // Don't have specific event
                        aggregate_id: row.aggregate_id,
                        error: e.to_string(),
                    });
                }
            }
        }

        stats.duration = start_time.elapsed();
        Ok(stats)
    }
}
```

### Key Design Decisions

**Q: Should replay be synchronous or background job?**
A: Both! Synchronous for single aggregates, background for ranges.

```rust
// Synchronous (immediate feedback)
let order = store.replay_aggregate(order_id, context).await?;

// Background (for large ranges)
let job_id = store.replay_range_async(start, end, context).await?;
let status = store.replay_job_status(job_id).await?;
```

**Q: What if replay fails mid-way?**
A: Depends on strategy:
- `FullRebuild`: Transactional (all-or-nothing)
- `Incremental`: Continue on errors, report via `ReplayStats`
- `Validate`: No state changes, just collect errors

**Q: How do reactors know they're in replay mode?**
A: Pass `ReplayContext` through reactor signature:

```rust
#[reactor(on = OrderEvent, replay_aware)]
fn apply_placed(state: &mut Order, ctx: &ReplayContext) -> Result<()> {
    state.status = OrderStatus::Placed;

    if ctx.mode == ReplayMode::Live {
        // Only log during live execution
        tracing::info!("Order placed: {}", state.id);
    }

    Ok(())
}
```

---

## Gap #5: Replay-Aware Gateways (CRITICAL)

### The Problem

Fowler: "Wrap any external systems with a Gateway so that when replaying events the Gateway can detect it's in replay mode and not actually send the notification."

**Current State:**
- ✅ Operations are idempotent (MERGE)
- ❌ No gateway pattern for external systems
- ❌ Non-idempotent ops (email, webhooks) would trigger on replay

**Example of the Problem:**
```rust
impl ShipOrder {
    async fn execute(&self, store: &EventStore, deps: &Deps) -> Result<Vec<Event>> {
        // ❌ PROBLEM: This will ACTUALLY ship again during replay!
        let tracking = deps.shipping_api.ship(self.order_id).await?;

        // ❌ PROBLEM: Customer gets duplicate email during replay!
        deps.email.send_shipped_notification(self.order_id).await?;

        Ok(vec![OrderEvent::Shipped { tracking }])
    }
}
```

### Solution Design

#### Gateway Pattern

```rust
/// Trait for all external system interactions
pub trait ExternalGateway: Clone + Send + Sync {
    type Output: Send + 'static;

    /// Execute operation, checking replay context
    async fn execute<F, Fut>(
        &self,
        ctx: &ReplayContext,
        op: F,
    ) -> Result<Self::Output>
    where
        F: FnOnce() -> Fut + Send,
        Fut: Future<Output = Result<Self::Output>> + Send;
}

/// Gateway for shipping API
#[derive(Clone)]
pub struct ShippingGateway {
    api: ShippingApi,
}

impl ExternalGateway for ShippingGateway {
    type Output = String; // tracking number

    async fn execute<F, Fut>(
        &self,
        ctx: &ReplayContext,
        op: F,
    ) -> Result<String>
    where
        F: FnOnce() -> Fut + Send,
        Fut: Future<Output = Result<String>> + Send,
    {
        match ctx.mode {
            ReplayMode::Live => {
                // Actually execute the operation
                op().await
            }
            ReplayMode::Replay | ReplayMode::DryRun => {
                // During replay, return cached result or error
                Err(anyhow!("External call suppressed during replay"))
            }
        }
    }
}

/// Gateway for email service
#[derive(Clone)]
pub struct EmailGateway {
    service: EmailService,
}

impl ExternalGateway for EmailGateway {
    type Output = ();

    async fn execute<F, Fut>(
        &self,
        ctx: &ReplayContext,
        op: F,
    ) -> Result<()>
    where
        F: FnOnce() -> Fut + Send,
        Fut: Future<Output = Result<()>> + Send,
    {
        match ctx.mode {
            ReplayMode::Live => op().await,
            ReplayMode::Replay | ReplayMode::DryRun => {
                tracing::debug!("Email suppressed during replay");
                Ok(()) // Silently succeed
            }
        }
    }
}
```

#### Usage in Commands

```rust
#[derive(Clone)]
struct Deps {
    shipping: ShippingGateway,  // Wrapped in gateway
    email: EmailGateway,        // Wrapped in gateway
}

struct ShipOrder {
    order_id: Uuid,
}

impl ShipOrder {
    async fn execute(
        &self,
        store: &EventStore,
        deps: &Deps,
        ctx: &ReplayContext,  // ← Pass context through
    ) -> Result<Vec<OrderEvent>> {
        // Load aggregate
        let order: Order = store.aggregate(self.order_id).load().await?;

        // Validate
        ensure!(order.status == OrderStatus::Paid, "Order must be paid");

        // ✅ Gateway prevents actual call during replay
        let tracking = deps.shipping
            .execute(ctx, || async {
                deps.shipping.api.ship(self.order_id).await
            })
            .await?;

        // ✅ Email suppressed during replay
        deps.email
            .execute(ctx, || async {
                deps.email.service.send_shipped_notification(self.order_id).await
            })
            .await?;

        // Events are still generated (for deterministic replay)
        Ok(vec![OrderEvent::Shipped {
            order_id: self.order_id,
            tracking,
        }])
    }
}
```

#### Smart Gateway: Cache Results

```rust
/// Gateway that caches results during live execution for replay
pub struct CachedGateway<G: ExternalGateway> {
    inner: G,
    cache: Arc<DashMap<String, G::Output>>,
}

impl<G: ExternalGateway> CachedGateway<G> {
    pub fn new(inner: G) -> Self {
        Self {
            inner,
            cache: Arc::new(DashMap::new()),
        }
    }
}

impl<G: ExternalGateway> ExternalGateway for CachedGateway<G>
where
    G::Output: Clone,
{
    type Output = G::Output;

    async fn execute<F, Fut>(
        &self,
        ctx: &ReplayContext,
        op: F,
    ) -> Result<G::Output>
    where
        F: FnOnce() -> Fut + Send,
        Fut: Future<Output = Result<G::Output>> + Send,
    {
        // Generate cache key from context
        let cache_key = format!("{:?}_{:?}", ctx.original_timestamp, ctx.metadata);

        match ctx.mode {
            ReplayMode::Live => {
                // Execute and cache result
                let result = op().await?;
                self.cache.insert(cache_key, result.clone());
                Ok(result)
            }
            ReplayMode::Replay | ReplayMode::DryRun => {
                // Return cached result
                self.cache
                    .get(&cache_key)
                    .map(|v| v.clone())
                    .ok_or_else(|| anyhow!("Cached result not found for replay"))
            }
        }
    }
}

// Usage
let shipping = CachedGateway::new(ShippingGateway { api });
```

### Alternative: Log Results in Events

Fowler's preferred approach - store external query results directly in events:

```rust
// Instead of:
enum OrderEvent {
    Shipped { tracking: String },
}

// Do this:
enum OrderEvent {
    Shipped {
        tracking: String,
        shipping_cost: f64,        // ← Query result logged
        exchange_rate: f64,        // ← Query result logged
        queried_at: DateTime<Utc>, // ← When we queried
    },
}

// Command execution
impl ShipOrder {
    async fn execute(&self, ...) -> Result<Vec<OrderEvent>> {
        // Query external systems
        let tracking = deps.shipping.ship(self.order_id).await?;
        let cost = deps.shipping.get_cost(self.order_id).await?;
        let rate = deps.currency.get_rate("USD", "EUR").await?;

        // ✅ Log ALL query results in the event
        Ok(vec![OrderEvent::Shipped {
            tracking,
            shipping_cost: cost,      // Stored for replay
            exchange_rate: rate,      // Stored for replay
            queried_at: Utc::now(),   // Stored for replay
        }])
    }
}
```

**Tradeoff:** Events get larger, but replay is deterministic without gateways.

---

## Gap #3: Event Schema Evolution (CRITICAL)

### The Problem

**Current State:**
- ✅ Have `schema_v` on every stored event
- ❌ No upcaster logic to transform old event shapes during replay

**Example:**
```rust
// V1: Original event
enum OrderEvent {
    Placed { order_id: Uuid, total: f64 },
}

// V2: Added currency field
enum OrderEvent {
    Placed { order_id: Uuid, total: f64, currency: String },
}

// ❌ Old events can't deserialize into new enum!
```

### Solution Design

#### Upcasting Trait

```rust
/// Trait for upcasting events from old versions to current
pub trait EventUpcast: Sized {
    /// Upcast from an older version
    fn upcast_from(version: u32, data: Value) -> Result<Self>;

    /// Current version of this event type
    fn current_version() -> u32;
}

/// Helper macro for implementing upcasting
#[macro_export]
macro_rules! impl_upcast {
    ($event_type:ty, $current_version:expr, {
        $($version:expr => $upcast_fn:expr),* $(,)?
    }) => {
        impl EventUpcast for $event_type {
            fn current_version() -> u32 {
                $current_version
            }

            fn upcast_from(version: u32, data: Value) -> Result<Self> {
                match version {
                    $($version => $upcast_fn(data),)*
                    v if v == Self::current_version() => {
                        serde_json::from_value(data)
                            .map_err(|e| anyhow!("Failed to deserialize: {}", e))
                    }
                    _ => Err(anyhow!("Unknown event version: {}", version)),
                }
            }
        }
    };
}
```

#### Example: OrderEvent Evolution

```rust
// V1 schema (legacy)
#[derive(Serialize, Deserialize)]
struct OrderPlacedV1 {
    order_id: Uuid,
    total: f64,
}

// V2 schema (added currency)
#[derive(Serialize, Deserialize)]
struct OrderPlacedV2 {
    order_id: Uuid,
    total: f64,
    currency: String,
}

// V3 schema (added items array)
#[derive(Serialize, Deserialize)]
struct OrderPlacedV3 {
    order_id: Uuid,
    total: f64,
    currency: String,
    items: Vec<String>,
}

// Current event type
#[derive(Clone, Serialize, Deserialize)]
enum OrderEvent {
    Placed {
        order_id: Uuid,
        total: f64,
        currency: String,
        items: Vec<String>,
    },
}

// Implement upcasting
impl_upcast!(OrderEvent, 3, {
    1 => |data: Value| {
        let v1: OrderPlacedV1 = serde_json::from_value(data)?;
        Ok(OrderEvent::Placed {
            order_id: v1.order_id,
            total: v1.total,
            currency: "USD".into(),  // Default for old events
            items: vec![],           // Default for old events
        })
    },
    2 => |data: Value| {
        let v2: OrderPlacedV2 = serde_json::from_value(data)?;
        Ok(OrderEvent::Placed {
            order_id: v2.order_id,
            total: v2.total,
            currency: v2.currency,
            items: vec![],  // Default for V2 events
        })
    },
});
```

#### EventStore Integration

```rust
impl<E> EventStore for PostgresEventStore<E>
where
    E: Clone + Serialize + DeserializeOwned + EventUpcast + Send + Sync + 'static,
{
    async fn load_events<Id>(&self, aggregate_id: Id, from_version: u64) -> Result<Vec<StoredEvent<E>>>
    where
        Id: Clone + Send + Sync + 'static + Into<Uuid>,
    {
        let rows = sqlx::query!(
            r#"
            SELECT sequence, event_type, event_data, timestamp, schema_v
            FROM causal_event_store
            WHERE aggregate_id = $1 AND sequence > $2
            ORDER BY sequence ASC
            "#,
            aggregate_id.into(),
            from_version as i64,
        )
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|row| {
                // ✅ Upcast old events to current schema
                let data: E = if row.schema_v < E::current_version() as i32 {
                    E::upcast_from(row.schema_v as u32, row.event_data)?
                } else {
                    serde_json::from_value(row.event_data)?
                };

                Ok(StoredEvent {
                    sequence: row.sequence as u64,
                    timestamp: row.timestamp,
                    data,
                    event_type: row.event_type,
                })
            })
            .collect()
    }
}
```

#### Migration Strategy

**Step 1:** Write upcaster BEFORE changing event schema
```rust
// 1. Implement upcaster for V1 → V2
impl_upcast!(OrderEvent, 2, {
    1 => |data| { /* upcast logic */ },
});

// 2. Test upcaster on production data
#[test]
fn test_upcast_v1_events() {
    let v1_json = json!({
        "order_id": "...",
        "total": 99.99
    });

    let event = OrderEvent::upcast_from(1, v1_json).unwrap();
    assert_eq!(event.currency, "USD");
}

// 3. Deploy upcaster to production
// 4. Change event schema to V2
```

**Step 2:** Verify with dry-run replay
```rust
let ctx = ReplayContext {
    mode: ReplayMode::DryRun,
    ..Default::default()
};

let stats = store.replay_aggregate(order_id, ctx).await?;

if stats.errors.is_empty() {
    println!("✅ All events upcast successfully");
}
```

---

## Gap #2: Mid-Chain Failure Recovery (MEDIUM)

### The Problem

**Scenario:**
1. Event 100 applied ✅
2. Event 101 applied ✅
3. Event 102 applied ✅
4. Event 103 applied ✅
5. Event 104 fails ❌
6. Events 100-103 are already persisted with no automatic compensation

**Current Approach:** Manual investigation required.

### Solution Options

#### Option 1: Transactional Batch Processing

```rust
/// Process events in a transaction - all or nothing
async fn process_batch(&self, aggregate_id: Uuid, events: Vec<Event>) -> Result<()> {
    let mut tx = self.pool.begin().await?;

    for (i, event) in events.iter().enumerate() {
        // Apply through reactors
        match self.apply_event(&mut aggregate, event) {
            Ok(_) => {
                // Store event
                self.store_event_tx(&mut tx, aggregate_id, i, event).await?;
            }
            Err(e) => {
                // Rollback entire batch
                tx.rollback().await?;
                return Err(anyhow!("Event {} failed: {}", i, e));
            }
        }
    }

    tx.commit().await?;
    Ok(())
}
```

**Pro:** Strong consistency
**Con:** Limits throughput (single transaction)

#### Option 2: Dead Letter Queue for Projections

```rust
/// Track projection failures
#[derive(Debug)]
pub struct ProjectionFailure {
    pub event_id: Uuid,
    pub aggregate_id: Uuid,
    pub reactor_id: String,
    pub error: String,
    pub retry_count: u32,
    pub failed_at: DateTime<Utc>,
}

/// DLQ for failed projections
pub trait ProjectionDLQ {
    async fn record_failure(&self, failure: ProjectionFailure) -> Result<()>;
    async fn list_failures(&self) -> Result<Vec<ProjectionFailure>>;
    async fn retry_failure(&self, event_id: Uuid) -> Result<()>;
}

// Usage
impl AggregateLoader {
    pub async fn load_with_dlq(&self, dlq: &impl ProjectionDLQ) -> Result<A> {
        let events = self.store.load_events(self.aggregate_id).await?;

        let mut aggregate = A::default();

        for event in events {
            match self.apply_handlers(&mut aggregate, &event) {
                Ok(_) => { /* continue */ }
                Err(e) => {
                    // Record failure in DLQ
                    dlq.record_failure(ProjectionFailure {
                        event_id: event.id,
                        aggregate_id: self.aggregate_id,
                        reactor_id: "apply_placed".into(),
                        error: e.to_string(),
                        retry_count: 0,
                        failed_at: Utc::now(),
                    }).await?;

                    // Continue processing or bail?
                    // Option A: Stop processing (safe)
                    return Err(e);

                    // Option B: Continue processing (eventual consistency)
                    // continue;
                }
            }
        }

        Ok(aggregate)
    }
}
```

**Pro:** Eventual consistency, can retry later
**Con:** More complex, projections should be pure (why would they fail?)

#### Option 3: Saga-Style Compensation

```rust
/// Compensating events for rollback
enum OrderEvent {
    ItemAdded { item: Item },
    ItemRemoved { item: Item },  // Compensation

    PaymentCharged { amount: f64 },
    PaymentRefunded { amount: f64 },  // Compensation
}

/// Generate compensation events on failure
impl PlaceOrder {
    async fn execute_with_compensation(&self, store: &EventStore) -> Result<Vec<OrderEvent>> {
        let mut applied_events = vec![];

        // Try to apply each event
        for event in self.generate_events()? {
            match store.append_events(self.order_id, self.version, vec![event.clone()]).await {
                Ok(_) => {
                    applied_events.push(event);
                }
                Err(e) => {
                    // Generate compensating events for what we've already done
                    let compensations = applied_events.iter()
                        .rev()
                        .filter_map(|e| e.compensate())
                        .collect();

                    store.append_events(self.order_id, self.version, compensations).await?;

                    return Err(e);
                }
            }
        }

        Ok(applied_events)
    }
}
```

**Pro:** Explicit compensation logic
**Con:** Most complex, requires designing compensations

### Recommendation

**For pure projections (aggregates):** Option 2 (DLQ) - failures should be rare but tracked

**For commands with side effects:** Option 3 (Compensation) - explicit rollback logic

---

## Gap #4: Snapshots (MEDIUM)

### The Problem

**Current:** Full replay from event 0 to rebuild state.
**Impact:** Slow with many events (1000+ events = seconds to rebuild)

**Fowler:** "You can speed up the temporal query by taking a snapshot... start from that snapshot and replay only the events since."

### Solution Design

#### Snapshot Storage

```sql
-- Already have this table!
CREATE TABLE causal_snapshots (
    aggregate_id UUID NOT NULL,
    aggregate_type TEXT NOT NULL,
    version BIGINT NOT NULL,
    state JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (aggregate_id, version)
);

-- Index for latest snapshot lookup
CREATE INDEX idx_causal_snapshots_latest
    ON causal_snapshots(aggregate_id, version DESC);
```

#### Snapshot API

```rust
pub trait EventStore {
    /// Save snapshot of aggregate state
    async fn save_snapshot<A: Aggregate>(&self, aggregate: &A) -> Result<()>;

    /// Load latest snapshot
    async fn load_snapshot<A: Aggregate>(&self, id: A::Id) -> Result<Option<(A, u64)>>;

    /// Load snapshot at specific version
    async fn load_snapshot_at<A: Aggregate>(&self, id: A::Id, version: u64) -> Result<Option<A>>;

    /// Delete old snapshots (keep last N)
    async fn prune_snapshots(&self, id: Uuid, keep_last: usize) -> Result<()>;
}
```

#### Auto-Snapshot Strategy

```rust
pub struct SnapshotConfig {
    /// Create snapshot every N events
    pub every_n_events: usize,

    /// Keep last N snapshots
    pub keep_last: usize,

    /// Minimum events before first snapshot
    pub min_events: usize,
}

impl Default for SnapshotConfig {
    fn default() -> Self {
        Self {
            every_n_events: 100,
            keep_last: 10,
            min_events: 50,
        }
    }
}

/// Aggregate loader with snapshot support
impl AggregateLoader {
    pub async fn load_with_snapshots(&self, config: SnapshotConfig) -> Result<A> {
        // Try to load latest snapshot
        let (mut aggregate, from_version) = match self.store.load_snapshot(self.aggregate_id).await? {
            Some((snapshot, version)) => {
                tracing::debug!("Loaded snapshot at version {}", version);
                (snapshot, version)
            }
            None => {
                tracing::debug!("No snapshot found, loading from event 0");
                (A::default(), 0)
            }
        };

        // Load events since snapshot
        let events = self.store.load_events(self.aggregate_id, from_version).await?;

        let mut events_applied = 0;

        for event in events {
            self.apply_handlers(&mut aggregate, &event)?;
            aggregate.set_version(event.sequence);
            events_applied += 1;
        }

        // Auto-snapshot if threshold reached
        if events_applied >= config.every_n_events {
            tracing::debug!("Auto-snapshotting after {} events", events_applied);
            self.store.save_snapshot(&aggregate).await?;
        }

        Ok(aggregate)
    }
}
```

#### Usage

```rust
// Load with automatic snapshots
let order: Order = store
    .aggregate(order_id)
    .with_reactors(order_handlers::reactors())
    .load_with_snapshots(SnapshotConfig::default())
    .await?;

// Manual snapshot
store.save_snapshot(&order).await?;

// Prune old snapshots (keep last 10)
store.prune_snapshots(order_id, 10).await?;
```

### Performance Impact

**Before (no snapshots):**
- 1000 events = 1000 reactor calls = ~500ms

**After (snapshot every 100 events):**
- Latest snapshot at event 900
- Load snapshot (1 DB query) + 100 events = ~50ms

**10x speedup!**

---

## Implementation Priority

### Phase 1: Foundation (Week 1)
1. ✅ **ReplayContext** - Add to all reactor signatures
2. ✅ **EventUpcast trait** - Foundation for schema evolution
3. ✅ **Gateway pattern** - Basic implementation

### Phase 2: Replay Tooling (Week 2)
4. ✅ **Replay API** - `replay_aggregate()`, `replay_range()`
5. ✅ **Dry run mode** - Validate without state changes
6. ✅ **Replay stats** - Track success/errors

### Phase 3: Optimization (Week 3)
7. ✅ **Snapshot implementation** - Auto-snapshot every N events
8. ✅ **DLQ for projections** - Track and retry failures
9. ✅ **Cached gateways** - Store results for replay

### Phase 4: Polish (Week 4)
10. ✅ **Temporal queries** - `aggregate_at()`, `aggregate_at_version()`
11. ✅ **Background replay** - Async replay jobs
12. ✅ **Monitoring** - Metrics and dashboards

---

## Next Actions

1. **Create tracking issues** for each gap
2. **Prototype ReplayContext** - Add to reactor signatures
3. **Implement EventUpcast** - Start with simple example
4. **Design Gateway API** - Get feedback on ergonomics
5. **Write integration tests** - Replay scenarios

Would you like to start with any specific gap? My recommendation:

**Start with #1 (Replay Tooling) + #5 (Gateways) together** - they're interconnected and provide the foundation for true event sourcing.
