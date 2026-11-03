//! `agents logs` (API v1 A11, A18): a turn's output journal, normalized
//! across harnesses, and an agent's turns one after another. NDJSON with
//! `--json`, one bare `mdkg.api.turn_output.v1` per line, flushed per
//! line; a refusal before the stream starts is a `mdkg.agents.logs.v1`
//! envelope instead.
//!
//! - `--turn <turn_id>`: that turn's journal; with `--follow` it closes
//!   after the turn's `turn.ended`.
//! - `--follow` alone: the agent's turns in number order, each from its
//!   `turn.started` to its `turn.ended`, never closing at a turn's end, with
//!   `agent.state` frames when it starts and whenever the state changes.
//! - neither: the current turn, else the last one, and exit.
//!
//! Positions (`--after`): `<turn_id>:<seq>` resumes after that event,
//! `<turn_id>:end` starts with the turn after that one, `turn:<N>` with
//! turn number N; with `--turn`, a plain `<seq>` too.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use clap::Args;
use mdkg_format::{Code, Diagnostic};
use mdkg_graph::{ErrorKind, GraphError};
use mdkg_run::state;
use mdkg_run::supervise::{registry, timestamp};
use mdkg_run::turn::index::{self, Entry, Index};
use mdkg_run::turn::journal::{self, Event};
use mdkg_run::turn::observe;
use serde::Serialize;
use serde_json::{Value, json};

use super::{configured, known};
use crate::coord::Repo;
use crate::output::{self, Report};
use crate::run::graph_error;

#[derive(Debug, Args)]
pub struct LogsArgs {
    /// The agent's name.
    pub agent: String,
    /// Only this turn (its turn id).
    #[arg(long)]
    pub turn: Option<String>,
    /// Keep following: a turn until it ends, or the agent's turns for good.
    #[arg(long)]
    pub follow: bool,
    /// Start after this position: `<turn_id>:<seq>`, `<turn_id>:end` or
    /// `turn:<N>` (with --turn, a plain seq too).
    #[arg(long)]
    pub after: Option<String>,
    /// Also show native events no mapping covers.
    #[arg(long)]
    pub native: bool,
    /// Print NDJSON, one turn output event per line.
    #[arg(long)]
    pub json: bool,
}

/// What a refusal carries.
#[derive(Debug, Default, Serialize, schemars::JsonSchema)]
pub struct LogsData {
    /// The turn's retained receipt, its lasting record, when its output is
    /// gone (`api.output_gone`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub receipt: Option<PathBuf>,
    /// The earliest position still held, when the one given is past the
    /// index's trim (`api.cursor_expired`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub earliest: Option<String>,
}

/// How often a follow looks: `$MDKG_WAIT_POLL_MS`, else once a second.
fn poll_interval() -> Duration {
    let ms = std::env::var("MDKG_WAIT_POLL_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1_000u64);
    Duration::from_millis(ms.max(10))
}

/// A refusal before the stream starts.
struct Refusal(GraphError, LogsData);

impl From<GraphError> for Refusal {
    fn from(e: GraphError) -> Self {
        Self(e, LogsData::default())
    }
}

/// Where a stream starts.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Position {
    /// After `seq` of this turn, then every later turn.
    After(String, u64),
    /// With the turn after this one.
    End(String),
    /// With turn number N.
    Number(u64),
}

fn bad_cursor(text: &str) -> GraphError {
    GraphError::usage(
        Code::ApiBadCursor,
        format!("{text:?} isn't a position: use <turn_id>:<seq>, <turn_id>:end or turn:<N>"),
    )
    .with_hint("pass the follow_from that agents status returns, as it is")
}

fn parse_position(text: &str) -> Result<Position, GraphError> {
    if let Some(n) = text.strip_prefix("turn:") {
        return n
            .parse::<u64>()
            .ok()
            .filter(|n| *n >= 1)
            .map(Position::Number)
            .ok_or_else(|| bad_cursor(text));
    }
    let (id, rest) = text.rsplit_once(':').ok_or_else(|| bad_cursor(text))?;
    if uuid::Uuid::parse_str(id).is_err() {
        return Err(bad_cursor(text));
    }
    match rest {
        "end" => Ok(Position::End(id.into())),
        seq => seq
            .parse::<u64>()
            .map(|s| Position::After(id.into(), s))
            .map_err(|_| bad_cursor(text)),
    }
}

/// The stream: lines written and flushed one at a time. A reader that went
/// away ends it quietly.
struct Out {
    json: bool,
    native: bool,
    closed: bool,
}

