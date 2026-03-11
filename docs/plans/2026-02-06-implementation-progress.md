# Implementation Progress - Distributed Production Readiness

**Date:** 2026-02-06
**Status:** Phase 1 - In Progress

## Summary

We're implementing the critical safety features to make Causal production-ready for distributed systems. This document tracks our progress through the roadmap defined in `2026-02-06-distributed-production-readiness.md`.

---

## Phase 1: Failure Safety (Week 1)

### ✅ Task 1.1: Dead Letter Queue - Schema (DONE)

**Files Created:**
- `/migrations/20260206_add_dead_letter_queue.sql`

**What was done:**
- Created `causal_dlq_status` enum with lifecycle states: `open`, `retrying`, `replayed`, `resolved`
- Created `causal_dead_letter_queue` table with:
  - Unique constraint on `intent_id` (prevents duplicate retries)
  - Full error tracking (message, details, retry counts)
  - Lifecycle tracking (status, retry attempts, resolution notes)
  - Original event payload (for replay)
- Created optimized indexes:
  - `idx_dlq_handler_open` - Find open entries by handler (partial index)
  - `idx_dlq_event` - Find all DLQ entries for an event
  - `idx_dlq_status` - Find entries by status

**Exit criteria met:**
- [x] Schema supports immutable history
- [x] Unique constraint prevents duplicate retries
- [x] Status enum tracks full lifecycle
- [x] Indexes support common query patterns

---

### ✅ Task 1.2: Dead Letter Queue - Core Logic (DONE)

**Files Created:**
- `/crates/causal/src/dead_letter_queue.rs`
- Updated `/crates/causal/src/lib.rs` to export DLQ types

**What was done:**
- Created `DeadLetterQueue` struct with all CRUD operations:
  - `insert()` - Add failed handler to DLQ (with ON CONFLICT for updates)
  - `list()` / `list_by_handler()` - Query open entries
  - `get()` - Fetch single entry by ID
  - `start_retry()` - Mark as retrying with row lock (`FOR UPDATE`)
  - `mark_replayed()` - Mark as successfully replayed
  - `mark_retry_failed()` - Revert to open status on retry failure
  - `mark_resolved()` - Mark as manually resolved
  - `stats()` - Get DLQ statistics
- Created supporting types:
  - `DlqStatus` enum
  - `DeadLetter` struct (sqlx::FromRow)
  - `RetrySummary` for bulk operations
- Row-level locking on retry prevents concurrent retry attempts

**Exit criteria met:**
- [x] Insert operation supports ON CONFLICT for idempotency
- [x] Retry operations use `FOR UPDATE` lock
- [x] All lifecycle states supported
- [x] Statistics available for monitoring

---

### ✅ Task 1.3: Graceful Shutdown - Core Logic (DONE)

**Files Created:**
- `/crates/causal/src/runtime/graceful_shutdown.rs`
- Updated `/crates/causal/src/runtime/mod.rs` to export graceful_shutdown module

**What was done:**
- Created `GracefulShutdown` coordinator using `CancellationToken`:
  - Signal handling for SIGTERM/SIGINT
  - Cancellation token shared across all workers
  - Configurable drain timeout (default: 30 seconds)
- Created `drain_tasks()` method:
  - Uses `JoinSet` to track all spawned tasks
  - Waits for tasks with timeout
  - Reports completion statistics (completed/errors)
  - Aborts remaining tasks on timeout
- Created `ShutdownAwareWorker` helper:
  - Template for worker loop with graceful shutdown
  - Stops claiming new work immediately on shutdown
  - Drains in-flight tasks with proper error handling
- Added comprehensive unit tests:
  - Tasks complete successfully
  - Timeout handling
  - Error propagation

**Exit criteria met:**
- [x] CancellationToken used for coordinated shutdown
- [x] JoinSet tracks spawned tasks explicitly
- [x] Drain timeout enforced
- [x] Statistics logged on drain completion
- [x] Tests cover success, timeout, and error cases

---

### ⚠️ Task 1.4: Integration - DLQ with Handler Worker (TODO)

**What needs to be done:**
- Update `HandlerWorker::process_next_effect()` to use new DLQ API
- Replace existing `store.dlq_effect()` calls with `DeadLetterQueue::insert()`
- Add proper error context when moving to DLQ
- Ensure `intent_id` is passed correctly

**Files to modify:**
- `/crates/causal/src/runtime/handler_worker.rs` (lines 140-165)

**Exit criteria:**
- [ ] Handler worker uses new DLQ API
- [ ] All permanent failures go to DLQ
- [ ] Error details properly captured
- [ ] Integration test verifies DLQ entry created

