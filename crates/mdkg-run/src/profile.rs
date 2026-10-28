//! The command line for one turn: which harness, which session, and the
//! role's permissions, stated on every run.
//!
//! mdkg 0.7.0 runs agents as the user would run them:
//! in the user's environment, with their own logins, settings and config,
//! and with no sandbox but each harness's own. Isolation is the worktree.
//! What the launcher adds is the role and the coordinator:
//! - **worker:** uses the harness's native auto mode and the user's own
//!   command permissions, with optional native overrides.
//! - **reviewer:** reads only. It writes nothing but its final message,
//!   which the launcher imports.
//! - both get the launcher's mdkg MCP server, acting as the agent, which
//!   opens the launcher's graph root rather than the agent's checkout.
//!
//! Only reviewers get a command allowlist. Workers retain mdkg's own auth
//! and abandon refusals. OS sandbox profiles and secret scanning are not
//! this crate's: run agents in a sandbox for those.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Harness {
    Claude,
    Codex,
    #[serde(rename = "opencode")]
    OpenCode,
}

impl Harness {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "claude" => Some(Harness::Claude),
            "codex" => Some(Harness::Codex),
            "opencode" => Some(Harness::OpenCode),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Harness::Claude => "claude",
            Harness::Codex => "codex",
            Harness::OpenCode => "opencode",
        }
    }

    /// The program, from `$MDKG_RUN_<HARNESS>` or PATH.
    pub fn program(self) -> PathBuf {
        let var = match self {
            Harness::Claude => "MDKG_RUN_CLAUDE",
            Harness::Codex => "MDKG_RUN_CODEX",
            Harness::OpenCode => "MDKG_RUN_OPENCODE",
        };
        std::env::var_os(var)
            .filter(|v| !v.is_empty())
            .map_or_else(|| PathBuf::from(self.as_str()), PathBuf::from)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Worker,
    Reviewer,
}

impl Role {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "worker" => Some(Role::Worker),
            "reviewer" => Some(Role::Reviewer),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Worker => "worker",
            Role::Reviewer => "reviewer",
        }
    }

    /// The Codex sandbox mode the role runs in: Codex's own, as a user
    /// would pick it.
    pub fn codex_sandbox(self) -> &'static str {
        match self {
            Role::Worker => "workspace-write",
            Role::Reviewer => "read-only",
        }
    }
}

/// A process to start for one turn. It runs with the launcher's whole
/// environment plus `env`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct AgentCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    /// Variables set on top of the launcher's environment.
    pub env: Vec<(String, String)>,
    /// Whether the prompt goes on stdin (otherwise it's in `args`).
    pub prompt_on_stdin: bool,
    /// Codex writes its final message here (`-o`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_file: Option<PathBuf>,
    /// Variables only this process gets, never shown, logged or passed to
    /// what the agent runs: an API key the fallback bills.
    #[serde(skip)]
    #[schemars(skip)]
    pub secret_env: Vec<(String, crate::auth::Secret)>,
    /// Identifies Codex workers for the per-attempt hook-trust receipt.
    /// The exact mdkg spelling alone is insufficient to grant global trust.
    #[serde(skip)]
    #[schemars(skip)]
    codex_hook_binary: Option<PathBuf>,
    /// Only OpenCode workers inspect the baseline build agent before spawn.
    #[serde(skip)]
    #[schemars(skip)]
    opencode_worker: bool,
}

/// The hook-trust decision for one actual Codex worker launch attempt.
/// A turn with an authentication retry may have a different second decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CodexHookTrust {
    /// Historical receipt variant. New launches do not emit this until a
    /// complete invocation-wide hook-source inventory is qualified.
    Bypassed,
    /// All active hook sources could not be verified; ordinary trust applies.
    Required,
}

impl CodexHookTrust {
    /// Fixed operator-facing explanation; no project content is echoed.
    pub fn reason(self) -> &'static str {
        match self {
            Self::Bypassed => {
                "verified mdkg-only project hooks; hook trust bypassed for this invocation"
            }
            Self::Required => {
                "all hook sources were not verified as mdkg-only; ordinary Codex hook trust is required"
            }
        }
    }
}

#[cfg(test)]
const CODEX_HOOK_TRUST_FLAG: &str = "--dangerously-bypass-hook-trust";

impl AgentCommand {
    /// Run immediately before each actual spawn, including retries, never
    /// for dry-run. No result is cached. Other harnesses and reviewers are
    /// unchanged. This starts only a native configuration probe, no model.
    pub fn preflight(&self) -> Result<(), crate::error::RunError> {
        if self.opencode_worker {
            opencode_preflight(self, std::time::Duration::from_secs(30))?;
        }
        Ok(())
    }

    /// Call for every spawn and retry so the receipt describes that attempt.
    /// A project-file predicate cannot authorize an invocation-wide bypass:
    /// user/system, session, profile, plugin and managed sources also load.
    /// Until that complete inventory is qualified, keep ordinary trust even
    /// when both project files look safe. Do not change worker config loading
    /// or write trust hashes to make the incomplete predicate pass.
    pub fn launch_args(&self) -> (Vec<String>, Option<CodexHookTrust>) {
        (
            self.args.clone(),
            self.codex_hook_binary
                .as_ref()
                .map(|_| CodexHookTrust::Required),
        )
    }
}

/// What a turn needs to build its command.
#[derive(Debug, Clone)]
pub struct TurnInput<'a> {
    /// The agent's actor id: its mdkg commands and MCP server act as it.
    pub agent: &'a str,
    /// The mdkg binary the agent's MCP server runs.
    pub mdkg: &'a Path,
    /// The user's home folder, passed to the MCP server, where mdkg's user
    /// config is.
    pub home: Option<&'a Path>,
    /// `$XDG_CONFIG_HOME`, when set: mdkg's config folder may be under it.
    pub xdg_config: Option<&'a Path>,
    /// The repository's git common dir, which the MCP server must use.
    pub common_dir: Option<&'a Path>,
    /// The worktree the MCP server's `--root` is in, which it must find
    /// there ($MDKG_TRUSTED_WORKTREE).
    pub trusted_worktree: Option<&'a Path>,
    /// The launcher's graph root: every agent's MCP server opens the
    /// repository there (`--root`), never through the agent's checkout.
    pub graph_root: Option<&'a Path>,
    /// The repository's main branch (`mdkg.mainRef`).
    pub main_ref: &'a str,
    pub harness: Harness,
    pub role: Role,
    pub cwd: &'a Path,
    pub model: Option<&'a str>,
    /// Reasoning effort, passed to Claude and Codex.
    pub effort: Option<&'a str>,
    /// Harness-native worker overrides, validated by state::check_native.
    pub sandbox: Option<&'a str>,
    pub approvals: Option<&'a str>,
    /// The session to resume; `None` starts one.
    pub resume: Option<&'a str>,
    /// A new id each turn: for Claude, the id to give a new session; for
    /// every agent, the turn's [`TURN_MARK`].
    pub new_session: &'a str,
    /// Extra allowed rules for a Claude worker.
    pub allow: &'a [String],
    /// Where Codex writes its final message.
    pub output_file: &'a Path,
    pub prompt: &'a str,
    /// The API key this turn bills, when the fallback is in use: its
    /// Keychain service and the key ([`crate::auth`]).
    pub api_key: Option<(&'a str, &'a crate::auth::Secret)>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct CodexSettings<'a> {
    pub sandbox: &'a str,
    pub approvals: &'a str,
}

pub(crate) fn codex_settings<'a>(
    role: Role,
    sandbox: Option<&'a str>,
    approvals: Option<&'a str>,
) -> CodexSettings<'a> {
    match role {
        Role::Reviewer => CodexSettings {
            sandbox: "read-only",
            approvals: "never",
        },
        Role::Worker => CodexSettings {
            sandbox: sandbox.unwrap_or("workspace-write"),
            approvals: approvals.unwrap_or("on-request"),
        },
    }
}

/// The MCP server name the installer writes into each agent's config.
pub const MCP_SERVER: &str = "mdkg";

/// Read-only MCP tools any role may use.
const MCP_READ: &[&str] = &[
    "room_read",
    "inbox",
    "show",
    "list",
    "search",
    "pack",
    "next",
    "scratch_path",
];
/// MCP tools that write to the coordinator, or block (`inbox_wait`), for
/// workers only.
const MCP_WRITE: &[&str] = &[
    "room_post",
    "inbox_wait",
    "ack",
    "lease_claim",
    "lease_release",
];

/// Commands a reviewer may run to read the repository.
const REVIEWER_COMMANDS: &[&str] = &[
    "git status",
    "git diff",
    "git log",
    "git show",
    "git blame",
    "git rev-parse",
    "git ls-files",
];

/// git's file-writing options on the read commands, denied for reviewers
/// wherever they appear: `--output=<file>` and `--output <file>` make
/// `diff`, `log` and `show` write their output to any path, outside the
/// checkout too (RC1 security audit). No `*` before `--output`'s dashes, so
/// a quoted `"--output=…"` matches as well, and the separate form is matched
/// bare or quoted. Options that only start with the name and print to
/// stdout (`--output-indicator-new=`, `--output-object-format=`) stay
/// allowed (review of d594575). The separate form is matched after a space
/// or a tab, the shell's word breaks within one command (review of
/// 9507252). git takes no abbreviation of `--output` (`--outp`), and none
/// of these commands takes `-o` as an output file.
const REVIEWER_OUTPUT_DENY: &[&str] = &[
    "git *--output=*",
    "git *--output *",
    "git *--output\" *",
    "git *--output' *",
    "git *--output\t*",
    "git *--output\"\t*",
    "git *--output'\t*",
];

/// mdkg commands every role may run, under each of mdkg's names.
const MDKG_READ: &[&str] = &["validate"];

/// mdkg commands no role may run, under each of mdkg's names: dropping
/// another agent's work, and the key helper (crate::auth).
const MDKG_DENY: &[&str] = &["abandon", "auth"];

/// The names an agent may call mdkg by: `mdkg`, and the name this turn's
/// binary is installed under when it's another (`mdkg-next`, beside
/// 0.6.0's `mdkg`). Taken from the binary's path, never hardcoded.
pub fn mdkg_names(mdkg: &Path) -> Vec<String> {
    let mut names = vec!["mdkg".to_string()];
    if let Some(own) = mdkg.file_name().and_then(|n| n.to_str())
        && !own.is_empty()
        && !names.iter().any(|n| n == own)
    {
        names.push(own.to_string());
    }
    names
}

/// An allow rule [`allow_command`] can't read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unreadable;

/// A `Bash(…)` rule's words, read as a shell does (single quotes, double
/// quotes, backslashes), and the first word's spelling as the rule writes
/// it. Claude's prefix form, `Bash(cmd args:*)`, is the rule content
/// ending in an unquoted `:*`; anywhere else a colon is literal.
fn bash_words(inner: &str) -> Result<(Vec<String>, String), Unreadable> {
    let content = inner.strip_suffix(":*").unwrap_or(inner);
    let mut words: Vec<String> = Vec::new();
    let mut word: Option<String> = None;
    let mut first_end: Option<usize> = None;
    let mut quote: Option<char> = None;
    let mut chars = content.char_indices();
    while let Some((at, c)) = chars.next() {
        match (quote, c) {
            (Some('\''), '\'') | (Some('"'), '"') => quote = None,
            (Some('"') | None, '\\') => {
                let (_, next) = chars.next().ok_or(Unreadable)?;
                word.get_or_insert_with(String::new).push(next);
            }
            (Some(_), _) => word.get_or_insert_with(String::new).push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                word.get_or_insert_with(String::new);
            }
            (None, c) if c.is_whitespace() => {
                if let Some(w) = word.take() {
                    if words.is_empty() {
                        first_end = Some(at);
                    }
                    words.push(w);
                }
            }
            (None, _) => word.get_or_insert_with(String::new).push(c),
        }
    }
    if quote.is_some() {
        return Err(Unreadable);
    }
    words.extend(word);
    if words.first().is_none_or(String::is_empty) {
        return Err(Unreadable);
    }
    let spelling = content[..first_end.unwrap_or(content.len())]
        .trim_start()
        .to_string();
    Ok((words, spelling))
}

/// An allow rule's command as Claude matches it: `Ok(None)` for a rule
/// that isn't `Bash(…)`; `Ok(Some((command, spelling)))`, the command read
/// as a shell word and spelled as the rule writes it; `Err` when the rule
/// can't be read (see [`bash_words`]; no closing `)`).
pub fn allow_command(rule: &str) -> Result<Option<(String, String)>, Unreadable> {
    let Some(inner) = rule.strip_prefix("Bash(") else {
        return Ok(None);
    };
    let inner = inner.strip_suffix(')').ok_or(Unreadable)?;
    let (mut words, spelling) = bash_words(inner)?;
    Ok(Some((words.swap_remove(0), spelling)))
}

/// Whether a command names an mdkg binary (by any path): a file name
/// starting `mdkg`, or the file name of `mdkg`, the binary the agent's
/// turns run (`--binary /opt/bin/graphctl`; Tier 1 round 2 of 53a11c9),
/// in any ASCII case, since a case-insensitive disk finds `MDKG` too
/// (round 3).
fn names_mdkg(command: &str, mdkg: &Path) -> bool {
    let Some(name) = Path::new(command).file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    name.to_ascii_lowercase().starts_with("mdkg")
        || mdkg
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|m| m.eq_ignore_ascii_case(name))
}

/// Whether an allow rule may be passed to a harness whose turns run the
/// mdkg binary `mdkg`. Fail closed (the orchestrator, 2026-10-25): one
/// rule, a tool's name with an optional `(…)` holding no `(`, `)` or `,`
/// (a list such as `Read,Bash(…)` or `Read(*),Bash(*)` isn't one); a
/// `Bash(…)` rule whose command word can be read; and one
/// naming an mdkg binary must name, next, a fixed subcommand mdkg allows
/// (`mdkg validate *`): an open one (`mdkg *`, `mdkg:*`, `mdkg --root .
/// *`) would admit `abandon` and `auth` past any deny written after the
/// binary. A guardrail against running those directly, not isolation
/// (see crate::auth).
pub fn passable_allow(rule: &str, mdkg: &Path) -> bool {
    refusal(rule, mdkg).is_none()
}

/// Why [`passable_allow`] refuses a rule, and what to write instead: the
/// warning's text for a dropped rule, `None` for a rule that's passed on.
pub fn dropped_allow_fix(rule: &str, mdkg: &Path) -> Option<String> {
    refusal(rule, mdkg).map(|r| match r {
        Refusal::NotOneRule => "it isn't one rule (a `(`, `)` or `,` inside its parentheses \
             could start another); pass each rule as its own `--allow`"
            .to_string(),
        Refusal::BareBash => "`Bash` alone would let it run any command, mdkg with any \
             arguments; name the command, as in `Bash(npm test *)`"
            .to_string(),
        Refusal::Unreadable => "it can't be read as a command (an unbalanced quote, or a \
             backslash with nothing after it); fix its quoting"
            .to_string(),
        Refusal::Glob => "a pattern as the command could match mdkg; name the command, \
             as in `Bash(npm test *)`"
            .to_string(),
        Refusal::ShellSyntax => "its command uses shell syntax the rule reader doesn't \
             define (`$`, a backtick, `=`, a redirection), so it may run another command than \
             it reads as; write the command as a plain word or a quoted path"
            .to_string(),
        Refusal::Escaped => "its command has a backslash or a letter outside ASCII, which \
             Claude, the shell or the disk may read as mdkg; quote the path instead, as in \
             `Bash('/opt/my tools/npm' test *)`"
            .to_string(),
        Refusal::OpenMdkg(bin) => format!(
            "it would let mdkg run with any arguments, `abandon` and `auth` included; \
             allow each subcommand the worker needs, as in `Bash({bin} validate *)`, or \
             let the worker use the mdkg MCP server it already has"
        ),
        Refusal::Denied(sub) => {
            format!("`{sub}` is never allowed to an agent; drop the rule")
        }
    })
}

/// Why a rule isn't passed on.
enum Refusal {
    /// A list of rules (`Read,Bash(…)`, `Read(*),Bash(*)`), or not a
    /// rule's shape.
    NotOneRule,
    /// `Bash` with no command.
    BareBash,
    /// An unbalanced quote, a trailing backslash, an empty command.
    Unreadable,
    /// A glob as the command (`*`, `mdk?`).
    Glob,
    /// A backslash or a non-ASCII character in the command.
    Escaped,
    /// Shell syntax the reader doesn't define in the command word.
    ShellSyntax,
    /// An mdkg binary, spelled as the rule spells it, with no fixed
    /// subcommand next (`mdkg *`, `mdkg:*`, `mdkg --root . *`).
    OpenMdkg(String),
    /// An mdkg subcommand no agent may run (`abandon`, `auth`).
    Denied(String),
}

