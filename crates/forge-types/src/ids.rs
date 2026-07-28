//! Newtyped aggregate identifiers.
//!
//! All ids are UUIDv7: time-ordered, so `ORDER BY id` is chronological and
//! keyset pagination needs no secondary sort key — which matters because gres
//! indexes are single-column equality only (see `docs/gres-gaps.md`).

use std::{fmt, str::FromStr};

use uuid::Uuid;

macro_rules! forge_id {
    ($name:ident, $prefix:literal, $doc:literal) => {
        #[doc = $doc]
        #[derive(
            Debug,
            Clone,
            Copy,
            PartialEq,
            Eq,
            PartialOrd,
            Ord,
            Hash,
            serde::Serialize,
            serde::Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            /// Mint a fresh time-ordered id.
            #[allow(clippy::new_without_default)] // minting is deliberate, never implicit
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }

            /// The short human-facing prefix used in logs and record keys.
            pub const PREFIX: &'static str = $prefix;

            pub fn as_uuid(&self) -> Uuid {
                self.0
            }

            /// Record-key spelling: `<prefix>:<uuid>`.
            pub fn record_key(&self) -> String {
                format!("{}:{}", $prefix, self.0)
            }
        }

        impl From<Uuid> for $name {
            fn from(u: Uuid) -> Self {
                Self(u)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        impl FromStr for $name {
            type Err = uuid::Error;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Ok(Self(Uuid::from_str(s)?))
            }
        }
    };
}

forge_id!(UserId, "user", "Identifies a user account.");
forge_id!(RepoId, "repo", "Identifies a repository.");
forge_id!(IssueId, "issue", "Identifies an issue.");
forge_id!(PrId, "pr", "Identifies a pull request.");
forge_id!(CommentId, "comment", "Identifies a comment.");
forge_id!(WebhookId, "webhook", "Identifies a webhook configuration.");
forge_id!(RunId, "run", "Identifies a CI run.");
forge_id!(JobId, "job", "Identifies a single job within a CI run.");

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;

    #[test]
    fn ids_sort_in_mint_order() {
        let a = RepoId::new();
        let b = RepoId::new();
        check!(a < b, "uuidv7 must be monotonic so keyset pagination works");
    }

    #[test]
    fn ids_round_trip_through_text() {
        let id = UserId::new();
        check!(UserId::from_str(&id.to_string()).unwrap() == id);
    }

    #[test]
    fn record_keys_carry_their_aggregate_prefix() {
        let id = IssueId::new();
        check!(id.record_key() == format!("issue:{id}"));
    }
}
