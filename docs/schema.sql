-- Causal Queue-Backed Architecture - Production Schema
-- Version: 0.9.1
-- Last Updated: 2026-02-06
--
-- This schema is designed for millions of users at 1000+ events/sec
-- Features: Per-workflow FIFO, idempotency, two-phase execution, table partitioning

-- ============================================================
-- Core Tables
-- ============================================================

-- Events Queue (Partitioned by date for vacuum efficiency)
-- Stores all events to be processed by event workers
CREATE TABLE causal_events (
    id BIGSERIAL,
    event_id UUID NOT NULL,
    parent_id UUID,
    correlation_id UUID NOT NULL,           -- Workflow identifier (envelope metadata)
    event_type VARCHAR(255) NOT NULL,
    payload JSONB NOT NULL,
    hops INT NOT NULL DEFAULT 0,     -- Infinite loop protection (DLQ after 50)
    batch_id UUID,                   -- Same-batch fan-in token
    batch_index INT,                 -- 0-based position in emitted batch
    batch_size INT,                  -- total items in emitted batch
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    processed_at TIMESTAMPTZ,        -- NULL = pending, set when complete
    locked_until TIMESTAMPTZ,        -- For retry backoff
    retry_count INT NOT NULL DEFAULT 0,
    PRIMARY KEY (id, created_at)     -- Include created_at for partitioning
) PARTITION BY RANGE (created_at);

-- Idempotency: Prevent duplicate event_ids (webhooks, crash+retry)
-- Must include partitioning column (created_at) for partitioned table
-- For deterministic events, framework uses fixed timestamp to ensure idempotency
CREATE UNIQUE INDEX idx_events_event_id ON causal_events(event_id, created_at);

-- Per-workflow FIFO with advisory locks (workers use this to poll)
CREATE INDEX idx_events_pending ON causal_events(created_at ASC)
WHERE processed_at IS NULL;

-- Lookup events by workflow (for introspection/debugging)
CREATE INDEX idx_events_workflow ON causal_events(correlation_id, created_at);

-- Cleanup: Find old events for archival
CREATE INDEX idx_events_cleanup ON causal_events(processed_at)
WHERE processed_at IS NOT NULL;

COMMENT ON TABLE causal_events IS 'Event queue - workers poll with SKIP LOCKED for parallel processing';
COMMENT ON COLUMN causal_events.hops IS 'Incremented on each event emission - DLQ after 50 to prevent infinite loops';
COMMENT ON COLUMN causal_events.correlation_id IS 'Envelope metadata - groups related events into a workflow';

-- LISTEN/NOTIFY trigger for .wait() pattern (CQRS support)
-- Enables engine.process(event).wait::<TerminalEvent>().await
-- Uses a constant payload so notifications can be coalesced per transaction.
CREATE OR REPLACE FUNCTION notify_workflow_event()
RETURNS TRIGGER AS $$
BEGIN
    PERFORM pg_notify(
        'causal_workflow_' || NEW.correlation_id::text,
        'wake'
    );
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER causal_events_notify
    AFTER INSERT ON causal_events
    FOR EACH ROW
    EXECUTE FUNCTION notify_workflow_event();

COMMENT ON FUNCTION notify_workflow_event IS 'Push wake-up notification for wait() pattern; consumers fetch new rows by cursor';

-- Create initial partitions (daily partitions - must create before inserts)
-- In production, automate partition creation (cron job or pg_partman)
CREATE TABLE causal_events_2026_02_05 PARTITION OF causal_events
FOR VALUES FROM ('2026-02-05') TO ('2026-02-06');

CREATE TABLE causal_events_2026_02_06 PARTITION OF causal_events
FOR VALUES FROM ('2026-02-06') TO ('2026-02-07');

CREATE TABLE causal_events_2026_02_07 PARTITION OF causal_events
FOR VALUES FROM ('2026-02-07') TO ('2026-02-08');

-- Note: Create partitions 7 days ahead, drop partitions older than 30 days
-- See docs/partitioning.md for automation scripts

