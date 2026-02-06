//! Seesaw Insight server binary

use anyhow::Result;
use seesaw_postgres::PostgresStore;
use sqlx::postgres::PgPoolOptions;
use std::env;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_target(false)
        .with_level(true)
        .init();

    // Get database URL from environment
    let database_url =
        env::var("DATABASE_URL").unwrap_or_else(|_| "postgres://localhost/seesaw".to_string());

    // Connect to database
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&database_url)
        .await?;

    tracing::info!("Connected to database");

    // Create PostgresStore
    let store = PostgresStore::new(pool);

    // Get bind address from environment
    let addr = env::var("BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:3000".to_string());

    // Determine static directory
    let static_dir = env::var("STATIC_DIR").ok().or_else(|| {
        // Try to find static directory relative to binary
        let exe = env::current_exe().ok()?;
        let exe_dir = exe.parent()?;
        let static_path = exe_dir.join("../../../crates/seesaw-insight/static");
        if static_path.exists() {
            Some(static_path.to_string_lossy().to_string())
        } else {
            None
        }
    });

    if let Some(ref dir) = static_dir {
        tracing::info!("Serving static files from: {}", dir);
    } else {
        tracing::warn!("No static directory found - dashboard will not be available");
    }

    // Start server
    seesaw_insight::serve(store, &addr, static_dir.as_deref()).await?;

    Ok(())
}
