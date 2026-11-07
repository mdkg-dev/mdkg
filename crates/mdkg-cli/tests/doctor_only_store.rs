//! `doctor --only store` reads the coordinator store's versions read-only:
//! no projection or harness file's content shows, and nothing is written.

use std::collections::BTreeMap;
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

/// A token that appears nowhere but in the planted files.
const CANARY: &str = concat!("canary", "-", "7f3a9c");

/// The projection and harness files a fuller read would open, in the
/// repository and in HOME.
const PLANTED: &[&str] = &[
    // The project config too: replaced by the canary, so a command that
    // read it would fail or show it.
    "repo/.mdkg/config.json",
    "repo/.mcp.json",
    "repo/.claude/settings.json",
    "repo/.claude/settings.local.json",
    "repo/opencode.json",
    "repo/.opencode/opencode.json",
    "repo/.codex/config.toml",
    "repo/AGENTS.md",
    "repo/CLAUDE.md",
    "home/.claude.json",
    "home/.claude/settings.json",
    "home/.codex/config.toml",
    "home/.config/opencode/opencode.json",
];

fn isolated<'a>(cmd: &'a mut Command, home: &Path) -> &'a mut Command {
    common::without_launcher_env(cmd)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("CODEX_HOME", home.join(".codex"))
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
}

fn run(dir: &Path, program: &str, args: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(program);
    isolated(&mut cmd, &dir.join("home"))
        .current_dir(dir.join("repo"))
        .args(args)
        .output()
        .expect("runs")
}

/// Every file under `dir` with its bytes and modification time.
fn snapshot(dir: &Path, into: &mut BTreeMap<PathBuf, (Vec<u8>, std::time::SystemTime)>) {
    for entry in fs::read_dir(dir).expect("read dir") {
        let entry = entry.expect("entry");
        let path = entry.path();
        let meta = fs::symlink_metadata(&path).expect("meta");
        if meta.is_dir() {
            snapshot(&path, into);
        } else if meta.is_file() {
            into.insert(
                path.clone(),
                (
                    fs::read(&path).expect("read"),
                    meta.modified().expect("mtime"),
                ),
            );
        }
    }
}

