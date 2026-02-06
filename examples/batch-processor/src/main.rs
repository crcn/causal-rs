//! Batch Processing Example
//!
//! Demonstrates:
//! 1. Emitting batches of events with Emit::Batch
//! 2. Joining batched events for bulk operations
//! 3. Error handling in batch workflows
//!
//! Scenario: Import CSV file with 1000s of rows, validate and bulk-insert to database

use anyhow::Result;
use seesaw_core::{effect, reducer, Emit, Engine};
use seesaw_memory::MemoryStore;
use uuid::Uuid;

// =============================================================================
// Domain Events
// =============================================================================

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
enum ImportEvent {
    /// User uploads CSV file - triggers batch parsing
    FileUploaded {
        file_id: Uuid,
        file_path: String,
        expected_rows: usize,
    },

    /// File parsed into rows - emitted as batch
    RowParsed {
        file_id: Uuid,
        row_id: Uuid,
        data: String,
    },

    /// Row validation result - emitted per row
    RowValidated {
        file_id: Uuid,
        row_id: Uuid,
        data: String,
    },
    RowRejected {
        file_id: Uuid,
        row_id: Uuid,
        reason: String,
    },

    /// Batch of validated rows inserted - emitted after join
    BatchInserted {
        file_id: Uuid,
        count: usize,
    },

    /// Import complete - terminal event
    ImportCompleted {
        file_id: Uuid,
        total_rows: usize,
        valid_rows: usize,
        rejected_rows: usize,
    },
    ImportFailed {
        file_id: Uuid,
        reason: String,
    },
}

// =============================================================================
// State
// =============================================================================

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
struct ImportState {
    file_id: Option<Uuid>,
    expected_rows: usize,
    parsed_count: usize,
    validated_count: usize,
    rejected_count: usize,
    inserted_batches: usize,
}

// =============================================================================
// Dependencies
// =============================================================================

#[derive(Clone)]
struct Deps {
    // In real app: actual database connection pool
}

impl Deps {
    /// Simulate parsing CSV into rows
    async fn parse_csv(&self, path: &str) -> Result<Vec<String>> {
        println!("📄 Parsing CSV: {}", path);

        // Simulate large file
        let rows = (0..1000)
            .map(|i| format!("Row {}: data,data,data", i))
            .collect();

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        println!("✅ Parsed 1000 rows");

        Ok(rows)
    }

    /// Simulate row validation
    async fn validate_row(&self, data: &str) -> Result<()> {
        // Simulate validation logic
        if data.contains("999") {
            anyhow::bail!("Row 999 is invalid (simulated failure)")
        }
        Ok(())
    }

    /// Simulate bulk database insert
    async fn bulk_insert(&self, rows: &[String]) -> Result<()> {
        println!("💾 Bulk inserting {} rows...", rows.len());
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        println!("✅ Inserted {} rows", rows.len());
        Ok(())
    }
}

// =============================================================================
// Effects
// =============================================================================

