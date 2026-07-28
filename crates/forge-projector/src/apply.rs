//! Turning events into rows.
//!
//! Every function here must be idempotent: at-least-once delivery means an
//! event can arrive twice, and a replay from offset zero re-applies the whole
//! history. Applying an event a second time must leave the same row it left the
//! first time.

use forge_events::{IssueEvent, PrEvent, RepoEvent, UserEvent};
use forge_store::{CommentRecord, IssueRecord, PullRecord, ReviewRecord, Store, StoreError};
use time::OffsetDateTime;

use crate::{repo_record_defaults, user_record};

/// Apply a user event to the read model.
pub async fn apply_user_event(
    store: &Store,
    event: &UserEvent,
    at: OffsetDateTime,
) -> Result<(), StoreError> {
    match event {
        UserEvent::Registered {
            user_id,
            username,
            username_lower,
            email,
            password_hash,
        } => {
            store
                .users()
                .upsert(&user_record(
                    &user_id.to_string(),
                    username,
                    username_lower,
                    email,
                    password_hash,
                    at,
                ))
                .await
        }
        UserEvent::ProfileUpdated {
            user_id,
            display_name,
            bio,
        } => {
            // Read-modify-write rather than a blind UPDATE, so a replay that
            // reaches this event before the registration it follows does not
            // silently do nothing.
            let Some(mut existing) = store.users().by_id(&user_id.to_string()).await? else {
                tracing::warn!(%user_id, "profile update for an unknown user; skipping");
                return Ok(());
            };
            existing.display_name = display_name.clone();
            existing.bio = bio.clone();
            existing.updated_at = at;
            store.users().upsert(&existing).await
        }
        UserEvent::Deactivated { user_id } => {
            let Some(mut existing) = store.users().by_id(&user_id.to_string()).await? else {
                return Ok(());
            };
            existing.state = "deactivated".to_string();
            existing.updated_at = at;
            store.users().upsert(&existing).await
        }
    }
}

/// Apply a repository event to the read model.
pub async fn apply_repo_event(
    store: &Store,
    event: &RepoEvent,
    at: OffsetDateTime,
) -> Result<(), StoreError> {
    match event {
        RepoEvent::Created {
            repo_id,
            owner_id,
            owner_name,
            name,
            full_name_lower,
            description,
            default_branch,
            visibility,
        } => {
            let mut record = repo_record_defaults(&repo_id.to_string(), at);
            record.owner_id = owner_id.to_string();
            record.owner_name = owner_name.clone();
            record.name = name.clone();
            record.full_name_lower = full_name_lower.clone();
            record.description = description.clone();
            record.default_branch = default_branch.clone();
            record.visibility = visibility.to_string();
            store.repos().upsert(&record).await
        }
        RepoEvent::Renamed {
            repo_id,
            name,
            full_name_lower,
        } => {
            update_repo(store, repo_id, at, |record| {
                record.name = name.clone();
                record.full_name_lower = full_name_lower.clone();
            })
            .await
        }
        RepoEvent::DescriptionChanged {
            repo_id,
            description,
        } => {
            update_repo(store, repo_id, at, |record| {
                record.description = description.clone();
            })
            .await
        }
        RepoEvent::VisibilityChanged {
            repo_id,
            visibility,
        } => {
            update_repo(store, repo_id, at, |record| {
                record.visibility = visibility.to_string();
            })
            .await
        }
        RepoEvent::DefaultBranchChanged {
            repo_id,
            default_branch,
        } => {
            update_repo(store, repo_id, at, |record| {
                record.default_branch = default_branch.clone();
            })
            .await
        }
        RepoEvent::Deleted { repo_id } => {
            // Soft delete: history and audit views still resolve the id, and
            // the repository's object topic can be reclaimed separately.
            update_repo(store, repo_id, at, |record| {
                record.deleted = true;
            })
            .await
        }
        // Collaborator membership gets its own table; not yet projected.
        RepoEvent::CollaboratorAdded { .. } | RepoEvent::CollaboratorRemoved { .. } => Ok(()),
    }
}

async fn update_repo<F>(
    store: &Store,
    repo_id: &forge_types::RepoId,
    at: OffsetDateTime,
    mutate: F,
) -> Result<(), StoreError>
where
    F: FnOnce(&mut forge_store::RepoRecord),
{
    let Some(mut record) = store.repos().by_id(&repo_id.to_string()).await? else {
        tracing::warn!(%repo_id, "event for an unknown repository; skipping");
        return Ok(());
    };
    mutate(&mut record);
    record.updated_at = at;
    store.repos().upsert(&record).await
}

