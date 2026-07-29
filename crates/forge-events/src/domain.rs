//! Domain events, one enum per aggregate.
//!
//! These are the system of record. Everything queryable is a projection of this
//! stream, so an event that is wrong is wrong forever — hence the versioning
//! discipline in [`crate::Envelope`].

use forge_types::{CommentId, IssueId, JobId, Oid, PrId, RepoId, Role, RunId, UserId, Visibility};
use serde::{Deserialize, Serialize};

use crate::{DomainEvent, topics};

/// Account lifecycle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UserEvent {
    Registered {
        user_id: UserId,
        username: String,
        /// Pre-lowered lookup key; also the uniqueness claim in the catalog.
        username_lower: String,
        email: String,
        /// argon2id PHC string. Never the password.
        password_hash: String,
    },
    ProfileUpdated {
        user_id: UserId,
        display_name: Option<String>,
        bio: Option<String>,
    },
    Deactivated {
        user_id: UserId,
    },
}

impl DomainEvent for UserEvent {
    fn topic(&self) -> &'static str {
        topics::EVENTS_USERS
    }

    fn event_type(&self) -> &'static str {
        match self {
            Self::Registered { .. } => "user.registered",
            Self::ProfileUpdated { .. } => "user.profile_updated",
            Self::Deactivated { .. } => "user.deactivated",
        }
    }

    fn aggregate_id(&self) -> String {
        match self {
            Self::Registered { user_id, .. }
            | Self::ProfileUpdated { user_id, .. }
            | Self::Deactivated { user_id } => user_id.to_string(),
        }
    }
}

/// Repository lifecycle and access control.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RepoEvent {
    Created {
        repo_id: RepoId,
        owner_id: UserId,
        owner_name: String,
        name: String,
        /// `owner/name`, pre-lowered — the indexed lookup key in gres.
        full_name_lower: String,
        description: Option<String>,
        default_branch: String,
        visibility: Visibility,
    },
    Renamed {
        repo_id: RepoId,
        name: String,
        full_name_lower: String,
    },
    DescriptionChanged {
        repo_id: RepoId,
        description: Option<String>,
    },
    VisibilityChanged {
        repo_id: RepoId,
        visibility: Visibility,
    },
    DefaultBranchChanged {
        repo_id: RepoId,
        default_branch: String,
    },
    CollaboratorAdded {
        repo_id: RepoId,
        user_id: UserId,
        username: String,
        role: Role,
    },
    CollaboratorRemoved {
        repo_id: RepoId,
        user_id: UserId,
    },
    Deleted {
        repo_id: RepoId,
    },
}

impl DomainEvent for RepoEvent {
    fn topic(&self) -> &'static str {
        topics::EVENTS_REPOS
    }

    fn event_type(&self) -> &'static str {
        match self {
            Self::Created { .. } => "repo.created",
            Self::Renamed { .. } => "repo.renamed",
            Self::DescriptionChanged { .. } => "repo.description_changed",
            Self::VisibilityChanged { .. } => "repo.visibility_changed",
            Self::DefaultBranchChanged { .. } => "repo.default_branch_changed",
            Self::CollaboratorAdded { .. } => "repo.collaborator_added",
            Self::CollaboratorRemoved { .. } => "repo.collaborator_removed",
            Self::Deleted { .. } => "repo.deleted",
        }
    }

    fn aggregate_id(&self) -> String {
        match self {
            Self::Created { repo_id, .. }
            | Self::Renamed { repo_id, .. }
            | Self::DescriptionChanged { repo_id, .. }
            | Self::VisibilityChanged { repo_id, .. }
            | Self::DefaultBranchChanged { repo_id, .. }
            | Self::CollaboratorAdded { repo_id, .. }
            | Self::CollaboratorRemoved { repo_id, .. }
            | Self::Deleted { repo_id } => repo_id.to_string(),
        }
    }
}

