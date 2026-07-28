//! Domain events, one enum per aggregate.
//!
//! These are the system of record. Everything queryable is a projection of this
//! stream, so an event that is wrong is wrong forever — hence the versioning
//! discipline in [`crate::Envelope`].

use forge_types::{CommentId, IssueId, Oid, PrId, RepoId, Role, UserId, Visibility};
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
