//! A turn's live output (API v1 A11): each harness's native output, line
//! by line, normalized to turn output events as the launcher reads it,
//! beside the post-turn reading in [`crate::parse`].
//!
//! - Claude: its `stream-json` lines: `assistant` text and `tool_use`,
//!   `user` `tool_result`, and the final `result` with its usage.
//! - Codex: run without `--json`, so its stderr progress, as `status`; its
//!   `tokens used` footer as `usage`; and its `-o` reply at the end, as
//!   `text`. It reports no tools (`output.tools` is false).
//! - OpenCode: its `--format json` events: text parts, tool parts, and
//!   each step's `tokens` and `cost`, summed.
//!
//! A line no mapping covers is a `native` event, which readers show only
//! when asked; a stderr line is a `status`. Output text is the agent's and
//! untrusted; on an API-key turn it's the redacted text the launcher
//! wrote.
//!
//! [`Tailer`] reads the turn's stdout and stderr files as they grow, in
//! the order it finds their lines, and appends every event to the journal,
//! so the journal is the one source every reader tails.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::{Value, json};

use super::journal;
use crate::Harness;

/// The longest text an event carries; the rest is cut, marked.
const MAX_TEXT: usize = 64 * 1024;
/// The longest tool summary.
const MAX_SUMMARY: usize = 200;
/// How often the tailer looks at the turn's files.
const POLL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

/// What each harness's live output can carry (A17, A20): a type it can't
/// produce is named here, never left out silently.
pub fn capabilities(harness: Harness) -> &'static [&'static str] {
    match harness {
        Harness::Claude => &["output.text", "output.tools", "output.usage"],
        Harness::Codex => &["output.text", "output.usage"],
        Harness::OpenCode => &["output.text", "output.tools", "output.usage"],
    }
}

/// edd-01a183ef's tool classes, from a tool's name.
pub fn tool_class(harness: Harness, name: &str) -> &'static str {
    let lower = name.to_ascii_lowercase();
    if lower.starts_with("mcp__") || (harness == Harness::OpenCode && lower.contains("mdkg_")) {
        return "mcp";
    }
    match lower.as_str() {
        "bash" | "shell" | "exec" | "local_shell" => "shell",
        "edit" | "multiedit" | "write" | "notebookedit" | "patch" | "apply_patch" => "edit",
        "read" | "glob" | "grep" | "ls" | "list" | "webfetch" | "websearch" => "read",
        _ => "other",
    }
}

