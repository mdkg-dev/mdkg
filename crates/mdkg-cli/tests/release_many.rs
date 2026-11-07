//! `release --all` (lease hygiene, 0.7.0): every active lease of the
//! actor's, each released in its own transaction, so a failed write leaves
//! the rest done and running the command again finishes the job.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

mod common;

const T1: &str = ".mdkg/work/task-1-basic.md";

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).ok();
    }
}

fn scratch() -> Scratch {
    let dir = common::temp::dir("mdkg-cli-release-many");
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

fn mdkg(worktree: &Path, actor: &str, args: &[&str]) -> std::process::Output {
    isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
        .arg("--root")
        .arg(worktree)
        .args(["--as", actor])
        .args(args)
        .output()
        .expect("run mdkg")
}

/// Runs mdkg as `actor` and returns (exit code, envelope).
fn run(worktree: &Path, actor: &str, args: &[&str]) -> (i32, Value) {
    let mut all = args.to_vec();
    all.push("--json");
    let out = mdkg(worktree, actor, &all);
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

fn ok(worktree: &Path, actor: &str, args: &[&str]) -> Value {
    let (code, out) = run(worktree, actor, args);
    assert_eq!(code, 0, "{args:?}: {out}");
    out
}

/// A graph on `main` with a `notes/` folder, and a worktree `wt-a` on
/// branch `a`. Returns (main checkout, worktree).
fn setup(dir: &Scratch) -> (PathBuf, PathBuf) {
    let repo = dir.0.join("repo");
    let conformance = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance");
    fs::create_dir_all(repo.join("notes")).expect("repo");
    copy_tree(&conformance.join("base"), &repo);
    copy_tree(&conformance.join("cases/g01-valid-basic"), &repo);
    fs::remove_file(repo.join("commands")).ok();
    fs::remove_file(repo.join("README.md")).ok();
    fs::write(repo.join("notes/a.txt"), "a\n").expect("note");
    fs::write(repo.join("notes/b.txt"), "b\n").expect("note");
    fs::write(repo.join("lib.txt"), "lib\n").expect("file");
    sh(&repo, &["init", "-q", "-b", "main"]);
    sh(&repo, &["add", "."]);
    sh(&repo, &["commit", "-q", "-m", "init"]);
    let wt = dir.0.join("wt-a");
    sh(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "a",
            wt.to_str().expect("utf-8"),
            "main",
        ],
    );
    (repo, wt)
}

fn scopes(list: &Value) -> Vec<String> {
    let mut out: Vec<String> = list
        .as_array()
        .expect("a list")
        .iter()
        .map(|l| {
            l["scope"]
                .as_str()
                .or_else(|| l["lease"]["scope"].as_str())
                .expect("scope")
                .to_string()
        })
        .collect();
    out.sort();
    out
}

/// The actor's active leases, by scope, as `status` lists them.
fn held(worktree: &Path, actor: &str) -> Vec<String> {
    let out = ok(worktree, actor, &["status"]);
    let mut v: Vec<String> = out["data"]["leases"]
        .as_array()
        .expect("leases")
        .iter()
        .filter(|l| l["owner"] == actor && l["status"] == "active")
        .map(|l| l["scope"].as_str().expect("scope").to_string())
        .collect();
    v.sort();
    v
}

#[test]
fn release_all_releases_every_lease_of_the_actor_once() {
    let dir = scratch();
    let (repo, wt) = setup(&dir);
    ok(&wt, "a", &["claim", "task-1"]);
    ok(&wt, "a", &["claim", "notes"]);
    ok(&wt, "a", &["claim", "role:reviewer"]);
    ok(&repo, "b", &["claim", "lib.txt"]);

    let out = ok(&wt, "a", &["release", "--all"]);
    assert_eq!(out["schema"], "mdkg.release.v1");
    assert_eq!(
        scopes(&out["data"]["released"]),
        vec![T1.to_string(), "notes".into(), "role:reviewer".into()],
        "{out}"
    );
    assert_eq!(out["data"]["skipped"], serde_json::json!([]));
    assert_eq!(out["data"]["failed"], serde_json::json!([]));
    for l in out["data"]["released"].as_array().expect("released") {
        assert_eq!(l["status"], "released", "{l}");
    }
    assert!(held(&wt, "a").is_empty());
    assert_eq!(held(&repo, "b"), vec!["lib.txt".to_string()], "b's stays");

    // Nothing left: the same command again is a quiet success.
    let out = ok(&wt, "a", &["release", "--all"]);
    assert_eq!(out["data"]["released"], serde_json::json!([]));
    let text = mdkg(&wt, "a", &["release", "--all"]);
    assert!(text.status.success());
    assert_eq!(
        String::from_utf8_lossy(&text.stdout).trim(),
        "no active leases to release"
    );
}

#[test]
fn a_failed_release_mid_batch_leaves_the_rest_and_a_rerun_finishes() {
    let dir = scratch();
    let (repo, wt) = setup(&dir);
    ok(&wt, "a", &["claim", "task-1"]);
    let notes = ok(&wt, "a", &["claim", "notes"]);
    ok(&wt, "a", &["claim", "lib.txt"]);
    let id = notes["data"]["lease"]["id"]
        .as_str()
        .expect("id")
        .to_string();

    // One failed write: the store refuses to journal the release of the
    // `notes` lease, as a full disk or an I/O error would.
    let store = repo.join(".git/mdkg/coord.sqlite");
    let conn = rusqlite::Connection::open(&store).expect("store");
    conn.execute_batch(&format!(
        "CREATE TRIGGER fail_one BEFORE INSERT ON journal
         WHEN NEW.command LIKE '%elease%' AND NEW.command LIKE '%\"{id}\"%'
         BEGIN SELECT RAISE(ABORT, 'injected write failure'); END;"
    ))
    .expect("trigger");
    drop(conn);

    let (code, out) = run(&wt, "a", &["release", "--all"]);
    assert_ne!(code, 0, "{out}");
    assert_eq!(out["ok"], false);
    assert_eq!(
        scopes(&out["data"]["released"]),
        vec![T1.to_string(), "lib.txt".into()],
        "the others are released: {out}"
    );
    let failed = out["data"]["failed"].as_array().expect("failed");
    assert_eq!(failed.len(), 1, "{out}");
    assert_eq!(failed[0]["lease"]["scope"], "notes");
    assert_eq!(failed[0]["why"], "failed");
    assert!(
        failed[0]["reason"]
            .as_str()
            .is_some_and(|r| r.contains("injected write failure")),
        "{out}"
    );
    let message = out["diagnostics"][0]["message"]
        .as_str()
        .unwrap_or_default();
    assert!(
        message.starts_with("1 lease(s) couldn't be released"),
        "{out}"
    );
    assert_eq!(held(&wt, "a"), vec!["notes".to_string()], "only it is held");

    // The write works again: the same command finishes the job.
    rusqlite::Connection::open(&store)
        .expect("store")
        .execute_batch("DROP TRIGGER fail_one;")
        .expect("drop trigger");
    let out = ok(&wt, "a", &["release", "--all"]);
    assert_eq!(scopes(&out["data"]["released"]), vec!["notes".to_string()]);
    assert_eq!(out["data"]["failed"], serde_json::json!([]));
    assert!(held(&wt, "a").is_empty());
}

#[test]
fn release_takes_a_target_or_all_never_both() {
    let dir = scratch();
    let (_repo, wt) = setup(&dir);
    // `--merged` isn't in 0.7.0 (it waits for leases that record their
    // worktree), so it's an unknown argument.
    for args in [
        &["release", "task-1", "--all"][..],
        &["release", "--all", "--scope", "lib.txt"],
        &["release", "--merged"],
        &["release"],
        &["release", "task-1", "--clean"],
        &["release", "--all", "--clean"],
        &["release", "task-1", "notes", "--all"],
        &["release", "task-1", "notes", "--committed"],
        &["release", "--scope", "lib.txt", "notes"],
    ] {
        let (code, out) = run(&wt, "a", args);
        assert_eq!(code, 1, "{args:?}: {out}");
        assert_eq!(
            out["diagnostics"][0]["code"], "usage.invalid_arguments",
            "{out}"
        );
    }
}

#[test]
fn beside_a_lease_authority_only_local_leases_go_and_it_says_so() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch();
    let (repo, wt) = setup(&dir);
    ok(&wt, "a", &["claim", "role:reviewer"]);
    // An authority that records any call: --all must make none (contract
    // 1 has no call that lists an actor's leases there).
    let calls = dir.0.join("calls.log");
    let script = dir.0.join("authority.sh");
    fs::write(
        &script,
        format!("#!/bin/sh\necho \"$@\" >> '{}'\nexit 1\n", calls.display()),
    )
    .expect("script");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod");
    let config = dir.0.join("config");
    let folder = config.join("mdkg/lease-authority");
    fs::create_dir_all(&folder).expect("folder");
    for f in [config.join("mdkg"), folder.clone()] {
        fs::set_permissions(&f, fs::Permissions::from_mode(0o700)).expect("chmod");
    }
    let common = String::from_utf8(
        isolated(Command::new("git").arg("-C").arg(&repo).args([
            "rev-parse",
            "--path-format=absolute",
            "--git-common-dir",
        ]))
        .output()
        .expect("git")
        .stdout,
    )
    .expect("utf-8");
    let common = fs::canonicalize(common.trim())
        .expect("canonical")
        .to_string_lossy()
        .into_owned();
    let key = mdkg_format::Digest::of_bytes(common.as_bytes());
    let file = folder.join(format!("{}.json", key.hex()));
    fs::write(
        &file,
        serde_json::json!({"common_dir": common, "command": [script.to_string_lossy()], "timeout_ms": 120_000})
            .to_string(),
    )
    .expect("authority file");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).expect("chmod");

    let out = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
        .arg("--root")
        .arg(&wt)
        .args(["--as", "a", "release", "--all", "--json"])
        .env("XDG_CONFIG_HOME", &config)
        .output()
        .expect("run mdkg");
    let out: Value = serde_json::from_slice(&out.stdout).expect("json");
    common::conforms_envelope(&out);
    assert_eq!(out["ok"], true, "{out}");
    assert_eq!(
        scopes(&out["data"]["released"]),
        vec!["role:reviewer".to_string()],
        "{out}"
    );
    let warned = out["diagnostics"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|d| d["code"] == "coord.authority_remote" && d["severity"] == "warning");
    assert!(warned, "{out}");
    assert!(!calls.exists(), "the authority was never called");

    // Several targets beside an authority: refused, one at a time.
    ok(&wt, "a", &["claim", "role:reviewer"]);
    let out = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
        .arg("--root")
        .arg(&wt)
        .args(["--as", "a", "release", "role:reviewer", "lib.txt", "--json"])
        .env("XDG_CONFIG_HOME", &config)
        .output()
        .expect("run mdkg");
    assert_eq!(out.status.code(), Some(1));
    let out: Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(
        out["diagnostics"][0]["code"], "usage.invalid_arguments",
        "{out}"
    );
    assert_eq!(held(&wt, "a"), vec!["role:reviewer".to_string()], "{out}");
    assert!(!calls.exists(), "the authority was never called");
}

