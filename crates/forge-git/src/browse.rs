//! Reading a repository's contents.
//!
//! Everything here answers questions a browser asks — what is in this
//! directory, what does this file contain, what changed in this commit — by
//! reading the cache, which is itself a replay of the object topic.
//!
//! Git's plumbing commands do the object-graph work. `cat-file`, `ls-tree` and
//! `rev-list` are stable, well-specified interfaces, and their output is far
//! cheaper to parse correctly than a commit graph is to reimplement.

use std::path::Path;

use forge_types::Oid;

use crate::{
    cache::{Cache, CacheError, run_git},
    frame::Kind,
};

/// One entry in a directory listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    pub name: String,
    /// Path from the repository root.
    pub path: String,
    pub oid: Oid,
    pub kind: EntryKind,
    /// `None` for anything that is not a file.
    pub size: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory,
    /// A file with the executable bit set.
    Executable,
    Symlink,
    /// A nested repository — listed, but not traversable.
    Submodule,
}

impl EntryKind {
    fn from_mode(mode: &str) -> Self {
        match mode {
            "040000" | "40000" => Self::Directory,
            "100755" => Self::Executable,
            "120000" => Self::Symlink,
            "160000" => Self::Submodule,
            _ => Self::File,
        }
    }

    pub fn is_directory(self) -> bool {
        matches!(self, Self::Directory)
    }
}

/// A commit, as shown in a history listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub oid: Oid,
    pub author_name: String,
    pub author_email: String,
    /// Seconds since the Unix epoch.
    pub authored_at: i64,
    /// The first line of the message.
    pub summary: String,
    /// Everything after the first blank line, if any.
    pub body: Option<String>,
    pub parents: Vec<Oid>,
}

impl Commit {
    /// The abbreviated id shown in a UI.
    pub fn short(&self) -> String {
        self.oid.to_hex()[..7].to_string()
    }

    pub fn is_merge(&self) -> bool {
        self.parents.len() > 1
    }
}

/// A file's contents, as far as it makes sense to show them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Blob {
    Text {
        content: String,
        size: u64,
    },
    /// Not decodable as UTF-8, so there is nothing useful to render.
    Binary {
        size: u64,
    },
}

impl Blob {
    pub fn size(&self) -> u64 {
        match self {
            Self::Text { size, .. } | Self::Binary { size } => *size,
        }
    }

    pub fn is_binary(&self) -> bool {
        matches!(self, Self::Binary { .. })
    }
}

