//! Schema migrations.
//!
//! There is no migration tool for gres, and the usual ones assume features it
//! does not have, so this is a deliberately small one:
//!
//! * Migrations are numbered SQL files embedded in the binary, applied in
//!   order.
//! * There are no down-migrations. The event log is the source of truth; the
//!   way back from a bad schema is to drop the tables and re-project, which is
//!   also the disaster-recovery drill.
//! * A ledger table records what has been applied.
//!
//! While the project is pre-deployment there is exactly one migration and it is
//! edited in place, because no database has the old schema and an incremental
//! one would describe a state nothing was ever in. The rule that a merged
//! migration is immutable starts applying the moment something is deployed —
//! from then on `0001_schema.sql` is frozen and changes arrive as `0002_*`.
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
pub const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    name: "schema",
    sql: include_str!("../../../migrations/0001_schema.sql"),
}];

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

/// The highest migration this database has applied, or `None` when none has.
///
/// Reads without writing. A database that has never been migrated has no ledger
/// table at all, and that reads as `None` rather than as an error: no caller
/// acts differently on "no ledger" than on "empty ledger", and creating the
/// table here would mean `crabforge doctor` — whose whole job is to report on a
/// database — quietly performing DDL against it.
pub async fn current_version(client: &Client) -> Result<Option<i64>, StoreError> {
    match client
        .query("SELECT version FROM schema_migrations", &[])
        .await
    {
        Ok(rows) => Ok(rows.iter().map(|row| row.get::<_, i64>(0)).max()),
        Err(e) if no_such_table(&e) => Ok(None),
        Err(e) => Err(StoreError::Sql(e)),
    }
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

/// Whether an error means "no such table" — matched on SQLSTATE, as above.
fn no_such_table(error: &tokio_postgres::Error) -> bool {
    matches!(
        error.code(),
        Some(&tokio_postgres::error::SqlState::UNDEFINED_TABLE)
    )
}

async fn applied_versions(client: &Client) -> Result<Vec<i64>, StoreError> {
    let rows = client
        .query("SELECT version FROM schema_migrations", &[])
        .await?;
    Ok(rows.iter().map(|row| row.get::<_, i64>(0)).collect())
}

async fn apply(client: &Client, migration: &Migration) -> Result<(), StoreError> {
    // Run the DDL and record it. gres does not make this atomic — a rolled-back
    // CREATE TABLE leaves the table behind (measured; see docs/gres-gaps.md) —
    // so a failure mid-migration leaves the schema partly applied and the ledger
    // row absent. Nothing here tolerates a repeated statement (the `42P07`
    // handling is in `ensure_ledger` and covers only the ledger), so re-running
    // fails on the first object the previous attempt created. Recovery is to
    // drop the schema and re-project, which is the disaster-recovery drill.
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
        // Nearly vacuous while there is one migration, and deliberately kept:
        // it starts doing real work the moment a second is appended, and a gap
        // or a duplicate would otherwise be found at deploy time.
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
