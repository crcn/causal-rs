//! Tests for #[event] proc macro.

use causal::Event;

// ── Enum with prefix + snake_case ───────────────────────────────────

#[causal::event(prefix = "scrape")]
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ScrapeEvent {
    WebScrapeCompleted { urls_scraped: usize },
    SourcesResolved,
}

#[test]
fn enum_snake_case_durable_name() {
    let e = ScrapeEvent::WebScrapeCompleted { urls_scraped: 5 };
    assert_eq!(e.durable_name(), "scrape:web_scrape_completed");

    let e = ScrapeEvent::SourcesResolved;
    assert_eq!(e.durable_name(), "scrape:sources_resolved");
}

#[test]
fn enum_event_prefix() {
    assert_eq!(ScrapeEvent::event_prefix(), "scrape");
}

#[test]
fn enum_not_ephemeral() {
    assert!(!ScrapeEvent::is_ephemeral());
}

// ── Enum with no rename_all (PascalCase variants) ───────────────────

#[causal::event(prefix = "order")]
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type")]
pub enum OrderEvent {
    OrderPlaced { total: f64 },
    OrderShipped,
}

#[test]
fn enum_no_rename_uses_pascal_case() {
    let e = OrderEvent::OrderPlaced { total: 99.99 };
    assert_eq!(e.durable_name(), "order:OrderPlaced");

    let e = OrderEvent::OrderShipped;
    assert_eq!(e.durable_name(), "order:OrderShipped");
}

// ── Enum with camelCase ─────────────────────────────────────────────

#[causal::event(prefix = "user")]
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum UserEvent {
    UserCreated,
    PasswordChanged,
}

#[test]
fn enum_camel_case_durable_name() {
    assert_eq!(UserEvent::UserCreated.durable_name(), "user:userCreated");
    assert_eq!(
        UserEvent::PasswordChanged.durable_name(),
        "user:passwordChanged"
    );
}

// ── Enum with SCREAMING_SNAKE_CASE ──────────────────────────────────

#[causal::event(prefix = "status")]
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum StatusEvent {
    StatusChanged,
    StatusReset,
}

#[test]
fn enum_screaming_snake_case() {
    assert_eq!(
        StatusEvent::StatusChanged.durable_name(),
        "status:STATUS_CHANGED"
    );
}

// ── Enum with kebab-case ────────────────────────────────────────────

#[causal::event(prefix = "nav")]
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum NavEvent {
    PageViewed,
    LinkClicked,
}

#[test]
fn enum_kebab_case() {
    assert_eq!(NavEvent::PageViewed.durable_name(), "nav:page-viewed");
}

// ── Ephemeral enum ──────────────────────────────────────────────────

#[causal::event(prefix = "synthesis", ephemeral)]
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SynthesisEvent {
    SimilarityComputed,
    ResponsesMapped,
}

#[test]
fn ephemeral_enum() {
    assert!(SynthesisEvent::is_ephemeral());
    assert_eq!(
        SynthesisEvent::SimilarityComputed.durable_name(),
        "synthesis:similarity_computed"
    );
    assert_eq!(SynthesisEvent::event_prefix(), "synthesis");
}

// ── Struct event ────────────────────────────────────────────────────

#[causal::event]
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct OrderPlaced {
    pub order_id: u64,
}

#[test]
fn struct_durable_name_is_snake_case() {
    let e = OrderPlaced { order_id: 1 };
    assert_eq!(e.durable_name(), "order_placed");
    assert_eq!(OrderPlaced::event_prefix(), "order_placed");
    assert!(!OrderPlaced::is_ephemeral());
}

// ── Ephemeral struct ────────────────────────────────────────────────

#[causal::event(ephemeral)]
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct EnrichmentReady {
    pub correlation_id: u64,
}

#[test]
fn ephemeral_struct() {
    assert!(EnrichmentReady::is_ephemeral());
    assert_eq!(
        EnrichmentReady { correlation_id: 1 }.durable_name(),
        "enrichment_ready"
    );
}

// ── Struct with explicit prefix ─────────────────────────────────────

#[causal::event(prefix = "custom_name")]
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct MyThing {
    pub value: i32,
}

#[test]
fn struct_with_explicit_prefix() {
    assert_eq!(MyThing { value: 1 }.durable_name(), "custom_name");
    assert_eq!(MyThing::event_prefix(), "custom_name");
}

// ── Enum with tuple variants ────────────────────────────────────────

#[causal::event(prefix = "batch")]
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BatchEvent {
    ItemProcessed { count: usize },
    BatchComplete,
}

#[test]
fn enum_with_unit_and_named_variants() {
    assert_eq!(
        BatchEvent::ItemProcessed { count: 5 }.durable_name(),
        "batch:item_processed"
    );
    assert_eq!(
        BatchEvent::BatchComplete.durable_name(),
        "batch:batch_complete"
    );
}

// ── Enum with per-variant #[serde(rename = "...")] ──────────────────

#[causal::event(prefix = "custom")]
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CustomRenameEvent {
    #[serde(rename = "my_special_name")]
    VariantA,
    VariantB,
}

