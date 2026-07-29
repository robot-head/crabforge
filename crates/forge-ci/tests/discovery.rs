//! Discovery against a real repository.
//!
//! The unit tests cover the parsing rules; these cover the part that talks to
//! git, where the failure modes are "the path was wrong" and "we read the
//! branch tip instead of the commit" — neither of which a parser test can see.

use assert2::check;
use forge_ci::{WORKFLOW_DIR, discover};
use forge_git::Cache;
use forge_types::RepoId;

const BUILD: &str =
    "name: build\non: [push]\njobs:\n  test:\n    steps:\n      - run: cargo test\n";

/// A cache holding a repository whose first commit contains `files`.
fn repo_with(files: &[(&str, &[u8])]) -> (tempfile::TempDir, Cache, String) {
    let root = tempfile::tempdir().unwrap();
    let repo = RepoId::new();
    let cache = Cache::new(root.path(), repo);
    let oid = forge_git::import::make_test_repo(&cache.path(), files).unwrap();
    (root, cache, oid.to_hex())
}

#[test]
fn workflows_are_found_at_a_commit() {
    let (_root, cache, head) = repo_with(&[
        (&format!("{WORKFLOW_DIR}/build.yml"), BUILD.as_bytes()),
        ("README.md", b"hello"),
    ]);

    let found = discover(&cache, &head);
    check!(found.errors.is_empty(), "{:?}", found.errors);
    check!(found.workflows.len() == 1);
    check!(found.workflows[0].path == format!("{WORKFLOW_DIR}/build.yml"));
    check!(found.workflows[0].workflow.jobs.contains_key("test"));
}

#[test]
fn a_repository_without_workflows_is_not_an_error() {
    // The common case, and it must not look like a failure.
    let (_root, cache, head) = repo_with(&[("README.md", b"hello")]);

    let found = discover(&cache, &head);
    check!(found.workflows.is_empty());
    check!(found.errors.is_empty());
}

#[test]
fn one_broken_workflow_does_not_hide_the_others() {
    // A repository with three workflows and one typo should run two of them
    // and say what is wrong with the third, not stop.
    let (_root, cache, head) = repo_with(&[
        (&format!("{WORKFLOW_DIR}/a.yml"), BUILD.as_bytes()),
        (
            &format!("{WORKFLOW_DIR}/broken.yml"),
            b"on: push\njobs:\n  a:\n    step:\n      - run: x\n",
        ),
        (&format!("{WORKFLOW_DIR}/c.yaml"), BUILD.as_bytes()),
    ]);

    let found = discover(&cache, &head);
    check!(found.workflows.len() == 2, "the good ones should still run");
    check!(found.errors.len() == 1, "the broken one should be reported");

    let forge_ci::WorkflowError::Invalid { path, .. } = &found.errors[0];
    check!(path.ends_with("broken.yml"), "wrong file blamed: {path}");
}

#[test]
fn non_workflow_files_in_the_directory_are_left_alone() {
    // People put READMEs and editor backups next to their workflows.
    let (_root, cache, head) = repo_with(&[
        (&format!("{WORKFLOW_DIR}/build.yml"), BUILD.as_bytes()),
        (&format!("{WORKFLOW_DIR}/README.md"), b"how these work"),
        (&format!("{WORKFLOW_DIR}/build.yml.bak"), b"garbage"),
    ]);

    let found = discover(&cache, &head);
    check!(found.workflows.len() == 1);
    check!(
        found.errors.is_empty(),
        "a README is not a broken workflow: {:?}",
        found.errors
    );
}

#[test]
fn discovery_reads_the_commit_it_is_given_and_not_the_branch_tip() {
    // The property the whole design rests on. A push is planned against the
    // commit that was pushed; if discovery followed the branch instead, a
    // second push landing mid-plan would silently change what the first runs —
    // which is both a wrong label and a way to execute unreviewed code.
    let (root, cache, first) = repo_with(&[(
        &format!("{WORKFLOW_DIR}/build.yml"),
        "name: original\non: [push]\njobs:\n  a:\n    steps:\n      - run: echo one\n".as_bytes(),
    )]);
    let _ = root;

    // A second commit replaces the workflow entirely.
    let path = cache.path();
    let workflow_dir = path.join(WORKFLOW_DIR);
    std::fs::create_dir_all(&workflow_dir).unwrap();
    std::fs::write(
        workflow_dir.join("build.yml"),
        "name: replaced\non: [push]\njobs:\n  b:\n    steps:\n      - run: echo two\n",
    )
    .unwrap();
    for args in [
        vec!["add", "-A"],
        vec![
            "-c",
            "user.email=t@example.com",
            "-c",
            "user.name=T",
            "commit",
            "-m",
            "second",
        ],
    ] {
        let out = std::process::Command::new("git")
            .args(&args)
            .current_dir(&path)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    // The old commit still says what it always said.
    let at_first = discover(&cache, &first);
    check!(at_first.workflows.len() == 1);
    check!(at_first.workflows[0].workflow.jobs.contains_key("a"));
    check!(!at_first.workflows[0].workflow.jobs.contains_key("b"));

    // And HEAD says the new thing, so the two are genuinely different.
    let at_head = discover(&cache, "HEAD");
    check!(at_head.workflows[0].workflow.jobs.contains_key("b"));
}
