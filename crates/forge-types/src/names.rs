//! Validated user and repository names.
//!
//! Both carry a pre-lowered form: gres indexes are single-column equality only,
//! so every name lookup goes through an indexed lowercase column rather than a
//! `lower(col) = $1` expression index.

use std::fmt;

/// Top-level paths the forge itself serves, which therefore cannot be usernames.
const RESERVED: &[&str] = &[
    "api", "assets", "explore", "healthz", "internal", "login", "logout", "new", "register",
    "settings", "static",
];

/// Whether `name` collides with a route the forge owns.
pub fn is_reserved_namespace(name: &str) -> bool {
    RESERVED.contains(&name.to_ascii_lowercase().as_str())
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum InvalidName {
    #[error("name must not be empty")]
    Empty,
    #[error("name must be at most {max} characters, got {got}")]
    TooLong { max: usize, got: usize },
    #[error("name may only contain letters, digits, '-', '_' and '.'")]
    IllegalCharacter,
    #[error("name must start with a letter or digit")]
    BadStart,
    #[error("name must not end with '.'")]
    BadEnd,
    #[error("'{0}' is reserved by the forge")]
    Reserved(String),
    #[error("'.git' and '.' style names are not addressable")]
    NotAddressable,
}

fn validate(raw: &str, max: usize) -> Result<(), InvalidName> {
    if raw.is_empty() {
        return Err(InvalidName::Empty);
    }
    if raw.len() > max {
        return Err(InvalidName::TooLong {
            max,
            got: raw.len(),
        });
    }
    if !raw
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err(InvalidName::IllegalCharacter);
    }
    let first = raw.chars().next().expect("non-empty checked above");
    if !first.is_ascii_alphanumeric() {
        return Err(InvalidName::BadStart);
    }
    if raw.ends_with('.') {
        return Err(InvalidName::BadEnd);
    }
    Ok(())
}

/// A validated account name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Username {
    raw: String,
    lower: String,
}

impl Username {
    pub const MAX_LEN: usize = 39;

    pub fn parse(raw: impl Into<String>) -> Result<Self, InvalidName> {
        let raw = raw.into();
        validate(&raw, Self::MAX_LEN)?;
        let lower = raw.to_ascii_lowercase();
        if is_reserved_namespace(&lower) {
            return Err(InvalidName::Reserved(raw));
        }
        Ok(Self { raw, lower })
    }

    /// The name as the user typed it — for display.
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// The lookup key. Indexed in gres as `users.username_lower`.
    pub fn lower(&self) -> &str {
        &self.lower
    }
}

/// A validated repository name (the part after `owner/`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RepoName {
    raw: String,
    lower: String,
}

impl RepoName {
    pub const MAX_LEN: usize = 100;

    pub fn parse(raw: impl Into<String>) -> Result<Self, InvalidName> {
        let mut raw = raw.into();
        // Clients habitually clone `owner/repo.git`; accept it and store the
        // bare name so `repo` and `repo.git` never become two repositories.
        if let Some(stripped) = raw.strip_suffix(".git") {
            raw = stripped.to_string();
        }
        validate(&raw, Self::MAX_LEN)?;
        if raw == "." || raw == ".." {
            return Err(InvalidName::NotAddressable);
        }
        let lower = raw.to_ascii_lowercase();
        Ok(Self { raw, lower })
    }

    pub fn as_str(&self) -> &str {
        &self.raw
    }

    pub fn lower(&self) -> &str {
        &self.lower
    }
}

/// The `owner/name` lookup key, pre-lowered. Indexed as `repos.full_name_lower`.
pub fn full_name_lower(owner: &Username, repo: &RepoName) -> String {
    format!("{}/{}", owner.lower(), repo.lower())
}

macro_rules! string_conversions {
    ($t:ty) => {
        impl TryFrom<String> for $t {
            type Error = InvalidName;

            fn try_from(s: String) -> Result<Self, Self::Error> {
                Self::parse(s)
            }
        }

        impl From<$t> for String {
            fn from(v: $t) -> String {
                v.raw
            }
        }

        impl fmt::Display for $t {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.raw)
            }
        }
    };
}

string_conversions!(Username);
string_conversions!(RepoName);

#[cfg(test)]
mod tests {
    use assert2::{assert, check};

    use super::*;

    #[test]
    fn ordinary_names_are_accepted_and_display_as_typed() {
        let u = Username::parse("Octocat").unwrap();
        check!(u.as_str() == "Octocat");
        check!(u.lower() == "octocat");
    }

    #[test]
    fn reserved_routes_cannot_be_usernames() {
        assert!(let Err(InvalidName::Reserved(_)) = Username::parse("settings"));
        assert!(let Err(InvalidName::Reserved(_)) = Username::parse("API"));
    }

    #[test]
    fn repo_names_may_shadow_reserved_routes() {
        // Reservation only applies at the top level; `octocat/settings` is fine.
        check!(RepoName::parse("settings").is_ok());
    }

    #[test]
    fn dot_git_suffix_is_stripped_so_clone_urls_normalize() {
        let r = RepoName::parse("hello-world.git").unwrap();
        check!(r.as_str() == "hello-world");
    }

    #[test]
    fn path_traversal_names_are_rejected() {
        // The separator trips the character check before the leading dot is
        // ever considered — either way the name never reaches the filesystem.
        assert!(let Err(InvalidName::IllegalCharacter) = RepoName::parse("../etc"));
        assert!(let Err(InvalidName::IllegalCharacter) = RepoName::parse("a/b"));
        assert!(let Err(InvalidName::BadStart) = RepoName::parse(".hidden"));
    }

    #[test]
    fn names_must_start_alphanumeric_and_not_end_with_a_dot() {
        assert!(let Err(InvalidName::BadStart) = Username::parse("-leading"));
        assert!(let Err(InvalidName::BadEnd) = Username::parse("trailing."));
    }

    #[test]
    fn length_is_bounded() {
        let long = "a".repeat(Username::MAX_LEN + 1);
        assert!(let Err(InvalidName::TooLong { .. }) = Username::parse(long));
    }

    #[test]
    fn full_name_key_is_lowercased() {
        let owner = Username::parse("OctoCat").unwrap();
        let repo = RepoName::parse("Hello-World").unwrap();
        check!(full_name_lower(&owner, &repo) == "octocat/hello-world");
    }
}
