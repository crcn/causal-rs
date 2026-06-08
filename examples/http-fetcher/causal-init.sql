-- Minimal causal schema for the http-fetcher hybrid example.
-- The only table PgReactorCheckpoint touches: the rest of the causal
-- schema (causal_log, snapshots, etc.) lives in KurrentDB. There is no
-- outbox table — reactors append their outputs directly to the log.

CREATE TABLE causal_checkpoints (
    consumer_id  TEXT PRIMARY KEY,
    position     BIGINT NOT NULL,
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