impl Out {
    fn send(&mut self, event: &Event) {
        if self.closed || (event.kind == "native" && !self.native) {
            return;
        }
        let line = if self.json {
            serde_json::to_string(event).unwrap_or_default()
        } else {
            text_line(event)
        };
        let mut stdout = std::io::stdout().lock();
        if writeln!(stdout, "{line}")
            .and_then(|()| stdout.flush())
            .is_err()
        {
            self.closed = true;
        }
    }
}

fn text_line(e: &Event) -> String {
    let safe = mdkg_run::digest::terminal_safe;
    let seq = e.seq.map_or_else(|| "-".into(), |s| s.to_string());
    let detail = match e.kind.as_str() {
        "text" | "status" | "error" => e
            .data
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        "tool.started" => format!(
            "{} {}",
            e.data
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            e.data
                .get("summary")
                .and_then(Value::as_str)
                .unwrap_or_default()
        ),
        _ => e.data.to_string(),
    };
    format!("[{seq}] {}: {}", e.kind, safe(&detail))
}

fn synthetic(agent: &str, turn_id: &str, seq: u64, kind: &str, data: Value) -> Event {
    Event {
        schema: journal::SCHEMA.into(),
        turn: Some(journal::turn_uri(agent, turn_id)),
        seq: Some(seq),
        time: timestamp(state::now_ms()),
        kind: kind.into(),
        data,
    }
}

fn agent_state_frame(state: &str) -> Event {
    Event {
        schema: journal::SCHEMA.into(),
        turn: None,
        seq: None,
        time: timestamp(state::now_ms()),
        kind: "agent.state".into(),
        data: json!({"state": state}),
    }
}

/// The turn's derived end, when its evidence says it's over.
fn derived_end(dir: &Path, agent: &str, entry: &Entry, events: Option<&[Event]>) -> Option<Value> {
    let derived = index::derive(dir, agent, entry, events, |s| {
        index::turn_running(dir, agent, s)
    });
    (derived.state != "running").then(|| json!({"outcome": derived.outcome}))
}

fn output_gone(dir: &Path, agent: &str, turn_id: &str) -> Refusal {
    let receipt = index::receipt_path(dir, agent, turn_id);
    Refusal(
        GraphError::new(
            ErrorKind::NotFound,
            Diagnostic::error(
                Code::ApiOutputGone,
                format!("turn {turn_id}'s output was purged"),
            ),
        )
        .with_hint("its receipt is the lasting record"),
        LogsData {
            receipt: receipt.exists().then_some(receipt),
            earliest: None,
        },
    )
}

fn expired(index: &Index) -> Refusal {
    Refusal(
        GraphError::new(
            ErrorKind::Conflict,
            Diagnostic::error(
                Code::ApiCursorExpired,
                "the position names a turn the index no longer holds",
            ),
        )
        .with_hint("start again from the earliest position, or from agents status"),
        LogsData {
            receipt: None,
            earliest: index.earliest().map(|e| format!("turn:{}", e.start.number)),
        },
    )
}

/// SIGINT and SIGTERM end a stream cleanly.
fn stop_flag() -> Arc<AtomicBool> {
    use signal_hook::consts::{SIGINT, SIGTERM};
    let flag = Arc::new(AtomicBool::new(false));
    for signal in [SIGINT, SIGTERM] {
        let _ = signal_hook::flag::register(signal, flag.clone());
    }
    flag
}

pub(super) fn run(root: &Path, a: &LogsArgs) -> u8 {
    match logs(root, a) {
        Ok(()) => 0,
        Err(Refusal(e, data)) => {
            let report = Report {
                data: Some(data),
                diagnostics: vec![*e.diagnostic],
                failure: Some(e.kind),
            };
            output::emit("agents.logs", a.json, report, |_| ())
        }
    }
}

