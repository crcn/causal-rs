-- Inspector read-model schema owned by `causal_replay` — the five tables
-- `PgReactorObserver` writes and `PgInspectorReadModel` reads. Exported as
-- `causal_replay::INSPECTOR_SCHEMA_SQL` and applied by
-- `PgReactorObserver::ensure_schema`.
--
-- Every statement is `IF NOT EXISTS` so it is idempotent and safe to run on
-- every boot. Consumers with a migration pipeline should apply this SQL
-- through that pipeline instead of calling `ensure_schema` at runtime.
--
-- KEEP IN SYNC (identical columns/indexes) with:
--   migrations/20260608_reactor_observability.sql  (4 tables below)
--   migrations/20260628_reactor_divergences.sql     (divergences)
--   docs/schema.sql                                  (authoritative single file)
-- The only intended difference is the `IF NOT EXISTS` clauses here (the
-- migration files use bare CREATE, since a migration runs exactly once).

-- One row per reactor attempt. status: running | completed | failed | dead_letter
CREATE TABLE IF NOT EXISTS causal_reactor_executions (
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
CREATE INDEX IF NOT EXISTS idx_reactor_exec_event ON causal_reactor_executions (event_id, reactor_id);
CREATE INDEX IF NOT EXISTS idx_reactor_exec_corr  ON causal_reactor_executions (correlation_id);
CREATE INDEX IF NOT EXISTS idx_reactor_exec_failed ON causal_reactor_executions (status)
    WHERE status IN ('failed', 'dead_letter');

-- Log lines pushed via ctx.log(...) during an attempt.
CREATE TABLE IF NOT EXISTS causal_reactor_logs (
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
CREATE INDEX IF NOT EXISTS idx_reactor_logs_event ON causal_reactor_logs (event_id, reactor_id);
CREATE INDEX IF NOT EXISTS idx_reactor_logs_corr  ON causal_reactor_logs (correlation_id);

-- A reactor's declared intent (describe()) for a trigger event.
CREATE TABLE IF NOT EXISTS causal_reactor_descriptions (
    event_id        UUID NOT NULL,
    reactor_id      TEXT NOT NULL,
    correlation_id  UUID NOT NULL,
    description     JSONB NOT NULL,
    PRIMARY KEY (event_id, reactor_id)
);
CREATE INDEX IF NOT EXISTS idx_reactor_desc_corr ON causal_reactor_descriptions (correlation_id);

-- Aggregate state after an event was folded.
CREATE TABLE IF NOT EXISTS causal_aggregate_snapshots (
    event_id        UUID NOT NULL,
    aggregate_key   TEXT NOT NULL,
    correlation_id  UUID NOT NULL,
    state           JSONB NOT NULL,
    PRIMARY KEY (event_id, aggregate_key)
);
CREATE INDEX IF NOT EXISTS idx_agg_snap_corr ON causal_aggregate_snapshots (correlation_id);
CREATE INDEX IF NOT EXISTS idx_agg_snap_key  ON causal_aggregate_snapshots (aggregate_key);

-- Divergent-redelivery observability: a reactor whose react() output is
-- nondeterministic re-derives the same identity-keyed event_id with a
-- DIFFERENT payload on redelivery. Recorded here (not as a failure status),
-- surfaced by the read model as a `diverged` flag.
CREATE TABLE IF NOT EXISTS causal_reactor_divergences (
    event_id        UUID NOT NULL,
    reactor_id      TEXT NOT NULL,
    correlation_id  UUID NOT NULL,
    diff            TEXT NOT NULL,
    at              TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (event_id, reactor_id)
);
CREATE INDEX IF NOT EXISTS idx_reactor_diverg_corr ON causal_reactor_divergences (correlation_id);
