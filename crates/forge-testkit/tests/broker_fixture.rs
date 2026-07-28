//! The fixture is load-bearing for every later milestone's tests, so it gets
//! its own coverage: if these fail, nothing downstream can be trusted.

use assert2::check;
use forge_testkit::TestBroker;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn in_process_broker_accepts_admin_connections() {
    let broker = TestBroker::start().await;
    let mut admin = broker.admin().await;

    let metadata = admin.metadata(&[]).await.expect("fetch cluster metadata");
    check!(
        metadata.controller_id >= 0,
        "a single-node broker is its own controller"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forge_topics_provision_onto_a_real_broker() {
    let broker = TestBroker::with_forge_topics().await;
    let mut admin = broker.admin().await;

    let missing = forge_topics::missing(&mut admin, &forge_topics::static_topics())
        .await
        .expect("list topics");
    check!(missing.is_empty(), "topics were not created: {missing:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provisioning_is_idempotent() {
    // Bootstrap runs on every boot, so `TOPIC_ALREADY_EXISTS` is the normal
    // steady-state outcome rather than an error.
    let broker = TestBroker::with_forge_topics().await;
    let mut admin = broker.admin().await;

    forge_topics::ensure_static(&mut admin)
        .await
        .expect("second provisioning pass must succeed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn per_repo_object_topics_are_created_on_demand() {
    let broker = TestBroker::start().await;
    let mut admin = broker.admin().await;
    let repo = forge_types::RepoId::new();

    forge_topics::ensure_repo(&mut admin, repo)
        .await
        .expect("create repo object topic");

    let spec = forge_topics::repo_objects_topic(repo);
    let missing = forge_topics::missing(&mut admin, std::slice::from_ref(&spec))
        .await
        .expect("list topics");
    check!(missing.is_empty(), "{} was not created", spec.name);
}