fn refusal(rule: &str, mdkg: &Path) -> Option<Refusal> {
    let name_end = rule.find('(').unwrap_or(rule.len());
    let name = &rule[..name_end];
    // With no parenthesis or comma inside the rule's own `(…)`, however a
    // harness splits a list, it sees this one rule.
    let one_rule = !name.is_empty()
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && (name_end == rule.len()
            || rule.len() > name_end + 1
                && rule.ends_with(')')
                && !rule[name_end + 1..rule.len() - 1].contains(['(', ')', ',']));
    if !one_rule {
        return Some(Refusal::NotOneRule);
    }
    // `Bash` alone admits every command, mdkg's with any arguments.
    let Some(inner) = rule.strip_prefix("Bash(").and_then(|r| r.strip_suffix(')')) else {
        return (name == "Bash").then_some(Refusal::BareBash);
    };
    let Ok((words, spelling)) = bash_words(inner) else {
        return Some(Refusal::Unreadable);
    };
    // A glob as the command (`*`, `mdk?`) may match an mdkg binary.
    if words[0].contains(['*', '?', '[']) {
        return Some(Refusal::Glob);
    }
    // The command word must be one the reader defines: letters, digits,
    // `/ . _ + : @ % ~ -`, and quotes (so a quoted space). Claude decodes a
    // backslash before matching and the shell decodes it again
    // (`/opt/bin/\\mdkg`; Tier 1 round 3 of 19c1fc3); a letter outside
    // ASCII may fold to one of mdkg's on a case-insensitive disk; and
    // `$'…'`, a backtick, `X=1 mdkg` or `<x mdkg` put the command elsewhere
    // (round 4). Only the command word is read: what follows, operators
    // included, is Claude's to split, and a command that runs other code
    // (`env`, `sh -c`, `cargo`) is out of scope, since the denies are a
    // guardrail against running mdkg directly, not isolation.
    if spelling.contains('\\') || !spelling.is_ascii() {
        return Some(Refusal::Escaped);
    }
    if !spelling
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "/._+:@%~-'\" ".contains(c))
    {
        return Some(Refusal::ShellSyntax);
    }
    if !names_mdkg(&words[0], mdkg) {
        return None;
    }
    let Some(sub) = words.get(1) else {
        return Some(Refusal::OpenMdkg(spelling));
    };
    let fixed = sub
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && sub.starts_with(|c: char| c.is_ascii_lowercase());
    if !fixed {
        return Some(Refusal::OpenMdkg(spelling));
    }
    MDKG_DENY
        .contains(&sub.as_str())
        .then(|| Refusal::Denied(sub.clone()))
}

/// The rules [`passable_allow`] refuses: never passed to the harness, and
/// named in the turn's report and `agents status`.
pub fn unreadable_allow(allow: &[String], mdkg: &Path) -> Vec<String> {
    allow
        .iter()
        .filter(|r| !passable_allow(r, mdkg))
        .cloned()
        .collect()
}

/// An allow rule that's never passed to the harness, and what to write
/// instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DroppedAllow {
    /// The rule, as configured.
    pub rule: String,
    /// Why it's dropped, and what to allow instead.
    pub fix: String,
}

/// [`unreadable_allow`], each with its [`dropped_allow_fix`].
pub fn dropped_allow(allow: &[String], mdkg: &Path) -> Vec<DroppedAllow> {
    allow
        .iter()
        .filter_map(|rule| {
            let fix = dropped_allow_fix(rule, mdkg)?;
            Some(DroppedAllow {
                rule: rule.clone(),
                fix,
            })
        })
        .collect()
}

/// [`mdkg_names`], plus every readable allow rule's command that names an
/// mdkg binary (by any path, in any quoting), spelled as the rule spells
/// it, which is how the agent must write it for the rule to match: abandon
/// and auth are denied under each (the orchestrator's fail-closed rule,
/// 2026-10-25).
pub fn mdkg_names_allowing(mdkg: &Path, allow: &[String]) -> Vec<String> {
    let mut names = mdkg_names(mdkg);
    for rule in allow {
        let Ok(Some((command, spelling))) = allow_command(rule) else {
            continue;
        };
        if names_mdkg(&command, mdkg) && !names.contains(&spelling) {
            names.push(spelling);
        }
    }
    names
}

/// `sub` under each of `names`: `mdkg validate`, `mdkg-next validate`.
fn mdkg_commands(names: &[String], subs: &[&str]) -> Vec<String> {
    names
        .iter()
        .flat_map(|n| subs.iter().map(move |s| format!("{n} {s}")))
        .collect()
}

/// Additional reviewer denials, including when user settings allow them.
/// Workers use the user's command policy instead.
const REVIEWER_DENY: &[&str] = &[
    "Bash(git push)",
    "Bash(git push *)",
    "Bash(git remote *)",
    "Bash(git config *)",
    "Bash(gh *)",
    "Bash(npm publish *)",
    "Bash(cargo publish *)",
    // Reviewers do not read the Keychain. Workers keep native permissions.
    "Bash(security)",
    "Bash(security *)",
    "Bash(/usr/bin/security *)",
];

fn bash_rules(commands: &[&str]) -> Vec<String> {
    // `Bash(cmd *)` needs an argument after the command; `Bash(cmd)` covers
    // the bare form.
    commands
        .iter()
        .flat_map(|c| [format!("Bash({c})"), format!("Bash({c} *)")])
        .collect()
}

fn mcp_rules(tools: &[&str]) -> Vec<String> {
    tools
        .iter()
        .map(|t| format!("mcp__{MCP_SERVER}__{t}"))
        .collect()
}

/// Claude's allow and deny rules for a role.
pub fn claude_rules(
    role: Role,
    extra_allow: &[String],
    names: &[String],
) -> (Vec<String>, Vec<String>) {
    let mut deny = bash_rules(
        &mdkg_commands(names, MDKG_DENY)
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
    );
    if role == Role::Worker {
        // Native worker tools and command permissions remain intact. Only
        // explicitly configured user grants are added, never a default list.
        return (extra_allow.to_vec(), deny);
    }
    let mut allow: Vec<String> = ["Read", "Glob", "Grep"].map(String::from).to_vec();
    allow.extend(bash_rules(REVIEWER_COMMANDS));
    let reads = mdkg_commands(names, MDKG_READ);
    allow.extend(bash_rules(
        &reads.iter().map(String::as_str).collect::<Vec<_>>(),
    ));
    allow.extend(mcp_rules(MCP_READ));
    deny.extend(REVIEWER_DENY.iter().map(|s| s.to_string()));
    deny.extend(REVIEWER_OUTPUT_DENY.iter().map(|c| format!("Bash({c})")));
    // Read-only: no edits or coordinator writes; extra allows are ignored.
    deny.extend(["Edit", "Write", "NotebookEdit"].map(String::from));
    deny.extend(mcp_rules(MCP_WRITE));
    (allow, deny)
}

/// The command for one turn.
pub fn command(t: &TurnInput<'_>) -> AgentCommand {
    command_with_path(
        t,
        std::env::var_os("PATH").as_deref(),
        std::env::current_exe().ok().as_deref(),
    )
}

fn command_with_path(
    t: &TurnInput<'_>,
    inherited: Option<&std::ffi::OsStr>,
    launcher: Option<&Path>,
) -> AgentCommand {
    command_with_resolved_mcp(
        t,
        inherited,
        launcher,
        resolve_mcp(t.mdkg, t.cwd, inherited),
    )
}

fn command_with_resolved_mcp(
    t: &TurnInput<'_>,
    inherited: Option<&std::ffi::OsStr>,
    launcher: Option<&Path>,
    mdkg: Option<PathBuf>,
) -> AgentCommand {
    // Resolve before changing PATH, but retain the configured spelling when
    // the new PATH still invokes that program (including its npm launcher).
    // Harness configs accept UTF-8 strings, unlike the process's PATH.
    // A lossy absolute spelling could name another file.
    let mdkg = mdkg.filter(|program| program.to_str().is_some());
    let path = mdkg.as_ref().and_then(|_| {
        let exe = launcher?.canonicalize().ok()?;
        launcher_path(exe.parent()?, inherited)
    });
    let pin = mdkg.as_ref().filter(|original| {
        t.mdkg.file_name() == Some(t.mdkg.as_os_str())
            && path.as_deref().is_some_and(|path| {
                !resolve_mcp(t.mdkg, t.cwd, Some(std::ffi::OsStr::new(path)))
                    .is_some_and(|selected| crate::program::same_invocation(original, &selected))
            })
    });
    let t = TurnInput {
        mdkg: pin.map_or(t.mdkg, |program| program.as_path()),
        ..t.clone()
    };
    let mut command = match t.harness {
        Harness::Claude => claude(&t),
        Harness::Codex => codex(&t, mdkg.as_deref()),
        Harness::OpenCode => opencode(&t),
    };
    // An unresolved bare command must not become a different executable
    // merely because the launcher folder was prepended.
    if mdkg.is_none() {
        eprintln!("mdkg couldn't pin the MCP command from inherited PATH; inherited PATH retained");
        return command;
    }
    // Shell startup may replace PATH again. Keep the original MCP selection
    // as an absolute invocation, including its symlink/npm-wrapper spelling.
    command.env.push((
        SHELL_BINARY.into(),
        mdkg.as_ref()
            .expect("resolved above")
            .to_str()
            .expect("UTF-8 above")
            .into(),
    ));
    if let Some(path) = path {
        command.env.push(("PATH".into(), path));
    } else {
        eprintln!("mdkg couldn't prepend the launcher folder to PATH; inherited PATH retained");
    }
    command
}

fn resolve_mcp(program: &Path, cwd: &Path, inherited: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    if program.is_absolute() {
        return Some(program.to_path_buf());
    }
    let cwd = std::path::absolute(cwd).ok()?;
    if program.file_name() != Some(program.as_os_str()) {
        return Some(cwd.join(program));
    }
    // Empty and relative PATH entries are relative to the agent's cwd,
    // which is where the harness and its MCP server previously searched.
    std::env::split_paths(inherited_path(inherited))
        .map(|folder| cwd.join(folder).join(program))
        .find(|candidate| {
            candidate.is_file()
                && rustix::fs::access(candidate, rustix::fs::Access::EXEC_OK).is_ok()
        })
}

/// What the launcher sets for the agent on top of its own environment: its
/// actor for every mdkg command it runs, and the turn's mark. No pins:
/// they'd make every mdkg its tests build refuse their own temporary
/// repositories (review of 6dd1cfd). The MCP server gets its pins from
/// [`mcp_env`].
fn agent_env(t: &TurnInput<'_>) -> Vec<(String, String)> {
    // git status mustn't refresh an index a reviewer shouldn't write.
    vec![
        ("MDKG_ACTOR".into(), t.agent.to_string()),
        ("GIT_OPTIONAL_LOCKS".into(), "0".into()),
        (TURN_MARK.into(), t.new_session.to_string()),
    ]
}

fn launcher_path(folder: &Path, inherited: Option<&std::ffi::OsStr>) -> Option<String> {
    let paths = std::iter::once(folder.to_path_buf())
        .chain(std::env::split_paths(inherited_path(inherited)));
    std::env::join_paths(paths).ok()?.into_string().ok()
}

fn inherited_path(inherited: Option<&std::ffi::OsStr>) -> &std::ffi::OsStr {
    inherited.unwrap_or_else(|| std::ffi::OsStr::new("/usr/bin:/bin"))
}

/// The exact configured MCP executable for shell invocations in this turn.
/// Unlike PATH lookup, this survives a login profile prepending an older build.
pub const SHELL_BINARY: &str = "MDKG_BIN";

/// Shared by the trusted turn prompt and opt-in AGENTS.md projection.
pub const SHELL_INSTRUCTION: &str = "In launcher turns, use the mdkg MCP tools or \
    invoke `\"$MDKG_BIN\"` for every mdkg shell command. If MDKG_BIN is empty or \
    unavailable, use MCP; never fall back to a bare mdkg command.";

/// A nested launcher must replace its parent's repository pins and exact
/// binary context. Unresolved commands leave no inherited MDKG_BIN behind.
pub const NOT_INHERITED: &[&str] = &[TRUSTED_COMMON_DIR, TRUSTED_WORKTREE, SHELL_BINARY];

/// The variable marking every process a turn starts, with an id of the
/// turn's own: the launcher finds what the agent left running by it, in
/// whatever session it ended up (review of 7e6f4b7).
pub const TURN_MARK: &str = "MDKG_RUN_TURN";

/// The MCP server's arguments: the launcher's graph root, then `mcp`. It
/// never finds the repository through the agent's checkout, whose `.git`
/// the agent can repoint (orchestrator, 2026-10-21).
fn mcp_args(t: &TurnInput<'_>) -> Vec<String> {
    let mut args = Vec::new();
    // A launcher allowed to upgrade the store passes that on to the agent's
    // server, which runs the same build (lane 2's guard).
    if mdkg_coord_sqlite::store_upgrades_allowed() {
        args.push("--allow-store-upgrade".to_string());
    }
    if let Some(r) = t.graph_root {
        args.extend(["--root".to_string(), r.display().to_string()]);
    }
    args.push("mcp".into());
    args
}

/// The MCP server's environment: the agent's actor, the turn's mark, and
/// the common dir and worktree the launcher trusts, so mdkg refuses a
/// worktree whose `.git` leads anywhere else (lease-guard, 2026-10-21 and
/// 2026-10-22). Also `HOME`, the config folder and the main branch, which
/// Codex doesn't pass on to MCP servers by itself.
fn mcp_env(t: &TurnInput<'_>) -> serde_json::Map<String, serde_json::Value> {
    let mut env = serde_json::Map::new();
    env.insert("MDKG_ACTOR".into(), t.agent.into());
    // The turn's mark, so the CLI calls the server runs in groups of their
    // own are found and stopped after the turn if the server dies first
    // (merge review of 6b9ac50).
    env.insert(TURN_MARK.into(), t.new_session.into());
    if let Some(h) = t.home {
        env.insert("HOME".into(), h.display().to_string().into());
    }
    let config = t
        .xdg_config
        .map(Path::to_path_buf)
        .or_else(|| t.home.map(|h| h.join(".config")));
    if let Some(x) = config {
        env.insert("XDG_CONFIG_HOME".into(), x.display().to_string().into());
    }
    env.insert("MDKG_MAIN_REF".into(), t.main_ref.into());
    if let Some(c) = t.common_dir {
        env.insert(TRUSTED_COMMON_DIR.into(), c.display().to_string().into());
    }
    if let Some(w) = t.trusted_worktree {
        env.insert(TRUSTED_WORKTREE.into(), w.display().to_string().into());
    }
    env
}

/// The variable naming the git common dir every mdkg process the
/// launcher starts must use.
pub const TRUSTED_COMMON_DIR: &str = "MDKG_TRUSTED_COMMON_DIR";

/// The variable naming the worktree every mdkg process the launcher
/// starts must find its root in.
pub const TRUSTED_WORKTREE: &str = "MDKG_TRUSTED_WORKTREE";

fn claude_mcp_config(t: &TurnInput<'_>) -> serde_json::Value {
    serde_json::json!({"mcpServers": {MCP_SERVER: {
        "command": t.mdkg.display().to_string(),
        "args": mcp_args(t),
        "env": mcp_env(t),
    }}})
}

fn toml_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// A TOML inline table from keys and TOML values.
fn toml_table(entries: &[(String, String)]) -> String {
    let body: Vec<String> = entries
        .iter()
        .map(|(k, v)| format!("{}={v}", toml_string(k)))
        .collect();
    format!("{{{}}}", body.join(", "))
}

/// The main branch (short name, when it's a local branch), `main`, and the
/// named branches.
pub fn protected(main_ref: &str, names: &[String]) -> Vec<String> {
    let main = match main_ref.strip_prefix("refs/heads/") {
        Some(short) => short,
        None if main_ref.starts_with("refs/") => "",
        None => main_ref,
    };
    let mut all = vec!["main".to_string()];
    if main != "main" && !main.is_empty() {
        all.push(main.to_string());
    }
    for n in names {
        if !all.contains(n) {
            all.push(n.clone());
        }
    }
    all
}

