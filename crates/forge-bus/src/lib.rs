//! The forge's interface to the crabka log.
//!
//! Two halves, matching the two directions:
//!
//! * [`FencedWriter`] — the single writer. Transactional, epoch-fenced.
//! * [`Tailer`] — group-less reads with a caller-owned cursor, for state
//!   hydration and projection.

mod tailer;
mod writer;

pub use tailer::{Batch, FetchedRecord, TailError, Tailer};
pub use writer::{
    COMMAND_TRANSACTIONAL_ID, Committed, FencedWriter, IndeterminateHandler,
    OBJECT_TRANSACTIONAL_ID, PendingRecord, WEBHOOK_TRANSACTIONAL_ID, WriteError,
};
