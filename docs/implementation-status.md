# Queue-Backed Architecture Implementation Status

## Overview

This document tracks implementation progress against the plan in `docs/plans/2026-02-05-feat-queue-backed-architecture-plan.md`.

**Date**: 2026-02-05
**Status**: ~60% Complete - Core infrastructure done, missing critical features

---

## ✅ What We've Built (Phase 1-2 Complete)

### Database Schema ✅
- [x] `seesaw_events` table with partitioning by date
- [x] `seesaw_state` table with optimistic locking (version column)
- [x] `seesaw_effect_executions` table with priority/timeout/retry
- [x] `seesaw_dlq` table for permanently failed effects
- [x] `seesaw_reaper_heartbeat` for monitoring
- [x] Per-workflow FIFO ordering with advisory locks
- [x] SKIP LOCKED for concurrent worker polling
- [x] Idempotency via unique constraint on `(event_id, created_at)`
- [x] LISTEN/NOTIFY trigger (schema defined, not used yet)
- [x] Helper functions for cleanup and partition management

**Location**: `docs/schema.sql`

### Store Abstraction ✅
- [x] `Store` trait unifying Queue + State operations
- [x] Methods: publish, poll_next, ack, nack, load_state, save_state
- [x] Effect execution methods: insert_intent, poll_next_effect, complete_effect, fail_effect, dlq_effect

**Location**: `crates/seesaw/src/store.rs`

### PostgreSQL Adapter ✅
- [x] `PostgresStore` implementing Store trait
- [x] Dynamic queries (no compile-time sqlx! macros)
- [x] Separate crate per architectural decision
- [x] FromRow derives for result mapping
- [x] 14 comprehensive integration tests (ALL PASSING)
  - Queue operations (publish, poll, ack, nack)
  - Idempotency
  - Per-workflow FIFO ordering
  - Concurrent workers with SKIP LOCKED
  - State management (save, load, optimistic locking)
  - Effect execution (insert, poll, complete, fail, DLQ)
  - Priority ordering
  - End-to-end workflows

**Location**: `crates/seesaw-postgres/`

### Event Worker ✅
- [x] Polls events from queue
- [x] Runs reducers (state transitions)
- [x] Inserts effect intents into seesaw_effect_executions
- [x] Per-workflow FIFO with advisory locks
- [x] Handles idempotency checks

**Location**: `crates/seesaw/src/runtime/event_worker.rs`

### Effect Worker ✅
- [x] Polls effect executions from queue
- [x] Priority-based ordering (lower number = higher priority)
- [x] Timeout handling
- [x] Retry logic with max_attempts
- [x] DLQ for permanently failed effects
- [x] Attempts tracking

**Location**: `crates/seesaw/src/runtime/effect_worker.rs`

### Runtime ✅
- [x] Manages event and effect workers
- [x] Spawns N event workers + M effect workers
- [x] JoinSet for worker lifecycle

**Location**: `crates/seesaw/src/runtime/mod.rs`

### ProcessFuture ✅
- [x] `.process(event)` returns ProcessFuture
- [x] `.wait(matcher)` for terminal event matching
- [x] Lazy event publishing (not yet published to queue)

**Location**: `crates/seesaw/src/process.rs`

---

## ❌ Critical Missing Pieces (Phase 3 Incomplete)

### 1. Effect IDs ❌ **CRITICAL**
**Status**: Not implemented
**Plan Reference**: Lines 34, 1053, 1113, checklist line 2907

**Missing**:
- `.id("name")` method on EffectBuilder
- `id: String` field on Effect struct
- `effect_id: String` field on EffectContext
- Auto-generation fallback if not specified

**Impact**:
- Cannot populate `produced_by` in event envelopes
- Debugging shows "Effect #3 failed" instead of "send_email failed"
- No metrics per effect
- Visualization edges have no labels

**Required For**: Task #21

---

### 2. Event Envelope Metadata ❌ **CRITICAL**
**Status**: Not implemented
**Plan Reference**: Lines 32-43, envelope refinement discussion

**Missing Schema Fields**:
- `produced_by VARCHAR(255)` - Which effect returned this event
- `dispatched_at TIMESTAMPTZ` - When engine dispatched event
- `state_version BIGINT` - State snapshot at dispatch time
- `metadata JSONB` - Extensible key-value pairs
- Consider renaming: correlation_id → correlation_id, parent_id → causation_id, hops → depth

**Missing Code**:
- QueuedEvent doesn't have envelope fields
- Effects don't populate produced_by when returning events
- No state_version tracking

**Impact**:
- Cannot visualize event flows (no causality tracking)
- Cannot replay accurately (no state context)
- Cannot debug which effect produced which event
- Missing observability data

