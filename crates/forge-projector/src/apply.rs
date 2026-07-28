//! Turning events into rows.
//!
//! Every function here must be idempotent: at-least-once delivery means an
//! event can arrive twice, and a replay from offset zero re-applies the whole
//! history. Applying an event a second time must leave the same row it left the
//! first time.

use forge_events::{RepoEvent, UserEvent};
use forge_store::{Store, StoreError};
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
