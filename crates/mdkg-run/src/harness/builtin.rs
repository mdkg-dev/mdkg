//! The built-in implementations delegate to the existing native mechanics.

use super::*;
use hook::event::EventName;
use hook::{Event, Input, Outcome, Rendered};

pub(super) struct Claude;
pub(super) struct Codex;
pub(super) struct OpenCode;

fn claude_launch(t: &TurnInput<'_>, _: Option<&Path>) -> AgentCommand {
    crate::profile::claude(t)
}
fn opencode_launch(t: &TurnInput<'_>, _: Option<&Path>) -> AgentCommand {
    crate::profile::opencode(t)
}
fn no_preflight(_: &AgentCommand) -> Result<(), RunError> {
    Ok(())
}
fn no_binding(_: &Path, _: &Input) -> Result<Option<String>, String> {
    Ok(None)
}
fn claude_output(
    _: Role,
    stdout: &str,
    _: &str,
    _: Option<&str>,
    _: CodexSettings<'_>,
) -> Result<Parsed, RunError> {
    crate::parse::claude(stdout)
}
fn codex_output(
    role: Role,
    _: &str,
    stderr: &str,
    output: Option<&str>,
    settings: CodexSettings<'_>,
) -> Result<Parsed, RunError> {
    crate::parse::codex(stderr, output, role, settings)
}
fn opencode_output(
    _: Role,
    stdout: &str,
    _: &str,
    _: Option<&str>,
    _: CodexSettings<'_>,
) -> Result<Parsed, RunError> {
    crate::parse::opencode(stdout)
}

/// `claude auth status --json`: a claude.ai account, by its OAuth login or
/// a long-lived subscription token on Anthropic's own API, is the
/// subscription; Claude's own `apiKeyHelper` or console key isn't (review
/// of 2f2320e).
fn claude_login(program: &Path, cwd: &Path, env: &crate::env::Environment) -> Login {
    let Some(out) = crate::auth::login_probe(program, cwd, &["auth", "status", "--json"], env)
    else {
        return Login::Out;
    };
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
    if v["loggedIn"] != true {
        return Login::Out;
    }
    let method = v["authMethod"].as_str().unwrap_or("unknown").to_string();
    Login::In {
        subscription: matches!(method.as_str(), "claude.ai" | "oauth_token")
            && v["apiProvider"].as_str().is_none_or(|p| p == "firstParty"),
        method,
    }
}
/// `codex login status`: "Logged in using ChatGPT" is the subscription; a
/// key Codex keeps itself (`codex login --with-api-key`) isn't.
fn codex_login(program: &Path, cwd: &Path, env: &crate::env::Environment) -> Login {
    let Some(out) = crate::auth::login_probe(program, cwd, &["login", "status"], env) else {
        return Login::Out;
    };
    let text =
        String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
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
/// No check: OpenCode's own config names its providers and their keys.
fn opencode_login(_: &Path, _: &Path, _: &crate::env::Environment) -> Login {
    Login::Own
}

fn json_field(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key).and_then(|f| f.as_str()).map(String::from)
}
/// Whether `text` is Claude's own usage-limit wording: "Claude AI usage
/// limit reached", or "You've hit your weekly limit" / "You've hit your
/// <n>-hour limit" (straight or curly apostrophe), each at its start.
fn claude_limit_text(text: &str) -> bool {
    let text = text.trim();
    if text.starts_with("Claude AI usage limit reached") {
        return true;
    }
    let Some(rest) = ["You've hit your ", "You\u{2019}ve hit your "]
        .iter()
        .find_map(|p| text.strip_prefix(p))
    else {
        return false;
    };
    if rest.starts_with("weekly limit") {
        return true;
    }
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    digits > 0 && rest[digits..].starts_with("-hour limit")
}

