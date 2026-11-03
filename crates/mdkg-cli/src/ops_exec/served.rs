//! The HTTP side of the executor: [`SubprocessBackend`] runs each served
//! operation as one CLI subprocess (dec-01a18dd9), with its arguments
//! derived from the command's clap definition.
//!
//! serve-run's listener builds it; until that lands only the tests do, so
//! nothing here is dead code in a build that serves.
#![cfg_attr(not(test), allow(dead_code))]

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use mdkg_format::{Code, Diagnostic};
use mdkg_graph::ErrorKind;
use mdkg_ops::registry::{Class, Kind, OpSpec};
use mdkg_ops::wire::{Backend, BackendKind, Event, EventStream, Report, Request};
use mdkg_ops::{Clock, Ctx, Parts, Transport};
use serde_json::{Map, Value, json};

use super::{
    Arg, Call, Limits, Prepared, assemble, check_keys, kill_group, map_args, run, self_exe,
};

/// How long a served call may run: as long as an MCP call, and for a long
/// poll its own wait and [`LONG_POLL_GRACE`] more.
const SERVED_TIMEOUT: Duration = Duration::from_secs(120);
/// What a long poll's command may take beyond its wait, as MCP allows.
pub(crate) const LONG_POLL_GRACE: Duration = Duration::from_secs(30);

/// Variables a served call never inherits from the server: who acts, and
/// in which turn, come from the request's context alone (dec-01a18dd9).
const SERVED_ENV_REMOVE: &[&str] = &[
    "MDKG_ACTOR",
    "MDKG_RUN_TURN",
    "MDKG_TRUSTED_WORKTREE",
    "MDKG_TRUSTED_COMMON_DIR",
];

/// The clap tree, built once: the derived argument tables borrow from it.
fn cli() -> &'static clap::Command {
    use clap::CommandFactory;
    static CLI: OnceLock<clap::Command> = OnceLock::new();
    CLI.get_or_init(crate::Cli::command)
}

fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

/// The arguments the CLI command `words` takes over HTTP, derived from its
/// clap definition so the two can't drift: each positional by its name,
/// each documented flag by its long name with `_` for `-`. Left out:
/// `--json` (always given), the global options (the server sets them),
/// hidden flags (internal to mdkg), and flags that read a file by path,
/// except a data file, which takes the JSON value itself on stdin as MCP's
/// `data` does.
fn derive_args(words: &[&str]) -> Option<Vec<Arg>> {
    let mut cmd = cli();
    for word in words {
        cmd = cmd.find_subcommand(word)?;
    }
    let mut positionals: Vec<&'static clap::Arg> = cmd.get_positionals().collect();
    positionals.sort_by_key(|a| a.get_index());
    let mut args: Vec<Arg> = positionals
        .into_iter()
        .filter(|a| !a.is_hide_set())
        .map(|a| {
            let id = a.get_id().as_str();
            if matches!(a.get_action(), clap::ArgAction::Append) {
                Arg::Positionals(id)
            } else {
                Arg::Positional(id)
            }
        })
        .collect();
    for a in cmd.get_arguments() {
        let Some(long) = a.get_long() else { continue };
        if a.is_positional() || a.is_global_set() || a.is_hide_set() || long == "json" {
            continue;
        }
        let flag = leak(format!("--{long}"));
        if let Some(stem) = long.strip_suffix("-file") {
            if matches!(stem, "data" | "op") {
                args.push(Arg::Data(leak(stem.to_string()), flag));
            }
            continue;
        }
        let key = leak(long.replace('-', "_"));
        args.push(match a.get_action() {
            clap::ArgAction::SetTrue => Arg::Flag(key, flag),
            clap::ArgAction::Append if a.get_value_delimiter().is_some() => Arg::List(key, flag),
            clap::ArgAction::Append => Arg::Repeated(key, flag),
            clap::ArgAction::Set => Arg::Text(key, flag),
            _ => continue,
        });
    }
    Some(args)
}

/// Each served operation's arguments, derived once.
fn served_args(op: &OpSpec) -> Option<&'static [Arg]> {
    static TABLE: OnceLock<HashMap<&'static str, Vec<Arg>>> = OnceLock::new();
    TABLE
        .get_or_init(|| {
            mdkg_ops::OPS
                .iter()
                .filter(|op| op.http.is_some())
                .filter_map(|op| Some((op.name, derive_args(op.cli)?)))
                .collect()
        })
        .get(op.name)
        .map(Vec::as_slice)
}

/// A request's values as `map_args` takes them: a query gives every value
/// as a string, and a JSON body may give a number or a boolean for a text
/// flag, so text takes either and a flag takes `"true"` or `"false"`.
fn normalise(args: &[Arg], given: &mut Map<String, Value>) {
    for arg in args {
        let Some(value) = given.get_mut(arg.key()) else {
            continue;
        };
        match (arg, &*value) {
            (Arg::Text(..), Value::Number(n)) => *value = Value::String(n.to_string()),
            (Arg::Text(..), Value::Bool(b)) => *value = Value::String(b.to_string()),
            (Arg::Flag(..), Value::String(s)) if s == "true" || s == "false" => {
                *value = Value::Bool(s == "true");
            }
            _ => {}
        }
    }
}

