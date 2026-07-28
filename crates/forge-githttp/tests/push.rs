//! A real `git push`, and the races a forge has to win.

use std::{path::Path, sync::Arc};

use assert2::check;
use forge_command::CommandService;
use forge_git::import;
use forge_githttp::GitState;
use forge_store::{RepoRecord, Store};
use forge_testkit::{TestBroker, require_gres};
use forge_types::RepoId;

struct Server {
    _gres: forge_testkit::Gres,
    _broker: TestBroker,
    _cache_root: tempfile::TempDir,
    base_url: String,
    commands: Arc<CommandService>,
    repo_id: RepoId,
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

impl Server {
    fn clone_url(&self, owner: &str, repo: &str) -> String {
        format!("{}/{owner}/{repo}.git", self.base_url)
    }
}

/// A forge serving one empty repository, ready to receive a push.
async fn serve_empty_repo(owner: &str, name: &str) -> Option<Server> {
    let gres = require_gres().await?;
    let broker = TestBroker::with_forge_topics().await;
    let cache_root = tempfile::tempdir().unwrap();

    let store = Arc::new(Store::connect(&gres.dsn()).await.unwrap());
    store.migrate().await.unwrap();

    let repo_id = RepoId::new();
    let mut admin = broker.admin().await;
    forge_topics::ensure_repo(&mut admin, repo_id)
        .await
        .unwrap();

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

    let commands = CommandService::start(&broker.bootstrap()).await.unwrap();
    let writer = Arc::new(
        forge_git::connect_object_writer(&broker.bootstrap())
            .await
            .unwrap(),
    );

    // Bind first so the hook callback URL can name the real port.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let state = Arc::new(GitState {
        store,
        bootstrap: broker.bootstrap(),
        cache_root: cache_root.path().to_path_buf(),
        commands: Some(Arc::clone(&commands)),
        writer: Some(writer),
        hook_callback_url: format!("http://{addr}/internal/hooks/pre-receive"),
        hook_token: "test-token-not-a-secret".to_string(),
    });
    let app = forge_githttp::router().with_state(state);
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    Some(Server {
        _gres: gres,
        _broker: broker,
        _cache_root: cache_root,
        base_url: format!("http://{addr}"),
        commands,
        repo_id,
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

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_push_lands_in_the_log_and_can_be_cloned_back() {
    let Some(server) = serve_empty_repo("octocat", "pushed").await else {
        return;
    };
    let work = tempfile::tempdir().unwrap();
    import::make_test_repo(work.path(), &[("hello.txt", b"pushed content\n")]).unwrap();

    let push = git(
        work.path(),
        &[
            "push",
            &server.clone_url("octocat", "pushed"),
            "refs/heads/main:refs/heads/main",
        ],
    );
    check!(push.status.success(), "push failed: {}", stderr(&push));

    // The reference is canonical now — held by the command service, which read
    // it from the log rather than from any repository on disk.
    let refs = server.commands.refs_for(server.repo_id).await;
    check!(refs.len() == 1);
    check!(refs[0].0 == "refs/heads/main");

    // And the pushed content comes back to a fresh clone.
    let dest = tempfile::tempdir().unwrap();
    let clone = std::process::Command::new("git")
        .args([
            "clone",
            "--quiet",
            &server.clone_url("octocat", "pushed"),
            "back",
        ])
        .current_dir(dest.path())
        .output()
        .unwrap();
    check!(clone.status.success(), "clone failed: {}", stderr(&clone));

    let content = std::fs::read_to_string(dest.path().join("back/hello.txt")).unwrap();
    check!(content == "pushed content\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_push_from_a_stale_starting_point_is_rejected() {
    // The race a forge must not lose: two clones from the same commit, both
    // pushing. Whoever is second has to be told to fetch first, or the loser's
    // work is silently overwritten.
    let Some(server) = serve_empty_repo("octocat", "contested").await else {
        return;
    };
    let url = server.clone_url("octocat", "contested");

    let first = tempfile::tempdir().unwrap();
    import::make_test_repo(first.path(), &[("base.txt", b"base\n")]).unwrap();
    let push = git(
        first.path(),
        &["push", &url, "refs/heads/main:refs/heads/main"],
    );
    check!(
        push.status.success(),
        "first push failed: {}",
        stderr(&push)
    );

    // Two people clone the same state.
    let alice = tempfile::tempdir().unwrap();
    let bob = tempfile::tempdir().unwrap();
    for (who, dir) in [("alice", &alice), ("bob", &bob)] {
        let out = std::process::Command::new("git")
            .args(["clone", "--quiet", &url, "repo"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        check!(
            out.status.success(),
            "{who} could not clone: {}",
            stderr(&out)
        );
    }

    let commit = |dir: &Path, name: &str, body: &str| {
        std::fs::write(dir.join(name), body).unwrap();
        git(dir, &["config", "user.email", "t@example.invalid"]);
        git(dir, &["config", "user.name", "T"]);
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "--quiet", "-m", name]);
    };

    let alice_repo = alice.path().join("repo");
    let bob_repo = bob.path().join("repo");
    commit(&alice_repo, "alice.txt", "alice was here\n");
    commit(&bob_repo, "bob.txt", "bob was here\n");

    let alice_push = git(&alice_repo, &["push", "origin", "main"]);
    check!(
        alice_push.status.success(),
        "alice's push should win: {}",
        stderr(&alice_push)
    );

    let bob_push = git(&bob_repo, &["push", "origin", "main"]);
    check!(
        !bob_push.status.success(),
        "bob pushed over alice's work — the compare-and-swap did not hold"
    );

    // Alice's commit is what the forge holds.
    let dest = tempfile::tempdir().unwrap();
    std::process::Command::new("git")
        .args(["clone", "--quiet", &url, "final"])
        .current_dir(dest.path())
        .output()
        .unwrap();
    check!(dest.path().join("final/alice.txt").exists());
    check!(!dest.path().join("final/bob.txt").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_force_push_is_still_a_compare_and_swap() {
    // `--force` tells git not to require a fast-forward. It does not mean
    // "ignore what the reference points at now", and the forge must still
    // reject a push whose starting point is stale.
    let Some(server) = serve_empty_repo("octocat", "forced").await else {
        return;
    };
    let url = server.clone_url("octocat", "forced");

    let work = tempfile::tempdir().unwrap();
    import::make_test_repo(work.path(), &[("a.txt", b"one\n")]).unwrap();
    git(
        work.path(),
        &["push", &url, "refs/heads/main:refs/heads/main"],
    );

    // Rewrite history locally, then force-push: the client's view of the old
    // value is still current, so this is legitimate and must succeed.
    std::fs::write(work.path().join("a.txt"), "two\n").unwrap();
    git(work.path(), &["add", "-A"]);
    git(
        work.path(),
        &["commit", "--quiet", "--amend", "-m", "amended"],
    );

    let forced = git(work.path(), &["push", "--force", &url, "main"]);
    check!(
        forced.status.success(),
        "a force push from a current starting point should succeed: {}",
        stderr(&forced)
    );

    let refs = server.commands.refs_for(server.repo_id).await;
    let head = git(work.path(), &["rev-parse", "HEAD"]);
    let local = String::from_utf8_lossy(&head.stdout).trim().to_string();
    check!(
        refs[0].1.to_hex() == local,
        "the forge holds the rewritten commit"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pushing_a_branch_and_a_tag_together_is_atomic() {
    let Some(server) = serve_empty_repo("octocat", "atomic").await else {
        return;
    };
    let url = server.clone_url("octocat", "atomic");

    let work = tempfile::tempdir().unwrap();
    import::make_test_repo(work.path(), &[("f.txt", b"tagged\n")]).unwrap();
    git(work.path(), &["tag", "v1.0.0"]);

    let push = git(
        work.path(),
        &[
            "push",
            &url,
            "refs/heads/main:refs/heads/main",
            "refs/tags/v1.0.0:refs/tags/v1.0.0",
        ],
    );
    check!(push.status.success(), "push failed: {}", stderr(&push));

    let refs = server.commands.refs_for(server.repo_id).await;
    let names: Vec<&str> = refs.iter().map(|(n, _)| n.as_str()).collect();
    check!(names == vec!["refs/heads/main", "refs/tags/v1.0.0"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deleted_branch_disappears_from_the_forge() {
    let Some(server) = serve_empty_repo("octocat", "deletable").await else {
        return;
    };
    let url = server.clone_url("octocat", "deletable");

    let work = tempfile::tempdir().unwrap();
    import::make_test_repo(work.path(), &[("f.txt", b"x\n")]).unwrap();
    git(work.path(), &["branch", "scratch"]);
    git(
        work.path(),
        &[
            "push",
            &url,
            "refs/heads/main:refs/heads/main",
            "refs/heads/scratch:refs/heads/scratch",
        ],
    );
    check!(server.commands.refs_for(server.repo_id).await.len() == 2);

    let deleted = git(work.path(), &["push", &url, ":refs/heads/scratch"]);
    check!(
        deleted.status.success(),
        "delete failed: {}",
        stderr(&deleted)
    );

    let refs = server.commands.refs_for(server.repo_id).await;
    check!(refs.len() == 1);
    check!(refs[0].0 == "refs/heads/main");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pushed_objects_survive_the_cache_being_deleted() {
    // The push path writes objects to the log, not merely to the cache. If it
    // did not, a wiped cache would lose pushed history.
    let Some(server) = serve_empty_repo("octocat", "durable").await else {
        return;
    };
    let url = server.clone_url("octocat", "durable");

    let work = tempfile::tempdir().unwrap();
    import::make_test_repo(work.path(), &[("kept.txt", b"survives a wipe\n")]).unwrap();
    let push = git(
        work.path(),
        &["push", &url, "refs/heads/main:refs/heads/main"],
    );
    check!(push.status.success(), "push failed: {}", stderr(&push));

    for entry in std::fs::read_dir(server._cache_root.path()).unwrap() {
        std::fs::remove_dir_all(entry.unwrap().path()).unwrap();
    }

    let dest = tempfile::tempdir().unwrap();
    let clone = std::process::Command::new("git")
        .args(["clone", "--quiet", &url, "recovered"])
        .current_dir(dest.path())
        .output()
        .unwrap();
    check!(
        clone.status.success(),
        "clone after cache wipe failed: {}",
        stderr(&clone)
    );

    let content = std::fs::read_to_string(dest.path().join("recovered/kept.txt")).unwrap();
    check!(
        content == "survives a wipe\n",
        "pushed content came back from the log"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_hook_callback_refuses_an_unauthenticated_caller() {
    // Anything on the host could otherwise approve a push.
    let Some(server) = serve_empty_repo("octocat", "guarded").await else {
        return;
    };

    let status = std::process::Command::new("curl")
        .args([
            "-s",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            "-X",
            "POST",
            "-H",
            "X-Forge-Token: wrong",
            "--data",
            "",
            &format!("{}/internal/hooks/pre-receive", server.base_url),
        ])
        .output()
        .unwrap();

    check!(String::from_utf8_lossy(&status.stdout) == "401");
}
