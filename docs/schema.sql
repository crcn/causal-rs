-- ============================================================
-- causal-rs v0.4 — Postgres backend schema
-- ============================================================
--
-- This is the authoritative schema for the v0.4 PG-backed
-- `EventLogBackend` / `CheckpointStore` / `ReactorOutbox` /
-- `SnapshotStore` / `ProjectionOps` implementations in the
-- `causal_replay` crate.
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
    '0-indexed global commit position. Maps to Kurrent commit position.';
COMMENT ON COLUMN causal_log.revision IS
    '0-indexed per-stream revision. First event in a stream has revision 0. NULL for non-aggregate events.';
COMMENT ON COLUMN causal_log.causation_id IS
    'event_id of the event that caused this one. KurrentDB convention.';
COMMENT ON COLUMN causal_log.correlation_id IS
    'Correlation chain id linking related events. KurrentDB convention.';

-- ── causal_checkpoints: per-consumer cursor storage ─────────────────
--
-- Backs `PgReactorOutbox`'s `CheckpointStore` impl. One row per
-- (reactor or projector) consumer; tracks how far it's read in $all.
CREATE TABLE causal_checkpoints (
    consumer_id   TEXT PRIMARY KEY,
    position      BIGINT NOT NULL,
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- ── causal_outbox: reactor output buffer ────────────────────────────
--
-- Backs `PgReactorOutbox::commit_reactor_batch`. Reactor outputs land
-- here in the same transaction as the trigger event's cursor advance
-- (C12: atomic outbox + cursor commit). The relay drains pending rows
-- and appends them to causal_log.
CREATE TABLE causal_outbox (
    id              BIGSERIAL PRIMARY KEY,
    reactor_id      TEXT NOT NULL,
    source_event_id UUID NOT NULL,
    output_index    INTEGER NOT NULL,
    event_id        UUID NOT NULL,
    event_type      VARCHAR(255) NOT NULL,
    fact_payload    JSONB NOT NULL,
    correlation_id  UUID NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (reactor_id, source_event_id, output_index)
);

CREATE INDEX idx_causal_outbox_pending
    ON causal_outbox (created_at, id);

COMMENT ON COLUMN causal_outbox.output_index IS
    '0..N for normal react() outputs; u32::MAX for DLQ-synthesized facts.';

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
-- Backs `ProjectionOps`. Per-projection cursor + pause/resume + DLQ
-- bookkeeping for async (independent-runner) projections.
CREATE TABLE causal_projection_cursors (
    projection_id        TEXT PRIMARY KEY,
    cursor_position      BIGINT NOT NULL,
    paused               BOOL NOT NULL DEFAULT FALSE,
    last_error           TEXT,
    last_attempt_at      TIMESTAMPTZ,
    consecutive_failures INT NOT NULL DEFAULT 0,
    -- Forward-compat columns for future multi-process leases (D3 in
    -- async-projections plan). Unused in v0.4; reserved so adding
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
