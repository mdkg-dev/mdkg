//! An agent's turn index (API v1 A7): `<run dir>/<agent>/turns.jsonl`,
//! internal. One start line per admitted turn attempt, written before its
//! journal and its D6 admission, and one end line after its receipt and
//! D6 completion:
//!
//! - start: `{line: "start", turn_id, number, files, started_at,
//!   supervisor_id, host, pid, pid_started_at}`, naming the launcher
//!   process that runs it and the `turn-<M>` stem of its raw logs;
//! - end: `{line: "end", turn_id, ended_at, outcome}`.
//!
//! The index gives a turn its API number: one more than the highest it
//! holds, so an attempt that never ran keeps its number and numbers are
//! never reused. Only the holder of the agent's `run:<agent>` turn lease
//! writes it. Every newline-terminated line is committed, a tail without
//! its newline isn't, and the writer drops such a tail before its next
//! append. Past 2000 turns it keeps the newest 1000 whole, and every start
//! line without an end.
//!
//! Readers derive a turn's state from evidence ([`derive`]): ended, from
//! the receipt, the end line or the journal's `turn.ended`; running, while
//! the launcher the start line names is alive; interrupted, when the
//! journal shows the harness was about to start; else not started.

use std::fs::{self, File};
use std::io::{Read as _, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::journal;
use crate::RunError;
use crate::state;

pub const FILE: &str = "turns.jsonl";
/// Past this many turns, the writer trims to [`KEEP`].
const TRIM_AT: usize = 2000;
const KEEP: usize = 1000;
/// The most the index file is read.
const CAP: u64 = 64 << 20;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "line", rename_all = "snake_case")]
pub enum Line {
    Start(Start),
    End(End),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Start {
    pub turn_id: String,
    pub number: u64,
    /// The `turn-<M>` stem of the turn's raw logs.
    pub files: String,
    pub started_at: String,
    pub supervisor_id: Option<String>,
    pub host: String,
    pub pid: u32,
    pub pid_started_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct End {
    pub turn_id: String,
    pub ended_at: String,
    pub outcome: String,
}

/// A turn as the index holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub start: Start,
    pub end: Option<End>,
}

/// The committed index.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Index {
    /// In the order their start lines were written, which is number order.
    pub entries: Vec<Entry>,
    /// The file holds a tail without its newline, which isn't committed.
    pub tail: bool,
    /// The file's inode, so a follower sees a rewrite.
    pub inode: Option<u64>,
}

pub fn path(run_dir: &Path, agent: &str) -> PathBuf {
    state::turns_dir(run_dir, agent).join(FILE)
}

fn parse(bytes: &[u8]) -> (Vec<Line>, bool) {
    let committed = match bytes.iter().rposition(|b| *b == b'\n') {
        Some(end) => &bytes[..=end],
        None => &bytes[..0],
    };
    let tail = committed.len() < bytes.len();
    let lines = committed
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
        .filter_map(|l| serde_json::from_slice(l).ok())
        .collect();
    (lines, tail)
}

fn entries(lines: &[Line]) -> Vec<Entry> {
    let mut entries: Vec<Entry> = Vec::new();
    for line in lines {
        match line {
            Line::Start(start) => {
                if !entries.iter().any(|e| e.start.turn_id == start.turn_id) {
                    entries.push(Entry {
                        start: start.clone(),
                        end: None,
                    });
                }
            }
            Line::End(end) => {
                // An end line without its start is never kept by the writer;
                // one read anyway is ignored.
                if let Some(entry) = entries
                    .iter_mut()
                    .find(|e| e.start.turn_id == end.turn_id && e.end.is_none())
                {
                    entry.end = Some(end.clone());
                }
            }
        }
    }
    entries
}

fn read_bytes(path: &Path) -> std::io::Result<Option<(Vec<u8>, u64)>> {
    use std::os::unix::fs::MetadataExt as _;
    let mut file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let inode = file.metadata()?.ino();
    let mut bytes = Vec::new();
    (&mut file).take(CAP).read_to_end(&mut bytes)?;
    Ok(Some((bytes, inode)))
}

