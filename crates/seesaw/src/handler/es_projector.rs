//! ES projector handler — persists every event to an EventStore before
//! reactive handlers run.

use std::sync::Arc;
use uuid::Uuid;

use super::builders::on;
use super::context::Context;
use super::types::Handler;
use crate::es::{Aggregate, EventStoreExt, EventUpcast, HasEventStore};

/// Create an ES projector handler that persists events to an EventStore.
///
/// Runs as a priority-0 inline handler, ensuring events are stored before
/// any other handler sees them.
///
/// `extract_id` maps the event to the aggregate ID it belongs to.
///
/// ```ignore
/// use seesaw::handler::es_projector::es_projector;
///
/// engine.with_handler(es_projector::<OrderEvent, OrderAggregate, _, _>(|e| e.order_id))
/// ```
pub fn es_projector<E, A, D, F>(extract_id: F) -> Handler<D>
where
    E: Clone + Send + Sync + serde::Serialize + serde::de::DeserializeOwned + EventUpcast + 'static,
    A: Aggregate<Event = E>,
    D: HasEventStore + Send + Sync + 'static,
    F: Fn(&E) -> Uuid + Send + Sync + Clone + 'static,
{
    on::<E>()
        .id(format!("es::projector::{}", A::aggregate_type()))
        .priority(0)
        .then(move |event: Arc<E>, ctx: Context<D>| {
            let extract_id = extract_id.clone();
            async move {
                let aggregate_id = extract_id(&event);
                let es = ctx.deps().event_store();
                let current = es.aggregate::<A>(aggregate_id).load().await?;
                es.aggregate::<A>(aggregate_id)
                    .append_caused_by(current.version(), ctx.event_id, vec![(*event).clone()])
                    .await?;
                Ok(())
            }
        })
}
