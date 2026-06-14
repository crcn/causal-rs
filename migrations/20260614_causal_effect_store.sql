-- Effect-store: durable memo of a reactor's side-effecting result.
-- Keyed by (consumer, trigger_event_id, label); first-write-wins via
-- INSERT ... ON CONFLICT DO NOTHING.
CREATE TABLE IF NOT EXISTS causal_effect_store (
    consumer         TEXT    NOT NULL,
    trigger_event_id UUID    NOT NULL,
    label            TEXT    NOT NULL,
    value            JSONB   NOT NULL,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (consumer, trigger_event_id, label)
);
