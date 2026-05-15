-- KurrentDB-alignment migration for causal v0.4 PG backend.
--
-- Renames columns to match KurrentDB vocabulary so causal-rs +
-- rootsignal stay aligned with the broader Kurrent ecosystem. See
-- `docs/plans/2026-05-14-kurrent-alignment-design.md` and the
-- 2026-05-14 entry in `CHANGELOG.md`.
--
-- Two tables touched:
--   * causal_log — `parent_id` → `causation_id`, `version` → `revision`
--   * causal_snapshots — `version` → `revision`
--
-- Idempotent: each rename guarded by `IF EXISTS` on the source
-- column name. Re-running on an already-migrated DB is a no-op.
--
-- VALUE SHIFT (NOT applied by this migration — manual operator step).
-- causal v0.4 changed `StreamVersion` (1-indexed) → `StreamRevision`
-- (0-indexed) at the same time as the column rename. If your DB
-- has historical data written under the old 1-indexed convention,
-- the values in `revision` AFTER this migration are still 1-indexed
-- (1, 2, 3...) and will be off-by-one compared to fresh writes from
-- v0.4 code. Two safe paths:
--
--   (a) Empty DB / pre-launch — run this migration, no value shift
--       needed. (rootsignal: this is your case.)
--   (b) Production DB with v0.3 data — coordinate downtime, then
--       `UPDATE causal_log SET revision = revision - 1
--          WHERE aggregate_type IS NOT NULL AND revision >= 1;`
--       and similarly for `causal_snapshots`. This shifts every
--       existing event's per-stream revision to match the new
--       0-indexed contract. Run AFTER this rename migration, BEFORE
--       v0.4 code reads from the table.
--
-- Verification queries at the bottom.

BEGIN;

-- ── causal_log: parent_id → causation_id ────────────────────────────
DO $$
BEGIN
    IF EXISTS (
        SELECT 1 FROM information_schema.columns
         WHERE table_name = 'causal_log' AND column_name = 'parent_id'
    ) THEN
        ALTER TABLE causal_log RENAME COLUMN parent_id TO causation_id;
    END IF;
END
$$;

-- ── causal_log: version → revision ──────────────────────────────────
DO $$
BEGIN
    IF EXISTS (
        SELECT 1 FROM information_schema.columns
         WHERE table_name = 'causal_log' AND column_name = 'version'
    ) THEN
        ALTER TABLE causal_log RENAME COLUMN version TO revision;
    END IF;
END
$$;

-- Indexes named with the old column name need to be recreated so
-- future schema dumps reflect the rename. Postgres keeps the index
-- functional under the renamed column automatically; we drop+recreate
-- so the index name + DDL stay coherent.
DROP INDEX IF EXISTS idx_causal_log_stream;
CREATE UNIQUE INDEX idx_causal_log_stream
    ON causal_log (aggregate_type, aggregate_id, revision)
    WHERE aggregate_type IS NOT NULL;

-- ── causal_snapshots: version → revision ────────────────────────────
DO $$
BEGIN
    IF EXISTS (
        SELECT 1 FROM information_schema.columns
         WHERE table_name = 'causal_snapshots' AND column_name = 'version'
    ) THEN
        ALTER TABLE causal_snapshots RENAME COLUMN version TO revision;
    END IF;
END
$$;

-- Snapshot primary key / unique index uses the version column; drop
-- and recreate against `revision` to stay in sync.
ALTER TABLE causal_snapshots DROP CONSTRAINT IF EXISTS causal_snapshots_key_version_key;
ALTER TABLE causal_snapshots
    ADD CONSTRAINT causal_snapshots_key_revision_key
    UNIQUE (key, revision);

COMMIT;

-- ── Verification ────────────────────────────────────────────────────
-- After running, these queries should each return a row (success) or
-- zero rows (column gone, rename successful).

-- 1. New column names exist:
--    SELECT column_name FROM information_schema.columns
--     WHERE table_name = 'causal_log'
--       AND column_name IN ('causation_id', 'revision');
--    -- Expect: 2 rows.

-- 2. Old column names are gone:
--    SELECT column_name FROM information_schema.columns
--     WHERE table_name = 'causal_log'
--       AND column_name IN ('parent_id', 'version');
--    -- Expect: 0 rows.

-- 3. Indexes use the new column:
--    SELECT indexdef FROM pg_indexes
--     WHERE tablename = 'causal_log' AND indexname = 'idx_causal_log_stream';
--    -- Expect: indexdef contains 'revision', not 'version'.

-- 4. Snapshot constraint:
--    SELECT conname FROM pg_constraint
--     WHERE conrelid = 'causal_snapshots'::regclass
--       AND contype = 'u';
--    -- Expect: row with conname = 'causal_snapshots_key_revision_key'.