-- Event processing ledger (atomic claim + phase tracking + audit trail)
-- Purpose: Separate from queue for atomic claiming and historical record
CREATE TABLE causal_processed (
    event_id UUID PRIMARY KEY,
    correlation_id UUID NOT NULL,
    completed_at TIMESTAMPTZ,        -- When Phase 2 (all effects) completed
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_processed_workflow ON causal_processed(correlation_id);
CREATE INDEX idx_processed_completed ON causal_processed(completed_at DESC)
WHERE completed_at IS NOT NULL;

COMMENT ON TABLE causal_processed IS 'Processing ledger - atomic claim via ON CONFLICT, persists after queue deletion';
COMMENT ON COLUMN causal_processed.completed_at IS 'Phase 2 complete - all effects finished';

-- Handler execution intents (framework-guaranteed idempotency)
-- Effect workers poll this table for ready effects
CREATE TABLE causal_handler_executions (
    event_id UUID NOT NULL,
    handler_id VARCHAR(255) NOT NULL,
    correlation_id UUID NOT NULL,
    status VARCHAR(50) NOT NULL DEFAULT 'pending',  -- pending, executing, completed, failed (retryable)
    result JSONB,
    error TEXT,
    attempts INT NOT NULL DEFAULT 0,

    -- Event payload (survives 30-day retention deletion)
    event_type VARCHAR(255) NOT NULL,
    event_payload JSONB NOT NULL,    -- Copied from causal_events for delayed effects
    parent_event_id UUID,
    batch_id UUID,
    batch_index INT,
    batch_size INT,

    -- Execution properties (from handler builder)
    execute_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),  -- When to execute (.delayed())
    timeout_seconds INT NOT NULL DEFAULT 30,
    max_attempts INT NOT NULL DEFAULT 3,
    priority INT NOT NULL DEFAULT 10,               -- Lower = higher priority
    join_window_timeout_seconds INT,

    claimed_at TIMESTAMPTZ,          -- NULL until claimed by worker
    completed_at TIMESTAMPTZ,
    last_attempted_at TIMESTAMPTZ,

    PRIMARY KEY (event_id, handler_id)
);

-- Worker polling: Find next ready handler (respects priority and schedule)
-- Note: execute_at <= NOW() check is done in query, not index (NOW() is STABLE not IMMUTABLE)
-- Lower priority number = higher priority, so ASC order
CREATE INDEX idx_handler_executions_pending
ON causal_handler_executions(priority ASC, execute_at ASC)
WHERE status = 'pending';

-- Retry polling for failed effects waiting for backoff expiry.
CREATE INDEX idx_handler_executions_retry
ON causal_handler_executions(priority ASC, execute_at ASC)
WHERE status = 'failed';

-- Lookup effects by event (for debugging)
CREATE INDEX idx_handler_executions_event ON causal_handler_executions(event_id);

-- Lookup effects by workflow (for progress tracking)
CREATE INDEX idx_handler_executions_workflow ON causal_handler_executions(correlation_id);

-- Monitor failures
CREATE INDEX idx_handler_executions_failed ON causal_handler_executions(status, attempts)
WHERE status = 'failed';

COMMENT ON TABLE causal_handler_executions IS 'Handler intents - created by event workers, executed by handler workers';
COMMENT ON COLUMN causal_handler_executions.event_payload IS 'Event data copied here - survives parent event deletion after 30 days';
COMMENT ON COLUMN causal_handler_executions.priority IS 'Worker polling priority - lower number = higher priority';

-- ============================================================
-- Join Tables (same-batch durable fan-in)
-- ============================================================

CREATE TABLE causal_join_entries (
    join_handler_id VARCHAR(255) NOT NULL,
    correlation_id UUID NOT NULL,
    source_event_id UUID NOT NULL,
    source_event_type VARCHAR(255) NOT NULL,
    source_payload JSONB NOT NULL,
    source_created_at TIMESTAMPTZ NOT NULL,
    batch_id UUID NOT NULL,
    batch_index INT NOT NULL,
    batch_size INT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (join_handler_id, correlation_id, source_event_id),
    UNIQUE (join_handler_id, correlation_id, batch_id, batch_index)
);

CREATE INDEX idx_join_entries_window
ON causal_join_entries(join_handler_id, correlation_id, batch_id, batch_index);

