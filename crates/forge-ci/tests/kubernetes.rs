//! The pod sandbox, against a real cluster.
//!
//! Skipped where no cluster answers, the way the docker-backed tests skip
//! without a daemon. `kind create cluster` is enough; nothing here needs a real
//! one.
//!
//! The unit tests next to the sandbox assert what the manifest *says*. These
//! assert what the cluster *does* with it, which is not the same thing: a
//! `securityContext` field the API server silently drops, an admission
//! controller that rewrites it, or a field name that moved between API versions
//! would all leave the manifest tests green.

use std::{collections::BTreeMap, time::Duration};

use assert2::check;
use forge_ci::{
    KubernetesSandboxes, PlannedJob, QueuedJob, Sandbox, SandboxFactory, StepOutcome,
    kubernetes_available,
};

/// Small, has a shell, and is already on most machines.
const IMAGE: &str = "alpine:3.21";

/// Generous: it includes pulling the image on a cold node.
const STEP_TIMEOUT: Duration = Duration::from_secs(240);

fn job(image: &str) -> QueuedJob {
    // Lowercase hex, so it makes a legal pod name.
    let id = forge_types::JobId::new()
        .to_string()
        .to_lowercase()
        .replace(|c: char| !c.is_ascii_alphanumeric(), "");
    QueuedJob {
        job_id: id,
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

/// Namespace the tests create their pods in. Left behind between runs on
/// purpose: a namespace is cheap and deleting one takes a slow finalizer pass.
const NAMESPACE: &str = "crabforge-ci-test";

/// How many of these tests may hold a pod at once.
///
/// `cargo test` runs the file's tests concurrently, and a single-node `kind`
/// cluster scheduling nine pods at once — one of which is deliberately
/// unpullable and sits in `ImagePullBackOff` until its deadline — starves the
/// rest, which then fail as infrastructure. That is a fact about the fixture
/// rather than about the sandbox, and a suite that reports it as a code failure
/// is worse than useless. Crabka's own share-group tests bound their
/// concurrency for the same reason.
static POD_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

/// Create the test namespace, enforcing the same Pod Security level
/// `deploy/k8s/00-namespaces.yaml` puts on the real one.
///
/// Not decoration: `restricted` is admission refusing any pod that is not
/// non-root, without privilege escalation, with all capabilities dropped and a
/// seccomp profile set. Running these tests in a permissive namespace would let
/// a manifest that had quietly lost one of those pass every assertion below,
/// and then be rejected on the cluster it was written for.
async fn ensure_namespace() {
    let namespace = serde_json::json!({
        "apiVersion": "v1",
        "kind": "Namespace",
        "metadata": {
            "name": NAMESPACE,
            "labels": {
                "pod-security.kubernetes.io/enforce": "restricted",
                "pod-security.kubernetes.io/enforce-version": "latest",
            },
        },
    });
    apply(&namespace).await;
}

/// Run one command in a pod, or `None` when no cluster answers.
async fn run(
    command: &str,
    env: BTreeMap<String, String>,
) -> Option<(StepOutcome, Vec<String>, Option<String>)> {
    if !kubernetes_available().await {
        eprintln!("SKIP: no reachable Kubernetes cluster (try `kind create cluster`)");
        return None;
    }
    ensure_namespace().await;
    let _slot = POD_SLOTS.acquire().await.expect("a pod slot");

    let sandboxes = KubernetesSandboxes::new(NAMESPACE);
    let sandbox = sandboxes.create(&job(IMAGE)).expect("sandbox");

    let mut lines = Vec::new();
    let result = sandbox
        .run_step(command, &env, STEP_TIMEOUT, &mut |line| {
            lines.push(line.to_string())
        })
        .await;
    Some((result.outcome, lines, result.detail))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_step_runs_in_the_pod_and_reports_its_output() {
    let Some((outcome, lines, detail)) = run("echo hello from a pod", BTreeMap::new()).await else {
        return;
    };
    check!(outcome == StepOutcome::Succeeded, "{detail:?} {lines:?}");
    check!(lines.iter().any(|l| l == "hello from a pod"), "{lines:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failing_command_carries_its_exit_code() {
    let Some((outcome, _, _)) = run("exit 7", BTreeMap::new()).await else {
        return;
    };
    check!(outcome == StepOutcome::Failed { exit_code: 7 });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn several_steps_share_one_workspace() {
    // The reason the pod is per job rather than per step. A pod per step would
    // pass every other test here and fail every real workflow, because the
    // second step would not find what the first one built.
    if !kubernetes_available().await {
        eprintln!("SKIP: no reachable Kubernetes cluster");
        return;
    }
    ensure_namespace().await;
    let _slot = POD_SLOTS.acquire().await.expect("a pod slot");
    let sandbox = KubernetesSandboxes::new(NAMESPACE)
        .create(&job(IMAGE))
        .unwrap();

    let mut lines = Vec::new();
    let first = sandbox
        .run_step(
            "echo built > artifact",
            &BTreeMap::new(),
            STEP_TIMEOUT,
            &mut |_| {},
        )
        .await;
    check!(
        first.outcome == StepOutcome::Succeeded,
        "{:?}",
        first.detail
    );

    let second = sandbox
        .run_step(
            "cat artifact",
            &BTreeMap::new(),
            STEP_TIMEOUT,
            &mut |line| lines.push(line.to_string()),
        )
        .await;
    check!(
        second.outcome == StepOutcome::Succeeded,
        "{:?}",
        second.detail
    );
    check!(lines.iter().any(|l| l == "built"), "{lines:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_pod_does_not_run_as_root() {
    let Some((outcome, lines, detail)) = run("id -u", BTreeMap::new()).await else {
        return;
    };
    check!(outcome == StepOutcome::Succeeded, "{detail:?}");
    check!(lines.iter().any(|l| l.trim() == "65534"), "{lines:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_root_filesystem_is_read_only_but_the_workspace_is_not() {
    let Some((outcome, lines, _)) = run(
        "touch /workspace/ok && echo workspace-writable; \
         touch /etc/nope 2>/dev/null && echo root-writable || echo root-read-only",
        BTreeMap::new(),
    )
    .await
    else {
        return;
    };
    check!(outcome == StepOutcome::Succeeded, "{lines:?}");
    check!(lines.iter().any(|l| l == "workspace-writable"), "{lines:?}");
    check!(lines.iter().any(|l| l == "root-read-only"), "{lines:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_pod_has_no_api_credentials() {
    // The one that matters most on a cluster: the default is to mount a
    // service-account token, and a token is read access to the namespace's
    // secrets from inside a build that runs pull-request code.
    let Some((outcome, lines, _)) = run(
        "test -e /var/run/secrets/kubernetes.io && echo token-mounted || echo no-token",
        BTreeMap::new(),
    )
    .await
    else {
        return;
    };
    check!(outcome == StepOutcome::Succeeded);
    check!(lines.iter().any(|l| l == "no-token"), "{lines:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_the_declared_environment_reaches_the_pod() {
    // Three things at once: the declared variable arrives, the runner's own
    // environment — which in a deployment holds the broker address and the
    // database password — does not, and neither do the namespace's service
    // addresses (`enableServiceLinks: false`).
    //
    // `CARGO_PKG_NAME` stands in for the runner's environment because cargo
    // sets it in this process and nothing sets it in a pod. Setting a variable
    // here instead would need `unsafe`, which this workspace forbids — and
    // rightly: `set_var` races every other thread's `getenv`.
    check!(
        std::env::var("CARGO_PKG_NAME").is_ok(),
        "the stand-in for a leaked variable is not set, so this proves nothing"
    );
    let mut env = BTreeMap::new();
    env.insert("DECLARED".to_string(), "arrived".to_string());

    let Some((outcome, lines, _)) = run(
        "echo \"declared=$DECLARED\"; echo \"leak=${CARGO_PKG_NAME:-absent}\"; \
         echo \"links=${KUBERNETES_PORT:-absent}\"",
        env,
    )
    .await
    else {
        return;
    };
    check!(outcome == StepOutcome::Succeeded);
    check!(lines.iter().any(|l| l == "declared=arrived"), "{lines:?}");
    check!(lines.iter().any(|l| l == "leak=absent"), "{lines:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pod_does_not_learn_the_addresses_of_its_neighbours() {
    // What `enableServiceLinks: false` is for. With it on — the default — every
    // service in the namespace is injected as `<NAME>_SERVICE_HOST` and friends,
    // which hands a build a map of everything it might reach.
    //
    // The `kubernetes` service is the documented exception: the kubelet always
    // injects `KUBERNETES_SERVICE_HOST` / `KUBERNETES_PORT*` regardless of this
    // field. That is the API server's address, which is not a secret and is not
    // usable without the token the pod does not have (see
    // `the_pod_has_no_api_credentials`) or through the default-deny
    // NetworkPolicy this namespace is meant to carry.
    if !kubernetes_available().await {
        eprintln!("SKIP: no reachable Kubernetes cluster");
        return;
    }
    ensure_namespace().await;

    // A service needs no backing pod to produce link variables — only to exist
    // before the pod is created, which is why this is applied first.
    let service = serde_json::json!({
        "apiVersion": "v1",
        "kind": "Service",
        "metadata": {"name": "neighbour", "namespace": NAMESPACE},
        "spec": {"ports": [{"port": 8080}], "selector": {"app": "nothing"}},
    });
    apply(&service).await;

    let _slot = POD_SLOTS.acquire().await.expect("a pod slot");
    let sandbox = KubernetesSandboxes::new(NAMESPACE)
        .create(&job(IMAGE))
        .unwrap();
    let mut lines = Vec::new();
    let result = sandbox
        .run_step(
            "echo \"neighbour=${NEIGHBOUR_SERVICE_HOST:-absent}\"; \
             echo \"api=${KUBERNETES_SERVICE_HOST:-absent}\"",
            &BTreeMap::new(),
            STEP_TIMEOUT,
            &mut |line| lines.push(line.to_string()),
        )
        .await;

    check!(
        result.outcome == StepOutcome::Succeeded,
        "{:?}",
        result.detail
    );
    check!(
        lines.iter().any(|l| l == "neighbour=absent"),
        "the pod was told where its neighbours are: {lines:?}"
    );
    // Pinned so the exception stays a known one rather than a surprise.
    check!(
        lines
            .iter()
            .any(|l| l == "api=10.96.0.1" || l.starts_with("api=")),
        "{lines:?}"
    );
}

/// `kubectl apply` a manifest, ignoring an already-exists.
async fn apply(manifest: &serde_json::Value) {
    use tokio::io::AsyncWriteExt as _;
    let mut child = tokio::process::Command::new("kubectl")
        .args(["apply", "-f", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("kubectl");
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(&serde_json::to_vec(manifest).unwrap())
            .await
            .unwrap();
    }
    let _ = child.wait().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_image_is_an_infrastructure_failure() {
    // Not a build failure: nothing was learned about the code, and reporting it
    // as one sends someone to debug tests that never ran.
    if !kubernetes_available().await {
        eprintln!("SKIP: no reachable Kubernetes cluster");
        return;
    }
    ensure_namespace().await;
    let _slot = POD_SLOTS.acquire().await.expect("a pod slot");
    let sandbox = KubernetesSandboxes::new(NAMESPACE)
        .create(&job("crabforge.invalid/no-such-image:9.99"))
        .unwrap();

    // Short: the pod will never start, and the point is the classification.
    let result = sandbox
        .run_step(
            "true",
            &BTreeMap::new(),
            Duration::from_secs(120),
            &mut |_| {},
        )
        .await;

    check!(result.outcome == StepOutcome::InfraFailed);
    check!(result.outcome.exit_code().is_none());
    // And it says why, rather than "timed out".
    let detail = result.detail.unwrap_or_default();
    check!(
        detail.contains("Pull") || detail.contains("pull"),
        "the failure did not name the cause: {detail}"
    );
}