/// Claude: a stream-json `rate_limit_event` whose `rate_limit_info.status`
/// is `rejected`; a `result` with `is_error` true whose text is its limit
/// wording; or that wording as the line. A 429 alone isn't: it may be
/// request-rate throttling (see [`claude_limit_in`]).
fn claude_usage_limit(line: &str) -> bool {
    let line = line.trim();
    claude_limit_text(line)
        || serde_json::from_str::<serde_json::Value>(line).is_ok_and(|v| {
            match json_field(&v, "type").as_deref() {
                Some("rate_limit_event") => {
                    v.pointer("/rate_limit_info/status")
                        .and_then(|s| s.as_str())
                        == Some("rejected")
                }
                Some("result") => {
                    v.get("is_error") == Some(&serde_json::Value::Bool(true))
                        && json_field(&v, "result").is_some_and(|r| claude_limit_text(&r))
                }
                _ => false,
            }
        })
}

/// Whether Claude's turn ended on its usage limit, from its stream-json
/// events in order: the final `result` is an error that either says the
/// limit wording or follows a rejected `rate_limit_event` (whatever its
/// own text, since Claude may end a limited turn with a result that names
/// no limit; review of 94ff686f). A 429 with no rejection may be
/// request-rate throttling, so it isn't one (review of 0f0a8dcb). A stream
/// with no result counts when its last event is a rejected
/// `rate_limit_event`.
fn claude_limit_in(events: &[String]) -> bool {
    let parse = |l: &String| serde_json::from_str::<serde_json::Value>(l.trim()).ok();
    let Some(at) = events.iter().rposition(|l| {
        parse(l).is_some_and(|v| json_field(&v, "type").as_deref() == Some("result"))
    }) else {
        return events.last().is_some_and(|l| claude_usage_limit(l));
    };
    let Some(result) = parse(&events[at]) else {
        return false;
    };
    if result.get("is_error") != Some(&serde_json::Value::Bool(true)) {
        return false;
    }
    if json_field(&result, "result").is_some_and(|r| claude_limit_text(&r)) {
        return true;
    }
    events[..at].iter().any(|l| {
        parse(l).is_some_and(|v| {
            json_field(&v, "type").as_deref() == Some("rate_limit_event")
                && v.pointer("/rate_limit_info/status")
                    .and_then(|s| s.as_str())
                    == Some("rejected")
        })
    })
}

/// Whether Claude's turn ended on provider throttling: its final `result`
/// is an error with `api_error_status` 429 that isn't its usage limit (no
/// limit wording, no rejected `rate_limit_event` before it). Transient: it
/// never sets mail aside and offers no key (orchestrator, under
/// dec-01a18ee0 item 8; review of d009bf16).
fn claude_throttled_in(events: &[String]) -> bool {
    let Some(result) = events.iter().rev().find_map(|l| {
        serde_json::from_str::<serde_json::Value>(l.trim())
            .ok()
            .filter(|v| json_field(v, "type").as_deref() == Some("result"))
    }) else {
        return false;
    };
    result.get("is_error") == Some(&serde_json::Value::Bool(true))
        && result.get("api_error_status").and_then(|s| s.as_u64()) == Some(429)
        && !claude_limit_in(events)
}

/// Codex and OpenCode report no throttling of their own: OpenCode's 429
/// quota bodies are its usage limit, and Codex's output is text.
fn never_throttled(_: &[String]) -> bool {
    false
}

/// Codex's output is text, so it has no event stream: any line of it.
fn codex_limit_in(events: &[String]) -> bool {
    events.iter().any(|l| codex_usage_limit(l))
}

/// OpenCode reports its quota as an error event, wherever it falls.
fn opencode_limit_in(events: &[String]) -> bool {
    events.iter().any(|l| opencode_usage_limit(l))
}

/// When Claude's limit resets, from a rejected `rate_limit_event`: its
/// `resetsAt` (epoch seconds) and `rateLimitType` (`five_hour`,
/// `seven_day`, ...). Codex and OpenCode don't say.
fn claude_limit_reset(line: &str) -> Option<(u64, Option<String>)> {
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    if json_field(&v, "type").as_deref() != Some("rate_limit_event")
        || v.pointer("/rate_limit_info/status")
            .and_then(|s| s.as_str())
            != Some("rejected")
    {
        return None;
    }
    let info = v.get("rate_limit_info")?;
    Some((
        info.get("resetsAt")?.as_u64()?,
        json_field(info, "rateLimitType"),
    ))
}
/// Codex and OpenCode say nothing of when their limit resets.
fn no_limit_reset(_: &str) -> Option<(u64, Option<String>)> {
    None
}

