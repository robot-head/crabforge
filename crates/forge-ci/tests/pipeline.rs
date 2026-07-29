//! Crab Actions end to end.
//!
//! A real broker, a real gres, a real git repository, and a real shell. What
//! this establishes is the thing no unit test can: that a ref update on the log
//! turns into a run, that its jobs reach a runner, that the steps execute, and
//! that the result comes back through the log into the tables a UI reads.

use std::sync::Arc;

use assert2::check;
use forge_bus::{FencedWriter, PendingRecord, WEBHOOK_TRANSACTIONAL_ID};
use forge_ci::{ProcessSandbox, QueuedJob, RunnerService, SandboxFactory, WORKFLOW_DIR};
use forge_events::{GitRefEvent, RepoEvent};
use forge_git::Cache;
use forge_store::Store;
use forge_testkit::{TestBroker, require_gres};
use forge_types::{Oid, RepoId, UserId, Visibility, topics};

/// Hands out process sandboxes rooted in one temporary directory.
///
/// Not what a deployment uses — see `sandbox`'s module docs — but it is what
/// lets this test exercise the whole pipeline without a container runtime.
struct Processes {
    root: std::path::PathBuf,
}

impl SandboxFactory for Processes {
    type Sandbox = ProcessSandbox;

    fn create(&self, job: &QueuedJob) -> Result<Self::Sandbox, String> {
        let dir = self.root.join(&job.job_id);
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        Ok(ProcessSandbox::new(dir))
    }
}

/// A factory that always refuses, standing in for a missing image.
struct NoSandboxes;

impl SandboxFactory for NoSandboxes {
    type Sandbox = ProcessSandbox;

    fn create(&self, _job: &QueuedJob) -> Result<Self::Sandbox, String> {
        Err("no such image: rust:9.99".into())
    }
}

struct Harness {
    broker: TestBroker,
    _gres: forge_testkit::Gres,
    store: Store,
    dsn: String,
    writer: Arc<FencedWriter>,
    cache_root: tempfile::TempDir,
    work_root: tempfile::TempDir,
}

impl Harness {
    async fn start() -> Option<Self> {
        let gres = require_gres().await?;
        let broker = TestBroker::with_forge_topics().await;
        let dsn = gres.dsn();
        let store = Store::connect(&dsn).await.unwrap();
        store.migrate().await.unwrap();
        let writer = Arc::new(
            FencedWriter::connect_with_id(&broker.bootstrap(), WEBHOOK_TRANSACTIONAL_ID)
                .await
                .unwrap(),
        );
        Some(Self {
            broker,
            _gres: gres,
            store,
            dsn,
            writer,
            cache_root: tempfile::tempdir().unwrap(),
            work_root: tempfile::tempdir().unwrap(),
        })
    }

    async fn store(&self) -> Store {
        Store::connect(&self.dsn).await.unwrap()
    }

    /// Create a repository whose commit contains `workflow`, and announce the
    /// push — everything the orchestrator needs to see.
    async fn push(&self, workflow: &str) -> (RepoId, Oid) {
        let repo_id = RepoId::new();
        let pusher = UserId::new();

        // Provisioned when a repository is created, so the orchestrator can
        // hydrate its cache. Without it there is no object topic to read and
        // the push cannot be planned.
        let mut admin = self.broker.admin().await;
        forge_topics::ensure_repo(&mut admin, repo_id)
            .await
            .unwrap();

        // The repository the orchestrator will look up.
        let commands = FencedWriter::connect(&self.broker.bootstrap())
            .await
            .unwrap();
        let created = RepoEvent::Created {
            repo_id,
            owner_id: pusher,
            owner_name: "octocat".into(),
            name: "hello".into(),
            full_name_lower: "octocat/hello".into(),
            description: None,
            default_branch: "main".into(),
            visibility: Visibility::Public,
        };
        commands
            .transact(vec![PendingRecord::event(&created, None).unwrap()])
            .await
            .unwrap();
        // Applied directly: this test is about CI, not about projection.
        forge_projector::apply_repo_event(&self.store, &created, forge_types::now())
            .await
            .unwrap();

        // A real repository in the cache, at a real commit.
        let cache = Cache::new(self.cache_root.path(), repo_id);
        let head = forge_git::import::make_test_repo(
            &cache.path(),
            &[(&format!("{WORKFLOW_DIR}/build.yml"), workflow.as_bytes())],
        )
        .unwrap();

        let pushed = GitRefEvent::RefUpdated {
            repo_id,
            r#ref: "refs/heads/main".into(),
            old: None,
            new: Some(head),
            pusher,
            forced: false,
        };
        commands
            .transact(vec![PendingRecord::event(&pushed, None).unwrap()])
            .await
            .unwrap();
        (repo_id, head)
    }

