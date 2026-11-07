//! SIGTERM and SIGINT reach the git mdkg started (o-omni item 1): each git
//! runs in its own process group, so mdkg forwards the signal, waits a
//! bounded time, kills what's left and ends by the same signal. After mdkg
//! exits, nothing it started is still running. A slow `git` on `PATH`
//! stands in for a write that would otherwise land late.

#![cfg(unix)]

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        // A failed test may leave the fake git behind: end it first.
        for pid in pids(&self.0) {
            signal(pid, "-KILL");
        }
        fs::remove_dir_all(&self.0).ok();
    }
}

fn isolated(cmd: &mut Command) -> &mut Command {
    common::without_launcher_env(cmd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
}

fn copy_tree(src: &Path, dest: &Path) {
    for entry in fs::read_dir(src).expect("read dir") {
        let entry = entry.expect("entry");
        let target = dest.join(entry.file_name());
        if entry.file_type().expect("type").is_dir() {
            fs::create_dir_all(&target).expect("mkdir");
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).expect("copy");
        }
    }
}

/// A committed graph in `<scratch>/repo`, and in `<scratch>/shim` a `git`
/// that, while `<scratch>/slow` exists and its arguments contain `slow_on`
/// (any call when empty), records its pid and a child's in
/// `<scratch>/pids` and waits on that child for 60 s. With `ignore_term`
/// it and its child ignore SIGTERM, so only a kill ends them.
fn setup(ignore_term: bool, slow_on: &str) -> Scratch {
    let dir = fs::canonicalize(common::temp::dir("mdkg-cli-git-signals")).expect("canonical");
    let scratch = Scratch(dir.clone());
    let repo = dir.join("repo");
    fs::create_dir_all(&repo).expect("repo");
    let conformance = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance");
    copy_tree(&conformance.join("base"), &repo);
    copy_tree(&conformance.join("cases/g01-valid-basic"), &repo);
    fs::remove_file(repo.join("commands")).ok();
    for args in [
        &["init", "-q", "-b", "main"][..],
        &["add", "."],
        &["commit", "-q", "-m", "init"],
    ] {
        let ok = isolated(Command::new("git").arg("-C").arg(&repo).args(args))
            .status()
            .expect("git runs")
            .success();
        assert!(ok, "git {args:?}");
    }
    let real = String::from_utf8(
        Command::new("/bin/sh")
            .args(["-c", "command -v git"])
            .output()
            .expect("sh")
            .stdout,
    )
    .expect("utf-8");
    let shim = dir.join("shim");
    fs::create_dir_all(&shim).expect("shim");
    let git = shim.join("git");
    let trap = if ignore_term { "trap '' TERM\n" } else { "" };
    fs::write(
        &git,
        format!(
            "#!/bin/sh\n\
             case \" $* \" in *'{slow_on}'*) on=1 ;; *) on= ;; esac\n\
             if [ -e '{slow}' ] && [ -n \"$on\" ]; then\n\
             {trap}\
             sleep 60 &\n\
             printf '%s\\n%s\\n' \"$$\" \"$!\" >> '{pids}'\n\
             wait\n\
             : > '{late}'\n\
             fi\n\
             exec {real} \"$@\"\n",
            slow = dir.join("slow").display(),
            slow_on = slow_on,
            pids = dir.join("pids").display(),
            late = dir.join("late").display(),
            real = real.trim(),
        ),
    )
    .expect("shim");
    fs::set_permissions(&git, fs::Permissions::from_mode(0o755)).expect("chmod");
    fs::write(dir.join("slow"), "").expect("slow");
    scratch
}