CREATE TABLE causal_join_windows (
    join_handler_id VARCHAR(255) NOT NULL,
    correlation_id UUID NOT NULL,
    mode VARCHAR(50) NOT NULL DEFAULT 'same_batch',
    batch_id UUID NOT NULL,
    target_count INT NOT NULL,
    status VARCHAR(50) NOT NULL DEFAULT 'open', -- open, processing, completed
    window_timeout_seconds INT,
    expires_at TIMESTAMPTZ,
    sealed_at TIMESTAMPTZ,
    processing_started_at TIMESTAMPTZ,
    completed_at TIMESTAMPTZ,
    last_error TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (join_handler_id, correlation_id, batch_id)
);

CREATE INDEX idx_join_windows_status
ON causal_join_windows(status, updated_at DESC);

-- ============================================================
-- Dead Letter Queue
-- ============================================================

-- Failed effects that exceeded retry limits
CREATE TABLE causal_dlq (
    event_id UUID NOT NULL,
    handler_id VARCHAR(255) NOT NULL,
    correlation_id UUID NOT NULL,
    error TEXT NOT NULL,
    event_type VARCHAR(255) NOT NULL,
    event_payload JSONB NOT NULL,
    reason VARCHAR(50) NOT NULL,     -- e.g. 'failed', 'timeout', 'infinite_loop', 'inline_failed', 'max_retries_exceeded'
    attempts INT NOT NULL DEFAULT 0,
    failed_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    resolved_at TIMESTAMPTZ,         -- NULL = unresolved, set when manually fixed
    PRIMARY KEY (event_id, handler_id)
);

-- Ops query: List unresolved failures
CREATE INDEX idx_dlq_unresolved ON causal_dlq(failed_at DESC)
WHERE resolved_at IS NULL;

-- Filter by failure reason
CREATE INDEX idx_dlq_reason ON causal_dlq(reason, failed_at DESC)
WHERE resolved_at IS NULL;

-- Find DLQ entries by workflow
CREATE INDEX idx_dlq_workflow ON causal_dlq(correlation_id);

COMMENT ON TABLE causal_dlq IS 'Dead letter queue - effects that failed permanently';
COMMENT ON COLUMN causal_dlq.reason IS 'Why it failed: failed (retry exhausted), timeout, infinite_loop, inline_failed, max_retries_exceeded';

-- ============================================================
-- Observability Stream (for real-time visualization)
-- ============================================================

-- Stream of all workflow events and handler transitions
-- Append-only table for real-time monitoring and debugging
CREATE TABLE causal_stream (
    seq BIGSERIAL PRIMARY KEY,
    stream_type VARCHAR(50) NOT NULL,  -- 'event_dispatched', 'effect_started', 'effect_completed', 'effect_failed'
    correlation_id UUID NOT NULL,
    event_id UUID,
    effect_event_id UUID,              -- For effects: the triggering event_id
    handler_id VARCHAR(255),             -- For handlers: the handler identifier
    status VARCHAR(50),                 -- For effects: 'executing', 'completed', 'failed'
    error TEXT,                         -- For failures: error message
    payload JSONB,                      -- Event payload or handler result
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Fast lookups by sequence (for cursor-based pagination)
CREATE INDEX idx_stream_seq ON causal_stream(seq DESC);

-- Filter by correlation_id (for workflow-specific streams)
CREATE INDEX idx_stream_correlation ON causal_stream(correlation_id, seq DESC);

-- Time-based cleanup/archival
CREATE INDEX idx_stream_created ON causal_stream(created_at DESC);

COMMENT ON TABLE causal_stream IS 'Observability stream - append-only log for real-time visualization';
COMMENT ON COLUMN causal_stream.seq IS 'Global sequence number for cursor-based pagination';
COMMENT ON COLUMN causal_stream.stream_type IS 'Event type: event_dispatched, effect_started, effect_completed, effect_failed';

-- Trigger: Stream event dispatches
CREATE OR REPLACE FUNCTION stream_event_dispatched()
RETURNS TRIGGER AS $$
BEGIN
    INSERT INTO causal_stream (
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
            'batch_id', NEW.batch_id,
            'batch_index', NEW.batch_index,
            'batch_size', NEW.batch_size,
            'payload', NEW.payload
        ),
        NEW.created_at
    );

    -- NOTIFY for SSE wake-up (correlation_id only, clients fetch from stream table)
    PERFORM pg_notify('causal_stream', NEW.correlation_id::text);

    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER causal_events_stream
    AFTER INSERT ON causal_events
    FOR EACH ROW
    EXECUTE FUNCTION stream_event_dispatched();

