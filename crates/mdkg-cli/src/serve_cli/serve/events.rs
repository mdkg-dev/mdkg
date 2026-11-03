//! `GET /v1/events` (API v1 A9, A10, A15): the event stream as SSE, each
//! `data:` line the line `mdkg msg watch --json` prints, because it is
//! that line: serve runs `msg watch` as a child and forwards its stdout.
//! So the two transports can't drift apart (dec-01a18dd9).
//!
//! - **The query** maps to the command's flags: `after` (a cursor, or
//!   `start`), `types` (comma-separated), `channel` (repeatable),
//!   `mailbox`. Anything else, or a repeated `after` or `mailbox`, is
//!   refused. `Last-Event-ID` is never read: a client resumes with `after`.
//! - **A refusal at the start** (a bad or reset cursor) is the envelope
//!   `msg watch` gives, with its exit code's status (A12).
//! - **The frames:** an event's SSE `id:` is its `cursor`, its `event:`
//!   its `type`; a heartbeat carries no `id:`.
//! - **The end:** when `serve` drains, each stream gets one last
//!   `mdkg.stream.error` frame with `api.draining`, then closes (docs/api.md:
//!   "a stream that has to stop sends a last `mdkg.stream.error`"; Omni's
//!   rule, orchestrator #11753). A client resumes from its last `id:`.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender};
use std::time::{Duration, Instant};

use mdkg_format::{Code, Diagnostic};
use mdkg_graph::ErrorKind;
use mdkg_serve::http::HeadExt;
use mdkg_wire::codec::{RequestHead, Response};
use serde_json::{Value, json};

use super::binary::Binary;
use super::local::refusal;

/// The route.
pub const PATH: &str = "/v1/events";

/// How long the start waits for `msg watch` to refuse before the stream
/// starts: a refusal comes at once, before any event.
const START_WAIT: Duration = Duration::from_millis(1500);

/// How long a child that printed a whole envelope by the start's
/// deadline has to exit, for that envelope to be the answer.
const EXIT_GRACE: Duration = Duration::from_millis(500);

/// The most a refusal at the start may print: more is no envelope.
const START_BYTES: usize = 64 * 1024;

/// Lines buffered between the child and the client: past this the child
/// blocks on its pipe, so a slow client slows the child, never serve's
/// memory.
const BUFFER: usize = 1024;

/// What runs `msg watch`.
#[derive(Debug, Clone)]
pub struct Watch {
    /// serve's own program, checked before each spawn.
    pub binary: Arc<Binary>,
    /// The graph's root.
    pub root: PathBuf,
}

/// An open stream: the child, and the lines it prints.
pub struct Stream {
    child: Child,
    lines: Receiver<String>,
    /// The first line, read during the start.
    pending: Option<String>,
    /// The last durable cursor sent, for the closing frame's `head`.
    last: Option<String>,
}

impl std::fmt::Debug for Stream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stream").finish_non_exhaustive()
    }
}

/// What a stream gives next.
pub enum Next {
    /// A line: its `id:` (a durable event's cursor), its `event:` (its
    /// type) and the line itself.
    Frame(Option<String>, String, String),
    /// Nothing yet.
    Idle,
    /// The child ended.
    Ended,
}

fn bad(reason: &str) -> Response {
    refusal(
        ErrorKind::Usage,
        Diagnostic::error(Code::UsageInvalidArguments, reason),
    )
}

/// `msg watch`'s flags for the request's query, each `--flag=value`, so
/// a value is data whatever it starts with.
pub fn flags(head: &RequestHead) -> Result<Vec<String>, Response> {
    let pairs = head
        .query_pairs()
        .map_err(|_| bad("the query isn't well formed"))?;
    let mut out = Vec::new();
    let (mut after, mut mailbox) = (false, false);
    for (name, value) in pairs {
        match name.as_str() {
            "after" if !after => {
                after = true;
                if value.is_empty() {
                    return Err(refusal(
                        ErrorKind::Usage,
                        Diagnostic::error(Code::ApiBadCursor, "after is empty"),
                    ));
                }
                out.push(format!("--after={value}"));
            }
            "mailbox" if !mailbox => {
                mailbox = true;
                out.push(format!("--mailbox={value}"));
            }
            "types" => out.push(format!("--types={value}")),
            "channel" => out.push(format!("--channel={value}")),
            "after" | "mailbox" => return Err(bad("a query parameter is repeated")),
            _ => {
                return Err(bad(
                    "the events stream takes after, types, channel and mailbox",
                ));
            }
        }
    }
    Ok(out)
}

