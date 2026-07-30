//! Turning a push into queued work.
//!
//! Follows `forge.events.git-refs`, and for each ref update reads the workflows
//! at the commit that was pushed, plans them, announces the result on the CI
//! event topic, and puts the jobs on the queue.
//!
//! ## Why this is not in the command service
//!
//! Planning a run needs the repository's *contents* at a commit, which means a
//! hydrated git cache — potentially a fetch of everything a push introduced.
//! The command service holds a per-aggregate lock while it decides, and a slow
//! filesystem operation inside that lock would stall every other command for
//! the repository. Reacting after the fact costs a little latency and keeps the
//! write path fast.
//!
//! ## Announce before queueing
//!
//! `RunQueued` goes to the log first, and only then do the jobs go on the
//! queue. The order matters: a runner that picks a job up looks its row up to
//! claim it, and a job queued before its run was announced would find nothing
//! there. The reverse failure — an announced run whose jobs never queued —
//! shows up as a run stuck in `queued`, which is visible and fixable, rather
//! than as a job that vanishes.

use std::sync::Arc;

use forge_bus::{FencedWriter, PendingRecord, TailError, Tailer, WriteError};
use forge_events::{CiEvent, GitRefEvent, PlannedJobSpec, decode_raw};
use forge_git::Cache;
use forge_store::{CI_ORCHESTRATOR, Store, StoreError};
use forge_types::{JobId, Oid, RepoId, RunId, topics};

use crate::{discover, plan::plan_push, queue::QueuedJob};

/// How long to wait for new records before looping again.
const POLL_WAIT_MS: i32 = 500;