/// What the HTTP surface refuses that the CLI takes (dec-01a18dd9): a
/// decision, from a post or as a ledger op; a turn's or a role's lease,
/// which belong to the launcher and interactive sessions; and a lease id
/// in place of the scope a release names, since it could name either.
fn refused(op: &OpSpec, given: &Map<String, Value>) -> Option<String> {
    let text = |key: &str| given.get(key).and_then(Value::as_str).map(str::trim);
    match op.name {
        "msg.send" if text("kind").is_some_and(|k| k.eq_ignore_ascii_case("decision")) => Some(
            "a decision isn't sent over HTTP: decisions come only from your human's channel".into(),
        ),
        "entry.update" => {
            let ops = match given.get("op") {
                Some(Value::Array(ops)) => ops.iter().collect(),
                Some(op) => vec![op],
                None => Vec::new(),
            };
            ops.iter()
                .any(|o| {
                    o.get("op")
                        .and_then(Value::as_str)
                        .is_some_and(|o| matches!(o.trim(), "decide" | "residual"))
                })
                .then(|| "a ledger decision or residual isn't made over HTTP".into())
        }
        "lease.claim" | "lease.release" => {
            let scope = |key| {
                text(key).is_some_and(|t| {
                    let t = t.to_ascii_lowercase();
                    t.starts_with("run:") || t.starts_with("role:")
                })
            };
            let lease_id = text("target").is_some_and(|t| {
                t.to_ascii_lowercase()
                    .strip_prefix("lease-")
                    .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            });
            if scope("target") || scope("scope") {
                Some("a turn's or a role's lease isn't taken or given up over HTTP".into())
            } else if op.name == "lease.release" && lease_id {
                Some("release takes the node id or path you claimed, not a lease id".into())
            } else {
                None
            }
        }
        _ => None,
    }
}

/// The CLI envelope for a call refused before, or without, the CLI's own:
/// the command's schema, as MCP's are.
fn refusal(op: &OpSpec, code: Code, kind: ErrorKind, message: &str) -> Report {
    refusal_with(op, Diagnostic::error(code, message), kind)
}

/// [`refusal`] for a diagnostic of its own, a hint and all.
fn refusal_with(op: &OpSpec, diagnostic: Diagnostic, kind: ErrorKind) -> Report {
    let envelope = json!({
        "schema": crate::output::schema_id(&op.cli.join(".")),
        "ok": false,
        "diagnostics": [crate::output::DiagnosticOut::from(&diagnostic)],
    });
    Report {
        data: Some(envelope),
        diagnostics: vec![diagnostic],
        failure: Some(kind),
        busy: false,
    }
}

/// The exit code's kind: none for 0, and a failure for one the contract
/// doesn't name (a crash, a signal).
fn exit_kind(code: Option<i32>) -> Option<ErrorKind> {
    match code {
        Some(0) => None,
        Some(n) => Some(
            ErrorKind::ALL
                .into_iter()
                .find(|k| i32::from(k.exit_code()) == n)
                .unwrap_or(ErrorKind::Failure),
        ),
        None => Some(ErrorKind::Failure),
    }
}

/// The machine a local-socket request runs as: serve-local enrols no
/// machines, and the CLI subprocess never reads it.
const LOCAL_MACHINE: &str = "local";

/// The context of one request on the local socket, the only way serve's
/// code gets one (`local_is_never_built_off_the_host`). A local-socket
/// request is the CLI's own operation, with `Mdkg-Actor` trusted as `--as`
/// (dec-01a18dd9), so it is built as the host's own: `Origin::Local`, over
/// `Transport::Http`, with no person, agent or turn. The clock's lease TTL
/// is 0: the CLI subprocess applies the store's own.
pub(crate) fn served_ctx(
    backend: Arc<dyn Backend>,
    actor: String,
    idempotency_key: Option<String>,
) -> Result<Ctx, Box<Diagnostic>> {
    Ctx::local(
        LOCAL_MACHINE.to_string(),
        None,
        Parts {
            actor,
            agent: None,
            turn: None,
            transport: Transport::Http,
            idempotency_key,
            clock: Clock {
                now_ms: crate::coord::now_ms(),
                lease_ttl_ms: 0,
            },
            trust: Default::default(),
            backend,
        },
    )
}

/// Runs each served operation as one CLI subprocess, as MCP runs its tools
/// (dec-01a18dd9): the request's arguments become the command's argv, the
/// context's actor `--as`, and the result is the envelope the CLI printed,
/// unchanged, with its exit code as the report's failure.
///
/// Every request it runs comes from a caller outside any agent turn: the
/// listener refuses one inside a turn by its peer pid before routing
/// (dec-01a18dd9's turn safety; serve-run's 25c9c6ce). So a call runs with
/// no turn mark, and never the server's own (`SERVED_ENV_REMOVE`).
pub(crate) struct SubprocessBackend {
    exe: PathBuf,
    root: PathBuf,
    /// The served graph's id; `{graph_id}` must name it.
    graph_id: Option<String>,
    /// Asked before every spawn ([`SubprocessBackend::with_check`]).
    check: Option<Box<PreSpawn>>,
    /// How long opening a stream waits for a refusal ([`OPEN_GRACE`]).
    open_grace: Duration,
}

