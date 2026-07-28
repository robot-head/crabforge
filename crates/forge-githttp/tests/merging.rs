//! Merging a pull request, end to end.
//!
//! A merge is the one operation that both writes git objects and moves a
//! reference, so it exercises the whole stack at once: the cache computes it,
//! the log stores it, and the command service decides whether it may land.

use std::{path::Path, sync::Arc};

use assert2::check;
use forge_command::{CommandService, CreateRepo, OpenPull, RegisterUser};
use forge_git::{Cache, ObjectWriter, import};
use forge_projector::Projector;
use forge_store::{Mergeable, Store};
use forge_testkit::{TestBroker, require_gres};
use forge_types::{Oid, PrId, RepoId, UserId, Username, Visibility, topics};

struct Harness {
    _gres: forge_testkit::Gres,
    broker: TestBroker,
    _cache_root: tempfile::TempDir,
    store: Arc<Store>,
    commands: Arc<CommandService>,
    writer: forge_bus::FencedWriter,
    cache: Cache,
    repo: RepoId,
    author: UserId,
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

impl Harness {
    /// A repository with `main` and `feature` diverged as described.
    async fn start(main_side: &str, feature_side: &str) -> Option<Self> {
        let gres = require_gres().await?;
        let broker = TestBroker::with_forge_topics().await;
        let cache_root = tempfile::tempdir().unwrap();

        let store = Arc::new(Store::connect(&gres.dsn()).await.unwrap());
        store.migrate().await.unwrap();
        let commands = CommandService::start(&broker.bootstrap()).await.unwrap();

        let author = commands
            .register_user(RegisterUser {
                username: "octocat".into(),
                email: "octocat@example.com".into(),
                password_hash: "$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA".into(),
            })
            .await
            .unwrap();
        let repo = commands
            .create_repo(CreateRepo {
                owner: author.id,
                owner_name: Username::parse("octocat").unwrap(),
                name: "hello".into(),
                description: None,
                visibility: Visibility::Public,
            })
            .await
            .unwrap();

        let mut admin = broker.admin().await;
        forge_topics::ensure_repo(&mut admin, repo.id)
            .await
            .unwrap();

        // Build the history in a scratch repository, then put it in the log.
        let source = tempfile::tempdir().unwrap();
        import::make_test_repo(source.path(), &[("f.txt", b"one\ntwo\nthree\n")]).unwrap();
        git(source.path(), &["checkout", "-qb", "feature"]);
        std::fs::write(source.path().join("f.txt"), feature_side).unwrap();
        git(source.path(), &["commit", "-qam", "feature work"]);
        git(source.path(), &["checkout", "-q", "main"]);
        if !main_side.is_empty() {
            std::fs::write(source.path().join("f.txt"), main_side).unwrap();
            git(source.path(), &["commit", "-qam", "main work"]);
        }

        let main: Oid = git(source.path(), &["rev-parse", "main"]).parse().unwrap();
        let feature: Oid = git(source.path(), &["rev-parse", "feature"])
            .parse()
            .unwrap();

        let writer = forge_git::connect_object_writer(&broker.bootstrap())
            .await
            .unwrap();
        ObjectWriter::new(&writer, repo.id)
            .put_all(&import::read_all_objects(source.path()).unwrap())
            .await
            .unwrap();

        // The references are canonical, so they go through the command service.
        commands
            .update_refs(
                repo.id,
                vec![
                    forge_command::RefUpdate {
                        name: "refs/heads/main".into(),
                        expected_old: None,
                        new: Some(main),
                    },
                    forge_command::RefUpdate {
                        name: "refs/heads/feature".into(),
                        expected_old: None,
                        new: Some(feature),
                    },
                ],
                author.id,
            )
            .await
            .unwrap();

        let cache = Cache::new(cache_root.path(), repo.id);
        cache.hydrate(&broker.bootstrap(), "main").await.unwrap();
        cache
            .sync_refs(&commands.refs_for(repo.id).await, "main")
            .unwrap();

        Some(Self {
            _gres: gres,
            broker,
            _cache_root: cache_root,
            store,
            commands,
            writer,
            cache,
            repo: repo.id,
            author: author.id,
        })
    }

    async fn project(&self) {
        for topic in [topics::EVENTS_PRS, topics::EVENTS_GIT_REFS] {
            Projector::open(&self.broker.bootstrap(), topic, Arc::clone(&self.store))
                .await
                .unwrap()
                .drain()
                .await
                .unwrap();
        }
    }

    /// Open a pull request from `feature` into `main`.
    async fn open_pr(&self) -> PrId {
        let head = self.cache.resolve("feature").unwrap().unwrap();
        let base = self.cache.resolve("main").unwrap().unwrap();
        let pr = self
            .commands
            .open_pull(OpenPull {
                repo: self.repo,
                author: self.author,
                author_name: "octocat".into(),
                title: "Merge my work".into(),
                body: None,
                source_branch: "feature".into(),
                target_branch: "main".into(),
                head_oid: head,
                base_oid: base,
            })
            .await
            .unwrap();
        self.project().await;
        pr.id
    }

