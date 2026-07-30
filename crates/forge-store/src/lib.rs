//! Read models, stored in crabka's `gres` Postgres engine.
//!
//! The log is authoritative. Most of this schema is derived from it and can be
//! dropped and rebuilt by replaying topics from offset zero — that is not a
//! theoretical property, it is the disaster-recovery procedure.
//!
//! ## Who writes what
//!
//! * The **projector** owns every projection table. Applies are `INSERT …
//!   ON CONFLICT DO UPDATE`, so replaying an event lands on the row it already
//!   wrote rather than beside it — idempotent because the statement says so,
//!   not because of who happens to be writing.
//! * A few tables are **operational** and are not derived from the log at all:
//!   `web_sessions`, `webhook_deliveries`, and the `access_tokens.last_used_at`
//!   column. Whichever service observes the event writes it directly. These do
//!   not survive the drill, which is accepted — the cost is that everyone is
//!   logged out and up to the delivery topic's retention window of integration
//!   diagnostics is gone, at the moment integrations are most likely broken.
//!
//! `access_tokens` is the one table two writers touch: the projector owns the
//! row, the web tier owns `last_used_at`. They are confined to disjoint columns
//! so the two never contend for the same value.
//!
//! `migrations/0001_schema.sql` labels each table with which kind it is, and is
//! the authority if these ever disagree.
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
mod ci;
mod hooks;
mod issues;
pub mod migrate;
mod pulls;
mod repos;
mod users;

pub use auth::{AccessToken, AuthStore, Session};
pub use ci::{CiStore, JobRecord, RunRecord};
pub use hooks::{DeliveryRecord, HookStore, WebhookRecord};
pub use issues::{CommentRecord, Counters, IssueRecord, IssueStore};
pub use pulls::{MergeCheck, Mergeable, PullRecord, PullStore, ReviewRecord};
pub use repos::{
    CI_ORCHESTRATOR, CursorStore, PROJECTOR, RepoRecord, RepoStore, WEBHOOK_MATCHER, WEBHOOK_WORKER,
};
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
    #[error("encoding a json column: {0}")]
    Json(serde_json::Error),
    #[error("connecting to gres at {dsn}: {source}")]
    Connect {
        /// Already redacted — see [`redact_dsn`]. This string reaches operator
        /// output, so it must not be the raw connection string.
        dsn: String,
        #[source]
        source: tokio_postgres::Error,
    },
    #[error("gres at {dsn} did not answer within {}s", waited.as_secs_f32())]
    ConnectTimeout {
        /// Already redacted, as above.
        dsn: String,
        /// How long the attempt was actually given — which is not necessarily
        /// the caller's budget, since a zero or tiny budget is floored so that
        /// it still buys a real attempt.
        waited: Duration,
    },
    #[error("{}", schema_mismatch_message(*found, *expected))]
    SchemaMismatch { found: Option<i64>, expected: i64 },
}

/// What to tell someone whose schema does not match their binary.
///
/// The remedy depends on the direction and the old message did not: it sent
/// everyone to `crabforge migrate`, which for a database *ahead* of the binary
/// has nothing to apply and no way to go back, so it would report success and
/// leave the server refusing to start for the same reason as before.
fn schema_mismatch_message(found: Option<i64>, expected: i64) -> String {
    match found {
        Some(found) if found > expected => format!(
            "schema is at version {found} but this build expects {expected}; it was migrated by a \
             newer build — deploy a build that matches, or reset the database and re-project"
        ),
        Some(found) => format!(
            "schema is at version {found} but this build expects {expected}; run `crabforge migrate`"
        ),
        None => format!(
            "no schema has been applied but this build expects version {expected}; run \
             `crabforge migrate`"
        ),
    }
}

/// Floor on a single connection attempt inside [`Store::connect_with_retry`].
const MIN_ATTEMPT: Duration = Duration::from_secs(1);

