//! Outbound webhooks.
//!
//! When something happens in a repository, anyone who asked to be told is told.
//! The mechanics — per-endpoint ordering, retry with backoff, a dead-letter
//! horizon — follow crabka's own gateway (`crates/grpc-gateway/src/outbound.rs`),
//! because that code has already learned the lessons. What differs is the
//! shape:
//!
//! * The gateway runs one consumer group per subscription, configured in TOML
//!   at boot. A forge has one subscription per repository per integration, and
//!   they are created and deleted by users all day, so subscriptions live in
//!   the database and one matcher serves all of them.
//! * The gateway sends a proprietary envelope and drops record headers. Forge
//!   events are already CloudEvents, so a delivery carries the standard
//!   attributes as well as the GitHub-compatible headers integrations expect.
//! * The gateway allow-lists target hosts, which suits an operator's own
//!   integrations. Users supply these, so [`target`] deny-lists destinations
//!   after resolution instead.

pub mod delivery;
pub mod matcher;
pub mod queue;
pub mod signature;
pub mod target;
pub mod worker;

pub use delivery::{Deliverer, Delivery, DeliveryOutcome, Payload};
pub use matcher::{MatchError, Matcher, matched_topics};
pub use queue::{DeliveryRequest, delivery_id};
pub use signature::{SIGNATURE_HEADER, sign, verify};
pub use target::{TargetError, check_url, is_public, resolve_and_check};
pub use worker::{Worker, WorkerError};

/// The CloudEvents attributes to put on an outgoing request.
///
/// Translated from the Kafka spelling (`ce_id`) to the HTTP one (`ce-id`) at
/// this boundary, which is what the CloudEvents Kafka binding specifies and
/// what a receiver's library will look for.
pub fn ce_headers(event: &forge_events::RawEnvelope) -> Vec<(String, String)> {
    forge_events::ce::kafka_headers_to_http(&forge_events::ce::kafka_headers(event))
}
