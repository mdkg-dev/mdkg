//! Upgrade safety end to end (orchestrator, 2026-10-21, D): an upgrade
//! copies the store first and says where, `store snapshots` lists the
//! copies, and `store restore` puts one back, refusing while anything has
//! the store open and refusing copies it can't trust.

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
    let dir = common::temp::dir("mdkg-cli-backup");
    Scratch(fs::canonicalize(dir).expect("canonical"))
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

fn isolated(cmd: &mut Command) -> &mut Command {
    common::without_launcher_env(cmd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
}

/// A repository with the basic graph committed on main.
fn repo(dir: &Scratch, name: &str) -> PathBuf {
    let repo = dir.0.join(name);
    let conformance = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance");
    fs::create_dir_all(&repo).expect("repo");
    copy_tree(&conformance.join("base"), &repo);
    copy_tree(&conformance.join("cases/g01-valid-basic"), &repo);
    fs::remove_file(repo.join("commands")).ok();
    for args in [
        &["init", "-q", "-b", "main"][..],
        &["add", "."],
        &["commit", "-q", "-m", "init"],
    ] {
        let out = isolated(Command::new("git").arg("-C").arg(&repo).args(args))
            .output()
            .expect("git");
        assert!(out.status.success(), "git {args:?}");
    }
    repo
}

/// Runs the test build, which sits in a Cargo build folder and so may
/// upgrade a store only when told to (task-39).
fn run(repo: &Path, args: &[&str]) -> (i32, Value) {
    let mut all = vec!["--allow-store-upgrade"];
    all.extend_from_slice(args);
    run_as(Path::new(env!("CARGO_BIN_EXE_mdkg")), repo, &all)
}

fn run_as(exe: &Path, repo: &Path, args: &[&str]) -> (i32, Value) {
    let out = isolated(&mut Command::new(exe))
        .arg("--root")
        .arg(repo)
        .args(["--as", "a"])
        .args(args)
        .arg("--json")
        .env("MDKG_NOW_MS", "1000")
        .output()
        .expect("run mdkg");
    let envelope: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|_| {
        panic!(
            "{args:?}: not JSON: {}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    });
    common::conforms_envelope(&envelope);
    (out.status.code().unwrap_or(-1), envelope)
}

fn codes(envelope: &Value) -> Vec<String> {
    envelope["diagnostics"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|d| d["code"].as_str().map(String::from))
        .collect()
}

fn set_schema(db: &Path, version: &str) {
    rusqlite::Connection::open(db)
        .expect("store")
        .execute("UPDATE meta SET value = ?1 WHERE key = 'schema'", [version])
        .expect("schema");
}

fn scopes(status: &Value) -> Vec<String> {
    status["data"]["leases"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|l| l["scope"].as_str().map(String::from))
        .collect()
}

#[test]
fn an_upgrade_copies_the_store_and_a_restore_puts_it_back() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    let db = repo.join(".git/mdkg/coord.sqlite");
    let (code, out) = run(&repo, &["claim", "task-1"]);
    assert_eq!(code, 0, "{out}");
    set_schema(&db, "1");

    // The command that upgrades the store says so, and where the copy is.
    let (code, out) = run(&repo, &["status"]);
    assert_eq!(code, 0, "{out}");
    let notice = out["diagnostics"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|d| d["code"] == "coord.store_upgraded")
        .unwrap_or_else(|| panic!("no upgrade notice: {out}"));
    let copy = db.with_file_name("coord.sqlite.pre-1+msg5-to-3+msg5.sqlite");
    assert!(
        notice["message"]
            .as_str()
            .is_some_and(|m| m.contains("coord.sqlite.pre-1+msg5-to-3+msg5.sqlite")),
        "{out}"
    );
    assert!(copy.exists());
    let (_, out) = run(&repo, &["status"]);
    assert!(
        !codes(&out).contains(&"coord.store_upgraded".to_string()),
        "once: {out}"
    );

    let (code, out) = run(&repo, &["store", "snapshots"]);
    assert_eq!(code, 0, "{out}");
    let listed = &out["data"]["snapshots"][0];
    assert_eq!(listed["schema"], "1", "{out}");
    assert_eq!(listed["restorable"], true, "{out}");

    // Work after the upgrade, then the copy back.
    let (code, out) = run(&repo, &["claim", "task-2"]);
    assert_eq!(code, 0, "{out}");

    // Not while anything has the store open.
    {
        let held = rusqlite::Connection::open(&db).expect("open");
        let _: String = held
            .query_row("SELECT value FROM meta WHERE key = 'schema'", [], |r| {
                r.get(0)
            })
            .expect("read");
        let (code, out) = run(
            &repo,
            &[
                "store",
                "restore",
                "coord.sqlite.pre-1+msg5-to-3+msg5.sqlite",
            ],
        );
        assert_eq!(
            (code, codes(&out)),
            (5, vec!["coord.store_failed".to_string()]),
            "{out}"
        );
        assert!(
            out["diagnostics"][0]["message"]
                .as_str()
                .is_some_and(|m| m.contains("another process")),
            "{out}"
        );
    }

    let (code, out) = run(
        &repo,
        &[
            "store",
            "restore",
            "coord.sqlite.pre-1+msg5-to-3+msg5.sqlite",
        ],
    );
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["data"]["replaced"], "3+msg5", "{out}");
    assert_eq!(out["data"]["restored"], "1+msg5", "{out}");
    assert!(
        codes(&out).contains(&"coord.store_restored".to_string()),
        "the restored store is older: {out}"
    );
    let previous = PathBuf::from(out["data"]["previous"].as_str().expect("previous"));
    assert!(previous.exists(), "{out}");
    // The restored store is the copy: task-1 held, task-2 not; it
    // upgrades again on use, copying first.
    let (code, out) = run(&repo, &["status", "--all"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(scopes(&out), vec![".mdkg/work/task-1-basic.md"], "{out}");
    assert!(
        codes(&out).contains(&"coord.store_upgraded".to_string()),
        "{out}"
    );

    // Copies it can't trust: a newer build's, another repository's, and a
    // store that isn't a copy.
    let newer = db.with_file_name("coord.sqlite.pre-9-to-10.sqlite");
    fs::copy(&copy, &newer).expect("copy");
    set_schema(&newer, "9");
    let (code, out) = run(&repo, &["store", "restore", newer.to_str().expect("utf-8")]);
    assert_eq!(code, 5, "{out}");
    assert!(
        out["diagnostics"][0]["message"]
            .as_str()
            .is_some_and(|m| m.contains("newer build")),
        "{out}"
    );
    let other = self::repo(&dir, "other");
    let (code, out) = run(&other, &["status"]);
    assert_eq!(code, 0, "{out}");
    let (code, out) = run(&other, &["store", "restore", copy.to_str().expect("utf-8")]);
    assert_eq!(code, 5, "{out}");
    assert!(
        out["diagnostics"][0]["message"]
            .as_str()
            .is_some_and(|m| m.contains("not of this repository's store")),
        "{out}"
    );
    let other_db = other.join(".git/mdkg/coord.sqlite");
    let (code, out) = run(
        &repo,
        &["store", "restore", other_db.to_str().expect("utf-8")],
    );
    assert_eq!(code, 5, "{out}");
}

/// task-39, the 10-22 incident: the build in the Cargo build folder won't
/// upgrade an existing store without `--allow-store-upgrade`, and changes
/// nothing; the same binary copied out of the build folder (as `cargo
/// install` and the package managers do) upgrades it.
#[test]
fn only_an_installed_build_upgrades_a_store_unasked() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    let db = repo.join(".git/mdkg/coord.sqlite");
    let built = Path::new(env!("CARGO_BIN_EXE_mdkg"));
    // A new store is no upgrade.
    let (code, out) = run_as(built, &repo, &["claim", "task-1"]);
    assert_eq!(code, 0, "{out}");
    set_schema(&db, "1");

    // Commands that act on the coordinator fail; reads warn and go on
    // without it.
    for (args, fails) in [
        (&["status"][..], true),
        (&["claim", "task-2"], true),
        (&["show", "task-1"], false),
    ] {
        let (code, out) = run_as(built, &repo, args);
        assert_eq!(code != 0, fails, "{args:?}: {out}");
        assert_eq!(codes(&out), vec!["coord.store_failed".to_string()], "{out}");
        let d = &out["diagnostics"][0];
        assert!(
            d["message"]
                .as_str()
                .is_some_and(|m| m.contains("Cargo build folder") && m.contains("1+msg5")),
            "{out}"
        );
        assert!(
            d["hint"]
                .as_str()
                .is_some_and(|h| h.contains("--allow-store-upgrade")),
            "{out}"
        );
    }
    let schema: String = rusqlite::Connection::open(&db)
        .expect("store")
        .query_row("SELECT value FROM meta WHERE key = 'schema'", [], |r| {
            r.get(0)
        })
        .expect("schema");
    assert_eq!(schema, "1", "not upgraded");
    let copies: Vec<_> = fs::read_dir(db.parent().expect("dir"))
        .expect("dir")
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().contains(".pre-"))
        .collect();
    assert!(copies.is_empty(), "nothing copied: {copies:?}");

    // Installed: the same binary outside any build folder.
    let bin = dir.0.join("bin");
    fs::create_dir_all(&bin).expect("bin");
    let installed = bin.join("mdkg");
    fs::copy(built, &installed).expect("install");
    let (code, out) = run_as(&installed, &repo, &["status"]);
    assert_eq!(code, 0, "{out}");
    assert!(
        codes(&out).contains(&"coord.store_upgraded".to_string()),
        "{out}"
    );
}