/// Issues and their conversations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IssueEvent {
    Opened {
        issue_id: IssueId,
        repo_id: RepoId,
        /// Per repository, allocated by the command service.
        number: i64,
        title: String,
        body: Option<String>,
        author_id: UserId,
        author_name: String,
    },
    Commented {
        comment_id: CommentId,
        issue_id: IssueId,
        repo_id: RepoId,
        author_id: UserId,
        author_name: String,
        body: String,
    },
    TitleChanged {
        issue_id: IssueId,
        repo_id: RepoId,
        title: String,
    },
    Closed {
        issue_id: IssueId,
        repo_id: RepoId,
        actor: UserId,
    },
    Reopened {
        issue_id: IssueId,
        repo_id: RepoId,
        actor: UserId,
    },
}

impl DomainEvent for IssueEvent {
    fn topic(&self) -> &'static str {
        topics::EVENTS_ISSUES
    }

    fn event_type(&self) -> &'static str {
        match self {
            Self::Opened { .. } => "issue.opened",
            Self::Commented { .. } => "issue.commented",
            Self::TitleChanged { .. } => "issue.title_changed",
            Self::Closed { .. } => "issue.closed",
            Self::Reopened { .. } => "issue.reopened",
        }
    }

    fn aggregate_id(&self) -> String {
        // Keyed by repository, not by issue: a repository's issue events must
        // stay mutually ordered, or a projector could apply a comment before
        // the issue it belongs to.
        match self {
            Self::Opened { repo_id, .. }
            | Self::Commented { repo_id, .. }
            | Self::TitleChanged { repo_id, .. }
            | Self::Closed { repo_id, .. }
            | Self::Reopened { repo_id, .. } => repo_id.to_string(),
        }
    }
}

/// Pull requests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PrEvent {
    Opened {
        pr_id: PrId,
        repo_id: RepoId,
        /// Shares the issue sequence, so `#7` is unambiguous.
        number: i64,
        title: String,
        body: Option<String>,
        author_id: UserId,
        author_name: String,
        source_branch: String,
        target_branch: String,
        head_oid: Oid,
        base_oid: Oid,
    },
    /// The source branch moved, so the diff and mergeability are both stale.
    Synchronized {
        pr_id: PrId,
        repo_id: RepoId,
        head_oid: Oid,
        base_oid: Oid,
    },
    /// The result of a trial merge.
    ///
    /// An event rather than a cached computation because it is what the merge
    /// button is enabled from, and because "when did this become conflicted"
    /// is a question people ask.
    MergeabilityComputed {
        pr_id: PrId,
        repo_id: RepoId,
        head_oid: Oid,
        base_oid: Oid,
        mergeable: bool,
        conflicts: Vec<String>,
    },
    Reviewed {
        review_id: CommentId,
        pr_id: PrId,
        repo_id: RepoId,
        reviewer_id: UserId,
        reviewer_name: String,
        verdict: ReviewVerdict,
        body: Option<String>,
    },
    Merged {
        pr_id: PrId,
        repo_id: RepoId,
        merge_commit_oid: Oid,
        merged_by: UserId,
        merged_by_name: String,
    },
    Closed {
        pr_id: PrId,
        repo_id: RepoId,
        actor: UserId,
    },
    Reopened {
        pr_id: PrId,
        repo_id: RepoId,
        actor: UserId,
    },
}

/// What a reviewer decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewVerdict {
    Approve,
    RequestChanges,
    Comment,
}

impl ReviewVerdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Approve => "approve",
            Self::RequestChanges => "request_changes",
            Self::Comment => "comment",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "approve" => Some(Self::Approve),
            "request_changes" => Some(Self::RequestChanges),
            "comment" => Some(Self::Comment),
            _ => None,
        }
    }
}

impl DomainEvent for PrEvent {
    fn topic(&self) -> &'static str {
        topics::EVENTS_PRS
    }

    fn event_type(&self) -> &'static str {
        match self {
            Self::Opened { .. } => "pr.opened",
            Self::Synchronized { .. } => "pr.synchronized",
            Self::MergeabilityComputed { .. } => "pr.mergeability_computed",
            Self::Reviewed { .. } => "pr.reviewed",
            Self::Merged { .. } => "pr.merged",
            Self::Closed { .. } => "pr.closed",
            Self::Reopened { .. } => "pr.reopened",
        }
    }

    fn aggregate_id(&self) -> String {
        // By repository, so a review cannot be applied before the pull request
        // it belongs to.
        match self {
            Self::Opened { repo_id, .. }
            | Self::Synchronized { repo_id, .. }
            | Self::MergeabilityComputed { repo_id, .. }
            | Self::Reviewed { repo_id, .. }
            | Self::Merged { repo_id, .. }
            | Self::Closed { repo_id, .. }
            | Self::Reopened { repo_id, .. } => repo_id.to_string(),
        }
    }
}

