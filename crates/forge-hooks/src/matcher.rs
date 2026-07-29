//! Deciding who hears about an event.
//!
//! One reader per domain topic, folding each event against the webhooks
//! configured for its repository and writing a [`DeliveryRequest`] per
//! subscriber onto the queue topic.
//!
//! ## Why not one consumer group per subscription
//!
//! Crabka's gateway fans out that way, and it is the obvious design: each
//! subscription gets its own group and reads the source topic itself. It does
//! not scale with subscribers — every webhook added to a repository means
//! another consumer group reading every domain topic in full, so the broker's
//! read load grows with the product of events and subscribers rather than with
//! events. Matching once and addressing the result keeps the read side flat.
//!
//! ## At-least-once, deliberately
//!
//! The cursor is committed after the deliveries it accounts for are produced,
//! not atomically with them. A crash in between replays the batch and some
//! receivers hear twice. That is the right trade here: webhook delivery is
//! at-least-once regardless — a receiver that times out after processing gets a
//! retry — so every receiver needs deduplication anyway, and
//! [`crate::queue::delivery_id`] is derived so the replay carries the id they
//! already saw. Paying for exactly-once between two stages of a pipeline that
//! is at-least-once at its far end would buy nothing.

use forge_bus::{FencedWriter, PendingRecord, TailError, Tailer, WriteError};
use forge_events::{RawEnvelope, decode_raw};
use forge_store::{Store, StoreError, WEBHOOK_MATCHER};
use forge_types::topics;

use crate::queue::DeliveryRequest;

/// How long to wait for new records before looping again.
const POLL_WAIT_MS: i32 = 500;

#[derive(Debug, thiserror::Error)]
pub enum MatchError {
    #[error("reading the log: {0}")]
    Tail(#[from] TailError),
    #[error("reading webhook configuration: {0}")]
    Store(#[from] StoreError),
    #[error("queueing a delivery: {0}")]
    Write(#[from] WriteError),
}

/// Follows one domain topic and queues deliveries for its subscribers.
pub struct Matcher {
    tailer: Tailer,
    store: Store,
    writer: std::sync::Arc<FencedWriter>,
}

impl Matcher {
    /// Open a matcher positioned at its durable cursor.
    pub async fn open(
        bootstrap: &str,
        topic: &str,
        store: Store,
        writer: std::sync::Arc<FencedWriter>,
    ) -> Result<Self, MatchError> {
        let resume_from = store.cursors(WEBHOOK_MATCHER).applied_offset(topic).await?;
        let tailer = Tailer::open_at(bootstrap, topic, resume_from).await?;
        tracing::info!(topic, resume_from, "webhook matcher opened");
        Ok(Self {
            tailer,
            store,
            writer,
        })
    }

    pub fn topic(&self) -> &str {
        self.tailer.topic()
    }

    /// Read one batch and queue whatever it implies. Returns how many
    /// deliveries were produced.
    pub async fn step(&mut self) -> Result<usize, MatchError> {
        let batch = self.tailer.next_batch(POLL_WAIT_MS).await?;
        if batch.records.is_empty() {
            return Ok(0);
        }

        let mut queued = 0;
        for record in &batch.records {
            queued += self.fan_out(record.value.as_deref()).await?;
        }

        // After the deliveries, never before: a cursor that moved first would
        // drop them entirely on a crash, and losing a webhook is worse than
        // sending it twice to a receiver that deduplicates.
        self.store
            .cursors(WEBHOOK_MATCHER)
            .set_applied_offset(self.tailer.topic(), batch.next_offset)
            .await?;
        Ok(queued)
    }

    /// Run until cancelled.
    pub async fn run(mut self, mut shutdown: tokio::sync::watch::Receiver<bool>) {
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        tracing::info!(topic = self.tailer.topic(), "webhook matcher stopping");
                        return;
                    }
                }
                result = self.step() => {
                    if let Err(error) = result {
                        // Log and continue: a transient gres or broker failure
                        // must not take the matcher down permanently, and the
                        // cursor has not moved, so nothing is lost by retrying.
                        tracing::warn!(
                            topic = self.tailer.topic(),
                            %error,
                            "webhook matcher step failed; retrying"
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    }
                }
            }
        }
    }

