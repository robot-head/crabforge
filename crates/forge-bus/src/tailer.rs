//! Reading a topic from the beginning, without a consumer group.
//!
//! Both of the forge's read paths need to replay a whole topic and then follow
//! it: the command service hydrates its authoritative state from compacted
//! topics at boot, and the projector rebuilds gres tables from event topics.
//! Neither wants consumer-group semantics — the offset belongs in the consumer's
//! own durable state (gres, or in-memory), not in the broker's offset store, and
//! there is no rebalancing to do because these consumers are singletons.
//!
//! Crabka's consumer is subscribe-only (there is no `assign()`), so this drops
//! to `client-core`'s partition fetch, which is what crabka's own schema
//! registry does for exactly this reason.
//!
//! ## The cursor
//!
//! Progress comes from [`FetchPartitionResult::next_offset`], never from
//! `records.last().offset + 1`. Those differ whenever the log contains control
//! batches or records from aborted transactions: those advance the log but are
//! invisible to the reader, so a cursor derived from visible records stops
//! advancing and the tailer spins forever on the same offset. Since every forge
//! write is transactional, that log shape is the normal case here, not an edge
//! case.

use std::net::{SocketAddr, ToSocketAddrs as _};

use crabka_client_core::{
    Client, ClientError, Connection, ConnectionOptions, FetchPartitionResult, IsolatedFetch,
    fetch_partition_with_isolation_progress,
};
use crabka_protocol::primitives::uuid::Uuid as WireUuid;
use tokio::sync::watch;

/// Read committed records only: work in progress and aborted transactions stay
/// invisible. Matches Kafka's `isolation.level=read_committed` (1).
const READ_COMMITTED: i8 = 1;

