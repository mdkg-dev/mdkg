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
fn claude_login(program: &Path, cwd: &Path) -> Login {
    let Some(out) = crate::auth::login_probe(program, cwd, &["auth", "status", "--json"]) else {
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
fn codex_login(program: &Path, cwd: &Path) -> Login {
    let Some(out) = crate::auth::login_probe(program, cwd, &["login", "status"]) else {
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
fn opencode_login(_: &Path, _: &Path) -> Login {
    Login::Own
}

fn json_field(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key).and_then(|f| f.as_str()).map(String::from)
}
/// Claude: its stream-json result with `is_error` true and a result that
/// starts "Claude AI usage limit reached", or that message as the line.
fn claude_usage_limit(line: &str) -> bool {
    const MESSAGE: &str = "Claude AI usage limit reached";
    let line = line.trim();
    line.starts_with(MESSAGE)
        || serde_json::from_str::<serde_json::Value>(line).is_ok_and(|v| {
            json_field(&v, "type").as_deref() == Some("result")
                && v.get("is_error") == Some(&serde_json::Value::Bool(true))
                && json_field(&v, "result").is_some_and(|r| r.starts_with(MESSAGE))
        })
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
/// OpenCode: its JSON `error` event whose error says `usage_limit_reached`
/// (its provider's code). OpenCode has no key, so the limit is only named.
fn opencode_usage_limit(line: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(line.trim()).is_ok_and(|v| {
        json_field(&v, "type").as_deref() == Some("error")
            && v.get("error").is_some_and(|e| {
                json_field(e, "code").as_deref() == Some("usage_limit_reached")
                    || e.get("data")
                        .and_then(|d| d.get("message"))
                        .and_then(|m| m.as_str())
                        == Some("usage_limit_reached")
            })
    })
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
     $login:path, $login_command:literal, $keychain:expr, $limit:path) => {
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
            fn login(&self, program: &Path, cwd: &Path) -> Login {
                $login(program, cwd)
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
            fn auth_status(
                &self,
                program: &Path,
                cwd: &Path,
                policy: &crate::policy::Auth,
                keys: &KeySource,
            ) -> AuthStatus {
                crate::auth::status(Harness::$type, program, cwd, policy, keys)
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
    claude_usage_limit
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
    codex_usage_limit
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
    opencode_usage_limit
);
