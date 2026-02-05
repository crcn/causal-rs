-- Seesaw Queue-Backed Architecture - Production Schema
-- Version: 0.8.0
-- Last Updated: 2026-02-05
--
-- This schema is designed for millions of users at 1000+ events/sec
-- Features: Per-saga FIFO, idempotency, two-phase execution, table partitioning

-- ============================================================
-- Core Tables
-- ============================================================

-- Events Queue (Partitioned by date for vacuum efficiency)
-- Stores all events to be processed by event workers
CREATE TABLE seesaw_events (
    id BIGSERIAL,
    event_id UUID NOT NULL,
    parent_id UUID,
    correlation_id UUID NOT NULL,           -- Workflow identifier (envelope metadata)
    event_type VARCHAR(255) NOT NULL,
    payload JSONB NOT NULL,
    hops INT NOT NULL DEFAULT 0,     -- Infinite loop protection (DLQ after 50)
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    processed_at TIMESTAMPTZ,        -- NULL = pending, set when complete
    locked_until TIMESTAMPTZ,        -- For retry backoff
    retry_count INT NOT NULL DEFAULT 0,
    PRIMARY KEY (id, created_at)     -- Include created_at for partitioning
) PARTITION BY RANGE (created_at);

-- Idempotency: Prevent duplicate event_ids (webhooks, crash+retry)
-- Must include partitioning column (created_at) for partitioned table
-- For deterministic events, framework uses fixed timestamp to ensure idempotency
CREATE UNIQUE INDEX idx_events_event_id ON seesaw_events(event_id, created_at);

-- Per-saga FIFO with advisory locks (workers use this to poll)
CREATE INDEX idx_events_pending ON seesaw_events(created_at ASC)
WHERE processed_at IS NULL;

-- Lookup events by saga (for introspection/debugging)
CREATE INDEX idx_events_saga ON seesaw_events(correlation_id, created_at);

-- Cleanup: Find old events for archival
CREATE INDEX idx_events_cleanup ON seesaw_events(processed_at)
WHERE processed_at IS NOT NULL;

COMMENT ON TABLE seesaw_events IS 'Event queue - workers poll with SKIP LOCKED for parallel processing';
COMMENT ON COLUMN seesaw_events.hops IS 'Incremented on each event emission - DLQ after 50 to prevent infinite loops';
COMMENT ON COLUMN seesaw_events.correlation_id IS 'Envelope metadata - groups related events into a workflow';

-- LISTEN/NOTIFY trigger for .wait() pattern (CQRS support)
-- Enables engine.process(event).wait::<TerminalEvent>().await
CREATE OR REPLACE FUNCTION notify_saga_event()
RETURNS TRIGGER AS $$
BEGIN
    PERFORM pg_notify(
        'seesaw_saga_' || NEW.correlation_id::text,
        json_build_object(
            'event_id', NEW.event_id,
            'correlation_id', NEW.correlation_id,
            'event_type', NEW.event_type,
            'payload', NEW.payload
        )::text
    );
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER seesaw_events_notify
    AFTER INSERT ON seesaw_events
    FOR EACH ROW
    EXECUTE FUNCTION notify_saga_event();

COMMENT ON FUNCTION notify_saga_event IS 'Push notification for wait() pattern - enables CQRS without polling';

-- Create initial partitions (daily partitions - must create before inserts)
-- In production, automate partition creation (cron job or pg_partman)
CREATE TABLE seesaw_events_2026_02_05 PARTITION OF seesaw_events
FOR VALUES FROM ('2026-02-05') TO ('2026-02-06');

CREATE TABLE seesaw_events_2026_02_06 PARTITION OF seesaw_events
FOR VALUES FROM ('2026-02-06') TO ('2026-02-07');

CREATE TABLE seesaw_events_2026_02_07 PARTITION OF seesaw_events
FOR VALUES FROM ('2026-02-07') TO ('2026-02-08');

-- Note: Create partitions 7 days ahead, drop partitions older than 30 days
-- See docs/partitioning.md for automation scripts

