//! A started `serve`'s threads, its timer, and its stop (S3, R5).
//!
//! - **The accept loop** on the local socket.
//! - **The timer:** every 10 s the lock and folder checks, which stop
//!   `serve` when one fails. serve-local keeps no other in-memory state
//!   for a timer to tend.
//! - **The stop:** the first SIGTERM or SIGINT drains, with
//!   `serve.drain_grace_s`, for as long as an agent turn runs on this
//!   repository; a second signal ends the grace. Requests under way get
//!   10 s. A replaced program (see `binary`) drains the same way. Then
//!   the socket and `serve.json` go, before the lock is
//!   released (`finish_drain`). A lost lock or folder stops `serve` at
//!   once and removes nothing, since the files at those names may be
//!   another `serve`'s. A listener or timer that ends on its own (an error
//!   or a panic) stops `serve` too, with exit 4.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use mdkg_graph::ErrorKind;
use mdkg_serve::listen::finish_drain;
use mdkg_wire::codec::Http1;
use serde_json::json;

use super::local::{Drained, Limits, Local};
use super::log::line;
use super::start::Ready;

/// How often the main thread looks for a signal or a lost lock.
const POLL: Duration = Duration::from_millis(100);

/// SIGTERM and SIGINT, as one flag the handlers set (signal-hook's safe
/// flag API, as `agents up` uses it).
#[derive(Debug, Clone)]
pub struct Signals {
    flag: Arc<AtomicBool>,
}

impl Signals {
    /// Registers SIGTERM and SIGINT. Neither then takes its default
    /// action: the first drains, the second ends the grace.
    pub fn install() -> Self {
        use signal_hook::consts::{SIGINT, SIGTERM};
        let flag = Arc::new(AtomicBool::new(false));
        for sig in [SIGTERM, SIGINT] {
            if let Err(e) = signal_hook::flag::register(sig, flag.clone()) {
                line(
                    "warn",
                    "serve.signal_unregistered",
                    json!({"signal": sig, "error": e.to_string()}),
                );
            }
        }
        Self { flag }
    }

    /// A flag the caller sets in place of a signal (tests).
    #[cfg(test)]
    pub fn manual(flag: Arc<AtomicBool>) -> Self {
        Self { flag }
    }

    /// Whether a signal came since the last look.
    fn take(&self) -> bool {
        self.flag.swap(false, Ordering::SeqCst)
    }
}

/// Why the wait ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    Signal,
    /// serve's program was replaced: drained as for a signal, so the
    /// restart runs the new build.
    Rebuilt,
    /// The lock, `serve.live` or the folder isn't ours any more.
    Lost,
    /// A listener or the timer ended on its own.
    Failed(&'static str),
}

/// What the threads report to the main one.
#[derive(Default)]
struct Flags {
    lost: AtomicBool,
    failed: std::sync::Mutex<Option<&'static str>>,
}

impl Flags {
    fn fail(&self, why: &'static str) {
        let mut f = self.failed.lock().unwrap_or_else(|e| e.into_inner());
        f.get_or_insert(why);
    }

    fn failed(&self) -> Option<&'static str> {
        *self.failed.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn urgent(&self) -> bool {
        self.lost.load(Ordering::SeqCst) || self.failed().is_some()
    }
}

/// Whether an agent turn runs on this repository: a launcher's turn
/// record under `<common>/mdkg/hooks/turns/` is locked while its turn
/// runs. The drain's grace lasts while one does (S3 step 2). A folder
/// that can't be read counts as yes, so a finishing turn keeps its grace.
pub fn turns_running(common: &Path) -> bool {
    let entries = match std::fs::read_dir(mdkg_run::hooks::dir(common).join("turns")) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return false,
        Err(_) => return true,
    };
    for entry in entries {
        let Ok(entry) = entry else { return true };
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Ok(file) = std::fs::File::open(&path) else {
            continue;
        };
        match file.try_lock_shared() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => return true,
            Err(_) => return true,
        }
    }
    false
}

