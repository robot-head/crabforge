//! Accepting a push.
//!
//! `git receive-pack` does the protocol work, but the decision about whether a
//! reference may move belongs to the forge — and the objects have to reach the
//! log before that decision is committed. Git's quarantine mechanism is what
//! makes both possible.
//!
//! ## The sequence
//!
//! 1. `receive-pack` unpacks the incoming pack into a *quarantine* directory.
//!    Nothing in the repository is modified yet.
//! 2. It runs the `pre-receive` hook, passing `<old> <new> <ref>` lines on
//!    stdin and the quarantine path in `GIT_QUARANTINE_PATH`.
//! 3. The hook calls back into this process. We enumerate the quarantined
//!    objects, write them to the repository's topic, and ask the command
//!    service to move the references with compare-and-swap.
//! 4. The hook exits zero, and git migrates the objects and updates the
//!    references in the cache. On a non-zero exit git discards the quarantine
//!    and the push fails — leaving at worst some unreferenced objects in the
//!    log, which is garbage rather than corruption.
//!
//! The ordering matters: objects reach the log *before* the reference that
//! names them. A reader can therefore never see a reference pointing at an
//! object the log does not have.

use std::path::Path;

use forge_command::{RefResult, RefUpdate};
use forge_git::{Object, ObjectWriter, import};
use forge_types::{Oid, RepoId, UserId};

use crate::service::{GitError, GitState};

/// One line of the pre-receive hook's stdin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProposedUpdate {
    pub old: Oid,
    pub new: Oid,
    pub name: String,
}

/// Parse the hook's stdin: `<old-oid> SP <new-oid> SP <ref-name> LF`.
pub fn parse_hook_input(input: &str) -> Vec<ProposedUpdate> {
    input
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let old = parts.next()?.parse().ok()?;
            let new = parts.next()?.parse().ok()?;
            let name = parts.next()?.to_string();
            Some(ProposedUpdate { old, new, name })
        })
        .collect()
}

impl ProposedUpdate {
    /// Convert git's all-zeroes convention into explicit creation and deletion.
    pub fn to_ref_update(&self) -> RefUpdate {
        RefUpdate {
            name: self.name.clone(),
            expected_old: (!self.old.is_zero()).then_some(self.old),
            new: (!self.new.is_zero()).then_some(self.new),
        }
    }
}

/// Handle one push, from the hook's perspective.
///
/// Returns the per-reference outcomes. An `Err` means the push could not be
/// evaluated at all; per-reference rejections come back as `Ok` with failures
/// inside.
pub async fn accept_push(
    state: &GitState,
    repo: RepoId,
    pusher: UserId,
    quarantine: Option<&Path>,
    repo_path: &Path,
    proposed: &[ProposedUpdate],
) -> Result<Vec<RefResult>, GitError> {
    let commands = state
        .commands
        .as_ref()
        .ok_or_else(|| GitError::Git("pushes need the command service".into()))?;
    let writer = state
        .writer
        .as_ref()
        .ok_or_else(|| GitError::Git("pushes need a log writer".into()))?;

    // Objects first, and outside the reference transaction. They are immutable
    // and content-addressed, so writing them early is safe even if the push is
    // then rejected: the cost is unreferenced objects, not a broken repository.
    if let Some(quarantine) = quarantine {
        let objects = read_quarantined_objects(quarantine, repo_path)?;
        if !objects.is_empty() {
            let count = ObjectWriter::new(writer, repo)
                .put_all(&objects)
                .await
                .map_err(|e| GitError::Git(e.to_string()))?;
            tracing::info!(%repo, objects = count, "stored pushed objects");
        }
    }

    let updates: Vec<RefUpdate> = proposed.iter().map(ProposedUpdate::to_ref_update).collect();
    commands
        .update_refs(repo, updates, pusher)
        .await
        .map_err(|e| GitError::Git(e.to_string()))
}

/// Read every object git put in the quarantine directory.
///
/// The quarantine is a real object directory; pointing git at it with the
/// repository as an alternate lets `cat-file` resolve deltas against objects
/// the repository already has, while enumerating only what is new.
fn read_quarantined_objects(quarantine: &Path, repo_path: &Path) -> Result<Vec<Object>, GitError> {
    let objects =
        import::read_objects_in(quarantine, repo_path).map_err(|e| GitError::Git(e.to_string()))?;
    Ok(objects)
}

