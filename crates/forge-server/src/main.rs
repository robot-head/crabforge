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

    /// Set the `Secure` flag on cookies.
    ///
    /// Off by default so the forge works over plain HTTP on a laptop. Turn it
    /// on anywhere reachable from a network — without it, a session cookie
    /// travels in the clear.
    #[arg(long, env = "CRABFORGE_SECURE_COOKIES", default_value_t = false)]
    secure_cookies: bool,
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
        .with_git(&args.cache_root, &args.listen, Arc::clone(&object_writer));

    // One projector per event topic. Each catches up before the server starts
    // listening, so the first request does not race an empty read model.
    for topic in [
        topics::EVENTS_USERS,
        topics::EVENTS_REPOS,
        topics::EVENTS_ISSUES,
        topics::EVENTS_PRS,
        topics::EVENTS_GIT_REFS,
    ] {
        // Its own connection: a projector runs transactions, and a transaction
        // belongs to a session.
        let projector_store = Store::connect(&args.dsn)
            .await
            .with_context(|| format!("connecting a database session for {topic}"))?;
        let mut projector = Projector::open(&args.bootstrap, topic, projector_store)
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

    // Webhook delivery. Its own writer identity: the matcher produces
    // continuously as events land, and sharing the command service's
    // transactional id would mean the first fan-out fenced it.
    let hook_writer = Arc::new(
        forge_bus::FencedWriter::connect_with_id(
            &args.bootstrap,
            forge_bus::WEBHOOK_TRANSACTIONAL_ID,
        )
        .await
        .context("starting the webhook writer")?,
    );
    let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    for topic in forge_hooks::matched_topics() {
        let store = Store::connect(&args.dsn)
            .await
            .with_context(|| format!("connecting a database session for webhooks on {topic}"))?;
        let matcher =
            forge_hooks::Matcher::open(&args.bootstrap, topic, store, Arc::clone(&hook_writer))
                .await
                .with_context(|| format!("opening the webhook matcher for {topic}"))?;
        tokio::spawn(matcher.run(shutdown_rx.clone()));
    }

    // One worker per partition of the delivery queue. The queue is partitioned
    // so that a receiver which has stopped answering holds up its own
    // deliveries and not the whole forge; a single worker would give that up.
    for partition in 0..forge_hooks::Worker::partitions() {
        let store = Store::connect(&args.dsn)
            .await
            .with_context(|| format!("connecting a database session for deliveries {partition}"))?;
        let worker = forge_hooks::Worker::open(
            &args.bootstrap,
            partition,
            store,
            forge_hooks::Deliverer::new(),
            Arc::clone(&hook_writer),
        )
        .await
        .with_context(|| format!("opening the webhook worker for partition {partition}"))?;
        tokio::spawn(worker.run(shutdown_rx.clone()));
    }
    tracing::info!(
        matchers = forge_hooks::matched_topics().len(),
        workers = forge_hooks::Worker::partitions(),
        "webhook delivery running"
    );

    // Stylesheets are embedded in the binary and served by the web router, so
    // a deployment is one file with no asset path to misconfigure.
    let state = state.with_web(&args.cache_root, args.secure_cookies, object_writer);
    let app = router(Arc::new(state));

    let listener = tokio::net::TcpListener::bind(&args.listen)
        .await
        .with_context(|| format!("binding {}", args.listen))?;
    tracing::info!(listen = %args.listen, broker = %args.bootstrap, "crabforge-server started");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")?;
    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutting down");
}
