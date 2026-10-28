use std::fmt;

use crate::diag::{Code, Diagnostic};

/// A format error: a stable code, where it happened, and a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatError {
    code: Code,
    path: Option<String>,
    line: Option<usize>,
    text: String,
    /// The reference raises these as usage errors, which stop indexing even
    /// in tolerant mode.
    usage: bool,
}

impl FormatError {
    /// An error tied to a frontmatter line.
    pub fn at_line(path: &str, line: usize, code: Code, message: impl fmt::Display) -> Self {
        Self {
            code,
            path: Some(path.to_string()),
            line: Some(line),
            text: message.to_string(),
            usage: false,
        }
    }

    /// An error tied to a file.
    pub fn in_file(path: &str, code: Code, message: impl fmt::Display) -> Self {
        Self {
            code,
            path: Some(path.to_string()),
            line: None,
            text: message.to_string(),
            usage: false,
        }
    }

    /// An error with no location yet; see [`FormatError::in_path`].
    pub fn bare(code: Code, message: impl fmt::Display) -> Self {
        Self {
            code,
            path: None,
            line: None,
            text: message.to_string(),
            usage: false,
        }
    }

    /// The same error, located in `path` if it had no location.
    pub fn in_path(mut self, path: &str) -> Self {
        if self.path.is_none() {
            self.path = Some(path.to_string());
        }
        self
    }

    /// The same error, marked as a usage error.
    pub fn into_usage(mut self) -> Self {
        self.usage = true;
        self
    }

    pub fn is_usage(&self) -> bool {
        self.usage
    }

    pub fn code(&self) -> Code {
        self.code
    }

    pub fn path(&self) -> Option<&str> {
        self.path.as_deref()
    }

    pub fn line(&self) -> Option<usize> {
        self.line
    }

    /// The message without its location.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The error as a diagnostic, with `path` mapped by `relative` (for
    /// example from an absolute label to a repository-relative path).
    pub fn to_diagnostic(&self, relative: impl Fn(&str) -> String) -> Diagnostic {
        let d = Diagnostic::error(self.code, self.text.clone());
        match &self.path {
            Some(p) => d.at(relative(p), self.line),
            None => d,
        }
    }
}

impl fmt::Display for FormatError {
    /// `path:line: message`, as the reference prints it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.path, self.line) {
            (Some(p), Some(l)) => write!(f, "{p}:{l}: {}", self.text),
            (Some(p), None) => write!(f, "{p}: {}", self.text),
            _ => f.write_str(&self.text),
        }
    }
}

impl std::error::Error for FormatError {}