/// Codex: "ERROR: You've hit your usage limit…" (straight or curly
/// apostrophe), or a JSON error event whose message starts so, or whose
/// code is `usage_limit_reached`.
fn codex_usage_limit(line: &str) -> bool {
    // Codex prints the apostrophe straight or curly, by version. Its JSON
    // forms below are for a launch with `--json`; mdkg launches it without,
    // so a turn reads only its `ERROR:` line (crate::turn's harness_error).
    let limit = |m: &str| {
        [
            "You've hit your usage limit",
            "You\u{2019}ve hit your usage limit",
        ]
        .iter()
        .any(|p| m.starts_with(p))
    };
    let line = line.trim();
    line.strip_prefix("ERROR: ").is_some_and(limit)
        || serde_json::from_str::<serde_json::Value>(line).is_ok_and(|v| {
            let nested = v.get("error").filter(|e| e.is_object());
            // A message counts only on an error event; a code is
            // structured, so it counts wherever it's the event's own.
            let is_error = nested.is_some()
                || matches!(
                    json_field(&v, "type").as_deref(),
                    Some("error" | "turn.failed")
                );
            let event = nested.unwrap_or(&v);
            json_field(event, "code").as_deref() == Some("usage_limit_reached")
                || (is_error && json_field(event, "message").is_some_and(|m| limit(&m)))
        })
}
/// The response bodies of OpenCode's quota errors.
const OPENCODE_QUOTA_BODIES: &[&str] = &["FreeUsageLimitError", "GoUsageLimitError"];

/// Whether `body` (an object, or a JSON string of one) is one of
/// [`OPENCODE_QUOTA_BODIES`], by its `type` or its `error.type`.
fn opencode_quota_body(body: &serde_json::Value) -> bool {
    let parsed;
    let body = match body.as_str() {
        Some(text) => match serde_json::from_str::<serde_json::Value>(text) {
            Ok(v) => {
                parsed = v;
                &parsed
            }
            Err(_) => return false,
        },
        None => body,
    };
    [body.get("type"), body.pointer("/error/type")]
        .into_iter()
        .flatten()
        .filter_map(|t| t.as_str())
        .any(|t| OPENCODE_QUOTA_BODIES.contains(&t))
}

/// OpenCode: its JSON `error` event for a usage limit. Its own quota form:
/// `error.type` is `provider.quota`, or `error.status` is 429 with a quota
/// body in `error.response.body`. Its API-error form: the same body, or a
/// message naming it (or the provider's `usage_limit_reached`), as
/// `error.data.message`, or 429 as
/// `error.data.statusCode` with a quota body in `error.data.responseBody`.
/// And `usage_limit_reached` as `error.code`. OpenCode has no key, so the
/// limit is only named.
fn opencode_usage_limit(line: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(line.trim()).is_ok_and(|v| {
        if json_field(&v, "type").as_deref() != Some("error") {
            return false;
        }
        let Some(e) = v.get("error") else {
            return false;
        };
        let status = |s: Option<&serde_json::Value>| s.and_then(|s| s.as_u64()) == Some(429);
        let body = |b: Option<&serde_json::Value>| b.is_some_and(opencode_quota_body);
        let message = e.pointer("/data/message");
        json_field(e, "type").as_deref() == Some("provider.quota")
            || (status(e.get("status")) && body(e.pointer("/response/body")))
            || json_field(e, "code").as_deref() == Some("usage_limit_reached")
            || message.and_then(|m| m.as_str()) == Some("usage_limit_reached")
            || body(message)
            || message
                .and_then(|m| m.as_str())
                .is_some_and(|m| OPENCODE_QUOTA_BODIES.iter().any(|b| m.contains(b)))
            || (status(e.pointer("/data/statusCode")) && body(e.pointer("/data/responseBody")))
    })
}

