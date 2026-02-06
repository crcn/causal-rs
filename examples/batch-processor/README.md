# Batch Processing Example

This example demonstrates Seesaw's event batching capabilities:

1. **Batch Emission** - Emit multiple events atomically with `Emit::Batch`
2. **Join Pattern** - Accumulate batched events for bulk operations
3. **Error Handling** - Handle per-item failures gracefully

## Scenario

Import a CSV file with 1000 rows:
- Parse file → emit batch of 1000 `RowParsed` events
- Validate each row individually
- Join validated rows → bulk insert to database
- Track progress and handle failures

## Key Concepts

### Emit::Batch
```rust
// Emit 1000 events atomically (same batch_id)
let events: Vec<_> = rows.into_iter()
    .map(|row| RowParsed { row })
    .collect();
Ok(Emit::Batch(events))
```

### Join Pattern
```rust
// Accumulate all RowParsed events from same batch
effect::on::<RowParsed>()
    .join()
    .then(|batch: Vec<RowParsed>, ctx| async move {
        // Handler receives Vec<Event>, fires once per batch
        ctx.deps().bulk_insert(&batch).await?;
        Ok(Emit::One(BatchInserted { count: batch.len() }))
    })
```

## Running

```bash
cargo run --example batch-processor
```

## Expected Output

```
🎯 Batch Processing Example

This example demonstrates:
  1. Emitting batches with Emit::Batch
  2. Joining batched events for bulk operations
  3. Error handling in batch workflows

📂 Starting import for file <uuid>

🚀 Effect 1: Parsing file <uuid>
📄 Parsing CSV: /data/import.csv
✅ Parsed 1000 rows
📤 Emitting batch of 1000 RowParsed events

🔄 Effect 3: Join handler called with 999 rows
💾 Bulk inserting 999 rows...
✅ Inserted 999 rows

✨ Import complete!
   Total: 1000 | Valid: 999 | Rejected: 1

📊 Final State:
   Expected rows: 1000
   Parsed: 1000
   Validated: 999
   Rejected: 1
   Batches inserted: 1
```

## Flow Diagram

```
FileUploaded
    ↓
Parse Effect → Emit::Batch([Row1, Row2, ..., Row1000])
    ↓
1000 RowParsed events (same batch_id)
    ↓
Validate Effect (runs 1000 times)
    ↓
999 RowValidated + 1 RowRejected
    ↓
Join Effect → Accumulates 999 validated rows
    ↓
Bulk Insert (runs once)
    ↓
BatchInserted
    ↓
Check Completion → ImportCompleted
```

## Error Handling

Row 999 is configured to fail validation (simulated):
```rust
if data.contains("999") {
    anyhow::bail!("Row 999 is invalid (simulated failure)")
}
```

This demonstrates per-item error handling:
- Failed row emits `RowRejected` event
- Valid rows continue to `RowValidated`
- Join effect only processes validated rows
- Final state shows rejected count

## Performance

**Without batching:**
- 1000 separate `handle.process()` calls
- 1000 database transactions
- ~5 seconds for 1000 rows

**With batching:**
- 1 batch emission (1000 events, 1 transaction)
- 1 bulk insert (1 transaction)
- <1 second for 1000 rows

**50x improvement!**

## Adapting to Your Use Case

Replace mock implementations with real ones:

```rust
impl Deps {
    async fn parse_csv(&self, path: &str) -> Result<Vec<String>> {
        // Replace with actual CSV parsing
        let mut reader = csv::Reader::from_path(path)?;
        // ...
    }

    async fn bulk_insert(&self, rows: &[String]) -> Result<()> {
        // Replace with actual database insert
        sqlx::query("INSERT INTO rows (data) SELECT * FROM UNNEST($1)")
            .bind(rows)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
```

## See Also

- [CLAUDE.md - Event Batching](../../CLAUDE.md#event-batching)
- [Plan Document](../../docs/plans/2026-02-05-feat-event-batching-api-plan.md)
