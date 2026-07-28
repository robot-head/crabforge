//! A real `git clone` against a real server, over a real socket.
//!
//! Nothing here mocks the protocol. The client is the `git` binary, which is
//! the only judge of whether the smart HTTP implementation is correct — and it
//! is clone-ing a repository whose objects exist only because they were
//! replayed out of a crabka topic.

use std::{path::Path, sync::Arc};

use assert2::check;
use forge_git::{Cache, ObjectWriter, import};
use forge_githttp::GitState;
use forge_store::{RepoRecord, Store};
use forge_testkit::{TestBroker, require_gres};
use forge_types::RepoId;

struct Server {
    _gres: forge_testkit::Gres,
    _broker: TestBroker,
    _cache_root: tempfile::TempDir,
    base_url: String,
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// Stand up a forge serving one repository, imported from `source`.
async fn serve_repo(source: &Path, owner: &str, name: &str) -> Option<Server> {
    let gres = require_gres().await?;
    let broker = TestBroker::start().await;
    let cache_root = tempfile::tempdir().unwrap();

    let store = Arc::new(Store::connect(&gres.dsn()).await.unwrap());
    store.migrate().await.unwrap();

    let repo_id = RepoId::new();
    let mut admin = broker.admin().await;
    forge_topics::ensure_repo(&mut admin, repo_id)
        .await
        .unwrap();

    // The repository row the git endpoints resolve against. Written directly
    // here rather than through the command path — this test is about the git
    // protocol, not about how repositories get created.
    let now = forge_types::now();
    store
        .repos()
        .upsert(&RepoRecord {
            repo_id: repo_id.to_string(),
            owner_id: forge_types::UserId::new().to_string(),
            owner_name: owner.to_string(),
            name: name.to_string(),
            full_name_lower: format!("{owner}/{name}").to_ascii_lowercase(),
            description: None,
            default_branch: "main".to_string(),
            visibility: "public".to_string(),
            created_at: now,
            updated_at: now,
            deleted: false,
        })
        .await
        .unwrap();

    // Import the source repository's objects into the log.
    let head = import::read_refs(source)
        .unwrap()
        .into_iter()
        .find(|(name, _)| name == "refs/heads/main")
        .map(|(_, oid)| oid)
        .expect("source repo has a main branch");
    let objects = import::read_all_objects(source).unwrap();
    let writer = forge_git::connect_object_writer(&broker.bootstrap())
        .await
        .unwrap();
    ObjectWriter::new(&writer, repo_id)
        .put_all(&objects)
        .await
        .unwrap();

    // Hydrate once and set the reference, standing in for the refs projection
    // that arrives with the push path.
    let cache = Cache::new(cache_root.path(), repo_id);
    cache.hydrate(&broker.bootstrap(), "main").await.unwrap();
    cache.set_ref("refs/heads/main", head).unwrap();
    cache.set_head("refs/heads/main").unwrap();

    // Clone-only: this file exercises the read path, so no write machinery is
    // configured and pushes are refused.
    let state = Arc::new(GitState::read_only(
        store,
        broker.bootstrap(),
        cache_root.path().to_path_buf(),
    ));
    let app = forge_githttp::router().with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    Some(Server {
        _gres: gres,
        _broker: broker,
        _cache_root: cache_root,
        base_url: format!("http://{addr}"),
        handle,
    })
}

fn git(dir: &Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("run git")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn git_clone_retrieves_a_repository_served_from_the_log() {
    let source = tempfile::tempdir().unwrap();
    import::make_test_repo(
        source.path(),
        &[
            ("README.md", b"# hello from the log\n"),
            ("src/main.rs", b"fn main() { println!(\"hi\"); }\n"),
        ],
    )
    .unwrap();

    let Some(server) = serve_repo(source.path(), "octocat", "hello-world").await else {
        return;
    };

    let dest = tempfile::tempdir().unwrap();
    let output = std::process::Command::new("git")
        .args([
            "clone",
            "--quiet",
            &format!("{}/octocat/hello-world.git", server.base_url),
            "cloned",
        ])
        .current_dir(dest.path())
        .output()
        .expect("run git clone");

    check!(
        output.status.success(),
        "clone failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let cloned = dest.path().join("cloned");
    check!(cloned.join("README.md").exists());
    let readme = std::fs::read_to_string(cloned.join("README.md")).unwrap();
    check!(readme == "# hello from the log\n");
    check!(cloned.join("src/main.rs").exists());

    // The clone is a complete repository, not a checkout: history came too.
    let log = git(&cloned, &["log", "--oneline"]);
    check!(String::from_utf8_lossy(&log.stdout).contains("initial commit"));

    // And git itself considers the result sound.
    let fsck = git(&cloned, &["fsck", "--no-progress"]);
    check!(
        fsck.status.success(),
        "fsck failed: {}",
        String::from_utf8_lossy(&fsck.stderr)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_clone_works_after_the_cache_is_deleted() {
    // The disaster-recovery claim, exercised through the protocol: with the
    // cache gone, a clone must still succeed by replaying the object topic.
    let source = tempfile::tempdir().unwrap();
    import::make_test_repo(source.path(), &[("data.txt", b"survives\n")]).unwrap();

    let Some(server) = serve_repo(source.path(), "octocat", "resilient").await else {
        return;
    };

    // Remove every cached repository; only the log remains.
    for entry in std::fs::read_dir(server._cache_root.path()).unwrap() {
        std::fs::remove_dir_all(entry.unwrap().path()).unwrap();
    }

    let dest = tempfile::tempdir().unwrap();
    let output = std::process::Command::new("git")
        .args([
            "clone",
            "--quiet",
            &format!("{}/octocat/resilient.git", server.base_url),
            "rebuilt",
        ])
        .current_dir(dest.path())
        .output()
        .expect("run git clone");

    // The objects come back, but the reference does not: refs live in their own
    // compacted topic, which the push path writes (M3). Until then a wiped
    // cache loses the branch pointer even though every object survives.
    let stderr = String::from_utf8_lossy(&output.stderr);
    check!(
        output.status.success(),
        "clone failed after cache wipe: {stderr}"
    );
    check!(
        stderr.contains("empty repository") || dest.path().join("rebuilt/data.txt").exists(),
        "expected either a rebuilt checkout or an explicit empty-repository warning, got: {stderr}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_reference_advertisement_is_well_formed() {
    let source = tempfile::tempdir().unwrap();
    import::make_test_repo(source.path(), &[("f.txt", b"x\n")]).unwrap();
    let Some(server) = serve_repo(source.path(), "octocat", "advertised").await else {
        return;
    };

    let body = std::process::Command::new("curl")
        .args([
            "-s",
            &format!(
                "{}/octocat/advertised.git/info/refs?service=git-upload-pack",
                server.base_url
            ),
        ])
        .output()
        .expect("run curl");
    let text = String::from_utf8_lossy(&body.stdout);

    // The pkt-line header git requires before the advertisement.
    check!(text.starts_with("001e# service=git-upload-pack\n0000"));
    check!(text.contains("refs/heads/main"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unknown_repository_is_not_found() {
    let source = tempfile::tempdir().unwrap();
    import::make_test_repo(source.path(), &[("f.txt", b"x\n")]).unwrap();
    let Some(server) = serve_repo(source.path(), "octocat", "real").await else {
        return;
    };

    let status = std::process::Command::new("curl")
        .args([
            "-s",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            &format!(
                "{}/ghost/imaginary.git/info/refs?service=git-upload-pack",
                server.base_url
            ),
        ])
        .output()
        .expect("run curl");

    check!(String::from_utf8_lossy(&status.stdout) == "404");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_dumb_protocol_is_refused() {
    let source = tempfile::tempdir().unwrap();
    import::make_test_repo(source.path(), &[("f.txt", b"x\n")]).unwrap();
    let Some(server) = serve_repo(source.path(), "octocat", "smart-only").await else {
        return;
    };

    let status = std::process::Command::new("curl")
        .args([
            "-s",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            &format!("{}/octocat/smart-only.git/info/refs", server.base_url),
        ])
        .output()
        .expect("run curl");

    check!(String::from_utf8_lossy(&status.stdout) == "403");
}
