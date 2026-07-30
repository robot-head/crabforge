//! Running a job in a pod.
//!
//! The same job the [`DockerSandbox`](crate::DockerSandbox) runs, on a cluster
//! instead of a host — which is what makes the runners horizontally scalable
//! rather than limited to the machines someone has installed docker on. A
//! runner using this holds no docker socket, which is worth saying plainly: a
//! mounted docker socket is root on the node, and handing one to a process that
//! executes pull-request code is the single worst thing a forge can do.
//!
//! Driven through `kubectl` for the same reason the docker sandbox is driven
//! through the CLI: it already has the operator's kubeconfig, context, and
//! in-cluster service-account credentials, and matching that exactly matters
//! more than avoiding a subprocess. It also keeps `kube`/`k8s-openapi` — and
//! their transitive TLS stack — out of every forge binary.
//!
//! ## One pod per job, not per step
//!
//! The docker sandbox runs each step as its own `docker run` and keeps the
//! workspace in a host bind mount. There is no equivalent of a host bind mount
//! here, so a pod per step would start every step in an empty directory. The
//! pod is therefore created once, sleeps for the job's budget, and each step
//! runs through `kubectl exec` against it. `activeDeadlineSeconds` means the
//! pod removes itself even if the runner that created it dies.
//!
//! ## What the pod is denied
//!
//! Everything the container sandbox denies, plus the two that only exist here:
//!
//! * **No service-account token.** The default is to mount one, and a token is
//!   an API credential — a build that can talk to the API server can read every
//!   secret its namespace can.
//! * **No map of the namespace** (`enableServiceLinks: false`). By default
//!   every service in the namespace is injected as `<NAME>_SERVICE_HOST` and
//!   friends, which tells a build exactly what is worth trying to reach. The
//!   one exception is the kubelet's own `KUBERNETES_SERVICE_HOST`, which is
//!   injected regardless of this field; that is the API server's address, which
//!   is not a secret and is not usable without the token the pod does not have.
//! * Not root, no capabilities, no privilege escalation, a read-only root
//!   filesystem with writable `/workspace` and `/tmp`, and capped memory and
//!   CPU.
//!
//! Two things the container sandbox denies and this one cannot, both because
//! they are properties of the cluster rather than of a pod, and both therefore
//! documented rather than claimed:
//!
//! * **The network.** Docker has `--network=none`; Kubernetes has no per-pod
//!   equivalent, because pod networking is the CNI's business. Denying it takes
//!   a default-deny `NetworkPolicy` in the namespace — one ships in
//!   `deploy/k8s/` — *and* a CNI that enforces NetworkPolicy at all. On a
//!   cluster without one the policy is accepted and does nothing.
//! * **The process count.** `DockerSandbox` passes `--pids-limit=512`; a pod
//!   spec has no such field. The equivalent is the kubelet's `podPidsLimit`,
//!   which is node configuration. Without it, a fork bomb in a pull request
//!   exhausts the node's process table and takes the runner and its neighbours
//!   with it. Set it on any node pool that runs CI.

use std::{collections::BTreeMap, process::Stdio, time::Duration};

use crate::{
    queue::QueuedJob,
    sandbox::{Sandbox, StepOutcome, StepResult},
    service::SandboxFactory,
};

/// Memory ceiling for a job's pod. Matches the docker sandbox.
const MEMORY_LIMIT: &str = "2Gi";

/// CPU ceiling. Unlike memory this only throttles, so it is a fairness knob
/// rather than a safety one.
const CPU_LIMIT: &str = "2";

/// Uid and gid the job runs as. `nobody` on essentially every base image.
const RUN_AS: i64 = 65534;

/// How long to wait for a pod to be running before calling it an
/// infrastructure failure.
///
/// Generous because it includes pulling the image, which on a cold node for a
/// large toolchain image is minutes rather than seconds.
const START_BUDGET: Duration = Duration::from_secs(300);

/// Slack added to a job's own timeout before the cluster kills the pod.
///
/// The runner enforces the real timeout per step and reports it properly; this
/// is the backstop for a runner that died, so it should fire strictly later
/// than the runner would have.
const DEADLINE_SLACK: Duration = Duration::from_secs(120);