---

### ⚠️ Task 1.5: Integration - Graceful Shutdown with Runtime (TODO)

**What needs to be done:**
- Update `Runtime::start()` to use `GracefulShutdown`
- Replace `Arc<AtomicBool>` with `CancellationToken`
- Update worker loop in `HandlerWorker::run()` to use shutdown token
- Update worker loop in `EventWorker::run()` to use shutdown token
- Implement proper drain logic in `Runtime::shutdown()`

**Files to modify:**
- `/crates/causal/src/runtime/mod.rs`
- `/crates/causal/src/runtime/handler_worker.rs`
- `/crates/causal/src/runtime/event_worker.rs`

**Exit criteria:**
- [ ] Runtime uses CancellationToken instead of AtomicBool
- [ ] Workers stop claiming on shutdown signal
- [ ] In-flight tasks tracked with JoinSet
- [ ] Drain timeout enforced
- [ ] Integration test verifies zero lost work on shutdown

---

### ⚠️ Task 1.6: DLQ Management API (TODO)

**What needs to be done:**
- Create retry API for operators:
  - `retry_dead_letter(dlq_id)` - Retry single entry
  - `retry_all_by_handler(handler_id)` - Bulk retry with bounded batches
- Add proper error handling for retry failures
- Update DLQ status on success/failure

**Files to create:**
- `/crates/causal/src/dlq_operations.rs` (or add to existing dead_letter_queue.rs)

**Exit criteria:**
- [ ] Single entry retry with row lock
- [ ] Bulk retry with LIMIT 100 per batch
- [ ] Retry failures update DLQ status (not delete)
- [ ] Tests cover duplicate retry prevention

---

### ✅ Task 1.7: Tests - DLQ Lifecycle (DONE)

**Files created:**
- `/crates/causal/tests/dlq_integration.rs` - 6 comprehensive DLQ tests

**What was done:**
- Test 1: `test_permanent_failure_creates_dlq_entry` - Handler fails 3 times → DLQ entry created
- Test 2: `test_dlq_retry_replays_event` - Retry from DLQ → Event successfully replayed
- Test 3: `test_dlq_duplicate_retry_prevented` - Row lock prevents concurrent retry attempts
- Test 4: `test_partial_batch_failure_creates_dlq_entries` - Multiple events, only failures go to DLQ
- Test 5: `test_dlq_statistics_accurate` - DLQ stats reflect actual state distribution
- Test 6: `test_crash_during_retry_preserves_state` - Entry stays in retrying state after crash

**Exit criteria met:**
- [x] All critical paths tested with TDD approach
- [x] Tests compile successfully
- [x] Tests use production-ready patterns (PostgresStore, real migrations)
- [ ] Tests will fail until DLQ integration completed (expected in TDD)

---

### ✅ Task 1.8: Tests - Graceful Shutdown (DONE)

**Files created:**
- `/crates/causal/tests/graceful_shutdown_integration.rs` - 6 comprehensive shutdown tests

**What was done:**
- Test 1: `test_shutdown_completes_inflight_tasks` - All started handlers complete before shutdown
- Test 2: `test_shutdown_stops_accepting_new_work` - No new work after shutdown signal
- Test 3: `test_shutdown_timeout_aborts_tasks` - Timeout enforced after configured duration
- Test 4: `test_worker_restart_after_shutdown` - Clean restart with no state corruption
- Test 5: `test_concurrent_shutdown_calls_safe` - Shutdown can be called safely (once integrated)
- Test 6: `test_shutdown_during_handler_error` - Shutdown waits for failing handlers too

**Exit criteria met:**
- [x] Zero lost work scenarios tested
- [x] Timeout behavior tested (2s timeout enforced)
- [x] Tests compile successfully
- [ ] Tests will fail until graceful shutdown integration completed (expected in TDD)

---

## Phase 2: Operability Baseline (Week 1)

### ⚠️ Task 2.1: Structured Logging (TODO)

**What needs to be done:**
- Add tracing instrumentation to all handler paths
- Include contextual fields: `event_id`, `handler_id`, `correlation_id`
- Normalize error fields and retry context
- Add spans for handler execution

**Files to modify:**
- `/crates/causal/src/runtime/handler_worker.rs`
- `/crates/causal/src/runtime/event_worker.rs`

**Exit criteria:**
- [ ] Can trace failed event end-to-end from logs alone
- [ ] All critical paths instrumented
- [ ] Correlation IDs propagate correctly

---

### ⚠️ Task 2.2: Metrics (TODO)

**What needs to be done:**
- Add Prometheus metrics:
  - Counters: dispatched, executed, failed, retried, dlq_entered
  - Gauges: queue_depth, in_flight_handlers, worker_count
  - Histograms: handler_duration, dispatch_latency
