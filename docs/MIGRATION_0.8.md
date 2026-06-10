# Migrating across the 2026-06-10 audit-remediation pass

Upgrade guide for consumers moving from `0.7.4` to the post-remediation
`[Unreleased]` shape. Full rationale in
[`CHANGELOG.md`](../CHANGELOG.md) `[Unreleased]`.

**The short version:** one change touches normal application code
(`build().await?`); everything else is either a symbol you weren't using
or a latent bug now surfaced loudly. The traits you implement are
unchanged.

---

## What did NOT change

The backend and consumer trait contracts are identical — no method you
implement changed signature:

`EventLogBackend`, `CheckpointStore`, `ReactorCheckpoint`,
`SnapshotStore`, `Event`, `Aggregate`, `Reactor`, `Projector`,
`MultiProjector`.

The Postgres schema is unchanged (no new migration to vendor). The
`#[event]`, `#[aggregator]`, `#[aggregators]` macros are unchanged.

---

## 1. `EngineBuilder::build()` is async + fallible — **required**

The only change that touches normal code. `build` now seeds fresh
reactor cursors and validates categories before returning, both of which
do I/O.

```rust
// before
let engine = EngineBuilder::new(log, checkpoint, reactor_checkpoint)
    .with_reactor(MyReactor)
    .build();

// after
let engine = EngineBuilder::new(log, checkpoint, reactor_checkpoint)
    .with_reactor(MyReactor)
    .build()
    .await?;          // ← now async + Result
```

If your construction site isn't already in an async fn, make it one (or
`block_on` it at startup).

### Behavioral consequence: fresh reactors no longer replay history

A reactor registered against a log that already has events now seeds its
cursor at `latest_position()` — it does **not** re-fire side effects for
pre-existing events. This is the correct default (a newly deployed
reactor shouldn't re-send every historical email), but it is a behavior
change if you relied on the old replay-from-zero.

- Want a reactor to process history? Use the explicit opt-in:
  ```rust
  .with_reactor_start(MyReactor, causal::StartPosition::Zero)
  ```
- **Test fixtures that append triggers before `build()`** now find those
  triggers treated as history (reactors won't fire for them). Either
  emit *after* `build()`, or register with `StartPosition::Zero`.

Projectors are unaffected — they still start from `LogCursor::ZERO` and
see full history (read models want it; side effects don't).

---

## 2. Reading an unregistered aggregate now panics — **check your reads**

`ctx.aggregate::<A>()`, `ctx.aggregate_of::<A>(id)`, `engine.snapshot::<A>(id)`,
and `engine.load_aggregate::<A>(id)` previously returned `A::default()`
when no aggregator for `A` had been registered. They now **panic**,
naming the type.

This is the one change most likely to surface a latent bug: if you read
an aggregate you forgot to register, that read was silently returning
empty state (e.g. a dedup gate that never fired). Two fixes:

```rust
// either register the aggregators that fold A …
let engine = builder.with_aggregators(my_aggregators::aggregators()).build().await?;

// … or remove the read if A genuinely has no aggregators.
```

A legitimately-empty *registered* aggregate is unaffected — it still
returns its `Default`/`None`.

---

## 3. `with_observer` takes a trait object — usually no change

```rust
// signature changed: with_observer<O>(Arc<O>)  →  with_observer(Arc<dyn ReactorObserver>)
.with_observer(store.clone())   // a concrete Arc<MyObserver> still coerces — no edit needed
```

You only need to touch this if you named the generic explicitly
(`with_observer::<MyObserver>(...)`) — drop the turbofish.

---

## 4. Removed symbols — delete if you used them (you probably didn't)

All of these were exported but wired to nothing in the runtime; if your
build references them, remove the reference.

| Removed | Replacement |
|---|---|
| `Upcaster`, `UpcasterRegistry` | none yet — re-introduced wired when schema evolution lands |
| `ProjectionMode`, `RetryPolicy`, `Backoff`, `FailureBehavior` | none (were unread config) |
| `ProjectionOps`, `ProjectionStatus`, `ProjectionFailure` | none (DLQ ops surface was unbuilt) |
| `#[reactor]`, `#[reactors]`, `#[projection]` macros | hand-write the trait impl (two items each) |
| `#[derive(DistributedSafe)]` | none |

`StartPosition` is **kept** (now wired — see §1).

---

## 5. Category naming rule — now enforced

A `:` in an `Event::CATEGORY` (or `Aggregate::STREAM_CATEGORY`) is now
rejected at `build()` with a clear error. The colon is the
`{category}:{name}` separator; one inside a category previously made
reactors fire while the aggregate silently never folded. If you hit the
error, rename the category to be colon-free. Categories must also be
non-empty.

---

## 6. `REPLAY` env var (only if you drive replay)

`causal_replay` now parses `REPLAY` strictly. If any script sets
`REPLAY=0` (or `false`) intending to *disable* replay, that now works as
expected — previously *any* value, including `0`, triggered a full
replay-and-promote. Use `REPLAY=1` to enable.

---

## Checklist

- [ ] `build()` → `build().await?` at every engine-construction site.
- [ ] Confirm no `ctx.aggregate*` / `snapshot` / `load_aggregate` reads
      a type you never registered (it will now panic, not return empty).
- [ ] Re-emit-after-build (or `StartPosition::Zero`) for any test that
      seeded triggers before `build()`.
- [ ] Grep for the removed symbols in §4 (likely zero hits).
- [ ] Grep `Event::CATEGORY` / `STREAM_CATEGORY` consts for `:`.
- [ ] Drop any `with_observer::<T>` turbofish.