/// Hands out one pod per job.
pub struct KubernetesSandboxes {
    namespace: String,
}

impl KubernetesSandboxes {
    /// Run jobs in `namespace`.
    ///
    /// Its own namespace, separate from the forge's: the isolation that matters
    /// — the default-deny NetworkPolicy, the resource quota, the pod security
    /// admission level — is all namespace-scoped, and sharing a namespace with
    /// the forge would mean applying it to the forge too.
    pub fn new(namespace: impl Into<String>) -> Self {
        Self {
            namespace: namespace.into(),
        }
    }
}

impl SandboxFactory for KubernetesSandboxes {
    type Sandbox = KubernetesSandbox;

    fn create(&self, job: &QueuedJob) -> Result<Self::Sandbox, String> {
        // A DNS-1123 name: lowercase alphanumerics and dashes, 63 characters.
        // Job ids are ULIDs or uuids, so this is a prefix rather than a
        // sanitiser — but it is still checked, because a name the API server
        // rejects would surface as a mysterious infrastructure failure per job.
        let pod = format!("crabforge-job-{}", job.job_id.to_lowercase());
        if pod.len() > 63 || !pod.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
            return Err(format!("job id {} does not fit a pod name", job.job_id));
        }

        Ok(KubernetesSandbox {
            namespace: self.namespace.clone(),
            pod,
            image: job.job.image.clone(),
            budget: Duration::from_secs(u64::from(job.job.timeout_minutes.max(1)) * 60),
            started: tokio::sync::OnceCell::new(),
        })
    }
}

/// A pod to run one job's steps in.
pub struct KubernetesSandbox {
    namespace: String,
    pod: String,
    image: String,
    /// The whole job's timeout, which is what the pod's lifetime is set from.
    budget: Duration,
    /// Created on first use. `SandboxFactory::create` is synchronous and
    /// creating a pod is not, so the pod cannot be made there — and creating it
    /// twice for a two-step job would throw away the first step's work.
    started: tokio::sync::OnceCell<Result<(), String>>,
}

impl KubernetesSandbox {
    /// The pod this sandbox runs steps in. For tests and diagnostics.
    #[must_use]
    pub fn pod_name(&self) -> &str {
        &self.pod
    }

