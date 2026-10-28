//! Each git check runs once per process (#53, task-45): the checked `Repo`
//! is kept for the process, so a command that opens it several times (as
//! `next` and `status` do) asks git once. A `git` on `PATH` that logs its
//! arguments counts them.

#![cfg(unix)]

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
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

/// A committed graph in `<scratch>/repo`, and a logging `git` in
/// `<scratch>/shim`.
fn setup() -> Scratch {
    let dir = common::temp::dir("mdkg-cli-git-spawns");
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
    fs::write(
        &git,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$SPAWN_LOG\"\nexec {} \"$@\"\n",
            real.trim()
        ),
    )
    .expect("shim");
    fs::set_permissions(&git, fs::Permissions::from_mode(0o755)).expect("chmod");
    Scratch(fs::canonicalize(dir).expect("canonical"))
}

/// The git calls one command makes.
fn calls(dir: &Path, args: &[&str]) -> Vec<String> {
    calls_with(dir, args, &[])
}

/// As [`calls`], with variables set (`Some`) or removed (`None`).
fn calls_with(dir: &Path, args: &[&str], vars: &[(&str, Option<&str>)]) -> Vec<String> {
    let log = dir.join("spawns.log");
    fs::write(&log, "").expect("log");
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
        .arg("--json")
        .env("PATH", path)
        .env("SPAWN_LOG", &log)
        // The user's own git config can't change what's counted.
        .env("HOME", dir)
        .env_remove("XDG_CONFIG_HOME");
    for (var, value) in vars {
        match value {
            Some(v) => cmd.env(var, v),
            None => cmd.env_remove(var),
        };
    }
    let out = cmd.output().expect("run mdkg");
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    fs::read_to_string(&log)
        .expect("log")
        .lines()
        .map(String::from)
        .collect()
}

#[test]
fn every_worktree_check_runs_once_per_command() {
    let s = setup();
    // The checks `Repo::open` makes: the top, git's view of the folder,
    // and the hooks config defines.
    let checks = [
        "rev-parse --show-toplevel",
        "rev-parse --path-format=absolute --git-dir",
        "--get-regexp ^hook",
    ];
    for args in [
        &["--as", "agent-a", "list"][..],
        &["--as", "agent-a", "show", "task-1"],
        &["--as", "agent-a", "next"],
        &["--as", "agent-a", "status"],
        &["--as", "agent-a", "claim", "task-1"],
        &["--as", "agent-a", "msg", "inbox"],
        // Unnamed, the actor is the worktree's folder: still once.
        &["list"],
    ] {
        let log = calls(&s.0, args);
        for check in checks {
            let n = log.iter().filter(|l| l.contains(check)).count();
            assert!(
                n <= 1,
                "{args:?} ran `{check}` {n} times:\n{}",
                log.join("\n")
            );
        }
    }
}

/// Tier 1 round 3 of 88c280a: a config that includes another file can name
/// hooks the check can't see change, so nothing is kept and every open
/// checks (as before #53).
#[test]
fn a_config_with_an_include_is_checked_every_time() {
    let s = setup();
    let repo = s.0.join("repo");
    fs::write(repo.join("extra.gitconfig"), "[core]\n").expect("extra");
    let ok = isolated(Command::new("git").arg("-C").arg(&repo).args([
        "config",
        "include.path",
        "../extra.gitconfig",
    ]))
    .status()
    .expect("git")
    .success();
    assert!(ok);
    let log = calls(&s.0, &["--as", "agent-a", "next"]);
    let n = log
        .iter()
        .filter(|l| l.contains("rev-parse --show-toplevel"))
        .count();
    assert!(n > 1, "kept despite an include:\n{}", log.join("\n"));
}

/// Tier 1 round 4 of 6754aba: the system config named by
/// `$GIT_CONFIG_SYSTEM`, and the user's XDG config when
/// `$XDG_CONFIG_HOME` is empty, are looked at too: an include in either
/// means every open checks.
#[test]
fn includes_in_the_system_and_xdg_configs_are_seen() {
    let s = setup();
    let system = s.0.join("system.gitconfig");
    fs::write(&system, "[include]\n\tpath = other.gitconfig\n").expect("system");
    let system = system.to_str().expect("utf-8").to_string();
    let xdg = s.0.join(".config/git");
    fs::create_dir_all(&xdg).expect("xdg");
    fs::write(xdg.join("config"), "[include]\n\tpath = other.gitconfig\n").expect("xdg");
    let toplevels = |log: &[String]| {
        log.iter()
            .filter(|l| l.contains("rev-parse --show-toplevel"))
            .count()
    };
    let next = ["--as", "agent-a", "next"];
    // The XDG config, found through HOME with XDG_CONFIG_HOME empty.
    let log = calls_with(
        &s.0,
        &next,
        &[("GIT_CONFIG_GLOBAL", None), ("XDG_CONFIG_HOME", Some(""))],
    );
    assert!(
        toplevels(&log) > 1,
        "XDG include missed:\n{}",
        log.join("\n")
    );
    // The system config, with GIT_CONFIG_NOSYSTEM false.
    fs::remove_file(xdg.join("config")).expect("rm");
    let log = calls_with(
        &s.0,
        &next,
        &[
            ("GIT_CONFIG_NOSYSTEM", Some("false")),
            ("GIT_CONFIG_SYSTEM", Some(system.as_str())),
        ],
    );
    assert!(
        toplevels(&log) > 1,
        "system include missed:\n{}",
        log.join("\n")
    );
}
