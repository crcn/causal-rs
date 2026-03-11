# Durable Choreography Workflows with Causal

## Can Causal Handle Multi-Day Workflows?

**Short Answer:** Yes, with the "everything in queue" pattern.

**Long Answer:** Causal can support multi-day workflows if you make ALL events durable and implement proper event context tracking. This is NOT traditional event sourcing or orchestration—it's **durable choreography**.

## Architecture Pattern

```
┌─────────────────────────────────────────────────────────────┐
│                    Durable Event Queue                       │
│              (NATS JetStream / SQS / Kafka)                 │
└─────────────────────────────────────────────────────────────┘
         ↑                                          ↓
         │ Publish                       Consume    │
         │                                          │
┌────────┴────────┐                    ┌───────────▼──────────┐
│  Transactional  │                    │   Causal Worker      │
│     Outbox      │                    │   (Stateless)        │
│  (Postgres)     │                    │                      │
└─────────────────┘                    └──────────────────────┘
         ↑                                          │
         │ Write Event                    Activate Engine
         │                                          ↓
┌────────┴────────┐                    ┌───────────▼──────────┐
│   Effect        │                    │   Load State from    │
│  (Processes     │◄───────────────────┤   Database/Store     │
│   Event)        │                    │                      │
└─────────────────┘                    └──────────────────────┘
```

**Core Principle:** The durable queue + event context = durable execution continuity.

## Example: 3-Day Order Approval Workflow

### Day 1: Order Placed

```rust
// Customer submits order
let event = OrderPlaced {
    order_id: Uuid::new_v4(),
    customer_id: customer.id,
    items: vec![...],
    total: 1000.00,
    correlation_id: Uuid::new_v4(), // ← Workflow correlation
    workflow_state: WorkflowState::Started,
    initiated_at: Utc::now(),
};

// Write to outbox + database in single transaction
let mut tx = db.begin().await?;
order.save(&mut tx).await?;
outbox.write_event(&event, &mut tx).await?;
tx.commit().await?;

// Background worker publishes to NATS JetStream
outbox_processor.publish_to_queue(&event).await?;
```

**Effect 1: Request Manager Approval**
```rust
effect::on::<OrderPlaced>().then(|event, ctx| async move {
    // Send approval request email
    ctx.deps().mailer.send_approval_request(
        event.order_id,
        "manager@company.com",
        approval_url(&event.correlation_id)
    ).await?;

    // Return event with context
    Ok(ManagerApprovalRequested {
        order_id: event.order_id,
        correlation_id: event.correlation_id, // ← Preserve workflow context
        requested_at: Utc::now(),
        expires_at: Utc::now() + Duration::days(3),
    })
});
```

**Worker processes, shuts down. Queue holds event for 3 days.**

---

### Day 3: Manager Approves (Human Action)

```rust
// Manager clicks link in email → webhook receives approval
async fn approval_webhook(approval: Approval) -> Result<()> {
    let event = ManagerApproved {
        order_id: approval.order_id,
        correlation_id: approval.correlation_id, // ← Context from URL
        approved_by: "manager@company.com".into(),
        approved_at: Utc::now(),
    };

    // Write to outbox → NATS
    let mut tx = db.begin().await?;
    outbox.write_event(&event, &mut tx).await?;
    tx.commit().await?;

    Ok(())
}
```

**Effect 2: Request Director Approval**
```rust
effect::on::<ManagerApproved>().then(|event, ctx| async move {
    // Load current order state from database
    let order = ctx.deps().db.get_order(event.order_id).await?;

    // Validate approval chain (idempotency guard)
    if order.status != OrderStatus::AwaitingManagerApproval {
        return Ok(()); // Already processed
    }

    // Update order status
    ctx.deps().db.update_order_status(
        event.order_id,
        OrderStatus::AwaitingDirectorApproval
    ).await?;

    // Request director approval
    ctx.deps().mailer.send_approval_request(
        event.order_id,
        "director@company.com",
        approval_url(&event.correlation_id)
    ).await?;

    Ok(DirectorApprovalRequested {
        order_id: event.order_id,
        correlation_id: event.correlation_id, // ← Still carrying context
        requested_at: Utc::now(),
        expires_at: Utc::now() + Duration::days(2),
    })
});
```

