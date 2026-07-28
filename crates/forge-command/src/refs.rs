//! The canonical reference map.
//!
//! Where a branch points is decided here, not in a git repository on disk and
//! not in gres. The cache is a mirror that can be rebuilt; the projection lags.
//! Neither can arbitrate a push.
//!
//! Reference updates are compare-and-swap: a client pushes "move `main` from
//! the commit I saw to this new one", and the update is refused if `main` has
//! moved since. Because the command service is the single fenced writer and
//! holds this map in memory, that comparison is exact — there is no window
//! between the check and the write for another push to slip through.

use std::collections::HashMap;

use forge_types::{Oid, RepoId};
use serde::{Deserialize, Serialize};

/// The current value of one reference, as stored in the compacted topic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefValue {
    pub oid: Oid,
}

/// Record key for a reference.
pub fn ref_key(repo: RepoId, name: &str) -> String {
    format!("{repo}/{name}")
}

/// What a client asked to do to one reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefUpdate {
    /// Fully qualified, e.g. `refs/heads/main`.
    pub name: String,
    /// What the client believes the reference points at. `None` means "this
    /// reference does not exist yet" — a creation.
    pub expected_old: Option<Oid>,
    /// Where it should point. `None` deletes it.
    pub new: Option<Oid>,
}

/// The outcome for one reference, reported back through git's report-status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefResult {
    pub name: String,
    pub outcome: Result<(), RefRejection>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RefRejection {
    /// The reference moved since the client last looked. Git calls this
    /// "fetch first" — the client has to reconcile before pushing again.
    #[error("stale info: expected {expected:?}, found {actual:?}")]
    Stale {
        expected: Option<Oid>,
        actual: Option<Oid>,
    },
    #[error("deleting a reference that does not exist")]
    DeletingMissing,
    #[error("reference name is not allowed")]
    BadName,
}

/// Every reference the forge knows about.
#[derive(Debug, Default)]
pub struct RefMap {
    refs: HashMap<String, Oid>,
}

impl RefMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply one record from the compacted topic.
    pub fn apply(&mut self, key: &str, value: Option<&[u8]>) {
        match value {
            Some(bytes) => match serde_json::from_slice::<RefValue>(bytes) {
                Ok(v) => {
                    self.refs.insert(key.to_string(), v.oid);
                }
                Err(e) => tracing::warn!(key, error = %e, "skipping unreadable ref record"),
            },
            None => {
                self.refs.remove(key);
            }
        }
    }

    pub fn get(&self, repo: RepoId, name: &str) -> Option<Oid> {
        self.refs.get(&ref_key(repo, name)).copied()
    }

    /// Every reference in one repository.
    pub fn for_repo(&self, repo: RepoId) -> Vec<(String, Oid)> {
        let prefix = format!("{repo}/");
        let mut out: Vec<(String, Oid)> = self
            .refs
            .iter()
            .filter_map(|(key, oid)| {
                key.strip_prefix(&prefix)
                    .map(|name| (name.to_string(), *oid))
            })
            .collect();
        out.sort();
        out
    }

    pub fn len(&self) -> usize {
        self.refs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.refs.is_empty()
    }

    /// Check one update against the current value without applying it.
    pub fn check(&self, repo: RepoId, update: &RefUpdate) -> Result<(), RefRejection> {
        if !is_valid_ref_name(&update.name) {
            return Err(RefRejection::BadName);
        }
        let actual = self.get(repo, &update.name);
        if actual != update.expected_old {
            return Err(RefRejection::Stale {
                expected: update.expected_old,
                actual,
            });
        }
        if update.new.is_none() && actual.is_none() {
            return Err(RefRejection::DeletingMissing);
        }
        Ok(())
    }

    /// Apply an update that has already passed [`RefMap::check`].
    pub fn set(&mut self, repo: RepoId, name: &str, oid: Option<Oid>) {
        let key = ref_key(repo, name);
        match oid {
            Some(oid) => {
                self.refs.insert(key, oid);
            }
            None => {
                self.refs.remove(&key);
            }
        }
    }
}

/// Whether a reference name is one the forge will store.
///
/// Deliberately stricter than git: these names end up in record keys, URLs and
/// filesystem paths, so anything that could escape a path or a key namespace is
/// refused rather than sanitized.
pub fn is_valid_ref_name(name: &str) -> bool {
    if !name.starts_with("refs/") || name.len() > 255 {
        return false;
    }
    if name.contains("..") || name.contains("//") || name.ends_with('/') || name.ends_with(".lock")
    {
        return false;
    }
    name.chars().all(|c| {
        !c.is_ascii_control()
            && !matches!(c, ' ' | '~' | '^' | ':' | '?' | '*' | '[' | '\\' | '\x7f')
    })
}

#[cfg(test)]
mod tests {
    use assert2::{assert, check};

    use super::*;
    use crate::catalog::tests_support::oid;

    fn encode(oid: Oid) -> Vec<u8> {
        serde_json::to_vec(&RefValue { oid }).unwrap()
    }

