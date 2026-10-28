use std::fmt;
use std::path::Path;

use mdkg_format::{Code, Diagnostic, FormatError};

use crate::index::relative_label;

/// How an error maps to a stable process exit code in the output contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// Bad arguments (exit 1).
    Usage,
    /// Validation failed (exit 2).
    Validation,
    /// A node, workspace or file was not found (exit 3).
    NotFound,
    /// The graph can't be read, or an internal failure (exit 4).
    Failure,
    /// A coordination conflict: sync or pick other work, then retry (exit 5).
    Conflict,
    /// Waited and nothing happened (exit 6): `msg wait` ran out of time.
    Timeout,
    /// A handle names several nodes (exit 7); the diagnostic lists them.
    Ambiguous,
}

impl ErrorKind {
    /// Every kind, in exit-code order: the published exit-code contract
    /// (docs/commands.md's "Exit codes", generated from this).
    pub const ALL: [ErrorKind; 7] = [
        ErrorKind::Usage,
        ErrorKind::Validation,
        ErrorKind::NotFound,
        ErrorKind::Failure,
        ErrorKind::Conflict,
        ErrorKind::Timeout,
        ErrorKind::Ambiguous,
    ];

    /// What the exit code tells a caller, for the published contract.
    pub fn meaning(self) -> &'static str {
        match self {
            ErrorKind::Usage => "Bad arguments: an unknown command or flag, a missing or bad value",
            ErrorKind::Validation => {
                "A check found errors: `validate`, `verify`, `doctor`, `skills validate` or \
                 `export`'s anchor check, or a write refused because the graph it would leave \
                 doesn't validate"
            }
            ErrorKind::NotFound => {
                "Something named doesn't exist (a node, workspace, file, lease, changeset, room, \
                 message or agent), or there's none to act on, such as no active goal"
            }
            ErrorKind::Failure => {
                "The graph, store or Keychain can't be read or written, or another failure"
            }
            ErrorKind::Conflict => {
                "A coordination conflict, such as a lease someone else holds: sync or pick \
                 other work, then retry"
            }
            ErrorKind::Timeout => "Waited and nothing happened: `msg wait` ran out of time",
            ErrorKind::Ambiguous => "A handle names several nodes; the diagnostic lists them",
        }
    }

    /// The kind's place in [`ErrorKind::ALL`]. A new kind needs an arm here
    /// and in the test that lists every kind, which checks ALL holds it.
    pub const fn index(self) -> usize {
        match self {
            ErrorKind::Usage => 0,
            ErrorKind::Validation => 1,
            ErrorKind::NotFound => 2,
            ErrorKind::Failure => 3,
            ErrorKind::Conflict => 4,
            ErrorKind::Timeout => 5,
            ErrorKind::Ambiguous => 6,
        }
    }

    pub fn exit_code(self) -> u8 {
        match self {
            ErrorKind::Usage => 1,
            ErrorKind::Validation => 2,
            ErrorKind::NotFound => 3,
            ErrorKind::Failure => 4,
            ErrorKind::Conflict => 5,
            ErrorKind::Timeout => 6,
            ErrorKind::Ambiguous => 7,
        }
    }
}

/// An error that stops a command, carrying its coded diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphError {
    pub kind: ErrorKind,
    /// Boxed: errors travel in `Result`s, and a diagnostic is large.
    pub diagnostic: Box<Diagnostic>,
}

impl GraphError {
    pub fn new(kind: ErrorKind, diagnostic: Diagnostic) -> Self {
        Self {
            kind,
            diagnostic: Box::new(diagnostic),
        }
    }

    pub fn failure(code: Code, message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Failure, Diagnostic::error(code, message))
    }

    pub fn not_found(code: Code, message: impl Into<String>) -> Self {
        Self::new(ErrorKind::NotFound, Diagnostic::error(code, message))
    }

    pub fn usage(code: Code, message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Usage, Diagnostic::error(code, message))
    }

    /// `node.ambiguous`: a handle names several nodes.
    pub fn ambiguous(message: impl Into<String>) -> Self {
        Self::new(
            ErrorKind::Ambiguous,
            Diagnostic::error(Code::NodeAmbiguous, message),
        )
    }

    pub fn conflict(code: Code, message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Conflict, Diagnostic::error(code, message))
    }

    /// A format error found under `root`, with its path made relative.
    pub fn from_format(e: &FormatError, root: &Path) -> Self {
        Self::new(
            if e.is_usage() {
                ErrorKind::Usage
            } else {
                ErrorKind::Failure
            },
            e.to_diagnostic(|p| relative_label(root, p)),
        )
    }

    pub fn at(mut self, path: impl Into<String>, line: Option<usize>) -> Self {
        self.diagnostic.path = Some(path.into());
        self.diagnostic.line = line;
        self
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.diagnostic.hint = Some(hint.into());
        self
    }

    pub fn code(&self) -> Code {
        self.diagnostic.code
    }

    pub fn exit_code(&self) -> u8 {
        self.kind.exit_code()
    }
}

impl fmt::Display for GraphError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.diagnostic.fmt(f)
    }
}

impl std::error::Error for GraphError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_is_in_the_published_contract_once() {
        // Every kind, by an exhaustive match: a new one doesn't compile
        // until it's added to this list, which must then match ALL.
        let listed = |k: ErrorKind| match k {
            ErrorKind::Usage
            | ErrorKind::Validation
            | ErrorKind::NotFound
            | ErrorKind::Failure
            | ErrorKind::Conflict
            | ErrorKind::Timeout
            | ErrorKind::Ambiguous => k,
        };
        let every = [
            ErrorKind::Usage,
            ErrorKind::Validation,
            ErrorKind::NotFound,
            ErrorKind::Failure,
            ErrorKind::Conflict,
            ErrorKind::Timeout,
            ErrorKind::Ambiguous,
        ]
        .map(listed);
        assert_eq!(every, ErrorKind::ALL);
        for (i, kind) in ErrorKind::ALL.iter().enumerate() {
            assert_eq!(kind.index(), i, "{kind:?}");
            assert_eq!(usize::from(kind.exit_code()), i + 1, "{kind:?}");
        }
    }
}
