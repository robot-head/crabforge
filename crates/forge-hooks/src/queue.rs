//! What travels on `forge.webhooks.deliveries`.
//!
//! The matcher decides *who* should hear about an event; the worker decides
//! *when* it actually arrives. Putting a queue topic between them is what makes
//! the second part survivable: a receiver that is down for an hour holds up its
//! own deliveries and nothing else, and a restart resumes from the log rather
//! than losing whatever was in flight.

use forge_events::RawEnvelope;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// One event, addressed to one webhook.
///
/// Self-contained on purpose. It could carry a reference to the source topic
/// and offset instead, but then the worker would need a reader on every domain
/// topic and would have to re-read history to retry a week-old delivery. The
/// duplication is one copy per subscriber of an event body that is already
/// small, and it buys a worker that needs nothing but this topic.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeliveryRequest {
    /// Stable per (event, webhook), so a receiver can deduplicate and a matcher
    /// replay produces the same id rather than looking like a second event.
    pub delivery_id: String,
    pub webhook_id: String,
    pub repo_id: String,
    /// The event, exactly as it appeared on its domain topic. This is what gets
    /// POSTed, so what the receiver sees is what the log holds.
    pub event: RawEnvelope,
}

impl DeliveryRequest {
    pub fn new(webhook_id: &str, repo_id: &str, event: RawEnvelope) -> Self {
        Self {
            delivery_id: delivery_id(&event.event_id, webhook_id),
            webhook_id: webhook_id.to_string(),
            repo_id: repo_id.to_string(),
            event,
        }
    }

    pub fn event_type(&self) -> &str {
        &self.event.event_type
    }

    /// The bytes to POST.
    pub fn body(&self) -> Vec<u8> {
        // Serializing the envelope we already hold, so this cannot fail for any
        // value that arrived as JSON — but a panic in a delivery worker would
        // take every other subscriber's deliveries down with it.
        serde_json::to_vec(&self.event).unwrap_or_else(|error| {
            tracing::error!(%error, delivery = %self.delivery_id, "unserializable event");
            b"{}".to_vec()
        })
    }
}

/// A delivery's identity, derived rather than minted.
///
/// The matcher is at-least-once: a crash between producing deliveries and
/// committing its cursor replays the batch. A random id would make the replay
/// look like a new event and every receiver would process it twice; deriving it
/// means the redelivery carries the id the receiver already saw, which is
/// exactly what its deduplication is for.
pub fn delivery_id(event_id: &Uuid, webhook_id: &str) -> String {
    format!("{event_id}:{webhook_id}")
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;

    fn envelope(event_type: &str) -> RawEnvelope {
        RawEnvelope {
            event_id: Uuid::now_v7(),
            event_type: event_type.to_string(),
            event_version: 1,
            aggregate_id: "issue-1".into(),
            actor: None,
            occurred_at: forge_types::now(),
            payload: serde_json::json!({"repo_id": "repo-1", "number": 7}),
        }
    }

    #[test]
    fn a_delivery_id_is_the_same_every_time_it_is_derived() {
        // The property replay safety rests on.
        let event = envelope("issue.opened");
        let a = DeliveryRequest::new("hook-1", "repo-1", event.clone());
        let b = DeliveryRequest::new("hook-1", "repo-1", event);
        check!(a.delivery_id == b.delivery_id);
        check!(a == b);
    }

    #[test]
    fn two_subscribers_to_one_event_get_different_delivery_ids() {
        // Otherwise a receiver's deduplication would drop the second webhook's
        // copy as one it had already seen.
        let event = envelope("issue.opened");
        let a = DeliveryRequest::new("hook-1", "repo-1", event.clone());
        let b = DeliveryRequest::new("hook-2", "repo-1", event);
        check!(a.delivery_id != b.delivery_id);
    }

    #[test]
    fn the_body_is_the_event_as_it_appeared_on_the_log() {
        let event = envelope("issue.opened");
        let request = DeliveryRequest::new("hook-1", "repo-1", event.clone());

        let sent: serde_json::Value = serde_json::from_slice(&request.body()).unwrap();
        check!(sent["event_type"] == "issue.opened");
        check!(sent["payload"]["number"] == 7);
        check!(sent["event_id"] == serde_json::json!(event.event_id));
    }

    #[test]
    fn a_request_survives_the_queue_topic() {
        let request = DeliveryRequest::new("hook-1", "repo-1", envelope("pr.merged"));
        let bytes = serde_json::to_vec(&request).unwrap();
        let back: DeliveryRequest = serde_json::from_slice(&bytes).unwrap();
        check!(back == request);
    }
}
