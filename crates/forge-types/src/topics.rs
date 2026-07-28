//! Topic names.
//!
//! One source of truth: `forge-topics` provisions these, `forge-events` routes
//! to them, and the projector and webhook services subscribe to them. Defining
//! them here — in the crate at the bottom of the dependency graph — means a
//! rename is a compile error everywhere rather than a silent mismatch between a
//! producer and a consumer.

use crate::RepoId;

/// Domain event streams, keyed by aggregate id and retained forever.
pub const EVENTS_USERS: &str = "forge.events.users";
pub const EVENTS_REPOS: &str = "forge.events.repos";
pub const EVENTS_ISSUES: &str = "forge.events.issues";
pub const EVENTS_PRS: &str = "forge.events.prs";
pub const EVENTS_GIT_REFS: &str = "forge.events.git-refs";
pub const EVENTS_CI: &str = "forge.events.ci";

/// Compacted state stores owned by the command service.
pub const META_CATALOG: &str = "forge.meta.catalog";
pub const GIT_REFS: &str = "forge.git.refs";

/// Webhook plane.
pub const WEBHOOKS_CONFIG: &str = "forge.webhooks.config";
pub const WEBHOOKS_DELIVERIES: &str = "forge.webhooks.deliveries";
pub const WEBHOOKS_ATTEMPTS: &str = "forge.webhooks.attempts";
pub const WEBHOOKS_DLQ: &str = "forge.webhooks.dlq";

/// CI plane.
pub const CI_JOBS: &str = "forge.ci.jobs";
pub const CI_LOGS: &str = "forge.ci.logs";

/// Every domain event topic.
pub const EVENT_TOPICS: &[&str] = &[
    EVENTS_USERS,
    EVENTS_REPOS,
    EVENTS_ISSUES,
    EVENTS_PRS,
    EVENTS_GIT_REFS,
    EVENTS_CI,
];

/// The object topic backing one repository.
///
/// Named by id so renaming a repository costs nothing.
pub fn repo_objects(repo: RepoId) -> String {
    format!("forge.git.objects.{repo}")
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;

    #[test]
    fn every_topic_shares_the_forge_prefix() {
        // Keeps forge topics visibly distinct from crabka's internal ones
        // (`__crabka_*`, `__gres_wal.*`) in admin tooling.
        for topic in EVENT_TOPICS {
            check!(topic.starts_with("forge."));
        }
        check!(repo_objects(RepoId::new()).starts_with("forge.git.objects."));
    }

    #[test]
    fn repo_topics_are_distinct_per_repository() {
        check!(repo_objects(RepoId::new()) != repo_objects(RepoId::new()));
    }
}
