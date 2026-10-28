//! Durable receiver routing, separate from the pruned thread-status listing.
//! A conflicting or unclassified session latches suspension. Only an explicit
//! reset followed by a new primary prompt selects a route again.
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{RunError, evidence, invalid, read_json};
use crate::{hooks, state};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    Queued,
    NotSent,
    DeliveryUnknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Attempt {
    pub id: String,
    pub session: String,
    pub through: u64,
    pub at_ms: u64,
    pub delivery: Delivery,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Route {
    pub actor: String,
    pub session: Option<String>,
    pub cwd: Option<PathBuf>,
    pub suspended: bool,
    pub working: bool,
    pub at_ms: u64,
    pub reset_at_ms: u64,
    #[serde(default)]
    pub generation: u64,
    #[serde(default = "pending_by_default")]
    pub observation_pending: bool,
    pub last_attempt: Option<Attempt>,
    // Retain provenance even when the general thread record is pruned.
    primaries: BTreeSet<String>,
    subagents: BTreeSet<String>,
}

fn pending_by_default() -> bool {
    true
}

impl Route {
    fn empty(actor: &str) -> Self {
        Self {
            actor: actor.into(),
            session: None,
            cwd: None,
            suspended: false,
            working: true,
            at_ms: 0,
            reset_at_ms: 0,
            generation: 0,
            observation_pending: true,
            last_attempt: None,
            primaries: BTreeSet::new(),
            subagents: BTreeSet::new(),
        }
    }
}

fn folder(common: &Path) -> PathBuf {
    hooks::dir(common).join("receiver")
}
fn path(common: &Path, actor: &str) -> PathBuf {
    folder(common).join(format!("{}.json", state::file_name(actor)))
}

pub fn read(common: &Path, actor: &str) -> Result<Option<Route>, RunError> {
    state::check_name(actor)?;
    let Some(value) = read_json(&path(common, actor))? else {
        return Ok(None);
    };
    let r: Route =
        serde_json::from_value(value).map_err(|_| invalid("receiver route is unreadable"))?;
    if r.actor != actor
        || r.primaries.len() + r.subagents.len() > 256
        || r.session
            .iter()
            .chain(r.primaries.iter())
            .chain(r.subagents.iter())
            .any(|s| Uuid::parse_str(s).is_err())
        || r.cwd.as_ref().is_some_and(|p| !p.is_absolute())
        || r.session.is_some() != r.cwd.is_some()
    {
        return Err(invalid("receiver route is inconsistent"));
    }
    Ok(Some(r))
}

/// Operator-visible eligibility includes independent observation evidence,
/// even when the last route write failed and its raw bytes still look idle.
pub fn status(common: &Path, actor: &str) -> Result<Option<Route>, RunError> {
    if read(common, actor)?.is_none() && !evidence::pending(common, actor)? {
        return Ok(None);
    }
    with_current(common, actor, |r| Ok(Some(r.clone())))
}

/// Read-only diagnostic view, never dispatch authority. Existing locks are
/// tried without waiting; missing/busy locks leave an explicit problem.
pub fn snapshot(common: &Path, actor: &str) -> Result<Option<Route>, RunError> {
    state::check_name(actor)?;
    let absent = match fs::symlink_metadata(path(common, actor)) {
        Ok(_) => false,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(e) => return Err(RunError::io("can't inspect receiver snapshot", e)),
    };
    if absent {
        // A hook can die after persisting its first marker but before
        // creating any lock/route. This conservative view needs no lock.
        return Ok(evidence::pending(common, actor)?.then(|| Route::empty(actor)));
    }
    let _lock = evidence::snapshot_lock(&path(common, actor).with_extension("lock"))?;
    let record = read(common, actor)?;
    if evidence::pending(common, actor)? {
        let mut r = record.unwrap_or_else(|| Route::empty(actor));
        r.observation_pending = true;
        return Ok(Some(r));
    }
    let mut r = record.ok_or_else(|| invalid("receiver busy: route disappeared"))?;
    let threads = evidence::snapshot_threads(common)?;
    reconcile(&mut r, &threads);
    r.observation_pending |= evidence::pending(common, actor)?;
    Ok(Some(r))
}

/// Shared by observation begin/finish and final validation/process creation.
fn locked<T>(
    common: &Path,
    actor: &str,
    f: impl FnOnce() -> Result<T, RunError>,
) -> Result<T, RunError> {
    use std::os::unix::fs::OpenOptionsExt;
    state::check_name(actor)?;
    fs::create_dir_all(folder(common))
        .map_err(|e| RunError::io("can't create receiver records", e))?;
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32)
        .open(path(common, actor).with_extension("lock"))
        .map_err(|e| RunError::io("can't lock receiver route", e))?;
    if !lock.metadata().is_ok_and(|m| m.is_file()) {
        return Err(invalid("receiver lock isn't a regular file"));
    }
    lock.lock()
        .map_err(|e| RunError::io("can't lock receiver route", e))?;
    f()
}

