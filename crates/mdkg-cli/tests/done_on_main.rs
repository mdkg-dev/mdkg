//! `done` in the fresh-agent happy path (claim, notes in the task's body,
//! commit, done), RC1: the holder's own commits under its lease, on main
//! itself or through its merged branch, aren't a change under the lease
//! when no outside work is lost; a real outside edit still is. A
//! second `done` is a no-op that says so, and a commit that landed but
//! couldn't be recorded says exactly that.

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
    let dir = common::temp::dir("mdkg-cli-done-main");
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

fn sh(dir: &Path, args: &[&str]) -> String {
    let out = isolated(Command::new("git").arg("-C").arg(dir).args(args))
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
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

/// Runs mdkg as `actor` and returns (exit code, envelope).
fn run(root: &Path, actor: &str, args: &[&str]) -> (i32, Value) {
    let out = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
        .arg("--root")
        .arg(root)
        .args(["--as", actor])
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

fn ok(root: &Path, actor: &str, args: &[&str]) -> Value {
    let (code, out) = run(root, actor, args);
    assert_eq!(code, 0, "{args:?}: {out}");
    out
}

/// A graph in a single checkout on `main`.
fn single(dir: &Scratch) -> PathBuf {
    let repo = dir.0.join("repo");
    let conformance = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance");
    fs::create_dir_all(&repo).expect("repo");
    copy_tree(&conformance.join("base"), &repo);
    copy_tree(&conformance.join("cases/g01-valid-basic"), &repo);
    fs::remove_file(repo.join("commands")).ok();
    fs::remove_file(repo.join("README.md")).ok();
    fs::write(repo.join("a.txt"), "a\n").expect("file");
    sh(&repo, &["init", "-q", "-b", "main"]);
    sh(&repo, &["add", "."]);
    sh(&repo, &["commit", "-q", "-m", "init"]);
    repo
}

fn commits(repo: &Path) -> usize {
    sh(repo, &["rev-list", "--count", "HEAD"])
        .parse()
        .expect("count")
}

/// The single checkout plus a worktree `wt` on branch `b`.
fn with_branch(dir: &Scratch) -> (PathBuf, PathBuf) {
    let repo = single(dir);
    let wt = dir.0.join("wt");
    sh(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "b",
            wt.to_str().expect("utf-8"),
            "main",
        ],
    );
    (repo, wt)
}

#[test]
fn done_twice_on_a_branch_commits_and_records_once() {
    let dir = scratch();
    let (_repo, wt) = with_branch(&dir);
    ok(&wt, "agent1", &["claim", "task-1"]);
    let first = ok(&wt, "agent1", &["done", "task-1"]);
    assert_eq!(
        first["data"]["changesets"].as_array().map(Vec::len),
        Some(1)
    );
    let before = commits(&wt);
    let (code, again) = run(&wt, "agent1", &["done", "task-1", "--checkpoint"]);
    assert_eq!(code, 0, "{again}");
    assert_eq!(commits(&wt), before, "no second commit");
    assert_eq!(again["data"]["changesets"], serde_json::json!([]));
    assert_eq!(again["data"]["checkpoint"], Value::Null);
    let note = &again["diagnostics"][0];
    assert_eq!(note["code"], "coord.node_done", "{again}");
    assert!(
        note["message"].as_str().is_some_and(
            |m| m.ends_with("already done: nothing to commit or record; no checkpoint written")
        ),
        "{again}"
    );
}

#[test]
fn a_real_change_under_the_lease_says_the_commit_landed_unrecorded() {
    let dir = scratch();
    let (repo, wt) = with_branch(&dir);
    ok(&wt, "agent1", &["claim", "task-1"]);
    // Someone edits the node on main directly, under agent1's lease.
    let theirs = fs::read_to_string(repo.join(T1)).expect("node") + "\nTheirs.\n";
    fs::write(repo.join(T1), theirs).expect("theirs");
    sh(&repo, &["commit", "-q", "-am", "a direct edit on main"]);
    let (code, out) = run(&wt, "agent1", &["done", "task-1"]);
    assert_eq!(code, 5, "a conflict: {out}");
    let d = &out["diagnostics"][0];
    assert_eq!(d["code"], "coord.cas_failed", "{out}");
    let head = sh(&wt, &["rev-parse", "--short=12", "HEAD"]);
    let message = d["message"].as_str().unwrap_or_default();
    assert!(
        message.starts_with(&format!(
            "the work is committed in {head}, but not recorded: {T1}"
        )),
        "{out}"
    );
    let hint = d["hint"].as_str().unwrap_or_default();
    assert!(hint.contains("merge main"), "{out}");
    assert!(!hint.contains("same command again"), "{out}");
    // Run again anyway: nothing is committed or recorded over main's
    // change, and the conflict is said again.
    let before = commits(&wt);
    let (code, again) = run(&wt, "agent1", &["done", "task-1"]);
    assert_eq!(code, 5, "{again}");
    assert_eq!(commits(&wt), before, "{again}");
    assert_eq!(
        again["diagnostics"][0]["code"], "coord.cas_failed",
        "{again}"
    );
    assert!(
        again["diagnostics"][0]["hint"]
            .as_str()
            .is_some_and(|h| h.contains("git merge main")),
        "{again}"
    );
}

#[test]
fn done_after_committing_your_own_notes_on_main_succeeds_once() {
    let dir = scratch();
    let repo = single(&dir);
    ok(&repo, "agent1", &["claim", "task-1"]);
    // Notes in the task's body and the work, committed on main.
    let notes = fs::read_to_string(repo.join(T1)).expect("node") + "\n## Progress\n\nDid it.\n";
    fs::write(repo.join(T1), notes).expect("notes");
    fs::write(repo.join("a.txt"), "a, done\n").expect("work");
    sh(&repo, &["commit", "-q", "-am", "the work and its notes"]);
    let before = commits(&repo);

    let out = ok(&repo, "agent1", &["done", "task-1"]);
    assert_eq!(out["data"]["node"]["status"], "done", "{out}");
    assert_eq!(commits(&repo), before + 1, "one done commit");
    let changesets = out["data"]["changesets"].clone();
    assert_eq!(changesets.as_array().map(Vec::len), Some(1), "{out}");

    // Done again: nothing to do, and it says so.
    let again = ok(&repo, "agent1", &["done", "task-1"]);
    assert_eq!(commits(&repo), before + 1, "no second commit: {again}");
    assert_eq!(
        again["data"]["changesets"],
        serde_json::json!([]),
        "{again}"
    );
}

#[test]
fn done_after_your_branch_merged_succeeds() {
    let dir = scratch();
    let (repo, wt) = with_branch(&dir);
    ok(&wt, "agent1", &["claim", "task-1"]);
    let notes = fs::read_to_string(wt.join(T1)).expect("node") + "\n## Progress\n\nDid it.\n";
    fs::write(wt.join(T1), notes).expect("notes");
    sh(&wt, &["commit", "-q", "-am", "notes on the branch"]);
    // Merged before done, as a reviewer merging a lane would.
    sh(&repo, &["merge", "-q", "--no-ff", "--no-edit", "b"]);
    let out = ok(&wt, "agent1", &["done", "task-1"]);
    assert_eq!(
        out["data"]["changesets"].as_array().map(Vec::len),
        Some(1),
        "{out}"
    );
    assert_eq!(out["data"]["node"]["status"], "done", "{out}");
}

#[test]
fn rewriting_after_the_merge_is_still_a_conflict() {
    let dir = scratch();
    let (repo, wt) = with_branch(&dir);
    ok(&wt, "agent1", &["claim", "task-1"]);
    let node = fs::read_to_string(wt.join(T1)).expect("node");
    fs::write(wt.join(T1), node.clone() + "\nFirst.\n").expect("first");
    sh(&wt, &["commit", "-q", "-am", "first"]);
    sh(&repo, &["merge", "-q", "--no-ff", "--no-edit", "b"]);
    // The branch moves on from what main has: main's copy isn't HEAD's.
    fs::write(wt.join(T1), node + "\nSecond.\n").expect("second");
    sh(&wt, &["commit", "-q", "-am", "second"]);
    let (code, out) = run(&wt, "agent1", &["done", "task-1"]);
    assert_eq!(code, 5, "{out}");
    assert_eq!(out["diagnostics"][0]["code"], "coord.cas_failed", "{out}");
}

#[test]
fn a_refused_done_mark_records_nothing_and_a_rerun_finishes_once() {
    let dir = scratch();
    let (repo, wt) = with_branch(&dir);
    ok(&wt, "agent1", &["claim", "task-1"]);
    ok(&wt, "agent1", &["update", "task-1", "--status", "review"]);
    // One failed write: the done mark can't be journaled. It shares the
    // changeset's transaction (RC1, O4), so neither is recorded and the
    // lease stays: no claim could land between them.
    let store = repo.join(".git/mdkg/coord.sqlite");
    rusqlite::Connection::open(&store)
        .expect("store")
        .execute_batch(
            r#"CREATE TRIGGER fail_mark BEFORE INSERT ON journal
             WHEN NEW.command LIKE '%"op":"set_work"%' AND NEW.command LIKE '%"status":"done"%'
             BEGIN SELECT RAISE(ABORT, 'injected write failure'); END;"#,
        )
        .expect("trigger");
    let (code, out) = run(&wt, "agent1", &["done", "task-1"]);
    assert_ne!(code, 0, "never ok when the record failed: {out}");
    assert_eq!(out["ok"], false, "{out}");
    let st = ok(&wt, "agent1", &["status"]);
    let held = st["data"]["leases"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|l| l["owner"] == "agent1" && l["scope"] == T1);
    assert!(held, "the lease is as it was: {st}");
    assert_eq!(
        st["data"]["queue"],
        serde_json::json!([]),
        "no changeset: {st}"
    );
    rusqlite::Connection::open(&store)
        .expect("store")
        .execute_batch("DROP TRIGGER fail_mark;")
        .expect("drop");
    // The rerun records the one changeset and its mark, with no new commit.
    let before = commits(&wt);
    let again = ok(&wt, "agent1", &["done", "task-1"]);
    assert_eq!(commits(&wt), before, "{again}");
    assert_eq!(
        again["data"]["changesets"].as_array().map(Vec::len),
        Some(1),
        "{again}"
    );
    let st = ok(&wt, "agent1", &["status"]);
    let review_left = st["data"]["work"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|w| w["status"] == "review");
    assert!(!review_left, "{st}");
    // And once more: already done.
    let third = ok(&wt, "agent1", &["done", "task-1"]);
    assert_eq!(third["data"]["already_done"], true, "{third}");
}

#[test]
fn changes_on_main_you_never_saw_keep_the_conflict_even_when_main_ends_where_you_are() {
    let dir = scratch();
    let (repo, wt) = with_branch(&dir);
    ok(&wt, "agent1", &["claim", "task-1"]);
    let mine = fs::read_to_string(wt.join(T1)).expect("node") + "\nMine.\n";
    fs::write(wt.join(T1), &mine).expect("mine");
    sh(&wt, &["commit", "-q", "-am", "mine"]);
    sh(&repo, &["merge", "-q", "--no-ff", "--no-edit", "b"]);
    // Someone edits the node on main and puts it back: main's copy is
    // the holder's again, but through commits the holder never saw.
    fs::write(repo.join(T1), mine.clone() + "\nTheirs.\n").expect("theirs");
    sh(&repo, &["commit", "-q", "-am", "theirs"]);
    fs::write(repo.join(T1), &mine).expect("back");
    sh(&repo, &["commit", "-q", "-am", "back"]);
    let (code, out) = run(&wt, "agent1", &["done", "task-1"]);
    assert_eq!(code, 5, "{out}");
    assert_eq!(out["diagnostics"][0]["code"], "coord.cas_failed", "{out}");
}

/// [`run`] with extra environment.
fn run_env(root: &Path, actor: &str, args: &[&str], env: &[(&str, String)]) -> (i32, Value) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mdkg"));
    isolated(&mut cmd)
        .arg("--root")
        .arg(root)
        .args(["--as", actor])
        .args(args)
        .arg("--json");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("run mdkg");
    let envelope: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|_| {
        panic!(
            "not JSON: {}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    });
    (out.status.code().unwrap_or(-1), envelope)
}

#[test]
fn main_moving_while_landed_is_checked_still_conflicts() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch();
    let (repo, wt) = with_branch(&dir);
    ok(&wt, "agent1", &["claim", "task-1"]);
    let mine = fs::read_to_string(wt.join(T1)).expect("node") + "\nMine.\n";
    fs::write(wt.join(T1), &mine).expect("mine");
    sh(&wt, &["commit", "-q", "-am", "mine"]);
    sh(&repo, &["merge", "-q", "--no-ff", "--no-edit", "b"]);
    // A git on PATH that, when done checks main's history for the node,
    // first lands someone's unseen edit on main and has it observed.
    let real = String::from_utf8(
        Command::new("sh")
            .args(["-c", "command -v git"])
            .output()
            .expect("which git")
            .stdout,
    )
    .expect("utf-8");
    let real = real.trim().to_string();
    let real_dir = Path::new(&real)
        .parent()
        .expect("git's folder")
        .display()
        .to_string();
    let hook = dir.0.join("hook");
    fs::create_dir_all(&hook).expect("hook dir");
    let marker = dir.0.join("moved");
    let theirs = repo.join(T1);
    let script = format!(
        "#!/bin/sh\ncase \"$*\" in\n  *rev-list*)\n    if [ ! -e '{marker}' ]; then\n      touch '{marker}'\n      \
         printf '\\nTheirs.\\n' >> '{theirs}' || exit 97\n      \
         PATH='{real_dir}:/usr/bin:/bin' GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 \
         GIT_AUTHOR_NAME=T GIT_AUTHOR_EMAIL=t@e.invalid GIT_COMMITTER_NAME=T GIT_COMMITTER_EMAIL=t@e.invalid \
         '{real}' -C '{repo}' commit -q -am theirs >/dev/null 2>&1 || exit 97\n      \
         PATH='{real_dir}:/usr/bin:/bin' '{mdkg}' --root '{repo}' --as other status --json >/dev/null 2>&1 || exit 97\n    fi ;;\nesac\nexec '{real}' \"$@\"\n",
        marker = marker.display(),
        theirs = theirs.display(),
        repo = repo.display(),
        mdkg = env!("CARGO_BIN_EXE_mdkg"),
    );
    let fake = hook.join("git");
    fs::write(&fake, script).expect("fake git");
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).expect("chmod");
    let path = format!(
        "{}:{}",
        hook.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let (code, out) = run_env(&wt, "agent1", &["done", "task-1"], &[("PATH", path)]);
    assert!(marker.exists(), "the hook ran");
    assert_eq!(code, 5, "{out}");
    assert_eq!(out["diagnostics"][0]["code"], "coord.cas_failed", "{out}");
}

#[test]
fn a_record_lost_past_the_lease_says_how_to_record_it() {
    let dir = scratch();
    let (repo, wt) = with_branch(&dir);
    ok(&wt, "agent1", &["claim", "task-1"]);
    // One failed write: the changeset can't be journaled after done's
    // commit, as a crash at that point would leave it.
    let store = repo.join(".git/mdkg/coord.sqlite");
    rusqlite::Connection::open(&store)
        .expect("store")
        .execute_batch(
            r#"CREATE TRIGGER fail_commit BEFORE INSERT ON journal
             WHEN NEW.command LIKE '%"op":"commit"%'
             BEGIN SELECT RAISE(ABORT, 'injected write failure'); END;"#,
        )
        .expect("trigger");
    let (code, out) = run(&wt, "agent1", &["done", "task-1"]);
    assert_ne!(code, 0, "{out}");
    rusqlite::Connection::open(&store)
        .expect("store")
        .execute_batch("DROP TRIGGER fail_commit;")
        .expect("drop");
    // Long after the lease ran out: done says it isn't recorded and how
    // to record it, and doing so records it with at most one more commit.
    let later = hours_later(6);
    let (code, again) = run_env(
        &wt,
        "agent1",
        &["done", "task-1"],
        &[("MDKG_NOW_MS", later.clone())],
    );
    assert_ne!(code, 0, "{again}");
    let d = &again["diagnostics"][0];
    assert_eq!(d["code"], "coord.no_lease", "{again}");
    let hint = d["hint"].as_str().unwrap_or_default();
    assert!(
        hint.contains("mdkg claim task-1") && hint.contains("mdkg done task-1"),
        "{again}"
    );
    let before = commits(&wt);
    let (code, claimed) = run_env(
        &wt,
        "agent1",
        &["claim", "task-1"],
        &[("MDKG_NOW_MS", later.clone())],
    );
    assert_eq!(code, 0, "{claimed}");
    let (code, recorded) = run_env(
        &wt,
        "agent1",
        &["done", "task-1"],
        &[("MDKG_NOW_MS", later)],
    );
    assert_eq!(code, 0, "{recorded}");
    assert!(commits(&wt) <= before + 1, "{recorded}");
    assert_eq!(
        recorded["data"]["changesets"].as_array().map(Vec::len),
        Some(1),
        "{recorded}"
    );
}

#[test]
fn finishing_a_done_under_someone_elses_lease_is_refused_not_ok() {
    let dir = scratch();
    let (repo, wt) = with_branch(&dir);
    ok(&wt, "agent1", &["claim", "task-1"]);
    ok(&wt, "agent1", &["done", "task-1"]);
    // Someone else takes the node and puts it in review.
    run(&repo, "agent2", &["claim", "task-1"]);
    ok(&repo, "agent2", &["update", "task-1", "--status", "review"]);
    let (code, out) = run(&wt, "agent1", &["done", "task-1"]);
    assert_ne!(code, 0, "{out}");
    assert_eq!(out["ok"], false, "{out}");
}

/// A trigger that refuses to journal one kind of command, as a failed
/// write would, and drops it again.
fn refuse_writes(repo: &Path, op: &str) -> impl FnOnce() {
    let store = repo.join(".git/mdkg/coord.sqlite");
    rusqlite::Connection::open(&store)
        .expect("store")
        .execute_batch(&format!(
            r#"CREATE TRIGGER refuse_one BEFORE INSERT ON journal
             WHEN NEW.command LIKE '%"op":"{op}"%'
             BEGIN SELECT RAISE(ABORT, 'injected write failure'); END;"#
        ))
        .expect("trigger");
    move || {
        rusqlite::Connection::open(&store)
            .expect("store")
            .execute_batch("DROP TRIGGER refuse_one;")
            .expect("drop");
    }
}

fn hours_later(h: u128) -> String {
    (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_millis()
        + h * 3_600_000)
        .to_string()
}

#[test]
fn a_lost_record_is_never_recorded_over_a_change_main_made_since() {
    let dir = scratch();
    let (repo, wt) = with_branch(&dir);
    ok(&wt, "agent1", &["claim", "task-1"]);
    let restore = refuse_writes(&repo, "commit");
    let (code, out) = run(&wt, "agent1", &["done", "task-1"]);
    assert_ne!(code, 0, "{out}");
    restore();
    // The lease runs out; someone edits the node on main meanwhile.
    let theirs = fs::read_to_string(repo.join(T1)).expect("node") + "\nTheirs.\n";
    fs::write(repo.join(T1), theirs).expect("theirs");
    sh(&repo, &["commit", "-q", "-am", "theirs"]);
    let before = commits(&wt);
    let (code, again) = run_env(
        &wt,
        "agent1",
        &["done", "task-1"],
        &[("MDKG_NOW_MS", hours_later(6))],
    );
    assert_eq!(code, 5, "{again}");
    assert_eq!(
        again["diagnostics"][0]["code"], "coord.cas_failed",
        "{again}"
    );
    assert_eq!(commits(&wt), before, "{again}");
}

#[test]
fn an_older_changeset_of_yours_is_no_proof_this_version_is_recorded() {
    if common::rerun_without_launcher_env(
        "an_older_changeset_of_yours_is_no_proof_this_version_is_recorded",
    ) {
        return;
    }
    let dir = scratch();
    let (repo, wt) = with_branch(&dir);
    // An earlier changeset of agent1's on the node, of another version
    // (as a retype or a format would leave), recorded through the store.
    {
        use mdkg_coord::{Command as C, LeaseId, Outcome, Scope};
        let mut store = mdkg_coord_sqlite::Store::open_for_repo(&wt, 7_200_000).expect("store");
        let lease = match store
            .execute(&C::Claim {
                owner: "agent1".into(),
                scope: Scope::new(T1),
                mode: mdkg_coord::Mode::Exclusive,
                now: 1,
                request: None,
                node: None,
            })
            .expect("claim")
        {
            Outcome::Lease { lease } => lease,
            other => panic!("{other:?}"),
        };
        let older = format!("sha256:{}", "1".repeat(64));
        match store
            .execute(&C::Commit {
                owner: "agent1".into(),
                lease: LeaseId(lease.to_string()),
                changes: [(Scope::new(T1), Some(older))].into(),
                source: None,
                now: 2,
                landed: false,
            })
            .expect("commit")
        {
            Outcome::Committed { .. } => {}
            other => panic!("{other:?}"),
        }
    }
    // Then the real work, and a change on main under it.
    ok(&wt, "agent1", &["claim", "task-1"]);
    let theirs = fs::read_to_string(repo.join(T1)).expect("node") + "\nTheirs.\n";
    fs::write(repo.join(T1), theirs).expect("theirs");
    sh(&repo, &["commit", "-q", "-am", "theirs"]);
    let (code, out) = run(&wt, "agent1", &["done", "task-1"]);
    assert_eq!(code, 5, "{out}");
    let (code, again) = run(&wt, "agent1", &["done", "task-1"]);
    assert_eq!(code, 5, "never done on the older changeset's word: {again}");
    assert_eq!(
        again["diagnostics"][0]["code"], "coord.cas_failed",
        "{again}"
    );
}

#[test]
fn a_format_07_retry_under_the_lease_commits_at_most_once_more() {
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
    fs::write(&path, config.to_string()).expect("write config");
    sh(&repo, &["init", "-q", "-b", "main"]);
    sh(&repo, &["add", "."]);
    sh(&repo, &["commit", "-q", "-m", "init"]);
    let wt = dir.0.join("wt");
    sh(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "b",
            wt.to_str().expect("utf-8"),
            "main",
        ],
    );
    let task = ok(&wt, "agent1", &["new", "task", "Timed"]);
    let id = task["data"]["node"]["id"].as_str().expect("id").to_string();
    let file = task["data"]["node"]["path"]
        .as_str()
        .expect("path")
        .to_string();
    sh(&wt, &["add", &file]);
    sh(&wt, &["commit", "-q", "-m", "the task"]);
    ok(&wt, "agent1", &["claim", &id]);
    let restore = refuse_writes(&repo, "commit");
    let (code, out) = run(&wt, "agent1", &["done", &id, "--checkpoint"]);
    assert_ne!(code, 0, "{out}");
    restore();
    let before = commits(&wt);
    let checkpoints = fs::read_dir(wt.join(".mdkg/work")).expect("work").count();
    let (code, again) = run_env(
        &wt,
        "agent1",
        &["done", &id, "--checkpoint"],
        &[("MDKG_NOW_MS", hours_later(1))],
    );
    // The lease is still held: the full command reruns, which may make
    // one more commit (and checkpoint), never more, and records it.
    assert_eq!(code, 0, "{again}");
    assert!(commits(&wt) <= before + 1, "{again}");
    assert!(
        fs::read_dir(wt.join(".mdkg/work")).expect("work").count() <= checkpoints + 1,
        "{again}"
    );
    assert_eq!(
        again["data"]["changesets"].as_array().map(Vec::len),
        Some(1),
        "{again}"
    );
}

