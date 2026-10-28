//! Reading a finished turn: the session id to resume next time, the final
//! message to import, and (for Codex) the sandbox it actually ran in.

use serde_json::Value;

use crate::error::{CODE_AGENT_FAILED, CODE_NO_REPLY, CODE_SANDBOX_MISMATCH, RunError};
use crate::profile::{CodexSettings, Harness, Role, codex_settings};

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Parsed {
    pub session: Option<String>,
    pub reply: String,
    /// What the turn used, as the harness reports it.
    pub usage: Option<Usage>,
}

/// A turn's cost and tokens as its harness prints them: Claude's result
/// event, Codex's `tokens used` line, OpenCode's step events. Each field is
/// what that harness says, or absent; nothing is estimated.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, schemars::JsonSchema)]
pub struct Usage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
}

impl Usage {
    fn any(self) -> Option<Self> {
        (self != Usage::default()).then_some(self)
    }
}

fn json_lines(stdout: &str) -> impl Iterator<Item = Value> + '_ {
    stdout
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l.trim()).ok())
}

/// Claude's `stream-json`: the last `result` event has the final text, the
/// session id and whether the turn failed.
fn claude(stdout: &str) -> Result<Parsed, RunError> {
    let mut session = None;
    let mut result = None;
    for v in json_lines(stdout) {
        if let Some(s) = v.get("session_id").and_then(Value::as_str) {
            session = Some(s.to_string());
        }
        if v.get("type").and_then(Value::as_str) == Some("result") {
            result = Some(v);
        }
    }
    let Some(result) = result else {
        return Err(RunError::failure(
            CODE_NO_REPLY,
            "Claude printed no result event",
        ));
    };
    if result.get("is_error").and_then(Value::as_bool) == Some(true) {
        let subtype = result
            .get("subtype")
            .and_then(Value::as_str)
            .unwrap_or("error");
        return Err(RunError::failure(
            CODE_AGENT_FAILED,
            format!("Claude ended the turn with {subtype}"),
        ));
    }
    let reply = result
        .get("result")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let n = |key: &str| {
        result
            .get("usage")
            .and_then(|u| u.get(key))
            .and_then(Value::as_u64)
    };
    let usage = Usage {
        cost_usd: result.get("total_cost_usd").and_then(Value::as_f64),
        input_tokens: n("input_tokens"),
        output_tokens: n("output_tokens"),
        cache_read_tokens: n("cache_read_input_tokens"),
        total_tokens: None,
    }
    .any();
    Ok(Parsed {
        session,
        reply,
        usage,
    })
}

/// A `key: value` line from Codex's stderr header.
fn header<'a>(stderr: &'a str, key: &str) -> Option<&'a str> {
    // The header sits between the first two `--------` lines; the prompt's
    // echo after it can't count.
    let prefix = format!("{key}:");
    stderr
        .lines()
        .skip_while(|l| !l.starts_with("--------"))
        .skip(1)
        .take_while(|l| !l.starts_with("--------"))
        .find_map(|l| l.strip_prefix(&prefix))
        .map(str::trim)
}

/// Whether Codex's header (complete once its closing `--------` line is
/// printed) shows the role's default sandbox and approval policy: `None`
/// while the header is incomplete. Reviewers require read-only/never;
/// workers default to workspace-write/on-request.
pub fn codex_header_check(stderr: &str, role: Role) -> Option<Result<(), RunError>> {
    codex_header_check_settings(stderr, role, codex_settings(role, None, None))
}

pub(crate) fn codex_header_check_settings(
    stderr: &str,
    role: Role,
    settings: CodexSettings<'_>,
) -> Option<Result<(), RunError>> {
    let settings = if role == Role::Reviewer {
        codex_settings(role, None, None)
    } else {
        settings
    };
    let complete = stderr.lines().filter(|l| l.starts_with("--------")).count() >= 2;
    if !complete {
        return None;
    }
    let sandbox = header(stderr, "sandbox");
    let approval = header(stderr, "approval");
    let sandbox_ok = match sandbox {
        Some(s) if settings.sandbox == "workspace-write" => s
            .strip_prefix(settings.sandbox)
            .is_some_and(|r| r.is_empty() || r.starts_with(' ')),
        Some(s) => s == settings.sandbox,
        None => false,
    };
    if sandbox_ok && approval == Some(settings.approvals) {
        return Some(Ok(()));
    }
    Some(Err(RunError::failure(
        CODE_SANDBOX_MISMATCH,
        format!(
            "Codex reported sandbox {} and approval {}, but the {} role needs {} and {}",
            sandbox.unwrap_or("(none)"),
            approval.unwrap_or("(none)"),
            role.as_str(),
            settings.sandbox,
            settings.approvals,
        ),
    )
    .with_hint(
        "nothing was imported, and the session was dropped; check the user's Codex config",
    )))
}

