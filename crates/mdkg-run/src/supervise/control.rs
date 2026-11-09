//! Cooperative stop at loop boundaries, with progress carried across exec.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use super::registry::{AgentState, Registered};
use crate::RunError;
use serde::{Deserialize, Serialize};

/// Watch progress, independent of the agent's lifetime cursor/session state.
/// Keeping it across exec preserves the turn budget and a failed turn's wait
/// for newer mail. `ran` also prevents repeating a first-turn `--fresh`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Progress {
    pub ran: u64,
    pub seen: Option<u64>,
    pub check_inbox: bool,
}

impl Default for Progress {
    fn default() -> Self {
        Self {
            ran: 0,
            seen: None,
            check_inbox: true,
        }
    }
}

#[derive(Debug, Default)]
struct Shared {
    stopped: bool,
    failure: Option<RunError>,
    progress: BTreeMap<String, Progress>,
    /// The registered supervisor, whose agents `agents stop` drains one
    /// at a time.
    supervisor: Option<Arc<Registered>>,
    /// A flag a signal handler sets ([`Control::stop_on`]): read on every
    /// [`Control::stopped`], so a stop is seen as soon as the handler runs.
    external: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// Why each agent's worker returned, when it said ([`note_end`]).
    ends: BTreeMap<String, End>,
    /// What a long-running watch restarts on, read again before each turn
    /// is admitted ([`admit_turn`]).
    watch: Option<Arc<super::reload::Watch>>,
    /// The first reason that check found: the watch drains and re-execs.
    restart: Option<super::reload::Reason>,
}

/// Why a worker returned on its own, not on a stop: kept so the
/// supervisor's exit record names its own reason (#20977).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    /// Its trigger is `manual`: a watch runs it no turns.
    Manual,
    /// It ran its `--max-turns`.
    Cap,
    /// The agent is no longer known.
    UnknownAgent,
    /// Its receiver mode changed (interactive to headless or back).
    ModeChanged,
}

impl Shared {
    /// Stopped by a request, or by the external flag (read each time).
    fn is_stopped(&self) -> bool {
        self.stopped
            || self
                .external
                .as_ref()
                .is_some_and(|f| f.load(std::sync::atomic::Ordering::SeqCst))
    }
}

/// Shared by the watcher and each supervisor thread. Stopping never signals
/// a harness or interrupts a queue reservation; workers finish that work and
/// return at the next loop boundary.
#[derive(Debug, Clone, Default)]
pub struct Control(Arc<(Mutex<Shared>, Condvar)>);

impl Control {
    pub fn from_progress(progress: BTreeMap<String, Progress>) -> Self {
        Self(Arc::new((
            Mutex::new(Shared {
                stopped: false,
                failure: None,
                progress,
                supervisor: None,
                external: None,
                ends: BTreeMap::new(),
                watch: None,
                restart: None,
            }),
            Condvar::new(),
        )))
    }

    /// Lets each agent's worker drain on its own stop request
    /// ([`super::stop`]) and record its state in the supervisor's record.
    pub fn set_supervisor(&self, supervisor: Arc<Registered>) {
        self.0
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .supervisor = Some(supervisor);
    }

    /// Why `agent`'s worker returned on its own, if it said.
    pub fn ended(&self, agent: &str) -> Option<End> {
        self.0
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .ends
            .get(agent)
            .copied()
    }

    pub fn supervisor(&self) -> Option<Arc<Registered>> {
        self.0
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .supervisor
            .clone()
    }

    pub fn request_stop(&self) {
        let (shared, wake) = &*self.0;
        shared.lock().unwrap_or_else(|e| e.into_inner()).stopped = true;
        wake.notify_all();
    }

    /// Stops when `flag` is set (a SIGTERM handler's), with no copy step:
    /// every [`Control::stopped`] and [`stop_requested`] reads it, so no
    /// worker is admitted to a turn once the handler has run.
    pub fn stop_on(&self, flag: Arc<std::sync::atomic::AtomicBool>) {
        let (shared, wake) = &*self.0;
        shared.lock().unwrap_or_else(|e| e.into_inner()).external = Some(flag);
        wake.notify_all();
    }

    /// Checks `watch` before admitting each turn ([`admit_turn`]).
    pub fn restart_on(&self, watch: Arc<super::reload::Watch>) {
        self.0.0.lock().unwrap_or_else(|e| e.into_inner()).watch = Some(watch);
    }

    /// Why a check before a turn stopped this supervisor to restart, if
    /// one did.
    pub fn restart(&self) -> Option<super::reload::Reason> {
        self.0
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .restart
            .clone()
    }

