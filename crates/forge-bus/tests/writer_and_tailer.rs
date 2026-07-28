//! The event spine, against a real broker.
//!
//! These lock the two properties the whole architecture rests on: at most one
//! writer can commit, and a reader can always replay a topic to its end.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use assert2::{assert, check};
use forge_bus::{Committed, FencedWriter, PendingRecord, Tailer, WriteError};
use forge_events::{RepoEvent, decode_raw};
use forge_testkit::TestBroker;
use forge_types::{RepoId, UserId, Visibility, topics};

fn repo_created(repo_id: RepoId, name: &str) -> RepoEvent {
    RepoEvent::Created {
        repo_id,
        owner_id: UserId::new(),
        owner_name: "octocat".into(),
        name: name.into(),
        full_name_lower: format!("octocat/{name}"),
        description: None,
        default_branch: "main".into(),
        visibility: Visibility::Public,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn events_written_transactionally_are_readable_with_their_cloudevents_headers() {
    let broker = TestBroker::with_forge_topics().await;
    let writer = FencedWriter::connect(&broker.bootstrap()).await.unwrap();

    let repo = RepoId::new();
    let event = repo_created(repo, "hello-world");
    let committed = writer
        .transact(vec![PendingRecord::event(&event, None).unwrap()])
        .await
        .expect("commit");
    check!(committed.offset_for(topics::EVENTS_REPOS).is_some());

    let mut tailer = Tailer::open(&broker.bootstrap(), topics::EVENTS_REPOS)
        .await
        .unwrap();
    let mut seen = Vec::new();
    tailer
        .replay_to_end(|record| seen.push(record.clone()))
        .await
        .unwrap();

    check!(seen.len() == 1);
    let record = &seen[0];

    // Keyed by aggregate id, so a partition split would preserve ordering.
    check!(record.key.as_deref() == Some(repo.to_string().as_bytes()));

    let envelope = decode_raw(record.value.as_deref()).unwrap();
    check!(envelope.event_type == "repo.created");
    check!(envelope.parse::<RepoEvent>().unwrap().payload == event);

    let header = |name: &str| {
        record
            .headers
            .iter()
            .find(|h| h.key == name)
            .and_then(|h| h.value.as_deref())
            .map(|v| String::from_utf8_lossy(v).into_owned())
    };
    check!(header("ce_specversion").as_deref() == Some("1.0"));
    check!(header("ce_type").as_deref() == Some("com.crabforge.repo.created"));
    check!(header("ce_id").as_deref() == Some(envelope.event_id.to_string().as_str()));
    check!(header("content-type").as_deref() == Some("application/json"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_transaction_spanning_topics_commits_atomically() {
    // The property that lets a command append history and update current state
    // without a window where they disagree.
    let broker = TestBroker::with_forge_topics().await;
    let writer = FencedWriter::connect(&broker.bootstrap()).await.unwrap();

    let repo = RepoId::new();
    let committed = writer
        .transact(vec![
            PendingRecord::event(&repo_created(repo, "atomic"), None).unwrap(),
            PendingRecord::state(topics::META_CATALOG, "repo:octocat/atomic", &repo).unwrap(),
        ])
        .await
        .expect("commit");

    check!(committed.offset_for(topics::EVENTS_REPOS).is_some());
    check!(committed.offset_for(topics::META_CATALOG).is_some());

    for topic in [topics::EVENTS_REPOS, topics::META_CATALOG] {
        let mut tailer = Tailer::open(&broker.bootstrap(), topic).await.unwrap();
        let mut count = 0;
        tailer.replay_to_end(|_| count += 1).await.unwrap();
        check!(count == 1, "{topic} should hold exactly one record");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_newer_writer_fences_the_older_one() {
    // The core safety property: during a rolling restart or a partition where
    // the old process is still alive, only one writer can commit.
    let broker = TestBroker::with_forge_topics().await;

    let elder = FencedWriter::connect(&broker.bootstrap()).await.unwrap();
    elder
        .transact(vec![
            PendingRecord::event(&repo_created(RepoId::new(), "before"), None).unwrap(),
        ])
        .await
        .expect("elder writes before being fenced");

    // Taking the same transactional id bumps the producer epoch.
    let younger = FencedWriter::connect(&broker.bootstrap()).await.unwrap();

    let result = elder
        .transact(vec![
            PendingRecord::event(&repo_created(RepoId::new(), "after"), None).unwrap(),
        ])
        .await;
    assert!(let Err(WriteError::Fenced) = result);
    check!(elder.is_fenced(), "fenced state must latch");

    // And it stays fenced — no retry can revive it.
    let retried = elder
        .transact(vec![
            PendingRecord::event(&repo_created(RepoId::new(), "again"), None).unwrap(),
        ])
        .await;
    assert!(let Err(WriteError::Fenced) = retried);

    younger
        .transact(vec![
            PendingRecord::event(&repo_created(RepoId::new(), "survivor"), None).unwrap(),
        ])
        .await
        .expect("the younger writer owns the id now");

    let mut tailer = Tailer::open(&broker.bootstrap(), topics::EVENTS_REPOS)
        .await
        .unwrap();
    let mut names = Vec::new();
    tailer
        .replay_to_end(|record| {
            let envelope = decode_raw(record.value.as_deref()).unwrap();
            if let Ok(parsed) = envelope.parse::<RepoEvent>()
                && let RepoEvent::Created { name, .. } = parsed.payload
            {
                names.push(name);
            }
        })
        .await
        .unwrap();
    check!(names == vec!["before".to_string(), "survivor".to_string()]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_cursor_advances_past_invisible_records() {
    // Every forge write is transactional, so the log is full of control
    // batches. A cursor derived from `records.last().offset + 1` stalls on
    // them; this asserts we use `next_offset` instead.
    let broker = TestBroker::with_forge_topics().await;
    let writer = FencedWriter::connect(&broker.bootstrap()).await.unwrap();

    for i in 0..5 {
        writer
            .transact(vec![
                PendingRecord::event(&repo_created(RepoId::new(), &format!("repo-{i}")), None)
                    .unwrap(),
            ])
            .await
            .unwrap();
    }

    let mut tailer = Tailer::open(&broker.bootstrap(), topics::EVENTS_REPOS)
        .await
        .unwrap();
    let mut count = 0;
    tailer.replay_to_end(|_| count += 1).await.unwrap();

    check!(count == 5);
    // Five records in five transactions leaves the cursor past the commit
    // markers, not at 5.
    check!(
        tailer.offset() > 5,
        "cursor {} should have stepped over commit markers",
        tailer.offset()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tailer_resumes_from_a_saved_cursor() {
    // How the projector restarts: its offset lives in gres, not the broker.
    let broker = TestBroker::with_forge_topics().await;
    let writer = FencedWriter::connect(&broker.bootstrap()).await.unwrap();

    for i in 0..3 {
        writer
            .transact(vec![
                PendingRecord::event(&repo_created(RepoId::new(), &format!("r{i}")), None).unwrap(),
            ])
            .await
            .unwrap();
    }

    let mut first = Tailer::open(&broker.bootstrap(), topics::EVENTS_REPOS)
        .await
        .unwrap();
    let batch = first.next_batch(0).await.unwrap();
    check!(!batch.records.is_empty());
    let saved = first.offset();

    let mut resumed = Tailer::open_at(&broker.bootstrap(), topics::EVENTS_REPOS, saved)
        .await
        .unwrap();
    let mut after = 0;
    resumed.replay_to_end(|_| after += 1).await.unwrap();

    check!(
        after + batch.records.len() == 3,
        "resuming must not skip or repeat records"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn applied_offsets_are_published_for_read_your_writes() {
    let broker = TestBroker::with_forge_topics().await;
    let writer = FencedWriter::connect(&broker.bootstrap()).await.unwrap();

    let committed: Committed = writer
        .transact(vec![
            PendingRecord::event(&repo_created(RepoId::new(), "watched"), None).unwrap(),
        ])
        .await
        .unwrap();
    let target = committed.offset_for(topics::EVENTS_REPOS).unwrap();

    let mut tailer = Tailer::open(&broker.bootstrap(), topics::EVENTS_REPOS)
        .await
        .unwrap();
    let mut applied = tailer.applied();
    check!(*applied.borrow() < target, "nothing applied yet");

    tailer.replay_to_end(|_| {}).await.unwrap();

    // This is what an HTTP handler awaits before reading its own write back.
    applied.changed().await.unwrap();
    check!(*applied.borrow() >= target);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_transaction_is_a_no_op() {
    let broker = TestBroker::with_forge_topics().await;
    let writer = FencedWriter::connect(&broker.bootstrap()).await.unwrap();

    let committed = writer.transact(Vec::new()).await.unwrap();
    check!(committed.offsets.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tombstones_are_written_for_compacted_deletes() {
    let broker = TestBroker::with_forge_topics().await;
    let writer = FencedWriter::connect(&broker.bootstrap()).await.unwrap();

    writer
        .transact(vec![
            PendingRecord::state(topics::META_CATALOG, "user:octocat", &"claimed").unwrap(),
        ])
        .await
        .unwrap();
    writer
        .transact(vec![PendingRecord::tombstone(
            topics::META_CATALOG,
            "user:octocat",
        )])
        .await
        .unwrap();

    let mut tailer = Tailer::open(&broker.bootstrap(), topics::META_CATALOG)
        .await
        .unwrap();
    let mut values = Vec::new();
    tailer
        .replay_to_end(|record| values.push(record.value.clone()))
        .await
        .unwrap();

    check!(values.len() == 2);
    check!(values[1].is_none(), "a tombstone carries no value");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn writers_survive_being_reconnected() {
    // Restarting the command service must not require any log surgery: it
    // reconnects, fences itself, and continues.
    let broker = TestBroker::with_forge_topics().await;

    let first = FencedWriter::connect(&broker.bootstrap()).await.unwrap();
    first
        .transact(vec![
            PendingRecord::event(&repo_created(RepoId::new(), "one"), None).unwrap(),
        ])
        .await
        .unwrap();
    drop(first);

    let second = FencedWriter::connect(&broker.bootstrap()).await.unwrap();
    second
        .transact(vec![
            PendingRecord::event(&repo_created(RepoId::new(), "two"), None).unwrap(),
        ])
        .await
        .unwrap();

    let mut tailer = Tailer::open(&broker.bootstrap(), topics::EVENTS_REPOS)
        .await
        .unwrap();
    let mut count = 0;
    tailer.replay_to_end(|_| count += 1).await.unwrap();
    check!(count == 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_indeterminate_handler_is_installable_for_tests() {
    // Production aborts the process on an unknown transaction outcome. This
    // only checks the seam exists, so the abort can be exercised without
    // taking a test runner down with it.
    let broker = TestBroker::with_forge_topics().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);

    let writer = FencedWriter::connect(&broker.bootstrap())
        .await
        .unwrap()
        .with_indeterminate_handler(Arc::new(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
        }));

    writer
        .transact(vec![
            PendingRecord::event(&repo_created(RepoId::new(), "fine"), None).unwrap(),
        ])
        .await
        .unwrap();

    check!(
        calls.load(Ordering::SeqCst) == 0,
        "a healthy commit is never indeterminate"
    );
}