    /// Queue a delivery for every webhook that wants this event.
    async fn fan_out(&self, value: Option<&[u8]>) -> Result<usize, MatchError> {
        let Some(event) = readable_event(value) else {
            return Ok(0);
        };
        let Some(repo_id) = repo_of(&event) else {
            // Not repo-scoped — account events, for instance. Webhooks are
            // configured on repositories, so there is nobody to tell.
            return Ok(0);
        };

        let hooks = self.store.hooks().for_repo(&repo_id).await?;
        let wanted: Vec<_> = hooks
            .iter()
            .filter(|hook| hook.wants(&event.event_type))
            .collect();
        if wanted.is_empty() {
            return Ok(0);
        }

        // Keyed by webhook so one endpoint's deliveries stay in order relative
        // to each other however the topic is later partitioned, and written in
        // one transaction so an event never reaches some of its subscribers and
        // not others.
        let records = wanted
            .iter()
            .map(|hook| {
                let request = DeliveryRequest::new(&hook.webhook_id, &repo_id, event.clone());
                PendingRecord::state(
                    topics::WEBHOOKS_DELIVERIES,
                    hook.webhook_id.clone(),
                    &request,
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        self.writer.transact(records).await?;

        tracing::debug!(
            event_type = %event.event_type,
            repo_id = %repo_id,
            subscribers = wanted.len(),
            "queued webhook deliveries"
        );
        Ok(wanted.len())
    }
}

/// Decode a record, or `None` if there is nothing a matcher can act on.
///
/// Unreadable records are skipped rather than failing the batch, for the same
/// reason the projector skips them: during co-development a writer may be ahead
/// of a reader, and a matcher that died on an unknown event would stop every
/// webhook in the forge until it was redeployed.
fn readable_event(value: Option<&[u8]>) -> Option<RawEnvelope> {
    match decode_raw(value) {
        Ok(event) => Some(event),
        Err(error) => {
            tracing::debug!(%error, "skipping a record the matcher cannot read");
            None
        }
    }
}

/// The repository an event belongs to, if it belongs to one.
///
/// Read out of the payload rather than the envelope. The envelope's
/// `aggregate_id` is the issue, pull request or repository the event is
/// *about*, which is only the repository for repository events — but every
/// repo-scoped payload carries `repo_id`, because the projector needs it too.
fn repo_of(event: &RawEnvelope) -> Option<String> {
    event
        .payload
        .get("repo_id")
        .and_then(|value| value.as_str())
        .map(str::to_string)
}

/// Every domain topic a matcher should follow.
///
/// Account events are absent: they are not repo-scoped, so no webhook can be
/// configured for them.
pub fn matched_topics() -> &'static [&'static str] {
    &[
        topics::EVENTS_REPOS,
        topics::EVENTS_ISSUES,
        topics::EVENTS_PRS,
        topics::EVENTS_GIT_REFS,
    ]
}

#[cfg(test)]
mod tests {
    use assert2::check;
    use uuid::Uuid;

    use super::*;

    fn envelope(event_type: &str, payload: serde_json::Value) -> RawEnvelope {
        RawEnvelope {
            event_id: Uuid::now_v7(),
            event_type: event_type.to_string(),
            event_version: 1,
            aggregate_id: "agg-1".into(),
            actor: None,
            occurred_at: forge_types::now(),
            payload,
        }
    }

    #[test]
    fn a_repo_scoped_event_names_its_repository() {
        let event = envelope("issue.opened", serde_json::json!({"repo_id": "repo-7"}));
        check!(repo_of(&event).as_deref() == Some("repo-7"));
    }

    #[test]
    fn an_account_event_belongs_to_no_repository() {
        // Webhooks are configured on repositories, so there is nothing to match
        // these against — and guessing would send someone else's account
        // activity to a repository's subscribers.
        let event = envelope("user.registered", serde_json::json!({"user_id": "u-1"}));
        check!(repo_of(&event).is_none());
    }

    #[test]
    fn a_non_string_repo_id_is_not_mistaken_for_one() {
        let event = envelope("issue.opened", serde_json::json!({"repo_id": 7}));
        check!(repo_of(&event).is_none());
    }

    #[test]
    fn a_tombstone_is_skipped_rather_than_failing_the_batch() {
        check!(readable_event(None).is_none());
        check!(readable_event(Some(b"not json")).is_none());
    }

    #[test]
    fn an_event_from_a_newer_writer_is_still_matchable() {
        // The forward-compatibility property: the matcher reads the envelope
        // and the repo id without understanding the payload, so a webhook for
        // `*` keeps firing for event types this build has never heard of.
        let json = serde_json::json!({
            "event_id": Uuid::now_v7(),
            "event_type": "issue.teleported",
            "event_version": 9,
            "aggregate_id": "issue-1",
            "actor": null,
            "occurred_at": "2026-07-29T00:00:00Z",
            "payload": {"repo_id": "repo-7", "destination": "elsewhere"},
        });
        let event = readable_event(Some(&serde_json::to_vec(&json).unwrap())).unwrap();
        check!(event.event_type == "issue.teleported");
        check!(repo_of(&event).as_deref() == Some("repo-7"));
    }
}
