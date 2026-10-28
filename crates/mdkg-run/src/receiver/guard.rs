//! Private queue subprocess: preserves the lease's epoch-millisecond deadline
//! across process startup, and watches it independently of coordinator calls.
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use rustix::process::{Signal, getpgrp, getppid, kill_process_group};
use serde::{Deserialize, Serialize};

use crate::state;

pub(super) const NOT_SENT: u8 = 125;
pub(super) const UNKNOWN: u8 = 1;
pub(super) const TICK: Duration = Duration::from_millis(10);

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Spec {
    pub deadline: u64,
    pub program: PathBuf,
    pub session: String,
    pub line: String,
}

fn alive(spec: &Spec, parent: u32, now: u64) -> bool {
    getppid().is_some_and(|pid| pid.as_raw_nonzero().get() as u32 == parent) && now < spec.deadline
}

/// Hidden CLI entry, before any repository/configuration work. Never prints
/// argv, queue output or errors. Exit 125 is reserved for proven non-launch;
/// the child's own exit codes are never passed through as that proof.
pub fn run(json: &str, owner: u32) -> u8 {
    run_at(json, owner, state::now_ms)
}

fn run_at(json: &str, owner: u32, now: impl Fn() -> u64) -> u8 {
    if json.len() > 64 * 1024 {
        return NOT_SENT;
    }
    let Ok(spec) = serde_json::from_str::<Spec>(json) else {
        return NOT_SENT;
    };
    // Only our private process group can be killed. An invocation from an
    // ordinary interactive shell must not be able to kill that shell's group.
    let group = getpgrp();
    if group.as_raw_nonzero().get() as u32 != owner
        || getppid() != Some(group)
        || !spec.program.is_absolute()
    {
        return NOT_SENT;
    }
    let mut command = Command::new(&spec.program);
    command
        .args([
            "queue",
            "--remote",
            "unix://",
            "--thread",
            &spec.session,
            "--message",
            &spec.line,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Resolve/build everything first. This absolute-clock check is adjacent
    // to spawn, including when the guard itself was stopped during startup.
    // The remaining check-to-daemon-acceptance interval is not a fence.
    if !alive(&spec, owner, now()) {
        return NOT_SENT;
    }
    let Ok(mut child) = command.spawn() else {
        return NOT_SENT;
    };
    loop {
        if !alive(&spec, owner, now()) {
            // Includes the native guard and its outer signal supervisor.
            // A killed guard means unknown, never a delivery receipt.
            let _ = kill_process_group(group, Signal::KILL);
            let _ = child.kill();
            let _ = child.wait();
            return UNKNOWN;
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                // A pause while reaping cannot turn late completion into a
                // successful receipt. Parent independently checks it too.
                return if alive(&spec, owner, now()) && status.success() {
                    0
                } else {
                    UNKNOWN
                };
            }
            Ok(None) => std::thread::sleep(TICK),
            Err(_) => {
                let _ = kill_process_group(group, Signal::KILL);
                let _ = child.kill();
                let _ = child.wait();
                return UNKNOWN;
            }
        }
    }
}

#[cfg(test)]
pub(super) const TEST_ENTRY: &str = "receiver::guard::tests::entry";
#[cfg(test)]
pub(super) const TEST_SPEC: &str = "MDKG_QUEUE_GUARD_TEST_SPEC";
#[cfg(test)]
pub(super) const TEST_CLOCK: &str = "MDKG_QUEUE_GUARD_TEST_CLOCK";

#[cfg(test)]
mod tests {
    use super::*;

    // Library tests execute the same guard in a separate process. The shipped
    // mdkg binary reaches run() through its private CLI entry instead.
    #[test]
    fn entry() {
        if let Ok(spec) = std::env::var(TEST_SPEC) {
            let owner = std::env::var("MDKG_QUEUE_GUARD_TEST_OWNER")
                .ok()
                .and_then(|owner| owner.parse().ok());
            let clock = std::env::var_os(TEST_CLOCK);
            let now = || {
                clock.as_ref().map_or_else(state::now_ms, |path| {
                    std::fs::read_to_string(path)
                        .expect("test clock")
                        .parse()
                        .expect("test clock value")
                })
            };
            std::process::exit(
                owner
                    .map_or(NOT_SENT, |owner| run_at(&spec, owner, now))
                    .into(),
            );
        }
    }
}
