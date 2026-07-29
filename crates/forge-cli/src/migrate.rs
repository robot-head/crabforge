//! Apply the schema.
//!
//! Split out from `bootstrap` rather than folded into it because the two
//! provision different things and fail for different reasons: topics live on
//! the broker, the schema lives in gres, and an operator who has just been told
//! "schema is at version None but this build expects 1" wants to run the second
//! without re-running the first.
//!
//! Idempotent, so the dev loop can run it on every boot and a production
//! pre-start job can retry.

use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use forge_store::{Store, migrate, redact_dsn};

/// How long to wait for gres by default, matching the server's budget.
///
/// Substrate-mode gres replays its whole write-ahead log on a cold start, and
/// crabka has not implemented checkpointing yet, so a migration job started
/// alongside the database has to be willing to wait rather than crash-loop.
/// Overridable because two minutes is right for a pre-start job and much too
/// long for someone at a terminal who has mistyped a port.
pub const DEFAULT_WAIT_SECS: u64 = 120;

/// Largest accepted `--wait`, one day.
///
/// The budget is turned into an `Instant`, and adding a large enough duration to
/// one panics. Anything past a day is a typo rather than a wait anyone means.
pub const MAX_WAIT_SECS: u64 = 24 * 60 * 60;

pub async fn run(dsn: &str, wait: Duration) -> Result<()> {
    let store = Store::connect_with_retry(dsn, wait)
        .await
        .with_context(|| format!("connecting to gres at {}", redact_dsn(dsn)))?;

    let applied = store.migrate().await.context("applying migrations")?;

    // The runner only skips versions already in the ledger; it never compares
    // the ledger's high-water mark to what this build knows. So a database
    // migrated by a *newer* build applies nothing and would otherwise be
    // reported as current — which closes a loop with no exit, because the
    // server refuses to start for exactly that reason and its error sends the
    // operator here. Fail instead, and say what would actually help.
    let expected = migrate::expected_version();
    let found = migrate::current_version(store.client())
        .await
        .context("re-reading the migration ledger")?;
    if found.is_some_and(|found| found > expected) {
        bail!(
            "the database is at schema version {} but this build only knows version {expected}; \
             it was migrated by a newer build. There is nothing to apply and no down-migrations: \
             deploy a build that matches, or reset the database and re-project.",
            found.unwrap_or_default()
        );
    }

    if applied.is_empty() {
        tracing::info!(version = expected, "schema is already current");
    } else {
        for version in &applied {
            let name = migrate::MIGRATIONS
                .iter()
                .find(|m| m.version == *version)
                .map_or("?", |m| m.name);
            tracing::info!(version, name, "applied");
        }
        tracing::info!(
            count = applied.len(),
            version = migrate::expected_version(),
            "schema is now current"
        );
    }
    Ok(())
}
