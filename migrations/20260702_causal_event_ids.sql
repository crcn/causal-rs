-- Global event_id registry (A2): authoritative, unbounded index of appended
-- event_ids so the Kurrent backend's `Any` idempotent append recognizes a
-- redelivery no matter how deep the original output is buried (past the
-- tail-window scan). Backs PgEventIdRegistry / EVENT_ID_REGISTRY_SCHEMA_SQL.
-- Keep in sync with docs/schema.sql.
CREATE TABLE IF NOT EXISTS causal_event_ids (
    event_id         UUID   NOT NULL PRIMARY KEY,
    stream_position  BIGINT NOT NULL,
    stream_revision  BIGINT NOT NULL,
    registered_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);
