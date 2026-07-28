//! Schema migrations.
//!
//! There is no migration tool for gres, and the usual ones assume features it
//! does not have, so this is a deliberately small one:
//!
//! * Migrations are numbered SQL files embedded in the binary, applied in
//!   order, never edited once merged.
//! * There are no down-migrations. The event log is the source of truth; the
//!   way back from a bad schema is to drop the tables and re-project, which is
//!   also the disaster-recovery drill.
//! * A ledger table records what has been applied.
//!
//! Concurrency is by convention rather than locking — gres has no advisory
//! locks. Migrations run from `crabforge migrate` (a pre-start job in
//! production), and services refuse to serve if the ledger is behind, so a
//! service that starts against an un-migrated database fails loudly instead of
//! erroring one query at a time.

use tokio_postgres::Client;

use crate::StoreError;

/// A migration, embedded at compile time so a deployed binary always carries
/// the schema it expects.
pub struct Migration {
    pub version: i64,
    pub name: &'static str,
    pub sql: &'static str,
}

/// Every migration, in application order.
pub const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "identity",
        sql: include_str!("../../../migrations/0001_identity.sql"),
    },
    Migration {
        version: 2,
        name: "auth_and_issues",
        sql: include_str!("../../../migrations/0002_auth_and_issues.sql"),
    },
];

const LEDGER_DDL: &str = "CREATE TABLE schema_migrations (
    version    int8 NOT NULL,
    name       text NOT NULL,
    applied_at timestamptz NOT NULL
)";

/// Apply every migration the database has not seen.
///
/// Idempotent, so the dev loop can run it on every boot.
pub async fn run(client: &Client) -> Result<Vec<i64>, StoreError> {
    ensure_ledger(client).await?;
    let applied = applied_versions(client).await?;

    let mut newly_applied = Vec::new();
    for migration in MIGRATIONS {
        if applied.contains(&migration.version) {
            continue;
        }
        tracing::info!(
            version = migration.version,
            name = migration.name,
            "applying migration"
        );
        apply(client, migration).await?;
        newly_applied.push(migration.version);
    }
    Ok(newly_applied)
}

/// The highest migration this database has applied, or `None` when empty.
pub async fn current_version(client: &Client) -> Result<Option<i64>, StoreError> {
    ensure_ledger(client).await?;
    Ok(applied_versions(client).await?.into_iter().max())
}

/// The version this binary expects.
pub fn expected_version() -> i64 {
    MIGRATIONS.last().map_or(0, |m| m.version)
}

/// Whether the database is at the version this binary was built against.
pub async fn is_current(client: &Client) -> Result<bool, StoreError> {
    Ok(current_version(client).await?.unwrap_or(0) == expected_version())
}

async fn ensure_ledger(client: &Client) -> Result<(), StoreError> {
    // No `CREATE TABLE IF NOT EXISTS`: attempt it and accept the failure that
    // means "already there". Distinguishing that from a real failure by message
    // is unpleasant, but the alternative is querying the catalog, which is a
    // larger gres surface to depend on.
    // TODO(gres:create-if-not-exists)
    match client.batch_execute(LEDGER_DDL).await {
        Ok(()) => Ok(()),
        Err(e) if already_exists(&e) => Ok(()),
        Err(e) => Err(StoreError::Sql(e)),
    }
}

/// Whether an error means "that object is already there".
///
/// Matched on SQLSTATE rather than message text: gres returns the standard
/// codes, and messages are not a stable interface.
fn already_exists(error: &tokio_postgres::Error) -> bool {
    matches!(
        error.code(),
        Some(&tokio_postgres::error::SqlState::DUPLICATE_TABLE)
    )
}

async fn applied_versions(client: &Client) -> Result<Vec<i64>, StoreError> {
    let rows = client
        .query("SELECT version FROM schema_migrations", &[])
        .await?;
    Ok(rows.iter().map(|row| row.get::<_, i64>(0)).collect())
}

async fn apply(client: &Client, migration: &Migration) -> Result<(), StoreError> {
    // Run the DDL and record it. Whether gres makes this atomic is an open
    // question (see docs/gres-gaps.md); if it does not, a failure mid-migration
    // leaves the schema partly applied and the ledger row absent, which
    // `crabforge doctor` reports as a mismatch rather than silently retrying.
    // TODO(gres:transactional-ddl)
    client.batch_execute(migration.sql).await?;
    client
        .execute(
            "INSERT INTO schema_migrations (version, name, applied_at) VALUES ($1, $2, $3)",
            &[&migration.version, &migration.name, &forge_types::now()],
        )
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;

    #[test]
    fn migrations_are_numbered_consecutively_from_one() {
        // A gap or a duplicate means a merge went wrong, and would be found at
        // deploy time rather than here.
        for (index, migration) in MIGRATIONS.iter().enumerate() {
            check!(
                migration.version == index as i64 + 1,
                "migration {} is out of sequence",
                migration.name
            );
        }
    }

    #[test]
    fn every_migration_carries_sql() {
        for migration in MIGRATIONS {
            check!(
                !migration.sql.trim().is_empty(),
                "{} is empty",
                migration.name
            );
        }
    }

    #[test]
    fn expected_version_tracks_the_last_migration() {
        check!(expected_version() == MIGRATIONS.len() as i64);
    }
}