/// A connection string with its password removed.
///
/// Every message naming a DSN goes somewhere a person will read it — a doctor
/// report pasted into a ticket, a failed pre-start job in a CI log — so the
/// password must not be in it. Both libpq spellings are handled: the
/// `key=value` form's `password=` keyword, and the URI form's `user:pass@`.
///
/// Everything else is preserved, because the host, port and database name are
/// exactly what makes such a message useful.
pub fn redact_dsn(dsn: &str) -> String {
    const MASK: &str = "password=<redacted>";

    if let Some(scheme_end) = dsn.find("://") {
        // URI form. The password is between the first ':' after the scheme and
        // the '@' that ends the userinfo.
        let (scheme, rest) = dsn.split_at(scheme_end + 3);
        if let Some(at) = rest.find('@') {
            let (userinfo, host) = rest.split_at(at);
            let user = userinfo.split(':').next().unwrap_or(userinfo);
            let redacted = if userinfo.contains(':') {
                format!("{user}:<redacted>")
            } else {
                user.to_string()
            };
            return format!("{scheme}{redacted}{host}");
        }
        return dsn.to_string();
    }

    dsn.split_whitespace()
        .map(|part| {
            if part
                .split_once('=')
                .is_some_and(|(key, _)| key.eq_ignore_ascii_case("password"))
            {
                MASK
            } else {
                part
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
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
                dsn: redact_dsn(dsn),
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
    ///
    /// `within` is a real upper bound on the whole operation, including the
    /// individual connect attempts. That takes saying, because it is not what
    /// you get for free: `tokio_postgres::connect` has no timeout of its own
    /// unless the DSN carries `connect_timeout`, so a host that blackholes
    /// packets — a mistyped address, a dead network route — would otherwise sit
    /// in one attempt for the kernel's TCP timeout of roughly two minutes,
    /// whatever budget the caller asked for.
    pub async fn connect_with_retry(dsn: &str, within: Duration) -> Result<Self, StoreError> {
        let deadline = tokio::time::Instant::now() + within;
        let mut backoff = Duration::from_millis(100);
        loop {
            // Each attempt is bounded by what is left of the budget, with a
            // floor so that a zero or tiny budget still buys a real attempt
            // rather than an instant timeout. The cost is that the call can
            // overrun `within` by up to `MIN_ATTEMPT`, which beats "try once"
            // meaning "do not try".
            let attempt_budget = deadline
                .saturating_duration_since(tokio::time::Instant::now())
                .max(MIN_ATTEMPT);
            let attempt = tokio::time::timeout(attempt_budget, Self::connect(dsn)).await;

            let error = match attempt {
                Ok(Ok(store)) => return Ok(store),
                Ok(Err(e)) => e,
                Err(_) => StoreError::ConnectTimeout {
                    dsn: redact_dsn(dsn),
                    waited: attempt_budget,
                },
            };

            if tokio::time::Instant::now() >= deadline {
                return Err(error);
            }
            tracing::warn!(error = %error, "gres not ready; retrying");
            // Never sleep past the deadline: the point of the budget is that a
            // caller who asked for two seconds does not wait five.
            let nap = backoff.min(deadline.saturating_duration_since(tokio::time::Instant::now()));
            tokio::time::sleep(nap).await;
            backoff = (backoff * 2).min(Duration::from_secs(5));
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

    /// Cursors for `reader` — see [`repos::PROJECTOR`] and
    /// [`repos::WEBHOOK_MATCHER`] for the names in use.
    pub fn cursors<'a>(&'a self, reader: &'a str) -> CursorStore<'a> {
        CursorStore::new(&self.client, reader)
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

    pub fn hooks(&self) -> HookStore<'_> {
        HookStore::new(&self.client)
    }

    pub fn ci(&self) -> CiStore<'_> {
        CiStore::new(&self.client)
    }
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;

    #[test]
    fn a_password_is_removed_from_a_dsn_but_nothing_else_is() {
        // Both libpq spellings, because a message naming the DSN is exactly the
        // text an operator pastes into a ticket.
        check!(
            redact_dsn("host=db port=5433 user=forge password=hunter2 dbname=crab")
                == "host=db port=5433 user=forge password=<redacted> dbname=crab"
        );
        check!(
            redact_dsn("postgresql://forge:hunter2@db:5433/crab")
                == "postgresql://forge:<redacted>@db:5433/crab"
        );

        // The host, port and database survive — redacting them would leave a
        // message that says nothing.
        let redacted = redact_dsn("host=db port=5433 user=forge password=hunter2 dbname=crab");
        check!(redacted.contains("host=db"));
        check!(redacted.contains("port=5433"));
        check!(redacted.contains("dbname=crab"));
        check!(!redacted.contains("hunter2"));
    }

    #[test]
    fn a_dsn_without_a_password_is_left_alone() {
        let plain = "host=127.0.0.1 port=5433 user=forge dbname=crab";
        check!(redact_dsn(plain) == plain);
        check!(redact_dsn("postgresql://forge@db/crab") == "postgresql://forge@db/crab");
        check!(redact_dsn("") == "");
    }

    #[test]
    fn a_password_keyword_is_matched_however_it_is_cased() {
        // libpq keywords are case-insensitive, and a check that missed
        // `PASSWORD=` would leak exactly as badly as no check at all.
        check!(!redact_dsn("host=db PASSWORD=hunter2").contains("hunter2"));
        check!(!redact_dsn("host=db PassWord=hunter2").contains("hunter2"));
        // But a keyword that merely ends in "password" is a different setting.
        check!(redact_dsn("host=db sslpassword=x").contains("sslpassword=x"));
    }

    #[test]
    fn the_schema_mismatch_message_points_the_right_way() {
        // Behind: migrating is the fix.
        check!(schema_mismatch_message(Some(1), 2).contains("crabforge migrate"));
        check!(schema_mismatch_message(None, 1).contains("crabforge migrate"));
        // Ahead: it is not, and saying so is the whole point — migrate has
        // nothing to apply and would report success.
        let ahead = schema_mismatch_message(Some(3), 1);
        check!(!ahead.contains("crabforge migrate"));
        check!(ahead.contains("newer build"));
    }

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
