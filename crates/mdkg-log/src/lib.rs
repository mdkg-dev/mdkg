//! The coordination plane's event log.
//!
//! - `envelope`: content-addressed events (canonical JSON, sha256 ids).
//! - `hlc`: hybrid logical clocks for ordering across machines.
//! - `log`: an in-memory, hash-linked DAG per shard with an idempotency window.
//! - `merkle`: Merkle roots and inclusion proofs.
//! - `checkpoint`: a chain of checkpoint headers committing to log and
//!   knowledge roots.
//! - `export`: shard export and verification without a coordinator.
//! - `store`: canonical-JSON segment files (the crate's only I/O).
//! - `record`: work-state records, one hash-linked chain per coordinator
//!   store, with anchors and bundles for independent verification.

pub mod bundle;
pub mod checkpoint;
pub mod envelope;
pub mod export;
pub mod hlc;
pub mod log;
pub mod merkle;
pub mod record;
pub mod store;

pub use envelope::{Draft, Envelope, PayloadRef, Retention, TraceContext, Visibility};
pub use hlc::{Clock, Hlc};
pub use log::{AppendError, Appended, Log};
