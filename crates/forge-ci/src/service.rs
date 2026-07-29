//! The runner process.
//!
//! Pulls a job off the queue, claims it, runs it, streams the log, and reports.
//! One of these per worker; run as many as there are jobs to absorb.
//!
//! ## Claim before running, report through the log
//!
//! Delivery is at-least-once, so two runners can hold the same job. The claim
//! is a compare-and-swap in gres (`CiStore::claim_job`) and the loser simply
//! drops it — accepted, not released, because the job is being run by someone
//! and returning it to the queue would start a third attempt.
//!
//! Everything the runner decides is appended to the log, and `ci_jobs` is a
//! projection of that. So a runner that dies has said nothing and its job is
//! redelivered; a runner that says "failed" has said something, and the stale
//! attempt guard on `finish_job` stops a resurrected zombie from unsaying it.

use std::{sync::Arc, time::Duration};

use forge_bus::{FencedWriter, PendingRecord, WriteError};
use forge_events::{CiEvent, JobConclusion};
use forge_store::{Store, StoreError};
use forge_types::topics;

use crate::{
    queue::{Disposition, JobQueue, QueueError, QueuedJob},
    runner::{JobOutcome, LogSink, run_job},
    sandbox::Sandbox,
};

/// How long to wait for a job before looping.
const POLL_WAIT: Duration = Duration::from_millis(500);