**Worker processes, shuts down. Queue holds event for 2 more days.**

---

### Day 5: Director Approves → Order Processed

```rust
effect::on::<DirectorApproved>().then(|event, ctx| async move {
    // Load FULL order state from database
    let order = ctx.deps().db.get_order(event.order_id).await?;

    // Validate COMPLETE approval chain
    if !order.has_manager_approval() || !order.has_director_approval() {
        return Ok(OrderApprovalFailed {
            order_id: event.order_id,
            reason: "Incomplete approval chain".into(),
        });
    }

    // Process order (charge card, fulfill, ship)
    ctx.deps().payment.charge(order.total).await?;
    ctx.deps().fulfillment.process(&order).await?;

    // Terminal event
    Ok(OrderCompleted {
        order_id: event.order_id,
        correlation_id: event.correlation_id,
        completed_at: Utc::now(),
    })
});
```

## Event Context: The Critical Piece

Every event must carry workflow context:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowEvent {
    // Event-specific data
    pub data: EventData,

    // Workflow context (CRITICAL)
    pub correlation_id: Uuid,           // Workflow identifier
    pub order_id: Uuid,          // Entity identifier
    pub correlation_id: Uuid,    // Request trace
    pub initiated_at: DateTime<Utc>,
    pub parent_event_id: Option<Uuid>,

    // Tracing context
    pub trace_context: HashMap<String, String>, // W3C trace propagation

    // Idempotency
    pub event_id: Uuid,          // Deduplication key
    pub source: String,          // "webhook", "scheduler", "effect"
}
```

## State Reconstruction: Database, Not Event Replay

**Key Difference from Event Sourcing:**

Causal does NOT replay events to reconstruct state. Instead:

```rust
effect::on::<DirectorApproved>().then(|event, ctx| async move {
    // ✅ CORRECT: Load current state from database
    let order = ctx.deps().db.get_order(event.order_id).await?;

    // ❌ WRONG: Don't try to reconstruct from event history
    // let order = reconstruct_from_events(event.correlation_id).await?;

    // Process with current state
    if order.is_fully_approved() {
        complete_order(&order, ctx).await?;
    }

    Ok(OrderCompleted { order_id: event.order_id })
});
```

**Why This Works:**
- Reducers update database state as events flow through
- Effects always query current state from database
- Event history provides audit trail, not state reconstruction

## Handling Timeouts

For multi-day workflows, you need timeout detection:

### Option 1: External Scheduler + Webhook

```rust
// When approval requested, schedule timeout check
effect::on::<ManagerApprovalRequested>().then(|event, ctx| async move {
    // Schedule timeout check 3 days from now
    ctx.deps().scheduler.schedule_webhook(
        Utc::now() + Duration::days(3),
        "/api/check-approval-timeout",
        json!({ "correlation_id": event.correlation_id, "order_id": event.order_id })
    ).await?;

    Ok(())
});

// Timeout webhook reactor
async fn check_approval_timeout(payload: TimeoutCheck) -> Result<()> {
    let order = db.get_order(payload.order_id).await?;

    if order.status == OrderStatus::AwaitingManagerApproval {
        // Still waiting, timeout expired
        let event = ApprovalTimedOut {
            order_id: payload.order_id,
            correlation_id: payload.correlation_id,
            timed_out_at: Utc::now(),
        };

        outbox.write_event(&event).await?;
    }

    Ok(())
}
```

### Option 2: NATS Consumer with Timeout

```rust
// Use NATS JetStream consumer timeout
let consumer = stream.create_consumer(jetstream::consumer::pull::Config {
    durable_name: Some("approval_worker".to_string()),
    ack_policy: AckPolicy::Explicit,
    ack_wait: Duration::from_secs(3 * 24 * 60 * 60), // 3 days
    max_deliver: 3, // Max attempts
    ..Default::default()
}).await?;

