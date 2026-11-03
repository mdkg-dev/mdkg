//! Each running `agents up` names itself: a UUIDv7 supervisor id, minted
//! at start and kept across a re-exec, and a record of the agents it runs
//! and the state of each (API v1 A7). The record is
//! `<run dir>/supervisors/<id>.json`, replaced whole on each change, and
//! the supervisor holds a lock on `<id>.lock` beside it for as long as it
//! runs: a record whose lock can be taken belongs to a supervisor that's
//! gone (`stale`), whatever the record says. Readers never write either.
//!
//! One exception: an exec closes the lock, as it closes every descriptor
//! std opens, and the new image takes it again only when it registers. In
//! that window the record's process is still alive, with the start time
//! the record holds, since an exec keeps both; such a record is live too.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Read as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::RunError;
use crate::builds::Build;

const SCHEMA: &str = "mdkg.run.supervisor.v1";
const CAP: u64 = 64 * 1024;

/// Carries this supervisor's id and the agents it stopped across a
/// re-exec, bound to its pid as the exec token is.
pub const ENV: &str = "MDKG_SUPERVISOR";

/// One agent's state in its supervisor (A7's values a supervisor knows;
/// `draining` is a stop request the supervisor hasn't acted on yet, and
/// `paused` the loop guard's, both read from elsewhere).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    /// Waiting for mail.
    Idle,
    /// A turn is running.
    Running,
    /// Drained by `agents stop`: it admits no more turns.
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub schema: String,
    pub supervisor_id: String,
    pub pid: u32,
    /// The process's start time as `ps` prints it, which an exec keeps, so
    /// a reused pid never passes for this supervisor. Older records lack it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid_started_at: Option<String>,
    /// When the supervisor started, in ms since the epoch; a re-exec keeps it.
    pub started_ms: u64,
    pub build: Option<Build>,
    pub agents: BTreeMap<String, AgentState>,
}

/// What the supervisor hands its re-exec ([`ENV`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Inherited {
    pid: u32,
    supervisor_id: String,
    #[serde(default)]
    stopped: Vec<String>,
}

/// The id and stopped agents this process was re-execed with, if any. One
/// for another pid, or one that doesn't parse, is ignored.
pub fn inherited() -> Option<(String, Vec<String>)> {
    let value = std::env::var_os(ENV)?;
    parse_inherited(value.to_str()?, std::process::id())
}

fn parse_inherited(value: &str, pid: u32) -> Option<(String, Vec<String>)> {
    let inherited: Inherited = serde_json::from_str(value).ok()?;
    (inherited.pid == pid && uuid::Uuid::parse_str(&inherited.supervisor_id).is_ok())
        .then_some((inherited.supervisor_id, inherited.stopped))
}

/// The value of [`ENV`] for this process's re-exec.
pub fn handoff(supervisor_id: &str, stopped: &[String]) -> String {
    serde_json::to_string(&Inherited {
        pid: std::process::id(),
        supervisor_id: supervisor_id.into(),
        stopped: stopped.to_vec(),
    })
    .unwrap_or_default()
}

/// A registered supervisor. Dropping it removes its record and lock; an
/// exec, which runs no Drop, leaves both for the next image to take over.
#[derive(Debug)]
pub struct Registered {
    lock: File,
    dir: PathBuf,
    record: Mutex<Record>,
}

impl Drop for Registered {
    fn drop(&mut self) {
        let id = self.id().to_string();
        fs::remove_file(self.dir.join(format!("{id}.json"))).ok();
        fs::remove_file(self.dir.join(format!("{id}.lock"))).ok();
        let _ = self.lock.unlock();
    }
}

