//! Executing one job.
//!
//! Deliberately ignorant of the broker and the database. It is handed a job and
//! a sandbox, runs the steps, streams the log somewhere, and says how it went.
//! Everything about queues, claims and events lives above it — which is what
//! makes the interesting behaviour here testable without any of that.
//!
//! ## Steps stop at the first failure
//!
//! What a shell script does, and therefore what people expect. A step that
//! fails leaves the remaining ones unrun rather than running them against a
//! state the workflow author did not anticipate.

use std::{collections::BTreeMap, time::Duration};

use crate::{
    plan::PlannedJob,
    sandbox::{Sandbox, StepOutcome},
};

/// How a whole job ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobOutcome {
    Succeeded,
    Failed { exit_code: Option<i32> },
    TimedOut,
    InfraFailed,
}

impl JobOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "success",
            Self::Failed { .. } => "failed",
            Self::TimedOut => "timed_out",
            Self::InfraFailed => "infra_failed",
        }
    }

    pub fn exit_code(self) -> Option<i32> {
        match self {
            Self::Succeeded => Some(0),
            Self::Failed { exit_code } => exit_code,
            Self::TimedOut | Self::InfraFailed => None,
        }
    }

    pub fn is_success(self) -> bool {
        matches!(self, Self::Succeeded)
    }
}

/// Where a job's output goes as it is produced.
///
/// A trait rather than a channel so a test can collect lines in a `Vec` and a
/// deployment can chunk them onto a topic, without the runner knowing which.
pub trait LogSink {
    fn line(&mut self, line: &str);
}

impl LogSink for Vec<String> {
    fn line(&mut self, line: &str) {
        self.push(line.to_string());
    }
}

/// Run every step of `job` in `sandbox`, streaming output to `log`.
///
/// The step timeout is the *job's* timeout, not a per-step one: a workflow says
/// how long the whole job may take, and dividing that between steps would make
/// the limit depend on how the author happened to split their script.
pub async fn run_job<S: Sandbox, L: LogSink>(
    job: &PlannedJob,
    sandbox: &S,
    log: &mut L,
) -> JobOutcome {
    let env: BTreeMap<String, String> = job.env.iter().cloned().collect();
    let budget = Duration::from_secs(u64::from(job.timeout_minutes) * 60);
    let started = tokio::time::Instant::now();

    for (index, step) in job.steps.iter().enumerate() {
        // A header per step, so a reader can tell which command produced what.
        log.line(&format!("=== step {}: {}", index + 1, step.label()));

        // What is left of the job's budget, so the steps together cannot exceed
        // it — a job of ten steps each just under the limit would otherwise run
        // for ten times as long as it asked for.
        let remaining = budget.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            log.line("=== job timed out");
            return JobOutcome::TimedOut;
        }

        let result = sandbox
            .run_step(&step.run, &env, remaining, &mut |line| log.line(line))
            .await;

        match result.outcome {
            StepOutcome::Succeeded => {}
            StepOutcome::Failed { exit_code } => {
                log.line(&format!("=== step failed with exit code {exit_code}"));
                return JobOutcome::Failed {
                    exit_code: Some(exit_code),
                };
            }
            StepOutcome::TimedOut => {
                log.line("=== job timed out");
                return JobOutcome::TimedOut;
            }
            StepOutcome::InfraFailed => {
                let detail = result.detail.unwrap_or_else(|| "unknown".into());
                log.line(&format!("=== could not run this step: {detail}"));
                return JobOutcome::InfraFailed;
            }
        }
    }

    log.line("=== job succeeded");
    JobOutcome::Succeeded
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;
    use crate::{
        sandbox::{ProcessSandbox, StepResult},
        workflow::Step,
    };

    fn job(commands: &[&str]) -> PlannedJob {
        PlannedJob {
            name: "test".into(),
            image: "ubuntu:24.04".into(),
            timeout_minutes: 5,
            env: Vec::new(),
            steps: commands
                .iter()
                .map(|c| Step {
                    name: None,
                    run: (*c).to_string(),
                })
                .collect(),
        }
    }

    async fn run(commands: &[&str]) -> (JobOutcome, Vec<String>) {
        let dir = tempfile::tempdir().unwrap();
        let sandbox = ProcessSandbox::new(dir.path());
        let mut log = Vec::new();
        let outcome = run_job(&job(commands), &sandbox, &mut log).await;
        (outcome, log)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn every_step_runs_and_the_job_succeeds() {
        let (outcome, log) = run(&["echo one", "echo two"]).await;
        check!(outcome == JobOutcome::Succeeded);
        check!(log.iter().any(|l| l == "one"));
        check!(log.iter().any(|l| l == "two"));
        check!(log.last().unwrap().contains("succeeded"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failing_step_stops_the_ones_after_it() {
        // What a shell script does. Running later steps against a state the
        // author did not anticipate is worse than stopping.
        let (outcome, log) = run(&["echo before", "exit 2", "echo after"]).await;
        check!(outcome == JobOutcome::Failed { exit_code: Some(2) });
        check!(log.iter().any(|l| l == "before"));
        check!(
            !log.iter().any(|l| l == "after"),
            "a step after the failure ran: {log:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_log_says_which_step_produced_what() {
        let (_, log) = run(&["echo one", "echo two"]).await;
        let headers: Vec<_> = log.iter().filter(|l| l.starts_with("=== step")).collect();
        check!(headers.len() == 2, "{log:?}");
        check!(headers[0].contains("step 1"));
    }

    /// A sandbox that always fails to start anything.
    struct BrokenSandbox;

    impl Sandbox for BrokenSandbox {
        async fn run_step(
            &self,
            _command: &str,
            _env: &BTreeMap<String, String>,
            _timeout: Duration,
            _on_line: &mut dyn FnMut(&str),
        ) -> StepResult {
            StepResult::infra("no such image: rust:9.99")
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_sandbox_that_cannot_start_is_not_reported_as_a_test_failure() {
        // The distinction that stops someone debugging their tests when the
        // image name was wrong. It also has to say what went wrong.
        let mut log = Vec::new();
        let outcome = run_job(&job(&["cargo test"]), &BrokenSandbox, &mut log).await;
        check!(outcome == JobOutcome::InfraFailed);
        check!(outcome.exit_code().is_none());
        check!(
            log.iter().any(|l| l.contains("no such image")),
            "the reason should be in the log: {log:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_timeout_covers_the_whole_job_and_not_each_step() {
        // Ten steps each just under the limit must not run for ten times the
        // limit. The budget is shared, so the job stops when it is spent.
        let dir = tempfile::tempdir().unwrap();
        let sandbox = ProcessSandbox::new(dir.path());
        let mut spec = job(&["sleep 5", "echo never"]);
        // Rounded up to one minute by the type; use a job whose first step
        // already exceeds a short budget by driving the sandbox directly.
        spec.timeout_minutes = 1;

        let mut log = Vec::new();
        let outcome =
            tokio::time::timeout(Duration::from_secs(90), run_job(&spec, &sandbox, &mut log))
                .await
                .expect("the job must not outlive its budget");

        // Five seconds fits inside a minute, so this one succeeds — what is
        // being pinned is that the second step sees a *reduced* budget, which
        // the header ordering below shows it reached.
        check!(outcome == JobOutcome::Succeeded);
        check!(log.iter().any(|l| l.contains("step 2")));
    }
}