    /// Run the orchestrator until it has queued at least one run.
    async fn orchestrate(&self) -> usize {
        let mut orchestrator = forge_ci::Orchestrator::open(
            &self.broker.bootstrap(),
            self.store().await,
            Arc::clone(&self.writer),
            self.cache_root.path(),
        )
        .await
        .unwrap();

        let mut queued = 0;
        for _ in 0..20 {
            queued += orchestrator.step().await.unwrap();
            if queued > 0 {
                break;
            }
        }
        queued
    }

    /// Project every CI event written so far.
    async fn project(&self) {
        let mut projector = forge_projector::Projector::open(
            &self.broker.bootstrap(),
            topics::EVENTS_CI,
            self.store().await,
        )
        .await
        .unwrap();
        projector.drain().await.unwrap();
    }

    /// Run jobs until `want` have been taken.
    async fn run_jobs<F: SandboxFactory>(&self, sandboxes: F, want: usize) -> usize {
        let mut service = RunnerService::open(
            &self.broker.bootstrap(),
            self.store().await,
            Arc::clone(&self.writer),
            sandboxes,
        )
        .await
        .unwrap();

        let mut taken = 0;
        for _ in 0..30 {
            if service.step().await.unwrap() {
                taken += 1;
            }
            if taken >= want {
                break;
            }
        }
        taken
    }

    fn processes(&self) -> Processes {
        Processes {
            root: self.work_root.path().to_path_buf(),
        }
    }
}

