//! Load characteristics of the choices this design makes.
//!
//! Two of them were made early and are expensive to revisit, so they are worth
//! probing rather than assuming:
//!
//! * **A topic per repository.** Git objects live on
//!   `forge.git.objects.<repo_id>`, which means a forge with ten thousand
//!   repositories asks the broker for ten thousand topics. That is a lot of
//!   metadata, and if it does not hold the alternative is a shared topic keyed
//!   by repository — a different partitioning story and a rewrite of hydration.
//! * **Chunking at 4 MiB.** Crabka's own largest tested record is 8 MiB and its
//!   wire frame is capped at 100 MiB, so a repository containing a large asset
//!   goes through a path upstream has not exercised.

use assert2::check;
use forge_git::{Cache, ObjectWriter, connect_object_writer, import};
use forge_testkit::TestBroker;
use forge_types::{ByteSize, RepoId, limits};

/// How many repositories to provision. Enough to catch a per-topic cost that
/// grows badly, small enough to stay a test rather than a benchmark.
const REPOSITORIES: usize = 40;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn many_repositories_each_get_their_own_object_topic() {
    let broker = TestBroker::with_forge_topics().await;
    let mut admin = broker.admin().await;

    let started = std::time::Instant::now();
    let mut repos = Vec::new();
    for _ in 0..REPOSITORIES {
        let repo = RepoId::new();
        forge_topics::ensure_repo(&mut admin, repo)
            .await
            .expect("provisioning a repository topic");
        repos.push(repo);
    }
    let elapsed = started.elapsed();
    tracing::info!(?elapsed, REPOSITORIES, "provisioned repository topics");

    // Not a performance assertion — machines differ — but a guard against the
    // shape being wrong. Per-topic cost that grew super-linearly would blow
    // through this long before it looked like a slow machine.
    check!(
        elapsed < std::time::Duration::from_secs(60),
        "provisioning {REPOSITORIES} topics took {elapsed:?}"
    );

    // And each one is genuinely independent: writing to one must not disturb
    // another, which is the property the per-repo split is bought for.
    let writer = connect_object_writer(&broker.bootstrap()).await.unwrap();
    let source = tempfile::tempdir().unwrap();
    import::make_test_repo(source.path(), &[("f.txt", b"hello")]).unwrap();
    let objects = import::read_all_objects(source.path()).unwrap();

    for repo in repos.iter().take(3) {
        ObjectWriter::new(&writer, *repo)
            .put_all(&objects)
            .await
            .expect("writing objects");
    }

    let cache_root = tempfile::tempdir().unwrap();
    for repo in repos.iter().take(3) {
        let cache = Cache::new(cache_root.path(), *repo);
        let hydrated = cache.hydrate(&broker.bootstrap(), "main").await.unwrap();
        check!(hydrated.written > 0, "repository {repo} hydrated nothing");
    }
    // A repository nobody wrote to is empty rather than holding someone else's
    // objects — the failure a shared topic would make possible.
    let untouched = Cache::new(cache_root.path(), repos[REPOSITORIES - 1]);
    let hydrated = untouched
        .hydrate(&broker.bootstrap(), "main")
        .await
        .unwrap();
    check!(hydrated.written == 0, "objects leaked between repositories");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_object_far_larger_than_crabkas_tested_envelope_round_trips() {
    // 24 MiB — six chunks, and three times the largest record crabka's own
    // suite exercises. A forge holds design assets and vendored binaries, so
    // this is an ordinary file rather than an adversarial one.
    let broker = TestBroker::with_forge_topics().await;
    let repo = RepoId::new();
    forge_topics::ensure_repo(&mut broker.admin().await, repo)
        .await
        .unwrap();

    let size = 24 * 1024 * 1024;
    check!(
        ByteSize::bytes(size as u64) > limits::object_chunk(),
        "the test must exceed one chunk to mean anything"
    );
    check!(
        ByteSize::bytes(size as u64) < limits::max_frame(),
        "and stay inside the wire frame"
    );
    // Not compressible to nothing: an all-zero blob would test the compressor
    // rather than the chunking.
    let content: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();

    let source = tempfile::tempdir().unwrap();
    import::make_test_repo(source.path(), &[("big.bin", &content)]).unwrap();
    let objects = import::read_all_objects(source.path()).unwrap();

    let writer = connect_object_writer(&broker.bootstrap()).await.unwrap();
    ObjectWriter::new(&writer, repo)
        .put_all(&objects)
        .await
        .expect("a large object should be writable");

    let cache_root = tempfile::tempdir().unwrap();
    let cache = Cache::new(cache_root.path(), repo);
    cache.hydrate(&broker.bootstrap(), "main").await.unwrap();

    // Reassembled byte for byte. A chunking bug that dropped or reordered a
    // part would still produce a plausible-looking file.
    let head = import::read_refs(source.path()).unwrap();
    let (_, head_oid) = head.first().expect("a ref");
    let blob = cache
        .read_blob(&head_oid.to_hex(), "big.bin")
        .unwrap()
        .expect("the blob should be back");
    check!(blob.size() == size as u64, "the reassembled size is wrong");
}
