//! The coordinator core: state transitions without I/O.
//!
//! Time, storage and transport are the caller's job (invariant 23): every
//! method takes the current time, and state changes come out as events for
//! the log. That keeps the core deterministic, so it can be driven by Quint
//! traces (`spec/lease.qnt`) and by deterministic simulation, and a backend
//! can rebuild it by replaying [`Command`]s.

pub mod aliases;
pub mod command;
pub mod lease;
pub mod scope;
pub mod sequencer;
pub mod settlement;
pub mod work;

pub use command::{Command, Outcome};
pub use lease::{
    Agent, ConflictReason, Coordinator, Error, Event, EventKind, Lease, LeaseId, MAX_DENIED,
    MAX_DENIED_PER_OWNER, Mode, Ms, Request, Status,
};
pub use scope::Scope;
pub use sequencer::{Finality, LocalSequencer, Ownership, Sequencer};
pub use settlement::{
    Changes, Changeset, ChangesetId, Entry, EntryState, Expected, Leaf, Observation, ObserveReport,
    Origin, Settlement, Source,
};
pub use work::{DoneMark, WorkStatus};
