//! Crabforge domain events.
//!
//! The event log is the system of record: every queryable thing in the forge is
//! a projection of these types. They are written to crabka topics as
//! CloudEvents (see [`ce`]) wrapped in an [`Envelope`].

pub mod ce;
mod domain;
mod envelope;

pub use domain::{GitRefEvent, IssueEvent, RepoEvent, UserEvent};
pub use envelope::{DecodeError, DomainEvent, Envelope, RawEnvelope, decode_raw};

/// Topic names, re-exported from `forge-types` where they are defined.
pub use forge_types::topics;