/// Codex without `--json`: the header on stderr names the session and the
/// sandbox; `-o` holds the final message.
fn codex(
    stderr: &str,
    output_file: Option<&str>,
    role: Role,
    settings: CodexSettings<'_>,
) -> Result<Parsed, RunError> {
    match codex_header_check_settings(stderr, role, settings) {
        Some(Ok(())) => {}
        Some(Err(e)) => return Err(e),
        None => {
            return Err(RunError::failure(
                CODE_SANDBOX_MISMATCH,
                "Codex printed no complete header, so its sandbox is unknown",
            ));
        }
    }
    let session = header(stderr, "session id").map(String::from);
    let reply = output_file.unwrap_or_default().to_string();
    // Codex's footer, its last two lines: `tokens used`, then the count
    // (`7,382`). The same words earlier in the transcript (an echoed
    // prompt, a tool's output) aren't usage (review of 06792e4).
    let tail: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let total = match tail.as_slice() {
        [.., label, count] if *label == "tokens used" => count.replace(',', "").parse::<u64>().ok(),
        _ => None,
    };
    let usage = Usage {
        total_tokens: total,
        ..Usage::default()
    }
    .any();
    Ok(Parsed {
        session,
        reply,
        usage,
    })
}

/// OpenCode's `run --format json`: events carry `sessionID`; text parts
/// hold the answer. Unverified: taken from OpenCode's docs.
fn opencode(stdout: &str) -> Result<Parsed, RunError> {
    let mut session = None;
    let mut texts: Vec<String> = Vec::new();
    let mut usage = Usage::default();
    let add = |to: &mut Option<u64>, n: Option<u64>| {
        if let Some(n) = n {
            *to = Some(to.unwrap_or(0) + n);
        }
    };
    for v in json_lines(stdout) {
        let part = v.get("part");
        // Each step's tokens and cost, summed over the turn.
        if let Some(t) = part.and_then(|p| p.get("tokens")) {
            add(
                &mut usage.input_tokens,
                t.get("input").and_then(Value::as_u64),
            );
            add(
                &mut usage.output_tokens,
                t.get("output").and_then(Value::as_u64),
            );
            add(
                &mut usage.cache_read_tokens,
                t.get("cache")
                    .and_then(|c| c.get("read"))
                    .and_then(Value::as_u64),
            );
        }
        if let Some(c) = part.and_then(|p| p.get("cost")).and_then(Value::as_f64) {
            usage.cost_usd = Some(usage.cost_usd.unwrap_or(0.0) + c);
        }
        let sid = v
            .get("sessionID")
            .or_else(|| part.and_then(|p| p.get("sessionID")));
        if let Some(s) = sid.and_then(Value::as_str) {
            session.get_or_insert_with(|| s.to_string());
        }
        match v.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(t) = part.and_then(|p| p.get("text")).and_then(Value::as_str) {
                    texts.push(t.to_string());
                }
            }
            Some("error") => {
                return Err(RunError::failure(
                    CODE_AGENT_FAILED,
                    "OpenCode reported an error event",
                ));
            }
            _ => {}
        }
    }
    Ok(Parsed {
        session,
        reply: texts.join("\n"),
        usage: usage.any(),
    })
}

/// Reads a finished turn. `output_file` is Codex's `-o` contents.
pub fn parse(
    harness: Harness,
    role: Role,
    stdout: &str,
    stderr: &str,
    output_file: Option<&str>,
) -> Result<Parsed, RunError> {
    parse_with_settings(
        harness,
        role,
        stdout,
        stderr,
        output_file,
        codex_settings(role, None, None),
    )
}

