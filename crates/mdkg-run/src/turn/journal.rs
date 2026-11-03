//! A turn attempt's output journal (API v1 A11):
//! `<run dir>/<agent>/journals/<turn_id>.events.jsonl`, mode 0600,
//! internal. One per indexed attempt, created exclusively before its D6
//! admission holding `turn.started`; `harness.starting` just before the
//! spawn; the turn's output events as the launcher reads them; and
//! `turn.ended`. Each event is a `mdkg.api.turn_output.v1` with the next
//! `seq`, assigned once here, so every reader sees the same events in the
//! same order with the same `seq`. The lifecycle events are synced before
//! the next step; output events are written as they come.
//!
//! Retention: when a turn starts, journals beyond the agent's last 3
//! turns are deleted, but only those of turns with a terminal record (an
//! end line or a receipt). A turn without one keeps its journal.

use std::fs::{self, File};
use std::io::{Read as _, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::index::{self, Index};
use crate::RunError;
use crate::state;

pub const SCHEMA: &str = "mdkg.api.turn_output.v1";
/// Turns whose journals are kept, newest first, whatever their state.
const RETAINED: usize = 3;
/// The most of one journal read.
const CAP: u64 = 256 << 20;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub schema: String,
    /// The turn's API path; null only for an agent's `agent.state`.
    pub turn: Option<String>,
    /// From 1 within the turn; null only for `agent.state`.
    pub seq: Option<u64>,
    pub time: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub data: Value,
}

