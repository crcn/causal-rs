use std::marker::PhantomData;

use anyhow::Result;
use async_trait::async_trait;
use uuid::Uuid;

use super::aggregate::Aggregate;
use super::types::{NewEvent, StoredEvent, Versioned};
use super::upcast::EventUpcast;

/// Permanent event storage with optimistic concurrency.
#[async_trait]
pub trait EventStore: Send + Sync + 'static {
    /// Load events for an aggregate starting after `from_version`.
    /// Pass 0 to load all events.
    async fn load_events(
        &self,
        aggregate_id: Uuid,
        from_version: u64,
    ) -> Result<Vec<StoredEvent>>;

    /// Append events with optimistic concurrency check.
    /// Returns the new version (sequence of the last appended event).
    async fn append(
        &self,
        aggregate_id: Uuid,
        aggregate_type: &str,
        expected_version: u64,
        events: Vec<NewEvent>,
    ) -> Result<u64>;

    /// Check if an aggregate exists (has any events).
    async fn exists(&self, aggregate_id: Uuid) -> Result<bool>;
}

/// Extension trait providing ergonomic aggregate operations.
pub trait EventStoreExt: EventStore {
    fn aggregate<A: Aggregate>(&self, id: impl Into<Uuid>) -> AggregateLoader<'_, Self, A>;
}

impl<T: EventStore> EventStoreExt for T {
    fn aggregate<A: Aggregate>(&self, id: impl Into<Uuid>) -> AggregateLoader<'_, Self, A> {
        AggregateLoader {
            store: self,
            aggregate_id: id.into(),
            _phantom: PhantomData,
        }
    }
}

/// Ergonomic loader/appender for a specific aggregate instance.
pub struct AggregateLoader<'a, S: EventStore + ?Sized, A: Aggregate> {
    store: &'a S,
    aggregate_id: Uuid,
    _phantom: PhantomData<A>,
}

impl<'a, S: EventStore + ?Sized, A: Aggregate> AggregateLoader<'a, S, A> {
    /// Load aggregate state by replaying all events.
    pub async fn load(&self) -> Result<Versioned<A>> {
        self.load_at_version(u64::MAX).await
    }

    /// Load aggregate state up to (and including) a specific version.
    pub async fn load_at_version(&self, up_to_version: u64) -> Result<Versioned<A>> {
        let events = self.store.load_events(self.aggregate_id, 0).await?;
        let mut state = A::default();
        let mut version = 0u64;

        for stored in events {
            if stored.sequence > up_to_version {
                break;
            }
            let event = A::Event::upcast_from(stored.schema_version, stored.data)?;
            state.apply(event);
            version = stored.sequence;
        }

        Ok(Versioned::new(state, version))
    }

    /// Append events with optimistic concurrency check.
    pub async fn append(
        &self,
        expected_version: u64,
        events: Vec<A::Event>,
    ) -> Result<u64> {
        let new_events = events
            .iter()
            .map(NewEvent::from_event)
            .collect::<Result<Vec<_>>>()?;

        self.store
            .append(
                self.aggregate_id,
                A::aggregate_type(),
                expected_version,
                new_events,
            )
            .await
    }

    /// Append events with a causal link to a causing event.
    pub async fn append_caused_by(
        &self,
        expected_version: u64,
        caused_by: Uuid,
        events: Vec<A::Event>,
    ) -> Result<u64> {
        let new_events = events
            .iter()
            .map(|e| Ok(NewEvent::from_event(e)?.caused_by(caused_by)))
            .collect::<Result<Vec<_>>>()?;

        self.store
            .append(
                self.aggregate_id,
                A::aggregate_type(),
                expected_version,
                new_events,
            )
            .await
    }
}