/// A check the server runs before each call's subprocess starts: `Ok` to
/// run it, or the refusal the request gets instead. serve-run's checks
/// that its binary is the one it started (`builds::BinaryWatch`), so a
/// reinstalled build drains the server rather than serving requests
/// (orchestrator #12137).
pub(crate) type PreSpawn = dyn Fn() -> Result<(), Refused> + Send + Sync;

/// Why a pre-spawn check refused: the envelope's code and message, and the
/// report's kind.
#[derive(Debug, Clone)]
pub(crate) struct Refused {
    pub(crate) code: Code,
    pub(crate) kind: ErrorKind,
    pub(crate) message: String,
}

impl SubprocessBackend {
    /// For the graph at `root`, running this process's own binary.
    pub(crate) fn new(root: &Path) -> std::io::Result<Self> {
        let graph_id = mdkg_graph::config::load(root)
            .ok()
            .and_then(|c| c.graph_id)
            .map(|id| id.to_string());
        Ok(Self::with_exe(self_exe()?, root, graph_id))
    }

    /// For the graph `graph_id`, served from the checkout at `root`: in ref
    /// mode the checkout holds no graph config to read the id from.
    pub(crate) fn for_graph(root: &Path, graph_id: &str) -> std::io::Result<Self> {
        Ok(Self::with_exe(self_exe()?, root, Some(graph_id.to_owned())))
    }

    fn with_exe(exe: PathBuf, root: &Path, graph_id: Option<String>) -> Self {
        Self {
            exe,
            root: root.to_path_buf(),
            graph_id,
            check: None,
            open_grace: OPEN_GRACE,
        }
    }

    /// Runs `check` before every call's subprocess, after the request is
    /// otherwise accepted; a refusal is the request's answer, and nothing
    /// is spawned.
    pub(crate) fn with_check(
        mut self,
        check: impl Fn() -> Result<(), Refused> + Send + Sync + 'static,
    ) -> Self {
        self.check = Some(Box::new(check));
        self
    }

    /// The pre-spawn check's refusal, when it gives one.
    fn checked(&self, op: &OpSpec) -> Result<(), Report> {
        match self.check.as_ref().map(|check| check()) {
            Some(Err(r)) => Err(refusal(op, r.code, r.kind, &r.message)),
            _ => Ok(()),
        }
    }

    /// The call `request` asks for, with the server's global options, or
    /// why it's refused.
    fn prepare(
        &self,
        ctx: &Ctx,
        op: &OpSpec,
        request: &Request,
    ) -> Result<(Prepared, Vec<OsString>), Report> {
        let usage = |m: &str| refusal(op, Code::UsageInvalidArguments, ErrorKind::Usage, m);
        let unsupported = || {
            refusal(
                op,
                Code::ApiUnsupported,
                ErrorKind::Usage,
                "this operation isn't served over HTTP",
            )
        };
        let Some(route) = op.http else {
            return Err(unsupported());
        };
        if op.class != Class::Any {
            return Err(unsupported());
        }
        let args = served_args(op).ok_or_else(unsupported)?;
        let mut given = match &request.body {
            Value::Null => Map::new(),
            Value::Object(m) => m.clone(),
            _ => return Err(usage("the request body isn't a JSON object")),
        };
        if route.names().any(|n| n == "graph_id") {
            let named = given.remove("graph_id");
            if named.as_ref().and_then(Value::as_str) != self.graph_id.as_deref() {
                return Err(refusal(
                    op,
                    Code::ApiGraphUnknown,
                    ErrorKind::NotFound,
                    "this server doesn't serve that graph",
                ));
            }
        }
        // The CLI already takes a key where it has one: a message's id.
        if let (Some(key), "msg.send") = (&ctx.idempotency_key, op.name) {
            match given.get("id") {
                None => {
                    given.insert("id".into(), Value::String(key.clone()));
                }
                Some(id) if id.as_str() == Some(key) => {}
                Some(_) => return Err(usage("the idempotency key and `id` differ")),
            }
        }
        normalise(args, &mut given);
        // A long poll's command waits exactly what the request asked for,
        // or the default when it named none. Over the longest is refused,
        // never quietly cut short (MCP's `inbox_wait` caps it instead). A
        // value that isn't a number goes as given, for the CLI to refuse.
        if let Some(poll) = op.long_poll() {
            let asked = given.get(poll.arg).and_then(|v| {
                v.as_u64()
                    .or_else(|| v.as_str().and_then(|s| s.trim().parse::<u64>().ok()))
            });
            if asked.is_some_and(|n| n > poll.max_secs) {
                return Err(usage(&format!(
                    "`{}` is at most {} seconds",
                    poll.arg, poll.max_secs
                )));
            }
            if asked.is_some() || given.get(poll.arg).is_none() {
                let wait = asked.unwrap_or(poll.default_secs);
                given.insert(poll.arg.into(), Value::String(wait.to_string()));
            }
        }
        check_keys(op.name, args, &[], &given).map_err(|m| usage(&m))?;
        if let Some(why) = refused(op, &given) {
            return Err(usage(&why));
        }
        let mapped = map_args(args, &given).map_err(|m| usage(&m))?;
        // Who acts comes from the request alone, never the server's own
        // actor (dec-01a18dd9: `Mdkg-Actor` is trusted exactly as `--as`).
        if ctx.actor.trim().is_empty() {
            return Err(refusal_with(
                op,
                Diagnostic::error(
                    Code::UsageInvalidArguments,
                    "the request names no actor: send the `Mdkg-Actor` header",
                )
                .with_hint("`Mdkg-Actor: <actor>` names who acts, as `--as` does for the CLI"),
                ErrorKind::Usage,
            ));
        }
        let global = vec![
            OsString::from("--root"),
            self.root.clone().into(),
            format!("--as={}", ctx.actor).into(),
        ];
        let flags = mapped.flags.into_iter().map(|(_, flag)| flag).collect();
        let prepared = Prepared {
            args: assemble(&op.cli.join(" "), flags, mapped.positionals),
            stdin: mapped.stdin,
        };
        Ok((prepared, global))
    }