    /// The manifest that is applied.
    ///
    /// Built as data rather than as a string so the hardening can be asserted
    /// on without a cluster — every field below is one a test checks, because
    /// a pod that quietly lost `automountServiceAccountToken: false` would
    /// behave identically until the day someone read a secret with it.
    #[must_use]
    pub fn manifest(&self) -> serde_json::Value {
        let deadline = (self.budget + DEADLINE_SLACK).as_secs();
        serde_json::json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {
                "name": self.pod,
                "namespace": self.namespace,
                "labels": {
                    "app.kubernetes.io/name": "crabforge-job",
                    "app.kubernetes.io/managed-by": "crabforge",
                },
            },
            "spec": {
                // Never: a job that fails is a result, not something to retry
                // behind the runner's back. Redelivery is the queue's job.
                "restartPolicy": "Never",
                "automountServiceAccountToken": false,
                "enableServiceLinks": false,
                "activeDeadlineSeconds": deadline,
                "terminationGracePeriodSeconds": 5,
                "securityContext": {
                    "runAsNonRoot": true,
                    "runAsUser": RUN_AS,
                    "runAsGroup": RUN_AS,
                    // So the emptyDir workspace is writable by a non-root user
                    // without granting a capability to chown it.
                    "fsGroup": RUN_AS,
                    "seccompProfile": {"type": "RuntimeDefault"},
                },
                "containers": [{
                    "name": "job",
                    "image": self.image,
                    // Sleeps for the job's budget while steps arrive over
                    // `kubectl exec`. Exits on its own if the runner vanishes
                    // before `activeDeadlineSeconds` would have fired.
                    "command": ["/bin/sh", "-c", format!("sleep {deadline}")],
                    "workingDir": "/workspace",
                    "securityContext": {
                        "allowPrivilegeEscalation": false,
                        "privileged": false,
                        "readOnlyRootFilesystem": true,
                        "capabilities": {"drop": ["ALL"]},
                    },
                    "resources": {
                        "limits": {"memory": MEMORY_LIMIT, "cpu": CPU_LIMIT},
                        "requests": {"memory": "256Mi", "cpu": "100m"},
                    },
                    "volumeMounts": [
                        {"name": "workspace", "mountPath": "/workspace"},
                        {"name": "tmp", "mountPath": "/tmp"},
                    ],
                }],
                "volumes": [
                    {"name": "workspace", "emptyDir": {}},
                    {"name": "tmp", "emptyDir": {}},
                ],
            },
        })
    }

    /// Create the pod and wait for it to be running, at most once.
    async fn ensure_started(&self) -> Result<(), String> {
        self.started
            .get_or_init(|| async { self.start().await })
            .await
            .clone()
    }

    async fn start(&self) -> Result<(), String> {
        let manifest = serde_json::to_vec(&self.manifest())
            .map_err(|e| format!("encoding the pod manifest: {e}"))?;

        let mut child = tokio::process::Command::new("kubectl")
            .args(["apply", "-f", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("could not start kubectl: {e}"))?;
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt as _;
            stdin
                .write_all(&manifest)
                .await
                .map_err(|e| format!("writing the pod manifest: {e}"))?;
        }
        let applied = child
            .wait_with_output()
            .await
            .map_err(|e| format!("waiting for kubectl apply: {e}"))?;
        if !applied.status.success() {
            return Err(format!(
                "creating the pod: {}",
                String::from_utf8_lossy(&applied.stderr).trim()
            ));
        }

        let waited = tokio::process::Command::new("kubectl")
            .args([
                "wait",
                "--namespace",
                &self.namespace,
                &format!("pod/{}", self.pod),
                "--for=condition=Ready",
                &format!("--timeout={}s", START_BUDGET.as_secs()),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| format!("waiting for the pod: {e}"))?;
        if waited.status.success() {
            return Ok(());
        }

        // `kubectl wait` says only that it timed out. What an operator needs is
        // `ImagePullBackOff` — the same distinction the docker sandbox draws
        // from exit code 125 — so the pod is asked why.
        Err(format!(
            "the pod did not start: {}",
            self.why_not_running()
                .await
                .unwrap_or_else(|| String::from_utf8_lossy(&waited.stderr).trim().to_string())
        ))
    }

    /// The container's waiting reason, if the API server has one.
    async fn why_not_running(&self) -> Option<String> {
        let out = tokio::process::Command::new("kubectl")
            .args([
                "get",
                "pod",
                &self.pod,
                "--namespace",
                &self.namespace,
                "-o",
                "jsonpath={.status.containerStatuses[0].state.waiting.reason}\
                 {\" \"}{.status.containerStatuses[0].state.waiting.message}",
            ])
            .output()
            .await
            .ok()?;
        let reason = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (!reason.is_empty()).then_some(reason)
    }
}

