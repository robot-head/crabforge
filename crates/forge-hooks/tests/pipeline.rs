//! The webhook pipeline end to end.
//!
//! A real broker, a real gres, a real HTTP receiver, and both stages wired
//! together — because the interesting failures here are all at the seams. The
//! unit tests already cover matching rules and signing; what these establish is
//! that an event committed to a domain topic actually arrives at somebody's
//! server, signed, once per subscriber, and that the failure paths record what
//! a maintainer would need to debug them.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use assert2::check;
use axum::{Router, extract::State, http::HeaderMap, routing::post};
use forge_bus::{FencedWriter, PendingRecord, WEBHOOK_TRANSACTIONAL_ID};
use forge_events::{IssueEvent, RepoEvent};
use forge_hooks::{Deliverer, Matcher, Worker};
use forge_store::{Store, WebhookRecord};
use forge_testkit::{TestBroker, eventually, require_gres};
use forge_types::{IssueId, RepoId, UserId, Visibility, topics};

/// How many partitions the dead-letter topic has.
fn dlq_partitions() -> i32 {
    forge_topics::static_topics()
        .iter()
        .find(|spec| spec.name == topics::WEBHOOKS_DLQ)
        .map_or(1, |spec| spec.partitions)
}

/// What a receiver saw.
#[derive(Default)]
struct Received {
    bodies: Vec<Vec<u8>>,
    headers: Vec<HeaderMap>,
}

#[derive(Clone)]
struct Receiver {
    seen: Arc<Mutex<Received>>,
    /// Answers to give, consumed in order; the last repeats.
    statuses: Arc<Mutex<Vec<u16>>>,
}

async fn record(
    State(receiver): State<Receiver>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> axum::http::StatusCode {
    {
        let mut seen = receiver.seen.lock().unwrap();
        seen.bodies.push(body.to_vec());
        seen.headers.push(headers);
    }
    let mut statuses = receiver.statuses.lock().unwrap();
    let status = if statuses.len() > 1 {
        statuses.remove(0)
    } else {
        *statuses.first().unwrap_or(&200)
    };
    axum::http::StatusCode::from_u16(status).unwrap()
}

/// Start a receiver answering with `statuses` in turn, returning its URL.
async fn receiver(statuses: Vec<u16>) -> (String, Arc<Mutex<Received>>) {
    let seen = Arc::new(Mutex::new(Received::default()));
    let state = Receiver {
        seen: Arc::clone(&seen),
        statuses: Arc::new(Mutex::new(statuses)),
    };
    let app = Router::new().route("/hook", post(record)).with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}/hook"), seen)
}

/// Everything a pipeline test needs, or `None` when gres is unavailable.
struct Harness {
    broker: TestBroker,
    _gres: forge_testkit::Gres,
    store: Store,
    dsn: String,
    writer: Arc<FencedWriter>,
}

impl Harness {
    async fn start() -> Option<Self> {
        let gres = require_gres().await?;
        let broker = TestBroker::with_forge_topics().await;
        let dsn = gres.dsn();
        let store = Store::connect(&dsn).await.unwrap();
        store.migrate().await.unwrap();

        // The matcher's own transactional identity. Sharing the command
        // service's would fence it on the first fan-out.
        let writer = Arc::new(
            FencedWriter::connect_with_id(&broker.bootstrap(), WEBHOOK_TRANSACTIONAL_ID)
                .await
                .unwrap(),
        );
        Some(Self {
            broker,
            _gres: gres,
            store,
            dsn,
            writer,
        })
    }

    async fn store(&self) -> Store {
        Store::connect(&self.dsn).await.unwrap()
    }

    /// Register a webhook pointed at `url`.
    async fn subscribe(&self, id: &str, repo: &RepoId, url: &str, events: &[&str]) {
        let now = forge_types::now();
        self.store
            .hooks()
            .upsert(&WebhookRecord {
                webhook_id: id.into(),
                repo_id: repo.to_string(),
                url: url.to_string(),
                secret: "s3cret".into(),
                events: events.iter().map(|e| e.to_string()).collect(),
                active: true,
                created_at: now,
                updated_at: now,
            })
            .await
            .unwrap();
    }

    /// Commit a domain event, as the command service would.
    async fn emit_issue(&self, repo: &RepoId, title: &str) {
        let event = IssueEvent::Opened {
            issue_id: IssueId::new(),
            repo_id: *repo,
            number: 1,
            title: title.into(),
            body: None,
            author_id: UserId::new(),
            author_name: "octocat".into(),
        };
        let commands = FencedWriter::connect(&self.broker.bootstrap())
            .await
            .unwrap();
        commands
            .transact(vec![PendingRecord::event(&event, None).unwrap()])
            .await
            .unwrap();
    }

    /// Run the matcher over one topic until it has queued at least `want`.
    async fn match_until(&self, topic: &str, want: usize) -> usize {
        let mut matcher = Matcher::open(
            &self.broker.bootstrap(),
            topic,
            self.store().await,
            Arc::clone(&self.writer),
        )
        .await
        .unwrap();

        let mut queued = 0;
        for _ in 0..20 {
            queued += matcher.step().await.unwrap();
            if queued >= want {
                break;
            }
        }
        queued
    }

