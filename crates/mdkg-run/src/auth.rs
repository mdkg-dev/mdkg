//! Which account a turn bills: the user's subscription first, an API key
//! only when the config allows it, and never one an agent can see.
//!
//! - **Subscription first.** Each harness uses its own login: Claude Code's
//!   (the keychain), Codex's (ChatGPT, in CODEX_HOME), OpenCode's own
//!   configured auth. The provider key variables in [`KEY_VARS`] never
//!   reach an agent, so a stray key in the launcher's shell can't switch a
//!   turn to API billing. The rest of the environment passes through.
//! - **A key only through config.** With `auth.api_key_fallback` on, a
//!   Claude or Codex turn uses an API key when the harness isn't logged in,
//!   or retries once with it when the subscription hits its usage limit.
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

/// Every environment variable OpenCode 2.1.0's provider catalog declares,
/// one per line, as `scripts/opencode-catalog-env.py` extracts them from
/// its binary. OpenCode bills a provider through the key one names when
/// nothing is stored for it, so all of them but [`KEPT_GENERAL`] are
/// withheld (review of c569050; the orchestrator's option A). A newer
/// catalog can add names: refresh the file with the script.
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
/// general settings (`MCP_*`, `DISABLE_*`) stay.
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
/// process's, and [`KEY_VARS`] whether set or not.
pub fn withheld_vars() -> Vec<std::ffi::OsString> {
    let mut vars: Vec<std::ffi::OsString> = KEY_VARS.iter().map(Into::into).collect();
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

/// The Keychain service an API key for a harness is under.
pub fn keychain_service(h: Harness) -> Option<&'static str> {
    match h {
        Harness::Claude => Some("mdkg-anthropic"),
        Harness::Codex => Some("mdkg-openai"),
        // OpenCode's own config names its providers and their keys.
        Harness::OpenCode => None,
    }
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

/// The command that logs a harness in.
pub fn login_command(h: Harness) -> &'static str {
    match h {
        Harness::Claude => "claude auth login",
        Harness::Codex => "codex login",
        Harness::OpenCode => "opencode auth login",
    }
}

/// Asks the harness whether it's logged in (`claude auth status --json`,
/// `codex login status`), running it as an agent would start, in the
/// agent's folder and with no key variables: a stray key doesn't count as
/// a login. Prints nothing it says.
pub fn login(h: Harness, program: &Path, cwd: &Path) -> Login {
    let mut cmd = Command::new(program);
    cmd.current_dir(cwd);
    for k in withheld_vars() {
        cmd.env_remove(k);
    }
    cmd.stdin(std::process::Stdio::null());
    match h {
        Harness::Claude => {
            cmd.args(["auth", "status", "--json"]);
            let Ok(out) = bounded(&mut cmd) else {
                return Login::Out;
            };
            let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
            if v["loggedIn"] != true {
                return Login::Out;
            }
            let method = v["authMethod"].as_str().unwrap_or("unknown").to_string();
            Login::In {
                // A claude.ai account, by its OAuth login or a long-lived
                // subscription token, on Anthropic's own API.
                subscription: matches!(method.as_str(), "claude.ai" | "oauth_token")
                    && v["apiProvider"].as_str().is_none_or(|p| p == "firstParty"),
                method,
            }
        }
        Harness::Codex => {
            cmd.args(["login", "status"]);
            let Ok(out) = bounded(&mut cmd) else {
                return Login::Out;
            };
            let text = String::from_utf8_lossy(&out.stdout).to_string()
                + &String::from_utf8_lossy(&out.stderr);
            match text
                .lines()
                .find_map(|l| l.trim().strip_prefix("Logged in using "))
            {
                Some(rest) if out.status.success() => {
                    // `an API key - sk-…`: only the words before any dash.
                    let method = rest.split(" - ").next().unwrap_or(rest).trim().to_string();
                    Login::In {
                        subscription: method == "ChatGPT",
                        method,
                    }
                }
                _ => Login::Out,
            }
        }
        Harness::OpenCode => Login::Own,
    }
}

/// What a turn bills.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Billing {
    /// The harness's own login.
    Subscription,
    /// An API key from the Keychain, for one harness.
    ApiKey { service: &'static str, key: Secret },
}

/// The account a turn starts on: the harness's login, else the key when
/// the policy allows it, else a refusal naming the login command.
pub fn choose(
    h: Harness,
    program: &Path,
    cwd: &Path,
    policy: &crate::policy::Auth,
    keys: &KeySource,
) -> Result<Billing, RunError> {
    let login = login(h, program, cwd);
    match &login {
        Login::In {
            subscription: true, ..
        }
        | Login::Own => Ok(Billing::Subscription),
        // Not logged in, or logged in with a key the harness keeps: a key
        // bills only as the policy allows, and only the Keychain's.
        Login::Out | Login::In { .. } => match (policy.api_key_fallback, keychain_service(h)) {
            (true, Some(service)) => Ok(Billing::ApiKey {
                service,
                key: keys.read(service)?,
            }),
            _ => Err(RunError::usage(
                CODE_UNSUPPORTED,
                match &login {
                    Login::In { method, .. } => format!(
                        "{} is logged in with {method}, an API key rather than a subscription, \
                         and this agent's policy bills no key",
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
                    ", or turn on `auth.api_key_fallback` in the agent's policy with a key in \
                     the Keychain"
                } else {
                    ""
                }
            ))),
        },
    }
}

/// Whether a failed turn's output says the subscription hit its usage
/// limit, which the key fallback retries once.
pub fn usage_limited(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    [
        "usage limit",
        "usage_limit",
        "hit your limit",
        "out of extra usage",
    ]
    .iter()
    .any(|p| t.contains(p))
}

/// The account a harness would bill now, for `agents status` and `doctor`,
/// without reading any key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct AuthStatus {
    /// `subscription`, `api_key` (the fallback), `own_config` (OpenCode) or
    /// `none`.
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
) -> AuthStatus {
    let service = keychain_service(h);
    let key_in_keychain = service.is_some_and(|s| keys.has(s));
    let fallback = policy.api_key_fallback && key_in_keychain;
    let (source, method) = match login(h, program, cwd) {
        Login::In {
            method,
            subscription: true,
        } => ("subscription", Some(method)),
        Login::Own => ("own_config", None),
        Login::In { method, .. } => (if fallback { "api_key" } else { "none" }, Some(method)),
        Login::Out => (if fallback { "api_key" } else { "none" }, None),
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
    fn usage_limit_errors_are_recognized() {
        assert!(usage_limited("Claude AI usage limit reached|1792900000"));
        assert!(usage_limited(
            "ERROR: You've hit your usage limit. Upgrade to Pro"
        ));
        assert!(usage_limited("{\"code\":\"usage_limit_reached\"}"));
        assert!(!usage_limited("the agent exited with exit status: 1"));
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

    /// Every name OpenCode 2.1.0's catalog declares is withheld, but the
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
