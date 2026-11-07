//! `mdkg agents up`: wakes agents on mail and runs their turns under their
//! policies ([`crate::policy`]). One thread per agent;
//! each waits for mail newer than its cursor and runs a turn, one at a
//! time per agent through the agent's turn lease.
//!
//! The loop guard counts each agent's automatic turns per thread in
//! `<run dir>/guard/<agent>.json`, charged by the turn under its lease
//! before the agent starts, whatever the turn's end. A thread past the
//! limit is paused: its
//! mail stays unacked, other threads still run, and it runs again once its
//! oldest counted turn leaves the window, or when a person runs the agent
//! (`mdkg agents up <name> --once`), or clears it (`mdkg agents resume`).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::RunError;
use crate::mail::Mail;
use crate::policy::{LoopGuard, Policy, Trigger};
use crate::state;
use crate::turn::{self, Outcome, TurnOptions, TurnReport};
use mdkg_format::program::BIN;

mod control;
mod files;
pub mod progress;
pub mod registry;
pub mod reload;
pub mod stop;
mod watch;
pub(crate) use control::note_end;
pub use control::{
    Control, End, Progress, drain_requested, load_progress, pause, save_progress, stop_requested,
    supervisor_id, wait_secs,
};
#[cfg(test)]
pub(crate) use control::{bounded_test, with_agent, with_control};
#[cfg(test)]
pub(crate) use files::Scratch;
pub use files::timestamp;
pub use watch::{TurnWatch, WatchedOutcome};

/// An agent's loop guard record: each thread's automatic turns, as
/// milliseconds since the epoch, oldest first.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Guard {
    #[serde(default)]
    pub threads: BTreeMap<String, Vec<u64>>,
}

/// Where an agent's guard record is. A folder of its own: the run folder's
/// `*.json` files are agents' states.
pub fn guard_path(dir: &Path, name: &str) -> PathBuf {
    dir.join("guard")
        .join(format!("{}.json", state::file_name(name)))
}

impl Guard {
    /// The agent's record, for a wake decision only: empty when there's
    /// none or it can't be read. What a turn is admitted by is
    /// [`Guard::read`] and [`Guard::update`].
    pub fn load(dir: &Path, name: &str) -> Self {
        Self::read(dir, name).unwrap_or_default()
    }

