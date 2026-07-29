//! `crabforge` — the operator CLI.
//!
//! Bootstrap logic lives here rather than in shell so that development and
//! production provisioning run the same code path.

use std::time::Duration;

use anyhow::{Context as _, Result};
use clap::{Parser, Subcommand};

mod bootstrap;
mod doctor;
mod migrate;

/// Default broker address for local development.
const DEFAULT_BOOTSTRAP: &str = "127.0.0.1:9092";

/// Default gres connection string. Matches `crabforge-server`'s, so the two
/// agree about which database the schema is in without being told.
const DEFAULT_DSN: &str = "host=127.0.0.1 port=5433 user=forge dbname=crab";

#[derive(Parser)]
#[command(name = "crabforge", version, about = "Crabforge operator CLI")]
struct Cli {
    /// Crabka broker bootstrap address.
    #[arg(long, short, global = true, env = "CRABFORGE_BOOTSTRAP", default_value = DEFAULT_BOOTSTRAP)]
    bootstrap: String,

    /// gres connection string.
    #[arg(long, global = true, env = "CRABFORGE_DSN", default_value = DEFAULT_DSN)]
    dsn: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Provision every topic the forge needs. Safe to re-run.
    Bootstrap,
    /// Apply any pending schema migrations. Safe to re-run.
    Migrate {
        /// Seconds to wait for gres to accept connections.
        ///
        /// The default suits a pre-start job running alongside a cold database.
        /// Lower it when running interactively, where waiting two minutes to be
        /// told the port is wrong is not useful.
        #[arg(long, default_value_t = migrate::DEFAULT_WAIT_SECS)]
        wait: u64,
    },
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
        Command::Migrate { wait } => migrate::run(&cli.dsn, Duration::from_secs(wait))
            .await
            .context("migrate failed")?,
        Command::Doctor => {
            let report = doctor::run(&cli.bootstrap, &cli.dsn).await;
            report.print();
            if !report.is_healthy() {
                std::process::exit(1);
            }
        }
    }
    Ok(())
}