/// Runs a started `serve` until it stops; returns the exit code.
pub fn run(ready: Ready, signals: &Signals) -> u8 {
    let Ready {
        lock,
        dispatch,
        unix,
        tcp,
        token,
        turn,
        binary,
        socket,
        common,
        state,
        settings,
        ..
    } = ready;
    let local = Local::new(
        dispatch,
        turn,
        Arc::new(Http1),
        Limits::from(&settings.limits),
        token,
    );
    let stop = Arc::new(AtomicBool::new(false));
    let ticking = AtomicBool::new(true);
    let flags = Flags::default();
    let drain_done = AtomicBool::new(false);
    line(
        "info",
        "serve.started",
        json!({"serve_id": state.serve_id, "socket": state.socket, "tcp": state.tcp}),
    );
    let (why, drained) = std::thread::scope(|s| {
        {
            let (local, stop, flags) = (local.clone(), stop.clone(), &flags);
            s.spawn(move || {
                // A panic ends the listener as an error does: caught, so
                // serve stops and cleans up instead of re-raising it at the
                // scope's end.
                let ended = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    local.run_unix(unix, stop.clone())
                }));
                listener_ended(ended, "serve.socket_failed");
                // A listener that ends before the stop serves nobody.
                if !stop.load(Ordering::SeqCst) {
                    flags.fail("socket_listener_ended");
                }
            });
        }
        if let Some(listener) = tcp {
            let (local, stop, flags) = (local.clone(), stop.clone(), &flags);
            s.spawn(move || {
                let ended = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    local.run_tcp(listener, stop.clone())
                }));
                listener_ended(ended, "serve.tcp_failed");
                if !stop.load(Ordering::SeqCst) {
                    flags.fail("tcp_listener_ended");
                }
            });
        }
        s.spawn(|| {
            while sleep_while(&ticking, Duration::from_secs(10)) {
                let held = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    matches!(lock.still_held(), Ok(true))
                }));
                match held {
                    Ok(true) => {}
                    Ok(false) => {
                        line(
                            "error",
                            "serve.lock_lost",
                            json!({"hint": "the serve lock, serve.live or <common>/mdkg isn't the one this serve took"}),
                        );
                        flags.lost.store(true, Ordering::SeqCst);
                        return;
                    }
                    Err(_) => {
                        flags.fail("timer_panicked");
                        return;
                    }
                }
            }
        });
        let why = loop {
            if signals.take() {
                break Stop::Signal;
            }
            // A stat or two per tick: an idle serve drains on a
            // reinstall as a busy one does.
            if binary.replaced() {
                break Stop::Rebuilt;
            }
            if flags.lost.load(Ordering::SeqCst) {
                break Stop::Lost;
            }
            if let Some(why) = flags.failed() {
                break Stop::Failed(why);
            }
            std::thread::sleep(POLL);
        };
        let grace = match why {
            Stop::Rebuilt | Stop::Signal => {
                if why == Stop::Rebuilt {
                    line(
                        "warn",
                        "serve.binary_changed",
                        json!({"hint": "serve's program was replaced; restart serve to run the new build"}),
                    );
                }
                line(
                    "info",
                    "serve.draining",
                    json!({"grace_s": settings.drain_grace.as_secs()}),
                );
                settings.drain_grace
            }
            Stop::Lost | Stop::Failed(_) => Duration::ZERO,
        };
        s.spawn(|| {
            let mut ended = false;
            while !drain_done.load(Ordering::SeqCst) {
                // A second signal, a lost lock or a dead thread ends the
                // grace at once.
                if (signals.take() || flags.urgent()) && !ended {
                    ended = true;
                    line("info", "serve.grace_ended", json!({}));
                    local.end_grace();
                }
                std::thread::sleep(POLL);
            }
        });
        let turns = || turns_running(&common);
        let drained = local.drain(grace, &turns, &stop);
        drain_done.store(true, Ordering::SeqCst);
        ticking.store(false, Ordering::SeqCst);
        (why, drained)
    });
    // Only a serve that still holds its names removes them.
    if flags.lost.load(Ordering::SeqCst) || !matches!(lock.still_held(), Ok(true)) {
        drop(lock);
        line("error", "serve.stopped", json!({"reason": "lock_lost"}));
        return ErrorKind::Failure.exit_code();
    }
    let finished = finish_drain(lock, &socket);
    if let Err(e) = &finished {
        line(
            "error",
            "serve.cleanup_failed",
            json!({"error": e.to_string()}),
        );
    }
    let failed = flags.failed();
    let clean = matches!(why, Stop::Signal | Stop::Rebuilt)
        && drained == Drained::Clean
        && finished.is_ok();
    line(
        "info",
        "serve.stopped",
        json!({"clean": clean, "cut_off": drained == Drained::CutOff, "failed": failed}),
    );
    if clean {
        0
    } else {
        ErrorKind::Failure.exit_code()
    }
}

/// Logs how a listener's accept loop ended, when it ended in trouble.
fn listener_ended(ended: std::thread::Result<std::io::Result<()>>, event: &str) {
    match ended {
        Ok(Ok(())) => {}
        Ok(Err(e)) => line("error", event, json!({"error": e.to_string()})),
        Err(_) => line("error", event, json!({"error": "panicked"})),
    }
}

/// Sleeps `total` in short steps while `flag` holds; false once it
/// doesn't. The clock, not a count of steps, says when `total` is up: a
/// process at a lowered priority oversleeps each step, and counting steps
/// stretched the 10 s lock check to 20 s and more (#15897).
fn sleep_while(flag: &AtomicBool, total: Duration) -> bool {
    sleep_while_with(flag, total, Instant::now, std::thread::sleep)
}

/// [`sleep_while`] with its clock and sleeper given, so a test can make
/// each step oversleep on a clock of its own.
fn sleep_while_with(
    flag: &AtomicBool,
    total: Duration,
    now: impl Fn() -> Instant,
    mut sleep: impl FnMut(Duration),
) -> bool {
    let deadline = now() + total;
    loop {
        if !flag.load(Ordering::SeqCst) {
            return false;
        }
        let left = deadline.saturating_duration_since(now());
        if left.is_zero() {
            return true;
        }
        sleep(left.min(Duration::from_millis(100)));
    }
}

#[cfg(test)]
mod sleep_tests {
    use super::*;
    use std::cell::Cell;

    /// Each step oversleeps by 150 ms on a fake clock, as at a lowered
    /// priority: the 1 s wait ends at 1 s on that clock, after 4 steps,
    /// where counting steps took 10 steps and 2.5 s (#15897). No real
    /// sleep, so scheduling can't move it.
    #[test]
    fn sleep_while_ends_on_the_clock_when_each_step_oversleeps() {
        let flag = AtomicBool::new(true);
        let start = Instant::now();
        let clock = Cell::new(start);
        let steps = Cell::new(0u32);
        let held = sleep_while_with(
            &flag,
            Duration::from_secs(1),
            || clock.get(),
            |d| {
                steps.set(steps.get() + 1);
                clock.set(clock.get() + d + Duration::from_millis(150));
            },
        );
        assert!(held, "the flag held");
        assert_eq!(steps.get(), 4);
        assert_eq!(clock.get() - start, Duration::from_secs(1));
    }

    #[test]
    fn sleep_while_stops_once_the_flag_drops() {
        let flag = AtomicBool::new(false);
        assert!(!sleep_while_with(
            &flag,
            Duration::from_secs(10),
            Instant::now,
            |_| panic!("no sleep once the flag is down")
        ));
    }
}
