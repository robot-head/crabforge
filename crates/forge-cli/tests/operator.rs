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

/// An address that accepts nothing and answers nothing.
///
/// Used instead of binding a port and dropping it: this test binary starts
/// several gres instances concurrently, and they draw from the same ephemeral
/// range, so a "known closed" port can be taken by one of them mid-test — which
/// would turn an unreachable-database assertion into a real connection.
/// 203.0.113.0/24 is reserved for documentation and is not routed.
const BLACKHOLE: &str = "203.0.113.1";

/// Run `crabforge` with the given arguments against `dsn`.
///
/// Returns (success, combined output).
///
/// `RUST_LOG` is pinned rather than inherited. The binary builds its subscriber
/// from the environment, and these tests read its output — so a developer with
/// `RUST_LOG=warn` exported would lose the info-level lines and a developer with
/// `RUST_LOG=debug` would gain `tokio_postgres` query logging that a naive
/// search matches instead of the report.
async fn crabforge(dsn: &str, args: &[&str]) -> (bool, String) {
    crabforge_at(dsn, BLACKHOLE_BOOTSTRAP, args).await
}

/// A bootstrap address nothing answers on, for the tests that only care about
/// gres. Spelled out rather than left to the default so a broker someone
/// happens to be running on 9092 cannot change what these tests observe.
const BLACKHOLE_BOOTSTRAP: &str = "203.0.113.1:9092";

/// Run `crabforge` against a specific broker as well as a specific database.
async fn crabforge_at(dsn: &str, bootstrap: &str, args: &[&str]) -> (bool, String) {
    let binary = env!("CARGO_BIN_EXE_crabforge");
    let output = Command::new(binary)
        .env("RUST_LOG", "info")
        // tracing-subscriber colours its output even into a pipe, and the escape
        // sequences land in the middle of the words being matched.
        .env("NO_COLOR", "1")
        .arg("--dsn")
        .arg(dsn)
        .arg("--bootstrap")
        .arg(bootstrap)
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

/// The doctor's report line for `name`, if the report contains one.
///
/// Matched on the report's own two-space-indented shape rather than by
/// searching the whole output for the check's name: log lines are interleaved
/// with the report on the same stream, and one of them mentions
/// `SELECT version FROM schema_migrations` — which a loose search for "schema"
/// finds first, and which contains both "ok" (inside `tokio_postgres`) and
/// "version". A test asserting on that line passes while the check it is
/// supposed to be reading says FAILED.
fn report_line<'a>(output: &'a str, name: &str) -> Option<&'a str> {
    output.lines().map(str::trim_end).find(|line| {
        let Some(rest) = line
            .strip_prefix("  ok      ")
            .or_else(|| line.strip_prefix("  FAILED  "))
        else {
            return false;
        };
        rest.trim_start().starts_with(name)
    })
}

