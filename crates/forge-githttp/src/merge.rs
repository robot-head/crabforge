//! Performing a merge.
//!
//! Three parties are involved and the order between them is the whole design:
//!
//! 1. The **cache** computes the merge and produces objects. It has the object
//!    graph; the command service does not.
//! 2. Those objects go to the **log**, before anything references them.
//! 3. The **command service** compare-and-swaps the branch, in one transaction
//!    with the event that marks the pull request merged.
//!
//! Reversing any two of those creates a state someone can observe: a branch
//! pointing at an object the log does not have, or a pull request marked merged
//! whose branch never moved.

use std::sync::Arc;

use forge_command::{CommandService, MergePull};
use forge_git::{Cache, MergeAttempt, Object, ObjectWriter};
use forge_store::PullRecord;
use forge_types::{Oid, RepoId, UserId};

#[derive(Debug, thiserror::Error)]
pub enum MergeError {
    #[error("this pull request has conflicts that need resolving first")]
    Conflicts(Vec<String>),
    #[error("the branch moved while you were looking; review the new changes")]
    Stale,
    #[error("this pull request is not open")]
    NotOpen,
    #[error("git: {0}")]
    Git(String),
    #[error("command: {0}")]
    Command(String),
}

/// Who is performing a merge.
///
/// Grouped rather than passed as three strings: `(actor, name, email)` in a
/// positional argument list is easy to get subtly wrong, and the values end up
/// in a commit that is permanent.
pub struct Actor {
    pub id: UserId,
    pub name: String,
    pub email: String,
}

/// What a completed merge produced.
pub struct Merged {
    pub merge_commit: Oid,
    pub committed: forge_bus::Committed,
}

/// Merge a pull request.
pub async fn perform(
    cache: &Cache,
    writer: &forge_bus::FencedWriter,
    commands: &Arc<CommandService>,
    repo: RepoId,
    pr: &PullRecord,
    actor: &Actor,
) -> Result<Merged, MergeError> {
    if !pr.is_open() {
        return Err(MergeError::NotOpen);
    }

    let head: Oid = pr
        .head_oid
        .parse()
        .map_err(|_| MergeError::Git("stored head is not an object id".into()))?;
    let base: Oid = pr
        .base_oid
        .parse()
        .map_err(|_| MergeError::Git("stored base is not an object id".into()))?;

    // The branch as it is *now*, not as the pull request remembers it. If they
    // disagree, someone pushed while this was being reviewed.
    let current_base = cache
        .resolve(&pr.target_branch)
        .map_err(|e| MergeError::Git(e.to_string()))?
        .ok_or(MergeError::Stale)?;
    if current_base != base {
        return Err(MergeError::Stale);
    }

    let attempt = cache
        .try_merge(&pr.target_branch, &head.to_hex())
        .map_err(|e| MergeError::Git(e.to_string()))?;
    let tree = match attempt {
        MergeAttempt::Clean { tree } => tree,
        MergeAttempt::Conflict { files } => return Err(MergeError::Conflicts(files)),
    };

    let message = format!(
        "Merge pull request #{} from {}\n\n{}",
        pr.number, pr.source_branch, pr.title
    );
    let merge_commit = cache
        .commit_merge(tree, base, head, &message, &actor.name, &actor.email)
        .map_err(|e| MergeError::Git(e.to_string()))?;

    // Everything the merge created goes to the log before the branch names it.
    let objects = collect_new_objects(cache, merge_commit, &[base, head])?;
    ObjectWriter::new(writer, repo)
        .put_all(&objects)
        .await
        .map_err(|e| MergeError::Git(e.to_string()))?;

    let outcome = commands
        .merge_pull(MergePull {
            repo,
            pr: pr
                .pr_id
                .parse()
                .map_err(|_| MergeError::Command("stored pull id is not a uuid".into()))?,
            target_branch: pr.target_branch.clone(),
            expected_base: base,
            expected_head: head,
            merge_commit,
            merged_by: actor.id,
            merged_by_name: actor.name.clone(),
        })
        .await
        .map_err(|e| match e {
            forge_command::CommandError::StaleMerge { .. } => MergeError::Stale,
            other => MergeError::Command(other.to_string()),
        })?;

    Ok(Merged {
        merge_commit,
        committed: outcome.committed,
    })
}

/// Everything a merge added that the log does not already have.
///
/// Asking git rather than reasoning about it. A clean merge of two edits to one
/// file produces a new blob holding the combined result — present in neither
/// parent — plus a new tree for every directory on the way to it. Storing only
/// the commit and its root tree would leave a reference pointing at objects
/// nobody can fetch, and the failure would appear later, on a clone, as a
/// corrupt repository.
fn collect_new_objects(
    cache: &Cache,
    commit: Oid,
    parents: &[Oid],
) -> Result<Vec<Object>, MergeError> {
    let listed = cache
        .objects_added_by(commit, parents)
        .map_err(|e| MergeError::Git(e.to_string()))?;

    let mut objects = Vec::with_capacity(listed.len());
    for (oid, kind) in listed {
        let content = forge_git::loose::read(&cache.objects_dir(), oid)
            .map_err(|e| MergeError::Git(e.to_string()))?
            .map(|(_, content)| content)
            .ok_or_else(|| {
                MergeError::Git(format!("git did not write {oid} where it could be read"))
            })?;
        objects.push(Object { oid, kind, content });
    }
    Ok(objects)
}

/// Recompute a pull request's mergeability and record the answer.
///
/// Cheap enough to run whenever a branch moves, and far too expensive to run on
/// every page view — which is why the answer is stored.
pub async fn refresh_mergeability(
    cache: &Cache,
    commands: &Arc<CommandService>,
    repo: RepoId,
    pr: &PullRecord,
) -> Result<bool, MergeError> {
    let head: Oid = pr
        .head_oid
        .parse()
        .map_err(|_| MergeError::Git("stored head is not an object id".into()))?;
    let base: Oid = pr
        .base_oid
        .parse()
        .map_err(|_| MergeError::Git("stored base is not an object id".into()))?;

    let attempt = cache
        .try_merge(&base.to_hex(), &head.to_hex())
        .map_err(|e| MergeError::Git(e.to_string()))?;

    let mergeable = attempt.is_clean();
    commands
        .record_mergeability(forge_command::RecordMergeability {
            repo,
            pr: pr
                .pr_id
                .parse()
                .map_err(|_| MergeError::Command("stored pull id is not a uuid".into()))?,
            head_oid: head,
            base_oid: base,
            mergeable,
            conflicts: attempt.conflicted_files().to_vec(),
        })
        .await
        .map_err(|e| MergeError::Command(e.to_string()))?;

    Ok(mergeable)
}
