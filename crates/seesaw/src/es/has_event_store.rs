use super::event_store::EventStore;

/// Deps must implement this for transition guards to access the event store.
pub trait HasEventStore {
    type Store: EventStore;
    fn event_store(&self) -> &Self::Store;
}
