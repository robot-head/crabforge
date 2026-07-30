//! The single writer.
//!
//! Every event and every piece of compacted command-service state is written
//! through one [`FencedWriter`], inside a broker transaction. Two properties
//! follow, and the whole design depends on both:
//!
//! **Atomicity across topics.** A command usually has to append a domain event
//! *and* update a compacted state record — claiming a username, moving a ref.
//! Kafka transactions span topics, so those land together or not at all. There
//! is no window where a ref has moved but the reflog does not show it.
//!
//! **Fencing.** The writer takes a fixed `transactional_id`. Calling
//! `init_transactions` bumps the producer epoch, which permanently fences any
//! older instance still holding that id: its next commit fails rather than
//! interleaving with ours. This is what makes "single writer" a property of the
//! broker rather than a deployment convention — during a rolling restart, or a
//! network partition where the old process is still running, at most one writer
//! can commit.
//!
//! The pattern is crabka's own, from `crates/gres-substrate/src/writer.rs`,
//! which uses it to keep one SQL tenant's write-ahead log linear.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use bytes::Bytes;
use crabka_client_producer::{Header, Producer, ProducerError, ProducerRecord};
use forge_events::{DomainEvent, Envelope, ce};

/// The transactional id the command service claims.
///
/// Fixed, because its purpose is for a *new* instance to fence an old one — a
/// per-process id would fence nothing.
pub const COMMAND_TRANSACTIONAL_ID: &str = "forge.cmd.main";

/// The transactional id for git object writes.
///
/// Deliberately *not* [`COMMAND_TRANSACTIONAL_ID`]: two writers sharing an id
/// fence each other, so reusing it would mean the first object write killed the
/// command service. They are separate logical writers because objects are
/// content-addressed and immutable — they need no ordering against domain
/// events, and a large push should not block command processing while it
/// uploads.
pub const OBJECT_TRANSACTIONAL_ID: &str = "forge.objects.main";

/// The transactional id the webhook matcher claims.
///
/// A third identity for the same reason as the second: the matcher writes
/// continuously as events arrive, and sharing an id with the command service
/// would mean the first webhook fan-out fenced it. It is also genuinely a
/// different writer — it produces nothing the command service decides, only
/// consequences of decisions already committed.
pub const WEBHOOK_TRANSACTIONAL_ID: &str = "forge.webhooks.main";

#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    /// Another writer took over. This instance must stop writing; it can no
    /// longer be sure its view of state is current.
    #[error("fenced by a newer writer instance")]
    Fenced,
    #[error("producer: {0}")]
    Producer(#[from] ProducerError),
    #[error("serializing event: {0}")]
    Encode(#[from] serde_json::Error),
}

/// Where a transaction's records landed, per topic.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Committed {
    /// Highest offset written per topic. The read-your-writes gate waits for a
    /// projector to reach these.
    pub offsets: Vec<(String, i64)>,
}

impl Committed {
    pub fn offset_for(&self, topic: &str) -> Option<i64> {
        self.offsets
            .iter()
            .find(|(t, _)| t == topic)
            .map(|(_, o)| *o)
    }
}

/// A record to write inside a transaction.
pub struct PendingRecord {
    pub topic: String,
    pub key: String,
    /// `None` writes a tombstone, which deletes the key from a compacted topic.
    pub value: Option<Vec<u8>>,
    pub headers: Vec<(String, String)>,
}

impl PendingRecord {
    /// A domain event, encoded with its CloudEvents headers.
    pub fn event<E: DomainEvent + Clone + serde::Serialize>(
        event: &E,
        actor: Option<forge_types::UserId>,
    ) -> Result<Self, WriteError> {
        let envelope = Envelope::new(event, actor);
        Ok(Self {
            topic: event.topic().to_string(),
            key: envelope.aggregate_id.clone(),
            value: Some(serde_json::to_vec(&envelope)?),
            headers: ce::kafka_headers(&envelope),
        })
    }

    /// A compacted state record: the current value for a key.
    pub fn state(
        topic: &str,
        key: impl Into<String>,
        value: &impl serde::Serialize,
    ) -> Result<Self, WriteError> {
        Ok(Self {
            topic: topic.to_string(),
            key: key.into(),
            value: Some(serde_json::to_vec(value)?),
            headers: Vec::new(),
        })
    }

    /// A tombstone: removes the key once compaction runs.
    pub fn tombstone(topic: &str, key: impl Into<String>) -> Self {
        Self {
            topic: topic.to_string(),
            key: key.into(),
            value: None,
            headers: Vec::new(),
        }
    }

    /// Encode for the producer, attaching the writer's W3C trace context.
    ///
    /// Every record, not just domain events: the log is the only thing joining
    /// the forge's processes, so a push, the command that decided it, the
    /// projection that applied it, the webhook it triggered and the CI job it
    /// queued are one trace only if the context travels with the record. The CI
    /// queue in particular carries no envelope, so attaching this in
    /// [`PendingRecord::event`] would leave the runner out of the trace.
    ///
    /// Attached here rather than at construction so it is the context of the
    /// task that *writes* — a record built in one span and transacted in
    /// another belongs to the transaction.
    fn into_producer_record(self) -> ProducerRecord {
        let headers = self
            .headers
            .into_iter()
            .chain(crabka_telemetry::propagation::current_trace_headers())
            .map(|(key, value)| Header {
                key,
                value: Some(Bytes::from(value.into_bytes())),
            })
            .collect();
        ProducerRecord {
            topic: self.topic,
            partition: None,
            key: Some(Bytes::from(self.key.into_bytes())),
            value: self.value.map(Bytes::from),
            headers,
            timestamp_ms: None,
        }
    }
}