fn logs(root: &Path, a: &LogsArgs) -> Result<(), Refusal> {
    state::check_name(&a.agent).map_err(graph_error)?;
    let repo = Repo::open(root)?;
    let dir = state::run_dir(&repo.top).map_err(graph_error)?;
    let turns = index::read(&dir, &a.agent).map_err(graph_error)?;
    let (found, _) = registry::list(&dir).unwrap_or_default();
    let defined = || -> Result<bool, GraphError> {
        let snapshot = super::config(&repo)?;
        Ok(configured(&snapshot.effective.values).contains(&a.agent)
            || known(&dir).contains(&a.agent))
    };
    if turns.entries.is_empty()
        && !found.iter().any(|f| f.record.agents.contains_key(&a.agent))
        && !defined()?
    {
        return Err(super::unknown_agent(&a.agent).into());
    }
    let mut out = Out {
        json: a.json,
        native: a.native,
        closed: false,
    };
    let stop = stop_flag();
    if let Some(turn_id) = &a.turn {
        return one_turn(&dir, &a.agent, turn_id, a, &mut out, &stop);
    }
    if !a.follow {
        // The current turn, else the last one.
        let view = observe::agent(&dir, &a.agent, &found, false);
        let id = view
            .current_turn
            .map(|t| t.turn_id)
            .or(view.last_turn.map(|t| t.turn_id));
        if let Some(id) = id {
            return one_turn(&dir, &a.agent, &id, a, &mut out, &stop);
        }
        return Ok(());
    }
    follow_agent(&dir, &a.agent, turns, a, &mut out, &stop)
}

fn one_turn(
    dir: &Path,
    agent: &str,
    turn_id: &str,
    a: &LogsArgs,
    out: &mut Out,
    stop: &AtomicBool,
) -> Result<(), Refusal> {
    // A turn id is a UUID, never a path.
    if uuid::Uuid::parse_str(turn_id).is_err() {
        return Err(
            GraphError::usage(Code::UsageBadValue, format!("{turn_id:?} isn't a turn id")).into(),
        );
    }
    let after = match a.after.as_deref() {
        None => 0,
        Some(text) if text.bytes().all(|b| b.is_ascii_digit()) && !text.is_empty() => {
            text.parse().map_err(|_| bad_cursor(text))?
        }
        Some(text) => match parse_position(text)? {
            Position::After(id, seq) if id == turn_id => seq,
            Position::End(id) if id == turn_id => u64::MAX,
            _ => return Err(bad_cursor(text).into()),
        },
    };
    let index = index::read(dir, agent).map_err(graph_error)?;
    let entry = index.find(turn_id).cloned();
    let mut tail = journal::Tail::new(dir, agent, turn_id);
    let first = loop {
        match tail.read_new() {
            Ok(Some(first)) => break first,
            Ok(None) => (),
            // A journal that's there but can't be read isn't one not
            // written yet: never wait on it in silence.
            Err(e) => {
                return Err(graph_error(mdkg_run::RunError::io(
                    &format!("can't read turn {turn_id}'s output journal"),
                    e,
                ))
                .into());
            }
        }
        let unknown = |why: String| -> Refusal {
            GraphError::new(
                ErrorKind::NotFound,
                Diagnostic::error(Code::ApiUnknownResource, why),
            )
            .into()
        };
        let Some(entry) = index::read(dir, agent)
            .ok()
            .and_then(|i| i.find(turn_id).cloned())
            .or_else(|| entry.clone())
        else {
            if index::receipt_path(dir, agent, turn_id).exists() {
                return Err(output_gone(dir, agent, turn_id));
            }
            return Err(unknown(format!("{agent} has no turn {turn_id}")));
        };
        // No journal yet is a turn just starting (its index line comes
        // first), or an attempt that never got one: neither was purged.
        let derived = index::derive(dir, agent, &entry, None, |s| {
            index::turn_running(dir, agent, s)
        });
        if derived.state == "running" {
            if !a.follow || out.closed || stop.load(Ordering::SeqCst) {
                return Ok(());
            }
            std::thread::sleep(poll_interval());
            continue;
        }
        if derived.outcome.as_deref() == Some("not_started") {
            return Err(unknown(format!("{agent}'s turn {turn_id} never started")));
        }
        return Err(output_gone(dir, agent, turn_id));
    };
    let mut seen: Vec<Event> = Vec::new();
    let mut last = 0;
    let mut ended = false;
    let mut batch = first;
    loop {
        for e in batch {
            last = last.max(e.seq.unwrap_or(0));
            ended |= e.kind == "turn.ended";
            if e.seq.unwrap_or(0) > after {
                out.send(&e);
            }
            seen.push(e);
        }
        if !a.follow || ended || out.closed || stop.load(Ordering::SeqCst) {
            return Ok(());
        }
        if let Some(entry) = index::read(dir, agent)
            .ok()
            .and_then(|i| i.find(turn_id).cloned())
            .or_else(|| entry.clone())
            && let Some(data) = derived_end(dir, agent, &entry, Some(&seen))
        {
            // Over by its evidence, without a journal end: say so, once.
            let tail_end = tail.read_new().ok().flatten().unwrap_or_default();
            if tail_end.is_empty() {
                out.send(&synthetic(agent, turn_id, last + 1, "turn.ended", data));
                return Ok(());
            }
            batch = tail_end;
            continue;
        }
        std::thread::sleep(poll_interval());
        batch = tail.read_new().ok().flatten().unwrap_or_default();
    }
}

