# Workflow Status Tracking Example

Demonstrates the 3 patterns for tracking workflow status with Seesaw v0.8.1+:

## Patterns Shown

### 1. Fire and Forget
Start a workflow and get the `correlation_id` to return to the client:

```rust
let handle = engine.process(OrderPlaced { ... }).await?;
let correlation_id = handle.correlation_id;  // Return this to client
```

### 2. Poll Workflow Status
Check if a workflow is settled (no effects running):

```rust
let status = store.get_workflow_status(correlation_id).await?;
println!("Settled: {}, Pending: {}",
    status.is_settled,
    status.pending_effects
);
```

### 3. Wait for Terminal Event
Block until the workflow truly completes (user-defined terminal event):

```rust
let result = engine
    .process(event)
    .wait(|event| {
        match event.event_type.as_str() {
            "OrderCompleted" => Some(Ok(true)),
            "OrderFailed" => Some(Err(anyhow!("Failed"))),
            _ => None,  // Keep waiting
        }
    })
    .timeout(Duration::from_secs(30))
    .await?;
```

## Key Distinction

- **`is_settled`** - No effects running *right now* (workflow is idle, can start again)
- **Terminal event** - User declares "this workflow is done forever" (e.g., `OrderCompleted`)

## Running

1. Start PostgreSQL:
```bash
docker run -d -p 5432:5432 \
  -e POSTGRES_PASSWORD=postgres \
  postgres:16-alpine
```

2. Set DATABASE_URL:
```bash
export DATABASE_URL="postgres://postgres:postgres@localhost/seesaw"
```

3. Run the example:
```bash
cargo run --example workflow-status
```

4. (Optional) Start the Insight server for real-time visualization:
```bash
# In another terminal
export DATABASE_URL="postgres://postgres:postgres@localhost/seesaw"
cargo run -p seesaw-insight

# Open http://localhost:3000 in your browser
# Watch events and effects flow in real-time!
```

## Output

You'll see:
- Pattern 1: Workflow started, correlation_id returned
- Pattern 2: Status polled 5 times showing `pending_effects` count
- Pattern 3: Wait for `OrderCompleted` terminal event
- Comparison: `is_settled` vs terminal events

## Use Cases

**Pattern 1 (Fire & Forget):**
- REST APIs that return immediately
- Background job processing
- Event-driven microservices

**Pattern 2 (Poll Status):**
- Progress indicators
- Admin dashboards
- Monitoring/observability

**Pattern 3 (Wait for Terminal):**
- Synchronous APIs that need results
- Webhooks that must confirm completion
- Testing/CI pipelines
