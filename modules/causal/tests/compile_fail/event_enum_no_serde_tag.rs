use serde::{Serialize, Deserialize};

#[causal::event(prefix = "test")]
#[derive(Clone, Serialize, Deserialize)]
pub enum BadEvent {
    VariantA,
    VariantB,
}

fn main() {}
