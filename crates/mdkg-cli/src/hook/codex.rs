//! Codex 0.164.0 command hooks. The shared core owns mail, leases, actor
//! resolution and reminder state; this module only reads and renders the wire.
//!
//! SessionStart and UserPromptSubmit both accept plain stdout context. Stop
//! blocks and PreToolUse denies with exit 2 and a stderr reason, so a login
//! shell's stdout cannot invalidate an exit-zero JSON decision. Codex reports
//! the patch for `apply_patch` as `tool_input.command`, including
//! when a matcher used the Edit or Write alias. Bash is a separate tool: shell
//! writes remain the shared stop audit's responsibility, not shell parsing here.
//!
//! References: https://learn.chatgpt.com/docs/hooks and the rust-v0.164.0
//! codex-rs/apply-patch/src/{parser,streaming_parser}.rs. No hook fires on idle;
//! `wait` must not pretend to wake a Codex session.

use std::collections::HashSet;
use std::path::Path;

use mdkg_run::hooks::AgentId;
use serde_json::Value;

use super::{Event, HarnessEvent, Input, Outcome, Rendered, StartProvenance};

/// Required wire text. Diagnostics name fields, never their contents: prompts,
/// transcripts, assistant messages and patch bodies must not become context.
fn text<'a>(v: &'a Value, key: &str) -> Result<&'a str, String> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && !s.contains('\0'))
        .ok_or_else(|| format!("Codex hook requires a nonempty {key} string"))
}