- Expose /metrics HTTP endpoint

**Files to create:**
- `/crates/causal/src/metrics.rs`

**Exit criteria:**
- [ ] All key metrics exported
- [ ] Alert on sustained queue growth
- [ ] Alert on failure-rate spike
- [ ] Alert on DLQ growth

---

## Phase 3: Load Protection (Week 2)

### ⚠️ Task 3.1: Backpressure - Admission Control (TODO)

**What needs to be done:**
- Implement dispatch-side soft/hard limits
- Add queue depth checking before dispatch
- Return error when hard limit exceeded
- Apply delay when soft limit exceeded

**Files to modify:**
- `/crates/causal/src/engine_v2.rs`

**Exit criteria:**
- [ ] Under load test, queue growth stabilizes
- [ ] System recovers after burst
- [ ] Callers receive actionable errors

---

### ⚠️ Task 3.2: Backpressure - Worker Drain Mode (TODO)

**What needs to be done:**
- Adjust worker behavior when backlog is high
- Increase batch size when queue depth > threshold
- Reduce poll delay when queue depth > threshold
- Never pause/sleep when backlog exists

**Files to modify:**
- `/crates/causal/src/runtime/handler_worker.rs`

**Exit criteria:**
- [ ] Workers drain faster when backlogged
- [ ] No pausing when work available

---

### ⚠️ Task 3.3: Configuration Management (TODO)

**What needs to be done:**
- Create `WorkerConfig` struct with validation
- Load from environment variables
- Load from TOML file
- Fail fast on invalid config

**Files to create:**
- `/crates/causal/src/config.rs`

**Exit criteria:**
- [ ] Invalid config prevents boot
- [ ] Actionable error messages
- [ ] All limits validated

---

## Implementation Statistics

### Files Created: 6
1. `/migrations/20260206_add_dead_letter_queue.sql` - DLQ schema
2. `/crates/causal/src/dead_letter_queue.rs` - DLQ core logic
3. `/crates/causal/src/runtime/graceful_shutdown.rs` - Graceful shutdown
4. `/crates/causal/tests/dlq_integration.rs` - DLQ integration tests (TDD)
5. `/crates/causal/tests/graceful_shutdown_integration.rs` - Graceful shutdown integration tests (TDD)
6. `/docs/plans/2026-02-06-implementation-progress.md` - This file

### Files Modified: 7
1. `/crates/causal/src/lib.rs` - Added DLQ exports
2. `/crates/causal/src/runtime/mod.rs` - Added graceful_shutdown module
3. `/Cargo.toml` - Added tokio-util workspace dependency
4. `/crates/causal/Cargo.toml` - Added sqlx, tokio-util, causal-postgres dependencies
5. `/crates/causal/src/handler/builders.rs` - Fixed join_window → join_window_timeout mappings
6. `/crates/causal/src/runtime/graceful_shutdown.rs` - Fixed CancellationToken borrowing
7. `/crates/causal/src/dead_letter_queue.rs` - Converted from time to chrono

### Lines of Code: ~1100
- DLQ schema: ~50 lines
- DLQ core logic: ~300 lines
- Graceful shutdown: ~200 lines
- DLQ integration tests: ~375 lines
- Graceful shutdown integration tests: ~430 lines

### Tests Written: 15
- Graceful shutdown unit tests: 3 tests
- DLQ integration tests: 6 tests (TDD - will fail until integration)
- Graceful shutdown integration tests: 6 tests (TDD - will fail until integration)

---

## Next Steps (Immediate)

**Priority 1: Complete Phase 1**
1. Integrate DLQ with HandlerWorker (Task 1.4)
2. Integrate GracefulShutdown with Runtime (Task 1.5)
3. Write DLQ lifecycle tests (Task 1.7)
4. Write graceful shutdown tests (Task 1.8)

**Priority 2: Start Phase 2**
5. Add structured logging (Task 2.1)
6. Add metrics (Task 2.2)

**Estimated time remaining:**
- Phase 1 completion: 2-3 days
- Phase 2: 2-3 days
- Total: 1 week to complete Phase 1 + 2

---

## Blockers

**None currently** - All dependencies available, schema ready, core logic implemented.

---

## Notes

**Good progress!** Core DLQ and graceful shutdown logic is complete. The foundation is solid:
- DLQ schema supports full lifecycle with immutable history
- Graceful shutdown uses proper Tokio patterns (CancellationToken + JoinSet)
- All operations are transaction-safe and lock-protected

**Next step:** Integration with existing worker code to wire everything together.
