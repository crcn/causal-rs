use serde::{Serialize, Deserialize};

#[seesaw_core::event(prefix = "test")]
#[derive(Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum BadEvent {
    VariantA { x: i32 },
    VariantB { y: String },
}

fn main() {}