#[test]
fn only_store_reads_the_store_versions_and_nothing_else() {
    let dir = Scratch(fs::canonicalize(common::temp::dir("mdkg-cli-doctor-store")).expect("dir"));
    let repo = dir.0.join("repo");
    fs::create_dir_all(&repo).expect("repo");
    fs::create_dir_all(dir.0.join("home")).expect("home");
    let mdkg = env!("CARGO_BIN_EXE_mdkg");
    assert!(
        run(&dir.0, "git", &["init", "-q", "-b", "main"])
            .status
            .success()
    );
    let init = run(&dir.0, mdkg, &["--as", "launcher", "init", "--json"]);
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stdout)
    );
    // A store at this build's versions.
    let sent = run(
        &dir.0,
        mdkg,
        &[
            "--as", "a", "msg", "send", "--to", "b", "--kind", "note", "--text", "x", "--json",
        ],
    );
    assert!(
        sent.status.success(),
        "{}",
        String::from_utf8_lossy(&sent.stdout)
    );
    for planted in PLANTED {
        let file = dir.0.join(planted);
        fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
        let text = if planted.ends_with(".json") {
            format!(
                "{{\"canary\": \"{CANARY}\", \"mcpServers\": {{\"{CANARY}\": {{\"command\": \"{CANARY}\"}}}}}}\n"
            )
        } else if planted.ends_with(".toml") {
            format!("model = \"{CANARY}\"\n[mcp_servers.{CANARY}]\ncommand = \"{CANARY}\"\n")
        } else {
            format!("{CANARY}\n")
        };
        fs::write(&file, text).expect("plant");
    }
    let mut before = BTreeMap::new();
    snapshot(&dir.0, &mut before);
    // Coarse file times: make any rewrite show in its time.
    std::thread::sleep(std::time::Duration::from_millis(1100));

    let out = run(&dir.0, mdkg, &["doctor", "--only", "store", "--json"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert!(!stdout.contains(CANARY), "{stdout}");
    assert!(!stderr.contains(CANARY), "{stderr}");
    let envelope: Value = serde_json::from_slice(&out.stdout).expect("JSON");
    common::conforms("doctor", &envelope);
    let checks = envelope["data"]["checks"].as_array().expect("checks");
    assert_eq!(checks.len(), 1, "{envelope}");
    let store = &checks[0];
    assert_eq!(store["name"], "store", "{store}");
    assert_eq!(store["status"], "ok", "{store}");
    assert!(
        store["store_schema"].as_u64().is_some_and(|v| v > 0),
        "{store}"
    );
    assert!(
        store["mail_schema"].as_u64().is_some_and(|v| v > 0),
        "{store}"
    );
    assert_eq!(store["upgrade_needed"], false, "{store}");

    let mut after = BTreeMap::new();
    snapshot(&dir.0, &mut after);
    // SQLite's own WAL index and log beside the store: a read-only reader
    // of a WAL database keeps them, as every read does. The database file
    // itself, and everything else, stays as it was.
    let sidecar = |path: &Path| {
        path.file_name()
            .is_some_and(|n| n == "coord.sqlite-wal" || n == "coord.sqlite-shm")
    };
    assert!(
        before.keys().any(|p| p.ends_with(".git/mdkg/coord.sqlite")),
        "the store exists"
    );
    let changed: Vec<_> = after
        .iter()
        .filter(|(path, _)| !sidecar(path))
        .filter(|(path, now)| before.get(*path) != Some(*now))
        .map(|(path, _)| path.display().to_string())
        .chain(
            before
                .keys()
                .filter(|path| !sidecar(path) && !after.contains_key(*path))
                .map(|path| format!("removed {}", path.display())),
        )
        .collect();
    assert!(changed.is_empty(), "doctor --only store wrote: {changed:?}");

    // `--only` and `--repair` don't mix.
    let both = run(&dir.0, mdkg, &["doctor", "--only", "store", "--repair"]);
    assert!(!both.status.success());
}

/// `doctor --only store --json`'s one check.
fn store_only(dir: &Path, cwd: &Path) -> (bool, Value) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mdkg"));
    let out = isolated(&mut cmd, &dir.join("home"))
        .current_dir(cwd)
        .args(["doctor", "--only", "store", "--json"])
        .output()
        .expect("runs");
    let envelope: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|_| {
        panic!(
            "not JSON: {}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    });
    common::conforms("doctor", &envelope);
    let checks = envelope["data"]["checks"].as_array().expect("checks");
    assert_eq!(checks.len(), 1, "{envelope}");
    (out.status.success(), checks[0].clone())
}

/// A repository with a graph and a store, at `dir/repo`.
fn with_store(name: &str) -> Scratch {
    let dir = Scratch(fs::canonicalize(common::temp::dir(name)).expect("dir"));
    fs::create_dir_all(dir.0.join("repo")).expect("repo");
    fs::create_dir_all(dir.0.join("home")).expect("home");
    let mdkg = env!("CARGO_BIN_EXE_mdkg");
    assert!(
        run(&dir.0, "git", &["init", "-q", "-b", "main"])
            .status
            .success()
    );
    for args in [
        &["--as", "launcher", "init", "--json"][..],
        &[
            "--as", "a", "msg", "send", "--to", "b", "--kind", "note", "--text", "x", "--json",
        ][..],
    ] {
        let out = run(&dir.0, mdkg, args);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stdout)
        );
    }
    dir
}

/// A git repository nested under a graph with a store reads its own
/// store, not the parent's its config would name (review of fad81967).
#[test]
fn only_store_in_a_nested_repository_reads_that_repository() {
    let dir = with_store("mdkg-cli-doctor-nested");
    let inner = dir.0.join("repo/inner");
    fs::create_dir_all(&inner).expect("inner");
    let out = Command::new("git")
        .args(["init", "-q", "-b", "main"])
        .current_dir(&inner)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git");
    assert!(out.status.success());
    // The parent's store is there to be read wrongly.
    let (ok, parent) = store_only(&dir.0, &dir.0.join("repo"));
    assert!(ok, "{parent}");
    assert!(parent["store_schema"].is_u64(), "{parent}");
    let (ok, store) = store_only(&dir.0, &inner);
    assert!(ok, "{store}");
    assert_eq!(store["status"], "ok", "{store}");
    assert!(
        store["summary"]
            .as_str()
            .is_some_and(|m| m.contains("no store yet")),
        "{store}"
    );
    assert!(store.get("store_schema").is_none(), "{store}");
    assert!(!inner.join(".git/mdkg/coord.sqlite").exists());
}

/// A `.git` git can't open fails the check; it's never skipped as no
/// repository (review of fad81967).
#[test]
fn only_store_fails_where_git_cant_open_the_repository() {
    let dir = Scratch(fs::canonicalize(common::temp::dir("mdkg-cli-doctor-broken")).expect("dir"));
    let repo = dir.0.join("repo");
    fs::create_dir_all(&repo).expect("repo");
    fs::create_dir_all(dir.0.join("home")).expect("home");
    fs::write(
        repo.join(".git"),
        format!("gitdir: {}\n", dir.0.join("gone").display()),
    )
    .expect(".git");
    let (ok, store) = store_only(&dir.0, &repo);
    assert!(!ok, "{store}");
    assert_eq!(store["status"], "error", "{store}");
    assert!(store.get("store_schema").is_none(), "{store}");
}

