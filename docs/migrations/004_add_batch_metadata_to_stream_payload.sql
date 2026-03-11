-- Include batch metadata in causal_stream event payload envelopes.
-- Safe to run multiple times.

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

    PERFORM pg_notify('causal_stream', NEW.correlation_id::text);

    RETURN NEW;
END;
$$ LANGUAGE plpgsql;
