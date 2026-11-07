//! The environment a harness turn starts with (0.7.2 security).
//!
//! A turn used to inherit the launcher's whole environment, less the
//! provider keys ([`crate::auth`]). A supervisor started from a shell then
//! handed every credential that shell exported to every agent, and a
//! reviewer printed three of them (2026-11-01). Now a turn starts with an
//! allowlist ([`Mode::Allowlist`], the default):
//!
//! - [`BASE`]: what any harness needs to run (`PATH`, `HOME`, `USER`,
//!   `LOGNAME`, `SHELL`, `TMPDIR`, `TERM`, `LANG`, `LC_*`, `TZ`, the XDG
//!   base folders) and what a worker's own git commits need (its identity
//!   and which config files git reads);
//! - mdkg's own `MDKG_*`;
//! - the harness's own documented set ([`harness_names`]):
//!   `CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `OPENCODE_*`;
//! - the names `env.allow` adds, in the agent's policy or `agents.defaults`.
//!
//! A pattern ending in `*` never admits a credential-shaped name
//! ([`credential_shaped`]); an exact name in `env.allow` does, since naming
//! it is the user's choice. Nothing admits a provider key or a host
//! session's variable ([`crate::auth::withheld_vars`]), which config refuses
//! to name. `env.inherit: true` puts the old behaviour back, with a warning
//! on every turn.
//!
//! Config holds names only, never values: a value comes from the
//! launcher's environment when the turn starts.

use std::ffi::OsStr;
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::profile::Harness;

/// What every harness turn may inherit, whatever its harness. A final `*`
/// is a prefix.
pub const BASE: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "TMPDIR",
    "TERM",
    "LANG",
    "LC_*",
    "TZ",
    // Where harnesses and git keep config, data and state on Linux; a
    // test's temp HOME sets them too.
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "XDG_CACHE_HOME",
    "XDG_RUNTIME_DIR",
    // A worker's own commits: who it commits as, and which config files
    // git reads. Never GIT_DIR and the like (crate::turn::GIT_ENV), nor
    // GIT_CONFIG_COUNT, whose values can carry an auth header.
    "GIT_AUTHOR_NAME",
    "GIT_AUTHOR_EMAIL",
    "GIT_COMMITTER_NAME",
    "GIT_COMMITTER_EMAIL",
    "GIT_CONFIG_GLOBAL",
    "GIT_CONFIG_SYSTEM",
    "GIT_CONFIG_NOSYSTEM",
    "GIT_ATTR_NOSYSTEM",
    "MDKG_*",
];

/// A harness's own documented variables, where it keeps its config and
/// login, never its token variables: Claude Code's `CLAUDE_CONFIG_DIR`,
/// Codex's `CODEX_HOME`, OpenCode's `OPENCODE_*` (its credential-shaped
/// ones dropped, and `OPENCODE_API_KEY` withheld as a provider key).
pub fn harness_names(h: Harness) -> &'static [&'static str] {
    match h {
        Harness::Claude => &["CLAUDE_CONFIG_DIR"],
        Harness::Codex => &["CODEX_HOME"],
        Harness::OpenCode => &["OPENCODE_*"],
    }
}

/// Which environment an agent's turns start with.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Only the allowlist ([`BASE`], `MDKG_*`, the harness's own names and
    /// `env.allow`).
    #[default]
    Allowlist,
    /// The launcher's whole environment less provider keys: `env.inherit:
    /// true`, warned on every turn.
    Inherit,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allowlist => "allowlist",
            Self::Inherit => "inherit",
        }
    }
}

/// The warning every inherited-environment turn prints.
pub const INHERIT_WARNING: &str = "env.inherit is true: this turn inherits the launcher's whole \
environment (less provider keys), so every credential your shell exports reaches the agent; \
remove env.inherit and name what the agent needs in env.allow";

/// Whether `name` looks like a credential by its name: it holds `TOKEN`, `SECRET`, `PASSWORD`,
/// `PASSWD`, `CREDENTIAL`, `AUTH` or `API_KEY`, or ends in `_KEY` or holds
/// `_KEY_`. Case-insensitive.
pub fn credential_shaped(name: &[u8]) -> bool {
    let upper = name.to_ascii_uppercase();
    let has = |part: &[u8]| upper.windows(part.len()).any(|w| w == part);
    [
        &b"TOKEN"[..],
        b"SECRET",
        b"PASSWORD",
        b"PASSWD",
        b"CREDENTIAL",
        b"AUTH",
        b"API_KEY",
        b"_KEY_",
    ]
    .iter()
    .any(|p| has(p))
        || upper.ends_with(b"_KEY")
}

/// Whether `pattern` (an exact name, or a prefix ending in `*`) admits
/// `name`. A prefix never admits a credential-shaped name.
fn admits(pattern: &str, name: &[u8]) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix.as_bytes()) && !credential_shaped(name),
        None => name == pattern.as_bytes(),
    }
}

