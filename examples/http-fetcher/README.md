# http-fetcher

Fans out HTTP fetches as `FetchRequested` events; a `FetchReactor` calls
`reqwest` and emits `Fetched` / `FetchFailed` per request. Production-
shape backend: KurrentDB for the event log, `PgReactorOutbox` for the
outbox + checkpoints.

## Run

```sh
docker compose up -d   # KurrentDB on :2113, Postgres on :54320
cargo run
```

Environment overrides:

- `KURRENT_URL` — default `esdb://localhost:2113?tls=false`
- `DATABASE_URL` — default `postgres://causal:causal@localhost:54320/causal`

Stop the stack with `docker compose down -v` (the `-v` clears volumes
so the next run starts fresh).
