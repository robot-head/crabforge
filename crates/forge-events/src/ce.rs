//! CloudEvents binary-mode encoding.
//!
//! Forge events are written to topics as CloudEvents from the start, following
//! crabka's own binding design (`docs/superpowers/specs/2026-07-06-crabka-cloudevents-binding-design.md`).
//! Crabka specified that binding but has not implemented it; writing conformant
//! records now costs nothing and means:
//!
//! * outbound webhooks carry a standard envelope instead of a bespoke one;
//! * Knative Eventing (or anything else speaking CloudEvents) can consume forge
//!   topics directly if the upstream `eventing-kafka-broker` is ever validated
//!   against crabka;
//! * the translation functions here are shaped like the spec's `ce_translate.rs`
//!   so they can be contributed upstream as a port rather than a rewrite.
//!
//! The one transformation between transports is the separator: attributes are
//! `ce_id` in Kafka headers and `ce-id` in HTTP headers. `datacontenttype` is
//! special-cased to the bare `content-type` header on both sides.

use crate::Envelope;

/// The CloudEvents specification version these records declare.
pub const SPEC_VERSION: &str = "1.0";

/// Payloads are JSON.
pub const CONTENT_TYPE: &str = "application/json";

/// Reverse-DNS prefix for forge event types, e.g. `com.crabforge.repo.created`.
pub const TYPE_PREFIX: &str = "com.crabforge";

/// A record header: a name and a UTF-8 value.
pub type Header = (String, String);

/// The CloudEvents attributes for an envelope, in Kafka header spelling.
///
/// Returned in a stable order so tests and golden files do not depend on map
/// iteration.
pub fn kafka_headers<P>(envelope: &Envelope<P>) -> Vec<Header> {
    vec![
        ("ce_specversion".into(), SPEC_VERSION.into()),
        ("ce_id".into(), envelope.event_id.to_string()),
        ("ce_source".into(), envelope.source()),
        ("ce_type".into(), qualified_type(&envelope.event_type)),
        (
            "ce_time".into(),
            envelope
                .occurred_at
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_default(),
        ),
        // Not `ce_datacontenttype`: the binding maps this attribute onto the
        // transport's native content-type header.
        ("content-type".into(), CONTENT_TYPE.into()),
        // Forge-native duplicates, so consumers that do not speak CloudEvents
        // can still route on the unqualified type and schema version.
        ("forge-event-type".into(), envelope.event_type.clone()),
        (
            "forge-event-version".into(),
            envelope.event_version.to_string(),
        ),
    ]
}

/// Translate Kafka-spelled CloudEvents headers to their HTTP spelling.
///
/// Only the separator changes, and only for `ce_` attributes: `content-type` is
/// already the HTTP name. Non-CloudEvents headers pass through untouched so
/// trace context (`traceparent`) survives the hop.
pub fn kafka_headers_to_http(headers: &[Header]) -> Vec<Header> {
    headers
        .iter()
        .map(|(name, value)| {
            let name = match name.strip_prefix("ce_") {
                Some(attr) => format!("ce-{attr}"),
                None => name.clone(),
            };
            (name, value.clone())
        })
        .collect()
}

/// Translate HTTP-spelled CloudEvents headers to their Kafka spelling.
pub fn http_headers_to_kafka(headers: &[Header]) -> Vec<Header> {
    headers
        .iter()
        .map(|(name, value)| {
            let lower = name.to_ascii_lowercase();
            let name = match lower.strip_prefix("ce-") {
                Some(attr) => format!("ce_{attr}"),
                None => lower,
            };
            (name, value.clone())
        })
        .collect()
}

/// Qualify a forge event type for the `ce_type` attribute.
///
/// CloudEvents asks that `type` be prefixed to avoid collisions between
/// producers, so `repo.created` is published as `com.crabforge.repo.created`.
pub fn qualified_type(event_type: &str) -> String {
    format!("{TYPE_PREFIX}.{event_type}")
}

/// Recover the forge event type from a qualified CloudEvents type.
pub fn unqualified_type(ce_type: &str) -> &str {
    ce_type
        .strip_prefix(TYPE_PREFIX)
        .and_then(|rest| rest.strip_prefix('.'))
        .unwrap_or(ce_type)
}

#[cfg(test)]
mod tests {
    use assert2::check;
    use forge_types::RepoId;

    use super::*;
    use crate::RepoEvent;

    fn envelope() -> Envelope<RepoEvent> {
        Envelope::new(
            &RepoEvent::Deleted {
                repo_id: RepoId::new(),
            },
            None,
        )
    }

    fn find<'a>(headers: &'a [Header], name: &str) -> Option<&'a str> {
        headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn required_cloudevents_attributes_are_present() {
        let headers = kafka_headers(&envelope());
        for required in ["ce_specversion", "ce_id", "ce_source", "ce_type"] {
            check!(
                find(&headers, required).is_some_and(|v| !v.is_empty()),
                "missing required attribute {required}"
            );
        }
        check!(find(&headers, "ce_specversion") == Some("1.0"));
    }

    #[test]
    fn content_type_is_the_bare_header_not_a_ce_attribute() {
        // The binding maps `datacontenttype` onto the transport's own header.
        let headers = kafka_headers(&envelope());
        check!(find(&headers, "content-type") == Some("application/json"));
        check!(find(&headers, "ce_datacontenttype").is_none());
    }

    #[test]
    fn event_types_are_reverse_dns_qualified() {
        let headers = kafka_headers(&envelope());
        check!(find(&headers, "ce_type") == Some("com.crabforge.repo.deleted"));
        check!(find(&headers, "forge-event-type") == Some("repo.deleted"));
    }

    #[test]
    fn qualification_round_trips() {
        check!(unqualified_type(&qualified_type("repo.created")) == "repo.created");
        // A type from another producer is left alone rather than mangled.
        check!(unqualified_type("io.example.thing") == "io.example.thing");
    }

    #[test]
    fn only_the_separator_changes_between_transports() {
        let kafka = kafka_headers(&envelope());
        let http = kafka_headers_to_http(&kafka);

        check!(find(&http, "ce-id") == find(&kafka, "ce_id"));
        check!(find(&http, "ce-type") == find(&kafka, "ce_type"));
        // Not a CloudEvents attribute, so untouched.
        check!(find(&http, "content-type") == Some("application/json"));
        check!(
            find(&http, "ce_id").is_none(),
            "kafka spelling must not survive"
        );
    }

    #[test]
    fn http_to_kafka_reverses_the_translation() {
        let kafka = kafka_headers(&envelope());
        let round_tripped = http_headers_to_kafka(&kafka_headers_to_http(&kafka));
        check!(round_tripped == kafka);
    }

    #[test]
    fn non_cloudevents_headers_pass_through_both_directions() {
        // Trace context rides alongside and must survive: losing it here is
        // exactly the bug crabka's own gateway has (MSG-1).
        let headers = vec![(
            "traceparent".to_string(),
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".to_string(),
        )];
        let http = kafka_headers_to_http(&headers);
        check!(find(&http, "traceparent").is_some());
        check!(http_headers_to_kafka(&http) == headers);
    }
}
