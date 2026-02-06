# Restate Backend MVP Plan

Status: Draft  
Date: 2026-02-06  
Target version: post-0.10.3

## Decision

Build a **hybrid Restate backend** first:

- Keep `PostgresStore` as the durable source of truth.
- Add a new crate, `seesaw-restate`, that implements `QueueBackend<PostgresStore>`.
- Use Restate for effect wakeups/dispatch coordination, not for canonical workflow state in v1.

This fits Seesaw's current runtime contracts (`Store` + `QueueBackend`) without a risky core rewrite.

## Why This Shape

Current Seesaw architecture already separates:

- Durable state and atomic commits: `Store`
- Optional dispatch acceleration: `QueueBackend`

`QueueBackend` is advisory and workers already fall back to `Store::poll_next_effect()` on failure/no work.  
That gives us a safe on-ramp for Restate.

## Scope

### MVP in scope

1. `seesaw-restate` crate with a `RestateQueueBackend`.
2. `on_effect_intent_inserted` pushes `(event_id, handler_id)` to Restate.
3. `poll_next_effect` tries Restate first, then claims that exact effect from Postgres.
4. If Restate fails, Seesaw behavior remains correct via existing store fallback.
5. Conformance tests for retries, dedupe, restart safety, and fallback behavior.

### Out of scope (MVP)

1. Replacing `Store` with a pure Restate state backend.
2. Removing Postgres.
3. Rewriting event worker/effect worker execution model.
4. Changing handler authoring API.

## Proposed Crate Layout

```text
crates/seesaw-restate/
  Cargo.toml
  src/
    lib.rs
    backend.rs
    transport.rs
    models.rs
    postgres_claim.rs
  tests/
    integration_restate_backend.rs
```

## Backend Interface (Concrete)

```rust
// crates/seesaw-restate/src/lib.rs
pub mod backend;
pub mod models;
pub mod transport;

pub use backend::{RestateQueueBackend, RestateQueueBackendConfig};
pub use models::EffectIntentRef;
pub use transport::{RestateTransport, RestateTransportError};
```

```rust
// crates/seesaw-restate/src/models.rs
use chrono::{DateTime, Utc};
use uuid::Uuid;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EffectIntentRef {
    pub event_id: Uuid,
    pub handler_id: String,
    pub execute_at: DateTime<Utc>,
    pub priority: i32,
}
```

```rust
// crates/seesaw-restate/src/transport.rs
use anyhow::Result;
use async_trait::async_trait;
use std::time::Duration;

use crate::models::EffectIntentRef;

#[derive(Debug, thiserror::Error)]
pub enum RestateTransportError {
    #[error("temporary transport failure: {0}")]
    Temporary(String),
    #[error("permanent transport failure: {0}")]
    Permanent(String),
}

#[async_trait]
pub trait RestateTransport: Send + Sync + 'static {
    async fn enqueue_intent(&self, intent: EffectIntentRef) -> Result<()>;
    async fn dequeue_intent(&self, wait: Duration) -> Result<Option<EffectIntentRef>>;
}
```

```rust
// crates/seesaw-restate/src/backend.rs
use anyhow::Result;
use async_trait::async_trait;
use seesaw_core::{QueueBackend, QueuedHandlerExecution};
use seesaw_postgres::PostgresStore;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

use crate::models::EffectIntentRef;
use crate::transport::RestateTransport;

#[derive(Debug, Clone)]
pub struct RestateQueueBackendConfig {
    pub dequeue_wait: Duration,
    pub enqueue_metadata_cache_ttl: Duration,
}

pub struct RestateQueueBackend {
    transport: Arc<dyn RestateTransport>,
    config: RestateQueueBackendConfig,
}

impl RestateQueueBackend {
    pub fn new(
        transport: Arc<dyn RestateTransport>,
        config: RestateQueueBackendConfig,
    ) -> Self {
        Self { transport, config }
    }

    async fn load_effect_intent(
        &self,
        store: &PostgresStore,
        event_id: Uuid,
        handler_id: &str,
    ) -> Result<EffectIntentRef> {
        // Query seesaw_handler_executions for execute_at + priority.
        // This keeps QueueBackend API unchanged in MVP.
        todo!()
    }
}

#[async_trait]
impl QueueBackend<PostgresStore> for RestateQueueBackend {
    fn name(&self) -> &'static str {
        "restate"
    }

    async fn on_effect_intent_inserted(
        &self,
        store: &PostgresStore,
        event_id: Uuid,
        handler_id: &str,
    ) -> Result<()> {
        let intent = self.load_effect_intent(store, event_id, handler_id).await?;
        self.transport.enqueue_intent(intent).await
    }

    async fn poll_next_effect(
        &self,
        store: &PostgresStore,
    ) -> Result<Option<QueuedHandlerExecution>> {
        let Some(intent) = self.transport.dequeue_intent(self.config.dequeue_wait).await? else {
            return Ok(None);
        };

        // Claim this exact effect row in Postgres. If already claimed/completed,
        // return None (idempotent/stale intent).
        crate::postgres_claim::claim_effect_by_key(store, intent).await
    }
}
```