impl Registered {
    /// Registers this process for `agents`, under the id it inherited
    /// across a re-exec, else a new one. Records left by supervisors that
    /// are gone are removed first. A missing store directory returns
    /// `None` without creating it.
    pub fn register(
        run_dir: &Path,
        agents: &[String],
        build: Option<Build>,
        inherited: Option<&str>,
    ) -> Result<Option<Self>, RunError> {
        let Some(dir) = super::files::folder(run_dir, "supervisors", true)? else {
            return Ok(None);
        };
        let id = inherited
            .map(str::to_string)
            .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
        let earlier = read_record(&dir.join(format!("{id}.json"))).ok();
        prune(&dir, &id);
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(super::files::flags())
            .open(dir.join(format!("{id}.lock")))
            .map_err(|e| RunError::io("can't open the supervisor record lock", e))?;
        lock.try_lock().map_err(|e| {
            RunError::failure(
                "io.write_failed",
                format!("can't lock the supervisor record: {e}"),
            )
        })?;
        let started_ms = earlier
            .filter(|r| r.supervisor_id == id)
            .map_or_else(crate::state::now_ms, |r| r.started_ms);
        let record = Record {
            schema: SCHEMA.into(),
            supervisor_id: id,
            pid: std::process::id(),
            pid_started_at: crate::turn::started(std::process::id()).ok().flatten(),
            started_ms,
            build,
            agents: agents
                .iter()
                .map(|a| (a.clone(), AgentState::Idle))
                .collect(),
        };
        let registered = Self {
            lock,
            dir,
            record: Mutex::new(record.clone()),
        };
        registered.write(&record)?;
        Ok(Some(registered))
    }

    pub fn id(&self) -> String {
        self.record
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .supervisor_id
            .clone()
    }

    pub fn run_dir(&self) -> &Path {
        // `<run dir>/supervisors`
        self.dir.parent().unwrap_or(&self.dir)
    }

    /// The agents drained so far.
    pub fn stopped(&self) -> Vec<String> {
        self.record
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .agents
            .iter()
            .filter(|(_, s)| **s == AgentState::Stopped)
            .map(|(a, _)| a.clone())
            .collect()
    }

    /// Records `agent`'s state. A stopped agent stays stopped.
    pub fn set(&self, agent: &str, state: AgentState) -> Result<(), RunError> {
        let mut record = self.record.lock().unwrap_or_else(|e| e.into_inner());
        match record.agents.get(agent) {
            Some(AgentState::Stopped) => return Ok(()),
            Some(s) if *s == state => return Ok(()),
            _ => (),
        }
        record.agents.insert(agent.into(), state);
        self.write(&record)
    }

    fn write(&self, record: &Record) -> Result<(), RunError> {
        let bytes = serde_json::to_vec(record)
            .map_err(|e| RunError::failure("io.write_failed", e.to_string()))?;
        super::files::replace(
            &self.dir.join(format!("{}.json", record.supervisor_id)),
            &bytes,
        )
        .map_err(|e| RunError::io("can't write the supervisor record", e))
    }
}

/// Removes the records and locks of supervisors that are gone, except
/// `keep`'s, which this process is about to take over.
fn prune(dir: &Path, keep: &str) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(id) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".json"))
        else {
            continue;
        };
        if id == keep || uuid::Uuid::parse_str(id).is_err() {
            continue;
        }
        if live_lock(&dir.join(format!("{id}.lock"))) == Some(false)
            && read_record(&path).map_or(Some(false), |r| re_execing(&r)) == Some(false)
        {
            fs::remove_file(&path).ok();
            fs::remove_file(dir.join(format!("{id}.lock"))).ok();
        }
    }
}

/// Whether a record whose lock is free belongs to a supervisor in the
/// middle of a re-exec: its process alive, with the start time it
/// recorded. `None` when `ps` can't tell.
fn re_execing(record: &Record) -> Option<bool> {
    let Some(recorded) = &record.pid_started_at else {
        return Some(false);
    };
    match crate::turn::started(record.pid) {
        Ok(now) => Some(now.as_ref() == Some(recorded)),
        Err(_) => None,
    }
}

/// Whether the lock is held: `None` when it can't be told.
fn live_lock(path: &Path) -> Option<bool> {
    let lock = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(super::files::flags())
        .open(path)
    {
        Ok(lock) => lock,
        // A record without its lock: its supervisor dropped it, or never took it.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Some(false),
        Err(_) => return None,
    };
    match lock.try_lock_shared() {
        Ok(()) => Some(false),
        Err(fs::TryLockError::WouldBlock) => Some(true),
        Err(fs::TryLockError::Error(_)) => None,
    }
}

