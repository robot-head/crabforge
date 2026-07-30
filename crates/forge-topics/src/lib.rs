//! The Crabforge topic taxonomy, and idempotent provisioning of it.
//!
//! Crabka's broker has no topic auto-creation and rejects `num_partitions = -1`,
//! so every topic the forge uses is declared here explicitly and created at
//! bootstrap. [`ensure`] is safe to run on every boot.

use std::collections::BTreeMap;

use crabka_client_admin::{AdminClient, AdminError, CreateTopicSpec};
use forge_types::RepoId;

mod manifest;

pub use manifest::{Cleanup, TopicSpec, repo_objects_topic, static_topics};

/// `TOPIC_ALREADY_EXISTS` — the expected outcome of re-running bootstrap.
const TOPIC_ALREADY_EXISTS: i16 = 36;

/// How long the broker may take to create a batch of topics.
const CREATE_TIMEOUT_MS: i32 = 30_000;

#[derive(Debug, thiserror::Error)]
pub enum TopicError {
    #[error("admin client: {0}")]
    Admin(#[from] AdminError),
    #[error("creating topic '{topic}' failed: {name} ({code})")]
    Create {
        topic: String,
        code: i16,
        name: &'static str,
    },
}

/// Create every topic in `specs` that does not already exist.
///
/// Idempotent: an existing topic reports `TOPIC_ALREADY_EXISTS`, which is the
/// normal steady-state result and is not an error. Config drift on an existing
/// topic is warned about rather than corrected — altering a live topic's
/// retention or cleanup policy is an operator decision, not a boot-time one.
pub async fn ensure(admin: &mut AdminClient, specs: &[TopicSpec]) -> Result<(), TopicError> {
    if specs.is_empty() {
        return Ok(());
    }
    let create: Vec<CreateTopicSpec> = specs.iter().map(TopicSpec::to_create_spec).collect();
    let outcomes = admin.create_topics(&create, CREATE_TIMEOUT_MS).await?;

    for outcome in outcomes {
        match outcome.error {
            None => tracing::info!(topic = %outcome.name, "created topic"),
            Some(e) if e.code == TOPIC_ALREADY_EXISTS => {
                tracing::debug!(topic = %outcome.name, "topic already exists");
            }
            Some(e) => {
                return Err(TopicError::Create {
                    topic: outcome.name,
                    code: e.code,
                    name: e.name,
                });
            }
        }
    }
    Ok(())
}

/// Provision the topics every forge deployment needs, unreplicated.
pub async fn ensure_static(admin: &mut AdminClient) -> Result<(), TopicError> {
    ensure_static_replicated(admin, 1).await
}

/// Provision them with `replicas` copies of every partition.
///
/// The number belongs to the deployment rather than to the manifest: one is
/// right for the single broker a laptop runs and wrong for the three-broker
/// cluster `deploy/k8s` describes, where it would mean three brokers and one
/// copy of the event history.
pub async fn ensure_static_replicated(
    admin: &mut AdminClient,
    replicas: i32,
) -> Result<(), TopicError> {
    let specs: Vec<_> = static_topics()
        .into_iter()
        .map(|spec| spec.replicated(replicas))
        .collect();
    ensure(admin, &specs).await
}

/// Provision the per-repository object topic.
///
/// Called when a repository is created, and again lazily on first push so a
/// repo created before this code shipped still gets its topic.
pub async fn ensure_repo(admin: &mut AdminClient, repo: RepoId) -> Result<(), TopicError> {
    ensure_repo_replicated(admin, repo, 1).await
}

/// The same, with `replicas` copies. A repository's objects are the repository;
/// on a multi-broker cluster they need as many copies as the event history.
pub async fn ensure_repo_replicated(
    admin: &mut AdminClient,
    repo: RepoId,
    replicas: i32,
) -> Result<(), TopicError> {
    ensure(admin, &[repo_objects_topic(repo).replicated(replicas)]).await
}

/// Report which of `specs` already exist, for `crabforge doctor`.
pub async fn missing(
    admin: &mut AdminClient,
    specs: &[TopicSpec],
) -> Result<Vec<String>, TopicError> {
    let metadata = admin.metadata(&[]).await?;
    let existing: BTreeMap<&str, ()> = metadata
        .topics
        .iter()
        .map(|t| (t.name.as_str(), ()))
        .collect();
    Ok(specs
        .iter()
        .filter(|s| !existing.contains_key(s.name.as_str()))
        .map(|s| s.name.clone())
        .collect())
}