    /// How long `op` may run: a long poll's own wait and a little more.
    fn timeout(op: &OpSpec, request: &Request) -> Duration {
        op.wait(&request.body).map_or(SERVED_TIMEOUT, |wait| {
            SERVED_TIMEOUT.max(wait.saturating_add(LONG_POLL_GRACE))
        })
    }
}

/// The most one event line may hold: a stream's whole budget (S4).
const MAX_EVENT_LINE: u64 = 1 << 20;
/// How long opening a stream waits for a refusal before it starts: the
/// CLI checks its cursor and arguments first, and prints an envelope.
const OPEN_GRACE: Duration = Duration::from_secs(1);

/// A stream command's output, one event per line: SSE's `id:` is the
/// line's `cursor` and its `data:` the line as the CLI printed it (A15).
/// Dropping it stops the command and everything it started.
struct Lines {
    child: std::process::Child,
    lines: std::sync::mpsc::Receiver<Vec<u8>>,
    first: Option<Vec<u8>>,
}

impl Lines {
    fn open(call: &Call<'_>, op: &OpSpec, grace: Duration) -> Result<Self, Report> {
        let failed = |m: String| refusal(op, Code::McpCliFailed, ErrorKind::Failure, &m);
        let mut command = Command::new(call.exe);
        command
            .args(call.global)
            .args(call.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for name in call.env_remove {
            command.env_remove(name);
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
            // The child names itself as this server was invoked, not as
            // `exe` (Linux's /proc/self/exe) or a link's target, so its
            // hints name a command that runs (Terra #15584).
            command.arg0(mdkg_format::program::name());
        }
        let mut child = command
            .spawn()
            .map_err(|e| failed(format!("couldn't start mdkg: {e}")))?;
        #[cfg(unix)]
        crate::ops_exec::register_group(child.id());
        let Some(stdout) = child.stdout.take() else {
            kill_group(&mut child);
            return Err(failed("no output pipe".into()));
        };
        let (send, lines) = std::sync::mpsc::channel();
        let reader = std::thread::Builder::new().spawn(move || {
            use std::io::BufRead;
            let mut stdout = std::io::BufReader::new(stdout);
            loop {
                let mut line = Vec::new();
                match (&mut stdout)
                    .take(MAX_EVENT_LINE + 1)
                    .read_until(b'\n', &mut line)
                {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        // A line over the budget ends the stream: the next
                        // one would start inside it.
                        let whole = line.ends_with(b"\n");
                        if send.send(line).is_err() || !whole {
                            break;
                        }
                    }
                }
            }
        });
        if reader.is_err() {
            kill_group(&mut child);
            return Err(failed(
                "the server couldn't start a thread for the stream".into(),
            ));
        }
        let mut stream = Self {
            child,
            lines,
            first: None,
        };
        // A refusal is the CLI's envelope, printed before any event; a
        // stream that has said nothing yet has started.
        match stream.lines.recv_timeout(grace) {
            Ok(line) => match envelope(&line) {
                Some(refused) => {
                    let code = crate::ops_exec::wait_reap(&mut stream.child).and_then(|s| s.code());
                    let diagnostics = errors(&refused);
                    return Err(Report {
                        data: Some(refused),
                        diagnostics,
                        failure: exit_kind(code).or(Some(ErrorKind::Failure)),
                        busy: false,
                    });
                }
                None => stream.first = Some(line),
            },
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err(failed("mdkg ended the stream before it started".into()));
            }
        }
        Ok(stream)
    }
}

/// The line as an envelope, when it is one: an object with `ok`.
fn envelope(line: &[u8]) -> Option<Value> {
    serde_json::from_slice::<Value>(line)
        .ok()
        .filter(|v| v.get("ok").is_some_and(Value::is_boolean))
}

/// An envelope's error diagnostics, for the status's per-code exceptions
/// (A12); the response itself is the envelope.
fn errors(envelope: &Value) -> Vec<Diagnostic> {
    envelope["diagnostics"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|d| d["severity"] == "error")
        .filter_map(|d| {
            let code = Code::parse(d["code"].as_str()?)?;
            Some(Diagnostic::error(code, d["message"].as_str().unwrap_or("")))
        })
        .collect()
}

