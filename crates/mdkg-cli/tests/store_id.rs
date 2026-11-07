//! `status --json` and `config show --json` name the coordinator store by
//! the id `anchor` uses: null before the store exists, then the same from
//! every command and every worktree. Its `graph` block names the
//! checkout's HEAD in checkout mode.

#![cfg(unix)]

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).ok();
    }
}

fn isolated<'a>(cmd: &'a mut Command, home: &Path) -> &'a mut Command {
    common::without_launcher_env(cmd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("CODEX_HOME", home.join(".codex"))
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

fn git(repo: &Path, home: &Path, args: &[&str]) {
    let ok = isolated(Command::new("git").arg("-C").arg(repo).args(args), home)
        .status()
        .expect("git runs")
        .success();
    assert!(ok, "git {args:?}");
}

fn mdkg(root: &Path, home: &Path, args: &[&str]) -> Value {
    let out = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")), home)
        .arg("--root")
        .arg(root)
        .args(["--as", "tester"])
        .args(args)
        .arg("--json")
        .output()
        .expect("mdkg runs");
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|_| panic!("{args:?}: {}", String::from_utf8_lossy(&out.stdout)))
}

#[test]
fn status_and_config_show_name_the_store_as_anchor_does() {
    let dir = fs::canonicalize(common::temp::dir("mdkg-cli-store-id")).expect("canonical");
    let scratch = Scratch(dir.clone());
    let home = dir.join("home");
    fs::create_dir_all(&home).expect("home");
    let repo = dir.join("repo");
    fs::create_dir_all(&repo).expect("repo");
    let conformance = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance");
    copy_tree(&conformance.join("base"), &repo);
    copy_tree(&conformance.join("cases/g01-valid-basic"), &repo);
    fs::remove_file(repo.join("commands")).ok();
    git(&repo, &home, &["init", "-q", "-b", "main"]);
    git(&repo, &home, &["add", "."]);
    git(&repo, &home, &["commit", "-q", "-m", "init"]);
    // No store yet: null, and reading it creates none.
    let status = mdkg(&repo, &home, &["status"]);
    assert_eq!(status["ok"], true, "{status}");
    assert_eq!(status["data"]["store_id"], Value::Null, "{status}");
    // Checkout mode names the graph's head too: no ref, the HEAD commit.
    let head = String::from_utf8(
        isolated(
            Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["rev-parse", "HEAD"]),
            &home,
        )
        .output()
        .expect("git runs")
        .stdout,
    )
    .expect("utf-8");
    assert_eq!(status["data"]["graph"]["ref"], Value::Null, "{status}");
    assert_eq!(status["data"]["graph"]["tip"], head.trim(), "{status}");
    let shown = mdkg(&repo, &home, &["config", "show"]);
    assert_eq!(shown["data"]["store_id"], Value::Null, "{shown}");
    assert!(!repo.join(".git/mdkg/coord.sqlite").exists());
    // A claim creates it.
    let claimed = mdkg(&repo, &home, &["claim", "--new", "notes/a.md"]);
    assert_eq!(claimed["ok"], true, "{claimed}");
    let status = mdkg(&repo, &home, &["status"]);
    let id = status["data"]["store_id"].as_str().expect("a store id");
    assert!(!id.is_empty(), "{status}");
    let shown = mdkg(&repo, &home, &["config", "show"]);
    assert_eq!(shown["data"]["store_id"], id, "{shown}");
    let anchored = mdkg(&repo, &home, &["anchor"]);
    assert_eq!(anchored["data"]["store"], id, "{anchored}");
    // Another worktree of the repository shares the store, so the id.
    let other = dir.join("other");
    git(
        &repo,
        &home,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "other",
            other.to_str().expect("utf-8"),
        ],
    );
    let status = mdkg(&other, &home, &["status"]);
    assert_eq!(status["data"]["store_id"], id, "{status}");
    // Each worktree reports its own HEAD: a commit there moves only its own.
    git(
        &other,
        &home,
        &["commit", "-q", "--allow-empty", "-m", "other"],
    );
    let moved = mdkg(&other, &home, &["status"]);
    assert_ne!(
        moved["data"]["graph"]["tip"], status["data"]["graph"]["tip"],
        "{moved}"
    );
    let main = mdkg(&repo, &home, &["status"]);
    assert_eq!(main["data"]["graph"]["tip"], head.trim(), "{main}");
    drop(scratch);
}
