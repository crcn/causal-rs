# Journaling + Effect Unification Plan

## Overview

Add `ctx.run()` journaling to the Store trait and unify inline/queued effects into a single execution model.

## Phase 1: Journal on Store

Add three methods to `Store` with default no-ops:

```rust
// store.rs — new section

async fn load_journal(
    &self,
    _reactor_id: &str,
    _event_id: Uuid,
) -> Result<Vec<JournalEntry>> {
    Ok(Vec::new())
}

async fn append_journal(
    &self,
    _reactor_id: &str,
    _event_id: Uuid,
    _seq: u32,
    _value: serde_json::Value,
) -> Result<()> {
    Ok(())
}

async fn clear_journal(
    &self,
    _reactor_id: &str,
    _event_id: Uuid,
) -> Result<()> {
    Ok(())
}
```

Add `JournalEntry` to `types.rs`:

```rust
pub struct JournalEntry {
    pub seq: u32,
    pub value: serde_json::Value,
}
```

MemoryStore implements with `HashMap<(String, Uuid), Vec<JournalEntry>>`.

### Files touched
- `store.rs` — add 3 methods
- `types.rs` — add `JournalEntry`
- `memory_store.rs` — implement journal storage

## Phase 2: Wire Journal into Context

Give `JobExecutor` a `store: Arc<dyn Store>` reference. Before reactor execution, load journal entries. Pass them into Context via a `JournalState`.

```rust
// context.rs

struct JournalState {
    store: Arc<dyn Store>,
    reactor_id: String,
    event_id: Uuid,
    entries: Vec<JournalEntry>,  // preloaded, indexed by seq
    next_seq: AtomicU32,
}
```

`ctx.run()` becomes:

```rust
pub async fn run<F, Fut, T>(&self, f: F) -> Result<T>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = Result<T>> + Send + 'static,
    T: Serialize + DeserializeOwned + Send + 'static,
{
    let Some(journal) = &self.journal else {
        // No journal wired — passthrough (backward compat)
        return f().await;
    };

    let seq = journal.next_seq.fetch_add(1, Ordering::SeqCst);

    // Replay from journal if entry exists
    if let Some(entry) = journal.entries.get(seq as usize) {
        return Ok(serde_json::from_value(entry.value.clone())?);
    }

    // Execute + persist
    let result = f().await?;
    journal.store.append_journal(
        &journal.reactor_id,
        journal.event_id,
        seq,
        serde_json::to_value(&result)?,
    ).await?;

    Ok(result)
}
```

When no journal state is present (inline effects today, or MemoryStore where it doesn't matter), `run()` passes through exactly as it does now.

### Integration in engine.rs settle loop

```
// Effect success (line ~504)
EffectStatus::Success => {
    // ... existing resolve_effect(Complete) ...
    self.store.clear_journal(&reactor_id, event_id).await?;
}

// Effect retry — journal is NOT cleared (that's the whole point)

// Effect DLQ — journal kept for debugging
```

### Files touched
- `reactor/context.rs` — add `JournalState`, update `run()`
- `job_executor.rs` — accept `store`, load journal before execution
- `engine.rs` — pass store to JobExecutor, clear journal on success

## Phase 3: Unify Inline + Queued Effects

Remove the inline/queued distinction. All effects go through the effect queue.

What "inline" becomes: an effect with `priority=0` and `delay=0`. The settle loop already processes effects by priority. No special execution path needed.

### What changes

1. **Remove `run_inline_effect`** from `job_executor.rs` (line 502-639) — all effects use `execute_handler`
2. **Remove inline filtering** from `execute_event` (line 182-214) — all matched effects become `EffectIntent` entries
3. **`execute_event` simplifies to**: decode event, find matching reactors, create intents, apply projections. No more inline execution.
4. **Retry support for previously-inline effects** comes for free from the existing effect queue retry/DLQ/backoff logic.
5. **Journaling for all effects** comes for free since `execute_handler` is the single path and it loads journal state.

### What stays the same
- Projections still run inline during event processing (they're observers, not effects — no retry semantics)
- Effect builder API unchanged — `on::<E>().then(...)` works the same
- The `.inline()` builder method becomes a shorthand for `priority(0)` (or we deprecate it)

### Serialize emitted events
`serialize_emitted_events` (line 641) and the inline version (line 583-638) are nearly identical. After unification there's one codepath.

### Files touched
- `job_executor.rs` — remove `run_inline_effect`, simplify `execute_event`
- `engine.rs` — settle loop handles all effects uniformly
- `reactor/builders.rs` — `.inline()` becomes `.priority(0)` alias or is removed
- `reactor/types.rs` — remove `is_inline` field if no longer needed

## Phase 4: Tests

- `ctx.run()` replays from journal on retry
- `ctx.run()` passthrough when no journal (MemoryStore default)
- Multiple sequential `run()` calls with crash-between simulation
- Previously-inline effects now retry correctly
- Journal cleared on success, kept on DLQ

## Determinism Contract

Document: code between `ctx.run()` calls must be deterministic. Same input event must produce the same sequence of `run()` calls. Non-determinism (random, wall clock, external reads outside `run()`) breaks replay. This matches Restate's contract.

## Migration

- Phase 1-2 are additive — no breaking changes
- Phase 3 changes behavior: previously-inline effects now queue. This means `.settled()` semantics are unchanged (settle loop still drives everything), but the execution order may shift slightly since inline effects no longer run during event processing. Projections still do.
- Consider a minor version bump for phase 3
