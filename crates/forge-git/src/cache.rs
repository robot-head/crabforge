//! The disposable per-repository cache.
//!
//! A real bare git repository on local disk, holding a copy of what the log
//! already contains. Nothing here is authoritative: delete the whole directory
//! and it is rebuilt by replaying the repository's object topic from offset
//! zero. That is the property the architecture claims, and
//! [`Cache::hydrate`] is where it is actually implemented.
//!
//! It exists for one reason: `git upload-pack` needs a repository to serve
//! from. Keeping the cache in git's own format means the protocol
//! implementation stays git's.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use forge_bus::{TailError, Tailer};
use forge_types::{Oid, RepoId, topics};

use crate::{
    frame::{self, Frame, Kind},
    loose::{self, LooseError},
};

#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("reading the log: {0}")]
    Tail(#[from] TailError),
    #[error("loose object: {0}")]
    Loose(#[from] LooseError),
    #[error("frame: {0}")]
    Frame(#[from] frame::FrameError),
    #[error("git {command} failed: {stderr}")]
    Git { command: String, stderr: String },
}

/// How far a cache has replayed its object topic.
///
/// Stored beside the repository so a restart resumes rather than re-reading
/// everything. Losing it costs a full rebuild, never correctness.
const CURSOR_FILE: &str = "forge-cursor";

pub struct Cache {
    root: PathBuf,
    repo: RepoId,
}

/// What a hydration pass did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Hydrated {
    /// Objects newly written to disk.
    pub written: usize,
    /// Objects already present and skipped.
    pub skipped: usize,
    /// Where the cursor ended up.
    pub cursor: i64,
}

impl Cache {
    /// A cache rooted at `<root>/<repo_id>.git`.
    pub fn new(root: impl Into<PathBuf>, repo: RepoId) -> Self {
        Self {
            root: root.into(),
            repo,
        }
    }

    pub fn path(&self) -> PathBuf {
        self.root.join(format!("{}.git", self.repo))
    }

    pub fn objects_dir(&self) -> PathBuf {
        self.path().join("objects")
    }

    pub fn exists(&self) -> bool {
        self.path().join("HEAD").exists()
    }

    /// Create the bare repository if it is not already there.
    ///
    /// `gc.auto=0` because the forge decides when to repack: a background `gc`
    /// could run while `upload-pack` is serving, and the cache is cheap to
    /// rebuild anyway.
    pub fn init(&self, default_branch: &str) -> Result<(), CacheError> {
        let path = self.path();
        if self.exists() {
            return Ok(());
        }
        std::fs::create_dir_all(&path)?;
        run_git(
            &path,
            &[
                "init",
                "--bare",
                "--quiet",
                &format!("--initial-branch={default_branch}"),
            ],
        )?;
        run_git(&path, &["config", "gc.auto", "0"])?;
        Ok(())
    }

    /// Throw the cache away.
    pub fn destroy(&self) -> Result<(), CacheError> {
        let path = self.path();
        if path.exists() {
            std::fs::remove_dir_all(path)?;
        }
        Ok(())
    }

    fn cursor_path(&self) -> PathBuf {
        self.path().join(CURSOR_FILE)
    }

    /// Where replay should resume. Zero when the cache is new or was reset.
    pub fn cursor(&self) -> i64 {
        std::fs::read_to_string(self.cursor_path())
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    }

    fn set_cursor(&self, offset: i64) -> Result<(), CacheError> {
        std::fs::write(self.cursor_path(), offset.to_string())?;
        Ok(())
    }

    /// Bring the cache up to date with the log.
    ///
    /// Replays from the stored cursor, so the usual case is a single empty
    /// fetch. A missing or corrupt cache replays from zero.
    pub async fn hydrate(
        &self,
        bootstrap: &str,
        default_branch: &str,
    ) -> Result<Hydrated, CacheError> {
        self.init(default_branch)?;

        let topic = topics::repo_objects(self.repo);
        let mut tailer = Tailer::open_at(bootstrap, &topic, self.cursor()).await?;
        let objects_dir = self.objects_dir();

        // Chunked objects arrive as a manifest plus parts. Buffer the pieces
        // until a manifest's full complement has been seen, then assemble.
        let mut manifests: HashMap<Oid, (Kind, u64, u32)> = HashMap::new();
        let mut partials: HashMap<Oid, Vec<Option<Vec<u8>>>> = HashMap::new();
        let mut result = Hydrated::default();
        let mut pending: Vec<(Oid, Kind, Vec<u8>)> = Vec::new();

        tailer
            .replay_to_end(|record| {
                let Some(key) = record.key.as_deref() else {
                    return;
                };
                let Some((oid, chunk_index)) = frame::parse_key(&String::from_utf8_lossy(key))
                else {
                    return;
                };
                let Some(value) = record.value.as_deref() else {
                    // A tombstone: the object was garbage-collected.
                    manifests.remove(&oid);
                    partials.remove(&oid);
                    return;
                };
                let Ok(frame) = frame::decode(value) else {
                    tracing::warn!(offset = record.offset, "skipping undecodable object record");
                    return;
                };

                match (frame, chunk_index) {
                    (Frame::Whole { kind, content }, None) => pending.push((oid, kind, content)),
                    (
                        Frame::Manifest {
                            kind,
                            total_len,
                            chunk_count,
                        },
                        None,
                    ) => {
                        manifests.insert(oid, (kind, total_len, chunk_count));
                        partials
                            .entry(oid)
                            .or_insert_with(|| vec![None; chunk_count as usize]);
                    }
                    (Frame::Chunk { data, .. }, Some(index)) => {
                        let slot = partials.entry(oid).or_default();
                        if slot.len() <= index as usize {
                            slot.resize(index as usize + 1, None);
                        }
                        slot[index as usize] = Some(data);
                    }
                    _ => tracing::warn!(%oid, "object record does not match its key shape"),
                }
            })
            .await?;

        // Assemble every chunked object whose parts all arrived.
        for (oid, (kind, total_len, chunk_count)) in manifests {
            let Some(parts) = partials.get(&oid) else {
                continue;
            };
            match frame::reassemble(total_len, chunk_count, parts) {
                Ok(content) => pending.push((oid, kind, content)),
                Err(e) => {
                    // Incomplete: the writer failed part-way. Leaving it out is
                    // right — a truncated object would corrupt a clone.
                    tracing::warn!(%oid, error = %e, "skipping incomplete chunked object");
                }
            }
        }

        for (oid, kind, content) in pending {
            // Verify on the way out as well as on the way in: this is the last
            // point before the bytes reach a clone.
            frame::verify(oid, kind, &content)?;
            if loose::write(&objects_dir, oid, kind, &content)? {
                result.written += 1;
            } else {
                result.skipped += 1;
            }
        }

        result.cursor = tailer.offset();
        self.set_cursor(result.cursor)?;
        Ok(result)
    }

    /// Point a reference at a commit.
    pub fn set_ref(&self, name: &str, oid: Oid) -> Result<(), CacheError> {
        run_git(&self.path(), &["update-ref", name, &oid.to_hex()])?;
        Ok(())
    }

    /// Set what `HEAD` points at, so a clone knows which branch to check out.
    pub fn set_head(&self, r#ref: &str) -> Result<(), CacheError> {
        run_git(&self.path(), &["symbolic-ref", "HEAD", r#ref])?;
        Ok(())
    }

    /// Every reference in the cache, as `(name, oid)`.
    pub fn refs(&self) -> Result<Vec<(String, Oid)>, CacheError> {
        let output = run_git(
            &self.path(),
            &["for-each-ref", "--format=%(refname) %(objectname)"],
        )?;
        Ok(output
            .lines()
            .filter_map(|line| {
                let (name, oid) = line.split_once(' ')?;
                Some((name.to_string(), oid.parse().ok()?))
            })
            .collect())
    }

    /// Whether the cache holds an object.
    pub fn contains(&self, oid: Oid) -> bool {
        loose::contains(&self.objects_dir(), oid)
    }
}

/// Run a git command in `dir`, returning stdout.
pub(crate) fn run_git(dir: &Path, args: &[&str]) -> Result<String, CacheError> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()?;
    if !output.status.success() {
        return Err(CacheError::Git {
            command: args.join(" "),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}
