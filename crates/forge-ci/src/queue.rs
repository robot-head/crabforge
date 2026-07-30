//! Handing a job to a runner.
//!
//! Share groups (KIP-932). Several runners pull from one queue, each job goes
//! to exactly one of them, and anything a runner drops is redelivered. That is
//! the shape a work queue wants and the reason this is not an ordinary consumer
//! group: consumer groups partition *ownership*, so adding a runner would only
//! help if there were a spare partition, and a slow job would block every other
//! job that hashed to the same one.
//!
//! One deployment prerequisite comes with it. Share groups must be enabled at
//! broker *format* time — `crabka format --feature share.version=1` — and a
//! broker formatted without it cannot be reconfigured, only reformatted. The
//! dev-loop recipe passes the flag and `TestBroker` enables it, but a
//! hand-formatted broker will refuse to serve the queue, which is what
//! [`QueueError::Unsupported`] is for: a clear message beats a runner that
//! silently never receives work.
//!
//! Delivery is at-least-once by construction — a runner that dies mid-job has
//! its job redelivered — so nothing here tries to be exactly-once. The
//! compare-and-swap in `CiStore::claim_job` is what makes that safe.
//!
//! ## Why a poll's whole batch is kept
//!
//! A runner executes one job at a time, but a `ShareFetch` returns up to 500
//! records and every one of them is *acquired* by this consumer the moment it
//! arrives — crabka's client hard-codes that limit, so there is no asking for
//! one. Returning the first record and dropping the rest of the batch on the
//! floor does not un-acquire them: they sit locked for the broker's 30-second
//! `record_lock_duration`, then time out, and a timed-out acquisition counts as
//! a delivery. At the default `max_delivery_attempts` of 5 a job pushed in a
//! burst of six can exhaust its attempts and be archived by the broker without
//! ever having been run, which the orchestrator's sweep then records as an
//! infrastructure failure. So the batch is kept and drained one lease at a
//! time.

use std::{collections::VecDeque, time::Duration};

use crabka_client_consumer::{
    ConsumerError, ShareAckMode, ShareAckType, ShareConsumer, ShareConsumerRecord,
};
use serde::{Deserialize, Serialize};

use crate::plan::PlannedJob;

/// The share group every runner joins.
pub const RUNNER_GROUP: &str = "forge.ci.runners";

#[derive(Debug, thiserror::Error)]
pub enum QueueError {
    #[error("the job queue is unavailable: {0}")]
    Consumer(#[from] ConsumerError),
    #[error(
        "this broker does not serve share groups; it must be formatted with \
         `--feature share.version=1` (a reformat, not a config change)"
    )]
    Unsupported,
}

/// A job as it travels to a runner.
///
/// Self-contained: everything needed to run it, so a runner needs no database
/// to start and no second round trip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedJob {
    pub job_id: String,
    pub run_id: String,
    pub repo_id: String,
    /// The commit to check out and run against.
    pub head_oid: String,
    pub job: PlannedJob,
}

/// How a runner reports back on a job it was given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// Ran it, for whatever outcome. Do not deliver it again.
    ///
    /// "Whatever outcome" includes a failed build: the job was executed and its
    /// verdict recorded, so redelivering would run someone's tests twice and
    /// overwrite the first answer.
    Done,
    /// Could not take it. Give it to someone else.
    Release,
    /// Will never be runnable. Do not deliver it to anyone.
    ///
    /// For a record that cannot even be decoded — redelivering it forever would
    /// wedge the queue behind one bad message.
    Reject,
}

/// A job handed to this runner, and the right to acknowledge it.
pub struct Lease {
    pub job: QueuedJob,
    /// How many times this job has been delivered, share-group counted. The
    /// first delivery is 1, which is what a job's `attempt` is set from.
    pub delivery: i64,
    record: ShareConsumerRecord,
}

impl Lease {
    pub fn attempt(&self) -> i64 {
        self.delivery
    }
}

/// Runners' view of the queue.
pub struct JobQueue {
    consumer: ShareConsumer,
    /// Records acquired by the last poll and not yet handed out. See the
    /// module docs: these are already locked to this consumer whether or not
    /// anyone asks for them.
    acquired: VecDeque<ShareConsumerRecord>,
}

impl JobQueue {
    /// Join the runner share group.
    pub async fn open(bootstrap: &str) -> Result<Self, QueueError> {
        let consumer = ShareConsumer::builder()
            .bootstrap(bootstrap)
            .group_id(RUNNER_GROUP)
            .subscribe([forge_types::topics::CI_JOBS.to_string()])
            // Explicit, so an un-acknowledged job returns to the queue rather
            // than being accepted by the next poll. A runner that crashes
            // mid-job must not have that job counted as done.
            .ack_mode(ShareAckMode::Explicit)
            .build()
            .await?;
        Ok(Self {
            consumer,
            acquired: VecDeque::new(),
        })
    }

    /// Wait for a job, or `None` if none arrived.
    ///
    /// Hands out one lease at a time from whatever the last poll acquired, and
    /// only polls again once that is exhausted.
    ///
    /// A record that will not decode is rejected rather than released: it can
    /// never become runnable, and releasing it would put it at the head of the
    /// queue forever.
    pub async fn next(&mut self, wait: Duration) -> Result<Option<Lease>, QueueError> {
        if self.acquired.is_empty() {
            self.acquired.extend(self.consumer.poll(wait).await?);
        }

        while let Some(record) = self.acquired.pop_front() {
            let decoded = record
                .value
                .as_deref()
                .and_then(|bytes| serde_json::from_slice::<QueuedJob>(bytes).ok());
            match decoded {
                Some(job) => {
                    let delivery = i64::from(record.delivery_count.max(1));
                    return Ok(Some(Lease {
                        job,
                        delivery,
                        record,
                    }));
                }
                None => {
                    tracing::warn!(
                        offset = record.offset,
                        "rejecting a job record that cannot be decoded"
                    );
                    self.consumer.acknowledge(&record, ShareAckType::Reject)?;
                }
            }
        }
        self.consumer.commit().await?;
        Ok(None)
    }

    /// How many acquired jobs are waiting behind the one just handed out.
    ///
    /// Exposed for the runner's queue-depth gauge: this consumer's share of the
    /// backlog is the only part of it visible from here, since a share group
    /// does not tell a member what else is unassigned.
    #[must_use]
    pub fn buffered(&self) -> usize {
        self.acquired.len()
    }

    /// Say how a leased job went.
    pub async fn settle(
        &mut self,
        lease: Lease,
        disposition: Disposition,
    ) -> Result<(), QueueError> {
        let ack = match disposition {
            Disposition::Done => ShareAckType::Accept,
            Disposition::Release => ShareAckType::Release,
            Disposition::Reject => ShareAckType::Reject,
        };
        self.consumer.acknowledge(&lease.record, ack)?;
        self.consumer.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;
    use crate::workflow::Step;

    #[test]
    fn a_queued_job_survives_the_queue_topic() {
        let job = QueuedJob {
            job_id: "j1".into(),
            run_id: "r1".into(),
            repo_id: "repo1".into(),
            head_oid: "abc123".into(),
            job: PlannedJob {
                name: "test".into(),
                image: "rust:1.97".into(),
                timeout_minutes: 30,
                env: vec![("K".into(), "V".into())],
                steps: vec![Step {
                    name: None,
                    run: "cargo test".into(),
                }],
            },
        };
        let bytes = serde_json::to_vec(&job).unwrap();
        let back: QueuedJob = serde_json::from_slice(&bytes).unwrap();
        check!(back == job);
    }
}