/// The most prompt OpenCode gets: it takes the prompt as one argument, and
/// Linux caps one argument at 128 KiB.
const OPENCODE_PROMPT_MAX: usize = 96 * 1024;

fn no_turn_check(_: Option<&str>, _: &TurnOptions) -> Result<(), RunError> {
    Ok(())
}
/// OpenCode's Zen models send prompts to OpenCode's servers: only when
/// asked for by name and allowed. A model the user configured is theirs.
fn opencode_turn_check(model: Option<&str>, opts: &TurnOptions) -> Result<(), RunError> {
    if let Some(m) = model
        && m.starts_with("opencode/")
        && !opts.allow_zen
    {
        return Err(RunError::usage(
            crate::error::CODE_UNSUPPORTED,
            format!("{m} is an OpenCode Zen model, which sends prompts to OpenCode's servers"),
        )
        .with_hint("use your own provider, or pass --allow-zen"));
    }
    Ok(())
}
fn always_resumable(_: &str, _: bool, _: &Path, _: &TurnOptions, _: Option<&Path>) -> bool {
    true
}
/// An OpenCode session from v0.1, started in the launcher's private data
/// folder (`<turns>/home`, which only v0.1 made), isn't where the user's
/// OpenCode looks to resume it: start a new one (review of 10915d4). One
/// the state doesn't mark either way is judged by that folder (review of
/// 99f5402).
fn opencode_resumable(
    _: &str,
    user_session: bool,
    turns: &Path,
    _: &TurnOptions,
    _: Option<&Path>,
) -> bool {
    user_session || std::fs::symlink_metadata(turns.join("home")).is_err()
}
/// A Codex session is judged by whether the user's CODEX_HOME holds it,
/// whatever else is left (review of 1a38a2c).
fn codex_resumable(id: &str, _: bool, _: &Path, opts: &TurnOptions, home: Option<&Path>) -> bool {
    opts.user_codex_home
        .clone()
        .or_else(|| user_codex_home(home))
        .map(|u| std::fs::canonicalize(&u).unwrap_or(u))
        .as_deref()
        .is_some_and(|h| codex_session_known(h, id))
}
fn no_early_check(_: &str, _: Role, _: CodexSettings<'_>) -> Option<Result<(), RunError>> {
    None
}
/// Codex's stderr session header, checked against the sandbox it was asked
/// for.
fn codex_early_check(
    stderr: &str,
    role: Role,
    settings: CodexSettings<'_>,
) -> Option<Result<(), RunError>> {
    crate::parse::codex_header_check_settings(stderr, role, settings)
}

/// Whether a Codex home holds a session file for `id`
/// (`sessions/YYYY/MM/DD/rollout-…-<id>.jsonl`).
fn codex_session_known(codex_home: &Path, id: &str) -> bool {
    fn walk(dir: &Path, id: &str, depth: usize) -> bool {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return false;
        };
        entries.flatten().any(|e| {
            let path = e.path();
            if path.is_dir() {
                depth > 0 && walk(&path, id, depth - 1)
            } else {
                e.file_name().to_string_lossy().contains(id)
            }
        })
    }
    !id.is_empty() && walk(&codex_home.join("sessions"), id, 4)
}

/// The user's own Codex folder: `$CODEX_HOME`, else ~/.codex.
fn user_codex_home(home: Option<&Path>) -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| home.map(|h| h.join(".codex")))
}