    #[test]
    fn a_ref_record_sets_the_current_value() {
        let mut map = RefMap::new();
        let repo = RepoId::new();
        let target = oid(1);
        map.apply(&ref_key(repo, "refs/heads/main"), Some(&encode(target)));

        check!(map.get(repo, "refs/heads/main") == Some(target));
        check!(map.get(repo, "refs/heads/other").is_none());
    }

    #[test]
    fn a_tombstone_deletes_the_ref() {
        let mut map = RefMap::new();
        let repo = RepoId::new();
        let key = ref_key(repo, "refs/heads/temp");
        map.apply(&key, Some(&encode(oid(1))));
        map.apply(&key, None);

        check!(map.get(repo, "refs/heads/temp").is_none());
    }

    #[test]
    fn refs_are_scoped_to_their_repository() {
        let mut map = RefMap::new();
        let (a, b) = (RepoId::new(), RepoId::new());
        map.apply(&ref_key(a, "refs/heads/main"), Some(&encode(oid(1))));

        check!(map.get(a, "refs/heads/main").is_some());
        check!(map.get(b, "refs/heads/main").is_none());
        check!(map.for_repo(b).is_empty());
    }

    #[test]
    fn creating_a_ref_requires_that_it_did_not_exist() {
        let map = RefMap::new();
        let repo = RepoId::new();

        check!(
            map.check(
                repo,
                &RefUpdate {
                    name: "refs/heads/main".into(),
                    expected_old: None,
                    new: Some(oid(1)),
                }
            )
            .is_ok()
        );
    }

    #[test]
    fn a_push_against_a_moved_ref_is_rejected() {
        // The race a forge must not lose: two people push to the same branch
        // from the same starting commit.
        let mut map = RefMap::new();
        let repo = RepoId::new();
        map.set(repo, "refs/heads/main", Some(oid(2)));

        let result = map.check(
            repo,
            &RefUpdate {
                name: "refs/heads/main".into(),
                expected_old: Some(oid(1)),
                new: Some(oid(3)),
            },
        );
        assert!(let Err(RefRejection::Stale { .. }) = result);
    }

    #[test]
    fn creating_a_ref_that_already_exists_is_rejected() {
        let mut map = RefMap::new();
        let repo = RepoId::new();
        map.set(repo, "refs/heads/main", Some(oid(1)));

        let result = map.check(
            repo,
            &RefUpdate {
                name: "refs/heads/main".into(),
                expected_old: None,
                new: Some(oid(2)),
            },
        );
        assert!(let Err(RefRejection::Stale { .. }) = result);
    }

    #[test]
    fn deleting_a_missing_ref_is_rejected() {
        let map = RefMap::new();
        let result = map.check(
            RepoId::new(),
            &RefUpdate {
                name: "refs/heads/gone".into(),
                expected_old: None,
                new: None,
            },
        );
        assert!(let Err(RefRejection::DeletingMissing) = result);
    }

    #[test]
    fn reference_names_are_validated() {
        check!(is_valid_ref_name("refs/heads/main"));
        check!(is_valid_ref_name("refs/tags/v1.0.0"));

        // Not under refs/.
        check!(!is_valid_ref_name("HEAD"));
        check!(!is_valid_ref_name("heads/main"));
        // Path traversal, which would escape a key namespace.
        check!(!is_valid_ref_name("refs/../../etc/passwd"));
        check!(!is_valid_ref_name("refs//heads/main"));
        // Characters git itself forbids.
        check!(!is_valid_ref_name("refs/heads/a b"));
        check!(!is_valid_ref_name("refs/heads/a~1"));
        check!(!is_valid_ref_name("refs/heads/a:b"));
        check!(!is_valid_ref_name("refs/heads/main.lock"));
        check!(!is_valid_ref_name("refs/heads/\nmain"));
    }

    #[test]
    fn a_bad_name_is_refused_before_any_comparison() {
        let map = RefMap::new();
        let result = map.check(
            RepoId::new(),
            &RefUpdate {
                name: "refs/../escape".into(),
                expected_old: None,
                new: Some(oid(1)),
            },
        );
        assert!(let Err(RefRejection::BadName) = result);
    }

    #[test]
    fn ref_keys_are_scoped_by_repository() {
        let (a, b) = (RepoId::new(), RepoId::new());
        check!(ref_key(a, "refs/heads/main") != ref_key(b, "refs/heads/main"));
        check!(ref_key(a, "refs/heads/main").starts_with(&a.to_string()));
    }

    #[test]
    fn listing_a_repository_returns_sorted_refs() {
        let mut map = RefMap::new();
        let repo = RepoId::new();
        map.set(repo, "refs/heads/zebra", Some(oid(1)));
        map.set(repo, "refs/heads/alpha", Some(oid(2)));

        let names: Vec<String> = map.for_repo(repo).into_iter().map(|(n, _)| n).collect();
        check!(names == vec!["refs/heads/alpha", "refs/heads/zebra"]);
    }
}
