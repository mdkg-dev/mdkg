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
/// A record's size limit: room for thousands of long agent names, so a
/// valid supervisor's record is never discarded as too large.
const CAP: u64 = 1024 * 1024;

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
    /// Whether this supervisor's build starts turns with the policy's
    /// environment (0.7.2 and later): it re-execs at a turn boundary when
    /// the config changes, so its agents run with the configured mode. A
    /// live record without it is an older build's, whose turns inherit the
    /// launcher's whole environment.
    #[serde(default)]
    pub env_tracked: bool,
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
            env_tracked: true,
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

/// Why a supervisor ended, kept per agent once its process is gone, so a
/// client that doesn't hold its output can still tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExitReason {
    /// It ran its `--max-turns`.
    Cap,
    /// Its trigger is `manual`: a watch runs it no turns.
    Manual,
    /// Its receiver mode changed (interactive to headless or back): start
    /// it again to run it in its new mode.
    ModeChanged,
    /// `--once`: what was waiting ran.
    Once,
    /// Drained by `agents stop`.
    Drained,
    /// Stopped by SIGTERM, its turns finished.
    Signal,
    /// It failed; `code` says why.
    Failed,
}

const EXIT_SCHEMA: &str = "mdkg.run.supervisor_exit.v1";
const EXITS: &str = "supervisor-exits";

/// How an agent's last supervisor ended: `<run dir>/supervisor-exits/
/// <agent>.json`, written by the holder of the agent's progress lock as
/// it lets go, naming that lock's watch. It counts only while the agent's
/// progress names the same watch, finished ([`current_exit`]): a later
/// run starts or resumes a watch and so hides it, with nothing to clear.
/// A supervisor killed outright (SIGKILL, a crash) writes none, so the
/// one there is older or absent: unknown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Exit {
    pub schema: String,
    pub supervisor_id: String,
    pub reason: ExitReason,
    /// The failure's diagnostic code, with `failed` only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// When it exited, in ms since the epoch.
    pub at_ms: u64,
    /// The watch it ended: its progress record's `watch_id`. A record
    /// without one (from before #20977) never counts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watch_id: Option<String>,
}

/// `agent`'s exit record in `dir`. A name that isn't an agent's (a path,
/// `..`, an absolute name from a forged handoff) is refused, so a record
/// is only ever written inside `dir` (#20977).
fn exit_path(dir: &Path, agent: &str) -> Result<PathBuf, RunError> {
    crate::state::check_name(agent)?;
    Ok(dir.join(format!("{}.json", crate::state::file_name(agent))))
}

/// Records why supervisor `supervisor_id` is letting go of the agent
/// whose progress lock is `held`, naming that lock's watch (#20977).
/// Only a holder writes: a released lock writes nothing, so a run that
/// let the agent go never overwrites a later one's reason. Best effort:
/// a failure is the caller's warning, never its exit code.
pub fn record_exit(
    run_dir: &Path,
    supervisor_id: &str,
    held: &super::progress::Locked,
    reason: ExitReason,
    code: Option<&str>,
) -> Result<(), RunError> {
    if held.released() {
        return Ok(());
    }
    let Some(record) = held.record() else {
        return Ok(());
    };
    let Some(dir) = super::files::folder(run_dir, EXITS, true)? else {
        return Ok(());
    };
    let path = exit_path(&dir, &record.agent)?;
    let exit = Exit {
        schema: EXIT_SCHEMA.into(),
        supervisor_id: supervisor_id.into(),
        reason,
        code: code.map(str::to_string),
        at_ms: crate::state::now_ms(),
        watch_id: Some(record.watch_id.clone()),
    };
    let bytes = serde_json::to_vec(&exit)
        .map_err(|e| RunError::failure("io.write_failed", e.to_string()))?;
    super::files::replace(&path, &bytes)
        .map_err(|e| RunError::io("can't record why the supervisor exited", e))
}

/// Whether a live supervisor runs `agent` and hasn't drained it: then its
/// last exit isn't shown, whatever older supervisor still names it.
pub fn holds(found: &[Found], agent: &str) -> bool {
    found.iter().any(|f| {
        f.live
            && f.record
                .agents
                .get(agent)
                .is_some_and(|s| *s != AgentState::Stopped)
    })
}

/// `agent`'s exit record as it is, if one is readable. Unreadable or
/// foreign records read as none: the reason is then unknown.
/// [`current_exit`] decides whether it still counts.
pub fn last_exit(run_dir: &Path, agent: &str) -> Option<Exit> {
    let dir = super::files::folder(run_dir, EXITS, false).ok()??;
    let bytes = read_owned(&exit_path(&dir, agent).ok()?, "a supervisor exit record").ok()?;
    serde_json::from_slice::<Exit>(&bytes)
        .ok()
        .filter(|e| e.schema == EXIT_SCHEMA)
}

