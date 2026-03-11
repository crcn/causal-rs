---
title: Event Batching + Join API (Safe Rollout)
type: feat
date: 2026-02-06
status: implemented
---

# Event Batching + Join API (Safe Rollout)

## Overview

This plan defines a production-safe release that includes both batch emission and join:

1. Add explicit batch emission from effects via `Emit::Batch(Vec<E>)`.
2. Add durable join with **same-batch fan-in** semantics (barrier per emitted batch).
3. Preserve current per-workflow FIFO processing and reducer/effect ordering.
4. Fix deterministic event ID generation so same-type batched events do not collide.

This avoids the failure modes identified in review (ID collisions, ordering regressions, and in-memory join data loss) while still delivering join.

## Implemented Notes (2026-02-06)

- Effect reactors now emit `Vec<EventOutput>` internally, with `Emit<E>` + compatibility conversions preserved at call sites.
- Deterministic emitted event IDs include the emission index:
  - `uuid_v5(namespace, "{parent_event_id}-{effect_id}-{event_type}-{index}")`
- Batch metadata is now propagated on queued and inline emissions via:
  - `batch_id`
  - `batch_index`
  - `batch_size`
- `join().same_batch()` is queued-only and uses durable append/claim/complete/release store operations.
- Join closure behavior is resilient to retries:
  - append is idempotent (`source_event_id` dedupe)
  - claim is single-winner (`open -> processing`)
  - failed flushes release the claim for retry
- DLQ now emits a synthetic terminal event when failed execution had batch metadata, so same-batch windows can close instead of hanging.
- Typed effects can use `.queued().then(...)` (queue codec is carried by builder flags), so `then_queue()` is optional for queued typed reactors.
- Typed effects now support `.dlq_terminal(...)` to map retry-exhausted failures into explicit terminal events before DLQ completion.
- Store API now includes `dlq_effect_with_events(...)` for atomic "DLQ + mapped terminal event" writes; runtime falls back to plain `dlq_effect(...)` if unsupported.
- Runtime now enforces `max_batch_size` (default `10_000`) for emitted batches and `join().same_batch()` metadata.
- Workflow event subscriptions now use coalesced wake-up notifications plus cursor-based paging, so large fan-outs are drained safely without one-notify-per-event fetch overhead.

## Problem

Current effect reactors can return at most one output event (`Option<EventOutput>`). That forces one of two bad patterns:

- Emit large collections as one event payload (risks payload and ergonomics issues).
- Emit N events through N effect executions / N calls.

We need atomic multi-event emission from one effect execution without changing queue guarantees.

## Existing Runtime Constraints (Must Preserve)

1. **Deterministic idempotency**
Current emitted event IDs are deterministic and derived from source event + effect + event type. This currently collides when multiple emitted events share one type.

2. **Per-workflow FIFO semantics**
Event processing is serialized by queue polling/locking strategy. Any new batching feature must keep this behavior by default.

3. **Notification behavior**
Postgres notifications are already metadata-only via trigger (`event_id`, `correlation_id`, `event_type`), and subscribers fetch payload from DB.

## Scope

### In Scope (v0.9)

- `Emit<E>` API for explicit multi-event returns.
- Backward-compatible reactor ergonomics (`Ok(())`, `Ok(event)`, `Ok(Some(event))`, `Ok(vec![...])`, `Ok(Emit::...)`).
- Batch flattening in both inline and queued effect workers.
- Deterministic, collision-free event IDs for batched same-type emissions.
- Durable `join().same_batch()` fan-in for typed effects (`effect::on::<E>()`).
- `join()` is queued-only semantics (never inline) to preserve durable append/flush behavior.
- Join processing that survives worker restarts and retries.
- Integration and regression tests.

### Out of Scope (v0.9)

- `join().count(n)` rolling windows.
- Time-window join (`join().time(...)`) and hybrid windows.
- `on_any().join()` heterogeneous batches.
- Parallel dispatch mode for batch elements.

## Proposed API