**Required For**: Task #22

---

### 3. Inline vs Queued Effects ❌ **CRITICAL**
**Status**: Not implemented
**Plan Reference**: Lines 1044-1126

**Missing**:
- `.inline()` method on EffectBuilder
- `is_inline: bool` field on Effect
- EventWorker branching logic (inline vs queued)
- Atomic event emission with inline effects

**Current Behavior**: All effects are queued (no inline execution)

**Impact**:
- Cannot execute fast effects in same transaction as reducer
- Cannot guarantee atomicity between state change and effect
- Performance hit for simple validations

**Use Cases**:
- Inline: Fast validations, immediate state-dependent logic, atomic event emission
- Queued: External APIs, slow operations, independent side effects

**Required For**: Task #23

---

### 4. Deterministic Event ID Generation ❌ **CRITICAL**
**Status**: Not implemented
**Plan Reference**: Lines 54-88, 1079-1100, 1219-1253

**Missing**:
- Deterministic ID generation when effects return events
- Formula: `hash(parent_event_id, effect_id, event_type)`
- Used in both EventWorker (inline) and EffectWorker (queued)

**Current Behavior**: Effects don't emit events yet (TODO comments)

**Impact**:
- Crash+retry creates duplicate events
- No idempotency for event emission
- Production bug waiting to happen

**Required For**: Task #24

---

### 5. process_with_id() for Webhooks ❌ **CRITICAL**
**Status**: Not implemented
**Plan Reference**: Lines 93-116, checklist line 2905

**Missing**:
- `.process_with_id(uuid, event)` method on Engine
- Accepts external event_id (e.g., Stripe webhook ID)
- Uses unique constraint for deduplication

**Current Behavior**: Only `.process()` exists (generates random UUID)

**Impact**:
- Webhook retries create duplicate events
- No external idempotency
- Production webhooks will break

**Required For**: Task #25

---

### 6. LISTEN/NOTIFY for .wait() ❌ **CRITICAL**
**Status**: Schema defined, not used
**Plan Reference**: Lines 835-853, CQRS support

