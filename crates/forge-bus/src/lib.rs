//! The forge's interface to the crabka log.
//!
//! Two halves, matching the two directions:
//!
//! * [`FencedWriter`] — the single writer. Transactional, epoch-fenced.
//! * [`Tailer`] — group-less reads with a caller-owned cursor, for state
//!   hydration and projection.
//!
//! Plus [`BrokerFeatures`], which asks the broker what it was formatted to do —
//! the one question whose answer cannot be changed after the fact.

mod features;
mod tailer;
mod writer;

pub use features::{BrokerFeatures, FeatureError, SHARE_GROUPS_LEVEL, SHARE_VERSION};
pub use tailer::{Batch, FetchedRecord, TailError, Tailer, join_trace};
pub use writer::{
    COMMAND_TRANSACTIONAL_ID, Committed, FencedWriter, IndeterminateHandler,
    OBJECT_TRANSACTIONAL_ID, PendingRecord, WEBHOOK_TRANSACTIONAL_ID, WriteError,
    runner_transactional_id,
};