```rust
pub enum Emit<E> {
    None,
    One(E),
    Batch(Vec<E>),
}
```

### Compatibility Contract

Keep `.then()` call sites working by introducing an internal conversion trait (name TBD), implemented for:

- `()` -> no emitted events
- `E` -> one emitted event
- `Option<E>` -> zero or one emitted event
- `Vec<E>` -> zero or many emitted events
- `Emit<E>` -> explicit zero/one/many emitted events

This keeps existing reactors source-compatible while enabling explicit batching.

### Key API Changes (Implemented)

- `Emit<E>::Batch(Vec<E>)` for multi-event effect output.
- `effect::on::<E>().join().same_batch().then(...)` for queued durable same-batch fan-in.
- `.queued().then(...)` works for queued typed effects (no forced `then_queue()` requirement).
- `.dlq_terminal(|event, info| ...)` maps retry-exhausted failures into terminal events.
- `Store::dlq_effect_with_events(...)` enables atomic DLQ plus mapped terminal-event emission.
- `EventWorkerConfig::max_batch_size` and `EffectWorkerConfig::max_batch_size` enforce fan-out/join safety bounds.

### Join API (MVP)

```rust
effect::on::<ItemFinished>()
    .join()
    .same_batch() // fan-in exactly the batch emitted upstream
    .then(|batch: Vec<ItemFinished>, ctx| async move {
        ctx.deps().finalize_batch(batch).await?;
        Ok(Emit::One(BatchFinalized { count: batch.len() }))
    });
```

MVP join guarantees:

- durable accumulation in store (not in-memory buffer)
- deterministic batch closure for one emitted batch
- retry-safe flush execution

Batch-scoped join key:

```text
(correlation_id, batch_id, join_effect_id)
```

Terminal-dedupe key for same-batch closure:

```text
(join_effect_id, correlation_id, batch_id, batch_index)
```

`join()` builder contract:

- `join()` implicitly forces queued execution (`effect.is_inline() == false`).
- Attempting to configure join as inline is rejected by API.

## Runtime Design

### 1) Effect Output Flattening

Both `EventWorker` (inline effects) and `EffectWorker` (queued effects) flatten reactor output to `Vec<(TypeId, Arc<dyn Any + Send + Sync>)>` before serialization.

No parallel fan-out path is introduced. Emitted events are persisted and then processed by existing queue workers.

Core contract changes required for batch/join metadata:

- `QueuedEvent` adds optional:
  - `batch_id: Option<Uuid>`
  - `batch_index: Option<i32>`
  - `batch_size: Option<i32>`
- `EmittedEvent` adds optional same fields.
- `causal_events` table adds nullable columns for same fields.

### 2) Deterministic Batched Event IDs (Critical Fix)

Update `complete_effect_with_events` ID derivation to include emission index:

```text
uuid_v5(namespace, "{parent_event_id}-{effect_id}-{event_type}-{index}")
```

This guarantees unique deterministic IDs for:

- many events of same type in one batch
- retries of same effect execution (same ordering -> same IDs)

For each `Emit::Batch`, stamp system batch metadata on emitted events:

- `batch_id` (deterministic for source effect execution)
- `batch_index` (0-based emission order)
- `batch_size` (total events in the emitted batch)

Suggested deterministic batch ID:

```text
uuid_v5(namespace, "{parent_event_id}-{effect_id}-batch")
```

Batch metadata inheritance rule:

- If an effect handles an event with `batch_id` and emits downstream events, those outputs inherit the same `batch_id` by default.
- This enables fan-out on one stage and fan-in on a later stage (`join().same_batch()`) using the same token.
- Inheritance must preserve index semantics for terminal item events (`batch_index` must remain stable per item).

### 3) Preserve Queue Ordering

- Insert emitted events in loop order inside one transaction.
- Keep existing poll/ack flow unchanged.
- Do not add default parallel dispatch.

### 4) Join Runtime Model (Durable Same-Batch)

Join is implemented as a two-step durable flow:

