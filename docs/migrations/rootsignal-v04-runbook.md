---
title: "Migration runbook: rootsignal v0.3 → v0.4"
type: migration-runbook
date: 2026-05-11
status: ready
target: rootsignal-scout (pre-launch)
companion_to: docs/plans/2026-05-11-v0.4-api-sharpening-plan.md
---

# Rootsignal v0.3 → v0.4 migration runbook

## Overview

Migrating rootsignal's persisted event log from v0.3's two-axis
Fact model (`type_prefix` + per-variant `stream().category`) to
v0.4's single-axis model (`CATEGORY` const).

Two things change in the persisted log:

1. **`events.event_type` strings.** Today composed as
   `"{type_prefix}:{variant_name}"`. Under v0.4 it's
   `"{CATEGORY}:{name()}"`. For aligned Facts (e.g., DiscoveryEvent
   with prefix="discovery" and category="discovery") this is a
   no-op. For diverging Facts (SchedulingEvent splits, Lifecycle
   and Scrape category renames) the string changes.

2. **`events.aggregate_type` strings.** Today's
   per-variant `stream().category`. Under v0.4 each Fact enum has
   ONE `CATEGORY` const. Categories may rename to align (e.g.,
   LifecycleEvent's `scout_run` → `lifecycle`).

Cursors in `CheckpointStore` are global log positions; they don't
change. Consumers resume at the same position with the new format.

This runbook is **forward-only safe up to Stage 4** and **terminal
after Stage 5** (deploying v0.4 code commits the format choice).

## What's changing — per-Fact mapping table

This is the **load-bearing table**. The migration SQL is generated
from these rows. Every Fact enum in rootsignal needs a row before
execution. Missing rows = events stuck in old format that v0.4 code
can't deserialize.

**Status: skeleton.** Rows for the 5 known-explicit Facts are filled
in. The 4 ephemeral domains have provisional CATEGORY assigned (per
P1.5 decision — see "Ephemeral-event domains" section below). The
other 6 enums (legacy-Event-only path today) need v0.4 Fact impls
during P12; their rows get filled in concurrently.

| Fact enum | v0.3 type_prefix | v0.3 stream categories | v0.4 CATEGORY | v0.4 split into | Notes |
|---|---|---|---|---|---|
| `DiscoveryEvent` | `discovery` | `discovery` | `discovery` | (no split) | Aligned — no event_type or aggregate_type changes |
| `TelemetryEvent` | `telemetry` | `telemetry` | `telemetry` | (no split) | Aligned — no changes |
| `LifecycleEvent` | `lifecycle` | `scout_run` | `lifecycle` | (no split) | event_type unchanged; **aggregate_type: `scout_run` → `lifecycle`** |
| `ScrapeEvent` | `scrape` | `scout_run` | `scrape` | (no split) | event_type unchanged; **aggregate_type: `scout_run` → `scrape`** |
| `SchedulingEvent` | `scheduling` | `schedule`, `scrape_schedule` | (split) | `ScheduleEvent` (CATEGORY=`schedule`) + `ScrapeScheduleEvent` (CATEGORY=`scrape_schedule`) | **event_type rewrite + split**: `scheduling:schedule_*` → `schedule:*`, `scheduling:scrape_scheduled` → `scrape_schedule:scheduled` |
| `SignalEvent` | `signal` | (no manual Fact impl) | TBD | (decide in P12) | Choose CATEGORY during P12; if `signal` then no rewrite. |
| `CuriosityEvent` | `curiosity` | (no manual Fact impl) | TBD | (decide in P12) | — |
| `CoalescingEvent` | `coalescing` | (ephemeral — never persisted) | `coalescing` | — | **P1.5: ephemeral → persistent.** Add `run_id: Uuid` to each variant. No SQL rewrite — no v0.3 rows exist. See "Ephemeral-event domains" section. |
| `ClusterWeavingEvent` | `cluster_weaving` | (ephemeral — never persisted) | `cluster_weaving` | — | **P1.5: ephemeral → persistent.** Same recipe as CoalescingEvent. |
| `EnrichmentEvent` | `enrichment` | (no manual Fact impl) | TBD | — | — |
| `ExpansionEvent` | `expansion` | (no manual Fact impl) | TBD | — | — |
| `SynthesisEvent` | `synthesis` | (no manual Fact impl) | TBD | — | — |
| `SituationWeavingEvent` | `situation_weaving` | (ephemeral — never persisted) | `situation_weaving` | — | **P1.5: ephemeral → persistent.** Same recipe. |
| `SupervisorEvent` | `supervisor` | (ephemeral — never persisted) | `supervisor` | — | **P1.5: ephemeral → persistent.** `SupervisionCompleted` is a unit variant today; will gain `run_id: Uuid`. |
| `SystemEvent` | (in `rootsignal-common`) | — | TBD | — | — |
| `WorldEvent` | `world` (in `rootsignal-world`) | — | TBD | — | — |

**Action item before Stage 2:** complete the TBD rows. For each
TBD: decide CATEGORY (default: keep current prefix string), decide
event name format (default: variant name in snake_case).

## Pre-flight checklist

Before ANY stage runs:

- [ ] All 16 Fact enums have rows in the mapping table above
- [ ] No Fact enum has variants targeting more than one category
      (i.e., all SchedulingEvent-shaped Facts identified and split)
- [ ] v0.4 code compiled in a staging branch (P1-P11 complete)
- [ ] Migration SQL drafted from the mapping table
- [ ] Migration SQL run against a copy of staging data (dry-run)
- [ ] Verification queries pass on dry-run output
- [ ] Rollback SQL drafted and tested on dry-run
- [ ] Cursor continuity test fixture exists and passes
- [ ] Operations team aware of deploy window

## Stage 1 — Stop consumers

```bash
# Stop rootsignal-scout supervisor + all worker processes.
# No emit traffic; no consumer reads.
systemctl stop rootsignal-scout
systemctl stop rootsignal-api  # if also writing to the event log
```

**Verification:** confirm via process check that nothing has the
DB connection open.

```sql
SELECT pid, query FROM pg_stat_activity
WHERE datname = 'rootsignal' AND state = 'active';
-- Should return zero rows (or only this query itself)
```

## Stage 2 — Rewrite `events.event_type`

Run inside one transaction per Fact for atomic per-Fact migration.

```sql
-- Template per Fact. Generated from the mapping table.
-- Example: SchedulingEvent split (two transactions):

-- (a) Schedule variants — namespace rename:
BEGIN;
UPDATE events
   SET event_type = REPLACE(event_type, 'scheduling:schedule_', 'schedule:')
 WHERE event_type LIKE 'scheduling:schedule_%';

-- Verify within the transaction before commit:
SELECT COUNT(*) FROM events WHERE event_type LIKE 'scheduling:schedule_%';
-- Expected: 0
COMMIT;

-- (b) Scrape-schedule variant — different namespace:
BEGIN;
UPDATE events
   SET event_type = 'scrape_schedule:scheduled'
 WHERE event_type = 'scheduling:scrape_scheduled';

SELECT COUNT(*) FROM events WHERE event_type = 'scheduling:scrape_scheduled';
-- Expected: 0
COMMIT;

-- (c) Aligned Facts — no rewrite needed (DiscoveryEvent, TelemetryEvent,
-- LifecycleEvent, ScrapeEvent; their type_prefix matches their new
-- CATEGORY for the prefix portion of event_type).

-- (d) TBD-row Facts — fill in similarly from the mapping table once
-- those rows are decided in P12. Most are expected to be no-ops
-- because the current type_prefix becomes the v0.4 CATEGORY.
```

**Idempotency:** every UPDATE has a `WHERE` clause that matches the
old format. Re-running is a no-op (zero rows updated). Safe to retry.

## Stage 3 — Rewrite `events.aggregate_type`

```sql
-- Lifecycle: aggregate_type 'scout_run' → 'lifecycle' for any
-- event with event_type starting with 'lifecycle:'
BEGIN;
UPDATE events
   SET aggregate_type = 'lifecycle'
 WHERE event_type LIKE 'lifecycle:%'
   AND aggregate_type = 'scout_run';

SELECT COUNT(*) FROM events
 WHERE event_type LIKE 'lifecycle:%' AND aggregate_type = 'scout_run';
-- Expected: 0
COMMIT;

-- Scrape: aggregate_type 'scout_run' → 'scrape' for scrape: events.
BEGIN;
UPDATE events
   SET aggregate_type = 'scrape'
 WHERE event_type LIKE 'scrape:%'
   AND aggregate_type = 'scout_run';
COMMIT;

-- SchedulingEvent splits: schedule:* events keep aggregate_type='schedule';
-- scrape_schedule:* events get aggregate_type='scrape_schedule'.
-- These should already be set correctly from v0.3 stream() impl,
-- but verify and fix if not:
BEGIN;
UPDATE events SET aggregate_type = 'schedule'
 WHERE event_type LIKE 'schedule:%' AND aggregate_type != 'schedule';
UPDATE events SET aggregate_type = 'scrape_schedule'
 WHERE event_type LIKE 'scrape_schedule:%' AND aggregate_type != 'scrape_schedule';
COMMIT;
```

After Stage 3, `aggregate_type` should match the prefix portion of
`event_type` for every event (since v0.4 forces this alignment).

## Stage 4 — Verify

Run all verification queries; all should return expected results.

```sql
-- 4.1 No events remain in old SchedulingEvent format:
SELECT COUNT(*) FROM events
 WHERE event_type LIKE 'scheduling:%';
-- Expected: 0

-- 4.2 No Lifecycle/Scrape events still under scout_run:
SELECT COUNT(*) FROM events
 WHERE aggregate_type = 'scout_run'
   AND event_type NOT LIKE 'scout_run:%';
-- Expected: 0 (no scout_run-prefixed events exist either, since
-- they were all lifecycle: or scrape: prefixed)

-- 4.3 Every event's aggregate_type matches its event_type prefix:
SELECT
  aggregate_type,
  split_part(event_type, ':', 1) AS expected_aggregate_type,
  COUNT(*)
FROM events
GROUP BY aggregate_type, split_part(event_type, ':', 1)
HAVING aggregate_type IS DISTINCT FROM split_part(event_type, ':', 1)
ORDER BY 3 DESC;
-- Expected: empty result (no rows with mismatched aggregate_type
-- and event_type prefix).

-- 4.4 Distinct (aggregate_type, event_type) pairs match v0.4 expected set:
SELECT DISTINCT aggregate_type, event_type
FROM events
ORDER BY aggregate_type, event_type;
-- Review against the mapping table. Should be:
--   ('discovery', 'discovery:*')
--   ('lifecycle', 'lifecycle:*')
--   ('schedule', 'schedule:*')
--   ('scrape', 'scrape:*')
--   ('scrape_schedule', 'scrape_schedule:*')
--   ('telemetry', 'telemetry:*')
--   ... etc for the TBD-row Facts once filled in.

-- 4.5 Cursor positions are unchanged:
SELECT consumer_id, position FROM causal_checkpoint;
-- Compare against a snapshot taken before Stage 2. Positions must
-- match exactly (cursors don't move during a halted-consumer migration).
```

If any verification fails: **DO NOT PROCEED to Stage 5**. Roll back
(below) and investigate.

## Stage 5 — Deploy v0.4 code

After all Stage 4 verifications pass:

```bash
# Deploy the v0.4 rootsignal-scout binary.
# This binary reads the new event_type/aggregate_type format
# and writes new emits in v0.4 format.
ansible-playbook deploy.yml --extra-vars "version=0.4.0"

# Wait for rolling deploy to complete.
```

**Past this point, rollback is not safe** — new events will be
written in v0.4 format, which v0.3 code can't read.

## Stage 6 — Restart consumers

```bash
systemctl start rootsignal-scout
systemctl start rootsignal-api

# Verify consumers come up cleanly:
journalctl -u rootsignal-scout -f --since "1 minute ago"
# Expected: consumers resume from their persisted cursors with no
# replay (cursors unchanged from pre-migration).
```

**Post-deploy monitoring:**

- Cursor advance rate per consumer should match pre-migration rate
- No new entries in `causal_projection_failures` table beyond
  baseline rate
- Sample 10 random recent events; verify `event_type` matches v0.4
  format (`{CATEGORY}:{name}`)

## Rollback (pre-Stage-5 only)

If verification fails OR v0.4 code has a known-blocking bug,
rollback BEFORE Stage 5 deploys.

```sql
-- Reverse Stage 3 (aggregate_type) first:
BEGIN;
UPDATE events SET aggregate_type = 'scout_run'
 WHERE event_type LIKE 'lifecycle:%';
UPDATE events SET aggregate_type = 'scout_run'
 WHERE event_type LIKE 'scrape:%';
COMMIT;

-- Reverse Stage 2 (event_type):
BEGIN;
UPDATE events
   SET event_type = REPLACE(event_type, 'schedule:', 'scheduling:schedule_')
 WHERE event_type LIKE 'schedule:%';
UPDATE events
   SET event_type = 'scheduling:scrape_scheduled'
 WHERE event_type = 'scrape_schedule:scheduled';
COMMIT;

-- Verify rollback:
SELECT DISTINCT event_type, aggregate_type FROM events
WHERE event_type LIKE 'schedule%' OR event_type LIKE 'scheduling:%';
-- Expected: only old-format entries remain.
```

Then restart v0.3 consumers (Stage 6 with v0.3 binaries).

**Rollback is impossible after Stage 5** because new events
written by v0.4 code use the new format; reverting them requires a
manual case-by-case rewrite that defeats the point.

## Ephemeral-event domains — v0.4 conversion (P12)

**Decision (P1.5, 2026-05-11): drop ephemerals entirely.** v0.4 has
no `.ephemeral()` mode; the Fact trait requires `stream_id()`. The
four domains currently emitting ephemeral events become persistent
Facts during P12.

The audit found **4 ephemeral event types** in
`modules/rootsignal-scout/src/domains/`:

| Domain              | Type                    | Today (v0.3)                                                | Under v0.4                                                            |
|---------------------|-------------------------|-------------------------------------------------------------|-----------------------------------------------------------------------|
| `coalescing`        | `CoalescingEvent`       | `#[causal::event(prefix = "coalescing", ephemeral)]`        | `CATEGORY = "coalescing"`, all variants gain `run_id: Uuid`           |
| `supervisor`        | `SupervisorEvent`       | `#[causal::event(prefix = "supervisor", ephemeral)]`        | `CATEGORY = "supervisor"`, all variants gain `run_id: Uuid`           |
| `cluster_weaving`   | `ClusterWeavingEvent`   | `#[causal::event(prefix = "cluster_weaving", ephemeral)]`   | `CATEGORY = "cluster_weaving"`, all variants gain `run_id: Uuid`      |
| `situation_weaving` | `SituationWeavingEvent` | `#[causal::event(prefix = "situation_weaving", ephemeral)]` | `CATEGORY = "situation_weaving"`, all variants gain `run_id: Uuid`    |

**Per-domain mechanical rewrite recipe (P12):**

1. Add `run_id: Uuid` to every variant that doesn't already have it
   (`SupervisorEvent::SupervisionCompleted` is the most affected —
   it's currently a unit variant).
2. Drop `, ephemeral` from the `#[causal::event(...)]` macro args.
3. Update `stream_id()` to return `run_id`.
4. Update the emitters in each `mod.rs` (`coalescing/mod.rs`,
   `supervisor/mod.rs`, etc.) to pass `run_id` from
   `ctx.deps().run_id` or `ctx.aggregate::<PipelineState>()`.
5. Update reactor filters that match on these variants — the match
   patterns now bind `run_id, ..` instead of nothing.

**Storage impact.** These four domains emit at most ~4-8 stage
signals per region per run plus per-group `GroupFeedCompleted`
events. At rootsignal launch scale this is well under 1% of total
log volume. The trade-off: extra log rows for a complete pipeline
timeline that's queryable post-hoc.

**No SQL needed.** Since these events were ephemeral in v0.3, they
were never persisted — there are no v0.3 rows in the log to rewrite.
The conversion is purely a source-code change in rootsignal-scout,
applied during P12 alongside the other domain refactors.

## Cursor continuity test plan

A test fixture lives in `causal-rs/modules/causal/tests/migration_continuity.rs`
(to be added during P0 execution). The fixture:

1. Seeds a MemoryStore with v0.3-format events
2. Runs the migration SQL against an equivalent in-memory
   representation
3. Constructs a v0.4 consumer with the same GROUP_NAME and
   starting cursor that v0.3 had recorded
4. Asserts: consumer reads events starting at the recorded position
   and successfully deserializes them as v0.4 Facts
5. Asserts: cursor advances monotonically, no skip, no duplicate
   delivery

This fixture serves as both a pre-deploy check (proves the
migration semantics) and a regression test (catches future shape
changes that break continuity).

## Estimated execution time

For a rootsignal-scale event log (~1M events at launch):

| Stage | Duration |
|---|---|
| Stage 1 (stop consumers) | 1-2 min |
| Stage 2 (event_type UPDATE) | 5-15 min (depends on indexed columns) |
| Stage 3 (aggregate_type UPDATE) | 5-15 min |
| Stage 4 (verification queries) | 1-2 min |
| Stage 5 (deploy v0.4) | 5-10 min (rolling) |
| Stage 6 (restart + verify) | 5-10 min |
| **Total** | **~30-60 min downtime window** |

Pre-launch rootsignal has zero production traffic, so the window
can be larger; post-launch, schedule maintenance accordingly.

## Open questions during execution

1. **TBD rows in the mapping table.** Decide CATEGORY for the 10
   legacy-Event-only Facts during P12 implementation. Default rule:
   keep current `type_prefix` as `CATEGORY` unless variants have
   diverging streams.

2. **Cursor continuity test fixture.** Implement during P0 or
   defer to P12? Recommend P0 since it validates the runbook
   itself, not the consumer migration.

3. **Test database snapshot.** Should the migration first SELECT
   pre-migration row counts per (aggregate_type, event_type) and
   compare post-migration? Defensive but slow on large tables.
   Recommend yes for the launch migration; optional for ongoing.
