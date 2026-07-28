//! `crabforge-server` — the forge process.

use std::sync::Arc;

use anyhow::{Context as _, Result};
use clap::Parser;
use forge_server::{AppState, router};

#[derive(Parser)]
#[command(name = "crabforge-server", version, about = "The Crabforge server")]
struct Args {
    /// Address to serve HTTP on.
    #[arg(long, env = "CRABFORGE_LISTEN", default_value = "127.0.0.1:7000")]
    listen: String,

    /// Crabka broker bootstrap address.
    #[arg(long, env = "CRABFORGE_BOOTSTRAP", default_value = "127.0.0.1:9092")]
    bootstrap: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    let state = Arc::new(AppState::new(&args.bootstrap));

    let listener = tokio::net::TcpListener::bind(&args.listen)
        .await
        .with_context(|| format!("binding {}", args.listen))?;
    tracing::info!(listen = %args.listen, broker = %args.bootstrap, "crabforge-server started");

    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")?;
    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutting down");
}