#[test]
fn enum_with_serde_rename_per_variant() {
    assert_eq!(
        CustomRenameEvent::VariantA.durable_name(),
        "custom:my_special_name"
    );
    assert_eq!(
        CustomRenameEvent::VariantB.durable_name(),
        "custom:variant_b"
    );
}

// ── Enum with adjacent tagging ──────────────────────────────────────

#[causal::event(prefix = "adj")]
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum AdjacentEvent {
    ThingHappened { x: i32 },
    OtherThing,
}

#[test]
fn enum_with_adjacent_tagging() {
    assert_eq!(
        AdjacentEvent::ThingHappened { x: 1 }.durable_name(),
        "adj:thing_happened"
    );
    assert_eq!(
        AdjacentEvent::OtherThing.durable_name(),
        "adj:other_thing"
    );
}

// ─── v0.3 Fact impl generation via stream_category + stream_id ──────

#[causal::event(
    prefix = "scheduling",
    stream_category = "schedule",
    stream_id = "schedule_id",
)]
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SchedulingFact {
    ScheduleCreated {
        schedule_id: uuid::Uuid,
        occurred_at: chrono::DateTime<chrono::Utc>,
        timeout: u32,
    },
    ScheduleToggled {
        schedule_id: uuid::Uuid,
        occurred_at: chrono::DateTime<chrono::Utc>,
        enabled: bool,
    },
}

#[test]
fn enum_event_fact_impl_provides_type_name() {
    use causal::Fact;
    let f = SchedulingFact::ScheduleCreated {
        schedule_id: uuid::Uuid::nil(),
        occurred_at: chrono::Utc::now(),
        timeout: 60,
    };
    assert_eq!(f.type_name(), "scheduling:schedule_created");
}

#[test]
fn enum_event_fact_impl_provides_type_prefix() {
    use causal::Fact;
    assert_eq!(<SchedulingFact as Fact>::type_prefix(), "scheduling");
}

#[test]
fn enum_event_fact_impl_extracts_occurred_at() {
    use causal::Fact;
    let pinned = chrono::DateTime::parse_from_rfc3339("2026-01-01T12:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let f = SchedulingFact::ScheduleToggled {
        schedule_id: uuid::Uuid::nil(),
        occurred_at: pinned,
        enabled: true,
    };
    assert_eq!(f.occurred_at(), Some(pinned));
}

#[test]
fn enum_event_fact_impl_builds_stream_ref() {
    use causal::Fact;
    let id = uuid::Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
    let f = SchedulingFact::ScheduleCreated {
        schedule_id: id,
        occurred_at: chrono::Utc::now(),
        timeout: 60,
    };
    let s = f.stream();
    assert_eq!(s.category, "schedule");
    assert_eq!(s.id, id);
    assert_eq!(s.name(), format!("schedule-{}", id));
}

// Override occurred_at_field name.

#[causal::event(
    prefix = "telemetry",
    stream_category = "ping",
    stream_id = "ping_id",
    occurred_at_field = "ts",
)]
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PingFact {
    Pinged {
        ping_id: uuid::Uuid,
        ts: chrono::DateTime<chrono::Utc>,
    },
}

#[test]
fn enum_event_fact_impl_honors_custom_occurred_at_field() {
    use causal::Fact;
    let pinned = chrono::DateTime::parse_from_rfc3339("2030-12-31T23:59:59Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let f = PingFact::Pinged {
        ping_id: uuid::Uuid::nil(),
        ts: pinned,
    };
    assert_eq!(f.occurred_at(), Some(pinned));
}

// Struct event with Fact attributes.

#[causal::event(
    prefix = "fulfillment",
    stream_category = "fulfillment",
    stream_id = "shipment_id",
)]
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct ShipmentDispatched {
    pub shipment_id: uuid::Uuid,
    pub occurred_at: chrono::DateTime<chrono::Utc>,
    pub total_cents: i64,
}

#[test]
fn struct_event_fact_impl_works() {
    use causal::Fact;
    let id = uuid::Uuid::new_v4();
    let pinned = chrono::Utc::now();
    let f = ShipmentDispatched {
        shipment_id: id,
        occurred_at: pinned,
        total_cents: 1234,
    };
    assert_eq!(f.type_name(), "fulfillment");
    assert_eq!(<ShipmentDispatched as Fact>::type_prefix(), "fulfillment");
    assert_eq!(f.occurred_at(), Some(pinned));
    let s = f.stream();
    assert_eq!(s.category, "fulfillment");
    assert_eq!(s.id, id);
}

// Without stream_category + stream_id, no Fact impl is generated.
// This is a NEGATIVE test: trying to use Fact methods on a plain
// #[event(prefix = "...")] enum should fail to compile. We don't
// include such a test directly here; instead, the ImpliedQueriesConsumed
// example below confirms compilation succeeds when Fact ISN'T generated.

#[causal::event(prefix = "system_only")]
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SystemOnly {
    PingedNoStream { x: i32 },
}

#[test]
fn event_without_stream_attrs_compiles_without_fact_impl() {
    // This test compiles only if no Fact impl was generated.
    // (If one were generated, it'd require a stream_id field.)
    let _ = SystemOnly::PingedNoStream { x: 1 };
}