/// Whether a configured `env.allow` entry is well formed: a variable name
/// (`[A-Za-z_][A-Za-z0-9_]*`), or such a name followed by one `*`. Never a
/// `NAME=value`: config holds names only.
pub fn valid_entry(entry: &str) -> bool {
    let name = entry.strip_suffix('*').unwrap_or(entry);
    let mut bytes = name.bytes();
    matches!(bytes.next(), Some(b) if b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Why an `env.allow` entry can't be admitted, if it can't: a provider key
/// or host-session variable mdkg never passes, or a variable only the
/// launcher sets. Checked by name, so a value is never read.
pub fn refused_entry(entry: &str) -> Option<&'static str> {
    if entry.ends_with('*') {
        return None;
    }
    let name = OsStr::new(entry);
    if crate::auth::withheld(name) || crate::auth::host_session(name) {
        return Some("is a provider key or host-session variable, which no agent inherits");
    }
    if crate::profile::NOT_INHERITED.contains(&entry) || entry == crate::supervise::reload::EXEC_ENV
    {
        return Some("is set only by the launcher");
    }
    None
}

/// The environment one turn's processes start with: its harness, mode and
/// extra names. Built from the agent's policy ([`crate::policy::Env`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Environment {
    pub harness: Harness,
    pub mode: Mode,
    /// `env.allow`, validated.
    pub allow: Vec<String>,
}

impl Environment {
    pub fn new(harness: Harness, policy: &crate::policy::Env) -> Self {
        Self {
            harness,
            mode: policy.mode(),
            allow: policy.allow.clone(),
        }
    }

    /// The allowlist, by default: no `env.allow`.
    pub fn allowlist(harness: Harness) -> Self {
        Self::new(harness, &crate::policy::Env::default())
    }

    /// Whether this environment passes the launcher's variable `name`,
    /// before mdkg's own removals ([`Self::apply`]).
    pub fn passes(&self, name: &OsStr) -> bool {
        if self.mode == Mode::Inherit {
            return true;
        }
        let bytes = name.as_encoded_bytes();
        BASE.iter()
            .chain(self.harness.adapter().inherited_env())
            .any(|p| admits(p, bytes))
            || self.allow.iter().any(|p| admits(p, bytes))
    }

    /// Gives `c` this environment from the launcher's own: the allowlist
    /// clears everything and adds back what [`Self::passes`]; either mode
    /// then takes out what no harness inherits (the launcher's pins, a
    /// supervisor's exec token, provider keys and host-session variables).
    /// Callers add the turn's own variables afterwards.
    pub fn apply(&self, c: &mut Command) {
        self.apply_from(c, std::env::vars_os());
    }

    /// [`Self::apply`] from a given environment (tests).
    pub(crate) fn apply_from(
        &self,
        c: &mut Command,
        from: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
    ) {
        if self.mode == Mode::Allowlist {
            c.env_clear();
        }
        for (k, v) in from {
            if removed(&k) {
                c.env_remove(k);
            } else if self.mode == Mode::Allowlist && self.passes(&k) {
                c.env(k, v);
            }
        }
        // Whether set or not, as before 0.7.2.
        for k in crate::profile::NOT_INHERITED {
            c.env_remove(k);
        }
        c.env_remove(crate::supervise::reload::EXEC_ENV);
        for k in crate::auth::KEY_VARS {
            c.env_remove(k);
        }
    }
}