#[test]
fn done_clears_a_review_set_after_its_changeset_settled() {
    let dir = scratch();
    let (repo, wt) = with_branch(&dir);
    ok(&wt, "agent1", &["claim", "task-1"]);
    ok(&wt, "agent1", &["done", "task-1"]);
    sh(&repo, &["merge", "-q", "--no-ff", "--no-edit", "b"]);
    ok(&repo, "agent1", &["status"]);
    ok(&wt, "agent1", &["update", "task-1", "--status", "review"]);
    let again = ok(&wt, "agent1", &["done", "task-1"]);
    assert_eq!(again["data"]["already_done"], true, "{again}");
    let shown = ok(&wt, "agent1", &["show", "task-1"]);
    assert_eq!(shown["data"]["node"]["status"], "done", "{shown}");
}

#[test]
fn reclaiming_after_the_hint_never_records_over_a_change_main_made_since() {
    let dir = scratch();
    let (repo, wt) = with_branch(&dir);
    ok(&wt, "agent1", &["claim", "task-1"]);
    let restore = refuse_writes(&repo, "commit");
    let (code, out) = run(&wt, "agent1", &["done", "task-1"]);
    assert_ne!(code, 0, "{out}");
    restore();
    let later = hours_later(6);
    // An outside edit reaches main, then agent1 follows the hint.
    let theirs = fs::read_to_string(repo.join(T1)).expect("node") + "\nTheirs.\n";
    fs::write(repo.join(T1), theirs).expect("theirs");
    sh(&repo, &["commit", "-q", "-am", "theirs"]);
    let (code, claimed) = run_env(
        &wt,
        "agent1",
        &["claim", "task-1"],
        &[("MDKG_NOW_MS", later.clone())],
    );
    assert_eq!(code, 0, "{claimed}");
    let (code, again) = run_env(
        &wt,
        "agent1",
        &["done", "task-1"],
        &[("MDKG_NOW_MS", later)],
    );
    assert_eq!(code, 5, "{again}");
    assert_eq!(
        again["diagnostics"][0]["code"], "coord.cas_failed",
        "{again}"
    );
}

