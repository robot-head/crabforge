//! Browsing a repository whose contents came out of the log.

use assert2::check;
use forge_git::{Cache, EntryKind, ObjectWriter, browse::Blob, import};
use forge_testkit::TestBroker;
use forge_types::RepoId;

/// The README the fixture repository contains.
const README: &[u8] = b"# Project\n\nSome prose.\n";

/// A repository imported into the log, hydrated into a cache, ready to browse.
async fn browsable() -> (TestBroker, tempfile::TempDir, Cache) {
    let broker = TestBroker::start().await;
    let repo = RepoId::new();
    let mut admin = broker.admin().await;
    forge_topics::ensure_repo(&mut admin, repo).await.unwrap();

    let source = tempfile::tempdir().unwrap();
    let head = import::make_test_repo(
        source.path(),
        &[
            ("README.md", README),
            ("src/main.rs", b"fn main() {\n    println!(\"hi\");\n}\n"),
            (
                "src/lib.rs",
                b"pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
            ),
            ("data/binary.bin", &[0u8, 159, 146, 150, 0]),
        ],
    )
    .unwrap();

    let writer = forge_git::connect_object_writer(&broker.bootstrap())
        .await
        .unwrap();
    let objects = import::read_all_objects(source.path()).unwrap();
    ObjectWriter::new(&writer, repo)
        .put_all(&objects)
        .await
        .unwrap();

    let cache_root = tempfile::tempdir().unwrap();
    let cache = Cache::new(cache_root.path(), repo);
    cache.hydrate(&broker.bootstrap(), "main").await.unwrap();
    cache.set_ref("refs/heads/main", head).unwrap();
    cache.set_head("refs/heads/main").unwrap();

    (broker, cache_root, cache)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_repository_root_lists_directories_before_files() {
    let (_broker, _root, cache) = browsable().await;
    let entries = cache.list_tree("main", "").unwrap();

    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    check!(names == vec!["data", "src", "README.md"]);
    check!(entries[0].kind == EntryKind::Directory);
    check!(entries[2].kind == EntryKind::File);
    // Files carry their size; directories have none to report.
    check!(entries[2].size == Some(README.len() as u64));
    check!(entries[0].size.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_subdirectory_lists_with_full_paths() {
    let (_broker, _root, cache) = browsable().await;
    let entries = cache.list_tree("main", "src").unwrap();

    let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
    check!(paths == vec!["src/lib.rs", "src/main.rs"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_text_file_reads_back_as_text() {
    let (_broker, _root, cache) = browsable().await;
    let blob = cache.read_blob("main", "src/main.rs").unwrap().unwrap();

    check!(!blob.is_binary());
    let Blob::Text { content, .. } = blob else {
        panic!("expected text")
    };
    check!(content.contains("println!"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_binary_file_is_recognized_rather_than_mangled() {
    // Rendering NUL-containing bytes as text would produce garbage; the browser
    // needs to know to offer a download instead.
    let (_broker, _root, cache) = browsable().await;
    let blob = cache.read_blob("main", "data/binary.bin").unwrap().unwrap();

    check!(blob.is_binary());
    check!(blob.size() == 5);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_path_is_absent_rather_than_an_error() {
    let (_broker, _root, cache) = browsable().await;
    check!(cache.read_blob("main", "nope.txt").unwrap().is_none());
    check!(cache.resolve("no-such-branch").unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn history_reads_back_with_its_metadata() {
    let (_broker, _root, cache) = browsable().await;
    let commits = cache.history("main", 10, 0).unwrap();

    check!(commits.len() == 1);
    check!(commits[0].summary == "initial commit");
    check!(commits[0].author_name == "Crabforge Test");
    check!(commits[0].parents.is_empty());
    check!(commits[0].authored_at > 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_readme_is_discoverable_for_the_repository_home_page() {
    let (_broker, _root, cache) = browsable().await;
    let found = cache
        .find_file("main", &["README.md", "README", "readme.md"])
        .unwrap();

    let (name, blob) = found.expect("README should be found");
    check!(name == "README.md");
    let Blob::Text { content, .. } = blob else {
        panic!("expected text")
    };
    check!(content.starts_with("# Project"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn branches_and_tags_are_listed_by_their_short_names() {
    let (_broker, _root, cache) = browsable().await;
    let head = cache.resolve("main").unwrap().unwrap();
    cache.set_ref("refs/tags/v1.0.0", head).unwrap();

    let branches = cache.branches().unwrap();
    check!(branches.iter().any(|(name, _)| name == "main"));

    let tags = cache.tags().unwrap();
    check!(tags.iter().any(|(name, _)| name == "v1.0.0"));
    // A tag must not appear as a branch.
    check!(!branches.iter().any(|(name, _)| name == "v1.0.0"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_commit_diff_shows_what_changed() {
    let (_broker, _root, cache) = browsable().await;
    let diff = cache.commit_diff("main").unwrap();

    check!(diff.contains("README.md"));
    check!(
        diff.contains("+# Project"),
        "the diff should show added lines"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_repository_with_no_commits_reports_itself_empty() {
    let broker = TestBroker::start().await;
    let repo = RepoId::new();
    let mut admin = broker.admin().await;
    forge_topics::ensure_repo(&mut admin, repo).await.unwrap();

    let root = tempfile::tempdir().unwrap();
    let cache = Cache::new(root.path(), repo);
    cache.hydrate(&broker.bootstrap(), "main").await.unwrap();

    check!(cache.is_empty_repo().unwrap());
    check!(cache.list_tree("main", "").unwrap_or_default().is_empty());
}