impl EventStream for Lines {
    fn next_event(&mut self) -> Result<Option<Event>, Vec<Diagnostic>> {
        let line = match self.first.take() {
            Some(line) => line,
            None => match self.lines.recv() {
                Ok(line) => line,
                Err(_) => return Ok(None),
            },
        };
        let broken = |m: &str| vec![Diagnostic::error(Code::McpCliFailed, m)];
        let Some(text) = std::str::from_utf8(&line)
            .ok()
            .and_then(|t| t.strip_suffix('\n'))
        else {
            return Err(broken("an event line was cut short or isn't UTF-8"));
        };
        let event: Value =
            serde_json::from_str(text).map_err(|_| broken("an event line isn't JSON"))?;
        // A stream error ends the stream with its diagnostics.
        if event.get("ok").is_some() {
            let d = errors(&event);
            return Err(if d.is_empty() {
                broken("the stream ended")
            } else {
                d
            });
        }
        let Some(cursor) = event["cursor"].as_str() else {
            return Err(broken("an event has no cursor"));
        };
        Ok(Some(Event {
            id: cursor.to_string(),
            data: text.to_string(),
        }))
    }
}

impl Drop for Lines {
    fn drop(&mut self) {
        kill_group(&mut self.child);
    }
}

impl Backend for SubprocessBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Local
    }

    fn apply(&self, ctx: &Ctx, op: &'static OpSpec, request: &Request) -> Report {
        self.apply_until(ctx, op, request, &AtomicBool::new(false))
    }

    fn stream(
        &self,
        ctx: &Ctx,
        op: &'static OpSpec,
        request: &Request,
    ) -> Result<Box<dyn EventStream>, Report> {
        if op.kind != Kind::Stream {
            return Err(refusal(
                op,
                Code::ApiUnsupported,
                ErrorKind::Usage,
                "only a stream is opened",
            ));
        }
        let (prepared, global) = self.prepare(ctx, op, request)?;
        self.checked(op)?;
        let call = Call {
            exe: &self.exe,
            global: &global,
            args: &prepared.args,
            input: None,
            env_remove: SERVED_ENV_REMOVE,
        };
        Lines::open(&call, op, self.open_grace).map(|s| Box::new(s) as Box<dyn EventStream>)
    }
}