/// Preserve only validated observation metadata in the shared input. A
/// SessionStart is an observation, not proof of the TUI's selected thread.
pub(super) fn parse(event: Event, v: &Value) -> Result<Input, String> {
    let name = text(v, "hook_event_name")?;
    let matches = match event {
        Event::Start => matches!(name, "SessionStart" | "UserPromptSubmit"),
        Event::Stop => name == "Stop",
        Event::Edit => name == "PreToolUse",
        Event::Wait => false,
    };
    if !matches {
        return Err("Codex hook event does not match the mdkg event".into());
    }
    let harness_event = event
        .observation()
        .and_then(|event| HarnessEvent::parse("codex", event, name));
    let source = if name == "SessionStart" {
        let source = text(v, "source")?;
        if !matches!(source, "startup" | "resume" | "clear" | "compact" | "fork") {
            return Err("Codex SessionStart source is unsupported".into());
        }
        Some(source)
    } else {
        None
    };
    let agent_id = match v.get("agent_id") {
        None | Some(Value::Null) => None,
        Some(_) => {
            let id = text(v, "agent_id")?;
            if AgentId::parse(id).is_none() {
                return Err("Codex agent_id is outside the supported grammar".into());
            }
            Some(id)
        }
    };
    let start_provenance = (harness_event == Some(HarnessEvent::SessionStart))
        .then(|| StartProvenance::parse(source, agent_id));
    let cwd = text(v, "cwd")?;
    if !Path::new(cwd).is_absolute() {
        return Err("Codex hook cwd must be an absolute local path".into());
    }
    let session = text(v, "session_id")?;
    let turn = match v.get("turn_id") {
        None | Some(Value::Null) => None,
        Some(_) => Some(text(v, "turn_id")?.to_string()),
    };
    let stop_active = match v.get("stop_hook_active") {
        None => false,
        Some(Value::Bool(active)) => *active,
        Some(_) => return Err("Codex stop_hook_active must be a boolean".into()),
    };
    let tool = if event == Event::Edit {
        Some(text(v, "tool_name")?.to_string())
    } else {
        None
    };
    let paths = if tool.as_deref() == Some("apply_patch") {
        let command = v
            .get("tool_input")
            .and_then(|input| input.get("command"))
            .and_then(Value::as_str)
            .ok_or("Codex apply_patch requires tool_input.command as a string")?;
        patch_paths(command)?
    } else {
        Vec::new()
    };
    Ok(Input {
        cwd: Some(cwd.into()),
        session: Some(session.to_string()),
        turn,
        harness_event,
        start_provenance,
        stop_active,
        tool,
        paths,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hunk {
    None,
    Add,
    Delete,
    Update,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Chunk {
    None,
    Empty,
    Lines,
    End,
}

/// Extract the local write set without interpreting patch contents or reading
/// files. Header whitespace follows 0.164.0: trim both ends outside an update,
/// only the end inside one (a leading space there is a context-line marker).
/// Preserve the path after its fixed marker, including a leading space.
/// Unsupported or malformed syntax is an error, never an empty write set.
fn patch_paths(patch: &str) -> Result<Vec<String>, String> {
    let mut lines: Vec<&str> = patch.trim().lines().collect();
    if lines.len() >= 4
        && matches!(lines[0], "<<EOF" | "<<'EOF'" | "<<\"EOF\"")
        && lines.last().is_some_and(|line| line.ends_with("EOF"))
    {
        lines = lines[1..lines.len() - 1].to_vec();
    }
    if lines.len() < 2
        || lines[0].trim() != "*** Begin Patch"
        || lines.last().map(|s| s.trim()) != Some("*** End Patch")
    {
        return Err("Codex apply_patch requires Begin Patch and End Patch markers".into());
    }
    let invalid = || "Codex apply_patch has malformed or unsupported patch syntax".to_string();
    let mut paths = Vec::new();
    let mut seen = HashSet::new();
    let mut path = |p: &str| -> Result<(), String> {
        if p.is_empty() || p.chars().any(char::is_control) {
            return Err("Codex apply_patch has an empty or control-containing path".into());
        }
        if seen.insert(p.to_string()) {
            paths.push(p.to_string());
        }
        Ok(())
    };
    let mut hunk = Hunk::None;
    let mut chunk = Chunk::None;
    let mut moved = false;
    for line in &lines[1..lines.len() - 1] {
        let header = if hunk == Hunk::Update {
            line.trim_end()
        } else {
            line.trim()
        };
        // A remote environment's paths cannot be checked against local leases.
        if header.starts_with("*** Environment ID:") {
            return Err(
                "Codex apply_patch environment routing is not supported by the local edit guard"
                    .into(),
            );
        }
        let next = [
            ("*** Add File: ", Hunk::Add),
            ("*** Delete File: ", Hunk::Delete),
            ("*** Update File: ", Hunk::Update),
        ]
        .into_iter()
        .find_map(|(prefix, next)| header.strip_prefix(prefix).map(|p| (p, next)));
        if let Some((p, next)) = next {
            if hunk == Hunk::Update && matches!(chunk, Chunk::None | Chunk::Empty) {
                return Err(invalid());
            }
            path(p)?;
            hunk = next;
            chunk = Chunk::None;
            moved = false;
            continue;
        }
        match hunk {
            Hunk::Add if line.starts_with('+') => {}
            Hunk::Update => {
                if chunk == Chunk::None
                    && !moved
                    && let Some(p) = header.strip_prefix("*** Move to: ")
                {
                    path(p)?;
                    moved = true;
                } else if header == "@@" || header.starts_with("@@ ") {
                    if chunk == Chunk::Empty {
                        return Err(invalid());
                    }
                    chunk = Chunk::Empty;
                } else if header == "*** End of File" {
                    if chunk != Chunk::Lines {
                        return Err(invalid());
                    }
                    chunk = Chunk::End;
                } else if chunk == Chunk::End {
                    if !header.is_empty() {
                        return Err(invalid());
                    }
                } else if line.is_empty() || line.starts_with([' ', '+', '-']) {
                    chunk = Chunk::Lines;
                } else {
                    return Err(invalid());
                }
            }
            _ => return Err(invalid()),
        }
    }
    if hunk == Hunk::Update && matches!(chunk, Chunk::None | Chunk::Empty) {
        return Err(invalid());
    }
    Ok(paths)
}

/// The shared core checks any session binding first. An unbound Codex hook
/// maps through saved locations or the configured worktree pattern when
/// available. A plain session with no mapping uses the core's effective actor
/// for mail, without acquiring receiver authority. Identity errors still refuse.
pub(super) fn bound_actor(root: &Path, input: &Input) -> Result<Option<String>, String> {
    let cwd = input
        .cwd
        .as_deref()
        .ok_or("Codex hook has no cwd for actor resolution")?;
    mdkg_run::identity::actor_for_cwd(root, cwd).map_err(|e| e.to_string())
}

pub(super) fn render(event: Event, outcome: &Outcome) -> Rendered {
    match (event, outcome) {
        (Event::Start, Outcome::Context(line)) => Rendered {
            stdout: format!("{line}\n"),
            ..Rendered::default()
        },
        (Event::Stop, Outcome::Block(reason)) | (Event::Edit, Outcome::Deny(reason)) => Rendered {
            stderr: format!("{reason}\n"),
            exit: 2,
            ..Rendered::default()
        },
        _ => Rendered::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn input(name: &str) -> Value {
        let mut value = json!({
            "hook_event_name": name,
            "cwd": std::env::temp_dir(),
            "session_id": "019b7fa0-0000-7000-8000-000000000001",
            "model": "gpt-6-astra",
            "permission_mode": "default",
            "transcript_path": "PRIVATE_TRANSCRIPT",
        });
        if name == "SessionStart" {
            value["source"] = json!("startup");
        } else {
            value["turn_id"] = json!("019b7fa0-0000-7000-8000-000000000002");
        }
        match name {
            "UserPromptSubmit" => value["prompt"] = json!("PRIVATE_PROMPT"),
            "Stop" => {
                value["stop_hook_active"] = json!(false);
                value["last_assistant_message"] = json!("PRIVATE_MESSAGE");
            }
            "PreToolUse" => value["tool_use_id"] = json!("call_1"),
            _ => {}
        }
        value
    }

    fn edit(patch: &str) -> Value {
        let mut v = input("PreToolUse");
        v["tool_name"] = json!("apply_patch");
        v["tool_input"] = json!({"command": patch});
        v
    }

    #[test]
    fn start_events_preserve_identity_without_prompt_or_transcript() {
        for name in ["SessionStart", "UserPromptSubmit"] {
            let mut v = input(name);
            if name == "SessionStart" {
                v.as_object_mut().expect("object").remove("turn_id");
            }
            let got = parse(Event::Start, &v).expect("parse");
            assert_eq!(got.cwd, Some(std::env::temp_dir()));
            assert_eq!(got.session.as_deref(), v["session_id"].as_str());
            assert_eq!(got.turn.as_deref(), v["turn_id"].as_str());
            assert!(got.paths.is_empty());
            assert!(!format!("{got:?}").contains("PRIVATE_"));
        }
    }

    #[test]
    fn provenance_distinguishes_start_events_and_keeps_every_known_source() {
        for source in ["startup", "resume", "clear", "compact", "fork"] {
            let mut v = input("SessionStart");
            v["source"] = json!(source);
            let got = parse(Event::Start, &v).expect("source");
            assert_eq!(got.harness_event, Some(HarnessEvent::SessionStart));
            assert_eq!(
                got.start_provenance,
                Some(StartProvenance::parse(Some(source), None))
            );
        }
        let mut v = input("UserPromptSubmit");
        v["source"] = json!("PRIVATE_UNRELATED_SOURCE");
        let got = parse(Event::Start, &v).expect("prompt");
        assert_eq!(got.harness_event, Some(HarnessEvent::UserPromptSubmit));
        assert_eq!(got.start_provenance, None);
    }

    #[test]
    fn provenance_keeps_optional_agent_id_only_on_session_start() {
        for (event, name, expected) in [
            (
                Event::Start,
                "SessionStart",
                Some(HarnessEvent::SessionStart),
            ),
            (
                Event::Start,
                "UserPromptSubmit",
                Some(HarnessEvent::UserPromptSubmit),
            ),
            (Event::Stop, "Stop", Some(HarnessEvent::Stop)),
            (Event::Edit, "PreToolUse", None),
        ] {
            let mut v = input(name);
            v["tool_name"] = json!("read_file");
            v["agent_type"] = json!("PRIVATE_AGENT_TYPE");
            v["agent_transcript_path"] = json!("PRIVATE_AGENT_TRANSCRIPT");
            let absent = parse(event, &v).expect("absent");
            assert!(
                absent
                    .start_provenance
                    .as_ref()
                    .is_none_or(|p| p.agent_id.is_none())
            );
            v["agent_id"] = Value::Null;
            assert_eq!(parse(event, &v).expect("null"), absent);
            let agent = "019b7fa0-0000-7000-8000-000000000003";
            v["agent_id"] = json!(agent);
            let got = parse(event, &v).expect("agent");
            assert_eq!(got.harness_event, expected);
            if name == "SessionStart" {
                assert_eq!(
                    got.start_provenance,
                    Some(StartProvenance::parse(Some("startup"), Some(agent)))
                );
            } else {
                assert_eq!(got.start_provenance, None);
            }
            assert!(!format!("{got:?}").contains("PRIVATE_"));
        }
    }

    #[test]
    fn invalid_start_source_is_not_reclassified_or_echoed() {
        let mut missing = input("SessionStart");
        missing.as_object_mut().expect("object").remove("source");
        assert!(parse(Event::Start, &missing).is_err());
        for wrong in [
            Value::Null,
            json!(17),
            json!({"private": "PRIVATE_SOURCE"}),
            json!(""),
            json!(" startup"),
            json!("resume\n"),
            json!("PRIVATE_UNKNOWN_SOURCE"),
        ] {
            let mut v = input("SessionStart");
            v["source"] = wrong;
            let err = parse(Event::Start, &v).expect_err("unknown source");
            assert!(!err.contains("PRIVATE_"));
        }
    }

    #[test]
    fn invalid_agent_id_is_not_treated_as_absent_or_echoed() {
        for wrong in [
            json!(17),
            json!(false),
            json!(["PRIVATE_AGENT"]),
            json!({"private": "PRIVATE_AGENT"}),
            json!(""),
            json!("   "),
            json!("PRIVATE_AGENT\n"),
            json!("PRIVATE_AGENT\0"),
            json!("PRIVATE_AGENT space"),
            json!("PRIVATE_AGENT:colon"),
            json!("é"),
            json!("x".repeat(129)),
        ] {
            for (event, name) in [(Event::Start, "SessionStart"), (Event::Stop, "Stop")] {
                let mut v = input(name);
                v["agent_id"] = wrong.clone();
                let err = parse(event, &v).expect_err("invalid agent id");
                assert!(!err.contains("PRIVATE_"));
            }
        }
    }

    #[test]
    fn malformed_metadata_is_not_silently_used_as_another_session() {
        for key in ["cwd", "session_id", "hook_event_name", "tool_name"] {
            for wrong in [Value::Null, json!(17), json!("")] {
                let mut v = edit("*** Begin Patch\n*** End Patch");
                v[key] = wrong;
                assert!(parse(Event::Edit, &v).is_err(), "{key}");
            }
        }
        let mut v = input("Stop");
        v["cwd"] = json!("relative");
        assert!(parse(Event::Stop, &v).is_err());
        let mut v = input("Stop");
        v["turn_id"] = json!({"bad": "PRIVATE_TURN"});
        let err = parse(Event::Stop, &v).expect_err("bad turn");
        assert!(!err.contains("PRIVATE_"));
        assert!(parse(Event::Edit, &input("Stop")).is_err());
        assert!(parse(Event::Start, &json!([])).is_err());
    }

    #[test]
    fn stop_active_is_typed_and_propagated() {
        for active in [false, true] {
            let mut v = input("Stop");
            v["stop_hook_active"] = json!(active);
            assert_eq!(parse(Event::Stop, &v).expect("parse").stop_active, active);
        }
        assert!(
            !parse(Event::Stop, &input("Stop"))
                .expect("parse")
                .stop_active
        );
        let mut v = input("Stop");
        v["stop_hook_active"] = json!("true");
        assert!(parse(Event::Stop, &v).is_err());
    }

    #[test]
    fn all_patch_operations_and_both_move_paths_are_checked_once() {
        let patch = "*** Begin Patch\n*** Add File: new.txt\n+new\n\
            *** Delete File: old.txt\n*** Update File: src/a.rs\n\
            *** Move to: dst/a.rs\n@@ function\n-old\n+new\n\
            *** Update File: src/a.rs\n@@\n+again\n*** End Patch";
        let got = parse(Event::Edit, &edit(patch)).expect("parse");
        assert_eq!(got.paths, ["new.txt", "old.txt", "src/a.rs", "dst/a.rs"]);
        assert_eq!(got.tool.as_deref(), Some("apply_patch"));
    }

    #[test]
    fn header_whitespace_matches_codex_without_trimming_the_path_again() {
        let patch = " \r\n *** Begin Patch \r\n  *** Add File:  leading name.txt  \r\n\
            +body\r\n *** Delete File: /absolute/name \r\n *** End Patch \r\n";
        assert_eq!(
            patch_paths(patch).expect("paths"),
            [" leading name.txt", "/absolute/name"]
        );
    }

    #[test]
    fn patch_like_content_is_not_a_file_header() {
        let patch = "*** Begin Patch\n*** Add File: a\n+*** Delete File: secret\n\
            *** Update File: b\n@@\n *** Delete File: context-only\n\
            +*** Add File: content-only\n-*** Move to: removed-content\n\
            *** End of File\n\n*** End Patch";
        assert_eq!(patch_paths(patch).expect("paths"), ["a", "b"]);
    }

    #[test]
    fn context_without_at_markers_and_empty_add_are_valid() {
        let patch = "*** Begin Patch\n*** Add File: empty\n*** Update File: existing\n\
            line\n*** End Patch";
        assert!(patch_paths(patch).is_err(), "unmarked text is not context");
        let patch = "*** Begin Patch\n*** Add File: empty\n*** Update File: existing\n\n context\n+line\n*** End Patch";
        assert_eq!(patch_paths(patch).expect("paths"), ["empty", "existing"]);
        assert!(
            patch_paths("*** Begin Patch\n*** End Patch")
                .expect("empty patch")
                .is_empty()
        );
    }

    #[test]
    fn malformed_patches_never_return_a_partial_write_set_or_echo_content() {
        for body in [
            "*** Add File: ",
            "*** Add File: a\nPRIVATE_CONTENT",
            "*** Delete File: a\n+PRIVATE_CONTENT",
            "*** Update File: a",
            "*** Update File: a\n@@",
            "*** Update File: a\n@@\n@@\n+x",
            "*** Update File: a\n*** Move to: ",
            "*** Update File: a\n*** Move to: b\n*** Move to: c\n+x",
            "*** Add File: a\n+x\n*** Move to: b",
            "*** Update File: a\n+x\n*** End of File\n+PRIVATE_CONTENT",
            "*** Update File: a\n@@\n*** Delete File: b",
            "*** Add File: a\0PRIVATE_CONTENT\n+x",
            "*** Begin Patch\n*** Delete File: b",
            "*** End Patch\n*** Delete File: b",
        ] {
            let patch = format!("*** Begin Patch\n{body}\n*** End Patch");
            let err = patch_paths(&patch).expect_err("invalid");
            assert!(!err.contains("PRIVATE_CONTENT"));
        }
        for patch in ["", "PRIVATE_CONTENT", "*** Begin Patch\n*** Delete File: a"] {
            assert!(patch_paths(patch).is_err());
        }
    }

    #[test]
    fn remote_environment_routing_is_not_a_local_write_set() {
        let patch = "*** Begin Patch\n*** Environment ID: other\n*** Delete File: a\n*** End Patch";
        assert!(
            patch_paths(patch)
                .expect_err("remote")
                .contains("environment routing")
        );
    }

    #[test]
    fn codex_lenient_literal_heredoc_wrappers_are_supported() {
        for opener in ["<<EOF", "<<'EOF'", "<<\"EOF\""] {
            let patch =
                format!("{opener}\n*** Begin Patch\n*** Delete File: a\n*** End Patch\nEOF\n");
            assert_eq!(patch_paths(&patch).expect("heredoc"), ["a"]);
        }
    }

    #[test]
    fn codex_lenient_heredoc_accepts_an_indented_or_prefixed_eof_suffix() {
        // 0.164.0 checks ends_with("EOF"), not equality, on the last line.
        for closer in ["  EOF", "prefixEOF"] {
            let patch =
                format!("<<'EOF'\n*** Begin Patch\n*** Delete File: a\n*** End Patch\n{closer}\n");
            assert_eq!(patch_paths(&patch).expect("lenient terminator"), ["a"]);
        }
    }

    #[test]
    fn patch_command_must_be_a_string_in_the_real_wire_field() {
        for wrong in [
            json!(null),
            json!("patch"),
            json!({"patch": "x"}),
            json!({"command": ["x"]}),
        ] {
            let mut v = edit("");
            v["tool_input"] = wrong;
            assert!(parse(Event::Edit, &v).is_err());
        }
    }

    #[test]
    fn shell_invoked_patch_is_left_to_the_shared_stop_audit() {
        let mut v = input("PreToolUse");
        v["tool_name"] = json!("Bash");
        v["tool_input"] = json!({"command": "apply_patch <<'EOF'\n*** Begin Patch\n*** Delete File: a\n*** End Patch\nEOF"});
        assert!(parse(Event::Edit, &v).expect("Bash").paths.is_empty());
        v["tool_name"] = json!("read_file");
        v["tool_input"] = json!({"path": "a"});
        assert!(parse(Event::Edit, &v).expect("read").paths.is_empty());
    }

    #[test]
    fn start_context_works_for_both_start_hooks() {
        for name in ["SessionStart", "UserPromptSubmit"] {
            parse(Event::Start, &input(name)).expect("start");
            let got = render(
                Event::Start,
                &Outcome::Context("You have mdkg mail: #7".into()),
            );
            assert_eq!(got.stdout, "You have mdkg mail: #7\n");
            assert!(got.stderr.is_empty());
            assert_eq!(got.exit, 0);
        }
    }

    #[test]
    fn stop_block_preserves_combined_mail_and_audit_on_stderr() {
        let reason = "Mail from sender.\n\nClaim \"src/a.rs\" before stopping.";
        let got = render(Event::Stop, &Outcome::Block(reason.into()));
        assert_eq!(got.stderr, format!("{reason}\n"));
        assert!(got.stdout.is_empty());
        assert_eq!(got.exit, 2);
    }

    #[test]
    fn edit_deny_exits_two_with_the_claim_command_on_stderr() {
        let reason = "Claim src/a.rs: `mdkg claim src/a.rs`";
        let got = render(Event::Edit, &Outcome::Deny(reason.into()));
        assert_eq!(got.stderr, format!("{reason}\n"));
        assert!(got.stdout.is_empty());
        assert_eq!(got.exit, 2);
    }

    #[cfg(unix)]
    #[test]
    fn stdout_banner_cannot_turn_a_deny_or_block_into_an_exit_zero_json_error() {
        let reason = "Claim \"src/a.rs\": `mdkg claim src/a.rs`\nMail remains unread.";
        for (event, outcome) in [
            (Event::Edit, Outcome::Deny(reason.into())),
            (Event::Stop, Outcome::Block(reason.into())),
        ] {
            let got = render(event, &outcome);
            // Simulate a login profile's stdout without reading real profiles.
            // Arguments are data: never interpolate a reason into shell code.
            let output = std::process::Command::new("/bin/sh")
                .env_clear()
                .args([
                    "-c",
                    "printf '%s\\n' 'login profile banner'; printf '%s' \"$1\"; printf '%s' \"$2\" >&2; exit \"$3\"",
                    "mdkg-codex-hook-test",
                ])
                .arg(&got.stdout)
                .arg(&got.stderr)
                .arg(got.exit.to_string())
                .output()
                .expect("shell output");
            assert!(serde_json::from_slice::<Value>(&output.stdout).is_err());
            // Codex's exit-2 contract uses stderr, not strict JSON on stdout.
            assert_eq!(output.status.code(), Some(2), "{event:?}");
            assert_eq!(output.stderr, format!("{reason}\n").as_bytes());
            assert_eq!(output.stdout, b"login profile banner\n");
        }
    }

    #[test]
    fn wait_and_unmatched_outcomes_are_silent() {
        for event in [Event::Start, Event::Stop, Event::Edit, Event::Wait] {
            assert_eq!(render(event, &Outcome::Silent), Rendered::default());
        }
        assert_eq!(
            render(Event::Wait, &Outcome::Wake("mail".into())),
            Rendered::default()
        );
        assert_eq!(
            render(Event::Edit, &Outcome::Block("mail".into())),
            Rendered::default()
        );
        assert!(parse(Event::Wait, &input("Stop")).is_err());
    }

    #[test]
    fn unbound_actor_never_falls_through_without_a_cwd() {
        assert!(bound_actor(Path::new("unused"), &Input::default()).is_err());
    }
}
