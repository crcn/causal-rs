# H10 — PG ordering-lock has no deterministic regression guard (LOW, test-only)

**Status:** open. **Decided direction:** deterministic two-connection PG test.
Test-only — no production change. Needs a live Postgres to author/validate.

## Finding
The Postgres catch-up-subscription ordering black swan (out-of-order-commit
skip) is correctly mitigated by a transaction-scoped global advisory lock
`pg_advisory_xact_lock(0xCA05, 0xA1)` taken by both and only the two
`causal_log` writers before position assignment, held to commit
(`event_log.rs:25-51,212`, `event_projector.rs:92`). This is **solid** — not a
correctness gap.

The only weakness is *test coverage of the mitigation*: it is guarded by a
**scheduling-jitter** stress test (`conformance::concurrent_appends_are_tailable_without_loss`,
8 appenders / 200 events), not a **deterministic** adversarial test that would
fail if the lock were removed. So a future refactor that dropped or weakened the
lock might not be caught.

## Recommended fix (test-only)
Add a raw two-connection SQL test that forces the worst-case interleaving the
lock prevents:
1. Conn A: `BEGIN`; take `pg_advisory_xact_lock(0xCA05,0xA1)`; `INSERT … RETURNING position` → N. Leave open.
2. Conn B: attempt the same append → **blocks** on the advisory lock (cannot get N+1 until A commits).
3. With the lock present, a tailer can never observe N+1 before N — the skip is structurally impossible.
4. A negative variant (lock omitted) demonstrates the skip, proving the lock is load-bearing.

Mark `#[ignore]` like the other PG conformance tests (`requires local
DATABASE_URL`). Validate against a live PG before relying on it — it could not be
run in the environment where this gap was filed.

## Evidence
- Mitigation: `event_log.rs:25-51` (doc), `:212` (lock in append), `event_projector.rs:92` (mirror path).
- Existing (jitter) guard: `conformance.rs` `concurrent_appends_are_tailable_without_loss`.
