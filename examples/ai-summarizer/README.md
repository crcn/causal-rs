# ai-summarizer

A `SummarizeReactor` calls the Anthropic API with `reqwest + serde` and
emits `Summarized` / `SummaryFailed` events. Minimal backend: KurrentDB
for the event log, `MemoryStore` for outbox + checkpoints
(single-process, ephemeral — fine for examples, swap to
`PgReactorOutbox` for production).

## Run

```sh
docker compose up -d                      # KurrentDB on :2113
export ANTHROPIC_API_KEY=your-key
cargo run
```

Environment overrides:

- `KURRENT_URL` — default `esdb://localhost:2113?tls=false`
- `ANTHROPIC_API_KEY` — required
