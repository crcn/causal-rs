use serde::{Serialize, Deserialize};

#[causal::event]
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum BadEvent {
    VariantA,
    VariantB,
}

fn main() {}