/// The agent's committed index; empty when there's none.
pub fn read(run_dir: &Path, agent: &str) -> Result<Index, RunError> {
    state::check_name(agent)?;
    let Some((bytes, inode)) = read_bytes(&path(run_dir, agent))
        .map_err(|e| RunError::io("can't read the turn index", e))?
    else {
        return Ok(Index::default());
    };
    let (lines, tail) = parse(&bytes);
    Ok(Index {
        entries: entries(&lines),
        tail,
        inode: Some(inode),
    })
}

impl Index {
    /// The number the next start line takes.
    pub fn next_number(&self) -> u64 {
        self.entries
            .iter()
            .map(|e| e.start.number)
            .max()
            .unwrap_or(0)
            + 1
    }

    pub fn find(&self, turn_id: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.start.turn_id == turn_id)
    }

    pub fn by_number(&self, number: u64) -> Option<&Entry> {
        self.entries.iter().find(|e| e.start.number == number)
    }

    /// The earliest turn still held.
    pub fn earliest(&self) -> Option<&Entry> {
        self.entries.iter().min_by_key(|e| e.start.number)
    }
}

/// Appends `line`, under the agent's turn lease. Drops an uncommitted tail
/// first, and trims a long index, each by rewriting the committed lines
/// whole: a temp file, synced, renamed over the index, the folder synced.
/// The appended line is synced before this returns.
pub fn append(run_dir: &Path, agent: &str, line: &Line) -> Result<(), RunError> {
    state::check_name(agent)?;
    let dir = state::turns_dir(run_dir, agent);
    create_folder(&dir).map_err(|e| RunError::io("can't create the turn folder", e))?;
    let path = dir.join(FILE);
    let io = |e| RunError::io("can't write the turn index", e);
    if let Some((bytes, _)) = read_bytes(&path).map_err(io)? {
        let (lines, tail) = parse(&bytes);
        let starts = lines.iter().filter(|l| matches!(l, Line::Start(_))).count();
        if tail || (matches!(line, Line::Start(_)) && starts >= TRIM_AT) {
            let kept = if starts >= TRIM_AT {
                trim(&lines)
            } else {
                lines
            };
            rewrite(&path, &kept).map_err(io)?;
        }
    }
    let mut text = serde_json::to_string(line)
        .map_err(|e| RunError::failure("io.write_failed", e.to_string()))?;
    text.push('\n');
    let created = !path.exists();
    let mut file = fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(&path)
        .map_err(io)?;
    file.write_all(text.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(io)?;
    if created {
        File::open(&dir).and_then(|d| d.sync_all()).map_err(io)?;
    }
    Ok(())
}

pub(crate) fn create_folder(dir: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dir)
}

/// The newest [`KEEP`] turns whole, and every start without an end,
/// however old. Never an end line without its start.
fn trim(lines: &[Line]) -> Vec<Line> {
    let all = entries(lines);
    let mut numbers: Vec<u64> = all.iter().map(|e| e.start.number).collect();
    numbers.sort_unstable();
    let floor = numbers.len().checked_sub(KEEP).map_or(0, |i| numbers[i]);
    let keep: std::collections::BTreeSet<&str> = all
        .iter()
        .filter(|e| e.start.number >= floor || e.end.is_none())
        .map(|e| e.start.turn_id.as_str())
        .collect();
    lines
        .iter()
        .filter(|l| match l {
            Line::Start(s) => keep.contains(s.turn_id.as_str()),
            Line::End(e) => keep.contains(e.turn_id.as_str()),
        })
        .cloned()
        .collect()
}

fn rewrite(path: &Path, lines: &[Line]) -> std::io::Result<()> {
    let mut bytes = Vec::new();
    for line in lines {
        serde_json::to_writer(&mut bytes, line)?;
        bytes.push(b'\n');
    }
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("no folder"))?;
    let tmp = path.with_extension(format!("{}.tmp", uuid::Uuid::now_v7()));
    let write = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open(&tmp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        File::open(dir)?.sync_all()
    })();
    if write.is_err() {
        fs::remove_file(&tmp).ok();
    }
    write
}

/// The launcher process a start line names: this one. Its host is read
/// afresh, so a renamed host is recorded by its new name.
pub fn launcher() -> (String, u32, Option<String>) {
    static STARTED: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    let pid = std::process::id();
    let started = STARTED.get_or_init(|| super::started(pid).ok().flatten());
    (super::this_host().unwrap_or_default(), pid, started.clone())
}