fn read_record(path: &Path) -> Result<Record, RunError> {
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(super::files::flags())
        .open(path)
        .map_err(|e| RunError::io("can't open a supervisor record", e))?;
    let m = file
        .metadata()
        .map_err(|e| RunError::io("can't inspect a supervisor record", e))?;
    if !m.is_file()
        || m.len() > CAP
        || m.uid() != rustix::process::geteuid().as_raw()
        || m.mode() & 0o022 != 0
    {
        return Err(RunError::failure(
            "io.read_failed",
            "a supervisor record isn't an owned, protected regular file of at most 64 KiB",
        ));
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take(CAP + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| RunError::io("can't read a supervisor record", e))?;
    let record: Record = serde_json::from_slice(&bytes)
        .map_err(|_| RunError::failure("io.read_failed", "invalid supervisor record"))?;
    if record.schema != SCHEMA {
        return Err(RunError::failure(
            "io.read_failed",
            "unknown supervisor record schema",
        ));
    }
    Ok(record)
}

/// A supervisor record as a reader finds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub record: Record,
    /// Its supervisor still holds the record's lock, or is re-execing.
    pub live: bool,
}

/// Every supervisor record, oldest first, live or stale. Read-only: stale
/// records are left for the next supervisor to remove. Unreadable records
/// are returned as problems.
pub fn list(run_dir: &Path) -> Result<(Vec<Found>, Vec<String>), RunError> {
    let Some(dir) = super::files::folder(run_dir, "supervisors", false)? else {
        return Ok((Vec::new(), Vec::new()));
    };
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok((Vec::new(), Vec::new()));
        }
        Err(e) => return Err(RunError::io("can't list supervisor records", e)),
    };
    let mut ids: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            e.file_name()
                .to_str()
                .and_then(|n| n.strip_suffix(".json"))
                .filter(|id| uuid::Uuid::parse_str(id).is_ok())
                .map(str::to_string)
        })
        .collect();
    // UUIDv7s sort by their start time.
    ids.sort();
    let mut found = Vec::new();
    let mut problems = Vec::new();
    for id in ids {
        // The lock first: a supervisor that ends between the two reads
        // removes its record, and a record read after its lock reads as live
        // only if it was live at the lock check.
        let live = live_lock(&dir.join(format!("{id}.lock")));
        match read_record(&dir.join(format!("{id}.json"))) {
            Ok(record) if record.supervisor_id == id => found.push(Found {
                live: live == Some(true)
                    || (live == Some(false) && re_execing(&record) == Some(true)),
                record,
            }),
            Ok(_) => problems.push(format!("supervisor record {id} names another id")),
            Err(e) if e.code == "io.not_found" => (),
            Err(e) => problems.push(format!("supervisor record {id}: {}", e.message)),
        }
    }
    Ok((found, problems))
}

