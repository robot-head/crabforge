//! `crabforge-server` — the forge process.
//!
//! Assembles every role in one process for now: HTTP, the command service, and
//! the projectors. See `state.rs` for why.

use std::{sync::Arc, time::Duration};

use anyhow::{Context as _, Result};
use clap::Parser;
use forge_command::CommandService;
use forge_projector::Projector;

mod telemetry;
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

    /// Address to serve `/metrics` and the profiling endpoints on.
    ///
    /// Separate from `--listen` because neither belongs on a public interface:
    /// the metric labels enumerate repositories and a profile is a snapshot of
    /// the process's stacks. Bind it to loopback or a cluster-internal address.
    #[arg(long, env = "CRABFORGE_ADMIN_LISTEN", default_value = "127.0.0.1:7101")]
    admin_listen: String,

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

    /// How many CI runners to start in this process.
    ///
    /// Zero disables CI here without disabling it for the forge — another
    /// process joining the same share group picks the work up.
    #[arg(long, env = "CRABFORGE_CI_RUNNERS", default_value_t = 2)]
    ci_runners: usize,

    /// Where CI jobs run.
    #[arg(long, env = "CRABFORGE_CI_SANDBOX", value_enum, default_value_t = CiSandbox::Docker)]
    ci_sandbox: CiSandbox,

    /// Namespace for CI job pods, with `--ci-sandbox=kubernetes`.
    ///
    /// Its own namespace rather than the forge's: the default-deny
    /// NetworkPolicy and the resource quota that isolate a job are
    /// namespace-scoped, and sharing would apply them to the forge too.
    #[arg(long, env = "CRABFORGE_CI_NAMESPACE", default_value = "crabforge-ci")]
    ci_namespace: String,

    /// Which part of the forge this process is.
    #[arg(long, env = "CRABFORGE_ROLE", value_enum, default_value_t = Role::All)]
    role: Role,

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
    // Held for the process's lifetime: dropping the guard flushes, and the
    // spans of a shutdown are the ones worth having.
    let _telemetry = telemetry::init("crabforge-server")?;

    let args = Args::parse();
    match args.role {
        Role::All => serve_everything(args).await,
        Role::Runner => serve_runners(args).await,
    }
}

/// The whole forge in one process.
async fn serve_everything(args: Args) -> Result<()> {
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

    // Crab Actions. The orchestrator watches pushes and plans runs; the runners
    // drain the share-group queue. Both take the webhook writer's identity —
    // they are consequences of decisions already committed, not decisions.
    let orchestrator = forge_ci::Orchestrator::open(
        &args.bootstrap,
        Store::connect(&args.dsn)
            .await
            .context("connecting a database session for CI orchestration")?,
        Arc::clone(&hook_writer),
        &args.cache_root,
    )
    .await
    .context("opening the CI orchestrator")?;
    tokio::spawn(orchestrator.run(shutdown_rx.clone()));

    let ci_workspaces = args.cache_root.join("ci");
    let started = match args.ci_sandbox {
        CiSandbox::Docker => {
            start_runners(&args, &hook_writer, &shutdown_rx, || {
                forge_ci::DockerSandboxes::new(ci_workspaces.clone())
            })
            .await?
        }
        CiSandbox::Kubernetes => {
            start_runners(&args, &hook_writer, &shutdown_rx, || {
                forge_ci::KubernetesSandboxes::new(args.ci_namespace.clone())
            })
            .await?
        }
    };
    tracing::info!(
        runners = started,
        sandbox = ?args.ci_sandbox,
        "Crab Actions running"
    );

    // The queue depth an autoscaler reads. Polled rather than maintained
    // in-process: the number that matters is the whole forge's backlog, and a
    // counter kept here would only ever know about the jobs this process
    // handled — so a second replica would report a second, smaller truth.
    let queue_depth_store = Store::connect(&args.dsn)
        .await
        .context("connecting a database session for the queue-depth gauge")?;
    tokio::spawn(publish_queue_depth(queue_depth_store, shutdown_rx.clone()));

    tokio::spawn({
        let addr = args.admin_listen.clone();
        async move { forge_metrics::serve(&addr).await }
    });

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

/// Which part of the forge a process is.
///
/// Two, not one per component, and the reason is the command service: it is a
/// fenced single writer, and a second one does not share the work — it fences
/// the first, which then stops. So "run more copies" is only a valid answer for
/// a role that has no writer in it, and the CI runners are that role. Splitting
/// the web tier out later is possible on the same terms; splitting the command
/// service is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Role {
    /// Everything in one process. What a laptop and a small deployment want.
    All,
    /// CI runners only: no HTTP, no command service, no projectors.
    ///
    /// Scales from zero, because the share group hands work to whoever is
    /// there and nothing breaks when nobody is.
    Runner,
}

/// Where CI jobs run.
///
/// `ProcessSandbox` is deliberately absent: it isolates nothing, and a flag
/// that let a deployment select it would be a flag that let a deployment hand
/// shell access to its own host to anyone who can open a pull request.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum CiSandbox {
    /// One container per job, through the local docker daemon.
    Docker,
    /// One pod per job, through `kubectl` against the current context.
    Kubernetes,
}

