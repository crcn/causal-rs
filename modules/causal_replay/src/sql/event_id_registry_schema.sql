-- Global event_id registry (A2). Authoritative, unbounded index of appended
-- event_ids so the Kurrent backend's `Any` (idempotent) append can recognize
-- a redelivery buried past its tail-window scan. Kept in sync with
-- migrations/20260702_causal_event_ids.sql and docs/schema.sql.
--
-- stream_position / stream_revision are the batch WriteResult coordinates
-- (the last event's), shared by every id in one atomic batch — what an `Any`
-- append returns on redelivery. Stored as BIGINT (Postgres has no unsigned).
CREATE TABLE IF NOT EXISTS causal_event_ids (
    event_id         UUID   NOT NULL PRIMARY KEY,
    stream_position  BIGINT NOT NULL,
    stream_revision  BIGINT NOT NULL,
    registered_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);
