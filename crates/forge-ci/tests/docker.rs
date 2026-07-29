//! The container sandbox, against a real docker daemon.
//!
//! Skipped where docker is absent, the way the gres-backed tests are: a suite
//! that fails on a machine without docker is telling you about the machine.
//!
//! What matters here is not that a command runs — the process sandbox already
//! proves the runner's logic — but that the isolation is actually applied. A
//! sandbox that silently ran without `--network=none` would pass every test
//! that only checked output.

use std::{collections::BTreeMap, time::Duration};

use assert2::check;
use forge_ci::{
    DockerSandboxes, QueuedJob, Sandbox, SandboxFactory, StepOutcome, docker_available,
    plan::PlannedJob,
};

const IMAGE: &str = "alpine:3.21";

fn job(image: &str) -> QueuedJob {
    QueuedJob {
        job_id: format!("test-{}", forge_types::JobId::new()),
        run_id: "run-1".into(),
        repo_id: "repo-1".into(),
        head_oid: "abc".into(),
        job: PlannedJob {
            name: "test".into(),
            image: image.into(),
            timeout_minutes: 5,
            env: Vec::new(),
            steps: Vec::new(),
        },
    }
}

/// Run one command in a container, or `None` when docker is unavailable.
async fn run(command: &str, env: BTreeMap<String, String>) -> Option<(StepOutcome, Vec<String>)> {
    if !docker_available().await {
        eprintln!("SKIP: no docker daemon");
        return None;
    }
    let root = tempfile::tempdir().unwrap();
    let sandboxes = DockerSandboxes::new(root.path());
    let sandbox = sandboxes.create(&job(IMAGE)).expect("sandbox");

    let mut lines = Vec::new();
    let result = sandbox
        .run_step(command, &env, Duration::from_secs(120), &mut |line| {
            lines.push(line.to_string())
        })
        .await;
    Some((result.outcome, lines))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_step_runs_in_the_container_and_reports_its_output() {
    let Some((outcome, lines)) = run("echo hello from a container", BTreeMap::new()).await else {
        return;
    };
    check!(outcome == StepOutcome::Succeeded, "{lines:?}");
    check!(lines.iter().any(|l| l.contains("hello from a container")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failing_command_carries_its_exit_code() {
    let Some((outcome, _)) = run("exit 5", BTreeMap::new()).await else {
        return;
    };
    check!(outcome == StepOutcome::Failed { exit_code: 5 });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_container_has_no_network() {
    // The isolation that matters most: a build that can reach the internet is a
    // build whose result depends on it, and the easiest exfiltration path for
    // anything the job can see.
    let Some((outcome, lines)) = run(
        "wget -q -T 3 -O - http://example.com >/dev/null 2>&1; echo exit=$?",
        BTreeMap::new(),
    )
    .await
    else {
        return;
    };
    check!(
        outcome == StepOutcome::Succeeded,
        "the probe itself should run"
    );
    check!(
        lines
            .iter()
            .any(|l| l.starts_with("exit=") && l != "exit=0"),
        "the container reached the network: {lines:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_root_filesystem_is_read_only_but_the_workspace_is_not() {
    // A build must be able to build. What it must not do is modify the image
    // it was given, which is how one job would affect the next.
    let Some((outcome, lines)) = run(
        "touch /workspace/ok && echo workspace=yes; touch /etc/passwd 2>/dev/null \
         && echo root=writable || echo root=readonly",
        BTreeMap::new(),
    )
    .await
    else {
        return;
    };
    check!(outcome == StepOutcome::Succeeded, "{lines:?}");
    check!(lines.iter().any(|l| l == "workspace=yes"), "{lines:?}");
    check!(lines.iter().any(|l| l == "root=readonly"), "{lines:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_the_declared_environment_reaches_the_container() {
    // Passed explicitly rather than inherited, so nothing the runner process
    // holds — broker addresses, database credentials — leaks into a build.
    //
    // `CARGO_PKG_NAME` stands in for those: cargo sets it in this process, so
    // if the container can see it the container is seeing the host's
    // environment. (Setting one specially would need `unsafe`, which this
    // workspace forbids.)
    check!(
        std::env::var("CARGO_PKG_NAME").as_deref() == Ok("forge-ci"),
        "the probe variable is not set; this test proves nothing"
    );
    let mut env = BTreeMap::new();
    env.insert("DECLARED".to_string(), "yes".to_string());

    let Some((_, lines)) = run("echo declared=$DECLARED leaked=[$CARGO_PKG_NAME]", env).await
    else {
        return;
    };
    let joined = lines.join("\n");
    check!(joined.contains("declared=yes"), "{joined}");
    check!(
        joined.contains("leaked=[]"),
        "the runner's environment leaked into the container: {joined}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_image_is_an_infrastructure_failure() {
    if !docker_available().await {
        eprintln!("SKIP: no docker daemon");
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let sandboxes = DockerSandboxes::new(root.path());
    let sandbox = sandboxes
        .create(&job("crabforge.invalid/no-such-image:9.99"))
        .expect("sandbox");

    let result = sandbox
        .run_step(
            "true",
            &BTreeMap::new(),
            Duration::from_secs(120),
            &mut |_| {},
        )
        .await;

    // Not a build failure: nothing was learned about the code, and reporting it
    // as one sends somebody to debug their tests.
    check!(
        result.outcome == StepOutcome::InfraFailed,
        "got {:?}",
        result.outcome
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_container_does_not_run_as_root() {
    // A build has no business being root, and this is easy to regress: the
    // `--user` flag is one line among a dozen, and dropping it would leave
    // every other isolation test still passing.
    let Some((outcome, lines)) = run("id -u", BTreeMap::new()).await else {
        return;
    };
    check!(outcome == StepOutcome::Succeeded, "{lines:?}");
    check!(
        lines
            .iter()
            .any(|l| l.trim() != "0" && !l.trim().is_empty()),
        "the container ran as root: {lines:?}"
    );
}
