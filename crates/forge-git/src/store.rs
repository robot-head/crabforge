//! Writing git objects to the log, and reading them back.
//!
//! Objects go in before the reference that names them, and outside any
//! transaction. They are content-addressed and immutable, so writing one twice
//! is harmless and writing one that ends up unreferenced leaves garbage rather
//! than corruption. That keeps the transaction that moves a reference small —
//! a push of several hundred megabytes does not become a several-hundred-
//! megabyte transaction.

use forge_bus::{FencedWriter, PendingRecord, WriteError};
use forge_types::{Oid, RepoId, topics};

use crate::frame::{self, Kind};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("writing objects: {0}")]
    Write(#[from] WriteError),
    #[error("frame: {0}")]
    Frame(#[from] frame::FrameError),
}

/// An object to store.
pub struct Object {
    pub oid: Oid,
    pub kind: Kind,
    pub content: Vec<u8>,
}

/// Write objects to a repository's topic.
///
/// Batched into transactions to bound how much a single failure has to redo,
/// not for atomicity — objects need none.
pub struct ObjectWriter<'a> {
    writer: &'a FencedWriter,
    topic: String,
}

/// How many objects to put in one transaction.
///
/// Small enough that a chunked multi-megabyte blob does not blow past the
/// broker's frame limit when its records are batched together.
const BATCH: usize = 64;

impl<'a> ObjectWriter<'a> {
    pub fn new(writer: &'a FencedWriter, repo: RepoId) -> Self {
        Self {
            writer,
            topic: topics::repo_objects(repo),
        }
    }

    pub fn topic(&self) -> &str {
        &self.topic
    }

    /// Store one object, chunking it if needed.
    pub async fn put(&self, object: &Object) -> Result<(), StoreError> {
        frame::verify(object.oid, object.kind, &object.content)?;
        let encoded = frame::encode_object(object.oid, object.kind, &object.content);
        let records = encoded
            .records
            .into_iter()
            .map(|(key, value)| PendingRecord {
                topic: self.topic.clone(),
                key,
                value: Some(value),
                headers: Vec::new(),
            })
            .collect();
        self.writer.transact(records).await?;
        Ok(())
    }

    /// Store many objects.
    ///
    /// Returns how many were written. Verification happens per object, so one
    /// corrupt object fails the batch it is in rather than being stored.
    pub async fn put_all(&self, objects: &[Object]) -> Result<usize, StoreError> {
        let mut written = 0;
        for group in objects.chunks(BATCH) {
            let mut records = Vec::new();
            for object in group {
                frame::verify(object.oid, object.kind, &object.content)?;
                let encoded = frame::encode_object(object.oid, object.kind, &object.content);
                records.extend(
                    encoded
                        .records
                        .into_iter()
                        .map(|(key, value)| PendingRecord {
                            topic: self.topic.clone(),
                            key,
                            value: Some(value),
                            headers: Vec::new(),
                        }),
                );
            }
            self.writer.transact(records).await?;
            written += group.len();
        }
        Ok(written)
    }
}