pub(super) fn with_current<T>(
    common: &Path,
    actor: &str,
    f: impl FnOnce(&Route) -> Result<T, RunError>,
) -> Result<T, RunError> {
    locked(common, actor, || {
        let record = read(common, actor)?;
        if evidence::pending(common, actor)? {
            let mut r = record.unwrap_or_else(|| Route::empty(actor));
            r.observation_pending = true;
            return f(&r);
        }
        let mut r = record.ok_or_else(|| invalid("receiver route disappeared"))?;
        evidence::with_threads(common, |threads| {
            reconcile(&mut r, threads);
            // Begin is lock-free: a marker may arrive while we wait for the
            // registry lock or read its records. Never report that as idle.
            r.observation_pending |= evidence::pending(common, actor)?;
            f(&r)
        })
    })
}

/// The route alone is not authority after a failed pre-truncation open.
/// Admission also requires completed independent observation evidence and a
/// matching registry, checked by with_current through process creation.
pub(super) fn update<T>(
    common: &Path,
    actor: &str,
    f: impl FnOnce(&mut Route) -> Result<T, RunError>,
) -> Result<T, RunError> {
    update_inner(common, actor, false, f)
}

fn update_inner<T>(
    common: &Path,
    actor: &str,
    recover: bool,
    f: impl FnOnce(&mut Route) -> Result<T, RunError>,
) -> Result<T, RunError> {
    locked(common, actor, || update_locked(common, actor, recover, f))
}

fn update_locked<T>(
    common: &Path,
    actor: &str,
    recover: bool,
    f: impl FnOnce(&mut Route) -> Result<T, RunError>,
) -> Result<T, RunError> {
    let mut record = match read(common, actor) {
        Ok(record) => record.unwrap_or_else(|| Route::empty(actor)),
        Err(_) if recover => {
            preserve_damaged(common, actor)?;
            Route::empty(actor)
        }
        Err(e) => return Err(e),
    };
    let result = f(&mut record)?;
    write_route(common, actor, &record)?;
    Ok(result)
}

fn write_route(common: &Path, actor: &str, record: &Route) -> Result<(), RunError> {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    let bytes = serde_json::to_vec(record).map_err(|_| invalid("can't encode receiver route"))?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32)
        .open(path(common, actor))
        .map_err(|e| RunError::io("can't invalidate receiver route", e))?;
    if !file.metadata().is_ok_and(|m| m.is_file()) {
        return Err(invalid("receiver route isn't a regular file"));
    }
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| RunError::io("can't persist receiver route", e))?;
    Ok(())
}

