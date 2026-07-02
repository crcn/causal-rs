//! Test suites: the in-memory workspace suite plus the live backend suites
//! (Postgres / KurrentDB), which spin up the live-test stack on demand.

use anyhow::Result;
use clap::Subcommand;
use devkit_core::AppContext;
use dialoguer::Select;
use std::process::Command;

use super::{stack, DEV_KURRENT_URL, DEV_PG_URL};

#[derive(Subcommand)]
pub enum TestCommand {
    /// In-memory suite — `cargo test --workspace` (no Docker)
    Unit,
    /// Live Postgres suites (spins up Postgres)
    Pg,
    /// Live KurrentDB suites (spins up Kurrent)
    Kurrent,
    /// Live hybrid suite — Kurrent log + Postgres checkpoint
    Hybrid,
    /// Pg + Kurrent + Hybrid
    Live,
    /// Unit + Live
    All,
}

pub fn run(ctx: &AppContext, cmd: TestCommand) -> Result<()> {
    match cmd {
        TestCommand::Unit => unit(ctx),
        TestCommand::Pg => pg(ctx),
        TestCommand::Kurrent => kurrent(ctx),
        TestCommand::Hybrid => hybrid(ctx),
        TestCommand::Live => live(ctx),
        TestCommand::All => all(ctx),
    }
}

/// Run `cargo test ...` with optional env, from the repo root.
/// Propagates failure — `./dev.sh test …` must exit non-zero when a
/// suite fails so scripts and CI can trust it.
fn cargo_test(ctx: &AppContext, args: &[&str], envs: &[(&str, &str)], label: &str) -> Result<()> {
    let mut command = Command::new("cargo");
    command.args(args).current_dir(&ctx.repo);
    for (k, v) in envs {
        command.env(k, v);
    }
    let status = command.status()?;
    println!();
    if status.success() {
        ctx.print_success(&format!("{label} — passed"));
        Ok(())
    } else {
        ctx.print_warning(&format!("{label} — failures above"));
        anyhow::bail!("{label} suite failed")
    }
}

fn unit(ctx: &AppContext) -> Result<()> {
    ctx.print_header("In-memory suite");
    cargo_test(ctx, &["test", "--workspace"], &[], "unit")
}

fn pg(ctx: &AppContext) -> Result<()> {
    ctx.print_header("Live Postgres suites");
    stack::ensure_up(ctx, &["postgres"])?;
    stack::apply_pg_schema(ctx)?;
    cargo_test(
        ctx,
        &[
            "test", "-p", "causal_replay", "--features", "postgres",
            "--test", "pg_event_log_test",
            "--test", "pg_event_log_conformance_test",
            "--test", "pg_checkpoint_conformance_test",
            "--test", "pg_snapshot_store_test",
            "--test", "pg_inspector_test",
            "--test", "pg_reactor_observer_test",
            "--", "--ignored",
        ],
        &[("DATABASE_URL", DEV_PG_URL)],
        "postgres",
    )
}

fn kurrent(ctx: &AppContext) -> Result<()> {
    ctx.print_header("Live KurrentDB suites");
    stack::ensure_up(ctx, &["kurrentdb"])?;
    cargo_test(
        ctx,
        &[
            "test", "-p", "causal_replay", "--features", "kurrent",
            "--test", "kurrent_event_log_test",
            "--test", "kurrent_event_log_conformance_test",
            "--", "--ignored",
        ],
        &[("KURRENT_URL", DEV_KURRENT_URL)],
        "kurrent",
    )
}

fn hybrid(ctx: &AppContext) -> Result<()> {
    ctx.print_header("Live hybrid suite");
    stack::ensure_up(ctx, &[])?;
    stack::apply_pg_schema(ctx)?;
    cargo_test(
        ctx,
        &[
            "test", "-p", "causal_replay", "--features", "postgres,kurrent",
            "--test", "kurrent_pg_hybrid_test", "--", "--ignored",
        ],
        &[("DATABASE_URL", DEV_PG_URL), ("KURRENT_URL", DEV_KURRENT_URL)],
        "hybrid",
    )
}

fn live(ctx: &AppContext) -> Result<()> {
    pg(ctx)?;
    kurrent(ctx)?;
    hybrid(ctx)
}

fn all(ctx: &AppContext) -> Result<()> {
    unit(ctx)?;
    live(ctx)
}

pub fn interactive_menu(ctx: &AppContext) -> Result<()> {
    let items = [
        "Unit       in-memory suite (no docker)",
        "Postgres   live PG suites (spins up Postgres)",
        "Kurrent    live KurrentDB suites (spins up Kurrent)",
        "Live       PG + Kurrent + hybrid",
        "All        unit + live",
        "← Back",
    ];
    let choice = Select::with_theme(&ctx.theme())
        .with_prompt("Test")
        .items(&items)
        .default(0)
        .interact()?;
    match choice {
        0 => unit(ctx),
        1 => pg(ctx),
        2 => kurrent(ctx),
        3 => live(ctx),
        4 => all(ctx),
        _ => Ok(()),
    }
}
