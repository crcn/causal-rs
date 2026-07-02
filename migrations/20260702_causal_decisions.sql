-- Decision records: one durable decision per (consumer, trigger).
-- The full output batch a reaction produced, sealed first-write-wins via
-- INSERT ... ON CONFLICT DO NOTHING. Redelivery replays this row instead
-- of re-running the reactor body, which makes partial-append completion
-- idempotent and the chimera-log failure impossible.
--
-- Keep in sync with causal_replay's DECISION_SCHEMA_SQL (sql/decision_schema.sql)
-- and docs/schema.sql.
CREATE TABLE IF NOT EXISTS causal_decisions (
    consumer          TEXT        NOT NULL,
    trigger_event_id  UUID        NOT NULL,
    outputs           JSONB       NOT NULL,
    sealed_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (consumer, trigger_event_id)
);

-- Retention-based GC (age-driven) scans by sealed_at.
CREATE INDEX IF NOT EXISTS causal_decisions_sealed_at_idx
    ON causal_decisions (sealed_at);
