//! The name the running binary was invoked by, for hints, help and
//! `--version`: `mdkg` as published, or the name a build is installed
//! under beside another (`mdkg-next`, next to 0.6.0's `mdkg`). The binary
//! sets it once from `argv[0]`; library code reads it.

use std::borrow::Cow;
use std::fmt;
use std::sync::OnceLock;

/// The published name, and the fallback when `argv[0]` can't be read.
pub const DEFAULT: &str = "mdkg";

static NAME: OnceLock<String> = OnceLock::new();

/// Sets the name from `argv[0]`'s file name (an `.exe` suffix dropped).
/// A name that's empty, not UTF-8, or holds anything but letters, digits,
/// `.`, `_` and `-` falls back to [`DEFAULT`], so a hint never carries
/// odd characters. Only the first call counts.
pub fn set_from_arg0(arg0: Option<&std::ffi::OsStr>) {
    let name = arg0
        .and_then(|a| std::path::Path::new(a).file_name())
        .and_then(|n| n.to_str())
        .map(|n| n.strip_suffix(".exe").unwrap_or(n))
        .filter(|n| {
            !n.is_empty()
                && n.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        })
        .unwrap_or(DEFAULT);
    NAME.set(name.to_string()).ok();
}

/// The running binary's name (default [`DEFAULT`]).
pub fn name() -> &'static str {
    NAME.get().map_or(DEFAULT, String::as_str)
}

/// `text` with every `` `mdkg `` (a command in a hint, in backticks)
/// naming the running binary instead, when it isn't called `mdkg`.
pub fn localize(text: &str) -> Cow<'_, str> {
    localize_as(text, name())
}

/// [`localize`] for the binary name `bin`.
pub fn localize_as<'a>(text: &'a str, bin: &str) -> Cow<'a, str> {
    if bin == DEFAULT || !text.contains("`mdkg ") {
        return Cow::Borrowed(text);
    }
    Cow::Owned(text.replace("`mdkg ", &format!("`{bin} ")))
}

/// The running binary's name, for `format!("`{BIN} status`")`.
#[derive(Debug, Clone, Copy)]
pub struct Bin;

/// See [`Bin`].
pub const BIN: Bin = Bin;

impl Bin {
    pub fn as_str(&self) -> &'static str {
        name()
    }
}

impl fmt::Display for Bin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(name())
    }
}

impl PartialEq<&str> for Bin {
    fn eq(&self, other: &&str) -> bool {
        name() == *other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn localize_names_only_backticked_commands() {
        // Unset in a test binary: the default, and nothing changes.
        assert_eq!(name(), DEFAULT);
        assert_eq!(localize("run `mdkg status`"), "run `mdkg status`");
        assert_eq!(
            localize_as("run `mdkg status`, or `mdkg doctor`", "mdkg-next"),
            "run `mdkg-next status`, or `mdkg-next doctor`"
        );
        // Paths and names that only contain mdkg stay as they are.
        let path = "see .mdkg/config.json and the mdkg-workflow skill";
        assert_eq!(localize_as(path, "mdkg-next"), path);
    }
}