/// `release a b c`: every target in one call,
/// all or none, with each target's lease listed.
#[test]
fn release_takes_several_targets_all_or_none() {
    let dir = scratch();
    let (repo, wt) = setup(&dir);
    ok(&wt, "a", &["claim", "task-1"]);
    ok(&wt, "a", &["claim", "notes"]);
    ok(&wt, "a", &["claim", "lib.txt"]);
    let out = ok(&wt, "a", &["release", "task-1", "notes", "lib.txt"]);
    assert_eq!(out["schema"], "mdkg.release.v1");
    let releases = out["data"]["releases"].as_array().expect("releases");
    let got: Vec<(&str, &str, &str)> = releases
        .iter()
        .map(|r| {
            (
                r["target"].as_str().expect("target"),
                r["lease"]["scope"].as_str().expect("scope"),
                r["lease"]["status"].as_str().expect("status"),
            )
        })
        .collect();
    assert_eq!(
        got,
        [
            ("task-1", T1, "released"),
            ("notes", "notes", "released"),
            ("lib.txt", "lib.txt", "released"),
        ],
        "{out}"
    );
    assert_eq!(out["data"]["lease"], releases[0]["lease"], "{out}");
    assert!(held(&wt, "a").is_empty());

    // One target with no lease of yours refuses them all: nothing goes.
    ok(&wt, "a", &["claim", "notes"]);
    ok(&wt, "a", &["claim", "lib.txt"]);
    let (code, out) = run(&wt, "a", &["release", "notes", "lib.txt", "role:other"]);
    assert_ne!(code, 0, "{out}");
    assert_eq!(out["diagnostics"][0]["code"], "coord.no_lease", "{out}");
    let message = out["diagnostics"][0]["message"].as_str().expect("message");
    assert!(message.starts_with("role:other: "), "{out}");
    let hint = out["diagnostics"][0]["hint"].as_str().expect("hint");
    assert!(hint.contains("none of the 3 targets was released"), "{out}");
    assert_eq!(
        held(&wt, "a"),
        vec!["lib.txt".to_string(), "notes".into()],
        "{out}"
    );
    // Another actor's lease, by id, refuses them all too.
    let b = ok(&repo, "b", &["claim", "task-1"]);
    let b_id = b["data"]["lease"]["id"].as_str().expect("id");
    let (code, out) = run(&wt, "a", &["release", "notes", b_id]);
    assert_ne!(code, 0, "{out}");
    // The store refuses it after `notes` in the same transaction, which
    // rolls back whole.
    assert_eq!(out["diagnostics"][0]["code"], "coord.not_owner", "{out}");
    let hint = out["diagnostics"][0]["hint"].as_str().expect("hint");
    assert!(hint.contains("none of the 2 targets was released"), "{out}");
    assert_eq!(
        held(&wt, "a"),
        vec!["lib.txt".to_string(), "notes".into()],
        "{out}"
    );
    assert_eq!(held(&repo, "b"), vec![T1.to_string()], "b's stays");

    // The same lease twice, and one already ended, are fine; text lists
    // each target's lease.
    let ended = ok(&wt, "a", &["claim", "role:reviewer"]);
    let ended_id = ended["data"]["lease"]["id"].as_str().expect("id");
    ok(&wt, "a", &["release", "role:reviewer"]);
    let text = mdkg(
        &wt,
        "a",
        &["release", "notes", "notes/", ended_id, "lib.txt"],
    );
    assert!(text.status.success(), "{text:?}");
    let lines: Vec<String> = String::from_utf8_lossy(&text.stdout)
        .lines()
        .map(|l| l.split(" (").next().unwrap_or_default().to_string())
        .collect();
    assert_eq!(
        lines,
        [
            "released notes",
            "released notes",
            "released role:reviewer",
            "released lib.txt"
        ],
        "{text:?}"
    );
    assert!(held(&wt, "a").is_empty());
}