/// Whether the doctor's line for `name` reports a pass.
fn passed(output: &str, name: &str) -> bool {
    report_line(output, name).is_some_and(|line| line.starts_with("  ok"))
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
    let line = report_line(&output, "schema").unwrap_or("<no schema line>");
    check!(
        passed(&output, "schema"),
        "expected a passing schema check; got: {line:?}\nfull output:\n{output}"
    );
    // Pin the number too: "at version 7" would otherwise satisfy this.
    let expected = forge_store::migrate::expected_version();
    check!(
        line.ends_with(&format!("at version {expected}")),
        "expected version {expected}; got: {line:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreachable_database_is_reported_as_such_and_not_as_a_missing_schema() {
    // Two different problems with two different fixes. Telling someone to run
    // `crabforge migrate` when gres is simply not running wastes the one piece
    // of information the doctor exists to provide.
    let dsn = format!("host={BLACKHOLE} port=5433 user=forge dbname=crab");

    let (ok, output) = crabforge(&dsn, &["doctor"]).await;
    check!(!ok);
    let line = report_line(&output, "schema").unwrap_or("<no schema line>");
    check!(
        line.contains("cannot reach gres") || line.contains("did not answer"),
        "expected an unreachable-database report; got: {line:?}\nfull output:\n{output}"
    );
    check!(
        !line.contains("no schema applied"),
        "a database that is merely down was reported as un-migrated: {line:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn migrate_reports_an_unreachable_database_rather_than_hanging() {
    // A blackhole rather than a refused port, deliberately: a refused
    // connection returns instantly, so it would pass whether or not the budget
    // bounds the attempt. Only a host that swallows packets tests that, and
    // without the bound this takes the kernel's TCP timeout — about two
    // minutes — regardless of `--wait`.
    let dsn = format!("host={BLACKHOLE} port=5433 user=forge dbname=crab");

    let started = std::time::Instant::now();
    let (ok, output) = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        crabforge(&dsn, &["migrate", "--wait", "2"]),
    )
    .await
    .expect("migrate should give up rather than hang forever");
    check!(
        started.elapsed() < std::time::Duration::from_secs(30),
        "`--wait 2` took {:?}; the budget is not bounding the connect attempt",
        started.elapsed()
    );

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

    // Asserted against the ledger by name rather than against an empty
    // catalogue: the table this used to create is `schema_migrations`, and
    // naming it says what is being protected instead of relying on gres's
    // catalogue coverage staying as it is.
    let ledger_exists = || async {
        store
            .client()
            .query_opt("SELECT 1 FROM schema_migrations LIMIT 1", &[])
            .await
            .is_ok()
    };
    check!(!ledger_exists().await, "expected a fresh database");

    let (_, output) = crabforge(&dsn, &["doctor"]).await;
    check!(
        passed(&output, "schema") == false
            && report_line(&output, "schema").is_some_and(|l| l.contains("no schema applied")),
        "the check did not run as expected:\n{output}"
    );

    check!(
        !ledger_exists().await,
        "doctor created the ledger; a diagnostic must only read"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn migrate_refuses_a_database_migrated_by_a_newer_build() {
    // The loop this closes: the server refuses to start against a schema it
    // does not match and tells the operator to run `crabforge migrate`. Migrate
    // has nothing to apply, because the runner only skips versions already in
    // the ledger and never compares its high-water mark to what this build
    // knows — so it used to report "already current" and exit 0, and the server
    // would refuse again for the same reason. Forever.
    let Some(gres) = require_gres().await else {
        return;
    };
    let dsn = gres.dsn();
    let (ok, _) = crabforge(&dsn, &["migrate"]).await;
    check!(ok);

    // A version from a build that does not exist yet.
    let store = forge_store::Store::connect(&dsn).await.unwrap();
    let ahead = forge_store::migrate::expected_version() + 1;
    store
        .client()
        .execute(
            "INSERT INTO schema_migrations (version, name, applied_at) VALUES ($1, $2, $3)",
            &[&ahead, &"from the future", &forge_types::now()],
        )
        .await
        .unwrap();

    let (ok, output) = crabforge(&dsn, &["migrate"]).await;
    check!(!ok, "migrate reported success on a newer schema:\n{output}");
    check!(
        !output.contains("already current"),
        "it claimed the schema was current:\n{output}"
    );
    check!(
        output.contains("newer build"),
        "the failure did not explain why:\n{output}"
    );

    // And the doctor agrees, rather than the two contradicting each other.
    let (_, output) = crabforge(&dsn, &["doctor"]).await;
    let line = report_line(&output, "schema").unwrap_or("<none>");
    check!(line.contains("newer build"), "doctor disagreed: {line:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_doctor_names_the_reformat_when_share_groups_are_missing() {
    // The one failure in this CLI that a config change cannot fix.
    // `share.version` is written at format time, so the fix has to say "format
    // again" — and it has to say it while the log is still empty, which is only
    // possible if the doctor reports it at all.
    let broker = forge_testkit::TestBroker::start().await;

    let (_, output) = crabforge_at(
        &format!("host={BLACKHOLE} port=5433 user=forge dbname=crab"),
        &broker.bootstrap(),
        &["doctor"],
    )
    .await;

    // The broker itself is up, so this is not a connectivity failure being
    // reported twice under a second name.
    check!(
        passed(&output, "broker"),
        "the broker check failed:\n{output}"
    );

    let line = report_line(&output, "share groups").unwrap_or("<no share groups line>");
    check!(
        !passed(&output, "share groups"),
        "an in-process broker has share.version=0; the check should say so: {line:?}"
    );
    check!(
        output.contains("crabka format --feature share.version=1"),
        "the fix did not name the command that would set it:\n{output}"
    );
    check!(
        output.contains("cannot be raised in place"),
        "the fix did not say that a reformat is the only route:\n{output}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreachable_broker_does_not_look_like_a_missing_feature() {
    // Two problems, two fixes: reformatting a broker that is merely down would
    // destroy a log that is fine.
    let dsn = format!("host={BLACKHOLE} port=5433 user=forge dbname=crab");

    let (_, output) = crabforge(&dsn, &["doctor"]).await;

    let line = report_line(&output, "share groups").unwrap_or("<none>");
    check!(
        line.contains("could not read the broker's features"),
        "expected an unreachable-broker report; got: {line:?}"
    );
    check!(
        !line.contains("formatted without"),
        "a broker that is down was reported as needing a reformat: {line:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_password_in_the_dsn_never_reaches_the_output() {
    // Doctor output is the text an operator pastes into a ticket or a CI log,
    // which makes it the one place in this CLI where a leaked credential is
    // near-certain to travel.
    let dsn = format!("host={BLACKHOLE} port=5433 user=forge password=hunter2 dbname=crab");

    for args in [vec!["doctor"], vec!["migrate", "--wait", "1"]] {
        let (_, output) = crabforge(&dsn, &args).await;
        check!(
            !output.contains("hunter2"),
            "`crabforge {}` leaked the password:\n{output}",
            args.join(" ")
        );
        // Still useful: the host has to survive, or the message says nothing.
        check!(
            output.contains(BLACKHOLE),
            "`crabforge {}` redacted too much to be diagnostic:\n{output}",
            args.join(" ")
        );
    }
}
