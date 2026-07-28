//! Projecting events into gres.
//!
//! The projector reads a topic and writes the rows the API queries. It is the
//! only writer of projection tables, which is what makes read-then-write safe
//! in place of the `ON CONFLICT` gres does not have.
//!
//! ## Exactly-once, without distributed transactions
//!
//! Delivery from the log is at-least-once, and a Kafka transaction cannot span
//! a gres write, so `send_offsets_to_transaction` does not help here — the
//! output is SQL, not another topic. Instead each batch is applied like this:
//!
//! ```text
//! BEGIN
//!   apply every event in the batch
//!   UPDATE projector_state SET applied_offset = <batch end>
//! COMMIT
//! ```
//!
//! The cursor moves in the same transaction as the rows it accounts for. A
//! crash before the commit replays the batch — harmless, because applying an
//! event twice produces the same row. A crash after it does not replay. The
//! effect is exactly-once even though the delivery is not.
//!
//! ## Applied offsets
//!
//! After each commit the projector publishes how far it has got. HTTP handlers
//! wait on that to read their own writes: the API returns only once the
//! projection covers the offset the command committed at.

use std::{sync::Arc, time::Duration};

use forge_bus::{TailError, Tailer};
use forge_events::{IssueEvent, RepoEvent, UserEvent, decode_raw};
use forge_store::{RepoRecord, Store, StoreError, UserRecord};
use forge_types::topics;
use tokio::sync::watch;

mod apply;

pub use apply::{apply_issue_event, apply_repo_event, apply_user_event};

/// How long to wait for new records before looping again.
const POLL_WAIT_MS: i32 = 500;