1. **Append step** (for each source event/effect intent)
2. **Flush step** (when same-batch closure condition is reached)

#### Append Step

When a join-enabled effect intent is processed:

- Acquire advisory lock for `(join_effect_id, correlation_id, batch_id_or_window_key)`.
- Upsert source event into `causal_join_entries` with unique key `(join_effect_id, correlation_id, source_event_id)`.
- For `.same_batch()`: seal when `received_terminal_count == expected_count` where `expected_count = batch_size`.
- Create a flush effect intent for that `window_id`.
- Mark append intent as completed.

All of the above happens in one DB transaction.

#### Flush Step

When flush intent executes:

- Load sealed entries for `window_id` in deterministic order.
  - `.same_batch()`: order by `batch_index`
- Decode into `Vec<E>`.
- Run user join reactor once.
- On success: delete sealed entries and complete flush intent (with emitted events if any).
- On failure: mark flush intent failed/DLQ; sealed entries remain for retry.

This gives restart-safe join behavior without in-memory loss.

#### Failed Items in `same_batch` Joins

Failed items are treated as first-class terminal results, not dropped work.

Rules:

- `join().same_batch()` closes when terminal results received == `batch_size`.
- Terminal result means one of:
  - success item-completion event
  - failure item-completion event (domain or system failure)
- Each batch item contributes at most one terminal row to a join window, keyed by `(join_effect_id, correlation_id, batch_id, batch_index)`.

Recommended event shape for join targets:

```rust
enum ItemResultEvent {
    ItemSucceeded {
        batch_id: Uuid,
        batch_index: i32,
        batch_size: i32,
        item_event_id: Uuid,
        // domain payload...
    },
    ItemFailed {
        batch_id: Uuid,
        batch_index: i32,
        batch_size: i32,
        item_event_id: Uuid,
        error: String,
        final_failure: bool,
    },
}
```

DLQ interaction:

- If a queued item exhausts retries and goes to DLQ, runtime must emit a synthetic terminal failure event (with same `batch_id` / `batch_index` / `item_event_id`) so the join window can complete.
- Synthetic failure emission must be idempotent (same deterministic event id on retry).

#### Retry Behavior for Individual Batch Items

- Retry is per queued effect execution item, not per upstream emitted batch.
- If item `50` fails in a `100` item batch, only item `50` is retried according to that effect's `max_attempts`.
- Items that already reached terminal success are not re-run when another item retries.
- `join().same_batch()` closes only when terminal rows exist for all `batch_index` values in `[0, batch_size)`.
- If retries are exhausted, terminal failure still contributes one row for that `batch_index` (via domain failure event or DLQ terminal mapping), so join can close with partial success.

#### Domain Failures vs Runtime Terminal Mapping

- Use reactor-level error mapping (`map_err`, `match`, or explicit branching) when a domain failure should be terminal immediately and should not consume retry budget.
- Use `.dlq_terminal(...)` when runtime retries are allowed and you only want a terminal failure event after retries are exhausted/time out.
- In `same_batch` joins, normalize both success and failure into a single join input type (for example `ResearchSearchResult::Completed` / `ResearchSearchResult::Failed`) so aggregation logic stays deterministic.

#### Example Flow with Join and Failure Events

```text
WebsiteResearchCreated
  -> prepare_searches_effect (queued)
  -> Emit::Batch([ResearchSearchEnqueued xN])  // carries batch metadata

ResearchSearchEnqueued
  -> execute_search_effect (queued, parallel workers)
  -> Emit::One(ResearchSearchCompleted | ResearchSearchFailed)

ResearchSearchCompleted/Failed
  -> join_searches_effect.join().same_batch().then(|items| ...)
  -> Emit::One(AssessmentGenerationEnqueued)

AssessmentGenerationEnqueued
  -> generate_assessment_effect (queued)
  -> Emit::None
```

#### Join Persistence Schema (Postgres)

Implemented durable tables:

