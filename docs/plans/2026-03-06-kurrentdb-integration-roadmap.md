---
date: 2026-03-06
topic: kurrentdb-integration
---

# KurrentDB Integration Roadmap

## Goal

Implement `causal-kurrentdb`, a Store backend that uses KurrentDB (formerly
EventStoreDB) as the durable event log and handler queue. Causal's Store trait
is already shaped for this — the v0.22 changes (`AppendResult`, stream-version
filtering, opaque position cursors, total idempotency contract) were
specifically designed to align with KurrentDB's semantics.

## What KurrentDB Gives Us

- **Per-stream ordering** with monotonic stream versions (natural fit for
  aggregate hydration via `load_stream`)
- **Global $all stream** with commit positions (maps to `load_global_from`)
- **Persistent subscriptions** for catch-up and competing consumers
- **Optimistic concurrency** via expected stream version on append
- **EventId-based dedup** (~1 minute cache; causal requires total idempotency,
  so we need a supplemental dedup mechanism)

## What Causal Already Has (v0.22)

| Store trait surface | KurrentDB mapping | Status |
|---|---|---|
| `append_event` returns `AppendResult { position, version }` | Commit position + stream revision | Done |
| `load_stream(after_version)` filters by stream version | Read stream from revision | Done |
| `load_global_from(after_position, limit)` | Read $all from commit position | Done |
| Total idempotency doc on `append_event` | Guides dedup index design | Done |
| `PersistedEvent.position` is "opaque cursor" | Commit position (gaps allowed) | Done |

## Phases

### Phase 1: Event persistence (read/write path)

Implement the optional persistence methods on Store:

- **`append_event`** — Append to a KurrentDB stream named
  `{aggregate_type}-{aggregate_id}` (or a global stream for non-aggregate
  events). Use `EventData::json()` with the causal `event_id` as the
  KurrentDB `EventId`. Return `AppendResult` from the write result's
  commit position and stream revision.

- **Idempotency beyond the dedup window** — KurrentDB's EventId cache expires
  after ~1 minute. Options:
  1. **Expected version guard**: append with `ExpectedRevision::Exact(n)`.
     A duplicate will see a version conflict; read back the stream to confirm
     it's a true duplicate vs. a legitimate conflict.
  2. **External dedup index**: a Postgres/Redis set of `event_id -> (position,
     version)`. Check before append, write after. Simpler but adds a
     dependency.
  3. **Hybrid**: use expected version for hot path, dedup index for crash
     recovery edge cases.

- **`load_stream`** — Read the per-aggregate stream from
  `StreamPosition::from(after_version + 1)`. Map each resolved event to
  `PersistedEvent`.

- **`load_global_from`** — Subscribe to `$all` from the given commit position
  with a count limit. Filter out system events (`$` prefix).

- **Snapshots** (`load_snapshot` / `save_snapshot`) — Store snapshots in a
  dedicated stream `{aggregate_type}-{aggregate_id}-snapshot` or in a
  separate backing store (Postgres/Redis). KurrentDB doesn't have native
  snapshot support, so this is an application-level concern.

### Phase 2: Handler queue

The handler queue (publish, poll_next, complete_event, poll_next_handler,
resolve_handler) is the harder problem. KurrentDB is an event store, not a
job queue. Options:

- **Option A: KurrentDB persistent subscriptions as the queue.** Use
  competing consumers for handler distribution. Track handler state
  (pending/running/completed) in a separate metadata stream or projection.
  Ack/nack maps to resolve_handler semantics.

- **Option B: Hybrid — KurrentDB for events, Postgres/SQLite for queues.**
  The event log lives in KurrentDB. The handler queue, join windows,
  journals, and cancellation state live in a relational store. This is
  simpler and plays to each system's strengths.

- **Option C: KurrentDB projections as queue driver.** Custom projections
  emit to handler-specific streams. Workers subscribe to their stream.
  Elegant but couples deeply to KurrentDB's projection system.

**Recommendation: Option B (hybrid).** The queue operations (poll with skip
locked, atomic status transitions, join window accounting) are inherently
relational. Forcing them into an event store adds complexity without benefit.
The event log is what KurrentDB excels at.

### Phase 3: Cross-node sync

With KurrentDB as the shared event log:

- **Catch-up subscriptions** replace polling `load_global_from`. Each node
  subscribes to `$all` and publishes events into its local causal engine.
- **Checkpointing**: each node persists its last-seen commit position so it
  can resume after restart.
- **Ordering guarantee**: `$all` is totally ordered, so events arrive in
  causal order across nodes.

### Phase 4: Production hardening

- **Connection pooling / reconnection** for the KurrentDB gRPC client
- **Batched appends** where multiple events from one handler can be appended
  atomically
- **Metrics**: append latency, subscription lag, dedup cache hit rate
- **Integration tests** against a real KurrentDB instance (Docker)

## Crate Structure

```
crates/
  causal/                    # core (Store trait, MemoryStore, engine)
  causal-kurrentdb/          # KurrentDB Store implementation
    src/
      lib.rs                 # KurrentDbStore struct + Store impl
      event_mapper.rs        # NewEvent <-> EventData conversion
      stream_naming.rs       # aggregate type/id -> stream name
      dedup.rs               # idempotency beyond the EventId cache
```

## Open Questions

1. **Stream naming convention** — `{AggregateType}-{aggregate_id}` is the
   EventStoreDB convention. Should causal enforce this or let the user
   configure it?

2. **Non-aggregate events** — Where do events without an aggregate go?
   A single `causal-global` stream? Per-event-type streams? The global `$all`
   handles reads, but writes need a target stream.

3. **Metadata mapping** — KurrentDB events have a separate metadata JSON
   field. Map causal's `NewEvent.metadata` there, or embed in the payload?
   Metadata field is more idiomatic and enables server-side projections that
   read correlation_id without parsing the payload.

4. **Which KurrentDB Rust client?** — `eventstore-rs` (official gRPC client)
   or `kurrentdb-rs` if a renamed crate exists.

## Dependencies

- KurrentDB Rust client (gRPC)
- `serde_json` (already in causal)
- Optional: Postgres/Redis for dedup index (Phase 1) and handler queue
  (Phase 2 Option B)