/// The live supervisor running `agent`, with its state there.
pub fn running(found: &[Found], agent: &str) -> Option<(Record, AgentState)> {
    found.iter().rev().filter(|f| f.live).find_map(|f| {
        f.record
            .agents
            .get(agent)
            .map(|state| (f.record.clone(), *state))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_dir() -> (super::super::files::Scratch, PathBuf) {
        let scratch = super::super::files::Scratch::new("supervisor-registry");
        let run = scratch.run_dir();
        (scratch, run)
    }

    #[test]
    fn a_registered_supervisor_is_live_and_its_states_are_read_back() {
        let (_scratch, run) = run_dir();
        let agents = vec!["alice".to_string(), "bob".to_string()];
        let reg = Registered::register(&run, &agents, None, None)
            .unwrap()
            .unwrap();
        reg.set("alice", AgentState::Running).unwrap();
        let (found, problems) = list(&run).unwrap();
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(found.len(), 1);
        assert!(found[0].live);
        assert_eq!(found[0].record.supervisor_id, reg.id());
        assert_eq!(found[0].record.agents["alice"], AgentState::Running);
        let (record, state) = running(&found, "bob").unwrap();
        assert_eq!(record.pid, std::process::id());
        assert_eq!(state, AgentState::Idle);
        assert!(running(&found, "carol").is_none());
        // Stopped is final.
        reg.set("bob", AgentState::Stopped).unwrap();
        reg.set("bob", AgentState::Running).unwrap();
        assert_eq!(reg.stopped(), vec!["bob".to_string()]);
        drop(reg);
        assert!(list(&run).unwrap().0.is_empty());
    }

    /// Leaves `reg`'s files as a crash would: lock released, and the
    /// record naming a process that's gone (a reused pid, here).
    fn crash(reg: Registered) {
        let mut record = reg.record.lock().unwrap().clone();
        record.pid_started_at = Some("Thu Jan  1 00:00:00 1970".into());
        reg.write(&record).unwrap();
        reg.lock.unlock().unwrap();
        std::mem::forget(reg);
    }

    #[test]
    fn a_record_whose_lock_is_free_is_stale_and_the_next_supervisor_prunes_it() {
        let (_scratch, run) = run_dir();
        let reg = Registered::register(&run, &["alice".into()], None, None)
            .unwrap()
            .unwrap();
        let id = reg.id();
        crash(reg);
        let (found, _) = list(&run).unwrap();
        assert_eq!(found.len(), 1);
        assert!(!found[0].live);
        assert!(running(&found, "alice").is_none());
        let next = Registered::register(&run, &["alice".into()], None, None)
            .unwrap()
            .unwrap();
        let (found, _) = list(&run).unwrap();
        assert_eq!(found.len(), 1);
        assert_ne!(found[0].record.supervisor_id, id);
        drop(next);
    }

    #[test]
    fn a_supervisor_in_the_middle_of_a_re_exec_stays_live_and_is_never_pruned() {
        let (_scratch, run) = run_dir();
        let reg = Registered::register(&run, &["alice".into()], None, None)
            .unwrap()
            .unwrap();
        let id = reg.id();
        assert!(reg.record.lock().unwrap().pid_started_at.is_some());
        // As an exec leaves it: files kept, lock closed, the same process.
        reg.lock.unlock().unwrap();
        std::mem::forget(reg);
        let (found, _) = list(&run).unwrap();
        assert!(found[0].live);
        assert_eq!(running(&found, "alice").unwrap().0.supervisor_id, id);
        // Another supervisor starting in that window leaves it alone.
        let other = Registered::register(&run, &["bob".into()], None, None)
            .unwrap()
            .unwrap();
        let (found, _) = list(&run).unwrap();
        assert_eq!(found.len(), 2);
        assert!(found.iter().all(|f| f.live));
        drop(other);
        // A record from before start times were kept reads as before.
        let path = run.join("supervisors").join(format!("{id}.json"));
        let mut record = read_record(&path).unwrap();
        record.pid_started_at = None;
        super::super::files::replace(&path, &serde_json::to_vec(&record).unwrap()).unwrap();
        assert!(!list(&run).unwrap().0[0].live);
    }

    #[test]
    fn a_re_exec_keeps_its_id_and_start_time() {
        let (_scratch, run) = run_dir();
        let reg = Registered::register(&run, &["alice".into()], None, None)
            .unwrap()
            .unwrap();
        let id = reg.id();
        let started = reg.record.lock().unwrap().started_ms;
        reg.lock.unlock().unwrap();
        std::mem::forget(reg);
        let value = handoff(&id, &["bob".to_string()]);
        let (inherited, stopped) = parse_inherited(&value, std::process::id()).unwrap();
        assert_eq!(stopped, vec!["bob".to_string()]);
        assert!(parse_inherited(&value, std::process::id() + 1).is_none());
        assert!(parse_inherited("{", std::process::id()).is_none());
        let next = Registered::register(&run, &["alice".into()], None, Some(&inherited))
            .unwrap()
            .unwrap();
        assert_eq!(next.id(), id);
        assert_eq!(next.record.lock().unwrap().started_ms, started);
        let (found, _) = list(&run).unwrap();
        assert_eq!(found.len(), 1);
        assert!(found[0].live);
    }
}
