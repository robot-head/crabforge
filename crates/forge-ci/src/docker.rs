//! Running a job inside a container.
//!
//! This is the sandbox a deployment uses. CI executes code from anyone who can
//! open a pull request, so "arbitrary code from strangers" is the threat model
//! and a process on the host is not an answer to it.
//!
//! Driven through the `docker` CLI rather than the daemon API. The API would be
//! tidier, but the CLI is what an operator already has configured — its
//! context, its credentials, its rootless or remote daemon — and matching that
//! exactly matters more here than avoiding a subprocess. A forge that could not
//! pull from the registry an operator had already logged into would be a poor
//! trade for a cleaner call.
//!
//! ## What the container is denied
//!
//! Everything not needed to run a build:
//!
//! * **The network**, by default. A build that reaches the internet is a build
//!   whose result depends on the internet, and it is also the easiest way to
//!   exfiltrate anything the job can see.
//! * **Privileges**: not root, no capabilities at all, no new privileges, and a
//!   read-only root filesystem with a writable workspace and `/tmp`.
//! * **The host's disk**: only the job's own workspace is mounted.
//! * **Unbounded resources**: memory and process count are capped, so one job
//!   cannot take the runner down with it.

use std::{collections::BTreeMap, path::PathBuf, process::Stdio, time::Duration};

use crate::{
    queue::QueuedJob,
    sandbox::{Sandbox, StepResult},
    service::SandboxFactory,
};

/// Memory ceiling for a job's container.
const MEMORY_LIMIT: &str = "2g";

/// Process ceiling, so a fork bomb stops at the container edge.
const PIDS_LIMIT: &str = "512";

/// Hands out one container per job.
pub struct DockerSandboxes {
    /// Where job workspaces are created on the host.
    root: PathBuf,
}

impl DockerSandboxes {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl SandboxFactory for DockerSandboxes {
    type Sandbox = DockerSandbox;

    fn create(&self, job: &QueuedJob) -> Result<Self::Sandbox, String> {
        let workspace = self.root.join(&job.job_id);
        std::fs::create_dir_all(&workspace).map_err(|e| format!("preparing a workspace: {e}"))?;
        // Run as whoever owns the workspace rather than as the container's
        // root. Two reasons, and the second is not obvious: a build has no
        // business being root, and `--cap-drop=ALL` takes CAP_DAC_OVERRIDE
        // with it — so a root process could not write to a bind mount owned by
        // the host user anyway. Matching the owner is what makes the workspace
        // writable without handing back a capability.
        let owner = std::fs::metadata(&workspace)
            .map(|m| {
                use std::os::unix::fs::MetadataExt as _;
                (m.uid(), m.gid())
            })
            .map_err(|e| format!("reading workspace ownership: {e}"))?;

        Ok(DockerSandbox {
            image: job.job.image.clone(),
            workspace,
            owner,
            // One name per job, so a leftover container from a previous attempt
            // is visible rather than silently reused.
            container: format!("crabforge-{}", job.job_id),
        })
    }
}

/// A container to run one job's steps in.
pub struct DockerSandbox {
    image: String,
    workspace: PathBuf,
    /// The uid and gid the container runs as — see `create`.
    owner: (u32, u32),
    container: String,
}

impl DockerSandbox {
    /// The arguments common to every step.
    fn docker_args(&self, timeout: Duration) -> Vec<String> {
        let workspace = self.workspace.display().to_string();
        vec![
            "run".into(),
            "--rm".into(),
            format!("--name={}-{}", self.container, timeout.as_secs()),
            // No network unless a workflow one day asks for it explicitly.
            "--network=none".into(),
            "--cap-drop=ALL".into(),
            format!("--user={}:{}", self.owner.0, self.owner.1),
            "--security-opt=no-new-privileges".into(),
            "--read-only".into(),
            format!("--memory={MEMORY_LIMIT}"),
            format!("--pids-limit={PIDS_LIMIT}"),
            // Writable where a build needs to write, and nowhere else.
            "--tmpfs=/tmp:rw,exec,nosuid,size=1g".into(),
            format!("--volume={workspace}:/workspace:rw"),
            "--workdir=/workspace".into(),
        ]
    }
}

impl Sandbox for DockerSandbox {
    async fn run_step(
        &self,
        command: &str,
        env: &BTreeMap<String, String>,
        timeout: Duration,
        on_line: &mut (dyn FnMut(&str) + Send),
    ) -> StepResult {
        let mut args = self.docker_args(timeout);
        for (key, value) in env {
            // Passed by name=value rather than through the host environment, so
            // nothing the runner holds leaks into the container by accident.
            args.push(format!("--env={key}={value}"));
        }
        args.push(self.image.clone());
        args.extend(["/bin/sh".to_string(), "-c".to_string(), command.to_string()]);

        let mut child = match tokio::process::Command::new("docker")
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(child) => child,
            Err(e) => return StepResult::infra(format!("could not start docker: {e}")),
        };

        // Same shape as the process sandbox, and for the same reason:
        // `next_line` is not cancellation-safe, so each stream gets its own
        // task rather than being `select!`ed over.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(256);
        let mut pumps = Vec::new();
        if let Some(stdout) = child.stdout.take() {
            let tx = tx.clone();
            pumps.push(tokio::spawn(crate::sandbox::pump_lines(stdout, tx)));
        }
        if let Some(stderr) = child.stderr.take() {
            let tx = tx.clone();
            pumps.push(tokio::spawn(crate::sandbox::pump_lines(stderr, tx)));
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
            Ok(Ok(status)) => {
                // 125 is docker's own "could not run", which is an
                // infrastructure problem — a missing image, usually — and not
                // the build telling you something.
                match status.code() {
                    Some(125) => StepResult::infra(format!("docker could not run {}", self.image)),
                    Some(code) => StepResult::failed(code),
                    None => StepResult::failed(-1),
                }
            }
            Ok(Err(e)) => StepResult::infra(format!("waiting for the container: {e}")),
            Err(_) => {
                let _ = child.start_kill();
                // The container outlives the client that started it, so it has
                // to be stopped by name rather than by killing `docker run`.
                let name = format!("{}-{}", self.container, timeout.as_secs());
                let _ = tokio::process::Command::new("docker")
                    .args(["kill", &name])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .await;
                StepResult {
                    outcome: crate::sandbox::StepOutcome::TimedOut,
                    detail: Some(format!("exceeded {}s", timeout.as_secs())),
                }
            }
        }
    }
}

/// Whether a usable docker daemon is present.
///
/// Tests that need one skip without it, the way the gres-backed tests do — a
/// suite that fails on a machine without docker tells you about the machine,
/// not about the code.
pub async fn docker_available() -> bool {
    tokio::process::Command::new("docker")
        .args(["version", "--format", "{{.Server.Version}}"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}
