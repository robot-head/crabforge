//! The `issues` read model.

use time::OffsetDateTime;
use tokio_postgres::Client;

use crate::{PageSize, StoreError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueRecord {
    pub issue_id: String,
    pub repo_id: String,
    pub number: i64,
    pub title: String,
    pub body: Option<String>,
    pub author_id: String,
    pub author_name: String,
    pub state: String,
    pub comment_count: i64,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
    pub closed_at: Option<OffsetDateTime>,
}

impl IssueRecord {
    pub fn is_open(&self) -> bool {
        self.state == "open"
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommentRecord {
    pub comment_id: String,
    pub issue_id: String,
    pub repo_id: String,
    pub author_id: String,
    pub author_name: String,
    pub body: String,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

/// Open and closed counts for a repository's tab badges.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counters {
    pub open_issues: i64,
    pub closed_issues: i64,
}

pub struct IssueStore<'a> {
    client: &'a Client,
}

impl<'a> IssueStore<'a> {
    pub fn new(client: &'a Client) -> Self {
        Self { client }
    }

    /// See `UserStore::upsert`. The repository, number, and author are absent
    /// from the update because an issue cannot move between repositories, be
    /// renumbered, or change who filed it.
    pub async fn upsert(&self, issue: &IssueRecord) -> Result<(), StoreError> {
        self.client
            .execute(
                "INSERT INTO issues (issue_id, repo_id, number, title, body, author_id, \
                 author_name, state, comment_count, created_at, updated_at, closed_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) \
                 ON CONFLICT (issue_id) DO UPDATE SET \
                 title = excluded.title, body = excluded.body, state = excluded.state, \
                 comment_count = excluded.comment_count, updated_at = excluded.updated_at, \
                 closed_at = excluded.closed_at",
                &[
                    &issue.issue_id,
                    &issue.repo_id,
                    &issue.number,
                    &issue.title,
                    &issue.body,
                    &issue.author_id,
                    &issue.author_name,
                    &issue.state,
                    &issue.comment_count,
                    &issue.created_at,
                    &issue.updated_at,
                    &issue.closed_at,
                ],
            )
            .await?;
        Ok(())
    }

    pub async fn by_id(&self, issue_id: &str) -> Result<Option<IssueRecord>, StoreError> {
        let row = self
            .client
            .query_opt(
                &format!("{ISSUE_COLUMNS} WHERE issue_id = $1"),
                &[&issue_id],
            )
            .await?;
        Ok(row.as_ref().map(row_to_issue))
    }

    /// Resolve the `#42` a user typed.
    pub async fn by_number(
        &self,
        repo_id: &str,
        number: i64,
    ) -> Result<Option<IssueRecord>, StoreError> {
        let row = self
            .client
            .query_opt(
                &format!("{ISSUE_COLUMNS} WHERE repo_id = $1 AND number = $2"),
                &[&repo_id, &number],
            )
            .await?;
        Ok(row.as_ref().map(row_to_issue))
    }

    /// A repository's issues in one state, newest first.
    ///
    /// Keyset paginated on `number`, which is monotonic per repository, so no
    /// OFFSET is involved and a page cannot skip or repeat a row when issues
    /// are opened during paging.
    pub async fn list(
        &self,
        repo_id: &str,
        open: bool,
        before: Option<i64>,
        limit: PageSize,
    ) -> Result<Vec<IssueRecord>, StoreError> {
        // TODO(gres:parameterized-limit) — see `RepoStore::for_owner`.
        let limit = *limit;
        let state = if open { "open" } else { "closed" };
        let rows = match before {
            Some(cursor) => {
                self.client
                    .query(
                        &format!(
                            "{ISSUE_COLUMNS} WHERE repo_id = $1 AND state = $2 AND number < $3 \
                             ORDER BY number DESC LIMIT {limit}"
                        ),
                        &[&repo_id, &state, &cursor],
                    )
                    .await?
            }
            None => {
                self.client
                    .query(
                        &format!(
                            "{ISSUE_COLUMNS} WHERE repo_id = $1 AND state = $2 \
                             ORDER BY number DESC LIMIT {limit}"
                        ),
                        &[&repo_id, &state],
                    )
                    .await?
            }
        };
        Ok(rows.iter().map(row_to_issue).collect())
    }

    /// Add a comment.
    ///
    /// Replay re-delivers comments, so one already present is left alone rather
    /// than inserted twice — the id is the event's, so a second delivery is the
    /// same comment, not a new one.
    pub async fn insert_comment(&self, comment: &CommentRecord) -> Result<(), StoreError> {
        self.client
            .execute(
                "INSERT INTO issue_comments (comment_id, issue_id, repo_id, author_id, \
                 author_name, body, created_at, updated_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
                 ON CONFLICT (comment_id) DO NOTHING",
                &[
                    &comment.comment_id,
                    &comment.issue_id,
                    &comment.repo_id,
                    &comment.author_id,
                    &comment.author_name,
                    &comment.body,
                    &comment.created_at,
                    &comment.updated_at,
                ],
            )
            .await?;
        Ok(())
    }

    /// A conversation, oldest first.
    ///
    /// Ordered by id, which is a UUIDv7 and therefore chronological — so no
    /// sort column is needed and the order cannot disagree with creation order.
    pub async fn comments(
        &self,
        issue_id: &str,
        limit: PageSize,
    ) -> Result<Vec<CommentRecord>, StoreError> {
        // TODO(gres:parameterized-limit)
        let limit = *limit;
        let rows = self
            .client
            .query(
                &format!(
                    "SELECT comment_id, issue_id, repo_id, author_id, author_name, body, \
                     created_at, updated_at FROM issue_comments WHERE issue_id = $1 \
                     ORDER BY comment_id ASC LIMIT {limit}"
                ),
                &[&issue_id],
            )
            .await?;

        Ok(rows
            .iter()
            .map(|row| CommentRecord {
                comment_id: row.get(0),
                issue_id: row.get(1),
                repo_id: row.get(2),
                author_id: row.get(3),
                author_name: row.get(4),
                body: row.get(5),
                created_at: row.get(6),
                updated_at: row.get(7),
            })
            .collect())
    }

    /// Open and closed counts.
    pub async fn counters(&self, repo_id: &str) -> Result<Counters, StoreError> {
        let row = self
            .client
            .query_opt(
                "SELECT open_issues, closed_issues FROM repo_counters WHERE repo_id = $1",
                &[&repo_id],
            )
            .await?;
        Ok(row.map_or_else(Counters::default, |row| Counters {
            open_issues: row.get(0),
            closed_issues: row.get(1),
        }))
    }

    /// Recompute a repository's counters from its issues.
    ///
    /// Derived rather than incremented, so a replay cannot double-count and a
    /// counter cannot drift away from the rows it describes.
    pub async fn refresh_counters(&self, repo_id: &str) -> Result<Counters, StoreError> {
        let row = self
            .client
            .query_one(
                "SELECT count(*) FROM issues WHERE repo_id = $1 AND state = 'open'",
                &[&repo_id],
            )
            .await?;
        let open: i64 = row.get(0);
        let row = self
            .client
            .query_one(
                "SELECT count(*) FROM issues WHERE repo_id = $1 AND state = 'closed'",
                &[&repo_id],
            )
            .await?;
        let closed: i64 = row.get(0);

        self.client
            .execute(
                "INSERT INTO repo_counters (repo_id, open_issues, closed_issues) \
                 VALUES ($1, $2, $3) \
                 ON CONFLICT (repo_id) DO UPDATE SET \
                 open_issues = excluded.open_issues, closed_issues = excluded.closed_issues",
                &[&repo_id, &open, &closed],
            )
            .await?;
        Ok(Counters {
            open_issues: open,
            closed_issues: closed,
        })
    }
}

const ISSUE_COLUMNS: &str = "SELECT issue_id, repo_id, number, title, body, author_id, \
     author_name, state, comment_count, created_at, updated_at, closed_at FROM issues";

fn row_to_issue(row: &tokio_postgres::Row) -> IssueRecord {
    IssueRecord {
        issue_id: row.get(0),
        repo_id: row.get(1),
        number: row.get(2),
        title: row.get(3),
        body: row.get(4),
        author_id: row.get(5),
        author_name: row.get(6),
        state: row.get(7),
        comment_count: row.get(8),
        created_at: row.get(9),
        updated_at: row.get(10),
        closed_at: row.get(11),
    }
}
