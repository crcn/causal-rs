//! Live-test infrastructure: Postgres + KurrentDB via `dev/docker-compose.yml`.
//!
//! Shared by `test pg|kurrent|live` (which call [`ensure_up`] / [`apply_pg_schema`])
//! and exposed directly as `stack up|down`.

use anyhow::{bail, Result};
use clap::Subcommand;
use devkit_core::AppContext;
use dialoguer::Select;
use std::process::Command;

use super::DEV_COMPOSE;

const PG_CONTAINER: &str = "causal-dev-postgres";

#[derive(Subcommand)]
pub enum StackCommand {
    /// Start the stack (Postgres + Kurrent) and apply migrations
    Up,
    /// Stop the stack and remove its volumes
    Down,
}

pub fn run(ctx: &AppContext, cmd: StackCommand) -> Result<()> {
    match cmd {
        StackCommand::Up => up(ctx),
        StackCommand::Down => down(ctx),
    }
}

/// Bring up the given compose services (empty = all), waiting for healthchecks.
pub fn ensure_up(ctx: &AppContext, services: &[&str]) -> Result<()> {
    let label = if services.is_empty() {
        "Postgres + Kurrent".to_string()
    } else {
        services.join(" + ")
    };
    ctx.print_info(&format!("Starting live-test stack ({label})…"));

    let mut args = vec!["compose", "-f", DEV_COMPOSE, "up", "-d", "--wait"];
    args.extend_from_slice(services);

    let status = Command::new("docker")
        .args(&args)
        .current_dir(&ctx.repo)
        .status()?;
    if !status.success() {
        bail!("failed to start the live-test stack (is Docker running?)");
    }
    ctx.print_success("Stack healthy.");
    Ok(())
}

/// Apply `migrations/` to the dev database — idempotent (skips if already applied).
pub fn apply_pg_schema(ctx: &AppContext) -> Result<()> {
    let probe = Command::new("docker")
        .args([
            "exec", PG_CONTAINER, "psql", "-U", "causal", "-d", "causal_dev",
            "-tAc", "SELECT to_regclass('causal_log');",
        ])
        .current_dir(&ctx.repo)
        .output()?;
    if String::from_utf8_lossy(&probe.stdout).trim() == "causal_log" {
        return Ok(());
    }

    ctx.print_info("Applying migrations to causal_dev…");
    let mut files: Vec<_> = std::fs::read_dir(ctx.repo.join("migrations"))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "sql"))
        .collect();
    files.sort();

    for file in files {
        let sql = std::fs::File::open(&file)?;
        let status = Command::new("docker")
            .args([
                "exec", "-i", PG_CONTAINER, "psql", "-U", "causal", "-d", "causal_dev",
                "-v", "ON_ERROR_STOP=1",
            ])
            .stdin(sql)
            .current_dir(&ctx.repo)
            .status()?;
        if !status.success() {
            bail!("migration failed: {}", file.display());
        }
    }
    ctx.print_success("Schema applied.");
    Ok(())
}

fn up(ctx: &AppContext) -> Result<()> {
    ctx.print_header("Live-test stack");
    ensure_up(ctx, &[])?;
    apply_pg_schema(ctx)?;
    ctx.print_info("Postgres → localhost:5433   Kurrent → localhost:2114");
    Ok(())
}

fn down(ctx: &AppContext) -> Result<()> {
    ctx.print_header("Stopping live-test stack");
    let status = Command::new("docker")
        .args(["compose", "-f", DEV_COMPOSE, "down", "-v"])
        .current_dir(&ctx.repo)
        .status()?;
    if !status.success() {
        bail!("failed to stop the live-test stack");
    }
    ctx.print_success("Stopped.");
    Ok(())
}

pub fn interactive_menu(ctx: &AppContext) -> Result<()> {
    let items = ["Start (up + migrate)", "Stop (down -v)", "← Back"];
    let choice = Select::with_theme(&ctx.theme())
        .with_prompt("Live-test stack")
        .items(&items)
        .default(0)
        .interact()?;
    match choice {
        0 => up(ctx),
        1 => down(ctx),
        _ => Ok(()),
    }
}