/// Reference updates — the forge's global reflog.
///
/// Retained forever, so "what did this branch point at last Tuesday" is always
/// answerable even after a force-push.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GitRefEvent {
    RefUpdated {
        repo_id: RepoId,
        /// Fully qualified, e.g. `refs/heads/main`.
        r#ref: String,
        /// `None` when the ref is being created.
        old: Option<Oid>,
        /// `None` when the ref is being deleted.
        new: Option<Oid>,
        pusher: UserId,
        /// True when the new tip does not descend from the old one.
        forced: bool,
    },
}

impl DomainEvent for GitRefEvent {
    fn topic(&self) -> &'static str {
        topics::EVENTS_GIT_REFS
    }

    fn event_type(&self) -> &'static str {
        match self {
            Self::RefUpdated { .. } => "git.ref_updated",
        }
    }

    fn aggregate_id(&self) -> String {
        match self {
            // Keyed by repository, not by ref: ref updates within a repository
            // must stay mutually ordered so a projector sees them in the order
            // the pusher made them.
            Self::RefUpdated { repo_id, .. } => repo_id.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;

    #[test]
    fn every_event_type_is_distinct_within_its_aggregate() {
        let repo = RepoId::new();
        let user = UserId::new();
        let events: Vec<Box<dyn Fn() -> &'static str>> = vec![];
        drop(events);

        let repo_events = [
            RepoEvent::Deleted { repo_id: repo },
            RepoEvent::VisibilityChanged {
                repo_id: repo,
                visibility: Visibility::Private,
            },
            RepoEvent::CollaboratorRemoved {
                repo_id: repo,
                user_id: user,
            },
        ];
        let mut names: Vec<&str> = repo_events.iter().map(DomainEvent::event_type).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        check!(names.len() == before);
    }

    #[test]
    fn issue_events_are_keyed_by_repository_so_they_stay_ordered() {
        // A comment applied before the issue it belongs to would be dropped.
        let repo = RepoId::new();
        let opened = IssueEvent::Opened {
            issue_id: IssueId::new(),
            repo_id: repo,
            number: 1,
            title: "t".into(),
            body: None,
            author_id: UserId::new(),
            author_name: "a".into(),
        };
        let commented = IssueEvent::Commented {
            comment_id: CommentId::new(),
            issue_id: IssueId::new(),
            repo_id: repo,
            author_id: UserId::new(),
            author_name: "a".into(),
            body: "b".into(),
        };
        check!(opened.aggregate_id() == commented.aggregate_id());
        check!(opened.topic() == topics::EVENTS_ISSUES);
    }

    #[test]
    fn events_route_to_their_aggregate_topic() {
        let repo = RepoId::new();
        check!(RepoEvent::Deleted { repo_id: repo }.topic() == topics::EVENTS_REPOS);
        check!(
            GitRefEvent::RefUpdated {
                repo_id: repo,
                r#ref: "refs/heads/main".into(),
                old: None,
                new: None,
                pusher: UserId::new(),
                forced: false,
            }
            .topic()
                == topics::EVENTS_GIT_REFS
        );
    }

    #[test]
    fn ref_updates_are_keyed_by_repository_so_they_stay_ordered() {
        let repo = RepoId::new();
        let make = |name: &str| GitRefEvent::RefUpdated {
            repo_id: repo,
            r#ref: name.into(),
            old: None,
            new: None,
            pusher: UserId::new(),
            forced: false,
        };
        check!(make("refs/heads/main").aggregate_id() == make("refs/heads/dev").aggregate_id());
    }

    #[test]
    fn payloads_round_trip_with_their_discriminant() {
        let event = RepoEvent::CollaboratorAdded {
            repo_id: RepoId::new(),
            user_id: UserId::new(),
            username: "octocat".into(),
            role: Role::Write,
        };
        let json = serde_json::to_value(&event).unwrap();
        check!(json["kind"] == "collaborator_added");
        check!(serde_json::from_value::<RepoEvent>(json).unwrap() == event);
    }
}

/// Something happened to a CI run or one of its jobs.
///
/// Runs and jobs are event-sourced like everything else, which is what lets a
/// dropped database be rebuilt with the same build history rather than an empty
/// one. It also decides where the truth lives while a job is executing: the
/// runner reports by appending here, and `ci_jobs` is a projection of what it
/// said — so a runner that dies has said nothing, and the reconciler can tell
/// that from a runner that said "failed".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CiEvent {
    /// A push matched a workflow and its jobs were planned.
    ///
    /// Carries every job up front rather than one event per job: they are
    /// decided together from one commit, and a partial plan reaching the
    /// projector would show a run with jobs still appearing.
    RunQueued {
        run_id: RunId,
        repo_id: RepoId,
        /// Per repository, allocated by the command service.
        number: i64,
        /// Repo-relative path of the workflow file.
        workflow: String,
        /// The workflow's display name.
        name: String,
        /// What triggered it.
        event: String,
        /// The commit the workflow was read at and jobs run against.
        head_oid: Oid,
        /// Fully qualified, e.g. `refs/heads/main`.
        ref_name: String,
        actor_name: String,
        jobs: Vec<PlannedJobSpec>,
    },
    /// A runner picked a job up.
    ///
    /// `attempt` is the delivery this claim is for. Consumption is
    /// at-least-once, so two runners can be handed the same job; the attempt
    /// number is what lets the projector keep the first claim and ignore a
    /// later one for an earlier delivery.
    JobStarted {
        job_id: JobId,
        run_id: RunId,
        repo_id: RepoId,
        attempt: i64,
        /// Where this job's log chunks begin on the log topic.
        log_offset: i64,
    },
    /// A runner finished a job, for any definition of finished.
    JobFinished {
        job_id: JobId,
        run_id: RunId,
        repo_id: RepoId,
        attempt: i64,
        conclusion: JobConclusion,
        /// Absent when the job never produced one — a timeout, or a sandbox
        /// that could not start.
        exit_code: Option<i32>,
    },
    /// Every job of a run has finished.
    ///
    /// Derived rather than observed: a run's conclusion is a function of its
    /// jobs', and recording it separately means the UI does not have to
    /// recompute it on every page view.
    RunFinished {
        run_id: RunId,
        repo_id: RepoId,
        conclusion: RunConclusion,
    },
}

