//! The full loop: a command appends to the log, the projector turns it into
//! SQL, and a reader sees it.
//!
//! This is the architecture's central claim under test — that a forge can keep
//! its system of record in a log and still answer queries.

use std::{sync::Arc, time::Duration};

use assert2::check;
use forge_command::{CommandService, CreateRepo, RegisterUser};
use forge_projector::{Projector, wait_for_offset};
use forge_store::Store;
use forge_testkit::{TestBroker, require_gres};
use forge_types::{Username, Visibility, topics};

struct Harness {
    _gres: forge_testkit::Gres,
    broker: TestBroker,
    store: Arc<Store>,
    commands: Arc<CommandService>,
}

impl Harness {
    async fn start() -> Option<Self> {
        let gres = require_gres().await?;
        let broker = TestBroker::with_forge_topics().await;
        let store = Arc::new(Store::connect(&gres.dsn()).await.expect("connect gres"));
        store.migrate().await.expect("migrate");
        let commands = CommandService::start(&broker.bootstrap())
            .await
            .expect("start command service");
        Some(Self {
            _gres: gres,
            broker,
            store,
            commands,
        })
    }

    async fn projector(&self, topic: &str) -> Projector {
        Projector::open(&self.broker.bootstrap(), topic, Arc::clone(&self.store))
            .await
            .expect("open projector")
    }
}

fn registration(username: &str) -> RegisterUser {
    RegisterUser {
        username: username.to_string(),
        email: format!("{username}@example.com"),
        password_hash: "$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA".to_string(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_registration_becomes_a_queryable_row() {
    let Some(h) = Harness::start().await else {
        return;
    };

    h.commands
        .register_user(registration("octocat"))
        .await
        .unwrap();
    let mut projector = h.projector(topics::EVENTS_USERS).await;
    let applied = projector.drain().await.unwrap();
    check!(applied == 1);

    let user = h.store.users().by_username_lower("octocat").await.unwrap();
    check!(user.is_some(), "the command must be visible in SQL");
    let user = user.unwrap();
    check!(user.username == "octocat");
    check!(user.state == "active");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn read_your_writes_gates_on_the_projection_catching_up() {
    // What an HTTP handler does between committing a command and answering.
    let Some(h) = Harness::start().await else {
        return;
    };

    let outcome = h
        .commands
        .register_user(registration("impatient"))
        .await
        .unwrap();
    let target = outcome.committed.offset_for(topics::EVENTS_USERS).unwrap();

    let mut projector = h.projector(topics::EVENTS_USERS).await;
    let applied = projector.applied();

    // Not yet caught up, so the gate must not pass.
    check!(!wait_for_offset(applied.clone(), target, Duration::from_millis(50)).await);

    projector.drain().await.unwrap();

    check!(wait_for_offset(applied, target, Duration::from_secs(5)).await);
    check!(
        h.store
            .users()
            .by_username_lower("impatient")
            .await
            .unwrap()
            .is_some(),
        "once the gate passes the row must be readable"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn projection_is_idempotent_across_a_full_replay() {
    // The disaster-recovery property: drop the read models, replay from zero,
    // get the same state back.
    let Some(h) = Harness::start().await else {
        return;
    };

    h.commands
        .register_user(registration("alice"))
        .await
        .unwrap();
    h.commands.register_user(registration("bob")).await.unwrap();

    h.projector(topics::EVENTS_USERS)
        .await
        .drain()
        .await
        .unwrap();
    check!(h.store.users().count().await.unwrap() == 2);

    // Rewind the cursor and replay everything from the beginning.
    h.store
        .cursors()
        .set_applied_offset(topics::EVENTS_USERS, 0)
        .await
        .unwrap();
    let mut replayed = h.projector(topics::EVENTS_USERS).await;
    replayed.drain().await.unwrap();

    check!(
        h.store.users().count().await.unwrap() == 2,
        "replaying must not duplicate rows"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_projector_resumes_where_it_left_off() {
    let Some(h) = Harness::start().await else {
        return;
    };

    h.commands
        .register_user(registration("first"))
        .await
        .unwrap();
    h.projector(topics::EVENTS_USERS)
        .await
        .drain()
        .await
        .unwrap();

    // A second projector instance, as after a restart.
    h.commands
        .register_user(registration("second"))
        .await
        .unwrap();
    let mut resumed = h.projector(topics::EVENTS_USERS).await;
    let applied = resumed.drain().await.unwrap();

    check!(applied == 1, "only the new event should be applied");
    check!(h.store.users().count().await.unwrap() == 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repositories_project_with_their_lookup_key() {
    let Some(h) = Harness::start().await else {
        return;
    };
    let owner = h
        .commands
        .register_user(registration("octocat"))
        .await
        .unwrap();

    h.commands
        .create_repo(CreateRepo {
            owner: owner.id,
            owner_name: Username::parse("octocat").unwrap(),
            name: "Hello-World".to_string(),
            description: Some("my first repository".to_string()),
            visibility: Visibility::Public,
        })
        .await
        .unwrap();

    h.projector(topics::EVENTS_REPOS)
        .await
        .drain()
        .await
        .unwrap();

    let repo = h
        .store
        .repos()
        .by_full_name("octocat/hello-world")
        .await
        .unwrap()
        .expect("repository must be resolvable by its lowercased path");
    check!(repo.name == "Hello-World", "display name keeps its case");
    check!(repo.default_branch == "main");
    check!(repo.description.as_deref() == Some("my first repository"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_cursor_never_outruns_the_rows_it_accounts_for() {
    // If the cursor could advance without its rows, a crash would silently
    // skip events forever. Assert they move together.
    let Some(h) = Harness::start().await else {
        return;
    };

    h.commands
        .register_user(registration("paired"))
        .await
        .unwrap();
    let mut projector = h.projector(topics::EVENTS_USERS).await;
    projector.drain().await.unwrap();

    let cursor = h
        .store
        .cursors()
        .applied_offset(topics::EVENTS_USERS)
        .await
        .unwrap();
    check!(cursor > 0, "cursor advanced");
    check!(
        h.store.users().count().await.unwrap() == 1,
        "and so did the rows"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn draining_an_empty_topic_is_a_no_op() {
    let Some(h) = Harness::start().await else {
        return;
    };

    let mut projector = h.projector(topics::EVENTS_REPOS).await;
    check!(projector.drain().await.unwrap() == 0);
    check!(
        h.store
            .cursors()
            .applied_offset(topics::EVENTS_REPOS)
            .await
            .unwrap()
            == 0
    );
}
