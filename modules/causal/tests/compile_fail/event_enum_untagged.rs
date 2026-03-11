use serde::{Serialize, Deserialize};

#[causal::event(prefix = "test")]
#[derive(Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum BadEvent {
    VariantA { x: i32 },
    VariantB { y: String },
}

fn main() {}
