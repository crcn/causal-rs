use std::any::TypeId;

use anyhow::Result;
use seesaw_core::{effect, effects, reducer, reducers, EffectContext, Emit, ErrorContext};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Default)]
struct TestState {
    count: u32,
}

#[derive(Clone)]
struct Deps;

#[derive(Clone, Serialize, Deserialize)]
struct OrderPlaced {
    order_id: Uuid,
}

#[derive(Clone, Serialize, Deserialize)]
struct OrderShipped {
    order_id: Uuid,
}

#[derive(Clone, Serialize, Deserialize)]
struct PaymentRequested {
    order_id: Uuid,
}

#[derive(Clone, Serialize, Deserialize)]
struct PaymentCharged {
    order_id: Uuid,
    status: String,
    error: Option<String>,
    attempts: i32,
}

#[derive(Clone, Serialize, Deserialize)]
struct RowValidated {
    row_id: Uuid,
}

#[derive(Clone, Serialize, Deserialize)]
struct BatchInserted {
    count: usize,
}

#[derive(Clone, Serialize, Deserialize)]
enum CrawlEvent {
    Ingested { website_id: Uuid, job_id: Uuid },
    Regenerated { website_id: Uuid, job_id: Uuid },
}

#[derive(Clone, Serialize, Deserialize)]
struct ExtractEnqueued {
    website_id: Uuid,
}

#[derive(Clone, Serialize, Deserialize)]
struct AnalyticsEvent;

#[effects]
mod order_effects {
    use super::*;

    #[effect(on = OrderPlaced, id = "ship_order")]
    async fn ship_order(
        event: OrderPlaced,
        _ctx: EffectContext<TestState, Deps>,
    ) -> Result<OrderShipped> {
        Ok(OrderShipped {
            order_id: event.order_id,
        })
    }

    #[effect(
        on = PaymentRequested,
        id = "charge_payment",
        retry = 3,
        timeout_secs = 30,
        priority = 1
    )]
    async fn charge_payment(
        event: PaymentRequested,
        _ctx: EffectContext<TestState, Deps>,
    ) -> Result<PaymentCharged> {
        Ok(PaymentCharged {
            order_id: event.order_id,
            status: "ok".to_string(),
            error: None,
            attempts: 0,
        })
    }

    #[effect(
        on = PaymentRequested,
        id = "run_search",
        retry = 3,
        dlq_terminal = build_payment_failure
    )]
    async fn run_search(
        event: PaymentRequested,
        _ctx: EffectContext<TestState, Deps>,
    ) -> Result<PaymentCharged> {
        Ok(PaymentCharged {
            order_id: event.order_id,
            status: "ok".to_string(),
            error: None,
            attempts: 0,
        })
    }

    fn build_payment_failure(input: PaymentRequested, err: ErrorContext) -> PaymentCharged {
        PaymentCharged {
            order_id: input.order_id,
            status: "failed".to_string(),
            error: Some(err.error),
            attempts: err.attempts,
        }
    }

    #[effect(on = OrderPlaced, queued, id = "queued_observer")]
    async fn queued_observer(
        _event: OrderPlaced,
        _ctx: EffectContext<TestState, Deps>,
    ) -> Result<Emit<AnalyticsEvent>> {
        Ok(Emit::None)
    }

    #[effect(
        on = PaymentRequested,
        queued,
        retry = 1,
        id = "queued_retry_one"
    )]
    async fn queued_retry_one(
        event: PaymentRequested,
        _ctx: EffectContext<TestState, Deps>,
    ) -> Result<PaymentCharged> {
        Ok(PaymentCharged {
            order_id: event.order_id,
            status: "ok".to_string(),
            error: None,
            attempts: 0,
        })
    }

    #[effect(on = RowValidated, join, id = "bulk_insert")]
    async fn bulk_insert(
        batch: Vec<RowValidated>,
        _ctx: EffectContext<TestState, Deps>,
    ) -> Result<BatchInserted> {
        Ok(BatchInserted { count: batch.len() })
    }

    #[effect(
        on = [CrawlEvent::Ingested, CrawlEvent::Regenerated],
        extract(website_id, job_id)
    )]
    async fn enqueue_extract(
        website_id: Uuid,
        job_id: Uuid,
        _ctx: EffectContext<TestState, Deps>,
    ) -> Result<ExtractEnqueued> {
        let _ = job_id;
        Ok(ExtractEnqueued { website_id })
    }

    #[effect(on = OrderPlaced, group = "analytics")]
    async fn log_order(
        _event: OrderPlaced,
        _ctx: EffectContext<TestState, Deps>,
    ) -> Result<Emit<AnalyticsEvent>> {
        Ok(Emit::None)
    }
}

#[reducers]
mod state_reducers {
    use super::*;

    #[reducer(on = OrderPlaced)]
    fn order_placed(state: TestState, _event: OrderPlaced) -> TestState {
        TestState {
            count: state.count + 1,
        }
    }

    #[reducer(
        on = [CrawlEvent::Ingested, CrawlEvent::Regenerated],
        extract(website_id, job_id)
    )]
    fn extraction_seen(state: TestState, website_id: Uuid, job_id: Uuid) -> TestState {
        let _ = (website_id, job_id);
        TestState {
            count: state.count + 1,
        }
    }
}

#[test]
fn effects_module_registration_works() {
    let effects = order_effects::effects();
    assert_eq!(effects.len(), 8);
    assert!(effects
        .iter()
        .any(|effect| effect.can_handle(TypeId::of::<OrderPlaced>())));
    assert!(effects
        .iter()
        .any(|effect| effect.can_handle(TypeId::of::<PaymentRequested>())));
    assert!(effects
        .iter()
        .any(|effect| effect.id == "analytics::log_order"));

    let ship_order = effects
        .iter()
        .find(|effect| effect.id == "ship_order")
        .expect("ship_order effect should exist");
    assert!(
        ship_order.is_inline(),
        "default effect should remain inline"
    );

    let queued_observer = effects
        .iter()
        .find(|effect| effect.id == "queued_observer")
        .expect("queued_observer effect should exist");
    assert!(
        !queued_observer.is_inline(),
        "queued attribute should force queued execution"
    );

    let queued_retry_one = effects
        .iter()
        .find(|effect| effect.id == "queued_retry_one")
        .expect("queued_retry_one effect should exist");
    assert!(
        !queued_retry_one.is_inline(),
        "queued attribute should force queued execution even when retry = 1"
    );
}

#[test]
fn reducers_module_registration_works() {
    let reducers = state_reducers::reducers();
    assert_eq!(reducers.len(), 2);
}