/// Claude Code with the user's own login, settings and MCP servers, plus
/// the launcher's: `-p` with the role's tools and rules, nobody answering
/// prompts, and the mdkg server added to whatever the user has.
/// `s` as one word for a POSIX shell.
fn shell_word(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The launcher's credential helper is not an agent command. Clear only
/// its turn mark so CLI semantic refusals do not reject this fixed helper.
/// An agent can clear its own environment too; this is not isolation.
pub fn key_helper(mdkg: &Path, service: &str) -> String {
    let provider = crate::auth::provider_of(service).unwrap_or("anthropic");
    format!(
        "/usr/bin/env -u {TURN_MARK} {} auth key {provider}",
        shell_word(&mdkg.display().to_string())
    )
}

fn claude(t: &TurnInput<'_>) -> AgentCommand {
    // A rule that can't be read never reaches Claude (fail closed); the
    // turn and `agents status` name it (TurnReport::dropped_allow).
    let readable: Vec<String> = t
        .allow
        .iter()
        .filter(|r| passable_allow(r, t.mdkg))
        .cloned()
        .collect();
    let (allow, mut deny) =
        claude_rules(t.role, &readable, &mdkg_names_allowing(t.mdkg, &readable));
    // The helper, and abandon, by the path this turn names mdkg by, too.
    let helper = shell_word(&t.mdkg.display().to_string());
    for sub in ["auth", "abandon"] {
        deny.push(format!("Bash({helper} {sub})"));
        deny.push(format!("Bash({helper} {sub} *)"));
        deny.push(format!("Bash({} {sub})", t.mdkg.display()));
        deny.push(format!("Bash({} {sub} *)", t.mdkg.display()));
    }
    let mode = match t.role {
        Role::Worker => t.approvals.unwrap_or("auto"),
        Role::Reviewer => "manual",
    };
    let mut args: Vec<String> = vec!["-p".into()];
    match t.resume {
        Some(id) => args.extend(["--resume".into(), id.to_string()]),
        None => args.extend(["--session-id".into(), t.new_session.to_string()]),
    }
    args.extend(
        [
            "--output-format",
            "stream-json",
            "--verbose",
            "--permission-mode",
            mode,
        ]
        .map(String::from),
    );
    if t.role == Role::Reviewer {
        args.extend([
            "--tools".into(),
            "Read,Glob,Grep,Bash".into(),
            "--permission-prompts".into(),
            "none".into(),
        ]);
        // A review reads; its checkout's skills and commands aren't its
        // instructions.
        args.push("--disable-slash-commands".into());
        // Nor are the checkout's settings: a PR head's .claude/settings
        // (hooks, permissions), .mcp.json and CLAUDE.md would run or
        // speak for its author. The user's own settings still apply (RC1
        // security audit SEC7).
        args.extend(["--setting-sources".into(), "user".into()]);
    }
    // The fallback's key, through a helper Claude runs itself: never in the
    // environment its tools see.
    if let Some((service, _)) = t.api_key {
        args.extend([
            "--settings".into(),
            serde_json::json!({"apiKeyHelper": key_helper(t.mdkg, service)}).to_string(),
        ]);
    }
    args.extend(["--mcp-config".into(), claude_mcp_config(t).to_string()]);
    if !allow.is_empty() {
        args.push("--allowedTools".into());
        args.extend(allow);
    }
    args.push("--disallowedTools".into());
    args.extend(deny);
    if let Some(m) = t.model {
        args.extend(["--model".into(), m.to_string()]);
    }
    if let Some(e) = t.effort {
        args.extend(["--effort".into(), e.to_string()]);
    }
    AgentCommand {
        program: Harness::Claude.program(),
        args,
        cwd: t.cwd.to_path_buf(),
        env: agent_env(t),
        prompt_on_stdin: true,
        output_file: None,
        secret_env: Vec::new(),
        codex_hook_binary: None,
        opencode_worker: false,
    }
}

/// Codex with the user's own CODEX_HOME (login, config, sessions), in
/// native worker auto mode or the reviewer's read-only mode, with the
/// launcher's mdkg server added.
fn codex(t: &TurnInput<'_>, shell_binary: Option<&Path>) -> AgentCommand {
    let settings = codex_settings(t.role, t.sandbox, t.approvals);
    let mut args: Vec<String> = vec!["exec".into()];
    if let Some(id) = t.resume {
        // `exec resume` has no -s or -C: the sandbox goes in as config, and
        // the folder is the process's working directory. The launcher then
        // checks Codex's header for the sandbox it actually used.
        args.extend(["resume".into(), id.to_string()]);
    } else {
        args.extend(["-C".into(), t.cwd.display().to_string()]);
    }
    if t.role == Role::Worker && t.resume.is_none() && t.sandbox.is_none() && t.approvals.is_none()
    {
        // This flag implies workspace-write and conflicts with --sandbox.
        args.push("--approve-for-me".into());
    } else {
        if t.role == Role::Reviewer && t.resume.is_none() {
            args.extend(["-s".into(), settings.sandbox.into()]);
        } else {
            args.extend([
                "-c".into(),
                format!("sandbox_mode={}", toml_string(settings.sandbox)),
            ]);
        }
        args.extend([
            "-c".into(),
            format!("approval_policy={}", toml_string(settings.approvals)),
        ]);
        if t.role == Role::Worker {
            args.extend(["-c".into(), "approvals_reviewer=\"auto_review\"".into()]);
        }
    }
    if t.role == Role::Reviewer {
        // A reviewer's checkout is a PR head: its AGENTS.md isn't the rules
        // (the launcher quotes the base's copy).
        args.extend(["-c".into(), "project_doc_max_bytes=0".into()]);
        // Nor is its .codex/config.toml, whose MCP servers and `notify`
        // would run the author's code. Codex loads it wherever the user's
        // config trusts the repository, its worktrees included, and no
        // `-c` override turns that off: only leaving the user's config out
        // does. So a Codex reviewer runs on Codex's defaults, the agent's
        // model and effort, and the user's login, which CODEX_HOME still
        // holds (review of 4fe6297).
        args.push("--ignore-user-config".into());
    }
    args.extend([
        "-c".into(),
        format!(
            "mcp_servers.{MCP_SERVER}.command={}",
            toml_string(&t.mdkg.display().to_string())
        ),
        "-c".into(),
        format!(
            "mcp_servers.{MCP_SERVER}.args=[{}]",
            mcp_args(t)
                .iter()
                .map(|a| toml_string(a))
                .collect::<Vec<_>>()
                .join(",")
        ),
        "-c".into(),
        format!(
            "mcp_servers.{MCP_SERVER}.env={}",
            toml_table(
                &mcp_env(t)
                    .iter()
                    .map(|(k, v)| (k.clone(), toml_string(v.as_str().unwrap_or_default())))
                    .collect::<Vec<_>>()
            )
        ),
        // A reviewer's server offers only the read tools.
        "-c".into(),
        format!(
            "mcp_servers.{MCP_SERVER}.enabled_tools=[{}]",
            mcp_tools(t.role)
                .iter()
                .map(|n| toml_string(n))
                .collect::<Vec<_>>()
                .join(",")
        ),
    ]);
    // Each mdkg tool the role may call is approved: Codex 0.164 asks before
    // a tool its annotations call destructive (`ack`, which can purge a
    // body), and a turn whose approval policy is never refuses it. The
    // role's enabled_tools above is the decision; Codex 0.136 takes the
    // setting too.
    for tool in mcp_tools(t.role) {
        args.extend([
            "-c".into(),
            format!("mcp_servers.{MCP_SERVER}.tools.{tool}.approval_mode=\"approve\""),
        ]);
    }
    // The final message: in the `-o` file, but on a key turn only on
    // stdout, which reaches its file through the launcher's redactor; a
    // file Codex writes itself could hold the key (review of 3a09b88).
    if t.api_key.is_none() {
        args.extend(["-o".into(), t.output_file.display().to_string()]);
    }
    if let Some(m) = t.model {
        args.extend(["-m".into(), m.to_string()]);
    }
    if let Some(e) = t.effort {
        args.extend(["-c".into(), format!("model_reasoning_effort=\"{e}\"")]);
    }
    // The fallback's key goes to the codex process alone, and never to the
    // shells it runs for the agent.
    let mut secret_env = Vec::new();
    if let Some((_, key)) = t.api_key {
        args.extend([
            "-c".into(),
            "shell_environment_policy.exclude=[\"CODEX_API_KEY\",\"OPENAI_API_KEY\"]".into(),
        ]);
        secret_env.push(("CODEX_API_KEY".to_string(), key.clone()));
    }
    // Codex may filter inherited variables (e.g. inherit=core). Set only
    // this one value; keep the user's filters and all other shell settings.
    // An empty override also prevents a stale user-config value when the
    // MCP command could not be resolved. An include allowlist can remove it.
    args.extend([
        "-c".into(),
        format!(
            "shell_environment_policy.set.{SHELL_BINARY}={}",
            toml_string(shell_binary.and_then(Path::to_str).unwrap_or(""))
        ),
    ]);
    args.push("-".into());
    AgentCommand {
        program: Harness::Codex.program(),
        args,
        cwd: t.cwd.to_path_buf(),
        env: agent_env(t),
        prompt_on_stdin: true,
        output_file: t.api_key.is_none().then(|| t.output_file.to_path_buf()),
        secret_env,
        codex_hook_binary: (t.role == Role::Worker).then(|| t.mdkg.to_path_buf()),
        opencode_worker: false,
    }
}

/// The mdkg MCP tools a role may call.
pub fn mcp_tools(role: Role) -> Vec<&'static str> {
    let mut tools = MCP_READ.to_vec();
    if role == Role::Worker {
        tools.extend(MCP_WRITE);
    }
    tools
}

fn opencode_agent(t: &TurnInput<'_>) -> String {
    match t.role {
        Role::Worker => "build".into(),
        // A fresh name avoids inheriting a user's agent model, prompt or
        // tools. Resumed sessions also select this turn's protected agent.
        Role::Reviewer => format!("mdkg-reviewer-{}", t.new_session),
    }
}

fn opencode_preflight_error(why: &str) -> crate::error::RunError {
    crate::error::RunError::failure(
        crate::error::CODE_AGENT_FAILED,
        format!("OpenCode worker preflight refused the turn: {why}"),
    )
    .with_hint("mdkg workers require an enabled build agent and OpenCode's 2.1 native configuration API; check OpenCode's configuration and plugin errors, then retry")
}

/// The native server's process group belongs to this probe. Holding the
/// unreaped leader until cleanup also keeps its process/group id reserved.
/// Same-user plugins that deliberately leave the group (for example with
/// setsid) escape this cleanup; this is not process-tree containment.
struct OpenCodeProbe(std::process::Child);

impl Drop for OpenCodeProbe {
    fn drop(&mut self) {
        use rustix::process::{Pid, Signal, WaitId, WaitIdOptions, kill_process_group, waitid};
        self.0.stdin.take(); // --stdio exits when its input closes.
        if let Some(pid) = Pid::from_raw(self.0.id() as i32) {
            let _ = kill_process_group(pid, Signal::TERM);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            while std::time::Instant::now() < deadline {
                match waitid(
                    WaitId::Pid(pid),
                    WaitIdOptions::EXITED | WaitIdOptions::NOWAIT | WaitIdOptions::NOHANG,
                ) {
                    Ok(None) => std::thread::sleep(std::time::Duration::from_millis(20)),
                    _ => break,
                }
            }
            let _ = kill_process_group(pid, Signal::KILL);
        }
        let _ = self.0.wait();
    }
}

