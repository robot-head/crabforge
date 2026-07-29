//! Handing a job to a runner.
//!
//! Behind a trait, because there are two ways to do this on crabka and the
//! right one is not yet settled.
//!
//! Share groups (KIP-932) are the shape this wants: several runners pulling
//! from one queue, each message going to exactly one of them, with per-message
//! acknowledgement and redelivery of anything a runner dropped. Crabka
//! implements them, but they must be enabled at *format* time
//! (`--feature share.version=1`) — a broker formatted without it cannot be
//! reconfigured, only reformatted. That makes them a deployment prerequisite
//! rather than something a runner can assume.
//!
//! So the queue is a trait with a partition-per-runner implementation that
//! works on any broker, and share groups become a second implementation once
//! the feature can be relied on. The trait is what keeps that from being a
//! rewrite: a runner asks for the next job and says how it went, and neither of
//! those changes.

use serde::{Deserialize, Serialize};

use crate::plan::PlannedJob;

/// A job as it travels to a runner.
///
/// Self-contained: everything needed to run it, so a runner needs no database
/// to start and no second round trip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedJob {
    pub job_id: String,
    pub run_id: String,
    pub repo_id: String,
    /// The commit to check out and run against.
    pub head_oid: String,
    pub job: PlannedJob,
}

/// How a runner reports back on a job it was given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// Ran it, for whatever outcome. Do not deliver it again.
    Done,
    /// Could not take it. Give it to someone else.
    Release,
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;
    use crate::workflow::Step;

    #[test]
    fn a_queued_job_survives_the_queue_topic() {
        let job = QueuedJob {
            job_id: "j1".into(),
            run_id: "r1".into(),
            repo_id: "repo1".into(),
            head_oid: "abc123".into(),
            job: PlannedJob {
                name: "test".into(),
                image: "rust:1.97".into(),
                timeout_minutes: 30,
                env: vec![("K".into(), "V".into())],
                steps: vec![Step {
                    name: None,
                    run: "cargo test".into(),
                }],
            },
        };
        let bytes = serde_json::to_vec(&job).unwrap();
        let back: QueuedJob = serde_json::from_slice(&bytes).unwrap();
        check!(back == job);
    }
}