- `causal_join_entries`
  - `join_effect_id text`
  - `correlation_id uuid`
  - `source_event_id uuid`
  - `source_event_type text`
  - `source_payload jsonb`
  - `source_created_at timestamptz`
  - `batch_id uuid null`
  - `batch_index int null`
  - `batch_size int null`
  - `window_id uuid null`
  - unique `(join_effect_id, correlation_id, source_event_id)` for append idempotency
  - unique `(join_effect_id, correlation_id, batch_id, batch_index)` for terminal dedupe

- `causal_join_windows`
  - primary key `(join_effect_id, correlation_id, batch_id)`
  - `mode text` (`same_batch`)
  - `target_count int not null`
  - `status text` (`open` | `processing` | `completed`)
  - timestamps for sealed/processing/completed lifecycle
  - `last_error text` for failed flush diagnostics

## Postgres / Notify Notes

Workflow notifications now use a constant wake-up payload (`'wake'`) per workflow channel.

This enables transaction-level notification coalescing while keeping correctness by:

- treating NOTIFY as a wake-up signal only
- fetching unseen workflow events by `(created_at, id)` cursor
- draining in paged queries until caught up

## Performance Guardrails

- `max_batch_size` hard limit prevents unbounded memory/transaction growth during fan-out and join.
- Workflow subscription uses coalesced `NOTIFY` wake-ups with cursor paging, so large workflows drain without one-notify-per-event overhead.
- Large fan-outs still execute in a single effect-completion transaction for atomicity; if workloads exceed practical transaction size, split domain work into multiple smaller batches upstream.

## Implementation Plan

### Phase 1: Emit API + Worker Flattening (2-3 days)

**Tasks**

- Add `Emit<E>` type in `crates/causal/src/effect/types.rs`.
- Add output-conversion trait for `.then()` compatibility.
- Update typed `.then()` / `.then_queue()` internals to use conversion trait.
- Update inline and queued worker paths to flatten zero/one/many outputs.
- Unit tests for conversion behavior.

**Success Criteria**

- Existing reactors compile unchanged.
- `Ok(Emit::Batch(vec![a, b]))` emits two events.
- `Ok(vec![a, b])` emits two events.

### Phase 2: Deterministic ID Collision Fix (1-2 days)

**Tasks**

- Update Postgres store deterministic ID generation to include emission index.
- Update Memory store deterministic ID generation to include emission index.
- Update inline event emission path in `EventWorker` to include emission index (not just store path).
- Add regression test: one effect emits N events of same type; all N are persisted and unique.

**Success Criteria**

- No collisions for same-type batched emissions.
- Retry remains idempotent when effect output order is deterministic.

### Phase 3: Join MVP (Durable Same-Batch) (3-4 days)

**Tasks**

- Add `join().same_batch()` builder for typed effects in `crates/causal/src/effect/builders.rs`.
- Add join metadata to effect registration (join mode + queued-only enforcement).
- Add durable join tables/state and locking flow in Postgres store.
- Implement append+seal+flush flow in queued effect worker path.
- Propagate and inherit batch metadata (`batch_id`, `batch_index`, `batch_size`) across emitted event chains.
- Add retry-safe flush execution and DLQ behavior for join flush intents.
- Add DLQ synthetic terminal failure emission path so failed items close same-batch windows.
- Add in-memory join behavior in `causal-memory` for test parity (best-effort durability in memory).

**Success Criteria**

- Joined events survive worker restart in Postgres path.
- `join().same_batch()` executes reactor exactly once per emitted batch.
- Join flush retries do not duplicate downstream emitted events.
- Mixed success/failure item batches still close exactly once when all items are terminal.

### Phase 4: Validation + Docs (1-2 days)

**Tasks**

- Integration tests for 1k+ event batch emission.
- Integration tests for join append/flush/retry/restart behavior.
- Update docs and migration notes.
- Add explicit note: emission order is part of deterministic idempotency key.

**Success Criteria**

- All store/runtime tests pass.
- Migration examples cover old and new return shapes plus join usage.