/// Keep the probe command's group owned until stdout closes, even if a
/// launcher shim exits first and a descendant still holds the pipe.
fn opencode_probe_output(
    mut command: std::process::Command,
    limit: std::time::Duration,
) -> Result<Vec<u8>, crate::error::RunError> {
    use rustix::process::{Pid, WaitId, WaitIdOptions, waitid};
    use std::io::Read;
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    let deadline = Instant::now() + limit;
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|_| opencode_preflight_error("cannot start the native probe command"))?;
    let mut child = OpenCodeProbe(child);
    let pid = Pid::from_raw(child.0.id() as i32).expect("child pid");
    let stdout = child.0.stdout.take().expect("piped probe output");
    let (tx, rx) = std::sync::mpsc::channel();
    const MAX: usize = 4 * 1024 * 1024;
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout.take(MAX as u64 + 1).read_to_end(&mut bytes);
        let _ = tx.send(result.map(|_| bytes));
    });
    let mut output = None;
    loop {
        if output.is_none() {
            match rx.try_recv() {
                Ok(Ok(bytes)) if bytes.len() <= MAX => output = Some(bytes),
                Ok(_) | Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    return Err(opencode_preflight_error(
                        "native probe output failed or exceeded its limit",
                    ));
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
        let status = waitid(
            WaitId::Pid(pid),
            WaitIdOptions::EXITED | WaitIdOptions::NOWAIT | WaitIdOptions::NOHANG,
        )
        .map_err(|_| opencode_preflight_error("cannot wait for the native probe command"))?;
        if let Some(status) = status {
            if status.exit_status() != Some(0) {
                return Err(opencode_preflight_error(
                    "native probe command reported an error",
                ));
            }
            if let Some(output) = output {
                return Ok(output);
            }
        }
        if Instant::now() >= deadline {
            return Err(opencode_preflight_error("native probe command timed out"));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// OpenCode 2.1 normalizes and resolves user config itself. Never parse
/// those files here. This observes the baseline before mdkg's permission
/// overlay can recreate a disabled build agent. This is an ordinary native
/// startup against the worker's HOME/XDG/OPENCODE_DB state, with its normal
/// startup side effects. mdkg reads/copies none of that config, cache or
/// credentials. An empty scratch database would omit cached remote configs
/// that can disable build. Validate the binary's version before starting
/// its server. This is not an atomic snapshot with the later run: OpenCode
/// watches config, and the run starts its own server. A concurrent config
/// or plugin change remains possible.
fn opencode_preflight(
    command: &AgentCommand,
    limit: std::time::Duration,
) -> Result<(), crate::error::RunError> {
    use std::io::{BufRead, Read};
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let deadline = Instant::now() + limit;
    let remaining = || {
        deadline
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
            .ok_or_else(|| opencode_preflight_error("configuration probe timed out"))
    };
    let cwd = command
        .cwd
        .canonicalize()
        .map_err(|_| opencode_preflight_error("cannot resolve the worker's checkout"))?;
    let directory = cwd
        .to_str()
        .ok_or_else(|| opencode_preflight_error("the native API requires a UTF-8 checkout path"))?;
    let password = format!("{}{}", uuid::Uuid::now_v7(), uuid::Uuid::now_v7());
    let process = || {
        let mut c = Command::new(&command.program);
        for key in NOT_INHERITED {
            c.env_remove(key);
        }
        for key in crate::auth::withheld_vars() {
            c.env_remove(key);
        }
        // Match the eventual worker's environment, except for mdkg's
        // overlay. The final worker replaces inherited inline config too;
        // letting it enable build only during this probe would be unsafe.
        c.envs(
            command
                .env
                .iter()
                .filter(|(key, _)| key != "OPENCODE_CONFIG_CONTENT")
                .map(|(key, value)| (key, value)),
        );
        c.env_remove("OPENCODE_CONFIG_CONTENT")
            .env("OPENCODE_PASSWORD", &password)
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stderr(Stdio::null());
        c
    };
    // Only 2.1.0's native configuration/activation contract is qualified.
    // Refuse unsupported or unknown versions before starting the server;
    // --version still performs ordinary native CLI initialization/logging.
    // Confirm the running server's version too.
    let mut version = process();
    version.arg("--version");
    let version = opencode_probe_output(version, remaining()?.min(Duration::from_secs(10)))?;
    let version = version.trim_ascii();
    if version != b"opencode v2.1.0" && version != b"2.1.0" {
        return Err(opencode_preflight_error(
            "this OpenCode version is not qualified (requires 2.1.0)",
        ));
    }
    let child = process()
        .args(["serve", "--stdio", "--hostname", "127.0.0.1", "--port", "0"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|_| opencode_preflight_error("cannot start the private configuration server"))?;
    let mut server = OpenCodeProbe(child);
    let stdout = server.0.stdout.take().expect("piped probe stdout");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(stdout);
        let mut line = Vec::new();
        let read = (&mut reader).take(8192).read_until(b'\n', &mut line);
        let _ = tx.send(read.map(|_| line));
        // Never collect native logs or config; avoid a full output pipe.
        let _ = std::io::copy(&mut reader, &mut std::io::sink());
    });
    let line = rx
        .recv_timeout(remaining()?.min(Duration::from_secs(10)))
        .ok()
        .and_then(Result::ok)
        .ok_or_else(|| opencode_preflight_error("private server did not report its endpoint"))?;
    let endpoint: serde_json::Value = serde_json::from_slice(&line)
        .map_err(|_| opencode_preflight_error("private server returned an invalid endpoint"))?;
    let url = endpoint["url"]
        .as_str()
        .filter(|url| {
            url.strip_prefix("http://127.0.0.1:")
                .and_then(|port| port.parse::<u16>().ok())
                .is_some_and(|port| port != 0)
        })
        .ok_or_else(|| opencode_preflight_error("private server endpoint is not loopback"))?;
    let api = |method: &str, location: bool| -> Result<serde_json::Value, crate::error::RunError> {
        let mut c = process();
        c.args(["api", "--server", url, method]);
        if location {
            c.args(["--param", &format!("location[directory]={directory}")]);
        }
        let out = opencode_probe_output(c, remaining()?.min(Duration::from_secs(15)))?;
        let result: serde_json::Value = serde_json::from_slice(&out).map_err(|_| {
            opencode_preflight_error("native configuration API returned invalid JSON")
        })?;
        if location && result["location"]["directory"].as_str() != Some(directory) {
            return Err(opencode_preflight_error(
                "native configuration API answered for another checkout",
            ));
        }
        Ok(result)
    };
    let info = api("server.info", false)?;
    // The fresh password and owned server's loopback endpoint authenticate
    // the API peer. Also verify kernel group membership: npm and other
    // non-exec shims own the group but start the native server as a child.
    let server_pid = info["pid"]
        .as_u64()
        .and_then(|p| i32::try_from(p).ok())
        .and_then(rustix::process::Pid::from_raw);
    if server_pid.and_then(|p| rustix::process::getpgid(Some(p)).ok())
        != rustix::process::Pid::from_raw(server.0.id() as i32)
    {
        return Err(opencode_preflight_error(
            "native API did not identify the owned server",
        ));
    }
    // Check the actual server too, in case a shim selected another binary.
    if info["version"].as_str() != Some("2.1.0") {
        return Err(opencode_preflight_error(
            "this native configuration API version is not qualified (requires 2.1.0)",
        ));
    }
    // model.list awaits activation, with a native 5-second error timeout.
    // agent.list alone can expose defaults before config has loaded.
    let models = api("model.list", true)?;
    if !models["data"].is_array() {
        return Err(opencode_preflight_error(
            "plugin activation returned an invalid response",
        ));
    }
    let plugins = api("plugin.list", true)?;
    opencode_plugins_ready(&plugins)?;
    let agents = api("agent.list", true)?;
    opencode_build_present(&agents)
}

fn opencode_plugins_ready(value: &serde_json::Value) -> Result<(), crate::error::RunError> {
    let plugins = value["data"]
        .as_array()
        .ok_or_else(|| opencode_preflight_error("plugin inventory is unavailable"))?;
    if plugins.iter().any(|p| p["state"]["status"] != "active")
        || ["opencode.agent", "opencode.config.agent"]
            .iter()
            .any(|id| {
                !plugins
                    .iter()
                    .any(|p| p["id"] == *id && p["source"]["type"] == "builtin")
            })
    {
        return Err(opencode_preflight_error(
            "configuration plugins failed or did not activate",
        ));
    }
    Ok(())
}

fn opencode_build_present(value: &serde_json::Value) -> Result<(), crate::error::RunError> {
    let agents = value["data"]
        .as_array()
        .ok_or_else(|| opencode_preflight_error("resolved agent list is unavailable"))?;
    let builds: Vec<_> = agents.iter().filter(|a| a["id"] == "build").collect();
    if builds.len() != 1 {
        return Err(opencode_preflight_error(
            "build is disabled or absent from OpenCode's resolved agents",
        ));
    }
    if !matches!(
        builds[0]["mode"].as_str(),
        Some("primary" | "subagent" | "all")
    ) || !builds[0]["permissions"].is_array()
    {
        return Err(opencode_preflight_error(
            "resolved build agent has an unsupported shape",
        ));
    }
    Ok(())
}

/// OpenCode's config for a turn, passed inline with
/// `OPENCODE_CONFIG_CONTENT` and merged by OpenCode over the user's own:
/// the role's permissions and the launcher's MCP server.
pub fn opencode_config(t: &TurnInput<'_>) -> serde_json::Value {
    let mut names = mdkg_names(t.mdkg);
    let path = t.mdkg.display().to_string();
    if !names.contains(&path) {
        names.push(path);
    }
    let mut cfg = opencode_permissions(t.role, &names);
    if t.role == Role::Reviewer {
        let off: serde_json::Map<String, serde_json::Value> = MCP_WRITE
            .iter()
            .map(|n| (format!("{MCP_SERVER}_{n}"), false.into()))
            .collect();
        cfg["tools"] = serde_json::Value::Object(off);
    }
    // 2.1 appends all per-agent rules after all global rules, so a user's
    // per-agent allow beats even our inline global deny. Append our limits
    // to the selected agent too. Keep the legacy spelling: 2.1 normalizes
    // it per document, and older OpenCode still accepts this schema.
    // 2.1 drops a disabled build entry before this inline entry recreates
    // it: that entry's settings are lost, while global permissions remain.
    let mut agent = cfg.clone();
    if t.role == Role::Reviewer {
        agent["mode"] = "primary".into();
    }
    cfg["agent"] = serde_json::json!({opencode_agent(t): agent});
    cfg["mcp"] = serde_json::json!({MCP_SERVER: {
        "type": "local",
        "command": std::iter::once(t.mdkg.display().to_string())
            .chain(mcp_args(t))
            .collect::<Vec<_>>(),
        "environment": mcp_env(t),
        "enabled": true,
    }});
    cfg
}

/// Workers add only mdkg semantic refusals to the user's permissions.
/// Reviewers get a read-only allowlist; the last matching rule wins.
pub fn opencode_permissions(role: Role, names: &[String]) -> serde_json::Value {
    let mut bash = serde_json::Map::new();
    if role == Role::Worker {
        for command in mdkg_commands(names, MDKG_DENY) {
            bash.insert(command.clone(), "deny".into());
            bash.insert(format!("{command} *"), "deny".into());
        }
        return serde_json::json!({"permission": {"bash": bash}});
    }
    bash.insert("*".into(), "deny".into());
    let reads = mdkg_commands(names, MDKG_READ);
    let mut commands = REVIEWER_COMMANDS.to_vec();
    commands.extend(reads.iter().map(String::as_str));
    for c in commands {
        bash.insert(c.to_string(), "allow".into());
        bash.insert(format!("{c} *"), "allow".into());
    }
    // After every allow: OpenCode takes the last pattern that matches.
    for c in REVIEWER_OUTPUT_DENY {
        bash.insert(c.to_string(), "deny".into());
    }
    serde_json::json!({
        "permission": {
            "edit": "deny",
            "bash": bash,
            // A child uses its own agent rules, not the parent's limits.
            // Legacy `task` normalizes to `subagent` in OpenCode 2.1.
            "task": "deny",
            "webfetch": "deny",
            // Its own folder only, as a person running it there would.
            "external_directory": "deny",
        }
    })
}

/// OpenCode with the user's own config and providers: `run` on a private
/// server (`--standalone`, so the turn's inline config applies rather than
/// a background service's), the model the user configured unless the
/// agent names one. Workers use the user's build agent settings, not a
/// custom default_agent; reviewers get a dedicated agent for this turn.
fn opencode(t: &TurnInput<'_>) -> AgentCommand {
    let mut args: Vec<String> = vec![
        "run".into(),
        "--standalone".into(),
        "--format".into(),
        "json".into(),
        "--agent".into(),
        opencode_agent(t),
    ];
    if let Some(id) = t.resume {
        args.extend(["--session".into(), id.to_string()]);
    }
    if let Some(m) = t.model {
        args.extend(["--model".into(), m.to_string()]);
    }
    args.push(t.prompt.to_string());
    let mut env = agent_env(t);
    env.push((
        "OPENCODE_CONFIG_CONTENT".into(),
        opencode_config(t).to_string(),
    ));
    if t.role == Role::Reviewer {
        // A reviewer's checkout is the author's: its opencode.json isn't
        // loaded, the user's own config is (RC1 security audit SEC7).
        // OpenCode 2.1 reads OPENCODE_CONFIG_PROJECT_DISABLE first, so both
        // are set, over any value the launcher inherited (review of
        // 4fe6297).
        env.push(("OPENCODE_CONFIG_PROJECT_DISABLE".into(), "1".into()));
        env.push(("OPENCODE_DISABLE_PROJECT_CONFIG".into(), "1".into()));
    }
    AgentCommand {
        program: Harness::OpenCode.program(),
        args,
        cwd: t.cwd.to_path_buf(),
        env,
        prompt_on_stdin: false,
        output_file: None,
        secret_env: Vec::new(),
        codex_hook_binary: None,
        opencode_worker: t.role == Role::Worker,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// mdkg's names for a build installed as `mdkg-next` (beside 0.6.0).
    fn names() -> Vec<String> {
        mdkg_names(Path::new("/opt/bin/mdkg-next"))
    }

    #[test]
    fn an_allowed_alias_of_mdkg_is_denied_abandon_and_auth() {
        // The turn runs /opt/bin/mdkg; the allowlist names other mdkg
        // binaries with a fixed subcommand: each gets abandon and auth
        // denied as it's spelled.
        let allow = [
            "Bash(/opt/x/mdkg-rc status)".to_string(),
            "Bash(mdkg-next validate *)".to_string(),
            "Bash(/opt/rc:1/mdkg-colon validate:*)".to_string(),
            "Bash('/opt/rc builds/mdkg-sp' validate *)".to_string(),
        ];
        let mut t = input(Harness::Claude, Role::Worker, None);
        t.mdkg = Path::new("/opt/bin/mdkg");
        t.allow = &allow;
        let c = command(&t);
        for d in [
            "Bash(/opt/x/mdkg-rc abandon *)",
            "Bash(/opt/x/mdkg-rc auth *)",
            "Bash(mdkg-next abandon *)",
            "Bash(mdkg-next auth *)",
            "Bash(/opt/rc:1/mdkg-colon abandon *)",
            "Bash('/opt/rc builds/mdkg-sp' auth *)",
            "Bash(mdkg abandon *)",
        ] {
            assert!(c.args.iter().any(|a| a == d), "{d}: {:?}", c.args);
        }
        assert!(c.args.iter().any(|a| a == "Bash(mdkg-next validate *)"));
    }

    #[test]
    fn a_dropped_rule_says_what_to_write_instead() {
        // The orchestrator, 2026-10-25: name the rule, and what to allow.
        let fix = |r: &str| dropped_allow_fix(r, Path::new("mdkg")).unwrap_or_default();
        for (rule, want) in [
            ("Bash(mdkg-next *)", "`Bash(mdkg-next validate *)`"),
            ("Bash(mdkg-next:*)", "`Bash(mdkg-next validate *)`"),
            (
                "Bash('/opt/rc builds/mdkg' --root . *)",
                "`Bash('/opt/rc builds/mdkg' validate *)`",
            ),
            ("Bash(mdkg *)", "the mdkg MCP server"),
            ("Bash(mdkg auth key *)", "`auth` is never allowed"),
            ("Bash", "name the command"),
            ("Bash(*)", "name the command"),
            ("Bash('mdkg *)", "fix its quoting"),
            ("Bash(/opt/bin/\\\\mdkg *)", "quote the path instead"),
            (
                "Bash($'mdkg' --root . *)",
                "write the command as a plain word",
            ),
            ("Read,Bash(npm test *)", "its own `--allow`"),
        ] {
            assert!(fix(rule).contains(want), "{rule}: {}", fix(rule));
        }
        for passed in ["Bash(mdkg validate *)", "Bash(npm test *)", "Read"] {
            assert_eq!(
                dropped_allow_fix(passed, Path::new("mdkg")),
                None,
                "{passed}"
            );
        }
    }

    #[test]
    fn an_mdkg_rule_with_open_arguments_never_reaches_claude() {
        // Each admits abandon or auth past any deny written after the
        // binary (Tier 1 round 1 of 53a11c9): never passed on.
        let open = [
            "Bash(mdkg-next *)",
            "Bash(mdkg-next:*)",
            "Bash(/opt/y/mdkg-path:*)",
            "Bash('/opt/rc builds/mdkg-next' *)",
            "Bash(/opt/bin/mdkg:* *)",
            "Bash('/opt/rc builds/mdkg-next' --root . *)",
            "Bash(mdkg-next abandon *)",
            "Bash(mdkg-next auth key *)",
            "Read,Bash(/opt/bin/mdkg-rc *)",
            // A list after a first rule with its own `(…)` (round 2).
            "Read(*),Bash(*)",
            "Read(*) Bash(*)",
            "Bash(npm test *),Bash(mdkg-next *)",
            "Bash(npm test *) Bash(*)",
            "Read(a(b)),Bash(*)",
            // Claude's `\\` decoding, and case (round 3).
            "Bash(/opt/bin/\\\\mdkg --root . *)",
            "Bash(/opt/bin/\\mdkg --root . *)",
            "Bash(/opt/bin/MDKG --root . *)",
            "Bash(MdKg-next *)",
            "Bash(/opt/bin/md\u{212A}g --root . *)",
            // Shell syntax in the command word (round 4).
            "Bash($'mdkg-next' --root . auth key anthropic)",
            "Bash($\"mdkg-next\" --root . *)",
            "Bash(<x mdkg --root . *)",
            "Bash(X=1 mdkg --root . *)",
            "Bash(`echo mdkg` --root . *)",
            "Bash",
            "Bash(*)",
            "Bash(mdk? *)",
        ];
        let allow: Vec<String> = open
            .iter()
            .map(|r| r.to_string())
            .chain(["Bash(npm test *)".to_string(), "Read".to_string()])
            .collect();
        let mdkg = Path::new("/opt/bin/mdkg");
        assert_eq!(unreadable_allow(&allow, mdkg), open.map(String::from));
        let dropped: Vec<String> = dropped_allow(&allow, mdkg)
            .into_iter()
            .map(|d| d.rule)
            .collect();
        assert_eq!(dropped, open.map(String::from));
        let mut t = input(Harness::Claude, Role::Worker, None);
        t.mdkg = mdkg;
        t.allow = &allow;
        let c = command(&t);
        let start = c
            .args
            .iter()
            .position(|a| a == "--allowedTools")
            .expect("allow");
        let end = c
            .args
            .iter()
            .position(|a| a == "--disallowedTools")
            .expect("deny");
        let allowed = &c.args[start + 1..end];
        for r in open {
            assert!(
                !allowed.iter().any(|a| a == r),
                "{r} reached Claude: {allowed:?}"
            );
        }
        assert!(
            allowed.iter().any(|a| a == "Bash(npm test *)"),
            "{allowed:?}"
        );
    }

    #[test]
    fn rules_that_run_other_code_pass_by_design() {
        // The denies stop an agent running mdkg's abandon
        // or auth directly. A worker's native permissions can run
        // any code, so a rule for a command that runs
        // other code is the user's call, and what follows the command word,
        // operators included, is Claude's to split.
        let mdkg = Path::new("/opt/bin/mdkg");
        for rule in [
            "Bash(git add .mdkg/work/*)",
            "Bash(cat .mdkg/README.md)",
            "Bash(grep -r mdkg *)",
            "Bash(env mdkg-next *)",
            "Bash(sh -c 'mdkg --root . auth key x')",
            "Bash(npm test && mdkg --root . *)",
            "Bash(exec mdkg --root . *)",
            "Bash(/usr/bin/./security find-generic-password *)",
            "Bash(SECURITY *)",
            "Bash('/usr/bin/security' dump-keychain)",
        ] {
            assert!(passable_allow(rule, mdkg), "{rule}");
        }
    }

    #[test]
    fn a_configured_name_with_a_backslash_is_never_allowed_by_name() {
        // Open 2 of the pass at the cap: `--binary '/opt/bin/gr\aphctl'`.
        // Any rule naming it directly spells a backslash, which the reader
        // refuses, so none reaches Claude.
        let mdkg = Path::new("/opt/bin/gr\\aphctl");
        for rule in [
            "Bash('/opt/bin/gr\\aphctl' --root . *)",
            "Bash(/opt/bin/gr\\\\aphctl --root . *)",
            "Bash(\"gr\\aphctl\":*)",
        ] {
            assert!(!passable_allow(rule, mdkg), "{rule}");
        }
    }

    #[test]
    fn the_configured_binary_is_mdkg_whatever_its_name() {
        // `--binary /opt/bin/graphctl` (Tier 1 round 2 of 53a11c9): an open
        // rule for it would admit `graphctl --root . abandon`.
        let mdkg = Path::new("/opt/bin/graphctl");
        let allow: Vec<String> = [
            "Bash(/opt/bin/graphctl *)",
            "Bash(graphctl:*)",
            "Bash(graphctl --root . *)",
            "Bash(/opt/bin/GRAPHCTL --root . *)",
            "Bash(graphctl validate *)",
        ]
        .map(String::from)
        .to_vec();
        assert_eq!(unreadable_allow(&allow, mdkg), allow[..4]);
        let fix = dropped_allow_fix(&allow[0], mdkg).unwrap_or_default();
        assert!(
            fix.contains("`Bash(/opt/bin/graphctl validate *)`"),
            "{fix}"
        );
        let mut t = input(Harness::Claude, Role::Worker, None);
        t.mdkg = mdkg;
        t.allow = &allow;
        let c = command(&t);
        let deny = c
            .args
            .iter()
            .position(|a| a == "--disallowedTools")
            .expect("deny");
        let (allowed, denied) = c.args.split_at(deny);
        for r in &allow[..4] {
            assert!(!allowed.contains(r), "{r} reached Claude: {allowed:?}");
        }
        assert!(allowed.contains(&allow[4]), "{allowed:?}");
        assert!(
            denied.iter().any(|a| a == "Bash(graphctl abandon *)"),
            "{denied:?}"
        );
    }

    #[test]
    fn allow_rules_are_read_as_shell_words() {
        let read = allow_command;
        let some = |c: &str, s: &str| Ok(Some((c.to_string(), s.to_string())));
        assert_eq!(read("Read"), Ok(None));
        assert_eq!(read("Bash(mdkg-next *)"), some("mdkg-next", "mdkg-next"));
        assert_eq!(read("Bash(mdkg-next:*)"), some("mdkg-next", "mdkg-next"));
        assert_eq!(
            read("Bash(/opt/rc:1/mdkg *)"),
            some("/opt/rc:1/mdkg", "/opt/rc:1/mdkg")
        );
        assert_eq!(
            read("Bash('/opt/rc builds/mdkg-next' *)"),
            some("/opt/rc builds/mdkg-next", "'/opt/rc builds/mdkg-next'")
        );
        assert_eq!(
            read("Bash(\"/opt/rc builds/mdkg-next\":*)"),
            some("/opt/rc builds/mdkg-next", "\"/opt/rc builds/mdkg-next\"")
        );
        assert_eq!(
            read("Bash(/opt/rc\\ builds/mdkg-next *)"),
            some("/opt/rc builds/mdkg-next", "/opt/rc\\ builds/mdkg-next")
        );
        // A quoted colon is the path's own, not the prefix form's.
        assert_eq!(read("Bash('mdkg:*')"), some("mdkg:*", "'mdkg:*'"));
        for bad in [
            "Bash('/opt/rc builds/mdkg-next *)",
            "Bash(\"mdkg-next *)",
            "Bash(mdkg-next \"abandon *)",
            "Bash(mdkg-next \\)",
            "Bash(mdkg-next *",
            "Bash()",
            "Bash(  )",
        ] {
            assert_eq!(read(bad), Err(Unreadable), "{bad}");
        }
    }

    #[test]
    fn an_unreadable_allow_rule_never_reaches_claude() {
        let allow = [
            "Bash(npm test *)".to_string(),
            "Bash('/opt/rc builds/mdkg-next validate *)".to_string(),
            "Bash('/opt/rc builds/mdkg-next' validate *)".to_string(),
        ];
        assert_eq!(
            unreadable_allow(&allow, Path::new("mdkg")),
            [allow[1].clone()]
        );
        let mut t = input(Harness::Claude, Role::Worker, None);
        t.allow = &allow;
        let c = command(&t);
        let all = c.args.join("\n");
        assert!(
            !all.contains("'/opt/rc builds/mdkg-next validate *)"),
            "{all}"
        );
        assert!(all.contains("Bash(npm test *)"), "{all}");
        // The readable quoted alias: abandon and auth denied as spelled.
        for d in [
            "Bash('/opt/rc builds/mdkg-next' abandon *)",
            "Bash('/opt/rc builds/mdkg-next' auth *)",
        ] {
            assert!(all.contains(d), "{d}: {all}");
        }
    }

    #[test]
    fn mdkgs_names_come_from_the_binary_it_runs() {
        assert_eq!(mdkg_names(Path::new("/bin/mdkg")), ["mdkg"]);
        assert_eq!(
            mdkg_names(Path::new("/opt/bin/mdkg-next")),
            ["mdkg", "mdkg-next"]
        );
        // A build under any other name: abandon is denied under it too.
        let (_, deny) = claude_rules(Role::Worker, &[], &mdkg_names(Path::new("/x/mdkg-rc")));
        assert!(
            deny.contains(&"Bash(mdkg-rc abandon *)".to_string()),
            "{deny:?}"
        );
        assert!(
            deny.contains(&"Bash(mdkg abandon *)".to_string()),
            "{deny:?}"
        );
    }

    fn input<'a>(h: Harness, r: Role, resume: Option<&'a str>) -> TurnInput<'a> {
        TurnInput {
            agent: "lane",
            mdkg: Path::new("/bin/mdkg"),
            home: Some(Path::new("/Users/u")),
            common_dir: Some(Path::new("/repo/.git")),
            trusted_worktree: Some(Path::new("/repo")),
            graph_root: None,
            main_ref: "main",
            xdg_config: None,
            harness: h,
            role: r,
            cwd: Path::new("/work/lane"),
            model: None,
            effort: None,
            sandbox: None,
            approvals: None,
            resume,
            new_session: "0199a0a0-0000-7000-8000-000000000001",
            allow: &[],
            output_file: Path::new("/run/a/turn-1.reply"),
            prompt: "the digest",
            api_key: None,
        }
    }

    fn has_pair(args: &[String], flag: &str, value: &str) -> bool {
        args.windows(2).any(|w| w[0] == flag && w[1] == value)
    }

    fn json_after(args: &[String], flag: &str) -> serde_json::Value {
        let i = args.iter().position(|a| a == flag).expect(flag);
        serde_json::from_str(&args[i + 1]).expect("inline json")
    }

    #[test]
    fn workers_leave_command_policy_and_tools_to_native_settings() {
        for resume in [None, Some("existing-session")] {
            let claude = command(&input(Harness::Claude, Role::Worker, resume));
            for absent in [
                "--allowedTools",
                "--tools",
                "--setting-sources",
                "--permission-prompts",
            ] {
                assert!(
                    !claude.args.iter().any(|arg| arg == absent),
                    "{absent}: {:?}",
                    claude.args
                );
            }
            let deny = claude
                .args
                .iter()
                .position(|arg| arg == "--disallowedTools")
                .expect("semantic refusals");
            let denied = &claude.args[deny + 1..];
            assert!(!denied.is_empty());
            assert!(
                denied
                    .iter()
                    .all(|rule| rule.contains(" auth") || rule.contains(" abandon")),
                "{denied:?}"
            );

            let worker = command(&input(Harness::OpenCode, Role::Worker, resume));
            let cfg: serde_json::Value = serde_json::from_str(
                &worker
                    .env
                    .iter()
                    .find(|(name, _)| name == "OPENCODE_CONFIG_CONTENT")
                    .expect("config")
                    .1,
            )
            .expect("json");
            let permissions = cfg["permission"].as_object().expect("semantic refusals");
            assert_eq!(permissions.len(), 1, "{cfg}");
            let bash = permissions["bash"].as_object().expect("bash refusals");
            assert!(!bash.is_empty());
            assert!(
                bash.iter().all(|(rule, value)| (rule.contains(" auth")
                    || rule.contains(" abandon"))
                    && value == "deny"),
                "{cfg}"
            );
            assert!(cfg.get("tools").is_none(), "{cfg}");
            assert_eq!(cfg["mcp"]["mdkg"]["enabled"], true);
        }

        let allow = ["Bash(git push *)", "Bash(security *)"].map(String::from);
        let mut t = input(Harness::Claude, Role::Worker, None);
        t.allow = &allow;
        let claude = command(&t);
        for rule in &allow {
            assert!(passable_allow(rule, t.mdkg), "{rule}");
            assert!(claude.args.contains(rule), "{rule}: {:?}", claude.args);
        }
        assert!(dropped_allow(&allow, t.mdkg).is_empty());
    }

    #[test]
    fn claude_starts_then_resumes_a_session_headless_with_the_launchers_server() {
        let first = command(&input(Harness::Claude, Role::Worker, None));
        assert!(has_pair(
            &first.args,
            "--session-id",
            "0199a0a0-0000-7000-8000-000000000001"
        ));
        assert!(!first.args.iter().any(|arg| arg == "--permission-prompts"));
        assert!(has_pair(&first.args, "--permission-mode", "auto"));
        assert!(has_pair(&first.args, "--output-format", "stream-json"));
        let cfg = json_after(&first.args, "--mcp-config");
        let env = &cfg["mcpServers"]["mdkg"]["env"];
        assert_eq!(env["MDKG_ACTOR"], "lane");
        assert_eq!(env["MDKG_TRUSTED_COMMON_DIR"], "/repo/.git");
        assert_eq!(env["MDKG_TRUSTED_WORKTREE"], "/repo");
        assert_eq!(env["HOME"], "/Users/u");
        assert_eq!(env["XDG_CONFIG_HOME"], "/Users/u/.config");
        assert_eq!(env["MDKG_MAIN_REF"], "main");
        assert_eq!(env["MDKG_RUN_TURN"], "0199a0a0-0000-7000-8000-000000000001");
        let next = command(&input(Harness::Claude, Role::Worker, Some("s-1")));
        assert!(has_pair(&next.args, "--resume", "s-1"));
        assert!(!next.args.contains(&"--session-id".to_string()));
        assert!(first.prompt_on_stdin);
    }

    #[test]
    fn claude_runs_in_the_users_own_setup_with_no_sandbox_of_the_launchers() {
        let c = command(&input(Harness::Claude, Role::Worker, None));
        // The user's settings, hooks and MCP servers load as usual.
        for gone in [
            "--restricted",
            "--strict-mcp-config",
            "--settings",
            "--bare",
        ] {
            assert!(!c.args.contains(&gone.to_string()), "{gone}");
        }
        // Parent launcher pins and binary context are replaced, not inherited.
        assert!(
            c.env
                .iter()
                .all(|(k, _)| k == SHELL_BINARY || !NOT_INHERITED.contains(&k.as_str()))
        );
    }

    #[test]
    fn a_claude_reviewer_has_no_write_tools_and_no_slash_commands() {
        let c = command(&input(Harness::Claude, Role::Reviewer, None));
        assert!(has_pair(&c.args, "--tools", "Read,Glob,Grep,Bash"));
        assert!(has_pair(&c.args, "--permission-mode", "manual"));
        assert!(c.args.contains(&"--disable-slash-commands".to_string()));
        // Only the user's settings: none of the reviewed checkout's.
        assert!(has_pair(&c.args, "--setting-sources", "user"));
        let w = command(&input(Harness::Claude, Role::Worker, None));
        assert!(!w.args.contains(&"--setting-sources".to_string()));
        let (allow, deny) = claude_rules(Role::Reviewer, &["Bash(npm test *)".into()], &names());
        for d in ["Edit", "Write", "NotebookEdit", "mcp__mdkg__lease_claim"] {
            assert!(deny.contains(&d.to_string()), "{d}");
        }
        assert!(!allow.contains(&"Bash(npm test *)".to_string()));
        let (allow, _) = claude_rules(Role::Worker, &["Bash(npm test *)".into()], &names());
        assert!(allow.contains(&"Bash(npm test *)".to_string()));
        assert_eq!(allow, ["Bash(npm test *)"]);
    }

    #[test]
    fn only_reviewers_deny_pushing_publishing_and_keychain_commands() {
        let (_, worker) = claude_rules(Role::Worker, &[], &names());
        let (_, reviewer) = claude_rules(Role::Reviewer, &[], &names());
        for d in [
            "Bash(git push)",
            "Bash(git push *)",
            "Bash(cargo publish *)",
            "Bash(security *)",
        ] {
            assert!(reviewer.iter().any(|rule| rule == d), "{d}");
            assert!(!worker.iter().any(|rule| rule == d), "{d}");
        }
        for role in [Role::Worker, Role::Reviewer] {
            let (_, deny) = claude_rules(role, &[], &names());
            for d in [
                "Bash(mdkg auth)",
                "Bash(mdkg auth *)",
                "Bash(mdkg-next auth *)",
                "Bash(mdkg abandon *)",
                "Bash(mdkg-next abandon *)",
            ] {
                assert!(deny.contains(&d.to_string()), "{role:?} {d}");
            }
        }
    }

    #[test]
    fn a_broad_allow_never_lets_a_worker_abandon_work() {
        // An allowlist entry for the whole CLI, under either installed name,
        // still leaves abandon denied: Claude applies deny rules first.
        let allow = ["Bash(mdkg-next *)".to_string(), "Bash(mdkg *)".to_string()];
        let (allowed, deny) = claude_rules(Role::Worker, &allow, &names());
        assert!(
            allowed.contains(&"Bash(mdkg-next *)".to_string()),
            "{allowed:?}"
        );
        for d in ["Bash(mdkg-next abandon *)", "Bash(mdkg abandon *)"] {
            assert!(deny.contains(&d.to_string()), "{d}: {deny:?}");
        }
        // And by the path this turn names mdkg by.
        let c = command(&input(Harness::Claude, Role::Worker, None));
        assert!(
            c.args.contains(&"Bash('/bin/mdkg' abandon *)".to_string()),
            "{:?}",
            c.args
        );
    }

    #[test]
    fn every_agent_carries_the_turns_mark_and_its_actor() {
        for h in [Harness::Claude, Harness::Codex, Harness::OpenCode] {
            let c = command(&input(h, Role::Worker, None));
            let env = |k: &str| c.env.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
            assert_eq!(env(TURN_MARK), Some("0199a0a0-0000-7000-8000-000000000001"));
            assert_eq!(env("MDKG_ACTOR"), Some("lane"));
        }
    }

    #[test]
    fn every_turn_prepends_the_actual_launcher_folder_to_path() {
        let launcher = std::env::current_exe()
            .expect("launcher")
            .canonicalize()
            .expect("real launcher");
        let folder = launcher.parent().expect("launcher folder");
        for h in [Harness::Claude, Harness::Codex, Harness::OpenCode] {
            for role in [Role::Worker, Role::Reviewer] {
                for resume in [None, Some("session")] {
                    let c = command(&input(h, role, resume));
                    let path = &c
                        .env
                        .iter()
                        .find(|(key, _)| key == "PATH")
                        .expect("turn PATH")
                        .1;
                    assert_eq!(std::env::split_paths(path).next().as_deref(), Some(folder));
                    assert_eq!(c.env.iter().filter(|(key, _)| key == "PATH").count(), 1);
                }
            }
        }
    }

    #[test]
    fn exact_mdkg_binary_survives_an_older_login_shell_path() {
        let fixture = PathFixture::new();
        let expected = fixture.executable("new \"quoted\" bin/mdkg-next", "new-build");
        let old = fixture.executable("old bin/mdkg", "old-build");
        let profile = fixture.0.join("login-profile");
        std::fs::write(
            &profile,
            format!(
                "export PATH={}:/usr/bin:/bin\n",
                shell_word(&old.parent().expect("folder").display().to_string())
            ),
        )
        .expect("profile");
        for harness in [Harness::Claude, Harness::Codex, Harness::OpenCode] {
            for role in [Role::Worker, Role::Reviewer] {
                for resume in [None, Some("existing-session")] {
                    let mut t = input(harness, role, resume);
                    t.mdkg = &expected;
                    t.cwd = &fixture.0;
                    let c = command_with_path(
                        &t,
                        Some(std::ffi::OsStr::new("/usr/bin:/bin")),
                        Some(&expected),
                    );
                    assert_eq!(
                        c.env
                            .iter()
                            .find(|(key, _)| key == SHELL_BINARY)
                            .map(|(_, value)| value.as_str()),
                        expected.to_str()
                    );
                    if harness == Harness::Codex {
                        let setting = c
                            .args
                            .iter()
                            .find(|value| {
                                value.starts_with("shell_environment_policy.set.MDKG_BIN=")
                            })
                            .expect("native shell environment value");
                        let document: toml_edit::DocumentMut =
                            setting.parse().expect("valid TOML override");
                        assert_eq!(
                            document["shell_environment_policy"]["set"]["MDKG_BIN"].as_str(),
                            expected.to_str()
                        );
                        assert!(!c.args.iter().any(|value| {
                            value.starts_with("shell_environment_policy.inherit=")
                                || value.starts_with("shell_environment_policy.include_only=")
                        }));
                    }
                    // A real login shell, sourcing only our fixture profile.
                    // Never read the account's shell startup files.
                    let result = std::process::Command::new("/bin/bash")
                        .args([
                            "--noprofile",
                            "--norc",
                            "-lc",
                            ". \"$MDKG_TEST_PROFILE\"; mdkg --version; \"$MDKG_BIN\" --version",
                        ])
                        .current_dir(&fixture.0)
                        // Noninteractive Bash reads BASH_ENV even with startup
                        // profiles disabled; inherit no account startup state.
                        .env_clear()
                        .env("HOME", &fixture.0)
                        .env("MDKG_TEST_PROFILE", &profile)
                        .envs(c.env.iter().map(|(key, value)| (key, value)))
                        .output()
                        .expect("login shell");
                    assert!(
                        result.status.success(),
                        "{harness:?} {role:?} {resume:?}: {result:?}"
                    );
                    assert_eq!(
                        result.stdout, b"old-buildnew-build",
                        "{harness:?} {role:?} {resume:?}"
                    );
                }
            }
        }
    }

    struct PathFixture(PathBuf);

    impl PathFixture {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("mdkg-path-{}", uuid::Uuid::now_v7()));
            std::fs::create_dir_all(&dir).expect("PATH fixture");
            Self(dir)
        }

        fn executable(&self, path: &str, output: &str) -> PathBuf {
            use std::os::unix::fs::PermissionsExt;
            let path = self.0.join(path);
            std::fs::create_dir_all(path.parent().expect("executable parent"))
                .expect("executable folder");
            std::fs::write(&path, format!("#!/bin/sh\nprintf '%s' '{output}'\n"))
                .expect("executable");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("executable permissions");
            path
        }
    }

    impl Drop for PathFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn configured_mcp(command: &AgentCommand, harness: Harness) -> String {
        match harness {
            Harness::Claude => {
                json_after(&command.args, "--mcp-config")["mcpServers"]["mdkg"]["command"]
                    .as_str()
                    .expect("Claude MCP program")
                    .into()
            }
            Harness::Codex => {
                let value = command
                    .args
                    .iter()
                    .find_map(|a| a.strip_prefix("mcp_servers.mdkg.command="))
                    .expect("Codex MCP program");
                serde_json::from_str(value).expect("Codex program string")
            }
            Harness::OpenCode => {
                let cfg: serde_json::Value = serde_json::from_str(
                    &command
                        .env
                        .iter()
                        .find(|(k, _)| k == "OPENCODE_CONFIG_CONTENT")
                        .expect("OpenCode config")
                        .1,
                )
                .expect("OpenCode JSON");
                cfg["mcp"]["mdkg"]["command"][0]
                    .as_str()
                    .expect("OpenCode MCP program")
                    .into()
            }
        }
    }

    #[test]
    fn mcp_resolution_uses_original_path_for_every_harness_and_role() {
        let fixture = PathFixture::new();
        let original = fixture.executable("original bin/mdkg", "original");
        let launcher = fixture.executable("launcher/mdkg", "launcher");
        let path = original.parent().expect("original folder").as_os_str();
        for harness in [Harness::Claude, Harness::Codex, Harness::OpenCode] {
            for role in [Role::Worker, Role::Reviewer] {
                for resume in [None, Some("session")] {
                    let mut t = input(harness, role, resume);
                    t.mdkg = Path::new("mdkg");
                    t.cwd = &fixture.0;
                    let c = command_with_path(&t, Some(path), Some(&launcher));
                    assert_eq!(configured_mcp(&c, harness), original.to_string_lossy());
                    assert_eq!(
                        c.env
                            .iter()
                            .find(|(key, _)| key == SHELL_BINARY)
                            .map(|(_, value)| value.as_str()),
                        original.to_str()
                    );
                    let mut process = std::process::Command::new(configured_mcp(&c, harness));
                    process.current_dir(&fixture.0);
                    process.envs(c.env);
                    let output = process.output().expect("configured MCP executable");
                    assert!(output.status.success());
                    assert_eq!(output.stdout, b"original");
                }
            }
        }
    }

    #[test]
    fn equivalent_mcp_path_selections_keep_the_bare_name_in_every_harness() {
        use std::os::unix::fs::symlink;
        for kind in ["unshadowed", "symlink", "hardlink", "npm"] {
            let fixture = PathFixture::new();
            let launcher = fixture.executable(
                if kind == "unshadowed" {
                    "launcher/graphctl"
                } else if kind == "npm" {
                    "package/node_modules/.bin_real/mdkg"
                } else {
                    "launcher/mdkg"
                },
                "original",
            );
            let original = fixture.0.join("original bin/mdkg");
            std::fs::create_dir_all(original.parent().expect("original folder"))
                .expect("original folder");
            match kind {
                "unshadowed" => {
                    fixture.executable("original bin/mdkg", "original");
                }
                "symlink" => symlink(&launcher, &original).expect("MCP link"),
                "hardlink" => std::fs::hard_link(&launcher, &original).expect("MCP hardlink"),
                "npm" => {
                    let wrapper = fixture.executable("package/run-mdkg.js", "unused");
                    std::fs::write(&wrapper,
                        "#!/usr/bin/env node\nconst { run } = require(\"./binary\");\nrun(\"mdkg\");\n")
                        .expect("npm launcher body");
                    symlink(wrapper, &original).expect("npm bin link");
                }
                _ => unreachable!(),
            }
            let path = original.parent().expect("original folder").as_os_str();
            for harness in [Harness::Claude, Harness::Codex, Harness::OpenCode] {
                for role in [Role::Worker, Role::Reviewer] {
                    for resume in [None, Some("session")] {
                        let mut t = input(harness, role, resume);
                        t.mdkg = Path::new("mdkg");
                        t.cwd = &fixture.0;
                        let c = command_with_path(&t, Some(path), Some(&launcher));
                        assert_eq!(configured_mcp(&c, harness), "mdkg", "{kind}");
                        assert_eq!(
                            c.env
                                .iter()
                                .find(|(key, _)| key == SHELL_BINARY)
                                .map(|(_, value)| value.as_str()),
                            original.to_str(),
                            "wrapper spelling: {kind}"
                        );
                        let output = std::process::Command::new(configured_mcp(&c, harness))
                            .current_dir(&fixture.0)
                            .envs(c.env)
                            .output()
                            .expect("configured bare MCP");
                        assert!(output.status.success(), "{kind}: {output:?}");
                        assert_eq!(output.stdout, b"original", "{kind}");
                    }
                }
            }
        }
    }

    #[test]
    fn mcp_search_preserves_path_order_relative_entries_and_symlink_spelling() {
        use std::ffi::OsStr;
        use std::os::unix::fs::PermissionsExt;
        let fixture = PathFixture::new();
        let blocked = fixture.executable("blocked/mdkg", "blocked");
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o644))
            .expect("nonexecutable candidate");
        std::fs::create_dir_all(fixture.0.join("directory/mdkg")).expect("directory candidate");
        let executable = fixture.executable("real/mdkg", "original");
        std::fs::create_dir(fixture.0.join("wrapper")).expect("wrapper folder");
        let link = fixture.0.join("wrapper/mdkg");
        std::os::unix::fs::symlink(&executable, &link).expect("MCP link");
        let cwd = fixture.0.canonicalize().expect("fixture cwd");
        let path = OsStr::new("blocked:directory:wrapper:real");
        assert_eq!(
            resolve_mcp(Path::new("mdkg"), &cwd, Some(path)),
            Some(cwd.join("wrapper/mdkg"))
        );
        let local = fixture.executable("mdkg", "cwd");
        assert_eq!(
            resolve_mcp(Path::new("mdkg"), &cwd, Some(OsStr::new(":real"))),
            Some(cwd.join("mdkg"))
        );
        assert_eq!(
            resolve_mcp(Path::new("./mdkg"), &cwd, Some(path)),
            Some(cwd.join("./mdkg"))
        );
        assert_eq!(resolve_mcp(&local, &cwd, Some(path)), Some(local));
    }

    #[test]
    fn unresolved_mcp_keeps_inherited_path_instead_of_selecting_the_launcher() {
        let fixture = PathFixture::new();
        let launcher = fixture.executable("launcher/mdkg", "wrong");
        let original = fixture.0.join("empty");
        std::fs::create_dir(&original).expect("empty PATH folder");
        for harness in [Harness::Claude, Harness::Codex, Harness::OpenCode] {
            let mut t = input(harness, Role::Worker, None);
            t.mdkg = Path::new("mdkg");
            t.cwd = &fixture.0;
            let c = command_with_path(&t, Some(original.as_os_str()), Some(&launcher));
            assert_eq!(configured_mcp(&c, harness), "mdkg");
            assert!(
                c.env
                    .iter()
                    .all(|(key, _)| key != "PATH" && key != SHELL_BINARY)
            );
            assert!(NOT_INHERITED.contains(&SHELL_BINARY));
            if harness == Harness::Codex {
                assert!(
                    c.args
                        .iter()
                        .any(|value| value == "shell_environment_policy.set.MDKG_BIN=\"\"")
                );
            }
            let error = std::process::Command::new(configured_mcp(&c, harness))
                .current_dir(&fixture.0)
                .env("PATH", &original)
                .envs(c.env)
                .output()
                .expect_err("unresolved MCP must remain unresolved");
            assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        }
    }

    #[test]
    fn non_utf8_mcp_path_retains_the_bare_command_and_original_path() {
        use std::os::unix::ffi::OsStringExt;
        let fixture = PathFixture::new();
        let original = fixture
            .0
            .join(std::ffi::OsString::from_vec(b"bin-\xff".to_vec()));
        let launcher = fixture.executable("launcher/mdkg", "wrong");
        for harness in [Harness::Claude, Harness::Codex, Harness::OpenCode] {
            let mut t = input(harness, Role::Worker, None);
            t.mdkg = Path::new("mdkg");
            t.cwd = &fixture.0;
            // This host rejects such directory names; exercise the
            // resolved-path serialization boundary without creating one.
            let c = command_with_resolved_mcp(
                &t,
                Some(original.as_os_str()),
                Some(&launcher),
                Some(original.join("mdkg")),
            );
            assert_eq!(configured_mcp(&c, harness), "mdkg");
            assert!(c.env.iter().all(|(key, _)| key != "PATH"));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn non_utf8_mcp_directory_keeps_native_executable_lookup() {
        use std::os::unix::ffi::OsStringExt;
        let fixture = PathFixture::new();
        let executable = fixture.executable("source/mdkg", "original");
        let original = fixture
            .0
            .join(std::ffi::OsString::from_vec(b"bin-\xff".to_vec()));
        std::fs::create_dir(&original).expect("non-UTF-8 PATH folder");
        std::fs::copy(&executable, original.join("mdkg")).expect("non-UTF-8 MCP path");
        let launcher = fixture.executable("launcher/mdkg", "wrong");
        for harness in [Harness::Claude, Harness::Codex, Harness::OpenCode] {
            let mut t = input(harness, Role::Worker, None);
            t.mdkg = Path::new("mdkg");
            t.cwd = &fixture.0;
            let c = command_with_path(&t, Some(original.as_os_str()), Some(&launcher));
            assert_eq!(configured_mcp(&c, harness), "mdkg");
            assert!(c.env.iter().all(|(key, _)| key != "PATH"));
            let output = std::process::Command::new(configured_mcp(&c, harness))
                .current_dir(&fixture.0)
                .env("PATH", &original)
                .envs(c.env)
                .output()
                .expect("MCP found by original non-UTF-8 PATH");
            assert!(output.status.success());
            assert_eq!(output.stdout, b"original");
        }
    }

    #[test]
    fn symlinked_launcher_prepends_real_install_folder() {
        let fixture = PathFixture::new();
        let target = fixture.executable("real install/mdkg", "launcher");
        std::fs::create_dir(fixture.0.join("links")).expect("link folder");
        let link = fixture.0.join("links/mdkg");
        std::os::unix::fs::symlink(&target, &link).expect("launcher link");
        let c = command_with_path(
            &input(Harness::Codex, Role::Worker, None),
            Some(std::ffi::OsStr::new("/usr/bin:/bin")),
            Some(&link),
        );
        let path = &c.env.iter().find(|(k, _)| k == "PATH").expect("PATH").1;
        let target = target.canonicalize().expect("real install");
        assert_eq!(
            std::env::split_paths(path).next().as_deref(),
            target.parent()
        );
    }

    #[test]
    fn launcher_path_preserves_user_entries_and_handles_an_absent_path() {
        use std::ffi::OsStr;
        let folder = Path::new("/opt/my tools");
        assert_eq!(
            launcher_path(folder, Some(OsStr::new("/custom/bin:/usr/bin:/bin"))).as_deref(),
            Some("/opt/my tools:/custom/bin:/usr/bin:/bin")
        );
        assert_eq!(
            launcher_path(folder, Some(OsStr::new(""))).as_deref(),
            Some("/opt/my tools:")
        );
        assert_eq!(
            launcher_path(folder, None).as_deref(),
            Some("/opt/my tools:/usr/bin:/bin")
        );
        assert_eq!(
            launcher_path(Path::new("/unrepresentable:folder"), None),
            None
        );
    }

    #[test]
    fn codex_exposes_read_only_scratch_path_to_both_roles_on_new_and_resumed_turns() {
        for role in [Role::Worker, Role::Reviewer] {
            for resume in [None, Some("t-9")] {
                let c = command(&input(Harness::Codex, role, resume));
                let enabled = c
                    .args
                    .iter()
                    .find_map(|arg| arg.strip_prefix("mcp_servers.mdkg.enabled_tools="))
                    .expect("Codex enabled tools configuration");
                let tools: Vec<String> =
                    serde_json::from_str(enabled).expect("enabled tools array");
                assert!(
                    tools.iter().any(|tool| tool == "scratch_path"),
                    "{role:?}, resume {resume:?}: {tools:?}"
                );
                assert!(has_pair(
                    &c.args,
                    "-c",
                    "mcp_servers.mdkg.tools.scratch_path.approval_mode=\"approve\""
                ));
            }
        }
    }

    #[test]
    fn codex_states_its_own_sandbox_for_the_role_on_a_new_run_and_on_resume() {
        let first = command(&input(Harness::Codex, Role::Reviewer, None));
        assert!(has_pair(&first.args, "-s", "read-only"));
        assert!(has_pair(&first.args, "-C", "/work/lane"));
        assert!(has_pair(&first.args, "-c", "project_doc_max_bytes=0"));
        assert!(has_pair(&first.args, "-c", "approval_policy=\"never\""));
        assert!(has_pair(&first.args, "-o", "/run/a/turn-1.reply"));
        assert!(has_pair(
            &first.args,
            "-c",
            "mcp_servers.mdkg.command=\"/bin/mdkg\""
        ));
        assert!(has_pair(
            &first.args,
            "-c",
            "mcp_servers.mdkg.enabled_tools=[\"room_read\",\"inbox\",\"show\",\"list\",\"search\",\"pack\",\"next\",\"scratch_path\"]"
        ));
        // Each tool the role may call is approved for Codex, and only those.
        let approved = |c: &AgentCommand| -> Vec<String> {
            c.args
                .iter()
                .filter_map(|a| {
                    a.strip_prefix("mcp_servers.mdkg.tools.")?
                        .strip_suffix(".approval_mode=\"approve\"")
                        .map(String::from)
                })
                .collect()
        };
        assert_eq!(approved(&first), mcp_tools(Role::Reviewer));
        let worker = command(&input(Harness::Codex, Role::Worker, None));
        assert!(approved(&worker).contains(&"ack".to_string()));
        assert_eq!(approved(&worker), mcp_tools(Role::Worker));
        // The user's own CODEX_HOME, but not their config: it trusts the
        // checkout, whose project config would load with it.
        assert!(first.args.contains(&"--ignore-user-config".to_string()));
        assert!(!first.args.contains(&"--ignore-rules".to_string()));
        assert!(!first.env.iter().any(|(k, _)| k == "CODEX_HOME"));
        assert_eq!(
            first
                .args
                .iter()
                .filter(|a| a.starts_with("shell_environment_policy"))
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["shell_environment_policy.set.MDKG_BIN=\"/bin/mdkg\""]
        );
        assert_eq!(first.args.last().map(String::as_str), Some("-"));
        let worker = command(&input(Harness::Codex, Role::Worker, None));
        assert!(worker.args.iter().any(|a| a == "--approve-for-me"));
        assert!(!worker.args.iter().any(|a| a == "-s"));
        assert!(!worker.args.contains(&"project_doc_max_bytes=0".to_string()));
        // A worker's checkout is the user's own: their config applies.
        for gone in ["--ignore-user-config", "--ignore-rules"] {
            assert!(!worker.args.contains(&gone.to_string()), "{gone}");
        }
        let next = command(&input(Harness::Codex, Role::Worker, Some("t-9")));
        assert_eq!(&next.args[..3], ["exec", "resume", "t-9"]);
        assert!(has_pair(
            &next.args,
            "-c",
            "sandbox_mode=\"workspace-write\""
        ));
        assert!(!next.args.contains(&"-s".to_string()));
        assert!(
            next.args
                .iter()
                .any(|a| a.contains("enabled_tools") && a.contains("lease_claim"))
        );
        assert_eq!(next.cwd, PathBuf::from("/work/lane"));
    }

    #[test]
    fn native_auto_arguments_cover_each_harness_and_session_state() {
        for harness in [Harness::Claude, Harness::Codex, Harness::OpenCode] {
            for resume in [None, Some("session")] {
                let mut t = input(harness, Role::Worker, resume);
                let default = command(&t);
                match harness {
                    Harness::Codex if resume.is_none() => {
                        assert!(default.args.iter().any(|a| a == "--approve-for-me"));
                        assert!(!default.args.iter().any(|a| a == "-s"));
                    }
                    Harness::Codex => {
                        assert!(!default.args.iter().any(|a| a == "--approve-for-me"));
                        for config in [
                            "sandbox_mode=\"workspace-write\"",
                            "approval_policy=\"on-request\"",
                            "approvals_reviewer=\"auto_review\"",
                        ] {
                            assert!(
                                has_pair(&default.args, "-c", config),
                                "{config}: {:?}",
                                default.args
                            );
                        }
                    }
                    Harness::Claude => {
                        assert!(has_pair(&default.args, "--permission-mode", "auto"))
                    }
                    Harness::OpenCode => assert!(
                        !default
                            .args
                            .iter()
                            .any(|a| a.contains("permission-mode") || a.contains("sandbox_mode"))
                    ),
                }
                t.approvals = Some(if harness == Harness::Claude {
                    "acceptEdits"
                } else {
                    "never"
                });
                t.sandbox = (harness == Harness::Codex).then_some("read-only");
                let checked = crate::state::check_native(
                    harness,
                    Role::Worker,
                    "auto",
                    t.sandbox,
                    t.approvals,
                );
                if harness == Harness::OpenCode {
                    assert!(checked.is_err());
                    continue;
                }
                checked.expect("supported native override");
                let custom = command(&t);
                if harness == Harness::Codex {
                    assert!(
                        !custom
                            .args
                            .iter()
                            .any(|a| a == "--approve-for-me" || a == "-s")
                    );
                    for config in [
                        "sandbox_mode=\"read-only\"",
                        "approval_policy=\"never\"",
                        "approvals_reviewer=\"auto_review\"",
                    ] {
                        assert!(has_pair(&custom.args, "-c", config));
                    }
                } else {
                    assert!(has_pair(&custom.args, "--permission-mode", "acceptEdits"));
                }
            }
        }
    }

    #[test]
    fn codex_hook_trust_requires_complete_coverage_for_every_worker_launch() {
        for resume in [None, Some("t-9")] {
            let cmd = command(&input(Harness::Codex, Role::Worker, resume));
            for _ in 0..2 {
                let (args, trust) = cmd.launch_args();
                assert_eq!(trust, Some(CodexHookTrust::Required));
                assert_eq!(args, cmd.args);
                assert!(!args.iter().any(|a| a == CODEX_HOOK_TRUST_FLAG));
            }
        }
    }

    #[test]
    fn other_harnesses_and_reviewers_never_check_or_bypass_codex_hook_trust() {
        for harness in [Harness::Claude, Harness::Codex, Harness::OpenCode] {
            for role in [Role::Worker, Role::Reviewer] {
                if harness == Harness::Codex && role == Role::Worker {
                    continue;
                }
                for resume in [None, Some("t-9")] {
                    let cmd = command(&input(harness, role, resume));
                    let (args, trust) = cmd.launch_args();
                    assert_eq!(args, cmd.args);
                    assert_eq!(trust, None);
                    assert!(!args.iter().any(|a| a == CODEX_HOOK_TRUST_FLAG));
                }
            }
        }
    }

    #[test]
    fn absent_hooks_require_trust_and_launch_metadata_is_not_reported() {
        let cmd = command(&input(Harness::Codex, Role::Worker, None));
        let (args, trust) = cmd.launch_args();
        assert_eq!(args, cmd.args);
        assert_eq!(trust, Some(CodexHookTrust::Required));
        assert!(trust.expect("decision").reason().contains("not verified"));
        let json = serde_json::to_value(&cmd).expect("command JSON");
        assert!(json.get("codex_hook_binary").is_none());
        let schema =
            serde_json::to_value(schemars::schema_for!(AgentCommand)).expect("schema JSON");
        assert!(schema["properties"].get("codex_hook_binary").is_none());
    }

    /// A rule pattern as both harnesses match a command: `*` stands for
    /// any text, spaces included; everything else is literal.
    fn glob(pattern: &str, command: &str) -> bool {
        let parts: Vec<&str> = pattern.split('*').collect();
        let (first, rest) = parts.split_first().expect("parts");
        let Some(mut left) = command.strip_prefix(first) else {
            return false;
        };
        let Some((last, middle)) = rest.split_last() else {
            return left.is_empty();
        };
        for part in middle {
            match left.find(part) {
                Some(at) => left = &left[at + part.len()..],
                None => return false,
            }
        }
        left.len() >= last.len() && left.ends_with(last)
    }

    /// Whether Claude runs `command` under the rules: a deny wins over any
    /// allow.
    fn claude_runs(role: Role, command: &str) -> bool {
        let (allow, deny) = claude_rules(role, &[], &names());
        let bash = |r: &String| r.strip_prefix("Bash(")?.strip_suffix(')').map(String::from);
        let hits = |rules: &[String]| rules.iter().filter_map(bash).any(|p| glob(&p, command));
        hits(&allow) && !hits(&deny)
    }

    /// Whether OpenCode runs `command`: the last pattern that matches wins.
    fn opencode_runs(role: Role, command: &str) -> bool {
        let cfg = opencode_permissions(role, &names());
        let bash = cfg["permission"]["bash"].as_object().expect("rules");
        bash.iter()
            .rfind(|(p, _)| glob(p, command))
            .is_some_and(|(_, v)| v == "allow")
    }

    // OpenCode 2.1 normalizes each legacy document before loading agents:
    // config/normalize.ts and v1/config/migrate.ts. Global rules from ALL
    // documents precede per-agent rules from ANY document (config/plugin/agent.ts).
    fn opencode_21_rules(document: &serde_json::Value) -> Vec<serde_json::Value> {
        let mut rules = Vec::new();
        if let Some(tools) = document["tools"].as_object() {
            for (action, enabled) in tools {
                rules.push(serde_json::json!({
                    "action": action, "resource": "*",
                    "effect": if enabled == true { "allow" } else { "deny" },
                }));
            }
        }
        if let Some(permission) = document["permission"].as_object() {
            for (action, value) in permission {
                let action = match action.as_str() {
                    "bash" => "shell",
                    "task" => "subagent",
                    _ => action,
                };
                if let Some(patterns) = value.as_object() {
                    for (resource, effect) in patterns {
                        rules.push(serde_json::json!({"action": action, "resource": resource, "effect": effect}));
                    }
                } else {
                    rules.push(
                        serde_json::json!({"action": action, "resource": "*", "effect": value}),
                    );
                }
            }
        }
        if let Some(native) = document["permissions"].as_array() {
            rules.extend(native.iter().cloned());
        }
        rules
    }

    fn opencode_21_effect(
        user: &serde_json::Value,
        launched: &AgentCommand,
        action: &str,
        resource: &str,
    ) -> String {
        let inline: serde_json::Value = serde_json::from_str(
            &launched
                .env
                .iter()
                .find(|(key, _)| key == "OPENCODE_CONFIG_CONTENT")
                .expect("inline config")
                .1,
        )
        .expect("inline JSON");
        let agent = launched
            .args
            .windows(2)
            .find(|args| args[0] == "--agent")
            .map(|args| args[1].as_str())
            .unwrap_or_else(|| user["default_agent"].as_str().unwrap_or("build"));
        // Minimal defaults for these permission-precedence fixtures, not
        // a complete upstream evaluator (e.g. read/env asks are omitted).
        // Native OpenCode qualification is separate from this unit model.
        let mut globals = vec![serde_json::json!({
            "action": "*", "resource": "*", "effect": "allow"
        })];
        globals.extend(opencode_21_rules(user));
        globals.extend(opencode_21_rules(&inline));
        let mut rules = globals.clone();
        // Only build is seeded here; fixtures define every other agent.
        let mut exists = agent == "build";
        for document in [user, &inline] {
            for definition in [&document["agent"][agent], &document["agents"][agent]] {
                if !definition.is_object() {
                    continue;
                }
                // ConfigAgentPlugin removes disabled entries altogether;
                // a later inline definition creates a new agent with the
                // defaults/globals, not settings from the removed entry.
                // Pinned source: OpenCode v2.1.0, packages/core/src/config/plugin/agent.ts
                // https://github.com/anomalyco/opencode/blob/v2.1.0/packages/core/src/config/plugin/agent.ts#L80-L98
                if definition["disable"] == true || definition["disabled"] == true {
                    exists = false;
                    rules = globals.clone();
                } else {
                    exists = true;
                    rules.extend(opencode_21_rules(definition));
                }
            }
        }
        if !exists {
            return "missing_agent".into();
        }
        rules
            .iter()
            .rfind(|rule| {
                glob(rule["action"].as_str().expect("action"), action)
                    && glob(rule["resource"].as_str().expect("resource"), resource)
            })
            .map_or("ask", |rule| rule["effect"].as_str().expect("effect"))
            .to_string()
    }

    #[test]
    fn opencode_21_per_agent_allow_cannot_widen_reviewer_permissions() {
        for resume in [None, Some("ses_1")] {
            let t = input(Harness::OpenCode, Role::Reviewer, resume);
            let allow = serde_json::json!({"permissions": [
                {"action": "*", "resource": "*", "effect": "allow"}
            ]});
            let user = serde_json::json!({"agents": {
                "build": allow, "general": allow, opencode_agent(&t): allow,
            }});
            let launched = command(&t);
            for (action, resource) in [
                ("edit", "file.txt"),
                ("shell", "touch file.txt"),
                ("shell", "git diff --output=file.txt"),
                ("webfetch", "https://example.com"),
                ("external_directory", "/outside"),
                ("mdkg_room_post", "*"),
                ("mdkg_lease_claim", "*"),
                ("subagent", "general"),
            ] {
                assert_eq!(
                    opencode_21_effect(&user, &launched, action, resource),
                    "deny",
                    "{action}: {resource}"
                );
            }
            assert_eq!(
                opencode_21_effect(&user, &launched, "shell", "git diff --stat"),
                "allow"
            );
        }
    }

    #[test]
    fn opencode_21_worker_keeps_build_permissions_but_appends_mdkg_refusals() {
        let user = serde_json::json!({"agents": {"build": {"permissions": [
            {"action": "shell", "resource": "*", "effect": "allow"},
            {"action": "shell", "resource": "git push *", "effect": "deny"},
            {"action": "edit", "resource": "*", "effect": "ask"}
        ]}}});
        for resume in [None, Some("ses_1")] {
            let mut t = input(Harness::OpenCode, Role::Worker, resume);
            t.mdkg = Path::new("/opt/bin/mdkg-next");
            let launched = command(&t);
            for command in [
                "mdkg auth",
                "mdkg-next abandon task-1",
                "/opt/bin/mdkg-next auth status",
            ] {
                assert_eq!(
                    opencode_21_effect(&user, &launched, "shell", command),
                    "deny",
                    "{command}"
                );
            }
            assert_eq!(
                opencode_21_effect(&user, &launched, "shell", "cargo test"),
                "allow"
            );
            assert_eq!(
                opencode_21_effect(&user, &launched, "shell", "git push origin topic"),
                "deny"
            );
            assert_eq!(
                opencode_21_effect(&user, &launched, "edit", "file.txt"),
                "ask"
            );
        }
    }

    #[test]
    fn opencode_21_recreates_disabled_build_with_globals_but_not_its_old_settings() {
        let user = serde_json::json!({
            "default_agent": "unrestricted",
            "permissions": [{"action": "shell", "resource": "git reset *", "effect": "deny"}],
            "agents": {
                "build": {"disabled": true, "permissions": [
                    {"action": "shell", "resource": "git push *", "effect": "deny"}
                ]},
                "unrestricted": {"permissions": [{"action": "*", "resource": "*", "effect": "allow"}]}
            }
        });
        for resume in [None, Some("ses_1")] {
            let launched = command(&input(Harness::OpenCode, Role::Worker, resume));
            for (resource, expected) in [
                ("mdkg auth status", "deny"),
                ("mdkg abandon task-1", "deny"),
                ("git reset --hard", "deny"),
                ("git push origin topic", "allow"),
            ] {
                assert_eq!(
                    opencode_21_effect(&user, &launched, "shell", resource),
                    expected,
                    "{resource}"
                );
            }
        }
    }

    fn opencode_probe_fixture(fixture: &PathFixture, resume: Option<&str>) -> AgentCommand {
        let program = fixture.executable("opencode-probe", "unused");
        std::fs::write(&program, r#"#!/bin/sh
set -eu
# The fake serves only its fixture; it never reads a real configuration.
[ -z "${OPENCODE_CONFIG_CONTENT:-}" ] || exit 91
[ -n "${OPENCODE_PASSWORD:-}" ] || exit 92
mode=$(cat probe-mode)
case "$1" in
--version)
  echo --version >> probe-calls
  case "$mode" in
    bare-cli) echo '2.1.0';;
    old-cli) echo 'opencode v1.2.0';;
    newer-cli) echo 'opencode v2.2.0';;
    malformed-cli) echo not-a-version;;
    error-cli) echo 'opencode v2.1.0'; exit 1;;
    silent-cli) echo $$ > api-group; while :; do /bin/sleep 1; done;;
    *) echo 'opencode v2.1.0';;
  esac
  ;;