    /// Reads the watch now. A change stops admitting work, as the
    /// watcher's own poll does, and is kept for [`Control::restart`]. An
    /// authority that can't be read is the error.
    fn stale(&self) -> Result<Option<super::reload::Reason>, RunError> {
        let Some(watch) = self
            .0
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .watch
            .clone()
        else {
            return Ok(None);
        };
        // Read without the lock: a full read runs git.
        let Some(reason) = watch.changed_now()? else {
            return Ok(None);
        };
        let (shared, wake) = &*self.0;
        let mut shared = shared.lock().unwrap_or_else(|e| e.into_inner());
        shared.restart.get_or_insert(reason.clone());
        shared.stopped = true;
        wake.notify_all();
        Ok(Some(reason))
    }

    pub fn stopped(&self) -> bool {
        self.0
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_stopped()
    }

    /// Stop admitting work and retain the first fatal reason. This differs
    /// from a normal binary-change stop: after draining, the caller must
    /// return this error and must not exec a replacement process.
    pub fn fail(&self, error: RunError) {
        let (shared, wake) = &*self.0;
        let mut shared = shared.lock().unwrap_or_else(|e| e.into_inner());
        shared.failure.get_or_insert(error);
        shared.stopped = true;
        wake.notify_all();
    }

    pub fn failure(&self) -> Option<RunError> {
        self.0
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .failure
            .clone()
    }

    pub fn progress(&self) -> BTreeMap<String, Progress> {
        self.0
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .progress
            .clone()
    }

    /// Waits up to `duration`, waking on a stop, and on the agent's own
    /// stop request within [`DRAIN_POLL`].
    fn pause(&self, duration: Duration) {
        let (shared, wake) = &*self.0;
        let until = std::time::Instant::now() + duration;
        loop {
            let left = until.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() || drain_requested() {
                return;
            }
            let held = shared.lock().unwrap_or_else(|e| e.into_inner());
            let (held, _) = wake
                .wait_timeout_while(held, left.min(DRAIN_POLL), |s| !s.is_stopped())
                .unwrap_or_else(|e| e.into_inner());
            if held.is_stopped() {
                return;
            }
        }
    }
}

/// How often a pause looks for the agent's stop request.
const DRAIN_POLL: Duration = Duration::from_millis(250);

