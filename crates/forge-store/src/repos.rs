//! The `repos` read model, and the projector's cursor.

use time::OffsetDateTime;
use tokio_postgres::Client;

use crate::StoreError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoRecord {
    pub repo_id: String,
    pub owner_id: String,
    pub owner_name: String,
    pub name: String,
    pub full_name_lower: String,
    pub description: Option<String>,
    pub default_branch: String,
    pub visibility: String,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
    pub deleted: bool,
}

pub struct RepoStore<'a> {
    client: &'a Client,
}

impl<'a> RepoStore<'a> {
    pub fn new(client: &'a Client) -> Self {
        Self { client }
    }

    /// TODO(gres:on-conflict) — see `UserStore::upsert`.
    pub async fn upsert(&self, repo: &RepoRecord) -> Result<(), StoreError> {
        let existing = self
            .client
            .query_opt(
                "SELECT repo_id FROM repos WHERE repo_id = $1",
                &[&repo.repo_id],
            )
            .await?;

        if existing.is_some() {
            self.client
                .execute(
                    "UPDATE repos SET owner_id = $2, owner_name = $3, name = $4, \
                     full_name_lower = $5, description = $6, default_branch = $7, \
                     visibility = $8, updated_at = $9, deleted = $10 WHERE repo_id = $1",
                    &[
                        &repo.repo_id,
                        &repo.owner_id,
                        &repo.owner_name,
                        &repo.name,
                        &repo.full_name_lower,
                        &repo.description,
                        &repo.default_branch,
                        &repo.visibility,
                        &repo.updated_at,
                        &repo.deleted,
                    ],
                )
                .await?;
        } else {
            self.client
                .execute(
                    "INSERT INTO repos (repo_id, owner_id, owner_name, name, full_name_lower, \
                     description, default_branch, visibility, created_at, updated_at, deleted) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
                    &[
                        &repo.repo_id,
                        &repo.owner_id,
                        &repo.owner_name,
                        &repo.name,
                        &repo.full_name_lower,
                        &repo.description,
                        &repo.default_branch,
                        &repo.visibility,
                        &repo.created_at,
                        &repo.updated_at,
                        &repo.deleted,
                    ],
                )
                .await?;
        }
        Ok(())
    }

    pub async fn by_id(&self, repo_id: &str) -> Result<Option<RepoRecord>, StoreError> {
        let row = self
            .client
            .query_opt(&format!("{SELECT_COLUMNS} WHERE repo_id = $1"), &[&repo_id])
            .await?;
        Ok(row.as_ref().map(row_to_repo))
    }

    /// Resolve `owner/name`. The caller passes the pre-lowered form, which is
    /// what `repos_by_full_name_lower` indexes.
    pub async fn by_full_name(
        &self,
        full_name_lower: &str,
    ) -> Result<Option<RepoRecord>, StoreError> {
        let row = self
            .client
            .query_opt(
                &format!("{SELECT_COLUMNS} WHERE full_name_lower = $1"),
                &[&full_name_lower],
            )
            .await?;
        Ok(row.as_ref().map(row_to_repo))
    }

    /// A user's repositories, newest first.
    ///
    /// Keyset paginated on `repo_id`: UUIDv7 is time-ordered, so the id is the
    /// cursor and no OFFSET is involved.
    pub async fn for_owner(
        &self,
        owner_id: &str,
        before: Option<&str>,
        limit: i64,
    ) -> Result<Vec<RepoRecord>, StoreError> {
        // gres's parser rejects a bound parameter in LIMIT ("expected LIMIT
        // count, found Param"), so the count is interpolated. `clamp_limit`
        // makes that safe: the value is an integer we bound, never caller text.
        // TODO(gres:parameterized-limit)
        let limit = crate::clamp_limit(limit);
        let rows = match before {
            Some(cursor) => {
                self.client
                    .query(
                        &format!(
                            "{SELECT_COLUMNS} WHERE owner_id = $1 AND deleted = false \
                             AND repo_id < $2 ORDER BY repo_id DESC LIMIT {limit}"
                        ),
                        &[&owner_id, &cursor],
                    )
                    .await?
            }
            None => {
                self.client
                    .query(
                        &format!(
                            "{SELECT_COLUMNS} WHERE owner_id = $1 AND deleted = false \
                             ORDER BY repo_id DESC LIMIT {limit}"
                        ),
                        &[&owner_id],
                    )
                    .await?
            }
        };
        Ok(rows.iter().map(row_to_repo).collect())
    }
}

const SELECT_COLUMNS: &str = "SELECT repo_id, owner_id, owner_name, name, full_name_lower, \
     description, default_branch, visibility, created_at, updated_at, deleted FROM repos";

fn row_to_repo(row: &tokio_postgres::Row) -> RepoRecord {
    RepoRecord {
        repo_id: row.get(0),
        owner_id: row.get(1),
        owner_name: row.get(2),
        name: row.get(3),
        full_name_lower: row.get(4),
        description: row.get(5),
        default_branch: row.get(6),
        visibility: row.get(7),
        created_at: row.get(8),
        updated_at: row.get(9),
        deleted: row.get(10),
    }
}

/// The projector's durable cursor.
///
/// Read and written inside the same transaction as the rows a batch produces,
/// which is what makes projection exactly-once in effect despite at-least-once
/// delivery from the log.
pub struct CursorStore<'a> {
    client: &'a Client,
}

impl<'a> CursorStore<'a> {
    pub fn new(client: &'a Client) -> Self {
        Self { client }
    }

    /// Where to resume `topic` from. Zero when it has never been projected.
    pub async fn applied_offset(&self, topic: &str) -> Result<i64, StoreError> {
        let row = self
            .client
            .query_opt(
                "SELECT applied_offset FROM projector_state WHERE topic = $1",
                &[&topic],
            )
            .await?;
        Ok(row.map_or(0, |row| row.get(0)))
    }

    /// Record progress. Must run inside the caller's transaction.
    /// TODO(gres:on-conflict)
    pub async fn set_applied_offset(&self, topic: &str, offset: i64) -> Result<(), StoreError> {
        let updated = self
            .client
            .execute(
                "UPDATE projector_state SET applied_offset = $2, updated_at = $3 WHERE topic = $1",
                &[&topic, &offset, &forge_types::now()],
            )
            .await?;
        if updated == 0 {
            self.client
                .execute(
                    "INSERT INTO projector_state (topic, partition, applied_offset, updated_at) \
                     VALUES ($1, 0, $2, $3)",
                    &[&topic, &offset, &forge_types::now()],
                )
                .await?;
        }
        Ok(())
    }
}