fn standard_native(native: &str) -> Option<(Event, EventName)> {
    Some(match native {
        "SessionStart" => (Event::Start, EventName::SessionStart),
        "UserPromptSubmit" => (Event::Start, EventName::PromptSubmit),
        "PreToolUse" => (Event::Edit, EventName::ToolBefore),
        "Stop" => (Event::Stop, EventName::TurnStop),
        _ => return None,
    })
}
fn claude_native(native: &str) -> Option<(Event, EventName)> {
    if native == "idle" {
        Some((Event::Wait, EventName::SessionIdle))
    } else {
        standard_native(native)
    }
}
fn opencode_native(native: &str) -> Option<(Event, EventName)> {
    Some(match native {
        "context" => (Event::Start, EventName::PromptSubmit),
        "tool.execute.before" => (Event::Edit, EventName::ToolBefore),
        "session.execution.succeeded" => (Event::Stop, EventName::TurnStop),
        "idle" => (Event::Wait, EventName::SessionIdle),
        _ => return None,
    })
}
fn claude_overrides(sandbox: Option<&str>, _: Option<&str>) -> Result<(), RunError> {
    if sandbox.is_some() {
        Err(RunError::usage(
            "usage.invalid_arguments",
            "Claude has no native sandbox string override; use approvals for its permission mode",
        ))
    } else {
        Ok(())
    }
}
fn codex_overrides(_: Option<&str>, _: Option<&str>) -> Result<(), RunError> {
    Ok(())
}
fn opencode_overrides(sandbox: Option<&str>, approvals: Option<&str>) -> Result<(), RunError> {
    if sandbox.is_some() || approvals.is_some() {
        Err(RunError::usage(
            "usage.invalid_arguments",
            "OpenCode sandbox and approvals overrides are not supported; its user defaults apply",
        ))
    } else {
        Ok(())
    }
}

fn validate_effort(values: Option<&[&str]>, effort: &str) -> Result<(), RunError> {
    match values {
        Some(values) if values.contains(&effort) => Ok(()),
        Some(values) => Err(RunError::usage("usage.invalid_arguments",
            format!("no effort {effort:?}: it takes {}", values.join(", ")))),
        None => {
            Err(RunError::usage(
                "usage.invalid_arguments",
                "OpenCode takes no effort setting",
            )
            .with_hint(format!(
                "name the model's variant with --model <provider>/<model>#<variant>; if there is no file definition, replace saved or inline settings with an effort-free definition: `{} agents add <name> --harness opencode --role <role>`; if .mdkg/agents/<name>.json exists, remove its effort field there, then commit that file in the graph checkout",
                mdkg_format::program::BIN
            )))
        }
    }
}

