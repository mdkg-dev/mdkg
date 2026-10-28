//! A repository with no commit yet (RC1 B7): main is unborn, holds
//! nothing, and work starts anyway: `new`, `claim` and `status` run before
//! the first commit. A configured main that's missing beside other
//! branches is still refused as misnamed.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

mod common;

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).ok();
    }
}

fn scratch() -> Scratch {
    let dir = common::temp::dir("mdkg-cli-unborn");
    Scratch(fs::canonicalize(dir).expect("canonical"))
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

fn sh(dir: &Path, args: &[&str]) {
    let out = isolated(Command::new("git").arg("-C").arg(dir).args(args))
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn run(root: &Path, args: &[&str]) -> (i32, Value) {
    let out = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
        .arg("--root")
        .arg(root)
        .args(["--as", "agent1"])
        .args(args)
        .arg("--json")
        .output()
        .expect("run mdkg");
    let envelope: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|_| {
        panic!(
            "not JSON: {}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    });
    common::conforms_envelope(&envelope);
    (out.status.code().unwrap_or(-1), envelope)
}

fn ok(root: &Path, args: &[&str]) -> Value {
    let (code, out) = run(root, args);
    assert_eq!(code, 0, "{args:?}: {out}");
    out
}

#[test]
fn work_starts_before_the_first_commit() {
    let dir = scratch();
    let repo = dir.0.join("repo");
    fs::create_dir_all(&repo).expect("repo");
    sh(&repo, &["init", "-q", "-b", "main"]);
    let (code, out) = run(&repo, &["init"]);
    assert_eq!(code, 0, "{out}");
    let task = ok(&repo, &["new", "task", "First"]);
    let id = task["data"]["node"]["id"].as_str().expect("id").to_string();
    let claimed = ok(&repo, &["claim", &id]);
    assert_eq!(claimed["data"]["lease"]["owner"], "agent1", "{claimed}");
    let status = ok(&repo, &["status"]);
    assert_eq!(status["data"]["main"], "main", "{status}");
}

#[test]
fn a_missing_main_beside_other_branches_is_still_refused() {
    let dir = scratch();
    let repo = dir.0.join("repo");
    fs::create_dir_all(&repo).expect("repo");
    sh(&repo, &["init", "-q", "-b", "master"]);
    let (code, out) = run(&repo, &["init"]);
    assert_eq!(code, 0, "{out}");
    sh(&repo, &["add", "-A"]);
    sh(&repo, &["commit", "-q", "-m", "start"]);
    // main is configured (the default) but the work is on master.
    let (code, out) = run(&repo, &["status"]);
    assert_ne!(code, 0, "{out}");
    let message = out["diagnostics"][0]["message"]
        .as_str()
        .unwrap_or_default();
    assert!(message.contains("main branch main not found"), "{out}");
}

#[test]
fn a_detached_head_with_commits_isnt_taken_for_unborn() {
    let dir = scratch();
    let repo = dir.0.join("repo");
    fs::create_dir_all(&repo).expect("repo");
    sh(&repo, &["init", "-q", "-b", "main"]);
    let (code, out) = run(&repo, &["init"]);
    assert_eq!(code, 0, "{out}");
    sh(&repo, &["add", "-A"]);
    sh(&repo, &["commit", "-q", "-m", "start"]);
    // Only a detached HEAD keeps the history now: no branch, no tag.
    sh(&repo, &["checkout", "-q", "--detach"]);
    sh(&repo, &["branch", "-q", "-D", "main"]);
    let (code, out) = run(&repo, &["status"]);
    assert_ne!(code, 0, "{out}");
    let message = out["diagnostics"][0]["message"]
        .as_str()
        .unwrap_or_default();
    assert!(message.contains("main branch main not found"), "{out}");
}
