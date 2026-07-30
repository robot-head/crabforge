//! The work queue itself, against a real broker.
//!
//! The pipeline test drives a runner end to end but needs a gres, so it skips
//! on a machine without one — and the share group is the piece of Crab Actions
//! with the most broker-specific behaviour and the least ability to be reasoned
//! about from the client side. These tests need only a broker.
//!
//! They also pin down a deployment claim. Share groups are gated by the KIP-932
//! `share.version` feature, which is settable only at `crabka format` time, so
//! everything the forge documents tells an operator to pass
//! `--feature share.version=1`. Whether the broker *enforces* that gate is a
//! separate question from whether the flag should be set, and the answer
//! decides whether a runner may refuse to start.

use std::time::Duration;

use assert2::check;
use forge_bus::{BrokerFeatures, FencedWriter, PendingRecord, WEBHOOK_TRANSACTIONAL_ID};
use forge_ci::{Disposition, JobQueue, PlannedJob, QueuedJob, Step};
use forge_testkit::TestBroker;
use forge_types::topics;

/// Long enough for a share-group poll to complete a join on a cold group.
const POLL: Duration = Duration::from_secs(10);

fn job(id: &str) -> QueuedJob {
    QueuedJob {
        job_id: id.to_string(),
        run_id: "run-1".into(),
        repo_id: "repo-1".into(),
        head_oid: "0".repeat(40),
        job: PlannedJob {
            name: "test".into(),
            image: "ubuntu:24.04".into(),
            timeout_minutes: 5,
            env: Vec::new(),
            steps: vec![Step {
                name: None,
                run: "true".into(),
            }],
        },
    }
}

/// Put `jobs` on the queue topic the way the orchestrator does.
async fn enqueue(bootstrap: &str, jobs: &[QueuedJob]) {
    let writer = FencedWriter::connect_with_id(bootstrap, WEBHOOK_TRANSACTIONAL_ID)
        .await
        .unwrap();
    let records = jobs
        .iter()
        .map(|j| PendingRecord::state(topics::CI_JOBS, j.job_id.clone(), j).unwrap())
        .collect();
    writer.transact(records).await.unwrap();
}

