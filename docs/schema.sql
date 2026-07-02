-- ============================================================
-- causal-rs v0.7 — Postgres backend schema
-- ============================================================
--
-- This is the authoritative schema for the PG-backed
-- `EventLogBackend` / `CheckpointStore` / `ReactorCheckpoint` /
-- `SnapshotStore` implementations in the `causal_replay` crate, plus
-- the 0.7.0 reactor-observability tables read by the inspector
-- (`PgReactorObserver` / `PgInspectorReadModel`).
--
-- KurrentDB vocabulary aligned: column names use Kurrent's terms
-- (`causation_id`, `revision`) so the data layout matches what a
-- developer migrating to/from Kurrent expects. See
-- `docs/plans/2026-05-14-kurrent-alignment-design.md`.
--
-- Run order on a fresh DB:
--   1. This schema (creates all tables + indexes).
--   2. Application-specific seed.
--
-- Run order on a v0.3 → v0.4 migration:
--   1. `migrations/20260514_kurrent_alignment.sql` (column renames).
--   2. Application value-shift script if upgrading from 1-indexed
--      versions (see migration file header).
--
-- ============================================================

-- ── causal_log: append-only event log ───────────────────────────────
--
-- Backs `PgEventLogBackend`. Every event causal writes lands here.
-- Aggregate-scoped events carry `aggregate_type` + `aggregate_id` +
-- `revision`; non-aggregate events carry NULLs in those columns.
CREATE TABLE causal_log (
    position        BIGSERIAL PRIMARY KEY,
    event_id        UUID NOT NULL UNIQUE,
    causation_id    UUID,
    correlation_id  UUID NOT NULL,
    event_type      VARCHAR(255) NOT NULL,
    payload         JSONB NOT NULL,
    aggregate_type  VARCHAR(255),
    aggregate_id    UUID,
    revision        BIGINT,
    metadata        JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    persistent      BOOLEAN NOT NULL DEFAULT TRUE
);

-- Per-stream optimistic concurrency: no two events at the same
-- (aggregate_type, aggregate_id, revision). The partial WHERE
-- means non-aggregate events (NULLs) don't participate.
CREATE UNIQUE INDEX idx_causal_log_stream
    ON causal_log (aggregate_type, aggregate_id, revision)
    WHERE aggregate_type IS NOT NULL;

CREATE INDEX idx_causal_log_correlation
    ON causal_log (correlation_id);

CREATE INDEX idx_causal_log_event_type
    ON causal_log (event_type);

COMMENT ON COLUMN causal_log.position IS
    'Global commit position (BIGSERIAL, starts at 1). Cursors compare with
     position > cursor and start at 0, so rolled-back-txn gaps are fine.
     Maps to Kurrent commit position.';
COMMENT ON COLUMN causal_log.revision IS
    '0-indexed per-stream revision. First event in a stream has revision 0. NULL for non-aggregate events.';
COMMENT ON COLUMN causal_log.causation_id IS
    'event_id of the event that caused this one. KurrentDB convention.';
COMMENT ON COLUMN causal_log.correlation_id IS
    'Correlation chain id linking related events. KurrentDB convention.';