fn pids(dir: &Path) -> Vec<u32> {
    fs::read_to_string(dir.join("pids"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect()
}

/// `kill <signal> <pid>`: whether it was delivered.
fn signal(pid: u32, signal: &str) -> bool {
    Command::new("/bin/kill")
        .args([signal, &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn alive(pid: u32) -> bool {
    signal(pid, "-0")
}

/// Starts `status` against the slow git, waits until git is running,
/// sends `sig`, and checks that mdkg ends by that signal within `bound`
/// and leaves nothing it started running.
fn ends_with_git(ignore_term: bool, sig: &str, number: i32, bound: Duration) {
    ends_with_git_in(ignore_term, "", &["status", "--json"], sig, number, bound);
}

/// [`ends_with_git`] for the command `args`, whose git is slow only when
/// its arguments contain `slow_on`.
fn ends_with_git_in(
    ignore_term: bool,
    slow_on: &str,
    args: &[&str],
    sig: &str,
    number: i32,
    bound: Duration,
) {
    let scratch = setup(ignore_term, slow_on);
    let dir = &scratch.0;
    let path = format!(
        "{}:{}",
        dir.join("shim").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mdkg"));
    isolated(&mut cmd)
        .arg("--root")
        .arg(dir.join("repo"))
        .args(args)
        .env("PATH", path)
        .env("HOME", dir)
        .env("XDG_CONFIG_HOME", dir.join("xdg-config"))
        .env("XDG_DATA_HOME", dir.join("xdg-data"))
        .env("CODEX_HOME", dir.join("codex"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut mdkg = cmd.spawn().expect("mdkg starts");
    let started = Instant::now();
    while pids(dir).len() < 2 {
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "git never started"
        );
        assert!(mdkg.try_wait().expect("wait").is_none(), "mdkg ended early");
        std::thread::sleep(Duration::from_millis(20));
    }
    let started = pids(dir);
    assert!(signal(mdkg.id(), sig), "signal sent");
    let sent = Instant::now();
    let status = loop {
        if let Some(status) = mdkg.try_wait().expect("wait") {
            break status;
        }
        assert!(sent.elapsed() < bound, "mdkg still running after {bound:?}");
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(status.signal(), Some(number), "{status:?}");
    // Once mdkg is gone, its git and git's own child are too (a killed
    // process may take a moment to be reaped by init once orphaned).
    let gone = Instant::now();
    while started.iter().any(|&pid| alive(pid)) {
        assert!(
            gone.elapsed() < Duration::from_secs(2),
            "still running after mdkg exited: {started:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!dir.join("late").exists(), "git went on after the signal");
}

#[test]
fn sigterm_reaches_git_and_mdkg_ends_by_it() {
    ends_with_git(false, "-TERM", 15, Duration::from_secs(2));
}

#[test]
fn sigint_reaches_git_and_mdkg_ends_by_it() {
    // A shell's background job ignores SIGINT, so the sleep is killed after
    // the grace.
    ends_with_git(false, "-INT", 2, Duration::from_secs(5));
}

/// A git that ignores the signal is killed once the grace (2 s) is over,
/// and mdkg exits within the documented bound (3 s, plus slack here).
#[test]
fn git_ignoring_sigterm_is_killed_after_the_grace() {
    ends_with_git(true, "-TERM", 15, Duration::from_secs(5));
}

/// `install`'s graph checkout comes from `git worktree add`, which mdkg-run
/// starts: a signal during it reaches it too.
#[test]
fn sigterm_reaches_an_installs_worktree_add() {
    ends_with_git_in(
        false,
        "worktree add",
        &["install", "--json"],
        "-TERM",
        15,
        Duration::from_secs(2),
    );
}

/// `agents up --once` has no drain of its own, so it forwards too.
#[test]
fn sigterm_reaches_git_under_agents_up_once() {
    ends_with_git_in(
        false,
        "",
        &["--as", "launcher", "agents", "up", "--once", "--json"],
        "-TERM",
        15,
        Duration::from_secs(2),
    );
}

/// A continuous `agents up` drains on SIGTERM instead of forwarding it
/// (review of 9e274d1f): a SIGTERM during its startup's git (the first git
/// it runs) no longer kills mdkg by the default action with that git still
/// running. mdkg waits for the git, then stops, and nothing it started
/// outlives it.
#[test]
fn sigterm_during_a_continuous_agents_ups_startup_git_waits_for_it() {
    let scratch = setup(false, "");
    let dir = &scratch.0;
    let path = format!(
        "{}:{}",
        dir.join("shim").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mdkg"));
    isolated(&mut cmd)
        .arg("--root")
        .arg(dir.join("repo"))
        .args(["--as", "launcher", "agents", "up", "--json"])
        .env("PATH", path)
        .env("HOME", dir)
        .env("XDG_CONFIG_HOME", dir.join("xdg-config"))
        .env("XDG_DATA_HOME", dir.join("xdg-data"))
        .env("CODEX_HOME", dir.join("codex"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut mdkg = cmd.spawn().expect("mdkg starts");
    let started = Instant::now();
    while pids(dir).len() < 2 {
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "git never started"
        );
        assert!(mdkg.try_wait().expect("wait").is_none(), "mdkg ended early");
        std::thread::sleep(Duration::from_millis(20));
    }
    let slow = pids(dir);
    assert!(signal(mdkg.id(), "-TERM"), "signal sent");
    // The drain holds: mdkg doesn't die under its running git.
    std::thread::sleep(Duration::from_secs(1));
    assert!(
        mdkg.try_wait().expect("wait").is_none(),
        "mdkg died by SIGTERM's default action with its git running"
    );
    // Let that git finish (no later git is slow), and mdkg stops.
    fs::remove_file(dir.join("slow")).expect("unslow");
    assert!(signal(slow[1], "-KILL"), "the slow git's sleep ends");
    let sent = Instant::now();
    loop {
        if mdkg.try_wait().expect("wait").is_some() {
            break;
        }
        assert!(
            sent.elapsed() < Duration::from_secs(60),
            "mdkg still running a minute after its git finished"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let gone = Instant::now();
    while slow.iter().any(|&pid| alive(pid)) {
        assert!(
            gone.elapsed() < Duration::from_secs(2),
            "still running after mdkg exited: {slow:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