impl SubprocessBackend {
    /// [`Backend::apply`], stopped once `cancelled` is set: its child and
    /// everything it started are killed and reaped, and the answer is
    /// `api.draining` (503), for a client to send again.
    pub(crate) fn apply_until(
        &self,
        ctx: &Ctx,
        op: &'static OpSpec,
        request: &Request,
        cancelled: &AtomicBool,
    ) -> Report {
        if op.kind == Kind::Stream {
            return refusal(
                op,
                Code::ApiUnsupported,
                ErrorKind::Usage,
                "a stream is opened, not applied",
            );
        }
        let (prepared, global) = match self.prepare(ctx, op, request) {
            Ok(p) => p,
            Err(refused) => return refused,
        };
        if let Err(refused) = self.checked(op) {
            return refused;
        }
        let call = Call {
            exe: &self.exe,
            global: &global,
            args: &prepared.args,
            input: prepared.stdin.as_deref(),
            env_remove: SERVED_ENV_REMOVE,
        };
        let limits = Limits {
            timeout: Self::timeout(op, request),
            progress_every: Duration::MAX,
        };
        match run(&call, limits, cancelled, &|_| {}) {
            Ok(ran) => {
                let diagnostics = errors(&ran.envelope);
                Report {
                    data: Some(ran.envelope),
                    diagnostics,
                    failure: exit_kind(ran.code),
                    busy: false,
                }
            }
            Err(_) if cancelled.load(std::sync::atomic::Ordering::SeqCst) => refusal_with(
                op,
                Diagnostic::error(
                    Code::ApiDraining,
                    "the request was stopped: serve is stopping, or its client went away",
                )
                .with_hint("send it again once serve is back"),
                ErrorKind::Conflict,
            ),
            Err(stopped) => {
                let kind = if stopped.code == Code::McpCallTimeout {
                    ErrorKind::Timeout
                } else {
                    ErrorKind::Failure
                };
                refusal(op, stopped.code, kind, &stopped.message)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use mdkg_ops::Origin;
    use mdkg_ops::registry::{OPS, find};
    use serde_json::json;

    use super::*;

    #[test]
    fn every_served_operation_takes_its_path_values() {
        for op in OPS {
            let Some(route) = op.http else { continue };
            let args = served_args(op).unwrap_or_else(|| panic!("{} has no arguments", op.name));
            let keys: Vec<&str> = args.iter().map(|a| a.key()).collect();
            let mut unique = keys.clone();
            unique.sort_unstable();
            unique.dedup();
            assert_eq!(unique.len(), keys.len(), "{}: {keys:?}", op.name);
            for name in route.names().filter(|n| *n != "graph_id") {
                assert!(keys.contains(&name), "{}: `{name}` in {keys:?}", op.name);
            }
            // The server sets these; a request never does.
            for global in ["root", "as", "allow_store_upgrade", "json"] {
                assert!(!keys.contains(&global), "{}: {global}", op.name);
            }
        }
    }

    const GRAPH: &str = "01a18dfe-04fb-71e2-9052-a7d796b38783";

    fn backend(exe: &str) -> Arc<SubprocessBackend> {
        Arc::new(SubprocessBackend::with_exe(
            PathBuf::from(exe),
            Path::new("/repo"),
            Some(GRAPH.to_string()),
        ))
    }

    fn ctx(backend: Arc<SubprocessBackend>, key: Option<&str>) -> Ctx {
        served_ctx(backend, "alice".into(), key.map(String::from)).expect("ctx")
    }

    #[test]
    fn a_served_request_is_the_host_s_own_over_http() {
        let ctx = ctx(backend("/nonexistent"), Some("k-1"));
        assert_eq!(ctx.origin(), &Origin::Local);
        assert_eq!(ctx.transport, Transport::Http);
        assert_eq!(ctx.actor, "alice");
        assert_eq!(ctx.idempotency_key.as_deref(), Some("k-1"));
        assert_eq!(
            (ctx.person(), ctx.agent.as_deref(), ctx.turn.as_deref()),
            (None, None, None)
        );
    }

    fn prepare(name: &str, body: Value, key: Option<&str>) -> Result<Prepared, Report> {
        let b = backend("/nonexistent");
        let op = find(name).expect("op");
        b.prepare(&ctx(b.clone(), key), op, &Request { op: op.name, body })
            .map(|(prepared, _)| prepared)
    }

    fn code(report: &Report) -> &str {
        report.data.as_ref().expect("envelope")["diagnostics"][0]["code"]
            .as_str()
            .expect("code")
    }

    #[test]
    fn a_long_poll_waits_what_it_asked_for_up_to_its_limit() {
        let wait = |body: Value| {
            prepare("msg.wait", body, None)
                .expect("wait")
                .args
                .into_iter()
                .find(|a| a.starts_with("--timeout"))
        };
        let timeout = |secs: &str| Some(format!("--timeout={secs}"));
        assert_eq!(wait(json!({"mailbox": "alice"})), timeout("600"));
        assert_eq!(
            wait(json!({"mailbox": "alice", "timeout": 30})),
            timeout("30")
        );
        assert_eq!(
            wait(json!({"mailbox": "alice", "timeout": "45"})),
            timeout("45")
        );
        assert_eq!(
            wait(json!({"mailbox": "alice", "timeout": 600})),
            timeout("600")
        );
        // Over the longest: refused, never quietly cut short (S3).
        for over in [json!(601), json!("601"), json!(86_400)] {
            let refused = prepare(
                "msg.wait",
                json!({"mailbox": "alice", "timeout": over}),
                None,
            )
            .expect_err("over 600");
            assert_eq!(code(&refused), "usage.invalid_arguments");
            assert!(
                refused.diagnostics[0].message.contains("at most 600"),
                "{:?}",
                refused.diagnostics
            );
        }
        // Not a number: the CLI refuses it, and fast.
        assert_eq!(
            wait(json!({"mailbox": "alice", "timeout": "soon"})),
            timeout("soon")
        );
        let op = find("msg.wait").expect("msg.wait");
        let limit = |body| SubprocessBackend::timeout(op, &Request { op: op.name, body });
        assert_eq!(limit(json!({"timeout": 30})), SERVED_TIMEOUT);
        assert_eq!(limit(json!({})), Duration::from_secs(630));
        assert_eq!(
            SubprocessBackend::timeout(
                find("msg.inbox").expect("inbox"),
                &Request {
                    op: "msg.inbox",
                    body: json!({"timeout": 900})
                }
            ),
            SERVED_TIMEOUT
        );
    }

    #[test]
    fn a_request_becomes_the_command_s_argv() {
        let show = prepare(
            "node.show",
            json!({"graph_id": GRAPH, "id": "task-1", "meta": "true", "fields": "id,title"}),
            None,
        )
        .expect("show");
        assert_eq!(
            show.args,
            [
                "show",
                "--json",
                "--meta",
                "--fields=id,title",
                "--",
                "task-1"
            ]
        );
        let search = prepare(
            "node.search",
            json!({"graph_id": GRAPH, "query": ["a", "b"], "limit": 5, "type": ["task", "bug"]}),
            None,
        )
        .expect("search");
        assert_eq!(
            search.args,
            [
                "search",
                "--json",
                "--type=task,bug",
                "--limit=5",
                "--",
                "a",
                "b"
            ]
        );
        let inbox = prepare("msg.inbox", json!({"mailbox": "alice"}), None).expect("inbox");
        assert_eq!(inbox.args, ["msg", "inbox", "--json", "--mailbox=alice"]);
        // A data file is the value itself, on stdin.
        let update = prepare(
            "entry.update",
            json!({"graph_id": GRAPH, "node": "task-1", "op": {"op": "add", "kind": "finding"}}),
            None,
        )
        .expect("entry update");
        assert_eq!(
            update.args,
            ["entry", "update", "--json", "--op-file=-", "--", "task-1"]
        );
        assert_eq!(
            update.stdin.as_deref(),
            Some(br#"{"op":"add","kind":"finding"}"#.as_slice())
        );
        // The idempotency key is a message's id, where the CLI has one.
        let send = prepare(
            "msg.send",
            json!({"to": "bob", "kind": "note", "text": "hi"}),
            Some("m-1"),
        )
        .expect("send");
        assert_eq!(
            send.args,
            [
                "msg",
                "send",
                "--json",
                "--to=bob",
                "--kind=note",
                "--id=m-1",
                "--text=hi"
            ]
        );
    }

    #[test]
    fn what_http_refuses_is_a_cli_envelope() {
        let cases = [
            (
                "node.show",
                json!({"graph_id": "other", "id": "t"}),
                "api.graph_unknown",
            ),
            ("node.show", json!({"id": "t"}), "api.graph_unknown"),
            (
                "node.show",
                json!({"graph_id": GRAPH, "root": "/"}),
                "usage.invalid_arguments",
            ),
            (
                "lease.renew",
                json!({"local_turn": "x"}),
                "usage.invalid_arguments",
            ),
            (
                "msg.send",
                json!({"kind": " Decision ", "to": "bob"}),
                "usage.invalid_arguments",
            ),
            (
                "entry.update",
                json!({"graph_id": GRAPH, "node": "t", "op": {"op": "decide"}}),
                "usage.invalid_arguments",
            ),
            (
                "lease.claim",
                json!({"target": "run:alice"}),
                "usage.invalid_arguments",
            ),
            (
                "lease.release",
                json!({"target": "ROLE:rev"}),
                "usage.invalid_arguments",
            ),
            (
                "lease.release",
                json!({"target": "lease-7"}),
                "usage.invalid_arguments",
            ),
            ("msg.send", json!([1]), "usage.invalid_arguments"),
            ("goal.activate", json!({"goal": "g"}), "api.unsupported"),
            ("entry.decide", json!({"node": "t"}), "api.unsupported"),
            ("node.format", json!({}), "api.unsupported"),
        ];
        for (name, body, want) in cases {
            let refused = prepare(name, body.clone(), None).expect_err(name);
            assert_eq!(code(&refused), want, "{name} {body}");
            let envelope = refused.data.as_ref().expect("envelope");
            let op = find(name).expect("op");
            assert_eq!(
                envelope["schema"],
                crate::output::schema_id(&op.cli.join(".")),
                "{name}"
            );
            assert_eq!(envelope["ok"], false);
            assert!(refused.failure.is_some());
        }
        // A key and a different id: which one would the retry use?
        let both = prepare(
            "msg.send",
            json!({"to": "b", "kind": "note", "id": "m-2"}),
            Some("m-1"),
        )
        .expect_err("differ");
        assert_eq!(code(&both), "usage.invalid_arguments");
        // A lease whose id spells like one is still claimed by path.
        assert!(prepare("lease.claim", json!({"target": "lease-notes.md"}), None).is_ok());
    }

    /// A stand-in CLI: prints an envelope with its arguments and exits
    /// `exit`.
    #[cfg(unix)]
    fn fake_cli(dir: &Path, exit: u8) -> String {
        use std::os::unix::fs::PermissionsExt;
        let (ok, diag) = if exit == 0 {
            ("true", "")
        } else {
            (
                "false",
                r#"{"code":"coord.lease_held","severity":"error","message":"held"}"#,
            )
        };
        let path = dir.join(format!("fake-{exit}"));
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\nprintf '{{\"schema\":\"mdkg.claim.v1\",\"ok\":{ok},\"data\":{{\"argv\":\"%s\"}},\"diagnostics\":[{diag}]}}' \"$*\"\nexit {exit}\n"
            ),
        )
        .expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        path.to_string_lossy().into_owned()
    }

    #[cfg(unix)]
    #[test]
    fn apply_returns_the_cli_envelope_and_its_exit_status() {
        let dir = std::env::temp_dir().join(format!("ops-exec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let op = find("lease.claim").expect("claim");
        let request = Request {
            op: op.name,
            body: json!({"target": "src/a.rs"}),
        };
        let granted = {
            let b = backend(&fake_cli(&dir, 0));
            b.apply(&ctx(b.clone(), None), op, &request)
        };
        assert_eq!(granted.failure, None);
        let envelope = granted.data.as_ref().expect("envelope");
        assert_eq!(envelope["ok"], true);
        assert_eq!(
            envelope["data"]["argv"],
            "--root /repo --as=alice claim --json -- src/a.rs"
        );
        assert_eq!(mdkg_ops::codes::http_status(Some(op), &granted), 201);
        let held = {
            let b = backend(&fake_cli(&dir, 5));
            b.apply(&ctx(b.clone(), None), op, &request)
        };
        assert_eq!(held.failure, Some(ErrorKind::Conflict));
        assert_eq!(code(&held), "coord.lease_held");
        assert_eq!(mdkg_ops::codes::http_status(Some(op), &held), 409);
        // No binary, no envelope: the CLI's shape all the same.
        let missing = {
            let b = backend("/nonexistent/mdkg");
            b.apply(&ctx(b.clone(), None), op, &request)
        };
        assert_eq!(missing.failure, Some(ErrorKind::Failure));
        assert_eq!(
            missing.data.as_ref().expect("envelope")["schema"],
            "mdkg.claim.v1"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    fn script(dir: &Path, name: &str, body: &str) -> String {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        path.to_string_lossy().into_owned()
    }

    #[cfg(unix)]
    #[test]
    fn a_stream_is_the_command_s_lines_by_cursor() {
        let dir = std::env::temp_dir().join(format!("ops-stream-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let op = find("msg.watch").expect("watch");
        let request = Request {
            op: op.name,
            body: json!({"after": "start", "types": ["mdkg.msg.sent"]}),
        };
        let events = script(
            &dir,
            "events",
            r#"printf '{"schema":"mdkg.api.event.v1","cursor":"c1","type":"a"}\n'
sleep 2
printf '{"cursor":"c2","type":"b"}\n'"#,
        );
        let b = backend(&events);
        let mut stream = b
            .stream(&ctx(b.clone(), None), op, &request)
            .unwrap_or_else(|r| panic!("{:?}", r.data));
        let first = stream.next_event().expect("first").expect("event");
        assert_eq!(first.id, "c1");
        assert_eq!(
            first.data,
            r#"{"schema":"mdkg.api.event.v1","cursor":"c1","type":"a"}"#
        );
        assert_eq!(
            stream.next_event().expect("second").expect("event").id,
            "c2"
        );
        assert_eq!(stream.next_event().expect("end"), None);
        // A refusal before the first event is the CLI's envelope.
        let refused = script(
            &dir,
            "refused",
            r#"printf '{"schema":"mdkg.msg.watch.v1","ok":false,"diagnostics":[{"code":"api.bad_cursor","severity":"error","message":"bad"}]}\n'
exit 1"#,
        );
        // A loaded machine may start the script after the usual grace; the
        // refusal ends the wait as soon as it's printed.
        let mut patient = SubprocessBackend::with_exe(
            PathBuf::from(&refused),
            Path::new("/repo"),
            Some(GRAPH.into()),
        );
        patient.open_grace = Duration::from_secs(30);
        let b = Arc::new(patient);
        let Err(report) = b.stream(&ctx(b.clone(), None), op, &request) else {
            panic!("opened");
        };
        assert_eq!(code(&report), "api.bad_cursor");
        assert_eq!(mdkg_ops::codes::http_status(Some(op), &report), 400);
        // An operation that isn't a stream isn't opened, and a stream isn't
        // applied.
        let claim = find("lease.claim").expect("claim");
        assert!(b.stream(&ctx(b.clone(), None), claim, &request).is_err());
        assert_eq!(
            code(&b.apply(&ctx(b.clone(), None), op, &request)),
            "api.unsupported"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn a_failing_pre_spawn_check_refuses_without_spawning() {
        let dir = std::env::temp_dir().join(format!("ops-check-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let ran = dir.join("ran");
        let exe = script(&dir, "marks", &format!("touch '{}'", ran.display()));
        let draining = || {
            Arc::new(
                SubprocessBackend::with_exe(PathBuf::from(&exe), Path::new("/repo"), None)
                    .with_check(|| {
                        Err(Refused {
                            code: Code::ApiDraining,
                            kind: ErrorKind::Conflict,
                            message: "the server's binary changed; it's draining".into(),
                        })
                    }),
            )
        };
        let claim = find("lease.claim").expect("claim");
        let request = Request {
            op: claim.name,
            body: json!({"target": "src/a.rs"}),
        };
        let b = draining();
        let report = b.apply(&ctx(b.clone(), None), claim, &request);
        assert_eq!(code(&report), "api.draining");
        assert_eq!(
            report.data.as_ref().expect("envelope")["schema"],
            "mdkg.claim.v1"
        );
        assert_eq!(mdkg_ops::codes::http_status(Some(claim), &report), 503);
        let watch = find("msg.watch").expect("watch");
        let request = Request {
            op: watch.name,
            body: json!({"after": "start"}),
        };
        let Err(report) = b.stream(&ctx(b.clone(), None), watch, &request) else {
            panic!("opened");
        };
        assert_eq!(code(&report), "api.draining");
        assert!(!ran.exists(), "a refused call spawned");
        // A passing check runs the call.
        let b = Arc::new(
            SubprocessBackend::with_exe(PathBuf::from(&exe), Path::new("/repo"), None)
                .with_check(|| Ok(())),
        );
        let request = Request {
            op: claim.name,
            body: json!({"target": "src/a.rs"}),
        };
        b.apply(&ctx(b.clone(), None), claim, &request);
        assert!(ran.exists(), "a checked call didn't run");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_served_binary_is_this_one_resolved() {
        let exe = self_exe().expect("exe");
        #[cfg(target_os = "linux")]
        assert_eq!(exe, PathBuf::from("/proc/self/exe"));
        #[cfg(not(target_os = "linux"))]
        assert_eq!(
            exe,
            std::fs::canonicalize(std::env::current_exe().expect("current")).expect("real")
        );
        assert!(exe.is_absolute());
        // A backend runs it, and serves no graph where there's none.
        let backend = SubprocessBackend::new(Path::new("/nonexistent")).expect("backend");
        assert_eq!(backend.exe, exe);
        assert_eq!(backend.graph_id, None);
    }

    #[test]
    fn exit_codes_are_the_contract_s_kinds() {
        assert_eq!(exit_kind(Some(0)), None);
        for kind in ErrorKind::ALL {
            assert_eq!(exit_kind(Some(i32::from(kind.exit_code()))), Some(kind));
        }
        assert_eq!(exit_kind(Some(101)), Some(ErrorKind::Failure));
        assert_eq!(exit_kind(None), Some(ErrorKind::Failure));
    }
}