-- Trigger: Stream handler execution lifecycle
CREATE OR REPLACE FUNCTION stream_handler_transition()
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

    INSERT INTO causal_stream (
        stream_type,
        correlation_id,
        event_id,
        effect_event_id,
        handler_id,
        status,
        error,
        payload,
        created_at
    ) VALUES (
        stream_type_val,
        NEW.correlation_id,
        NULL,  -- No new event for handler lifecycle
        NEW.event_id,
        NEW.handler_id,
        NEW.status,
        NEW.error,
        NEW.result,
        NOW()
    );

    -- NOTIFY for SSE wake-up
    PERFORM pg_notify('causal_stream', NEW.correlation_id::text);

    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER causal_handler_executions_stream
    AFTER INSERT OR UPDATE ON causal_handler_executions
    FOR EACH ROW
    EXECUTE FUNCTION stream_handler_transition();

COMMENT ON FUNCTION stream_event_dispatched IS 'Trigger: Log event dispatches to observability stream';
COMMENT ON FUNCTION stream_handler_transition IS 'Trigger: Log handler lifecycle transitions to observability stream';

-- ============================================================
-- Operational Tables
-- ============================================================

-- Reaper heartbeat (critical for production monitoring)
-- Single-row table to track background cleanup process
CREATE TABLE causal_reaper_heartbeat (
    id INT PRIMARY KEY DEFAULT 1,
    last_run TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    events_reaped INT NOT NULL DEFAULT 0,
    handlers_reaped INT NOT NULL DEFAULT 0,
    CHECK (id = 1)  -- Enforce single row
);

-- Insert initial row
INSERT INTO causal_reaper_heartbeat (id, last_run, events_reaped, handlers_reaped)
VALUES (1, NOW(), 0, 0);

CREATE INDEX idx_reaper_last_run ON causal_reaper_heartbeat(last_run);

COMMENT ON TABLE causal_reaper_heartbeat IS 'Monitor reaper health - alert if last_run > 3 minutes ago';

-- ============================================================
-- Archive Table (Optional - for long-term storage)
-- ============================================================

-- Archive old events (older than 30 days) here before deletion
-- Allows historical analysis without impacting production tables
CREATE TABLE causal_events_archive (LIKE causal_events INCLUDING ALL);

COMMENT ON TABLE causal_events_archive IS 'Long-term storage for events older than 30 days';

-- ============================================================
-- Helper Functions
-- ============================================================

-- Function to clean up old completed events (run via cron)
CREATE OR REPLACE FUNCTION causal_cleanup_old_events(retention_days INT DEFAULT 30)
RETURNS TABLE(events_deleted BIGINT, handlers_deleted BIGINT) AS $$
DECLARE
    events_count BIGINT;
    handlers_count BIGINT;
BEGIN
    -- Archive events to archive table (optional)
    INSERT INTO causal_events_archive
    SELECT * FROM causal_events
    WHERE processed_at < NOW() - (retention_days || ' days')::INTERVAL;

    -- Delete old events
    DELETE FROM causal_events
    WHERE processed_at < NOW() - (retention_days || ' days')::INTERVAL;
    GET DIAGNOSTICS events_count = ROW_COUNT;

    -- Delete old completed handlers
    DELETE FROM causal_handler_executions
    WHERE completed_at < NOW() - (retention_days || ' days')::INTERVAL
    AND status = 'completed';
    GET DIAGNOSTICS handlers_count = ROW_COUNT;

    -- Update reaper heartbeat
    UPDATE causal_reaper_heartbeat
    SET last_run = NOW(),
        events_reaped = events_reaped + events_count,
        handlers_reaped = handlers_reaped + handlers_count;

    RETURN QUERY SELECT events_count, handlers_count;
END;
$$ LANGUAGE plpgsql;

