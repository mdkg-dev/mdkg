//! Which account a turn bills: the user's subscription first, an API key
//! only when the config allows it, and never one an agent can see.
//!
//! - **Subscription first.** Each harness uses its own login: Claude Code's
//!   (the keychain), Codex's (ChatGPT, in CODEX_HOME), OpenCode's own
//!   configured auth. The provider key variables in [`KEY_VARS`] never
//!   reach an agent, so a stray key in the launcher's shell can't switch a
//!   turn to API billing, whatever the agent's environment mode
//!   ([`crate::env`]).
//! - **A key only by the user's choice.** Nothing bills the key on its own
//!   (decided 2026-10-29): `mdkg agents use-key <agent>` bills the agent's
//!   next turn, one, on it, where `auth.api_key_fallback` allows that. A
//!   harness that isn't logged in refuses the turn, and a usage limit ends
//!   it; both name use-key. 0.7.0's automatic key turns (after a limit, and
//!   for a harness that isn't logged in) are gone, and a turn is never
//!   re-run on the key within itself.
//!   The key comes from the macOS Keychain (`mdkg-anthropic`,
//!   `mdkg-openai`), never a file, the config or the agent's environment:
//!   Claude reads it through an `apiKeyHelper` (`mdkg auth key`), and Codex
//!   gets it in its own process only, kept out of its tool shells.
//! - **Never shown.** The key is never logged or printed, and it's taken
//!   out of turn files and replies.
//!
//! The limit, stated plainly: with no sandbox, an agent
//! runs as the user. Claude's rules deny running `security`, `mdkg auth`
//! and `mdkg abandon` directly, under every mdkg name an allow admits
//! (crate::profile). That's a guardrail, not isolation: a worker that can
//! edit files and run `cargo` (the default) can run any code as the user,
//! the Keychain read included. So with `auth.api_key_fallback` on, treat
//! the key as reachable by workers, and keep it off unless it's needed.
//! Isolation needs a sandbox, which 0.7.0 doesn't provide.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde::Serialize;

use crate::error::{CODE_UNSUPPORTED, RunError};
use crate::profile::Harness;

/// Provider key and token variables no agent inherits, by name: the ones
/// Claude Code, Codex and OpenCode read to bill an API account instead of
/// the user's login (OpenCode's from its 2.1 provider list; RC1 security
/// audit). [`KEY_PREFIXES`] withholds whole families besides. Subscription
/// tokens (`CLAUDE_CODE_OAUTH_TOKEN`) stay, and so do general cloud and
/// service credentials (`AWS_PROFILE`, `AWS_ACCESS_KEY_ID`,
/// `GOOGLE_APPLICATION_CREDENTIALS`, `GITLAB_TOKEN`), which an agent's own
/// work may need and which bill no turn without a provider switch.
pub const KEY_VARS: &[&str] = &[
    "CLAUDE_API_KEY",
    "AWS_BEARER_TOKEN_BEDROCK",
    "CODEX_API_KEY",
    "AZURE_OPENAI_API_KEY",
    "AZURE_API_KEY",
    "AZURE_COGNITIVE_SERVICES_API_KEY",
    "AZURE_COGNITIVE_SERVICES_RESOURCE_NAME",
    "AZURE_RESOURCE_NAME",
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "GOOGLE_GENERATIVE_AI_API_KEY",
    "OPENROUTER_API_KEY",
    "AI_GATEWAY_API_KEY",
    "CF_AIG_TOKEN",
    "CLOUDFLARE_WORKERS_AI_TOKEN",
    "NEON_AI_GATEWAY_TOKEN",
    "GROQ_API_KEY",
    "DEEPSEEK_API_KEY",
    "DEEPINFRA_API_KEY",
    "XAI_API_KEY",
    "ZAI_API_KEY",
    "MISTRAL_API_KEY",
    "COHERE_API_KEY",
    "PERPLEXITY_API_KEY",
    "TOGETHER_API_KEY",
    "TOGETHER_AI_API_KEY",
    "FIREWORKS_API_KEY",
    "CEREBRAS_API_KEY",
    "BASETEN_API_KEY",
    "ALIBABA_API_KEY",
    "DASHSCOPE_API_KEY",
    "MOONSHOT_API_KEY",
    "MOONSHOTAI_API_KEY",
    "MINIMAX_API_KEY",
    "META_API_KEY",
    "VENICE_API_KEY",
    "DO_INFERENCE_API_KEY",
    "OPENCODE_API_KEY",
];

/// Whole families no agent inherits, so a provider switch or credential a
/// harness adds later is withheld too (fail closed; review of 14867b8):
/// every `ANTHROPIC_*` (keys, tokens, custom headers, endpoints, cloud
/// projects, identity federation) but the model choices in
/// [`KEPT_ANTHROPIC`]; every `CLAUDE_CODE_USE_*`, which selects a provider
/// (Bedrock, Vertex, Foundry, Mantle, Anthropic on AWS or Google Cloud, a
/// gateway) among a few feature switches; every `OPENAI_*`; and
/// OpenCode's provider-only families, `GOOGLE_VERTEX_*` (its Vertex key,
/// project and location) and `SNOWFLAKE_CORTEX_*` (a token or PAT that
/// replaces its configured auth; review of 516603c). General Google Cloud
/// settings (`GOOGLE_CLOUD_PROJECT` and the credentials) stay: with them,
/// a turn whose model is a Vertex one bills that project, as the model the
/// user chose says.
pub const KEY_PREFIXES: &[&str] = &[
    "ANTHROPIC_",
    "CLAUDE_CODE_USE_",
    "OPENAI_",
    "GOOGLE_VERTEX_",
    "SNOWFLAKE_CORTEX_",
];

/// The `ANTHROPIC_*` variables that only choose a model, which an agent
/// keeps: `ANTHROPIC_MODEL`, `ANTHROPIC_SMALL_FAST_MODEL`, and those
/// starting `ANTHROPIC_DEFAULT_` or `ANTHROPIC_CUSTOM_MODEL_OPTION`.
const KEPT_ANTHROPIC: &[&str] = &[
    "ANTHROPIC_MODEL",
    "ANTHROPIC_SMALL_FAST_MODEL",
    "ANTHROPIC_DEFAULT_*",
    "ANTHROPIC_CUSTOM_MODEL_OPTION*",
];

/// Every environment variable the provider catalogs of the qualified
/// OpenCode releases declare (2.1.0 and 2.2.5, `OPENCODE_QUALIFIED` in
/// profile.rs), one per line: the union of what
/// `scripts/opencode-catalog-env.py` extracts from each binary, sorted.
/// 2.2.5's came from the desktop app's
/// `/Applications/OpenCode.app/Contents/Resources/opencode-cli`. None is
/// dropped while a release naming it stays qualified. OpenCode bills a
/// provider through the key one names when nothing is stored for it, so
/// all of them but [`KEPT_GENERAL`] are withheld (review of c569050; the
/// orchestrator's option A). Before qualifying another release, add its
/// script output to the union.
pub const OPENCODE_CATALOG: &str = include_str!("opencode-catalog-env.txt");

/// The general cloud and service credentials an agent's own work may need,
/// kept though OpenCode's catalog names some of them (`*` ends a prefix).
/// [`KEY_VARS`] still withholds a provider-only one among them
/// (`AWS_BEARER_TOKEN_BEDROCK`).
pub const KEPT_GENERAL: &[&str] = &[
    "AWS_*",
    "GOOGLE_APPLICATION_CREDENTIALS",
    "GITHUB_TOKEN",
    "GITLAB_TOKEN",
    "CLOUDFLARE_*",
    "DATABRICKS_*",
    "DIGITALOCEAN_ACCESS_TOKEN",
    "SNOWFLAKE_ACCOUNT",
    "HF_TOKEN",
    "WANDB_API_KEY",
];

/// Whether `bytes` is one of `patterns`, where a final `*` is a prefix.
fn listed(bytes: &[u8], patterns: &[&str]) -> bool {
    patterns.iter().any(|k| match k.strip_suffix('*') {
        Some(prefix) => bytes.starts_with(prefix.as_bytes()),
        None => bytes == k.as_bytes(),
    })
}

/// Whether no agent inherits the variable `name`.
pub fn withheld(name: &std::ffi::OsStr) -> bool {
    let bytes = name.as_encoded_bytes();
    if KEY_VARS.iter().any(|k| bytes == k.as_bytes()) {
        return true;
    }
    let catalog = OPENCODE_CATALOG.lines().any(|n| n.as_bytes() == bytes);
    (catalog && !listed(bytes, KEPT_GENERAL))
        || (!listed(bytes, KEPT_ANTHROPIC)
            && KEY_PREFIXES.iter().any(|p| bytes.starts_with(p.as_bytes())))
}

