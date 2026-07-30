//! Getting a queued delivery to its receiver.
//!
//! Reads the queue topic, sends, records what happened, and retries what is
//! worth retrying. Every attempt is written to `webhook_deliveries` whether it
//! worked or not, because the question a maintainer actually asks is "why has
//! my integration stopped working", and that is unanswerable from successes.
//!
//! ## Retries happen in place
//!
//! An attempt that fails is retried inside this worker, with backoff, rather
//! than by re-queueing onto the topic. Re-queueing would be more elastic, but
//! it would also reorder: a delivery that failed once would land behind
//! everything queued since, and a receiver that cares about sequence — most do,
//! for a repository's pushes — would see them out of order. Holding the record
//! while it retries preserves per-webhook ordering, which is the property this
//! topic's keying exists to provide.
//!
//! The cost is head-of-line blocking: one dead endpoint stalls its own
//! partition. That is why the topic is keyed by webhook and has more than one
//! partition — a dead receiver blocks itself and its partition-mates, not the
//! forge. Bounding it is what `MAX_ATTEMPTS` is for.

use std::{sync::Arc, time::Duration};

use forge_bus::{FencedWriter, PendingRecord, TailError, Tailer, WriteError};
use forge_store::{DeliveryRecord, Store, StoreError};
use forge_types::topics;

use crate::{
    Deliverer, Delivery, DeliveryOutcome, Payload, delivery::MAX_ATTEMPTS, queue::DeliveryRequest,
};

/// How long to wait for new records before looping again.
const POLL_WAIT_MS: i32 = 500;

