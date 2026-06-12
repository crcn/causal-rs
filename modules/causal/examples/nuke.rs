//! Scale assault harness — drive the engine and MemoryStore into the
//! millions and watch where they bend.
//!
//!   cargo run --release -p causal --example nuke -- emit 200000
//!   cargo run --release -p causal --example nuke -- drain 200000
//!   cargo run --release -p causal --example nuke -- settle_flood 20000 64
//!
//! Each scenario prints per-decile timings: a flat decile column is
//! linear scaling; a growing column is the quadratic curve.

use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use causal::checkpoint_store::{CheckpointStore, ReactorCheckpoint};
use causal::contexts::Ctx;
use causal::event::Event;
use causal::event_log::EventLogBackend;
use causal::memory_store::MemoryStore;
use causal::reactor::{Events, Reactor};
use causal::{EngineBuilder, Projector};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Ping {
    id: Uuid,
    occurred_at: DateTime<Utc>,
}
impl Event for Ping {
    const CATEGORY: &'static str = "nuke_ping";
    fn event_type(&self) -> &str {
        "ping"
    }
    fn stream_id(&self) -> Uuid {
        self.id
    }
    fn occurred_at(&self) -> Option<DateTime<Utc>> {
        Some(self.occurred_at)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Pong {
    id: Uuid,
}
impl Event for Pong {
    const CATEGORY: &'static str = "nuke_pong";
    fn event_type(&self) -> &str {
        "pong"
    }
    fn stream_id(&self) -> Uuid {
        self.id
    }
}

struct Echo;
#[async_trait]
impl Reactor for Echo {
    type Trigger = Ping;
    const NAME: &'static str = "nuke.echo";
    async fn react(&self, t: &Ping, _ctx: Ctx<'_>) -> Result<Events> {
        Ok(causal::events![Pong { id: t.id }])
    }
}

#[derive(Default, Clone)]
struct CountPings {
    n: Arc<std::sync::atomic::AtomicU64>,
}
#[async_trait]
impl Projector for CountPings {
    type Event = Ping;
    const NAME: &'static str = "nuke.count";
    async fn project(&self, _f: &Ping, _ctx: Ctx<'_>) -> Result<()> {
        self.n.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }
}

fn decile_report(label: &str, marks: &[(u64, std::time::Duration)]) {
    println!("\n{label} — per-decile elapsed (flat = linear, growing = superlinear):");
    let mut prev = std::time::Duration::ZERO;
    for (events, total) in marks {
        let delta = *total - prev;
        prev = *total;
        println!(
            "  {:>10} events  decile {:>8.2?}  cumulative {:>8.2?}  ({:>9.0} ev/s in decile)",
            events,
            delta,
            total,
            if delta.as_secs_f64() > 0.0 {
                (marks[0].0 as f64) / delta.as_secs_f64()
            } else {
                f64::INFINITY
            },
        );
    }
}

async fn emit_scenario(n: u64) -> Result<()> {
    let store = Arc::new(MemoryStore::new());
    let engine = EngineBuilder::new(
        store.clone() as Arc<dyn EventLogBackend>,
        store.clone() as Arc<dyn CheckpointStore>,
        store.clone() as Arc<dyn ReactorCheckpoint>,
    )
    .build()
    .await?;

    let start = Instant::now();
    let decile = (n / 10).max(1);
    let mut marks = Vec::new();
    for i in 0..n {
        engine
            .emit(Ping {
                id: Uuid::new_v4(),
                occurred_at: Utc::now(),
            })
            .await?;
        if (i + 1) % decile == 0 {
            marks.push((i + 1, start.elapsed()));
        }
    }
    decile_report(&format!("emit {n} (fire-and-forget, distinct streams)"), &marks);
    engine.shutdown().await?;
    Ok(())
}

async fn drain_scenario(n: u64) -> Result<()> {
    // Seed N events with NO consumers, then start a counting projector
    // from ZERO and time the catch-up — the read_all-from-cursor path.
    let store = Arc::new(MemoryStore::new());
    {
        let seeder = EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        )
        .build()
        .await?;
        let t = Instant::now();
        for _ in 0..n {
            seeder
                .emit(Ping {
                    id: Uuid::new_v4(),
                    occurred_at: Utc::now(),
                })
                .await?;
        }
        println!("seeded {n} events in {:.2?}", t.elapsed());
        seeder.shutdown().await?;
    }

    let counter = CountPings::default();
    let seen = counter.n.clone();
    let engine = EngineBuilder::new(
        store.clone() as Arc<dyn EventLogBackend>,
        store.clone() as Arc<dyn CheckpointStore>,
        store.clone() as Arc<dyn ReactorCheckpoint>,
    )
    .with_projector(counter)
    .build()
    .await?;

    let start = Instant::now();
    let decile = (n / 10).max(1);
    let mut marks = Vec::new();
    let mut next_mark = decile;
    loop {
        let c = seen.load(std::sync::atomic::Ordering::Relaxed);
        while c >= next_mark {
            marks.push((next_mark, start.elapsed()));
            next_mark += decile;
        }
        if c >= n {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    decile_report(&format!("drain {n} (projector catch-up from ZERO)"), &marks);
    engine.shutdown().await?;
    Ok(())
}

async fn settle_flood_scenario(total: u64, conc: u64) -> Result<()> {
    let store = Arc::new(MemoryStore::new());
    let engine = Arc::new(
        EngineBuilder::new(
            store.clone() as Arc<dyn EventLogBackend>,
            store.clone() as Arc<dyn CheckpointStore>,
            store.clone() as Arc<dyn ReactorCheckpoint>,
        )
        .with_reactor(Echo)
        .build()
        .await?,
    );

    let start = Instant::now();
    let per_task = total / conc;
    let mut handles = Vec::new();
    for _ in 0..conc {
        let engine = engine.clone();
        handles.push(tokio::spawn(async move {
            let mut worst = std::time::Duration::ZERO;
            let mut sum = std::time::Duration::ZERO;
            for _ in 0..per_task {
                let t = Instant::now();
                engine
                    .emit(Ping {
                        id: Uuid::new_v4(),
                        occurred_at: Utc::now(),
                    })
                    .settled()
                    .await
                    .expect("settled emit failed");
                let d = t.elapsed();
                sum += d;
                if d > worst {
                    worst = d;
                }
            }
            (sum, worst)
        }));
    }
    let mut worst = std::time::Duration::ZERO;
    let mut sum = std::time::Duration::ZERO;
    for h in handles {
        let (s, w) = h.await?;
        sum += s;
        if w > worst {
            worst = w;
        }
    }
    let done = total - (total % conc);
    println!(
        "\nsettle_flood {done} settles x{conc} concurrent: total {:.2?}, mean settle {:.2?}, worst settle {:.2?}, {:.0} settles/s",
        start.elapsed(),
        sum / (done as u32),
        worst,
        done as f64 / start.elapsed().as_secs_f64(),
    );
    Arc::try_unwrap(engine)
        .map_err(|_| anyhow::anyhow!("engine still shared"))?
        .shutdown()
        .await?;
    Ok(())
}

async fn chase_scenario(n: u64) -> Result<()> {
    // Fire-and-forget flood with the Echo reactor registered: the
    // serial consumer loop must chase a log that's growing under it,
    // and the SettleTracker accumulates one entry per un-settled
    // correlation (crossing its 65,536 CAP well before n = 100k —
    // eviction must stay bounded, the chase must still finish).
    let store = Arc::new(MemoryStore::new());
    let counter = CountPings::default();
    let pongs = counter.n.clone();

    #[derive(Default, Clone)]
    struct CountPongs {
        n: Arc<std::sync::atomic::AtomicU64>,
    }
    #[async_trait]
    impl Projector for CountPongs {
        type Event = Pong;
        const NAME: &'static str = "nuke.count_pongs";
        async fn project(&self, _f: &Pong, _ctx: Ctx<'_>) -> Result<()> {
            self.n.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        }
    }
    let pong_counter = CountPongs::default();
    let pongs_seen = pong_counter.n.clone();
    let _ = pongs;

    let engine = EngineBuilder::new(
        store.clone() as Arc<dyn EventLogBackend>,
        store.clone() as Arc<dyn CheckpointStore>,
        store.clone() as Arc<dyn ReactorCheckpoint>,
    )
    .with_reactor(Echo)
    .with_projector(counter)
    .with_projector(pong_counter)
    .build()
    .await?;

    let start = Instant::now();
    for _ in 0..n {
        engine
            .emit(Ping {
                id: Uuid::new_v4(),
                occurred_at: Utc::now(),
            })
            .await?;
    }
    let emitted_at = start.elapsed();
    println!("emitted {n} pings (fire-and-forget) in {emitted_at:.2?}");

    // Wait for the full cascade: every ping echoed to a pong, every
    // pong projected.
    let decile = (n / 10).max(1);
    let mut marks = Vec::new();
    let mut next_mark = decile;
    loop {
        let c = pongs_seen.load(std::sync::atomic::Ordering::Relaxed);
        while c >= next_mark {
            marks.push((next_mark, start.elapsed()));
            next_mark += decile;
        }
        if c >= n {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    decile_report(
        &format!("chase {n} (echo reactor + pong projector vs live flood)"),
        &marks,
    );
    engine.shutdown().await?;
    Ok(())
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let scenario = args.get(1).map(String::as_str).unwrap_or("emit");
    let n: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(100_000);
    match scenario {
        "emit" => emit_scenario(n).await?,
        "drain" => drain_scenario(n).await?,
        "settle_flood" => {
            let conc: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(64);
            settle_flood_scenario(n, conc).await?
        }
        "chase" => chase_scenario(n).await?,
        other => anyhow::bail!("unknown scenario '{other}' (emit | drain | settle_flood | chase)"),
    }
    Ok(())
}