#[test]
fn an_outside_edit_behind_a_merge_that_took_your_version_still_conflicts() {
    let dir = scratch();
    let (repo, wt) = with_branch(&dir);
    ok(&wt, "agent1", &["claim", "task-1"]);
    let node = fs::read_to_string(wt.join(T1)).expect("node");
    fs::write(wt.join(T1), node.clone() + "\nMine.\n").expect("mine");
    sh(&wt, &["commit", "-q", "-am", "mine"]);
    // Someone edits the node on main; then main merges b, keeping b's
    // node whole, so main's copy is HEAD's but the edit is in main's
    // history, not HEAD's.
    fs::write(repo.join(T1), node + "\nTheirs.\n").expect("theirs");
    sh(&repo, &["commit", "-q", "-am", "theirs"]);
    let merge = isolated(Command::new("git").arg("-C").arg(&repo).args([
        "merge",
        "--no-commit",
        "--no-ff",
        "b",
    ]))
    .output()
    .expect("merge");
    let _ = merge;
    sh(&repo, &["checkout", "b", "--", T1]);
    sh(&repo, &["add", T1]);
    sh(
        &repo,
        &[
            "commit",
            "-q",
            "--no-edit",
            "-m",
            "merge b, taking its node",
        ],
    );
    let (code, out) = run(&wt, "agent1", &["done", "task-1"]);
    assert_eq!(code, 5, "{out}");
    assert_eq!(out["diagnostics"][0]["code"], "coord.cas_failed", "{out}");
}