// Explicit reset alone can recover damaged regular-file evidence. Keep a
// bounded private copy, synced before replacing the authority; never follow
// links or repair a special file. Ordinary reads/updates still refuse it.
fn preserve_damaged(common: &Path, actor: &str) -> Result<(), RunError> {
    use std::{
        io::{Read, Write},
        os::unix::fs::OpenOptionsExt,
    };
    let original = fs::OpenOptions::new()
        .read(true)
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32)
        .open(path(common, actor))
        .map_err(|e| RunError::io("can't preserve damaged receiver route", e))?;
    const CAP: u64 = 1 << 20;
    if !original
        .metadata()
        .is_ok_and(|m| m.is_file() && m.len() <= CAP)
    {
        return Err(invalid(
            "damaged receiver route isn't a bounded regular file",
        ));
    }
    let mut bytes = Vec::new();
    original
        .take(CAP + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| RunError::io("can't read damaged receiver route", e))?;
    if bytes.len() as u64 > CAP {
        return Err(invalid("damaged receiver route is too large"));
    }
    let backup = path(common, actor).with_extension(format!("json.damaged-{}", Uuid::now_v7()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(backup)
        .map_err(|e| RunError::io("can't preserve damaged receiver route", e))?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| RunError::io("can't preserve damaged receiver route", e))
}

#[derive(Debug)]
pub struct Observation {
    generation: u64,
    thread: hooks::Thread,
    marker: evidence::Marker,
}

/// Persist authoritative observation evidence before status recording or any
/// other fallible hook work. A failed/incomplete status update leaves pending
/// evidence, which admission refuses even after restart. Legacy records start
/// pending until a complete observation upgrades them.
pub fn begin_observation(
    common: &Path,
    actor: &str,
    session: &str,
    cwd: &Path,
    observation: &hooks::HookObservation,
) -> Result<Observation, RunError> {
    begin_with(common, actor, session, cwd, observation, || Ok(()))
}

// The seam is private to this module and offline tests, never environment
// controlled in a production hook. It injects failure at the reviewed gap.
pub(super) fn begin_with(
    common: &Path,
    actor: &str,
    session: &str,
    cwd: &Path,
    observation: &hooks::HookObservation,
    before_truncate: impl FnOnce() -> Result<(), RunError>,
) -> Result<Observation, RunError> {
    begin_at(
        common,
        actor,
        session,
        cwd,
        observation,
        || Ok(()),
        before_truncate,
    )
}

pub(super) fn begin_at(
    common: &Path,
    actor: &str,
    session: &str,
    cwd: &Path,
    observation: &hooks::HookObservation,
    after_marker: impl FnOnce() -> Result<(), RunError>,
    before_truncate: impl FnOnce() -> Result<(), RunError>,
) -> Result<Observation, RunError> {
    state::check_name(actor)?;
    // First durable act, before route locks, identity reads or status writes.
    // Failure before this marker exists equals a hook that never fired.
    let marker = evidence::begin(common, actor)?;
    after_marker()?;
    let parsed = Uuid::parse_str(session).map_err(|_| invalid("receiver session isn't a UUID"))?;
    if parsed.to_string() != session {
        return Err(invalid("receiver session isn't a canonical UUID"));
    }
    let cwd = mdkg_coord_sqlite::git::toplevel(cwd)
        .map_err(|e| RunError::io("can't identify receiver worktree", e))?;
    let thread = hooks::Thread::observed(
        actor.into(),
        "codex".into(),
        session.into(),
        cwd.clone(),
        hooks::ThreadState::Working,
        observation.clone(),
    );
    locked(common, actor, || {
        marker.check()?;
        // Independent writes: never short-circuit the route invalidation if
        // the witness or registry write failed. A crash at the route open now
        // leaves the marker and the working registry observation behind.
        let witnessed = evidence::persist_thread(&marker, &thread);
        let recorded = hooks::record_thread(common, &thread)
            .map_err(|e| RunError::io("can't record pending receiver thread", e));
        let updated = update_locked(common, actor, false, |r| {
            r.generation = r
                .generation
                .checked_add(1)
                .ok_or_else(|| invalid("receiver generation exhausted"))?;
            r.observation_pending = true;
            // Reconcile before applying this observation, so a later Stop from
            // A cannot hide the independent registry's failed observation of B.
            if evidence::with_threads(common, |threads| {
                reconcile(r, threads);
                Ok(())
            })
            .is_err()
            {
                r.suspended = true;
                // Still persist the invalidation even if registry reads fail.
            }
            apply_observation(r, session, &cwd, hooks::ThreadState::Working, observation);
            before_truncate()?;
            Ok(r.generation)
        });
        witnessed?;
        recorded?;
        let generation = updated?;
        Ok(Observation {
            generation,
            thread,
            marker,
        })
    })
}

