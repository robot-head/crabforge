//! The `ci_runs` and `ci_jobs` read models.

use time::OffsetDateTime;
use tokio_postgres::Client;

use crate::{PageSize, StoreError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRecord {
    pub run_id: String,
    pub repo_id: String,
    pub number: i64,
    pub workflow: String,
    pub event: String,
    pub head_oid: String,
    pub ref_name: String,
    pub actor_name: String,
    pub status: String,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
    pub started_at: Option<OffsetDateTime>,
    pub finished_at: Option<OffsetDateTime>,
}

impl RunRecord {
    pub fn is_finished(&self) -> bool {
        matches!(self.status.as_str(), "success" | "failed" | "cancelled")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRecord {
    pub job_id: String,
    pub run_id: String,
    pub repo_id: String,
    pub name: String,
    pub image: String,
    pub status: String,
    pub attempt: i64,
    pub exit_code: Option<i64>,
    pub log_offset: Option<i64>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
    pub started_at: Option<OffsetDateTime>,
    pub finished_at: Option<OffsetDateTime>,
}

impl JobRecord {
    pub fn is_finished(&self) -> bool {
        matches!(
            self.status.as_str(),
            "success" | "failed" | "timed_out" | "infra_failed" | "cancelled"
        )
    }
}

pub struct CiStore<'a> {
    client: &'a Client,
}

impl<'a> CiStore<'a> {
    pub fn new(client: &'a Client) -> Self {
        Self { client }
    }

    /// See `UserStore::upsert`. A run's workflow, commit and number are absent
    /// from the update: they are what the run *is*, fixed when it was planned.
    pub async fn upsert_run(&self, run: &RunRecord) -> Result<(), StoreError> {
        self.client
            .execute(
                "INSERT INTO ci_runs (run_id, repo_id, number, workflow, event, head_oid, \
                 ref_name, actor_name, status, created_at, updated_at, started_at, finished_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13) \
                 ON CONFLICT (run_id) DO UPDATE SET \
                 status = excluded.status, updated_at = excluded.updated_at, \
                 started_at = excluded.started_at, finished_at = excluded.finished_at",
                &[
                    &run.run_id,
                    &run.repo_id,
                    &run.number,
                    &run.workflow,
                    &run.event,
                    &run.head_oid,
                    &run.ref_name,
                    &run.actor_name,
                    &run.status,
                    &run.created_at,
                    &run.updated_at,
                    &run.started_at,
                    &run.finished_at,
                ],
            )
            .await?;
        Ok(())
    }

    /// See `upsert_run`. A job's name and image are equally fixed.
    pub async fn upsert_job(&self, job: &JobRecord) -> Result<(), StoreError> {
        self.client
            .execute(
                "INSERT INTO ci_jobs (job_id, run_id, repo_id, name, image, status, attempt, \
                 exit_code, log_offset, created_at, updated_at, started_at, finished_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13) \
                 ON CONFLICT (job_id) DO UPDATE SET \
                 status = excluded.status, attempt = excluded.attempt, \
                 exit_code = excluded.exit_code, log_offset = excluded.log_offset, \
                 updated_at = excluded.updated_at, started_at = excluded.started_at, \
                 finished_at = excluded.finished_at",
                &[
                    &job.job_id,
                    &job.run_id,
                    &job.repo_id,
                    &job.name,
                    &job.image,
                    &job.status,
                    &job.attempt,
                    &job.exit_code,
                    &job.log_offset,
                    &job.created_at,
                    &job.updated_at,
                    &job.started_at,
                    &job.finished_at,
                ],
            )
            .await?;
        Ok(())
    }

    /// Claim a job for `attempt`, unless someone already has it.
    ///
    /// Returns whether the claim took. This is the compare-and-swap that makes
    /// at-least-once job delivery safe: two runners can be handed the same job,
    /// and exactly one of them gets to run it. The predicate is
    /// `status = 'queued'`, so a job already running or already finished is not
    /// restarted — a rerun is a new job, not a second go at this one.
    pub async fn claim_job(
        &self,
        job_id: &str,
        attempt: i64,
        log_offset: i64,
        at: OffsetDateTime,
    ) -> Result<bool, StoreError> {
        let claimed = self
            .client
            .execute(
                "UPDATE ci_jobs SET status = 'running', attempt = $2, log_offset = $3, \
                 started_at = $4, updated_at = $4 \
                 WHERE job_id = $1 AND status = 'queued'",
                &[&job_id, &attempt, &log_offset, &at],
            )
            .await?;
        Ok(claimed > 0)
    }

    /// Record how a job ended, if this attempt is the one that owns it.
    ///
    /// The attempt predicate matters: a runner that was declared dead and had
    /// its job redelivered may still be alive and about to report. Its verdict
    /// is about a run nobody is waiting for, and applying it would overwrite the
    /// live attempt's.
    pub async fn finish_job(
        &self,
        job_id: &str,
        attempt: i64,
        status: &str,
        exit_code: Option<i64>,
        at: OffsetDateTime,
    ) -> Result<bool, StoreError> {
        let updated = self
            .client
            .execute(
                "UPDATE ci_jobs SET status = $3, exit_code = $4, finished_at = $5, \
                 updated_at = $5 WHERE job_id = $1 AND attempt = $2",
                &[&job_id, &attempt, &status, &exit_code, &at],
            )
            .await?;
        Ok(updated > 0)
    }

    pub async fn run_by_id(&self, run_id: &str) -> Result<Option<RunRecord>, StoreError> {
        let row = self
            .client
            .query_opt(&format!("{RUN_COLUMNS} WHERE run_id = $1"), &[&run_id])
            .await?;
        Ok(row.as_ref().map(row_to_run))
    }

    pub async fn job_by_id(&self, job_id: &str) -> Result<Option<JobRecord>, StoreError> {
        let row = self
            .client
            .query_opt(&format!("{JOB_COLUMNS} WHERE job_id = $1"), &[&job_id])
            .await?;
        Ok(row.as_ref().map(row_to_job))
    }

    /// A run's jobs, in a stable order.
    pub async fn jobs_of(&self, run_id: &str) -> Result<Vec<JobRecord>, StoreError> {
        let rows = self
            .client
            .query(
                &format!("{JOB_COLUMNS} WHERE run_id = $1 ORDER BY name ASC"),
                &[&run_id],
            )
            .await?;
        Ok(rows.iter().map(row_to_job).collect())
    }

    /// Runs for a repository, newest first.
    pub async fn runs_for_repo(
        &self,
        repo_id: &str,
        limit: PageSize,
    ) -> Result<Vec<RunRecord>, StoreError> {
        // TODO(gres:parameterized-limit)
        let limit = *limit;
        let rows = self
            .client
            .query(
                &format!("{RUN_COLUMNS} WHERE repo_id = $1 ORDER BY run_id DESC LIMIT {limit}"),
                &[&repo_id],
            )
            .await?;
        Ok(rows.iter().map(row_to_run).collect())
    }

    /// Runs for one commit — what a pull request's checks list shows.
    pub async fn runs_for_commit(
        &self,
        head_oid: &str,
        limit: PageSize,
    ) -> Result<Vec<RunRecord>, StoreError> {
        // TODO(gres:parameterized-limit)
        let limit = *limit;
        let rows = self
            .client
            .query(
                &format!("{RUN_COLUMNS} WHERE head_oid = $1 ORDER BY run_id DESC LIMIT {limit}"),
                &[&head_oid],
            )
            .await?;
        Ok(rows.iter().map(row_to_run).collect())
    }
}

const RUN_COLUMNS: &str = "SELECT run_id, repo_id, number, workflow, event, head_oid, ref_name, \
     actor_name, status, created_at, updated_at, started_at, finished_at FROM ci_runs";

const JOB_COLUMNS: &str = "SELECT job_id, run_id, repo_id, name, image, status, attempt, \
     exit_code, log_offset, created_at, updated_at, started_at, finished_at FROM ci_jobs";

fn row_to_run(row: &tokio_postgres::Row) -> RunRecord {
    RunRecord {
        run_id: row.get(0),
        repo_id: row.get(1),
        number: row.get(2),
        workflow: row.get(3),
        event: row.get(4),
        head_oid: row.get(5),
        ref_name: row.get(6),
        actor_name: row.get(7),
        status: row.get(8),
        created_at: row.get(9),
        updated_at: row.get(10),
        started_at: row.get(11),
        finished_at: row.get(12),
    }
}

fn row_to_job(row: &tokio_postgres::Row) -> JobRecord {
    JobRecord {
        job_id: row.get(0),
        run_id: row.get(1),
        repo_id: row.get(2),
        name: row.get(3),
        image: row.get(4),
        status: row.get(5),
        attempt: row.get(6),
        exit_code: row.get(7),
        log_offset: row.get(8),
        created_at: row.get(9),
        updated_at: row.get(10),
        started_at: row.get(11),
        finished_at: row.get(12),
    }
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;

    fn job(status: &str) -> JobRecord {
        let now = forge_types::now();
        JobRecord {
            job_id: "j".into(),
            run_id: "r".into(),
            repo_id: "repo".into(),
            name: "test".into(),
            image: "ubuntu:24.04".into(),
            status: status.into(),
            attempt: 1,
            exit_code: None,
            log_offset: None,
            created_at: now,
            updated_at: now,
            started_at: None,
            finished_at: None,
        }
    }

    #[test]
    fn a_job_is_finished_only_once_it_has_actually_stopped() {
        check!(!job("queued").is_finished());
        check!(!job("running").is_finished());
        for status in [
            "success",
            "failed",
            "timed_out",
            "infra_failed",
            "cancelled",
        ] {
            check!(job(status).is_finished(), "{status} should be terminal");
        }
    }

    #[test]
    fn an_unrecognised_status_is_not_treated_as_finished() {
        // A status this build does not know is more likely a newer writer's than
        // corruption, and treating it as finished would let a run report a
        // conclusion while a job is still going.
        check!(!job("provisioning").is_finished());
    }
}