fn cut(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// A tool call's one-line summary: its command, path or pattern.
fn summary(input: Option<&Value>) -> String {
    let Some(input) = input else {
        return String::new();
    };
    for key in [
        "command",
        "file_path",
        "filePath",
        "path",
        "pattern",
        "url",
        "query",
        "description",
    ] {
        if let Some(text) = input.get(key).and_then(Value::as_str) {
            let line = text.lines().next().unwrap_or_default();
            return cut(line, MAX_SUMMARY);
        }
    }
    String::new()
}

/// One normalized event: its type and data.
pub type Out = (&'static str, Value);

/// A harness's live parser for one turn.
#[derive(Debug)]
pub struct Parser {
    harness: Harness,
    /// Tool calls started and not ended: id to (class, name).
    tools: BTreeMap<String, (&'static str, String)>,
    /// Codex printed `tokens used`; its count is next.
    codex_tokens: bool,
    /// OpenCode's usage so far, summed over its steps.
    input: Option<u64>,
    output: Option<u64>,
    cache_read: Option<u64>,
    cost: Option<f64>,
}

impl Parser {
    pub fn new(harness: Harness) -> Self {
        Self {
            harness,
            tools: BTreeMap::new(),
            codex_tokens: false,
            input: None,
            output: None,
            cache_read: None,
            cost: None,
        }
    }

    /// The events one line gives.
    pub fn line(&mut self, stream: Stream, line: &str) -> Vec<Out> {
        let line = line.trim_end_matches('\r');
        if line.trim().is_empty() {
            return Vec::new();
        }
        match (self.harness, stream) {
            (Harness::Codex, _) => self.codex(line),
            (_, Stream::Stderr) => vec![("status", json!({"text": cut(line, MAX_TEXT)}))],
            (Harness::Claude, Stream::Stdout) => self.claude(line),
            (Harness::OpenCode, Stream::Stdout) => self.opencode(line),
        }
    }

    fn native(&self, event: Value) -> Out {
        ("native", json!({"harness": self.harness, "event": event}))
    }

    fn claude(&mut self, line: &str) -> Vec<Out> {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return vec![("status", json!({"text": cut(line, MAX_TEXT)}))];
        };
        let content = v
            .pointer("/message/content")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut out = Vec::new();
        match v.get("type").and_then(Value::as_str) {
            Some("assistant") => {
                for block in &content {
                    match block.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(text) = block.get("text").and_then(Value::as_str) {
                                out.push(("text", json!({"text": cut(text, MAX_TEXT)})));
                            }
                        }
                        Some("tool_use") => {
                            let name = block
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string();
                            let class = tool_class(Harness::Claude, &name);
                            let id = block
                                .get("id")
                                .and_then(Value::as_str)
                                .filter(|id| !id.is_empty());
                            if let Some(id) = id {
                                self.tools.insert(id.into(), (class, name.clone()));
                            }
                            out.push((
                                "tool.started",
                                json!({"class": class, "name": name, "call_id": id,
                                    "summary": summary(block.get("input"))}),
                            ));
                        }
                        _ => out.push(self.native(block.clone())),
                    }
                }
            }
            Some("user") => {
                for block in &content {
                    if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                        out.push(self.native(block.clone()));
                        continue;
                    }
                    let id = block
                        .get("tool_use_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let (class, name) = self.tools.remove(id).unwrap_or(("other", String::new()));
                    let ok = block.get("is_error").and_then(Value::as_bool) != Some(true);
                    let call_id = Some(id).filter(|id| !id.is_empty());
                    out.push((
                        "tool.ended",
                        json!({"class": class, "name": name, "call_id": call_id, "ok": ok}),
                    ));
                }
            }
            Some("result") => {
                let n = |key: &str| v.pointer(&format!("/usage/{key}")).and_then(Value::as_u64);
                out.push((
                    "usage",
                    json!({
                        "input_tokens": n("input_tokens"),
                        "output_tokens": n("output_tokens"),
                        "cache_read_tokens": n("cache_read_input_tokens"),
                        "total_tokens": null,
                        "cost_usd": v.get("total_cost_usd").and_then(Value::as_f64),
                    }),
                ));
                if v.get("is_error").and_then(Value::as_bool) == Some(true) {
                    let what = v.get("subtype").and_then(Value::as_str).unwrap_or("error");
                    out.push((
                        "error",
                        json!({"text": format!("Claude ended the turn with {what}")}),
                    ));
                }
            }
            _ => out.push(self.native(v)),
        }
        out
    }

    fn codex(&mut self, line: &str) -> Vec<Out> {
        let trimmed = line.trim();
        if self.codex_tokens {
            self.codex_tokens = false;
            if let Ok(total) = trimmed.replace(',', "").parse::<u64>() {
                return vec![(
                    "usage",
                    json!({"input_tokens": null, "output_tokens": null,
                        "cache_read_tokens": null, "total_tokens": total, "cost_usd": null}),
                )];
            }
        }
        if trimmed == "tokens used" {
            self.codex_tokens = true;
            return Vec::new();
        }
        vec![("status", json!({"text": cut(line, MAX_TEXT)}))]
    }

    fn opencode(&mut self, line: &str) -> Vec<Out> {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return vec![("status", json!({"text": cut(line, MAX_TEXT)}))];
        };
        let part = v.get("part");
        let mut out = Vec::new();
        match v.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = part.and_then(|p| p.get("text")).and_then(Value::as_str) {
                    out.push(("text", json!({"text": cut(text, MAX_TEXT)})));
                }
            }
            Some("tool_use") | Some("tool") => {
                let name = part
                    .and_then(|p| p.get("tool"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let id = part
                    .and_then(|p| p.get("callID").or_else(|| p.get("id")))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let state = part.and_then(|p| p.get("state"));
                let status = state
                    .and_then(|s| s.get("status"))
                    .and_then(Value::as_str)
                    .unwrap_or("completed");
                let class = tool_class(Harness::OpenCode, &name);
                let call_id = Some(id.as_str()).filter(|id| !id.is_empty());
                if !self.tools.contains_key(&id) {
                    self.tools.insert(id.clone(), (class, name.clone()));
                    out.push((
                        "tool.started",
                        json!({"class": class, "name": name, "call_id": call_id,
                            "summary": summary(state.and_then(|s| s.get("input")))}),
                    ));
                }
                if matches!(status, "completed" | "error") {
                    self.tools.remove(&id);
                    out.push((
                        "tool.ended",
                        json!({"class": class, "name": name, "call_id": call_id,
                            "ok": status == "completed"}),
                    ));
                }
            }
            Some("step_finish") => {
                let tokens = part.and_then(|p| p.get("tokens"));
                let add = |to: &mut Option<u64>, n: Option<u64>| {
                    if let Some(n) = n {
                        *to = Some(to.unwrap_or(0) + n);
                    }
                };
                add(
                    &mut self.input,
                    tokens.and_then(|t| t.get("input")).and_then(Value::as_u64),
                );
                add(
                    &mut self.output,
                    tokens.and_then(|t| t.get("output")).and_then(Value::as_u64),
                );
                add(
                    &mut self.cache_read,
                    tokens
                        .and_then(|t| t.pointer("/cache/read"))
                        .and_then(Value::as_u64),
                );
                if let Some(c) = part.and_then(|p| p.get("cost")).and_then(Value::as_f64) {
                    self.cost = Some(self.cost.unwrap_or(0.0) + c);
                }
                out.push((
                    "usage",
                    json!({"input_tokens": self.input, "output_tokens": self.output,
                        "cache_read_tokens": self.cache_read, "total_tokens": null,
                        "cost_usd": self.cost}),
                ));
            }
            Some("error") => {
                let text = v
                    .pointer("/error/data/message")
                    .or_else(|| v.pointer("/error/name"))
                    .and_then(Value::as_str)
                    .unwrap_or("OpenCode reported an error");
                out.push(("error", json!({"text": cut(text, MAX_TEXT)})));
            }
            _ => out.push(self.native(v)),
        }
        out
    }
}

