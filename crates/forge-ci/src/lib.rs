//! Crab Actions: the forge's own CI.
//!
//! A push produces a *run* per triggered workflow, a run produces a *job* per
//! entry in its `jobs:` map, and a job is one container executing a list of
//! shell steps. Runs and jobs are projections of events on the log like
//! everything else, so a rebuilt database has the same history.
//!
//! The parts:
//!
//! * [`workflow`] — what a `.crabforge/workflows/*.yml` file may say.
//! * [`discover`] — reading those files at the pushed commit.
//! * [`plan`] — turning a discovery into the runs and jobs to create.
//! * [`sandbox`] — where a job's steps actually execute.

pub mod discover;
pub mod plan;
pub mod sandbox;
pub mod workflow;

pub use discover::{Discovered, Found, WORKFLOW_DIR, discover};
pub use plan::{PlannedJob, PlannedRun, plan_push};
pub use sandbox::{ProcessSandbox, Sandbox, StepOutcome, StepResult};
pub use workflow::{Job, Step, Trigger, Workflow, WorkflowError};
