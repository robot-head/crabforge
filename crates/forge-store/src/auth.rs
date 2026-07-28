//! Sessions and access tokens.
//!
//! Neither a session cookie nor a token is stored. What is stored is its
//! SHA-256, and lookups hash the presented credential and compare. A database
//! leak then yields nothing that can be replayed, and the fixed-width hash is
//! also a better index key than a variable-length secret.

use time::OffsetDateTime;
use tokio_postgres::Client;

use crate::StoreError;

/// A logged-in browser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub user_id: String,
    pub created_at: OffsetDateTime,
    pub expires_at: OffsetDateTime,
}

/// A personal access token, as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessToken {
    pub token_id: String,
    pub user_id: String,
    pub name: String,
    pub token_hash: String,
    /// Space-separated. Parsed by `forge-auth`, which owns the scope model.
    pub scopes: String,
    pub created_at: OffsetDateTime,
    pub expires_at: Option<OffsetDateTime>,
    pub revoked_at: Option<OffsetDateTime>,
    pub last_used_at: Option<OffsetDateTime>,
}

impl AccessToken {
    /// Whether this token may still be used.
    pub fn is_usable(&self, now: OffsetDateTime) -> bool {
        self.revoked_at.is_none() && self.expires_at.is_none_or(|expiry| expiry > now)
    }
}

pub struct AuthStore<'a> {
    client: &'a Client,
}

impl<'a> AuthStore<'a> {
    pub fn new(client: &'a Client) -> Self {
        Self { client }
    }

    // ── sessions: written directly by the web tier ───────────────────────────

    /// Record a new session.
    pub async fn create_session(
        &self,
        session_hash: &str,
        user_id: &str,
        expires_at: OffsetDateTime,
    ) -> Result<(), StoreError> {
        self.client
            .execute(
                "INSERT INTO web_sessions (session_hash, user_id, created_at, expires_at) \
                 VALUES ($1, $2, $3, $4)",
                &[&session_hash, &user_id, &forge_types::now(), &expires_at],
            )
            .await?;
        Ok(())
    }

    /// Look up a session by the hash of its cookie.
    ///
    /// An expired session reads as absent rather than being returned with a
    /// flag: every caller would have to check, and one that forgot would be a
    /// silent authentication bypass.
    pub async fn session(&self, session_hash: &str) -> Result<Option<Session>, StoreError> {
        let row = self
            .client
            .query_opt(
                "SELECT user_id, created_at, expires_at FROM web_sessions WHERE session_hash = $1",
                &[&session_hash],
            )
            .await?;

        Ok(row.and_then(|row| {
            let session = Session {
                user_id: row.get(0),
                created_at: row.get(1),
                expires_at: row.get(2),
            };
            (session.expires_at > forge_types::now()).then_some(session)
        }))
    }

    /// End one session.
    pub async fn delete_session(&self, session_hash: &str) -> Result<(), StoreError> {
        self.client
            .execute(
                "DELETE FROM web_sessions WHERE session_hash = $1",
                &[&session_hash],
            )
            .await?;
        Ok(())
    }

    /// End every session belonging to a user, as after a password change.
    pub async fn delete_sessions_for(&self, user_id: &str) -> Result<u64, StoreError> {
        Ok(self
            .client
            .execute("DELETE FROM web_sessions WHERE user_id = $1", &[&user_id])
            .await?)
    }

    /// Remove sessions that have already expired.
    pub async fn purge_expired_sessions(&self) -> Result<u64, StoreError> {
        Ok(self
            .client
            .execute(
                "DELETE FROM web_sessions WHERE expires_at < $1",
                &[&forge_types::now()],
            )
            .await?)
    }

    // ── tokens: projected from the log, except last_used_at ──────────────────