/// A path segment, percent-encoded outside RFC 3986's unreserved set (A2).
pub fn segment(text: &str) -> String {
    let mut out = String::new();
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `/v1/agents/{name}/turns/{turn_id}`.
pub fn turn_uri(agent: &str, turn_id: &str) -> String {
    format!("/v1/agents/{}/turns/{}", segment(agent), segment(turn_id))
}

pub fn folder(run_dir: &Path, agent: &str) -> PathBuf {
    state::turns_dir(run_dir, agent).join("journals")
}

pub fn path(run_dir: &Path, agent: &str, turn_id: &str) -> PathBuf {
    folder(run_dir, agent).join(format!("{turn_id}.events.jsonl"))
}

/// Writes one attempt's journal; only the turn's launcher holds one.
#[derive(Debug)]
pub struct Writer {
    file: File,
    turn: String,
    seq: u64,
    /// A write failed partway, so the file may end in a fragment.
    torn: bool,
    harness_started: bool,
    ended: bool,
    /// Tests: the next write puts this many bytes, then fails.
    #[cfg(test)]
    fail_after: Option<usize>,
}

fn io(e: std::io::Error) -> RunError {
    RunError::io("can't write the turn's output journal", e)
}

impl Writer {
    /// Creates the journal, holding `turn.started`, synced. An existing
    /// journal for this id is a bug that refuses the turn, never an
    /// overwrite.
    pub fn create(
        run_dir: &Path,
        agent: &str,
        turn_id: &str,
        number: u64,
    ) -> Result<Self, RunError> {
        state::check_name(agent)?;
        uuid::Uuid::parse_str(turn_id)
            .map_err(|_| RunError::failure("run.agent_failed", "invalid turn id"))?;
        let dir = folder(run_dir, agent);
        index::create_folder(&dir).map_err(io)?;
        let file = fs::OpenOptions::new()
            .append(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open(path(run_dir, agent, turn_id))
            .map_err(io)?;
        File::open(&dir).and_then(|d| d.sync_all()).map_err(io)?;
        let mut writer = Self {
            file,
            turn: turn_uri(agent, turn_id),
            seq: 0,
            torn: false,
            harness_started: false,
            ended: false,
            #[cfg(test)]
            fail_after: None,
        };
        writer.write(
            "turn.started",
            json!({"turn_id": turn_id, "number": number}),
            true,
        )?;
        Ok(writer)
    }

    /// Appends an event with the next `seq`, returning it; `sync` waits
    /// for it to reach the disk.
    ///
    /// A write that fails partway leaves a fragment, so the next write
    /// starts with a newline: the fragment becomes a line of its own that
    /// every reader drops, and the events after it, `turn.ended` among
    /// them, stay whole. The failed write keeps its `seq`, so no number is
    /// ever used twice, even when the fragment was the whole event.
    pub fn write(&mut self, kind: &str, data: Value, sync: bool) -> Result<u64, RunError> {
        let event = Event {
            schema: SCHEMA.into(),
            turn: Some(self.turn.clone()),
            seq: Some(self.seq + 1),
            time: crate::supervise::timestamp(state::now_ms()),
            kind: kind.into(),
            data,
        };
        let mut line = Vec::new();
        if self.torn {
            line.push(b'\n');
        }
        serde_json::to_writer(&mut line, &event)
            .map_err(|e| RunError::failure("io.write_failed", e.to_string()))?;
        line.push(b'\n');
        self.seq += 1;
        if let Err(e) = self.put(&line) {
            self.torn = true;
            return Err(io(e));
        }
        self.torn = false;
        if sync {
            self.file.sync_all().map_err(io)?;
        }
        Ok(self.seq)
    }

    fn put(&mut self, line: &[u8]) -> std::io::Result<()> {
        #[cfg(test)]
        if let Some(n) = self.fail_after.take() {
            self.file.write_all(&line[..n.min(line.len())])?;
            return Err(std::io::Error::other("injected write failure"));
        }
        self.file.write_all(line)
    }

    /// `harness.starting`, synced: from here the harness may have run.
    pub fn harness_starting(&mut self, harness: crate::Harness) -> Result<(), RunError> {
        self.write("harness.starting", json!({"harness": harness}), true)?;
        self.harness_started = true;
        Ok(())
    }

    pub fn harness_started(&self) -> bool {
        self.harness_started
    }

    /// `turn.ended`, synced; once.
    pub fn ended(&mut self, outcome: &str) -> Result<(), RunError> {
        if self.ended {
            return Ok(());
        }
        self.write("turn.ended", json!({"outcome": outcome}), true)?;
        self.ended = true;
        Ok(())
    }
}

/// The committed events of a journal; `None` when it's gone.
pub fn read(run_dir: &Path, agent: &str, turn_id: &str) -> std::io::Result<Option<Vec<Event>>> {
    // A turn id is a UUID, never a path; anything else has no journal.
    if uuid::Uuid::parse_str(turn_id).is_err() {
        return Ok(None);
    }
    let mut file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(path(run_dir, agent, turn_id))
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut bytes = Vec::new();
    (&mut file).take(CAP).read_to_end(&mut bytes)?;
    Ok(Some(parse(&bytes)))
}

/// Reads a journal as it grows: each call returns the events committed
/// since the last.
#[derive(Debug)]
pub struct Tail {
    path: PathBuf,
    /// The turn id is a UUID; anything else has no journal.
    valid: bool,
    file: Option<File>,
    offset: u64,
    pending: Vec<u8>,
}

impl Tail {
    pub fn new(run_dir: &Path, agent: &str, turn_id: &str) -> Self {
        Self {
            path: path(run_dir, agent, turn_id),
            valid: uuid::Uuid::parse_str(turn_id).is_ok(),
            file: None,
            offset: 0,
            pending: Vec::new(),
        }
    }

    /// The new committed events; `None` when the journal is gone (never
    /// created, or removed by retention).
    pub fn read_new(&mut self) -> std::io::Result<Option<Vec<Event>>> {
        use std::io::{Seek as _, SeekFrom};
        if !self.valid {
            return Ok(None);
        }
        if self.file.is_none() {
            match fs::OpenOptions::new()
                .read(true)
                .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
                .open(&self.path)
            {
                Ok(file) => self.file = Some(file),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(e),
            }
        }
        let file = self.file.as_mut().expect("opened above");
        file.seek(SeekFrom::Start(self.offset))?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)?;
        self.offset += buf.len() as u64;
        self.pending.extend_from_slice(&buf);
        let end = self
            .pending
            .iter()
            .rposition(|b| *b == b'\n')
            .map_or(0, |i| i + 1);
        let complete: Vec<u8> = self.pending.drain(..end).collect();
        Ok(Some(parse(&complete)))
    }
}

/// Newline-terminated lines only: a tail without its newline is still
/// being written.
pub fn parse(bytes: &[u8]) -> Vec<Event> {
    let end = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
    bytes[..end]
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
        .filter_map(|l| serde_json::from_slice(l).ok())
        .collect()
}

/// Deletes the journals beyond the agent's last [`RETAINED`] turns whose
/// turns have a terminal record (an end line or a receipt), and those of
/// turns the index no longer holds. Best effort: a journal left behind is
/// removed next time.
pub fn retain(run_dir: &Path, agent: &str, index: &Index) {
    let Ok(entries) = fs::read_dir(folder(run_dir, agent)) else {
        return;
    };
    let mut numbers: Vec<u64> = index.entries.iter().map(|e| e.start.number).collect();
    numbers.sort_unstable_by(|a, b| b.cmp(a));
    let newest: Vec<u64> = numbers.into_iter().take(RETAINED).collect();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(id) = name.to_str().and_then(|n| n.strip_suffix(".events.jsonl")) else {
            continue;
        };
        let keep = match index.find(id) {
            Some(turn) => {
                newest.contains(&turn.start.number)
                    || (turn.end.is_none() && !index::receipt_path(run_dir, agent, id).exists())
            }
            None => false,
        };
        if !keep {
            fs::remove_file(entry.path()).ok();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turn::index::{append, read as read_index};

    #[test]
    fn a_journal_numbers_its_events_once_and_is_never_overwritten() {
        let scratch = crate::supervise::Scratch::new("turn-journal");
        let run = scratch.run_dir();
        let id = uuid::Uuid::now_v7().to_string();
        let mut w = Writer::create(&run, "codex:pr19", &id, 4).unwrap();
        w.harness_starting(crate::Harness::Claude).unwrap();
        assert_eq!(w.write("text", json!({"text": "hi"}), false).unwrap(), 3);
        w.ended("replied").unwrap();
        w.ended("replied").unwrap();
        let events = read(&run, "codex:pr19", &id).unwrap().unwrap();
        let kinds: Vec<_> = events.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(
            kinds,
            ["turn.started", "harness.starting", "text", "turn.ended"]
        );
        let seqs: Vec<_> = events.iter().map(|e| e.seq.unwrap()).collect();
        assert_eq!(seqs, [1, 2, 3, 4]);
        assert_eq!(
            events[0].turn.as_deref(),
            Some(format!("/v1/agents/codex%3Apr19/turns/{id}").as_str())
        );
        assert_eq!(events[0].data, json!({"turn_id": id, "number": 4}));
        assert!(Writer::create(&run, "codex:pr19", &id, 4).is_err());
        // A torn tail isn't an event yet.
        let mut bytes = fs::read(path(&run, "codex:pr19", &id)).unwrap();
        bytes.extend_from_slice(b"{\"schema\"");
        assert_eq!(parse(&bytes).len(), 4);
        assert_eq!(read(&run, "codex:pr19", "gone").unwrap(), None);
    }

    #[test]
    fn retention_keeps_the_last_three_and_any_turn_without_a_terminal_record() {
        let scratch = crate::supervise::Scratch::new("turn-journal-retain");
        let run = scratch.run_dir();
        let ids: Vec<String> = (0..6).map(|_| uuid::Uuid::now_v7().to_string()).collect();
        for (n, id) in ids.iter().enumerate() {
            Writer::create(&run, "a", id, n as u64 + 1).unwrap();
            append(
                &run,
                "a",
                &crate::turn::index::tests::start(id, n as u64 + 1),
            )
            .unwrap();
            // The second turn is an orphan: no end line, no receipt.
            if n != 1 {
                append(&run, "a", &crate::turn::index::tests::end(id, "replied")).unwrap();
            }
        }
        retain(&run, "a", &read_index(&run, "a").unwrap());
        let kept: Vec<bool> = ids.iter().map(|id| path(&run, "a", id).exists()).collect();
        assert_eq!(kept, [false, true, false, true, true, true]);
    }

    #[test]
    fn a_write_that_fails_partway_never_takes_the_next_event_with_it() {
        let scratch = crate::supervise::Scratch::new("turn-journal-torn");
        let run = scratch.run_dir();
        let id = uuid::Uuid::now_v7().to_string();
        let mut w = Writer::create(&run, "a", &id, 1).unwrap();
        w.harness_starting(crate::Harness::Codex).unwrap();
        // A fragment of an event, then a failure.
        w.fail_after = Some(20);
        assert!(w.write("text", json!({"text": "lost"}), false).is_err());
        // Even a failure after the whole event but before its newline.
        let whole = serde_json::to_vec(&Event {
            schema: SCHEMA.into(),
            turn: Some(turn_uri("a", &id)),
            seq: Some(4),
            time: crate::supervise::timestamp(state::now_ms()),
            kind: "text".into(),
            data: json!({"text": "kept"}),
        })
        .unwrap();
        // It starts with the newline that ends the first fragment.
        w.fail_after = Some(1 + whole.len());
        assert!(w.write("text", json!({"text": "kept"}), false).is_err());
        w.ended("replied").unwrap();
        let events = read(&run, "a", &id).unwrap().unwrap();
        let got: Vec<_> = events
            .iter()
            .map(|e| (e.seq.unwrap(), e.kind.as_str()))
            .collect();
        // The first fragment is dropped; no seq is used twice; turn.ended
        // survives.
        assert_eq!(
            got,
            [
                (1, "turn.started"),
                (2, "harness.starting"),
                (4, "text"),
                (5, "turn.ended")
            ]
        );
        // A follower that read the fragment sees the same.
        let mut tail = Tail::new(&run, "a", &id);
        assert_eq!(tail.read_new().unwrap().unwrap(), events);
    }
}