/// Apply an issue event to the read model.
///
/// Counters are recomputed rather than incremented: an increment applied twice
/// during a replay would drift, and a counter that disagrees with the rows it
/// describes is worse than one that costs an extra query to maintain.
pub async fn apply_issue_event(
    store: &Store,
    event: &IssueEvent,
    at: OffsetDateTime,
) -> Result<(), StoreError> {
    match event {
        IssueEvent::Opened {
            issue_id,
            repo_id,
            number,
            title,
            body,
            author_id,
            author_name,
        } => {
            store
                .issues()
                .upsert(&IssueRecord {
                    issue_id: issue_id.to_string(),
                    repo_id: repo_id.to_string(),
                    number: *number,
                    title: title.clone(),
                    body: body.clone(),
                    author_id: author_id.to_string(),
                    author_name: author_name.clone(),
                    state: "open".to_string(),
                    comment_count: 0,
                    created_at: at,
                    updated_at: at,
                    closed_at: None,
                })
                .await?;
            store
                .issues()
                .refresh_counters(&repo_id.to_string())
                .await?;
            Ok(())
        }

        IssueEvent::Commented {
            comment_id,
            issue_id,
            repo_id,
            author_id,
            author_name,
            body,
        } => {
            store
                .issues()
                .insert_comment(&CommentRecord {
                    comment_id: comment_id.to_string(),
                    issue_id: issue_id.to_string(),
                    repo_id: repo_id.to_string(),
                    author_id: author_id.to_string(),
                    author_name: author_name.clone(),
                    body: body.clone(),
                    created_at: at,
                    updated_at: at,
                })
                .await?;

            // The count is derived from the comments actually stored, so a
            // redelivered comment cannot inflate it.
            if let Some(mut issue) = store.issues().by_id(&issue_id.to_string()).await? {
                let comments = store
                    .issues()
                    .comments(&issue_id.to_string(), forge_store::page_size(100))
                    .await?;
                issue.comment_count = comments.len() as i64;
                issue.updated_at = at;
                store.issues().upsert(&issue).await?;
            }
            Ok(())
        }

        IssueEvent::TitleChanged {
            issue_id, title, ..
        } => {
            update_issue(store, &issue_id.to_string(), at, |issue| {
                issue.title = title.clone();
            })
            .await
        }

        IssueEvent::Closed {
            issue_id, repo_id, ..
        } => {
            update_issue(store, &issue_id.to_string(), at, |issue| {
                issue.state = "closed".to_string();
                issue.closed_at = Some(at);
            })
            .await?;
            store
                .issues()
                .refresh_counters(&repo_id.to_string())
                .await?;
            Ok(())
        }

        IssueEvent::Reopened {
            issue_id, repo_id, ..
        } => {
            update_issue(store, &issue_id.to_string(), at, |issue| {
                issue.state = "open".to_string();
                issue.closed_at = None;
            })
            .await?;
            store
                .issues()
                .refresh_counters(&repo_id.to_string())
                .await?;
            Ok(())
        }
    }
}

async fn update_issue<F>(
    store: &Store,
    issue_id: &str,
    at: OffsetDateTime,
    mutate: F,
) -> Result<(), StoreError>
where
    F: FnOnce(&mut IssueRecord),
{
    let Some(mut issue) = store.issues().by_id(issue_id).await? else {
        tracing::warn!(issue_id, "event for an unknown issue; skipping");
        return Ok(());
    };
    mutate(&mut issue);
    issue.updated_at = at;
    store.issues().upsert(&issue).await
}

