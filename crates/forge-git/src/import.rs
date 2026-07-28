//! Importing an existing repository into the log.
//!
//! Enumerates every object in a repository and writes it to the repository's
//! topic. Used by `crabforge import-repo`, and by tests that need a repository
//! with real history in it.
//!
//! Object enumeration goes through `git cat-file --batch-all-objects`, which
//! reads packed and loose objects alike and streams them in one process rather
//! than one per object.

use std::{
    io::{BufRead as _, BufReader, Read as _, Write as _},
    path::Path,
    process::{Command, Stdio},
};

use forge_types::Oid;

use crate::{
    cache::{CacheError, run_git},
    frame::Kind,
    store::Object,
};

/// Read every object in the repository at `path`.
///
/// Held in memory, which is fine for the repositories this is used on today
/// (imports and tests). A streaming version becomes necessary when someone
/// imports something large; the seam is this function's signature.
pub fn read_all_objects(path: &Path) -> Result<Vec<Object>, CacheError> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(path)
        .args([
            "cat-file",
            "--batch-all-objects",
            "--batch",
            "--buffer",
            "--unordered",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let stdout = child.stdout.take().expect("piped");
    let mut reader = BufReader::new(stdout);
    let mut objects = Vec::new();
    let mut header = String::new();

    loop {
        header.clear();
        if reader.read_line(&mut header)? == 0 {
            break;
        }
        let header = header.trim_end();
        if header.is_empty() {
            continue;
        }

        // `<oid> <kind> <size>`
        let mut parts = header.split(' ');
        let (Some(oid), Some(kind), Some(size)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let Ok(oid) = oid.parse::<Oid>() else {
            continue;
        };
        let Some(kind) = Kind::parse(kind) else {
            continue;
        };
        let size: usize = size.parse().unwrap_or(0);

        let mut content = vec![0u8; size];
        reader.read_exact(&mut content)?;
        // git writes a newline after each object's content.
        let mut newline = [0u8; 1];
        let _ = reader.read_exact(&mut newline);

        objects.push(Object { oid, kind, content });
    }

    let status = child.wait()?;
    if !status.success() {
        let mut stderr = String::new();
        if let Some(mut e) = child.stderr.take() {
            let _ = e.read_to_string(&mut stderr);
        }
        return Err(CacheError::Git {
            command: "cat-file --batch-all-objects".to_string(),
            stderr,
        });
    }
    Ok(objects)
}

/// Every reference in the repository at `path`.
pub fn read_refs(path: &Path) -> Result<Vec<(String, Oid)>, CacheError> {
    let output = run_git(path, &["for-each-ref", "--format=%(refname) %(objectname)"])?;
    Ok(output
        .lines()
        .filter_map(|line| {
            let (name, oid) = line.split_once(' ')?;
            Some((name.to_string(), oid.parse().ok()?))
        })
        .collect())
}

/// Build a small repository with real history. Test support.
#[doc(hidden)]
pub fn make_test_repo(path: &Path, files: &[(&str, &[u8])]) -> Result<Oid, CacheError> {
    std::fs::create_dir_all(path)?;
    run_git(path, &["init", "--quiet", "--initial-branch=main"])?;
    run_git(path, &["config", "user.email", "test@crabforge.invalid"])?;
    run_git(path, &["config", "user.name", "Crabforge Test"])?;

    for (name, content) in files {
        let file = path.join(name);
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut handle = std::fs::File::create(&file)?;
        handle.write_all(content)?;
    }

    run_git(path, &["add", "-A"])?;
    run_git(path, &["commit", "--quiet", "-m", "initial commit"])?;
    let head = run_git(path, &["rev-parse", "HEAD"])?;
    head.trim().parse().map_err(|_| CacheError::Git {
        command: "rev-parse HEAD".to_string(),
        stderr: "could not parse commit id".to_string(),
    })
}
