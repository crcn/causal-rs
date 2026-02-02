# Seesaw Workspace Restructuring Plan

## Current State

Single crate `seesaw_core` with all code in one place (~416KB source).

## Proposed Structure

```
seesaw-rs/
├── Cargo.toml                    # Workspace manifest
├── README.md                     # Workspace overview
├── CLAUDE.md                     # Architecture guidelines
├── crates/
│   ├── seesaw/                   # Core framework
│   │   ├── Cargo.toml
│   │   ├── src/
│   │   │   ├── lib.rs
│   │   │   ├── core.rs           # Event, Command, CorrelationId
│   │   │   ├── bus.rs            # EventBus
│   │   │   ├── machine.rs        # Machine trait
│   │   │   ├── effect_impl.rs    # Effect trait
│   │   │   ├── dispatch.rs       # Dispatcher
│   │   │   ├── runtime.rs        # Runtime loop
│   │   │   ├── engine.rs         # Engine orchestration
│   │   │   ├── request.rs        # dispatch_request
│   │   │   ├── error.rs          # SeesawError, CommandFailed
│   │   │   ├── command_macro.rs  # auto_serialize!
│   │   │   └── audit.rs          # Debug auditing
│   │   └── tests/
│   │       ├── codesmell_tests.rs
│   │       ├── stress_tests.rs
│   │       └── serde_auto_tests.rs
│   │
│   ├── seesaw-job/               # Job queue abstractions
│   │   ├── Cargo.toml
│   │   ├── README.md
│   │   └── src/
│   │       └── lib.rs            # job.rs content
│   │
│   ├── seesaw-outbox/            # Transactional outbox
│   │   ├── Cargo.toml
│   │   ├── README.md
│   │   └── src/
│   │       └── lib.rs            # outbox.rs content
│   │
│   ├── seesaw-persistence/       # Machine state persistence
│   │   ├── Cargo.toml
│   │   ├── README.md
│   │   └── src/
│   │       └── lib.rs            # persistence.rs content
│   │
│   └── seesaw-testing/           # Testing utilities
│       ├── Cargo.toml
│       ├── README.md
│       └── src/
│           └── lib.rs            # testing.rs content
```

## Crate Breakdown

### 1. `seesaw` (Core Framework) - ~229KB

**Purpose**: Event-driven coordination kernel

**Contents**:
- Event/Command separation
- State machines (Machine trait)
- Effect handlers
- EventBus
- Runtime/Engine orchestration
- Request/response helpers
- Error types
- Debug audit log

**Dependencies**:
```toml
[dependencies]
anyhow = "1.0"
async-trait = "0.1"
chrono = "0.4"
dashmap = "6.1"
futures = "0.3"
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
tokio = { version = "1.0", features = ["sync", "rt", "macros"] }
tracing = "0.1"
uuid = { version = "1.0", features = ["v4", "serde"] }

# Optional workspace members
seesaw-job = { path = "../seesaw-job", optional = true }
seesaw-outbox = { path = "../seesaw-outbox", optional = true }
seesaw-persistence = { path = "../seesaw-persistence", optional = true }

[dev-dependencies]
seesaw-testing = { path = "../seesaw-testing" }

[features]
default = []
job = ["dep:seesaw-job"]
outbox = ["dep:seesaw-outbox"]
persistence = ["dep:seesaw-persistence"]
full = ["job", "outbox", "persistence"]
```

**Re-exports**:
```rust
// Re-export optional features when enabled
#[cfg(feature = "job")]
pub use seesaw_job as job;

#[cfg(feature = "outbox")]
pub use seesaw_outbox as outbox;

#[cfg(feature = "persistence")]
pub use seesaw_persistence as persistence;
```

---

### 2. `seesaw-job` (Job Queue Abstractions) - ~16KB

**Purpose**: Durable background/scheduled command execution

**Contents**:
- `JobStore` trait
- `ClaimedJob` struct
- `CommandRegistry` for deserialization
- `FailureKind` enum

**Dependencies**:
```toml
[dependencies]
anyhow = "1.0"
async-trait = "0.1"
chrono = "0.4"
dashmap = "6.1"
serde = "1.0"
serde_json = "1.0"
uuid = "1.0"
```

**README snippet**:
```markdown
# seesaw-job

Job queue abstractions for Seesaw framework.

Provides traits for implementing durable background and scheduled command execution.

## Implementations

- PostgreSQL: `seesaw-job-postgres` (separate crate)
- Redis: `seesaw-job-redis` (separate crate)
- In-memory: Built-in for testing

## Usage

Implement `JobStore` for your storage backend, then integrate with `Dispatcher`.
```

---

### 3. `seesaw-outbox` (Transactional Outbox) - ~14KB

**Purpose**: At-least-once event delivery via transactional outbox

**Contents**:
- `OutboxEvent` trait
- `OutboxWriter` trait
- `OutboxPayload` wrapper
- Correlation ID handling

**Dependencies**:
```toml
[dependencies]
anyhow = "1.0"
async-trait = "0.1"
dashmap = "6.1"
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
uuid = "1.0"
```

**README snippet**:
```markdown
# seesaw-outbox

Transactional outbox pattern for Seesaw framework.

Enables durable event persistence in the same transaction as business data.

## Implementations

- PostgreSQL: `seesaw-outbox-postgres` (separate crate)
- MySQL: `seesaw-outbox-mysql` (separate crate)

## Usage

1. Mark events with `OutboxEvent` trait
2. Implement `OutboxWriter` for your database
3. Write events in same transaction as business logic
```

---

### 4. `seesaw-persistence` (Machine State Persistence) - ~21KB

**Purpose**: Crash recovery for machines with persistent state

