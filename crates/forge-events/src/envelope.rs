//! The event envelope.
//!
//! Every record the forge writes to a domain topic is an [`Envelope`]: routing
//! and provenance metadata around an opaque payload. The metadata is duplicated
//! into record headers (see [`crate::ce`]) so consumers can filter without
//! deserializing bodies — including SQL consumers reading through crabka's
//! `gres-fdw`, which exposes topic headers as a pseudo-column.

use forge_types::UserId;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

/// A domain event, wrapped for transport.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope<P> {
    /// Unique per event. UUIDv7, so it is also a creation-ordered sort key.
    /// Doubles as the CloudEvents `id` and the webhook delivery id, which is
    /// what lets receivers deduplicate redeliveries.
    pub event_id: Uuid,
    /// Dotted type name, e.g. `repo.created`.
    pub event_type: String,
    /// Payload schema version. Additive changes do not bump it; anything a
    /// current reader could misinterpret does.
    pub event_version: u16,
    /// The aggregate this event belongs to. Also the record key, so a topic
    /// keeps per-aggregate ordering even after a partition split.
    pub aggregate_id: String,
    /// Who caused this. `None` for system-originated events.
    pub actor: Option<UserId>,
    #[serde(with = "time::serde::rfc3339")]
    pub occurred_at: OffsetDateTime,
    pub payload: P,
}

impl<P> Envelope<P> {
    /// Wrap a payload, minting an id and timestamp.
    pub fn new(event: &P, actor: Option<UserId>) -> Self
    where
        P: DomainEvent + Clone,
    {
        Self {
            event_id: Uuid::now_v7(),
            event_type: event.event_type().to_string(),
            event_version: event.event_version(),
            aggregate_id: event.aggregate_id(),
            actor,
            occurred_at: forge_types::now(),
            payload: event.clone(),
        }
    }

    /// The CloudEvents `source` for this event: a forge-relative path.
    pub fn source(&self) -> String {
        format!("/{}", self.aggregate_id)
    }
}

/// Implemented by every domain event enum.
pub trait DomainEvent {
    /// The topic this event is written to.
    fn topic(&self) -> &'static str;
    /// The dotted type name, stable across versions.
    fn event_type(&self) -> &'static str;
    /// The aggregate id, used as the record key.
    fn aggregate_id(&self) -> String;
    /// Payload schema version.
    fn event_version(&self) -> u16 {
        1
    }
}

/// A decoded envelope whose payload has not been interpreted yet.
///
/// Projectors decode to this first so an unrecognized `event_type` can be
/// skipped rather than failing the batch. During co-development a writer may
/// legitimately be ahead of a reader; a projector that crashes on an unknown
/// event would take the read models down until it is redeployed.
pub type RawEnvelope = Envelope<serde_json::Value>;

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("record has no value (tombstone)")]
    Tombstone,
    #[error("malformed envelope: {0}")]
    Malformed(#[from] serde_json::Error),
}

/// Decode a record value into an envelope with an uninterpreted payload.
pub fn decode_raw(value: Option<&[u8]>) -> Result<RawEnvelope, DecodeError> {
    let bytes = value.ok_or(DecodeError::Tombstone)?;
    Ok(serde_json::from_slice(bytes)?)
}

impl RawEnvelope {
    /// Interpret the payload as a concrete event type.
    pub fn parse<P: for<'de> Deserialize<'de>>(&self) -> Result<Envelope<P>, DecodeError> {
        Ok(Envelope {
            event_id: self.event_id,
            event_type: self.event_type.clone(),
            event_version: self.event_version,
            aggregate_id: self.aggregate_id.clone(),
            actor: self.actor,
            occurred_at: self.occurred_at,
            payload: serde_json::from_value(self.payload.clone())?,
        })
    }
}

#[cfg(test)]
mod tests {
    use assert2::{assert, check};

    use super::*;
    use crate::RepoEvent;

    fn sample() -> RepoEvent {
        RepoEvent::Deleted {
            repo_id: forge_types::RepoId::new(),
        }
    }

    #[test]
    fn envelopes_round_trip_through_json() {
        let event = sample();
        let envelope = Envelope::new(&event, None);
        let bytes = serde_json::to_vec(&envelope).unwrap();

        let decoded = decode_raw(Some(&bytes)).unwrap();
        check!(decoded.event_type == "repo.deleted");
        check!(decoded.event_id == envelope.event_id);

        let parsed: Envelope<RepoEvent> = decoded.parse().unwrap();
        check!(parsed.payload == event);
    }

    #[test]
    fn a_tombstone_is_not_an_envelope() {
        assert!(let Err(DecodeError::Tombstone) = decode_raw(None));
    }

    #[test]
    fn unknown_payload_shapes_survive_decoding() {
        // Forward compatibility: a reader older than the writer must still be
        // able to read the envelope and skip the event, not fail the batch.
        let json = serde_json::json!({
            "event_id": Uuid::now_v7(),
            "event_type": "repo.teleported",
            "event_version": 9,
            "aggregate_id": "repo-1",
            "actor": null,
            "occurred_at": "2026-07-28T00:00:00Z",
            "payload": {"destination": "elsewhere"},
        });
        let raw = decode_raw(Some(&serde_json::to_vec(&json).unwrap())).unwrap();
        check!(raw.event_type == "repo.teleported");
        check!(
            raw.parse::<RepoEvent>().is_err(),
            "payload is not a known event"
        );
    }

    #[test]
    fn ids_are_time_ordered_so_they_double_as_sort_keys() {
        let a = Envelope::new(&sample(), None);
        let b = Envelope::new(&sample(), None);
        check!(a.event_id < b.event_id);
    }
}
