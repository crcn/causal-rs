-- Base schema for the causal-rs Postgres backend.
--
-- Creates every table + index the `causal_replay` PG backend uses:
-- `PgEventLogBackend`, `PgReactorCheckpoint`, `PgSnapshotStore`,
-- `ProjectionOps`. Column names already use KurrentDB vocabulary
-- (`causation_id`, `revision`), so on a fresh DB the later
-- `20260514_kurrent_alignment.sql` rename migration is a no-op.
--
-- This is the migration-runner equivalent of `docs/schema.sql` (kept in
-- sync with it). A fresh deployment can apply `migrations/` in order OR
-- apply `docs/schema.sql` directly — both yield the same schema.

-- ── causal_log: append-only event log ───────────────────────────────
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
-- (aggregate_type, aggregate_id, revision). Partial — non-aggregate
-- events (NULLs) don't participate. The backend's OCC path keys on this
-- index name (`idx_causal_log_stream`); don't rename it without updating
-- `PgEventLogBackend::append_to_stream`.
CREATE UNIQUE INDEX idx_causal_log_stream
    ON causal_log (aggregate_type, aggregate_id, revision)
    WHERE aggregate_type IS NOT NULL;

CREATE INDEX idx_causal_log_correlation ON causal_log (correlation_id);
CREATE INDEX idx_causal_log_event_type  ON causal_log (event_type);

-- ── causal_checkpoints: per-consumer cursor storage ─────────────────
CREATE TABLE causal_checkpoints (
    consumer_id   TEXT PRIMARY KEY,
    position      BIGINT NOT NULL,
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- ── causal_snapshots: aggregate state snapshots ─────────────────────
CREATE TABLE causal_snapshots (
    key         TEXT NOT NULL,
    revision    BIGINT NOT NULL,
    blob        BYTEA NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (key, revision)
);

CREATE INDEX idx_causal_snapshots_latest
    ON causal_snapshots (key, revision DESC);

-- ── causal_projection_cursors / _failures: async-projection ops ─────
CREATE TABLE causal_projection_cursors (
    projection_id        TEXT PRIMARY KEY,
    cursor_position      BIGINT NOT NULL,
    paused               BOOL NOT NULL DEFAULT FALSE,
    last_error           TEXT,
    last_attempt_at      TIMESTAMPTZ,
    consecutive_failures INT NOT NULL DEFAULT 0,
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
