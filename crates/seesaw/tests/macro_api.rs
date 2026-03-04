use std::any::TypeId;

use anyhow::Result;
use seesaw_core::{aggregator, aggregators, events, handle, handles, projection, AnyEvent, Context, Emit, ErrorContext, Events};
use seesaw_core::{Aggregate, Apply};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

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

#[derive(Clone, Serialize, Deserialize)]
struct HighValueOrder {
    order_id: Uuid,
    total: f64,
}

#[derive(Clone, Serialize, Deserialize)]
struct HighValueShipped {
    order_id: Uuid,
}

fn is_high_value(event: &HighValueOrder) -> bool {
    event.total > 500.0
}

#[handles]
mod order_effects {
    use super::*;

    #[handle(on = OrderPlaced, id = "ship_order")]
    async fn ship_order(event: OrderPlaced, _ctx: Context<Deps>) -> Result<OrderShipped> {
        Ok(OrderShipped {
            order_id: event.order_id,
        })
    }

    #[handle(
        on = PaymentRequested,
        id = "charge_payment",
        retry = 3,
        timeout_secs = 30,
        priority = 1
    )]
    async fn charge_payment(
        event: PaymentRequested,
        _ctx: Context<Deps>,
    ) -> Result<PaymentCharged> {
        Ok(PaymentCharged {
            order_id: event.order_id,
            status: "ok".to_string(),
            error: None,
            attempts: 0,
        })
    }

    #[handle(
        on = PaymentRequested,
        id = "run_search",
        retry = 3,
        dlq_terminal = build_payment_failure
    )]
    async fn run_search(
        event: PaymentRequested,
        _ctx: Context<Deps>,
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

    #[handle(on = OrderPlaced, queued, id = "bg_observer")]
    async fn bg_observer(
        _event: OrderPlaced,
        _ctx: Context<Deps>,
    ) -> Result<Emit<AnalyticsEvent>> {
        Ok(Emit::None)
    }

    #[handle(
        on = PaymentRequested,
        queued,
        retry = 1,
        id = "bg_retry_one"
    )]
    async fn bg_retry_one(
        event: PaymentRequested,
        _ctx: Context<Deps>,
    ) -> Result<PaymentCharged> {
        Ok(PaymentCharged {
            order_id: event.order_id,
            status: "ok".to_string(),
            error: None,
            attempts: 0,
        })
    }

    #[handle(
        on = RowValidated,
        accumulate,
        window_timeout_secs = 60,
        id = "bulk_insert"
    )]
    async fn bulk_insert(
        batch: Vec<RowValidated>,
        _ctx: Context<Deps>,
    ) -> Result<BatchInserted> {
        Ok(BatchInserted { count: batch.len() })
    }

    #[handle(
        on = [CrawlEvent::Ingested, CrawlEvent::Regenerated],
        extract(website_id, job_id)
    )]
    async fn enqueue_extract(
        website_id: Uuid,
        job_id: Uuid,
        _ctx: Context<Deps>,
    ) -> Result<ExtractEnqueued> {
        let _ = job_id;
        Ok(ExtractEnqueued { website_id })
    }

    #[handle(on = OrderPlaced, group = "analytics")]
    async fn log_order(
        _event: OrderPlaced,
        _ctx: Context<Deps>,
    ) -> Result<Emit<AnalyticsEvent>> {
        Ok(Emit::None)
    }

    #[handle(on = HighValueOrder, filter = is_high_value, id = "ship_high_value")]
    async fn ship_high_value(
        event: HighValueOrder,
        _ctx: Context<Deps>,
    ) -> Result<HighValueShipped> {
        Ok(HighValueShipped {
            order_id: event.order_id,
        })
    }

    #[projection(id = "order_read_model")]
    async fn update_read_model(
        _event: AnyEvent,
        _ctx: Context<Deps>,
    ) -> Result<()> {
        Ok(())
    }

    #[handle(on_any, id = "event_logger")]
    async fn log_all_events(
        _event: AnyEvent,
        _ctx: Context<Deps>,
    ) -> Result<()> {
        Ok(())
    }
}