#[test]
fn profile_c_node_leases_go_too() {
    let dir = scratch();
    let repo = dir.0.join("repo");
    fs::create_dir_all(&repo).expect("repo");
    copy_tree(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance/base"),
        &repo,
    );
    let path = repo.join(".mdkg/config.json");
    let mut config: Value =
        serde_json::from_str(&fs::read_to_string(&path).expect("config")).expect("json");
    config["schema_version"] = 3.into();
    config["format_version"] = "0.7".into();
    config["graph_id"] = "0192f3a0-0000-7000-8000-000000000000".into();
    if config.get("workspaces").is_none() {
        config["workspaces"] =
            serde_json::json!({ "root": { "path": ".", "enabled": true, "mdkg_dir": ".mdkg" } });
    }
    config["work"]["committed"] = "none".into();
    fs::write(&path, config.to_string()).expect("write config");
    fs::write(repo.join("lib.txt"), "lib\n").expect("file");
    sh(&repo, &["init", "-q", "-b", "main"]);
    sh(&repo, &["add", "."]);
    sh(&repo, &["commit", "-q", "-m", "init"]);
    let task = ok(&repo, "a", &["new", "task", "Local"]);
    let id = task["data"]["node"]["id"].as_str().expect("id").to_string();
    let claimed = ok(&repo, "a", &["claim", &id]);
    let node_scope = claimed["data"]["lease"]["scope"]
        .as_str()
        .expect("scope")
        .to_string();
    assert!(node_scope.starts_with("node:"), "{claimed}");
    ok(&repo, "a", &["claim", "lib.txt"]);
    let out = ok(&repo, "a", &["release", "--all"]);
    let mut want = vec![node_scope, "lib.txt".to_string()];
    want.sort();
    assert_eq!(scopes(&out["data"]["released"]), want, "{out}");
    assert!(held(&repo, "a").is_empty());
}

