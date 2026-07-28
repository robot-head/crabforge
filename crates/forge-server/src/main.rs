//! `crabforge-server` — the forge process.
//!
//! Assembles every role in one process for now: HTTP, the command service, and
//! the projectors. See `state.rs` for why.

use std::{sync::Arc, time::Duration};

use anyhow::{Context as _, Result};
use clap::Parser;
use forge_command::CommandService;
use forge_projector::Projector;
use forge_server::{AppState, router};
use forge_store::Store;
use forge_types::topics;

/// How long to wait for gres. Substrate-mode gres replays its whole
/// write-ahead log on a cold start, so this is generous.
const GRES_STARTUP_BUDGET: Duration = Duration::from_secs(120);

#[derive(Parser)]
#[command(name = "crabforge-server", version, about = "The Crabforge server")]
struct Args {
    /// Address to serve HTTP on.
    #[arg(long, env = "CRABFORGE_LISTEN", default_value = "127.0.0.1:7000")]
    listen: String,

    /// Crabka broker bootstrap address.
    #[arg(long, env = "CRABFORGE_BOOTSTRAP", default_value = "127.0.0.1:9092")]
    bootstrap: String,

    /// gres connection string.
    #[arg(
        long,
        env = "CRABFORGE_DSN",
        default_value = "host=127.0.0.1 port=5433 user=forge dbname=crab"
    )]
    dsn: String,

    /// Where per-repository git caches live.
    ///
    /// Disposable: every byte under here is rebuilt from the log on demand, so
    /// it can be a container's ephemeral disk.
    #[arg(long, env = "CRABFORGE_CACHE", default_value = ".dev/git-cache")]
    cache_root: std::path::PathBuf,
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

    let store = Arc::new(
        Store::connect_with_retry(&args.dsn, GRES_STARTUP_BUDGET)
            .await
            .context("connecting to gres")?,
    );
    store
        .require_current_schema()
        .await
        .context("schema check — run `crabforge migrate`")?;

    // Fences any predecessor and rebuilds decision state before serving.
    let commands = CommandService::start(&args.bootstrap)
        .await
        .context("starting the command service")?;

    std::fs::create_dir_all(&args.cache_root).context("creating the git cache directory")?;

    // Git objects get their own transactional identity: sharing the command
    // service's would fence it on the first push.
    let object_writer = Arc::new(
        forge_git::connect_object_writer(&args.bootstrap)
            .await
            .context("connecting the object writer")?,
    );

    let mut state = AppState::new(&args.bootstrap)
        .with_commands(Arc::clone(&commands))
        .with_store(Arc::clone(&store))
        .with_git(&args.cache_root, &args.listen, object_writer);

    // One projector per event topic. Each catches up before the server starts
    // listening, so the first request does not race an empty read model.
    for topic in [topics::EVENTS_USERS, topics::EVENTS_REPOS] {
        let mut projector = Projector::open(&args.bootstrap, topic, Arc::clone(&store))
            .await
            .with_context(|| format!("opening projector for {topic}"))?;
        let applied = projector.applied();
        let caught_up = projector
            .drain()
            .await
            .with_context(|| format!("draining {topic}"))?;
        tracing::info!(topic, events = caught_up, "projector caught up");

        state = state.with_projection(topic, applied);
        tokio::spawn(async move {
            if let Err(e) = projector.run().await {
                tracing::error!(error = %e, "projector stopped");
            }
        });
    }

    let listener = tokio::net::TcpListener::bind(&args.listen)
        .await
        .with_context(|| format!("binding {}", args.listen))?;
    tracing::info!(listen = %args.listen, broker = %args.bootstrap, "crabforge-server started");

    axum::serve(listener, router(Arc::new(state)))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")?;
    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutting down");
}
