-- Adds the `parked` column to causal_decisions (0.19 park-as-decision fix).
--
-- A terminal PARK is now a sealed decision ("the decision was: fail
-- terminally"), so redelivery replays it instead of re-running the body and
-- possibly producing a contradictory success (the audit's park chimera, #3).
-- `parked` distinguishes a park record from a success record.
--
-- Idempotent and additive: existing rows default to FALSE (success), which is
-- correct — no pre-0.19-park row was ever sealed. Applied by
-- PgDecisionStore::ensure_schema; ship through your migration pipeline too.
ALTER TABLE causal_decisions
    ADD COLUMN IF NOT EXISTS parked BOOLEAN NOT NULL DEFAULT FALSE;

-- Companion backfill for tables created between the initial decisions schema
-- and the trigger_position fix: CREATE TABLE IF NOT EXISTS never adds a column
-- to an existing table, so ensure_schema alone could leave it missing.
ALTER TABLE causal_decisions
    ADD COLUMN IF NOT EXISTS trigger_position BIGINT NOT NULL DEFAULT 0;
