//! Command service behaviour against a real broker.

use assert2::{assert, check};
use forge_command::{CommandError, CommandService, CreateRepo, RegisterUser};
use forge_testkit::TestBroker;
use forge_types::{Username, Visibility};

fn registration(username: &str) -> RegisterUser {
    RegisterUser {
        username: username.to_string(),
        email: format!("{username}@example.com"),
        password_hash: "$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA".to_string(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registering_a_user_claims_the_name() {
    let broker = TestBroker::with_forge_topics().await;
    let service = CommandService::start(&broker.bootstrap()).await.unwrap();

    let outcome = service
        .register_user(registration("octocat"))
        .await
        .unwrap();
    check!(
        outcome
            .committed
            .offset_for(forge_types::topics::EVENTS_USERS)
            .is_some()
    );
    check!(service.claim_count().await == 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_duplicate_username_is_rejected() {
    let broker = TestBroker::with_forge_topics().await;
    let service = CommandService::start(&broker.bootstrap()).await.unwrap();

    service
        .register_user(registration("octocat"))
        .await
        .unwrap();
    let second = service.register_user(registration("octocat")).await;
    assert!(let Err(CommandError::UsernameTaken) = second);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn username_uniqueness_ignores_case() {
    // Otherwise `Octocat` and `octocat` would resolve to the same URL but be
    // two accounts.
    let broker = TestBroker::with_forge_topics().await;
    let service = CommandService::start(&broker.bootstrap()).await.unwrap();

    service
        .register_user(registration("Octocat"))
        .await
        .unwrap();
    let clash = service.register_user(registration("OCTOCAT")).await;
    assert!(let Err(CommandError::UsernameTaken) = clash);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reserved_route_names_cannot_be_registered() {
    let broker = TestBroker::with_forge_topics().await;
    let service = CommandService::start(&broker.bootstrap()).await.unwrap();

    let result = service.register_user(registration("settings")).await;
    assert!(let Err(CommandError::InvalidName(_)) = result);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claims_survive_a_restart_by_replaying_the_log() {
    // The property that makes in-memory decision state safe: it is a cache of
    // the log, not the only copy.
    let broker = TestBroker::with_forge_topics().await;

    let first = CommandService::start(&broker.bootstrap()).await.unwrap();
    first
        .register_user(registration("persistent"))
        .await
        .unwrap();
    drop(first);

    let restarted = CommandService::start(&broker.bootstrap()).await.unwrap();
    check!(
        restarted.claim_count().await == 1,
        "catalog rebuilt from the log"
    );

    let clash = restarted.register_user(registration("persistent")).await;
    assert!(let Err(CommandError::UsernameTaken) = clash);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repositories_are_unique_per_owner_not_globally() {
    let broker = TestBroker::with_forge_topics().await;
    let service = CommandService::start(&broker.bootstrap()).await.unwrap();

    let alice = service.register_user(registration("alice")).await.unwrap();
    let bob = service.register_user(registration("bob")).await.unwrap();

    let make = |owner, name: &str, owner_name: &str| CreateRepo {
        owner,
        owner_name: Username::parse(owner_name).unwrap(),
        name: name.to_string(),
        description: None,
        visibility: Visibility::Public,
    };

    service
        .create_repo(make(alice.id, "notes", "alice"))
        .await
        .unwrap();
    // Same repository name under a different owner is a different repository.
    service
        .create_repo(make(bob.id, "notes", "bob"))
        .await
        .unwrap();

    let duplicate = service.create_repo(make(alice.id, "notes", "alice")).await;
    assert!(let Err(CommandError::RepoExists) = duplicate);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_repo_may_be_named_after_a_reserved_route() {
    // Reservation applies to top-level paths, not to `owner/settings`.
    let broker = TestBroker::with_forge_topics().await;
    let service = CommandService::start(&broker.bootstrap()).await.unwrap();
    let owner = service.register_user(registration("alice")).await.unwrap();

    service
        .create_repo(CreateRepo {
            owner: owner.id,
            owner_name: Username::parse("alice").unwrap(),
            name: "settings".to_string(),
            description: None,
            visibility: Visibility::Public,
        })
        .await
        .expect("owner/settings is addressable");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_registrations_of_one_name_yield_exactly_one_winner() {
    // The catalog is read and written under one lock, so the check cannot be
    // observed as free by two commands at once.
    let broker = TestBroker::with_forge_topics().await;
    let service = CommandService::start(&broker.bootstrap()).await.unwrap();

    let attempts = (0..8).map(|_| {
        let service = service.clone();
        tokio::spawn(async move { service.register_user(registration("contested")).await })
    });

    let mut winners = 0;
    for attempt in attempts {
        if attempt.await.unwrap().is_ok() {
            winners += 1;
        }
    }
    check!(winners == 1, "exactly one registration may succeed");
    check!(service.claim_count().await == 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn creating_a_repository_provisions_its_object_topic() {
    // Git objects live in a topic per repository and the broker does not
    // auto-create topics, so a repository without one cannot be cloned — the
    // first fetch fails with `UNKNOWN_TOPIC` and the forge answers 500.
    //
    // This went unnoticed because every test that clones or pushes provisions
    // the topic itself before starting, which is a thing no production caller
    // does. The sequence in the README — create a repository over the API, then
    // `git clone` it — did not work.
    let broker = TestBroker::with_forge_topics().await;
    let service = CommandService::start(&broker.bootstrap()).await.unwrap();

    let owner = service
        .register_user(registration("octocat"))
        .await
        .unwrap();
    let repo = service
        .create_repo(CreateRepo {
            owner: owner.id,
            owner_name: Username::parse("octocat").unwrap(),
            name: "Hello-World".to_string(),
            description: None,
            visibility: Visibility::Public,
        })
        .await
        .unwrap();

    let mut admin = broker.admin().await;
    let missing = forge_topics::missing(&mut admin, &[forge_topics::repo_objects_topic(repo.id)])
        .await
        .unwrap();
    check!(
        missing.is_empty(),
        "the repository's object topic was not provisioned: {missing:?}"
    );
}