/// Apply a pull request event to the read model.
pub async fn apply_pr_event(
    store: &Store,
    event: &PrEvent,
    at: OffsetDateTime,
) -> Result<(), StoreError> {
    match event {
        PrEvent::Opened {
            pr_id,
            repo_id,
            number,
            title,
            body,
            author_id,
            author_name,
            source_branch,
            target_branch,
            head_oid,
            base_oid,
        } => {
            store
                .pulls()
                .upsert(&PullRecord {
                    pr_id: pr_id.to_string(),
                    repo_id: repo_id.to_string(),
                    number: *number,
                    title: title.clone(),
                    body: body.clone(),
                    author_id: author_id.to_string(),
                    author_name: author_name.clone(),
                    state: "open".to_string(),
                    source_branch: source_branch.clone(),
                    target_branch: target_branch.clone(),
                    head_oid: head_oid.to_hex(),
                    base_oid: base_oid.to_hex(),
                    // Nothing has tried to merge it yet. The worker will.
                    mergeable: "unknown".to_string(),
                    merge_commit_oid: None,
                    merged_by_name: None,
                    comment_count: 0,
                    created_at: at,
                    updated_at: at,
                    merged_at: None,
                    closed_at: None,
                })
                .await
        }

        PrEvent::Synchronized {
            pr_id,
            head_oid,
            base_oid,
            ..
        } => {
            update_pull(store, &pr_id.to_string(), at, |pr| {
                pr.head_oid = head_oid.to_hex();
                pr.base_oid = base_oid.to_hex();
                // The commits moved, so whatever the last trial merge concluded
                // was about a different pair. Saying "unknown" is honest and
                // disables the merge button until it is recomputed.
                pr.mergeable = "unknown".to_string();
            })
            .await
        }

        PrEvent::MergeabilityComputed {
            pr_id,
            head_oid,
            base_oid,
            mergeable,
            conflicts,
            ..
        } => {
            let pr_id = pr_id.to_string();
            let (head, base) = (head_oid.to_hex(), base_oid.to_hex());

            // Only apply if the pull request still points at the commits this
            // was computed for. A result that arrives after another push is
            // about history that has moved on.
            let Some(current) = store.pulls().by_id(&pr_id).await? else {
                return Ok(());
            };
            if current.head_oid != head || current.base_oid != base {
                tracing::debug!(%pr_id, "discarding a mergeability result for older commits");
                return Ok(());
            }

            store
                .pulls()
                .set_conflicts(&pr_id, &head, &base, conflicts)
                .await?;
            update_pull(store, &pr_id, at, |pr| {
                pr.mergeable = if *mergeable { "clean" } else { "conflict" }.to_string();
            })
            .await
        }

        PrEvent::Reviewed {
            review_id,
            pr_id,
            repo_id,
            reviewer_id,
            reviewer_name,
            verdict,
            body,
        } => {
            store
                .pulls()
                .insert_review(&ReviewRecord {
                    review_id: review_id.to_string(),
                    pr_id: pr_id.to_string(),
                    repo_id: repo_id.to_string(),
                    reviewer_id: reviewer_id.to_string(),
                    reviewer_name: reviewer_name.clone(),
                    verdict: verdict.as_str().to_string(),
                    body: body.clone(),
                    created_at: at,
                })
                .await
        }

        PrEvent::Merged {
            pr_id,
            merge_commit_oid,
            merged_by_name,
            ..
        } => {
            update_pull(store, &pr_id.to_string(), at, |pr| {
                pr.state = "merged".to_string();
                pr.merge_commit_oid = Some(merge_commit_oid.to_hex());
                pr.merged_by_name = Some(merged_by_name.clone());
                pr.merged_at = Some(at);
                pr.closed_at = Some(at);
            })
            .await
        }

        PrEvent::Closed { pr_id, .. } => {
            update_pull(store, &pr_id.to_string(), at, |pr| {
                // A merged pull request stays merged: closing is what happens
                // to one that was not.
                if pr.state != "merged" {
                    pr.state = "closed".to_string();
                    pr.closed_at = Some(at);
                }
            })
            .await
        }

        PrEvent::Reopened { pr_id, .. } => {
            update_pull(store, &pr_id.to_string(), at, |pr| {
                if pr.state != "merged" {
                    pr.state = "open".to_string();
                    pr.closed_at = None;
                    // Its mergeability was computed before it was closed and
                    // the base has probably moved since.
                    pr.mergeable = "unknown".to_string();
                }
            })
            .await
        }
    }
}

async fn update_pull<F>(
    store: &Store,
    pr_id: &str,
    at: OffsetDateTime,
    mutate: F,
) -> Result<(), StoreError>
where
    F: FnOnce(&mut PullRecord),
{
    let Some(mut pr) = store.pulls().by_id(pr_id).await? else {
        tracing::warn!(pr_id, "event for an unknown pull request; skipping");
        return Ok(());
    };
    mutate(&mut pr);
    pr.updated_at = at;
    store.pulls().upsert(&pr).await
}
