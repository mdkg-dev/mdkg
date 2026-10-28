//! The launcher: runs one headless turn of a Claude Code, Codex or OpenCode
//! agent whose prompt is its inbox digest, imports the agent's final answer
//! as a reply, and acks the mail it showed. Agents run in the user's own
//! environment, each isolated in its own worktree.
//!
//! Mail and leases go through the mdkg CLI's JSON contract ([`Mail`]),
//! the same boundary any other harness would use, so the launcher doesn't depend on
//! the messaging store's internals. Everything that decides what runs
//! ([`profile`], [`digest`], [`parse`]) is a pure function, tested without
//! starting an agent. [`install`] sets a project up for the harness (M5).

pub mod approval;
pub mod auth;
pub mod builds;
pub mod codex_hooks;
pub mod config;
pub mod digest;
pub mod error;
pub mod hooks;
pub mod identity;
pub mod install;
pub mod mail;
pub mod orchestration;
pub mod parse;
pub mod policy;
pub mod profile;
pub mod program;
pub mod projection;
pub mod receiver;
pub mod secret;
pub mod state;
pub mod supervise;
pub mod turn;

pub use error::{ErrorKind, RunError};
pub use mail::{CliMail, Inbox, Mail, Message};
pub use profile::{AgentCommand, Harness, Role};
pub use state::{AgentSpec, AgentState};
pub use turn::{Outcome, TurnOptions, TurnReport};