#[test]
fn a_stale_done_never_clears_the_review_of_a_newer_version() {
    let dir = scratch();
    let (repo, wt) = with_branch(&dir);
    ok(&wt, "agent1", &["claim", "task-1"]);
    ok(&wt, "agent1", &["done", "task-1"]);
    sh(&repo, &["merge", "-q", "--no-ff", "--no-edit", "b"]);
    ok(&repo, "agent2", &["status"]);
    // Main moves on: the node is reopened there and goes into review.
    let reopened = fs::read_to_string(repo.join(T1)).expect("node").replacen(
        "status: done",
        "status: todo",
        1,
    );
    fs::write(repo.join(T1), reopened).expect("reopen");
    sh(&repo, &["commit", "-q", "-am", "reopen"]);
    ok(&repo, "agent2", &["update", "task-1", "--status", "review"]);
    // agent1's branch still has the old done bytes.
    let (code, out) = run(&wt, "agent1", &["done", "task-1"]);
    assert_eq!(code, 0, "{out}");
    let st = ok(&repo, "agent2", &["status"]);
    let review = st["data"]["work"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|w| w["status"] == "review");
    assert!(review, "the newer version's review stays: {st}");
}

#[test]
fn done_after_a_merge_beside_other_work_on_main_succeeds() {
    let dir = scratch();
    let (repo, wt) = with_branch(&dir);
    ok(&wt, "agent1", &["claim", "task-1"]);
    let notes = fs::read_to_string(wt.join(T1)).expect("node") + "\n## Progress\n\nDid it.\n";
    fs::write(wt.join(T1), notes).expect("notes");
    sh(&wt, &["commit", "-q", "-am", "notes on the branch"]);
    // Main has other work too, then merges b: the merge only takes b's
    // node, which changes nothing about it main hadn't seen.
    fs::write(repo.join("a.txt"), "a, on main\n").expect("other");
    sh(&repo, &["commit", "-q", "-am", "other work on main"]);
    sh(&repo, &["merge", "-q", "--no-ff", "--no-edit", "b"]);
    let out = ok(&wt, "agent1", &["done", "task-1"]);
    assert_eq!(
        out["data"]["changesets"].as_array().map(Vec::len),
        Some(1),
        "{out}"
    );
}