// ── Aggregator macro tests ──────────────────────────────────────────────

#[derive(Debug, Default, PartialEq, Clone, Serialize, Deserialize)]
enum TestOrderStatus {
    #[default]
    Draft,
    Placed,
    Shipped,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct TestOrder {
    status: TestOrderStatus,
    total: f64,
}

impl Aggregate for TestOrder {
    fn aggregate_type() -> &'static str {
        "TestOrder"
    }
}

#[aggregators]
mod order_aggregators {
    use super::*;

    #[aggregator(id = "order_id")]
    fn on_placed(order: &mut TestOrder, event: OrderPlaced) {
        order.status = TestOrderStatus::Placed;
        order.total = 99.99;
        let _ = event;
    }

    #[aggregator(id = "order_id")]
    fn on_shipped(order: &mut TestOrder, event: OrderShipped) {
        order.status = TestOrderStatus::Shipped;
        let _ = event;
    }
}

#[test]
fn aggregators_module_produces_vec() {
    let aggs = order_aggregators::aggregators();
    assert_eq!(aggs.len(), 2);
    assert_eq!(aggs[0].aggregate_type, "TestOrder");
    assert_eq!(aggs[1].aggregate_type, "TestOrder");
}

#[test]
fn aggregator_apply_trait_generated() {
    let mut order = TestOrder::default();
    assert_eq!(order.status, TestOrderStatus::Draft);

    order.apply(OrderPlaced {
        order_id: Uuid::nil(),
    });
    assert_eq!(order.status, TestOrderStatus::Placed);

    order.apply(OrderShipped {
        order_id: Uuid::nil(),
    });
    assert_eq!(order.status, TestOrderStatus::Shipped);
}

// ── Aggregator id_fn tests (enum events with method access) ───────────

#[derive(Clone, Serialize, Deserialize)]
enum PipelineEvent {
    Started { run_id: Uuid },
    Completed { run_id: Uuid },
}

impl PipelineEvent {
    fn run_id(&self) -> Uuid {
        match self {
            PipelineEvent::Started { run_id } => *run_id,
            PipelineEvent::Completed { run_id } => *run_id,
        }
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
struct PipelineState {
    started: bool,
    completed: bool,
}

impl Aggregate for PipelineState {
    fn aggregate_type() -> &'static str {
        "PipelineState"
    }
}

#[aggregators]
mod pipeline_aggregators {
    use super::*;

    #[aggregator(id_fn = "run_id")]
    fn on_started(state: &mut PipelineState, event: PipelineEvent) {
        if let PipelineEvent::Started { .. } = event {
            state.started = true;
        }
    }
}

#[test]
fn aggregator_id_fn_produces_vec() {
    let aggs = pipeline_aggregators::aggregators();
    assert_eq!(aggs.len(), 1);
    assert_eq!(aggs[0].aggregate_type, "PipelineState");
}

#[test]
fn aggregator_id_fn_apply_works() {
    let mut state = PipelineState::default();
    assert!(!state.started);

    state.apply(PipelineEvent::Started {
        run_id: Uuid::nil(),
    });
    assert!(state.started);
}

// ── Singleton aggregator tests ─────────────────────────────────────────

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct RunStats {
    event_count: u32,
}

impl Aggregate for RunStats {
    fn aggregate_type() -> &'static str {
        "RunStats"
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct StepCompleted {
    name: String,
}

#[derive(Clone, Serialize, Deserialize)]
struct StepFailed {
    reason: String,
}

// Module-level singleton — no #[aggregator] needed on each fn
#[aggregators(singleton)]
mod singleton_aggregators {
    use super::*;

    fn on_step(stats: &mut RunStats, _event: StepCompleted) {
        stats.event_count += 1;
    }