**Missing**:
- WaitFuture doesn't actually listen to PostgreSQL notifications
- Current implementation is stubbed (doesn't wait)
- No connection to LISTEN channel `seesaw_workflow_{correlation_id}`

**Impact**:
- `.wait()` doesn't work
- Cannot wait for terminal events
- CQRS pattern broken

**Required For**: Task #26

---

### 7. Idempotency Key in Context ❌ **CRITICAL**
**Status**: Not implemented
**Plan Reference**: Lines 1041, 1201-1204, checklist line 2909

**Missing**:
- `idempotency_key: String` field on EffectContext
- Deterministic generation per effect execution
- Accessor method `ctx.idempotency_key()`

**Impact**:
- External API calls (Stripe, etc.) not idempotent on crash+retry
- Payment charges could duplicate
- Production bug waiting to happen

**Required For**: Task #27

---

### 8. Graceful Shutdown ❌ **CRITICAL**
**Status**: Not implemented
**Plan Reference**: Checklist line 2912

**Missing**:
- `.shutdown()` method on Runtime
- Signal handler for CTRL-C
- Wait for in-flight work to complete
- Close database connections

**Impact**:
- Lost events on shutdown
- Database connection leaks
- Incomplete transactions

**Required For**: Task #28

---

## 🔄 Partially Implemented

### Event Emission from Effects
**Status**: Stubbed with TODO comments
**Location**:
- `crates/seesaw/src/runtime/event_worker.rs:125` - "TODO: Emit next events"
- `crates/seesaw/src/runtime/effect_worker.rs:125` - "TODO: Emit next events"

**What's Missing**:
- Actually inserting returned events into seesaw_events
- Using deterministic event IDs
- Atomic insertion with effect completion
- Incrementing hops counter

---

## 📊 Completion Status by Phase

### Phase 1: Core Queue ✅ 100%
- Schema ✅
- PostgresQueue ✅
- Basic methods (publish, poll, ack) ✅
- Tests ✅

### Phase 2: State Management ✅ 100%
- Schema ✅
- State load/save ✅
- Optimistic locking ✅
- Effect executions table ✅
- Tests ✅

### Phase 3: Two-Phase Workers ⚠️ 60%
- EventWorker structure ✅
- EffectWorker structure ✅
- Runtime structure ✅
- Event emission ❌ (stubbed)
- Inline effects ❌
- Effect IDs ❌
- Deterministic event IDs ❌
- Idempotency keys ❌

### Phase 3.5: Production Hardening ❌ 0%
- Graceful shutdown ❌
- LISTEN/NOTIFY ❌
- Webhook idempotency ❌
- Envelope metadata ❌

---

## 🎯 Priority Order for Completion

### P0 - Blocking Production (Do First)
1. **Task #21**: Effect IDs - Required for everything else
2. **Task #24**: Deterministic event IDs - Prevents duplicate events
3. **Task #27**: Idempotency keys - Prevents duplicate API calls
4. **Task #25**: process_with_id() - Required for webhooks

### P1 - Core Functionality (Do Second)
5. **Task #23**: Inline vs queued effects - Performance + semantics
6. **Task #26**: LISTEN/NOTIFY - Makes .wait() actually work
7. **Task #22**: Envelope metadata - Observability

### P2 - Production Stability (Do Third)
8. **Task #28**: Graceful shutdown - Prevents data loss

---

## 📝 Migration Checklist from Plan

From plan line 2902-2912:

- [ ] **Remove `correlation_id` from all event structs** - now in envelope
- [ ] Create `Runtime` and spawn workers ✅ (done)
- [ ] Replace `handle.run(|_| Ok(Event))` with `engine.process(event)` ✅ (done)
- [ ] For webhooks, use `engine.process_with_id(webhook_id, event)` ❌ (missing)
- [ ] Replace `handle.settled()` with `engine.shutdown()` ❌ (missing)
- [ ] Add `.id("name")` to all effects ❌ (missing - compile won't enforce yet)
- [ ] Use `ctx.correlation_id` instead of `event.correlation_id` ✅ (done)
- [ ] Use `ctx.idempotency_key` for external API calls ❌ (missing)
- [ ] Add database setup (migrations for queue tables) ✅ (done)
- [ ] Configure connection pool sizing ⚠️ (not configured)
- [ ] Add graceful shutdown handler ❌ (missing)

---

## 🔍 Code Locations Reference

```
crates/
├── seesaw/
│   ├── src/
│   │   ├── store.rs              ✅ Store trait
│   │   ├── process.rs            ✅ ProcessFuture, ⚠️ WaitFuture (stubbed)
│   │   ├── engine_v2.rs          ✅ Queue-backed Engine
│   │   ├── effect/
│   │   │   ├── builders.rs       ⚠️ Missing .id() and .inline()
│   │   │   ├── types.rs          ⚠️ Effect missing id field
│   │   │   └── context.rs        ⚠️ Missing effect_id, idempotency_key
│   │   └── runtime/
│   │       ├── event_worker.rs   ⚠️ Missing inline effect execution
│   │       ├── effect_worker.rs  ⚠️ Missing event emission
│   │       └── mod.rs            ⚠️ Missing graceful shutdown
│   └── Cargo.toml
└── seesaw-postgres/
    ├── src/
    │   ├── lib.rs                ✅ PostgresStore implementation
    │   └── lib_checked.rs        ✅ Compile-time checked version
    ├── tests/
    │   └── integration_tests.rs  ✅ 14 tests, all passing
    └── Cargo.toml

docs/
├── schema.sql                    ✅ Production schema
└── plans/
    └── 2026-02-05-feat-queue-backed-architecture-plan.md  📋 The source of truth
```

---

## 🚀 Next Steps

1. **Review this document with the team**
2. **Prioritize P0 tasks** (Effect IDs, Deterministic IDs, Idempotency keys, Webhooks)
3. **Implement P0 tasks in order** (Tasks #21, #24, #27, #25)
4. **Test thoroughly** after each task
5. **Move to P1 tasks** (Inline effects, LISTEN/NOTIFY, Envelopes)
6. **Production hardening** (Graceful shutdown)

**Estimated Time**:
- P0 tasks: 2-3 days
- P1 tasks: 2-3 days
- P2 tasks: 1 day
- **Total**: ~1 week to production-ready

---

## ✅ What's Working Well

- ✅ **Schema design is solid** - Handles millions of events
- ✅ **Store abstraction is clean** - Easy to test and extend
- ✅ **Separate crate architecture** - Good modularity
- ✅ **Integration tests are comprehensive** - 14 tests covering all major operations
- ✅ **Per-workflow FIFO works** - Advisory locks + SKIP LOCKED
- ✅ **Priority ordering works** - Lower number = higher priority
- ✅ **DLQ works** - Failed effects are tracked
- ✅ **Optimistic locking works** - State version conflicts detected

---

## 📚 References

- Plan: `docs/plans/2026-02-05-feat-queue-backed-architecture-plan.md`
- Schema: `docs/schema.sql`
- Tasks: See tasks #21-#28 in task tracker