#[test]
fn a_version_reopened_while_done_checks_keeps_its_review() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch();
    let repo = single(&dir);
    // Main holds the node done, committed by hand: no changeset of
    // agent1's records it. A worktree at main, and a review on the node.
    let done = fs::read_to_string(repo.join(T1)).expect("node").replacen(
        "status: todo",
        "status: done",
        1,
    );
    fs::write(repo.join(T1), done).expect("done");
    sh(&repo, &["commit", "-q", "-am", "done by hand"]);
    let wt = dir.0.join("wt");
    sh(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "b",
            wt.to_str().expect("utf-8"),
            "main",
        ],
    );
    ok(&wt, "agent1", &["update", "task-1", "--status", "review"]);
    // A git on PATH that, when done compares main with HEAD, first has
    // main reopen the node and agent2 put that version in review.
    let real = String::from_utf8(
        Command::new("sh")
            .args(["-c", "command -v git"])
            .output()
            .expect("which git")
            .stdout,
    )
    .expect("utf-8");
    let real = real.trim().to_string();
    let real_dir = Path::new(&real)
        .parent()
        .expect("git's folder")
        .display()
        .to_string();
    let hook = dir.0.join("hook");
    fs::create_dir_all(&hook).expect("hook dir");
    let marker = dir.0.join("reopened");
    let script = format!(
        "#!/bin/sh\ncase \"$*\" in\n  *diff*--quiet*--no-renames*)\n    if [ ! -e '{marker}' ]; then\n      touch '{marker}'\n      \
         sed -i.bak 's/status: done/status: todo/' '{node}' || exit 97\n      \
         rm -f '{node}.bak' || exit 97\n      \
         PATH='{real_dir}:/usr/bin:/bin' GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 \
         GIT_AUTHOR_NAME=T GIT_AUTHOR_EMAIL=t@e.invalid GIT_COMMITTER_NAME=T GIT_COMMITTER_EMAIL=t@e.invalid \
         '{real}' -C '{repo}' commit -q -am reopen >/dev/null 2>&1 || exit 97\n      \
         PATH='{real_dir}:/usr/bin:/bin' '{mdkg}' --root '{repo}' --as agent2 update task-1 --status blocked --json >/dev/null 2>&1 || exit 97\n    fi ;;\nesac\nexec '{real}' \"$@\"\n",
        marker = marker.display(),
        node = repo.join(T1).display(),
        repo = repo.display(),
        mdkg = env!("CARGO_BIN_EXE_mdkg"),
    );
    let fake = hook.join("git");
    fs::write(&fake, script).expect("fake git");
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).expect("chmod");
    let path = format!(
        "{}:{}",
        hook.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let (code, out) = run_env(&wt, "agent1", &["done", "task-1"], &[("PATH", path)]);
    assert!(marker.exists(), "the hook ran: {out}");
    assert_eq!(code, 0, "{out}");
    let st = ok(&repo, "agent2", &["status"]);
    let blocked = st["data"]["work"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|w| w["status"] == "blocked");
    assert!(blocked, "the reopened version's state stays: {st}");
}
