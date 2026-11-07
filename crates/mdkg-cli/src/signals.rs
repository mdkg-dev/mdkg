//! SIGTERM and SIGINT reach the git mdkg started (o-omni item 1).
//!
//! Each git runs in its own process group, so a signal sent to mdkg alone
//! would leave it running, and a write could land after the caller that
//! killed mdkg reported the outcome unknown. For a one-shot command, a
//! SIGTERM or SIGINT is forwarded to every git group mdkg started
//! ([`mdkg_git::children::stop`]): they get
//! [`mdkg_git::children::TERM_GRACE`] to exit, then are killed and reaped
//! within [`mdkg_git::children::KILL_GRACE`], and mdkg then ends by that
//! signal, as it would have without a handler. When both have arrived by
//! the time the forwarder looks, SIGTERM wins. A later signal during the
//! wait changes nothing: the wait is already bounded.
//!
//! Commands that take a signal over for a drain of their own don't forward
//! it (see [`Forwarded`]): their drain lets each git finish.

use std::io::Read as _;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use signal_hook::consts::{SIGINT, SIGTERM};

/// Which signals a command forwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Forwarded {
    /// Both: a one-shot command.
    Both,
    /// Only SIGINT: continuous `agents up`, which drains on SIGTERM.
    InterruptOnly,
}

/// Installs the forwarder. A registration that fails leaves that signal's
/// default action, as before, with a warning.
pub(crate) fn forward_to_git(which: Forwarded) {
    let (wake, waker) = match UnixStream::pair() {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("warning: SIGTERM and SIGINT won't reach git: {e}");
            return;
        }
    };
    let signals: &[i32] = match which {
        Forwarded::Both => &[SIGTERM, SIGINT],
        Forwarded::InterruptOnly => &[SIGINT],
    };
    let mut caught = Vec::new();
    for &signal in signals {
        match register(signal, &waker) {
            Ok(flag) => caught.push((signal, flag)),
            Err(e) => eprintln!("warning: signal {signal} won't reach git: {e}"),
        }
    }
    if caught.is_empty() {
        return;
    }
    std::thread::spawn(move || {
        let Some(signal) = wait(&caught, wake) else {
            return;
        };
        mdkg_git::children::stop(match signal {
            SIGINT => mdkg_git::children::Forward::Int,
            _ => mdkg_git::children::Forward::Term,
        });
        // Ends the process by `signal`, as its default action would.
        if signal_hook::low_level::emulate_default_handler(signal).is_err() {
            signal_hook::low_level::exit(128 + signal);
        }
    });
}

/// Registers `signal` to set its flag, then to write a wake-up byte (the
/// order signal-hook runs them in). Nothing is left registered on failure:
/// the pipe is cloned before either handler is installed, and if the
/// second can't be, the first is replaced by the default action.
fn register(signal: i32, waker: &UnixStream) -> std::io::Result<Arc<AtomicBool>> {
    let pipe = waker.try_clone()?;
    let flag = Arc::new(AtomicBool::new(false));
    let id = signal_hook::flag::register(signal, flag.clone())?;
    if let Err(e) = signal_hook::low_level::pipe::register(signal, pipe) {
        // signal-hook never restores the default, so an unregistered
        // signal would be ignored: take the default on the first one.
        signal_hook::low_level::unregister(id);
        signal_hook::flag::register_conditional_default(signal, Arc::new(AtomicBool::new(true)))
            .ok();
        return Err(e);
    }
    Ok(flag)
}

/// Waits for a caught signal: the flags are read before each wait, so one
/// that arrived while the handlers were being installed (its flag set, its
/// wake-up not yet registered) is still seen. With more than one set, the
/// first in `caught` wins (SIGTERM). `None` once the pipe can't wake it.
fn wait(caught: &[(i32, Arc<AtomicBool>)], mut wake: UnixStream) -> Option<i32> {
    let mut byte = [0u8];
    loop {
        if let Some(&(signal, _)) = caught.iter().find(|(_, f)| f.load(Ordering::SeqCst)) {
            return Some(signal);
        }
        match wake.read(&mut byte) {
            Ok(1..) => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            _ => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    #[test]
    fn a_flag_set_before_any_wake_up_is_seen_without_one() {
        let (wake, _waker) = UnixStream::pair().unwrap();
        let term = Arc::new(AtomicBool::new(false));
        let int = Arc::new(AtomicBool::new(true));
        let caught = [(SIGTERM, term), (SIGINT, int)];
        assert_eq!(wait(&caught, wake), Some(SIGINT));
    }

    #[test]
    fn sigterm_wins_when_both_are_set_and_a_stray_wake_up_waits_on() {
        let (wake, mut waker) = UnixStream::pair().unwrap();
        let term = Arc::new(AtomicBool::new(false));
        let int = Arc::new(AtomicBool::new(false));
        let caught = [(SIGTERM, term.clone()), (SIGINT, int.clone())];
        let waiter = std::thread::spawn(move || wait(&caught, wake));
        // A wake-up with no flag set is skipped.
        waker.write_all(&[0]).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(!waiter.is_finished());
        int.store(true, Ordering::SeqCst);
        term.store(true, Ordering::SeqCst);
        waker.write_all(&[0]).unwrap();
        assert_eq!(waiter.join().unwrap(), Some(SIGTERM));
    }

    #[test]
    fn a_closed_pipe_ends_the_wait() {
        let (wake, waker) = UnixStream::pair().unwrap();
        drop(waker);
        let caught = [(SIGTERM, Arc::new(AtomicBool::new(false)))];
        assert_eq!(wait(&caught, wake), None);
    }
}
