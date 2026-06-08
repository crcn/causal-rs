-- Best-effort reactor observability for the inspector (PgReactorObserver →
-- PgInspectorReadModel). KurrentDB remains the durable source of truth; these
-- tables are a lossy, fleet-shared read model — losing rows just means the
-- inspector is briefly behind. Written off the hot path, batched, ON CONFLICT
-- DO NOTHING. Kept in sync with docs/schema.sql.

-- One row per reactor attempt. (event_id, reactor_id, attempt) is best-effort:
-- `attempt` is an in-process counter that resets on restart, so a rare
-- cross-restart collision overwrites — acceptable for a non-bulletproof store.
-- status: running | completed | failed | dead_letter
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

-- Log lines pushed via ctx.log(...) during an attempt.
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

-- A reactor's declared intent (describe()) for a trigger event.
CREATE TABLE causal_reactor_descriptions (
    event_id        UUID NOT NULL,
    reactor_id      TEXT NOT NULL,
    correlation_id  UUID NOT NULL,
    description     JSONB NOT NULL,
    PRIMARY KEY (event_id, reactor_id)
);
CREATE INDEX idx_reactor_desc_corr ON causal_reactor_descriptions (correlation_id);

-- Aggregate state after an event was folded. PK (event_id, aggregate_key)
-- collapses the (consumers+1)×aggregators duplicate folds of one event into a
-- single row (identical post-state) via ON CONFLICT DO NOTHING.
CREATE TABLE causal_aggregate_snapshots (
    event_id        UUID NOT NULL,
    aggregate_key   TEXT NOT NULL,
    correlation_id  UUID NOT NULL,
    state           JSONB NOT NULL,
    PRIMARY KEY (event_id, aggregate_key)
);
CREATE INDEX idx_agg_snap_corr ON causal_aggregate_snapshots (correlation_id);
CREATE INDEX idx_agg_snap_key  ON causal_aggregate_snapshots (aggregate_key);

-- The read model derives display `seq` by joining these tables to causal_log on
-- event_id (causal_log.position is the single seq authority).
