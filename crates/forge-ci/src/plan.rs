//! Turning what was found at a commit into what should run.
//!
//! Pure: no clock, no ids, no I/O. Given the same discovery and the same push
//! it produces the same plan, which is what lets the orchestrator be replayed
//! and what makes this testable without a broker.

use crate::{discover::Discovered, workflow::Job};

/// One job to create and queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedJob {
    /// The key from the workflow's `jobs:` map. Unique within its run.
    pub name: String,
    pub image: String,
    pub timeout_minutes: u32,
    pub env: Vec<(String, String)>,
    pub steps: Vec<crate::workflow::Step>,
}

impl PlannedJob {
    fn from(name: &str, job: &Job) -> Self {
        Self {
            name: name.to_string(),
            image: job.image().to_string(),
            timeout_minutes: job.timeout_minutes(),
            env: job
                .env
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            steps: job.steps.clone(),
        }
    }
}

/// One run to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedRun {
    /// Repo-relative path of the workflow file.
    pub workflow: String,
    /// The workflow's display name.
    pub name: String,
    pub jobs: Vec<PlannedJob>,
}

/// What a push should produce.
///
/// A deleted branch produces nothing: there is no commit to read a workflow
/// from, and running the workflow from the commit that *was* there would be
/// executing code the push removed.
pub fn plan_push(discovered: &Discovered) -> Vec<PlannedRun> {
    discovered
        .triggered_by("push")
        .map(|found| PlannedRun {
            workflow: found.path.clone(),
            name: found.workflow.display_name(&found.path),
            // BTreeMap iteration is sorted, so job order is stable across
            // replays — two plans of the same commit must agree.
            jobs: found
                .workflow
                .jobs
                .iter()
                .map(|(name, job)| PlannedJob::from(name, job))
                .collect(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;
    use crate::{discover::Found, workflow::Workflow};

    fn discovered(files: &[(&str, &str)]) -> Discovered {
        Discovered {
            workflows: files
                .iter()
                .map(|(path, yaml)| Found {
                    path: (*path).to_string(),
                    workflow: Workflow::parse(path, yaml).unwrap(),
                })
                .collect(),
            errors: Vec::new(),
        }
    }

    const TWO_JOBS: &str = r#"
name: build
on: [push]
jobs:
  zebra:
    runs-on: rust:1.97
    steps:
      - run: cargo test
  alpha:
    steps:
      - run: echo hi
"#;

    #[test]
    fn a_push_plans_one_run_per_triggered_workflow() {
        let found = discovered(&[
            (".crabforge/workflows/build.yml", TWO_JOBS),
            (
                ".crabforge/workflows/nightly.yml",
                "on: schedule\njobs:\n  a:\n    steps:\n      - run: x\n",
            ),
        ]);
        let plan = plan_push(&found);
        check!(plan.len() == 1, "only the push-triggered workflow runs");
        check!(plan[0].name == "build");
        check!(plan[0].workflow == ".crabforge/workflows/build.yml");
    }

    #[test]
    fn every_job_in_a_workflow_is_planned_with_its_settings() {
        let plan = plan_push(&discovered(&[(".crabforge/workflows/build.yml", TWO_JOBS)]));
        let jobs = &plan[0].jobs;
        check!(jobs.len() == 2);

        // Sorted, not authored order: the plan has to be the same every time.
        check!(jobs[0].name == "alpha");
        check!(jobs[1].name == "zebra");

        check!(jobs[1].image == "rust:1.97");
        check!(jobs[0].image == crate::workflow::DEFAULT_IMAGE);
        check!(jobs[1].steps[0].run == "cargo test");
    }

    #[test]
    fn planning_the_same_commit_twice_gives_the_same_plan() {
        // The property replay rests on. If job order came from a hash map, a
        // re-plan could produce a different set of job names for one run.
        let found = discovered(&[(".crabforge/workflows/build.yml", TWO_JOBS)]);
        check!(plan_push(&found) == plan_push(&found));
    }

    #[test]
    fn a_repository_with_no_workflows_plans_nothing() {
        check!(plan_push(&Discovered::default()).is_empty());
    }
}