-- State per saga (optimistic locking with version column)
CREATE TABLE seesaw_state (
    correlation_id UUID PRIMARY KEY,
    state JSONB NOT NULL,             -- User's domain state (keep under 10KB!)
    version INT NOT NULL DEFAULT 1,   -- Optimistic locking
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_state_updated ON seesaw_state(updated_at DESC);

COMMENT ON TABLE seesaw_state IS 'Per-saga state - loaded/saved by event workers, read by effect workers';
COMMENT ON COLUMN seesaw_state.state IS 'Keep under 10KB - store large blobs in S3, not here!';
COMMENT ON COLUMN seesaw_state.version IS 'Incremented on each update - detects concurrent modifications';

-- Event processing ledger (atomic claim + phase tracking + audit trail)
-- Purpose: Separate from queue for atomic claiming and historical record
CREATE TABLE seesaw_processed (
    event_id UUID PRIMARY KEY,
    correlation_id UUID NOT NULL,
    state_committed_at TIMESTAMPTZ,  -- When Phase 1 (reducers) completed
    completed_at TIMESTAMPTZ,        -- When Phase 2 (all effects) completed
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_processed_saga ON seesaw_processed(correlation_id);
CREATE INDEX idx_processed_completed ON seesaw_processed(completed_at DESC)
WHERE completed_at IS NOT NULL;

COMMENT ON TABLE seesaw_processed IS 'Processing ledger - atomic claim via ON CONFLICT, persists after queue deletion';
COMMENT ON COLUMN seesaw_processed.state_committed_at IS 'Phase 1 complete - state saved, effect intents inserted';
COMMENT ON COLUMN seesaw_processed.completed_at IS 'Phase 2 complete - all effects finished';

-- Effect execution intents (framework-guaranteed idempotency)
-- Effect workers poll this table for ready effects
CREATE TABLE seesaw_effect_executions (
    event_id UUID NOT NULL,
    effect_id VARCHAR(255) NOT NULL,
    correlation_id UUID NOT NULL,
    status VARCHAR(50) NOT NULL DEFAULT 'pending',  -- pending, executing, completed, failed
    result JSONB,
    error TEXT,
    attempts INT NOT NULL DEFAULT 0,

    -- Event payload (survives 30-day retention deletion)
    event_type VARCHAR(255) NOT NULL,
    event_payload JSONB NOT NULL,    -- Copied from seesaw_events for delayed effects
    parent_event_id UUID,

    -- Execution properties (from effect builder)
    execute_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),  -- When to execute (.delayed())
    timeout_seconds INT NOT NULL DEFAULT 30,
    max_attempts INT NOT NULL DEFAULT 3,
    priority INT NOT NULL DEFAULT 10,               -- Lower = higher priority

    claimed_at TIMESTAMPTZ,          -- NULL until claimed by worker
    completed_at TIMESTAMPTZ,
    last_attempted_at TIMESTAMPTZ,

    PRIMARY KEY (event_id, effect_id)
);

-- Worker polling: Find next ready effect (respects priority and schedule)
-- Note: execute_at <= NOW() check is done in query, not index (NOW() is STABLE not IMMUTABLE)
-- Lower priority number = higher priority, so ASC order
CREATE INDEX idx_effect_executions_pending
ON seesaw_effect_executions(priority ASC, execute_at ASC)
WHERE status = 'pending';

-- Lookup effects by event (for debugging)
CREATE INDEX idx_effect_executions_event ON seesaw_effect_executions(event_id);

-- Lookup effects by saga (for progress tracking)
CREATE INDEX idx_effect_executions_saga ON seesaw_effect_executions(correlation_id);

-- Monitor failures
CREATE INDEX idx_effect_executions_failed ON seesaw_effect_executions(status, attempts)
WHERE status = 'failed';

COMMENT ON TABLE seesaw_effect_executions IS 'Effect intents - created by event workers, executed by effect workers';
COMMENT ON COLUMN seesaw_effect_executions.event_payload IS 'Event data copied here - survives parent event deletion after 30 days';
COMMENT ON COLUMN seesaw_effect_executions.priority IS 'Worker polling priority - lower number = higher priority';

-- ============================================================
-- Dead Letter Queue
-- ============================================================

-- Failed effects that exceeded retry limits
CREATE TABLE seesaw_dlq (
    event_id UUID NOT NULL,
    effect_id VARCHAR(255) NOT NULL,
    correlation_id UUID NOT NULL,
    error TEXT NOT NULL,
    event_type VARCHAR(255) NOT NULL,
    event_payload JSONB NOT NULL,
    reason VARCHAR(50) NOT NULL,     -- 'failed', 'timeout', 'infinite_loop'
    attempts INT NOT NULL DEFAULT 0,
    failed_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    resolved_at TIMESTAMPTZ,         -- NULL = unresolved, set when manually fixed
    PRIMARY KEY (event_id, effect_id)
);

-- Ops query: List unresolved failures
CREATE INDEX idx_dlq_unresolved ON seesaw_dlq(failed_at DESC)
WHERE resolved_at IS NULL;

-- Filter by failure reason
CREATE INDEX idx_dlq_reason ON seesaw_dlq(reason, failed_at DESC)
WHERE resolved_at IS NULL;

-- Find DLQ entries by saga
CREATE INDEX idx_dlq_saga ON seesaw_dlq(correlation_id);

COMMENT ON TABLE seesaw_dlq IS 'Dead letter queue - effects that failed permanently';
COMMENT ON COLUMN seesaw_dlq.reason IS 'Why it failed: failed (retry exhausted), timeout, infinite_loop';

-- ============================================================
-- Observability Stream (for real-time visualization)
-- ============================================================

-- Stream of all workflow events and effect transitions
-- Append-only table for real-time monitoring and debugging
CREATE TABLE seesaw_stream (
    seq BIGSERIAL PRIMARY KEY,
    stream_type VARCHAR(50) NOT NULL,  -- 'event_dispatched', 'effect_started', 'effect_completed', 'effect_failed'
    correlation_id UUID NOT NULL,
    event_id UUID,
    effect_event_id UUID,              -- For effects: the triggering event_id
    effect_id VARCHAR(255),             -- For effects: the effect identifier
    status VARCHAR(50),                 -- For effects: 'executing', 'completed', 'failed'
    error TEXT,                         -- For failures: error message
    payload JSONB,                      -- Event payload or effect result
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Fast lookups by sequence (for cursor-based pagination)
CREATE INDEX idx_stream_seq ON seesaw_stream(seq DESC);

-- Filter by correlation_id (for workflow-specific streams)
CREATE INDEX idx_stream_correlation ON seesaw_stream(correlation_id, seq DESC);

-- Time-based cleanup/archival
CREATE INDEX idx_stream_created ON seesaw_stream(created_at DESC);

COMMENT ON TABLE seesaw_stream IS 'Observability stream - append-only log for real-time visualization';
COMMENT ON COLUMN seesaw_stream.seq IS 'Global sequence number for cursor-based pagination';
COMMENT ON COLUMN seesaw_stream.stream_type IS 'Event type: event_dispatched, effect_started, effect_completed, effect_failed';

-- Trigger: Stream event dispatches
CREATE OR REPLACE FUNCTION stream_event_dispatched()
RETURNS TRIGGER AS $$
BEGIN
    INSERT INTO seesaw_stream (
        stream_type,
        correlation_id,
        event_id,
        payload,
        created_at
    ) VALUES (
        'event_dispatched',
        NEW.correlation_id,
        NEW.event_id,
        jsonb_build_object(
            'event_type', NEW.event_type,
            'hops', NEW.hops,
            'payload', NEW.payload
        ),
        NEW.created_at
    );

    -- NOTIFY for SSE wake-up (correlation_id only, clients fetch from stream table)
    PERFORM pg_notify('seesaw_stream', NEW.correlation_id::text);

    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER seesaw_events_stream
    AFTER INSERT ON seesaw_events
    FOR EACH ROW
    EXECUTE FUNCTION stream_event_dispatched();

-- Trigger: Stream effect execution lifecycle
CREATE OR REPLACE FUNCTION stream_effect_transition()
RETURNS TRIGGER AS $$
DECLARE
    stream_type_val VARCHAR(50);
BEGIN
    -- Determine stream type based on status transition
    IF TG_OP = 'INSERT' THEN
        stream_type_val := 'effect_started';
    ELSIF NEW.status = 'completed' AND OLD.status != 'completed' THEN
        stream_type_val := 'effect_completed';
    ELSIF NEW.status = 'failed' AND OLD.status != 'failed' THEN
        stream_type_val := 'effect_failed';
    ELSE
        -- No stream entry for other transitions (e.g., retry attempts)
        RETURN NEW;
    END IF;

    INSERT INTO seesaw_stream (
        stream_type,
        correlation_id,
        event_id,
        effect_event_id,
        effect_id,
        status,
        error,
        payload,
        created_at
    ) VALUES (
        stream_type_val,
        NEW.correlation_id,
        NULL,  -- No new event for effect lifecycle
        NEW.event_id,
        NEW.effect_id,
        NEW.status,
        NEW.error,
        NEW.result,
        NOW()
    );

    -- NOTIFY for SSE wake-up
    PERFORM pg_notify('seesaw_stream', NEW.correlation_id::text);

    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER seesaw_effect_executions_stream
    AFTER INSERT OR UPDATE ON seesaw_effect_executions
    FOR EACH ROW
    EXECUTE FUNCTION stream_effect_transition();

COMMENT ON FUNCTION stream_event_dispatched IS 'Trigger: Log event dispatches to observability stream';
COMMENT ON FUNCTION stream_effect_transition IS 'Trigger: Log effect lifecycle transitions to observability stream';

-- ============================================================
-- Operational Tables
-- ============================================================

-- Reaper heartbeat (critical for production monitoring)
-- Single-row table to track background cleanup process
CREATE TABLE seesaw_reaper_heartbeat (
    id INT PRIMARY KEY DEFAULT 1,
    last_run TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    events_reaped INT NOT NULL DEFAULT 0,
    effects_reaped INT NOT NULL DEFAULT 0,
    CHECK (id = 1)  -- Enforce single row
);

-- Insert initial row
INSERT INTO seesaw_reaper_heartbeat (id, last_run, events_reaped, effects_reaped)
VALUES (1, NOW(), 0, 0);

CREATE INDEX idx_reaper_last_run ON seesaw_reaper_heartbeat(last_run);

COMMENT ON TABLE seesaw_reaper_heartbeat IS 'Monitor reaper health - alert if last_run > 3 minutes ago';

-- ============================================================
-- Archive Table (Optional - for long-term storage)
-- ============================================================

-- Archive old events (older than 30 days) here before deletion
-- Allows historical analysis without impacting production tables
CREATE TABLE seesaw_events_archive (LIKE seesaw_events INCLUDING ALL);

COMMENT ON TABLE seesaw_events_archive IS 'Long-term storage for events older than 30 days';

-- ============================================================
-- Helper Functions
-- ============================================================

-- Function to clean up old completed events (run via cron)
CREATE OR REPLACE FUNCTION seesaw_cleanup_old_events(retention_days INT DEFAULT 30)
RETURNS TABLE(events_deleted BIGINT, effects_deleted BIGINT) AS $$
DECLARE
    events_count BIGINT;
    effects_count BIGINT;
BEGIN
    -- Archive events to archive table (optional)
    INSERT INTO seesaw_events_archive
    SELECT * FROM seesaw_events
    WHERE processed_at < NOW() - (retention_days || ' days')::INTERVAL;

    -- Delete old events
    DELETE FROM seesaw_events
    WHERE processed_at < NOW() - (retention_days || ' days')::INTERVAL;
    GET DIAGNOSTICS events_count = ROW_COUNT;

    -- Delete old completed effects
    DELETE FROM seesaw_effect_executions
    WHERE completed_at < NOW() - (retention_days || ' days')::INTERVAL
    AND status = 'completed';
    GET DIAGNOSTICS effects_count = ROW_COUNT;

    -- Update reaper heartbeat
    UPDATE seesaw_reaper_heartbeat
    SET last_run = NOW(),
        events_reaped = events_reaped + events_count,
        effects_reaped = effects_reaped + effects_count;

    RETURN QUERY SELECT events_count, effects_count;
END;
$$ LANGUAGE plpgsql;

COMMENT ON FUNCTION seesaw_cleanup_old_events IS 'Cleanup function - run via cron every hour';

-- Function to create next partition (run daily via cron)
CREATE OR REPLACE FUNCTION seesaw_create_next_partition(days_ahead INT DEFAULT 7)
RETURNS TEXT AS $$
DECLARE
    partition_date DATE;
    partition_name TEXT;
    start_date TEXT;
    end_date TEXT;
BEGIN
    partition_date := CURRENT_DATE + (days_ahead || ' days')::INTERVAL;
    partition_name := 'seesaw_events_' || TO_CHAR(partition_date, 'YYYY_MM_DD');
    start_date := TO_CHAR(partition_date, 'YYYY-MM-DD');
    end_date := TO_CHAR(partition_date + INTERVAL '1 day', 'YYYY-MM-DD');

    -- Check if partition already exists
    IF EXISTS (
        SELECT 1 FROM pg_class WHERE relname = partition_name
    ) THEN
        RETURN 'Partition ' || partition_name || ' already exists';
    END IF;

    -- Create partition
    EXECUTE format(
        'CREATE TABLE %I PARTITION OF seesaw_events FOR VALUES FROM (%L) TO (%L)',
        partition_name,
        start_date,
        end_date
    );

    RETURN 'Created partition ' || partition_name;
END;
$$ LANGUAGE plpgsql;

COMMENT ON FUNCTION seesaw_create_next_partition IS 'Create partition 7 days ahead - run daily via cron';

-- ============================================================
-- Monitoring Queries (Copy these to your observability tool)
-- ============================================================

-- Alert: Reaper hasn't run in 3 minutes (critical!)
-- SELECT EXTRACT(EPOCH FROM (NOW() - last_run)) as seconds_stale
-- FROM seesaw_reaper_heartbeat
-- WHERE last_run < NOW() - INTERVAL '3 minutes';

-- Alert: Queue depth > 10,000 (backlog)
-- SELECT COUNT(*) as pending_events
-- FROM seesaw_events
-- WHERE processed_at IS NULL;

-- Alert: State size > 10KB (performance risk)
-- SELECT correlation_id, LENGTH(state::text) as state_bytes
-- FROM seesaw_state
-- WHERE LENGTH(state::text) > 10240
-- ORDER BY state_bytes DESC;

-- Alert: High failure rate
-- SELECT COUNT(*) FILTER (WHERE status = 'failed') as failed,
--        COUNT(*) as total,
--        (COUNT(*) FILTER (WHERE status = 'failed')::float / COUNT(*)) as failure_rate
-- FROM seesaw_effect_executions
-- WHERE completed_at > NOW() - INTERVAL '5 minutes';

-- Dashboard: Active sagas
-- SELECT COUNT(DISTINCT correlation_id) as active_sagas
-- FROM seesaw_effect_executions
-- WHERE status IN ('pending', 'executing');

-- Dashboard: Effect execution latency (p50, p95, p99)
-- SELECT
--     PERCENTILE_CONT(0.5) WITHIN GROUP (ORDER BY EXTRACT(EPOCH FROM (completed_at - execute_at))) as p50,
--     PERCENTILE_CONT(0.95) WITHIN GROUP (ORDER BY EXTRACT(EPOCH FROM (completed_at - execute_at))) as p95,
--     PERCENTILE_CONT(0.99) WITHIN GROUP (ORDER BY EXTRACT(EPOCH FROM (completed_at - execute_at))) as p99
-- FROM seesaw_effect_executions
-- WHERE completed_at > NOW() - INTERVAL '1 hour' AND status = 'completed';

-- ============================================================
-- Grants (Adjust for your security model)
-- ============================================================

-- Example: Grant permissions to app user
-- GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO seesaw_app;
-- GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA public TO seesaw_app;
-- GRANT EXECUTE ON ALL FUNCTIONS IN SCHEMA public TO seesaw_app;

-- Example: Read-only user for monitoring
-- GRANT SELECT ON ALL TABLES IN SCHEMA public TO seesaw_readonly;

-- ============================================================
-- Maintenance Notes
-- ============================================================

-- Daily:
--   - Run seesaw_create_next_partition() to create partitions 7 days ahead
--   - Monitor reaper heartbeat (alert if > 3 minutes stale)

-- Hourly:
--   - Run seesaw_cleanup_old_events() to archive and delete old data

-- Weekly:
--   - DROP old partitions older than 30 days:
--     DROP TABLE seesaw_events_2026_01_01;

-- Monthly:
--   - VACUUM ANALYZE all tables
--   - Check state size distribution
--   - Review DLQ for patterns

-- Schema complete! Ready for production at 1000+ events/sec.
