//! Native hook formats, independent of hook policy and coordinator access.

pub(crate) mod claude;
pub(crate) mod codex;
pub mod event;
pub(crate) mod opencode;

pub use crate::hooks::{HarnessEvent, HookEvent, StartProvenance};
use std::path::PathBuf;

/// The events a hook entry names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// Turn or session start, and each user prompt: the mail line.
    Start,
    /// Before the agent stops: block once on unread wake-worthy mail.
    Stop,
    /// An idle session's wait: wake it when wake-worthy mail arrives.
    Wait,
    /// Before a tool call: refuse an edit to a path the agent doesn't hold.
    Edit,
}

impl Event {
    pub fn observation(self) -> Option<HookEvent> {
        match self {
            Self::Start => Some(HookEvent::Start),
            Self::Stop => Some(HookEvent::Stop),
            Self::Wait | Self::Edit => None,
        }
    }
}

/// What a harness's hook input says, harness-neutrally.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Input {
    /// The folder the agent works in, when the harness says.
    pub cwd: Option<PathBuf>,
    /// The harness's session, for the stop guard's memory.
    pub session: Option<String>,
    /// The harness's turn, where it names one (Codex's `turn_id`).
    pub turn: Option<String>,
    /// The validated harness event, distinct from mdkg's start/stop event.
    pub harness_event: Option<HarnessEvent>,
    /// Only a SessionStart carries this whitelisted provenance.
    pub start_provenance: Option<StartProvenance>,
    /// The harness says this stop already follows a block (Claude's
    /// `stop_hook_active`).
    pub stop_active: bool,
    /// The tool about to run (`edit`).
    pub tool: Option<String>,
    /// A shell tool's command text, when the tool runs one.
    pub command: Option<String>,
    /// The paths that tool would write: empty for a tool that writes none.
    pub paths: Vec<String>,
}

/// What a hook decides, harness-neutrally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing to say: the harness goes on.
    Silent,
    /// Context for the agent's turn.
    Context(String),
    /// Keep the agent from stopping, with why.
    Block(String),
    /// Refuse the tool call, with why.
    Deny(String),
    /// Wake an idle session with this line.
    Wake(String),
}

/// A harness's output for an outcome.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Rendered {
    pub stdout: String,
    pub stderr: String,
    pub exit: u8,
}
