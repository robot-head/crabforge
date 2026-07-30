//! Readiness must distinguish "the process is up" from "the platform is up",
//! because orchestrators act on the difference.

use std::sync::Arc;

use assert2::check;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use forge_server::{AppState, router};
use forge_testkit::TestBroker;
use tower::ServiceExt as _;

async fn get(app: axum::Router, path: &str) -> (StatusCode, serde_json::Value) {
    let response = app
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .expect("router response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("read body");
    let json = serde_json::from_slice(&bytes).expect("body is json");
    (status, json)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_runner_answers_probes_and_serves_nothing_else() {
    // `--role=runner` exists so the CI tier can scale from zero, which means
    // its pods are the ones most likely to be reachable from somewhere
    // unexpected. It has no business serving the API, and an accidental
    // `.merge(api::router())` would not fail any other test.
    let state = Arc::new(AppState::new("127.0.0.1:1"));
    let app = forge_server::health_router(state);

    let (status, body) = get(app.clone(), "/healthz").await;
    check!(status == StatusCode::OK);
    check!(body["status"] == "ok");

    for path in [
        "/api/v1/repos/octocat/hello",
        "/octocat/hello.git/info/refs",
    ] {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .expect("router response");
        check!(
            response.status() == StatusCode::NOT_FOUND,
            "a runner served {path}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn liveness_passes_without_any_dependency() {
    // Points at a port nothing is listening on: liveness must not probe it.
    let state = Arc::new(AppState::new("127.0.0.1:1"));
    let (status, body) = get(router(state), "/healthz").await;

    check!(status == StatusCode::OK);
    check!(body["status"] == "ok");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readiness_reports_ready_when_the_broker_answers() {
    let broker = TestBroker::start().await;
    let state = Arc::new(AppState::new(broker.bootstrap()));

    let (status, body) = get(router(state), "/readyz").await;
    check!(status == StatusCode::OK);
    check!(body["status"] == "ready");
    check!(body["broker"]["reachable"] == true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readiness_reports_degraded_when_the_broker_is_unreachable() {
    let state = Arc::new(AppState::new("127.0.0.1:1"));

    let (status, body) = get(router(state), "/readyz").await;
    check!(status == StatusCode::SERVICE_UNAVAILABLE);
    check!(body["status"] == "degraded");
    check!(body["broker"]["reachable"] == false);
    check!(
        body["broker"]["detail"].is_string(),
        "an unreachable broker must say why"
    );
}
