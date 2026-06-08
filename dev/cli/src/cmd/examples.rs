//! Examples: discover everything under `examples/`, bring up each one's
//! `docker-compose.yml` (health-waited) before running it.

use anyhow::{bail, Result};
use clap::Subcommand;
use devkit_core::AppContext;
use dialoguer::Select;
use std::path::PathBuf;
use std::process::Command;

#[derive(Subcommand)]
pub enum ExampleCommand {
    /// List runnable examples
    List,
    /// Run an example (brings up its docker stack first)
    Run { name: String },
    /// Bring up an example's docker stack
    Up { name: String },
    /// Stop an example's docker stack
    Down { name: String },
}

pub fn run(ctx: &AppContext, cmd: ExampleCommand) -> Result<()> {
    match cmd {
        ExampleCommand::List => {
            for name in discover(ctx) {
                let docker = if compose_file(ctx, &name).is_some() {
                    "  (docker)"
                } else {
                    ""
                };
                println!("  • {}{}", name, docker);
            }
            Ok(())
        }
        ExampleCommand::Run { name } => run_example(ctx, &name),
        ExampleCommand::Up { name } => compose_up(ctx, &name),
        ExampleCommand::Down { name } => compose_down(ctx, &name),
    }
}

/// Every directory under `examples/` that is a runnable crate.
pub fn discover(ctx: &AppContext) -> Vec<String> {
    let mut names = Vec::new();
    if let Ok(entries) = std::fs::read_dir(ctx.repo.join("examples")) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.join("Cargo.toml").is_file() {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    names.push(name.to_string());
                }
            }
        }
    }
    names.sort();
    names
}

/// The example's compose file, if it has one.
pub fn compose_file(ctx: &AppContext, name: &str) -> Option<PathBuf> {
    let dir = ctx.repo.join("examples").join(name);
    for f in ["docker-compose.yml", "docker-compose.yaml", "compose.yml", "compose.yaml"] {
        let candidate = dir.join(f);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn require_example(ctx: &AppContext, name: &str) -> Result<PathBuf> {
    let dir = ctx.repo.join("examples").join(name);
    if !dir.join("Cargo.toml").is_file() {
        bail!("no example '{name}' — run `dev example list`");
    }
    Ok(dir)
}

fn compose_up(ctx: &AppContext, name: &str) -> Result<()> {
    require_example(ctx, name)?;
    let Some(file) = compose_file(ctx, name) else {
        bail!("'{name}' has no docker stack");
    };
    ctx.print_info(&format!("Bringing up '{name}' infrastructure…"));
    let status = Command::new("docker")
        .arg("compose")
        .arg("-f")
        .arg(&file)
        .args(["up", "-d", "--wait"])
        .current_dir(&ctx.repo)
        .status()?;
    if !status.success() {
        bail!("failed to start '{name}' stack (is Docker running?)");
    }
    ctx.print_success("Infrastructure healthy.");
    Ok(())
}

fn compose_down(ctx: &AppContext, name: &str) -> Result<()> {
    let Some(file) = compose_file(ctx, name) else {
        bail!("'{name}' has no docker stack");
    };
    ctx.print_info(&format!("Stopping '{name}' stack…"));
    let status = Command::new("docker")
        .arg("compose")
        .arg("-f")
        .arg(&file)
        .arg("down")
        .current_dir(&ctx.repo)
        .status()?;
    if !status.success() {
        bail!("failed to stop '{name}' stack");
    }
    ctx.print_success("Stopped.");
    Ok(())
}

fn run_example(ctx: &AppContext, name: &str) -> Result<()> {
    let dir = require_example(ctx, name)?;
    let has_stack = compose_file(ctx, name).is_some();

    ctx.print_header(&format!("Example: {name}"));
    if has_stack {
        compose_up(ctx, name)?;
    }
    if name == "ai-summarizer" && std::env::var("ANTHROPIC_API_KEY").is_err() {
        ctx.print_warning(
            "ANTHROPIC_API_KEY is unset — this example will exit asking for it. \
             `export ANTHROPIC_API_KEY=…` and re-run.",
        );
    }
    let ui = dir.join("ui");
    if ui.is_dir() && !ui.join("dist").is_dir() {
        ctx.print_warning(&format!(
            "{name} ships a UI that isn't built yet — its /causal route will 404. \
             Build it first: (cd examples/{name}/ui && npm install && npm run build)"
        ));
    }

    ctx.print_info("Running (cargo run) — Ctrl-C to stop.");
    println!();
    let status = Command::new("cargo").arg("run").current_dir(&dir).status()?;
    println!();

    if has_stack {
        ctx.print_info(&format!(
            "'{name}' exited; its containers stay up. Stop them with: dev example down {name}"
        ));
    }
    if !status.success() {
        ctx.print_warning("Example exited with a non-zero status.");
    }
    Ok(())
}

pub fn interactive_menu(ctx: &AppContext) -> Result<()> {
    let examples = discover(ctx);
    if examples.is_empty() {
        ctx.print_warning("No examples found under examples/.");
        return Ok(());
    }

    let mut items: Vec<String> = examples
        .iter()
        .map(|name| {
            if compose_file(ctx, name).is_some() {
                format!("{name}  (docker)")
            } else {
                name.clone()
            }
        })
        .collect();
    items.push("← Back".to_string());

    let choice = Select::with_theme(&ctx.theme())
        .with_prompt("Run which example?")
        .items(&items)
        .default(0)
        .interact()?;

    if choice >= examples.len() {
        return Ok(());
    }
    run_example(ctx, &examples[choice])
}
