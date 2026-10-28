//! Launcher errors, each with the stable dotted code the CLI
//! reports. Codes from the mail CLI pass through unchanged.

use std::fmt;

/// Maps to the output contract's stable process exit codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// Bad arguments (exit 1).
    Usage,
    /// Not found (exit 3).
    NotFound,
    /// An internal or agent failure (exit 4).
    Failure,
    /// A coordination conflict, such as a turn already running (exit 5).
    Conflict,
}

impl ErrorKind {
    pub fn exit_code(self) -> u8 {
        match self {
            ErrorKind::Usage => 1,
            ErrorKind::NotFound => 3,
            ErrorKind::Failure => 4,
            ErrorKind::Conflict => 5,
        }
    }

    /// The kind for an exit code from the mail CLI.
    pub fn from_exit(code: i32) -> Self {
        match code {
            1 => ErrorKind::Usage,
            3 => ErrorKind::NotFound,
            5 => ErrorKind::Conflict,
            _ => ErrorKind::Failure,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunError {
    pub kind: ErrorKind,
    /// A dotted code such as `run.agent_failed` or `coord.lease_held`.
    pub code: String,
    pub message: String,
    pub hint: Option<String>,
    /// The file the error is about, relative to the project root.
    pub path: Option<String>,
}

impl RunError {
    pub fn new(kind: ErrorKind, code: &str, message: impl Into<String>) -> Self {
        Self {
            kind,
            code: code.to_string(),
            message: message.into(),
            hint: None,
            path: None,
        }
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    /// Adds `hint` after any hint the error already has.
    pub fn and_hint(mut self, hint: impl Into<String>) -> Self {
        let hint = hint.into();
        self.hint = Some(match self.hint.take() {
            Some(h) => format!("{h}; {hint}"),
            None => hint,
        });
        self
    }

    pub fn usage(code: &str, message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Usage, code, message)
    }

    pub fn failure(code: &str, message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Failure, code, message)
    }

    pub fn io(what: &str, e: std::io::Error) -> Self {
        Self::failure(CODE_IO, format!("{what}: {e}"))
    }
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for RunError {}

/// No agent by that name, and no `--harness` and `--role` to create it.
pub const CODE_UNKNOWN_AGENT: &str = "run.unknown_agent";
/// The agent process failed, timed out, or reported an error.
pub const CODE_AGENT_FAILED: &str = "run.agent_failed";
/// The agent ran without the sandbox its role requires.
pub const CODE_SANDBOX_MISMATCH: &str = "run.sandbox_mismatch";
/// The agent finished without a final message to import.
pub const CODE_NO_REPLY: &str = "run.no_reply";
/// The mail CLI couldn't be run, or its output wasn't the JSON contract.
pub const CODE_MAIL_FAILED: &str = "run.mail_failed";
/// The launcher's own files couldn't be read or written.
pub const CODE_IO: &str = "run.io_failed";
/// A harness and role combination v0.1 doesn't run.
pub const CODE_UNSUPPORTED: &str = "run.unsupported";
/// A worker's turn left its branch, or would commit to a protected one.
pub const CODE_BRANCH: &str = "run.branch_refused";
/// `--watch` stopped after failed turns in a row; the mail waits for a
/// human.
pub const CODE_TOO_MANY_FAILURES: &str = "run.too_many_failures";
/// An agent's folder isn't inside one of the repository's worktrees.
pub const CODE_CWD_OUTSIDE: &str = "run.cwd_outside";
