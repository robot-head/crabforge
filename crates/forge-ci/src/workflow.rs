//! What a `.crabforge/workflows/*.yml` file says.
//!
//! Deliberately a small subset of the GitHub Actions shape rather than a
//! different one. People arrive already knowing `on:`, `jobs:`, `runs-on:` and
//! `run:`, and a forge that spelled them differently for no reason would be
//! asking everyone to learn a second vocabulary for the same ideas. What is
//! missing here is missing because it is not built yet, not because it was
//! rejected — so the names stay free for it.
//!
//! Unknown keys are rejected rather than ignored. A workflow that silently does
//! nothing because `step:` was written for `steps:` is worse than one that
//! refuses to load and says which line was wrong.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The default image a job runs in when `runs-on` is absent.
pub const DEFAULT_IMAGE: &str = "ubuntu:24.04";

/// How long a job may run before it is killed, when it does not say.
pub const DEFAULT_TIMEOUT_MINUTES: u32 = 60;

/// The largest `timeout-minutes` a workflow may ask for.
///
/// A job that has been stuck for six hours is not going to finish, and it is
/// holding a runner the whole time.
pub const MAX_TIMEOUT_MINUTES: u32 = 360;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum WorkflowError {
    #[error("{path}: {message}")]
    Invalid { path: String, message: String },
}

impl WorkflowError {
    fn invalid(path: &str, message: impl Into<String>) -> Self {
        Self::Invalid {
            path: path.to_string(),
            message: message.into(),
        }
    }
}

/// One workflow file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Workflow {
    /// Shown in the UI. Defaults to the file name.
    #[serde(default)]
    pub name: Option<String>,
    /// What triggers it. Only `push` is understood today.
    pub on: Trigger,
    /// Keyed by job name, which is what the UI and the checks list show.
    pub jobs: BTreeMap<String, Job>,
}

/// The `on:` key, in either of the spellings people write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Trigger {
    /// `on: push`
    One(String),
    /// `on: [push]`
    Many(Vec<String>),
}

impl Trigger {
    pub fn covers(&self, event: &str) -> bool {
        match self {
            Self::One(one) => one == event,
            Self::Many(many) => many.iter().any(|e| e == event),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Job {
    /// The container image. `runs-on` rather than `image` because that is what
    /// people already type, even though ours names an image and GitHub's names
    /// a runner label.
    #[serde(rename = "runs-on", default)]
    pub runs_on: Option<String>,
    #[serde(rename = "timeout-minutes", default)]
    pub timeout_minutes: Option<u32>,
    /// Environment variables for every step.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub steps: Vec<Step>,
}

impl Job {
    pub fn image(&self) -> &str {
        self.runs_on.as_deref().unwrap_or(DEFAULT_IMAGE)
    }

    pub fn timeout_minutes(&self) -> u32 {
        self.timeout_minutes.unwrap_or(DEFAULT_TIMEOUT_MINUTES)
    }
}

/// One shell command, with an optional label.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    #[serde(default)]
    pub name: Option<String>,
    pub run: String,
}

impl Step {
    /// What to show in the log. The command itself when unnamed, because an
    /// unlabelled step is still recognisable by what it does.
    pub fn label(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.run)
    }
}

impl Workflow {
    /// Parse and validate one workflow file.
    ///
    /// `path` is only for error messages, and is worth passing accurately: a
    /// repository can have many workflows and "invalid workflow" without a name
    /// sends someone looking through all of them.
    pub fn parse(path: &str, yaml: &str) -> Result<Self, WorkflowError> {
        let workflow: Self =
            serde_yaml::from_str(yaml).map_err(|e| WorkflowError::invalid(path, e.to_string()))?;
        workflow.validate(path)?;
        Ok(workflow)
    }

    fn validate(&self, path: &str) -> Result<(), WorkflowError> {
        if self.jobs.is_empty() {
            return Err(WorkflowError::invalid(path, "no jobs"));
        }
        for (name, job) in &self.jobs {
            if job.steps.is_empty() {
                return Err(WorkflowError::invalid(
                    path,
                    format!("job `{name}` has no steps"),
                ));
            }
            if let Some(timeout) = job.timeout_minutes {
                if timeout == 0 {
                    return Err(WorkflowError::invalid(
                        path,
                        format!("job `{name}` has a zero timeout"),
                    ));
                }
                if timeout > MAX_TIMEOUT_MINUTES {
                    return Err(WorkflowError::invalid(
                        path,
                        format!(
                            "job `{name}` asks for {timeout} minutes; the limit is \
                             {MAX_TIMEOUT_MINUTES}"
                        ),
                    ));
                }
            }
            if job.image().trim().is_empty() {
                return Err(WorkflowError::invalid(
                    path,
                    format!("job `{name}` has an empty `runs-on`"),
                ));
            }
        }
        Ok(())
    }