/// A turn's state, derived from its evidence (A7 step 4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Derived {
    /// `running`, `ended` or `interrupted`.
    pub state: &'static str,
    /// Once it isn't running: `replied`, `no_reply`, `ran`, `idle`, `dry_run`,
    /// `failed`, `interrupted` or `not_started`.
    pub outcome: Option<String>,
    pub ended_at: Option<String>,
}

/// The retained receipt of a turn, if there is one.
pub fn receipt_path(run_dir: &Path, agent: &str, turn_id: &str) -> PathBuf {
    run_dir
        .join("receipts")
        .join(state::file_name(agent))
        .join(format!("{turn_id}.txt"))
}

/// The outcome a retained receipt records: its result's outcome, or
/// `failed` for an error result.
pub fn receipt_outcome(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let data: Value = text
        .lines()
        .find(|l| l.starts_with('{'))
        .and_then(|l| serde_json::from_str(l).ok())?;
    let result = data.get("result")?;
    match result.get("status").and_then(Value::as_str)? {
        "ok" => result
            .get("outcome")
            .and_then(Value::as_str)
            .map(str::to_string),
        _ => Some("failed".into()),
    }
}

/// Whether the turn the start line names may still be running, for
/// [`finalize_orphans`] alone, which runs under the agent's exclusive turn
/// lease: no other launcher can run one of its turns then, so a supervisor
/// record that doesn't show the agent running is enough to close it. A
/// supervisor lives across many turns and agents, so its process being
/// alive isn't enough: its record must still be live, be that process,
/// and show the agent running. A launcher with no supervisor (a plain
/// `run`) counts while its process is alive. Readers without the lease
/// use [`launcher_alive`].
pub fn turn_running(run_dir: &Path, agent: &str, start: &Start) -> bool {
    let Some(id) = &start.supervisor_id else {
        return launcher_alive(start);
    };
    let Ok((found, _)) = crate::supervise::registry::list(run_dir) else {
        // Can't tell: never prove it gone.
        return launcher_alive(start);
    };
    found.iter().any(|f| {
        f.live
            && f.record.supervisor_id == *id
            && f.record.pid == start.pid
            && f.record.agents.get(agent) == Some(&crate::supervise::registry::AgentState::Running)
    }) && launcher_alive(start)
}

/// Whether the launcher the start line names is alive: its pid running
/// here with the start time it recorded, or, with none recorded, its pid
/// running. A `ps` that can't answer never proves it gone. The host isn't
/// compared (decision of 2026-11-04, Terra #18919): a host renamed during
/// the turn keeps its launcher alive, and a launcher on another host is
/// gone here unless a local process has its pid and start time.
///
/// This is the one rule for whether a turn with no terminal evidence
/// (receipt, end line, journal `turn.ended`) is still running, for every
/// reader: `agents status`, `agents turns` and `agents logs`. A supervisor
/// record that doesn't show the agent running doesn't end a turn whose
/// launcher lives, since a failed write leaves the record behind.
pub fn launcher_alive(start: &Start) -> bool {
    match super::started(start.pid) {
        Ok(Some(now)) => start.pid_started_at.as_deref().is_none_or(|s| s == now),
        Ok(None) => false,
        Err(_) => true,
    }
}