/// The journal and the parser, which only the tailer uses while the
/// harness runs.
#[derive(Debug)]
pub struct Sink {
    pub writer: journal::Writer,
    pub parser: Parser,
    /// The first journal write that failed; journaling stops there, and
    /// the turn goes on.
    pub failed: Option<crate::RunError>,
}

impl Sink {
    pub fn feed(&mut self, stream: Stream, line: &str) {
        if self.failed.is_some() {
            return;
        }
        if let Err(e) = crate::seam::output(&mut self.writer, self.parser.line(stream, line)) {
            self.failed = Some(e);
        }
    }
}

/// One of the turn's files, read as it grows.
struct Follow {
    path: PathBuf,
    stream: Stream,
    file: Option<File>,
    offset: u64,
    pending: Vec<u8>,
}

impl Follow {
    fn new(path: &Path, stream: Stream) -> Self {
        Self {
            path: path.to_path_buf(),
            stream,
            file: None,
            offset: 0,
            pending: Vec::new(),
        }
    }

    /// Feeds every complete line written since the last read; at the end,
    /// the last line too, newline or not.
    fn read(&mut self, sink: &mut Sink, end: bool) {
        if self.file.is_none() {
            self.file = File::open(&self.path).ok();
        }
        if let Some(file) = self.file.as_mut()
            && file.seek(SeekFrom::Start(self.offset)).is_ok()
        {
            let mut buf = Vec::new();
            if let Ok(n) = file.read_to_end(&mut buf) {
                self.offset += n as u64;
                self.pending.extend_from_slice(&buf);
            }
        }
        while let Some(i) = self.pending.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=i).collect();
            sink.feed(self.stream, &String::from_utf8_lossy(&line[..i]));
        }
        if end && !self.pending.is_empty() {
            let line = std::mem::take(&mut self.pending);
            sink.feed(self.stream, &String::from_utf8_lossy(&line));
        }
    }
}

/// Follows a turn's stdout and stderr files into its journal.
#[derive(Debug)]
pub struct Tailer {
    stop: Arc<AtomicBool>,
    handle: std::thread::JoinHandle<Sink>,
}