/// A job as planned, carried in [`CiEvent::RunQueued`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedJobSpec {
    pub job_id: JobId,
    /// The key from the workflow's `jobs:` map.
    pub name: String,
    pub image: String,
}

/// How a job ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobConclusion {
    Success,
    /// Ran and reported failure — something about the code.
    Failed,
    /// Killed for overrunning its timeout.
    TimedOut,
    /// Never really ran: no such image, no runner, a sandbox that broke.
    /// Kept apart from `Failed` so nobody debugs their tests over it.
    InfraFailed,
    Cancelled,
}

impl JobConclusion {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failed => "failed",
            Self::TimedOut => "timed_out",
            Self::InfraFailed => "infra_failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn is_success(self) -> bool {
        matches!(self, Self::Success)
    }
}

/// How a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunConclusion {
    Success,
    Failed,
    Cancelled,
}

impl RunConclusion {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// A run is as good as its worst job.
    ///
    /// Anything that is not a success fails the run, including a job that never
    /// ran: a green check on a run whose jobs did not execute is the one
    /// outcome a CI system must never produce.
    pub fn from_jobs(conclusions: impl IntoIterator<Item = JobConclusion>) -> Self {
        let mut cancelled_only = true;
        let mut any = false;
        for conclusion in conclusions {
            any = true;
            match conclusion {
                JobConclusion::Success => cancelled_only = false,
                JobConclusion::Cancelled => {}
                _ => return Self::Failed,
            }
        }
        match (any, cancelled_only) {
            (true, true) => Self::Cancelled,
            _ => Self::Success,
        }
    }
}

