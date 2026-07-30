//! Where a job's steps actually run.
//!
//! A trait with two implementations, because the two have genuinely different
//! jobs to do:
//!
//! * [`ProcessSandbox`] runs steps as child processes in a temporary
//!   directory. It isolates nothing — a step can read the host's disk and
//!   reach its network. It exists so the CI pipeline can be tested end to end
//!   without a container runtime, and it must never be what a public forge
//!   runs. [`ProcessSandbox::new`] says so in its name.
//! * `DockerSandbox` runs each job in its own container. That is the one a
//!   forge deploys, because CI executes code from anyone who can open a pull
//!   request, and "arbitrary code from strangers" is the threat model.
//!
//! Steps run in sequence and stop at the first failure, which is what a shell
//! script does and therefore what people expect. Each step's output is streamed
//! rather than collected, so a job that hangs still shows what it managed
//! before it did.

use std::{collections::BTreeMap, path::PathBuf, process::Stdio, time::Duration};

use tokio::io::{AsyncBufReadExt as _, AsyncRead, BufReader};

/// Which of a child's two output streams a pump is reading.
enum StreamOf {
    Out(tokio::process::ChildStdout),
    Err(tokio::process::ChildStderr),
}

/// Forward every line of `stream` into `tx` until it ends.
pub(crate) async fn pump_lines<R: AsyncRead + Unpin>(
    stream: R,
    tx: tokio::sync::mpsc::Sender<String>,
) {
    let mut lines = BufReader::new(stream).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if tx.send(line).await.is_err() {
            // The consumer gave up — the step timed out and is being killed.
            return;
        }
    }
}

/// How a step ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepOutcome {
    Succeeded,
    /// Ran to completion and reported failure.
    Failed {
        exit_code: i32,
    },
    /// Killed for taking too long.
    TimedOut,
    /// Could not be run at all — the sandbox itself failed.
    ///
    /// Distinct from `Failed` on purpose: a job whose container image does not
    /// exist has not told you anything about the code, and reporting it as a
    /// test failure would send someone looking in the wrong place.
    InfraFailed,
}

impl StepOutcome {
    pub fn is_success(self) -> bool {
        matches!(self, Self::Succeeded)
    }

    /// The exit code to record, if the step produced one.
    pub fn exit_code(self) -> Option<i32> {
        match self {
            Self::Succeeded => Some(0),
            Self::Failed { exit_code } => Some(exit_code),
            Self::TimedOut | Self::InfraFailed => None,
        }
    }
}

/// What running one step produced.
#[derive(Debug, Clone)]
pub struct StepResult {
    pub outcome: StepOutcome,
    /// Why, when the outcome does not speak for itself.
    pub detail: Option<String>,
}

impl StepResult {
    pub fn ok() -> Self {
        Self {
            outcome: StepOutcome::Succeeded,
            detail: None,
        }
    }

    pub fn failed(exit_code: i32) -> Self {
        Self {
            outcome: StepOutcome::Failed { exit_code },
            detail: None,
        }
    }

    pub fn infra(detail: impl Into<String>) -> Self {
        Self {
            outcome: StepOutcome::InfraFailed,
            detail: Some(detail.into()),
        }
    }
}

/// Somewhere a job's steps can be executed.
///
/// `on_line` is called for every line of output as it arrives. Taking a
/// callback rather than returning the output keeps a hung job's partial log
/// visible, which is when a log is most wanted.
///
/// The callback is `Send` because a runner is spawned onto the executor, and a
/// future holding a non-`Send` reference across an await cannot be.
///
/// The returned future is spelled out rather than written as an `async fn` for
/// the same reason: `async fn` in a trait leaves the future's auto traits
/// unbounded, so a caller generic over `Sandbox` cannot spawn it. That only
/// shows up when there is more than one implementation to be generic over,
/// which is what the Kubernetes sandbox made true.
pub trait Sandbox {
    /// Run one shell command, streaming its output.
    fn run_step(
        &self,
        command: &str,
        env: &BTreeMap<String, String>,
        timeout: Duration,
        on_line: &mut (dyn FnMut(&str) + Send),
    ) -> impl Future<Output = StepResult> + Send;
}

/// Runs steps as child processes on this host.
///
/// **Not an isolation boundary.** See the module docs.
pub struct ProcessSandbox {
    workspace: PathBuf,
}

impl ProcessSandbox {
    /// Create a sandbox that runs commands in `workspace`.
    ///
    /// Named for what it is rather than what it sounds like: this provides no
    /// isolation whatsoever, and a forge that runs untrusted code through it is
    /// handing out shell access to its own host.
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
        }
    }
}

