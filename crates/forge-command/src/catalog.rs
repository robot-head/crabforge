//! The uniqueness catalog.
//!
//! Usernames and repository names must be unique, and that decision cannot be
//! delegated to the database: gres has no unique indexes, and even if it did,
//! the projector lags the log, so a check against gres would let two
//! registrations for the same name both pass before either was projected.
//!
//! Instead the command service keeps the claims in memory, rebuilt at boot by
//! folding the compacted `forge.meta.catalog` topic, and writes each new claim
//! in the same broker transaction as the event that depends on it. Because the
//! service is the single fenced writer, its in-memory view is authoritative:
//! nothing else can claim a name behind its back.

use std::collections::HashMap;

use forge_types::{RepoId, UserId};
use serde::{Deserialize, Serialize};

/// What a catalog key currently points at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Claim {
    User {
        user_id: UserId,
    },
    Repo {
        repo_id: RepoId,
    },
    /// The next issue number to hand out for a repository.
    ///
    /// Kept in the catalog rather than derived by counting issues, so a boot
    /// costs one compacted read instead of replaying every issue ever opened —
    /// and so a deleted issue cannot cause a number to be reused.
    IssueCounter {
        next: i64,
    },
}

/// Record keys. Namespaced so users and repositories cannot collide.
pub fn user_key(username_lower: &str) -> String {
    format!("user:{username_lower}")
}

pub fn repo_key(full_name_lower: &str) -> String {
    format!("repo:{full_name_lower}")
}

/// Where a repository's issue counter lives.
pub fn issue_counter_key(repo: RepoId) -> String {
    format!("issuenum:{repo}")
}

/// Every name currently claimed.
#[derive(Debug, Default)]
pub struct Catalog {
    claims: HashMap<String, Claim>,
}

impl Catalog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply one record from the compacted topic.
    ///
    /// A `None` value is a tombstone: the name was released and is free again.
    pub fn apply(&mut self, key: &str, value: Option<&[u8]>) {
        match value {
            Some(bytes) => match serde_json::from_slice::<Claim>(bytes) {
                Ok(claim) => {
                    self.claims.insert(key.to_string(), claim);
                }
                Err(e) => {
                    // Skip rather than fail: a claim written by a newer version
                    // should not stop this one from booting.
                    tracing::warn!(key, error = %e, "skipping unreadable catalog record");
                }
            },
            None => {
                self.claims.remove(key);
            }
        }
    }

    pub fn is_claimed(&self, key: &str) -> bool {
        self.claims.contains_key(key)
    }

    pub fn get(&self, key: &str) -> Option<&Claim> {
        self.claims.get(key)
    }

    pub fn is_username_taken(&self, username_lower: &str) -> bool {
        self.is_claimed(&user_key(username_lower))
    }

    pub fn is_repo_name_taken(&self, full_name_lower: &str) -> bool {
        self.is_claimed(&repo_key(full_name_lower))
    }

    /// The next issue number for a repository. Starts at one.
    pub fn next_issue_number(&self, repo: RepoId) -> i64 {
        match self.get(&issue_counter_key(repo)) {
            Some(Claim::IssueCounter { next }) => *next,
            _ => 1,
        }
    }

    pub fn len(&self) -> usize {
        self.claims.len()
    }

    pub fn is_empty(&self) -> bool {
        self.claims.is_empty()
    }
}

#[cfg(test)]
pub(crate) mod tests_support {
    use forge_types::Oid;

    /// A deterministic object id, for tests that only need distinct values.
    pub fn oid(seed: u8) -> Oid {
        let mut bytes = [0u8; 20];
        bytes[0] = seed;
        bytes[19] = seed;
        Oid::from_bytes(bytes)
    }
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;

    fn encode(claim: &Claim) -> Vec<u8> {
        serde_json::to_vec(claim).unwrap()
    }

    #[test]
    fn a_claim_makes_a_name_taken() {
        let mut catalog = Catalog::new();
        let claim = Claim::User {
            user_id: UserId::new(),
        };
        catalog.apply(&user_key("octocat"), Some(&encode(&claim)));

        check!(catalog.is_username_taken("octocat"));
        check!(!catalog.is_username_taken("someone-else"));
    }

    #[test]
    fn a_tombstone_releases_the_name() {
        let mut catalog = Catalog::new();
        let key = user_key("temporary");
        catalog.apply(
            &key,
            Some(&encode(&Claim::User {
                user_id: UserId::new(),
            })),
        );
        catalog.apply(&key, None);

        check!(!catalog.is_username_taken("temporary"));
        check!(catalog.is_empty());
    }

    #[test]
    fn later_records_win_which_is_what_compaction_leaves_behind() {
        let mut catalog = Catalog::new();
        let key = user_key("rebound");
        let second = UserId::new();
        catalog.apply(
            &key,
            Some(&encode(&Claim::User {
                user_id: UserId::new(),
            })),
        );
        catalog.apply(&key, Some(&encode(&Claim::User { user_id: second })));

        check!(catalog.get(&key) == Some(&Claim::User { user_id: second }));
    }

    #[test]
    fn users_and_repos_occupy_separate_namespaces() {
        let mut catalog = Catalog::new();
        catalog.apply(
            &user_key("octocat"),
            Some(&encode(&Claim::User {
                user_id: UserId::new(),
            })),
        );

        // A repository may legitimately be named after a user.
        check!(!catalog.is_repo_name_taken("octocat"));
    }

    #[test]
    fn issue_numbers_start_at_one_and_are_per_repository() {
        let mut catalog = Catalog::new();
        let (a, b) = (RepoId::new(), RepoId::new());
        check!(catalog.next_issue_number(a) == 1);

        catalog.apply(
            &issue_counter_key(a),
            Some(&encode(&Claim::IssueCounter { next: 7 })),
        );
        check!(catalog.next_issue_number(a) == 7);
        check!(
            catalog.next_issue_number(b) == 1,
            "counters do not leak between repos"
        );
    }

    #[test]
    fn an_unreadable_record_is_skipped_rather_than_fatal() {
        // A claim shape from a newer writer must not stop this process booting.
        let mut catalog = Catalog::new();
        catalog.apply(&user_key("future"), Some(b"{\"kind\":\"quantum\"}"));

        check!(catalog.is_empty());
    }
}