/// Derives the state of `entry` (A7 step 4), terminal evidence first:
/// the receipt's outcome, then the end line's, then the journal's
/// `turn.ended`; then a live launcher; then the journal's
/// `harness.starting`; else not started.
pub fn derive(
    run_dir: &Path,
    agent: &str,
    entry: &Entry,
    events: Option<&[journal::Event]>,
    alive: impl FnOnce(&Start) -> bool,
) -> Derived {
    let id = &entry.start.turn_id;
    let journal_end = events.and_then(|events| {
        events
            .iter()
            .rev()
            .find(|e| e.kind == "turn.ended")
            .map(|e| {
                (
                    e.data
                        .get("outcome")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    e.time.clone(),
                )
            })
    });
    let receipt = receipt_outcome(&receipt_path(run_dir, agent, id));
    let end_outcome = entry.end.as_ref().map(|e| e.outcome.clone());
    let ended_at = entry
        .end
        .as_ref()
        .map(|e| e.ended_at.clone())
        .or_else(|| journal_end.as_ref().map(|(_, t)| t.clone()));
    if receipt.is_some() || entry.end.is_some() || journal_end.is_some() {
        let outcome = receipt
            .or(end_outcome)
            .or_else(|| journal_end.and_then(|(o, _)| o));
        let state = if outcome.as_deref() == Some("interrupted") {
            "interrupted"
        } else {
            "ended"
        };
        return Derived {
            state,
            outcome,
            ended_at,
        };
    }
    if alive(&entry.start) {
        return Derived {
            state: "running",
            outcome: None,
            ended_at: None,
        };
    }
    let started = events.is_some_and(|events| events.iter().any(|e| e.kind == "harness.starting"));
    if started {
        Derived {
            state: "interrupted",
            outcome: Some("interrupted".into()),
            ended_at: None,
        }
    } else {
        Derived {
            state: "ended",
            outcome: Some("not_started".into()),
            ended_at: None,
        }
    }
}

