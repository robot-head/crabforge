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

use refinement_types::{Refinement, int};
use tokio_postgres::{Client, NoTls};

mod auth;
mod issues;
pub mod migrate;
mod pulls;
mod repos;
mod users;

pub use auth::{AccessToken, AuthStore, Session};
pub use issues::{CommentRecord, Counters, IssueRecord, IssueStore};
pub use pulls::{Mergeable, PullRecord, PullStore, ReviewRecord};
pub use repos::{CursorStore, RepoRecord, RepoStore};
pub use users::{UserRecord, UserStore};

/// Largest page any listing will return.
pub const MAX_PAGE_SIZE: i64 = 100;

/// A page size known to be within `1..=MAX_PAGE_SIZE`.
///
/// This one carries real weight. gres cannot bind a parameter in `LIMIT`
/// (TODO(gres:parameterized-limit)), so the count is formatted into the SQL
/// text — and a value interpolated into SQL needs a guarantee, not a habit.
///
/// Because the query functions take a `PageSize` rather than an `i64`, that
/// guarantee is the compiler's: the only way to obtain one is [`page_size`],
/// which clamps, or `PageSize::refine`, which rejects. A future caller cannot
/// forget to validate, because there is nothing to forget — an unvalidated
/// integer will not typecheck.
pub type PageSize = Refinement<i64, int::i64::Closed<1, MAX_PAGE_SIZE>>;

/// Clamp a caller-supplied page size into range.
///
/// Out-of-range requests are clamped rather than rejected: a client asking for
/// 1000 rows wants "as many as you will give me", and an error would be a worse
/// answer than a full page.
pub fn page_size(requested: i64) -> PageSize {
    PageSize::refine(requested.clamp(1, MAX_PAGE_SIZE))
        .expect("a clamped value is in range by construction")
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

    pub fn auth(&self) -> AuthStore<'_> {
        AuthStore::new(&self.client)
    }

    pub fn issues(&self) -> IssueStore<'_> {
        IssueStore::new(&self.client)
    }

    pub fn pulls(&self) -> PullStore<'_> {
        PullStore::new(&self.client)
    }
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;

    #[test]
    fn page_sizes_are_bounded_in_both_directions() {
        check!(*page_size(25) == 25);
        check!(
            *page_size(0) == 1,
            "a zero-row page is never what the caller meant"
        );
        check!(*page_size(-5) == 1);
        check!(*page_size(i64::MAX) == MAX_PAGE_SIZE);
    }

    #[test]
    fn an_out_of_range_page_size_cannot_be_constructed_directly() {
        // The property the type provides: interpolating a `PageSize` into SQL
        // is safe because no out-of-range value of that type exists.
        check!(PageSize::refine(0).is_err());
        check!(PageSize::refine(MAX_PAGE_SIZE + 1).is_err());
        check!(PageSize::refine(i64::MIN).is_err());
        check!(PageSize::refine(1).is_ok());
        check!(PageSize::refine(MAX_PAGE_SIZE).is_ok());
    }

    #[test]
    fn every_page_size_renders_as_plain_digits() {
        for requested in [-1, 0, 1, 50, 1_000_000, i64::MIN, i64::MAX] {
            let rendered = page_size(requested).to_string();
            check!(
                rendered.chars().all(|c| c.is_ascii_digit()),
                "page size rendered as {rendered:?}"
            );
        }
    }
}
