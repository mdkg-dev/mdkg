//! `.git/mdkg` is created 0700 when the search index, `work localize` or
//! the store makes it first (Opus #16893); install and some dry runs still
//! use the umask's mode. In a fresh checkout-mode clone, `search` builds the index
//! before anything opens the store: it used to make the folder with the
//! umask's mode (0755 under 022).

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;

mod common;

struct Dir(PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .expect("git");
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

/// Runs mdkg under umask 022, as most shells start.
fn mdkg(dir: &Path, args: &[&str]) -> std::process::Output {
    let mut c = Command::new("/bin/sh");
    c.args([
        "-c",
        "umask 022; exec \"$0\" \"$@\"",
        env!("CARGO_BIN_EXE_mdkg"),
    ]);
    common::without_launcher_env(&mut c)
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .expect("mdkg")
}

#[test]
fn search_first_in_a_fresh_clone_makes_the_state_folder_0700() {
    let tmp = Dir(std::env::temp_dir().join(format!("mdkg-state-dir-{}", uuid::Uuid::now_v7())));
    let origin = tmp.0.join("origin");
    std::fs::create_dir_all(&origin).expect("origin");
    git(&origin, &["init", "-q", "-b", "main"]);
    let out = mdkg(&origin, &["init", "--no-gitignore", "--json"]);
    assert!(out.status.success(), "init: {out:?}");
    git(&origin, &["add", "-A"]);
    git(&origin, &["commit", "-qm", "graph"]);
    let clone = tmp.0.join("clone");
    git(&tmp.0, &["clone", "-q", "origin", "clone"]);
    let state = clone.join(".git/mdkg");
    assert!(!state.exists(), "a fresh clone has no state folder");

    let out = mdkg(&clone, &["search", "anything", "--json"]);
    assert!(out.status.success(), "search: {out:?}");
    let mode = |p: &Path| std::fs::metadata(p).expect("folder").permissions().mode() & 0o777;
    assert_eq!(mode(&state), 0o700, "{out:?}");
    assert_eq!(mode(&state.join("index")), 0o700);
}