const PASSING: &str = "name: build\non: [push]\njobs:\n  test:\n    steps:\n      - run: echo building\n      - run: exit 0\n";
const FAILING: &str = "name: build\non: [push]\njobs:\n  test:\n    steps:\n      - run: echo trying\n      - run: exit 7\n";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_push_runs_its_workflow_and_records_success() {
    let Some(h) = Harness::start().await else {
        return;
    };
    let (repo_id, head) = h.push(PASSING).await;

    check!(h.orchestrate().await == 1, "the push should queue one run");
    h.project().await;

    // The run and its job exist, queued, at the pushed commit.
    let runs = h
        .store
        .ci()
        .runs_for_commit(&head.to_hex(), forge_store::page_size(10))
        .await
        .unwrap();
    check!(runs.len() == 1);
    check!(runs[0].repo_id == repo_id.to_string());
    check!(runs[0].status == "queued");
    check!(runs[0].ref_name == "refs/heads/main");
    check!(runs[0].number == 1, "runs are numbered per repository");

    let jobs = h.store.ci().jobs_of(&runs[0].run_id).await.unwrap();
    check!(jobs.len() == 1);
    check!(jobs[0].name == "test");

    // Run it, then project what the runner said.
    check!(h.run_jobs(h.processes(), 1).await == 1);
    h.project().await;

    let job = h
        .store
        .ci()
        .job_by_id(&jobs[0].job_id)
        .await
        .unwrap()
        .unwrap();
    check!(job.status == "success", "job ended {}", job.status);
    check!(job.exit_code == Some(0));
    check!(job.attempt == 1);

    let run = h
        .store
        .ci()
        .run_by_id(&runs[0].run_id)
        .await
        .unwrap()
        .unwrap();
    check!(run.status == "success", "run ended {}", run.status);
    check!(run.finished_at.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failing_step_fails_the_job_and_the_run() {
    let Some(h) = Harness::start().await else {
        return;
    };
    h.push(FAILING).await;
    h.orchestrate().await;
    h.project().await;
    h.run_jobs(h.processes(), 1).await;
    h.project().await;

    let runs = h
        .store
        .ci()
        .runs_for_repo(
            &h.store
                .ci()
                .runs_for_commit("", forge_store::page_size(1))
                .await
                .unwrap()
                .first()
                .map_or_else(String::new, |r| r.repo_id.clone()),
            forge_store::page_size(10),
        )
        .await
        .unwrap_or_default();
    let _ = runs;

    // Find the job directly — one repository, one run, one job.
    let jobs = h
        .store
        .ci()
        .jobs_of(&last_run(&h).await.run_id)
        .await
        .unwrap();
    check!(jobs[0].status == "failed");
    check!(jobs[0].exit_code == Some(7), "the exit code should survive");

    check!(last_run(&h).await.status == "failed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_sandbox_that_will_not_start_is_not_a_test_failure() {
    // The distinction that stops someone debugging their tests when the image
    // name was wrong: it fails the run, but as infrastructure.
    let Some(h) = Harness::start().await else {
        return;
    };
    h.push(PASSING).await;
    h.orchestrate().await;
    h.project().await;
    h.run_jobs(NoSandboxes, 1).await;
    h.project().await;

    let run = last_run(&h).await;
    let jobs = h.store.ci().jobs_of(&run.run_id).await.unwrap();
    check!(jobs[0].status == "infra_failed");
    check!(
        jobs[0].exit_code.is_none(),
        "there was no exit code to have"
    );
    // And it still fails the run — a green check on a job that never executed
    // is the one outcome CI must never produce.
    check!(run.status == "failed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_repository_with_no_workflows_queues_nothing() {
    let Some(h) = Harness::start().await else {
        return;
    };
    // A workflow that asks for a different event.
    h.push("name: nightly\non: [schedule]\njobs:\n  a:\n    steps:\n      - run: x\n")
        .await;
    check!(h.orchestrate().await == 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_orchestrator_resumes_where_it_left_off() {
    // A restart must not re-queue every push in the repository's history.
    let Some(h) = Harness::start().await else {
        return;
    };
    h.push(PASSING).await;
    check!(h.orchestrate().await == 1);
    check!(
        h.orchestrate().await == 0,
        "a fresh orchestrator re-planned work it had already done"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_jobs_log_reaches_the_log_topic() {
    let Some(h) = Harness::start().await else {
        return;
    };
    h.push(PASSING).await;
    h.orchestrate().await;
    h.project().await;
    h.run_jobs(h.processes(), 1).await;

    // Scanned across every partition: chunks are keyed by job, so which
    // partition they land on is a hash rather than a choice.
    let partitions = forge_topics::static_topics()
        .iter()
        .find(|spec| spec.name == topics::CI_LOGS)
        .map_or(1, |spec| spec.partitions);
    let mut chunks = Vec::new();
    for partition in 0..partitions {
        let mut tailer = forge_bus::Tailer::open_partition_at(
            &h.broker.bootstrap(),
            topics::CI_LOGS,
            partition,
            0,
        )
        .await
        .unwrap();
        tailer
            .replay_to_end(|record| {
                if let Some(value) = record.value.as_deref()
                    && let Ok(json) = serde_json::from_slice::<serde_json::Value>(value)
                {
                    chunks.push(json);
                }
            })
            .await
            .unwrap();
    }
    // Chunks of one job are ordered by their sequence number, which is what a
    // tailing UI reassembles them by.
    chunks.sort_by_key(|c| c["seq"].as_i64().unwrap_or(0));

    check!(!chunks.is_empty(), "no log chunks were written");
    let text: String = chunks
        .iter()
        .filter_map(|c| c["text"].as_str())
        .collect::<Vec<_>>()
        .join("");
    check!(
        text.contains("building"),
        "the step output is missing: {text}"
    );
    check!(text.contains("=== step 1"), "step headers are missing");
    // An explicit end marker, so a tailing UI can stop rather than guess.
    check!(
        chunks.iter().any(|c| c["eof"] == serde_json::json!(true)),
        "no end-of-log marker"
    );
}

/// The most recent run in the forge. These tests create exactly one.
async fn last_run(h: &Harness) -> forge_store::RunRecord {
    let rows = h
        .store
        .client()
        .query("SELECT run_id FROM ci_runs LIMIT 1", &[])
        .await
        .unwrap();
    let run_id: String = rows[0].get(0);
    h.store.ci().run_by_id(&run_id).await.unwrap().unwrap()
}