/// The session variables a host Claude Code session (the desktop app, an
/// SDK host) exports to what it runs, which an agent mdkg starts from
/// inside one must not take as its own: bare mode (`CLAUDE_CODE_SIMPLE`
/// skips hooks), the host's session ids and messaging socket and token,
/// its account identity, its effort and its SDK-host switches (spike,
/// 2026-10-25). The user's own login (`CLAUDE_CODE_OAUTH_TOKEN`) and
/// general settings (`MCP_*`, `DISABLE_*`) aren't withheld here; since
/// 0.7.2 they reach a turn only when its `env.allow` names them or
/// `env.inherit` is on ([`crate::env`]).
pub const HOST_SESSION_VARS: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_SIMPLE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_ENTRYPOINT_ON_REQUESTS",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_HOST_SESSION_ID",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_SESSION_ATTENDED",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_CODE_SDK_HAS_HOST_AUTH_REFRESH",
    "CLAUDE_CODE_EXECPATH",
    "CLAUDE_CODE_DESKTOP_APP_VERSION",
    "CLAUDE_CODE_ACCOUNT_UUID",
    "CLAUDE_CODE_ORGANIZATION_UUID",
    "CLAUDE_CODE_USER_EMAIL",
    "CLAUDE_CODE_OAUTH_SCOPES",
    "CLAUDE_CODE_REPORT_FINDINGS",
    "CLAUDE_CODE_STARTUP_FAILURE_RESULTS",
    "CLAUDE_CODE_TERMINAL_MCP_TOOLS",
    "CLAUDE_CODE_EAGER_FLUSH",
    "CLAUDE_CODE_DISABLE_CRON",
    "CLAUDE_CODE_DISABLE_TERMINAL_TITLE",
    "CLAUDE_EFFORT",
    "CLAUDE_PID",
    "CLAUDE_PREVIEW_CLASSIFIER_FLOOR",
    // SDK-host switches among the user-settable CLAUDE_CODE_ENABLE_*,
    // which otherwise stay (review of a43d8c1).
    "CLAUDE_CODE_ENABLE_ASK_USER_QUESTION_TOOL",
    "CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING",
    // A host's sessions and their services' credentials (2.1.309).
    "CLAUDE_CODE_CLOUD_SESSION_ID",
    "CLAUDE_CODE_CONTAINER_ID",
    "CLAUDE_CODE_HOSTED_DESKTOP",
    "CLAUDE_CODE_PROJECTS_SESSION",
    "CLAUDE_CODE_FORCE_BRIDGE",
    "CLAUDE_CODE_RESUME_FROM_SESSION",
    "CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST",
    "CLAUDE_CODE_AUTOUPDATER_DISABLED_BY_HOST",
    "CLAUDE_CODE_WEBSOCKET_AUTH_FILE_DESCRIPTOR",
    "CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR",
    "CLAUDE_CODE_GATEWAY_TOKEN",
    "CLAUDE_CODE_GATEWAY_TOKEN_FILE_DESCRIPTOR",
    "CLAUDE_CODE_ARTIFACTS_API_TOKEN",
    "CLAUDE_CODE_MEMORY_API_TOKEN",
    "CLAUDE_CODE_MCP_SERVE_AUTH_TOKEN",
    "CLAUDE_CODE_SLACK_TAG_TOKEN",
    "CLAUDE_CODE_HFI_BEARER_TOKEN",
    // Outside CLAUDE_CODE_: a host's credentials and its session's context
    // (review of 8cbef6f), from 2.1.309's names.
    "CLAUDE_TRUSTED_DEVICE_TOKEN",
    "CLAUDE_ARTIFACT_HOST_GRANT",
    "CLAUDE_CLIENT_PRESENCE_FILE",
    "CLAUDE_HELPER_CONTEXT",
    "CLAUDE_SCRIPT_HOOK_ACK",
    "CLAUDE_PROJECT_DIR",
    "CLAUDE_PROJECT_UUID",
    "CLAUDE_JOB_DIR",
    "CLAUDE_SKILL_DIR",
    "CLAUDE_HOST",
];

/// Host-session families, by prefix, from Claude Code 2.1.309's own names:
/// a host session's ids and its ingress credential
/// (CLAUDE_CODE_SESSION_ACCESS_TOKEN), remote and bridge sessions, the
/// host's own settings and credentials file, the SDK host's, and its
/// switches for what the session emits (review of a43d8c1).
pub const HOST_SESSION_PREFIXES: &[&str] = &[
    "CLAUDE_AGENT_SDK_",
    "CLAUDE_CODE_SDK_",
    "CLAUDE_CODE_SESSION_",
    "CLAUDE_CODE_REMOTE",
    "CLAUDE_CODE_BRIDGE_",
    "CLAUDE_CODE_HOST_",
    "CLAUDE_CODE_EMIT_",
    // Outside CLAUDE_CODE_: a session's ingress token file
    // (CLAUDE_SESSION_INGRESS_TOKEN_FILE) and ids, a bridge's OAuth token
    // and URLs, background, runner, remote, SSH and PTY hosts' state, the
    // Chrome pairing, claude.ai's OAuth scopes, and a host plugin's paths.
    "CLAUDE_SESSION_",
    "CLAUDE_BRIDGE_",
    "CLAUDE_BG_",
    "CLAUDE_RUNNER_",
    "CLAUDE_REMOTE_",
    "CLAUDE_SSH_",
    "CLAUDE_PTY_",
    "CLAUDE_CHROME_",
    "CLAUDE_IN_CHROME_",
    "CLAUDE_AI_",
    "CLAUDE_LOCAL_OAUTH_",
    "CLAUDE_PLUGIN_",
];

/// User settings inside the host families, which an agent keeps: the
/// remote-control session names' prefix (review of ebdf137).
const HOST_FAMILY_KEPT: &[&str] = &["CLAUDE_REMOTE_CONTROL_SESSION_NAME_PREFIX"];

/// Whether a variable is a host session's own ([`HOST_SESSION_VARS`]).
pub fn host_session(name: &std::ffi::OsStr) -> bool {
    let bytes = name.as_encoded_bytes();
    if HOST_FAMILY_KEPT.iter().any(|k| bytes == k.as_bytes()) {
        return false;
    }
    HOST_SESSION_VARS.iter().any(|k| bytes == k.as_bytes())
        || HOST_SESSION_PREFIXES
            .iter()
            .any(|p| bytes.starts_with(p.as_bytes()))
}

/// The variables to take out of an agent's environment, and its login
/// check's: every one [`withheld`] or a host session's own in this
/// process's, and [`KEY_VARS`] and a re-execed supervisor's exec token
/// whether set or not.
pub fn withheld_vars() -> Vec<std::ffi::OsString> {
    let mut vars: Vec<std::ffi::OsString> = KEY_VARS.iter().map(Into::into).collect();
    vars.push(crate::supervise::reload::EXEC_ENV.into());
    vars.extend(
        std::env::vars_os()
            .map(|(k, _)| k)
            .filter(|k| (withheld(k) || host_session(k)) && !KEY_VARS.iter().any(|v| k == v)),
    );
    vars
}

/// The withheld variables set in this process's environment, by name only.
pub fn key_vars_set() -> Vec<String> {
    std::env::vars_os()
        .filter(|(k, v)| withheld(k) && !v.is_empty())
        .map(|(k, _)| k.to_string_lossy().into_owned())
        .collect()
}

/// A secret: its value never reaches `Debug`, `Display` or serde.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// A secret made up in a test.
    #[cfg(test)]
    pub(crate) fn for_test(s: &str) -> Self {
        Self(s.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }

    /// `text` with every occurrence of the secret replaced.
    pub fn redact(&self, text: &str) -> String {
        if self.0.is_empty() {
            return text.to_string();
        }
        text.replace(&self.0, REDACTED)
    }
}

/// What a secret is shown as.
pub const REDACTED: &str = "[redacted]";

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(REDACTED)
    }
}

/// The Keychain service an API key for a harness is under (its adapter's).
pub fn keychain_service(h: Harness) -> Option<&'static str> {
    h.adapter().keychain_service()
}

/// The provider name `mdkg auth key <provider>` takes for a service.
pub fn provider_of(service: &str) -> Option<&'static str> {
    match service {
        "mdkg-anthropic" => Some("anthropic"),
        "mdkg-openai" => Some("openai"),
        _ => None,
    }
}

/// Where API keys come from: macOS's `security`, or `$MDKG_RUN_SECURITY`
/// (a fake, in tests).
#[derive(Debug, Clone)]
pub struct KeySource {
    pub program: PathBuf,
    /// How long one ask may take ([`CHECK_LIMIT`] unless a test sets
    /// more).
    pub limit: Duration,
}

impl Default for KeySource {
    fn default() -> Self {
        Self {
            program: std::env::var_os("MDKG_RUN_SECURITY")
                .filter(|v| !v.is_empty())
                .map_or_else(|| PathBuf::from("/usr/bin/security"), PathBuf::from),
            limit: CHECK_LIMIT,
        }
    }
}

/// How long a key source or login check may take.
pub const CHECK_LIMIT: Duration = Duration::from_secs(30);

fn bounded(cmd: &mut Command) -> std::io::Result<std::process::Output> {
    bounded_within(cmd, CHECK_LIMIT)
}

fn bounded_within(cmd: &mut Command, limit: Duration) -> std::io::Result<std::process::Output> {
    crate::turn::output_within(cmd, limit)
}

impl KeySource {
    /// Whether the Keychain holds an item for `service`, without reading
    /// its secret.
    pub fn has(&self, service: &str) -> bool {
        bounded_within(
            Command::new(&self.program)
                .args(["find-generic-password", "-s", service])
                .stdin(std::process::Stdio::null()),
            self.limit,
        )
        .is_ok_and(|o| o.status.success())
    }

    /// The key under `service`. Its errors never carry the key.
    pub fn read(&self, service: &str) -> Result<Secret, RunError> {
        let out = bounded_within(
            Command::new(&self.program)
                .args(["find-generic-password", "-s", service, "-w"])
                .stdin(std::process::Stdio::null()),
            self.limit,
        )
        .map_err(|e| RunError::io("can't ask the Keychain for the API key", e))?;
        let key = String::from_utf8(out.stdout).unwrap_or_default();
        let key = key.trim_end_matches(['\n', '\r']);
        if !out.status.success() || key.is_empty() || key.contains(['\n', '\r']) {
            return Err(RunError::usage(
                CODE_UNSUPPORTED,
                format!("the Keychain holds no API key under {service}"),
            )
            .with_hint(format!(
                "add one with `security add-generic-password -s {service} -a mdkg -w`, which asks \
                 for it"
            )));
        }
        Ok(Secret(key.to_string()))
    }
}

/// How a harness is logged in, as its own CLI reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Login {
    /// Logged in; the method as the harness names it (`claude.ai`,
    /// `ChatGPT`), and whether it's a subscription rather than an API key
    /// the harness keeps itself (`codex login --with-api-key`, Claude's
    /// own `apiKeyHelper` or console key; review of 2f2320e).
    In {
        method: String,
        subscription: bool,
    },
    Out,
    /// No check: OpenCode's own config decides.
    Own,
}

/// The command that logs a harness in (its adapter's).
pub fn login_command(h: Harness) -> &'static str {
    h.adapter().login_command()
}

/// Asks the harness whether it's logged in (`claude auth status --json`,
/// `codex login status`), through its adapter, which reads the answer from
/// [`login_probe`]. Prints nothing it says.
pub fn login(h: Harness, program: &Path, cwd: &Path, env: &crate::env::Environment) -> Login {
    h.adapter().login(program, cwd, env)
}

/// Runs a harness's login check, `program args`, as an agent would start
/// it: in the agent's folder and with the agent's environment
/// ([`crate::env`]), never a key variable, so a stray key doesn't count as
/// a login and a login the allowlist hides doesn't either. `None` when it
/// can't run or finish in time.
pub(crate) fn login_probe(
    program: &Path,
    cwd: &Path,
    args: &[&str],
    env: &crate::env::Environment,
) -> Option<std::process::Output> {
    let mut cmd = Command::new(program);
    cmd.current_dir(cwd);
    env.apply(&mut cmd);
    cmd.stdin(std::process::Stdio::null());
    cmd.args(args);
    bounded(&mut cmd).ok()
}

