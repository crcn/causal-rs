---
date: 2026-03-01
topic: a-plus-gaps
---

# Seesaw A+ Gaps: Evaluation & Plan

## Context

After implementing upcasters (v0.15.10), evaluated what's needed to bring seesaw from "good library" to "A+ production framework."

## Gaps (Priority Order)

### 1. `replay_events` must require `&UpcasterRegistry` (bug fix)

`load_aggregate()` calls `replay_events()` without upcasters. Old persisted events fail deserialization. Fix: make upcasters a required parameter — no way to forget them.

**Status: Implementing now.**

### 2. Settled timeout

`.settled()` can block forever on long-but-finite handler chains. Hop limit catches infinite loops, but no wall-clock protection.

```rust
engine.emit(event)
    .settled()
    .timeout(Duration::from_secs(30))
    .await?;
```

### 3. Examples for key patterns

Missing runnable examples for:
- Event sourcing with `EventStore` + `persist_event` + `load_aggregate`
- Retry + DLQ recovery with `on_failure` compensation
- Aggregator + transition guard patterns

### 4. Snapshotting for aggregates

Every `load_aggregate` replays entire event stream. For aggregates with hundreds of events, this gets slow.

```rust
pub trait EventStore: Send + Sync {
    // ... existing ...
    fn load_snapshot(&self, aggregate_id: Uuid) -> ...;
    fn save_snapshot(&self, snapshot: Snapshot) -> ...;
}
```

Smart loader: snapshot + partial replay of events after snapshot version.

### 5. Exponential backoff on retries

Retries execute immediately. Real effects need backoff.

```rust
handler::on::<PaymentRequested>()
    .id("charge")
    .queued()
    .retry(5)
    .backoff(Duration::from_secs(1)) // base delay, doubles each retry
    .then(...)
```

### 6. Event metadata / headers

No way to pass trace IDs, tenant IDs, or custom metadata through event chains. Solves multi-tenancy, distributed tracing, and routing hints.

```rust
engine.emit(event)
    .with_metadata("tenant_id", "acme-corp")
    .settled()
    .await?;
```

Lowest priority — workarounds exist (deps, event fields, tracing spans).

### 7. Panics in handler_registry

`panic!("Duplicate effect id '{}'"` and `panic!("Handler ID cannot be empty")` should be warnings or Results, not process crashes during setup.

## Priority Ranking

| # | Gap | Impact | Effort | Priority |
|---|-----|--------|--------|----------|
| 1 | `replay_events` require upcasters | High (broken feature) | Small | **Now** |
| 2 | Settled timeout | High (production hang risk) | Small | **Now** |
| 3 | Examples (ES, retry, transitions) | High (adoption) | Medium | **Next** |
| 4 | Snapshotting | High (perf at scale) | Medium | **Next** |
| 5 | Backoff on retries | Medium (production quality) | Small | **Next** |
| 6 | Event metadata | Medium (observability) | Medium | **Later** |
| 7 | Panic -> warn in registry | Low (DX) | Tiny | **Later** |
