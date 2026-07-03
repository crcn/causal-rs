-- Decision records schema. Kept in sync with
-- migrations/20260702_causal_decisions.sql, migrations/20260703_causal_decisions_parked.sql,
-- and docs/schema.sql.
-- Applied via PgDecisionStore::ensure_schema or through a migration pipeline.
CREATE TABLE IF NOT EXISTS causal_decisions (
    consumer          TEXT        NOT NULL,
    trigger_event_id  UUID        NOT NULL,
    trigger_position  BIGINT      NOT NULL,
    outputs           JSONB       NOT NULL,
    -- TRUE for a terminal PARK decision (the reaction failed to the DLQ),
    -- FALSE for a success. See DecisionRecord::parked.
    parked            BOOLEAN     NOT NULL DEFAULT FALSE,
    sealed_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (consumer, trigger_event_id)
);

-- Retroactive column adds for tables created by an earlier ensure_schema.
-- CREATE TABLE IF NOT EXISTS is a no-op on an existing table, so a bare
-- column addition above never reaches it — these ALTERs do. Idempotent.
ALTER TABLE causal_decisions
    ADD COLUMN IF NOT EXISTS trigger_position BIGINT NOT NULL DEFAULT 0;
ALTER TABLE causal_decisions
    ADD COLUMN IF NOT EXISTS parked BOOLEAN NOT NULL DEFAULT FALSE;

CREATE INDEX IF NOT EXISTS causal_decisions_sealed_at_idx
    ON causal_decisions (sealed_at);
