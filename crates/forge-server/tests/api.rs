//! The API end to end: HTTP in, log and SQL underneath, HTTP out.

use std::sync::Arc;

use assert2::check;
use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use forge_command::CommandService;
use forge_projector::Projector;
use forge_server::{AppState, router};
use forge_store::Store;
use forge_testkit::{TestBroker, require_gres};
use forge_types::topics;
use tower::ServiceExt as _;

struct Harness {
    _gres: forge_testkit::Gres,
    _broker: TestBroker,
    router: axum::Router,
}

impl Harness {
    async fn start() -> Option<Self> {
        let gres = require_gres().await?;
        let broker = TestBroker::with_forge_topics().await;

        let store = Arc::new(Store::connect(&gres.dsn()).await.unwrap());
        store.migrate().await.unwrap();
        let commands = CommandService::start(&broker.bootstrap()).await.unwrap();

        let mut state = AppState::new(broker.bootstrap())
            .with_commands(commands)
            .with_store(Arc::clone(&store));

        for topic in [topics::EVENTS_USERS, topics::EVENTS_REPOS] {
            let projector = Projector::open(
                &broker.bootstrap(),
                topic,
                Store::connect(&gres.dsn()).await.unwrap(),
            )
            .await
            .unwrap();
            state = state.with_projection(topic, projector.applied());
            tokio::spawn(async move {
                let _ = projector.run().await;
            });
        }

        Some(Self {
            _gres: gres,
            _broker: broker,
            router: router(Arc::new(state)),
        })
    }

    async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, serde_json::Value) {
        let builder = Request::builder().method(method).uri(path);
        let request = match body {
            Some(json) => builder
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&json).unwrap()))
                .unwrap(),
            None => builder.body(Body::empty()).unwrap(),
        };

        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    async fn register(&self, username: &str) -> (StatusCode, serde_json::Value) {
        self.request(
            "POST",
            "/api/v1/users",
            Some(serde_json::json!({
                "username": username,
                "email": format!("{username}@example.com"),
                "password": "correct horse battery staple",
            })),
        )
        .await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn registering_then_fetching_a_user_works_through_http() {
    let Some(h) = Harness::start().await else {
        return;
    };

    let (status, body) = h.register("octocat").await;
    check!(status == StatusCode::CREATED, "got {status}: {body}");
    check!(body["username"] == "octocat");

    // The write is immediately readable — the handler waited for its own
    // projection before answering.
    let (status, body) = h.request("GET", "/api/v1/users/octocat", None).await;
    check!(status == StatusCode::OK);
    check!(body["email"] == "octocat@example.com");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn passwords_are_never_echoed_back() {
    let Some(h) = Harness::start().await else {
        return;
    };

    let (_, body) = h.register("careful").await;
    let rendered = body.to_string();
    check!(
        !rendered.contains("correct horse"),
        "plaintext password leaked"
    );
    check!(!rendered.contains("argon2"), "password hash leaked");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_duplicate_username_is_a_conflict() {
    let Some(h) = Harness::start().await else {
        return;
    };

    h.register("octocat").await;
    let (status, body) = h.register("octocat").await;

    check!(status == StatusCode::CONFLICT);
    check!(body["code"] == "username_taken");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reserved_name_is_rejected_before_it_reaches_the_log() {
    let Some(h) = Harness::start().await else {
        return;
    };

    let (status, body) = h.register("settings").await;
    check!(status == StatusCode::UNPROCESSABLE_ENTITY);
    check!(body["code"] == "invalid_name");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn creating_and_reading_a_repository() {
    let Some(h) = Harness::start().await else {
        return;
    };
    h.register("octocat").await;

    let (status, body) = h
        .request(
            "POST",
            "/api/v1/repos",
            Some(serde_json::json!({
                "owner": "octocat",
                "name": "Hello-World",
                "description": "my first repository",
            })),
        )
        .await;
    check!(status == StatusCode::CREATED, "got {status}: {body}");
    check!(body["full_name"] == "octocat/Hello-World");
    check!(body["default_branch"] == "main");
    check!(body["private"] == false);

    // Case-insensitive resolution, as clone URLs require.
    let (status, body) = h
        .request("GET", "/api/v1/repos/OCTOCAT/hello-world", None)
        .await;
    check!(status == StatusCode::OK);
    check!(body["name"] == "Hello-World", "display name keeps its case");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repositories_are_listed_for_their_owner() {
    let Some(h) = Harness::start().await else {
        return;
    };
    h.register("prolific").await;

    for name in ["alpha", "beta", "gamma"] {
        h.request(
            "POST",
            "/api/v1/repos",
            Some(serde_json::json!({"owner": "prolific", "name": name})),
        )
        .await;
    }

    let (status, body) = h.request("GET", "/api/v1/users/prolific/repos", None).await;
    check!(status == StatusCode::OK);
    let names: Vec<&str> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap())
        .collect();
    // Newest first, by time-ordered id.
    check!(names == vec!["gamma", "beta", "alpha"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_repository_for_an_unknown_owner_is_not_found() {
    let Some(h) = Harness::start().await else {
        return;
    };

    let (status, body) = h
        .request(
            "POST",
            "/api/v1/repos",
            Some(serde_json::json!({"owner": "ghost", "name": "nothing"})),
        )
        .await;
    check!(status == StatusCode::NOT_FOUND);
    check!(body["code"] == "not_found");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn missing_resources_return_a_structured_error() {
    let Some(h) = Harness::start().await else {
        return;
    };

    let (status, body) = h.request("GET", "/api/v1/users/nobody", None).await;
    check!(status == StatusCode::NOT_FOUND);
    check!(body["code"] == "not_found");
    check!(body["message"].is_string());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn writes_are_refused_when_the_command_service_is_absent() {
    // Degraded mode: reads may still work, writes must fail honestly rather
    // than appear to succeed.
    let Some(gres) = require_gres().await else {
        return;
    };
    let store = Arc::new(Store::connect(&gres.dsn()).await.unwrap());
    store.migrate().await.unwrap();
    let state = AppState::new("127.0.0.1:1").with_store(store);
    let app = router(Arc::new(state));

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/users")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({"username": "x", "email": "x@e.com", "password": "p"})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    check!(response.status() == StatusCode::SERVICE_UNAVAILABLE);
}
