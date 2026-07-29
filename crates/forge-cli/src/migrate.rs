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

use anyhow::{Context as _, Result};
use forge_store::{Store, migrate};

/// How long to wait for gres by default, matching the server's budget.
///
/// Substrate-mode gres replays its whole write-ahead log on a cold start, and
/// crabka has not implemented checkpointing yet, so a migration job started
/// alongside the database has to be willing to wait rather than crash-loop.
/// Overridable because two minutes is right for a pre-start job and much too
/// long for someone at a terminal who has mistyped a port.
pub const DEFAULT_WAIT_SECS: u64 = 120;

pub async fn run(dsn: &str, wait: Duration) -> Result<()> {
    let store = Store::connect_with_retry(dsn, wait)
        .await
        .with_context(|| format!("connecting to gres at {dsn}"))?;

    let applied = store.migrate().await.context("applying migrations")?;

    if applied.is_empty() {
        tracing::info!(
            version = migrate::expected_version(),
            "schema is already current"
        );
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
