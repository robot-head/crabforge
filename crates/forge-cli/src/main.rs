//! `crabforge` — the operator CLI.
//!
//! Bootstrap logic lives here rather than in shell so that development and
//! production provisioning run the same code path.

use anyhow::{Context as _, Result};
use clap::{Parser, Subcommand};

mod bootstrap;
mod doctor;

/// Default broker address for local development.
const DEFAULT_BOOTSTRAP: &str = "127.0.0.1:9092";

#[derive(Parser)]
#[command(name = "crabforge", version, about = "Crabforge operator CLI")]
struct Cli {
    /// Crabka broker bootstrap address.
    #[arg(long, short, global = true, env = "CRABFORGE_BOOTSTRAP", default_value = DEFAULT_BOOTSTRAP)]
    bootstrap: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Provision every topic the forge needs. Safe to re-run.
    Bootstrap,
    /// Check that the platform is ready to serve, and explain what is not.
    Doctor,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Bootstrap => bootstrap::run(&cli.bootstrap)
            .await
            .context("bootstrap failed")?,
        Command::Doctor => {
            let report = doctor::run(&cli.bootstrap).await;
            report.print();
            if !report.is_healthy() {
                std::process::exit(1);
            }
        }
    }
    Ok(())
}
