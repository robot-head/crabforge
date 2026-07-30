//! The admin port, as a scraper sees it.

use assert2::check;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use tower::ServiceExt as _;

async fn get(path: &str) -> (StatusCode, String, String) {
    let response = forge_metrics::router()
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .expect("router response");
    let status = response.status();
    let content_type = response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .map_or(String::new(), |v| {
            String::from_utf8_lossy(v.as_bytes()).into_owned()
        });
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("read body");
    (
        status,
        content_type,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_scrape_returns_openmetrics() {
    forge_metrics::set_jobs_queued(7);

    let (status, content_type, body) = get("/metrics").await;

    check!(status == StatusCode::OK);
    // Prometheus negotiates OpenMetrics from the content type; without the
    // version it falls back to the older text format and quietly drops the
    // trailing `# EOF` check, so this is worth pinning.
    check!(
        content_type.starts_with("application/openmetrics-text"),
        "{content_type}"
    );
    check!(body.contains("forge_ci_jobs_queued 7"), "{body}");
    check!(
        body.ends_with("# EOF\n"),
        "the exposition is not terminated"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_profiling_endpoints_are_on_the_admin_port_too() {
    // Not the profile itself — taking one takes seconds by design. What matters
    // here is that the route exists, because it is mounted from crabka's
    // telemetry crate and a rename upstream would otherwise remove it silently.
    let (status, _, _) = get("/debug/pprof/profile?seconds=0").await;

    check!(
        status != StatusCode::NOT_FOUND,
        "the pprof routes are not mounted"
    );
}