#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    #[error("reading the delivery queue: {0}")]
    Tail(#[from] TailError),
    #[error("recording an attempt: {0}")]
    Store(#[from] StoreError),
    #[error("writing to the dead-letter topic: {0}")]
    Write(#[from] WriteError),
}

/// Delivers what the matcher queued.
pub struct Worker {
    tailer: Tailer,
    store: Store,
    deliverer: Deliverer,
    writer: Arc<FencedWriter>,
    /// How long to wait before attempt `n + 1`. Injected so tests can exercise
    /// the retry ladder without spending its real minutes.
    backoff: fn(i64) -> Duration,
}

impl Worker {
    /// How many partitions the delivery queue has.
    ///
    /// Taken from the topic manifest rather than from broker metadata so that a
    /// worker fleet and the topic it reads cannot disagree about the count —
    /// a worker short of a partition would leave those deliveries unsent
    /// forever, and nothing would say so.
    pub fn partitions() -> i32 {
        forge_topics::static_topics()
            .iter()
            .find(|spec| spec.name == topics::WEBHOOKS_DELIVERIES)
            .map_or(1, |spec| spec.partitions)
    }

    /// Open a worker on one partition of the delivery queue.
    pub async fn open(
        bootstrap: &str,
        partition: i32,
        store: Store,
        deliverer: Deliverer,
        writer: Arc<FencedWriter>,
    ) -> Result<Self, WorkerError> {
        let resume_from = store
            .cursors(forge_store::WEBHOOK_WORKER)
            .applied_offset_for(topics::WEBHOOKS_DELIVERIES, partition)
            .await?;
        let tailer = Tailer::open_partition_at(
            bootstrap,
            topics::WEBHOOKS_DELIVERIES,
            partition,
            resume_from,
        )
        .await?;
        tracing::info!(partition, resume_from, "webhook worker opened");
        Ok(Self {
            tailer,
            store,
            deliverer,
            writer,
            backoff: crate::delivery::backoff,
        })
    }

    /// Replace the backoff schedule. Tests only — the real ladder spans
    /// minutes, which is right in production and useless in a test.
    pub fn with_backoff(mut self, backoff: fn(i64) -> Duration) -> Self {
        self.backoff = backoff;
        self
    }

    /// Read one batch and deliver it. Returns how many deliveries were handled.
    pub async fn step(&mut self) -> Result<usize, WorkerError> {
        let batch = self.tailer.next_batch(POLL_WAIT_MS).await?;
        if batch.records.is_empty() {
            return Ok(0);
        }

        let mut handled = 0;
        for record in &batch.records {
            let Some(request) = readable_request(record.value.as_deref()) else {
                continue;
            };
            let span = tracing::info_span!(
                "webhook_deliver",
                webhook_id = %request.webhook_id,
                delivery = %request.delivery_id,
            );
            forge_bus::join_trace(&span, record);
            tracing::Instrument::instrument(self.deliver_with_retries(&request), span).await?;
            handled += 1;
        }

        self.store
            .cursors(forge_store::WEBHOOK_WORKER)
            .set_applied_offset_for(
                topics::WEBHOOKS_DELIVERIES,
                self.tailer.partition(),
                batch.next_offset,
            )
            .await?;
        Ok(handled)
    }

    /// Run until cancelled.
    pub async fn run(mut self, mut shutdown: tokio::sync::watch::Receiver<bool>) {
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        tracing::info!(
                            partition = self.tailer.partition(),
                            "webhook worker stopping"
                        );
                        return;
                    }
                }
                result = self.step() => {
                    if let Err(error) = result {
                        tracing::warn!(%error, "webhook worker step failed; retrying");
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                }
            }
        }
    }

    /// Send, and keep sending until it works or stops being worth trying.
    async fn deliver_with_retries(&self, request: &DeliveryRequest) -> Result<(), WorkerError> {
        let Some(hook) = self.store.hooks().by_id(&request.webhook_id).await? else {
            // Deleted between queueing and delivery. Not an error: the operator
            // has said they no longer want these, and the queue is allowed to
            // hold work that has since become pointless.
            tracing::debug!(
                webhook_id = %request.webhook_id,
                "dropping a delivery for a webhook that no longer exists"
            );
            return Ok(());
        };
        if !hook.active {
            tracing::debug!(webhook_id = %hook.webhook_id, "webhook is disabled; not delivering");
            return Ok(());
        }

        for attempt in 1..=MAX_ATTEMPTS {
            let delivery = Delivery {
                webhook: hook.clone(),
                payload: Payload {
                    event_id: request.delivery_id.clone(),
                    event_type: request.event_type().to_string(),
                    body: request.body(),
                    ce_headers: crate::ce_headers(&request.event),
                },
                attempt,
            };

            let started = std::time::Instant::now();
            let outcome = self.deliverer.send(&delivery).await;
            // Labelled with the same word the stored history uses, so a
            // dashboard and the deliveries table cannot disagree about what
            // "dead" means.
            forge_metrics::record_delivery(
                outcome.status_word(attempt),
                started.elapsed().as_secs_f64(),
            );
            self.record(request, attempt, &outcome).await?;

            match &outcome {
                DeliveryOutcome::Delivered { .. } => return Ok(()),
                DeliveryOutcome::Permanent { reason, .. } => {
                    tracing::info!(
                        webhook_id = %hook.webhook_id,
                        delivery = %request.delivery_id,
                        reason,
                        "delivery abandoned"
                    );
                    return self.dead_letter(request, reason).await;
                }
                DeliveryOutcome::Retry { reason, .. } if attempt >= MAX_ATTEMPTS => {
                    tracing::info!(
                        webhook_id = %hook.webhook_id,
                        delivery = %request.delivery_id,
                        attempts = attempt,
                        reason,
                        "delivery exhausted its attempts"
                    );
                    return self.dead_letter(request, reason).await;
                }
                DeliveryOutcome::Retry { reason, .. } => {
                    let wait = (self.backoff)(attempt);
                    tracing::debug!(
                        webhook_id = %hook.webhook_id,
                        attempt,
                        ?wait,
                        reason,
                        "delivery failed; will retry"
                    );
                    tokio::time::sleep(wait).await;
                }
            }
        }
        Ok(())
    }

    /// Write one attempt to the history a maintainer reads.
    async fn record(
        &self,
        request: &DeliveryRequest,
        attempt: i64,
        outcome: &DeliveryOutcome,
    ) -> Result<(), StoreError> {
        let (status_code, error, duration_ms) = match outcome {
            DeliveryOutcome::Delivered {
                status,
                duration_ms,
            } => (Some(*status as i64), None, Some(*duration_ms)),
            DeliveryOutcome::Retry { reason, status }
            | DeliveryOutcome::Permanent { reason, status } => {
                (status.map(|s| s as i64), Some(reason.clone()), None)
            }
        };

        self.store
            .hooks()
            .record_attempt(&DeliveryRecord {
                // One row per attempt, so the id is per attempt too. The
                // receiver-facing identity is `request.delivery_id`, which is
                // stable across all of them.
                delivery_id: format!("{}#{attempt}", request.delivery_id),
                webhook_id: request.webhook_id.clone(),
                repo_id: request.repo_id.clone(),
                event_type: request.event_type().to_string(),
                event_id: request.delivery_id.clone(),
                attempt,
                status: outcome.status_word(attempt).to_string(),
                status_code,
                error,
                duration_ms,
                created_at: forge_types::now(),
            })
            .await
    }

    /// Park a delivery nobody could take.
    ///
    /// On the log rather than only in SQL, so a redelivery button can replay it
    /// and an operator can see what was lost without a database query.
    async fn dead_letter(
        &self,
        request: &DeliveryRequest,
        reason: &str,
    ) -> Result<(), WorkerError> {
        let record = PendingRecord::state(
            topics::WEBHOOKS_DLQ,
            request.webhook_id.clone(),
            &serde_json::json!({
                "delivery_id": request.delivery_id,
                "webhook_id": request.webhook_id,
                "repo_id": request.repo_id,
                "event_type": request.event_type(),
                "reason": reason,
                "abandoned_at": forge_types::now(),
                "event": request.event,
            }),
        )?;
        self.writer.transact(vec![record]).await?;
        Ok(())
    }
}

/// Decode a queued request, skipping anything unreadable.
fn readable_request(value: Option<&[u8]>) -> Option<DeliveryRequest> {
    let bytes = value?;
    match serde_json::from_slice(bytes) {
        Ok(request) => Some(request),
        Err(error) => {
            tracing::warn!(%error, "skipping an unreadable delivery request");
            None
        }
    }
}
