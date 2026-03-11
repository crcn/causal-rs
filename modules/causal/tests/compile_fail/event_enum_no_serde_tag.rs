use serde::{Serialize, Deserialize};

#[causal_core::event(prefix = "test")]
#[derive(Clone, Serialize, Deserialize)]
pub enum BadEvent {
    VariantA,
    VariantB,
}

fn main() {}
