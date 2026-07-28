//! What a credential is allowed to do.
//!
//! Scopes are coarse on purpose. A fine-grained permission model that nobody
//! understands produces tokens with every box ticked, which is worse than a
//! small set of scopes people actually reason about.

use std::{fmt, str::FromStr};

use forge_types::Role;

/// One capability a token may carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scope {
    /// Read repository contents and metadata. Enough to clone.
    RepoRead,
    /// Push, and open or comment on issues.
    RepoWrite,
    /// Change repository settings, collaborators and webhooks.
    RepoAdmin,
    /// Read and update the account itself.
    User,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RepoRead => "repo:read",
            Self::RepoWrite => "repo:write",
            Self::RepoAdmin => "repo:admin",
            Self::User => "user",
        }
    }

    /// Every scope, for rendering the token-creation form.
    pub fn all() -> [Scope; 4] {
        [Self::RepoRead, Self::RepoWrite, Self::RepoAdmin, Self::User]
    }

    /// Whether holding `self` implies holding `needed`.
    ///
    /// Write implies read, and admin implies both: a token that can rewrite a
    /// repository's settings but not read it would be a strange thing to issue,
    /// and forcing callers to tick three boxes invites ticking all of them.
    pub fn implies(self, needed: Scope) -> bool {
        match self {
            Self::RepoAdmin => matches!(needed, Self::RepoAdmin | Self::RepoWrite | Self::RepoRead),
            Self::RepoWrite => matches!(needed, Self::RepoWrite | Self::RepoRead),
            Self::RepoRead => needed == Self::RepoRead,
            Self::User => needed == Self::User,
        }
    }

    /// The repository role this scope corresponds to, if any.
    pub fn as_role(self) -> Option<Role> {
        match self {
            Self::RepoRead => Some(Role::Read),
            Self::RepoWrite => Some(Role::Write),
            Self::RepoAdmin => Some(Role::Admin),
            Self::User => None,
        }
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("'{0}' is not a scope")]
pub struct UnknownScope(String);

impl FromStr for Scope {
    type Err = UnknownScope;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "repo:read" => Ok(Self::RepoRead),
            "repo:write" => Ok(Self::RepoWrite),
            "repo:admin" => Ok(Self::RepoAdmin),
            "user" => Ok(Self::User),
            other => Err(UnknownScope(other.to_string())),
        }
    }
}

/// The set of scopes a credential carries.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scopes(Vec<Scope>);

impl Scopes {
    pub fn new(scopes: impl IntoIterator<Item = Scope>) -> Self {
        let mut scopes: Vec<Scope> = scopes.into_iter().collect();
        scopes.sort_unstable();
        scopes.dedup();
        Self(scopes)
    }

    /// Parse the stored space-separated form.
    ///
    /// Unrecognized entries are dropped rather than failing: a token issued by
    /// a newer version with a scope this one does not know should still work
    /// for the scopes it does, not stop authenticating entirely.
    pub fn parse(stored: &str) -> Self {
        Self::new(stored.split_whitespace().filter_map(|s| s.parse().ok()))
    }

    /// The stored form.
    pub fn encode(&self) -> String {
        self.0
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Whether this set grants `needed`.
    pub fn allows(&self, needed: Scope) -> bool {
        self.0.iter().any(|held| held.implies(needed))
    }

    pub fn iter(&self) -> impl Iterator<Item = Scope> + '_ {
        self.0.iter().copied()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;

    #[test]
    fn scopes_round_trip_through_their_stored_form() {
        let scopes = Scopes::new([Scope::RepoWrite, Scope::User]);
        check!(Scopes::parse(&scopes.encode()) == scopes);
    }

    #[test]
    fn write_implies_read_and_admin_implies_both() {
        let write = Scopes::new([Scope::RepoWrite]);
        check!(write.allows(Scope::RepoRead));
        check!(write.allows(Scope::RepoWrite));
        check!(!write.allows(Scope::RepoAdmin));

        let admin = Scopes::new([Scope::RepoAdmin]);
        check!(admin.allows(Scope::RepoRead));
        check!(admin.allows(Scope::RepoWrite));
        check!(admin.allows(Scope::RepoAdmin));
    }

    #[test]
    fn repository_scopes_do_not_grant_account_access() {
        // Someone handing a CI system a push token must not be handing it their
        // account.
        let admin = Scopes::new([Scope::RepoAdmin]);
        check!(!admin.allows(Scope::User));

        let user = Scopes::new([Scope::User]);
        check!(!user.allows(Scope::RepoRead));
    }

    #[test]
    fn an_empty_set_grants_nothing() {
        let none = Scopes::default();
        for scope in Scope::all() {
            check!(!none.allows(scope));
        }
    }

    #[test]
    fn duplicates_collapse() {
        let scopes = Scopes::new([Scope::RepoRead, Scope::RepoRead]);
        check!(scopes.encode() == "repo:read");
    }

    #[test]
    fn an_unknown_scope_is_dropped_rather_than_failing_the_token() {
        // A token issued by a newer version must keep working for what this
        // version understands.
        let scopes = Scopes::parse("repo:read repo:teleport user");
        check!(scopes.allows(Scope::RepoRead));
        check!(scopes.allows(Scope::User));
        check!(scopes.encode() == "repo:read user");
    }

    #[test]
    fn parsing_tolerates_odd_spacing() {
        check!(
            Scopes::parse("  repo:read   user  ") == Scopes::new([Scope::RepoRead, Scope::User])
        );
        check!(Scopes::parse("").is_empty());
    }

    #[test]
    fn scopes_map_onto_repository_roles() {
        check!(Scope::RepoWrite.as_role() == Some(Role::Write));
        check!(Scope::User.as_role().is_none());
    }

    #[test]
    fn the_stored_form_is_stable_regardless_of_input_order() {
        // So a token's scope string does not churn between writes.
        let a = Scopes::new([Scope::User, Scope::RepoRead]);
        let b = Scopes::new([Scope::RepoRead, Scope::User]);
        check!(a.encode() == b.encode());
    }
}
