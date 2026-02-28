use anyhow::Result;
use seesaw_core::{handler, Context};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone)]
struct Deps;

#[derive(Clone, Serialize, Deserialize)]
struct MyEvent {
    id: Uuid,
    value: u32,
}

#[derive(Clone, Serialize, Deserialize)]
struct MyOutput {
    id: Uuid,
}

fn my_filter(event: &MyEvent) -> bool {
    event.value > 100
}

#[handler(on = MyEvent, filter = my_filter, extract(id), id = "bad")]
async fn bad_handler(id: Uuid, _ctx: Context<Deps>) -> Result<MyOutput> {
    Ok(MyOutput { id })
}

fn main() {}