serve|server-child)
  if [ "$1" = serve ]; then
    echo $$ > probe-group
    if [ "$mode" = wrapped ]; then
      /bin/sh "$0" server-child
      exit
    fi
  fi
  echo $$ > probe-pid
  if [ "$mode" = exit ]; then exit 1; fi
  if [ "$mode" = forced ]; then
    trap '' TERM
    /bin/sh -c 'trap "" TERM; echo $$ > probe-descendant; while :; do /bin/sleep 1; done' &
    # Do not let API completion race the descendant's startup.
    while [ ! -s probe-descendant ]; do /bin/sleep 0.01; done
  fi
  if [ "$mode" = remote ]; then
    echo '{"url":"http://example.invalid:1234"}'
  elif [ "$mode" != silent ]; then
    echo '{"url":"http://127.0.0.1:1234"}'
  fi
  if [ "$mode" = forced ]; then
    while :; do /bin/sleep 1; done
  fi
  while read -r line; do :; done
  ;;
api)
  [ "$2" = --server ] && [ "$3" = http://127.0.0.1:1234 ] || exit 93
  echo "$4" >> probe-calls
  case "$4" in
  server.info)
    pid=$(cat probe-pid); version=2.1.0
    [ "$mode" != old ] || version=1.2.0
    [ "$mode" != foreign ] || pid=1
    printf '{"version":"%s","pid":%s}\n' "$version" "$pid"
    ;;
  *)
    [ "$5" = --param ] && [ "$6" = "location[directory]=$PWD" ] || exit 94
    if [ "$mode" = malformed ]; then echo not-json; exit; fi
    location=$PWD
    [ "$mode" != location ] || location=/elsewhere
    case "$4" in
    model.list)
      [ "$mode" != activation-error ] || exit 1
      if [ "$mode" = inherited-pipe ]; then
        echo $$ > api-group
        /bin/sh -c 'trap "" TERM; echo $$ > api-descendant; while :; do /bin/sleep 1; done' &
        # The reader must outlive the wrapper, holding the inherited pipe.
        while [ ! -s api-descendant ]; do /bin/sleep 0.01; done
        exit 0
      fi
      data='[]'
      ;;
    plugin.list)
      state=active
      [ "$mode" != plugin-error ] || state=failed
      data='[{"id":"opencode.agent","source":{"type":"builtin"},"state":{"status":"active"}},{"id":"opencode.config.agent","source":{"type":"builtin"},"state":{"status":"'"$state"'"}}]'
      [ "$mode" != missing-plugin ] || data='[]'
      ;;
    agent.list)
      data='[{"id":"build","mode":"primary","permissions":[]}]'
      [ "$mode" != disabled ] || data='[]'
      [ "$mode" != demoted ] || data='[{"id":"build","mode":"subagent","permissions":[]}]'
      [ "$mode" != invalid-build ] || data='[{"id":"build"}]'
      ;;
    *) exit 95;;
    esac
    printf '{"location":{"directory":"%s"},"data":%s}\n' "$location" "$data"
    ;;
  esac
  ;;
