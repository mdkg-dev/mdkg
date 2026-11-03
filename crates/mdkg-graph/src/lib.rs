//! Loading, indexing, querying and validating mdkg graphs.
//!
//! Phase 1 reproduces the read behaviour of mdkg 0.6.0 for the core node
//! types. Everything here is synchronous; file access goes through
//! `discovery`, `templates` and `fs_walk`.

pub mod bundled;
pub mod cache;
pub mod committed;
pub mod config;
pub mod discovery;
pub mod error;
mod fs_walk;
pub mod graph_checks;
pub mod graphref;
pub mod identity_v2;
pub mod index;
pub mod intent;
pub mod query;
pub mod refwrite;
pub mod requirements;
pub mod resolve;
pub mod store_work;
pub mod templates;
pub mod validate;

pub use config::Config;
pub use error::{ErrorKind, GraphError};
pub use index::{Index, IndexNode};