## Required Postgres Claim Query

`postgres_claim::claim_effect_by_key` should atomically claim only the requested row:

- Match by `(event_id, handler_id)`.
- Allow status in `pending` or retryable `failed`.
- Require `execute_at <= NOW()`.
- `FOR UPDATE SKIP LOCKED`.
- Increment attempts and set `executing`, `claimed_at`, `last_attempted_at`.
- Return full `QueuedHandlerExecution`.

If no row is returned, treat as stale/duplicate and continue.

## Runtime Wiring Example

```rust
use seesaw_core::Engine;
use seesaw_postgres::PostgresStore;
use seesaw_restate::{RestateQueueBackend, RestateQueueBackendConfig};

let store = PostgresStore::new(pool);
let queue = RestateQueueBackend::new(transport, RestateQueueBackendConfig::default());
let engine = Engine::new(deps, (store, queue));
```

## Rollout Plan

### Phase 1: Scaffold + feature-gated backend

1. Add `crates/seesaw-restate` to workspace.
2. Define transport trait and in-memory fake transport for tests.
3. Implement `RestateQueueBackend` with no-op dequeue fallback.
4. Add docs and usage snippet.

Exit criteria:

- Workspace builds without enabling Restate runtime transport.
- Existing tests unchanged.

### Phase 2: Postgres-aware enqueue + exact-claim dequeue

1. Implement metadata load query (`execute_at`, `priority`) in enqueue hook.
2. Implement exact claim query for `(event_id, handler_id)` on dequeue.
3. Handle stale intents idempotently.
4. Add metrics/logging around enqueue/dequeue/claim misses.

Exit criteria:

- Effect is never executed twice after duplicate intents.
- Store fallback still processes work if Restate path is unavailable.

### Phase 3: Conformance suite

1. New integration tests in `crates/seesaw-restate/tests/`:
   - duplicate enqueue
   - worker crash after dequeue before completion
   - max retry to DLQ
   - delayed effect (`execute_at` in future)
   - backend transport down -> store fallback
2. Compare with current `StoreQueueBackend` behavior in deterministic scenarios.

Exit criteria:

- Behavioral parity for success/failure/retry semantics at Seesaw API boundary.

### Phase 4: Production hardening

1. Backpressure controls and bounded dequeue wait.
2. Observability:
   - queue depth (Restate + Postgres pending)
   - dequeue latency
   - stale intent rate
3. Operational docs: required Restate services, retries, timeouts.

Exit criteria:

- Clear SLOs and runbook for failover/fallback.

## Test Matrix (Minimum)

1. **Idempotency**: same `(event_id, handler_id)` intent dequeued multiple times executes once.
2. **Retry**: handler failure increments attempts and re-schedules correctly.
3. **Timeout**: timed-out handler follows fail-or-DLQ path unchanged.
4. **Join mode**: same-batch join behavior unchanged under Restate wakeups.
5. **Fallback**: with Restate transport disabled, system drains via `Store::poll_next_effect`.

## Risk Register

1. **Semantic drift** between Restate intent ordering and Postgres priority ordering.
   - Mitigation: Postgres claim query remains the final arbiter.
2. **Duplicate wakeups** causing hot-looping.
   - Mitigation: stale intent detection + bounded dequeue wait.
3. **Hidden lock-in** from Restate-specific fields in core traits.
   - Mitigation: keep all Restate types in `seesaw-restate`.

## Follow-up (Post-MVP)

If MVP is stable and beneficial, evaluate a larger backend abstraction that can support:

1. Non-poll worker model.
2. Native durable timers/workflows.
3. Optional reduced dependency on SQL-centric store contracts.

Do this only after benchmark evidence shows a material win over current Postgres + store queue path.
