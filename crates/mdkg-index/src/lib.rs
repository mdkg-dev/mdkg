//! The derived plane: a rebuildable SQLite index of
//! one worktree's graph, kept beside the coordinator store in git's common
//! folder and never committed.
//!
//! The index is a cache, never an authority: the knowledge plane (the node
//! files) is read into an [`mdkg_graph::Index`] as every command reads it,
//! and [`Db::sync`] brings the SQLite copy up to date by each file's hash.
//! A damaged or outdated index is dropped and rebuilt.
//!
//! On top of it:
//! - [`resolve`]: handles (a full id, a unique id prefix or a title
//!   fragment) for every command;
//! - [`search`]: FTS5 full-text search ranked by BM25, with snippets;
//! - [`graph`]: refs, backrefs, the ready set and the critical path;
//! - [`pack`]: markdown context packs within a token budget;
//! - [`provider`]: the `DerivedProvider` interface for embeddings and code.

pub mod graph;
pub mod pack;
pub mod provider;
pub mod resolve;
pub mod search;
mod store;

pub use store::{Db, IndexError, IndexInfo, SCHEMA_VERSION, SyncStats, index_file_name};