/// What a turn bills.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Billing {
    /// The harness's own login.
    Subscription,
    /// An API key from the Keychain, for one harness.
    ApiKey { service: &'static str, key: Secret },
}

/// The account a turn's login gives it: the harness's subscription (or
/// OpenCode's own configured auth), else a refusal naming the login command
/// and `agents use-key`. It never picks the key: only a turn whose use-key
/// mark is consumed bills it (crate::turn). 0.7.0's fallback to the key for
/// a harness that isn't logged in, or that bills a key of its own, is gone
/// (decided 2026-10-29), and no setting bills the key on every turn.
pub fn choose(
    h: Harness,
    program: &Path,
    cwd: &Path,
    agent: &str,
    env: &crate::env::Environment,
) -> Result<Billing, RunError> {
    let login = login(h, program, cwd, env);
    match &login {
        Login::In {
            subscription: true, ..
        }
        | Login::Own => Ok(Billing::Subscription),
        Login::Out | Login::In { .. } => Err(RunError::usage(
            crate::error::CODE_LOGIN_REQUIRED,
            match &login {
                Login::In { method, .. } => format!(
                    "{} is logged in with {method}, an API key rather than a subscription, \
                     and nothing marked this turn to bill mdkg's key",
                    h.as_str()
                ),
                _ => format!(
                    "{} isn't logged in, so the turn has no account to run on",
                    h.as_str()
                ),
            },
        )
        .with_hint(format!(
            "run `{}` once{}",
            login_command(h),
            if keychain_service(h).is_some() {
                format!(
                    ", or `{} agents use-key {agent}` to bill your API key for its next turn \
                     (auth.api_key_fallback allows it)",
                    mdkg_format::program::BIN
                )
            } else {
                String::new()
            }
        ))),
    }
}

/// The mark `agents use-key` writes, so the agent's next turn bills the key
/// (`auth.api_key_fallback` allows it). Only that command writes it:
/// nothing arms a key charge automatically, and nothing is re-run within a
/// turn, for a limit or any other reason.
pub fn key_next_path(dir: &Path, agent: &str) -> PathBuf {
    dir.join("billing")
        .join(format!("{}.json", crate::state::file_name(agent)))
}

/// Syncs the folder holding the mark, so a rename or removal in it
/// survives a crash.
fn sync_folder(path: &Path) -> std::io::Result<()> {
    std::fs::File::open(path.parent().unwrap_or(Path::new(".")))?.sync_all()
}

fn mark_error(path: &Path, what: &str, e: std::io::Error) -> RunError {
    RunError::failure(
        crate::error::CODE_IO,
        format!("can't {what} the key fallback in {}: {e}", path.display()),
    )
}

/// Whether `folder`, a billing folder, is there and fit to use (orchestrator,
/// 2026-10-30, option B on #6097): never a link, a folder, owned by this
/// user and writable by no one else. `Ok(false)` is absence; anything unfit
/// is an error, never followed. Races with a same-user writer of the run
/// folder are the stated residual (D1).
fn checked_folder(folder: &Path) -> std::io::Result<bool> {
    use std::os::unix::fs::MetadataExt as _;
    let m = match std::fs::symlink_metadata(folder) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    let unfit = |why: &str| Err(std::io::Error::other(format!("{} {why}", folder.display())));
    if m.file_type().is_symlink() || !m.is_dir() {
        return unfit("is a link or not a folder");
    }
    if m.uid() != rustix::process::getuid().as_raw() {
        return unfit("isn't owned by you");
    }
    if m.mode() & 0o022 != 0 {
        return unfit("is writable by others");
    }
    Ok(true)
}

/// The run folder's `billing/`, checked ([`checked_folder`]) and created
/// owner-only if it's missing. Every folder this makes, the run folder too
/// for an agent that has never run, and the existing ones every time, are
/// synced into their parents, top down, so a mark written in it survives a
/// crash, even after an earlier sync failed (reviews of e52416d5 and
/// 7ff5295e).
fn billing_folder(dir: &Path) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt as _;
    let folder = dir.join("billing");
    // The run folder and billing/ always; above them, whatever this makes.
    let mut chain = vec![folder.clone(), dir.to_path_buf()];
    if !checked_folder(&folder)? {
        let mut at = dir;
        while !at.exists() {
            match at.parent() {
                Some(p) if !p.as_os_str().is_empty() => {
                    chain.push(p.to_path_buf());
                    at = p;
                }
                _ => break,
            }
        }
        std::fs::DirBuilder::new().recursive(true).create(dir)?;
        match std::fs::DirBuilder::new().mode(0o700).create(&folder) {
            Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => return Err(e),
            _ => {}
        }
        if !checked_folder(&folder)? {
            return Err(std::io::Error::other(format!(
                "{} wasn't made",
                folder.display()
            )));
        }
    }
    // Top down: each folder's entry in its parent.
    for path in chain.iter().rev() {
        sync_folder(path)?;
    }
    Ok(folder)
}

/// Reads a billing file of at most 4 KiB, never following a link: it must be
/// a regular file before it's opened, and the opened file must be that same
/// file (device and inode). `Ok(None)` is absence, as is a billing folder
/// that isn't there; an unfit folder or file is an error.
fn read_small(path: &Path) -> std::io::Result<Option<String>> {
    use std::io::Read as _;
    use std::os::unix::fs::MetadataExt as _;
    if !checked_folder(path.parent().unwrap_or(Path::new(".")))? {
        return Ok(None);
    }
    let before = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    if !before.is_file() || before.len() > 4096 {
        return Err(std::io::Error::other(format!(
            "{} isn't a file of at most 4 KiB",
            path.display()
        )));
    }
    let file = std::fs::File::open(path)?;
    let after = file.metadata()?;
    if (after.dev(), after.ino()) != (before.dev(), before.ino()) {
        return Err(std::io::Error::other(format!(
            "{} changed while it was read",
            path.display()
        )));
    }
    let mut text = String::new();
    file.take(4096).read_to_string(&mut text)?;
    Ok(Some(text))
}

/// Creates `path` afresh: anything already there, a link included, is
/// removed first (removing a link never touches its target), and the file
/// is created exclusively, which never follows a link planted meanwhile.
fn create_fresh(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

/// Whether something is at `path`, without following a link: `NotFound` is
/// absence, and any other error is an error, never taken for absence
/// (review of 807dde3a).
fn present(path: &Path) -> Result<bool, RunError> {
    match checked_folder(path.parent().unwrap_or(Path::new("."))) {
        Ok(true) => {}
        Ok(false) => return Ok(false),
        Err(e) => return Err(mark_error(path, "check", e)),
    }
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(mark_error(path, "check", e)),
    }
}

/// The file that holds a mark whose state is uncertain: a mark that may be
/// in place but couldn't be removed durably. While it exists, no turn bills
/// the key on the mark ([`settle_held`]).
pub fn held_path(dir: &Path, agent: &str) -> PathBuf {
    dir.join("billing")
        .join(format!("{}.unsure", crate::state::file_name(agent)))
}

/// Serializes everything that writes, consumes or revokes an agent's mark
/// (use-key, a turn, a policy change), so a turn consumes exactly the mark
/// it read. Held until dropped.
fn mark_lock(dir: &Path, agent: &str) -> Result<std::fs::File, RunError> {
    let path = dir
        .join("billing")
        .join(format!("{}.lock", crate::state::file_name(agent)));
    (|| -> std::io::Result<std::fs::File> {
        use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
        billing_folder(dir)?;
        // Made exclusively when missing; otherwise a regular file, opened
        // without truncation and checked to be the file looked at. Another
        // caller making it first is the existing lock (review of 7c674f52).
        let open_existing = || -> std::io::Result<std::fs::File> {
            let m = std::fs::symlink_metadata(&path)?;
            if !m.is_file() {
                return Err(std::io::Error::other("the lock is a link or not a file"));
            }
            let f = std::fs::OpenOptions::new().write(true).open(&path)?;
            let opened = f.metadata()?;
            if (opened.dev(), opened.ino()) != (m.dev(), m.ino()) {
                return Err(std::io::Error::other(
                    "the lock changed while it was opened",
                ));
            }
            Ok(f)
        };
        let file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => open_existing()?,
            Err(e) => return Err(e),
        };
        file.lock()?;
        Ok(file)
    })()
    .map_err(|e| mark_error(&path, "lock", e))
}

/// A use-key mark: the harness and the Keychain service it was written for,
/// so it bills only that harness's key, and an id, so a turn consumes the
/// very mark it read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mark {
    pub harness: String,
    pub service: String,
    pub id: String,
}

impl Mark {
    /// Whether this mark bills `h`'s key: written for that harness and its
    /// service. A mark for another harness never bills this one.
    pub fn bills(&self, h: Harness) -> bool {
        self.harness == h.as_str() && keychain_service(h) == Some(self.service.as_str())
    }

    fn json(&self) -> String {
        format!(
            "{}\n",
            serde_json::json!({"harness": self.harness, "service": self.service, "id": self.id})
        )
    }
}

/// An agent's mark, as read: none, one, or one that can't be read (a file
/// that can't be opened, isn't a mark, or is an older build's empty mark),
/// which fails closed: it bills nothing and refuses the turn until cleared.
#[derive(Debug)]
pub enum MarkRead {
    None,
    For(Mark),
    Unreadable(RunError),
}

pub fn read_mark(dir: &Path, agent: &str) -> MarkRead {
    let path = key_next_path(dir, agent);
    let text = match read_small(&path) {
        Ok(Some(t)) => t,
        Ok(None) => return MarkRead::None,
        Err(e) => return MarkRead::Unreadable(mark_error(&path, "read", e)),
    };
    let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
    let field = |k: &str| v.get(k).and_then(|f| f.as_str()).map(String::from);
    match (field("harness"), field("service"), field("id")) {
        (Some(harness), Some(service), Some(id)) => MarkRead::For(Mark {
            harness,
            service,
            id,
        }),
        _ => MarkRead::Unreadable(RunError::failure(
            crate::error::CODE_IO,
            format!("{} isn't a use-key mark", path.display()),
        )),
    }
}

/// How [`mark_key_next`] went, which decides what use-key may promise about
/// the next turn's billing.
#[derive(Debug)]
pub enum Marked {
    /// Recorded durably: the next turn bills the key.
    Armed,
    /// Not recorded, and any mark it placed was removed and that removal
    /// synced: the next turn stays on the subscription.
    Absent(RunError),
    /// The mark may be in place, but it is held ([`held_path`]): the next
    /// turn clears it rather than bill the key, and refuses until it can.
    Held(RunError),
    /// The mark may be in place and couldn't be held: the next turn may
    /// bill the key.
    MayBill(RunError),
}