/// Appends the missing end line of every turn whose start line has none,
/// with the outcome its evidence gives (A7 step 7). Only under the agent's
/// exclusive turn lease, before the new turn's start line. A start line of
/// this launcher's own is closed whatever its record says: it holds the
/// lease, so that turn is over. Another launcher's turn that still runs
/// is left open, since the lease it held may only have lapsed and its own
/// end line is still to come. It never runs a harness again.
pub fn finalize_orphans(run_dir: &Path, agent: &str) -> Result<(), RunError> {
    let index = read(run_dir, agent)?;
    let (own_host, own_pid, _) = launcher();
    for entry in index.entries.iter().filter(|e| e.end.is_none()) {
        let events = journal::read(run_dir, agent, &entry.start.turn_id)
            .ok()
            .flatten();
        let derived = derive(run_dir, agent, entry, events.as_deref(), |s| {
            (s.host != own_host || s.pid != own_pid) && turn_running(run_dir, agent, s)
        });
        if derived.state == "running" {
            continue;
        }
        append(
            run_dir,
            agent,
            &Line::End(End {
                turn_id: entry.start.turn_id.clone(),
                ended_at: derived
                    .ended_at
                    .unwrap_or_else(|| crate::supervise::timestamp(state::now_ms())),
                outcome: derived.outcome.unwrap_or_else(|| "interrupted".into()),
            }),
        )?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn start(id: &str, number: u64) -> Line {
        Line::Start(Start {
            turn_id: id.into(),
            number,
            files: format!("turn-{number}"),
            started_at: "2026-10-30T12:00:00.000Z".into(),
            supervisor_id: None,
            host: "h".into(),
            // pid 1 runs, but never with this start time: a gone launcher.
            pid: 1,
            pid_started_at: Some("never".into()),
        })
    }

    pub(crate) fn end(id: &str, outcome: &str) -> Line {
        Line::End(End {
            turn_id: id.into(),
            ended_at: "2026-10-30T12:01:00.000Z".into(),
            outcome: outcome.into(),
        })
    }

    fn scratch() -> (crate::supervise::Scratch, PathBuf) {
        let scratch = crate::supervise::Scratch::new("turn-index");
        let run = scratch.run_dir();
        (scratch, run)
    }

    #[test]
    fn numbers_continue_past_an_attempt_that_never_ran_and_a_torn_tail_is_dropped() {
        let (_s, run) = scratch();
        assert_eq!(read(&run, "a").unwrap().next_number(), 1);
        append(&run, "a", &start("t1", 1)).unwrap();
        append(&run, "a", &end("t1", "not_started")).unwrap();
        let index = read(&run, "a").unwrap();
        assert_eq!(index.next_number(), 2);
        assert_eq!(
            index.find("t1").unwrap().end.as_ref().unwrap().outcome,
            "not_started"
        );
        // A tail without its newline isn't a line, through two restarts.
        let path = path(&run, "a");
        let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(br#"{"line":"start","turn_id":"torn","number":9"#)
            .unwrap();
        let index = read(&run, "a").unwrap();
        assert!(index.tail);
        assert!(index.find("torn").is_none());
        assert_eq!(index.next_number(), 2);
        append(&run, "a", &start("t2", 2)).unwrap();
        let index = read(&run, "a").unwrap();
        assert!(!index.tail);
        assert!(index.find("torn").is_none());
        assert_eq!(index.entries.len(), 2);
        assert!(fs::read_to_string(&path).unwrap().ends_with('\n'));
    }

    #[test]
    fn a_complete_line_whose_fsync_failed_reads_as_committed_and_derives_right() {
        // A file can't record whether its fsync succeeded: bytes that
        // reached it with their newline, and no sync, are what's left.
        let (_s, run) = scratch();
        let id = "01a18cb6-0000-7000-8000-000000000004";
        let mut text = serde_json::to_string(&start(id, 1)).unwrap();
        text.push('\n');
        fs::create_dir_all(path(&run, "a").parent().unwrap()).unwrap();
        fs::write(path(&run, "a"), text).unwrap();
        let index = read(&run, "a").unwrap();
        assert!(!index.tail);
        let entry = index.find(id).unwrap().clone();
        assert_eq!(index.next_number(), 2);
        // The start line alone: nothing spawned.
        let d = derive(&run, "a", &entry, None, |_| false);
        assert_eq!(
            (d.state, d.outcome.as_deref()),
            ("ended", Some("not_started"))
        );
        // An unsynced `harness.starting` that survived: it may have run.
        let mut journal = journal::Writer::create(&run, "a", id, 1).unwrap();
        journal
            .write(
                "harness.starting",
                serde_json::json!({"harness": "codex"}),
                false,
            )
            .unwrap();
        drop(journal);
        let events = journal::read(&run, "a", id).unwrap().unwrap();
        let d = derive(&run, "a", &entry, Some(&events), |_| false);
        assert_eq!(
            (d.state, d.outcome.as_deref()),
            ("interrupted", Some("interrupted"))
        );
        // The next holder closes it with that outcome.
        finalize_orphans(&run, "a").unwrap();
        let index = read(&run, "a").unwrap();
        assert_eq!(
            index.find(id).unwrap().end.as_ref().unwrap().outcome,
            "interrupted"
        );
    }

    #[test]
    fn trimming_keeps_the_newest_turns_whole_and_every_open_start() {
        let mut lines = vec![start("open", 1)];
        for n in 2..=(TRIM_AT as u64 + 1) {
            let id = format!("t{n}");
            lines.push(start(&id, n));
            lines.push(end(&id, "replied"));
        }
        let kept = trim(&lines);
        let index = entries(&kept);
        assert_eq!(index.len(), KEEP + 1);
        assert!(
            index
                .iter()
                .any(|e| e.start.turn_id == "open" && e.end.is_none())
        );
        let max = index.iter().map(|e| e.start.number).max().unwrap();
        assert_eq!(max, TRIM_AT as u64 + 1);
        // No end line without its start.
        for line in &kept {
            if let Line::End(e) = line {
                assert!(
                    kept.iter()
                        .any(|l| matches!(l, Line::Start(s) if s.turn_id == e.turn_id))
                );
            }
        }
    }

    #[test]
    fn appending_past_the_limit_rewrites_the_file_and_numbering_continues() {
        let (_s, run) = scratch();
        let dir = state::turns_dir(&run, "a");
        fs::create_dir_all(&dir).unwrap();
        let mut text = String::new();
        for n in 1..=(TRIM_AT as u64) {
            let id = format!("t{n}");
            text.push_str(&serde_json::to_string(&start(&id, n)).unwrap());
            text.push('\n');
            text.push_str(&serde_json::to_string(&end(&id, "ran")).unwrap());
            text.push('\n');
        }
        fs::write(dir.join(FILE), text).unwrap();
        let before = read(&run, "a").unwrap().inode;
        append(&run, "a", &start("next", TRIM_AT as u64 + 1)).unwrap();
        let index = read(&run, "a").unwrap();
        assert_ne!(index.inode, before);
        assert_eq!(index.entries.len(), KEEP + 1);
        assert_eq!(index.next_number(), TRIM_AT as u64 + 2);
        assert!(index.find("t1").is_none());
    }

    #[test]
    fn a_live_supervisor_counts_only_while_its_record_shows_the_agent_running() {
        let (_s, run) = scratch();
        let reg = crate::supervise::registry::Registered::register(&run, &["a".into()], None, None)
            .unwrap()
            .unwrap();
        let (host, pid, pid_started_at) = launcher();
        let s = Start {
            turn_id: "01a18cb6-0000-7000-8000-000000000002".into(),
            number: 1,
            files: "turn-1".into(),
            started_at: String::new(),
            supervisor_id: Some(reg.id()),
            host,
            pid,
            pid_started_at,
        };
        // This process is alive, but the agent is idle in its record: a turn
        // whose end was never written isn't running.
        assert!(launcher_alive(&s));
        assert!(!turn_running(&run, "a", &s));
        reg.set("a", crate::supervise::registry::AgentState::Running)
            .unwrap();
        assert!(turn_running(&run, "a", &s));
        assert!(!turn_running(&run, "b", &s));
        // Another supervisor id, or none at all for a plain run.
        let other = Start {
            supervisor_id: Some(uuid::Uuid::now_v7().to_string()),
            ..s.clone()
        };
        assert!(!turn_running(&run, "a", &other));
        let plain = Start {
            supervisor_id: None,
            ..s.clone()
        };
        assert!(turn_running(&run, "a", &plain));
        // Under the lease, this launcher's own open start is closed even
        // though its record shows the agent running.
        append(&run, "a", &Line::Start(s.clone())).unwrap();
        finalize_orphans(&run, "a").unwrap();
        let index = read(&run, "a").unwrap();
        assert_eq!(
            index
                .find(&s.turn_id)
                .unwrap()
                .end
                .as_ref()
                .unwrap()
                .outcome,
            "not_started"
        );
    }

    /// The one rule for readers (review of ade58853): with no terminal
    /// evidence, a turn whose launcher lives is running, whatever its
    /// supervisor record says, and only a launcher proven gone ends it.
    #[test]
    fn readers_end_a_turn_only_when_its_launcher_is_proven_gone() {
        let (_s, run) = scratch();
        let reg = crate::supervise::registry::Registered::register(&run, &["a".into()], None, None)
            .unwrap()
            .unwrap();
        let (host, pid, pid_started_at) = launcher();
        let s = Start {
            turn_id: "01a18cb6-0000-7000-8000-000000000004".into(),
            number: 1,
            files: "turn-1".into(),
            started_at: String::new(),
            supervisor_id: Some(reg.id()),
            host: host.clone(),
            pid,
            pid_started_at,
        };
        let entry = |s: &Start| Entry {
            start: s.clone(),
            end: None,
        };
        // The agent idle in the record (a failed Running write), its
        // launcher alive: running.
        let d = derive(&run, "a", &entry(&s), None, launcher_alive);
        assert_eq!((d.state, d.outcome), ("running", None));
        // Another host recorded (this one renamed mid-turn, or none read):
        // the pid and its start time decide, and they match.
        for other in [String::new(), format!("{host}-renamed")] {
            let renamed = Start {
                host: other,
                ..s.clone()
            };
            assert!(launcher_alive(&renamed));
            let d = derive(&run, "a", &entry(&renamed), None, launcher_alive);
            assert_eq!(d.state, "running");
        }
        // The same pid with another start time: gone.
        let reused = Start {
            pid_started_at: Some("Thu Jan  1 00:00:00 1970".into()),
            ..s.clone()
        };
        assert!(!launcher_alive(&reused));
        // A pid that isn't running: gone.
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead = child.id();
        child.wait().unwrap();
        let gone = Start {
            pid: dead,
            pid_started_at: None,
            ..s.clone()
        };
        assert!(!launcher_alive(&gone));
    }

    /// The host name is `uname -n`'s, read on each call (review of
    /// 0e53de0a): no `uname` binary to find, and no name kept from before a
    /// rename.
    #[test]
    fn this_host_is_uname_n_read_each_time() {
        let out = std::process::Command::new("uname")
            .arg("-n")
            .output()
            .unwrap();
        let want = String::from_utf8(out.stdout).unwrap().trim().to_string();
        assert_eq!(crate::turn::this_host().as_deref(), Some(want.as_str()));
        assert_eq!(launcher().0, want);
    }

    #[test]
    fn finalizing_leaves_another_launchers_live_turn_open_for_its_own_end() {
        let (_s, run) = scratch();
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id();
        let s = Start {
            turn_id: "01a18cb6-0000-7000-8000-000000000003".into(),
            number: 1,
            files: "turn-1".into(),
            started_at: String::new(),
            supervisor_id: None,
            host: crate::turn::this_host().expect("this host"),
            pid,
            pid_started_at: crate::turn::started(pid).unwrap(),
        };
        append(&run, "a", &Line::Start(s.clone())).unwrap();
        // Its lease lapsed and another launcher took it: the turn still runs.
        finalize_orphans(&run, "a").unwrap();
        assert!(
            read(&run, "a")
                .unwrap()
                .find(&s.turn_id)
                .unwrap()
                .end
                .is_none()
        );
        // Its own end line, appended when it finishes, is the one kept.
        append(&run, "a", &end(&s.turn_id, "replied")).unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
        finalize_orphans(&run, "a").unwrap();
        let index = read(&run, "a").unwrap();
        assert_eq!(
            index
                .find(&s.turn_id)
                .unwrap()
                .end
                .as_ref()
                .unwrap()
                .outcome,
            "replied"
        );
    }

    #[test]
    fn terminal_evidence_wins_over_a_live_launcher_and_harness_starting_means_interrupted() {
        let (_s, run) = scratch();
        let Line::Start(s) = start("01a18cb6-0000-7000-8000-000000000001", 1) else {
            unreachable!()
        };
        let open = Entry {
            start: s.clone(),
            end: None,
        };
        let ev = |kind: &str, data: Value| journal::Event {
            schema: journal::SCHEMA.into(),
            turn: Some("/v1/agents/a/turns/x".into()),
            seq: Some(1),
            time: "2026-10-30T12:02:00.000Z".into(),
            kind: kind.into(),
            data,
        };
        let started = [ev("turn.started", Value::Null)];
        let spawned = [
            ev("turn.started", Value::Null),
            ev("harness.starting", serde_json::json!({"harness": "claude"})),
        ];
        let ended = [
            ev("turn.started", Value::Null),
            ev("harness.starting", serde_json::json!({"harness": "claude"})),
            ev("turn.ended", serde_json::json!({"outcome": "replied"})),
        ];
        let live = |_: &Start| true;
        let dead = |_: &Start| false;
        assert_eq!(
            derive(&run, "a", &open, Some(&started), live).state,
            "running"
        );
        let d = derive(&run, "a", &open, Some(&ended), live);
        assert_eq!((d.state, d.outcome.as_deref()), ("ended", Some("replied")));
        let d = derive(&run, "a", &open, Some(&spawned), dead);
        assert_eq!(
            (d.state, d.outcome.as_deref()),
            ("interrupted", Some("interrupted"))
        );
        let d = derive(&run, "a", &open, Some(&started), dead);
        assert_eq!(
            (d.state, d.outcome.as_deref()),
            ("ended", Some("not_started"))
        );
        let d = derive(&run, "a", &open, None, dead);
        assert_eq!(d.outcome.as_deref(), Some("not_started"));
        // The end line outranks the journal; the receipt outranks both.
        let closed = Entry {
            start: s.clone(),
            end: Some(End {
                turn_id: s.turn_id.clone(),
                ended_at: "2026-10-30T12:03:00.000Z".into(),
                outcome: "ran".into(),
            }),
        };
        let d = derive(&run, "a", &closed, Some(&ended), live);
        assert_eq!(d.outcome.as_deref(), Some("ran"));
        let receipt = receipt_path(&run, "a", &s.turn_id);
        fs::create_dir_all(receipt.parent().unwrap()).unwrap();
        fs::write(
            &receipt,
            "Advisory\n{\"result\":{\"status\":\"error\",\"code\":\"run.agent_failed\"}}\n",
        )
        .unwrap();
        let d = derive(&run, "a", &closed, None, live);
        assert_eq!(d.outcome.as_deref(), Some("failed"));
    }
}