    fn on_failure(stats: &mut RunStats, _event: StepFailed) {
        stats.event_count += 1;
    }
}

#[test]
fn singleton_aggregator_produces_vec() {
    let aggs = singleton_aggregators::aggregators();
    assert_eq!(aggs.len(), 2);
    assert_eq!(aggs[0].aggregate_type, "RunStats");
    assert_eq!(aggs[1].aggregate_type, "RunStats");
}

#[test]
fn singleton_aggregator_apply_works() {
    let mut stats = RunStats::default();
    assert_eq!(stats.event_count, 0);

    stats.apply(StepCompleted {
        name: "fetch".into(),
    });
    assert_eq!(stats.event_count, 1);

    stats.apply(StepFailed {
        reason: "timeout".into(),
    });
    assert_eq!(stats.event_count, 2);
}

// ── Handler macro tests ────────────────────────────────────────────────

#[test]
fn effects_module_registration_works() {
    let effects = order_effects::handles();
    assert_eq!(effects.len(), 10); // was 11 before projection moved out
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

    let bg_observer = effects
        .iter()
        .find(|effect| effect.id == "bg_observer")
        .expect("bg_observer effect should exist");
    assert!(
        !bg_observer.is_inline(),
        "queued attribute should force background execution"
    );

    let bg_retry_one = effects
        .iter()
        .find(|effect| effect.id == "bg_retry_one")
        .expect("bg_retry_one effect should exist");
    assert!(
        !bg_retry_one.is_inline(),
        "queued attribute should force background execution even when retry = 1"
    );

    let ship_high_value = effects
        .iter()
        .find(|effect| effect.id == "ship_high_value")
        .expect("ship_high_value effect should exist");
    assert!(
        ship_high_value.can_handle(TypeId::of::<HighValueOrder>()),
        "filter handler should handle HighValueOrder events"
    );

    // Projection is now in a separate projections() collection
    let projections = order_effects::projections();
    assert_eq!(projections.len(), 1);
    assert_eq!(projections[0].id, "order_read_model");

    let event_logger = effects
        .iter()
        .find(|effect| effect.id == "event_logger")
        .expect("event_logger on_any handler should exist");
    // on_any handlers match all event types
    assert!(
        event_logger.can_handle(TypeId::of::<OrderPlaced>()),
        "on_any handler should match OrderPlaced"
    );
    assert!(
        event_logger.can_handle(TypeId::of::<PaymentRequested>()),
        "on_any handler should match PaymentRequested"
    );
}

// ── Bare handler inference tests ──────────────────────────────────────

#[derive(Clone, Serialize, Deserialize)]
struct TaskCreated {
    task_id: Uuid,
}

#[derive(Clone, Serialize, Deserialize)]
struct TaskFinished {
    task_id: Uuid,
}

#[handles]
mod bare_handlers {
    use super::*;

    // No #[handle] — inferred from event param type
    async fn on_task_created(event: TaskCreated, _ctx: Context<Deps>) -> Result<Events> {
        Ok(events![TaskFinished {
            task_id: event.task_id,
        }])
    }

    // Explicit #[handle] still works alongside bare fns
    #[handle(on = TaskFinished, id = "log_finished")]
    async fn log_finished(_event: TaskFinished, _ctx: Context<Deps>) -> Result<()> {
        Ok(())
    }
}

#[test]
fn bare_handler_inference_works() {
    let effects = bare_handlers::handles();
    assert_eq!(effects.len(), 2);

    let inferred = effects
        .iter()
        .find(|e| e.id == "on_task_created")
        .expect("bare fn should produce handler with fn name as id");
    assert!(inferred.can_handle(TypeId::of::<TaskCreated>()));

    let explicit = effects
        .iter()
        .find(|e| e.id == "log_finished")
        .expect("explicit #[handle] should still work");
    assert!(explicit.can_handle(TypeId::of::<TaskFinished>()));
}