/// The file operations [`mark_key_next`] can fail at, for failure-injection
/// tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Write,
    SyncMark,
    Remove,
    SyncRemoval,
    Hold,
}

/// Writes `mark`: to a temporary file, synced, renamed into place and the
/// folder synced, so a mark is whole or absent after a crash. A failure
/// anywhere removes the mark again; only a removal that's synced promises
/// the subscription ([`Marked::Absent`]). One that isn't holds the mark
/// ([`Marked::Held`]), or, if even that fails, says the next turn may bill
/// the key ([`Marked::MayBill`]). Errors name the file. The caller holds
/// [`mark_lock`].
fn mark_key_next(dir: &Path, agent: &str, mark: &Mark) -> Marked {
    mark_key_next_with(dir, agent, mark, &|_| Ok(()))
}

fn mark_key_next_with(
    dir: &Path,
    agent: &str,
    mark: &Mark,
    fault: &dyn Fn(Step) -> std::io::Result<()>,
) -> Marked {
    use std::io::Write as _;
    let path = key_next_path(dir, agent);
    let tmp = path.with_extension("json.tmp");
    let folder = path.parent().unwrap_or(dir);
    let written = (|| -> std::io::Result<()> {
        fault(Step::Write)?;
        billing_folder(dir)?;
        let mut file = create_fresh(&tmp)?;
        file.write_all(mark.json().as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&tmp, &path)?;
        fault(Step::SyncMark)?;
        sync_folder(&path)
    })();
    let Err(e) = written else {
        return Marked::Armed;
    };
    let _ = std::fs::remove_file(&tmp);
    let error = mark_error(&path, "record", e);
    let removed = (|| -> std::io::Result<()> {
        fault(Step::Remove)?;
        match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        fault(Step::SyncRemoval)?;
        // Synced even when nothing was there: a rename that failed may
        // still have reached the folder.
        if folder.is_dir() {
            sync_folder(&path)?;
        }
        Ok(())
    })();
    if removed.is_ok() {
        return Marked::Absent(error);
    }
    match hold(dir, agent, fault) {
        Ok(()) => Marked::Held(error),
        Err(_) => Marked::MayBill(error),
    }
}

/// Writes the hold on an agent's mark, durably.
fn hold(
    dir: &Path,
    agent: &str,
    fault: &dyn Fn(Step) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let held = held_path(dir, agent);
    fault(Step::Hold)?;
    billing_folder(dir)?;
    // A hold already there is kept, never removed and recreated: a crash
    // between the two would leave an uncertain mark unheld (review of
    // 9989c849). Anything else at the path, a link included, is replaced.
    if std::fs::symlink_metadata(&held).is_ok_and(|m| m.is_file()) {
        std::fs::File::open(&held)?.sync_all()?;
    } else {
        create_fresh(&held)?.sync_all()?;
    }
    sync_folder(&held)
}

/// The note a turn the subscription's usage limit ended leaves, so `agents
/// status` can offer the key. Display only: it never bills anything.
pub fn limited_path(dir: &Path, agent: &str) -> PathBuf {
    dir.join("billing")
        .join(format!("{}.limited", crate::state::file_name(agent)))
}

/// Leaves the note; a failure only costs the status line.
pub fn note_limited(dir: &Path, agent: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    let path = limited_path(dir, agent);
    let tmp = path.with_extension("limited.tmp");
    billing_folder(dir)?;
    create_fresh(&tmp)?.write_all(format!("{}\n", crate::state::now_ms()).as_bytes())?;
    std::fs::rename(&tmp, &path)
}

