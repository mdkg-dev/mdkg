//! A config newer than this build reads (`schema_version` 4, or version 3
//! with format "0.8") is refused by every command that reads a graph,
//! before anything is written: no coordinator store, record, worktree,
//! branch, agent state or planned file (RC1 blocker S1). A released binary
//! can't learn this refusal later, so it's checked once, up front.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).ok();
    }
}

fn isolated<'a>(cmd: &'a mut Command, home: &Path) -> &'a mut Command {
    common::without_launcher_env(cmd)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
}

fn git(dir: &Path, home: &Path, args: &[&str]) -> String {
    let out = isolated(Command::new("git").arg("-C").arg(dir).args(args), home)
        .output()
        .expect("git runs");
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn mdkg(repo: &Path, home: &Path, args: &[&str]) -> Output {
    isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")), home)
        .current_dir(repo)
        .args(["--root", repo.to_str().expect("utf-8"), "--as", "agent-a"])
        .args(args)
        .output()
        .expect("run mdkg")
}

/// Both configs this build can't read. Each names an agent, so `run` and
/// `agents` would otherwise go on to make its branch, worktree and state.
const NEWER: [&str; 2] = [
    r#"{"schema_version": 4, "format_version": "0.8", "graph_id": "0192f3a4-7b2c-7def-8a9b-1c2d3e4f5a6b", "agents": {"newbie": {"harness": "claude", "role": "worker"}}}"#,
    r#"{"schema_version": 3, "format_version": "0.8", "graph_id": "0192f3a4-7b2c-7def-8a9b-1c2d3e4f5a6b", "agents": {"newbie": {"harness": "claude", "role": "worker"}}}"#,
];

/// A committed repository whose config is `config`.
fn repo_with(config: &str) -> (Scratch, PathBuf) {
    let dir = common::temp::dir("mdkg-cli-config-version");
    let repo = dir.join("repo");
    fs::create_dir_all(repo.join(".mdkg")).expect("dir");
    let repo = fs::canonicalize(repo).expect("canonical");
    let home = fs::canonicalize(&dir).expect("canonical");
    fs::write(repo.join(".mdkg/config.json"), config).expect("config");
    fs::create_dir_all(repo.join("src")).expect("src");
    fs::write(repo.join("src/file.rs"), "x").expect("file");
    git(&repo, &home, &["init", "-q", "-b", "main"]);
    git(&repo, &home, &["add", "-A"]);
    git(&repo, &home, &["commit", "-qm", "init"]);
    (Scratch(dir), repo)
}

/// Everything a refused command must leave as it was: the files outside
/// `.git`, whether `.git/mdkg` (the store, agent state) exists, the
/// worktrees and the refs.
fn state(repo: &Path, home: &Path) -> (Vec<String>, bool, String, String) {
    let mut files = Vec::new();
    let mut stack = vec![repo.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).expect("read dir").flatten() {
            let p = e.path();
            // git's own files change on a read (an index refresh).
            if p == repo.join(".git") {
                continue;
            }
            if p.is_dir() {
                stack.push(p.clone());
            }
            files.push(p.strip_prefix(repo).expect("inside").display().to_string());
        }
    }
    files.sort();
    (
        files,
        repo.join(".git/mdkg").exists(),
        git(repo, home, &["worktree", "list", "--porcelain"]),
        git(repo, home, &["for-each-ref", "--format=%(refname)"]),
    )
}

/// Lane 5's reproduction on RC1 (`run newbie --worktree` made
/// agent/newbie, its worktree and its state before failing), and every
/// other command that wrote first: msg, install, claim.
#[test]
fn a_newer_config_is_refused_before_any_side_effect() {
    let commands: &[&[&str]] = &[
        &[
            "msg", "send", "--to", "someone", "--kind", "note", "--text", "hi",
        ],
        &["msg", "inbox"],
        &["agents", "status"],
        &["agents", "up", "--once", "newbie"],
        &["install", "--dry-run"],
        &[
            "agents",
            "add",
            "newbie",
            "--harness",
            "claude",
            "--role",
            "worker",
            "--worktree",
        ],
        &["agents", "up", "newbie", "--once", "--dry-run"],
        &["claim", "src/file.rs"],
        &["status"],
        &["list"],
        // A store's verify would open (and make) the store; migrate's apply
        // would write its lock (Tier 1 round 1 of 044d9f8).
        &["verify"],
        &[
            "migrate",
            "--apply",
            "sha256:0000000000000000000000000000000000000000000000000000000000000000",
        ],
    ];
    for config in NEWER {
        let (scratch, repo) = repo_with(config);
        let home = scratch.0.clone();
        let before = state(&repo, &home);
        assert!(!before.1, "no store before");
        for args in commands {
            let out = mdkg(&repo, &home, &[args, &["--json"][..]].concat());
            let stdout = String::from_utf8_lossy(&out.stdout);
            // `agents up --json` prints one envelope per line.
            let v: Value = serde_json::from_str(&stdout)
                .or_else(|_| serde_json::from_str(stdout.lines().next().unwrap_or_default()))
                .unwrap_or_else(|_| {
                    panic!(
                        "{args:?}: not JSON: {stdout}\n{}",
                        String::from_utf8_lossy(&out.stderr)
                    )
                });
            assert_eq!(out.status.code(), Some(4), "{args:?}: {stdout}");
            assert_eq!(v["ok"], false, "{args:?}: {stdout}");
            assert_eq!(
                v["diagnostics"][0]["code"], "config.invalid",
                "{args:?}: {stdout}"
            );
            assert_eq!(state(&repo, &home), before, "{args:?} left a side effect");
        }
        // The default watch prints text.
        let out = mdkg(
            &repo,
            &home,
            &["agents", "up", "newbie", "--max-turns", "1"],
        );
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(4), "{err}");
        assert!(err.contains("config"), "{err}");
        assert_eq!(state(&repo, &home), before, "agents up left a side effect");
    }
}

#[test]
fn setup_commands_still_run_on_a_newer_config() {
    // version and schema read no graph.
    let (scratch, repo) = repo_with(NEWER[0]);
    let home = scratch.0.clone();
    for args in [&["version"][..], &["schema", "list"]] {
        let out = mdkg(&repo, &home, args);
        assert_eq!(out.status.code(), Some(0), "{args:?}");
    }
}