impl Cache {
    /// List a directory at `revision`.
    ///
    /// `path` is empty for the repository root. Directories sort before files,
    /// which is the order every git host displays and the order people expect.
    pub fn list_tree(&self, revision: &str, path: &str) -> Result<Vec<TreeEntry>, CacheError> {
        let spec = if path.is_empty() {
            format!("{revision}^{{tree}}")
        } else {
            format!("{revision}:{}", path.trim_end_matches('/'))
        };

        let output = run_git(&self.path(), &["ls-tree", "--long", "-z", &spec])?;
        let mut entries: Vec<TreeEntry> = output
            .split('\0')
            .filter(|line| !line.is_empty())
            .filter_map(|line| parse_tree_line(line, path))
            .collect();

        entries.sort_by(|a, b| {
            b.kind
                .is_directory()
                .cmp(&a.kind.is_directory())
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        Ok(entries)
    }

    /// Read a file at `revision`.
    pub fn read_blob(&self, revision: &str, path: &str) -> Result<Option<Blob>, CacheError> {
        let spec = format!("{revision}:{path}");
        let Some((kind, bytes)) = self.cat_file(&spec)? else {
            return Ok(None);
        };
        if kind != Kind::Blob {
            return Ok(None);
        }

        let size = bytes.len() as u64;
        Ok(Some(match String::from_utf8(bytes) {
            Ok(content) if !content.contains('\0') => Blob::Text { content, size },
            // Either invalid UTF-8 or containing NUL: git's own heuristic for
            // "binary", and the point at which showing the contents is useless.
            _ => Blob::Binary { size },
        }))
    }

    /// The commit a revision resolves to.
    pub fn resolve(&self, revision: &str) -> Result<Option<Oid>, CacheError> {
        match run_git(
            &self.path(),
            &["rev-parse", "--verify", &format!("{revision}^{{commit}}")],
        ) {
            Ok(output) => Ok(output.trim().parse().ok()),
            // An unknown revision is a 404, not a failure.
            Err(CacheError::Git { .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// One commit's metadata.
    pub fn commit(&self, revision: &str) -> Result<Option<Commit>, CacheError> {
        let commits = self.history(revision, 1, 0)?;
        Ok(commits.into_iter().next())
    }

    /// Commit history reachable from `revision`, newest first.
    pub fn history(
        &self,
        revision: &str,
        limit: usize,
        skip: usize,
    ) -> Result<Vec<Commit>, CacheError> {
        // A record separator that cannot appear in a commit message, so parsing
        // is unambiguous regardless of what people write in commits.
        const RECORD: &str = "%x1e";
        const FIELD: &str = "%x1f";
        let format =
            format!("--pretty=format:{RECORD}%H{FIELD}%an{FIELD}%ae{FIELD}%at{FIELD}%P{FIELD}%B");

        let output = match run_git(
            &self.path(),
            &[
                "rev-list",
                &format!("--max-count={limit}"),
                &format!("--skip={skip}"),
                &format,
                revision,
            ],
        ) {
            Ok(output) => output,
            Err(CacheError::Git { .. }) => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };

        Ok(output
            .split('\u{1e}')
            .filter(|record| !record.trim().is_empty())
            .filter_map(parse_commit_record)
            .collect())
    }

    /// The unified diff a commit introduced, against its first parent.
    pub fn commit_diff(&self, revision: &str) -> Result<String, CacheError> {
        run_git(
            &self.path(),
            &[
                "show",
                "--format=",
                "--no-color",
                // Rename detection on: showing a rename as a delete plus an add
                // buries the actual change.
                "--find-renames",
                revision,
            ],
        )
    }

    /// Every branch, as `(short name, tip)`.
    pub fn branches(&self) -> Result<Vec<(String, Oid)>, CacheError> {
        Ok(self
            .refs()?
            .into_iter()
            .filter_map(|(name, oid)| {
                name.strip_prefix("refs/heads/")
                    .map(|short| (short.to_string(), oid))
            })
            .collect())
    }

    /// Every tag, as `(short name, target)`.
    pub fn tags(&self) -> Result<Vec<(String, Oid)>, CacheError> {
        Ok(self
            .refs()?
            .into_iter()
            .filter_map(|(name, oid)| {
                name.strip_prefix("refs/tags/")
                    .map(|short| (short.to_string(), oid))
            })
            .collect())
    }

    /// Whether the repository has any commits yet.
    pub fn is_empty_repo(&self) -> Result<bool, CacheError> {
        Ok(self.refs()?.is_empty())
    }

    /// The first file matching `candidates`, for finding a README.
    pub fn find_file(
        &self,
        revision: &str,
        candidates: &[&str],
    ) -> Result<Option<(String, Blob)>, CacheError> {
        let entries = self.list_tree(revision, "")?;
        for candidate in candidates {
            if let Some(entry) = entries
                .iter()
                .find(|e| e.name.eq_ignore_ascii_case(candidate) && !e.kind.is_directory())
                && let Some(blob) = self.read_blob(revision, &entry.path)?
            {
                return Ok(Some((entry.name.clone(), blob)));
            }
        }
        Ok(None)
    }

    /// Raw object bytes for a revision-and-path spec.
    fn cat_file(&self, spec: &str) -> Result<Option<(Kind, Vec<u8>)>, CacheError> {
        let repo = self.path();
        let kind = match run_git(&repo, &["cat-file", "-t", spec]) {
            Ok(kind) => kind.trim().to_string(),
            Err(CacheError::Git { .. }) => return Ok(None),
            Err(e) => return Err(e),
        };
        let Some(kind) = Kind::parse(&kind) else {
            return Ok(None);
        };

        // Bytes, not a String: a blob is arbitrary content and may not be text.
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["cat-file", "blob", spec])
            .output()?;
        if !output.status.success() {
            return Ok(None);
        }
        Ok(Some((kind, output.stdout)))
    }
}

fn parse_tree_line(line: &str, parent: &str) -> Option<TreeEntry> {
    // `<mode> SP <type> SP <oid> SP* <size> TAB <name>`
    let (meta, name) = line.split_once('\t')?;
    let mut parts = meta.split_whitespace();
    let mode = parts.next()?;
    let _type = parts.next()?;
    let oid: Oid = parts.next()?.parse().ok()?;
    // `-` for anything without a size, i.e. trees and submodules.
    let size = parts.next().and_then(|s| s.parse::<u64>().ok());

    let path = if parent.is_empty() {
        name.to_string()
    } else {
        format!("{}/{name}", parent.trim_end_matches('/'))
    };

    Some(TreeEntry {
        name: name.to_string(),
        path,
        oid,
        kind: EntryKind::from_mode(mode),
        size,
    })
}

fn parse_commit_record(record: &str) -> Option<Commit> {
    let mut fields = record.split('\u{1f}');
    let oid: Oid = fields.next()?.trim().parse().ok()?;
    let author_name = fields.next()?.to_string();
    let author_email = fields.next()?.to_string();
    let authored_at = fields.next()?.trim().parse().ok()?;
    let parents = fields
        .next()?
        .split_whitespace()
        .filter_map(|p| p.parse().ok())
        .collect();
    let message = fields.next().unwrap_or_default();

    let mut lines = message.trim_end().splitn(2, '\n');
    let summary = lines.next().unwrap_or_default().trim().to_string();
    let body = lines
        .next()
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .map(str::to_string);

    Some(Commit {
        oid,
        author_name,
        author_email,
        authored_at,
        summary,
        body,
        parents,
    })
}

/// Whether a path is safe to look up in a repository.
///
/// Repository paths come from URLs. They are passed to git as part of a
/// revision spec, so anything that could escape the repository or be read as an
/// option is refused rather than cleaned up.
pub fn is_safe_path(path: &str) -> bool {
    !path.starts_with('/')
        && !path.starts_with('-')
        && !path.contains("..")
        && !path.contains('\0')
        && !path.contains(':')
        && path.len() <= 4096
}

/// Whether a revision string is safe to pass to git.
pub fn is_safe_revision(revision: &str) -> bool {
    !revision.is_empty()
        && !revision.starts_with('-')
        && revision.len() <= 255
        && !revision.contains("..")
        && !revision.contains('\0')
        && revision
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
}

/// Where a cache lives, for callers that need the path directly.
pub fn repo_path(root: &Path, repo: forge_types::RepoId) -> std::path::PathBuf {
    Cache::new(root, repo).path()
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;

    #[test]
    fn tree_lines_parse_into_entries() {
        let line = "100644 blob e69de29bb2d1d6434b8b29ae775ad8c2e48c5391      42\tREADME.md";
        let entry = parse_tree_line(line, "").unwrap();
        check!(entry.name == "README.md");
        check!(entry.path == "README.md");
        check!(entry.kind == EntryKind::File);
        check!(entry.size == Some(42));
    }

    #[test]
    fn nested_entries_carry_their_full_path() {
        let line = "100644 blob e69de29bb2d1d6434b8b29ae775ad8c2e48c5391      10\tmain.rs";
        let entry = parse_tree_line(line, "src").unwrap();
        check!(entry.path == "src/main.rs");
    }

    #[test]
    fn modes_map_to_entry_kinds() {
        check!(EntryKind::from_mode("040000") == EntryKind::Directory);
        check!(EntryKind::from_mode("100644") == EntryKind::File);
        check!(EntryKind::from_mode("100755") == EntryKind::Executable);
        check!(EntryKind::from_mode("120000") == EntryKind::Symlink);
        check!(EntryKind::from_mode("160000") == EntryKind::Submodule);
    }

    #[test]
    fn a_commit_record_parses_including_a_multiline_message() {
        let record = "e83c5163316f89bfbde7d9ab23ca2e25604af290\u{1f}Octocat\u{1f}o@example.com\u{1f}1700000000\u{1f}\u{1f}Add a thing\n\nWith an explanation.\n";
        let commit = parse_commit_record(record).unwrap();

        check!(commit.summary == "Add a thing");
        check!(commit.body.as_deref() == Some("With an explanation."));
        check!(commit.author_name == "Octocat");
        check!(commit.parents.is_empty());
        check!(!commit.is_merge());
        check!(commit.short() == "e83c516");
    }

    #[test]
    fn a_merge_commit_records_every_parent() {
        let record = "e83c5163316f89bfbde7d9ab23ca2e25604af290\u{1f}A\u{1f}a@e.com\u{1f}1\u{1f}ce013625030ba8dba906f756967f9e9ca394464a e69de29bb2d1d6434b8b29ae775ad8c2e48c5391\u{1f}Merge";
        let commit = parse_commit_record(record).unwrap();
        check!(commit.parents.len() == 2);
        check!(commit.is_merge());
    }

    #[test]
    fn paths_that_could_escape_the_repository_are_refused() {
        check!(is_safe_path("src/main.rs"));
        check!(is_safe_path(""));

        check!(!is_safe_path("../etc/passwd"));
        check!(!is_safe_path("/etc/passwd"));
        // Would be read as an option by git.
        check!(!is_safe_path("--upload-pack=evil"));
        // Would change the meaning of a revision spec.
        check!(!is_safe_path("HEAD:../../etc"));
    }

    #[test]
    fn revisions_are_restricted_to_plausible_names() {
        check!(is_safe_revision("main"));
        check!(is_safe_revision("refs/heads/feature/thing"));
        check!(is_safe_revision("e83c5163316f89bfbde7d9ab23ca2e25604af290"));

        check!(!is_safe_revision(""));
        check!(!is_safe_revision("--all"));
        // Range syntax would ask git a different question than the caller meant.
        check!(!is_safe_revision("main..other"));
        check!(!is_safe_revision("main^{tree}"));
        check!(!is_safe_revision("main; rm -rf /"));
    }
}
