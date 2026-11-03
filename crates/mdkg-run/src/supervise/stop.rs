//! `agents stop --drain` (API v1 A18): a request, from outside, that an
//! agent's supervisor stop admitting its turns. The command writes
//! `<run dir>/stop/<agent>.json` for the supervisor it names; that
//! supervisor finds it at its next loop boundary or idle poll, lets a turn
//! in flight finish, finishes the agent's watch, deletes the request and
//! marks the agent stopped. A request naming another supervisor is stale:
//! the next `agents up` for the agent removes it.

use std::fs;
use std::io::Read as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::RunError;
use crate::state;

pub const SCHEMA: &str = "mdkg.run.stop.v1";
const CAP: u64 = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub schema: String,
    /// `drain`, the only mode.
    pub mode: String,
    pub requested_by: String,
    pub requested_at: String,
    pub supervisor_id: String,
}

impl Request {
    pub fn drain(requested_by: &str, requested_at: String, supervisor_id: &str) -> Self {
        Self {
            schema: SCHEMA.into(),
            mode: "drain".into(),
            requested_by: requested_by.into(),
            requested_at,
            supervisor_id: supervisor_id.into(),
        }
    }
}

fn path(dir: &Path, agent: &str) -> PathBuf {
    dir.join(format!("{}.json", state::file_name(agent)))
}

/// Writes the request atomically. A request already there for the same
/// supervisor is kept as it is, so asking twice changes nothing; one for
/// another supervisor is replaced.
pub fn write(run_dir: &Path, agent: &str, request: &Request) -> Result<(), RunError> {
    state::check_name(agent)?;
    let dir = super::files::folder(run_dir, "stop", true)?
        .ok_or_else(|| RunError::failure("io.write_failed", "the store's folder is missing"))?;
    if read(run_dir, agent)?.is_some_and(|r| r.supervisor_id == request.supervisor_id) {
        return Ok(());
    }
    let bytes = serde_json::to_vec(request)
        .map_err(|e| RunError::failure("io.write_failed", e.to_string()))?;
    super::files::replace(&path(&dir, agent), &bytes)
        .map_err(|e| RunError::io("can't write the stop request", e))
}

/// The agent's stop request, if there is one.
pub fn read(run_dir: &Path, agent: &str) -> Result<Option<Request>, RunError> {
    state::check_name(agent)?;
    let Some(dir) = super::files::folder(run_dir, "stop", false)? else {
        return Ok(None);
    };
    let mut file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(super::files::flags())
        .open(path(&dir, agent))
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(RunError::io("can't open the stop request", e)),
    };
    let m = file
        .metadata()
        .map_err(|e| RunError::io("can't inspect the stop request", e))?;
    if !m.is_file()
        || m.len() > CAP
        || m.uid() != rustix::process::geteuid().as_raw()
        || m.mode() & 0o022 != 0
    {
        return Err(RunError::failure(
            "io.read_failed",
            "the stop request isn't an owned, protected regular file",
        ));
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take(CAP + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| RunError::io("can't read the stop request", e))?;
    let request: Request = serde_json::from_slice(&bytes)
        .map_err(|_| RunError::failure("io.read_failed", "invalid stop request"))?;
    if request.schema != SCHEMA {
        return Err(RunError::failure(
            "io.read_failed",
            "unknown stop request schema",
        ));
    }
    Ok(Some(request))
}

/// Whether `supervisor_id` has a drain request for `agent`. An unreadable
/// request counts as none: the supervisor keeps running, and `agents
/// status` names the problem.
pub fn requested(run_dir: &Path, agent: &str, supervisor_id: &str) -> bool {
    read(run_dir, agent).is_ok_and(|r| r.is_some_and(|r| r.supervisor_id == supervisor_id))
}

/// Removes the agent's request, returning it. Removing none is fine.
pub fn remove(run_dir: &Path, agent: &str) -> Result<Option<Request>, RunError> {
    let found = read(run_dir, agent).ok().flatten();
    let Some(dir) = super::files::folder(run_dir, "stop", false)? else {
        return Ok(None);
    };
    match fs::remove_file(path(&dir, agent)) {
        Ok(()) => Ok(found),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(RunError::io("can't remove the stop request", e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_round_trips_is_kept_when_repeated_and_removed_once() {
        let scratch = super::super::files::Scratch::new("supervise-stop");
        let run = scratch.run_dir();
        assert_eq!(read(&run, "alice").unwrap(), None);
        assert!(!requested(&run, "alice", "s1"));
        let first = Request::drain("nick", "2026-10-30T12:00:00.000Z".into(), "s1");
        write(&run, "alice", &first).unwrap();
        // Asking again keeps the first request's time.
        let again = Request::drain("nick", "2026-10-30T12:01:00.000Z".into(), "s1");
        write(&run, "alice", &again).unwrap();
        assert_eq!(read(&run, "alice").unwrap(), Some(first.clone()));
        assert!(requested(&run, "alice", "s1"));
        assert!(!requested(&run, "alice", "s2"));
        assert!(!requested(&run, "bob", "s1"));
        // Another supervisor's replaces it.
        let other = Request::drain("nick", "2026-10-30T12:02:00.000Z".into(), "s2");
        write(&run, "alice", &other).unwrap();
        assert_eq!(read(&run, "alice").unwrap(), Some(other.clone()));
        assert_eq!(remove(&run, "alice").unwrap(), Some(other));
        assert_eq!(remove(&run, "alice").unwrap(), None);
        assert!(read(&run, "../alice").is_err());
    }
}