/// `release --clean` at `minutes` past now (the store's clock).
fn clean_at(worktree: &Path, actor: &str, minutes: u64) -> Value {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_millis() as u64
        + minutes * 60_000;
    let out = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
        .arg("--root")
        .arg(worktree)
        .args(["--as", actor, "release", "--clean", "--json"])
        .env("MDKG_NOW_MS", now.to_string())
        .output()
        .expect("run mdkg");
    let envelope: Value = serde_json::from_slice(&out.stdout).expect("JSON");
    common::conforms_envelope(&envelope);
    assert!(out.status.success(), "{envelope}");
    envelope
}

/// f-wake step 5 (orchestrator, #12639): `--clean` frees the path leases
/// `renew` lets lapse (a file lease protects uncommitted edits only):
/// a clean path claimed over 30 minutes ago goes; a path with changes, a
/// fresh claim, a node, a role and anyone else's stay.
#[test]
fn release_clean_frees_only_clean_path_leases_past_their_first_30_minutes() {
    let dir = scratch();
    let (repo, wt) = setup(&dir);
    ok(&wt, "a", &["claim", "lib.txt"]);
    ok(&wt, "a", &["claim", "notes/a.txt"]);
    ok(&wt, "a", &["claim", "task-1"]);
    ok(&wt, "a", &["claim", "role:reviewer"]);
    ok(&repo, "b", &["claim", "notes/b.txt"]);
    fs::write(wt.join("notes/a.txt"), "edited\n").expect("edit");

    // Just claimed: nothing goes yet, dirty or not.
    let out = clean_at(&wt, "a", 0);
    assert_eq!(out["data"]["released"], serde_json::json!([]), "{out}");
    let text = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
        .arg("--root")
        .arg(&wt)
        .args(["--as", "a", "release", "--clean"])
        .output()
        .expect("run mdkg");
    assert_eq!(
        String::from_utf8_lossy(&text.stdout).trim(),
        "no clean leases to release"
    );

    // Past the first 30 minutes: the clean path goes, the edited one stays.
    let out = clean_at(&wt, "a", 31);
    assert_eq!(scopes(&out["data"]["released"]), vec!["lib.txt"], "{out}");
    assert_eq!(out["data"]["failed"], serde_json::json!([]));
    assert_eq!(
        held(&wt, "a"),
        vec![T1.to_string(), "notes/a.txt".into(), "role:reviewer".into()]
    );
    assert_eq!(
        held(&repo, "b"),
        vec!["notes/b.txt".to_string()],
        "b's stays"
    );

    // Committed, the edited path is clean too, and goes.
    sh(&wt, &["commit", "-q", "-am", "edit"]);
    let out = clean_at(&wt, "a", 31);
    assert_eq!(
        scopes(&out["data"]["released"]),
        vec!["notes/a.txt"],
        "{out}"
    );
    assert_eq!(held(&wt, "a"), vec![T1.to_string(), "role:reviewer".into()]);
}