impl Tailer {
    pub fn start(stdout: &Path, stderr: &Path, mut sink: Sink) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let mut files = [
            Follow::new(stdout, Stream::Stdout),
            Follow::new(stderr, Stream::Stderr),
        ];
        let stopping = stop.clone();
        let handle = std::thread::spawn(move || {
            loop {
                let end = stopping.load(Ordering::SeqCst);
                for f in &mut files {
                    f.read(&mut sink, end);
                }
                if end {
                    return sink;
                }
                std::thread::sleep(POLL);
            }
        });
        Self { stop, handle }
    }

    /// Reads what's left once the harness and its output copies are done,
    /// and gives the journal back. `None` if the tailer itself failed.
    pub fn finish(self) -> Option<Sink> {
        self.stop.store(true, Ordering::SeqCst);
        self.handle.join().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn replay(harness: Harness, lines: &[(Stream, &str)]) -> Vec<Out> {
        let mut p = Parser::new(harness);
        lines.iter().flat_map(|(s, l)| p.line(*s, l)).collect()
    }

    fn kinds(events: &[Out]) -> Vec<&str> {
        events.iter().map(|(k, _)| *k).collect()
    }

    #[test]
    fn claude_stream_json_replays_to_text_tools_and_usage() {
        let out = replay(
            Harness::Claude,
            &[
                (
                    Stream::Stdout,
                    r#"{"type":"system","subtype":"init","session_id":"s"}"#,
                ),
                (
                    Stream::Stdout,
                    r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Looking."},{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"cargo test -p mdkg\nsecond line"}}]}}"#,
                ),
                (
                    Stream::Stdout,
                    r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","is_error":false,"content":"ok"}]}}"#,
                ),
                (
                    Stream::Stdout,
                    r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t2","name":"mcp__mdkg__inbox","input":{}}]}}"#,
                ),
                (
                    Stream::Stdout,
                    r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t2","is_error":true}]}}"#,
                ),
                (Stream::Stderr, "a warning"),
                (
                    Stream::Stdout,
                    r#"{"type":"result","subtype":"success","is_error":false,"result":"done","session_id":"s","total_cost_usd":0.5,"usage":{"input_tokens":7,"output_tokens":90,"cache_read_input_tokens":1000}}"#,
                ),
            ],
        );
        assert_eq!(
            kinds(&out),
            [
                "native",
                "text",
                "tool.started",
                "tool.ended",
                "tool.started",
                "tool.ended",
                "status",
                "usage"
            ]
        );
        assert_eq!(out[1].1, json!({"text": "Looking."}));
        assert_eq!(
            out[2].1,
            json!({"class": "shell", "name": "Bash", "call_id": "t1", "summary": "cargo test -p mdkg"})
        );
        assert_eq!(
            out[3].1,
            json!({"class": "shell", "name": "Bash", "call_id": "t1", "ok": true})
        );
        assert_eq!(out[4].1["class"], "mcp");
        assert_eq!(out[5].1["ok"], false);
        assert_eq!(
            out[7].1,
            json!({"input_tokens": 7, "output_tokens": 90, "cache_read_tokens": 1000,
                "total_tokens": null, "cost_usd": 0.5})
        );
        assert_eq!(out[0].1["harness"], "claude");
        assert_eq!(out[0].1["event"]["subtype"], "init");
    }

    #[test]
    fn a_failed_claude_result_is_an_error_event() {
        let out = replay(
            Harness::Claude,
            &[(
                Stream::Stdout,
                r#"{"type":"result","subtype":"error_max_turns","is_error":true}"#,
            )],
        );
        assert_eq!(kinds(&out), ["usage", "error"]);
    }

    #[test]
    fn codex_progress_replays_to_status_and_its_footer_to_usage() {
        let out = replay(
            Harness::Codex,
            &[
                (Stream::Stderr, "OpenAI Codex v0.163.0"),
                (Stream::Stderr, "--------"),
                (Stream::Stderr, "session id: abc"),
                (Stream::Stderr, ""),
                (Stream::Stderr, "exec bash -lc 'cargo test'"),
                (Stream::Stderr, "tokens used"),
                (Stream::Stderr, "7,382"),
            ],
        );
        assert_eq!(
            kinds(&out),
            ["status", "status", "status", "status", "usage"]
        );
        assert_eq!(out[4].1["total_tokens"], 7382);
        // Codex reports no tools.
        assert!(!capabilities(Harness::Codex).contains(&"output.tools"));
        assert!(out.iter().all(|(k, _)| !k.starts_with("tool.")));
    }

    #[test]
    fn opencode_json_events_replay_to_text_tools_and_summed_usage() {
        let out = replay(
            Harness::OpenCode,
            &[
                (
                    Stream::Stdout,
                    r#"{"type":"step_start","sessionID":"ses","part":{}}"#,
                ),
                (
                    Stream::Stdout,
                    r#"{"type":"tool_use","sessionID":"ses","part":{"type":"tool","callID":"c1","tool":"bash","state":{"status":"completed","input":{"command":"ls"}}}}"#,
                ),
                (
                    Stream::Stdout,
                    r#"{"type":"tool_use","sessionID":"ses","part":{"type":"tool","callID":"c2","tool":"edit","state":{"status":"error","input":{"filePath":"/x/a.rs"}}}}"#,
                ),
                (
                    Stream::Stdout,
                    r#"{"type":"step_finish","part":{"tokens":{"input":5,"output":6,"cache":{"read":7}},"cost":0.25}}"#,
                ),
                (
                    Stream::Stdout,
                    r#"{"type":"text","sessionID":"ses","part":{"text":"opencode reply"}}"#,
                ),
                (
                    Stream::Stdout,
                    r#"{"type":"step_finish","part":{"tokens":{"input":1,"output":2,"cache":{"read":3}},"cost":0.25}}"#,
                ),
            ],
        );
        assert_eq!(
            kinds(&out),
            [
                "native",
                "tool.started",
                "tool.ended",
                "tool.started",
                "tool.ended",
                "usage",
                "text",
                "usage"
            ]
        );
        assert_eq!(
            out[1].1,
            json!({"class": "shell", "name": "bash", "call_id": "c1", "summary": "ls"})
        );
        assert_eq!(
            out[4].1,
            json!({"class": "edit", "name": "edit", "call_id": "c2", "ok": false})
        );
        assert_eq!(
            out[7].1,
            json!({"input_tokens": 6, "output_tokens": 8, "cache_read_tokens": 10,
                "total_tokens": null, "cost_usd": 0.5})
        );
        assert_eq!(out[6].1, json!({"text": "opencode reply"}));
    }

    /// Two calls of one tool overlap and end in the other order: each
    /// tool.ended carries the call_id of the tool.started it ends.
    #[test]
    fn overlapping_calls_with_one_name_pair_up_by_call_id() {
        let ids = |out: &[Out]| -> Vec<(String, Value, Value)> {
            out.iter()
                .filter(|(k, _)| k.starts_with("tool."))
                .map(|(k, d)| (k.to_string(), d["call_id"].clone(), d["ok"].clone()))
                .collect()
        };
        let claude = replay(
            Harness::Claude,
            &[
                (
                    Stream::Stdout,
                    r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_a","name":"Bash","input":{"command":"sleep 2"}},{"type":"tool_use","id":"toolu_b","name":"Bash","input":{"command":"false"}}]}}"#,
                ),
                (
                    Stream::Stdout,
                    r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_b","is_error":true}]}}"#,
                ),
                (
                    Stream::Stdout,
                    r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_a","is_error":false}]}}"#,
                ),
            ],
        );
        let opencode = replay(
            Harness::OpenCode,
            &[
                (
                    Stream::Stdout,
                    r#"{"type":"tool_use","part":{"type":"tool","callID":"call_a","tool":"bash","state":{"status":"running","input":{"command":"sleep 2"}}}}"#,
                ),
                (
                    Stream::Stdout,
                    r#"{"type":"tool_use","part":{"type":"tool","callID":"call_b","tool":"bash","state":{"status":"running","input":{"command":"false"}}}}"#,
                ),
                (
                    Stream::Stdout,
                    r#"{"type":"tool_use","part":{"type":"tool","callID":"call_b","tool":"bash","state":{"status":"error"}}}"#,
                ),
                (
                    Stream::Stdout,
                    r#"{"type":"tool_use","part":{"type":"tool","callID":"call_a","tool":"bash","state":{"status":"completed"}}}"#,
                ),
            ],
        );
        for (out, a, b) in [
            (&claude, "toolu_a", "toolu_b"),
            (&opencode, "call_a", "call_b"),
        ] {
            assert_eq!(
                ids(out),
                [
                    ("tool.started".into(), json!(a), Value::Null),
                    ("tool.started".into(), json!(b), Value::Null),
                    ("tool.ended".into(), json!(b), json!(false)),
                    ("tool.ended".into(), json!(a), json!(true)),
                ]
            );
        }
        // Codex runs without --json and reports no tools, so it has no
        // call ids to carry (`output.tools` is false).
        let codex = replay(
            Harness::Codex,
            &[
                (Stream::Stderr, "exec bash -lc 'ls'"),
                (Stream::Stderr, "tokens used"),
            ],
        );
        assert!(ids(&codex).is_empty());
        // An OpenCode part with no callID is named by its part id.
        let by_part = replay(
            Harness::OpenCode,
            &[(
                Stream::Stdout,
                r#"{"type":"tool_use","part":{"type":"tool","id":"prt_1","tool":"read","state":{"status":"completed","input":{"filePath":"/x"}}}}"#,
            )],
        );
        assert_eq!(
            ids(&by_part),
            [
                ("tool.started".into(), json!("prt_1"), Value::Null),
                ("tool.ended".into(), json!("prt_1"), json!(true)),
            ]
        );
        // A call the harness gave no id has a null call_id.
        let bare = replay(
            Harness::Claude,
            &[(
                Stream::Stdout,
                r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{}}]}}"#,
            )],
        );
        assert_eq!(bare[0].1["call_id"], Value::Null);
    }

    #[test]
    fn each_harness_emits_only_what_its_capabilities_name() {
        for harness in Harness::ALL {
            let caps = capabilities(harness);
            assert!(caps.contains(&"output.text") && caps.contains(&"output.usage"));
            assert_eq!(
                caps.contains(&"output.tools"),
                harness != Harness::Codex,
                "{harness:?}"
            );
        }
    }

    /// A22 case 9: each harness's native output in the conformance case
    /// replays to its normalized events, type and data, and the case's
    /// capabilities are the harness's and cover every event it gives.
    #[test]
    fn conformance_09_native_output_replays_to_the_normalized_events() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../conformance/api-v1/cases/09-turn-output.json");
        let case: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let harnesses = case["harnesses"].as_object().unwrap();
        assert_eq!(harnesses.len(), Harness::ALL.len());
        for harness in Harness::ALL {
            let h = &harnesses[harness.as_str()];
            let caps: Vec<&str> = h["capabilities"]
                .as_array()
                .unwrap()
                .iter()
                .map(|c| c.as_str().unwrap())
                .collect();
            assert_eq!(caps, capabilities(harness), "{harness:?}");
            let lines: Vec<(Stream, &str)> = h["native"]
                .as_array()
                .unwrap()
                .iter()
                .map(|n| {
                    let stream = match n["stream"].as_str().unwrap() {
                        "stdout" => Stream::Stdout,
                        "stderr" => Stream::Stderr,
                        other => panic!("stream {other}"),
                    };
                    (stream, n["line"].as_str().unwrap())
                })
                .collect();
            let out = replay(harness, &lines);
            let expect = h["expect"].as_array().unwrap();
            assert_eq!(
                kinds(&out),
                expect
                    .iter()
                    .map(|e| e["type"].as_str().unwrap())
                    .collect::<Vec<_>>(),
                "{harness:?}"
            );
            for ((kind, data), e) in out.iter().zip(expect) {
                if let Some(want) = e.get("data") {
                    assert_eq!(data, want, "{harness:?} {kind}");
                }
                let needs = match *kind {
                    "text" => Some("output.text"),
                    k if k.starts_with("tool.") => Some("output.tools"),
                    "usage" => Some("output.usage"),
                    _ => None,
                };
                if let Some(cap) = needs {
                    assert!(caps.contains(&cap), "{harness:?} gave {kind}");
                }
            }
        }
    }

    #[test]
    fn the_tailer_journals_lines_as_the_files_grow_in_seq_order() {
        let scratch = crate::supervise::Scratch::new("turn-live");
        let run = scratch.run_dir();
        let id = uuid::Uuid::now_v7().to_string();
        let writer = journal::Writer::create(&run, "a", &id, 1).unwrap();
        let out = run.join("turn-1.stdout");
        let err = run.join("turn-1.stderr");
        std::fs::write(&out, "").unwrap();
        std::fs::write(&err, "").unwrap();
        let tailer = Tailer::start(
            &out,
            &err,
            Sink {
                writer,
                parser: Parser::new(Harness::Claude),
                failed: None,
            },
        );
        use std::io::Write as _;
        let mut o = std::fs::OpenOptions::new().append(true).open(&out).unwrap();
        writeln!(
            o,
            r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"one"}}]}}}}"#
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let mut e = std::fs::OpenOptions::new().append(true).open(&err).unwrap();
        writeln!(e, "careful").unwrap();
        std::thread::sleep(Duration::from_millis(200));
        // The last line, without its newline, is read at the end.
        write!(
            o,
            r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"two"}}]}}}}"#
        )
        .unwrap();
        let mut sink = tailer.finish().unwrap();
        assert!(sink.failed.is_none());
        sink.writer.ended("replied").unwrap();
        let events = journal::read(&run, "a", &id).unwrap().unwrap();
        let got: Vec<(u64, &str)> = events
            .iter()
            .map(|e| (e.seq.unwrap(), e.kind.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                (1, "turn.started"),
                (2, "text"),
                (3, "status"),
                (4, "text"),
                (5, "turn.ended")
            ]
        );
        assert_eq!(events[3].data["text"], "two");
    }
}
