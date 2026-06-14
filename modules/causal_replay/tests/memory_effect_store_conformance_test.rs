//! EffectStore conformance suite run against `causal::InMemoryEffectStore`.
//!
//! `InMemoryEffectStore` is the reference implementation — these tests must
//! always pass. If any fail, the conformance scenarios themselves are wrong
//! (or the trait contract drifted).

use anyhow::Result;
use causal::InMemoryEffectStore;
use causal_replay::conformance;

fn s() -> InMemoryEffectStore {
    InMemoryEffectStore::new()
}

#[tokio::test]
async fn effect_store_get_miss() -> Result<()> {
    conformance::effect_store_get_miss(&s()).await
}

#[tokio::test]
async fn effect_store_put_and_get_round_trips() -> Result<()> {
    conformance::effect_store_put_and_get_round_trips(&s()).await
}

#[tokio::test]
async fn effect_store_first_write_wins() -> Result<()> {
    conformance::effect_store_first_write_wins(&s()).await
}

#[tokio::test]
async fn effect_store_remove_makes_entry_absent() -> Result<()> {
    conformance::effect_store_remove_makes_entry_absent(&s()).await
}

#[tokio::test]
async fn effect_store_remove_is_idempotent() -> Result<()> {
    conformance::effect_store_remove_is_idempotent(&s()).await
}

#[tokio::test]
async fn effect_store_keys_isolated_by_consumer() -> Result<()> {
    conformance::effect_store_keys_isolated_by_consumer(&s()).await
}

#[tokio::test]
async fn effect_store_keys_isolated_by_trigger() -> Result<()> {
    conformance::effect_store_keys_isolated_by_trigger(&s()).await
}

#[tokio::test]
async fn effect_store_keys_isolated_by_label() -> Result<()> {
    conformance::effect_store_keys_isolated_by_label(&s()).await
}

#[tokio::test]
async fn effect_store_remember_calls_compute_once() -> Result<()> {
    conformance::effect_store_remember_calls_compute_once(&s()).await
}

#[tokio::test]
async fn effect_store_remember_replays_cached_on_redelivery() -> Result<()> {
    conformance::effect_store_remember_replays_cached_on_redelivery(&s()).await
}
