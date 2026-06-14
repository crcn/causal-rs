-- Partial index on causation_id for efficient recursive CTE traversal.
-- Required by the inspector's subject_chain DESCENDANTS mode, which walks
-- the causation chain via a depth-capped recursive CTE. Without this index
-- each recursion level does a full sequential scan of causal_log.
CREATE INDEX IF NOT EXISTS idx_causal_log_causation
    ON causal_log (causation_id)
    WHERE causation_id IS NOT NULL;