#[derive(Debug, thiserror::Error)]
pub enum OrchestrateError {
    #[error("reading the log: {0}")]
    Tail(#[from] TailError),
    #[error("reading forge state: {0}")]
    Store(#[from] StoreError),
    #[error("announcing a run: {0}")]
    Write(#[from] WriteError),
}

/// Watches pushes and plans the CI they imply.
pub struct Orchestrator {
    tailer: Tailer,
    store: Store,
    writer: Arc<FencedWriter>,
    bootstrap: String,
    cache_root: std::path::PathBuf,
}

impl Orchestrator {
    pub async fn open(
        bootstrap: &str,
        store: Store,
        writer: Arc<FencedWriter>,
        cache_root: impl Into<std::path::PathBuf>,
    ) -> Result<Self, OrchestrateError> {
        let resume_from = store
            .cursors(CI_ORCHESTRATOR)
            .applied_offset(topics::EVENTS_GIT_REFS)
            .await?;
        let tailer = Tailer::open_at(bootstrap, topics::EVENTS_GIT_REFS, resume_from).await?;
        tracing::info!(resume_from, "CI orchestrator opened");
        Ok(Self {
            tailer,
            store,
            writer,
            bootstrap: bootstrap.to_string(),
            cache_root: cache_root.into(),
        })
    }

    /// Read one batch of ref updates and plan whatever they imply.
    ///
    /// Returns how many runs were queued.
    pub async fn step(&mut self) -> Result<usize, OrchestrateError> {
        let batch = self.tailer.next_batch(POLL_WAIT_MS).await?;
        if batch.records.is_empty() {
            return Ok(0);
        }

        let mut queued = 0;
        for record in &batch.records {
            queued += self.on_record(record.value.as_deref()).await?;
        }

        self.store
            .cursors(CI_ORCHESTRATOR)
            .set_applied_offset(topics::EVENTS_GIT_REFS, batch.next_offset)
            .await?;
        Ok(queued)
    }

    /// Run until cancelled.
    pub async fn run(mut self, mut shutdown: tokio::sync::watch::Receiver<bool>) {
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        tracing::info!("CI orchestrator stopping");
                        return;
                    }
                }
                result = self.step() => {
                    if let Err(error) = result {
                        // The cursor has not moved, so retrying loses nothing.
                        tracing::warn!(%error, "CI orchestrator step failed; retrying");
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    }
                }
            }
        }
    }

    async fn on_record(&self, value: Option<&[u8]>) -> Result<usize, OrchestrateError> {
        let Ok(raw) = decode_raw(value) else {
            return Ok(0);
        };
        let Ok(envelope) = raw.parse::<GitRefEvent>() else {
            tracing::debug!(
                event_type = %raw.event_type,
                "skipping a ref event the orchestrator cannot read"
            );
            return Ok(0);
        };

        let GitRefEvent::RefUpdated {
            repo_id,
            r#ref,
            new,
            pusher,
            ..
        } = envelope.payload;

        // A deletion has no commit to read a workflow from. Running the one
        // from the commit that *was* there would be executing code the push
        // removed.
        let Some(head) = new else {
            return Ok(0);
        };

        self.plan_for(repo_id, &r#ref, head, &pusher.to_string())
            .await
    }

    /// Plan and queue every run a pushed commit implies.
    async fn plan_for(
        &self,
        repo_id: RepoId,
        ref_name: &str,
        head: Oid,
        pusher_id: &str,
    ) -> Result<usize, OrchestrateError> {
        let Some(repo) = self.store.repos().by_id(&repo_id.to_string()).await? else {
            tracing::warn!(%repo_id, "ref update for an unknown repository; skipping");
            return Ok(0);
        };

        // The cache has to be current for the pushed commit to be readable:
        // the objects reached the log before the ref did, but this process may
        // never have seen them.
        let cache = Cache::new(&self.cache_root, repo_id);
        if let Err(error) = cache.hydrate(&self.bootstrap, &repo.default_branch).await {
            // Not fatal, and not silent: without the objects there is nothing
            // to plan, and the cursor stays put so the next pass tries again.
            tracing::warn!(%repo_id, %error, "could not hydrate the cache to plan CI");
            return Ok(0);
        }

        let head_hex = head.to_hex();
        let found = discover(&cache, &head_hex);
        for error in &found.errors {
            // A broken workflow is the author's to fix, but it must not be
            // invisible — this is the only place it is noticed today.
            tracing::warn!(%repo_id, %error, "ignoring an invalid workflow");
        }

        let plans = plan_push(&found);
        if plans.is_empty() {
            return Ok(0);
        }

        let actor_name = self
            .store
            .users()
            .by_id(pusher_id)
            .await?
            .map_or_else(|| "unknown".to_string(), |user| user.username);

        let mut queued = 0;
        for planned in plans {
            let run_id = RunId::new();
            let number = self
                .store
                .ci()
                .next_run_number(&repo_id.to_string())
                .await?;

            // Ids are minted here so the announcement and the queued jobs agree
            // about them.
            let jobs: Vec<(JobId, crate::plan::PlannedJob)> = planned
                .jobs
                .iter()
                .map(|job| (JobId::new(), job.clone()))
                .collect();

            let announcement = CiEvent::RunQueued {
                run_id,
                repo_id,
                number,
                workflow: planned.workflow.clone(),
                name: planned.name.clone(),
                event: "push".to_string(),
                head_oid: head,
                ref_name: ref_name.to_string(),
                actor_name: actor_name.clone(),
                jobs: jobs
                    .iter()
                    .map(|(job_id, job)| PlannedJobSpec {
                        job_id: *job_id,
                        name: job.name.clone(),
                        image: job.image.clone(),
                    })
                    .collect(),
            };

            // Announced first — see the module docs.
            self.writer
                .transact(vec![PendingRecord::event(&announcement, None)?])
                .await?;

            let records = jobs
                .iter()
                .map(|(job_id, job)| {
                    let queued = QueuedJob {
                        job_id: job_id.to_string(),
                        run_id: run_id.to_string(),
                        repo_id: repo_id.to_string(),
                        head_oid: head_hex.clone(),
                        job: job.clone(),
                    };
                    // Keyed by job so a redelivery lands on the same partition
                    // and one runner's backlog cannot reorder another's.
                    PendingRecord::state(topics::CI_JOBS, job_id.to_string(), &queued)
                })
                .collect::<Result<Vec<_>, _>>()?;
            self.writer.transact(records).await?;

            tracing::info!(
                %repo_id,
                %run_id,
                number,
                workflow = %planned.workflow,
                jobs = jobs.len(),
                "queued a CI run"
            );
            queued += 1;
        }
        Ok(queued)
    }
}
