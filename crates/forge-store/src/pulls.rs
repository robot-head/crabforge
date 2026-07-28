//! The `pulls` read model.

use time::OffsetDateTime;
use tokio_postgres::Client;

use crate::{PageSize, StoreError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRecord {
    pub pr_id: String,
    pub repo_id: String,
    pub number: i64,
    pub title: String,
    pub body: Option<String>,
    pub author_id: String,
    pub author_name: String,
    pub state: String,
    pub source_branch: String,
    pub target_branch: String,
    pub head_oid: String,
    pub base_oid: String,
    pub mergeable: String,
    pub merge_commit_oid: Option<String>,
    pub merged_by_name: Option<String>,
    pub comment_count: i64,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
    pub merged_at: Option<OffsetDateTime>,
    pub closed_at: Option<OffsetDateTime>,
}

/// What a trial merge concluded, as stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mergeable {
    /// Not computed yet, or computed against commits that have since moved.
    Unknown,
    Clean,
    Conflict,
}

impl Mergeable {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Clean => "clean",
            Self::Conflict => "conflict",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "clean" => Self::Clean,
            "conflict" => Self::Conflict,
            _ => Self::Unknown,
        }
    }
}

impl PullRecord {
    pub fn is_open(&self) -> bool {
        self.state == "open"
    }

    pub fn is_merged(&self) -> bool {
        self.state == "merged"
    }

    pub fn mergeability(&self) -> Mergeable {
        Mergeable::parse(&self.mergeable)
    }

    /// Whether the merge button should be offered.
    ///
    /// Only for an open pull request whose trial merge is known to be clean.
    /// `Unknown` deliberately does not qualify: offering a button that then
    /// fails is worse than a spinner.
    pub fn can_merge(&self) -> bool {
        self.is_open() && self.mergeability() == Mergeable::Clean
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewRecord {
    pub review_id: String,
    pub pr_id: String,
    pub repo_id: String,
    pub reviewer_id: String,
    pub reviewer_name: String,
    pub verdict: String,
    pub body: Option<String>,
    pub created_at: OffsetDateTime,
}

pub struct PullStore<'a> {
    client: &'a Client,
}

impl<'a> PullStore<'a> {
    pub fn new(client: &'a Client) -> Self {
        Self { client }
    }

    /// TODO(gres:on-conflict) — see `UserStore::upsert`.
    pub async fn upsert(&self, pr: &PullRecord) -> Result<(), StoreError> {
        let existing = self
            .client
            .query_opt("SELECT pr_id FROM pulls WHERE pr_id = $1", &[&pr.pr_id])
            .await?;

        if existing.is_some() {
            self.client
                .execute(
                    "UPDATE pulls SET title = $2, body = $3, state = $4, head_oid = $5, \
                     base_oid = $6, mergeable = $7, merge_commit_oid = $8, merged_by_name = $9, \
                     comment_count = $10, updated_at = $11, merged_at = $12, closed_at = $13 \
                     WHERE pr_id = $1",
                    &[
                        &pr.pr_id,
                        &pr.title,
                        &pr.body,
                        &pr.state,
                        &pr.head_oid,
                        &pr.base_oid,
                        &pr.mergeable,
                        &pr.merge_commit_oid,
                        &pr.merged_by_name,
                        &pr.comment_count,
                        &pr.updated_at,
                        &pr.merged_at,
                        &pr.closed_at,
                    ],
                )
                .await?;
        } else {
            self.client
                .execute(
                    "INSERT INTO pulls (pr_id, repo_id, number, title, body, author_id, \
                     author_name, state, source_branch, target_branch, head_oid, base_oid, \
                     mergeable, merge_commit_oid, merged_by_name, comment_count, created_at, \
                     updated_at, merged_at, closed_at) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, \
                     $16, $17, $18, $19, $20)",
                    &[
                        &pr.pr_id,
                        &pr.repo_id,
                        &pr.number,
                        &pr.title,
                        &pr.body,
                        &pr.author_id,
                        &pr.author_name,
                        &pr.state,
                        &pr.source_branch,
                        &pr.target_branch,
                        &pr.head_oid,
                        &pr.base_oid,
                        &pr.mergeable,
                        &pr.merge_commit_oid,
                        &pr.merged_by_name,
                        &pr.comment_count,
                        &pr.created_at,
                        &pr.updated_at,
                        &pr.merged_at,
                        &pr.closed_at,
                    ],
                )
                .await?;
        }
        Ok(())
    }

