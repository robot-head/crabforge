//! Merging one branch into another.
//!
//! The three-way merge is `git merge-tree --write-tree`, which computes a
//! merged tree without a working directory and reports conflicts without
//! touching the repository. That matters here: the cache is disposable and may
//! be serving a clone concurrently, so a merge must not leave anything behind
//! if it fails, and must not require a checkout.
//!
//! A successful merge produces objects — a tree, and a commit. Those go to the
//! log like any other objects, before the reference that names them moves.

use forge_types::Oid;

use crate::cache::{Cache, CacheError, run_git};

/// What a trial merge found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeAttempt {
    /// The branches merge cleanly. `tree` is the result.
    Clean { tree: Oid },
    /// They do not. `files` are the paths a human has to reconcile.
    Conflict { files: Vec<String> },
}

impl MergeAttempt {
    pub fn is_clean(&self) -> bool {
        matches!(self, Self::Clean { .. })
    }

    pub fn conflicted_files(&self) -> &[String] {
        match self {
            Self::Conflict { files } => files,
            Self::Clean { .. } => &[],
        }
    }
}

impl Cache {
    /// The commit where two branches diverged.
    pub fn merge_base(&self, a: &str, b: &str) -> Result<Option<Oid>, CacheError> {
        match run_git(&self.path(), &["merge-base", a, b]) {
            Ok(output) => Ok(output.trim().parse().ok()),
            // No common ancestor: unrelated histories, which is a legitimate
            // answer rather than a failure.
            Err(CacheError::Git { .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Try merging `head` into `base` without changing anything.
    ///
    /// Nothing is written to the repository on either outcome — the tree exists
    /// as an object, but no reference points at it, so it is unreferenced
    /// garbage until a merge is actually committed.
    pub fn try_merge(&self, base: &str, head: &str) -> Result<MergeAttempt, CacheError> {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(self.path())
            .args(["merge-tree", "--write-tree", base, head])
            .output()?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut lines = stdout.lines();
        let tree = lines
            .next()
            .and_then(|line| line.trim().parse::<Oid>().ok())
            .ok_or_else(|| CacheError::Git {
                command: "merge-tree".to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            })?;

        if output.status.success() {
            return Ok(MergeAttempt::Clean { tree });
        }

        // On conflict git lists the unmerged stages as
        // `<mode> <oid> <stage>\t<path>`, one per stage per file. The same path
        // appears up to three times, so collect distinct names.
        let mut files: Vec<String> = lines
            .filter_map(|line| line.split_once('\t'))
            .map(|(_, path)| path.to_string())
            .collect();
        files.sort();
        files.dedup();

        Ok(MergeAttempt::Conflict { files })
    }

    /// Create a merge commit for `tree` with two parents.
    ///
    /// Returns the new commit's id. The commit exists as an object; moving a
    /// reference to it is a separate decision, made by the command service.
    pub fn commit_merge(
        &self,
        tree: Oid,
        base: Oid,
        head: Oid,
        message: &str,
        author_name: &str,
        author_email: &str,
    ) -> Result<Oid, CacheError> {
        let repo = self.path();
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args([
                "commit-tree",
                &tree.to_hex(),
                "-p",
                &base.to_hex(),
                "-p",
                &head.to_hex(),
                "-m",
                message,
            ])
            // The forge records who asked for the merge. Committer and author
            // are the same: nobody else touched this commit.
            .env("GIT_AUTHOR_NAME", author_name)
            .env("GIT_AUTHOR_EMAIL", author_email)
            .env("GIT_COMMITTER_NAME", author_name)
            .env("GIT_COMMITTER_EMAIL", author_email)
            .output()?;

        if !output.status.success() {
            return Err(CacheError::Git {
                command: "commit-tree".to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .map_err(|_| CacheError::Git {
                command: "commit-tree".to_string(),
                stderr: "commit-tree did not return an object id".to_string(),
            })
    }

    /// Commits on `head` that are not on `base`, newest first.
    pub fn commits_between(
        &self,
        base: &str,
        head: &str,
        limit: usize,
    ) -> Result<Vec<crate::browse::Commit>, CacheError> {
        self.history(head, limit, 0).map(|all| {
            // `history` walks one revision; filtering against the base's
            // reachable set keeps this to plumbing already in use.
            let base_commits: std::collections::HashSet<Oid> = self
                .history(base, 1000, 0)
                .unwrap_or_default()
                .into_iter()
                .map(|c| c.oid)
                .collect();
            all.into_iter()
                .filter(|c| !base_commits.contains(&c.oid))
                .collect()
        })
    }

    /// The diff between two revisions.
    pub fn diff_between(&self, base: &str, head: &str) -> Result<String, CacheError> {
        run_git(
            &self.path(),
            &[
                "diff",
                "--no-color",
                "--find-renames",
                &format!("{base}...{head}"),
            ],
        )
    }

    /// Every object reachable from `commit` but not from `excluding`.
    ///
    /// The exact set a merge added. Enumerating it by hand is a trap: a clean
    /// merge of two edits to one file produces a *new blob* holding the
    /// combined result, which exists in neither parent, and new subtrees for
    /// every directory on the path to it. `rev-list --objects` answers this
    /// correctly for any shape of change.
    pub fn objects_added_by(
        &self,
        commit: Oid,
        excluding: &[Oid],
    ) -> Result<Vec<(Oid, crate::frame::Kind)>, CacheError> {
        let mut args = vec![
            "rev-list".to_string(),
            "--objects".to_string(),
            commit.to_hex(),
            "--not".to_string(),
        ];
        args.extend(excluding.iter().map(|o| o.to_hex()));
        let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();

        let listed = run_git(&self.path(), &borrowed)?;
        let mut out = Vec::new();
        for line in listed.lines() {
            let oid: Oid = match line.split_whitespace().next().and_then(|o| o.parse().ok()) {
                Some(oid) => oid,
                None => continue,
            };
            // The listing does not say what kind each object is, and the frame
            // needs it, so ask.
            let kind = run_git(&self.path(), &["cat-file", "-t", &oid.to_hex()])?;
            if let Some(kind) = crate::frame::Kind::parse(kind.trim()) {
                out.push((oid, kind));
            }
        }
        Ok(out)
    }

    /// Whether `ancestor` is reachable from `descendant`.
    ///
    /// What distinguishes a fast-forward from a force-push, and what tells a
    /// pull request whether it has anything left to merge.
    pub fn is_ancestor(&self, ancestor: Oid, descendant: Oid) -> Result<bool, CacheError> {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(self.path())
            .args([
                "merge-base",
                "--is-ancestor",
                &ancestor.to_hex(),
                &descendant.to_hex(),
            ])
            .output()?;
        Ok(output.status.success())
    }
}
