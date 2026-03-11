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