#[derive(Debug, thiserror::Error)]
pub enum ProjectorError {
    #[error("reading the log: {0}")]
    Tail(#[from] TailError),
    #[error("writing read models: {0}")]
    Store(#[from] StoreError),
}

/// Projects one topic.
pub struct Projector {
    tailer: Tailer,
    store: Arc<Store>,
    applied: watch::Sender<i64>,
}

impl Projector {
    /// Open a projector positioned at its durable cursor.
    pub async fn open(
        bootstrap: &str,
        topic: &str,
        store: Arc<Store>,
    ) -> Result<Self, ProjectorError> {
        // Resume from gres, not from a broker-side consumer group offset: the
        // cursor has to move atomically with the rows, so it lives with them.
        let resume_from = store.cursors().applied_offset(topic).await?;
        let tailer = Tailer::open_at(bootstrap, topic, resume_from).await?;
        let (applied, _) = watch::channel(resume_from - 1);
        tracing::info!(topic, resume_from, "projector opened");
        Ok(Self {
            tailer,
            store,
            applied,
        })
    }

    /// Watch how far this projector has applied.
    pub fn applied(&self) -> watch::Receiver<i64> {
        self.applied.subscribe()
    }

    pub fn topic(&self) -> &str {
        self.tailer.topic()
    }

    /// Apply everything available right now, and report how many events landed.
    pub async fn drain(&mut self) -> Result<u64, ProjectorError> {
        let mut total = 0;
        loop {
            let batch = self.tailer.next_batch(0).await?;
            if batch.records.is_empty() {
                if batch.caught_up {
                    return Ok(total);
                }
                // The batch held only invisible records (control or aborted
                // batches); the cursor still moved, so record that and continue.
                self.commit_cursor(batch.next_offset).await?;
                continue;
            }

            let client = self.store.client();
            client
                .batch_execute("BEGIN")
                .await
                .map_err(StoreError::Sql)?;

            let mut applied = 0;
            for record in &batch.records {
                match self.apply_record(record).await {
                    Ok(true) => applied += 1,
                    Ok(false) => {}
                    Err(e) => {
                        // Roll back so the cursor does not advance past an event
                        // we failed to apply; the batch is retried on the next
                        // pass rather than silently skipped.
                        let _ = client.batch_execute("ROLLBACK").await;
                        return Err(e);
                    }
                }
            }

            if let Err(e) = self
                .store
                .cursors()
                .set_applied_offset(self.tailer.topic(), batch.next_offset)
                .await
            {
                let _ = client.batch_execute("ROLLBACK").await;
                return Err(e.into());
            }
            client
                .batch_execute("COMMIT")
                .await
                .map_err(StoreError::Sql)?;

            self.publish_applied(batch.next_offset - 1);
            total += applied;
        }
    }

    /// Run until cancelled, applying events as they arrive.
    pub async fn run(mut self) -> Result<(), ProjectorError> {
        loop {
            self.drain().await?;
            // Long-poll: the broker holds the fetch open until records arrive
            // or the wait elapses, so an idle projector is not a busy loop.
            let batch = self.tailer.next_batch(POLL_WAIT_MS).await?;
            if !batch.records.is_empty() {
                // Records arrived during the long poll. `next_batch` already
                // advanced the cursor past them, so rewind is impossible —
                // apply this batch directly rather than re-fetching.
                self.apply_batch(&batch).await?;
            }
        }
    }

    async fn apply_batch(&mut self, batch: &forge_bus::Batch) -> Result<(), ProjectorError> {
        let client = self.store.client();
        client
            .batch_execute("BEGIN")
            .await
            .map_err(StoreError::Sql)?;
        for record in &batch.records {
            if let Err(e) = self.apply_record(record).await {
                let _ = client.batch_execute("ROLLBACK").await;
                return Err(e);
            }
        }
        if let Err(e) = self
            .store
            .cursors()
            .set_applied_offset(self.tailer.topic(), batch.next_offset)
            .await
        {
            let _ = client.batch_execute("ROLLBACK").await;
            return Err(e.into());
        }
        client
            .batch_execute("COMMIT")
            .await
            .map_err(StoreError::Sql)?;
        self.publish_applied(batch.next_offset - 1);
        Ok(())
    }

    /// Apply one record. Returns whether it was a recognized event.
    async fn apply_record(
        &self,
        record: &forge_bus::FetchedRecord,
    ) -> Result<bool, ProjectorError> {
        let envelope = match decode_raw(record.value.as_deref()) {
            Ok(envelope) => envelope,
            Err(e) => {
                tracing::warn!(offset = record.offset, error = %e, "skipping undecodable record");
                return Ok(false);
            }
        };

        let topic = self.tailer.topic();
        if topic == topics::EVENTS_USERS {
            match envelope.parse::<UserEvent>() {
                Ok(parsed) => {
                    apply_user_event(&self.store, &parsed.payload, parsed.occurred_at).await?;
                    return Ok(true);
                }
                Err(e) => {
                    // Forward compatibility: an event this build does not know
                    // about is skipped, not fatal. The cursor still advances,
                    // so a newer writer cannot wedge an older projector.
                    tracing::warn!(
                        event_type = %envelope.event_type,
                        error = %e,
                        "skipping unrecognized user event"
                    );
                }
            }
        } else if topic == topics::EVENTS_ISSUES {
            match envelope.parse::<IssueEvent>() {
                Ok(parsed) => {
                    apply_issue_event(&self.store, &parsed.payload, parsed.occurred_at).await?;
                    return Ok(true);
                }
                Err(e) => {
                    tracing::warn!(
                        event_type = %envelope.event_type,
                        error = %e,
                        "skipping unrecognized issue event"
                    );
                }
            }
        } else if topic == topics::EVENTS_REPOS {
            match envelope.parse::<RepoEvent>() {
                Ok(parsed) => {
                    apply_repo_event(&self.store, &parsed.payload, parsed.occurred_at).await?;
                    return Ok(true);
                }
                Err(e) => {
                    tracing::warn!(
                        event_type = %envelope.event_type,
                        error = %e,
                        "skipping unrecognized repo event"
                    );
                }
            }
        }
        Ok(false)
    }

    async fn commit_cursor(&self, offset: i64) -> Result<(), ProjectorError> {
        self.store
            .cursors()
            .set_applied_offset(self.tailer.topic(), offset)
            .await?;
        self.publish_applied(offset - 1);
        Ok(())
    }

    fn publish_applied(&self, offset: i64) {
        self.applied.send_if_modified(|current| {
            if offset > *current {
                *current = offset;
                true
            } else {
                false
            }
        });
    }
}

/// Wait until `applied` reaches `offset`.
///
/// The read-your-writes gate: an HTTP handler calls this with the offset its
/// command committed at, then reads the projection knowing the write is there.
/// Returns `false` on timeout, which the caller turns into a 202 rather than a
/// failure — the write did land, the projection just has not caught up.
pub async fn wait_for_offset(
    mut applied: watch::Receiver<i64>,
    offset: i64,
    within: Duration,
) -> bool {
    if *applied.borrow() >= offset {
        return true;
    }
    tokio::time::timeout(within, async {
        while applied.changed().await.is_ok() {
            if *applied.borrow() >= offset {
                return true;
            }
        }
        false
    })
    .await
    .unwrap_or(false)
}

/// Convenience: a `UserRecord` from a registration event.
pub(crate) fn user_record(
    user_id: &str,
    username: &str,
    username_lower: &str,
    email: &str,
    password_hash: &str,
    at: time::OffsetDateTime,
) -> UserRecord {
    UserRecord {
        user_id: user_id.to_string(),
        username: username.to_string(),
        username_lower: username_lower.to_string(),
        email: email.to_string(),
        password_hash: password_hash.to_string(),
        display_name: None,
        bio: None,
        state: "active".to_string(),
        created_at: at,
        updated_at: at,
    }
}

pub(crate) fn repo_record_defaults(repo_id: &str, at: time::OffsetDateTime) -> RepoRecord {
    RepoRecord {
        repo_id: repo_id.to_string(),
        owner_id: String::new(),
        owner_name: String::new(),
        name: String::new(),
        full_name_lower: String::new(),
        description: None,
        default_branch: "main".to_string(),
        visibility: "public".to_string(),
        created_at: at,
        updated_at: at,
        deleted: false,
    }
}
