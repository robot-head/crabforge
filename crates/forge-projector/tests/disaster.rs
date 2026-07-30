//! The disaster drill.
//!
//! The whole architecture rests on one claim: the broker log is the only source
//! of truth, and everything else — the SQL read models, the git caches — is a
//! disposable projection of it. That claim is either true or it is marketing,
//! and the difference is testable.
//!
//! So: build up real state, destroy every derived store, rebuild from the log
//! alone, and assert the forge is the same forge. If this test ever fails, the
//! recovery procedure documented everywhere else is fiction.

use assert2::check;
use forge_bus::{FencedWriter, PendingRecord};
use forge_events::{IssueEvent, RepoEvent, UserEvent};
use forge_projector::Projector;
use forge_store::Store;
use forge_testkit::{TestBroker, require_gres};
use forge_types::{IssueId, RepoId, UserId, Visibility, topics};

/// Everything the forge knows, as a comparable snapshot.
///
/// Compared as data rather than field by field so that a column added later is
/// covered by this test without anyone remembering to add it.
#[derive(Debug, PartialEq)]
struct Snapshot {
    users: Vec<forge_store::UserRecord>,
    repos: Vec<forge_store::RepoRecord>,
    issues: Vec<forge_store::IssueRecord>,
    counters: (i64, i64),
}

async fn snapshot(store: &Store, user_id: &str, repo_id: &str) -> Snapshot {
    let counters = store.issues().counters(repo_id).await.unwrap();
    Snapshot {
        users: store
            .users()
            .by_id(user_id)
            .await
            .unwrap()
            .into_iter()
            .collect(),
        repos: store
            .repos()
            .by_id(repo_id)
            .await
            .unwrap()
            .into_iter()
            .collect(),
        issues: {
            let mut all = store
                .issues()
                .list(repo_id, true, None, forge_store::page_size(100))
                .await
                .unwrap();
            all.extend(
                store
                    .issues()
                    .list(repo_id, false, None, forge_store::page_size(100))
                    .await
                    .unwrap(),
            );
            all.sort_by_key(|i| i.number);
            all
        },
        counters: (counters.open_issues, counters.closed_issues),
    }
}