impl Drop for KubernetesSandbox {
    fn drop(&mut self) {
        // Only if we got as far as creating one. `--wait=false` because Drop
        // cannot await and blocking the runner on a graceful pod termination
        // would stall the next job; `activeDeadlineSeconds` is the guarantee,
        // this is just the tidy path.
        if self.started.get().is_none() {
            return;
        }
        let _ = std::process::Command::new("kubectl")
            .args([
                "delete",
                "pod",
                &self.pod,
                "--namespace",
                &self.namespace,
                "--wait=false",
                "--ignore-not-found",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

impl Sandbox for KubernetesSandbox {
    async fn run_step(
        &self,
        command: &str,
        env: &BTreeMap<String, String>,
        timeout: Duration,
        on_line: &mut (dyn FnMut(&str) + Send),
    ) -> StepResult {
        // Starting the pod is inside the step's budget, not beside it. The
        // container sandbox works this way by construction — `docker run` pulls
        // the image within the same timeout — and doing otherwise here means a
        // job with a one-minute budget can spend five minutes on a pull it will
        // never complete and then still be given its minute, holding a runner
        // for six.
        let started = std::time::Instant::now();
        match tokio::time::timeout(timeout, self.ensure_started()).await {
            Ok(Ok(())) => {}
            Ok(Err(reason)) => return StepResult::infra(reason),
            Err(_) => {
                return StepResult::infra(format!(
                    "the pod did not start within the step's {}s budget",
                    timeout.as_secs()
                ));
            }
        }
        // Whatever is left after starting. Saturating, so a step that spent its
        // whole budget on the pull gets zero rather than wrapping to forever.
        let timeout = timeout.saturating_sub(started.elapsed());

        let mut child = match tokio::process::Command::new("kubectl")
            .args([
                "exec",
                &self.pod,
                "--namespace",
                &self.namespace,
                "--",
                "/bin/sh",
                "-c",
                &with_environment(command, env),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(child) => child,
            Err(e) => return StepResult::infra(format!("could not start kubectl: {e}")),
        };

        // One task per stream, as in the other two sandboxes: `next_line` is not
        // cancellation-safe, so a `select!` would drop buffered output.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(256);
        let mut pumps = Vec::new();
        if let Some(stdout) = child.stdout.take() {
            pumps.push(tokio::spawn(crate::sandbox::pump_lines(stdout, tx.clone())));
        }
        if let Some(stderr) = child.stderr.take() {
            pumps.push(tokio::spawn(crate::sandbox::pump_lines(stderr, tx.clone())));
        }
        drop(tx);

        let status = tokio::time::timeout(timeout, async {
            while let Some(line) = rx.recv().await {
                on_line(&line);
            }
            for pump in pumps {
                let _ = pump.await;
            }
            child.wait().await
        })
        .await;

        match status {
            Ok(Ok(status)) if status.success() => StepResult::ok(),
            Ok(Ok(status)) => match status.code() {
                Some(code) => StepResult::failed(code),
                None => StepResult::failed(-1),
            },
            Ok(Err(e)) => StepResult::infra(format!("waiting for the step: {e}")),
            Err(_) => {
                // Killing the local `kubectl exec` leaves the remote process
                // running until the pod's deadline. That is a leak of one
                // pod's worth of CPU rather than a correctness problem — the
                // step is already reported as timed out and no later step will
                // be sent — and killing it properly would mean a second exec
                // racing the first.
                let _ = child.start_kill();
                StepResult {
                    outcome: StepOutcome::TimedOut,
                    detail: Some(format!("exceeded {}s", timeout.as_secs())),
                }
            }
        }
    }
}

/// Prefix `command` with `export`s for `env`.
///
/// `kubectl exec` has no `--env`, so the environment has to travel in the
/// command. It is deliberately not put on the pod spec instead: a pod spec is
/// readable by anything with `get pod` in the namespace, and a job's
/// environment is the most likely place for a token to be.
fn with_environment(command: &str, env: &BTreeMap<String, String>) -> String {
    let mut script = String::new();
    for (key, value) in env {
        script.push_str("export ");
        script.push_str(key);
        script.push('=');
        script.push_str(&shell_quote(value));
        script.push_str("; ");
    }
    script.push_str(command);
    script
}

/// Quote `value` so a shell reads it as one literal word.
///
/// Single quotes make every character literal, so the only case to handle is a
/// single quote itself: close the string, emit an escaped quote, reopen. The
/// values here come from workflow YAML, which is to say from anyone who can
/// open a pull request — `run: echo $(id)` in a variable must stay text.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// Whether a usable `kubectl` and a reachable cluster are present.
pub async fn kubernetes_available() -> bool {
    tokio::process::Command::new("kubectl")
        .args(["version", "--output=json"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;
    use crate::{plan::PlannedJob, queue::QueuedJob};

    fn sandbox() -> KubernetesSandbox {
        let job = QueuedJob {
            job_id: "01hqp7x9k2m4n6p8r0s2t4v6w8".into(),
            run_id: "run-1".into(),
            repo_id: "repo-1".into(),
            head_oid: "abc".into(),
            job: PlannedJob {
                name: "test".into(),
                image: "rust:1.97".into(),
                timeout_minutes: 30,
                env: Vec::new(),
                steps: Vec::new(),
            },
        };
        KubernetesSandboxes::new("crabforge-ci")
            .create(&job)
            .expect("a sandbox")
    }

    #[test]
    fn the_pod_gets_no_api_credentials() {
        // The one that matters most: with a mounted token, a build can read
        // every secret in its namespace.
        let spec = sandbox().manifest();
        check!(spec["spec"]["automountServiceAccountToken"] == serde_json::json!(false));
        check!(spec["spec"]["enableServiceLinks"] == serde_json::json!(false));
    }

    #[test]
    fn the_pod_is_unprivileged() {
        let spec = sandbox().manifest();
        let pod = &spec["spec"]["securityContext"];
        check!(pod["runAsNonRoot"] == serde_json::json!(true));
        check!(pod["runAsUser"] == serde_json::json!(RUN_AS));

        let container = &spec["spec"]["containers"][0]["securityContext"];
        check!(container["allowPrivilegeEscalation"] == serde_json::json!(false));
        check!(container["privileged"] == serde_json::json!(false));
        check!(container["readOnlyRootFilesystem"] == serde_json::json!(true));
        check!(container["capabilities"]["drop"] == serde_json::json!(["ALL"]));
    }

    #[test]
    fn the_workspace_and_tmp_are_writable_and_nothing_else_is() {
        let spec = sandbox().manifest();
        let mounts = spec["spec"]["containers"][0]["volumeMounts"]
            .as_array()
            .expect("mounts")
            .iter()
            .map(|m| m["mountPath"].as_str().unwrap_or_default().to_string())
            .collect::<Vec<_>>();
        check!(mounts == ["/workspace", "/tmp"]);

        // emptyDir, not hostPath: nothing of the node's disk is reachable.
        for volume in spec["spec"]["volumes"].as_array().expect("volumes") {
            check!(volume["emptyDir"].is_object(), "{volume}");
        }
    }

    #[test]
    fn the_pod_outlives_the_job_by_a_margin_and_no_more() {
        // A pod whose deadline was shorter than the job's would kill a build
        // that was still within its timeout and report it as infrastructure.
        let spec = sandbox().manifest();
        let deadline = spec["spec"]["activeDeadlineSeconds"].as_u64().unwrap();
        check!(deadline == 30 * 60 + DEADLINE_SLACK.as_secs());
    }

    #[test]
    fn resources_are_capped() {
        let spec = sandbox().manifest();
        let limits = &spec["spec"]["containers"][0]["resources"]["limits"];
        check!(limits["memory"] == serde_json::json!(MEMORY_LIMIT));
        check!(limits["cpu"] == serde_json::json!(CPU_LIMIT));
    }

    #[test]
    fn a_job_id_that_will_not_make_a_pod_name_is_refused_up_front() {
        // Better than a per-job infrastructure failure with an API-server error
        // nobody can act on.
        let mut job = QueuedJob {
            job_id: "not a dns name".into(),
            run_id: "r".into(),
            repo_id: "r".into(),
            head_oid: "abc".into(),
            job: PlannedJob {
                name: "t".into(),
                image: "alpine".into(),
                timeout_minutes: 1,
                env: Vec::new(),
                steps: Vec::new(),
            },
        };
        check!(KubernetesSandboxes::new("ns").create(&job).is_err());

        job.job_id = "x".repeat(64);
        check!(KubernetesSandboxes::new("ns").create(&job).is_err());
    }

    #[test]
    fn an_environment_value_cannot_escape_into_the_command() {
        // Workflow YAML is written by anyone who can open a pull request, and
        // this string is pasted into a shell script. Run through a real shell
        // rather than asserted on as text: the quoting is only correct if `sh`
        // agrees, and eyeballing nested quotes is how this kind of bug ships.
        for payload in [
            "'; id; echo '",
            "$(id)",
            "`id`",
            "\\'; touch /tmp/pwned; #",
            "plain value",
        ] {
            let mut env = BTreeMap::new();
            env.insert("VALUE".to_string(), payload.to_string());
            // `printf %s` rather than `echo`, so a value containing a newline
            // or a leading `-` is compared as itself.
            let script = with_environment(r#"printf %s "$VALUE""#, &env);

            let out = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(&script)
                .output()
                .expect("running the script");

            check!(out.status.success(), "{script}");
            check!(
                String::from_utf8_lossy(&out.stdout) == payload,
                "the shell did not see the value as one literal word: {script}"
            );
        }
    }

    #[test]
    fn the_environment_is_not_written_into_the_pod_spec() {
        // Where it would be readable by anything with `get pod`.
        let spec = sandbox().manifest();
        check!(spec["spec"]["containers"][0]["env"].is_null());
    }
}
