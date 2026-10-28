//! Claude Code's hook input and output (2.1.309).
//!
//! - `start`, from SessionStart and UserPromptSubmit: stdout is added to the
//!   turn's context.
//! - `stop`, from Stop: `{"decision": "block", "reason": …}` keeps Claude
//!   going once. Its input's `stop_hook_active` says this stop follows a
//!   block.
//! - `wait`, from an async Stop hook with `asyncRewake`: exit 2 wakes the
//!   session, with stderr as what it's told.
//! - `edit`, from PreToolUse on Edit, Write, MultiEdit and NotebookEdit:
//!   `permissionDecision: deny` refuses the call.

use serde_json::{Value, json};

use super::{Event, HarnessEvent, Input, Outcome, Rendered, StartProvenance};

/// The tools that write a file, and the input field naming it.
const EDIT_TOOLS: &[(&str, &str)] = &[
    ("Edit", "file_path"),
    ("Write", "file_path"),
    ("MultiEdit", "file_path"),
    ("NotebookEdit", "notebook_path"),
];

pub(super) fn parse(event: Event, v: &Value) -> Result<Input, String> {
    let text = |k: &str| v.get(k).and_then(Value::as_str).map(String::from);
    let harness_event = event.observation().and_then(|event| {
        HarnessEvent::parse("claude", event, v.get("hook_event_name")?.as_str()?)
    });
    let tool = text("tool_name");
    // A tool that writes a file must name it: one whose input can't be
    // read is an error, never an edit that writes nothing.
    let paths = match tool
        .as_deref()
        .and_then(|t| EDIT_TOOLS.iter().find(|(name, _)| *name == t))
    {
        Some((name, field)) => match v.get("tool_input").and_then(|i| i.get(*field)) {
            Some(Value::String(p)) if !p.is_empty() => vec![p.clone()],
            _ => return Err(format!("{name}'s input names no {field}")),
        },
        None => Vec::new(),
    };
    Ok(Input {
        cwd: text("cwd").map(Into::into),
        session: text("session_id"),
        turn: None,
        harness_event,
        start_provenance: (harness_event == Some(HarnessEvent::SessionStart)).then(|| {
            StartProvenance::parse(
                v.get("source").and_then(Value::as_str),
                v.get("agent_id").and_then(Value::as_str),
            )
        }),
        stop_active: v.get("stop_hook_active").and_then(Value::as_bool) == Some(true),
        tool,
        paths,
    })
}

pub(super) fn render(event: Event, o: &Outcome) -> Rendered {
    match (event, o) {
        (Event::Start, Outcome::Context(line)) => Rendered {
            stdout: format!("{line}\n"),
            ..Rendered::default()
        },
        (Event::Stop, Outcome::Block(reason)) => Rendered {
            stdout: format!("{}\n", json!({"decision": "block", "reason": reason})),
            ..Rendered::default()
        },
        (Event::Wait, Outcome::Wake(line)) => Rendered {
            stderr: format!("{line}\n"),
            exit: 2,
            ..Rendered::default()
        },
        (Event::Edit, Outcome::Deny(why)) => Rendered {
            stdout: format!(
                "{}\n",
                json!({"hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "permissionDecision": "deny",
                    "permissionDecisionReason": why,
                }})
            ),
            ..Rendered::default()
        },
        _ => Rendered::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_provenance_is_whitelisted_and_only_belongs_to_session_start() {
        let raw =
            json!({"hook_event_name":"SessionStart", "source":"resume", "agent_id":"agent_01-ABC"});
        let input = parse(Event::Start, &raw).expect("start");
        assert_eq!(input.harness_event, Some(HarnessEvent::SessionStart));
        assert_eq!(
            input.start_provenance,
            Some(StartProvenance::parse(Some("resume"), Some("agent_01-ABC")))
        );
        for (event, name) in [
            (Event::Start, "UserPromptSubmit"),
            (Event::Stop, "Stop"),
            (Event::Start, "unrecognized"),
            (Event::Stop, "SessionStart"),
        ] {
            let input = parse(
                event,
                &json!({"hook_event_name":name, "source":"fork", "agent_id":"agent1"}),
            )
            .expect("parse");
            assert!(input.start_provenance.is_none());
        }
        let input = parse(Event::Start, &json!({"hook_event_name":"SessionStart", "source":"startup\nfree text", "agent_id":"do something"})).expect("parse");
        assert_eq!(input.start_provenance, Some(StartProvenance::default()));
        let input = parse(Event::Start, &json!({"source":"startup"})).expect("no event");
        assert!(input.harness_event.is_none());
        assert!(input.start_provenance.is_none());
    }

    #[test]
    fn an_edit_names_its_file_and_a_read_names_none() {
        let edit = parse(
            Event::Edit,
            &json!({"session_id": "s1", "cwd": "/w", "tool_name": "Edit",
                    "tool_input": {"file_path": "/w/src/lib.rs", "old_string": "a"}}),
        )
        .expect("parse");
        assert_eq!(edit.paths, ["/w/src/lib.rs"]);
        assert_eq!(edit.session.as_deref(), Some("s1"));
        assert_eq!(edit.cwd.as_deref(), Some(std::path::Path::new("/w")));
        let nb = parse(
            Event::Edit,
            &json!({"tool_name": "NotebookEdit", "tool_input": {"notebook_path": "n.ipynb"}}),
        )
        .expect("parse");
        assert_eq!(nb.paths, ["n.ipynb"]);
        let read = parse(
            Event::Edit,
            &json!({"tool_name": "Read", "tool_input": {"file_path": "/w/x"}}),
        )
        .expect("parse");
        assert!(read.paths.is_empty());
    }

    #[test]
    fn an_edit_naming_no_file_is_an_error() {
        for v in [
            json!({"tool_name": "Write", "tool_input": {"content": "x"}}),
            json!({"tool_name": "Edit", "tool_input": {"file_path": 7}}),
            json!({"tool_name": "Edit"}),
        ] {
            assert!(parse(Event::Edit, &v).is_err(), "{v}");
        }
    }

    #[test]
    fn stop_hook_active_is_read() {
        assert!(
            parse(Event::Stop, &json!({"stop_hook_active": true}))
                .expect("parse")
                .stop_active
        );
        assert!(!parse(Event::Stop, &json!({})).expect("parse").stop_active);
    }

    #[test]
    fn each_outcome_renders_as_claude_reads_it() {
        let ctx = render(Event::Start, &Outcome::Context("You have mdkg mail".into()));
        assert_eq!(ctx.stdout, "You have mdkg mail\n");
        assert_eq!(ctx.exit, 0);
        let block = render(Event::Stop, &Outcome::Block("why".into()));
        let v: Value = serde_json::from_str(&block.stdout).expect("json");
        assert_eq!(v, json!({"decision": "block", "reason": "why"}));
        let wake = render(Event::Wait, &Outcome::Wake("mail".into()));
        assert_eq!((wake.exit, wake.stderr.as_str()), (2, "mail\n"));
        let deny = render(Event::Edit, &Outcome::Deny("claim it".into()));
        let v: Value = serde_json::from_str(&deny.stdout).expect("json");
        assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
        assert_eq!(
            v["hookSpecificOutput"]["permissionDecisionReason"],
            "claim it"
        );
        assert_eq!(render(Event::Stop, &Outcome::Silent), Rendered::default());
    }
}