**Contents**:
- `PersistentMachine<M>` wrapper
- `MachineStore<M, K>` trait
- `Revision` optimistic locking
- `Router` for machine ID routing
- `InMemoryStore` for testing

**Dependencies**:
```toml
[dependencies]
anyhow = "1.0"
async-trait = "0.1"
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
thiserror = "2.0"
tokio = { version = "1.0", features = ["sync"] }
```

**README snippet**:
```markdown
# seesaw-persistence

Machine state persistence for Seesaw framework.

Wrap machines with `PersistentMachine` to enable automatic state snapshots and crash recovery.

## Implementations

- PostgreSQL: `seesaw-persistence-postgres` (separate crate)
- Redis: `seesaw-persistence-redis` (separate crate)
- In-memory: Built-in for testing

## Usage

1. Implement `MachineStore` for your storage backend
2. Wrap machine with `PersistentMachine::new(machine, store, key)`
3. Machine state automatically persists after each decision
```

---

### 5. `seesaw-testing` (Testing Utilities) - ~51KB

**Purpose**: Ergonomic testing helpers for state machines and workflows

**Contents**:
- `WorkflowTest` fluent builder
- `assert_workflow!` macro
- `EventLatch` for fan-out testing
- `SpyJobQueue` for job assertions
- `MockJobStore` for job lifecycle tests
- `PredicateBuilder` for conditionals

**Dependencies**:
```toml
[dependencies]
anyhow = "1.0"
async-trait = "0.1"
serde = "1.0"
serde_json = "1.0"
tokio = { version = "1.0", features = ["sync", "time"] }
uuid = "1.0"
```

**README snippet**:
```markdown
# seesaw-testing

Testing utilities for Seesaw framework.

Provides macros, builders, and helpers for testing state machines, workflows, and event-driven systems.

## Features

- `assert_workflow!` macro for transition testing
- Fluent `WorkflowTest` builder
- `EventLatch` for fan-out coordination
- `SpyJobQueue` for background job assertions
- `MockJobStore` for job lifecycle tests

## Usage

Add to dev-dependencies:

```toml
[dev-dependencies]
seesaw-testing = "0.1"
```

See main README for examples.
```

---

## Migration Strategy

### Phase 1: Create Workspace Structure
1. Create root `Cargo.toml` workspace manifest
2. Create `crates/` directory
3. Keep current `src/` as `crates/seesaw/src/`

### Phase 2: Extract Job Queue
1. Create `crates/seesaw-job/`
2. Move `job.rs` → `crates/seesaw-job/src/lib.rs`
3. Add `seesaw-job` to workspace
4. Update `seesaw` to depend on `seesaw-job` (optional feature)

### Phase 3: Extract Outbox
1. Create `crates/seesaw-outbox/`
2. Move `outbox.rs` → `crates/seesaw-outbox/src/lib.rs`
3. Add to workspace
4. Update `seesaw` dependency (optional feature)

### Phase 4: Extract Persistence
1. Create `crates/seesaw-persistence/`
2. Move `persistence.rs` → `crates/seesaw-persistence/src/lib.rs`
3. Add to workspace
4. Update `seesaw` dependency (optional feature)

### Phase 5: Extract Testing
1. Create `crates/seesaw-testing/`
2. Move `testing.rs` → `crates/seesaw-testing/src/lib.rs`
3. Add to workspace
4. Update `seesaw` dev-dependency

### Phase 6: Update Public API
1. Update `seesaw/src/lib.rs` to re-export feature-gated modules
2. Update `README.md` with workspace structure
3. Update `CLAUDE.md` with architectural boundaries
4. Add `README.md` to each sub-crate

### Phase 7: Verification
1. `cargo build --workspace`
2. `cargo test --workspace`
3. `cargo build --all-features`
4. Verify examples still work

---

## Benefits

### Modularity
- Users only pull in what they need
- Smaller compile times for minimal setups
- Clear separation of concerns

### Flexibility
- Swap implementations (e.g., Postgres vs Redis for jobs)
- Mix and match features
- Easier to maintain

### Discoverability
- Each crate has focused README
- Clearer entry points
- Better documentation structure

### Publishing
- Can version sub-crates independently
- Users can depend on stable core + experimental features
- Easier to deprecate/evolve APIs

---

## User Impact

### Before (monolithic)
```toml
[dependencies]
seesaw = "0.1"
```

### After (modular)
```toml
[dependencies]
# Core only
seesaw = "0.1"

# Or with features
seesaw = { version = "0.1", features = ["job", "outbox", "persistence"] }

# Or full bundle
seesaw = { version = "0.1", features = ["full"] }

# Or granular
seesaw = "0.1"
seesaw-job = "0.1"
seesaw-outbox = "0.1"

[dev-dependencies]
seesaw-testing = "0.1"
```

**Code changes**: None required - re-exports maintain compatibility

---

## Timeline

- **Phase 1-2**: 30 minutes (workspace setup + job extraction)
- **Phase 3-4**: 20 minutes (outbox + persistence extraction)
- **Phase 5**: 15 minutes (testing extraction)
- **Phase 6**: 30 minutes (docs + verification)
- **Total**: ~2 hours

---

## Open Questions

1. **Naming**: Keep `seesaw-*` prefix or use `seesaw_*`?
   - Recommendation: `seesaw-*` (more conventional)

2. **Version alignment**: All crates start at 0.1.0?
   - Recommendation: Yes, simplifies initial release

3. **Feature defaults**: Should core include any features by default?
   - Recommendation: No, keep default minimal

4. **Publishing**: Publish all together or individually?
   - Recommendation: Together initially, independent versioning later