/// Project every domain topic to its end.
async fn project_everything(bootstrap: &str, dsn: &str) {
    for topic in [
        topics::EVENTS_USERS,
        topics::EVENTS_REPOS,
        topics::EVENTS_ISSUES,
    ] {
        let store = Store::connect(dsn).await.unwrap();
        let mut projector = Projector::open(bootstrap, topic, store).await.unwrap();
        projector.drain().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_forge_survives_losing_every_read_model() {
    let Some(gres) = require_gres().await else {
        return;
    };
    let broker = TestBroker::with_forge_topics().await;
    let dsn = gres.dsn();
    let store = Store::connect(&dsn).await.unwrap();
    store.migrate().await.unwrap();

    // ── A forge with some history in it ──────────────────────────────────
    let writer = FencedWriter::connect(&broker.bootstrap()).await.unwrap();
    let user_id = UserId::new();
    let repo_id = RepoId::new();

    let registered = UserEvent::Registered {
        user_id,
        username: "octocat".into(),
        username_lower: "octocat".into(),
        email: "octocat@example.com".into(),
        password_hash: "$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA".into(),
    };
    let created = RepoEvent::Created {
        repo_id,
        owner_id: user_id,
        owner_name: "octocat".into(),
        name: "hello".into(),
        full_name_lower: "octocat/hello".into(),
        description: Some("a repository".into()),
        default_branch: "main".into(),
        visibility: Visibility::Public,
    };
    let opened = IssueEvent::Opened {
        issue_id: IssueId::new(),
        repo_id,
        number: 1,
        title: "something is broken".into(),
        body: Some("here is what happened".into()),
        author_id: user_id,
        author_name: "octocat".into(),
    };
    let closed_id = IssueId::new();
    let also_opened = IssueEvent::Opened {
        issue_id: closed_id,
        repo_id,
        number: 2,
        title: "and another".into(),
        body: None,
        author_id: user_id,
        author_name: "octocat".into(),
    };
    let closed = IssueEvent::Closed {
        issue_id: closed_id,
        repo_id,
        actor: user_id,
    };

    for record in [
        PendingRecord::event(&registered, Some(user_id)).unwrap(),
        PendingRecord::event(&created, Some(user_id)).unwrap(),
        PendingRecord::event(&opened, Some(user_id)).unwrap(),
        PendingRecord::event(&also_opened, Some(user_id)).unwrap(),
        PendingRecord::event(&closed, Some(user_id)).unwrap(),
    ] {
        writer.transact(vec![record]).await.unwrap();
    }

    project_everything(&broker.bootstrap(), &dsn).await;
    let before = snapshot(&store, &user_id.to_string(), &repo_id.to_string()).await;
    check!(
        before.users.len() == 1,
        "the forge should have state to lose"
    );
    check!(before.issues.len() == 2);
    check!(before.counters == (1, 1), "one open, one closed");

    // ── The disaster ─────────────────────────────────────────────────────
    //
    // Not a truncate: every table dropped, the schema gone, the cursors gone.
    // This is what "we lost the database" means, and anything short of it
    // would be testing a gentler failure than the one being claimed.
    for table in [
        "users",
        "repos",
        "repo_collaborators",
        "issues",
        "issue_comments",
        "repo_counters",
        "pulls",
        "pr_reviews",
        "access_tokens",
        "web_sessions",
        "webhooks",
        "webhook_deliveries",
        "ci_runs",
        "ci_jobs",
        "reader_cursors",
        "schema_migrations",
    ] {
        store
            .client()
            .execute(&format!("DROP TABLE {table}"), &[])
            .await
            .unwrap_or_else(|e| panic!("dropping {table}: {e}"));
    }
    check!(
        store.users().count().await.is_err(),
        "the read models should really be gone"
    );

    // ── The recovery ─────────────────────────────────────────────────────
    //
    // Exactly what an operator would do: apply the schema, replay the log.
    // No backup, no export, nothing but the broker.
    let recovered = Store::connect(&dsn).await.unwrap();
    recovered.migrate().await.unwrap();
    project_everything(&broker.bootstrap(), &dsn).await;

    let after = snapshot(&recovered, &user_id.to_string(), &repo_id.to_string()).await;
    check!(after == before, "the forge did not come back the same");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_git_cache_rebuilds_itself_from_the_log() {
    // The other half of the claim. Repository contents live in the log as
    // objects; the bare repository on disk is a cache. Deleting it must cost
    // nothing but the time to refill it.
    let Some(_gres) = require_gres().await else {
        return;
    };
    let broker = TestBroker::with_forge_topics().await;
    let repo_id = RepoId::new();
    let mut admin = broker.admin().await;
    forge_topics::ensure_repo(&mut admin, repo_id)
        .await
        .unwrap();

    // A repository whose objects are on the log.
    let source = tempfile::tempdir().unwrap();
    let seed = forge_git::Cache::new(source.path(), repo_id);
    let head = forge_git::import::make_test_repo(
        &seed.path(),
        &[("README.md", b"hello"), ("src/main.rs", b"fn main() {}")],
    )
    .unwrap();

    let objects = forge_git::import::read_all_objects(&seed.path()).unwrap();
    let writer = forge_git::connect_object_writer(&broker.bootstrap())
        .await
        .unwrap();
    let object_writer = forge_git::ObjectWriter::new(&writer, repo_id);
    for object in &objects {
        object_writer.put(object).await.unwrap();
    }

    // A cache hydrated from the log alone, in a directory that has never seen
    // this repository.
    let cache_root = tempfile::tempdir().unwrap();
    let cache = forge_git::Cache::new(cache_root.path(), repo_id);
    cache.hydrate(&broker.bootstrap(), "main").await.unwrap();

    let readme = cache.read_blob(&head.to_hex(), "README.md").unwrap();
    check!(readme.is_some(), "the cache did not rebuild");

    // Now destroy it and do it again — the operation an operator performs when
    // a cache is corrupt.
    cache.destroy().unwrap();
    check!(!cache.exists());
    cache.hydrate(&broker.bootstrap(), "main").await.unwrap();

    let readme = cache.read_blob(&head.to_hex(), "README.md").unwrap();
    check!(readme.is_some(), "the cache did not rebuild a second time");
    let main = cache.read_blob(&head.to_hex(), "src/main.rs").unwrap();
    check!(main.is_some(), "a nested path was lost");
}
