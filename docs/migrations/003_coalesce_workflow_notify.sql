-- Switch workflow LISTEN/NOTIFY payload to constant wake-up token.
-- This allows Postgres to coalesce duplicate notifications within a transaction.

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