/// The `pre-receive` hook script.
///
/// Deliberately tiny: it forwards stdin and the quarantine path to the forge
/// and exits with whatever the forge says. All the logic stays in Rust, where
/// it can be tested.
///
/// The token authenticates the callback to the forge's internal endpoint so
/// that nothing else on the host can approve a push by calling it.
pub fn hook_script(callback_url: &str, token: &str, repo_id: RepoId) -> String {
    format!(
        r#"#!/bin/sh
# Generated by Crabforge. Forwards a proposed push to the forge for a decision.
set -eu
payload=$(cat)
response=$(printf '%s' "$payload" | curl -sS -X POST \
  -H "X-Forge-Token: {token}" \
  -H "X-Forge-Repo: {repo_id}" \
  -H "X-Forge-Quarantine: ${{GIT_QUARANTINE_PATH:-}}" \
  --data-binary @- \
  -w '\n%{{http_code}}' \
  '{callback_url}')
status=$(printf '%s' "$response" | tail -n1)
body=$(printf '%s' "$response" | sed '$d')
if [ "$status" = "200" ]; then
  exit 0
fi
printf '%s\n' "$body" >&2
exit 1
"#
    )
}

/// Install the hook into a repository.
pub fn install_hook(
    repo_path: &Path,
    callback_url: &str,
    token: &str,
    repo_id: RepoId,
) -> Result<(), GitError> {
    let hooks = repo_path.join("hooks");
    std::fs::create_dir_all(&hooks)?;
    let path = hooks.join("pre-receive");
    std::fs::write(&path, hook_script(callback_url, token, repo_id))?;

    // The hook has to be executable, and git silently ignores one that is not —
    // which would mean every push was accepted without the forge ever seeing it.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;

    fn oid(seed: u8) -> Oid {
        let mut bytes = [0u8; 20];
        bytes[0] = seed;
        bytes[19] = seed;
        Oid::from_bytes(bytes)
    }

    #[test]
    fn hook_input_parses_into_updates() {
        let input = format!(
            "{} {} refs/heads/main\n{} {} refs/tags/v1\n",
            oid(1),
            oid(2),
            oid(3),
            oid(4)
        );
        let parsed = parse_hook_input(&input);

        check!(parsed.len() == 2);
        check!(parsed[0].name == "refs/heads/main");
        check!(parsed[1].old == oid(3));
    }

    #[test]
    fn the_zero_oid_means_creation_or_deletion() {
        // Git's convention on the wire, which the forge models explicitly so a
        // creation cannot be confused with a move from a real commit.
        let created = ProposedUpdate {
            old: Oid::zero(),
            new: oid(1),
            name: "refs/heads/new".into(),
        }
        .to_ref_update();
        check!(created.expected_old.is_none());
        check!(created.new == Some(oid(1)));

        let deleted = ProposedUpdate {
            old: oid(1),
            new: Oid::zero(),
            name: "refs/heads/gone".into(),
        }
        .to_ref_update();
        check!(deleted.expected_old == Some(oid(1)));
        check!(deleted.new.is_none());
    }

    #[test]
    fn malformed_hook_lines_are_ignored() {
        let parsed = parse_hook_input("garbage\n\n  \nnot even close\n");
        check!(parsed.is_empty());
    }

    #[test]
    fn the_hook_script_fails_the_push_when_the_forge_says_no() {
        let script = hook_script(
            "http://127.0.0.1:7000/internal/hooks/pre-receive",
            "s3cret",
            RepoId::new(),
        );
        // Only a 200 may let the push through.
        check!(script.contains(r#"if [ "$status" = "200" ]"#));
        check!(script.contains("exit 1"));
        // The quarantine path has to reach the forge or objects are never read.
        check!(script.contains("GIT_QUARANTINE_PATH"));
        // `set -eu` so a failed curl cannot leave the script accepting.
        check!(script.contains("set -eu"));
    }

    #[test]
    fn the_hook_carries_its_authentication_token() {
        let script = hook_script("http://localhost/hook", "tok3n", RepoId::new());
        check!(script.contains("X-Forge-Token: tok3n"));
    }
}