pub fn finish_observation(
    common: &Path,
    actor: &str,
    mut ticket: Observation,
    state: hooks::ThreadState,
) -> Result<(), RunError> {
    locked(common, actor, || {
        ticket.marker.check()?;
        update_locked(common, actor, false, |r| {
            if r.generation == ticket.generation {
                ticket.thread.state = state;
                hooks::record_thread(common, &ticket.thread)
                    .map_err(|e| RunError::io("can't finish receiver thread", e))?;
                if r.session.as_deref() == Some(&ticket.thread.session)
                    && ticket.thread.last_seen_ms >= r.at_ms
                {
                    r.working = state == hooks::ThreadState::Working;
                }
                r.observation_pending = false;
            }
            Ok(())
        })?;
        evidence::finish(&ticket.marker)
    })
}

/// Registry evidence may remove eligibility, never clear the route's latch.
/// Missing selected-session evidence is also a refusal, not an empty history.
fn reconcile(r: &mut Route, threads: &[hooks::Thread]) {
    let was_working = r.working;
    let mut matched = r.session.is_none();
    for t in threads {
        if t.actor != r.actor && r.cwd.as_ref().is_none_or(|cwd| !t.cwd.starts_with(cwd)) {
            continue;
        }
        if t.last_seen_ms < r.reset_at_ms {
            continue;
        }
        if t.actor != r.actor {
            r.suspended = true;
            continue;
        }
        if t.last_observation.is_none() {
            r.suspended = true;
        }
        if r.session.as_deref() == Some(&t.session) {
            matched = true;
            if t.last_observation
                .as_ref()
                .is_none_or(|o| o.at_ms != r.at_ms)
            {
                r.observation_pending = true;
            }
            if t.state == hooks::ThreadState::Working {
                r.working = true;
            }
        }
        // Retained classification must precede the latest observation.
        for o in t.last_session_start.iter().chain(t.last_observation.iter()) {
            apply_observation(r, &t.session, &t.cwd, t.state, o);
        }
    }
    if !matched {
        r.observation_pending = true;
    }
    r.working |= was_working;
}

/// Convenience for callers with no separate status write (offline fixtures).
pub fn observe(
    common: &Path,
    actor: &str,
    session: &str,
    cwd: &Path,
    state: hooks::ThreadState,
    observation: &hooks::HookObservation,
) -> Result<(), RunError> {
    let ticket = begin_observation(common, actor, session, cwd, observation)?;
    finish_observation(common, actor, ticket, state)
}

