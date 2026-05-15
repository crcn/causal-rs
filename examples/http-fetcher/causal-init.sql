-- Minimal causal schema for the http-fetcher hybrid example.
-- Only the two tables PgReactorOutbox touches: the rest of the causal
-- schema (causal_log, snapshots, etc.) lives in KurrentDB.

CREATE TABLE causal_checkpoints (
    consumer_id  TEXT PRIMARY KEY,
    position     BIGINT NOT NULL,
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

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

CREATE INDEX idx_causal_outbox_pending ON causal_outbox (created_at, id);