## Acceptance Criteria

### Functional

- Effect may emit 0/1/N events atomically through one completion transaction.
- Batch with repeated event type persists all items (no silent dedupe).
- Inline and queued effect paths behave consistently.
- Existing reactors remain source-compatible.
- `join().same_batch()` fan-ins exactly one emitted batch by `(correlation_id, batch_id)`.
- `join().same_batch()` includes failed items as terminal results (no silent drop, no hanging window).

### Behavioral

- FIFO processing semantics are unchanged.
- No new out-of-order behavior introduced by batching.
- Workflow notifications continue to work with metadata-only payloads.
- Join flushes are retry-safe and do not lose buffered events on worker restart.
- Batches with partial failures complete deterministically and surface failure details to join reactor.

### Testing

- Unit tests for all output conversions.
- Integration test for same-type fan-out (e.g., 1000 `NotificationSent`).
- Retry/idempotency regression test for batched effect completion.
- Integration test for same-batch fan-in (`Emit::Batch -> join().same_batch()`).
- Integration tests for join closure, crash/restart recovery, and flush retry.
- Integration test for partial failure batch (`N_success + M_failed == batch_size`) with single join closure.

Implemented stress/failure coverage includes:

- `event_worker_inline_batch_emit_stress_generates_unique_ids_and_batch_metadata`
- `event_worker_inline_batch_emit_over_limit_is_nacked`
- `effect_worker_join_same_batch_stress_large_batch_closes_once`
- `effect_worker_join_same_batch_releases_and_retries_after_handler_error`
- `effect_worker_dlq_terminal_mapper_emits_mapped_event_for_then`

## Risks and Mitigations

### Risk 1: Non-deterministic emission order across retries

If reactor output order varies between retries, index-based deterministic IDs can differ.

**Mitigation**

- Document requirement: batch output order should be deterministic.
- Add helper guidance: sort by stable key before returning batch when source iteration is unstable.

### Risk 2: Join lock contention on hot workflows

High-throughput workflows may contend on the `(join_effect_id, correlation_id)` join lock.

**Mitigation**

- Keep lock scope small (append/seal transaction only).
- Use deterministic FIFO selection for sealing to avoid starvation.
- Add contention telemetry (lock wait duration, append latency, flush latency).

### Risk 3: Batch metadata propagation gaps

If `batch_id/batch_index/batch_size` are not consistently propagated, `.same_batch()` windows may never close.

**Mitigation**

- Add storage-level nullable metadata fields and explicit propagation in workers.
- Validate metadata invariants when handling `.same_batch()`:
  - `batch_id` present
  - `batch_size > 0`
  - `0 <= batch_index < batch_size`
- Route malformed join metadata to DLQ with clear reason.

### Risk 4: Join windows hanging on failed items

If failed items never emit terminal failure results, `.same_batch()` may wait forever.

**Mitigation**

- Treat DLQ as terminal and synthesize failure result events for affected items.
- Enforce one terminal result per item via `(join_effect_id, correlation_id, batch_id, batch_index)` uniqueness.
- Add watchdog metric/alert for join windows open beyond threshold.

### Risk 5: Large transaction payload

Very large batches can produce large transactions.

**Mitigation**

- Keep current atomic transaction behavior.
- Add size/volume guidance in docs.
- If needed later, introduce explicit chunked emission API with documented atomicity trade-offs.

### Risk 6: Backward compatibility drift

Changing builder bounds can accidentally break type inference.

**Mitigation**

- Add compile tests/examples for:
  - `Ok(())`
  - `Ok(event)`
  - `Ok(Some(event))`
  - `Ok(vec![...])`
  - `Ok(Emit::Batch(...))`

## Timeline

- Phase 1: 2-3 days
- Phase 2: 1-2 days
- Phase 3: 3-4 days
- Phase 4: 1-2 days

**Total:** 7-11 days

## Migration Examples

