-- Divergent-redelivery observability (PgReactorObserver → PgInspectorReadModel).
--
-- DEPRECATED (0.19): decision records (causal_decisions) make the reactor body
-- replay from a sealed batch instead of re-deciding, so first-delivery
-- divergence now fires only on the rare re-decide of a GC'd record. Replay
-- divergence is instead an integrity PARK (a terminal failure), not a row here.
-- Retained for that residual first-delivery signal; slated for drop in 0.20.
--
-- A reactor whose react() output is nondeterministic (bad emission order,
-- Uuid::new_v4(), an un-effect'd clock/RNG, HashMap iteration order) re-derives
-- the same identity-keyed event_id with a DIFFERENT payload on redelivery (a
-- crash between append and ack, or a full replay / cursor rewind). The store
-- keeps the persisted row and the runner accepts it — advances the cursor,
-- never retries (every retry re-diverges) and never parks (the canonical output
-- succeeded). It is NOT a failure, so it is recorded here, apart from the
-- lifecycle statuses in causal_reactor_executions, and surfaced by the read
-- model as a `diverged` flag — never an error/dead_letter.
--
-- Best-effort like the rest of the inspector tables: written off the hot path,
-- batched, ON CONFLICT upsert (keep the latest diff/at). KurrentDB remains the
-- durable source of truth. Kept in sync with docs/schema.sql.
CREATE TABLE causal_reactor_divergences (
    event_id        UUID NOT NULL,
    reactor_id      TEXT NOT NULL,
    correlation_id  UUID NOT NULL,
    diff            TEXT NOT NULL,
    at              TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (event_id, reactor_id)
);
CREATE INDEX idx_reactor_diverg_corr ON causal_reactor_divergences (correlation_id);