/// A target that is both a lease id and a file here refuses the whole
/// batch, as it refuses `release <target>` (#18223): never the lease
/// that id names elsewhere.
#[test]
fn an_ambiguous_lease_id_refuses_the_whole_batch() {
    let dir = scratch();
    let (_repo, wt) = setup(&dir);
    let first = ok(&wt, "a", &["claim", "task-1"]);
    let id = first["data"]["lease"]["id"]
        .as_str()
        .expect("id")
        .to_string();
    fs::write(wt.join(&id), "a file named like a lease id\n").expect("file");
    ok(&wt, "a", &["claim", &format!("./{id}")]);
    ok(&wt, "a", &["claim", "notes"]);
    let before = held(&wt, "a");
    assert_eq!(before, vec![T1.to_string(), id.clone(), "notes".into()]);
    for args in [&["release", &id][..], &["release", &id, "notes"]] {
        let (code, out) = run(&wt, "a", args);
        assert_eq!(code, 1, "{args:?}: {out}");
        assert_eq!(
            out["diagnostics"][0]["code"], "usage.invalid_arguments",
            "{out}"
        );
        assert_eq!(held(&wt, "a"), before, "{args:?}: nothing released");
    }
    let (code, out) = run(&wt, "a", &["release", &id, "notes"]);
    assert_eq!(code, 1, "{out}");
    let hint = out["diagnostics"][0]["hint"].as_str().expect("hint");
    assert!(hint.contains("none of the 2 targets was released"), "{out}");
}