// All methods are required by the trait. This macro shares the forwarding
// plumbing, while each implementation must name every native operation.
macro_rules! adapter {
    ($type:ident, $display:literal, $override:literal, $idle:literal,
     $launch:path, $environment:path, $preflight:path, $output:path,
     $mcp:expr, $project:path, $layout:expr, $hooks:path,
     $native:path, $parse:path, $render:path, $binding:path,
     $efforts:expr, $overrides:path,
     $login:path, $login_command:literal, $keychain:expr, $limit:path, $limit_in:path, $throttled_in:path, $reset:path,
     $check_turn:path, $prompt_max:expr, $resumable:path, $chooses_session:expr,
     $early:path, $reply_on_stdout:expr, $structured:expr) => {
        impl HarnessAdapter for $type {
            fn id(&self) -> &'static str {
                Harness::$type.as_str()
            }
            fn display_name(&self) -> &'static str {
                $display
            }
            fn program_override(&self) -> &'static str {
                $override
            }
            fn version_probe(&self) -> &'static [&'static str] {
                &["--version"]
            }
            fn capabilities(&self) -> &'static [&'static str] {
                if $idle {
                    &[
                        "resume",
                        "hooks.tool_before",
                        "hooks.prompt_submit",
                        "hooks.turn_stop",
                        "hooks.session_idle",
                    ]
                } else {
                    &[
                        "resume",
                        "hooks.tool_before",
                        "hooks.prompt_submit",
                        "hooks.turn_stop",
                    ]
                }
            }
            fn preflight(&self, command: &AgentCommand) -> Result<(), RunError> {
                $preflight(command)
            }
            fn launch(&self, input: &TurnInput<'_>, shell: Option<&Path>) -> AgentCommand {
                $launch(input, shell)
            }
            fn environment(&self, input: &TurnInput<'_>) -> Vec<(String, String)> {
                $environment(input)
            }
            fn withheld(&self, name: &OsStr) -> bool {
                crate::auth::withheld(name) || crate::auth::host_session(name)
            }
            fn parse_output(
                &self,
                role: Role,
                stdout: &str,
                stderr: &str,
                output_file: Option<&str>,
                settings: CodexSettings<'_>,
            ) -> Result<Parsed, RunError> {
                $output(role, stdout, stderr, output_file, settings)
            }
            fn login(&self, program: &Path, cwd: &Path, env: &crate::env::Environment) -> Login {
                $login(program, cwd, env)
            }
            fn inherited_env(&self) -> &'static [&'static str] {
                crate::env::harness_names(Harness::$type)
            }
            fn login_command(&self) -> &'static str {
                $login_command
            }
            fn keychain_service(&self) -> Option<&'static str> {
                $keychain
            }
            fn usage_limit(&self, line: &str) -> bool {
                $limit(line)
            }
            fn usage_limit_in(&self, events: &[String]) -> bool {
                $limit_in(events)
            }
            fn throttled_in(&self, events: &[String]) -> bool {
                $throttled_in(events)
            }
            fn usage_limit_reset(&self, line: &str) -> Option<(u64, Option<String>)> {
                $reset(line)
            }
            fn auth_status(
                &self,
                program: &Path,
                cwd: &Path,
                policy: &crate::policy::Auth,
                keys: &KeySource,
                env: &crate::env::Environment,
            ) -> AuthStatus {
                crate::auth::status(Harness::$type, program, cwd, policy, keys, env)
            }
            fn mcp_projection(&self) -> McpProjection {
                $mcp
            }
            fn mcp_project(&self, root: &Path, server: &Server<'_>) -> Result<Planned, RunError> {
                $project(root, server)
            }
            fn hook_layout(&self) -> HookLayout {
                $layout
            }
            fn hook_project(&self, binary: &str) -> String {
                $hooks(binary)
            }
            fn hook_native_event(&self, native: &str) -> Option<(Event, EventName)> {
                $native(native)
            }
            fn hook_parse(&self, event: Event, input: &serde_json::Value) -> Result<Input, String> {
                $parse(event, input)
            }
            fn hook_render(&self, event: Event, outcome: &Outcome) -> Rendered {
                $render(event, outcome)
            }
            fn hook_bound_actor(
                &self,
                root: &Path,
                input: &Input,
            ) -> Result<Option<String>, String> {
                $binding(root, input)
            }
            fn hook_supports(&self, event: EventName) -> bool {
                matches!(
                    event,
                    EventName::SessionStart
                        | EventName::PromptSubmit
                        | EventName::ToolBefore
                        | EventName::TurnStop
                ) || ($idle && event == EventName::SessionIdle)
            }
            fn efforts(&self) -> Option<&'static [&'static str]> {
                $efforts
            }
            fn validate_effort(&self, effort: &str) -> Result<(), RunError> {
                validate_effort(self.efforts(), effort)
            }
            fn validate_overrides(
                &self,
                sandbox: Option<&str>,
                approvals: Option<&str>,
            ) -> Result<(), RunError> {
                $overrides(sandbox, approvals)
            }
            fn check_turn(&self, model: Option<&str>, opts: &TurnOptions) -> Result<(), RunError> {
                $check_turn(model, opts)
            }
            fn prompt_max(&self) -> Option<usize> {
                $prompt_max
            }
            fn resumable(
                &self,
                id: &str,
                user_session: bool,
                turns: &Path,
                opts: &TurnOptions,
                home: Option<&Path>,
            ) -> bool {
                $resumable(id, user_session, turns, opts, home)
            }
            fn chooses_session(&self) -> bool {
                $chooses_session
            }
            fn early_check(
                &self,
                stderr: &str,
                role: Role,
                settings: CodexSettings<'_>,
            ) -> Option<Result<(), RunError>> {
                $early(stderr, role, settings)
            }
            fn reply_on_stdout(&self) -> bool {
                $reply_on_stdout
            }
            fn structured_output(&self) -> bool {
                $structured
            }
        }
    };
}