COMMENT ON FUNCTION causal_cleanup_old_events IS 'Cleanup function - run via cron every hour';

-- Function to create next partition (run daily via cron)
CREATE OR REPLACE FUNCTION causal_create_next_partition(days_ahead INT DEFAULT 7)
RETURNS TEXT AS $$
DECLARE
    partition_date DATE;
    partition_name TEXT;
    start_date TEXT;
    end_date TEXT;
BEGIN
    partition_date := CURRENT_DATE + (days_ahead || ' days')::INTERVAL;
    partition_name := 'causal_events_' || TO_CHAR(partition_date, 'YYYY_MM_DD');
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
        'CREATE TABLE %I PARTITION OF causal_events FOR VALUES FROM (%L) TO (%L)',
        partition_name,
        start_date,
        end_date
    );

    RETURN 'Created partition ' || partition_name;
END;
$$ LANGUAGE plpgsql;

COMMENT ON FUNCTION causal_create_next_partition IS 'Create partition 7 days ahead - run daily via cron';

-- ============================================================
-- Monitoring Queries (Copy these to your observability tool)
-- ============================================================

-- Alert: Reaper hasn't run in 3 minutes (critical!)
-- SELECT EXTRACT(EPOCH FROM (NOW() - last_run)) as seconds_stale
-- FROM causal_reaper_heartbeat
-- WHERE last_run < NOW() - INTERVAL '3 minutes';

-- Alert: Queue depth > 10,000 (backlog)
-- SELECT COUNT(*) as pending_events
-- FROM causal_events
-- WHERE processed_at IS NULL;

-- Alert: Pending handler payload size > 10KB (performance risk)
-- SELECT event_id, LENGTH(event_payload::text) as payload_bytes
-- FROM causal_handler_executions
-- WHERE LENGTH(event_payload::text) > 10240
-- ORDER BY payload_bytes DESC;

-- Alert: High failure rate
-- SELECT COUNT(*) FILTER (WHERE status = 'failed') as failed,
--        COUNT(*) as total,
--        (COUNT(*) FILTER (WHERE status = 'failed')::float / COUNT(*)) as failure_rate
-- FROM causal_handler_executions
-- WHERE completed_at > NOW() - INTERVAL '5 minutes';

-- Dashboard: Active workflows
-- SELECT COUNT(DISTINCT correlation_id) as active_workflows
-- FROM causal_handler_executions
-- WHERE status IN ('pending', 'executing', 'failed');

-- Dashboard: Handler execution latency (p50, p95, p99)
-- SELECT
--     PERCENTILE_CONT(0.5) WITHIN GROUP (ORDER BY EXTRACT(EPOCH FROM (completed_at - execute_at))) as p50,
--     PERCENTILE_CONT(0.95) WITHIN GROUP (ORDER BY EXTRACT(EPOCH FROM (completed_at - execute_at))) as p95,
--     PERCENTILE_CONT(0.99) WITHIN GROUP (ORDER BY EXTRACT(EPOCH FROM (completed_at - execute_at))) as p99
-- FROM causal_handler_executions
-- WHERE completed_at > NOW() - INTERVAL '1 hour' AND status = 'completed';

-- ============================================================
-- Grants (Adjust for your security model)
-- ============================================================

-- Example: Grant permissions to app user
-- GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO causal_app;
-- GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA public TO causal_app;
-- GRANT EXECUTE ON ALL FUNCTIONS IN SCHEMA public TO causal_app;

-- Example: Read-only user for monitoring
-- GRANT SELECT ON ALL TABLES IN SCHEMA public TO causal_readonly;

-- ============================================================
-- Maintenance Notes
-- ============================================================

-- Daily:
--   - Run causal_create_next_partition() to create partitions 7 days ahead
--   - Monitor reaper heartbeat (alert if > 3 minutes stale)

-- Hourly:
--   - Run causal_cleanup_old_events() to archive and delete old data

-- Weekly:
--   - DROP old partitions older than 30 days:
--     DROP TABLE causal_events_2026_01_01;

-- Monthly:
--   - VACUUM ANALYZE all tables
--   - Check handler payload size distribution
--   - Review DLQ for patterns

-- Schema complete! Ready for production at 1000+ events/sec.