    pub async fn by_id(&self, pr_id: &str) -> Result<Option<PullRecord>, StoreError> {
        let row = self
            .client
            .query_opt(&format!("{PULL_COLUMNS} WHERE pr_id = $1"), &[&pr_id])
            .await?;
        Ok(row.as_ref().map(row_to_pull))
    }

    pub async fn by_number(
        &self,
        repo_id: &str,
        number: i64,
    ) -> Result<Option<PullRecord>, StoreError> {
        let row = self
            .client
            .query_opt(
                &format!("{PULL_COLUMNS} WHERE repo_id = $1 AND number = $2"),
                &[&repo_id, &number],
            )
            .await?;
        Ok(row.as_ref().map(row_to_pull))
    }

    /// Open or closed pull requests, newest first.
    pub async fn list(
        &self,
        repo_id: &str,
        open: bool,
        limit: PageSize,
    ) -> Result<Vec<PullRecord>, StoreError> {
        // TODO(gres:parameterized-limit)
        let limit = *limit;
        // "Closed" in the list sense includes merged: both are done.
        let rows = if open {
            self.client
                .query(
                    &format!(
                        "{PULL_COLUMNS} WHERE repo_id = $1 AND state = 'open' \
                         ORDER BY number DESC LIMIT {limit}"
                    ),
                    &[&repo_id],
                )
                .await?
        } else {
            self.client
                .query(
                    &format!(
                        "{PULL_COLUMNS} WHERE repo_id = $1 AND state <> 'open' \
                         ORDER BY number DESC LIMIT {limit}"
                    ),
                    &[&repo_id],
                )
                .await?
        };
        Ok(rows.iter().map(row_to_pull).collect())
    }

    /// Every open pull request targeting a branch.
    ///
    /// Used when that branch moves: each one's diff and mergeability are now
    /// stale and have to be recomputed.
    pub async fn open_targeting(
        &self,
        repo_id: &str,
        branch: &str,
    ) -> Result<Vec<PullRecord>, StoreError> {
        let rows = self
            .client
            .query(
                &format!(
                    "{PULL_COLUMNS} WHERE repo_id = $1 AND state = 'open' AND target_branch = $2"
                ),
                &[&repo_id, &branch],
            )
            .await?;
        Ok(rows.iter().map(row_to_pull).collect())
    }

    /// Every open pull request whose source is a branch.
    pub async fn open_from(
        &self,
        repo_id: &str,
        branch: &str,
    ) -> Result<Vec<PullRecord>, StoreError> {
        let rows = self
            .client
            .query(
                &format!(
                    "{PULL_COLUMNS} WHERE repo_id = $1 AND state = 'open' AND source_branch = $2"
                ),
                &[&repo_id, &branch],
            )
            .await?;
        Ok(rows.iter().map(row_to_pull).collect())
    }

    /// Replace a pull request's conflict list.
    ///
    /// Deleted and rewritten rather than merged, because the list describes one
    /// pair of commits: keeping stale rows would send someone to reconcile a
    /// file that no longer disagrees.
    pub async fn set_conflicts(
        &self,
        pr_id: &str,
        head: &str,
        base: &str,
        paths: &[String],
    ) -> Result<(), StoreError> {
        self.client
            .execute("DELETE FROM pr_conflicts WHERE pr_id = $1", &[&pr_id])
            .await?;
        for path in paths {
            self.client
                .execute(
                    "INSERT INTO pr_conflicts (row_id, pr_id, path, computed_for_head, \
                     computed_for_base) VALUES ($1, $2, $3, $4, $5)",
                    &[
                        &forge_types::CommentId::new().to_string(),
                        &pr_id,
                        path,
                        &head,
                        &base,
                    ],
                )
                .await?;
        }
        Ok(())
    }

    /// The conflicting paths, if they were computed for the current commits.
    pub async fn conflicts(
        &self,
        pr_id: &str,
        head: &str,
        base: &str,
    ) -> Result<Vec<String>, StoreError> {
        let rows = self
            .client
            .query(
                "SELECT path FROM pr_conflicts WHERE pr_id = $1 AND computed_for_head = $2 \
                 AND computed_for_base = $3",
                &[&pr_id, &head, &base],
            )
            .await?;
        Ok(rows.iter().map(|row| row.get(0)).collect())
    }