/// Take one job, or `None` if the queue stayed empty for `POLL`.
async fn take(queue: &mut JobQueue) -> Option<forge_ci::Lease> {
    for _ in 0..5 {
        if let Some(lease) = queue.next(POLL).await.expect("polling the queue") {
            return Some(lease);
        }
    }
    None
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_queued_job_reaches_a_runner() {
    let broker = TestBroker::with_forge_topics().await;
    enqueue(&broker.bootstrap(), &[job("j1")]).await;

    let mut queue = JobQueue::open(&broker.bootstrap()).await.unwrap();
    let lease = take(&mut queue).await.expect("no job was delivered");

    check!(lease.job.job_id == "j1");
    check!(lease.attempt() == 1, "the first delivery is attempt 1");
    queue.settle(lease, Disposition::Done).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_released_job_is_delivered_again() {
    // What makes at-least-once true, and therefore what makes the CAS in
    // `claim_job` load-bearing. A runner that cannot take a job — no capacity,
    // no matching image — must put it back rather than drop it.
    let broker = TestBroker::with_forge_topics().await;
    enqueue(&broker.bootstrap(), &[job("j2")]).await;

    let mut queue = JobQueue::open(&broker.bootstrap()).await.unwrap();
    let first = take(&mut queue).await.expect("no first delivery");
    queue.settle(first, Disposition::Release).await.unwrap();

    let again = take(&mut queue)
        .await
        .expect("a released job was not redelivered");
    check!(again.job.job_id == "j2");
    check!(
        again.attempt() > 1,
        "a redelivery should count as a later attempt, got {}",
        again.attempt()
    );
    queue.settle(again, Disposition::Done).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_accepted_job_is_not_delivered_twice() {
    let broker = TestBroker::with_forge_topics().await;
    enqueue(&broker.bootstrap(), &[job("j3")]).await;

    let mut queue = JobQueue::open(&broker.bootstrap()).await.unwrap();
    let lease = take(&mut queue).await.expect("no delivery");
    queue.settle(lease, Disposition::Done).await.unwrap();

    // One poll, not the retry loop: the assertion is that nothing arrives, and
    // looping five times only makes a passing test slow.
    let again = queue.next(POLL).await.unwrap();
    check!(
        again.is_none(),
        "an accepted job came back: {:?}",
        again.map(|l| l.job.job_id)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_runners_never_receive_the_same_job() {
    // Scaling out is "start another runner", so the queue has to tolerate two
    // of them on the same group without handing the same work to both — a
    // duplicate here means someone's tests run twice and the second answer
    // overwrites the first.
    //
    // What this deliberately does not assert is that both runners get some of
    // the work. The queue topic has sixteen partitions and the group assigns
    // them, so which member sees a given job is a hash; requiring a particular
    // split would be asserting on that hash.
    let broker = TestBroker::with_forge_topics().await;
    let jobs: Vec<_> = (0..8).map(|i| job(&format!("split-{i}"))).collect();

    let mut a = JobQueue::open(&broker.bootstrap()).await.unwrap();
    let mut b = JobQueue::open(&broker.bootstrap()).await.unwrap();
    enqueue(&broker.bootstrap(), &jobs).await;

    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(40);
    while seen.len() < jobs.len() && tokio::time::Instant::now() < deadline {
        for queue in [&mut a, &mut b] {
            if let Some(lease) = queue.next(Duration::from_secs(2)).await.unwrap() {
                seen.push(lease.job.job_id.clone());
                queue.settle(lease, Disposition::Done).await.unwrap();
            }
        }
    }

    let mut unique = seen.clone();
    unique.sort();
    unique.dedup();
    check!(
        unique.len() == seen.len(),
        "a job was delivered to both runners: {seen:?}"
    );
    check!(
        unique.len() == jobs.len(),
        "only {} of {} jobs were delivered: {unique:?}",
        unique.len(),
        jobs.len()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_burst_of_jobs_is_drained_without_waiting_out_a_lock_timeout() {
    // A `ShareFetch` acquires up to 500 records at once and there is no way to
    // ask for fewer, so a runner that reads one and discards the rest of the
    // batch leaves them locked to itself until the broker's 30-second lock
    // expires — and each expiry burns one of the five delivery attempts before
    // the record is archived unrun. Six jobs pushed together were enough to
    // lose one.
    //
    // The budget is what makes this a test rather than a description: five jobs
    // that each take a lock timeout to reach a runner cannot finish inside it.
    let broker = TestBroker::with_forge_topics().await;
    let jobs: Vec<_> = (0..5).map(|i| job(&format!("burst-{i}"))).collect();
    enqueue(&broker.bootstrap(), &jobs).await;

    let mut queue = JobQueue::open(&broker.bootstrap()).await.unwrap();
    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(25);
    while seen.len() < jobs.len() && tokio::time::Instant::now() < deadline {
        let Some(lease) = queue.next(POLL).await.unwrap() else {
            continue;
        };
        seen.push(lease.job.job_id.clone());
        queue.settle(lease, Disposition::Done).await.unwrap();
    }

    seen.sort();
    seen.dedup();
    check!(
        seen.len() == jobs.len(),
        "only {} of {} jobs were delivered: {seen:?}",
        seen.len(),
        jobs.len()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn share_groups_are_served_even_though_share_version_is_not_finalized() {
    // Recorded as a test because it contradicts what this repository documents
    // and what crabka's own CLI implies. `crabka format --feature
    // share.version=1` is the documented prerequisite for the CI queue, but the
    // broker's share-group handlers do not consult the finalized level the way
    // `consumer_group_heartbeat` consults `group.version` — so a broker without
    // the feature serves the queue anyway.
    //
    // Two things follow. A runner must not refuse to start on `share.version=0`,
    // because it would be refusing a broker that works. And this test fails the
    // day crabka starts enforcing the gate, which is the notice needed to make
    // the flag mandatory rather than advisory.
    let broker = TestBroker::with_forge_topics().await;

    let features = BrokerFeatures::probe(&broker.bootstrap()).await.unwrap();
    check!(
        !features.share_groups(),
        "this fixture is meant to have share.version unset; the finding below \
         no longer needs recording if it is now finalized by default"
    );

    enqueue(&broker.bootstrap(), &[job("ungated")]).await;
    let mut queue = JobQueue::open(&broker.bootstrap()).await.unwrap();
    let lease = take(&mut queue).await;
    check!(
        lease.is_some(),
        "crabka now enforces share.version — the runner may (and should) refuse \
         to start against a broker without it, and docs/gres-gaps.md's sibling \
         note in docs/PLAN.md should be updated"
    );
    if let Some(lease) = lease {
        queue.settle(lease, Disposition::Done).await.unwrap();
    }
}