fn apply_observation(
    r: &mut Route,
    session: &str,
    cwd: &Path,
    state: hooks::ThreadState,
    observation: &hooks::HookObservation,
) {
    if observation.at_ms < r.reset_at_ms {
        return;
    }
    if observation.harness_event == Some(hooks::HarnessEvent::SessionStart) {
        let Some(provenance) = &observation.start_provenance else {
            r.suspended = true;
            return;
        };
        let subagent = provenance.agent_id.is_some();
        let (mine, other) = if subagent {
            (&mut r.subagents, &r.primaries)
        } else {
            (&mut r.primaries, &r.subagents)
        };
        if other.contains(session) {
            r.suspended = true;
            return;
        }
        if !mine.contains(session) && mine.len() + other.len() >= 256 {
            r.suspended = true;
            return;
        }
        mine.insert(session.to_string());
    }
    if r.subagents.contains(session) {
        return;
    }
    if !r.primaries.contains(session) {
        // Missing SessionStart provenance cannot prove this is a child.
        if r.session.is_some() {
            r.suspended = true;
        }
        return;
    }
    if r.session.is_none() {
        // After a reset, a delayed Start or Stop cannot select a route.
        if r.reset_at_ms > 0
            && observation.harness_event != Some(hooks::HarnessEvent::UserPromptSubmit)
        {
            return;
        }
        r.session = Some(session.to_string());
        r.cwd = Some(cwd.to_path_buf());
    }
    if r.session.as_deref() != Some(session) || r.cwd.as_deref() != Some(cwd) {
        r.suspended = true;
    } else if observation.at_ms >= r.at_ms {
        r.working = state == hooks::ThreadState::Working;
        r.at_ms = observation.at_ms;
    }
}

pub fn reset(common: &Path, actor: &str) -> Result<(), RunError> {
    locked(common, actor, || {
        update_locked(common, actor, true, |r| {
            // Reset is the bounded provenance recovery operation. Keep only the
            // previous bound primary (or the sole retained primary on repeated
            // reset). Every other session needs fresh SessionStart provenance;
            // unknown later observations still suspend a selected route.
            let keep = r
                .session
                .as_ref()
                .filter(|s| r.primaries.contains(*s))
                .cloned()
                .or_else(|| {
                    (r.session.is_none() && r.primaries.len() == 1)
                        .then(|| r.primaries.iter().next().cloned())
                        .flatten()
                });
            r.generation = r
                .generation
                .checked_add(1)
                .ok_or_else(|| invalid("receiver generation exhausted"))?;
            r.observation_pending = true;
            r.primaries.clear();
            r.subagents.clear();
            if let Some(session) = keep {
                r.primaries.insert(session);
            }
            r.session = None;
            r.cwd = None;
            r.suspended = false;
            r.working = true;
            r.reset_at_ms = state::now_ms();
            Ok(())
        })?;
        evidence::reset(common, actor)
    })
}

/// Names only, including damaged records; callers display read errors rather
/// than silently dropping evidence. This listing never authorizes a dispatch.
pub fn actors(common: &Path) -> Vec<String> {
    let mut names: Vec<_> = fs::read_dir(folder(common))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|e| {
            let name = e.file_name();
            let name = name.to_str()?;
            [".json", ".pending", ".observed"]
                .iter()
                .find_map(|suffix| name.strip_suffix(suffix).map(|s| s.replace("%3A", ":")))
        })
        .filter(|s| state::check_name(s).is_ok())
        .collect();
    names.sort();
    names.dedup();
    names
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;

    #[test]
    fn pending_only_snapshot_is_read_only() {
        struct Scratch(PathBuf);
        impl Drop for Scratch {
            fn drop(&mut self) {
                fs::remove_dir_all(&self.0).ok();
            }
        }
        let common =
            Scratch(std::env::temp_dir().join(format!("mdkg-route-snapshot-{}", Uuid::now_v7())));
        let pending = folder(&common.0).join("observer.pending");
        fs::create_dir_all(&pending).expect("pending directory");
        fs::write(pending.join(Uuid::now_v7().to_string()), "").expect("pending marker");
        let route = snapshot(&common.0, "observer")
            .expect("snapshot")
            .expect("pending route");
        assert!(route.observation_pending);
        assert!(route.working);
        assert!(route.session.is_none());
        assert!(!path(&common.0, "observer").exists());
        assert!(!path(&common.0, "observer").with_extension("lock").exists());
        assert!(!hooks::dir(&common.0).join("threads.lock").exists());
    }
}