// If not acked after 3 days, NATS redelivers to DLQ
```

## Idempotency: Mandatory for Multi-Day Workflows

Network retries, queue redeliveries, and human errors mean the same event may arrive multiple times:

```rust
effect::on::<ManagerApproved>().then(|event, ctx| async move {
    // Guard 1: Check if already processed
    if ctx.deps().db.event_processed(event.event_id).await? {
        tracing::warn!("Event {} already processed, skipping", event.event_id);
        return Ok(());
    }

    // Guard 2: Check order status
    let order = ctx.deps().db.get_order(event.order_id).await?;
    if order.manager_approved_at.is_some() {
        tracing::warn!("Order {} already has manager approval", event.order_id);
        return Ok(());
    }

    // Process (guaranteed to run once)
    ctx.deps().db.mark_manager_approved(event.order_id, Utc::now()).await?;
    ctx.deps().db.mark_event_processed(event.event_id).await?;

    Ok(DirectorApprovalRequested {
        order_id: event.order_id,
        correlation_id: event.correlation_id,
    })
});
```

## Comparison: Causal vs Temporal

| Feature | Causal (Durable Choreography) | Temporal |
|---------|------------------------------|----------|
| **Workflow Style** | Choreography (event chains) | Orchestration (workflow DSL) |
| **State Persistence** | Database (explicit) | Automatic (event sourcing) |
| **Timers** | External scheduler required | Built-in durable timers |
| **Retries** | You implement | Automatic with backoff |
| **Visualization** | Build with causal-viz | Built-in UI |
| **Complexity** | Simpler (no DSL) | Higher (workflow runtime) |
| **Control** | Full (you own durability) | Managed (framework handles) |
| **Best For** | Choreography, simple flows | Complex orchestration |

## When to Use This Pattern

**✅ Good Fit:**
- Choreography-based workflows (approval chains, order processing)
- Events naturally partition by entity (order_id, user_id)
- You need full control over durability/retry logic
- You're already using NATS/Kafka for other reasons
- Team prefers event-driven over workflow DSLs

**❌ Poor Fit:**
- Complex branching/looping workflows (use Temporal)
- Need visual workflow designer (use Temporal)
- Human-in-the-loop with complex routing (use dedicated BPM)
- Require exactly-once guarantees across services (distributed transaction)

## Implementation Checklist

- [ ] **Durable Queue**: NATS JetStream, AWS SQS, or Kafka with persistence
- [ ] **Transactional Outbox**: Atomic writes (event + state) to database
- [ ] **Event Context**: Every event carries `correlation_id`, `correlation_id`, `event_id`
- [ ] **Idempotency Guards**: Check event_id or state before processing
- [ ] **State in Database**: Don't rely on event replay for state
- [ ] **Terminal Events**: Define success/failure completion events
- [ ] **Timeout Handling**: External scheduler or queue timeout + DLQ
- [ ] **Correlation Tracking**: Propagate context through entire workflow
- [ ] **Dead Letter Queue**: Handle poison messages and permanent failures
- [ ] **Monitoring**: Track workflow progress via correlation_id

## Example: Complete Implementation

See `examples/durable-approval-workflow/` for a working example including:
- NATS JetStream setup
- Transactional outbox pattern
- 3-day approval workflow
- Timeout handling
- Idempotency guards
- State management

## Key Takeaway

**Causal CAN handle multi-day workflows** with the "everything in queue" + event context pattern.

The trade-off:
- **You implement**: Durability, timeouts, retries, idempotency
- **You get**: Full control, no workflow runtime, choreography-native

If you want automatic durability and orchestration, use Temporal.
If you want explicit control and choreography, use Causal + durable queue.
