//! Diagnose a development or production environment.
//!
//! Every check reports what is wrong *and how to fix it* — the failure modes on
//! this stack (an unformatted log directory, a broker formatted without the
//! share-group feature) are not self-explanatory, and some need a reformat
//! rather than a config change.

use crabka_client_admin::AdminClient;

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

pub async fn run(bootstrap: &str) -> Report {
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

    // TODO(M6): verify `share.version >= 1` in the broker's finalized features.
    // A broker formatted without `--feature share.version=1` cannot run the CI
    // work queue, and the flag is only settable at format time — recovering
    // means re-formatting the log directory, not editing config. The dev-loop
    // `just format` recipe passes the flag, so this check is a safety net for
    // environments formatted before that existed. It needs a raw ApiVersions
    // round-trip via client-core; the admin client does not expose features.

    Report { checks }
}
