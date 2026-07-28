//! Shared identifier and value types for Crabforge.
//!
//! Every crate in the workspace speaks these types, and this crate depends on
//! nothing from crabka — it is the bottom of the dependency graph.

mod clock;
mod ids;
mod names;
mod oid;
mod size;
pub mod topics;

pub use clock::{now, truncate_to_micros};
pub use ids::{CommentId, IssueId, JobId, PrId, RepoId, RunId, UserId, WebhookId};
pub use names::{InvalidName, RepoName, Username, full_name_lower, is_reserved_namespace};
pub use oid::{InvalidOid, Oid};
pub use size::{ByteSize, chunk_count, limits};

/// Repository visibility.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    derive_more::Display,
)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    #[display("public")]
    Public,
    #[display("private")]
    Private,
}

impl Visibility {
    /// Parse the wire/database spelling.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "public" => Some(Self::Public),
            "private" => Some(Self::Private),
            _ => None,
        }
    }
}

/// A collaborator's capability on a repository, ordered least to most powerful.
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
    derive_more::Display,
)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    #[display("read")]
    Read,
    #[display("write")]
    Write,
    #[display("admin")]
    Admin,
}

impl Role {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "read" => Some(Self::Read),
            "write" => Some(Self::Write),
            "admin" => Some(Self::Admin),
            _ => None,
        }
    }

    /// Whether this role permits everything `needed` permits.
    pub fn allows(self, needed: Role) -> bool {
        self >= needed
    }
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;

    #[test]
    fn role_ordering_is_least_to_most_powerful() {
        check!(Role::Admin.allows(Role::Write));
        check!(Role::Write.allows(Role::Read));
        check!(!Role::Read.allows(Role::Write));
    }

    #[test]
    fn visibility_round_trips_through_its_wire_spelling() {
        for v in [Visibility::Public, Visibility::Private] {
            check!(Visibility::parse(&v.to_string()) == Some(v));
        }
    }
}