/// Bytes per fetch. Large enough to make replay of a big topic reasonable,
/// far below the broker's 100 MiB frame.
const FETCH_MAX_BYTES: i32 = 8 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum TailError {
    #[error("client: {0}")]
    Client(#[from] ClientError),
    #[error("topic '{0}' does not exist")]
    UnknownTopic(String),
    #[error("bootstrap address '{0}' did not resolve")]
    BadBootstrap(String),
}

/// One record as read from the log.
pub type FetchedRecord = crabka_client_core::FetchedRecord;

/// A batch of records plus the cursor to resume from.
#[derive(Debug, Clone)]
pub struct Batch {
    pub records: Vec<FetchedRecord>,
    /// Where the next fetch should start.
    pub next_offset: i64,
    /// True when this fetch returned nothing new — the reader has caught up.
    pub caught_up: bool,
}

/// Reads one partition of one topic, tracking its own offset.
pub struct Tailer {
    conn: Connection,
    topic: String,
    topic_id: WireUuid,
    partition: i32,
    offset: i64,
    /// Published so other tasks can wait for the tailer to pass an offset —
    /// the read-your-writes gate.
    applied: watch::Sender<i64>,
}

impl Tailer {
    /// Open a tailer positioned at the start of the topic.
    pub async fn open(bootstrap: &str, topic: &str) -> Result<Self, TailError> {
        Self::open_at(bootstrap, topic, 0).await
    }

    /// Open a tailer positioned at `offset` — used when the cursor was
    /// recovered from durable state.
    ///
    /// Partition 0, which is every topic the forge keys by aggregate and gives
    /// a single partition. Readers of a genuinely partitioned topic want
    /// [`Tailer::open_partition_at`] and one tailer each.
    pub async fn open_at(bootstrap: &str, topic: &str, offset: i64) -> Result<Self, TailError> {
        Self::open_partition_at(bootstrap, topic, 0, offset).await
    }

    /// Open a tailer on one partition of a topic.
    ///
    /// A tailer is deliberately single-partition — it owns a cursor, and a
    /// cursor over several partitions is not a number. Reading a topic with
    /// more than one partition means one tailer and one cursor per partition,
    /// which is also what lets them make progress independently: the reason the
    /// delivery queue is partitioned at all is so a receiver that has stopped
    /// answering blocks its own partition rather than the whole forge.
    pub async fn open_partition_at(
        bootstrap: &str,
        topic: &str,
        partition: i32,
        offset: i64,
    ) -> Result<Self, TailError> {
        // A raw connection rather than a `Client`: this is a single-partition
        // cursor-owning reader, so the pool and routing a `Client` provides
        // would only add indirection. Crabka's own schema registry reads its
        // compacted state topic the same way.
        let addr = resolve_bootstrap(bootstrap)?;
        let opts = ConnectionOptions {
            client_id: "forge-tailer".to_string(),
            ..Default::default()
        };
        let topic_id = resolve_topic_id(bootstrap, topic).await?;
        let conn = Connection::connect_with_options(addr, opts).await?;
        let (applied, _) = watch::channel(offset - 1);
        Ok(Self {
            conn,
            topic: topic.to_string(),
            topic_id,
            partition,
            offset,
            applied,
        })
    }

    /// Which partition this tailer reads.
    pub fn partition(&self) -> i32 {
        self.partition
    }

    /// Current cursor: the offset the next fetch starts from.
    pub fn offset(&self) -> i64 {
        self.offset
    }

    pub fn topic(&self) -> &str {
        &self.topic
    }

    /// Watch the highest offset this tailer has handed out.
    pub fn applied(&self) -> watch::Receiver<i64> {
        self.applied.subscribe()
    }

    /// Publish that everything up to and including `offset` has been applied.
    ///
    /// Separate from fetching because the consumer decides when work is done —
    /// the projector only reports an offset after gres has committed it.
    pub fn mark_applied(&self, offset: i64) {
        self.applied.send_if_modified(|current| {
            if offset > *current {
                *current = offset;
                true
            } else {
                false
            }
        });
    }

    /// Fetch the next batch, waiting up to `max_wait_ms` for records to appear.
    pub async fn next_batch(&mut self, max_wait_ms: i32) -> Result<Batch, TailError> {
        let result: FetchPartitionResult = fetch_partition_with_isolation_progress(
            &self.conn,
            IsolatedFetch {
                topic: &self.topic,
                topic_id: self.topic_id,
                partition: self.partition,
                fetch_offset: self.offset,
                max_wait_ms,
                partition_max_bytes: FETCH_MAX_BYTES,
                isolation_level: READ_COMMITTED,
            },
        )
        .await?;

        let previous = self.offset;
        // `next_offset` steps over control and aborted-transaction batches; a
        // cursor derived from visible records would stall on them.
        if let Some(next) = result.next_offset {
            self.offset = next.max(self.offset);
        }
        Ok(Batch {
            caught_up: result.records.is_empty() && self.offset == previous,
            records: result.records,
            next_offset: self.offset,
        })
    }

    /// Read until caught up, calling `apply` for each record.
    ///
    /// This is the hydration path: the command service folds its compacted state
    /// topics through this at boot, before serving anything.
    pub async fn replay_to_end<F>(&mut self, mut apply: F) -> Result<u64, TailError>
    where
        F: FnMut(&FetchedRecord),
    {
        let mut count = 0u64;
        loop {
            // No wait: replay should finish as fast as the broker can serve it,
            // and an empty response means the end of the log rather than "come
            // back later".
            let batch = self.next_batch(0).await?;
            if batch.records.is_empty() {
                if batch.caught_up {
                    return Ok(count);
                }
                continue;
            }
            for record in &batch.records {
                apply(record);
                count += 1;
            }
            self.mark_applied(batch.next_offset - 1);
        }
    }
}

/// Make `span` a child of the trace the record was written in.
///
/// The other half of the `traceparent` header [`crate::PendingRecord::event`]
/// attaches. A no-op if the record carries no context — records written before
/// this existed, or by something that is not the forge — so it is safe to call
/// on every record rather than only the ones known to have one.
pub fn join_trace(span: &tracing::Span, record: &FetchedRecord) {
    crabka_telemetry::propagation::set_remote_parent(
        span,
        record
            .headers
            .iter()
            .filter_map(|h| h.value.as_ref().map(|v| (h.key.as_str(), v.as_ref()))),
    );
}

fn resolve_bootstrap(bootstrap: &str) -> Result<SocketAddr, TailError> {
    bootstrap
        .to_socket_addrs()
        .ok()
        .and_then(|mut addrs| addrs.next())
        .ok_or_else(|| TailError::BadBootstrap(bootstrap.to_string()))
}

/// Ask the cluster for a topic's UUID.
///
/// Produce and Fetch above v13 carry only `topic_id` on the wire, so the name
/// has to be resolved once up front.
async fn resolve_topic_id(bootstrap: &str, topic: &str) -> Result<WireUuid, TailError> {
    use crabka_protocol::owned::metadata_request::{MetadataRequest, MetadataRequestTopic};

    let client = Client::builder()
        .bootstrap(bootstrap)
        .client_id("forge-tailer-metadata")
        .build()
        .await?;

    let response = client
        .send(MetadataRequest {
            topics: Some(vec![MetadataRequestTopic {
                name: Some(topic.into()),
                ..Default::default()
            }]),
            ..Default::default()
        })
        .await?;

    response
        .topics
        .iter()
        .find(|t| t.name.as_deref() == Some(topic))
        .filter(|t| t.error_code == 0)
        .map(|t| t.topic_id)
        .ok_or_else(|| TailError::UnknownTopic(topic.to_string()))
}