thread_local! {
    static CURRENT: RefCell<Option<Control>> = const { RefCell::new(None) };
    /// The agent this worker thread supervises.
    static AGENT: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Runs `run` as `agent`'s worker: [`stop_requested`] then also answers
/// for the agent's own stop request.
pub(crate) fn with_agent<T>(agent: &str, run: impl FnOnce() -> T) -> T {
    struct Restore(Option<String>);
    impl Drop for Restore {
        fn drop(&mut self) {
            AGENT.with(|a| *a.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(AGENT.with(|a| a.replace(Some(agent.into()))));
    run()
}

/// This worker's agent and its supervisor, when both are known.
fn agent_supervisor() -> Option<(String, Arc<Registered>)> {
    let agent = AGENT.with(|a| a.borrow().clone())?;
    let supervisor = CURRENT.with(|c| c.borrow().as_ref().and_then(Control::supervisor))?;
    Some((agent, supervisor))
}

/// Whether `agents stop` asked this worker's supervisor to drain its agent.
pub fn drain_requested() -> bool {
    agent_supervisor().is_some_and(|(agent, supervisor)| {
        super::stop::requested(supervisor.run_dir(), &agent, &supervisor.id())
    })
}

/// Notes why this worker's agent is returning on its own ([`End`]).
pub(crate) fn note_end(end: End) {
    let Some(agent) = AGENT.with(|a| a.borrow().clone()) else {
        return;
    };
    CURRENT.with(|c| {
        if let Some(control) = c.borrow().as_ref() {
            control
                .0
                .0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .ends
                .insert(agent, end);
        }
    });
}

/// The id of the supervisor this thread works for, if it registered.
pub fn supervisor_id() -> Option<String> {
    CURRENT
        .with(|c| c.borrow().as_ref().and_then(Control::supervisor))
        .map(|s| s.id())
}

/// Records this worker's agent's state in its supervisor's record.
pub(crate) fn set_state(state: AgentState) -> Result<(), RunError> {
    match agent_supervisor() {
        Some((agent, supervisor)) => supervisor.set(&agent, state),
        None => Ok(()),
    }
}

pub(crate) fn with_control<T>(control: Option<&Control>, run: impl FnOnce() -> T) -> T {
    struct Restore(Option<Control>);
    impl Drop for Restore {
        fn drop(&mut self) {
            CURRENT.with(|c| *c.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(CURRENT.with(|c| c.replace(control.cloned())));
    run()
}

/// Called only between turns/queue dispatches. Never use it to abort work
/// after a reminder reservation has been made. True on a stop of the whole
/// supervisor, and once `agents stop` asked it to drain this worker's agent.
pub fn stop_requested() -> bool {
    CURRENT.with(|c| c.borrow().as_ref().is_some_and(Control::stopped)) || drain_requested()
}

/// Whether this worker may start a turn now.
#[derive(Debug)]
pub(crate) enum Admit {
    /// No stop was asked, and its supervisor's committed definitions,
    /// binary and store are those it runs on.
    Yes,
    /// A stop was asked, or a change was found here. A change stops the
    /// supervisor, which finishes running turns and re-execs on the new
    /// definitions, so the mail that woke this worker starts its turn
    /// there.
    Stop,
    /// The committed definitions couldn't be read: the turn waits and is
    /// tried again, rather than starting from one that may be stale.
    Unreadable(RunError),
}

/// Called once per turn, just before it is admitted ([`Admit`]).
pub(crate) fn admit_turn() -> Admit {
    if stop_requested() {
        return Admit::Stop;
    }
    let Some(control) = CURRENT.with(|c| c.borrow().clone()) else {
        return Admit::Yes;
    };
    match control.stale() {
        Ok(None) => Admit::Yes,
        Ok(Some(_)) => Admit::Stop,
        Err(e) => Admit::Unreadable(e),
    }
}

/// Controlled waits have a thirty-second floor even with a short requested
/// poll interval. Local cancellation still notices stop/drain promptly.
pub fn wait_secs(requested: u64) -> u64 {
    CURRENT.with(|c| {
        if c.borrow().is_some() {
            requested.max(30)
        } else {
            requested
        }
    })
}

/// Retry backoff that wakes immediately on a cooperative stop.
pub fn pause(duration: Duration) {
    let control = CURRENT.with(|c| c.borrow().clone());
    match control {
        Some(c) => c.pause(duration),
        None => std::thread::sleep(duration),
    }
}

pub fn load_progress(agent: &str) -> Progress {
    CURRENT.with(|c| {
        c.borrow()
            .as_ref()
            .and_then(|c| c.progress().get(agent).copied())
            .unwrap_or_default()
    })
}

/// Update the in-process snapshot only. Durable callers must first persist
/// their progress and call `Control::fail` if persistence or its protocol fails;
/// this snapshot must never turn such an error into a successful drain/exec.
pub fn save_progress(agent: &str, progress: Progress) {
    CURRENT.with(|c| {
        if let Some(c) = c.borrow().as_ref() {
            c.0.0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .progress
                .insert(agent.into(), progress);
        }
    });
}

#[cfg(test)]
pub(super) const STARTUP: Duration = Duration::from_secs(30);

/// Stop before the scope starts joining, even when a readiness assertion fails.
#[cfg(test)]
pub(super) struct StopOnDrop(pub(super) Control);

#[cfg(test)]
impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.request_stop();
    }
}

#[cfg(test)]
pub(super) fn force_readiness_timeout() {
    let (_pending, ready) = std::sync::mpsc::channel::<()>();
    ready
        .recv_timeout(Duration::ZERO)
        .expect("forced readiness timeout");
}

/// A broken cleanup must fail this regression without hanging its parent suite.
#[cfg(test)]
pub(super) fn bounded_timeout_test(name: &str, test: impl FnOnce()) {
    bounded_test(name, || {
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(test))
            .expect_err("the forced readiness failure must propagate");
        let message = panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&str>().copied())
            .unwrap_or_default();
        assert!(message.contains("forced readiness timeout"), "{message}");
    });
}

#[cfg(test)]
pub(crate) fn bounded_test(name: &str, test: impl FnOnce()) {
    const CHILD: &str = "MDKG_TEST_CONTROL_TIMEOUT";
    if std::env::var(CHILD).is_ok_and(|v| v == name) {
        test();
        return;
    }
    let mut child = std::process::Command::new(std::env::current_exe().expect("test executable"))
        .args(["--exact", name, "--test-threads=1"])
        .env(CHILD, name)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("timeout regression child");
    let deadline = std::time::Instant::now() + Duration::from_secs(45);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let output = child.wait_with_output().expect("child output");
                assert!(
                    status.success(),
                    "timeout regression child failed: {}\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                let stdout = String::from_utf8_lossy(&output.stdout);
                let mut summaries = stdout.lines().filter(|l| l.starts_with("test result: "));
                assert!(
                    summaries.next().is_some_and(|line| line.starts_with(
                        "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured;"
                    )) && summaries.next().is_none(),
                    "timeout regression child did not pass exactly one test: {stdout}"
                );
                return;
            }
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            result => {
                let _ = child.kill();
                let output = child.wait_with_output().expect("reap timeout child");
                panic!(
                    "timeout cleanup did not finish: {result:?}; {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    /// A set external flag (a SIGTERM handler's) stops the control at once,
    /// with no copy step, for `stopped()` and a worker's `stop_requested()`
    /// (review of 5d8b5299).
    #[test]
    fn an_external_flag_stops_at_once() {
        let control = Control::default();
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        control.stop_on(flag.clone());
        assert!(!control.stopped());
        let seen = std::thread::scope(|s| {
            s.spawn(|| {
                with_control(Some(&control), || {
                    let before = stop_requested();
                    flag.store(true, std::sync::atomic::Ordering::SeqCst);
                    (before, stop_requested())
                })
            })
            .join()
            .unwrap()
        });
        assert_eq!(seen, (false, true));
        assert!(control.stopped());
    }

    use super::*;

    #[test]
    fn bounded_test_rejects_a_filter_that_runs_no_tests() {
        let error = std::panic::catch_unwind(|| {
            bounded_test("supervise::control::tests::missing_watchdog_test", || {
                panic!("a nonexistent test cannot run this closure");
            });
        })
        .expect_err("a successful child with zero tests must be refused");
        let message = error
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| error.downcast_ref::<&str>().copied())
            .unwrap_or_default();
        assert!(
            message.contains("did not pass exactly one test"),
            "{message}"
        );
    }

    #[test]
    fn fatal_error_survives_normal_stop_and_later_failures() {
        let c = Control::default();
        c.request_stop();
        let first = RunError::failure("run.io_failed", "durable completion failed");
        c.fail(first.clone());
        c.fail(RunError::failure("run.agent_failed", "later panic"));
        c.request_stop();
        assert!(c.stopped());
        assert_eq!(c.failure(), Some(first.clone()));
        assert_eq!(c.clone().failure(), Some(first));
    }

    #[test]
    fn control_is_scoped_per_thread_and_restored_after_unwind() {
        assert_eq!(wait_secs(600), 600);
        let c = Control::default();
        c.request_stop();
        let _ = std::panic::catch_unwind(|| {
            with_control(Some(&c), || {
                assert!(stop_requested());
                assert_eq!(wait_secs(600), 600);
                assert_eq!(wait_secs(1), 30);
                assert_eq!(wait_secs(0), 30);
                assert!(!std::thread::spawn(stop_requested).join().expect("child"));
                panic!("restore test");
            })
        });
        assert!(!stop_requested());
        assert_eq!(wait_secs(600), 600);
    }

    #[test]
    fn stopping_wakes_backoff_and_keeps_restart_progress() {
        let c = backoff_case(false);
        let next = Control::from_progress(c.progress());
        with_control(Some(&next), || {
            assert!(!stop_requested());
            assert_eq!(
                load_progress("worker"),
                Progress {
                    ran: 2,
                    seen: Some(17),
                    check_inbox: false
                }
            );
        });
    }

    fn backoff_case(fail_readiness: bool) -> Control {
        let c = Control::default();
        std::thread::scope(|s| {
            let _stop = StopOnDrop(c.clone());
            let (ready, waiting) = std::sync::mpsc::channel();
            let worker_control = c.clone();
            s.spawn(move || {
                with_control(Some(&worker_control), || {
                    save_progress(
                        "worker",
                        Progress {
                            ran: 2,
                            seen: Some(17),
                            check_inbox: false,
                        },
                    );
                    ready.send(()).expect("ready");
                    pause(Duration::from_secs(600));
                    assert!(stop_requested());
                })
            });
            waiting.recv_timeout(STARTUP).expect("waiting");
            if fail_readiness {
                force_readiness_timeout();
            }
            c.request_stop();
        });
        c
    }

    #[test]
    fn backoff_readiness_timeout_stops_before_joining() {
        bounded_timeout_test(
            "supervise::control::tests::backoff_readiness_timeout_stops_before_joining",
            || {
                backoff_case(true);
            },
        );
    }
}
