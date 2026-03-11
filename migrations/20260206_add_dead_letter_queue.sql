-- Migration: Add Dead Letter Queue
-- Date: 2026-02-06
-- Purpose: Track handlers that fail permanently after max retries

-- Status enum for DLQ lifecycle
CREATE TYPE causal_dlq_status AS ENUM ('open', 'retrying', 'replayed', 'resolved');

-- Dead letter queue table
CREATE TABLE causal_dead_letter_queue (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),

    -- References to original event and handler
    event_id UUID NOT NULL REFERENCES causal_events(id) ON DELETE CASCADE,
    handler_id TEXT NOT NULL,
    intent_id UUID NOT NULL UNIQUE,  -- Prevent duplicate retries

    -- Error details
    error_message TEXT NOT NULL,
    error_details JSONB,
    retry_count INTEGER NOT NULL,

    -- Timestamps
    first_failed_at TIMESTAMPTZ NOT NULL,
    last_failed_at TIMESTAMPTZ NOT NULL,

    -- Original event payload (for replay)
    event_payload JSONB NOT NULL,

    -- Lifecycle tracking
    status causal_dlq_status NOT NULL DEFAULT 'open',
    retry_attempts INTEGER NOT NULL DEFAULT 0,
    last_retry_at TIMESTAMPTZ,
    resolved_at TIMESTAMPTZ,
    resolution_note TEXT,

    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Index for finding open DLQ entries by handler
CREATE INDEX idx_dlq_handler_open
    ON causal_dead_letter_queue(handler_id, created_at DESC)
    WHERE status IN ('open', 'retrying');

-- Index for finding all DLQ entries for an event
CREATE INDEX idx_dlq_event
    ON causal_dead_letter_queue(event_id);

-- Index for finding entries by status
CREATE INDEX idx_dlq_status
    ON causal_dead_letter_queue(status, created_at DESC);

-- Add new status to handler_intents to track DLQ'd intents
-- (Assuming causal_handler_intents already exists from prior migrations)
-- ALTER TYPE causal_intent_status ADD VALUE IF NOT EXISTS 'dead_letter';
-- Note: PostgreSQL doesn't support conditional ADD VALUE in migrations,
-- so we'll handle this in code or assume it's added manually if needed.
