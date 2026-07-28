//! Merging, against real repositories.
//!
//! Merge correctness is not a place to trust reasoning over evidence: the
//! result becomes permanent history, and a merge that silently drops a change
//! is close to unrecoverable once people have pulled it.

use std::path::Path;

use assert2::check;
use forge_git::{Cache, MergeAttempt, import};
use forge_testkit::TestBroker;
use forge_types::{Oid, RepoId};

fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A bare repository with two divergent branches, built through the log so the
/// cache is populated the way production populates it.
async fn diverged(
    base_content: &str,
    main_content: &str,
    feature_content: &str,
) -> (TestBroker, tempfile::TempDir, Cache) {
    let broker = TestBroker::start().await;
    let repo = RepoId::new();
    let mut admin = broker.admin().await;
    forge_topics::ensure_repo(&mut admin, repo).await.unwrap();

    let source = tempfile::tempdir().unwrap();
    import::make_test_repo(source.path(), &[("f.txt", base_content.as_bytes())]).unwrap();

    // Two branches from the same base.
    git(source.path(), &["checkout", "-qb", "feature"]);
    std::fs::write(source.path().join("f.txt"), feature_content).unwrap();
    git(source.path(), &["commit", "-qam", "feature change"]);

    git(source.path(), &["checkout", "-q", "main"]);
    std::fs::write(source.path().join("f.txt"), main_content).unwrap();
    git(source.path(), &["commit", "-qam", "main change"]);

    let main = git(source.path(), &["rev-parse", "main"]).parse().unwrap();
    let feature: Oid = git(source.path(), &["rev-parse", "feature"])
        .parse()
        .unwrap();

    let writer = forge_git::connect_object_writer(&broker.bootstrap())
        .await
        .unwrap();
    forge_git::ObjectWriter::new(&writer, repo)
        .put_all(&import::read_all_objects(source.path()).unwrap())
        .await
        .unwrap();

    let cache_root = tempfile::tempdir().unwrap();
    let cache = Cache::new(cache_root.path(), repo);
    cache.hydrate(&broker.bootstrap(), "main").await.unwrap();
    cache.set_ref("refs/heads/main", main).unwrap();
    cache.set_ref("refs/heads/feature", feature).unwrap();
    cache.set_head("refs/heads/main").unwrap();

    (broker, cache_root, cache)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn branches_touching_different_lines_merge_cleanly() {
    let (_b, _root, cache) = diverged(
        "one\ntwo\nthree\n",
        "one\ntwo\nthree\nfour\n",
        "ONE\ntwo\nthree\n",
    )
    .await;

    let attempt = cache.try_merge("main", "feature").unwrap();
    check!(attempt.is_clean(), "got {attempt:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn branches_touching_the_same_line_conflict_and_name_the_file() {
    let (_b, _root, cache) = diverged(
        "one\ntwo\nthree\n",
        "one\nMAIN\nthree\n",
        "one\nFEATURE\nthree\n",
    )
    .await;

    let attempt = cache.try_merge("main", "feature").unwrap();
    check!(!attempt.is_clean());
    check!(
        attempt.conflicted_files() == ["f.txt"],
        "the person resolving needs to know which file: {attempt:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_trial_merge_leaves_the_repository_untouched() {
    // Mergeability is computed speculatively and often. It must not move a
    // reference or change what a concurrent clone would see.
    let (_b, _root, cache) = diverged(
        "one\ntwo\nthree\n",
        "one\ntwo\nthree\nfour\n",
        "ONE\ntwo\nthree\n",
    )
    .await;

    let before = cache.refs().unwrap();
    cache.try_merge("main", "feature").unwrap();
    cache.try_merge("main", "feature").unwrap();
    check!(
        cache.refs().unwrap() == before,
        "a trial merge moved a reference"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_merge_commit_records_both_parents_and_the_merged_content() {
    let (_b, _root, cache) = diverged(
        "one\ntwo\nthree\n",
        "one\ntwo\nthree\nfour\n",
        "ONE\ntwo\nthree\n",
    )
    .await;

    let MergeAttempt::Clean { tree } = cache.try_merge("main", "feature").unwrap() else {
        panic!("expected a clean merge")
    };
    let base = cache.resolve("main").unwrap().unwrap();
    let head = cache.resolve("feature").unwrap().unwrap();

    let merge = cache
        .commit_merge(
            tree,
            base,
            head,
            "Merge pull request #1",
            "Octocat",
            "octocat@example.com",
        )
        .unwrap();

    let commit = cache.commit(&merge.to_hex()).unwrap().unwrap();
    check!(commit.is_merge(), "a merge commit has two parents");
    check!(commit.parents == vec![base, head], "base first, then head");
    check!(commit.summary == "Merge pull request #1");
    check!(commit.author_name == "Octocat");

    // Both sides' changes are present in the result.
    cache.set_ref("refs/heads/merged", merge).unwrap();
    let merged = cache.read_blob("merged", "f.txt").unwrap().unwrap();
    let forge_git::browse::Blob::Text { content, .. } = merged else {
        panic!("expected text")
    };
    check!(content.starts_with("ONE"), "the feature change survived");
    check!(content.contains("four"), "and so did the main change");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_merge_base_is_where_the_branches_diverged() {
    let (_b, _root, cache) = diverged(
        "one\ntwo\nthree\n",
        "one\ntwo\nthree\nfour\n",
        "ONE\ntwo\nthree\n",
    )
    .await;

    let base = cache.merge_base("main", "feature").unwrap();
    check!(base.is_some());

    // It is an ancestor of both tips, which is what makes it the base.
    let base = base.unwrap();
    check!(
        cache
            .is_ancestor(base, cache.resolve("main").unwrap().unwrap())
            .unwrap()
    );
    check!(
        cache
            .is_ancestor(base, cache.resolve("feature").unwrap().unwrap())
            .unwrap()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_branch_already_merged_has_nothing_left_to_do() {
    let (_b, _root, cache) = diverged(
        "one\ntwo\nthree\n",
        "one\ntwo\nthree\nfour\n",
        "ONE\ntwo\nthree\n",
    )
    .await;

    let main = cache.resolve("main").unwrap().unwrap();
    let base = cache.merge_base("main", "feature").unwrap().unwrap();

    // The base is an ancestor of main, so merging it in would change nothing.
    check!(cache.is_ancestor(base, main).unwrap());
    check!(
        !cache.is_ancestor(main, base).unwrap(),
        "and not the other way"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_diff_between_branches_shows_only_the_head_side() {
    // A pull request shows what the author changed, not what happened on the
    // base in the meantime — which is why this is a three-dot diff.
    let (_b, _root, cache) = diverged(
        "one\ntwo\nthree\n",
        "one\ntwo\nthree\nfour\n",
        "ONE\ntwo\nthree\n",
    )
    .await;

    let diff = cache.diff_between("main", "feature").unwrap();
    check!(diff.contains("+ONE"), "the feature change should appear");
    check!(
        !diff.contains("-four"),
        "the base's own change must not look like a removal: {diff}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn commits_between_lists_only_what_the_branch_adds() {
    let (_b, _root, cache) = diverged(
        "one\ntwo\nthree\n",
        "one\ntwo\nthree\nfour\n",
        "ONE\ntwo\nthree\n",
    )
    .await;

    let commits = cache.commits_between("main", "feature", 50).unwrap();
    let summaries: Vec<&str> = commits.iter().map(|c| c.summary.as_str()).collect();
    check!(summaries == vec!["feature change"], "got {summaries:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unrelated_histories_have_no_merge_base() {
    let (_b, _root, cache) = diverged(
        "one\ntwo\nthree\n",
        "one\ntwo\nthree\nfour\n",
        "ONE\ntwo\nthree\n",
    )
    .await;

    // An object id that is not in this repository at all.
    let stranger: Oid = "0123456789abcdef0123456789abcdef01234567".parse().unwrap();
    check!(
        cache
            .merge_base("main", &stranger.to_hex())
            .unwrap()
            .is_none()
    );
}
