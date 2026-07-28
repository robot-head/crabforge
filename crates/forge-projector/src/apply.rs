//! Turning events into rows.
//!
//! Every function here must be idempotent: at-least-once delivery means an
//! event can arrive twice, and a replay from offset zero re-applies the whole
//! history. Applying an event a second time must leave the same row it left the
//! first time.

use forge_events::{IssueEvent, RepoEvent, UserEvent};
use forge_store::{CommentRecord, IssueRecord, Store, StoreError};
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
