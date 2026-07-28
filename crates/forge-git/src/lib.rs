//! Git object storage on the crabka log, and the disposable cache over it.
//!
//! The log holds the objects; the cache is a real bare git repository rebuilt
//! from that log on demand. Nothing on disk is authoritative — see
//! [`cache::Cache::hydrate`].

pub mod cache;
pub mod frame;
pub mod import;
pub mod loose;
pub mod store;

pub use cache::{Cache, CacheError, Hydrated};
pub use frame::{Frame, FrameError, Kind, compute_oid, decode, encode_object, object_key, verify};
pub use store::{Object, ObjectWriter, StoreError, connect_object_writer};