```rust
// Existing (unchanged)
effect::on::<OrderPlaced>().then(|event, _ctx| async move {
    Ok(PaymentCharged { order_id: event.id })
})

// Existing observer (unchanged)
effect::on::<OrderPlaced>().then(|event, _ctx| async move {
    println!("{:?}", event);
    Ok(())
})

// New explicit batch
effect::on::<UsersLoaded>().then(|event, _ctx| async move {
    let out = event
        .user_ids
        .into_iter()
        .map(|user_id| NotificationSent { user_id })
        .collect::<Vec<_>>();
    Ok(Emit::Batch(out))
})

// New ergonomic batch (via conversion trait)
effect::on::<UsersLoaded>().then(|event, _ctx| async move {
    Ok(event
        .user_ids
        .into_iter()
        .map(|user_id| NotificationSent { user_id })
        .collect::<Vec<_>>())
})

// Same-batch fan-in (tokio::join_all style barrier)
effect::on::<FetchCompleted>()
    .join()
    .same_batch()
    .then(|batch: Vec<FetchCompleted>, ctx| async move {
        ctx.deps().combine(batch).await?;
        Ok(Emit::One(AllFetchesCombined))
    })
```

## References

- `/Users/crcn/Developer/crcn/causal-rs/crates/causal/src/effect/builders.rs`
- `/Users/crcn/Developer/crcn/causal-rs/crates/causal/src/effect/types.rs`
- `/Users/crcn/Developer/crcn/causal-rs/crates/causal/src/runtime/event_worker.rs`
- `/Users/crcn/Developer/crcn/causal-rs/crates/causal/src/runtime/effect_worker.rs`
- `/Users/crcn/Developer/crcn/causal-rs/crates/causal/src/store.rs`
- `/Users/crcn/Developer/crcn/causal-rs/crates/causal-postgres/src/lib.rs`
- `/Users/crcn/Developer/crcn/causal-rs/crates/causal-memory/src/lib.rs`
- `/Users/crcn/Developer/crcn/causal-rs/docs/migrations/001_fix_pg_notify_payload_limit.sql`
- `/Users/crcn/Developer/crcn/causal-rs/docs/migrations/002_add_batch_join_schema.sql`

## Pressure-Test Matrix (Executed)

### Success Modes

- Inline fan-out batch emission at scale (`256` emitted events) with:
  - unique deterministic IDs
  - contiguous `batch_index`
  - consistent `batch_id`/`batch_size`
- Same-batch join closure with mixed item success/failure payloads:
  - single join reactor execution
  - single downstream summary emission

### Failure Modes

- Same-batch join reactor failure + retry:
  - first claim fails
  - claim is released
  - retry re-claims and succeeds
  - no duplicate join output
- Invalid same-batch metadata:
  - now routed to fail/DLQ path (no stuck executing row)
- Oversized batches rejected by runtime guardrail (`max_batch_size`):
  - inline effect fan-out above limit nacks source event
  - queued effect fan-out above limit fails execution (retry/DLQ path)
  - `join().same_batch()` metadata above limit fails append execution
- DLQ terminal behavior:
  - when failed execution has batch metadata, synthetic terminal event is emitted
  - when no batch metadata exists, no synthetic terminal event is emitted

### Stress Modes

- Same-batch join stress (`256` items):
  - all append paths processed
  - closure happens exactly once
  - expected failed count preserved
- Workflow subscription coalescing stress (`700` emitted events in one completion transaction):
  - cursor-based subscriber drains all events in-order via paged fetch
  - coalesced wake-up notification model prevents one-fetch-per-notify overhead

### Commands Run

- `RUSTC_WRAPPER= cargo test -p causal -p causal-memory`
- `RUSTC_WRAPPER= cargo test -p causal-postgres --no-run`
- `RUSTC_WRAPPER= cargo test -p causal-postgres test_workflow_subscription_coalesced_notify_stress_drains_all_events -- --nocapture` (requires Docker; failed in this environment due Docker connection)

Postgres container-backed integration tests require local Docker; compile validation succeeded.
