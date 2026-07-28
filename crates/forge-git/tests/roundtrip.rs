//! Git objects through the log and back.
//!
//! The claim under test: local disk holds nothing that cannot be rebuilt by
//! replaying a topic. These tests delete the cache and assert the repository
//! comes back — the same procedure as the disaster-recovery drill, at one
//! repository's scale.

use assert2::check;
use forge_bus::FencedWriter;
use forge_git::{Cache, Kind, Object, ObjectWriter, compute_oid, import};
use forge_testkit::TestBroker;
use forge_types::{ByteSize, RepoId, limits};

struct Fixture {
    broker: TestBroker,
    writer: FencedWriter,
    repo: RepoId,
    cache_root: tempfile::TempDir,
}

impl Fixture {
    async fn start() -> Self {
        let broker = TestBroker::start().await;
        let repo = RepoId::new();
        let mut admin = broker.admin().await;
        forge_topics::ensure_repo(&mut admin, repo)
            .await
            .expect("create the repository's object topic");
        let writer = FencedWriter::connect(&broker.bootstrap()).await.unwrap();
        Self {
            broker,
            writer,
            repo,
            cache_root: tempfile::tempdir().unwrap(),
        }
    }

    fn objects(&self) -> ObjectWriter<'_> {
        ObjectWriter::new(&self.writer, self.repo)
    }

    fn cache(&self) -> Cache {
        Cache::new(self.cache_root.path(), self.repo)
    }

    async fn hydrate(&self) -> forge_git::Hydrated {
        self.cache()
            .hydrate(&self.broker.bootstrap(), "main")
            .await
            .expect("hydrate")
    }
}

fn blob(content: &[u8]) -> Object {
    Object {
        oid: compute_oid(Kind::Blob, content),
        kind: Kind::Blob,
        content: content.to_vec(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_object_written_to_the_log_appears_in_the_cache() {
    let f = Fixture::start().await;
    let object = blob(b"hello from the log\n");
    let oid = object.oid;

    f.objects().put(&object).await.unwrap();
    let hydrated = f.hydrate().await;

    check!(hydrated.written == 1);
    check!(f.cache().contains(oid));

    // And git itself can read it back.
    let (kind, content) = forge_git::loose::read(&f.cache().objects_dir(), oid)
        .unwrap()
        .unwrap();
    check!(kind == Kind::Blob);
    check!(content == b"hello from the log\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_destroyed_cache_is_rebuilt_from_the_log() {
    // The architecture's central claim, at one repository's scale.
    let f = Fixture::start().await;
    let objects: Vec<Object> = (0..25)
        .map(|i| blob(format!("object number {i}\n").as_bytes()))
        .collect();
    f.objects().put_all(&objects).await.unwrap();

    let first = f.hydrate().await;
    check!(first.written == 25);

    f.cache().destroy().unwrap();
    check!(!f.cache().exists(), "cache is gone");

    let rebuilt = f.hydrate().await;
    check!(rebuilt.written == 25, "everything came back from the log");
    for object in &objects {
        check!(f.cache().contains(object.oid));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hydration_is_incremental() {
    // A warm cache should do almost no work: the cursor means the second pass
    // reads only what arrived since the first.
    let f = Fixture::start().await;
    f.objects().put(&blob(b"first\n")).await.unwrap();
    let first = f.hydrate().await;
    check!(first.written == 1);

    let second = f.hydrate().await;
    check!(
        second.written == 0 && second.skipped == 0,
        "nothing new to do"
    );
    check!(second.cursor == first.cursor);

    f.objects().put(&blob(b"second\n")).await.unwrap();
    let third = f.hydrate().await;
    check!(third.written == 1, "only the new object");
    check!(third.cursor > first.cursor);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn storing_the_same_object_twice_is_harmless() {
    // Re-pushing shared history is the common case; compaction collapses the
    // duplicate keys and hydration must not care.
    let f = Fixture::start().await;
    let object = blob(b"shared history\n");

    f.objects().put(&object).await.unwrap();
    f.objects().put(&object).await.unwrap();

    let hydrated = f.hydrate().await;
    check!(
        hydrated.written == 1,
        "one object, however many times written"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_blob_larger_than_the_chunk_size_round_trips_intact() {
    // The path that carries real risk: crabka's own tests top out around 8 MiB
    // and its wire frame is capped at 100 MiB, so large objects are chunked.
    let f = Fixture::start().await;
    let chunk = limits::object_chunk().as_bytes() as usize;
    let content: Vec<u8> = (0..chunk * 2 + 4096).map(|i| (i % 251) as u8).collect();
    let object = blob(&content);
    let oid = object.oid;

    check!(ByteSize::bytes(content.len() as u64) > limits::object_chunk());
    f.objects().put(&object).await.unwrap();

    let hydrated = f.hydrate().await;
    check!(hydrated.written == 1, "chunks reassemble into one object");

    let (_, read_back) = forge_git::loose::read(&f.cache().objects_dir(), oid)
        .unwrap()
        .unwrap();
    check!(read_back.len() == content.len());
    check!(read_back == content, "every byte survived the round trip");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_real_repository_imports_and_rebuilds() {
    let f = Fixture::start().await;
    let source = tempfile::tempdir().unwrap();
    let head = import::make_test_repo(
        source.path(),
        &[
            ("README.md", b"# hello\n"),
            ("src/main.rs", b"fn main() {}\n"),
        ],
    )
    .unwrap();

    let objects = import::read_all_objects(source.path()).unwrap();
    check!(objects.len() >= 4, "commit, tree, subtree and two blobs");
    f.objects().put_all(&objects).await.unwrap();

    let cache = f.cache();
    f.hydrate().await;
    cache.set_ref("refs/heads/main", head).unwrap();
    cache.set_head("refs/heads/main").unwrap();

    // Git can now walk the history entirely out of the rebuilt cache.
    let refs = cache.refs().unwrap();
    check!(
        refs.iter()
            .any(|(name, oid)| name == "refs/heads/main" && *oid == head)
    );
    check!(cache.contains(head));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_object_whose_content_does_not_match_its_id_is_refused() {
    // Content addressing is only worth anything if it is checked.
    let f = Fixture::start().await;
    let lying = Object {
        oid: compute_oid(Kind::Blob, b"the real content"),
        kind: Kind::Blob,
        content: b"something else entirely".to_vec(),
    };

    let result = f.objects().put(&lying).await;
    check!(
        result.is_err(),
        "a mismatched object must not reach the log"
    );
}