impl Watch {
    /// Starts `msg watch` for the request: a stream, or the envelope it
    /// refused with.
    pub fn open(&self, head: &RequestHead, actor: Option<&str>) -> Result<Stream, Response> {
        let flags = flags(head)?;
        let mut cmd = Command::new(self.binary.program()?);
        cmd.arg("--root").arg(&self.root);
        if let Some(actor) = actor {
            cmd.args(["--as", actor]);
        }
        cmd.args(["msg", "watch", "--json"])
            .args(&flags)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // Its own process group, recorded, so the drain can kill it however
        // late this stream's thread is (orchestrator #15424). It names
        // itself as serve was invoked, not `exe` (Linux's /proc/self/exe) or
        // a link's target, as ops_exec's calls do (Terra #15805).
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
            cmd.arg0(mdkg_format::program::name());
        }
        let mut child = cmd.spawn().map_err(|_| {
            refusal(
                ErrorKind::Failure,
                Diagnostic::error(Code::IoReadFailed, "serve couldn't start the event stream"),
            )
        })?;
        crate::ops_exec::register_group(child.id());
        let stdout = child.stdout.take().expect("piped");
        let (tx, rx): (SyncSender<String>, Receiver<String>) =
            std::sync::mpsc::sync_channel(BUFFER);
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        // A refusal comes first and the child exits: its envelope, which
        // `msg watch` prints pretty, over several lines. The deadline
        // holds whatever arrives, and so does a bound on what's kept.
        let until = Instant::now() + START_WAIT;
        let mut first: Vec<String> = Vec::new();
        let mut bytes = 0;
        let unexpected = |mut child: Child| {
            crate::ops_exec::kill_group(&mut child);
            Err(refusal(
                ErrorKind::Failure,
                Diagnostic::error(
                    Code::IoReadFailed,
                    "the event stream printed something other than events",
                ),
            ))
        };
        loop {
            let wait = until.saturating_duration_since(Instant::now());
            match rx.recv_timeout(wait.min(Duration::from_millis(50))) {
                Ok(line) => {
                    if first.is_empty() && is_event(&line) {
                        return Ok(Stream {
                            child,
                            lines: rx,
                            pending: Some(line),
                            last: None,
                        });
                    }
                    bytes += line.len() + 1;
                    if bytes > START_BYTES {
                        return unexpected(child);
                    }
                    first.push(line);
                }
                Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) if Instant::now() < until => {}
                // Silence: a stream that has no event yet.
                Err(RecvTimeoutError::Timeout) if first.is_empty() => {
                    return Ok(Stream {
                        child,
                        lines: rx,
                        pending: None,
                        last: None,
                    });
                }
                // A whole envelope, its child not yet seen exiting (a slow
                // machine): a short grace to exit, then it's the answer
                // (#12292).
                Err(RecvTimeoutError::Timeout)
                    if serde_json::from_str::<Value>(&first.join("\n")).is_ok() =>
                {
                    let until = Instant::now() + EXIT_GRACE;
                    while matches!(crate::ops_exec::try_reap(&mut child), Ok(None))
                        && Instant::now() < until
                    {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    if matches!(crate::ops_exec::try_reap(&mut child), Ok(Some(_))) {
                        break;
                    }
                    return unexpected(child);
                }
                // Lines that aren't events, and the child still running.
                Err(RecvTimeoutError::Timeout) => return unexpected(child),
            }
        }
        let exit = crate::ops_exec::wait_reap(&mut child).and_then(|s| s.code());
        let text = first.join("\n");
        match serde_json::from_str::<Value>(&text) {
            Ok(v) => Err(Response {
                status: http_status(exit, &v),
                headers: vec![("Content-Type".into(), "application/json".into())],
                body: serde_json::to_vec(&v).unwrap_or_default(),
            }),
            Err(_) => Err(refusal(
                ErrorKind::Failure,
                Diagnostic::error(Code::IoReadFailed, "the event stream ended before it began"),
            )),
        }
    }
}

/// A compact event line, as `msg watch` prints each.
fn is_event(line: &str) -> bool {
    serde_json::from_str::<Value>(line)
        .is_ok_and(|v| v.get("schema").and_then(Value::as_str) == Some("mdkg.api.event.v1"))
}

/// A12's status for the envelope `msg watch` refused with and its exit
/// code: mdkg-ops' table, as every served operation's.
fn http_status(exit: Option<i32>, envelope: &Value) -> u16 {
    let failure = match exit {
        Some(0) => None,
        Some(n) => Some(
            ErrorKind::ALL
                .into_iter()
                .find(|k| i32::from(k.exit_code()) == n)
                .unwrap_or(ErrorKind::Failure),
        ),
        None => Some(ErrorKind::Failure),
    };
    let diagnostics = envelope["diagnostics"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|d| d["severity"] == "error")
        .filter_map(|d| {
            let code = Code::parse(d["code"].as_str()?)?;
            Some(Diagnostic::error(code, d["message"].as_str().unwrap_or("")))
        })
        .collect();
    let report: mdkg_ops::wire::Report = mdkg_ops::wire::Report {
        data: None,
        diagnostics,
        failure,
        busy: false,
    };
    mdkg_ops::codes::http_status(None, &report)
}