    pub async fn insert_review(&self, review: &ReviewRecord) -> Result<(), StoreError> {
        // TODO(gres:on-conflict)
        let existing = self
            .client
            .query_opt(
                "SELECT review_id FROM pr_reviews WHERE review_id = $1",
                &[&review.review_id],
            )
            .await?;
        if existing.is_some() {
            return Ok(());
        }

        self.client
            .execute(
                "INSERT INTO pr_reviews (review_id, pr_id, repo_id, reviewer_id, reviewer_name, \
                 verdict, body, created_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
                &[
                    &review.review_id,
                    &review.pr_id,
                    &review.repo_id,
                    &review.reviewer_id,
                    &review.reviewer_name,
                    &review.verdict,
                    &review.body,
                    &review.created_at,
                ],
            )
            .await?;
        Ok(())
    }

    pub async fn reviews(&self, pr_id: &str) -> Result<Vec<ReviewRecord>, StoreError> {
        let rows = self
            .client
            .query(
                "SELECT review_id, pr_id, repo_id, reviewer_id, reviewer_name, verdict, body, \
                 created_at FROM pr_reviews WHERE pr_id = $1 ORDER BY review_id ASC",
                &[&pr_id],
            )
            .await?;
        Ok(rows
            .iter()
            .map(|row| ReviewRecord {
                review_id: row.get(0),
                pr_id: row.get(1),
                repo_id: row.get(2),
                reviewer_id: row.get(3),
                reviewer_name: row.get(4),
                verdict: row.get(5),
                body: row.get(6),
                created_at: row.get(7),
            })
            .collect())
    }
}

const PULL_COLUMNS: &str = "SELECT pr_id, repo_id, number, title, body, author_id, author_name, \
     state, source_branch, target_branch, head_oid, base_oid, mergeable, merge_commit_oid, \
     merged_by_name, comment_count, created_at, updated_at, merged_at, closed_at FROM pulls";

fn row_to_pull(row: &tokio_postgres::Row) -> PullRecord {
    PullRecord {
        pr_id: row.get(0),
        repo_id: row.get(1),
        number: row.get(2),
        title: row.get(3),
        body: row.get(4),
        author_id: row.get(5),
        author_name: row.get(6),
        state: row.get(7),
        source_branch: row.get(8),
        target_branch: row.get(9),
        head_oid: row.get(10),
        base_oid: row.get(11),
        mergeable: row.get(12),
        merge_commit_oid: row.get(13),
        merged_by_name: row.get(14),
        comment_count: row.get(15),
        created_at: row.get(16),
        updated_at: row.get(17),
        merged_at: row.get(18),
        closed_at: row.get(19),
    }
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;

    fn pull(state: &str, mergeable: &str) -> PullRecord {
        let now = forge_types::now();
        PullRecord {
            pr_id: "p".into(),
            repo_id: "r".into(),
            number: 1,
            title: "t".into(),
            body: None,
            author_id: "u".into(),
            author_name: "octocat".into(),
            state: state.into(),
            source_branch: "feature".into(),
            target_branch: "main".into(),
            head_oid: "a".into(),
            base_oid: "b".into(),
            mergeable: mergeable.into(),
            merge_commit_oid: None,
            merged_by_name: None,
            comment_count: 0,
            created_at: now,
            updated_at: now,
            merged_at: None,
            closed_at: None,
        }
    }

    #[test]
    fn the_merge_button_is_offered_only_for_a_clean_open_request() {
        check!(pull("open", "clean").can_merge());

        // A conflicted one cannot be merged.
        check!(!pull("open", "conflict").can_merge());
        // Nor one whose mergeability nobody has computed yet: a button that
        // then fails is worse than waiting for the answer.
        check!(!pull("open", "unknown").can_merge());
        // Nor a finished one.
        check!(!pull("merged", "clean").can_merge());
        check!(!pull("closed", "clean").can_merge());
    }

    #[test]
    fn an_unrecognised_mergeability_reads_as_unknown() {
        // Rather than as mergeable, which would enable a button on a guess.
        check!(Mergeable::parse("something else") == Mergeable::Unknown);
        check!(Mergeable::parse("") == Mergeable::Unknown);
    }

    #[test]
    fn mergeability_round_trips_through_its_stored_form() {
        for value in [Mergeable::Unknown, Mergeable::Clean, Mergeable::Conflict] {
            check!(Mergeable::parse(value.as_str()) == value);
        }
    }
}