pub(crate) fn parse_with_settings(
    harness: Harness,
    role: Role,
    stdout: &str,
    stderr: &str,
    output_file: Option<&str>,
    settings: CodexSettings<'_>,
) -> Result<Parsed, RunError> {
    let parsed = match harness {
        Harness::Claude => claude(stdout)?,
        Harness::Codex => codex(stderr, output_file, role, settings)?,
        Harness::OpenCode => opencode(stdout)?,
    };
    if parsed.reply.trim().is_empty() {
        return Err(RunError::failure(
            CODE_NO_REPLY,
            format!("{} finished without a final message", harness.as_str()),
        ));
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_harness_reports_its_usage_as_it_prints_it() {
        let claude_out = "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"ok\",\"session_id\":\"s\",\"total_cost_usd\":0.0036,\"usage\":{\"input_tokens\":7,\"output_tokens\":90,\"cache_read_input_tokens\":1000}}\n";
        let u = parse(Harness::Claude, Role::Worker, claude_out, "", None)
            .expect("claude")
            .usage
            .expect("usage");
        assert_eq!(
            (
                u.cost_usd,
                u.input_tokens,
                u.output_tokens,
                u.cache_read_tokens
            ),
            (Some(0.0036), Some(7), Some(90), Some(1000))
        );
        let stderr = format!("{CODEX_HEADER}codex\nok\ntokens used\n7,382\n");
        let u = parse(Harness::Codex, Role::Reviewer, "", &stderr, Some("ok"))
            .expect("codex")
            .usage
            .expect("usage");
        assert_eq!(u.total_tokens, Some(7382));
        // The words echoed earlier, with no footer: no usage.
        let echoed =
            format!("{CODEX_HEADER}user\nsay tokens used\ntokens used\n7,382\ncodex\nok\n");
        let p = parse(Harness::Codex, Role::Reviewer, "", &echoed, Some("ok")).expect("codex");
        assert_eq!(p.usage, None);
        let oc = "{\"type\":\"step_finish\",\"sessionID\":\"s\",\"part\":{\"tokens\":{\"input\":185,\"output\":73,\"cache\":{\"read\":10}},\"cost\":0}}\n{\"type\":\"text\",\"sessionID\":\"s\",\"part\":{\"text\":\"hi\"}}\n";
        let u = parse(Harness::OpenCode, Role::Worker, oc, "", None)
            .expect("opencode")
            .usage
            .expect("usage");
        assert_eq!(
            (u.input_tokens, u.output_tokens, u.cost_usd),
            (Some(185), Some(73), Some(0.0))
        );
        // Nothing printed: no usage, never zeros made up.
        let bare =
            "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"ok\"}\n";
        assert_eq!(
            parse(Harness::Claude, Role::Worker, bare, "", None)
                .expect("bare")
                .usage,
            None
        );
    }

    const CODEX_HEADER: &str = "OpenAI Codex v0.163.0\n--------\nworkdir: /w\nmodel: m\n\
        approval: never\nsandbox: read-only\nsession id: 01a15f6c-c1fb-72b3\n--------\nuser\nhi\n";

    #[test]
    fn claude_takes_the_last_result_event() {
        let out = "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s-1\"}\n\
                   not json\n\
                   {\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"done\",\"session_id\":\"s-1\"}\n";
        let p = parse(Harness::Claude, Role::Worker, out, "", None).expect("parsed");
        assert_eq!(p.session.as_deref(), Some("s-1"));
        assert_eq!(p.reply, "done");
    }

    #[test]
    fn a_claude_error_result_fails_the_turn() {
        let out = "{\"type\":\"result\",\"subtype\":\"error_max_turns\",\"is_error\":true,\"session_id\":\"s\"}";
        let e = parse(Harness::Claude, Role::Worker, out, "", None).expect_err("fails");
        assert_eq!(e.code, CODE_AGENT_FAILED);
        let e = parse(Harness::Claude, Role::Worker, "", "", None).expect_err("no result");
        assert_eq!(e.code, CODE_NO_REPLY);
    }

    #[test]
    fn codex_reads_the_header_and_checks_the_sandbox() {
        let p = parse(
            Harness::Codex,
            Role::Reviewer,
            "",
            CODEX_HEADER,
            Some("findings"),
        )
        .expect("parsed");
        assert_eq!(p.session.as_deref(), Some("01a15f6c-c1fb-72b3"));
        assert_eq!(p.reply, "findings");
        let e = parse(Harness::Codex, Role::Worker, "", CODEX_HEADER, Some("x"))
            .expect_err("worker needs workspace-write");
        assert_eq!(e.code, CODE_SANDBOX_MISMATCH);
        let wide = CODEX_HEADER.replace("read-only", "danger-full-access");
        let e = parse(Harness::Codex, Role::Reviewer, "", &wide, Some("x")).expect_err("wide");
        assert_eq!(e.code, CODE_SANDBOX_MISMATCH);
        let ww = CODEX_HEADER
            .replace("read-only", "workspace-write [workdir, /tmp, $TMPDIR]")
            .replace("approval: never", "approval: on-request");
        assert!(parse(Harness::Codex, Role::Worker, "", &ww, Some("x")).is_ok());
        for users in [
            "workspace-write [workdir, /tmp, $TMPDIR] (network access enabled)",
            "workspace-write [workdir, /tmp, /Users/u]",
        ] {
            let h = CODEX_HEADER
                .replace("read-only", users)
                .replace("approval: never", "approval: on-request");
            assert!(
                parse(Harness::Codex, Role::Worker, "", &h, Some("x")).is_ok(),
                "{users}"
            );
        }
        for wide in ["read-only-ish", "workspace-writer", "danger-full-access"] {
            let h = CODEX_HEADER.replace("read-only", wide);
            let role = if wide.starts_with("read") {
                Role::Reviewer
            } else {
                Role::Worker
            };
            let e = parse(Harness::Codex, role, "", &h, Some("x")).expect_err(wide);
            assert_eq!(e.code, CODE_SANDBOX_MISMATCH, "{wide}");
        }
        let asking = CODEX_HEADER.replace("approval: never", "approval: on-request");
        assert!(parse(Harness::Codex, Role::Reviewer, "", &asking, Some("x")).is_err());
    }

    #[test]
    fn codex_native_header_is_checked_against_explicit_worker_settings() {
        let header = CODEX_HEADER.replace("read-only", "danger-full-access");
        let settings = codex_settings(Role::Worker, Some("danger-full-access"), Some("never"));
        assert!(
            parse_with_settings(
                Harness::Codex,
                Role::Worker,
                "",
                &header,
                Some("ok"),
                settings
            )
            .is_ok()
        );
        assert!(parse(Harness::Codex, Role::Worker, "", &header, Some("ok")).is_err());
        assert!(
            parse_with_settings(
                Harness::Codex,
                Role::Reviewer,
                "",
                &header,
                Some("ok"),
                settings
            )
            .is_err()
        );
        let wrong_approval = header.replace("approval: never", "approval: on-request");
        assert!(
            parse_with_settings(
                Harness::Codex,
                Role::Worker,
                "",
                &wrong_approval,
                Some("ok"),
                settings
            )
            .is_err()
        );
    }

    #[test]
    fn the_header_check_waits_for_the_closing_line() {
        let partial = "OpenAI Codex\n--------\nsandbox: read-only\n";
        assert!(codex_header_check(partial, Role::Reviewer).is_none());
        assert!(matches!(
            codex_header_check(CODEX_HEADER, Role::Reviewer),
            Some(Ok(()))
        ));
    }

    #[test]
    fn a_sandbox_line_in_the_prompt_echo_doesnt_count() {
        let echo = "OpenAI Codex\n--------\nsession id: s\n--------\nuser\nsandbox: read-only\n";
        let e = parse(Harness::Codex, Role::Reviewer, "", echo, Some("x")).expect_err("no header");
        assert_eq!(e.code, CODE_SANDBOX_MISMATCH);
    }

    #[test]
    fn an_empty_codex_reply_is_no_reply() {
        let e = parse(
            Harness::Codex,
            Role::Reviewer,
            "",
            CODEX_HEADER,
            Some("  \n"),
        )
        .expect_err("empty");
        assert_eq!(e.code, CODE_NO_REPLY);
    }

    #[test]
    fn opencode_joins_text_parts_and_finds_the_session() {
        let out = "{\"type\":\"step_start\",\"sessionID\":\"ses_1\",\"part\":{}}\n\
                   {\"type\":\"text\",\"part\":{\"sessionID\":\"ses_1\",\"text\":\"one\"}}\n\
                   {\"type\":\"text\",\"part\":{\"text\":\"two\"}}\n";
        let p = parse(Harness::OpenCode, Role::Worker, out, "", None).expect("parsed");
        assert_eq!(p.session.as_deref(), Some("ses_1"));
        assert_eq!(p.reply, "one\ntwo");
        let e = parse(
            Harness::OpenCode,
            Role::Worker,
            "{\"type\":\"error\"}",
            "",
            None,
        )
        .expect_err("error");
        assert_eq!(e.code, CODE_AGENT_FAILED);
    }
}