/// An older store this build would upgrade: its versions are read and
/// `upgrade_needed` is true, and the store stays at its version with its
/// data (review of fad81967).
#[test]
fn only_store_reads_an_older_store_without_upgrading_it() {
    let dir = with_store("mdkg-cli-doctor-older");
    let path = dir.0.join("repo/.git/mdkg/coord.sqlite");
    let logical = || {
        let conn = rusqlite::Connection::open(&path).expect("open");
        let mut meta = conn
            .prepare("SELECT key, value FROM meta ORDER BY key")
            .expect("meta");
        let meta: Vec<(String, String)> = meta
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .expect("rows")
            .map(|r| r.expect("row"))
            .collect();
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .expect("tables")
            .query_map([], |r| r.get(0))
            .expect("rows")
            .map(|r| r.expect("row"))
            .collect();
        let rows: Vec<(String, i64)> = tables
            .iter()
            .map(|t| {
                let n = conn
                    .query_row(&format!("SELECT count(*) FROM \"{t}\""), [], |r| r.get(0))
                    .expect("count");
                (t.clone(), n)
            })
            .collect();
        (meta, rows)
    };
    // Version 3 is one this build upgrades from (store.rs UPGRADES_FROM).
    rusqlite::Connection::open(&path)
        .expect("open")
        .execute("UPDATE meta SET value = '3' WHERE key = 'schema'", [])
        .expect("downgrade");
    let before = logical();
    let bytes = fs::read(&path).expect("store");
    let (_, store) = store_only(&dir.0, &dir.0.join("repo"));
    assert_eq!(store["store_schema"], 3, "{store}");
    assert_eq!(store["upgrade_needed"], true, "{store}");
    assert_eq!(
        fs::read(&path).expect("store"),
        bytes,
        "the store file changed"
    );
    assert_eq!(logical(), before, "the store's contents changed");
    let snapshots: Vec<_> = fs::read_dir(path.parent().expect("dir"))
        .expect("dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains(".pre-"))
        .collect();
    assert!(snapshots.is_empty(), "an upgrade copy was taken");
}

/// A store newer than this build reports its own versions as a warning,
/// not a failure (orchestrator #20626), and stays as it was.
#[test]
fn only_store_reports_a_newer_stores_versions_as_a_warning() {
    let dir = with_store("mdkg-cli-doctor-newer");
    let path = dir.0.join("repo/.git/mdkg/coord.sqlite");
    let conn = rusqlite::Connection::open(&path).expect("open");
    conn.execute("UPDATE meta SET value = '99' WHERE key = 'schema'", [])
        .expect("schema");
    conn.execute("UPDATE meta SET value = '98' WHERE key = 'msg_schema'", [])
        .expect("mail schema");
    drop(conn);
    let bytes = fs::read(&path).expect("store");
    let (ok, store) = store_only(&dir.0, &dir.0.join("repo"));
    assert!(ok, "a newer store doesn't fail the command: {store}");
    assert_eq!(store["status"], "warning", "{store}");
    assert_eq!(store["store_schema"], 99, "{store}");
    assert_eq!(store["mail_schema"], 98, "{store}");
    assert_eq!(store["upgrade_needed"], false, "{store}");
    assert_eq!(
        fs::read(&path).expect("store"),
        bytes,
        "the store file changed"
    );
}

/// A store that can't be read fails the check and the command; it never
/// reads as no store (review of cc12f194).
#[test]
fn only_store_fails_on_an_unreadable_store() {
    let dir = with_store("mdkg-cli-doctor-unreadable");
    let path = dir.0.join("repo/.git/mdkg/coord.sqlite");
    for sidecar in ["coord.sqlite-wal", "coord.sqlite-shm"] {
        fs::remove_file(path.with_file_name(sidecar)).ok();
    }
    fs::write(&path, b"not a sqlite database, just bytes\n").expect("corrupt");
    let (ok, store) = store_only(&dir.0, &dir.0.join("repo"));
    assert!(!ok, "{store}");
    assert_eq!(store["status"], "error", "{store}");
    assert!(store.get("store_schema").is_none(), "{store}");
}

/// A repository with no store whose git config git can't read fails the
/// check with git's reason; it's never "not in a git repository"
/// (review of f167c83d).
#[test]
fn only_store_fails_where_the_git_config_is_broken() {
    let dir =
        Scratch(fs::canonicalize(common::temp::dir("mdkg-cli-doctor-badconfig")).expect("dir"));
    fs::create_dir_all(dir.0.join("repo")).expect("repo");
    fs::create_dir_all(dir.0.join("home")).expect("home");
    assert!(
        run(&dir.0, "git", &["init", "-q", "-b", "main"])
            .status
            .success()
    );
    let config = dir.0.join("repo/.git/config");
    let mut text = fs::read_to_string(&config).expect("config");
    text.push_str("[broken\n");
    fs::write(&config, text).expect("break config");
    let (ok, store) = store_only(&dir.0, &dir.0.join("repo"));
    assert!(!ok, "{store}");
    assert_eq!(store["status"], "error", "{store}");
    assert!(
        !store["summary"]
            .as_str()
            .is_some_and(|s| s.contains("not in a git repository")),
        "{store}"
    );
    assert!(!dir.0.join("repo/.git/mdkg").exists());
}

/// `doctor --only store --root <root> --json`'s one check and exit code,
/// with `envs` added.
fn store_at(dir: &Path, root: &Path, envs: &[(&str, &str)]) -> (i32, Value) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mdkg"));
    isolated(&mut cmd, &dir.join("home"))
        .current_dir(dir)
        .arg("--root")
        .arg(root)
        .args(["doctor", "--only", "store", "--json"]);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("runs");
    let envelope: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|_| {
        panic!(
            "not JSON: {}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    });
    common::conforms("doctor", &envelope);
    let checks = envelope["data"]["checks"].as_array().expect("checks");
    assert_eq!(checks.len(), 1, "{envelope}");
    (out.status.code().unwrap_or(-1), checks[0].clone())
}

/// An inherited GIT_DIR outside any repository is skipped as no
/// repository, as without it: hardened git clears it (#20870).
#[test]
fn only_store_ignores_an_inherited_git_dir_outside_a_repository() {
    let dir = Scratch(fs::canonicalize(common::temp::dir("mdkg-cli-doctor-gitdir")).expect("dir"));
    let plain = dir.0.join("plain");
    fs::create_dir_all(&plain).expect("plain");
    fs::create_dir_all(dir.0.join("home")).expect("home");
    let (code, bare) = store_at(&dir.0, &plain, &[]);
    assert_eq!((code, bare["status"].as_str()), (0, Some("skip")), "{bare}");
    let (code, store) = store_at(&dir.0, &plain, &[("GIT_DIR", "/nonexistent")]);
    assert_eq!(
        (code, store["status"].as_str()),
        (0, Some("skip")),
        "{store}"
    );
}

/// A --root that isn't there fails the check with the reason; it's never
/// skipped as a folder outside git (#20870).
#[test]
fn only_store_fails_on_an_absent_root() {
    let dir = Scratch(fs::canonicalize(common::temp::dir("mdkg-cli-doctor-absent")).expect("dir"));
    fs::create_dir_all(dir.0.join("home")).expect("home");
    let gone = dir.0.join("gone");
    let (code, store) = store_at(&dir.0, &gone, &[]);
    assert_ne!(code, 0, "{store}");
    assert_eq!(store["status"], "error", "{store}");
    assert!(
        store["summary"]
            .as_str()
            .is_some_and(|s| s.contains(&*gone.to_string_lossy())),
        "{store}"
    );
}

/// A repository folder git can't enter (no search permission) fails the
/// check, with or without an inherited GIT_DIR: its `.git` can't be
/// looked up, which isn't "no repository" (#20928).
#[test]
fn only_store_fails_on_a_repository_it_cant_enter() {
    use std::os::unix::fs::PermissionsExt;
    let dir = Scratch(fs::canonicalize(common::temp::dir("mdkg-cli-doctor-noexec")).expect("dir"));
    let repo = dir.0.join("repo");
    fs::create_dir_all(&repo).expect("repo");
    fs::create_dir_all(dir.0.join("home")).expect("home");
    assert!(
        run(&dir.0, "git", &["init", "-q", "-b", "main"])
            .status
            .success()
    );
    fs::set_permissions(&repo, fs::Permissions::from_mode(0o600)).expect("drop search");
    let results: Vec<(i32, Value)> = [&[][..], &[("GIT_DIR", "/nonexistent")]]
        .iter()
        .map(|envs| store_at(&dir.0, &repo, envs))
        .collect();
    fs::set_permissions(&repo, fs::Permissions::from_mode(0o755)).expect("restore");
    for (code, store) in results {
        assert_ne!(code, 0, "{store}");
        assert_eq!(store["status"], "error", "{store}");
    }
}