impl Stream {
    /// The next frame, waiting up to `wait`.
    pub fn next(&mut self, wait: Duration) -> Next {
        let got = match self.pending.take() {
            Some(line) => Ok(line),
            None => self.lines.recv_timeout(wait),
        };
        match got {
            Ok(line) => {
                let v: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
                let kind = v
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("message")
                    .to_owned();
                let id = (v.get("durable").and_then(Value::as_bool) == Some(true))
                    .then(|| v.get("cursor").and_then(Value::as_str).map(str::to_owned))
                    .flatten();
                if id.is_some() {
                    self.last = id.clone();
                }
                Next::Frame(id, kind, line)
            }
            Err(RecvTimeoutError::Timeout) => Next::Idle,
            Err(RecvTimeoutError::Disconnected) => Next::Ended,
        }
    }

    /// The last frame of a drained stream: `mdkg.stream.error` with
    /// `api.draining`, its `head` the last cursor sent.
    pub fn draining_frame(&self) -> String {
        let d = Diagnostic::error(
            Code::ApiDraining,
            "serve is stopping; resume from your last id",
        )
        .with_hint("reconnect with ?after=<the last id you saw> once serve is back");
        json!({
            "schema": "mdkg.api.event.v1",
            "id": uuid::Uuid::now_v7().to_string(),
            "cursor": self.last.clone().unwrap_or_default(),
            "type": "mdkg.stream.error",
            "time": super::rfc3339_ms(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0),
            ),
            "actor": "mdkg",
            "subjects": [],
            "durable": false,
            "data": {
                "diagnostics": [crate::output::DiagnosticOut::from(&d)],
                "head": self.last,
            },
        })
        .to_string()
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        crate::ops_exec::kill_group(&mut self.child);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn head(query: &str) -> RequestHead {
        RequestHead {
            method: "GET".into(),
            path: PATH.into(),
            query: Some(query.into()),
            headers: vec![],
        }
    }

    #[test]
    fn each_value_is_passed_as_data_whatever_it_starts_with() {
        let flags = flags(&head(
            "after=-h&types=a,b&channel=--help&channel=c&mailbox=m",
        ))
        .expect("flags");
        assert_eq!(
            flags,
            [
                "--after=-h",
                "--types=a,b",
                "--channel=--help",
                "--channel=c",
                "--mailbox=m"
            ]
        );
        for bad in ["after=a&after=b", "mailbox=a&mailbox=b", "since=1"] {
            assert!(flags_of(bad).is_err(), "{bad}");
        }
    }

    fn flags_of(q: &str) -> Result<Vec<String>, Response> {
        flags(&head(q))
    }

    /// A `Watch` running `script` as its program.
    fn watch(dir: &std::path::Path, script: &str) -> Watch {
        let exe = dir.join("fake-mdkg");
        std::fs::write(&exe, format!("#!/bin/sh\n{script}\n")).expect("script");
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o700)).expect("mode");
        Watch {
            binary: Arc::new(Binary::fixed(&exe)),
            root: dir.to_path_buf(),
        }
    }

    #[test]
    fn a_start_that_prints_no_events_ends_at_the_deadline_with_a_bounded_buffer() {
        let dir = std::env::temp_dir().join(format!("mdkg-events-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).expect("dir");
        // A banner, then silence: refused at the deadline, not waited on.
        let started = Instant::now();
        let w = watch(&dir, "echo banner; exec sleep 30");
        let e = w.open(&head(""), None).expect_err("refused");
        assert!(started.elapsed() < START_WAIT + Duration::from_secs(5));
        assert_eq!(e.status, 500);
        assert!(String::from_utf8_lossy(&e.body).contains("other than events"));
        // Endless lines that aren't events: refused once over the bound.
        let w = watch(&dir, "exec yes not-an-event");
        let e = w.open(&head(""), None).expect_err("refused");
        assert_eq!(e.status, 500);
        // A refusal envelope over several lines, then exit 1: its own
        // status from A12's table.
        let w = watch(
            &dir,
            r#"printf '{\n "schema": "mdkg.msg.watch.v1",\n "ok": false,\n "diagnostics": [{"code": "api.bad_cursor", "severity": "error", "message": "x"}]\n}\n'; exit 1"#,
        );
        let e = w.open(&head(""), None).expect_err("refused");
        assert_eq!(e.status, 400);
        // The same envelope, its child slow to exit past the deadline (a
        // loaded machine): still the answer, not a 500 (#12292).
        let w = watch(
            &dir,
            r#"printf '{"schema": "mdkg.msg.watch.v1", "ok": false, "diagnostics": [{"code": "api.bad_cursor", "severity": "error", "message": "x"}]}\n'; sleep 1.52; exit 1"#,
        );
        let e = w.open(&head(""), None).expect_err("refused");
        assert_eq!(e.status, 400, "{}", String::from_utf8_lossy(&e.body));
        let v: Value = serde_json::from_slice(&e.body).expect("json");
        assert_eq!(v["diagnostics"][0]["code"], "api.bad_cursor");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
