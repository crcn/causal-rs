//! Postgres NOTIFY/LISTEN-based event broadcast.
//!
//! Listens for `NOTIFY events` from Postgres and fans out [`StoredEvent`]s
//! to all subscribers via a `tokio::sync::broadcast` channel.

use sqlx::PgPool;
use tokio::sync::broadcast;
use tracing::{info, warn};

use crate::read_model::StoredEvent;

/// Shared handle to the event broadcast channel.
/// Each subscriber (GraphQL subscription, cache listener) receives events
/// by calling `subscribe()`.
#[derive(Clone)]
pub struct EventBroadcast {
    sender: broadcast::Sender<StoredEvent>,
}

impl EventBroadcast {
    /// Create the broadcast channel and spawn a background task that
    /// listens to Postgres `NOTIFY events` and fans out `StoredEvent`s.
    pub fn spawn(pool: PgPool) -> Self {
        let (sender, _) = broadcast::channel::<StoredEvent>(1024);
        let tx = sender.clone();

        tokio::spawn(async move {
            loop {
                match run_listener(&pool, &tx).await {
                    Ok(()) => info!("PgListener disconnected cleanly, reconnecting..."),
                    Err(e) => warn!(error = %e, "PgListener error, reconnecting in 2s..."),
                }
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        });

        info!("EventBroadcast spawned — listening on pg_notify('events')");
        Self { sender }
    }

    /// Get a new receiver for live events.
    pub fn subscribe(&self) -> broadcast::Receiver<StoredEvent> {
        self.sender.subscribe()
    }

    /// Get the sender (for injecting into GraphQL context).
    pub fn sender(&self) -> &broadcast::Sender<StoredEvent> {
        &self.sender
    }
}

async fn run_listener(
    pool: &PgPool,
    tx: &broadcast::Sender<StoredEvent>,
) -> Result<(), sqlx::Error> {
    let mut listener = sqlx::postgres::PgListener::connect_with(pool).await?;
    listener.listen("events").await?;

    loop {
        let notification = listener.recv().await?;
        let payload = notification.payload();

        let seq: i64 = match payload.parse() {
            Ok(s) => s,
            Err(_) => {
                warn!(payload, "Non-integer payload on events channel, skipping");
                continue;
            }
        };

        match crate::queries::get_event_by_seq(pool, seq).await {
            Ok(Some(event)) => {
                let _ = tx.send(event);
            }
            Ok(None) => {
                warn!(seq, "Event seq from notification not found in DB");
            }
            Err(e) => {
                warn!(seq, error = %e, "Failed to fetch event by seq");
            }
        }
    }
}
