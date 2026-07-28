//! Read models, stored in crabka's `gres` Postgres engine.
//!
//! Everything here is derived state: the log is authoritative, and any table in
//! this schema can be dropped and rebuilt by replaying topics from offset zero.
//! That is not a theoretical property — it is the disaster-recovery procedure.
//!
//! ## Who writes what
//!
//! * The **projector** owns every projection table. It is the single writer, so
//!   read-then-write is safe without `ON CONFLICT`, which gres does not have.
//! * The **web tier** owns only session and token-usage rows: operational state
//!   with no place in domain history.
//!
//! The two sets are disjoint, so gres never sees a write conflict between them.
//!
//! ## Dialect
//!
//! Standard PostgreSQL, with today's gres limitations worked around behind
//! `TODO(gres:*)` markers and tracked in `docs/gres-gaps.md`. The workarounds
//! are confined to this crate so the rest of the forge is unaware of them.

use std::time::Duration;

use tokio_postgres::{Client, NoTls};

pub mod migrate;
mod repos;
mod users;

pub use repos::{CursorStore, RepoRecord, RepoStore};
pub use users::{UserRecord, UserStore};

/// Largest page any listing will return.
pub const MAX_PAGE_SIZE: i64 = 100;

/// Bound a caller-supplied page size.
///
/// Also the safety argument for interpolating the count into `LIMIT`: gres
/// cannot bind a parameter there (TODO(gres:parameterized-limit)), so the value
/// is formatted into the SQL text. Passing it through here guarantees what
/// reaches the query is an integer in a fixed range — never caller-controlled
/// text, and never a page large enough to be a denial-of-service lever.
pub(crate) fn clamp_limit(requested: i64) -> i64 {
    requested.clamp(1, MAX_PAGE_SIZE)
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sql: {0}")]
    Sql(#[from] tokio_postgres::Error),
    #[error("connecting to gres at {dsn}: {source}")]
    Connect {
        dsn: String,
        #[source]
        source: tokio_postgres::Error,
    },
    #[error(
        "schema is at version {found:?} but this build expects {expected}; run `crabforge migrate`"
    )]
    SchemaMismatch { found: Option<i64>, expected: i64 },
}

/// A connection to gres.
///
/// One connection, not a pool: the projector is a single writer and the read
/// path is not yet hot enough to need more. Pooling belongs here when it does.
pub struct Store {
    client: Client,
}

impl Store {
    /// Connect and spawn the connection driver.
    pub async fn connect(dsn: &str) -> Result<Self, StoreError> {
        let (client, connection) = tokio_postgres::connect(dsn, NoTls)
            .await
            .map_err(|source| StoreError::Connect {
                dsn: dsn.to_string(),
                source,
            })?;

        // tokio-postgres splits the client from the I/O driver; the driver has
        // to be polled or nothing moves.
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                tracing::error!(error = %e, "gres connection closed");
            }
        });

        Ok(Self { client })
    }

    /// Connect, retrying while gres warms up.
    ///
    /// Substrate-mode gres replays its whole write-ahead log before accepting
    /// connections, and crabka has not implemented checkpointing yet, so a cold
    /// start can take a while on a busy forge. Retrying here means the forge
    /// starts alongside its database rather than crash-looping until it is
    /// ready.
    pub async fn connect_with_retry(dsn: &str, within: Duration) -> Result<Self, StoreError> {
        let deadline = tokio::time::Instant::now() + within;
        let mut backoff = Duration::from_millis(100);
        loop {
            match Self::connect(dsn).await {
                Ok(store) => return Ok(store),
                Err(e) if tokio::time::Instant::now() >= deadline => return Err(e),
                Err(e) => {
                    tracing::warn!(error = %e, "gres not ready; retrying");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(5));
                }
            }
        }
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Apply any pending migrations.
    pub async fn migrate(&self) -> Result<Vec<i64>, StoreError> {
        migrate::run(&self.client).await
    }

    /// Fail unless the schema matches what this binary was built against.
    ///
    /// Called at startup so a version skew is one clear error rather than a
    /// stream of confusing query failures.
    pub async fn require_current_schema(&self) -> Result<(), StoreError> {
        let found = migrate::current_version(&self.client).await?;
        let expected = migrate::expected_version();
        if found == Some(expected) {
            Ok(())
        } else {
            Err(StoreError::SchemaMismatch { found, expected })
        }
    }

    pub fn users(&self) -> UserStore<'_> {
        UserStore::new(&self.client)
    }

    pub fn repos(&self) -> RepoStore<'_> {
        RepoStore::new(&self.client)
    }

    pub fn cursors(&self) -> CursorStore<'_> {
        CursorStore::new(&self.client)
    }
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;

    #[test]
    fn page_sizes_are_bounded_in_both_directions() {
        check!(clamp_limit(25) == 25);
        check!(
            clamp_limit(0) == 1,
            "a zero-row page is never what the caller meant"
        );
        check!(clamp_limit(-5) == 1);
        check!(clamp_limit(i64::MAX) == MAX_PAGE_SIZE);
    }

    #[test]
    fn clamped_limits_are_always_plain_digits() {
        // The safety property behind interpolating LIMIT into SQL text.
        for requested in [-1, 0, 1, 50, 1_000_000, i64::MIN, i64::MAX] {
            let rendered = clamp_limit(requested).to_string();
            check!(
                rendered.chars().all(|c| c.is_ascii_digit()),
                "limit rendered as {rendered:?}"
            );
        }
    }
}
