//! causal-rs development CLI.
//!
//! The entry point for development, testing, and running examples.
//! Built + executed by `./dev.sh`.

use anyhow::Result;
use clap::{Parser, Subcommand};
use devkit_core::AppContext;
use dialoguer::FuzzySelect;
use std::process::{Command, ExitCode};

mod cmd;

#[derive(Parser)]
#[command(name = "dev")]
#[command(about = "causal-rs development CLI")]
#[command(version)]
struct Cli {
    /// Run in quiet mode (non-interactive)
    #[arg(short, long, global = true)]
    quiet: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the test suites (in-memory + live backends)
    #[command(subcommand)]
    Test(cmd::test::TestCommand),

    /// List and run examples (brings up each one's docker stack)
    #[command(subcommand, alias = "examples")]
    Example(cmd::examples::ExampleCommand),

    /// Control the live-test infra stack (Postgres + Kurrent)
    #[command(subcommand)]
    Stack(cmd::stack::StackCommand),

    /// cargo clippy across the workspace
    Lint,

    /// cargo fmt across the workspace
    Fmt,

    /// cargo check --all-features
    Check,

    /// Check system prerequisites
    Doctor,
}

fn main() -> ExitCode {
    let _ = dotenvy::dotenv();

    if let Err(e) = run() {
        eprintln!("Error: {:#}", e);
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let ctx = AppContext::new(cli.quiet)?;

    match cli.command {
        Some(Commands::Test(cmd)) => cmd::test::run(&ctx, cmd),
        Some(Commands::Example(cmd)) => cmd::examples::run(&ctx, cmd),
        Some(Commands::Stack(cmd)) => cmd::stack::run(&ctx, cmd),
        Some(Commands::Lint) => cmd_lint(&ctx),
        Some(Commands::Fmt) => cmd_fmt(&ctx),
        Some(Commands::Check) => cmd_check(&ctx),
        Some(Commands::Doctor) => cmd_doctor(&ctx),
        None => interactive_menu(&ctx),
    }
}

/// Run `cargo <args...>` from the repo root, reporting success/failure.
fn cargo(ctx: &AppContext, args: &[&str], success: &str) -> Result<()> {
    let status = Command::new("cargo")
        .args(args)
        .current_dir(&ctx.repo)
        .status()?;
    println!();
    if status.success() {
        ctx.print_success(success);
    } else {
        ctx.print_warning("Command exited with errors — see output above");
    }
    Ok(())
}

fn cmd_lint(ctx: &AppContext) -> Result<()> {
    ctx.print_header("Clippy");
    cargo(
        ctx,
        &["clippy", "--workspace", "--all-features", "--tests"],
        "Clippy clean",
    )
}

fn cmd_fmt(ctx: &AppContext) -> Result<()> {
    ctx.print_header("Format");
    cargo(ctx, &["fmt", "--all"], "Formatted")
}

fn cmd_check(ctx: &AppContext) -> Result<()> {
    ctx.print_header("Check");
    cargo(ctx, &["check", "--workspace", "--all-features"], "Checks pass")
}

fn cmd_doctor(ctx: &AppContext) -> Result<()> {
    ctx.print_header("System Health Check");
    println!();

    let tools = [
        ("cargo", devkit_core::utils::cmd_exists("cargo")),
        ("rustc", devkit_core::utils::cmd_exists("rustc")),
        ("git", devkit_core::utils::cmd_exists("git")),
        ("docker", devkit_core::utils::docker_available()),
    ];
    for (tool, ok) in tools {
        if ok {
            ctx.print_success(&format!("✓ {}", tool));
        } else {
            ctx.print_warning(&format!("✗ {} (not found)", tool));
        }
    }

    println!();
    ctx.print_info("Examples:");
    for name in cmd::examples::discover(ctx) {
        let docker = if cmd::examples::compose_file(ctx, &name).is_some() {
            "  (docker)"
        } else {
            ""
        };
        println!("  • {}{}", name, docker);
    }

    println!();
    ctx.print_success("Health check complete");
    Ok(())
}

fn interactive_menu(ctx: &AppContext) -> Result<()> {
    let items = [
        "📦 Examples →",
        "🧪 Test →",
        "🐳 Live-test stack →",
        "🔎 Lint",
        "🎨 Format",
        "✅ Check",
        "🩺 Doctor",
        "❌ Exit",
    ];

    loop {
        println!();
        let choice = FuzzySelect::with_theme(&ctx.theme())
            .with_prompt("What would you like to do?")
            .items(&items)
            .default(0)
            .interact()?;

        match choice {
            0 => cmd::examples::interactive_menu(ctx)?,
            1 => cmd::test::interactive_menu(ctx)?,
            2 => cmd::stack::interactive_menu(ctx)?,
            3 => cmd_lint(ctx)?,
            4 => cmd_fmt(ctx)?,
            5 => cmd_check(ctx)?,
            6 => cmd_doctor(ctx)?,
            _ => break,
        }
    }

    Ok(())
}
