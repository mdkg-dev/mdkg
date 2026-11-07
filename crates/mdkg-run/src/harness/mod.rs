//! One registry and behavior boundary for every supported agent harness.
//!
//! The identity enum is a compatibility detail for 0.7.1's existing Rust
//! consumers and schemas. Implementations, flag choices and projections all
//! come from the registry. Turn policy and coordinator state do not live here.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use crate::auth::{AuthStatus, KeySource, Login};
use crate::error::RunError;
use crate::install::{Planned, Server};
use crate::parse::Parsed;
use crate::profile::{AgentCommand, CodexSettings, Role, TurnInput};
use crate::turn::TurnOptions;

mod builtin;
pub mod hook;

/// The single list of built-in agent harnesses. Compatibility wrappers use
/// the same list so a fourth entry cannot silently miss an enum or CLI flag.
#[macro_export]
macro_rules! harness_registry {
    ($with:ident) => {
        $with! {
            (Claude, "claude", Claude),
            (Codex, "codex", Codex),
            (OpenCode, "opencode", OpenCode),
        }
    };
}

macro_rules! registry {
    ($(($variant:ident, $id:literal, $adapter:ident)),* $(,)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash,
                 serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
        pub enum Harness {
            $(#[serde(rename = $id)] $variant),*
        }

        impl Harness {
            pub const ALL: [Self; [$(stringify!($variant)),*].len()] = [$(Self::$variant),*];
            pub fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $id),* }
            }
            pub fn adapter(self) -> &'static dyn HarnessAdapter {
                match self { $(Self::$variant => &builtin::$adapter),* }
            }
        }
    };
}
harness_registry!(registry);

impl Harness {
    pub fn parse(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|h| h.as_str() == id)
    }

    pub fn name(self) -> &'static str {
        self.as_str()
    }

    /// The program override, retaining its invocation spelling.
    pub fn program(self) -> PathBuf {
        std::env::var_os(self.adapter().program_override())
            .filter(|v| !v.is_empty())
            .map_or_else(|| PathBuf::from(self.as_str()), PathBuf::from)
    }
}

/// The registry, in the historical CLI and projection order.
pub fn adapters() -> impl Iterator<Item = &'static dyn HarnessAdapter> {
    Harness::ALL.into_iter().map(Harness::adapter)
}

/// Native file syntax; independent of which harness uses it.
#[derive(Debug, Clone, Copy)]
pub enum McpFormat {
    Json { command_pointer: &'static str },
    Toml,
}

#[derive(Debug, Clone, Copy)]
pub struct McpProjection {
    pub name: &'static str,
    pub path: &'static str,
    pub section: &'static str,
    pub format: McpFormat,
    /// A schema marker mdkg adds to a newly created JSON config.
    pub schema: Option<&'static str>,
}

impl McpProjection {
    pub fn present(self, root: &Path) -> bool {
        let Ok(text) = std::fs::read_to_string(root.join(self.path)) else {
            return false;
        };
        match self.format {
            McpFormat::Json { .. } => serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .is_some_and(|v| v.get(self.section).and_then(|s| s.get("mdkg")).is_some()),
            McpFormat::Toml => text
                .parse::<toml_edit::DocumentMut>()
                .ok()
                .is_some_and(|d| d.get(self.section).and_then(|s| s.get("mdkg")).is_some()),
        }
    }

