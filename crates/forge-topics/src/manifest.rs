//! The topic manifest.
//!
//! Only the 15 config keys on crabka's broker whitelist are expressible here
//! (`crates/broker/src/config_keys.rs` upstream). Two consequences shape the
//! specs below:
//!
//! * `cleanup.policy = compact,delete` is rejected — a topic is one or the
//!   other.
//! * There is no `min.cleanable.dirty.ratio` or `segment.ms`, so the only lever
//!   that makes the cleaner (which sweeps sealed segments every 30s) fire on a
//!   low-volume compacted topic is a small `segment.bytes`.

use std::collections::BTreeMap;

use crabka_client_admin::CreateTopicSpec;
use forge_types::{ByteSize, RepoId};

// Re-exported so callers can name a topic without depending on `forge-types`
// directly. `EVENTS_*` names are used via `EVENT_TOPICS` below.
#[allow(unused_imports)]
pub use forge_types::topics::{
    CI_JOBS, CI_LOGS, EVENT_TOPICS, EVENTS_CI, EVENTS_GIT_REFS, EVENTS_ISSUES, EVENTS_PRS,
    EVENTS_REPOS, EVENTS_USERS, GIT_REFS, META_CATALOG, WEBHOOKS_ATTEMPTS, WEBHOOKS_CONFIG,
    WEBHOOKS_DELIVERIES, WEBHOOKS_DLQ,
};

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// Retention policy, mapped onto the broker's `cleanup.policy` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cleanup {
    /// Keep everything forever — the event history *is* the system of record.
    Forever,
    /// Age out after `days`.
    Delete { days: i64 },
    /// Retain the latest record per key; tombstones delete.
    Compact,
}

/// A topic the forge requires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopicSpec {
    pub name: String,
    pub partitions: i32,
    pub replicas: i32,
    pub cleanup: Cleanup,
    pub segment_bytes: ByteSize,
    /// `None` leaves the broker default (`producer` passthrough).
    pub compression: Option<&'static str>,
}

impl TopicSpec {
    fn new(
        name: impl Into<String>,
        partitions: i32,
        cleanup: Cleanup,
        segment_bytes: ByteSize,
    ) -> Self {
        Self {
            name: name.into(),
            partitions,
            // Single-node broker for local development. Multi-broker clusters
            // are driven by crabka's Kubernetes operator, which is where a
            // higher replication factor becomes available.
            replicas: 1,
            cleanup,
            segment_bytes,
            compression: Some("zstd"),
        }
    }

    /// Render to the broker's config map, using only whitelisted keys.
    pub fn configs(&self) -> BTreeMap<String, String> {
        let mut c = BTreeMap::new();
        match self.cleanup {
            Cleanup::Forever => {
                c.insert("cleanup.policy".into(), "delete".into());
                c.insert("retention.ms".into(), "-1".into());
            }
            Cleanup::Delete { days } => {
                c.insert("cleanup.policy".into(), "delete".into());
                c.insert("retention.ms".into(), (days * DAY_MS).to_string());
            }
            Cleanup::Compact => {
                c.insert("cleanup.policy".into(), "compact".into());
            }
        }
        c.insert("segment.bytes".into(), self.segment_bytes.as_config_value());
        if let Some(codec) = self.compression {
            c.insert("compression.type".into(), codec.into());
        }
        c
    }

    pub fn to_create_spec(&self) -> CreateTopicSpec {
        CreateTopicSpec {
            name: self.name.clone(),
            partitions: self.partitions,
            replicas: self.replicas,
            configs: self.configs(),
        }
    }
}

/// The topics every deployment needs, independent of repository count.
pub fn static_topics() -> Vec<TopicSpec> {
    let mut specs = Vec::new();

    // Domain history. One partition: the command service is a single writer, so
    // ordering is global anyway, and a single partition makes the read-your-
    // writes gate one watch channel instead of a per-partition map. Records are
    // still keyed by aggregate id, so a future split preserves per-aggregate
    // ordering.
    for topic in EVENT_TOPICS {
        let segment = if *topic == EVENTS_ISSUES || *topic == EVENTS_PRS {
            ByteSize::mib(32)
        } else {
            ByteSize::mib(16)
        };
        specs.push(TopicSpec::new(*topic, 1, Cleanup::Forever, segment));
    }

    // Command-service state stores. Small segments so compaction actually
    // reclaims superseded records on a quiet forge.
    specs.push(TopicSpec::new(
        META_CATALOG,
        1,
        Cleanup::Compact,
        ByteSize::mib(1),
    ));
    specs.push(TopicSpec::new(
        GIT_REFS,
        1,
        Cleanup::Compact,
        ByteSize::mib(1),
    ));

    // Webhook plane. Deliveries are keyed by webhook id so one dead endpoint
    // only blocks its own partition.
    specs.push(TopicSpec::new(
        WEBHOOKS_CONFIG,
        1,
        Cleanup::Compact,
        ByteSize::mib(1),
    ));
    specs.push(TopicSpec::new(
        WEBHOOKS_DELIVERIES,
        16,
        Cleanup::Delete { days: 7 },
        ByteSize::mib(16),
    ));
    specs.push(TopicSpec::new(
        WEBHOOKS_ATTEMPTS,
        4,
        Cleanup::Delete { days: 7 },
        ByteSize::mib(16),
    ));
    specs.push(TopicSpec::new(
        WEBHOOKS_DLQ,
        4,
        Cleanup::Delete { days: 7 },
        ByteSize::mib(16),
    ));

    // CI plane. Jobs are a share-group work queue; logs are tailed by offset.
    specs.push(TopicSpec::new(
        CI_JOBS,
        16,
        Cleanup::Delete { days: 30 },
        ByteSize::mib(16),
    ));
    specs.push(TopicSpec::new(
        CI_LOGS,
        16,
        Cleanup::Delete { days: 7 },
        ByteSize::mib(16),
    ));

    specs
}