/// How `agent`'s last run ended, when that's still the news: its exit
/// record names the watch `progress` (the agent's current progress
/// record) names, and that watch is finished. A later run that started
/// or resumed a watch, a record from before watches were named, or none
/// at all: `None`, never a guess.
pub fn current_exit(
    run_dir: &Path,
    agent: &str,
    progress: Option<&super::progress::Record>,
) -> Option<Exit> {
    let progress = progress.filter(|p| p.finished && p.agent == agent)?;
    last_exit(run_dir, agent).filter(|e| e.watch_id.as_deref() == Some(progress.watch_id.as_str()))
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

/// An owned, protected regular file of at most [`CAP`], read whole.
fn read_owned(path: &Path, what: &str) -> Result<Vec<u8>, RunError> {
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(super::files::flags())
        .open(path)
        .map_err(|e| RunError::io(&format!("can't open {what}"), e))?;
    let m = file
        .metadata()
        .map_err(|e| RunError::io(&format!("can't inspect {what}"), e))?;
    if !m.is_file()
        || m.len() > CAP
        || m.uid() != rustix::process::geteuid().as_raw()
        || m.mode() & 0o022 != 0
    {
        return Err(RunError::failure(
            "io.read_failed",
            format!("{what} isn't an owned, protected regular file of at most 1 MiB"),
        ));
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take(CAP + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| RunError::io(&format!("can't read {what}"), e))?;
    Ok(bytes)
}

fn read_record(path: &Path) -> Result<Record, RunError> {
    let bytes = read_owned(path, "a supervisor record")?;
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

/// The agents run by a live supervisor whose turns may inherit the
/// launcher's whole environment whatever the config says, because nothing
/// confirms it is a 0.7.2 or later build. Two pieces of evidence:
/// - a live supervisor record without `env_tracked` (0.7.1);
/// - a live launcher's process build receipt (`agents up` writes one
///   from 0.7.0 on) with no live `env_tracked` record of the same pid:
///   0.7.0, which keeps no record, or any build whose registration
///   failed.
///
/// Sorted, each name once. Names only.
pub fn unconfirmed_env(found: &[Found], processes: &[crate::builds::Process]) -> Vec<String> {
    let tracked = |pid: u32| {
        found
            .iter()
            .any(|f| f.live && f.record.env_tracked && f.record.pid == pid)
    };
    let mut names: Vec<String> = found
        .iter()
        .filter(|f| f.live && !f.record.env_tracked)
        .flat_map(|f| f.record.agents.keys().cloned())
        .chain(
            processes
                .iter()
                .filter(|p| p.kind == crate::builds::Kind::Launcher && !tracked(p.pid))
                .flat_map(|p| p.agents.iter().cloned()),
        )
        .collect();
    names.sort();
    names.dedup();
    names
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

    /// An exit record names its progress lock's watch and counts only
    /// while the agent's progress names that watch, finished: a later
    /// run's start or resume hides it with nothing to clear, a released
    /// lock writes nothing, a name that isn't an agent's is refused, and a
    /// record from before watches were named, or one that isn't the user's
    /// protected file, reads as none (#20587, #20921, #20977).
    #[test]
    fn an_exit_counts_only_for_the_finished_watch_it_names() {
        use super::super::progress::Locked;
        let (_scratch, run) = run_dir();
        let graph = uuid::Uuid::now_v7();
        let inspect = |agent: &str| super::super::progress::inspect(&run, graph, agent).unwrap();
        let shown = |agent: &str| current_exit(&run, agent, inspect(agent).as_ref());

        // A run ends with a cap: shown once its watch is finished.
        let mut alice = Locked::open(&run, graph, "alice").unwrap();
        alice.start(&"a".repeat(64), None).unwrap();
        let first = alice.record().unwrap().watch_id.clone();
        record_exit(&run, "sup-1", &alice, ExitReason::Cap, None).unwrap();
        assert!(shown("alice").is_none(), "not while its watch runs");
        alice.finish().unwrap();
        let exit = shown("alice").expect("cap shown");
        assert_eq!(exit.reason, ExitReason::Cap);
        assert_eq!(exit.watch_id.as_deref(), Some(first.as_str()));
        assert_eq!(exit.supervisor_id, "sup-1");
        drop(alice);

        // A later run starts a new watch: the old reason is hidden, though
        // nothing cleared its file; then that run's own reason shows.
        let mut alice = Locked::open(&run, graph, "alice").unwrap();
        alice.start(&"a".repeat(64), None).unwrap();
        assert_ne!(alice.record().unwrap().watch_id, first);
        assert!(
            shown("alice").is_none(),
            "an older watch's reason is hidden"
        );
        assert!(
            last_exit(&run, "alice").is_some(),
            "its file is still there"
        );
        // Released (drained), then a write by that run is a no-op.
        record_exit(&run, "sup-2", &alice, ExitReason::Drained, None).unwrap();
        alice.release().unwrap();
        record_exit(&run, "sup-2", &alice, ExitReason::Failed, Some("x.y")).unwrap();
        let exit = shown("alice").expect("drained shown");
        assert_eq!(
            (exit.reason, exit.supervisor_id.as_str()),
            (ExitReason::Drained, "sup-2")
        );
        drop(alice);

        // A failure carries its code; a manual trigger is its own reason.
        let mut bob = Locked::open(&run, graph, "b:ob").unwrap();
        bob.start(&"b".repeat(64), None).unwrap();
        record_exit(
            &run,
            "sup-3",
            &bob,
            ExitReason::Failed,
            Some("coord.store_failed"),
        )
        .unwrap();
        bob.finish().unwrap();
        let exit = shown("b:ob").unwrap();
        assert_eq!(exit.reason, ExitReason::Failed);
        assert_eq!(exit.code.as_deref(), Some("coord.store_failed"));
        drop(bob);

        // A record whose watch isn't named (an older build's) never counts.
        let path = run.join(EXITS).join("alice.json");
        let mut old: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        old.as_object_mut().unwrap().remove("watch_id");
        super::super::files::replace(&path, &serde_json::to_vec(&old).unwrap()).unwrap();
        assert!(last_exit(&run, "alice").is_some());
        assert!(shown("alice").is_none());

        // A name that isn't an agent's makes no path: nothing outside.
        let outside = run.join("outside");
        for bad in [
            outside.to_str().unwrap().to_string(),
            "../outside".to_string(),
            "a/../../outside".to_string(),
            ".hidden".to_string(),
        ] {
            let dir = super::super::files::folder(&run, EXITS, false)
                .unwrap()
                .unwrap();
            assert!(exit_path(&dir, &bad).is_err(), "{bad}");
            assert!(last_exit(&run, &bad).is_none());
        }
        assert!(!outside.with_extension("json").exists());

        // A group-writable record isn't trusted.
        let path = run
            .join(EXITS)
            .join(format!("{}.json", crate::state::file_name("b:ob")));
        fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o666)).unwrap();
        assert!(last_exit(&run, "b:ob").is_none());
    }

    #[test]
    fn an_older_builds_live_supervisor_names_its_agents_as_untracked() {
        let (_scratch, run) = run_dir();
        let agents = vec!["alice".to_string(), "bob".to_string()];
        // A launcher's receipt with no supervisor record: 0.7.0, which
        // keeps none, or a build whose registration failed.
        let exe = std::env::current_exe().unwrap();
        let build = Build {
            version: "0.7.0".into(),
            commit: "0".repeat(40),
            dirty: Some(false),
            built: "2026-10-28T00:00:00Z".into(),
        };
        let _receipt = crate::builds::publish(
            &run,
            crate::builds::Kind::Launcher,
            &exe,
            build.clone(),
            agents.clone(),
        )
        .unwrap()
        .unwrap();
        // An MCP server's receipt runs no agents' turns.
        let _mcp = crate::builds::publish(
            &run,
            crate::builds::Kind::Mcp,
            &exe,
            build,
            vec!["carol".into()],
        )
        .unwrap()
        .unwrap();
        let processes = crate::builds::live(&run).unwrap().processes;
        assert_eq!(processes.len(), 2);
        let (found, _) = list(&run).unwrap();
        assert_eq!(unconfirmed_env(&found, &processes), ["alice", "bob"]);
        // Its tracked record, same pid: confirmed.
        let reg = Registered::register(&run, &agents, None, None)
            .unwrap()
            .unwrap();
        let (found, _) = list(&run).unwrap();
        assert!(found[0].record.env_tracked);
        assert!(unconfirmed_env(&found, &processes).is_empty());
        assert!(unconfirmed_env(&found, &[]).is_empty());
        // A 0.7.1 record has no env_tracked: it still reads, and its
        // agents are named, with or without a receipt.
        let path = run.join("supervisors").join(format!("{}.json", reg.id()));
        let mut old: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        old.as_object_mut().unwrap().remove("env_tracked");
        fs::write(&path, old.to_string()).unwrap();
        let (found, problems) = list(&run).unwrap();
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(unconfirmed_env(&found, &[]), ["alice", "bob"]);
        assert_eq!(unconfirmed_env(&found, &processes), ["alice", "bob"]);
    }

    #[test]
    fn a_record_of_many_long_named_agents_still_reads() {
        let (_scratch, run) = run_dir();
        // 600 agents with 120-character names: over 64 KiB of record.
        let agents: Vec<String> = (0..600).map(|i| format!("a{i:0>119}")).collect();
        let reg = Registered::register(&run, &agents, None, None)
            .unwrap()
            .unwrap();
        let path = run.join("supervisors").join(format!("{}.json", reg.id()));
        assert!(fs::metadata(&path).unwrap().len() > 64 * 1024);
        let (found, problems) = list(&run).unwrap();
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(found.len(), 1);
        assert!(found[0].live);
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