/// Largest log chunk written as one record.
///
/// Chunked rather than one record per line: a chatty build would otherwise
/// produce hundreds of thousands of records, and the broker's per-record
/// overhead would dominate. Chunked rather than one record per job because a
/// log nobody can watch until the job ends is not much of a log.
const LOG_CHUNK_BYTES: usize = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error("the job queue: {0}")]
    Queue(#[from] QueueError),
    #[error("recording progress: {0}")]
    Store(#[from] StoreError),
    #[error("reporting to the log: {0}")]
    Write(#[from] WriteError),
}

/// Builds a sandbox for a job. A function so the runner does not have to know
/// whether it is handing out containers or child processes.
pub trait SandboxFactory {
    type Sandbox: Sandbox;

    /// Prepare somewhere to run `job`, or explain why not.
    fn create(&self, job: &QueuedJob) -> Result<Self::Sandbox, String>;
}

/// Collects a job's output and flushes it to the log topic in chunks.
struct TopicLog {
    writer: Arc<FencedWriter>,
    job_id: String,
    buffer: String,
    sequence: i64,
    pending: Vec<(i64, String)>,
}

impl TopicLog {
    fn new(writer: Arc<FencedWriter>, job_id: String) -> Self {
        Self {
            writer,
            job_id,
            buffer: String::new(),
            sequence: 0,
            pending: Vec::new(),
        }
    }

    /// Move whatever has accumulated into a pending chunk.
    fn seal(&mut self) {
        if self.buffer.is_empty() {
            return;
        }
        let chunk = std::mem::take(&mut self.buffer);
        self.pending.push((self.sequence, chunk));
        self.sequence += 1;
    }

    /// Write every sealed chunk to the log topic.
    ///
    /// Not transactional with anything: a log is not domain history, and a lost
    /// tail is a cosmetic loss where a blocked job would not be.
    async fn flush(&mut self, final_chunk: bool) -> Result<(), WriteError> {
        self.seal();
        if self.pending.is_empty() && !final_chunk {
            return Ok(());
        }
        let mut records = Vec::new();
        for (sequence, text) in self.pending.drain(..) {
            records.push(PendingRecord::state(
                topics::CI_LOGS,
                self.job_id.clone(),
                &serde_json::json!({
                    "job_id": self.job_id,
                    "seq": sequence,
                    "text": text,
                    "eof": false,
                }),
            )?);
        }
        if final_chunk {
            // An explicit end marker, so a tailing UI can stop waiting rather
            // than guess from silence.
            records.push(PendingRecord::state(
                topics::CI_LOGS,
                self.job_id.clone(),
                &serde_json::json!({
                    "job_id": self.job_id,
                    "seq": self.sequence,
                    "text": "",
                    "eof": true,
                }),
            )?);
        }
        if !records.is_empty() {
            self.writer.transact(records).await?;
        }
        Ok(())
    }
}

impl LogSink for TopicLog {
    fn line(&mut self, line: &str) {
        self.buffer.push_str(line);
        self.buffer.push('\n');
        if self.buffer.len() >= LOG_CHUNK_BYTES {
            self.seal();
        }
    }
}

/// One runner.
pub struct RunnerService<F: SandboxFactory> {
    queue: JobQueue,
    store: Store,
    writer: Arc<FencedWriter>,
    sandboxes: F,
}

impl<F: SandboxFactory> RunnerService<F> {
    pub async fn open(
        bootstrap: &str,
        store: Store,
        writer: Arc<FencedWriter>,
        sandboxes: F,
    ) -> Result<Self, ServiceError> {
        let queue = JobQueue::open(bootstrap).await?;
        tracing::info!("CI runner joined the queue");
        Ok(Self {
            queue,
            store,
            writer,
            sandboxes,
        })
    }

    /// Take one job if there is one. Returns whether anything ran.
    pub async fn step(&mut self) -> Result<bool, ServiceError> {
        let Some(lease) = self.queue.next(POLL_WAIT).await? else {
            return Ok(false);
        };
        let attempt = lease.attempt();
        let job = lease.job.clone();

        // Claim it. The loser accepts rather than releases: somebody is running
        // this, and returning it would start a third attempt.
        let claimed = self
            .store
            .ci()
            .claim_job(&job.job_id, attempt, 0, forge_types::now())
            .await?;
        if !claimed {
            tracing::debug!(job_id = %job.job_id, attempt, "another runner has this job");
            self.queue.settle(lease, Disposition::Done).await?;
            return Ok(true);
        }

        let outcome = self.execute(&job, attempt).await?;
        tracing::info!(
            job_id = %job.job_id,
            attempt,
            outcome = outcome.as_str(),
            "job finished"
        );
        self.queue.settle(lease, Disposition::Done).await?;
        Ok(true)
    }

    /// Run until cancelled.
    pub async fn run(mut self, mut shutdown: tokio::sync::watch::Receiver<bool>) {
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        tracing::info!("CI runner stopping");
                        return;
                    }
                }
                result = self.step() => {
                    if let Err(error) = result {
                        tracing::warn!(%error, "runner step failed; continuing");
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                }
            }
        }
    }

    /// Announce the start, run the steps, announce the result.
    async fn execute(&self, job: &QueuedJob, attempt: i64) -> Result<JobOutcome, ServiceError> {
        let repo_id = forge_types::RepoId::from(
            uuid::Uuid::parse_str(&job.repo_id).unwrap_or_else(|_| uuid::Uuid::nil()),
        );
        let run_id = forge_types::RunId::from(
            uuid::Uuid::parse_str(&job.run_id).unwrap_or_else(|_| uuid::Uuid::nil()),
        );
        let job_id = forge_types::JobId::from(
            uuid::Uuid::parse_str(&job.job_id).unwrap_or_else(|_| uuid::Uuid::nil()),
        );

        let started = CiEvent::JobStarted {
            job_id,
            run_id,
            repo_id,
            attempt,
            log_offset: 0,
        };
        self.writer
            .transact(vec![PendingRecord::event(&started, None)?])
            .await?;

        let mut log = TopicLog::new(Arc::clone(&self.writer), job.job_id.clone());
        let outcome = match self.sandboxes.create(job) {
            Ok(sandbox) => run_job(&job.job, &sandbox, &mut log).await,
            Err(reason) => {
                // A sandbox that will not start says nothing about the code, so
                // it must not read as a test failure.
                log.line(&format!("=== could not prepare a sandbox: {reason}"));
                JobOutcome::InfraFailed
            }
        };
        log.flush(true).await?;

        let conclusion = match outcome {
            JobOutcome::Succeeded => JobConclusion::Success,
            JobOutcome::Failed { .. } => JobConclusion::Failed,
            JobOutcome::TimedOut => JobConclusion::TimedOut,
            JobOutcome::InfraFailed => JobConclusion::InfraFailed,
        };
        let finished = CiEvent::JobFinished {
            job_id,
            run_id,
            repo_id,
            attempt,
            conclusion,
            exit_code: outcome.exit_code(),
        };
        // Only what this runner observed. Whether the *run* is over is a
        // function of every job's result, which this process cannot see: it has
        // just written its own to the log and nothing has projected it yet, so
        // every runner would find its own job still running and no run would
        // ever finish. The projector derives it when it applies this event.
        self.writer
            .transact(vec![PendingRecord::event(&finished, None)?])
            .await?;
        Ok(outcome)
    }
}