    /// Apply a token from the event log.
    /// TODO(gres:on-conflict)
    pub async fn upsert_token(&self, token: &AccessToken) -> Result<(), StoreError> {
        let existing = self
            .client
            .query_opt(
                "SELECT token_id FROM access_tokens WHERE token_id = $1",
                &[&token.token_id],
            )
            .await?;

        if existing.is_some() {
            self.client
                .execute(
                    "UPDATE access_tokens SET name = $2, scopes = $3, expires_at = $4, \
                     revoked_at = $5 WHERE token_id = $1",
                    &[
                        &token.token_id,
                        &token.name,
                        &token.scopes,
                        &token.expires_at,
                        &token.revoked_at,
                    ],
                )
                .await?;
        } else {
            self.client
                .execute(
                    "INSERT INTO access_tokens (token_id, user_id, name, token_hash, scopes, \
                     created_at, expires_at, revoked_at, last_used_at) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
                    &[
                        &token.token_id,
                        &token.user_id,
                        &token.name,
                        &token.token_hash,
                        &token.scopes,
                        &token.created_at,
                        &token.expires_at,
                        &token.revoked_at,
                        &token.last_used_at,
                    ],
                )
                .await?;
        }
        Ok(())
    }

    /// Look up a token by the hash of the presented secret.
    pub async fn token_by_hash(&self, token_hash: &str) -> Result<Option<AccessToken>, StoreError> {
        let row = self
            .client
            .query_opt(
                &format!("{TOKEN_COLUMNS} WHERE token_hash = $1"),
                &[&token_hash],
            )
            .await?;
        Ok(row.as_ref().map(row_to_token))
    }

    /// Every token a user has, for the settings page.
    pub async fn tokens_for(&self, user_id: &str) -> Result<Vec<AccessToken>, StoreError> {
        let rows = self
            .client
            .query(&format!("{TOKEN_COLUMNS} WHERE user_id = $1"), &[&user_id])
            .await?;
        Ok(rows.iter().map(row_to_token).collect())
    }

    /// Record that a token was just used.
    ///
    /// Written directly rather than as an event: recording every use in the log
    /// would put a write on it for every git fetch, which is a lot of history
    /// for a field nobody audits.
    pub async fn touch_token(&self, token_id: &str) -> Result<(), StoreError> {
        self.client
            .execute(
                "UPDATE access_tokens SET last_used_at = $2 WHERE token_id = $1",
                &[&token_id, &forge_types::now()],
            )
            .await?;
        Ok(())
    }
}

const TOKEN_COLUMNS: &str = "SELECT token_id, user_id, name, token_hash, scopes, created_at, \
     expires_at, revoked_at, last_used_at FROM access_tokens";

fn row_to_token(row: &tokio_postgres::Row) -> AccessToken {
    AccessToken {
        token_id: row.get(0),
        user_id: row.get(1),
        name: row.get(2),
        token_hash: row.get(3),
        scopes: row.get(4),
        created_at: row.get(5),
        expires_at: row.get(6),
        revoked_at: row.get(7),
        last_used_at: row.get(8),
    }
}

#[cfg(test)]
mod tests {
    use assert2::check;
    use time::Duration;

    use super::*;

    fn token(revoked: bool, expires_in: Option<Duration>) -> AccessToken {
        let now = forge_types::now();
        AccessToken {
            token_id: "t".into(),
            user_id: "u".into(),
            name: "laptop".into(),
            token_hash: "h".into(),
            scopes: "repo:read".into(),
            created_at: now,
            expires_at: expires_in.map(|d| now + d),
            revoked_at: revoked.then_some(now),
            last_used_at: None,
        }
    }

    #[test]
    fn a_fresh_token_is_usable() {
        check!(token(false, None).is_usable(forge_types::now()));
        check!(token(false, Some(Duration::days(30))).is_usable(forge_types::now()));
    }

    #[test]
    fn a_revoked_token_is_never_usable() {
        check!(!token(true, None).is_usable(forge_types::now()));
        // Even one that has not expired.
        check!(!token(true, Some(Duration::days(30))).is_usable(forge_types::now()));
    }

    #[test]
    fn an_expired_token_is_not_usable() {
        check!(!token(false, Some(Duration::seconds(-1))).is_usable(forge_types::now()));
    }
}