*) exit 96;;
esac
"#).expect("probe fixture");
        let mut t = input(Harness::OpenCode, Role::Worker, resume);
        t.cwd = &fixture.0;
        let mut cmd = opencode(&t);
        cmd.program = program;
        cmd.env
            .push(("HOME".into(), fixture.0.display().to_string()));
        cmd.env.push(("PATH".into(), "/usr/bin:/bin".into()));
        cmd
    }

    fn assert_probe_stopped(fixture: &PathFixture) {
        // These fixtures keep their descendants in the owned group. This
        // assertion cannot detect a descendant that deliberately leaves it
        // (for example via setsid); process-group cleanup cannot stop one.
        for (file, group) in [
            ("probe-group", "probe-group"),
            ("probe-pid", "probe-group"),
            ("probe-descendant", "probe-group"),
            ("api-group", "api-group"),
            ("api-descendant", "api-group"),
        ] {
            let Ok(pid) = std::fs::read_to_string(fixture.0.join(file)) else {
                continue;
            };
            let group = std::fs::read_to_string(fixture.0.join(group)).expect("owned group");
            let pid = rustix::process::Pid::from_raw(pid.trim().parse().unwrap()).unwrap();
            let expected = rustix::process::Pid::from_raw(group.trim().parse().unwrap()).unwrap();
            if rustix::process::getpgid(Some(pid)).ok() != Some(expected) {
                continue;
            }
            // An orphan zombie awaits the OS reaper; a reused PID in a
            // different group is not this probe's process either.
            let state = std::process::Command::new("/bin/ps")
                .args(["-o", "stat=", "-p", &pid.as_raw_nonzero().to_string()])
                .output()
                .expect("process state");
            let state = String::from_utf8_lossy(&state.stdout);
            assert!(
                state.trim().is_empty() || state.trim().starts_with('Z'),
                "{file} survived: {state}"
            );
        }
    }

    #[test]
    fn opencode_preflight_observes_build_without_the_overlay_on_every_attempt() {
        for resume in [None, Some("existing-session")] {
            let fixture = PathFixture::new();
            let cmd = opencode_probe_fixture(&fixture, resume);
            for mode in ["normal", "bare-cli", "disabled", "demoted", "wrapped"] {
                std::fs::write(fixture.0.join("probe-mode"), mode).expect("mode");
                let result = cmd.preflight();
                assert_eq!(result.is_ok(), mode != "disabled", "{mode}: {result:?}");
                assert_probe_stopped(&fixture);
            }
            assert_eq!(
                std::fs::read_to_string(fixture.0.join("probe-calls")).unwrap(),
                "--version\nserver.info\nmodel.list\nplugin.list\nagent.list\n".repeat(5)
            );
        }
    }

    #[test]
    fn opencode_preflight_refuses_unqualified_versions_before_server_start() {
        for mode in ["old-cli", "newer-cli", "malformed-cli", "error-cli"] {
            let fixture = PathFixture::new();
            let cmd = opencode_probe_fixture(&fixture, None);
            std::fs::write(fixture.0.join("probe-mode"), mode).unwrap();
            let error = cmd.preflight().expect_err(mode);
            assert_eq!(error.code, crate::error::CODE_AGENT_FAILED);
            assert!(
                !fixture.0.join("probe-pid").exists(),
                "{mode} started serve"
            );
            assert_eq!(
                std::fs::read_to_string(fixture.0.join("probe-calls")).unwrap(),
                "--version\n",
                "{mode} reached the native API"
            );
        }
    }

    #[test]
    fn opencode_preflight_bounds_version_check_before_server_start() {
        let fixture = PathFixture::new();
        let cmd = opencode_probe_fixture(&fixture, None);
        std::fs::write(fixture.0.join("probe-mode"), "silent-cli").unwrap();
        let started = std::time::Instant::now();
        assert!(opencode_preflight(&cmd, std::time::Duration::from_millis(200)).is_err());
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert!(!fixture.0.join("probe-pid").exists());
        assert_probe_stopped(&fixture);
    }

    #[test]
    fn opencode_preflight_refuses_unknown_or_failed_native_state_and_reaps_its_server() {
        let fixture = PathFixture::new();
        let cmd = opencode_probe_fixture(&fixture, None);
        for mode in [
            "old",
            "foreign",
            "malformed",
            "location",
            "activation-error",
            "plugin-error",
            "missing-plugin",
            "invalid-build",
            "remote",
            "exit",
        ] {
            std::fs::write(fixture.0.join("probe-mode"), mode).expect("mode");
            let error = cmd.preflight().expect_err(mode);
            assert_eq!(error.code, crate::error::CODE_AGENT_FAILED);
            assert!(
                error
                    .message
                    .starts_with("OpenCode worker preflight refused")
            );
            assert!(!error.message.contains("not-json")); // No native output echoed.
            assert_probe_stopped(&fixture);
        }
    }

    #[test]
    fn opencode_preflight_bounds_startup_and_stops_a_silent_server() {
        let fixture = PathFixture::new();
        let cmd = opencode_probe_fixture(&fixture, None);
        std::fs::write(fixture.0.join("probe-mode"), "silent").unwrap();
        let started = std::time::Instant::now();
        assert!(opencode_preflight(&cmd, std::time::Duration::from_millis(200)).is_err());
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        // Under load the deadline can kill the child before its script
        // even starts. Every server that did start must have been reaped.
        if fixture.0.join("probe-pid").exists() {
            assert_probe_stopped(&fixture);
        }
    }

    #[test]
    fn opencode_preflight_kills_term_ignoring_server_descendants() {
        let fixture = PathFixture::new();
        let cmd = opencode_probe_fixture(&fixture, None);
        std::fs::write(fixture.0.join("probe-mode"), "forced").unwrap();
        cmd.preflight()
            .expect("baseline answered before forced cleanup");
        assert!(fixture.0.join("probe-descendant").exists());
        assert_probe_stopped(&fixture);
    }

    #[test]
    fn opencode_preflight_cleans_api_descendants_after_the_wrapper_exits() {
        let fixture = PathFixture::new();
        let cmd = opencode_probe_fixture(&fixture, None);
        std::fs::write(fixture.0.join("probe-mode"), "inherited-pipe").unwrap();
        let cwd = fixture.0.canonicalize().unwrap();
        let mut api = std::process::Command::new(&cmd.program);
        api.current_dir(&cwd)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("OPENCODE_PASSWORD", "fixture")
            .args([
                "api",
                "--server",
                "http://127.0.0.1:1234",
                "model.list",
                "--param",
                &format!("location[directory]={}", cwd.display()),
            ]);
        let error = opencode_probe_output(api, std::time::Duration::from_secs(5)).unwrap_err();
        assert!(error.message.contains("timed out"), "{error}");
        assert!(fixture.0.join("api-descendant").exists());
        assert_probe_stopped(&fixture);
    }

    #[test]
    fn opencode_preflight_never_starts_for_reviewers_or_other_harnesses() {
        for harness in [Harness::Claude, Harness::Codex, Harness::OpenCode] {
            for role in [Role::Reviewer, Role::Worker] {
                if harness == Harness::OpenCode && role == Role::Worker {
                    continue;
                }
                let mut cmd = command(&input(harness, role, None));
                cmd.program = PathBuf::from("/missing/preflight-must-not-start");
                cmd.preflight()
                    .expect("no configuration probe for this role");
            }
        }
    }

    #[test]
    fn opencode_selects_the_protected_agent_on_fresh_and_resumed_turns() {
        let user = serde_json::json!({"default_agent": "unrestricted", "agents": {
            "unrestricted": {"permissions": [{"action": "*", "resource": "*", "effect": "allow"}]}
        }});
        for role in [Role::Reviewer, Role::Worker] {
            for resume in [None, Some("ses_1")] {
                let mut t = input(Harness::OpenCode, role, resume);
                let launched = command(&t);
                let selected = opencode_agent(&t);
                assert!(has_pair(&launched.args, "--agent", &selected));
                assert_eq!(
                    opencode_21_effect(&user, &launched, "shell", "mdkg auth"),
                    "deny"
                );
                let cfg = opencode_config(&t);
                // No plural 2.1-only keys: the same inline document is
                // parseable by the legacy schema, with globals retained.
                assert_eq!(cfg["agent"][&selected]["permission"], cfg["permission"]);
                assert!(cfg.get("agents").is_none());
                assert!(cfg["agent"][&selected].get("permissions").is_none());
                if role == Role::Reviewer {
                    assert_ne!(selected, "build");
                    assert_eq!(cfg["agent"][&selected]["tools"], cfg["tools"]);
                    assert_eq!(cfg["agent"][&selected]["mode"], "primary");
                    t.new_session = "another-turn";
                    assert_ne!(opencode_agent(&t), selected);
                } else {
                    assert_eq!(selected, "build");
                    assert!(cfg["agent"][&selected].get("tools").is_none());
                    assert!(cfg["agent"][&selected].get("model").is_none());
                }
            }
        }
    }

    /// git's read commands write files through `--output` (RC1 security
    /// audit): refused for reviewers, in Claude's rules and OpenCode's,
    /// wherever the option is; the plain reads still run.
    #[test]
    fn reviewers_cannot_write_files_through_gits_output_option() {
        let writes = [
            "git diff --output=important.txt",
            "git log --output=../escaped.txt -1",
            "git show --output f HEAD",
            "git diff --stat --output=x",
            "git diff \"--output=x\"",
            "git diff \"--output\" x",
            "git show '--output' x HEAD",
            "git diff --output\t../victim.txt",
            "git diff \"--output\"\tx",
            "git log '--output'\tx -1",
            "git diff --no-index --output=w /dev/null other.txt",
        ];
        let reads = [
            "git diff",
            "git diff --stat",
            "git log --oneline -20",
            "git show HEAD",
            "git log -- src/output.rs",
            "git status",
            "git diff --output-indicator-new=X HEAD~1 HEAD",
            "git rev-parse --output-object-format=sha1 HEAD",
        ];
        let role = Role::Reviewer;
        for c in writes {
            assert!(!claude_runs(role, c), "claude {role:?}: {c}");
            assert!(!opencode_runs(role, c), "opencode {role:?}: {c}");
        }
        for c in reads {
            assert!(claude_runs(role, c), "claude {role:?}: {c}");
            assert!(opencode_runs(role, c), "opencode {role:?}: {c}");
        }
    }

    #[test]
    fn opencode_passes_the_prompt_and_the_roles_permissions_inline() {
        let c = command(&input(Harness::OpenCode, Role::Reviewer, Some("ses_1")));
        assert!(has_pair(&c.args, "--session", "ses_1"));
        assert_eq!(c.args.last().map(String::as_str), Some("the digest"));
        assert!(!c.prompt_on_stdin);
        assert!(c.args.contains(&"--standalone".to_string()));
        let env = |k: &str| c.env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        // The user's own config folders and providers, but not the
        // reviewed checkout's config.
        for gone in ["XDG_CONFIG_HOME", "XDG_DATA_HOME"] {
            assert_eq!(env(gone), None, "{gone}");
        }
        assert_eq!(env("OPENCODE_DISABLE_PROJECT_CONFIG").as_deref(), Some("1"));
        assert_eq!(env("OPENCODE_CONFIG_PROJECT_DISABLE").as_deref(), Some("1"));
        let worker = command(&input(Harness::OpenCode, Role::Worker, None));
        assert!(
            !worker
                .env
                .iter()
                .any(|(k, _)| k == "OPENCODE_DISABLE_PROJECT_CONFIG"),
            "a worker's checkout is the user's own"
        );
        let cfg: serde_json::Value =
            serde_json::from_str(&env("OPENCODE_CONFIG_CONTENT").expect("config")).expect("json");
        assert_eq!(cfg["permission"]["edit"], "deny");
        let bash = cfg["permission"]["bash"].as_object().expect("rules");
        assert_eq!(bash.keys().next().map(String::as_str), Some("*"));
        assert_eq!(bash["*"], "deny");
        assert_eq!(bash["git diff *"], "allow");
        assert!(!bash.contains_key("cargo *"), "a reviewer builds nothing");
        assert_eq!(cfg["tools"]["mdkg_lease_claim"], false);
        assert_eq!(cfg["mcp"]["mdkg"]["command"][0], "/bin/mdkg");
        let menv = &cfg["mcp"]["mdkg"]["environment"];
        assert_eq!(menv["MDKG_TRUSTED_COMMON_DIR"], "/repo/.git");
        assert_eq!(menv["MDKG_TRUSTED_WORKTREE"], "/repo");
        assert_eq!(menv["XDG_CONFIG_HOME"], "/Users/u/.config");
        let w = opencode_permissions(Role::Worker, &names());
        assert!(w["permission"].get("edit").is_none());
        assert!(w["permission"]["bash"].get("cargo *").is_none());
    }

    #[test]
    fn every_mcp_server_opens_the_launchers_root() {
        let mut t = input(Harness::Claude, Role::Worker, None);
        t.graph_root = Some(Path::new("/repo/.worktrees/graph"));
        let cfg = json_after(&command(&t).args, "--mcp-config");
        assert_eq!(
            cfg["mcpServers"]["mdkg"]["args"],
            serde_json::json!(["--root", "/repo/.worktrees/graph", "mcp"])
        );
        t.harness = Harness::Codex;
        assert!(has_pair(
            &command(&t).args,
            "-c",
            "mcp_servers.mdkg.args=[\"--root\",\"/repo/.worktrees/graph\",\"mcp\"]"
        ));
    }

    #[test]
    fn effort_reaches_claude_and_codex() {
        let mut t = input(Harness::Claude, Role::Worker, None);
        t.effort = Some("low");
        assert!(has_pair(&command(&t).args, "--effort", "low"));
        t.harness = Harness::Codex;
        assert!(has_pair(
            &command(&t).args,
            "-c",
            "model_reasoning_effort=\"low\""
        ));
        let none = command(&input(Harness::Codex, Role::Worker, None));
        assert!(
            !none
                .args
                .iter()
                .any(|a| a.contains("model_reasoning_effort"))
        );
        assert!(
            !none.args.contains(&"-m".to_string()),
            "no model: the account's default"
        );
    }

    #[test]
    fn a_key_reaches_claude_only_through_its_helper_and_codex_only_in_its_process() {
        let key = crate::auth::Secret::for_test("sk-test-1");
        let mut t = input(Harness::Claude, Role::Worker, None);
        let plain = command(&t);
        assert!(!plain.args.iter().any(|a| a.contains("apiKeyHelper")));
        // The helper by the path this turn names mdkg, denied to the agent.
        assert!(plain.args.contains(&"Bash('/bin/mdkg' auth *)".to_string()));
        t.api_key = Some(("mdkg-anthropic", &key));
        let c = command(&t);
        let settings = json_after(&c.args, "--settings");
        assert_eq!(
            settings["apiKeyHelper"],
            "/usr/bin/env -u MDKG_RUN_TURN '/bin/mdkg' auth key anthropic"
        );
        assert!(c.secret_env.is_empty());
        assert!(!format!("{c:?}").contains("sk-test-1"));
        t.harness = Harness::Codex;
        let c = command(&t);
        assert_eq!(c.secret_env.len(), 1);
        assert_eq!(c.secret_env[0].0, "CODEX_API_KEY");
        assert!(!format!("{c:?}").contains("sk-test-1"));
        assert!(
            !serde_json::to_string(&c)
                .expect("json")
                .contains("sk-test-1")
        );
        assert!(has_pair(
            &c.args,
            "-c",
            "shell_environment_policy.exclude=[\"CODEX_API_KEY\",\"OPENAI_API_KEY\"]"
        ));
    }

    #[test]
    fn the_launcher_helper_clears_only_its_agent_turn_mark() {
        let fixture = PathFixture::new();
        let helper = fixture.executable("helper", "unused");
        std::fs::write(
            &helper,
            "#!/bin/sh\nprintf '%s\\n' \"${MDKG_RUN_TURN-unmarked}\" \"$MDKG_ACTOR\" \"$*\"\n",
        )
        .expect("fake helper");
        let output = std::process::Command::new("/bin/sh")
            .args(["-c", &key_helper(&helper, "mdkg-anthropic")])
            .env(TURN_MARK, "outer-turn")
            .env("MDKG_ACTOR", "worker")
            .output()
            .expect("helper");
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).expect("utf8"),
            "unmarked\nworker\nauth key anthropic\n"
        );
    }

    #[test]
    fn the_main_branch_is_always_protected() {
        assert_eq!(protected("refs/heads/trunk", &[]), ["main", "trunk"]);
        assert_eq!(protected("main", &["release".into()]), ["main", "release"]);
    }
}
