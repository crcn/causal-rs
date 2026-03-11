-- Migration: Fix pg_notify 8000-byte payload limit
-- Issue: Large event payloads (e.g., 13 extracted posts) exceed pg_notify's 8000-byte limit
-- Solution: Remove payload from notification - it's only a wake-up signal
-- Date: 2026-02-06

-- Drop and recreate the trigger function without payload
CREATE OR REPLACE FUNCTION notify_workflow_event()
RETURNS TRIGGER AS $$
BEGIN
    PERFORM pg_notify(
        'causal_workflow_' || NEW.correlation_id::text,
        json_build_object(
            'event_id', NEW.event_id,
            'correlation_id', NEW.correlation_id,
            'event_type', NEW.event_type
            -- payload removed - pg_notify has 8000-byte limit
            -- Rust code will fetch from database if needed
        )::text
    );
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

-- Trigger remains the same, function is already replaced above
-- No need to drop/recreate trigger since it references function by name

COMMENT ON FUNCTION notify_workflow_event IS
'LISTEN/NOTIFY for .wait() pattern - notification is wake-up signal only, payload fetched from DB';
