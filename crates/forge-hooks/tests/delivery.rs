//! Delivering to a real receiver over a real socket.
//!
//! The signature, the headers and the retry decision are what an integration
//! actually sees, so they are tested against a server that records what
//! arrived rather than against a mock of our own making.

use std::sync::{Arc, Mutex};

use assert2::check;
use axum::{Router, extract::State, http::HeaderMap, routing::post};
use forge_hooks::{Deliverer, Delivery, DeliveryOutcome, Payload};
use forge_store::WebhookRecord;

/// What a receiver saw.
#[derive(Default)]
struct Received {
    bodies: Vec<Vec<u8>>,
    headers: Vec<HeaderMap>,
}

#[derive(Clone)]
struct Receiver {
    seen: Arc<Mutex<Received>>,
    /// Status to answer with.
    status: u16,
}

async fn record(
    State(receiver): State<Receiver>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> axum::http::StatusCode {
    let mut seen = receiver.seen.lock().unwrap();
    seen.bodies.push(body.to_vec());
    seen.headers.push(headers);
    axum::http::StatusCode::from_u16(receiver.status).unwrap()
}

/// A deliverer that will talk to the loopback receivers below.
///
/// Every receiver in this file is on 127.0.0.1, which the default deliverer
/// refuses — correctly. The refusal itself is tested separately, with the
/// default.
fn deliverer() -> Deliverer {
    Deliverer::with_private_targets(true)
}

/// Start a receiver that answers with `status`, returning its URL.
async fn receiver(status: u16) -> (String, Arc<Mutex<Received>>) {
    let seen = Arc::new(Mutex::new(Received::default()));
    let state = Receiver {
        seen: Arc::clone(&seen),
        status,
    };
    let app = Router::new().route("/hook", post(record)).with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    (format!("http://{addr}/hook"), seen)
}

fn webhook(url: &str) -> WebhookRecord {
    let now = forge_types::now();
    WebhookRecord {
        webhook_id: "w".into(),
        repo_id: "r".into(),
        url: url.to_string(),
        secret: "s3cret".into(),
        events: vec!["*".into()],
        active: true,
        created_at: now,
        updated_at: now,
    }
}

fn delivery(webhook: WebhookRecord) -> Delivery {
    Delivery {
        webhook,
        payload: Payload {
            event_id: "01900000-0000-7000-8000-000000000000".into(),
            event_type: "git.ref_updated".into(),
            body: br#"{"ref":"refs/heads/main"}"#.to_vec(),
            ce_headers: vec![
                (
                    "ce-id".into(),
                    "01900000-0000-7000-8000-000000000000".into(),
                ),
                ("ce-type".into(), "com.crabforge.git.ref_updated".into()),
                ("ce-specversion".into(), "1.0".into()),
            ],
        },
        attempt: 1,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_delivery_arrives_signed_and_labelled() {
    let (url, seen) = receiver(200).await;
    let outcome = deliverer().send(&delivery(webhook(&url))).await;

    check!(outcome.is_success(), "got {outcome:?}");

    let seen = seen.lock().unwrap();
    check!(seen.bodies.len() == 1);
    check!(seen.bodies[0] == br#"{"ref":"refs/heads/main"}"#);

    let headers = &seen.headers[0];
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string()
    };

    // A receiver verifies this before trusting anything else.
    let signature = header("x-hub-signature-256");
    check!(
        forge_hooks::verify("s3cret", &seen.bodies[0], &signature),
        "signature did not verify: {signature}"
    );

    // The GitHub-shaped headers every integration library already reads.
    check!(header("x-forge-event") == "git.ref_updated");
    check!(header("x-forge-delivery") == "01900000-0000-7000-8000-000000000000");
    // And the CloudEvents attributes, carried through rather than dropped.
    check!(header("ce-type") == "com.crabforge.git.ref_updated");
    check!(header("ce-specversion") == "1.0");
    check!(header("content-type").starts_with("application/json"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_signature_covers_the_exact_bytes_sent() {
    // If the signature were computed over anything but the body as sent, every
    // receiver's verification would fail and nobody would be able to say why.
    let (url, seen) = receiver(200).await;
    let mut d = delivery(webhook(&url));
    // Multi-byte UTF-8 on purpose: a signature computed over anything but the
    // bytes on the wire would still pass an ASCII-only test.
    d.payload.body = r#"{"unicode":"café","nested":{"a":[1,2,3]}}"#.as_bytes().to_vec();
    let expected = forge_hooks::sign("s3cret", &d.payload.body);

    deliverer().send(&d).await;

    let seen = seen.lock().unwrap();
    let presented = seen.headers[0]
        .get("x-hub-signature-256")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    check!(presented == expected);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_error_asks_to_be_retried() {
    let (url, _) = receiver(503).await;
    let outcome = deliverer().send(&delivery(webhook(&url))).await;

    check!(
        matches!(outcome, DeliveryOutcome::Retry { .. }),
        "a struggling receiver should be retried, got {outcome:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_error_is_not_retried() {
    // Sending the same request again would produce the same refusal.
    let (url, _) = receiver(404).await;
    let outcome = deliverer().send(&delivery(webhook(&url))).await;

    check!(
        matches!(outcome, DeliveryOutcome::Permanent { .. }),
        "got {outcome:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rate_limiting_is_treated_as_temporary() {
    let (url, _) = receiver(429).await;
    let outcome = deliverer().send(&delivery(webhook(&url))).await;

    check!(
        matches!(outcome, DeliveryOutcome::Retry { .. }),
        "429 means later, not never: {outcome:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_loopback_target_is_refused_before_any_request_is_made() {
    // The receiver here is real and would happily answer; the point is that we
    // never call it, because a webhook pointed at the forge's own network is
    // a request-forgery primitive.
    let (url, seen) = receiver(200).await;
    let outcome = deliverer().send(&delivery(webhook(&url))).await;
    check!(outcome.is_success(), "sanity: the receiver works");

    // Same receiver, addressed by a name that resolves to loopback.
    let port = url.rsplit(':').next().unwrap().trim_end_matches("/hook");
    let by_name = format!("http://localhost:{port}/hook");
    // The default deliverer — the one a public forge runs.
    let refused = Deliverer::new().send(&delivery(webhook(&by_name))).await;

    check!(
        matches!(refused, DeliveryOutcome::Permanent { .. }),
        "got {refused:?}"
    );
    check!(
        seen.lock().unwrap().bodies.len() == 1,
        "the blocked delivery must not have reached the receiver"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreachable_target_is_retried_rather_than_abandoned() {
    // Nothing is listening on this port, which is what a receiver being
    // restarted looks like.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let hook = webhook(&format!("http://127.0.0.1:{port}/hook"));
    let outcome = deliverer().send(&delivery(hook)).await;

    check!(
        matches!(outcome, DeliveryOutcome::Retry { .. }),
        "a connection failure is temporary: {outcome:?}"
    );
}