/// The object topic backing one repository.
///
/// Per-repo rather than shared so a cache rebuild replays only that repo's
/// objects, and deleting a repository is a topic deletion rather than a
/// tombstone sweep. Named by id, so renaming a repository costs nothing.
pub fn repo_objects_topic(repo: RepoId) -> TopicSpec {
    TopicSpec {
        name: forge_types::topics::repo_objects(repo),
        partitions: 1,
        replicas: 1,
        cleanup: Cleanup::Compact,
        // Well above the 4 MiB chunk size: compaction here is a dedupe
        // optimization, not a correctness requirement, so slow sealing on a
        // quiet repository is fine.
        segment_bytes: ByteSize::mib(64),
        // Git content is frequently incompressible and blobs are large; leave
        // the bytes as the producer framed them rather than burning broker CPU.
        compression: Some("producer"),
    }
}

#[cfg(test)]
mod tests {
    use assert2::{assert, check};

    use super::*;

    /// Exactly the keys crabka's broker accepts (`config_keys.rs` upstream).
    /// A topic carrying anything else is rejected with `INVALID_CONFIG` at
    /// create time, which would only surface at runtime.
    const WHITELIST: &[&str] = &[
        "retention.ms",
        "retention.bytes",
        "segment.bytes",
        "cleanup.policy",
        "compression.type",
        "min.insync.replicas",
        "unclean.leader.election.enable",
        "unclean.recovery.strategy",
        "remote.storage.enable",
        "local.retention.ms",
        "local.retention.bytes",
        "delete.retention.ms",
        "qos.tier",
    ];

    #[test]
    fn every_config_key_is_on_the_broker_whitelist() {
        let mut specs = static_topics();
        specs.push(repo_objects_topic(RepoId::new()));
        for spec in specs {
            for key in spec.configs().keys() {
                check!(
                    WHITELIST.contains(&key.as_str()),
                    "topic {} sets non-whitelisted key {key}",
                    spec.name
                );
            }
        }
    }

    #[test]
    fn cleanup_policy_is_never_the_rejected_compound_value() {
        for spec in static_topics() {
            let policy = spec.configs().get("cleanup.policy").cloned();
            assert!(let Some(policy) = policy);
            check!(
                policy == "delete" || policy == "compact",
                "broker rejects '{policy}' for {}",
                spec.name
            );
        }
    }

    #[test]
    fn compacted_topics_seal_segments_small_enough_for_the_cleaner() {
        for spec in static_topics()
            .into_iter()
            .filter(|s| s.cleanup == Cleanup::Compact)
        {
            check!(
                spec.segment_bytes <= ByteSize::mib(1),
                "{} needs a small segment.bytes or compaction never fires",
                spec.name
            );
        }
    }

    #[test]
    fn partitions_are_always_explicit_and_positive() {
        // The broker rejects `num_partitions = -1` ("use cluster default").
        for spec in static_topics() {
            check!(
                spec.partitions > 0,
                "{} has no explicit partition count",
                spec.name
            );
            check!(spec.replicas > 0);
        }
    }

    #[test]
    fn event_topics_are_retained_forever() {
        for spec in static_topics()
            .into_iter()
            .filter(|s| EVENT_TOPICS.contains(&s.name.as_str()))
        {
            check!(spec.configs().get("retention.ms").map(String::as_str) == Some("-1"));
        }
    }

    #[test]
    fn object_segments_hold_many_chunks_without_approaching_the_frame_limit() {
        // Segments must be comfortably larger than a chunk (so a chunk is never
        // split across segments) while each chunk stays under the broker's
        // unconfigurable wire frame. `ByteSize` is what keeps these three
        // magnitudes comparable without unit arithmetic by hand.
        let spec = repo_objects_topic(RepoId::new());
        let chunk = forge_types::limits::object_chunk();
        check!(spec.segment_bytes > chunk);
        check!(chunk < forge_types::limits::max_frame());
    }

    #[test]
    fn repo_object_topics_are_named_by_id_so_renames_are_free() {
        let repo = RepoId::new();
        check!(repo_objects_topic(repo).name == forge_types::topics::repo_objects(repo));
    }

    #[test]
    fn topic_names_are_unique() {
        let specs = static_topics();
        let mut names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        check!(
            names.len() == before,
            "duplicate topic name in the manifest"
        );
    }
}
