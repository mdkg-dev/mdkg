//! One worktree-check cache per process (orchestrator, 2026-10-30, on
//! g-authority #10952): a ref-mode probe through mdkg-git followed by the
//! coordinator's open checks the worktree once, in the default trust context
//! and inside a request trust scope set through the coordinator's API.
//!
//! The counting runs in a child of this test binary, with a logging `git`
//! first on its `PATH`: the git program is found once per process, and this
//! workspace sets no environment in-process.

#[path = "common/temp.rs"]
mod temp;

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use mdkg_coord_sqlite::git;

/// The checks a worktree check makes: the top, git's view of the folder,
/// and the hooks config defines (as `git_spawns.rs` counts them).
const CHECKS: [&str; 3] = [
    "rev-parse --show-toplevel",
    "rev-parse --path-format=absolute --git-dir",
    "--get-regexp ^hook",
];

const CHILD: &str = "MDKG_TEST_ONE_CHECK_REPO";

fn isolated(cmd: &mut Command) -> &mut Command {
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            cmd.env_remove(key);
        }
    }
    cmd.env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env_remove("MDKG_TRUSTED_COMMON_DIR")
        .env_remove("MDKG_TRUSTED_WORKTREE")
}

fn real_git() -> String {
    let out = Command::new("/bin/sh")
        .args(["-c", "command -v git"])
        .output()
        .expect("sh");
    String::from_utf8(out.stdout)
        .expect("utf-8")
        .trim()
        .to_owned()
}

/// A repository with one commit, and a `git` shim that logs each call.
fn setup() -> (PathBuf, PathBuf) {
    let dir = fs::canonicalize(temp::dir("mdkg-one-worktree-check")).expect("canonical");
    let repo = dir.join("repo");
    fs::create_dir_all(&repo).expect("repo");
    fs::write(repo.join("README.md"), "one\n").expect("file");
    for args in [
        &["init", "-q", "-b", "main"][..],
        &["add", "."],
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.invalid",
            "commit",
            "-q",
            "-m",
            "init",
        ],
    ] {
        let ok = isolated(Command::new("git").arg("-C").arg(&repo).args(args))
            .status()
            .expect("git runs")
            .success();
        assert!(ok, "git {args:?}");
    }
    let shim = dir.join("shim");
    fs::create_dir_all(&shim).expect("shim");
    fs::write(
        shim.join("git"),
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$SPAWN_LOG\"\nexec {} \"$@\"\n",
            real_git()
        ),
    )
    .expect("shim");
    fs::set_permissions(shim.join("git"), fs::Permissions::from_mode(0o755)).expect("chmod");
    (dir, repo)
}

/// The ref-mode probe (`graphref::resolve` reads mdkg-git's common dir),
/// then what the coordinator's `Repo::open` reads: the top, then the store
/// path under the common dir.
fn probe_then_open(repo: &Path) {
    mdkg_git::common_dir(repo).expect("probe");
    let top = git::toplevel(repo).expect("top");
    git::store_path(&top).expect("store path");
}

/// Runs in the child only: the parent sets `CHILD` to the repository.
#[test]
fn child_probe_then_open() {
    let Some(repo) = std::env::var_os(CHILD) else {
        return;
    };
    let repo = PathBuf::from(repo);
    let which = std::env::var("MDKG_TEST_ONE_CHECK_TRUST").unwrap_or_default();
    if which == "scoped" {
        // The CLI sets its request trust through the coordinator's API; the
        // probe through mdkg-git sees the same scope.
        let trust = git::WorktreeTrust {
            common: Some(repo.join(".git")),
            worktree: Some(repo.clone()),
        };
        git::with_worktree_trust(&trust, || probe_then_open(&repo));
    } else {
        probe_then_open(&repo);
    }
}

#[test]
fn a_ref_mode_probe_then_the_coordinator_open_check_the_worktree_once() {
    if std::env::var_os(CHILD).is_some() {
        return;
    }
    let (dir, repo) = setup();
    let path = format!(
        "{}:{}",
        dir.join("shim").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    for which in ["default", "scoped"] {
        let log = dir.join(format!("spawns-{which}.log"));
        fs::write(&log, "").expect("log");
        let out = isolated(&mut Command::new(
            std::env::current_exe().expect("test binary"),
        ))
        .args([
            "--exact",
            "child_probe_then_open",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, &repo)
        .env("MDKG_TEST_ONE_CHECK_TRUST", which)
        .env("PATH", &path)
        .env("SPAWN_LOG", &log)
        .env("HOME", &dir)
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .expect("child runs");
        assert!(
            out.status.success(),
            "{which}: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let calls: Vec<String> = fs::read_to_string(&log)
            .expect("log")
            .lines()
            .map(String::from)
            .collect();
        assert!(
            calls.iter().any(|l| l.contains(CHECKS[0])),
            "{which}: the child ran no worktree check, so nothing was counted:\n{}",
            calls.join("\n")
        );
        for check in CHECKS {
            let n = calls.iter().filter(|l| l.contains(check)).count();
            assert!(
                n <= 1,
                "{which}: `{check}` ran {n} times:\n{}",
                calls.join("\n")
            );
        }
    }
}
