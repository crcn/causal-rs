-- Add batch metadata and durable same-batch join tables.
-- Safe to run multiple times.

ALTER TABLE causal_events
    ADD COLUMN IF NOT EXISTS batch_id UUID,
    ADD COLUMN IF NOT EXISTS batch_index INT,
    ADD COLUMN IF NOT EXISTS batch_size INT;

ALTER TABLE causal_effect_executions
    ADD COLUMN IF NOT EXISTS batch_id UUID,
    ADD COLUMN IF NOT EXISTS batch_index INT,
    ADD COLUMN IF NOT EXISTS batch_size INT;

CREATE TABLE IF NOT EXISTS causal_join_entries (
    join_effect_id VARCHAR(255) NOT NULL,
    correlation_id UUID NOT NULL,
    source_event_id UUID NOT NULL,
    source_event_type VARCHAR(255) NOT NULL,
    source_payload JSONB NOT NULL,
    source_created_at TIMESTAMPTZ NOT NULL,
    batch_id UUID NOT NULL,
    batch_index INT NOT NULL,
    batch_size INT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (join_effect_id, correlation_id, source_event_id),
    UNIQUE (join_effect_id, correlation_id, batch_id, batch_index)
);

CREATE INDEX IF NOT EXISTS idx_join_entries_window
ON causal_join_entries(join_effect_id, correlation_id, batch_id, batch_index);

CREATE TABLE IF NOT EXISTS causal_join_windows (
    join_effect_id VARCHAR(255) NOT NULL,
    correlation_id UUID NOT NULL,
    mode VARCHAR(50) NOT NULL DEFAULT 'same_batch',
    batch_id UUID NOT NULL,
    target_count INT NOT NULL,
    status VARCHAR(50) NOT NULL DEFAULT 'open',
    sealed_at TIMESTAMPTZ,
    processing_started_at TIMESTAMPTZ,
    completed_at TIMESTAMPTZ,
    last_error TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (join_effect_id, correlation_id, batch_id)
);

CREATE INDEX IF NOT EXISTS idx_join_windows_status
ON causal_join_windows(status, updated_at DESC);
