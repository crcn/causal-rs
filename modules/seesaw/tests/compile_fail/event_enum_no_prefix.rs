use serde::{Serialize, Deserialize};

#[seesaw_core::event]
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum BadEvent {
    VariantA,
    VariantB,
}

fn main() {}
