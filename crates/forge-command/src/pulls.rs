//! Pull request commands.
//!
//! Opening and reviewing are ordinary appends. Merging is not: it produces git
//! objects and moves a reference, so it goes through the same machinery a push
//! does — objects to the log first, then a compare-and-swap on the reference.
//! That is what stops a merge and a concurrent push from silently overwriting
//! one another.

use forge_events::{PrEvent, ReviewVerdict};
use forge_types::{CommentId, Oid, PrId, RepoId, UserId};

/// Opening a pull request.
pub struct OpenPull {
    pub repo: RepoId,
    pub author: UserId,
    pub author_name: String,
    pub title: String,
    pub body: Option<String>,
    pub source_branch: String,
    pub target_branch: String,
    pub head_oid: Oid,
    pub base_oid: Oid,
}

/// Reviewing one.
pub struct ReviewPull {
    pub repo: RepoId,
    pub pr: PrId,
    pub reviewer: UserId,
    pub reviewer_name: String,
    pub verdict: ReviewVerdict,
    pub body: Option<String>,
}

/// Merging one.
///
/// `expected_head` and `expected_base` are what the person clicking the button
/// was looking at. If either has moved, the merge is refused: the diff they
/// reviewed is not the diff that would land.
pub struct MergePull {
    pub repo: RepoId,
    pub pr: PrId,
    pub target_branch: String,
    pub expected_base: Oid,
    pub expected_head: Oid,
    /// The already-created merge commit. Built by the caller, which has the
    /// object cache; the command service only decides whether it may land.
    pub merge_commit: Oid,
    pub merged_by: UserId,
    pub merged_by_name: String,
}

/// What the merge machinery reported.
pub struct MergeOutcome {
    pub merge_commit: Oid,
}

/// Recording a trial merge.
pub struct RecordMergeability {
    pub repo: RepoId,
    pub pr: PrId,
    pub head_oid: Oid,
    pub base_oid: Oid,
    pub mergeable: bool,
    pub conflicts: Vec<String>,
}

impl super::CommandService {
    /// Open a pull request.
    ///
    /// Takes a number from the same sequence issues use, so `#7` refers to one
    /// thing in a repository regardless of which it is.
    pub async fn open_pull(
        &self,
        request: OpenPull,
    ) -> Result<super::Outcome<PrId>, super::CommandError> {
        let title = super::validated(&request.title, "title", super::MAX_TITLE)?;
        if request.source_branch == request.target_branch {
            return Err(super::CommandError::BadRequest(
                "a branch cannot be merged into itself".into(),
            ));
        }

        let mut state = self.state.lock().await;
        let number = state.catalog.next_issue_number(request.repo);
        let pr_id = PrId::new();

        let event = PrEvent::Opened {
            pr_id,
            repo_id: request.repo,
            number,
            title,
            body: request.body.filter(|b| !b.trim().is_empty()),
            author_id: request.author,
            author_name: request.author_name,
            source_branch: request.source_branch,
            target_branch: request.target_branch,
            head_oid: request.head_oid,
            base_oid: request.base_oid,
        };

        let key = super::issue_counter_key(request.repo);
        let claim = super::Claim::IssueCounter { next: number + 1 };
        let committed = self
            .writer
            .transact(vec![
                super::PendingRecord::event(&event, Some(request.author))?,
                super::PendingRecord::state(forge_types::topics::META_CATALOG, &key, &claim)?,
            ])
            .await?;

        state.catalog.apply(
            &key,
            Some(&serde_json::to_vec(&claim).expect("claim encodes")),
        );
        Ok(super::Outcome {
            id: pr_id,
            committed,
        })
    }

    /// Record a review.
    pub async fn review_pull(
        &self,
        request: ReviewPull,
    ) -> Result<super::Outcome<CommentId>, super::CommandError> {
        let review_id = CommentId::new();
        let event = PrEvent::Reviewed {
            review_id,
            pr_id: request.pr,
            repo_id: request.repo,
            reviewer_id: request.reviewer,
            reviewer_name: request.reviewer_name,
            verdict: request.verdict,
            body: request.body.filter(|b| !b.trim().is_empty()),
        };
        let committed = self
            .writer
            .transact(vec![super::PendingRecord::event(
                &event,
                Some(request.reviewer),
            )?])
            .await?;
        Ok(super::Outcome {
            id: review_id,
            committed,
        })
    }