/// What to do when a transaction's outcome cannot be determined.
///
/// Production aborts the process. That looks drastic, and it is deliberate:
/// after an indeterminate `EndTxn` the writer does not know whether its records
/// are committed. Reporting failure could tell a user their push was rejected
/// when it actually landed; reporting success could do the reverse. Neither is
/// recoverable by the caller. Dying means the records are either committed or
/// aborted by the broker's transaction timeout, and the restarted instance
/// re-reads the truth from the log.
///
/// Tests substitute a recorder so they can assert on the behaviour without
/// taking the test process down.
pub type IndeterminateHandler = Arc<dyn Fn(&ProducerError) + Send + Sync>;

fn abort_process() -> IndeterminateHandler {
    Arc::new(|error| {
        tracing::error!(%error, "indeterminate transaction outcome; terminating to avoid acknowledging an unknown result");
        std::process::abort();
    })
}

/// A transactional producer that fences its predecessors.
pub struct FencedWriter {
    producer: Producer,
    /// Latched once fencing is observed. A fenced writer never recovers: its
    /// in-memory state may already be stale relative to the new writer's.
    fenced: AtomicBool,
    on_indeterminate: IndeterminateHandler,
}

impl FencedWriter {
    /// Connect and fence any previous holder of [`COMMAND_TRANSACTIONAL_ID`].
    pub async fn connect(bootstrap: &str) -> Result<Self, WriteError> {
        Self::connect_with_id(bootstrap, COMMAND_TRANSACTIONAL_ID).await
    }

    pub async fn connect_with_id(
        bootstrap: &str,
        transactional_id: &str,
    ) -> Result<Self, WriteError> {
        let producer = Producer::builder()
            .bootstrap(bootstrap)
            .client_id("forge-command")
            .transactional_id(transactional_id.to_string())
            .build()
            .await?;

        // Bumps the producer epoch, fencing any older instance holding this id.
        producer.init_transactions().await?;

        Ok(Self {
            producer,
            fenced: AtomicBool::new(false),
            on_indeterminate: abort_process(),
        })
    }

    /// Replace the indeterminate-outcome action. Tests only.
    #[doc(hidden)]
    pub fn with_indeterminate_handler(mut self, handler: IndeterminateHandler) -> Self {
        self.on_indeterminate = handler;
        self
    }

    pub fn is_fenced(&self) -> bool {
        self.fenced.load(Ordering::Acquire)
    }

    /// Append `records` atomically.
    ///
    /// Resolves only once the broker has committed. Records land together or
    /// not at all, across however many topics they span.
    pub async fn transact(&self, records: Vec<PendingRecord>) -> Result<Committed, WriteError> {
        if self.is_fenced() {
            return Err(WriteError::Fenced);
        }
        if records.is_empty() {
            return Ok(Committed::default());
        }

        let txn = match self.producer.begin_transaction().await {
            Ok(txn) => txn,
            Err(e) => return Err(self.classify(e)),
        };

        // Fan the sends out, then collect acknowledgements: `send` returns a
        // receiver rather than awaiting the broker, so a multi-record command
        // costs one round trip rather than one per record.
        let mut acks = Vec::with_capacity(records.len());
        for record in records {
            let topic = record.topic.clone();
            let rx = self.producer.send(record.into_producer_record()).await;
            acks.push((topic, rx));
        }

        let mut offsets: Vec<(String, i64)> = Vec::new();
        for (topic, rx) in acks {
            let metadata = match rx.await {
                Ok(Ok(metadata)) => metadata,
                Ok(Err(e)) => {
                    let _ = txn.abort().await;
                    return Err(self.classify(e));
                }
                Err(_) => {
                    let _ = txn.abort().await;
                    return Err(WriteError::Producer(ProducerError::Closed));
                }
            };
            match offsets.iter_mut().find(|(t, _)| *t == topic) {
                Some((_, offset)) => *offset = (*offset).max(metadata.offset),
                None => offsets.push((topic, metadata.offset)),
            }
        }

        if let Err(e) = txn.commit().await {
            return Err(self.classify(e.source));
        }
        Ok(Committed { offsets })
    }

    /// Map a producer error onto our failure model, latching fenced state and
    /// escalating an unknown outcome.
    fn classify(&self, error: ProducerError) -> WriteError {
        match error {
            ProducerError::FencedProducer | ProducerError::TransactionAborted => {
                self.fenced.store(true, Ordering::Release);
                tracing::error!(
                    "writer fenced by a newer instance; this process must stop writing"
                );
                WriteError::Fenced
            }
            ProducerError::RecoveryRequired => {
                // The broker's answer never arrived. We cannot tell the caller
                // anything truthful about whether the write happened.
                (self.on_indeterminate)(&error);
                WriteError::Producer(error)
            }
            other => WriteError::Producer(other),
        }
    }
}