    pub fn command(self, root: &Path) -> Option<String> {
        let text = std::fs::read_to_string(root.join(self.path)).ok()?;
        match self.format {
            McpFormat::Json { command_pointer } => serde_json::from_str::<serde_json::Value>(&text)
                .ok()?
                .pointer(command_pointer)?
                .as_str()
                .map(String::from),
            McpFormat::Toml => text
                .parse::<toml_edit::DocumentMut>()
                .ok()?
                .get(self.section)?
                .get("mdkg")?
                .get("command")?
                .as_str()
                .map(String::from),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct HookLayout {
    pub name: &'static str,
    pub canonical: &'static str,
    pub target: &'static str,
    pub merged: bool,
    /// The target is the harness's own per-user file, never the team's
    /// shared one, so install adds no ignore rule for it (dec-01a18ee0
    /// item 11: Claude's `.claude/settings.local.json`).
    pub personal: bool,
}

/// Harness-specific mechanics only. Every registered implementation supplies
/// every operation; unsupported behavior is explicit, never an empty fallback.
pub trait HarnessAdapter: Send + Sync {
    fn id(&self) -> &'static str;
    fn display_name(&self) -> &'static str;
    fn program_override(&self) -> &'static str;
    fn version_probe(&self) -> &'static [&'static str];
    fn capabilities(&self) -> &'static [&'static str];
    fn preflight(&self, command: &AgentCommand) -> Result<(), RunError>;
    fn launch(&self, input: &TurnInput<'_>, shell_binary: Option<&Path>) -> AgentCommand;
    fn environment(&self, input: &TurnInput<'_>) -> Vec<(String, String)>;
    fn withheld(&self, name: &OsStr) -> bool;
    fn parse_output(
        &self,
        role: Role,
        stdout: &str,
        stderr: &str,
        output_file: Option<&str>,
        settings: CodexSettings<'_>,
    ) -> Result<Parsed, RunError>;
    /// Whether the harness is logged in, asked in `env`, the environment
    /// its turns start with.
    fn login(&self, program: &Path, cwd: &Path, env: &crate::env::Environment) -> Login;
    fn login_command(&self) -> &'static str;
    fn keychain_service(&self) -> Option<&'static str>;
    /// Whether `line`, a line of the harness's final output, is its own
    /// usage-limit error as its CLI prints it.
    fn usage_limit(&self, line: &str) -> bool;
    /// Whether a structured harness's stdout events (its JSON lines, in
    /// order) show the turn ended on its usage limit, wherever in the
    /// stream the harness says so.
    fn usage_limit_in(&self, events: &[String]) -> bool;
    /// Whether a structured harness's stdout events show the turn ended on
    /// the provider throttling requests, not on its usage limit: transient,
    /// so its shows are taken back and no key is offered.
    fn throttled_in(&self, events: &[String]) -> bool;
    /// When the limit `line` reports resets (epoch seconds) and which
    /// limit it is, when the harness says.
    fn usage_limit_reset(&self, line: &str) -> Option<(u64, Option<String>)>;
    fn auth_status(
        &self,
        program: &Path,
        cwd: &Path,
        policy: &crate::policy::Auth,
        keys: &KeySource,
        env: &crate::env::Environment,
    ) -> AuthStatus;
    /// The variables this harness documents for its config and login
    /// folders, which its turns inherit besides crate::env's base set
    /// (never its token variables; a final `*` is a prefix).
    fn inherited_env(&self) -> &'static [&'static str];
    fn mcp_projection(&self) -> McpProjection;
    fn mcp_project(&self, root: &Path, server: &Server<'_>) -> Result<Planned, RunError>;
    fn hook_layout(&self) -> HookLayout;
    fn hook_project(&self, binary: &str) -> String;
    fn hook_native_event(&self, native: &str) -> Option<(hook::Event, hook::event::EventName)>;
    fn hook_parse(
        &self,
        event: hook::Event,
        input: &serde_json::Value,
    ) -> Result<hook::Input, String>;
    fn hook_render(&self, event: hook::Event, outcome: &hook::Outcome) -> hook::Rendered;
    fn hook_bound_actor(&self, root: &Path, input: &hook::Input) -> Result<Option<String>, String>;
    fn hook_supports(&self, event: hook::event::EventName) -> bool;
    fn efforts(&self) -> Option<&'static [&'static str]>;
    fn validate_effort(&self, effort: &str) -> Result<(), RunError>;
    fn validate_overrides(
        &self,
        sandbox: Option<&str>,
        approvals: Option<&str>,
    ) -> Result<(), RunError>;

    // The in-process runner's side of a turn (edd-01a1a454, step 1): what a
    // runner decides on its own, around the seam's kernel operations.

    /// Refuses a turn whose model the harness won't send prompts to, before
    /// the turn's conversation is rendered.
    fn check_turn(&self, model: Option<&str>, opts: &TurnOptions) -> Result<(), RunError>;
    /// The longest prompt, in bytes, the harness takes; `None` for no cap
    /// of its own. The turn shows fewer messages to fit it.
    fn prompt_max(&self) -> Option<usize>;
    /// Whether the agent's saved session `id` can still be resumed;
    /// `user_session` says it was started where the user's harness looks,
    /// `turns` is the agent's turn folder and `home` the account's home.
    fn resumable(
        &self,
        id: &str,
        user_session: bool,
        turns: &Path,
        opts: &TurnOptions,
        home: Option<&Path>,
    ) -> bool;
    /// Whether the launcher chooses a new session's id, so the session is
    /// known before the harness runs.
    fn chooses_session(&self) -> bool;
    /// A look at the harness's stderr while it runs: `Some(Err)` stops it,
    /// `Some(Ok)` ends the looking, `None` looks again later.
    fn early_check(
        &self,
        stderr: &str,
        role: Role,
        settings: CodexSettings<'_>,
    ) -> Option<Result<(), RunError>>;
    /// Whether, with no reply file, the harness's stdout is its final
    /// message.
    fn reply_on_stdout(&self) -> bool;
    /// Whether the harness's stdout is a structured protocol, one JSON
    /// event per line, so a JSON line in it is the harness's own.
    fn structured_output(&self) -> bool;
}

#[cfg(test)]
mod tests;