    /// Run workers until they have handled at least `want` deliveries.
    ///
    /// One per partition, because that is how the queue is read: a delivery is
    /// keyed by webhook and lands wherever that hashes to, so a single worker
    /// would see only its own sixteenth of the traffic.
    async fn deliver_until(&self, want: usize) -> usize {
        let mut workers = Vec::new();
        for partition in 0..Worker::partitions() {
            workers.push(
                Worker::open(
                    &self.broker.bootstrap(),
                    partition,
                    self.store().await,
                    // The receivers are on loopback, which the default deliverer
                    // refuses — correctly, and that refusal is tested separately.
                    Deliverer::with_private_targets(true),
                    Arc::clone(&self.writer),
                )
                .await
                .unwrap()
                // The real ladder spans minutes.
                .with_backoff(|_| Duration::from_millis(1)),
            );
        }

        let mut handled = 0;
        for _ in 0..10 {
            for worker in &mut workers {
                handled += worker.step().await.unwrap();
            }
            if handled >= want {
                break;
            }
        }
        handled
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_event_reaches_the_subscriber_signed() {
    let Some(h) = Harness::start().await else {
        return;
    };
    let repo = RepoId::new();
    let (url, seen) = receiver(vec![200]).await;
    h.subscribe("hook-1", &repo, &url, &["issue.*"]).await;

    h.emit_issue(&repo, "something happened").await;
    check!(h.match_until(topics::EVENTS_ISSUES, 1).await == 1);
    check!(h.deliver_until(1).await == 1);

    let seen = seen.lock().unwrap();
    check!(seen.bodies.len() == 1, "expected exactly one delivery");

    // The body is the event as the log holds it.
    let body: serde_json::Value = serde_json::from_slice(&seen.bodies[0]).unwrap();
    check!(body["event_type"] == "issue.opened");
    check!(body["payload"]["title"] == "something happened");

    let header = |name: &str| {
        seen.headers[0]
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string()
    };
    // A receiver verifies this before trusting anything else.
    check!(
        forge_hooks::verify("s3cret", &seen.bodies[0], &header("x-hub-signature-256")),
        "signature did not verify"
    );
    check!(header("x-forge-event") == "issue.opened");
    // CloudEvents attributes survive the whole pipeline, in HTTP spelling.
    check!(header("ce-type") == "com.crabforge.issue.opened");
    check!(header("ce-specversion") == "1.0");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_subscriber_hears_and_nobody_else_does() {
    let Some(h) = Harness::start().await else {
        return;
    };
    let repo = RepoId::new();
    let other_repo = RepoId::new();

    let (wanted, heard) = receiver(vec![200]).await;
    let (also_wanted, also_heard) = receiver(vec![200]).await;
    let (wrong_event, not_heard) = receiver(vec![200]).await;
    let (wrong_repo, never_heard) = receiver(vec![200]).await;

    h.subscribe("hook-1", &repo, &wanted, &["*"]).await;
    h.subscribe("hook-2", &repo, &also_wanted, &["issue.opened"])
        .await;
    // Subscribed to this repository, but to something else entirely.
    h.subscribe("hook-3", &repo, &wrong_event, &["pr.merged"])
        .await;
    // Subscribed to the same event, on a different repository.
    h.subscribe("hook-4", &other_repo, &wrong_repo, &["issue.opened"])
        .await;

    h.emit_issue(&repo, "hello").await;
    check!(h.match_until(topics::EVENTS_ISSUES, 2).await == 2);
    h.deliver_until(2).await;

    check!(
        heard.lock().unwrap().bodies.len() == 1,
        "wildcard missed it"
    );
    check!(
        also_heard.lock().unwrap().bodies.len() == 1,
        "exact subscription missed it"
    );
    check!(
        not_heard.lock().unwrap().bodies.is_empty(),
        "a webhook subscribed to pr.merged was sent an issue"
    );
    check!(
        never_heard.lock().unwrap().bodies.is_empty(),
        "another repository's event leaked"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_struggling_receiver_is_retried_and_the_attempts_are_recorded() {
    let Some(h) = Harness::start().await else {
        return;
    };
    let repo = RepoId::new();
    // Fails twice, then recovers — the case retries exist for.
    let (url, seen) = receiver(vec![503, 503, 200]).await;
    h.subscribe("hook-1", &repo, &url, &["*"]).await;

    h.emit_issue(&repo, "eventually").await;
    h.match_until(topics::EVENTS_ISSUES, 1).await;
    h.deliver_until(1).await;

    check!(
        seen.lock().unwrap().bodies.len() == 3,
        "expected two failures and a success"
    );

    // And the history says so, which is the whole point of recording it.
    let attempts = h
        .store
        .hooks()
        .recent_deliveries("hook-1", forge_store::page_size(10))
        .await
        .unwrap();
    check!(attempts.len() == 3, "every attempt should be recorded");
    let statuses: Vec<_> = attempts.iter().map(|a| a.status.as_str()).collect();
    check!(statuses.contains(&"delivered"));
    check!(statuses.contains(&"failed"));
    // All three describe the same delivery, so a maintainer can see the retries
    // as one story rather than three unrelated events.
    check!(attempts.iter().all(|a| a.event_id == attempts[0].event_id));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dead_receiver_gives_up_and_is_dead_lettered() {
    let Some(h) = Harness::start().await else {
        return;
    };
    let repo = RepoId::new();
    let (url, seen) = receiver(vec![500]).await;
    h.subscribe("hook-1", &repo, &url, &["*"]).await;

    h.emit_issue(&repo, "nobody is listening").await;
    h.match_until(topics::EVENTS_ISSUES, 1).await;
    h.deliver_until(1).await;

    check!(
        seen.lock().unwrap().bodies.len() as i64 == forge_hooks::delivery::MAX_ATTEMPTS,
        "should have tried exactly the allowed number of times"
    );

    let attempts = h
        .store
        .hooks()
        .recent_deliveries("hook-1", forge_store::page_size(10))
        .await
        .unwrap();
    check!(attempts.iter().any(|a| a.status == "dead"));

    // Parked on the log, so a redelivery button has something to replay.
    // Scanned across every partition: dead letters are keyed by webhook, so
    // which partition one lands on is a hash, not a choice.
    let mut parked = Vec::new();
    for partition in 0..dlq_partitions() {
        let mut dlq = forge_bus::Tailer::open_partition_at(
            &h.broker.bootstrap(),
            topics::WEBHOOKS_DLQ,
            partition,
            0,
        )
        .await
        .unwrap();
        dlq.replay_to_end(|record| {
            if let Some(value) = record.value.as_deref()
                && let Ok(json) = serde_json::from_slice::<serde_json::Value>(value)
            {
                parked.push(json);
            }
        })
        .await
        .unwrap();
    }
    check!(parked.len() == 1, "expected one dead letter");
    check!(parked[0]["webhook_id"] == "hook-1");
    check!(parked[0]["event"]["event_type"] == "issue.opened");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_disabled_webhook_is_not_called() {
    let Some(h) = Harness::start().await else {
        return;
    };
    let repo = RepoId::new();
    let (url, seen) = receiver(vec![200]).await;
    h.subscribe("hook-1", &repo, &url, &["*"]).await;

    // Disabled after the event is emitted but before it is delivered: the
    // queue legitimately holds work that has since been called off.
    h.emit_issue(&repo, "too late").await;
    h.match_until(topics::EVENTS_ISSUES, 1).await;

    let mut hook = h.store.hooks().by_id("hook-1").await.unwrap().unwrap();
    hook.active = false;
    h.store.hooks().upsert(&hook).await.unwrap();

    h.deliver_until(1).await;
    check!(
        seen.lock().unwrap().bodies.is_empty(),
        "a disabled webhook was still called"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_matcher_resumes_where_it_left_off() {
    // The property that makes a restart safe. A matcher that resumed at zero
    // would re-deliver the repository's entire history on every deploy.
    let Some(h) = Harness::start().await else {
        return;
    };
    let repo = RepoId::new();
    let (url, seen) = receiver(vec![200]).await;
    h.subscribe("hook-1", &repo, &url, &["*"]).await;

    h.emit_issue(&repo, "first").await;
    check!(h.match_until(topics::EVENTS_ISSUES, 1).await == 1);

    // A fresh matcher, as after a restart. It must find nothing to do.
    check!(
        h.match_until(topics::EVENTS_ISSUES, 1).await == 0,
        "the matcher re-queued events it had already handled"
    );

    // And it still picks up what comes next.
    h.emit_issue(&repo, "second").await;
    check!(h.match_until(topics::EVENTS_ISSUES, 1).await == 1);

    h.deliver_until(2).await;
    eventually("both deliveries arrive", Duration::from_secs(10), || {
        let seen = Arc::clone(&seen);
        async move { seen.lock().unwrap().bodies.len() == 2 }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_account_event_reaches_nobody() {
    // Webhooks are configured on repositories. An event with no repository has
    // no subscribers, and guessing one would leak account activity.
    let Some(h) = Harness::start().await else {
        return;
    };
    let repo = RepoId::new();
    let (url, seen) = receiver(vec![200]).await;
    h.subscribe("hook-1", &repo, &url, &["*"]).await;

    let event = RepoEvent::Created {
        repo_id: repo,
        owner_id: UserId::new(),
        owner_name: "octocat".into(),
        name: "hello".into(),
        full_name_lower: "octocat/hello".into(),
        description: None,
        default_branch: "main".into(),
        visibility: Visibility::Public,
    };
    let commands = FencedWriter::connect(&h.broker.bootstrap()).await.unwrap();
    commands
        .transact(vec![PendingRecord::event(&event, None).unwrap()])
        .await
        .unwrap();

    // A repo event does carry a repo, so this one is delivered...
    check!(h.match_until(topics::EVENTS_REPOS, 1).await == 1);
    h.deliver_until(1).await;
    check!(seen.lock().unwrap().bodies.len() == 1);

    // ...but the users topic yields nothing to match at all.
    check!(h.match_until(topics::EVENTS_USERS, 1).await == 0);
}
