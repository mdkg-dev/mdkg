//! OpenCode's side: what mdkg's own plugin (`.mdkg/hooks/opencode/mdkg.js`,
//! OpenCode 2.1's plugin API) sends and reads back. The plugin passes
//! `{"cwd", "sessionID", "tool", "args"}` on stdin and reads one JSON line:
//! `{"context": …}` for the prompt hook, `{"block": …}` for a queued
//! reminder before the session goes idle, `{"wake": …}` to prompt an idle
//! session, `{"deny": …}` to throw from `execute.before`, or `{}`.

use serde_json::{Value, json};

use super::{Event, HarnessEvent, Input, Outcome, Rendered};

use mdkg_run::hooks::{OPENCODE_FILE_TOOLS as FILE_TOOLS, OPENCODE_PATCH_TOOLS as PATCH_TOOLS};

/// The lines of a patch that name a file it writes.
const PATCH_FILE_LINES: &[&str] = &[
    "*** Add File: ",
    "*** Update File: ",
    "*** Delete File: ",
    "*** Move to: ",
];

pub(super) fn parse(event: Event, v: &Value) -> Result<Input, String> {
    let text = |k: &str| v.get(k).and_then(Value::as_str).map(String::from);
    let tool = text("tool");
    let args = v.get("args");
    let arg = |k: &str| args.and_then(|a| a.get(k)).and_then(Value::as_str);
    // OpenCode 2.1's `write` takes `path` (spike); older tools `filePath`.
    // A tool that writes a file must name it, and a patch must name what it
    // writes: one whose input can't be read is an error, never an edit that
    // writes nothing.
    let paths = match tool.as_deref() {
        Some(t) if FILE_TOOLS.contains(&t) => {
            match arg("path")
                .or_else(|| arg("filePath"))
                .or_else(|| arg("file_path"))
            {
                Some(p) if !p.is_empty() => vec![p.to_string()],
                _ => return Err(format!("{t}'s input names no file")),
            }
        }
        Some(t) if PATCH_TOOLS.contains(&t) => {
            let paths = arg("patchText")
                .or_else(|| arg("patch"))
                .or_else(|| arg("command"))
                .map(patch_paths)
                .unwrap_or_default();
            if paths.is_empty() {
                return Err(format!("{t}'s input names no file it writes"));
            }
            paths
        }
        _ => Vec::new(),
    };
    Ok(Input {
        cwd: text("cwd").map(Into::into),
        session: text("sessionID"),
        turn: None,
        harness_event: event.observation().and_then(|event| {
            HarnessEvent::parse("opencode", event, v.get("hook_event_name")?.as_str()?)
        }),
        start_provenance: None,
        stop_active: false,
        tool,
        paths,
    })
}

/// The files a patch writes, in order, once each.
fn patch_paths(patch: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in patch.lines() {
        if let Some(p) = PATCH_FILE_LINES
            .iter()
            .find_map(|prefix| line.strip_prefix(prefix))
        {
            let p = p.trim().to_string();
            if !p.is_empty() && !out.contains(&p) {
                out.push(p);
            }
        }
    }
    out
}

pub(super) fn render(_event: Event, o: &Outcome) -> Rendered {
    let v = match o {
        Outcome::Silent => json!({}),
        Outcome::Context(s) => json!({"context": s}),
        Outcome::Block(s) => json!({"block": s}),
        Outcome::Deny(s) => json!({"deny": s}),
        Outcome::Wake(s) => json!({"wake": s}),
    };
    Rendered {
        stdout: format!("{v}\n"),
        ..Rendered::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observations_name_only_the_plugins_matching_event() {
        for (event, name, expected) in [
            (Event::Start, "context", Some(HarnessEvent::Context)),
            (
                Event::Stop,
                "session.execution.succeeded",
                Some(HarnessEvent::SessionExecutionSucceeded),
            ),
            (Event::Start, "SessionStart", None),
            (Event::Stop, "context", None),
            (Event::Start, "context\nfree text", None),
        ] {
            let input = parse(
                event,
                &json!({"hook_event_name":name, "source":"startup", "agent_id":"agent-1"}),
            )
            .expect("parse");
            assert_eq!(input.harness_event, expected);
            assert!(input.start_provenance.is_none());
        }
    }

    #[test]
    fn edit_write_and_patch_name_their_files() {
        let write = parse(
            Event::Edit,
            &json!({"tool": "write", "args": {"path": "a.txt", "content": "hi"}}),
        )
        .expect("parse");
        assert_eq!(write.paths, ["a.txt"]);
        for bad in [
            json!({"tool": "write", "args": {"content": "hi"}}),
            json!({"tool": "apply_patch", "args": {"patchText": "no files"}}),
        ] {
            assert!(parse(Event::Edit, &bad).is_err(), "{bad}");
        }
        let edit = parse(
            Event::Edit,
            &json!({"cwd": "/w", "sessionID": "ses_1", "tool": "edit",
                    "args": {"filePath": "src/a.rs", "oldString": "x"}}),
        )
        .expect("parse");
        assert_eq!(edit.paths, ["src/a.rs"]);
        assert_eq!(edit.session.as_deref(), Some("ses_1"));
        let patch = parse(
            Event::Edit,
            &json!({"tool": "apply_patch", "args": {"patchText":
                "*** Begin Patch\n*** Update File: a.rs\n@@\n-x\n+y\n*** Add File: b.rs\n+z\n\
                 *** Update File: c.rs\n*** Move to: d.rs\n*** Update File: a.rs\n*** End Patch"}}),
        )
        .expect("parse");
        assert_eq!(patch.paths, ["a.rs", "b.rs", "c.rs", "d.rs"]);
        let read = parse(
            Event::Edit,
            &json!({"tool": "read", "args": {"filePath": "a"}}),
        )
        .expect("parse");
        assert!(read.paths.is_empty());
    }

    #[test]
    fn every_outcome_is_one_json_line() {
        for (o, key) in [
            (Outcome::Context("c".into()), "context"),
            (Outcome::Block("b".into()), "block"),
            (Outcome::Deny("d".into()), "deny"),
            (Outcome::Wake("w".into()), "wake"),
        ] {
            let r = render(Event::Start, &o);
            assert_eq!(r.exit, 0);
            let v: Value = serde_json::from_str(r.stdout.trim()).expect("json");
            assert!(v[key].is_string(), "{v}");
        }
        assert_eq!(render(Event::Edit, &Outcome::Silent).stdout, "{}\n");
    }
}