    async fn record_mergeability(&self, pr: PrId) {
        let record = self
            .store
            .pulls()
            .by_id(&pr.to_string())
            .await
            .unwrap()
            .unwrap();
        forge_githttp::refresh_mergeability(&self.cache, &self.commands, self.repo, &record)
            .await
            .unwrap();
        self.project().await;
    }

    async fn pull(&self, pr: PrId) -> forge_store::PullRecord {
        self.store
            .pulls()
            .by_id(&pr.to_string())
            .await
            .unwrap()
            .unwrap()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_clean_pull_request_merges_and_moves_the_branch() {
    let Some(h) = Harness::start("one\ntwo\nthree\nfour\n", "ONE\ntwo\nthree\n").await else {
        return;
    };
    let pr = h.open_pr().await;
    h.record_mergeability(pr).await;

    let record = h.pull(pr).await;
    check!(record.mergeability() == Mergeable::Clean);
    check!(record.can_merge(), "the button should be offered");

    let merged = forge_githttp::perform_merge(
        &h.cache,
        &h.writer,
        &h.commands,
        h.repo,
        &record,
        &forge_githttp::Actor {
            id: h.author,
            name: "Octocat".into(),
            email: "octocat@example.com".into(),
        },
    )
    .await
    .expect("the merge should succeed");
    h.project().await;

    // The branch moved, in the log rather than only in the cache.
    let refs = h.commands.refs_for(h.repo).await;
    let main = refs
        .iter()
        .find(|(name, _)| name == "refs/heads/main")
        .map(|(_, oid)| *oid)
        .unwrap();
    check!(main == merged.merge_commit);

    let record = h.pull(pr).await;
    check!(record.is_merged());
    check!(record.merge_commit_oid.as_deref() == Some(merged.merge_commit.to_hex().as_str()));
    check!(record.merged_by_name.as_deref() == Some("Octocat"));
    check!(
        !record.can_merge(),
        "a merged request cannot be merged again"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_conflicting_pull_request_is_refused_and_names_the_files() {
    let Some(h) = Harness::start("one\nMAIN\nthree\n", "one\nFEATURE\nthree\n").await else {
        return;
    };
    let pr = h.open_pr().await;
    h.record_mergeability(pr).await;

    let record = h.pull(pr).await;
    check!(record.mergeability() == Mergeable::Conflict);
    check!(!record.can_merge(), "the button must not be offered");

    let result = forge_githttp::perform_merge(
        &h.cache,
        &h.writer,
        &h.commands,
        h.repo,
        &record,
        &forge_githttp::Actor {
            id: h.author,
            name: "Octocat".into(),
            email: "octocat@example.com".into(),
        },
    )
    .await;

    match result {
        Err(forge_githttp::MergeError::Conflicts(files)) => {
            check!(
                files == ["f.txt"],
                "the person resolving needs the file list"
            );
        }
        other => panic!("expected a conflict, got {:?}", other.map(|_| "success")),
    }

    // And the branch did not move.
    let refs = h.commands.refs_for(h.repo).await;
    let main = refs
        .iter()
        .find(|(name, _)| name == "refs/heads/main")
        .unwrap()
        .1;
    check!(main == h.cache.resolve("main").unwrap().unwrap());

    // The conflicting paths are recorded against the commits they were computed
    // for, so the page can show them.
    let conflicts = h
        .store
        .pulls()
        .conflicts(&record.pr_id, &record.head_oid, &record.base_oid)
        .await
        .unwrap();
    check!(conflicts == ["f.txt"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_merge_is_refused_when_the_branch_moved_underneath_it() {
    // The race: someone pushes to main between the reviewer loading the page
    // and clicking merge. The diff they approved is not the diff that would
    // land, so the merge has to lose.
    let Some(h) = Harness::start("one\ntwo\nthree\nfour\n", "ONE\ntwo\nthree\n").await else {
        return;
    };
    let pr = h.open_pr().await;
    h.record_mergeability(pr).await;
    let record = h.pull(pr).await;

    // Someone pushes to main.
    let moved = h.cache.resolve("feature").unwrap().unwrap();
    let old_main = h.cache.resolve("main").unwrap().unwrap();
    h.commands
        .update_refs(
            h.repo,
            vec![forge_command::RefUpdate {
                name: "refs/heads/main".into(),
                expected_old: Some(old_main),
                new: Some(moved),
            }],
            h.author,
        )
        .await
        .unwrap();
    h.cache
        .sync_refs(&h.commands.refs_for(h.repo).await, "main")
        .unwrap();

    let result = forge_githttp::perform_merge(
        &h.cache,
        &h.writer,
        &h.commands,
        h.repo,
        &record,
        &forge_githttp::Actor {
            id: h.author,
            name: "Octocat".into(),
            email: "octocat@example.com".into(),
        },
    )
    .await;

    check!(
        matches!(result, Err(forge_githttp::MergeError::Stale)),
        "a merge against a moved branch must be refused"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_merged_pull_request_survives_the_cache_being_deleted() {
    // The merge commit has to be in the log, not only in the cache that
    // computed it.
    let Some(h) = Harness::start("one\ntwo\nthree\nfour\n", "ONE\ntwo\nthree\n").await else {
        return;
    };
    let pr = h.open_pr().await;
    h.record_mergeability(pr).await;
    let record = h.pull(pr).await;

    let merged = forge_githttp::perform_merge(
        &h.cache,
        &h.writer,
        &h.commands,
        h.repo,
        &record,
        &forge_githttp::Actor {
            id: h.author,
            name: "Octocat".into(),
            email: "octocat@example.com".into(),
        },
    )
    .await
    .unwrap();

    h.cache.destroy().unwrap();
    h.cache
        .hydrate(&h.broker.bootstrap(), "main")
        .await
        .unwrap();
    h.cache
        .sync_refs(&h.commands.refs_for(h.repo).await, "main")
        .unwrap();

    check!(
        h.cache.contains(merged.merge_commit),
        "the merge commit came back from the log"
    );
    let commit = h
        .cache
        .commit(&merged.merge_commit.to_hex())
        .unwrap()
        .unwrap();
    check!(commit.is_merge());
    check!(commit.summary.contains("Merge pull request #1"));

    // And both sides' content is intact.
    let blob = h.cache.read_blob("main", "f.txt").unwrap().unwrap();
    let forge_git::browse::Blob::Text { content, .. } = blob else {
        panic!("expected text")
    };
    check!(content.starts_with("ONE"));
    check!(content.contains("four"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pull_request_starts_with_unknown_mergeability() {
    // Nothing has tried to merge it yet, and guessing would enable a button
    // that then fails.
    let Some(h) = Harness::start("one\ntwo\nthree\nfour\n", "ONE\ntwo\nthree\n").await else {
        return;
    };
    let pr = h.open_pr().await;

    let record = h.pull(pr).await;
    check!(record.mergeability() == Mergeable::Unknown);
    check!(!record.can_merge());
    check!(record.is_open());
    check!(record.number == 1, "pull requests share the issue sequence");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stale_mergeability_result_is_discarded() {
    // A trial merge that finishes after another push describes history that has
    // moved on. Applying it would enable the merge button on the wrong diff.
    let Some(h) = Harness::start("one\ntwo\nthree\nfour\n", "ONE\ntwo\nthree\n").await else {
        return;
    };
    let pr = h.open_pr().await;

    // A result computed for commits the pull request no longer points at.
    h.commands
        .record_mergeability(forge_command::RecordMergeability {
            repo: h.repo,
            pr,
            head_oid: Oid::zero(),
            base_oid: Oid::zero(),
            mergeable: true,
            conflicts: Vec::new(),
        })
        .await
        .unwrap();
    h.project().await;

    let record = h.pull(pr).await;
    check!(
        record.mergeability() == Mergeable::Unknown,
        "a result for other commits must not enable the button"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn closing_and_reopening_leaves_mergeability_to_be_recomputed() {
    let Some(h) = Harness::start("one\ntwo\nthree\nfour\n", "ONE\ntwo\nthree\n").await else {
        return;
    };
    let pr = h.open_pr().await;
    h.record_mergeability(pr).await;
    check!(h.pull(pr).await.mergeability() == Mergeable::Clean);

    h.commands
        .set_pull_state(h.repo, pr, h.author, false)
        .await
        .unwrap();
    h.project().await;
    check!(!h.pull(pr).await.is_open());

    h.commands
        .set_pull_state(h.repo, pr, h.author, true)
        .await
        .unwrap();
    h.project().await;

    let record = h.pull(pr).await;
    check!(record.is_open());
    check!(
        record.mergeability() == Mergeable::Unknown,
        "the base has probably moved since it was closed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_merged_pull_request_cannot_be_reopened() {
    let Some(h) = Harness::start("one\ntwo\nthree\nfour\n", "ONE\ntwo\nthree\n").await else {
        return;
    };
    let pr = h.open_pr().await;
    h.record_mergeability(pr).await;
    let record = h.pull(pr).await;
    forge_githttp::perform_merge(
        &h.cache,
        &h.writer,
        &h.commands,
        h.repo,
        &record,
        &forge_githttp::Actor {
            id: h.author,
            name: "Octocat".into(),
            email: "octocat@example.com".into(),
        },
    )
    .await
    .unwrap();
    h.project().await;

    h.commands
        .set_pull_state(h.repo, pr, h.author, true)
        .await
        .unwrap();
    h.project().await;

    check!(
        h.pull(pr).await.is_merged(),
        "reopening must not undo a merge that already landed"
    );
}
