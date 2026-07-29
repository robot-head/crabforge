//! The operator commands, against a real gres.
//!
//! These run the shipped binary rather than calling library functions, because
//! the bug they exist to prevent was not in any function: `crabforge migrate`
//! was named in the server's error message and in the runner's own docs while
//! no such subcommand existed, so a forge could not be brought up from a clean
//! database by any documented path. Only invoking the binary catches that.

use std::process::Stdio;

use assert2::check;
use forge_testkit::require_gres;
use tokio::process::Command;

/// Run `crabforge` with the given arguments against `dsn`.
///
/// Returns (success, combined output).
async fn crabforge(dsn: &str, args: &[&str]) -> (bool, String) {
    let binary = env!("CARGO_BIN_EXE_crabforge");
    let output = Command::new(binary)
        .arg("--dsn")
        .arg(dsn)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .expect("running crabforge");

    let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&output.stderr));
    (output.status.success(), combined)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn migrate_applies_the_schema_to_an_empty_database() {
    let Some(gres) = require_gres().await else {
        return;
    };
    let dsn = gres.dsn();

    let (ok, output) = crabforge(&dsn, &["migrate"]).await;
    check!(ok, "migrate failed:\n{output}");
    check!(
        output.contains("applied"),
        "no migration reported:\n{output}"
    );

    // The schema is really there, not merely reported.
    let store = forge_store::Store::connect(&dsn).await.unwrap();
    check!(
        forge_store::migrate::is_current(store.client())
            .await
            .unwrap()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn migrate_is_safe_to_run_again() {
    // `just dev-up` runs it unconditionally on every boot.
    let Some(gres) = require_gres().await else {
        return;
    };
    let dsn = gres.dsn();

    let (ok, _) = crabforge(&dsn, &["migrate"]).await;
    check!(ok);

    let (ok, output) = crabforge(&dsn, &["migrate"]).await;
    check!(ok, "the second run failed:\n{output}");
    check!(
        output.contains("already current"),
        "expected it to report no work; got:\n{output}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_doctor_names_the_command_that_fixes_an_unmigrated_database() {
    // The whole point of the exercise: the fix text has to name a command that
    // exists, and the check has to notice the schema is missing at all.
    let Some(gres) = require_gres().await else {
        return;
    };
    let dsn = gres.dsn();

    let (ok, output) = crabforge(&dsn, &["doctor"]).await;
    check!(
        !ok,
        "doctor should fail on an unmigrated database:\n{output}"
    );
    check!(
        output.contains("schema"),
        "no schema check in the report:\n{output}"
    );
    check!(
        output.contains("crabforge migrate"),
        "the fix did not name the command:\n{output}"
    );

    // And that command actually resolves.
    let (ok, _) = crabforge(&dsn, &["migrate"]).await;
    check!(ok, "the fix the doctor recommends does not run");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_doctor_reports_a_migrated_schema_as_healthy() {
    let Some(gres) = require_gres().await else {
        return;
    };
    let dsn = gres.dsn();
    let (ok, _) = crabforge(&dsn, &["migrate"]).await;
    check!(ok);

    let (_, output) = crabforge(&dsn, &["doctor"]).await;
    // The broker is not running in this test, so the report as a whole fails.
    // What matters is that the schema line passes rather than being reported as
    // missing — a doctor that blamed the schema for a broker outage would send
    // an operator to reset a database that is fine.
    let schema_line = output
        .lines()
        .find(|l| l.contains("schema"))
        .unwrap_or_default();
    check!(
        schema_line.contains("ok") && schema_line.contains("version"),
        "expected a passing schema check; got: {schema_line:?}\nfull output:\n{output}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreachable_database_is_reported_as_such_and_not_as_a_missing_schema() {
    // Two different problems with two different fixes. Telling someone to run
    // `crabforge migrate` when gres is simply not running wastes the one piece
    // of information the doctor exists to provide.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let dsn = format!("host=127.0.0.1 port={port} user=forge dbname=crab connect_timeout=2");

    let (ok, output) = crabforge(&dsn, &["doctor"]).await;
    check!(!ok);
    let schema_line = output
        .lines()
        .find(|l| l.contains("schema"))
        .unwrap_or_default();
    check!(
        schema_line.contains("cannot reach gres") || schema_line.contains("did not answer"),
        "expected an unreachable-database report; got: {schema_line:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn migrate_reports_an_unreachable_database_rather_than_hanging() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let dsn = format!("host=127.0.0.1 port={port} user=forge dbname=crab connect_timeout=1");

    // `--wait 2` rather than the two-minute default: this asserts that the
    // budget is honoured and bounded, which does not need the whole budget.
    let (ok, output) = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        crabforge(&dsn, &["migrate", "--wait", "2"]),
    )
    .await
    .expect("migrate should give up rather than hang forever");

    check!(!ok);
    check!(
        output.contains("connecting to gres"),
        "the failure did not say what it was trying to do:\n{output}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_doctor_does_not_write_to_the_database_it_is_reporting_on() {
    // It used to. Reading the schema version went through a helper that created
    // the ledger table if it was missing, so running a diagnostic against an
    // empty database left a table behind — and against production, a read-only
    // sounding command performed DDL.
    let Some(gres) = require_gres().await else {
        return;
    };
    let dsn = gres.dsn();
    let store = forge_store::Store::connect(&dsn).await.unwrap();

    let tables = || async {
        let rows = store
            .client()
            .query(
                "SELECT DISTINCT table_name FROM information_schema.columns",
                &[],
            )
            .await
            .unwrap();
        let mut names: Vec<String> = rows.iter().map(|r| r.get(0)).collect();
        names.sort();
        names
    };

    let before = tables().await;
    check!(before.is_empty(), "expected an empty database: {before:?}");

    let (_, output) = crabforge(&dsn, &["doctor"]).await;
    check!(
        output.contains("no schema applied"),
        "the check did not run:\n{output}"
    );

    check!(
        tables().await == before,
        "doctor created something; it must only read"
    );
}