-- ── causal_checkpoints: per-consumer cursor storage ─────────────────
--
-- Backs `PgReactorCheckpoint`'s `CheckpointStore` impl. One row per
-- (reactor or projector) consumer; tracks how far it's read in $all.
CREATE TABLE causal_checkpoints (
    consumer_id   TEXT PRIMARY KEY,
    position      BIGINT NOT NULL,
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- (No causal_outbox table: reactors append their outputs directly to
-- causal_log with a deterministic event_id and advance their cursor —
-- at-least-once + idempotent, no transactional outbox / relay.)

-- ── causal_snapshots: aggregate state snapshots ─────────────────────
--
-- Backs `PgSnapshotStore`. Snapshots are keyed by encoded
-- "{aggregate_type}:{aggregate_id}". `revision` is the 0-indexed
-- stream revision at the time the snapshot was taken.
CREATE TABLE causal_snapshots (
    key         TEXT NOT NULL,
    revision    BIGINT NOT NULL,
    blob        BYTEA NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (key, revision)
);

CREATE INDEX idx_causal_snapshots_latest
    ON causal_snapshots (key, revision DESC);

-- ── causal_projection_cursors: async-projection ops ─────────────────
--
-- Per-projection cursor + pause/resume + DLQ bookkeeping for async
-- (independent-runner) projections. The operational surface that drove
-- these (`ProjectionOps`) was removed pre-1.0 (2026-06-10 audit
-- remediation); the tables are kept reserved for a future, wired
-- re-introduction.
CREATE TABLE causal_projection_cursors (
    projection_id        TEXT PRIMARY KEY,
    cursor_position      BIGINT NOT NULL,
    paused               BOOL NOT NULL DEFAULT FALSE,
    last_error           TEXT,
    last_attempt_at      TIMESTAMPTZ,
    consecutive_failures INT NOT NULL DEFAULT 0,
    -- Forward-compat columns for future multi-process leases (D3 in
    -- async-projections plan). Currently unused; reserved so adding
    -- leases later is a column-population change, not a schema
    -- migration.
    leased_by            TEXT,
    leased_until         TIMESTAMPTZ,
    fencing_token        BIGINT
);

CREATE TABLE causal_projection_failures (
    projection_id  TEXT NOT NULL,
    event_id       UUID NOT NULL,
    error          TEXT NOT NULL,
    attempts       INT NOT NULL,
    failed_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (projection_id, event_id)
);

COMMENT ON TABLE causal_projection_failures IS
    'DLQ for async projections in AdvanceAfter mode. PK is load-bearing — makes advance_past_failure idempotent on retry.';

-- ── Reactor observability (best-effort inspector read model) ─────────
--
-- Written by `PgReactorObserver` (off the hot path, batched, lossy) and read by
-- `PgInspectorReadModel`. KurrentDB stays the durable source of truth; these
-- tables are a fleet-shared, queryable read model — losing rows just means the
-- inspector is briefly behind. The read model derives display `seq` by joining
-- these to causal_log on event_id (causal_log.position is the seq authority).

-- One row per reactor attempt. status: running | completed | failed | dead_letter
CREATE TABLE causal_reactor_executions (
    event_id        UUID NOT NULL,
    reactor_id      TEXT NOT NULL,
    attempt         INT NOT NULL,
    correlation_id  UUID NOT NULL,
    status          TEXT NOT NULL,
    error           TEXT,
    started_at      TIMESTAMPTZ NOT NULL,
    completed_at    TIMESTAMPTZ,
    PRIMARY KEY (event_id, reactor_id, attempt)
);
CREATE INDEX idx_reactor_exec_event ON causal_reactor_executions (event_id, reactor_id);
CREATE INDEX idx_reactor_exec_corr  ON causal_reactor_executions (correlation_id);
CREATE INDEX idx_reactor_exec_failed ON causal_reactor_executions (status)
    WHERE status IN ('failed', 'dead_letter');

-- Accepted divergent redeliveries. A nondeterministic reactor re-derives the
-- same identity-keyed event_id with a different payload on redelivery; the store
-- keeps the persisted row and the runner accepts it (advances, never parks).
-- This is NOT a failure, so it is recorded apart from the lifecycle statuses
-- above and surfaced as a `diverged` flag — never an error/dead_letter.
CREATE TABLE causal_reactor_divergences (
    event_id        UUID NOT NULL,
    reactor_id      TEXT NOT NULL,
    correlation_id  UUID NOT NULL,
    diff            TEXT NOT NULL,
    at              TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (event_id, reactor_id)
);
CREATE INDEX idx_reactor_diverg_corr ON causal_reactor_divergences (correlation_id);

CREATE TABLE causal_reactor_logs (
    event_id        UUID NOT NULL,
    reactor_id      TEXT NOT NULL,
    attempt         INT NOT NULL,
    ord             INT NOT NULL,
    correlation_id  UUID NOT NULL,
    level           TEXT NOT NULL,
    message         TEXT NOT NULL,
    data            JSONB,
    logged_at       TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (event_id, reactor_id, attempt, ord)
);
CREATE INDEX idx_reactor_logs_event ON causal_reactor_logs (event_id, reactor_id);
CREATE INDEX idx_reactor_logs_corr  ON causal_reactor_logs (correlation_id);

CREATE TABLE causal_reactor_descriptions (
    event_id        UUID NOT NULL,
    reactor_id      TEXT NOT NULL,
    correlation_id  UUID NOT NULL,
    description     JSONB NOT NULL,
    PRIMARY KEY (event_id, reactor_id)
);
CREATE INDEX idx_reactor_desc_corr ON causal_reactor_descriptions (correlation_id);

CREATE TABLE causal_aggregate_snapshots (
    event_id        UUID NOT NULL,
    aggregate_key   TEXT NOT NULL,
    correlation_id  UUID NOT NULL,
    state           JSONB NOT NULL,
    PRIMARY KEY (event_id, aggregate_key)
);
CREATE INDEX idx_agg_snap_corr ON causal_aggregate_snapshots (correlation_id);
CREATE INDEX idx_agg_snap_key  ON causal_aggregate_snapshots (aggregate_key);

-- ── causal_effect_store: durable memo of reactor side effects ────────
--
-- Backs `PgEffectStore`. Keyed by (consumer, trigger_event_id, label);
-- first-write-wins via INSERT ... ON CONFLICT DO NOTHING.
-- `trigger_event_id` links to causal_log.event_id — effects are
-- associated with the event that triggered the reactor that produced them.
CREATE TABLE IF NOT EXISTS causal_effect_store (
    consumer         TEXT    NOT NULL,
    trigger_event_id UUID    NOT NULL,
    label            TEXT    NOT NULL,
    value            JSONB   NOT NULL,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (consumer, trigger_event_id, label)
);

-- Partial index for efficient causation-chain traversal in recursive CTEs.
-- Required by the inspector's DESCENDANTS subject-chain mode.
CREATE INDEX IF NOT EXISTS idx_causal_log_causation
    ON causal_log (causation_id)
    WHERE causation_id IS NOT NULL;

-- ── causal_decisions: one durable decision per (consumer, trigger) ───
--
-- Backs `PgDecisionStore`. Stores the full output batch a reaction
-- produced, sealed first-write-wins via INSERT ... ON CONFLICT DO NOTHING.
-- Redelivery replays this row instead of re-running the reactor body,
-- making partial-append completion idempotent and the chimera log
-- impossible. GC is retention-based (age-driven) via the sealed_at index.
CREATE TABLE IF NOT EXISTS causal_decisions (
    consumer          TEXT        NOT NULL,
    trigger_event_id  UUID        NOT NULL,
    trigger_position  BIGINT      NOT NULL,
    outputs           JSONB       NOT NULL,
    sealed_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (consumer, trigger_event_id)
);
CREATE INDEX IF NOT EXISTS causal_decisions_sealed_at_idx
    ON causal_decisions (sealed_at);

-- ── causal_event_ids: global event_id registry (A2) ─────────────────
--
-- Backs PgEventIdRegistry. Authoritative, unbounded index of appended
-- event_ids so the Kurrent backend's `Any` idempotent append recognizes a
-- redelivery no matter how deep the original output is buried (past its
-- tail-window scan) — required for the decision-record completion path to be
-- safe on Kurrent. Postgres already enforces this for its own log via the
-- causal_log PK; this lifts the same guarantee in front of Kurrent.
CREATE TABLE IF NOT EXISTS causal_event_ids (
    event_id         UUID   NOT NULL PRIMARY KEY,
    stream_position  BIGINT NOT NULL,
    stream_revision  BIGINT NOT NULL,
    registered_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);
