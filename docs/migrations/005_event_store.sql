-- Event Sourcing: permanent event storage with optimistic concurrency
CREATE TABLE causal_event_store (
    position BIGINT GENERATED ALWAYS AS IDENTITY,
    id UUID UNIQUE DEFAULT gen_random_uuid(),
    aggregate_id UUID NOT NULL,
    aggregate_type TEXT NOT NULL,
    sequence BIGINT NOT NULL,
    event_type TEXT NOT NULL,
    event_data JSONB NOT NULL,
    metadata JSONB NOT NULL DEFAULT '{}',
    schema_version INT NOT NULL DEFAULT 1,
    caused_by UUID,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (position),
    UNIQUE (aggregate_id, sequence)
);

CREATE INDEX idx_es_aggregate ON causal_event_store(aggregate_id, sequence ASC);
CREATE INDEX idx_es_type ON causal_event_store(aggregate_type);
CREATE INDEX idx_es_caused_by ON causal_event_store(caused_by) WHERE caused_by IS NOT NULL;
CREATE INDEX idx_es_position ON causal_event_store(position ASC);
