//! The local coordinator backend, backed by SQLite and shared across worktrees.
//!
//! One SQLite database in the git common directory, so every worktree of a
//! repository shares the same leases, versions and settlement queue. It is
//! never committed: the common directory isn't part of any worktree.
//!
//! - [`Store`] journals every [`mdkg_coord::Command`] with its outcome and
//!   keeps the coordinator's state next to it, both in one transaction. The
//!   journal is the record; the state is a cache that [`Store::rebuild`] can
//!   recreate by replay.
//! - [`git`] reads main and worktrees: the leaves on a branch with their
//!   content hashes, the commit a worktree is on, and checking a file out of
//!   another branch's commit (`sync`).
//! - [`mail`] keeps rooms and messages beside the leases (harness v0.1 M1),
//!   without touching the lease journal.
//! - [`ledger`] keeps the flow tables beside the mail: which nodes a message
//!   is about, what it supersedes and when it expires; [`entries`] is the
//!   ledger itself, typed entries on each node and the gate they feed.
//! - [`Store::observe_main`] reads main and applies it inside the write
//!   transaction, so no other change can land between the read and the apply.

mod activity;
mod digest_cache;
pub mod entries;
pub mod events;
pub mod git;
pub mod ledger;
pub mod log;
pub mod mail;
pub mod serve;
mod store;

pub use activity::{
    ActivityCursor, ActivitySnapshot, Completion, LeaseClaim, completions_for_receipt,
};

pub use store::{
    BackupInfo, BeforeCommit, ChainOutcomes, Chained, LogReport, PLAIN_FSYNC_ENV, Restored, Store,
    StoreError, Upgrade, allow_store_upgrades, backups, build_label, copy_of, has_cargo_markers,
    in_build_folder, restore, restore_if_head, snapshot, store_upgrades_allowed, stored_label,
};