/// CI runners and nothing else.
///
/// No command service, so this scales horizontally; no projectors, so it does
/// not compete for a cursor; no HTTP beyond health, because there is nothing to
/// serve. What it does need is the database — the claim that stops two runners
/// executing one job is a compare-and-swap in gres — and the log, to report
/// what happened.
async fn serve_runners(args: Args) -> Result<()> {
    let store = Store::connect_with_retry(&args.dsn, GRES_STARTUP_BUDGET)
        .await
        .context("connecting to gres")?;
    store
        .require_current_schema()
        .await
        .context("schema check — run `crabforge migrate`")?;
    drop(store);

    // The webhook writer's identity rather than the command service's: a runner
    // reports consequences of decisions already committed, and taking the
    // command service's transactional id would fence the process that makes
    // them.
    let writer = Arc::new(
        forge_bus::FencedWriter::connect_with_id(
            &args.bootstrap,
            forge_bus::WEBHOOK_TRANSACTIONAL_ID,
        )
        .await
        .context("starting the runner's writer")?,
    );
    let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    let ci_workspaces = args.cache_root.join("ci");
    let started = match args.ci_sandbox {
        CiSandbox::Docker => {
            std::fs::create_dir_all(&ci_workspaces).context("creating the workspace directory")?;
            start_runners(&args, &writer, &shutdown_rx, || {
                forge_ci::DockerSandboxes::new(ci_workspaces.clone())
            })
            .await?
        }
        CiSandbox::Kubernetes => {
            start_runners(&args, &writer, &shutdown_rx, || {
                forge_ci::KubernetesSandboxes::new(args.ci_namespace.clone())
            })
            .await?
        }
    };
    if started == 0 {
        // Unlike the all-in-one process, there is nothing else for this one to
        // do. Exiting lets the orchestrator restart it, and makes the failure
        // visible as a crash loop rather than as a pod that is Ready and idle.
        anyhow::bail!("no CI runners could be started; this process has no other purpose");
    }
    tracing::info!(runners = started, sandbox = ?args.ci_sandbox, "CI runners running");

    tokio::spawn({
        let addr = args.admin_listen.clone();
        async move { forge_metrics::serve(&addr).await }
    });

    // Health only: an orchestrator needs somewhere to probe, and a runner has
    // no other HTTP surface.
    let app = forge_server::health_router(Arc::new(AppState::new(&args.bootstrap)));
    let listener = tokio::net::TcpListener::bind(&args.listen)
        .await
        .with_context(|| format!("binding {}", args.listen))?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("health server error")?;
    Ok(())
}

/// Start `args.ci_runners` runners, each with its own sandbox factory.
///
/// Generic over the factory rather than boxing it: `SandboxFactory` has an
/// associated type, so a trait object would need the sandbox boxed too, and
/// `Sandbox::run_step` is an `async fn` in a trait.
async fn start_runners<F, M>(
    args: &Args,
    writer: &Arc<forge_bus::FencedWriter>,
    shutdown: &tokio::sync::watch::Receiver<bool>,
    make: M,
) -> Result<usize>
where
    F: forge_ci::SandboxFactory + Send + Sync + 'static,
    F::Sandbox: Send + Sync,
    M: Fn() -> F,
{
    let mut started = 0;
    for worker in 0..args.ci_runners {
        let store = Store::connect(&args.dsn)
            .await
            .with_context(|| format!("connecting a database session for runner {worker}"))?;
        match forge_ci::RunnerService::open(&args.bootstrap, store, Arc::clone(writer), make())
            .await
        {
            Ok(runner) => {
                tokio::spawn(runner.run(shutdown.clone()));
                started += 1;
            }
            Err(e) => {
                // Worth saying loudly, and worth continuing without: a forge
                // that serves git and no CI is far more useful than one that
                // refuses to start.
                tracing::error!(error = %e, "CI runners unavailable; the forge will serve without CI");
                break;
            }
        }
    }
    Ok(started)
}

/// How often the CI queue depth is re-read.
///
/// A scaler's polling interval is measured in tens of seconds, so anything
/// faster is a query gres runs for nobody.
const QUEUE_DEPTH_INTERVAL: Duration = Duration::from_secs(5);

/// Keep `forge_ci_jobs_queued` current until shutdown.
async fn publish_queue_depth(store: Store, mut shutdown: tokio::sync::watch::Receiver<bool>) {
    let mut ticker = tokio::time::interval(QUEUE_DEPTH_INTERVAL);
    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    return;
                }
            }
            _ = ticker.tick() => match store.ci().queued_jobs().await {
                Ok(queued) => forge_metrics::set_jobs_queued(queued),
                // Logged at debug: gres being briefly unavailable is not news,
                // and a scaler that sees a stale gauge holds its replica count
                // rather than scaling to zero — which is the safe direction.
                Err(error) => tracing::debug!(%error, "could not read the CI queue depth"),
            },
        }
    }
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutting down");
}