/// Whether mdkg takes `name` out of every harness's environment, whatever
/// the mode ([`Environment::apply`]).
fn removed(name: &OsStr) -> bool {
    let bytes = name.as_encoded_bytes();
    crate::profile::NOT_INHERITED
        .iter()
        .any(|k| bytes == k.as_bytes())
        || bytes == crate::supervise::reload::EXEC_ENV.as_bytes()
        || crate::auth::withheld(name)
        || crate::auth::host_session(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    /// A synthetic launcher environment: nothing here is real.
    fn launcher() -> Vec<(OsString, OsString)> {
        let fake = concat!("not-", "a-secret");
        [
            ("PATH", "/usr/bin:/bin"),
            ("HOME", "/tmp/fake-home"),
            ("USER", "fake"),
            ("LC_CTYPE", "C"),
            ("MDKG_ACTOR", "worker"),
            ("MDKG_TRUSTED_WORKTREE", "/tmp/pin"),
            ("MDKG_SUPERVISOR_EXEC", "exec-token"),
            ("CLAUDE_CONFIG_DIR", "/tmp/fake-claude"),
            ("CODEX_HOME", "/tmp/fake-codex"),
            ("OPENCODE_CONFIG", "/tmp/fake-opencode.json"),
            ("OPENCODE_SERVER_PASSWORD", fake),
            ("OPENCODE_API_KEY", fake),
            ("FAKE_TOKEN", fake),
            ("CLAUDE_CODE_OAUTH_TOKEN", fake),
            ("GITHUB_TOKEN", fake),
            ("ANTHROPIC_API_KEY", fake),
            ("MY_TOOL_HOME", "/tmp/tool"),
            ("MY_TOOL_SECRET", fake),
            ("GIT_AUTHOR_NAME", "Fake"),
            ("GIT_DIR", "/tmp/elsewhere"),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect()
    }

    /// The names `env` gives a command, from the synthetic launcher.
    fn names(env: &Environment) -> Vec<String> {
        let mut c = Command::new("/usr/bin/true");
        env.apply_from(&mut c, launcher());
        let mut out: Vec<String> = c
            .get_envs()
            .filter(|(_, v)| v.is_some())
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect();
        out.sort();
        out
    }

    #[test]
    fn the_allowlist_keeps_what_a_harness_needs_and_drops_every_secret() {
        for (h, own, others) in [
            (
                Harness::Claude,
                "CLAUDE_CONFIG_DIR",
                ["CODEX_HOME", "OPENCODE_CONFIG"],
            ),
            (
                Harness::Codex,
                "CODEX_HOME",
                ["CLAUDE_CONFIG_DIR", "OPENCODE_CONFIG"],
            ),
            (
                Harness::OpenCode,
                "OPENCODE_CONFIG",
                ["CLAUDE_CONFIG_DIR", "CODEX_HOME"],
            ),
        ] {
            let got = names(&Environment::allowlist(h));
            for keep in [
                "PATH",
                "HOME",
                "USER",
                "LC_CTYPE",
                "MDKG_ACTOR",
                "GIT_AUTHOR_NAME",
                own,
            ] {
                assert!(got.iter().any(|n| n == keep), "{h:?} lost {keep}: {got:?}");
            }
            for drop in [
                "FAKE_TOKEN",
                "CLAUDE_CODE_OAUTH_TOKEN",
                "GITHUB_TOKEN",
                "ANTHROPIC_API_KEY",
                "OPENCODE_SERVER_PASSWORD",
                "OPENCODE_API_KEY",
                "MY_TOOL_HOME",
                "MY_TOOL_SECRET",
                "GIT_DIR",
                "MDKG_TRUSTED_WORKTREE",
                "MDKG_SUPERVISOR_EXEC",
            ]
            .into_iter()
            .chain(others)
            {
                assert!(!got.iter().any(|n| n == drop), "{h:?} kept {drop}: {got:?}");
            }
        }
    }

    #[test]
    fn env_allow_adds_names_and_prefixes_but_a_prefix_never_a_credential() {
        let policy = crate::policy::Env {
            allow: vec!["GITHUB_TOKEN".into(), "MY_TOOL_*".into()],
            inherit: None,
        };
        let got = names(&Environment::new(Harness::Codex, &policy));
        assert!(got.iter().any(|n| n == "GITHUB_TOKEN"), "{got:?}");
        assert!(got.iter().any(|n| n == "MY_TOOL_HOME"), "{got:?}");
        assert!(!got.iter().any(|n| n == "MY_TOOL_SECRET"), "{got:?}");
        assert!(!got.iter().any(|n| n == "FAKE_TOKEN"), "{got:?}");
    }

    #[test]
    fn inherit_passes_the_rest_but_still_withholds_keys_and_pins() {
        let policy = crate::policy::Env {
            allow: Vec::new(),
            inherit: Some(true),
        };
        let env = Environment::new(Harness::Claude, &policy);
        assert_eq!(env.mode, Mode::Inherit);
        let mut c = Command::new("/usr/bin/true");
        env.apply_from(&mut c, launcher());
        // Inherit leaves the launcher's environment in place and only
        // removes, so the removals are what the command records.
        let removed: Vec<String> = c
            .get_envs()
            .filter(|(_, v)| v.is_none())
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect();
        for k in [
            "ANTHROPIC_API_KEY",
            "MDKG_TRUSTED_WORKTREE",
            "MDKG_SUPERVISOR_EXEC",
        ] {
            assert!(removed.iter().any(|n| n == k), "{k}: {removed:?}");
        }
        assert!(!removed.iter().any(|n| n == "FAKE_TOKEN"));
    }

    #[test]
    fn credential_shapes_and_entries() {
        for n in [
            "FAKE_TOKEN",
            "X_SECRET_Y",
            "DB_PASSWORD",
            "SSH_KEY",
            "A_KEY_B",
            "MY_API_KEY",
            "GH_AUTH",
            "OPENCODE_SERVER_PASSWORD",
            "aws_credential_file",
        ] {
            assert!(credential_shaped(n.as_bytes()), "{n}");
        }
        for n in [
            "PATH",
            "MDKG_ACTOR",
            "OPENCODE_CONFIG",
            "KEYBOARD",
            "MONKEY",
        ] {
            assert!(!credential_shaped(n.as_bytes()), "{n}");
        }
        for e in ["GITHUB_TOKEN", "_X", "MY_TOOL_*", "A1"] {
            assert!(valid_entry(e), "{e}");
        }
        for e in ["", "*", "1A", "A=B", "A B", "A**", "A*B", "-X"] {
            assert!(!valid_entry(e), "{e}");
        }
        assert!(refused_entry("ANTHROPIC_API_KEY").is_some());
        assert!(refused_entry("CLAUDE_CODE_SESSION_ID").is_some());
        assert!(refused_entry("MDKG_TRUSTED_WORKTREE").is_some());
        assert!(refused_entry("MDKG_SUPERVISOR_EXEC").is_some());
        assert!(refused_entry("GITHUB_TOKEN").is_none());
    }
}
