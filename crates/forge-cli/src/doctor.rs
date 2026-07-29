//! Diagnose a development or production environment.
//!
//! Every check reports what is wrong *and how to fix it* — the failure modes on
//! this stack (an unformatted log directory, a broker formatted without the
//! share-group feature) are not self-explanatory, and some need a reformat
//! rather than a config change.

use std::time::Duration;

use crabka_client_admin::AdminClient;
use forge_store::{Store, migrate};

/// How long to wait for gres before calling it unreachable.
///
/// Short on purpose, and much shorter than `crabforge migrate`'s budget. The
/// doctor's job is to report the state of things now; an operator running it
/// while a cold gres replays its log wants to be told that, not made to wait
/// two minutes for the same answer.
const GRES_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

pub struct Check {
    pub name: &'static str,
    pub outcome: Outcome,
}

pub enum Outcome {
    Pass(String),
    Fail { problem: String, fix: String },
}

pub struct Report {
    pub checks: Vec<Check>,
}

impl Report {
    pub fn is_healthy(&self) -> bool {
        self.checks
            .iter()
            .all(|c| matches!(c.outcome, Outcome::Pass(_)))
    }

    pub fn print(&self) {
        for check in &self.checks {
            match &check.outcome {
                Outcome::Pass(detail) => println!("  ok      {:<20} {detail}", check.name),
                Outcome::Fail { problem, fix } => {
                    println!("  FAILED  {:<20} {problem}", check.name);
                    println!("          {:<20} fix: {fix}", "");
                }
            }
        }
        println!();
        if self.is_healthy() {
            println!("all checks passed");
        } else {
            println!("environment is not ready to serve");
        }
    }
}

pub async fn run(bootstrap: &str, dsn: &str) -> Report {
    let mut checks = Vec::new();

    let admin = match AdminClient::connect(&[bootstrap.to_string()]).await {
        Ok(admin) => {
            checks.push(Check {
                name: "broker",
                outcome: Outcome::Pass(format!("reachable at {bootstrap}")),
            });
            Some(admin)
        }
        Err(e) => {
            checks.push(Check {
                name: "broker",
                outcome: Outcome::Fail {
                    problem: format!("cannot reach {bootstrap}: {e}"),
                    fix: "run `just broker` (and `just format` first if the log dir is empty)"
                        .into(),
                },
            });
            None
        }
    };

    if let Some(mut admin) = admin {
        let specs = forge_topics::static_topics();
        match forge_topics::missing(&mut admin, &specs).await {
            Ok(missing) if missing.is_empty() => checks.push(Check {
                name: "topics",
                outcome: Outcome::Pass(format!("all {} present", specs.len())),
            }),
            Ok(missing) => checks.push(Check {
                name: "topics",
                outcome: Outcome::Fail {
                    problem: format!("{} missing: {}", missing.len(), missing.join(", ")),
                    fix: "run `crabforge bootstrap`".into(),
                },
            }),
            Err(e) => checks.push(Check {
                name: "topics",
                outcome: Outcome::Fail {
                    problem: format!("could not list topics: {e}"),
                    fix: "check broker health and authorization".into(),
                },
            }),
        }
    }

    checks.push(schema_check(dsn).await);

    // TODO(M6): verify `share.version >= 1` in the broker's finalized features.
    // A broker formatted without `--feature share.version=1` cannot run the CI
    // work queue, and the flag is only settable at format time — recovering
    // means re-formatting the log directory, not editing config. The dev-loop
    // `just format` recipe passes the flag, so this check is a safety net for
    // environments formatted before that existed. It needs a raw ApiVersions
    // round-trip via client-core; the admin client does not expose features.

    Report { checks }
}

/// Whether gres holds the schema this build expects.
///
/// Reported separately from broker reachability because the two fail
/// independently and have different fixes: a missing schema is one command
/// away, and the server refuses to start until it is applied, so an operator
/// who sees only "topics ok" would otherwise be surprised.
async fn schema_check(dsn: &str) -> Check {
    let expected = migrate::expected_version();

    let connected = match tokio::time::timeout(GRES_PROBE_TIMEOUT, Store::connect(dsn)).await {
        Ok(Ok(store)) => store,
        Ok(Err(e)) => {
            return Check {
                name: "schema",
                outcome: Outcome::Fail {
                    problem: format!("cannot reach gres: {e}"),
                    fix: "run `just gres` (and `just gres-tenant` if the tenant is new)".into(),
                },
            };
        }
        Err(_) => {
            return Check {
                name: "schema",
                outcome: Outcome::Fail {
                    problem: format!(
                        "gres did not answer within {}s at {dsn}",
                        GRES_PROBE_TIMEOUT.as_secs()
                    ),
                    // Worth saying: a cold substrate gres replays its whole
                    // write-ahead log before accepting connections, so this is
                    // as likely to mean "still starting" as "not running".
                    fix: "check `just gres` — a cold start replays the whole WAL before it listens"
                        .into(),
                },
            };
        }
    };

    match migrate::current_version(connected.client()).await {
        Ok(Some(found)) if found == expected => Check {
            name: "schema",
            outcome: Outcome::Pass(format!("at version {found}")),
        },
        Ok(found) => Check {
            name: "schema",
            outcome: Outcome::Fail {
                problem: match found {
                    Some(found) if found > expected => format!(
                        "database is at version {found} but this build expects {expected}; \
                         it was migrated by a newer build"
                    ),
                    Some(found) => {
                        format!("database is at version {found}, this build expects {expected}")
                    }
                    None => format!("no schema applied, this build expects version {expected}"),
                },
                fix: match found {
                    // Running `migrate` cannot help here — there is nothing to
                    // apply, and no down-migrations. Saying so beats sending
                    // someone to a command that will report success and change
                    // nothing.
                    Some(found) if found > expected => {
                        "deploy a build that matches, or reset the database and re-project".into()
                    }
                    _ => "run `crabforge migrate`".into(),
                },
            },
        },
        Err(e) => Check {
            name: "schema",
            outcome: Outcome::Fail {
                problem: format!("could not read the migration ledger: {e}"),
                fix: "check gres health and that the forge user can read `schema_migrations`"
                    .into(),
            },
        },
    }
}