    /// The display name: `name:` if given, else the file's stem.
    pub fn display_name(&self, path: &str) -> String {
        self.name.clone().unwrap_or_else(|| {
            path.rsplit('/')
                .next()
                .and_then(|f| f.rsplit_once('.').map(|(stem, _)| stem))
                .unwrap_or(path)
                .to_string()
        })
    }
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;

    const SAMPLE: &str = r#"
name: build
on: [push]
jobs:
  test:
    runs-on: rust:1.97
    timeout-minutes: 30
    env:
      RUST_LOG: debug
    steps:
      - name: check
        run: cargo check
      - run: cargo test
"#;

    #[test]
    fn a_workflow_parses_into_the_jobs_it_describes() {
        let workflow = Workflow::parse("build.yml", SAMPLE).unwrap();
        check!(workflow.display_name("build.yml") == "build");
        check!(workflow.on.covers("push"));
        check!(!workflow.on.covers("pull_request"));

        let job = &workflow.jobs["test"];
        check!(job.image() == "rust:1.97");
        check!(job.timeout_minutes() == 30);
        check!(job.env["RUST_LOG"] == "debug");
        check!(job.steps.len() == 2);
        check!(job.steps[0].label() == "check");
        // An unnamed step is labelled by what it runs.
        check!(job.steps[1].label() == "cargo test");
    }

    #[test]
    fn both_spellings_of_the_trigger_work() {
        // `on: push` and `on: [push]` are both things people write.
        let one = Workflow::parse(
            "w.yml",
            "on: push\njobs:\n  a:\n    steps:\n      - run: x\n",
        );
        check!(one.unwrap().on.covers("push"));
        let many = Workflow::parse(
            "w.yml",
            "on: [push]\njobs:\n  a:\n    steps:\n      - run: x\n",
        );
        check!(many.unwrap().on.covers("push"));
    }

    #[test]
    fn defaults_apply_when_a_job_says_little() {
        let workflow = Workflow::parse(
            "w.yml",
            "on: push\njobs:\n  a:\n    steps:\n      - run: true\n",
        )
        .unwrap();
        let job = &workflow.jobs["a"];
        check!(job.image() == DEFAULT_IMAGE);
        check!(job.timeout_minutes() == DEFAULT_TIMEOUT_MINUTES);
        check!(job.env.is_empty());
    }

    #[test]
    fn the_display_name_falls_back_to_the_file_name() {
        let workflow = Workflow::parse(
            "w.yml",
            "on: push\njobs:\n  a:\n    steps:\n      - run: x\n",
        )
        .unwrap();
        check!(workflow.display_name(".crabforge/workflows/nightly.yml") == "nightly");
    }

    #[test]
    fn a_misspelled_key_is_refused_rather_than_ignored() {
        // The failure this prevents: a workflow that loads, runs nothing, and
        // gives no reason. `step:` for `steps:` is the classic.
        let yaml = "on: push\njobs:\n  a:\n    step:\n      - run: x\n";
        let error = Workflow::parse("w.yml", yaml).unwrap_err();
        let WorkflowError::Invalid { path, message } = &error;
        check!(path == "w.yml");
        check!(message.contains("step"), "unhelpful message: {message}");
    }

    #[test]
    fn a_workflow_with_nothing_to_do_is_refused() {
        check!(Workflow::parse("w.yml", "on: push\njobs: {}\n").is_err());
        check!(Workflow::parse("w.yml", "on: push\njobs:\n  a:\n    steps: []\n").is_err());
    }

    #[test]
    fn an_unrunnable_timeout_is_refused() {
        let zero = "on: push\njobs:\n  a:\n    timeout-minutes: 0\n    steps:\n      - run: x\n";
        check!(Workflow::parse("w.yml", zero).is_err());

        let huge =
            "on: push\njobs:\n  a:\n    timeout-minutes: 100000\n    steps:\n      - run: x\n";
        let error = Workflow::parse("w.yml", huge).unwrap_err();
        let WorkflowError::Invalid { message, .. } = &error;
        check!(message.contains("360"), "should name the limit: {message}");
    }

    #[test]
    fn the_file_name_is_in_the_error() {
        // A repository can have many workflows; "invalid workflow" alone sends
        // someone through all of them.
        let error = Workflow::parse(".crabforge/workflows/nightly.yml", "not: valid").unwrap_err();
        let WorkflowError::Invalid { path, .. } = &error;
        check!(path == ".crabforge/workflows/nightly.yml");
    }

    #[test]
    fn malformed_yaml_is_an_error_and_not_a_panic() {
        check!(Workflow::parse("w.yml", "\t\tnot: [yaml").is_err());
        check!(Workflow::parse("w.yml", "").is_err());
    }
}
