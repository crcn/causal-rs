use serde::{Serialize, Deserialize};

#[seesaw_core::event(prefix = "test")]
#[derive(Clone, Serialize, Deserialize)]
pub enum BadEvent {
    VariantA,
    VariantB,
}

fn main() {}