/// Takes the note away once a turn is admitted; absent is fine.
pub fn clear_limited(dir: &Path, agent: &str) -> std::io::Result<()> {
    // Never through a billing folder that's a link or unfit (option B, #6097
    // item 2): that could remove a file elsewhere.
    if !checked_folder(&dir.join("billing"))? {
        return Ok(());
    }
    match std::fs::remove_file(limited_path(dir, agent)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// What a turn the subscription's usage limit ended says, as its hint and
/// in `agents status`: the key is offered, never charged.
pub fn limit_hint(h: Harness, agent: &str, policy: &crate::policy::Auth) -> String {
    let mut hint = "subscription limit reached".to_string();
    if keychain_service(h).is_some() {
        hint.push_str(&format!(
            "; `{} agents use-key {agent}` bills your API key for its next turn",
            mdkg_format::program::BIN
        ));
        if !policy.api_key_fallback {
            hint.push_str(" (once auth.api_key_fallback allows it)");
        }
    }
    hint
}

/// What `agents status` says about the key for an agent: a hold, a mark
/// waiting for its next turn (or one for another harness, or unreadable),
/// or the offer after a limit.
pub fn billing_note(
    dir: &Path,
    agent: &str,
    h: Harness,
    policy: &crate::policy::Auth,
) -> Option<String> {
    if held_path(dir, agent).is_file() {
        return Some(format!(
            "a key mark couldn't be recorded or removed durably and is held ({}): the next turn \
             clears it instead of billing the API key",
            held_path(dir, agent).display()
        ));
    }
    match read_mark(dir, agent) {
        MarkRead::Unreadable(e) => Some(format!(
            "{}: no turn bills the key on it, and turns are refused until it's removed",
            e.message
        )),
        MarkRead::For(m) if !m.bills(h) => Some(format!(
            "a use-key mark for {} waits, but this agent runs {}: the next turn removes it",
            m.harness,
            h.as_str()
        )),
        MarkRead::For(_) if !policy.api_key_fallback => Some(
            "a use-key mark waits, but auth.api_key_fallback is off: the next turn removes it"
                .to_string(),
        ),
        MarkRead::For(_) => Some("use-key: the next turn bills the API key".to_string()),
        MarkRead::None if limited_path(dir, agent).is_file() => Some(limit_hint(h, agent, policy)),
        MarkRead::None => None,
    }
}

/// The note `agents use-key` sends the agent to wake its supervisor. From
/// the launcher's own actor it's set aside unseen when its message id is
/// one use-key recorded ([`record_wake`]), or its text is exactly this,
/// never merely starting so (orchestrator, 2026-10-29/30, #6209 D-5). Any
/// other note is mail.
pub const USE_KEY_NOTE: &str = "use-key: your next turn bills the user's API key. The mail the \
                                usage limit left unread is still waiting; carry on with it.";

/// The wake notes' ids use-key recorded for an agent, newest last.
fn wake_path(dir: &Path, agent: &str) -> PathBuf {
    dir.join("billing")
        .join(format!("{}.wake", crate::state::file_name(agent)))
}

/// The ids of the wake notes use-key sent the agent, as recorded. A record
/// that's a link, isn't a file or is over 4 KiB counts as none, whole
/// (review of 37c6c894): a note it would have named is then known only by
/// its exact text.
pub fn wake_ids(dir: &Path, agent: &str) -> Vec<String> {
    read_small(&wake_path(dir, agent))
        .ok()
        .flatten()
        .map(|text| {
            text.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

/// How many wake-note ids the record keeps: well more than the notes an
/// agent leaves unread, and within [`read_small`]'s 4 KiB.
const WAKE_IDS: usize = 64;

/// Records `id`, the wake note use-key is about to send, keeping the last
/// [`WAKE_IDS`], under the mark lock, so two use-key runs can't lose each
/// other's id (review of 37c6c894).
pub fn record_wake(dir: &Path, agent: &str, id: &str) -> Result<(), RunError> {
    use std::io::Write as _;
    let path = wake_path(dir, agent);
    let _lock = mark_lock(dir, agent)?;
    let mut ids = wake_ids(dir, agent);
    ids.push(id.to_string());
    let keep = ids.len().saturating_sub(WAKE_IDS);
    let tmp = path.with_extension("wake.tmp");
    (|| -> std::io::Result<()> {
        billing_folder(dir)?;
        let mut file = create_fresh(&tmp)?;
        file.write_all(format!("{}\n", ids[keep..].join("\n")).as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&tmp, &path)?;
        sync_folder(&path)
    })()
    .map_err(|e| mark_error(&path, "record the wake note for", e))
}

/// `agents use-key`: bills the agent's next turn, one, on its harness's API
/// key. It refuses unless the policy allows it (`auth.api_key_fallback`)
/// and the harness has a key fallback (Claude, Codex; not OpenCode). A
/// hold on an earlier mark is settled first, or the command refuses, so a
/// new mark is never reported armed and then cleared. The mark is written
/// durably, bound to the harness; one that fails is removed again or held,
/// as [`mark_key_next`] says, and the error says what the next turn does.
/// Who may run it is the CLI's to check: never an agent.
pub fn use_key(
    dir: &Path,
    agent: &str,
    h: Harness,
    policy: &crate::policy::Auth,
) -> Result<PathBuf, RunError> {
    let Some(service) = keychain_service(h) else {
        return Err(RunError::usage(
            CODE_UNSUPPORTED,
            format!("{} has no API key fallback", h.as_str()),
        ));
    };
    if !policy.api_key_fallback {
        return Err(RunError::usage(
            CODE_UNSUPPORTED,
            format!("{agent}'s policy doesn't allow use-key"),
        )
        .with_hint(
            "turn on `auth.api_key_fallback` in the agent's policy, with a key in the Keychain",
        ));
    }
    let _lock = mark_lock(dir, agent)?;
    // A hold that can't be removed would clear the new mark at the next
    // turn: refuse rather than report it armed (review of 807dde3a).
    if let Some(w) = settle_held_locked(dir, agent)? {
        return Err(RunError::failure(crate::error::CODE_IO, w).with_hint(held_hint(dir, agent)));
    }
    let mark = Mark {
        harness: h.as_str().to_string(),
        service: service.to_string(),
        id: uuid::Uuid::now_v7().to_string(),
    };
    marked(dir, agent, mark_key_next(dir, agent, &mark))
}

/// What a mark's outcome means for the next turn: the subscription is
/// promised only once a failed mark's removal is synced.
fn marked(dir: &Path, agent: &str, m: Marked) -> Result<PathBuf, RunError> {
    match m {
        Marked::Armed => Ok(key_next_path(dir, agent)),
        Marked::Absent(e) => {
            Err(e.and_hint("nothing is marked: the next turn stays on the subscription"))
        }
        Marked::Held(e) => Err(e.and_hint(format!(
            "the mark may be in place, so {} holds it: the next turn clears it instead of \
             billing the API key, and is refused until it can",
            held_path(dir, agent).display()
        ))),
        Marked::MayBill(e) => Err(e.and_hint(
            "the mark may be in place and couldn't be held: the next turn may bill the API key",
        )),
    }
}

/// Consumes `mark`, the one the turn read, before the turn is admitted and
/// its key charged: renamed away (which fails if it's gone, so a missing
/// mark is never a consume), checked to be the same mark, and the rename
/// synced. Any failure refuses the turn: it never falls back to another
/// account, the harness's own key included.
pub fn consume_mark(dir: &Path, agent: &str, mark: &Mark) -> Result<(), RunError> {
    let _lock = mark_lock(dir, agent)?;
    let path = key_next_path(dir, agent);
    // A hold written since the turn read the mark (use-key's write that
    // couldn't be synced or undone) means the mark may not stand: never
    // billed on (review of bc72ad53).
    if present(&held_path(dir, agent))? {
        return Err(RunError::failure(
            crate::error::CODE_IO,
            format!(
                "the use-key mark in {} is held: it couldn't be recorded durably",
                path.display()
            ),
        )
        .with_hint(held_hint(dir, agent)));
    }
    // Compared before it's moved, under the lock every writer takes, so a
    // mark that replaced the one read is never moved aside (review of
    // 807dde3a).
    match read_mark(dir, agent) {
        MarkRead::For(m) if m.id == mark.id => {}
        MarkRead::None => {
            return Err(RunError::failure(
                crate::error::CODE_IO,
                format!("the use-key mark in {} is gone", path.display()),
            ));
        }
        _ => {
            return Err(RunError::failure(
                crate::error::CODE_IO,
                format!(
                    "the use-key mark in {} changed after this turn read it",
                    path.display()
                ),
            ));
        }
    }
    let taken = path.with_extension("json.consumed");
    std::fs::rename(&path, &taken).map_err(|e| mark_error(&path, "consume", e))?;
    sync_folder(&path).map_err(|e| mark_error(&path, "consume", e))?;
    let _ = std::fs::remove_file(&taken);
    Ok(())
}

/// Revokes the agent's mark durably: when the policy doesn't allow a key,
/// or the mark is for another harness, so it can't bill a later turn. A
/// mark that can't be removed is held instead, which blocks billing on it
/// until it's cleared; one that can't be held either refuses the turn.
/// `Ok(Some(_))` is a warning.
pub fn revoke_mark(dir: &Path, agent: &str) -> Result<Option<String>, RunError> {
    revoke(dir, agent, None)
}

/// [`revoke_mark`] for the mark with id `id` only: under the lock, a mark
/// written since it was read (a new use-key) is left alone (review of
/// 807dde3a).
pub fn revoke_mark_read(dir: &Path, agent: &str, id: &str) -> Result<Option<String>, RunError> {
    revoke(dir, agent, Some(id))
}

/// A billing folder that isn't fit to use ([`checked_folder`]) refuses
/// every turn, fallback on or off, so its error says what to fix: the
/// folder, not a mark (review of 7c674f52).
fn unfit_folder(dir: &Path) -> Result<(), RunError> {
    let folder = dir.join("billing");
    checked_folder(&folder).map(|_| ()).map_err(|e| {
        RunError::failure(crate::error::CODE_IO, e.to_string()).with_hint(format!(
            "make it a folder of yours that no one else can write: `chmod 700 {}`, or \
             remove it if it isn't yours; turns are refused until then",
            folder.display()
        ))
    })
}

fn revoke(dir: &Path, agent: &str, id: Option<&str>) -> Result<Option<String>, RunError> {
    unfit_folder(dir)?;
    let path = key_next_path(dir, agent);
    let held = held_path(dir, agent);
    // An error here isn't absence: it goes on to the revoke, which fails
    // closed.
    if !present(&path).unwrap_or(true) && !present(&held).unwrap_or(true) {
        return Ok(None);
    }
    let lock = mark_lock(dir, agent);
    if lock.is_ok()
        && let Some(id) = id
        // Only a mark read whole, with another id, is a newer one; one that
        // can't be read is revoked, failing closed (review of 9989c849).
        && matches!(read_mark(dir, agent), MarkRead::For(m) if m.id != id)
        && !present(&held).unwrap_or(true)
    {
        return Ok(None);
    }
    let cleared = lock
        .as_ref()
        .map_err(Clone::clone)
        .and_then(|_| clear_key_next(dir, agent));
    match cleared {
        Ok(()) => Ok(clear_held(dir, agent).err().map(|e| e.message)),
        Err(e) if present(&held).unwrap_or(false) => {
            Ok(Some(format!("{}; it stays held", e.message)))
        }
        Err(e) => match hold(dir, agent, &|_| Ok(())) {
            Ok(()) => Ok(Some(format!(
                "{}; {} holds it, so no turn bills the key on it",
                e.message,
                held.display()
            ))),
            Err(_) => Err(e.and_hint(format!(
                "the mark can't be removed or held, so turns are refused until {} is removed",
                path.display()
            ))),
        },
    }
}

/// Settles a held mark ([`Marked::Held`]) before a turn chooses its
/// account: the mark is cleared durably and the hold removed, so no turn
/// bills the key on it. A mark that still can't be cleared refuses the
/// turn. `Ok(Some(_))` is a warning: the mark is gone but the hold stayed.
pub fn settle_held(dir: &Path, agent: &str) -> Result<Option<String>, RunError> {
    unfit_folder(dir)?;
    if !present(&held_path(dir, agent)).map_err(|e| e.and_hint(held_hint(dir, agent)))? {
        return Ok(None);
    }
    let _lock = mark_lock(dir, agent).map_err(|e| e.and_hint(held_hint(dir, agent)))?;
    settle_held_locked(dir, agent)
}

fn held_hint(dir: &Path, agent: &str) -> String {
    format!(
        "an earlier mark couldn't be recorded or removed durably, so no turn bills the API key \
         on it, and turns are refused until it's cleared: remove {} and {}, or turn \
         auth.api_key_fallback off",
        key_next_path(dir, agent).display(),
        held_path(dir, agent).display()
    )
}

fn settle_held_locked(dir: &Path, agent: &str) -> Result<Option<String>, RunError> {
    if !present(&held_path(dir, agent)).map_err(|e| e.and_hint(held_hint(dir, agent)))? {
        return Ok(None);
    }
    clear_key_next(dir, agent).map_err(|e| e.and_hint(held_hint(dir, agent)))?;
    Ok(clear_held(dir, agent).err().map(|e| e.message))
}

/// Removes the hold, durably; absent is fine.
pub fn clear_held(dir: &Path, agent: &str) -> Result<(), RunError> {
    let held = held_path(dir, agent);
    match std::fs::remove_file(&held) {
        Ok(()) => sync_folder(&held).map_err(|e| mark_error(&held, "clear the hold on", e)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(mark_error(&held, "clear the hold on", e)),
    }
}

/// Whether a mark file is there, read or not.
pub fn key_next(dir: &Path, agent: &str) -> bool {
    key_next_path(dir, agent).exists()
}

/// Removes the mark durably (removed, then the folder synced). An error
/// means the mark may still be there. Revoking, not consuming: a missing
/// mark is fine here, never in [`consume_mark`].
fn clear_key_next(dir: &Path, agent: &str) -> Result<(), RunError> {
    let path = key_next_path(dir, agent);
    match std::fs::remove_file(&path) {
        Ok(()) => sync_folder(&path).map_err(|e| mark_error(&path, "clear", e)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(mark_error(&path, "clear", e)),
    }
}

/// Whether `line`, a line of a harness's final output, is that harness's own
/// usage-limit error in the form its CLI prints it, not a phrase quoted
/// inside some other line (its adapter's
/// [`usage_limit`](crate::harness::HarnessAdapter::usage_limit)). It only
/// offers the key (`agents use-key`), never charges it, so a miss costs
/// only the hint.
pub fn usage_limit_error(h: Harness, line: &str) -> bool {
    h.adapter().usage_limit(line)
}

/// Whether a failed turn's output says the subscription hit its usage
/// limit, after which the key fallback bills the agent's next turn.
pub fn usage_limited(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    [
        "usage limit",
        "usage_limit",
        "hit your limit",
        "hit your weekly limit",
        "out of extra usage",
    ]
    .iter()
    .any(|p| t.contains(p))
        || t.match_indices("hit your ").any(|(i, p)| {
            let rest = &t[i + p.len()..];
            let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
            digits > 0 && rest[digits..].starts_with("-hour limit")
        })
}

/// Whether text says the model is at capacity: the provider turned the
/// request away, not the subscription's limit. A long resumed session can
/// keep failing so while fresh ones of the same model work, so after two
/// such turns in a row the supervisor starts the agent's next turn in a
/// fresh session; nothing is retried within a turn. The wording is Codex's
/// (friction log item 21). Claude's "Overloaded" (HTTP 529) isn't matched:
/// that's provider-wide, and a fresh session wouldn't help. OpenCode passes
/// its provider's text through, so an OpenAI model's matches. A turn counts
/// only the harness's own error form ([`capacity_error`]).
pub fn model_at_capacity(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    ["model is at capacity", "model_at_capacity"]
        .iter()
        .any(|p| t.contains(p))
}

/// Whether `line`, a harness's final error line, is its own capacity
/// error, not the phrase quoted in some other line (review of 6b4ee963): an
/// `ERROR:` line whose message starts "Selected model is at capacity", or a
/// JSON error event (Codex's, or OpenCode's passing its provider's text on)
/// whose code is `model_at_capacity` or whose message starts so.
pub fn capacity_error(line: &str) -> bool {
    const MESSAGE: &str = "selected model is at capacity";
    let starts = |m: &str| m.trim_start().to_ascii_lowercase().starts_with(MESSAGE);
    let line = line.trim();
    if let Some(rest) = line
        .get(..6)
        .filter(|p| p.eq_ignore_ascii_case("error:"))
        .map(|_| &line[6..])
    {
        return starts(rest);
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return false;
    };
    let nested = v.get("error").filter(|e| e.is_object());
    let is_error = nested.is_some()
        || matches!(
            v.get("type").and_then(|t| t.as_str()),
            Some("error" | "turn.failed")
        );
    let event = nested.unwrap_or(&v);
    let text = |e: &serde_json::Value, k: &str| e.get(k).and_then(|f| f.as_str()).map(String::from);
    is_error
        && (text(event, "code").as_deref() == Some("model_at_capacity")
            || text(event, "message").is_some_and(|m| starts(&m))
            || event
                .get("data")
                .and_then(|d| text(d, "message"))
                .is_some_and(|m| starts(&m)))
}

/// The account a harness would bill now, for `agents status` and `doctor`,
/// without reading any key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct AuthStatus {
    /// `subscription`, `own_config` (OpenCode) or `none`; never `api_key`
    /// (0.7.0's fallback), since the key bills only a turn use-key marks.
    pub source: String,
    /// The login method the harness reports, when logged in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    pub api_key_fallback: bool,
    /// Whether the Keychain holds the fallback's key.
    pub key_in_keychain: bool,
}

pub fn status(
    h: Harness,
    program: &Path,
    cwd: &Path,
    policy: &crate::policy::Auth,
    keys: &KeySource,
    env: &crate::env::Environment,
) -> AuthStatus {
    let service = keychain_service(h);
    let key_in_keychain = service.is_some_and(|s| keys.has(s));
    let (source, method) = match login(h, program, cwd, env) {
        Login::In {
            method,
            subscription: true,
        } => ("subscription", Some(method)),
        Login::Own => ("own_config", None),
        // The key bills only a turn use-key marks, never on its own.
        Login::In { method, .. } => ("none", Some(method)),
        Login::Out => ("none", None),
    };
    AuthStatus {
        source: source.into(),
        method,
        api_key_fallback: policy.api_key_fallback,
        key_in_keychain,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_secret_never_shows_and_is_taken_out_of_text() {
        let s = Secret("sk-test-123".into());
        assert_eq!(format!("{s:?}"), REDACTED);
        assert_eq!(
            s.redact("key sk-test-123 twice sk-test-123"),
            "key [redacted] twice [redacted]"
        );
        assert_eq!(Secret(String::new()).redact("x"), "x");
    }

    #[test]
    fn a_failed_mark_is_removed_held_or_said_to_maybe_bill_as_far_as_it_got() {
        let dir = std::env::temp_dir().join(format!("mdkg-mark-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let fail_at = |steps: &'static [Step]| {
            move |s: Step| {
                if steps.contains(&s) {
                    Err(std::io::Error::other("injected"))
                } else {
                    Ok(())
                }
            }
        };
        let hint = |m: Marked| match marked(&dir, "lane", m) {
            Ok(_) => "the next turn bills the API key".to_string(),
            Err(e) => e.hint.unwrap_or_default(),
        };
        let mark = Mark {
            harness: "claude".into(),
            service: "mdkg-anthropic".into(),
            id: "m1".into(),
        };
        // The first mark creates its folder and is armed.
        let m = mark_key_next(&dir, "lane", &mark);
        assert!(matches!(m, Marked::Armed), "{m:?}");
        assert!(hint(m).contains("the next turn bills the API key"));
        clear_key_next(&dir, "lane").unwrap();
        // The folder sync after the rename fails: the mark is removed and
        // that removal synced, so the subscription is promised.
        let m = mark_key_next_with(&dir, "lane", &mark, &fail_at(&[Step::SyncMark]));
        assert!(matches!(m, Marked::Absent(_)), "{m:?}");
        assert!(!key_next(&dir, "lane"));
        assert!(hint(m).contains("stays on the subscription"));
        assert!(!key_next(&dir, "lane"));
        // A failure before the rename leaves nothing either.
        let m = mark_key_next_with(&dir, "lane", &mark, &fail_at(&[Step::Write]));
        assert!(matches!(m, Marked::Absent(_)), "{m:?}");
        // The removal's sync fails too: the mark is held, never promised.
        let m = mark_key_next_with(
            &dir,
            "lane",
            &mark,
            &fail_at(&[Step::SyncMark, Step::Remove]),
        );
        assert!(matches!(m, Marked::Held(_)), "{m:?}");
        assert!(key_next(&dir, "lane") && held_path(&dir, "lane").is_file());
        let h = hint(m);
        assert!(h.contains("instead of billing the API key"), "{h}");
        assert!(!h.contains("stays on the subscription"), "{h}");
        // The next turn settles it: the mark and the hold are cleared.
        assert_eq!(settle_held(&dir, "lane").unwrap(), None);
        assert!(!key_next(&dir, "lane") && !held_path(&dir, "lane").exists());
        // Nothing can be removed or held: the hint says it may bill.
        let m = mark_key_next_with(
            &dir,
            "lane",
            &mark,
            &fail_at(&[Step::SyncMark, Step::SyncRemoval, Step::Hold]),
        );
        assert!(matches!(m, Marked::MayBill(_)), "{m:?}");
        assert!(hint(m).contains("may bill the API key"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_mark_bills_only_its_harness_and_is_consumed_only_as_read() {
        let dir = std::env::temp_dir().join(format!("mdkg-mark-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let on = crate::policy::Auth {
            api_key_fallback: true,
            ..crate::policy::Auth::default()
        };
        use_key(&dir, "lane", Harness::Claude, &on).unwrap();
        let MarkRead::For(mark) = read_mark(&dir, "lane") else {
            panic!("a mark")
        };
        assert!(mark.bills(Harness::Claude));
        assert!(!mark.bills(Harness::Codex), "never another harness's key");
        assert!(!mark.bills(Harness::OpenCode));
        // A mark replaced since it was read isn't the one consumed: it's
        // put back, and the consume fails.
        let stale = Mark {
            id: "older".into(),
            ..mark.clone()
        };
        assert!(consume_mark(&dir, "lane", &stale).is_err());
        assert!(key_next(&dir, "lane"), "the newer mark is kept");
        consume_mark(&dir, "lane", &mark).unwrap();
        assert!(!key_next(&dir, "lane"));
        // A missing mark is never a successful consume.
        assert!(consume_mark(&dir, "lane", &mark).is_err());
        // An older build's empty mark, or any other file, fails closed.
        std::fs::write(key_next_path(&dir, "lane"), "{}\n").unwrap();
        assert!(matches!(read_mark(&dir, "lane"), MarkRead::Unreadable(_)));
        // Revoking removes it; a hold on an earlier mark is settled before
        // use-key reports a new one armed.
        assert_eq!(revoke_mark(&dir, "lane").unwrap(), None);
        assert!(!key_next(&dir, "lane"));
        std::fs::write(held_path(&dir, "lane"), "").unwrap();
        use_key(&dir, "lane", Harness::Codex, &on).unwrap();
        assert!(!held_path(&dir, "lane").exists());
        assert!(matches!(read_mark(&dir, "lane"), MarkRead::For(m) if m.bills(Harness::Codex)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn billing_writes_never_follow_a_planted_link() {
        use std::os::unix::fs::symlink;
        let dir = std::env::temp_dir().join(format!("mdkg-mark-{}", uuid::Uuid::now_v7()));
        let billing = dir.join("billing");
        std::fs::create_dir_all(&billing).unwrap();
        let victim = dir.join("victim");
        std::fs::write(&victim, "keep").unwrap();
        let on = crate::policy::Auth {
            api_key_fallback: true,
            ..crate::policy::Auth::default()
        };
        // Links at the mark's temporary file, the hold, the limit note and
        // its temporary file: each is replaced, never written through.
        for name in [
            "lane.json.tmp",
            "lane.limited",
            "lane.limited.tmp",
            "lane.unsure",
            "lane.wake.tmp",
        ] {
            symlink(&victim, billing.join(name)).unwrap();
        }
        use_key(&dir, "lane", Harness::Claude, &on).unwrap();
        note_limited(&dir, "lane").unwrap();
        hold(&dir, "lane", &|_| Ok(())).unwrap();
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep");
        assert!(
            !std::fs::symlink_metadata(limited_path(&dir, "lane"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        // A billing folder that's a link is refused outright.
        let other = std::env::temp_dir().join(format!("mdkg-mark-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&other).unwrap();
        let elsewhere = other.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        symlink(&elsewhere, other.join("billing")).unwrap();
        assert!(use_key(&other, "lane", Harness::Claude, &on).is_err());
        assert!(std::fs::read_dir(&elsewhere).unwrap().next().is_none());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&other);
    }

    #[test]
    fn a_mark_held_after_the_turn_read_it_is_never_consumed() {
        // use-key's write published the mark, then couldn't sync or undo
        // it and held it: a turn that read the mark meanwhile doesn't bill
        // on it.
        let dir = std::env::temp_dir().join(format!("mdkg-mark-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let on = crate::policy::Auth {
            api_key_fallback: true,
            ..crate::policy::Auth::default()
        };
        use_key(&dir, "lane", Harness::Claude, &on).unwrap();
        let MarkRead::For(mark) = read_mark(&dir, "lane") else {
            panic!("a mark")
        };
        hold(&dir, "lane", &|_| Ok(())).unwrap();
        let e = consume_mark(&dir, "lane", &mark).expect_err("held");
        assert!(e.message.contains("is held"), "{}", e.message);
        assert!(key_next(&dir, "lane"), "not consumed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn revoking_a_read_mark_never_removes_a_newer_one() {
        let dir = std::env::temp_dir().join(format!("mdkg-mark-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let on = crate::policy::Auth {
            api_key_fallback: true,
            ..crate::policy::Auth::default()
        };
        use_key(&dir, "lane", Harness::Claude, &on).unwrap();
        let MarkRead::For(old) = read_mark(&dir, "lane") else {
            panic!("a mark")
        };
        // A new use-key since the turn read the old mark: revoking the old
        // one leaves the new one alone.
        use_key(&dir, "lane", Harness::Codex, &on).unwrap();
        assert_eq!(revoke_mark_read(&dir, "lane", &old.id).unwrap(), None);
        assert!(matches!(read_mark(&dir, "lane"), MarkRead::For(m) if m.bills(Harness::Codex)));
        let MarkRead::For(new) = read_mark(&dir, "lane") else {
            panic!("a mark")
        };
        revoke_mark_read(&dir, "lane", &new.id).unwrap();
        assert!(!key_next(&dir, "lane"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn use_key_refuses_while_a_hold_cant_be_removed_and_reads_fail_closed() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("mdkg-mark-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let on = crate::policy::Auth {
            api_key_fallback: true,
            ..crate::policy::Auth::default()
        };
        // The lock file exists; then a hold that the read-only folder
        // keeps: a new mark would be cleared by it, so none is armed.
        use_key(&dir, "lane", Harness::Claude, &on).unwrap();
        revoke_mark(&dir, "lane").unwrap();
        hold(&dir, "lane", &|_| Ok(())).unwrap();
        let billing = dir.join("billing");
        std::fs::set_permissions(&billing, std::fs::Permissions::from_mode(0o500)).unwrap();
        let refused = use_key(&dir, "lane", Harness::Claude, &on);
        // A folder that can't be looked into fails closed: not "no hold".
        std::fs::set_permissions(&billing, std::fs::Permissions::from_mode(0o000)).unwrap();
        let unreadable = settle_held(&dir, "lane");
        std::fs::set_permissions(&billing, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(refused.is_err(), "{refused:?}");
        assert!(!key_next(&dir, "lane"), "nothing armed");
        assert!(unreadable.is_err(), "{unreadable:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_hold_already_in_place_is_kept_never_removed_and_recreated() {
        use std::os::unix::fs::MetadataExt;
        let dir = std::env::temp_dir().join(format!("mdkg-mark-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        hold(&dir, "lane", &|_| Ok(())).unwrap();
        let before = std::fs::metadata(held_path(&dir, "lane")).unwrap().ino();
        hold(&dir, "lane", &|_| Ok(())).unwrap();
        let after = std::fs::metadata(held_path(&dir, "lane")).unwrap().ino();
        assert_eq!(before, after, "the same file: no window without a hold");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn revoking_a_read_mark_that_became_unreadable_fails_closed() {
        let dir = std::env::temp_dir().join(format!("mdkg-mark-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let on = crate::policy::Auth {
            api_key_fallback: true,
            ..crate::policy::Auth::default()
        };
        use_key(&dir, "lane", Harness::Claude, &on).unwrap();
        let MarkRead::For(read) = read_mark(&dir, "lane") else {
            panic!("a mark")
        };
        // Unreadable since it was read: not a newer mark, so it's revoked.
        std::fs::write(key_next_path(&dir, "lane"), "not a mark").unwrap();
        revoke_mark_read(&dir, "lane", &read.id).unwrap();
        assert!(!key_next(&dir, "lane"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn use_key_creates_a_missing_run_folder_too() {
        // An agent that has never run: neither the run folder nor billing/
        // exists yet. Both are created (and synced) for the mark.
        let root = std::env::temp_dir().join(format!("mdkg-mark-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&root).unwrap();
        let dir = root.join("run");
        let on = crate::policy::Auth {
            api_key_fallback: true,
            ..crate::policy::Auth::default()
        };
        use_key(&dir, "lane", Harness::Claude, &on).unwrap();
        assert!(matches!(read_mark(&dir, "lane"), MarkRead::For(_)));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_unfit_billing_folder_or_mark_is_refused_never_used() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("mdkg-mark-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let on = crate::policy::Auth {
            api_key_fallback: true,
            ..crate::policy::Auth::default()
        };
        // Made owner-only.
        use_key(&dir, "lane", Harness::Claude, &on).unwrap();
        let billing = dir.join("billing");
        let mode = std::fs::metadata(&billing).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "{mode:o}");
        // Writable by others: refused, and its mark isn't read.
        std::fs::set_permissions(&billing, std::fs::Permissions::from_mode(0o770)).unwrap();
        assert!(use_key(&dir, "lane", Harness::Claude, &on).is_err());
        // The refusal a turn gets, fallback on or off, names the folder's
        // fault and its fix, not a mark (review of 7c674f52).
        for e in [
            revoke_mark(&dir, "lane").unwrap_err(),
            settle_held(&dir, "lane").unwrap_err(),
        ] {
            assert!(e.message.contains("is writable by others"), "{}", e.message);
            assert!(e.hint.unwrap_or_default().contains("chmod 700"));
        }
        assert!(matches!(read_mark(&dir, "lane"), MarkRead::Unreadable(_)));
        assert!(clear_limited(&dir, "lane").is_err());
        std::fs::set_permissions(&billing, std::fs::Permissions::from_mode(0o700)).unwrap();
        // A mark over 4 KiB is never read whole: unreadable, failing closed.
        std::fs::write(key_next_path(&dir, "lane"), "x".repeat(5000)).unwrap();
        assert!(matches!(read_mark(&dir, "lane"), MarkRead::Unreadable(_)));
        // A billing folder that's a link isn't used, even to remove a note.
        let other = std::env::temp_dir().join(format!("mdkg-mark-{}", uuid::Uuid::now_v7()));
        let elsewhere = other.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("lane.limited"), "keep").unwrap();
        std::os::unix::fs::symlink(&elsewhere, other.join("billing")).unwrap();
        assert!(clear_limited(&other, "lane").is_err());
        assert!(elsewhere.join("lane.limited").exists());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&other);
    }

    #[test]
    fn two_first_time_lockers_both_get_the_lock() {
        // The second to make the lock file opens the one the first made,
        // never failing because it appeared (review of 7c674f52).
        let dir = std::env::temp_dir().join(format!("mdkg-mark-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let dir = dir.clone();
                std::thread::spawn(move || mark_lock(&dir, "lane").map(drop))
            })
            .collect();
        for t in threads {
            t.join().unwrap().unwrap();
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn capacity_is_the_harness_s_own_error_never_a_quote() {
        for own in [
            "ERROR: Selected model is at capacity. Please try a different model.",
            "error: Selected model is at capacity.",
            "{\"type\":\"error\",\"message\":\"Selected model is at capacity.\"}",
            "{\"type\":\"turn.failed\",\"error\":{\"code\":\"model_at_capacity\"}}",
            "{\"type\":\"error\",\"error\":{\"data\":{\"message\":\"Selected model is at capacity\"}}}",
        ] {
            assert!(capacity_error(own), "{own}");
        }
        for quoted in [
            "ERROR: tests failed; mail says Selected model is at capacity",
            "mail says: ERROR: Selected model is at capacity",
            "{\"type\":\"item.completed\",\"message\":\"Selected model is at capacity\"}",
            "Selected model is at capacity",
        ] {
            assert!(!capacity_error(quoted), "{quoted}");
        }
    }

    #[test]
    fn usage_limit_errors_are_recognized() {
        assert!(usage_limited("Claude AI usage limit reached|1792900000"));
        assert!(usage_limited(
            "ERROR: You've hit your usage limit. Upgrade to Pro"
        ));
        assert!(usage_limited("{\"code\":\"usage_limit_reached\"}"));
        assert!(!usage_limited("the agent exited with exit status: 1"));
        assert!(model_at_capacity(
            "ERROR: Selected model is at capacity. Please try a different model."
        ));
        assert!(model_at_capacity("{\"code\":\"model_at_capacity\"}"));
        assert!(!model_at_capacity("usage limit reached"));
        assert!(!usage_limited("Selected model is at capacity"));
        // The exact lines, never a phrase inside another.
        assert!(usage_limit_error(
            Harness::Claude,
            "{\"type\":\"result\",\"is_error\":true,\"result\":\"Claude AI usage limit reached|1792900000\"}"
        ));
        assert!(usage_limit_error(
            Harness::Claude,
            "Claude AI usage limit reached|1792900000"
        ));
        assert!(usage_limit_error(
            Harness::Codex,
            "ERROR: You've hit your usage limit. Upgrade to Pro"
        ));
        assert!(usage_limit_error(
            Harness::Codex,
            "{\"code\":\"usage_limit_reached\"}"
        ));
        assert!(usage_limit_error(
            Harness::Codex,
            "ERROR: You\u{2019}ve hit your usage limit. Try again later."
        ));
        assert!(usage_limit_error(
            Harness::Codex,
            "{\"type\":\"error\",\"message\":\"You've hit your usage limit. Try again later.\"}"
        ));
        assert!(usage_limit_error(
            Harness::Codex,
            "{\"type\":\"turn.failed\",\"error\":{\"message\":\"You've hit your usage limit.\"}}"
        ));
        // A message on some other event is quoted text, not the error.
        assert!(!usage_limit_error(
            Harness::Codex,
            "{\"type\":\"item.completed\",\"message\":\"You've hit your usage limit.\"}"
        ));
        assert!(!usage_limit_error(
            Harness::Claude,
            "{\"type\":\"result\",\"is_error\":false,\"result\":\"Claude AI usage limit reached|1\"}"
        ));
        for quoted in [
            "ERROR: tests failed; mail said 'usage limit reached'",
            "ERROR: something else (Claude AI usage limit reached)",
            "{\"type\":\"assistant\",\"text\":\"Claude AI usage limit reached\"}",
        ] {
            assert!(!usage_limit_error(Harness::Claude, quoted), "{quoted}");
            assert!(!usage_limit_error(Harness::Codex, quoted), "{quoted}");
        }
        assert!(usage_limit_error(
            Harness::OpenCode,
            "{\"type\":\"error\",\"error\":{\"name\":\"APIError\",\"data\":{\"message\":\"usage_limit_reached\"}}}"
        ));
        assert!(!usage_limit_error(
            Harness::OpenCode,
            "{\"code\":\"usage_limit_reached\"}"
        ));
    }

    /// Claude's and OpenCode's current limit shapes (synthetic values).
    #[test]
    fn current_claude_and_opencode_limit_shapes_are_recognized() {
        let claude = |l: &str| usage_limit_error(Harness::Claude, l);
        assert!(claude(
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":1793000000,"rateLimitType":"five_hour"}}"#
        ));
        assert!(!claude(
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","resetsAt":1793000000}}"#
        ));
        // A 429 alone may be request-rate throttling (review of 0f0a8dcb).
        assert!(!claude(
            r#"{"type":"result","is_error":true,"api_error_status":429,"result":"x"}"#
        ));
        assert!(!claude(
            r#"{"type":"result","is_error":false,"api_error_status":429,"result":"x"}"#
        ));
        for text in [
            r"You've hit your weekly limit \u00b7 resets 3pm",
            r"You\u2019ve hit your 5-hour limit \u00b7 resets 9am",
        ] {
            let result = format!(r#"{{"type":"result","is_error":true,"result":"{text}"}}"#);
            assert!(claude(&result), "{result}");
        }
        assert!(claude("You've hit your 24-hour limit · resets 1am"));
        assert!(!claude("You've hit your -hour limit"));
        assert!(!claude(
            r#"{"type":"assistant","text":"You've hit your weekly limit"}"#
        ));
        assert!(usage_limited("You've hit your weekly limit · resets 3pm"));
        assert!(usage_limited("you've hit your 5-hour limit"));
        assert!(!usage_limited("hit your 5 hour target"));
        let reset = |l: &str| Harness::Claude.adapter().usage_limit_reset(l);
        assert_eq!(
            reset(
                r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":1793000000,"rateLimitType":"seven_day"}}"#
            ),
            Some((1793000000, Some("seven_day".into())))
        );
        assert_eq!(
            reset(
                r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":1}}"#
            ),
            None
        );
        for h in [Harness::Codex, Harness::OpenCode] {
            assert_eq!(
                h.adapter().usage_limit_reset(r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":1}}"#),
                None
            );
        }
        // The whole stream: an error result counts after a rejection, or
        // with the limit wording; a 429 alone doesn't.
        let stream = |lines: &[&str]| {
            let lines: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
            Harness::Claude.adapter().usage_limit_in(&lines)
        };
        let rejected = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":1793000000}}"#;
        let throttled = r#"{"type":"result","is_error":true,"api_error_status":429,"result":"x"}"#;
        let other = r#"{"type":"result","is_error":true,"result":"some other failure"}"#;
        let worded = r#"{"type":"result","is_error":true,"result":"You've hit your weekly limit"}"#;
        let ok = r#"{"type":"result","is_error":false,"result":"done"}"#;
        assert!(stream(&[rejected, throttled]));
        assert!(stream(&[worded]));
        assert!(stream(&[rejected]), "a stream cut off at the rejection");
        assert!(!stream(&[throttled]), "a 429 with no rejection");
        assert!(
            stream(&[rejected, other]),
            "an error after a rejection (review of 94ff686f)"
        );
        assert!(!stream(&[rejected, ok]));
        assert!(!stream(&[other]), "an error with no rejection or wording");
        assert!(!stream(&[]));
        // Throttling: a 429 error result with no limit named.
        let throttled_in = |h: Harness, lines: &[&str]| {
            let lines: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
            h.adapter().throttled_in(&lines)
        };
        assert!(throttled_in(Harness::Claude, &[throttled]));
        assert!(!throttled_in(Harness::Claude, &[rejected, throttled]));
        assert!(!throttled_in(Harness::Claude, &[worded]));
        assert!(!throttled_in(Harness::Claude, &[other]));
        assert!(!throttled_in(Harness::Claude, &[throttled, ok]));
        for h in [Harness::Codex, Harness::OpenCode] {
            assert!(!throttled_in(h, &[throttled]));
        }
        let opencode = |l: &str| usage_limit_error(Harness::OpenCode, l);
        // Its quota form, by type, and by status with each body (an
        // object, or the JSON text of one).
        assert!(opencode(
            r#"{"type":"error","error":{"type":"provider.quota","status":429,"response":{"body":{}}}}"#
        ));
        assert!(opencode(
            r#"{"type":"error","error":{"type":"provider.other","status":429,"response":{"body":{"type":"FreeUsageLimitError"}}}}"#
        ));
        assert!(opencode(
            r#"{"type":"error","error":{"status":429,"response":{"body":"{\"type\":\"GoUsageLimitError\",\"message\":\"x\"}"}}}"#
        ));
        // Its API-error form, the message under error.data.message.
        assert!(opencode(
            r#"{"type":"error","error":{"name":"APIError","data":{"message":"{\"type\":\"FreeUsageLimitError\"}","statusCode":429}}}"#
        ));
        assert!(opencode(
            r#"{"type":"error","error":{"name":"APIError","data":{"message":"GoUsageLimitError: quota used","statusCode":429}}}"#
        ));
        // Not a limit: another 429 body, another status, another event.
        assert!(!opencode(
            r#"{"type":"error","error":{"status":429,"response":{"body":{"type":"RateLimitError"}}}}"#
        ));
        assert!(!opencode(
            r#"{"type":"error","error":{"status":500,"response":{"body":{"type":"FreeUsageLimitError"}}}}"#
        ));
        assert!(!opencode(
            r#"{"type":"text","part":{"text":"FreeUsageLimitError"}}"#
        ));
    }

    /// A host session's own variables are taken out, the user's login and
    /// general settings kept (spike, 2026-10-25).
    #[test]
    fn a_host_sessions_variables_never_reach_an_agent() {
        let host = |k: &str| host_session(std::ffi::OsStr::new(k));
        for k in [
            "CLAUDE_CODE_SIMPLE",
            "CLAUDECODE",
            "CLAUDE_CODE_SESSION_ID",
            "CLAUDE_CODE_MESSAGING_TOKEN",
            "CLAUDE_CODE_ENABLE_ASK_USER_QUESTION_TOOL",
            "CLAUDE_CODE_EMIT_TOOL_USE_SUMMARIES",
            "CLAUDE_AGENT_SDK_VERSION",
            "CLAUDE_EFFORT",
        ] {
            assert!(host(k), "{k}");
        }
        for k in [
            "CLAUDE_CODE_SESSION_ACCESS_TOKEN",
            "CLAUDE_CODE_REMOTE_SESSION_ID",
            "CLAUDE_CODE_REMOTE",
            "CLAUDE_CODE_BRIDGE_SESSION_ID",
            "CLAUDE_CODE_HOST_CREDS_FILE",
            "CLAUDE_CODE_SDK_HAS_OAUTH_REFRESH",
            "CLAUDE_CODE_GATEWAY_TOKEN",
            "CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING",
            "CLAUDE_SESSION_INGRESS_TOKEN_FILE",
            "CLAUDE_BRIDGE_OAUTH_TOKEN",
            "CLAUDE_TRUSTED_DEVICE_TOKEN",
            "CLAUDE_BG_SOCKET_TOKENS_PATH",
            "CLAUDE_RUNNER_SESSION_ID",
            "CLAUDE_PROJECT_DIR",
        ] {
            assert!(host(k), "{k}");
        }
        for k in [
            "CLAUDE_CODE_OAUTH_TOKEN",
            "CLAUDE_CODE_MAX_OUTPUT_TOKENS",
            "CLAUDE_CODE_ENABLE_TELEMETRY",
            "CLAUDE_CODE_SESSIONEND_HOOKS_TIMEOUT_MS",
            "CLAUDE_CONFIG_DIR",
            "CLAUDE_DEBUG",
            "CLAUDE_REMOTE_CONTROL_SESSION_NAME_PREFIX",
            "CLAUDE_STREAM_IDLE_TIMEOUT_MS",
            "MCP_TIMEOUT",
            "DISABLE_AUTOUPDATER",
            "PATH",
        ] {
            assert!(!host(k), "{k}");
        }
    }

    /// Every name the qualified releases' catalogs declare is withheld, but the
    /// general credentials kept on purpose; the pinned list is the
    /// script's output, sorted and whole.
    #[test]
    fn every_opencode_catalog_key_is_withheld_but_the_general_ones() {
        let names: Vec<&str> = OPENCODE_CATALOG.lines().collect();
        assert!(names.len() > 200, "{} names: refresh the list", names.len());
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(names, sorted, "sorted and unique, as the script prints");
        let mut kept = Vec::new();
        for n in names {
            assert!(
                !n.is_empty()
                    && n.bytes()
                        .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_'),
                "{n:?}"
            );
            if !withheld(std::ffi::OsStr::new(n)) {
                assert!(listed(n.as_bytes(), KEPT_GENERAL), "{n} reaches agents");
                kept.push(n);
            }
        }
        assert!(
            kept.contains(&"GITHUB_TOKEN") && kept.contains(&"AWS_REGION"),
            "{kept:?}"
        );
        assert!(withheld(std::ffi::OsStr::new("AWS_BEARER_TOKEN_BEDROCK")));
    }

    #[test]
    fn the_key_variables_cover_the_providers_each_harness_reads() {
        let held = |k: &str| withheld(std::ffi::OsStr::new(k));
        for k in [
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "OPENAI_API_KEY",
            "CODEX_API_KEY",
            // The switches to another provider's billing, endpoints, and
            // headers that can carry a key (reviews of 14867b8).
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "CLAUDE_CODE_USE_FOUNDRY",
            "CLAUDE_CODE_USE_MANTLE",
            "CLAUDE_CODE_USE_ANTHROPIC_AWS",
            "CLAUDE_CODE_USE_ANTHROPIC_GOOGLE_CLOUD",
            "CLAUDE_CODE_USE_GATEWAY",
            "ANTHROPIC_CUSTOM_HEADERS",
            "ANTHROPIC_BASE_URL",
            "ANTHROPIC_VERTEX_PROJECT_ID",
            "ANTHROPIC_GOOGLE_CLOUD_PROJECT",
            "ANTHROPIC_IDENTITY_TOKEN_FILE",
            "ANTHROPIC_SOMETHING_NEW",
            "AWS_BEARER_TOKEN_BEDROCK",
            "OPENAI_BASE_URL",
            // OpenCode's provider keys.
            "AZURE_API_KEY",
            "AZURE_COGNITIVE_SERVICES_API_KEY",
            "ZAI_API_KEY",
            "AI_GATEWAY_API_KEY",
            // OpenCode's provider-only aliases (review of 516603c).
            "SNOWFLAKE_CORTEX_PAT",
            "SNOWFLAKE_CORTEX_TOKEN",
            "GOOGLE_VERTEX_PROJECT",
            "GOOGLE_VERTEX_LOCATION",
            "GOOGLE_VERTEX_API_KEY",
            "AZURE_COGNITIVE_SERVICES_RESOURCE_NAME",
            // OpenCode's catalog keys (review of c569050).
            "ZHIPU_API_KEY",
            "NVIDIA_API_KEY",
            "KIMI_API_KEY",
            "META_MODEL_API_KEY",
            // A name may start with a digit (review of 1b8db3e).
            "302AI_API_KEY",
        ] {
            assert!(held(k), "{k}");
        }
        for k in [
            "CLAUDE_CODE_OAUTH_TOKEN",
            "ANTHROPIC_MODEL",
            "ANTHROPIC_SMALL_FAST_MODEL",
            "ANTHROPIC_DEFAULT_SONNET_MODEL",
            "ANTHROPIC_CUSTOM_MODEL_OPTION_NAME",
            "AWS_PROFILE",
            "AWS_ACCESS_KEY_ID",
            "GOOGLE_APPLICATION_CREDENTIALS",
            "GITLAB_TOKEN",
            "CODEX_HOME",
            "GOOGLE_CLOUD_PROJECT",
            "CLOUDFLARE_API_TOKEN",
            "DATABRICKS_TOKEN",
        ] {
            assert!(!held(k), "{k}: a login, a model choice or the work's own");
        }
    }
}