fn build_effects() -> Vec<seesaw_core::Effect<ImportState, Deps>> {
    vec![
        // Effect 1: Parse CSV and emit batch of RowParsed events
        effect::on::<ImportEvent>()
            .extract(|e| match e {
                ImportEvent::FileUploaded {
                    file_id,
                    file_path,
                    ..
                } => Some((file_id.clone(), file_path.clone())),
                _ => None,
            })
            .then(|(file_id, file_path), ctx: seesaw_core::EffectContext<ImportState, Deps>| async move {
                println!("\n🚀 Effect 1: Parsing file {}", file_id);

                // Parse CSV into rows
                let rows = ctx.deps().parse_csv(&file_path).await?;

                // Emit batch of RowParsed events
                let events: Vec<_> = rows
                    .into_iter()
                    .map(|data| ImportEvent::RowParsed {
                        file_id,
                        row_id: Uuid::new_v4(),
                        data,
                    })
                    .collect();

                println!("📤 Emitting batch of {} RowParsed events", events.len());

                // This is the key: Emit::Batch emits all events atomically with same batch_id
                // Vec<E> automatically converts to Emit::Batch via From impl
                Ok(events)
            }),

        // Effect 2: Validate each row (runs once per RowParsed event)
        effect::on::<ImportEvent>()
            .extract(|e| match e {
                ImportEvent::RowParsed {
                    file_id,
                    row_id,
                    data,
                } => Some((file_id.clone(), row_id.clone(), data.clone())),
                _ => None,
            })
            .then(|(file_id, row_id, data), ctx: seesaw_core::EffectContext<ImportState, Deps>| async move {
                // Validate row
                match ctx.deps().validate_row(&data).await {
                    Ok(()) => {
                        // Valid row - emit success event
                        Ok(ImportEvent::RowValidated {
                            file_id,
                            row_id,
                            data,
                        })
                    }
                    Err(e) => {
                        // Invalid row - emit failure event
                        println!("❌ Row validation failed: {}", e);
                        Ok(ImportEvent::RowRejected {
                            file_id,
                            row_id,
                            reason: e.to_string(),
                        })
                    }
                }
            }),

        // Effect 3: Join validated rows and bulk insert
        // This is the key join pattern - accumulates all RowValidated events from same batch
        effect::on::<ImportEvent>()
            .join() // Enable batch accumulation
            .then::<ImportState, Deps, _, _, ImportEvent, ImportEvent>(|batch: Vec<ImportEvent>, ctx: seesaw_core::EffectContext<ImportState, Deps>| async move {
                println!("\n🔄 Effect 3: Join handler called with {} events", batch.len());

                // Filter to only RowValidated events and extract data
                let mut rows = Vec::new();
                let mut file_id = None;

                for event in batch {
                    if let ImportEvent::RowValidated { file_id: fid, data, .. } = event {
                        if file_id.is_none() {
                            file_id = Some(fid);
                        }
                        rows.push(data);
                    }
                }

                if rows.is_empty() {
                    return Ok(());
                }

                let file_id = file_id.expect("file_id should be set");

                // Bulk insert to database
                ctx.deps().bulk_insert(&rows).await?;

                // Emit success event
                Ok(ImportEvent::BatchInserted {
                    file_id,
                    count: rows.len(),
                })
            }),

        // Effect 4: Check if import is complete (terminal event)
        effect::on::<ImportEvent>()
            .extract(|e| match e {
                ImportEvent::BatchInserted { file_id, .. }
                | ImportEvent::RowRejected { file_id, .. } => Some(*file_id),
                _ => None,
            })
            .then(|file_id, ctx: seesaw_core::EffectContext<ImportState, Deps>| async move {
                let state = ctx.next_state();

                // Check if all rows processed
                let total_processed = state.validated_count + state.rejected_count;

                if total_processed >= state.expected_rows {
                    println!("\n✨ Import complete!");
                    println!(
                        "   Total: {} | Valid: {} | Rejected: {}",
                        state.expected_rows, state.validated_count, state.rejected_count
                    );

                    Ok(ImportEvent::ImportCompleted {
                        file_id,
                        total_rows: state.expected_rows,
                        valid_rows: state.validated_count,
                        rejected_rows: state.rejected_count,
                    })
                } else {
                    // Not complete yet
                    Ok(())
                }
            }),
    ]
}

// =============================================================================
// Reducers
// =============================================================================

fn build_reducers() -> Vec<seesaw_core::Reducer<ImportState>> {
    vec![
        reducer::fold::<ImportEvent>().into(|state, event| match event {
            ImportEvent::FileUploaded {
                file_id,
                expected_rows,
                ..
            } => ImportState {
                file_id: Some(*file_id),
                expected_rows: *expected_rows,
                ..state
            },
            ImportEvent::RowParsed { .. } => ImportState {
                parsed_count: state.parsed_count + 1,
                ..state
            },
            ImportEvent::RowValidated { .. } => ImportState {
                validated_count: state.validated_count + 1,
                ..state
            },
            ImportEvent::RowRejected { .. } => ImportState {
                rejected_count: state.rejected_count + 1,
                ..state
            },
            ImportEvent::BatchInserted { .. } => ImportState {
                inserted_batches: state.inserted_batches + 1,
                ..state
            },
            _ => state,
        }),
    ]
}

// =============================================================================
// Main
// =============================================================================

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    println!("🎯 Batch Processing Example\n");
    println!("This example demonstrates:");
    println!("  1. Emitting batches with Emit::Batch");
    println!("  2. Joining batched events for bulk operations");
    println!("  3. Error handling in batch workflows\n");

    // Create store and dependencies
    let store = MemoryStore::new();
    let deps = Deps {};

    // Build engine
    let mut engine = Engine::new(deps, store);

    for effect in build_effects() {
        engine = engine.with_effect(effect);
    }

    for reducer in build_reducers() {
        engine = engine.with_reducer(reducer);
    }

    // Trigger import
    let file_id = Uuid::new_v4();
    println!("📂 Starting import for file {}\n", file_id);

    engine.process(ImportEvent::FileUploaded {
        file_id,
        file_path: "/data/import.csv".to_string(),
        expected_rows: 1000,
    }).await?;

    // Wait for completion
    println!("\n⏳ Waiting for import to complete...\n");
    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

    // Note: Final state inspection would require access to the engine's internal state
    println!("\n✅ Import workflow initiated!");

    Ok(())
}