impl Sandbox for ProcessSandbox {
    async fn run_step(
        &self,
        command: &str,
        env: &BTreeMap<String, String>,
        timeout: Duration,
        on_line: &mut (dyn FnMut(&str) + Send),
    ) -> StepResult {
        let mut child = match tokio::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(command)
            .current_dir(&self.workspace)
            .envs(env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            // Merged into one stream rather than kept apart: a log is read as a
            // narrative, and interleaving is what makes an error line up with
            // the output that preceded it.
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(child) => child,
            Err(e) => return StepResult::infra(format!("could not start a shell: {e}")),
        };

        // Both streams are pumped by their own task into one channel, rather
        // than `select!`ed over here. `next_line` is not cancellation-safe: the
        // losing branch of a `select!` is dropped mid-read and whatever it had
        // buffered goes with it, which shows up as lines silently missing from
        // a build log. One task per stream never cancels a read.
        let (lines_tx, mut lines_rx) = tokio::sync::mpsc::channel::<String>(256);
        let mut pumps = Vec::new();
        for stream in [
            child.stdout.take().map(StreamOf::Out),
            child.stderr.take().map(StreamOf::Err),
        ]
        .into_iter()
        .flatten()
        {
            let tx = lines_tx.clone();
            pumps.push(tokio::spawn(async move {
                match stream {
                    StreamOf::Out(s) => pump_lines(s, tx).await,
                    StreamOf::Err(s) => pump_lines(s, tx).await,
                }
            }));
        }
        // The loop below ends when every sender is gone, so this one must go.
        drop(lines_tx);

        let status = tokio::time::timeout(timeout, async {
            while let Some(line) = lines_rx.recv().await {
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
            Ok(Ok(status)) => StepResult::failed(status.code().unwrap_or(-1)),
            Ok(Err(e)) => StepResult::infra(format!("waiting for the step: {e}")),
            Err(_) => {
                // The child is killed by `kill_on_drop` when the future is
                // dropped, so nothing is left behind holding the workspace.
                let _ = child.start_kill();
                StepResult {
                    outcome: StepOutcome::TimedOut,
                    detail: Some(format!("exceeded {}s", timeout.as_secs())),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;

    async fn run(command: &str) -> (StepResult, Vec<String>) {
        let dir = tempfile::tempdir().unwrap();
        let sandbox = ProcessSandbox::new(dir.path());
        let mut lines = Vec::new();
        let result = sandbox
            .run_step(
                command,
                &BTreeMap::new(),
                Duration::from_secs(30),
                &mut |line| lines.push(line.to_string()),
            )
            .await;
        (result, lines)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_successful_step_reports_its_output() {
        let (result, lines) = run("echo hello; echo world").await;
        check!(result.outcome == StepOutcome::Succeeded);
        check!(lines == ["hello", "world"]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failing_step_carries_its_exit_code() {
        let (result, _) = run("exit 3").await;
        check!(result.outcome == StepOutcome::Failed { exit_code: 3 });
        check!(result.outcome.exit_code() == Some(3));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stderr_is_in_the_log_too() {
        // A build that fails and shows nothing about why is the worst outcome.
        let (result, lines) = run("echo oops >&2; exit 1").await;
        check!(!result.outcome.is_success());
        check!(lines.contains(&"oops".to_string()));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_step_that_overruns_is_killed_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let sandbox = ProcessSandbox::new(dir.path());
        let result = sandbox
            .run_step(
                "sleep 30",
                &BTreeMap::new(),
                Duration::from_millis(200),
                &mut |_| {},
            )
            .await;
        check!(result.outcome == StepOutcome::TimedOut);
        // Not reported as a failed build: nothing was learned about the code.
        check!(result.outcome.exit_code().is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn steps_run_in_the_workspace_and_see_their_environment() {
        let dir = tempfile::tempdir().unwrap();
        // With the trailing newline a real file would have: without it the
        // shell runs the two outputs together, which is correct but makes the
        // assertion below about line splitting rather than about the workspace.
        std::fs::write(dir.path().join("marker"), b"here\n").unwrap();
        let sandbox = ProcessSandbox::new(dir.path());

        let mut env = BTreeMap::new();
        env.insert("GREETING".to_string(), "hi".to_string());

        let mut lines = Vec::new();
        let result = sandbox
            .run_step(
                "cat marker; echo $GREETING",
                &env,
                Duration::from_secs(30),
                &mut |line| lines.push(line.to_string()),
            )
            .await;

        check!(result.outcome.is_success());
        check!(lines.iter().any(|l| l.contains("here")), "{lines:?}");
        check!(lines.iter().any(|l| l == "hi"), "{lines:?}");
    }

    #[test]
    fn an_infrastructure_failure_is_not_a_build_failure() {
        // The distinction that keeps someone from debugging their tests when
        // the image name was wrong.
        let infra = StepResult::infra("no such image");
        check!(infra.outcome == StepOutcome::InfraFailed);
        check!(infra.outcome.exit_code().is_none());
        check!(!infra.outcome.is_success());
    }
}
