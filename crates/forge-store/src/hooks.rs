//! Webhook configuration and delivery history.

use time::OffsetDateTime;
use tokio_postgres::Client;

use crate::{PageSize, StoreError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookRecord {
    pub webhook_id: String,
    pub repo_id: String,
    pub url: String,
    /// Stored as written, not digested: signatures are recomputed from it on
    /// every delivery, so a one-way hash would be useless.
    pub secret: String,
    /// Subscribed event types: exact (`issue.opened`), prefix (`issue.*`), or
    /// the single element `*` for all.
    pub events: Vec<String>,
    pub active: bool,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

impl WebhookRecord {
    /// Whether this webhook wants to hear about `event_type`.
    pub fn wants(&self, event_type: &str) -> bool {
        self.active
            && self
                .events
                .iter()
                .any(|w| w == "*" || w == event_type || prefix_matches(w, event_type))
    }
}

/// Whether a subscription like `issue.*` covers `issue.opened`.
fn prefix_matches(pattern: &str, event_type: &str) -> bool {
    pattern.strip_suffix(".*").is_some_and(|prefix| {
        event_type.starts_with(prefix) && event_type[prefix.len()..].starts_with('.')
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryRecord {
    pub delivery_id: String,
    pub webhook_id: String,
    pub repo_id: String,
    pub event_type: String,
    pub event_id: String,
    pub attempt: i64,
    pub status: String,
    pub status_code: Option<i64>,
    pub error: Option<String>,
    pub duration_ms: Option<i64>,
    pub created_at: OffsetDateTime,
}

pub struct HookStore<'a> {
    client: &'a Client,
}

impl<'a> HookStore<'a> {
    pub fn new(client: &'a Client) -> Self {
        Self { client }
    }

    pub async fn upsert(&self, hook: &WebhookRecord) -> Result<(), StoreError> {
        self.client
            .execute(
                "INSERT INTO webhooks (webhook_id, repo_id, url, secret, events, active, \
                 created_at, updated_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
                 ON CONFLICT (webhook_id) DO UPDATE SET \
                 url = excluded.url, secret = excluded.secret, events = excluded.events, \
                 active = excluded.active, updated_at = excluded.updated_at",
                &[
                    &hook.webhook_id,
                    &hook.repo_id,
                    &hook.url,
                    &hook.secret,
                    &hook.events,
                    &hook.active,
                    &hook.created_at,
                    &hook.updated_at,
                ],
            )
            .await?;
        Ok(())
    }

    /// Every webhook configured for a repository.
    pub async fn for_repo(&self, repo_id: &str) -> Result<Vec<WebhookRecord>, StoreError> {
        let rows = self
            .client
            .query(&format!("{HOOK_COLUMNS} WHERE repo_id = $1"), &[&repo_id])
            .await?;
        Ok(rows.iter().map(row_to_hook).collect())
    }

    pub async fn by_id(&self, webhook_id: &str) -> Result<Option<WebhookRecord>, StoreError> {
        let row = self
            .client
            .query_opt(
                &format!("{HOOK_COLUMNS} WHERE webhook_id = $1"),
                &[&webhook_id],
            )
            .await?;
        Ok(row.as_ref().map(row_to_hook))
    }

    pub async fn delete(&self, webhook_id: &str) -> Result<(), StoreError> {
        self.client
            .execute("DELETE FROM webhooks WHERE webhook_id = $1", &[&webhook_id])
            .await?;
        Ok(())
    }

    /// Record one attempt.
    pub async fn record_attempt(&self, delivery: &DeliveryRecord) -> Result<(), StoreError> {
        self.client
            .execute(
                "INSERT INTO webhook_deliveries (delivery_id, webhook_id, repo_id, event_type, \
                 event_id, attempt, status, status_code, error, duration_ms, created_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
                &[
                    &delivery.delivery_id,
                    &delivery.webhook_id,
                    &delivery.repo_id,
                    &delivery.event_type,
                    &delivery.event_id,
                    &delivery.attempt,
                    &delivery.status,
                    &delivery.status_code,
                    &delivery.error,
                    &delivery.duration_ms,
                    &delivery.created_at,
                ],
            )
            .await?;
        Ok(())
    }

    /// Recent attempts for a webhook, newest first.
    ///
    /// What a maintainer looks at when an integration has stopped working.
    pub async fn recent_deliveries(
        &self,
        webhook_id: &str,
        limit: PageSize,
    ) -> Result<Vec<DeliveryRecord>, StoreError> {
        // TODO(gres:parameterized-limit)
        let limit = *limit;
        let rows = self
            .client
            .query(
                &format!(
                    "SELECT delivery_id, webhook_id, repo_id, event_type, event_id, attempt, \
                     status, status_code, error, duration_ms, created_at FROM webhook_deliveries \
                     WHERE webhook_id = $1 ORDER BY delivery_id DESC LIMIT {limit}"
                ),
                &[&webhook_id],
            )
            .await?;
        Ok(rows
            .iter()
            .map(|row| DeliveryRecord {
                delivery_id: row.get(0),
                webhook_id: row.get(1),
                repo_id: row.get(2),
                event_type: row.get(3),
                event_id: row.get(4),
                attempt: row.get(5),
                status: row.get(6),
                status_code: row.get(7),
                error: row.get(8),
                duration_ms: row.get(9),
                created_at: row.get(10),
            })
            .collect())
    }
}

const HOOK_COLUMNS: &str = "SELECT webhook_id, repo_id, url, secret, events, active, created_at, \
     updated_at FROM webhooks";

fn row_to_hook(row: &tokio_postgres::Row) -> WebhookRecord {
    WebhookRecord {
        webhook_id: row.get(0),
        repo_id: row.get(1),
        url: row.get(2),
        secret: row.get(3),
        events: row.get(4),
        active: row.get(5),
        created_at: row.get(6),
        updated_at: row.get(7),
    }
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;

    fn hook(events: &str, active: bool) -> WebhookRecord {
        let now = forge_types::now();
        WebhookRecord {
            webhook_id: "w".into(),
            repo_id: "r".into(),
            url: "https://example.com/hook".into(),
            secret: "s".into(),
            events: events.split_whitespace().map(str::to_string).collect(),
            active,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn a_wildcard_subscription_wants_everything() {
        let h = hook("*", true);
        check!(h.wants("git.ref_updated"));
        check!(h.wants("issue.opened"));
    }

    #[test]
    fn an_exact_subscription_wants_only_that_event() {
        let h = hook("issue.opened", true);
        check!(h.wants("issue.opened"));
        check!(!h.wants("issue.closed"));
        check!(!h.wants("git.ref_updated"));
    }

    #[test]
    fn a_prefix_subscription_covers_a_family() {
        let h = hook("issue.*", true);
        check!(h.wants("issue.opened"));
        check!(h.wants("issue.commented"));
        check!(!h.wants("pr.opened"));
        // Not a prefix match on a longer name that merely starts the same way.
        check!(!hook("issue.*", true).wants("issues.opened"));
    }

    #[test]
    fn several_subscriptions_can_be_listed() {
        let h = hook("issue.opened pr.merged", true);
        check!(h.wants("issue.opened"));
        check!(h.wants("pr.merged"));
        check!(!h.wants("pr.opened"));
    }

    #[test]
    fn a_disabled_webhook_wants_nothing() {
        // Disabling is how a maintainer stops a noisy integration without
        // losing its configuration.
        check!(!hook("*", false).wants("issue.opened"));
    }

    #[test]
    fn an_empty_subscription_list_wants_nothing() {
        check!(!hook("", true).wants("issue.opened"));
    }
}
