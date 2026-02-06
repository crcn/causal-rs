//! Batch Processing Example (stateless)

use anyhow::Result;
use seesaw_core::{effect, Context, Engine};
use seesaw_memory::MemoryStore;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
enum ImportEvent {
    FileUploaded {
        file_id: Uuid,
        file_path: String,
        expected_rows: usize,
    },
    RowParsed {
        file_id: Uuid,
        row_id: Uuid,
        data: String,
    },
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
    BatchInserted {
        file_id: Uuid,
        count: usize,
    },
    ImportCompleted {
        file_id: Uuid,
        total_rows: usize,
        valid_rows: usize,
        rejected_rows: usize,
    },
}

#[derive(Default)]
struct ImportProgress {
    expected_rows: usize,
    validated_rows: usize,
    rejected_rows: usize,
}

// ⚠️ WARNING: Arc<Mutex> is ONLY safe for single-worker deployments!
// For distributed/multi-worker setups, use:
// - External storage (Postgres, Redis) for progress tracking
// - Event-threaded state (store progress in event fields)
// See docs/DISTRIBUTED-SAFETY.md for details
#[derive(Clone, Default)]
struct Deps {
    progress: Arc<Mutex<HashMap<Uuid, ImportProgress>>>,
}

impl Deps {
    async fn parse_csv(&self, path: &str) -> Result<Vec<String>> {
        println!("parsing CSV: {}", path);
        let rows = (0..1000)
            .map(|i| format!("Row {}: data,data,data", i))
            .collect();
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        Ok(rows)
    }

    async fn validate_row(&self, data: &str) -> Result<()> {
        if data.contains("999") {
            anyhow::bail!("row 999 is invalid")
        }
        Ok(())
    }

    async fn bulk_insert(&self, rows: &[String]) -> Result<()> {
        println!("bulk inserting {} rows", rows.len());
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        Ok(())
    }

    fn start_import(&self, file_id: Uuid, expected_rows: usize) {
        let mut guard = self.progress.lock().expect("progress lock poisoned");
        guard.insert(
            file_id,
            ImportProgress {
                expected_rows,
                ..Default::default()
            },
        );
    }

    fn mark_progress(&self, file_id: Uuid, validated: bool) -> Option<(usize, usize, usize)> {
        let mut guard = self.progress.lock().expect("progress lock poisoned");
        let progress = guard.get_mut(&file_id)?;

        if validated {
            progress.validated_rows += 1;
        } else {
            progress.rejected_rows += 1;
        }

        let processed = progress.validated_rows + progress.rejected_rows;
        if processed >= progress.expected_rows {
            let completed = (
                progress.expected_rows,
                progress.validated_rows,
                progress.rejected_rows,
            );
            guard.remove(&file_id);
            Some(completed)
        } else {
            None
        }
    }
}

fn build_effects() -> Vec<seesaw_core::Handler<Deps>> {
    vec![
        handler::on::<ImportEvent>()
            .extract(|e| match e {
                ImportEvent::FileUploaded {
                    file_id,
                    file_path,
                    expected_rows,
                } => Some((*file_id, file_path.clone(), *expected_rows)),
                _ => None,
            })
            .then::<Deps, (Uuid, String, usize), _, _, Vec<ImportEvent>, ImportEvent>(
                |(file_id, file_path, expected_rows), ctx: Context<Deps>| async move {
                    ctx.deps().start_import(file_id, expected_rows);
                    let rows = ctx.deps().parse_csv(&file_path).await?;

                    let events: Vec<_> = rows
                        .into_iter()
                        .map(|data| ImportEvent::RowParsed {
                            file_id,
                            row_id: Uuid::new_v4(),
                            data,
                        })
                        .collect();

                    Ok(events)
                },
            ),
        handler::on::<ImportEvent>()
            .extract(|e| match e {
                ImportEvent::RowParsed {
                    file_id,
                    row_id,
                    data,
                } => Some((*file_id, *row_id, data.clone())),
                _ => None,
            })
            .then(|(file_id, row_id, data), ctx: Context<Deps>| async move {
                match ctx.deps().validate_row(&data).await {
                    Ok(()) => Ok(ImportEvent::RowValidated {
                        file_id,
                        row_id,
                        data,
                    }),
                    Err(error) => Ok(ImportEvent::RowRejected {
                        file_id,
                        row_id,
                        reason: error.to_string(),
                    }),
                }
            }),
        handler::on::<ImportEvent>()
            .join()
            .same_batch()
            .then::<Deps, _, _, Vec<ImportEvent>, ImportEvent>(
                |batch: Vec<ImportEvent>, ctx: Context<Deps>| async move {
                    let mut rows = Vec::new();
                    let mut file_id = None;

                    for event in batch {
                        if let ImportEvent::RowValidated {
                            file_id: fid, data, ..
                        } = event
                        {
                            if file_id.is_none() {
                                file_id = Some(fid);
                            }
                            rows.push(data);
                        }
                    }

                    if rows.is_empty() {
                        return Ok(Vec::<ImportEvent>::new());
                    }

                    let file_id = file_id.expect("validated rows must have file id");
                    let count = rows.len();
                    ctx.deps().bulk_insert(&rows).await?;

                    Ok(vec![ImportEvent::BatchInserted { file_id, count }])
                },
            ),
        handler::on::<ImportEvent>()
            .extract(|e| match e {
                ImportEvent::RowValidated { file_id, .. } => Some((*file_id, true)),
                ImportEvent::RowRejected { file_id, .. } => Some((*file_id, false)),
                _ => None,
            })
            .then::<Deps, (Uuid, bool), _, _, Vec<ImportEvent>, ImportEvent>(
                |(file_id, validated), ctx: Context<Deps>| async move {
                    if let Some((total, valid, rejected)) =
                        ctx.deps().mark_progress(file_id, validated)
                    {
                        Ok(vec![ImportEvent::ImportCompleted {
                            file_id,
                            total_rows: total,
                            valid_rows: valid,
                            rejected_rows: rejected,
                        }])
                    } else {
                        Ok(Vec::<ImportEvent>::new())
                    }
                },
            ),
    ]
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    let store = MemoryStore::new();
    let deps = Deps::default();

    let engine = Engine::new(deps, store).with_handlers(build_effects());

    let file_id = Uuid::new_v4();
    engine
        .dispatch(ImportEvent::FileUploaded {
            file_id,
            file_path: "/data/import.csv".to_string(),
            expected_rows: 1000,
        })
        .await?;

    Ok(())
}
