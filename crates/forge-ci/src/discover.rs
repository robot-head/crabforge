//! Finding the workflows a push should run.
//!
//! Read at the pushed commit, never at the branch tip. The two differ as soon
//! as a second push lands while the first is still being planned, and the
//! difference matters twice: a run labelled with one commit that executed
//! another's workflow is a lie, and reading the tip would let a push that has
//! already landed change what an earlier, still-queued push executes.

use forge_git::{Blob, Cache};

use crate::workflow::{Workflow, WorkflowError};

/// Where workflows live in a repository.
pub const WORKFLOW_DIR: &str = ".crabforge/workflows";

/// A workflow file found at a commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// Repo-relative path, e.g. `.crabforge/workflows/build.yml`.
    pub path: String,
    pub workflow: Workflow,
}

/// What a commit's workflow directory contained.
///
/// Invalid files are carried rather than thrown away. A repository with one
/// broken workflow should still run its other three, and the breakage should be
/// visible — silently running fewer jobs than the author expects is the failure
/// mode worth designing against.
#[derive(Debug, Default)]
pub struct Discovered {
    pub workflows: Vec<Found>,
    pub errors: Vec<WorkflowError>,
}

impl Discovered {
    /// Workflows that want to hear about `event`.
    pub fn triggered_by(&self, event: &str) -> impl Iterator<Item = &Found> {
        self.workflows
            .iter()
            .filter(move |found| found.workflow.on.covers(event))
    }
}

/// Read every workflow at `revision`.
///
/// A repository with no workflow directory is the common case and is not an
/// error — it returns nothing to run.
pub fn discover(cache: &Cache, revision: &str) -> Discovered {
    let mut found = Discovered::default();

    let entries = match cache.list_tree(revision, WORKFLOW_DIR) {
        Ok(entries) => entries,
        Err(error) => {
            // Almost always "no such path", which is most repositories.
            tracing::debug!(%error, revision, "no workflow directory");
            return found;
        }
    };

    for entry in entries {
        if !is_workflow_file(&entry.name) {
            continue;
        }
        let path = format!("{WORKFLOW_DIR}/{}", entry.name);
        let blob = match cache.read_blob(revision, &path) {
            Ok(Some(blob)) => blob,
            Ok(None) => continue,
            Err(error) => {
                tracing::warn!(%error, path, "could not read a workflow");
                continue;
            }
        };

        match blob {
            Blob::Text { content, .. } => match Workflow::parse(&path, &content) {
                Ok(workflow) => found.workflows.push(Found { path, workflow }),
                Err(error) => found.errors.push(error),
            },
            // A binary file in the workflow directory is a mistake worth
            // surfacing rather than skipping silently.
            Blob::Binary { .. } => found.errors.push(WorkflowError::Invalid {
                path,
                message: "not valid UTF-8".into(),
            }),
        }
    }

    // Sorted so a run's jobs are planned in the same order every time. Two
    // pushes of the same commit should produce the same plan, and a map
    // iteration order that varies would make that untrue.
    found.workflows.sort_by(|a, b| a.path.cmp(&b.path));
    found
}

/// Whether a directory entry is a workflow file.
///
/// Both YAML spellings, because people write both and being told your workflow
/// "was not found" when it is right there is a bad afternoon.
fn is_workflow_file(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    // A stem is required: `.yml` is an extension, not a workflow, and treating
    // it as one would try to parse a file nobody meant to write.
    [".yml", ".yaml"]
        .iter()
        .any(|ext| lower.strip_suffix(ext).is_some_and(|stem| !stem.is_empty()))
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;

    #[test]
    fn workflow_files_are_recognised_by_either_extension() {
        check!(is_workflow_file("build.yml"));
        check!(is_workflow_file("build.yaml"));
        check!(is_workflow_file("BUILD.YML"));
        check!(!is_workflow_file("README.md"));
        check!(!is_workflow_file("build.yml.bak"));
        check!(!is_workflow_file(".yml"), "an extension is not a file name");
    }

    fn found(path: &str, on: &str) -> Found {
        Found {
            path: path.into(),
            workflow: Workflow::parse(
                path,
                &format!("on: {on}\njobs:\n  a:\n    steps:\n      - run: x\n"),
            )
            .unwrap(),
        }
    }

    #[test]
    fn only_workflows_that_asked_for_the_event_are_triggered() {
        let discovered = Discovered {
            workflows: vec![found("a.yml", "push"), found("b.yml", "schedule")],
            errors: Vec::new(),
        };
        let paths: Vec<_> = discovered.triggered_by("push").map(|f| &f.path).collect();
        check!(paths == ["a.yml"]);
    }
}