/// A failed journal write inside `release a b` rolls the whole batch
/// back (#18223): reopened, the store holds both leases, and the same
/// command then releases both.
#[test]
fn a_failed_write_in_a_batch_release_releases_none() {
    let dir = scratch();
    let (repo, wt) = setup(&dir);
    ok(&wt, "a", &["claim", "notes"]);
    let lib = ok(&wt, "a", &["claim", "lib.txt"]);
    let id = lib["data"]["lease"]["id"].as_str().expect("id").to_string();
    // The second release's journal write fails, as a full disk would.
    let store = repo.join(".git/mdkg/coord.sqlite");
    rusqlite::Connection::open(&store)
        .expect("store")
        .execute_batch(&format!(
            "CREATE TRIGGER fail_one BEFORE INSERT ON journal
             WHEN NEW.command LIKE '%elease%' AND NEW.command LIKE '%\"{id}\"%'
             BEGIN SELECT RAISE(ABORT, 'injected write failure'); END;"
        ))
        .expect("trigger");
    let (code, out) = run(&wt, "a", &["release", "notes", "lib.txt"]);
    assert_ne!(code, 0, "{out}");
    assert_eq!(out["ok"], false, "{out}");
    assert_eq!(
        held(&wt, "a"),
        vec!["lib.txt".to_string(), "notes".into()],
        "neither is released: {out}"
    );
    rusqlite::Connection::open(&store)
        .expect("store")
        .execute_batch("DROP TRIGGER fail_one;")
        .expect("drop trigger");
    let out = ok(&wt, "a", &["release", "notes", "lib.txt"]);
    let released: Vec<&str> = out["data"]["releases"]
        .as_array()
        .expect("releases")
        .iter()
        .map(|r| r["lease"]["status"].as_str().expect("status"))
        .collect();
    assert_eq!(released, ["released", "released"], "{out}");
    assert!(held(&wt, "a").is_empty());
}

/// A handle naming several nodes refuses the whole batch (#18292), even
/// when a lease of yours sits on a path spelled like it: never that path.
#[test]
fn an_ambiguous_handle_refuses_the_whole_batch() {
    let dir = scratch();
    let (_repo, wt) = setup(&dir);
    // "task" is in both nodes' titles; `./task` is a path not yet written.
    ok(&wt, "a", &["claim", "--new", "./task"]);
    ok(&wt, "a", &["claim", "notes"]);
    let before = held(&wt, "a");
    assert_eq!(before, vec!["notes".to_string(), "task".into()]);
    let (code, out) = run(&wt, "a", &["release", "task", "notes"]);
    assert_eq!(code, 7, "{out}");
    assert_eq!(out["diagnostics"][0]["code"], "node.ambiguous", "{out}");
    let hint = out["diagnostics"][0]["hint"].as_str().unwrap_or_default();
    assert!(hint.contains("none of the 2 targets was released"), "{out}");
    assert_eq!(held(&wt, "a"), before, "nothing released");
}
