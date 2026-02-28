use serde::{de::DeserializeOwned, Serialize};

use super::upcast::EventUpcast;

/// Domain aggregate whose state is reconstructed by replaying events.
pub trait Aggregate: Default + Send + Sync + 'static {
    /// The event type this aggregate processes.
    type Event: Clone + Serialize + DeserializeOwned + EventUpcast + Send + Sync + 'static;

    /// Unique string identifying this aggregate type (used in storage).
    fn aggregate_type() -> &'static str;

    /// Pure state transition — apply an event to mutate state. No side effects.
    fn apply(&mut self, event: Self::Event);
}