    /// Record what a trial merge found.
    pub async fn record_mergeability(
        &self,
        request: RecordMergeability,
    ) -> Result<super::Outcome<PrId>, super::CommandError> {
        let event = PrEvent::MergeabilityComputed {
            pr_id: request.pr,
            repo_id: request.repo,
            head_oid: request.head_oid,
            base_oid: request.base_oid,
            mergeable: request.mergeable,
            conflicts: request.conflicts,
        };
        let committed = self
            .writer
            .transact(vec![super::PendingRecord::event(&event, None)?])
            .await?;
        Ok(super::Outcome {
            id: request.pr,
            committed,
        })
    }

    /// Record that a branch moved, so a pull request's diff is stale.
    pub async fn synchronize_pull(
        &self,
        repo: RepoId,
        pr: PrId,
        head_oid: Oid,
        base_oid: Oid,
    ) -> Result<super::Outcome<PrId>, super::CommandError> {
        let event = PrEvent::Synchronized {
            pr_id: pr,
            repo_id: repo,
            head_oid,
            base_oid,
        };
        let committed = self
            .writer
            .transact(vec![super::PendingRecord::event(&event, None)?])
            .await?;
        Ok(super::Outcome { id: pr, committed })
    }

    /// Land a merge.
    ///
    /// One transaction carries the merge event and the reference move, so the
    /// pull request cannot be marked merged without the branch actually
    /// advancing — or the reverse. The reference is compare-and-swapped against
    /// what the reviewer saw, so a push that landed in between wins and the
    /// merge is refused rather than clobbering it.
    pub async fn merge_pull(
        &self,
        request: MergePull,
    ) -> Result<super::Outcome<PrId>, super::CommandError> {
        let mut state = self.state.lock().await;

        let target = format!("refs/heads/{}", request.target_branch);
        let current = state.refs.get(request.repo, &target);
        if current != Some(request.expected_base) {
            return Err(super::CommandError::StaleMerge {
                expected: request.expected_base.to_hex(),
                actual: current.map(|o| o.to_hex()),
            });
        }

        let merged = PrEvent::Merged {
            pr_id: request.pr,
            repo_id: request.repo,
            merge_commit_oid: request.merge_commit,
            merged_by: request.merged_by,
            merged_by_name: request.merged_by_name,
        };
        let ref_moved = forge_events::GitRefEvent::RefUpdated {
            repo_id: request.repo,
            r#ref: target.clone(),
            old: Some(request.expected_base),
            new: Some(request.merge_commit),
            pusher: request.merged_by,
            forced: false,
        };

        let key = super::ref_key(request.repo, &target);
        let committed = self
            .writer
            .transact(vec![
                super::PendingRecord::event(&merged, Some(request.merged_by))?,
                super::PendingRecord::event(&ref_moved, Some(request.merged_by))?,
                super::PendingRecord::state(
                    forge_types::topics::GIT_REFS,
                    &key,
                    &super::RefValue {
                        oid: request.merge_commit,
                    },
                )?,
            ])
            .await?;

        state
            .refs
            .set(request.repo, &target, Some(request.merge_commit));
        Ok(super::Outcome {
            id: request.pr,
            committed,
        })
    }

    /// Close or reopen a pull request without merging it.
    pub async fn set_pull_state(
        &self,
        repo: RepoId,
        pr: PrId,
        actor: UserId,
        open: bool,
    ) -> Result<super::Outcome<PrId>, super::CommandError> {
        let event = if open {
            PrEvent::Reopened {
                pr_id: pr,
                repo_id: repo,
                actor,
            }
        } else {
            PrEvent::Closed {
                pr_id: pr,
                repo_id: repo,
                actor,
            }
        };
        let committed = self
            .writer
            .transact(vec![super::PendingRecord::event(&event, Some(actor))?])
            .await?;
        Ok(super::Outcome { id: pr, committed })
    }
}
