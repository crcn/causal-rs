//! Seesaw Insight - Real-time observability for Seesaw workflows
//!
//! This crate provides real-time visualization and monitoring for Seesaw workflows
//! through WebSockets, Server-Sent Events (SSE), and a web dashboard.
//!
//! # Architecture
//!
//! - `stream` module: Cursor-based reader for `seesaw_stream` table
//! - `tree` module: Workflow causality tree builder
//! - `websocket` module: WebSocket support for real-time updates
//! - `web` module: Axum server with SSE/WS endpoints and static file serving
//!
//! # Usage
//!
//! ```rust,no_run
//! use seesaw_insight::web;
//! use seesaw_postgres::PostgresStore;
//! use sqlx::postgres::PgPoolOptions;
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let pool = PgPoolOptions::new()
//!         .max_connections(5)
//!         .connect("postgres://localhost/seesaw")
//!         .await?;
//!
//!     let store = PostgresStore::new(pool);
//!     web::serve(store, "127.0.0.1:3000", Some("./static")).await?;
//!     Ok(())
//! }
//! ```

pub mod stream;
pub mod tree;
pub mod web;
pub mod websocket;

pub use stream::{StreamEntry, StreamReader};
pub use tree::{EventNode, TreeBuilder};
pub use web::{app, serve};
