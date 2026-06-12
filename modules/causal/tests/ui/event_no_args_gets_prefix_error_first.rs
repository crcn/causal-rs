// An enum with NO args must get the prefix error — the more
// fundamental one — not the stream-identity error followed by a
// second error after the developer "fixes" it. No stair-stepping.
use serde::{Deserialize, Serialize};

#[causal::event]
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Bare {
    Something { n: u64 },
}

fn main() {}
