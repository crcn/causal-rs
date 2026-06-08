# Migrations

Two equivalent ways to provision the causal-rs Postgres schema:

1. **Apply `migrations/` in filename order** (via your migration runner). A
   fresh database gets the core tables from `20260101_create_causal_tables.sql`
   and the inspector observability tables from
   `20260608_reactor_observability.sql`; the intervening
   `20260514_kurrent_alignment.sql` is a no-op (its renames are guarded by
   `IF EXISTS`).
2. **Apply [`docs/schema.sql`](../docs/schema.sql) directly** — the same schema
   as a single file, handy for tests and one-shot setups.

Both yield an identical schema (verified: the PG conformance suite passes against
a database built either way).

| File | Purpose |
|---|---|
| `20260101_create_causal_tables.sql` | Core schema — event log, checkpoints, snapshots, projection tables + indexes. Kept in sync with `docs/schema.sql`. |
| `20260514_kurrent_alignment.sql` | Incremental upgrade for a DB on the *older* causal schema (`parent_id` → `causation_id`, `version` → `revision`). Idempotent; a no-op on a fresh DB. Note the 0-indexed `StreamRevision` value shift in the file header. |
| `20260608_reactor_observability.sql` | Best-effort inspector read model — reactor executions, logs, descriptions, and aggregate snapshots. Kept in sync with `docs/schema.sql`. |