impl DomainEvent for CiEvent {
    fn topic(&self) -> &'static str {
        topics::EVENTS_CI
    }

    fn event_type(&self) -> &'static str {
        match self {
            Self::RunQueued { .. } => "ci.run_queued",
            Self::JobStarted { .. } => "ci.job_started",
            Self::JobFinished { .. } => "ci.job_finished",
            Self::RunFinished { .. } => "ci.run_finished",
        }
    }

    fn aggregate_id(&self) -> String {
        // Keyed by run, not by job: a run's events must stay mutually ordered,
        // or a projector could see a job finish before the run that owns it
        // exists.
        match self {
            Self::RunQueued { run_id, .. }
            | Self::JobStarted { run_id, .. }
            | Self::JobFinished { run_id, .. }
            | Self::RunFinished { run_id, .. } => run_id.to_string(),
        }
    }
}

#[cfg(test)]
mod ci_tests {
    use assert2::check;

    use super::*;

    #[test]
    fn a_run_is_as_good_as_its_worst_job() {
        use JobConclusion::*;
        check!(RunConclusion::from_jobs([Success, Success]) == RunConclusion::Success);
        check!(RunConclusion::from_jobs([Success, Failed]) == RunConclusion::Failed);
        check!(RunConclusion::from_jobs([Success, TimedOut]) == RunConclusion::Failed);
    }

    #[test]
    fn a_job_that_never_ran_does_not_produce_a_green_check() {
        // The outcome a CI system must never produce: passing because nothing
        // was executed. An infrastructure failure fails the run.
        use JobConclusion::*;
        check!(RunConclusion::from_jobs([Success, InfraFailed]) == RunConclusion::Failed);
        check!(RunConclusion::from_jobs([InfraFailed]) == RunConclusion::Failed);
    }

    #[test]
    fn a_run_whose_jobs_were_all_cancelled_is_cancelled_not_failed() {
        // Somebody stopped it on purpose; calling that a failure would put a
        // red cross on a pull request that was never tested.
        use JobConclusion::*;
        check!(RunConclusion::from_jobs([Cancelled, Cancelled]) == RunConclusion::Cancelled);
        // But a cancelled job alongside a real one does not hide the real one.
        check!(RunConclusion::from_jobs([Cancelled, Failed]) == RunConclusion::Failed);
        check!(RunConclusion::from_jobs([Cancelled, Success]) == RunConclusion::Success);
    }

    #[test]
    fn a_run_with_no_jobs_succeeds_vacuously() {
        // Unreachable today — a workflow with no jobs is refused at parse time
        // — but the fold has to answer something, and "failed" for a run that
        // was never asked to do anything would be a lie.
        check!(RunConclusion::from_jobs([]) == RunConclusion::Success);
    }

    #[test]
    fn ci_events_are_keyed_by_their_run() {
        // Ordering within a run is what stops a projector seeing a job finish
        // before the run that owns it exists.
        let run_id = RunId::new();
        let queued = CiEvent::RunQueued {
            run_id,
            repo_id: RepoId::new(),
            number: 1,
            workflow: ".crabforge/workflows/build.yml".into(),
            name: "build".into(),
            event: "push".into(),
            head_oid: Oid::from_bytes([7u8; 20]),
            ref_name: "refs/heads/main".into(),
            actor_name: "octocat".into(),
            jobs: Vec::new(),
        };
        let finished = CiEvent::RunFinished {
            run_id,
            repo_id: RepoId::new(),
            conclusion: RunConclusion::Success,
        };
        check!(queued.aggregate_id() == finished.aggregate_id());
        check!(queued.topic() == topics::EVENTS_CI);
        check!(queued.event_type() == "ci.run_queued");
    }

    #[test]
    fn ci_events_survive_their_json_encoding() {
        let event = CiEvent::JobFinished {
            job_id: JobId::new(),
            run_id: RunId::new(),
            repo_id: RepoId::new(),
            attempt: 2,
            conclusion: JobConclusion::TimedOut,
            exit_code: None,
        };
        let bytes = serde_json::to_vec(&event).unwrap();
        let back: CiEvent = serde_json::from_slice(&bytes).unwrap();
        check!(back == event);
    }
}
