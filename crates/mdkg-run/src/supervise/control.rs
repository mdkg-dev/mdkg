//! Cooperative stop at loop boundaries, with progress carried across exec.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

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
    progress: BTreeMap<String, Progress>,
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
                progress,
            }),
            Condvar::new(),
        )))
    }

    pub fn request_stop(&self) {
        let (shared, wake) = &*self.0;
        shared.lock().unwrap_or_else(|e| e.into_inner()).stopped = true;
        wake.notify_all();
    }

    pub fn stopped(&self) -> bool {
        self.0.0.lock().unwrap_or_else(|e| e.into_inner()).stopped
    }

    pub fn progress(&self) -> BTreeMap<String, Progress> {
        self.0
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .progress
            .clone()
    }

    fn pause(&self, duration: Duration) {
        let (shared, wake) = &*self.0;
        let held = shared.lock().unwrap_or_else(|e| e.into_inner());
        let _held = wake
            .wait_timeout_while(held, duration, |s| !s.stopped)
            .unwrap_or_else(|e| e.into_inner());
    }
}

thread_local! {
    static CURRENT: RefCell<Option<Control>> = const { RefCell::new(None) };
}

pub(super) fn with_control<T>(control: Option<&Control>, run: impl FnOnce() -> T) -> T {
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
/// after a reminder reservation has been made.
pub fn stop_requested() -> bool {
    CURRENT.with(|c| c.borrow().as_ref().is_some_and(Control::stopped))
}

/// Controlled idle waits poll at least once per second so a stopped process
/// can join promptly. The mail cursor still decides which messages are new.
pub fn wait_secs(requested: u64) -> u64 {
    CURRENT.with(|c| {
        if c.borrow().is_some() {
            requested.min(1)
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
    const CHILD: &str = "MDKG_TEST_CONTROL_TIMEOUT";
    if std::env::var(CHILD).is_ok_and(|v| v == name) {
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(test))
            .expect_err("the forced readiness failure must propagate");
        let message = panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&str>().copied())
            .unwrap_or_default();
        assert!(message.contains("forced readiness timeout"), "{message}");
        return;
    }
    let mut child = std::process::Command::new(std::env::current_exe().expect("test executable"))
        .args(["--exact", name, "--test-threads=1"])
        .env(CHILD, name)
        .stdout(std::process::Stdio::null())
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
                    "timeout regression child failed: {}",
                    String::from_utf8_lossy(&output.stderr)
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
    use super::*;

    #[test]
    fn control_is_scoped_per_thread_and_restored_after_unwind() {
        assert_eq!(wait_secs(600), 600);
        let c = Control::default();
        c.request_stop();
        let _ = std::panic::catch_unwind(|| {
            with_control(Some(&c), || {
                assert!(stop_requested());
                assert_eq!(wait_secs(600), 1);
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