fn follow_agent(
    dir: &Path,
    agent: &str,
    turns: Index,
    a: &LogsArgs,
    out: &mut Out,
    stop: &AtomicBool,
) -> Result<(), Refusal> {
    // The first turn number to send, and the seq to resume after in it.
    let (mut number, mut after) = match a.after.as_deref().map(parse_position).transpose()? {
        Some(Position::After(id, seq)) => match turns.find(&id) {
            Some(e) => (e.start.number, seq),
            None => return Err(expired(&turns)),
        },
        Some(Position::End(id)) => match turns.find(&id) {
            Some(e) => (e.start.number + 1, 0),
            None => return Err(expired(&turns)),
        },
        Some(Position::Number(n)) => {
            if turns.earliest().is_some_and(|e| n < e.start.number) {
                return Err(expired(&turns));
            }
            (n, 0)
        }
        None => {
            let (found, _) = registry::list(dir).unwrap_or_default();
            let view = observe::agent(dir, agent, &found, false);
            match view.current_turn {
                Some(t) => (t.number, 0),
                None => (turns.next_number(), 0),
            }
        }
    };
    let mut state: Option<String> = None;
    let mut tail: Option<(Entry, journal::Tail, Vec<Event>)> = None;
    loop {
        if out.closed || stop.load(Ordering::SeqCst) {
            return Ok(());
        }
        let index = index::read(dir, agent).unwrap_or_default();
        let (found, _) = registry::list(dir).unwrap_or_default();
        let now = observe::agent(dir, agent, &found, false).state;
        if state.as_deref() != Some(now.as_str()) {
            out.send(&agent_state_frame(&now));
            state = Some(now);
        }
        if tail.is_none() {
            match index.by_number(number) {
                Some(entry) => {
                    let mut t = journal::Tail::new(dir, agent, &entry.start.turn_id);
                    match t.read_new().ok().flatten() {
                        Some(first) => tail = Some((entry.clone(), t, first)),
                        None => {
                            // Its output is gone: the turn still shows, never
                            // skipped silently.
                            let id = &entry.start.turn_id;
                            let derived = index::derive(dir, agent, entry, None, |s| {
                                index::turn_running(dir, agent, s)
                            });
                            if derived.state == "running" {
                                std::thread::sleep(poll_interval());
                                continue;
                            }
                            out.send(&synthetic(
                                agent,
                                id,
                                1,
                                "turn.started",
                                json!({"turn_id": id, "number": entry.start.number}),
                            ));
                            out.send(&synthetic(
                                agent,
                                id,
                                2,
                                "turn.ended",
                                json!({"outcome": derived.outcome, "output_gone": true}),
                            ));
                            number += 1;
                            after = 0;
                            continue;
                        }
                    }
                }
                None if index.entries.iter().any(|e| e.start.number > number) => {
                    // A number no attempt holds: go on to the next.
                    number += 1;
                    after = 0;
                    continue;
                }
                None => {
                    std::thread::sleep(poll_interval());
                    continue;
                }
            }
        }
        let (entry, t, pending) = tail.as_mut().expect("a turn is open");
        let mut ended = false;
        let mut last = 0;
        for e in std::mem::take(pending) {
            last = last.max(e.seq.unwrap_or(0));
            ended |= e.kind == "turn.ended";
            if e.seq.unwrap_or(0) > after {
                out.send(&e);
            }
        }
        after = after.max(last);
        if !ended {
            let more = t.read_new().ok().flatten().unwrap_or_default();
            if !more.is_empty() {
                *pending = more;
                continue;
            }
            let entry = index
                .find(&entry.start.turn_id)
                .cloned()
                .unwrap_or(entry.clone());
            let events = journal::read(dir, agent, &entry.start.turn_id)
                .ok()
                .flatten();
            match derived_end(dir, agent, &entry, events.as_deref()) {
                Some(data) => {
                    let more = t.read_new().ok().flatten().unwrap_or_default();
                    if !more.is_empty() {
                        *pending = more;
                        continue;
                    }
                    out.send(&synthetic(
                        agent,
                        &entry.start.turn_id,
                        after + 1,
                        "turn.ended",
                        data,
                    ));
                }
                None => {
                    std::thread::sleep(poll_interval());
                    continue;
                }
            }
        }
        tail = None;
        number += 1;
        after = 0;
    }
}
