//! The `users` read model.

use time::OffsetDateTime;
use tokio_postgres::Client;

use crate::StoreError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserRecord {
    pub user_id: String,
    pub username: String,
    pub username_lower: String,
    pub email: String,
    pub password_hash: String,
    pub display_name: Option<String>,
    pub bio: Option<String>,
    pub state: String,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

pub struct UserStore<'a> {
    client: &'a Client,
}

impl<'a> UserStore<'a> {
    pub fn new(client: &'a Client) -> Self {
        Self { client }
    }

    /// Insert a user, or update it if the projector is replaying.
    ///
    /// Replaces `INSERT … ON CONFLICT`, which gres does not have. Safe without
    /// one because the projector is the only writer of this table: no other
    /// session can insert the same row between the read and the write.
    /// TODO(gres:on-conflict)
    pub async fn upsert(&self, user: &UserRecord) -> Result<(), StoreError> {
        let existing = self
            .client
            .query_opt(
                "SELECT user_id FROM users WHERE user_id = $1",
                &[&user.user_id],
            )
            .await?;

        if existing.is_some() {
            self.client
                .execute(
                    "UPDATE users SET username = $2, username_lower = $3, email = $4, \
                     password_hash = $5, display_name = $6, bio = $7, state = $8, updated_at = $9 \
                     WHERE user_id = $1",
                    &[
                        &user.user_id,
                        &user.username,
                        &user.username_lower,
                        &user.email,
                        &user.password_hash,
                        &user.display_name,
                        &user.bio,
                        &user.state,
                        &user.updated_at,
                    ],
                )
                .await?;
        } else {
            self.client
                .execute(
                    "INSERT INTO users (user_id, username, username_lower, email, password_hash, \
                     display_name, bio, state, created_at, updated_at) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
                    &[
                        &user.user_id,
                        &user.username,
                        &user.username_lower,
                        &user.email,
                        &user.password_hash,
                        &user.display_name,
                        &user.bio,
                        &user.state,
                        &user.created_at,
                        &user.updated_at,
                    ],
                )
                .await?;
        }
        Ok(())
    }

    pub async fn by_id(&self, user_id: &str) -> Result<Option<UserRecord>, StoreError> {
        let row = self
            .client
            .query_opt(&format!("{SELECT_COLUMNS} WHERE user_id = $1"), &[&user_id])
            .await?;
        Ok(row.as_ref().map(row_to_user))
    }

    /// Look up by name. Hits `users_by_username_lower`, so callers must pass an
    /// already-lowercased value — gres has no expression indexes, and a
    /// `lower(username) = $1` predicate would scan.
    pub async fn by_username_lower(&self, lower: &str) -> Result<Option<UserRecord>, StoreError> {
        let row = self
            .client
            .query_opt(
                &format!("{SELECT_COLUMNS} WHERE username_lower = $1"),
                &[&lower],
            )
            .await?;
        Ok(row.as_ref().map(row_to_user))
    }

    pub async fn count(&self) -> Result<i64, StoreError> {
        let row = self
            .client
            .query_one("SELECT count(*) FROM users", &[])
            .await?;
        Ok(row.get(0))
    }
}

const SELECT_COLUMNS: &str = "SELECT user_id, username, username_lower, email, password_hash, \
     display_name, bio, state, created_at, updated_at FROM users";

fn row_to_user(row: &tokio_postgres::Row) -> UserRecord {
    UserRecord {
        user_id: row.get(0),
        username: row.get(1),
        username_lower: row.get(2),
        email: row.get(3),
        password_hash: row.get(4),
        display_name: row.get(5),
        bio: row.get(6),
        state: row.get(7),
        created_at: row.get(8),
        updated_at: row.get(9),
    }
}
