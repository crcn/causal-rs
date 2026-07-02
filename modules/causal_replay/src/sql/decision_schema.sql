-- Decision records schema. Kept in sync with
-- migrations/20260702_causal_decisions.sql and docs/schema.sql.
-- Applied via PgDecisionStore::ensure_schema or through a migration pipeline.
CREATE TABLE IF NOT EXISTS causal_decisions (
    consumer          TEXT        NOT NULL,
    trigger_event_id  UUID        NOT NULL,
    outputs           JSONB       NOT NULL,
    sealed_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (consumer, trigger_event_id)
);

CREATE INDEX IF NOT EXISTS causal_decisions_sealed_at_idx
    ON causal_decisions (sealed_at);