adapter!(
    Claude,
    "Claude Code",
    "MDKG_RUN_CLAUDE",
    true,
    claude_launch,
    crate::profile::agent_env,
    no_preflight,
    claude_output,
    McpProjection {
        name: "mcp:claude",
        path: ".mcp.json",
        section: "mcpServers",
        format: McpFormat::Json {
            command_pointer: "/mcpServers/mdkg/command"
        },
        schema: None
    },
    crate::install::claude_mcp,
    HookLayout {
        name: "hooks:claude",
        canonical: ".mdkg/hooks/claude/settings.json",
        target: ".claude/settings.local.json",
        merged: true,
        personal: true
    },
    crate::hooks::claude_canonical,
    claude_native,
    hook::claude::parse,
    hook::claude::render,
    no_binding,
    Some(crate::state::CLAUDE_EFFORTS),
    claude_overrides,
    claude_login,
    "claude auth login",
    Some("mdkg-anthropic"),
    claude_usage_limit,
    claude_limit_in,
    claude_throttled_in,
    claude_limit_reset,
    no_turn_check,
    None,
    always_resumable,
    // Claude takes the new session's id as an argument.
    true,
    no_early_check,
    false,
    // `stream-json`.
    true
);
adapter!(
    Codex,
    "Codex",
    "MDKG_RUN_CODEX",
    false,
    crate::profile::codex,
    crate::profile::agent_env,
    no_preflight,
    codex_output,
    McpProjection {
        name: "mcp:codex",
        path: ".codex/config.toml",
        section: "mcp_servers",
        format: McpFormat::Toml,
        schema: None
    },
    crate::install::codex_mcp,
    HookLayout {
        name: "hooks:codex",
        canonical: ".mdkg/hooks/codex/hooks.json",
        target: ".codex/hooks.json",
        merged: true,
        personal: false
    },
    crate::codex_hooks::codex_fragment,
    standard_native,
    hook::codex::parse,
    hook::codex::render,
    hook::codex::bound_actor,
    Some(crate::state::CODEX_EFFORTS),
    codex_overrides,
    codex_login,
    "codex login",
    Some("mdkg-openai"),
    codex_usage_limit,
    codex_limit_in,
    never_throttled,
    no_limit_reset,
    no_turn_check,
    None,
    codex_resumable,
    false,
    codex_early_check,
    // On a key turn, with no `-o` file, its stdout is the reply.
    true,
    // Run without `--json`: its output is text, and a JSON line in it may
    // be quoted (orchestrator, 2026-10-29, on #6209).
    false
);
adapter!(
    OpenCode,
    "OpenCode",
    "MDKG_RUN_OPENCODE",
    true,
    opencode_launch,
    crate::profile::opencode_env,
    crate::profile::opencode_check,
    opencode_output,
    McpProjection {
        name: "mcp:opencode",
        path: "opencode.json",
        section: "mcp",
        format: McpFormat::Json {
            command_pointer: "/mcp/mdkg/command/0"
        },
        schema: Some("https://opencode.ai/config.json")
    },
    crate::install::opencode_mcp,
    HookLayout {
        name: "hooks:opencode",
        canonical: ".mdkg/hooks/opencode/mdkg/server.ts",
        target: ".opencode/plugins/mdkg/server.ts",
        merged: false,
        personal: false
    },
    crate::hooks::opencode_plugin,
    opencode_native,
    hook::opencode::parse,
    hook::opencode::render,
    no_binding,
    None,
    opencode_overrides,
    opencode_login,
    "opencode auth login",
    // OpenCode's own config names its providers and their keys.
    None,
    opencode_usage_limit,
    opencode_limit_in,
    never_throttled,
    no_limit_reset,
    opencode_turn_check,
    Some(OPENCODE_PROMPT_MAX),
    opencode_resumable,
    false,
    no_early_check,
    false,
    // `--format json`.
    true
);