    /// The agent's record; empty when there's none, and an error when one
    /// is there but can't be read, so no automatic turn runs on a record
    /// that lost its count.
    pub fn read(dir: &Path, name: &str) -> Result<Self, RunError> {
        match fs::read(guard_path(dir, name)) {
            Ok(b) => serde_json::from_slice(&b).map_err(|e| {
                RunError::failure(
                    crate::error::CODE_AGENT_FAILED,
                    format!("{name}'s loop guard record can't be read: {e}"),
                )
                .with_hint(format!("`{BIN} agents resume {name}` starts it over"))
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(RunError::io("can't read the loop guard", e)),
        }
    }

    /// Changes the agent's record under its lock, so a turn's charge and a
    /// person's clear never lose each other's write. `start_over` ignores a
    /// record that can't be read (`agents resume`).
    pub fn update<R>(
        dir: &Path,
        name: &str,
        start_over: bool,
        f: impl FnOnce(&mut Guard) -> Result<R, RunError>,
    ) -> Result<R, RunError> {
        let path = guard_path(dir, name);
        let folder = path.parent().unwrap_or(dir);
        fs::create_dir_all(folder).map_err(|e| RunError::io("can't write the loop guard", e))?;
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path.with_extension("lock"))
            .map_err(|e| RunError::io("can't lock the loop guard", e))?;
        // No lock, no change: without one a charge and a clear could lose
        // each other's write (review of ce461ba).
        lock.lock()
            .map_err(|e| RunError::io("can't lock the loop guard", e))?;
        let mut guard = match Self::read(dir, name) {
            Ok(g) => g,
            Err(_) if start_over => Self::default(),
            Err(e) => return Err(e),
        };
        let out = f(&mut guard)?;
        guard.save(dir, name)?;
        let _ = lock.unlock();
        Ok(out)
    }

    fn save(&self, dir: &Path, name: &str) -> Result<(), RunError> {
        let path = guard_path(dir, name);
        let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
        let text = serde_json::to_vec(self)
            .map_err(|e| RunError::io("can't write the loop guard", std::io::Error::other(e)))?;
        fs::write(&tmp, text).map_err(|e| RunError::io("can't write the loop guard", e))?;
        fs::rename(&tmp, &path).map_err(|e| RunError::io("can't write the loop guard", e))
    }

    /// Drops turns that left the window, and threads with none left.
    fn prune(&mut self, g: &LoopGuard, now_ms: u64) {
        let window = u64::try_from(g.window.as_millis()).unwrap_or(u64::MAX);
        for turns in self.threads.values_mut() {
            turns.retain(|t| now_ms.saturating_sub(*t) < window);
        }
        self.threads.retain(|_, t| !t.is_empty());
    }

    /// The threads the guard pauses now.
    pub fn paused(&self, g: &LoopGuard, now_ms: u64) -> Vec<String> {
        let mut live = self.clone();
        live.prune(g, now_ms);
        live.threads
            .into_iter()
            .filter(|(_, t)| t.len() >= g.turns)
            .map(|(k, _)| k)
            .collect()
    }

    /// How long until a paused thread runs again, the soonest of them.
    pub fn next_cooldown(&self, g: &LoopGuard, now_ms: u64) -> Option<Duration> {
        let window = u64::try_from(g.window.as_millis()).unwrap_or(u64::MAX);
        let mut live = self.clone();
        live.prune(g, now_ms);
        live.threads
            .values()
            .filter(|t| t.len() >= g.turns)
            // The turn that must leave the window for one fewer than the
            // limit to remain.
            .filter_map(|t| t.get(t.len() - g.turns))
            .map(|oldest| Duration::from_millis((oldest + window).saturating_sub(now_ms)))
            .min()
    }

    /// Counts an automatic turn in `thread`.
    pub fn record(&mut self, g: &LoopGuard, thread: &str, now_ms: u64) {
        self.prune(g, now_ms);
        self.threads
            .entry(thread.to_string())
            .or_default()
            .push(now_ms);
    }

    /// Clears a thread (a person ran the agent on it), or every thread.
    pub fn clear(&mut self, thread: Option<&str>) {
        match thread {
            Some(t) => {
                self.threads.remove(t);
            }
            None => self.threads.clear(),
        }
    }
}

/// One agent `agents up` runs.
#[derive(Debug, Clone)]
pub struct Supervised {
    pub name: String,
    pub policy: Policy,
    /// The turn's options, before the policy's limits.
    pub opts: TurnOptions,
}

/// What `agents up` tells its caller, per agent.
#[derive(Debug)]
pub enum Event<'a> {
    Wake(&'a crate::receiver::Report),
    /// A turn ran, or failed.
    Turn(&'a Result<TurnReport, RunError>),
    /// Threads the loop guard paused, with mail waiting, and when the
    /// soonest runs again.
    Paused(&'a [String], Option<Duration>),
    /// The agent isn't woken (`trigger: manual`).
    Manual,
    /// A wait or a check failed; the agent tries again shortly.
    Trouble(&'a RunError),
    /// `agents stop` drained the agent: it admits no more turns.
    Stopped,
}

/// How `agents up` runs.
#[derive(Debug, Clone)]
pub struct Up {
    /// How long each wait lasts before it starts again, in seconds.
    pub wait_secs: u64,
    /// Stop each agent after this many turns.
    pub max_turns: Option<u64>,
    /// Run what's waiting, then stop, rather than wait for more.
    pub once: bool,
    /// The pause after trouble (a lease held elsewhere, a failing wait).
    pub pause: Duration,
    /// Each agent's first turn starts a new session.
    pub fresh: bool,
}

/// At most `cap` turns at once across the agents.
struct Slots {
    free: Mutex<usize>,
    freed: Condvar,
}

impl Slots {
    fn take(&self) -> Option<Slot<'_>> {
        let mut free = self.free.lock().unwrap_or_else(|e| e.into_inner());
        while *free == 0 {
            if stop_requested() {
                return None;
            }
            free = self
                .freed
                .wait_timeout(free, Duration::from_millis(100))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        if stop_requested() {
            return None;
        }
        *free -= 1;
        Some(Slot(self))
    }

    fn give(&self) {
        let mut free = self.free.lock().unwrap_or_else(|e| e.into_inner());
        *free += 1;
        self.freed.notify_one();
    }
}

/// Return capacity on every exit, including unwinding from a failed worker.
struct Slot<'a>(&'a Slots);
impl Drop for Slot<'_> {
    fn drop(&mut self) {
        self.0.give();
    }
}

/// The cap on turns at once: the smallest `concurrency` any agent's policy
/// sets (in 0.7.0 one cap for the supervisor), or none.
pub fn cap(agents: &[Supervised]) -> Option<usize> {
    agents.iter().filter_map(|a| a.policy.concurrency).min()
}

/// Runs every agent until each stops (`once`, `max_turns`) or forever.
/// `mail` makes each agent's own mail client, by its name. Returns the turns run per
/// agent, in order. A policy naming a later kind refuses the whole run
/// before anything starts.
///
/// A worker panic drains its siblings and returns an error. Ordinary turn
/// errors retain their retry policy. This entry point does not yet adopt the
/// durable turn bridge. CLI launchers use [`up_durable`], which drains on
/// persistence/protocol errors as well as worker panics.
pub fn up<M: Mail>(
    mail: &(dyn Fn(&str) -> M + Sync),
    dir: &Path,
    agents: &[Supervised],
    how: &Up,
    on: &(dyn Fn(&str, Event<'_>) + Sync),
) -> Result<Vec<u64>, RunError> {
    up_inner(mail, dir, agents, how, on, None, None)
}

/// A watch that can drain between turns. The caller requests stop, waits for
/// this function to join every worker, then may replace its own process.
pub fn up_controlled<M: Mail>(
    mail: &(dyn Fn(&str) -> M + Sync),
    dir: &Path,
    agents: &[Supervised],
    how: &Up,
    on: &(dyn Fn(&str, Event<'_>) + Sync),
    control: &Control,
) -> Result<Vec<u64>, RunError> {
    up_inner(mail, dir, agents, how, on, Some(control), None)
}

/// Run with one started, watch-long progress lock per selected agent. A fatal
/// progress error drains every sibling before returning; it never permits exec.
#[allow(clippy::too_many_arguments)]
pub fn up_durable<M: Mail>(
    mail: &(dyn Fn(&str) -> M + Sync),
    dir: &Path,
    agents: &[Supervised],
    how: &Up,
    on: &(dyn Fn(&str, Event<'_>) + Sync),
    control: &Control,
    saved: &mut [progress::Locked],
) -> Result<Vec<u64>, RunError> {
    if saved.len() != agents.len() {
        return Err(RunError::failure(
            "run.progress_invalid",
            "progress/agent count mismatch",
        ));
    }
    for (a, saved) in agents.iter().zip(saved.iter()) {
        let record = saved.record().ok_or_else(|| {
            RunError::failure(
                "run.progress_invalid",
                "supervisor progress has not started",
            )
        })?;
        record.require_ready()?;
        if record.agent != a.name || record.finished {
            return Err(RunError::failure(
                "run.progress_invalid",
                "progress/agent mismatch",
            ));
        }
    }
    up_inner(mail, dir, agents, how, on, Some(control), Some(saved))
}

#[allow(clippy::too_many_arguments)]
fn up_inner<M: Mail>(
    mail: &(dyn Fn(&str) -> M + Sync),
    dir: &Path,
    agents: &[Supervised],
    how: &Up,
    on: &(dyn Fn(&str, Event<'_>) + Sync),
    control: Option<&Control>,
    saved: Option<&mut [progress::Locked]>,
) -> Result<Vec<u64>, RunError> {
    for a in agents {
        a.policy.check_supported(&a.name)?;
    }
    // Even callers that don't watch a binary must stop siblings after a
    // panic. Catch inside each worker, before an ordered join can wait on a
    // different worker's idle loop. Do not expose panic payloads as diagnostics.
    let local_control = Control::default();
    let control = control.unwrap_or(&local_control);
    let slots = cap(agents).map(|n| Slots {
        free: Mutex::new(n),
        freed: Condvar::new(),
    });
    let records: Vec<Option<&mut progress::Locked>> = match saved {
        Some(records) => records.iter_mut().map(Some).collect(),
        None => agents.iter().map(|_| None).collect(),
    };
    let ran: Vec<Result<u64, RunError>> = std::thread::scope(|s| {
        let handles: Vec<_> = agents
            .iter()
            .zip(records)
            .map(|(a, saved)| {
                let slots = slots.as_ref();
                s.spawn(move || {
                    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        control::with_control(Some(control), || {
                            control::with_agent(&a.name, || {
                                let mut saved = saved;
                                let mut m = mail(&a.name);
                                let ran = supervise(
                                    &mut m,
                                    dir,
                                    a,
                                    how,
                                    slots,
                                    on,
                                    saved.as_deref_mut(),
                                )?;
                                if control::drain_requested() {
                                    drained(dir, &a.name, control, saved, on)?;
                                }
                                Ok::<u64, RunError>(ran)
                            })
                        })
                    })) {
                        Ok(result) => {
                            if let Err(error) = &result {
                                control.fail(error.clone());
                            }
                            result
                        }
                        Err(_) => {
                            let error = worker_panicked(&a.name);
                            control.fail(error.clone());
                            Err(error)
                        }
                    }
                })
            })
            .collect();
        handles
            .into_iter()
            .zip(agents)
            .map(|(h, a)| {
                h.join().unwrap_or_else(|_| {
                    let error = worker_panicked(&a.name);
                    control.fail(error.clone());
                    Err(error)
                })
            })
            .collect()
    });
    if let Some(error) = control.failure() {
        return Err(error);
    }
    ran.into_iter().collect()
}

/// `agents stop` drained `name` and its worker returned at a loop
/// boundary, with no turn in flight: finish its watch and give up its
/// progress lock, so it can be started again while this supervisor runs
/// its other agents; then remove the request and mark it stopped.
fn drained(
    dir: &Path,
    name: &str,
    control: &Control,
    saved: Option<&mut progress::Locked>,
    on: &(dyn Fn(&str, Event<'_>) + Sync),
) -> Result<(), RunError> {
    // Recorded under the progress lock, before it's released, naming its
    // watch (#20977). Best effort: a warning.
    if let Some(supervisor) = control.supervisor()
        && let Some(held) = saved.as_deref()
        && let Err(e) = registry::record_exit(
            supervisor.run_dir(),
            &supervisor.id(),
            held,
            registry::ExitReason::Drained,
            None,
        )
    {
        eprintln!(
            "warning: can't record that {} was drained: {}",
            crate::digest::terminal_safe(name),
            crate::digest::terminal_safe(&e.message)
        );
    }
    if let Some(saved) = saved {
        saved.release()?;
    }
    stop::remove(dir, name)?;
    if let Some(supervisor) = control.supervisor() {
        supervisor.set(name, registry::AgentState::Stopped)?;
    }
    on(name, Event::Stopped);
    Ok(())
}

fn worker_panicked(name: &str) -> RunError {
    RunError::failure(
        "run.agent_failed",
        format!("{name}'s supervisor worker panicked; execution stopped"),
    )
}

/// One agent's loop; returns the turns it ran.
#[allow(clippy::too_many_arguments)]
fn supervise(
    mail: &mut dyn Mail,
    dir: &Path,
    a: &Supervised,
    how: &Up,
    slots: Option<&Slots>,
    on: &(dyn Fn(&str, Event<'_>) + Sync),
    mut saved: Option<&mut progress::Locked>,
) -> Result<u64, RunError> {
    let name = a.name.as_str();
    if a.policy.trigger == Trigger::Manual {
        on(name, Event::Manual);
        note_end(End::Manual);
        return Ok(0);
    }
    if let Some(root) = a.opts.graph_root.as_deref() {
        match crate::receiver::mode(root, dir, name) {
            Ok(crate::receiver::Mode::Interactive) => {
                return match saved {
                    Some(saved) => crate::receiver::supervise_durable(mail, dir, a, how, on, saved),
                    None => Ok(crate::receiver::supervise(mail, dir, a, how, on)),
                };
            }
            Ok(crate::receiver::Mode::Headless) => (),
            Err(e) => {
                on(name, Event::Trouble(&e));
                return Ok(0);
            }
        }
    }
    let progress = saved
        .as_ref()
        .and_then(|s| s.record())
        .map_or_else(|| load_progress(name), |r| r.progress);
    let mut ran = progress.ran;
    let mut check_inbox = progress.check_inbox;
    // Mail at or below this is seen, and waits for a paused thread or a
    // person; newer mail wakes the agent.
    let mut seen: Option<u64> = progress.seen;
    let mut capacity = Capacity::load(dir, name);
    let opts = TurnOptions {
        limit: a.policy.batch_max(),
        digest_budget: match a.policy.shape {
            crate::policy::Shape::Digest { budget } => Some(budget),
            _ => None,
        },
        timeout: a.policy.timeout.or(a.opts.timeout),
        ..a.opts.clone()
    };
    loop {
        let current = Progress {
            ran,
            seen,
            check_inbox,
        };
        checkpoint(saved.as_deref_mut(), current)?;
        save_progress(name, current);
        record_state(name, registry::AgentState::Idle, on);
        if stop_requested() {
            return Ok(ran);
        }
        if how.max_turns.is_some_and(|m| ran >= m) {
            note_end(End::Cap);
            return Ok(ran);
        }
        let agent = match state::load(dir, name) {
            Ok(Some(agent)) => agent,
            Ok(None) => {
                on(
                    name,
                    Event::Trouble(&RunError::usage(
                        crate::error::CODE_UNKNOWN_AGENT,
                        format!("no agent named {name}"),
                    )),
                );
                note_end(End::UnknownAgent);
                return Ok(ran);
            }
            Err(e) => {
                on(name, Event::Trouble(&e));
                pause(how.pause);
                continue;
            }
        };
        let now = state::now_ms();
        let guard = Guard::load(dir, name);
        let paused = a
            .policy
            .loop_guard
            .map(|g| guard.paused(&g, now))
            .unwrap_or_default();
        let cooldown = a
            .policy
            .loop_guard
            .and_then(|g| guard.next_cooldown(&g, now));
        if check_inbox {
            let inbox = match turn::peek_for(mail, name, &paused, opts.digest_budget.is_some()) {
                Ok(i) => i,
                Err(e) => {
                    on(name, Event::Trouble(&e));
                    if how.once {
                        return Ok(ran);
                    }
                    pause(how.pause);
                    continue;
                }
            };
            if !inbox.live {
                if !inbox.paused.is_empty() {
                    on(name, Event::Paused(&inbox.paused, cooldown));
                }
                seen = inbox.cursor.max(seen);
                check_inbox = false;
                continue;
            }
            let mut agent = agent;
            let mut turn_opts = TurnOptions {
                require_headless: a.opts.graph_root.is_some(),
                loop_guard: a.policy.loop_guard,
                ..opts.clone()
            };
            let capacity_fresh = capacity.fresh_next();
            turn_opts.flags.fresh = (how.fresh && ran == 0) || capacity_fresh;
            let permit = match slots {
                Some(s) => match s.take() {
                    Some(permit) => Some(permit),
                    None => return Ok(ran),
                },
                None => None,
            };
            if stop_requested() {
                return Ok(ran);
            }
            record_state(name, registry::AgentState::Running, on);
            let durable = saved.is_some();
            let result = if let Some(saved) = saved.as_deref_mut() {
                let watch = TurnWatch::new(saved, inbox.cursor)?;
                match turn::turn_watched(mail, dir, &mut agent, &turn_opts, watch)? {
                    WatchedOutcome::Unclaimed(error) => {
                        drop(permit);
                        on(name, Event::Trouble(&error));
                        if how.once {
                            return Ok(ran);
                        }
                        pause(how.pause);
                        continue;
                    }
                    WatchedOutcome::Completed { result, progress } => {
                        ran = progress.ran;
                        seen = progress.seen;
                        check_inbox = progress.check_inbox;
                        *result
                    }
                }
            } else {
                turn::turn(mail, dir, &mut agent, &turn_opts)
            };
            drop(permit);
            // A lease held elsewhere (a person's `run`, an install): not a
            // turn, tried again shortly.
            if let Err(e) = &result
                && e.code == "coord.lease_held"
            {
                on(name, Event::Trouble(e));
                if how.once {
                    return Ok(ran);
                }
                pause(how.pause);
                continue;
            }
            // The turn charged the guard itself, under its lease, before
            // the agent started.
            on(name, Event::Turn(&result));
            capacity.after(&result, capacity_fresh);
            if let Err(e) = capacity.save(dir, name) {
                on(name, Event::Trouble(&e));
            }
            if durable {
                continue;
            }
            if !matches!(&result, Ok(r) if r.outcome == Outcome::Idle) {
                ran += 1;
            }
            // retry `none`: a failed turn's mail waits for the next
            // trigger, mail newer than any it could have been shown, even
            // when it failed before the agent's cursor moved. An idle turn
            // that set mail aside (launcher notes, mail no reply could
            // reach) made progress, so the inbox is looked at again at once
            // until nothing more goes (review of 3a9bddd); one that changed
            // nothing waits, so it never loops.
            let idle_stuck = matches!(&result, Ok(r) if r.outcome == Outcome::Idle
                && r.launcher_notes.is_empty()
                && r.not_answered.is_empty());
            check_inbox = result.is_ok() && !idle_stuck;
            if !check_inbox {
                seen = inbox.cursor.max(seen);
            }
            continue;
        }
        if how.once {
            return Ok(ran);
        }
        let after = agent.cursor.max(seen);
        let wait = cooldown.map_or(how.wait_secs, |c| how.wait_secs.min(c.as_secs().max(1)));
        let waited = if opts.digest_budget.is_some() {
            mail.wait_digest(name, after, wait_secs(wait))
        } else {
            mail.wait(name, after, wait_secs(wait))
        };
        match waited {
            Ok(Some(_)) => check_inbox = true,
            // Timed out: a cooled-down thread may run now.
            Ok(None) => check_inbox = cooldown.is_some(),
            Err(e) => {
                on(name, Event::Trouble(&e));
                pause(how.pause);
            }
        }
    }
}

/// The supervisor's record is for readers; failing to write it never stops
/// the agent.
fn record_state(name: &str, state: registry::AgentState, on: &(dyn Fn(&str, Event<'_>) + Sync)) {
    if let Err(e) = control::set_state(state) {
        on(name, Event::Trouble(&e));
    }
}

/// Persist changes only; idle polling must not rewrite an unchanged record.
pub(crate) fn checkpoint(
    saved: Option<&mut progress::Locked>,
    now: Progress,
) -> Result<(), RunError> {
    if let Some(saved) = saved
        && saved.record().is_none_or(|r| r.progress != now)
    {
        saved.checkpoint(now)?;
    }
    Ok(())
}

/// One agent's turns in a row that failed with the model at capacity
/// (friction log item 21): a long resumed session can keep failing so
/// while fresh ones of the same model work. After two, the agent's next
/// turn starts a fresh session. A turn is never retried within itself for
/// capacity, so nothing it did runs twice; its mail waits for the next
/// turn, as any failed turn's does. Any other turn that ran resets the
/// count, and an idle one leaves it. Saved in the run folder after each
/// turn (`capacity/<agent>.json`), so a re-exec, a restart or a run of
/// `--once` passes keeps it.
#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capacity {
    streak: u32,
}

pub fn capacity_path(dir: &Path, name: &str) -> PathBuf {
    dir.join("capacity")
        .join(format!("{}.json", state::file_name(name)))
}

impl Capacity {
    const FRESH_AFTER: u32 = 2;

    /// The agent's saved count; none when there's no record or it can't
    /// be read, which at worst delays a fresh session.
    pub fn load(dir: &Path, name: &str) -> Self {
        fs::read(capacity_path(dir, name))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    /// A count that can't be saved only shifts a fresh session by a turn,
    /// so the caller warns and carries on; the error names the file.
    pub fn save(&self, dir: &Path, name: &str) -> Result<(), RunError> {
        let path = capacity_path(dir, name);
        let failed = |e: std::io::Error| {
            RunError::failure(
                crate::error::CODE_IO,
                format!("can't save the capacity count in {}: {e}", path.display()),
            )
        };
        if self.streak == 0 {
            return match fs::remove_file(&path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(failed(e)),
                _ => Ok(()),
            };
        }
        let tmp = path.with_extension("json.tmp");
        (|| -> std::io::Result<()> {
            fs::create_dir_all(path.parent().unwrap_or(dir))?;
            fs::write(
                &tmp,
                serde_json::to_vec(self).map_err(std::io::Error::other)?,
            )?;
            fs::rename(&tmp, &path)
        })()
        .map_err(failed)
    }

    pub fn fresh_next(&self) -> bool {
        self.streak >= Self::FRESH_AFTER
    }

    /// `fresh` says this turn was the fresh one, which starts the count
    /// again for its new session.
    pub fn after(&mut self, result: &Result<TurnReport, RunError>, fresh: bool) {
        if fresh {
            self.streak = 0;
        }
        match result {
            Err(e) if turn::at_capacity(e) => self.streak += 1,
            Ok(r) if r.outcome == Outcome::Idle => {}
            _ => self.streak = 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use control::{STARTUP, StopOnDrop, bounded_timeout_test, force_readiness_timeout};

    const G: LoopGuard = LoopGuard {
        turns: 3,
        window: Duration::from_secs(60),
    };

    #[test]
    fn a_thread_pauses_at_the_limit_and_runs_again_as_turns_leave_the_window() {
        let mut g = Guard::default();
        for t in [0, 10_000, 20_000] {
            assert!(g.paused(&G, t).is_empty());
            g.record(&G, "c room:r", t);
        }
        assert_eq!(g.paused(&G, 20_000), ["c room:r"]);
        assert_eq!(g.next_cooldown(&G, 20_000), Some(Duration::from_secs(40)));
        // Another thread isn't paused.
        g.record(&G, "d from:x", 20_000);
        assert_eq!(g.paused(&G, 20_000), ["c room:r"]);
        // The first turn leaves the window at 60 s.
        assert!(g.paused(&G, 60_000).is_empty());
        assert_eq!(g.next_cooldown(&G, 60_000), None);
    }

    #[test]
    fn a_person_clears_a_thread_and_the_record_round_trips() {
        let dir = std::env::temp_dir().join(format!("mdkg-guard-{}", uuid::Uuid::now_v7()));
        Guard::update(&dir, "codex:pr1", false, |g| {
            for t in [0, 1, 2] {
                g.record(&G, "c room:r", t);
            }
            Ok(())
        })
        .expect("charge");
        assert!(guard_path(&dir, "codex:pr1").ends_with("guard/codex%3Apr1.json"));
        let back = Guard::read(&dir, "codex:pr1").expect("read");
        assert_eq!(back.paused(&G, 3), ["c room:r"]);
        Guard::update(&dir, "codex:pr1", false, |g| {
            g.clear(Some("c room:r"));
            Ok(())
        })
        .expect("clear");
        assert!(
            Guard::read(&dir, "codex:pr1")
                .expect("read")
                .paused(&G, 3)
                .is_empty()
        );
        // A record that can't be read refuses a charge, and only a person's
        // start-over replaces it.
        fs::write(guard_path(&dir, "codex:pr1"), "{").expect("w");
        assert!(Guard::read(&dir, "codex:pr1").is_err());
        assert!(Guard::update(&dir, "codex:pr1", false, |_| Ok(())).is_err());
        Guard::update(&dir, "codex:pr1", true, |g| {
            g.clear(None);
            Ok(())
        })
        .expect("start over");
        assert!(Guard::read(&dir, "codex:pr1").is_ok());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn concurrent_updates_lose_no_charge() {
        let dir = std::env::temp_dir().join(format!("mdkg-guard-{}", uuid::Uuid::now_v7()));
        let wide = LoopGuard {
            turns: 1000,
            window: Duration::from_secs(3600),
        };
        std::thread::scope(|s| {
            for t in 0..8 {
                let dir = &dir;
                s.spawn(move || {
                    for _ in 0..25 {
                        Guard::update(dir, "a", false, |g| {
                            g.record(&wide, &format!("t{t}"), state::now_ms());
                            Ok(())
                        })
                        .expect("charge");
                    }
                });
            }
        });
        let g = Guard::read(&dir, "a").expect("read");
        assert_eq!(g.threads.values().map(Vec::len).sum::<usize>(), 200);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn two_capacity_failures_in_a_row_make_the_next_turn_fresh() {
        use crate::error::CODE_AGENT_FAILED;
        let at_capacity = || {
            Err(
                RunError::failure(CODE_AGENT_FAILED, "the agent exited with exit status: 1")
                    .and_hint(turn::CAPACITY_HINT)
                    .and_hint("the turn's files are in /tmp/t"),
            )
        };
        let other = || {
            Err(RunError::failure(
                CODE_AGENT_FAILED,
                "the agent exited with exit status: 1",
            ))
        };
        let mut c = Capacity::default();
        c.after(&at_capacity(), false);
        assert!(!c.fresh_next(), "one isn't enough");
        // Any other failure breaks the run.
        c.after(&other(), false);
        c.after(&at_capacity(), false);
        assert!(!c.fresh_next());
        c.after(&at_capacity(), false);
        assert!(c.fresh_next(), "two in a row");
        // The fresh turn starts the count again for its new session, whether
        // it then runs or is at capacity too.
        c.after(&at_capacity(), true);
        assert!(!c.fresh_next());
        c.after(&at_capacity(), false);
        assert!(c.fresh_next());
        c.after(&other(), true);
        assert!(!c.fresh_next());
    }

    #[test]
    fn the_capacity_count_survives_a_restart() {
        use crate::error::CODE_AGENT_FAILED;
        let dir = std::env::temp_dir().join(format!("mdkg-capacity-{}", uuid::Uuid::now_v7()));
        let failed = Err(RunError::failure(
            CODE_AGENT_FAILED,
            "the agent exited with exit status: 1",
        )
        .and_hint(turn::CAPACITY_HINT));
        let mut c = Capacity::load(&dir, "codex:pr19");
        c.after(&failed, false);
        c.save(&dir, "codex:pr19").expect("save");
        // A new process reads it back.
        let mut c = Capacity::load(&dir, "codex:pr19");
        assert!(!c.fresh_next());
        c.after(&failed, false);
        c.save(&dir, "codex:pr19").expect("save");
        assert!(Capacity::load(&dir, "codex:pr19").fresh_next());
        // Reset clears the file.
        let mut c = Capacity::load(&dir, "codex:pr19");
        c.after(&failed, true);
        c.after(&Err(RunError::failure(CODE_AGENT_FAILED, "other")), false);
        c.save(&dir, "codex:pr19").expect("save");
        assert!(!capacity_path(&dir, "codex:pr19").exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_capacity_count_that_cant_be_saved_names_its_file() {
        let dir = std::env::temp_dir().join(format!("mdkg-capacity-{}", uuid::Uuid::now_v7()));
        fs::create_dir_all(&dir).unwrap();
        // A file where the folder belongs: neither a save nor a reset works.
        fs::write(dir.join("capacity"), "").unwrap();
        let mut c = Capacity::default();
        c.after(
            &Err(RunError::failure(crate::error::CODE_AGENT_FAILED, "x")
                .and_hint(turn::CAPACITY_HINT)),
            false,
        );
        let e = c.save(&dir, "a").expect_err("can't save");
        assert!(e.message.contains("capacity/a.json"), "{}", e.message);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn the_cap_is_the_smallest_set_concurrency() {
        let mk = |c: Option<usize>| Supervised {
            name: "a".into(),
            policy: Policy {
                concurrency: c,
                ..Policy::default()
            },
            opts: TurnOptions::default(),
        };
        assert_eq!(cap(&[mk(None), mk(None)]), None);
        assert_eq!(cap(&[mk(Some(3)), mk(None), mk(Some(2))]), Some(2));
    }

    #[derive(Debug)]
    struct BlockedRead {
        entered: std::sync::mpsc::Sender<()>,
        released: std::sync::Arc<Mutex<std::sync::mpsc::Receiver<()>>>,
    }

    #[derive(Debug)]
    struct IdleMail {
        control: Control,
        waited: std::sync::mpsc::Sender<(Option<u64>, u64)>,
        blocked_read: Option<BlockedRead>,
        live_mail: bool,
        claims: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        stop_on_wait: bool,
    }

    impl Mail for IdleMail {
        fn inbox(&mut self, _: &str, _: usize) -> Result<crate::Inbox, RunError> {
            if let Some(blocked) = self.blocked_read.take() {
                let _ = blocked.entered.send(());
                // Dropping the scope's only sender cancels this read on
                // success or unwind; there is no second barrier party to lose.
                let _ = blocked.released.lock().expect("read gate").recv();
            }
            if self.live_mail {
                Ok(crate::Inbox {
                    messages: vec![crate::Message {
                        message_id: "sample-message".into(),
                        seq: 1,
                        context_id: None,
                        kind: "handoff".into(),
                        sender: "sender".into(),
                        via: None,
                        room: None,
                        sent_at: None,
                        digest: String::new(),
                        nodes: Vec::new(),
                        supersedes: Vec::new(),
                        parts: vec![],
                    }],
                    cursor: Some(1),
                    more: false,
                })
            } else {
                Ok(crate::Inbox::default())
            }
        }
        fn wait(
            &mut self,
            _: &str,
            after: Option<u64>,
            seconds: u64,
        ) -> Result<Option<crate::Inbox>, RunError> {
            self.waited
                .send((after, seconds))
                .expect("wait observation");
            if self.stop_on_wait {
                self.control.request_stop();
            } else {
                pause(Duration::from_secs(600));
            }
            Ok(None)
        }
        fn import(&mut self, _: &crate::mail::Import<'_>) -> Result<String, RunError> {
            panic!("unexpected import")
        }
        fn ack(&mut self, _: &str, _: &[String], _: &str) -> Result<(), RunError> {
            panic!("unexpected ack")
        }
        fn claim(&mut self, _: &str) -> Result<(), RunError> {
            self.claims
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Err(RunError::failure("run.agent_failed", "unexpected turn"))
        }
        fn release(&mut self, _: &str) -> Result<(), RunError> {
            panic!("unexpected release")
        }
        fn holder(&self) -> String {
            "test".into()
        }
        fn release_as(&mut self, _: &str, _: &str) -> Result<(), RunError> {
            panic!("unexpected reclaim")
        }
        fn holds(&mut self, _: &str, _: &str, _: Option<&str>) -> Result<bool, RunError> {
            panic!("unexpected holds")
        }
        fn note(&mut self, _: &crate::mail::Note<'_>) -> Result<String, RunError> {
            panic!("unexpected note")
        }
    }

    struct Scratch(PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).ok();
        }
    }

    fn saved_idle_agent() -> (Scratch, Supervised, Up) {
        let path = std::env::temp_dir().join(format!("mdkg-control-{}", uuid::Uuid::now_v7()));
        fs::create_dir_all(&path).expect("scratch");
        let saved = state::resolve(
            "worker",
            None,
            state::SpecFlags {
                harness: Some(crate::Harness::Claude),
                role: Some(crate::Role::Reviewer),
                ..Default::default()
            },
            &path,
        )
        .expect("spec");
        state::save(&path, &saved).expect("save");
        (
            Scratch(path),
            Supervised {
                name: "worker".into(),
                policy: Policy::default(),
                opts: TurnOptions::default(),
            },
            Up {
                wait_secs: 600,
                max_turns: Some(3),
                once: false,
                pause: Duration::from_secs(600),
                fresh: true,
            },
        )
    }

    #[test]
    fn durable_unclaimed_once_returns_without_spinning_or_charging() {
        bounded_test(
            "supervise::tests::durable_unclaimed_once_returns_without_spinning_or_charging",
            || {
                let (scratch, agent, mut how) = saved_idle_agent();
                let dir = scratch.0.join(".git/mdkg/run");
                fs::create_dir_all(&dir).unwrap();
                let state = state::load(&scratch.0, "worker").unwrap().unwrap();
                state::save(&dir, &state).unwrap();
                let mut saved =
                    progress::Locked::open(&dir, uuid::Uuid::now_v7(), "worker").unwrap();
                saved.start(&"a".repeat(64), None).unwrap();
                let before = saved.record().unwrap().clone();
                let control = Control::default();
                let (waited, _) = std::sync::mpsc::channel();
                let claims = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let make = |_: &str| IdleMail {
                    control: control.clone(),
                    waited: waited.clone(),
                    blocked_read: None,
                    live_mail: true,
                    claims: claims.clone(),
                    stop_on_wait: false,
                };
                how.once = true;
                let counts = up_durable(
                    &make,
                    &dir,
                    &[agent],
                    &how,
                    &|_, _| (),
                    &control,
                    std::slice::from_mut(&mut saved),
                )
                .unwrap();
                assert_eq!(counts, [0]);
                assert_eq!(claims.load(std::sync::atomic::Ordering::Relaxed), 1);
                assert_eq!(saved.record().unwrap(), &before);
            },
        );
    }

    #[test]
    fn controlled_wait_preserves_retry_cursor_and_remaining_turn_budget() {
        let (dir, agent, mut how) = saved_idle_agent();
        let progress = Progress {
            ran: 2,
            seen: Some(17),
            check_inbox: false,
        };
        let control = Control::from_progress(BTreeMap::from([("worker".into(), progress)]));
        let (waited, observations) = std::sync::mpsc::channel();
        let claims = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let make = |_: &str| IdleMail {
            control: control.clone(),
            waited: waited.clone(),
            blocked_read: None,
            live_mail: false,
            claims: claims.clone(),
            stop_on_wait: true,
        };
        let ran = up_controlled(
            &make,
            &dir.0,
            std::slice::from_ref(&agent),
            &how,
            &|_, _| (),
            &control,
        )
        .expect("up");
        assert_eq!(ran, [2]);
        assert_eq!(observations.try_recv().expect("wait"), (Some(17), 1));
        assert_eq!(control.progress()["worker"], progress);
        // At the original cap, the replacement process does not reset its
        // budget or inspect the inbox, even if --fresh is still in argv.
        let resumed = Control::from_progress(control.progress());
        how.max_turns = Some(2);
        let make_resumed = |_: &str| IdleMail {
            control: resumed.clone(),
            waited: waited.clone(),
            blocked_read: None,
            live_mail: false,
            claims: claims.clone(),
            stop_on_wait: true,
        };
        assert_eq!(
            up_controlled(&make_resumed, &dir.0, &[agent], &how, &|_, _| (), &resumed)
                .expect("capped"),
            [2]
        );
        assert!(observations.try_recv().is_err());
        assert_eq!(claims.load(std::sync::atomic::Ordering::Relaxed), 0);
    }

    #[test]
    fn a_stop_request_drains_one_agent_and_frees_its_progress_while_a_sibling_runs() {
        bounded_test(
            "supervise::tests::a_stop_request_drains_one_agent_and_frees_its_progress_while_a_sibling_runs",
            || {
                let (scratch, worker, how) = saved_idle_agent();
                let dir = scratch.0.join(".git/mdkg/run");
                fs::create_dir_all(&dir).unwrap();
                let graph = uuid::Uuid::now_v7();
                let mut other = worker.clone();
                other.name = "other".into();
                let mut saved = Vec::new();
                for name in ["other", "worker"] {
                    let mut state = state::load(&scratch.0, "worker").unwrap().unwrap();
                    state.spec.name = name.into();
                    state::save(&dir, &state).unwrap();
                    let mut locked = progress::Locked::open(&dir, graph, name).unwrap();
                    locked.start(&"a".repeat(64), None).unwrap();
                    saved.push(locked);
                }
                let names = ["other".to_string(), "worker".to_string()];
                let supervisor = std::sync::Arc::new(
                    registry::Registered::register(&dir, &names, None, None)
                        .unwrap()
                        .unwrap(),
                );
                let control = Control::default();
                control.set_supervisor(supervisor.clone());
                let (waited, observations) = std::sync::mpsc::channel();
                let claims = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let make = |_: &str| IdleMail {
                    control: control.clone(),
                    waited: waited.clone(),
                    blocked_read: None,
                    live_mail: false,
                    claims: claims.clone(),
                    stop_on_wait: false,
                };
                let (events, stopped) = std::sync::mpsc::channel::<String>();
                let events = Mutex::new(events);
                let on = |name: &str, event: Event<'_>| {
                    if matches!(event, Event::Stopped) {
                        let _ = events.lock().unwrap().send(name.to_string());
                    }
                };
                let agents = [other, worker];
                std::thread::scope(|s| {
                    let _stop = StopOnDrop(control.clone());
                    let run = s.spawn(|| {
                        up_durable(&make, &dir, &agents, &how, &on, &control, &mut saved)
                    });
                    // Both idle in their mail waits.
                    for _ in 0..2 {
                        observations.recv_timeout(STARTUP).expect("idle wait");
                    }
                    let request =
                        stop::Request::drain("nick", timestamp(state::now_ms()), &supervisor.id());
                    stop::write(&dir, "worker", &request).unwrap();
                    // Within a poll, not at the end of its wait.
                    assert_eq!(
                        stopped.recv_timeout(Duration::from_secs(5)).unwrap(),
                        "worker"
                    );
                    assert_eq!(stop::read(&dir, "worker").unwrap(), None);
                    let (found, _) = registry::list(&dir).unwrap();
                    assert_eq!(
                        registry::running(&found, "worker").unwrap().1,
                        registry::AgentState::Stopped
                    );
                    assert_eq!(
                        registry::running(&found, "other").unwrap().1,
                        registry::AgentState::Idle
                    );
                    // The drained agent's watch is finished and free; its
                    // sibling's is still held.
                    let mut again = progress::Locked::open(&dir, graph, "worker").unwrap();
                    assert!(again.record().unwrap().finished);
                    again.start(&"a".repeat(64), None).unwrap();
                    drop(again);
                    assert!(progress::Locked::open(&dir, graph, "other").is_err());
                    assert!(!run.is_finished());
                    control.request_stop();
                    assert_eq!(run.join().unwrap().unwrap(), [0, 0]);
                });
                assert!(saved[1].released());
                assert!(!saved[0].released());
                assert_eq!(claims.load(std::sync::atomic::Ordering::Relaxed), 0);
            },
        );
    }

    #[test]
    fn a_stop_request_for_another_supervisor_is_not_this_ones() {
        let (scratch, worker, _) = saved_idle_agent();
        let dir = scratch.0.join(".git/mdkg/run");
        fs::create_dir_all(&dir).unwrap();
        let supervisor = std::sync::Arc::new(
            registry::Registered::register(&dir, &["worker".into()], None, None)
                .unwrap()
                .unwrap(),
        );
        let control = Control::default();
        control.set_supervisor(supervisor.clone());
        let request = stop::Request::drain("nick", timestamp(0), &uuid::Uuid::now_v7().to_string());
        stop::write(&dir, &worker.name, &request).unwrap();
        control::with_control(Some(&control), || {
            control::with_agent(&worker.name, || {
                assert!(!stop_requested());
                stop::write(
                    &dir,
                    &worker.name,
                    &stop::Request::drain("nick", timestamp(0), &supervisor.id()),
                )
                .unwrap();
                assert!(stop_requested());
                // Only this worker's agent.
                control::with_agent("other", || assert!(!stop_requested()));
            })
        });
        assert!(!control.stopped());
    }

    #[test]
    fn controlled_stop_joins_in_flight_work_before_returning() {
        in_flight_case(false);
    }

    fn in_flight_case(fail_readiness: bool) {
        let (dir, agent, how) = saved_idle_agent();
        let control = Control::default();
        let (entered, reading) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let released = std::sync::Arc::new(Mutex::new(released));
        let (waited, waits) = std::sync::mpsc::channel();
        let claims = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let make = |_: &str| IdleMail {
            control: control.clone(),
            waited: waited.clone(),
            blocked_read: Some(BlockedRead {
                entered: entered.clone(),
                released: released.clone(),
            }),
            live_mail: true,
            claims: claims.clone(),
            stop_on_wait: true,
        };
        std::thread::scope(|s| {
            // Both guards belong to the scope body, so they drop before
            // scope joins. Stop first, then disconnect the blocked read.
            let release = release;
            let _stop = StopOnDrop(control.clone());
            let (finished, returned) = std::sync::mpsc::channel();
            let make = &make;
            let dir = &dir;
            let worker_control = &control;
            s.spawn(move || {
                let result =
                    up_controlled(make, &dir.0, &[agent], &how, &|_, _| (), worker_control);
                let _ = finished.send(result);
            });
            reading.recv_timeout(STARTUP).expect("in-flight read");
            if fail_readiness {
                force_readiness_timeout();
            }
            control.request_stop();
            assert!(matches!(
                returned.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty)
            ));
            drop(release);
            assert_eq!(
                returned.recv_timeout(STARTUP).expect("joined").expect("up"),
                [0]
            );
            assert!(
                waits.try_recv().is_err(),
                "stop must not start another wait"
            );
        });
        assert_eq!(claims.load(std::sync::atomic::Ordering::Relaxed), 0);
    }

    #[test]
    fn in_flight_readiness_timeout_cancels_before_joining() {
        bounded_timeout_test(
            "supervise::tests::in_flight_readiness_timeout_cancels_before_joining",
            || in_flight_case(true),
        );
    }

    #[test]
    fn worker_panic_stops_an_earlier_idle_sibling_and_returns_an_error() {
        bounded_test(
            "supervise::tests::worker_panic_stops_an_earlier_idle_sibling_and_returns_an_error",
            || {
                for controlled in [false, true] {
                    let (dir, agent, how) = saved_idle_agent();
                    let control = Control::default();
                    let (waited, observations) = std::sync::mpsc::channel();
                    let observations = Mutex::new(observations);
                    let claims = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
                    let mut second = agent.clone();
                    second.name = "panicking-worker".into();
                    let make = |name: &str| {
                        if name == second.name {
                            observations
                                .lock()
                                .unwrap()
                                .recv_timeout(STARTUP)
                                .expect("first worker waiting");
                            panic!("fixture worker failure");
                        }
                        IdleMail {
                            control: control.clone(),
                            waited: waited.clone(),
                            blocked_read: None,
                            live_mail: false,
                            claims: claims.clone(),
                            stop_on_wait: false,
                        }
                    };
                    let agents = [agent, second.clone()];
                    let error = if controlled {
                        up_controlled(&make, &dir.0, &agents, &how, &|_, _| (), &control)
                    } else {
                        up(&make, &dir.0, &agents, &how, &|_, _| ())
                    }
                    .unwrap_err();
                    assert_eq!(error, worker_panicked(&second.name));
                    if controlled {
                        assert_eq!(control.failure(), Some(error));
                    }
                    assert_eq!(claims.load(std::sync::atomic::Ordering::Relaxed), 0);
                }
            },
        );
    }

    #[test]
    fn slots_return_capacity_after_unwind_and_cancel_waiters_on_stop() {
        let slots = Slots {
            free: Mutex::new(1),
            freed: Condvar::new(),
        };
        let control = Control::default();
        let panicked = std::panic::catch_unwind(|| {
            control::with_control(Some(&control), || {
                let _permit = slots.take().unwrap();
                panic!("fixture inside slot");
            });
        });
        assert!(panicked.is_err());
        assert_eq!(*slots.free.lock().unwrap(), 1);
        std::thread::scope(|scope| {
            // Drop the holder before scope joins, including readiness failure.
            let _held = slots.take().unwrap();
            let _stop = StopOnDrop(control.clone());
            let (entered, waiting) = std::sync::mpsc::channel();
            let (done, exited) = std::sync::mpsc::channel();
            let worker_control = &control;
            let worker_slots = &slots;
            scope.spawn(move || {
                control::with_control(Some(worker_control), || {
                    entered.send(()).unwrap();
                    let acquired = worker_slots.take().is_some();
                    let _ = done.send(acquired);
                })
            });
            waiting.recv_timeout(STARTUP).unwrap();
            control.request_stop();
            assert!(!exited.recv_timeout(STARTUP).expect("slot waiter cancelled"));
            assert_eq!(*slots.free.lock().unwrap(), 0);
        });
        assert_eq!(*slots.free.lock().unwrap(), 1);
    }

    #[test]
    fn fatal_stop_still_joins_in_flight_work_before_returning_error() {
        bounded_test(
            "supervise::tests::fatal_stop_still_joins_in_flight_work_before_returning_error",
            || {
                let (dir, agent, how) = saved_idle_agent();
                let control = Control::default();
                let (entered, reading) = std::sync::mpsc::channel();
                let reading = Mutex::new(reading);
                let (release, released) = std::sync::mpsc::channel::<()>();
                let released = std::sync::Arc::new(Mutex::new(released));
                let (waited, _) = std::sync::mpsc::channel();
                let claims = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let mut second = agent.clone();
                second.name = "panicking-worker".into();
                let make = |name: &str| {
                    if name == second.name {
                        reading
                            .lock()
                            .unwrap()
                            .recv_timeout(STARTUP)
                            .expect("active sibling");
                        panic!("fixture worker failure");
                    }
                    IdleMail {
                        control: control.clone(),
                        waited: waited.clone(),
                        blocked_read: Some(BlockedRead {
                            entered: entered.clone(),
                            released: released.clone(),
                        }),
                        live_mail: true,
                        claims: claims.clone(),
                        stop_on_wait: true,
                    }
                };
                std::thread::scope(|scope| {
                    let release = release;
                    let _stop = StopOnDrop(control.clone());
                    let (finished, returned) = std::sync::mpsc::channel();
                    let make = &make;
                    let worker_control = &control;
                    let dir = &dir;
                    let agents = [agent, second.clone()];
                    scope.spawn(move || {
                        let result =
                            up_controlled(make, &dir.0, &agents, &how, &|_, _| (), worker_control);
                        let _ = finished.send(result);
                    });
                    let deadline = std::time::Instant::now() + STARTUP;
                    while control.failure().is_none() {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "fatal stop not published"
                        );
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    assert!(matches!(
                        returned.try_recv(),
                        Err(std::sync::mpsc::TryRecvError::Empty)
                    ));
                    drop(release);
                    assert_eq!(
                        returned.recv_timeout(STARTUP).unwrap().unwrap_err(),
                        worker_panicked(&second.name)
                    );
                });
                assert_eq!(claims.load(std::sync::atomic::Ordering::Relaxed), 0);
            },
        );
    }
}
