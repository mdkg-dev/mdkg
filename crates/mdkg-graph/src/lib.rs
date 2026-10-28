//! Loading, indexing, querying and validating mdkg graphs.
//!
//! Phase 1 reproduces the read behaviour of mdkg 0.6.0 for the core node
//! types. Everything here is synchronous; file access goes through
//! `discovery`, `templates` and `fs_walk`.

pub mod bundled;
pub mod config;
pub mod discovery;
pub mod error;
mod fs_walk;
pub mod graph_checks;
pub mod identity_v2;
pub mod index;
pub mod query;
pub mod resolve;
pub mod store_work;
pub mod templates;
pub mod validate;

pub use config::Config;
pub use error::{ErrorKind, GraphError};
pub use index::{Index, IndexNode};
