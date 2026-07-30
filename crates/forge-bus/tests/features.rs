//! The feature probe, against a real broker.
//!
//! The unit tests next to `BrokerFeatures` cover the decision it makes from a
//! set of levels. What they cannot cover is whether a broker actually puts
//! those levels on the wire where the probe looks for them — the finalized
//! features live in `ApiVersionsResponse`, which the admin client does not
//! surface, so this path is hand-rolled and would fail silently as "no features
//! finalized" if the field ever moved.

use assert2::check;
use forge_bus::{BrokerFeatures, SHARE_VERSION};
use forge_testkit::TestBroker;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_in_process_broker_has_share_version_unset() {
    // Not the state anyone would choose — it is simply the only state reachable
    // from `Broker::start`. KIP-584 levels are seeded when a log directory is
    // formatted, and `BrokerConfig` exposes no way to set them, so an
    // in-process fixture always comes up at the registry defaults and
    // `share.version` defaults to 0.
    //
    // Recorded here because the fixture's own documentation used to claim the
    // opposite, and because the CI queue works anyway: see
    // `forge-ci/tests/queue.rs`, which establishes that crabka does not
    // currently enforce the gate.
    let broker = TestBroker::start().await;

    let features = BrokerFeatures::probe(&broker.bootstrap())
        .await
        .expect("probing broker features");

    check!(
        !features.share_groups(),
        "share.version is now {} on an in-process broker — crabka has gained a \
         way to seed feature levels, and forge-testkit should use it",
        features.level(SHARE_VERSION)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_probe_reports_more_than_the_one_feature_it_is_asked_about() {
    // Guards against the probe appearing to work while reading an empty list:
    // `level()` answers 0 for anything absent, so a response that decoded to no
    // features at all would look exactly like a broker with share groups off.
    let broker = TestBroker::start().await;

    let features = BrokerFeatures::probe(&broker.bootstrap()).await.unwrap();

    check!(features.level("metadata.version") > 0, "{:?}", features);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreachable_broker_is_an_error_rather_than_an_empty_answer() {
    // The distinction the doctor depends on. A probe that returned
    // `BrokerFeatures::default()` when it could not reach the broker would
    // report a perfectly healthy cluster as needing a reformat.
    let result = BrokerFeatures::probe("127.0.0.1:1").await;

    check!(result.is_err());
}
