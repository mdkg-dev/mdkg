//! Profile C's write paths (task-47): a store-folder node is leased by its
//! id, so a rename keeps its lease and states; goals hand out and finish
//! such nodes; and done and update stay consistent through a crash at each
//! step or one failed write (Tier 1, edd-6's fault bar). Throwaway
//! repositories only.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

mod common;

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        if std::env::var_os("MDKG_KEEP_SCRATCH").is_none() {
            fs::remove_dir_all(&self.0).ok();
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
        .env_remove("MDKG_CRASH_AFTER")
        .env_remove("MDKG_FAIL_AFTER")
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

/// Runs the binary as `actor`, with extra environment.
fn run_env(root: &Path, actor: &str, args: &[&str], env: &[(&str, &str)]) -> (i32, Value) {
    let out = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
        .arg("--root")
        .arg(root)
        .args(["--as", actor])
        .args(args)
        .arg("--json")
        .envs(env.iter().copied())
        .output()
        .expect("run mdkg");
    let code = out.status.code().unwrap_or(-1);
    if code == 86 {
        // Stopped dead by the crash hook: no envelope.
        return (code, Value::Null);
    }
    let envelope: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|_| {
        panic!(
            "{args:?}: not JSON: {}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    });
    common::conforms_envelope(&envelope);
    (code, envelope)
}

fn run(root: &Path, args: &[&str]) -> (i32, Value) {
    run_env(root, "lane-a", args, &[])
}

fn ok(root: &Path, args: &[&str]) -> Value {
    let (code, out) = run(root, args);
    assert_eq!(code, 0, "{args:?}: {out}");
    out
}

fn codes(envelope: &Value) -> Vec<String> {
    envelope["diagnostics"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|d| d["code"].as_str().map(String::from))
        .collect()
}

/// A profile C repository with a committed design and a task in the store.
fn c_repo(name: &str) -> (Scratch, String) {
    let dir = std::env::temp_dir().join(format!(
        "mdkg-cli-c-writes-{name}-{}-{}",
        std::process::id(),
        mdkg_format::Uuid7::now()
    ));
    fs::create_dir_all(&dir).expect("scratch");
    let dir = fs::canonicalize(dir).expect("canonical");
    fs::write(dir.join("README.md"), "scratch\n").expect("readme");
    sh(&dir, &["init", "-q", "-b", "main"]);
    sh(&dir, &["add", "."]);
    sh(&dir, &["commit", "-q", "-m", "start"]);
    ok(&dir, &["init", "--work", "local"]);
    sh(&dir, &["add", "."]);
    sh(&dir, &["commit", "-q", "-m", "init"]);
    let task = ok(&dir, &["new", "task", "Ship", "the", "thing"]);
    let id = task["data"]["node"]["id"].as_str().expect("id").to_string();
    (Scratch(dir), id)
}

/// A store node's scope: `node:` and its id's UUID (format 9.1).
fn scope_of(id: &str) -> String {
    let uuid = id.split_once('-').map_or(id, |(_, u)| u);
    format!("node:{uuid}")
}

/// The node's file, wherever the store folder keeps it.
fn file_of(root: &Path, id: &str) -> PathBuf {
    let shown = ok(root, &["show", id, "--fields", "path"]);
    let path = shown["data"]["node"]["path"].as_str().expect("path");
    root.join(path)
}

fn status_of(root: &Path, id: &str) -> String {
    let shown = ok(root, &["show", id, "--fields", "status"]);
    shown["data"]["node"]["status"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// The status the node's file holds.
fn file_status(root: &Path, id: &str) -> String {
    let text = fs::read_to_string(file_of(root, id)).expect("node file");
    text.lines()
        .find_map(|l| l.strip_prefix("status: "))
        .unwrap_or_default()
        .to_string()
}

/// Every lease and temporary state the store holds for `scope`.
fn store_rows(root: &Path, scope: &str) -> (Vec<Value>, Vec<Value>) {
    let st = ok(root, &["status", "--all"]);
    let leases = st["data"]["leases"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|l| l["scope"] == scope && l["status"] == "active")
        .cloned()
        .collect();
    let states = st["data"]["work"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|s| s["scope"] == scope)
        .cloned()
        .collect();
    (leases, states)
}

#[test]
fn a_store_node_is_leased_by_its_id_and_keeps_it_through_a_rename() {
    let (repo, id) = c_repo("rename");
    let scope = scope_of(&id);
    let claimed = ok(&repo.0, &["claim", &id]);
    assert_eq!(
        claimed["data"]["lease"]["scope"],
        scope.as_str(),
        "{claimed}"
    );
    ok(&repo.0, &["update", &id, "--status", "review"]);
    // A reader sees the node beside its UUID scope, never the UUID alone.
    assert_eq!(
        claimed["data"]["lease"]["node"]["id"],
        id.as_str(),
        "{claimed}"
    );
    assert_eq!(claimed["data"]["lease"]["node"]["title"], "Ship the thing");
    let st = ok(&repo.0, &["status"]);
    let named = |list: &str| {
        st["data"][list]
            .as_array()
            .into_iter()
            .flatten()
            .any(|r| r["scope"] == scope.as_str() && r["node"]["id"] == id.as_str())
    };
    assert!(named("leases") && named("work"), "{st}");
    let held = ok(&repo.0, &["holds", &scope]);
    assert_eq!(held["data"]["lease"]["node"]["id"], id.as_str(), "{held}");
    let (leases, states) = store_rows(&repo.0, &scope);
    assert_eq!(
        (leases.len(), states.len()),
        (1, 1),
        "the rows these checks read"
    );
    // The file is renamed, as anyone may at any time.
    let file = file_of(&repo.0, &id);
    let renamed = file.with_file_name("task-renamed-for-tidiness.md");
    fs::rename(&file, &renamed).expect("rename");
    // The lease and the state followed it.
    let (code, out) = run_env(&repo.0, "lane-b", &["claim", &id], &[]);
    assert_eq!(
        (code, codes(&out)),
        (5, vec!["coord.lease_held".to_string()]),
        "{out}"
    );
    assert_eq!(status_of(&repo.0, &id), "review");
    ok(&repo.0, &["update", &id, "--clear"]);
    ok(&repo.0, &["done", &id]);
    assert_eq!(status_of(&repo.0, &id), "done");
    assert_eq!(file_status(&repo.0, &id), "done");
    let (leases, states) = store_rows(&repo.0, &scope);
    assert!(
        leases.is_empty() && states.is_empty(),
        "{leases:?} {states:?}"
    );
}

#[test]
fn a_claimed_store_node_is_in_progress_and_next_skips_it() {
    let (repo, id) = c_repo("next");
    ok(&repo.0, &["claim", &id]);
    assert_eq!(status_of(&repo.0, &id), "progress");
    let (_, out) = run_env(&repo.0, "lane-b", &["next"], &[]);
    assert!(
        out["data"]["node"].is_null(),
        "lane-b isn't offered lane-a's node: {out}"
    );
    let out = ok(&repo.0, &["next"]);
    assert_eq!(
        out["data"]["node"]["id"],
        id.as_str(),
        "its holder is: {out}"
    );
}

#[test]
fn goals_hand_out_and_finish_store_nodes() {
    let (repo, _) = c_repo("goals");
    // A committed goal names committed containers only (RC1 audit E1):
    // here an epic, which the store task names.
    let epic = ok(&repo.0, &["new", "epic", "Shipping"]);
    let epic_id = epic["data"]["node"]["id"].as_str().expect("id").to_string();
    let task = ok(&repo.0, &["new", "task", "Ship", "--epic", &epic_id]);
    let id = task["data"]["node"]["id"].as_str().expect("id").to_string();
    let goal = ok(&repo.0, &["new", "goal", "Ship", "it"]);
    let goal_id = goal["data"]["node"]["id"].as_str().expect("id").to_string();
    let goal_path = repo
        .0
        .join(goal["data"]["node"]["path"].as_str().expect("path"));
    let text = fs::read_to_string(&goal_path).expect("goal");
    fs::write(
        &goal_path,
        text.replace("scope_refs: []", &format!("scope_refs: [{epic_id}]")),
    )
    .expect("scope");
    sh(&repo.0, &["add", "."]);
    sh(&repo.0, &["commit", "-q", "-m", "goal"]);
    ok(&repo.0, &["goal", "activate", &goal_id]);
    let next = ok(&repo.0, &["goal", "next", &goal_id]);
    assert_eq!(next["data"]["node"]["id"], id.as_str(), "{next}");
    // goal claim (0.6.0's spelling) leases the store node by its id.
    let claimed = ok(&repo.0, &["goal", "claim", &goal_id]);
    let scopes: Vec<&str> = claimed["data"]["leases"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|l| l["scope"].as_str())
        .collect();
    assert_eq!(scopes, [scope_of(&id).as_str()], "{claimed}");
    ok(&repo.0, &["goal", "pause", &goal_id]);
    ok(&repo.0, &["goal", "activate", &goal_id]);
    ok(&repo.0, &["done", &id]);
    ok(&repo.0, &["goal", "done", &goal_id]);
    // The goal is knowledge, in git; the task never was.
    let seen = sh(&repo.0, &["log", "--all", "--name-only", "--format="]);
    assert!(seen.contains("goal-"), "{seen}");
    assert!(!seen.contains("task-"), "{seen}");
}

/// After whatever stopped a command, the node and the store agree: the
/// file is whole and says one status, `show` says the same, and running
/// the command again finishes it, leaving no lease or state behind.
fn converges(root: &Path, id: &str, again: &[&str], want: &str) {
    let scope = scope_of(id);
    let file = file_status(root, id);
    assert!(
        ["todo", "backlog", "done"].contains(&file.as_str()),
        "file: {file}"
    );
    let shown = status_of(root, id);
    if file == "done" {
        assert_eq!(shown, "done", "a done file shows done");
    }
    let (code, out) = run(root, again);
    assert_eq!(code, 0, "{again:?} again: {out}");
    assert_eq!(status_of(root, id), want);
    let (code, v) = run(root, &["validate"]);
    assert_eq!(code, 0, "{v}");
    if want == "done" {
        assert_eq!(file_status(root, id), "done");
        let (leases, states) = store_rows(root, &scope);
        assert!(
            leases.is_empty() && states.is_empty(),
            "{leases:?} {states:?}"
        );
    }
}

fn activity_store(root: &Path) -> mdkg_coord_sqlite::Store {
    // The fixture is an ordinary repository, not a linked worktree. Use its
    // explicit path, so inherited launcher Git pins cannot redirect a read.
    mdkg_coord_sqlite::Store::open_with(
        &root.join(".git/mdkg/coord.sqlite"),
        7_200_000,
        mdkg_coord_sqlite::Upgrade::Refuse,
    )
    .expect("fixture activity store")
}

#[test]
fn local_done_is_a_native_activity_fact_without_a_persistent_mark() {
    let (repo, id) = c_repo("done-activity");
    let scope = scope_of(&id);
    let store = activity_store(&repo.0);
    let before = store.activity_cursor().unwrap();
    ok(&repo.0, &["done", &id]);
    let facts = store.activity_since(&before, "lane-a", 0).unwrap();
    assert_eq!(facts.done_marks.as_slice(), std::slice::from_ref(&scope));
    assert!(facts.completions[0].node.ends_with(&format!("#{id}")));
    assert_eq!(
        facts.completions[0].revision,
        mdkg_format::Digest::of_bytes(&fs::read(file_of(&repo.0, &id)).unwrap()).to_string()
    );
    let state = store.coordinator().unwrap();
    let leaf = mdkg_coord::Scope::new(&scope);
    assert!(state.work(&leaf).is_none());
    assert!(state.done_mark(&leaf).is_none());
    assert!(store_rows(&repo.0, &scope).0.is_empty());
    let already = ok(&repo.0, &["done", &id]);
    assert_eq!(already["data"]["already_done"], true);
    let again = store.activity_since(&facts.through, "lane-a", 0).unwrap();
    assert!(
        again.done_marks.is_empty(),
        "already done is not another completion"
    );
}

#[test]
fn retry_after_a_recorded_completion_does_not_repeat_it_in_a_later_cursor() {
    for failure in ["MDKG_CRASH_AFTER", "MDKG_FAIL_AFTER"] {
        let (repo, id) = c_repo(failure);
        ok(&repo.0, &["claim", &id]);
        // No review mark: only the unfinished lease remains after marking.
        let store = activity_store(&repo.0);
        let before = store.activity_cursor().unwrap();
        let (code, out) = run_env(&repo.0, "lane-a", &["done", &id], &[(failure, "marked")]);
        assert_eq!(
            code,
            if failure == "MDKG_CRASH_AFTER" { 86 } else { 4 },
            "{out}"
        );
        assert_eq!(file_status(&repo.0, &id), "done");
        let first = store.activity_since(&before, "lane-a", 0).unwrap();
        assert_eq!(first.done_marks, [scope_of(&id)]);
        let after_first_fact = first.through;
        let done_bytes = fs::read(file_of(&repo.0, &id)).unwrap();
        ok(&repo.0, &["done", &id]);
        assert_eq!(
            fs::read(file_of(&repo.0, &id)).unwrap(),
            done_bytes,
            "cleanup preserves the done revision"
        );
        let retried = store
            .activity_since(&after_first_fact, "lane-a", 0)
            .unwrap();
        assert!(
            retried.done_marks.is_empty(),
            "same revision repeated after {failure}: {retried:?}"
        );
    }
}

#[test]
fn written_then_expired_without_a_review_mark_does_not_invent_a_completion() {
    let (repo, id) = c_repo("written-expired-no-mark");
    ok(&repo.0, &["claim", &id]);
    let mut store = activity_store(&repo.0);
    let before = store.activity_cursor().unwrap();
    let starting_head = sh(&repo.0, &["rev-parse", "HEAD"]);
    let (code, _) = run_env(
        &repo.0,
        "lane-a",
        &["done", &id],
        &[("MDKG_CRASH_AFTER", "written")],
    );
    assert_eq!(code, 86);
    assert_eq!(file_status(&repo.0, &id), "done");
    assert!(
        store
            .coordinator()
            .unwrap()
            .work(&mdkg_coord::Scope::new(scope_of(&id)))
            .is_none()
    );
    store
        .execute(&mdkg_coord::Command::ExpireDue {
            now: 9_007_199_254_740_991,
        })
        .unwrap();
    let retried = ok(&repo.0, &["done", &id]);
    assert_eq!(retried["data"]["already_done"], true);
    let facts = store.activity_since(&before, "lane-a", 0).unwrap();
    assert!(
        facts.done_marks.is_empty(),
        "current done status alone is not completion evidence"
    );
    assert_eq!(
        sh(&repo.0, &["rev-parse", "HEAD"]),
        starting_head,
        "no done transition in this turn's commit span"
    );
    assert!(mdkg_coord_sqlite::completions_for_receipt(&facts.completions, &[]).is_empty());
}

#[test]
fn a_refused_local_done_is_not_a_completion_fact() {
    let (repo, id) = c_repo("done-activity-refused");
    ok(&repo.0, &["claim", &id]);
    let store = activity_store(&repo.0);
    let before = store.activity_cursor().unwrap();
    let (code, out) = run_env(&repo.0, "lane-b", &["done", &id], &[]);
    assert_eq!(code, 5, "{out}");
    for actor in ["lane-a", "lane-b"] {
        assert!(
            store
                .activity_since(&before, actor, 0)
                .unwrap()
                .done_marks
                .is_empty()
        );
    }
    assert_ne!(file_status(&repo.0, &id), "done");
}

#[test]
fn done_survives_a_crash_at_each_step() {
    for step in ["leased", "written", "released", "marked"] {
        let (repo, id) = c_repo(&format!("crash-{step}"));
        ok(&repo.0, &["claim", &id]);
        ok(&repo.0, &["update", &id, "--status", "review"]);
        let store = activity_store(&repo.0);
        let before = store.activity_cursor().unwrap();
        let (code, _) = run_env(
            &repo.0,
            "lane-a",
            &["done", &id],
            &[("MDKG_CRASH_AFTER", step)],
        );
        assert_eq!(code, 86, "{step}: the hook stopped it");
        let file = file_status(&repo.0, &id);
        let written = matches!(step, "written" | "released" | "marked");
        assert_eq!(file == "done", written, "{step}: file says {file}");
        let marked = store.activity_since(&before, "lane-a", 0).unwrap();
        if matches!(step, "marked" | "released") {
            assert_eq!(marked.done_marks, [scope_of(&id)], "{step}");
        } else {
            assert!(
                marked.done_marks.is_empty(),
                "{step}: no completion before marking"
            );
        }
        converges(&repo.0, &id, &["done", &id], "done");
        let completed = store.activity_since(&before, "lane-a", 0).unwrap();
        assert_eq!(completed.done_marks, [scope_of(&id)], "{step}");
    }
}

#[test]
fn done_survives_one_failed_write_at_each_step() {
    for step in ["leased", "written", "released", "marked"] {
        let (repo, id) = c_repo(&format!("fail-{step}"));
        ok(&repo.0, &["claim", &id]);
        ok(&repo.0, &["update", &id, "--status", "review"]);
        let store = activity_store(&repo.0);
        let before = store.activity_cursor().unwrap();
        let (code, out) = run_env(
            &repo.0,
            "lane-a",
            &["done", &id],
            &[("MDKG_FAIL_AFTER", step)],
        );
        assert_eq!(
            (code, codes(&out)),
            (4, vec!["io.write_failed".to_string()]),
            "{step}: {out}"
        );
        // A failure before the file is written puts everything back: the
        // node is as it was, still claimed and in review.
        if step == "leased" || step == "written" {
            assert_ne!(file_status(&repo.0, &id), "done", "{step}");
            assert_eq!(status_of(&repo.0, &id), "review", "{step}");
        }
        let marked = store.activity_since(&before, "lane-a", 0).unwrap();
        if matches!(step, "marked" | "released") {
            assert_eq!(marked.done_marks, [scope_of(&id)], "{step}");
        } else {
            assert!(
                marked.done_marks.is_empty(),
                "{step}: no completion before marking"
            );
        }
        converges(&repo.0, &id, &["done", &id], "done");
        let completed = store.activity_since(&before, "lane-a", 0).unwrap();
        assert_eq!(completed.done_marks, [scope_of(&id)], "{step}");
    }
}

#[test]
fn a_store_folder_that_refuses_the_write_leaves_the_node_as_it_was() {
    let (repo, id) = c_repo("readonly");
    ok(&repo.0, &["claim", &id]);
    let file = file_of(&repo.0, &id);
    let dir = file.parent().expect("folder").to_path_buf();
    let before = fs::read(&file).expect("file");
    let mut perms = fs::metadata(&dir).expect("meta").permissions();
    use std::os::unix::fs::PermissionsExt as _;
    let mode = perms.mode();
    perms.set_mode(0o555);
    fs::set_permissions(&dir, perms.clone()).expect("read-only");
    let (code, out) = run(&repo.0, &["done", &id]);
    perms.set_mode(mode);
    fs::set_permissions(&dir, perms).expect("writable again");
    assert_eq!(
        (code, codes(&out)),
        (4, vec!["io.write_failed".to_string()]),
        "{out}"
    );
    assert_eq!(fs::read(&file).expect("file"), before, "untouched");
    // No temporary file is left beside it.
    let strays: Vec<String> = fs::read_dir(&dir)
        .expect("dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(".mdkg-write-"))
        .collect();
    assert!(strays.is_empty(), "{strays:?}");
    converges(&repo.0, &id, &["done", &id], "done");
}

#[test]
fn update_survives_a_crash_or_a_failed_write() {
    let (repo, id) = c_repo("update");
    ok(&repo.0, &["claim", &id]);
    let (code, _) = run_env(
        &repo.0,
        "lane-a",
        &["update", &id, "--status", "review"],
        &[("MDKG_CRASH_AFTER", "marked")],
    );
    assert_eq!(code, 86);
    // The state is one store transaction: it's there, whole.
    assert_eq!(status_of(&repo.0, &id), "review");
    let (code, out) = run_env(
        &repo.0,
        "lane-a",
        &["update", &id, "--clear"],
        &[("MDKG_FAIL_AFTER", "marked")],
    );
    assert_eq!(code, 4, "{out}");
    converges(&repo.0, &id, &["update", &id, "--clear"], "progress");
    // The file was never touched by update.
    assert_ne!(file_status(&repo.0, &id), "done");
}

#[test]
fn done_leaves_no_store_mark_so_the_file_can_reopen_it() {
    // A store node's file is its record: once done, the store keeps no
    // mark that would outlast an edit reopening it.
    let (repo, id) = c_repo("reopen");
    ok(&repo.0, &["done", &id]);
    let file = file_of(&repo.0, &id);
    let text = fs::read_to_string(&file).expect("file");
    fs::write(&file, text.replace("status: done", "status: todo")).expect("reopen");
    assert_eq!(status_of(&repo.0, &id), "todo");
}

#[test]
fn a_retyped_store_node_keeps_its_lease_and_state_by_its_uuid() {
    // The scope is the UUID, so a retype moves nothing: the lease and the
    // state stay, and a crash in the middle can't split them (task-47 R1).
    let (repo, id) = c_repo("retype");
    ok(&repo.0, &["claim", &id]);
    ok(&repo.0, &["update", &id, "--status", "blocked"]);
    let out = ok(&repo.0, &["retype", &id, "bug"]);
    let new_id = out["data"]["node"]["id"].as_str().expect("id").to_string();
    assert!(new_id.starts_with("bug-"), "{out}");
    assert_eq!(scope_of(&new_id), scope_of(&id));
    assert_eq!(status_of(&repo.0, &new_id), "blocked");
    // Again: still blocked (a retype that moved states cleared it here).
    ok(&repo.0, &["retype", &new_id, "bug"]);
    assert_eq!(status_of(&repo.0, &new_id), "blocked");
    // A crash mid-retype leaves one identity holding the node.
    let (code, _) = run_env(
        &repo.0,
        "lane-a",
        &["retype", &new_id, "task"],
        &[("MDKG_CRASH_AFTER", "written")],
    );
    assert_eq!(code, 86);
    let (code, out) = run_env(&repo.0, "lane-b", &["claim", &id], &[]);
    assert_eq!(
        (code, codes(&out)),
        (5, vec!["coord.lease_held".to_string()]),
        "{out}"
    );
    assert_eq!(status_of(&repo.0, &id), "blocked");
}

#[test]
fn a_store_claim_is_logged_with_its_node_subject() {
    // Format 9.1: a profile C lease's subject is `node:<uuid>`, beside the
    // node's id; never a `path:`.
    let (repo, id) = c_repo("subjects");
    ok(&repo.0, &["claim", &id]);
    let out = repo.0.join("bundle.jsonl");
    ok(
        &repo.0,
        &["export", "--work", "--out", out.to_str().expect("utf-8")],
    );
    let bundle = fs::read_to_string(&out).expect("bundle");
    let claim = bundle
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|r| r["kind"] == "lease.claim")
        .unwrap_or_else(|| panic!("a lease.claim record: {bundle}"));
    let subjects: Vec<&str> = claim["subject"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    assert!(subjects.contains(&scope_of(&id).as_str()), "{subjects:?}");
    assert!(
        subjects.iter().all(|s| !s.starts_with("path:")),
        "{subjects:?}"
    );
}

/// Makes a store node's file valid but not canonical (its first two
/// frontmatter lines swapped), and returns the canonical text it had.
fn make_messy(file: &Path) -> String {
    let text = fs::read_to_string(file).expect("node");
    let mut lines: Vec<&str> = text.lines().collect();
    lines.swap(1, 2);
    fs::write(file, format!("{}\n", lines.join("\n"))).expect("messy");
    text
}

#[test]
fn format_gives_back_only_a_lease_it_took() {
    // Codex, 185df99: a lease the actor already held (here or in another
    // worktree) stays held after format rewrites the node under it.
    let (repo, id) = c_repo("format-held");
    ok(&repo.0, &["claim", &id]);
    let canonical = make_messy(&file_of(&repo.0, &id));
    ok(&repo.0, &["format", "--write"]);
    assert_eq!(
        fs::read_to_string(file_of(&repo.0, &id)).expect("file"),
        canonical
    );
    let (code, out) = run_env(&repo.0, "lane-b", &["claim", &id], &[]);
    assert_eq!(
        (code, codes(&out)),
        (5, vec!["coord.lease_held".to_string()]),
        "{out}"
    );
}

#[test]
fn format_gets_past_a_lease_that_ran_out() {
    // Codex, 185df99: a lease past its deadline that nothing has expired
    // yet never stops format.
    let (repo, id) = c_repo("format-expired");
    // Claimed three hours ago: its two-hour lease ran out an hour back.
    let then = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
        - 3 * 3600 * 1000;
    let (code, out) = run_env(
        &repo.0,
        "lane-b",
        &["claim", &id],
        &[("MDKG_NOW_MS", &then.to_string())],
    );
    assert_eq!(code, 0, "{out}");
    let canonical = make_messy(&file_of(&repo.0, &id));
    let out = ok(&repo.0, &["format", "--write"]);
    assert!(out["data"]["skipped"].is_null(), "{out}");
    assert_eq!(
        fs::read_to_string(file_of(&repo.0, &id)).expect("file"),
        canonical
    );
}

#[test]
fn a_unicode_space_in_a_field_keeps_the_node_its_scope() {
    // Codex, 185df99: the id is read with format 0.7's lexical rules, so a
    // valid value of U+00A0 doesn't cost the node its scope.
    let (repo, id) = c_repo("nbsp");
    let file = file_of(&repo.0, &id);
    let text = fs::read_to_string(&file).expect("node");
    let text = text.replacen("\ntitle:", "\nx-acme.note: \u{a0}\ntitle:", 1);
    fs::write(&file, text).expect("write");
    let (code, v) = run(&repo.0, &["validate"]);
    assert_eq!(code, 0, "the node is valid: {v}");
    let claimed = ok(&repo.0, &["claim", &id]);
    assert_eq!(
        claimed["data"]["lease"]["scope"],
        scope_of(&id).as_str(),
        "{claimed}"
    );
    ok(&repo.0, &["done", &id]);
}

#[test]
fn a_shared_task_waits_for_a_prerequisite_on_another_branch() {
    // RC1 audit I3: branch A commits feat F; the shared (store) task T is
    // blocked by F. From main's worktree, which lacks F, `next` and
    // `graph ready` don't offer T: its prerequisite isn't done anywhere
    // this worktree can see.
    let (repo, task) = c_repo("cross-branch");
    let linked = repo.0.with_file_name(format!(
        "{}-a",
        repo.0.file_name().expect("name").to_string_lossy()
    ));
    sh(
        &repo.0,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "branch-a",
            linked.to_str().expect("utf-8"),
        ],
    );
    let feat = ok(&linked, &["new", "feat", "Prerequisite"]);
    let feat_id = feat["data"]["node"]["id"].as_str().expect("id").to_string();
    sh(&linked, &["add", "-A"]);
    sh(&linked, &["commit", "-q", "-m", "feat"]);
    // T, in the shared store folder, now waits on F.
    let file = file_of(&repo.0, &task);
    let text = fs::read_to_string(&file).expect("task");
    fs::write(
        &file,
        text.replacen("blocked_by: []", &format!("blocked_by: [{feat_id}]"), 1),
    )
    .expect("blocked_by");
    let out = ok(&repo.0, &["next"]);
    assert!(
        out["data"]["node"].is_null(),
        "main's worktree doesn't offer T: {out}"
    );
    let out = ok(&repo.0, &["graph", "ready"]);
    assert_eq!(out["data"]["count"], 0, "{out}");
    // Branch A sees F unfinished: T waits there too.
    let out = ok(&linked, &["graph", "ready"]);
    let ready: Vec<&str> = out["data"]["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|n| n["id"].as_str())
        .collect();
    assert!(!ready.contains(&task.as_str()), "{out}");
    fs::remove_dir_all(&linked).ok();
}

/// Writes a local task template: the bundled one with `extra` lines added
/// to its frontmatter.
fn task_template(root: &Path, extra: &str) {
    let bundled = include_str!("../../mdkg-graph/bundled/default/task.md");
    let dir = root.join(".mdkg/templates/default");
    fs::create_dir_all(&dir).expect("templates");
    let text = bundled.replacen("---\n", &format!("---\n{extra}"), 1);
    fs::write(dir.join("task.md"), text).expect("template");
}

#[test]
fn new_takes_no_legacy_id_from_a_template() {
    // RC1 audit I2: a local template copied from a migrated node keeps its
    // legacy_id. `new` must not copy it: a second node with that
    // legacy_id would break every read of the shared graph.
    let (repo, id) = c_repo("template-legacy");
    let file = file_of(&repo.0, &id);
    let text = fs::read_to_string(&file).expect("node");
    fs::write(
        &file,
        text.replacen("\ntitle:", "\nlegacy_id: task-1\ntitle:", 1),
    )
    .expect("write");
    ok(&repo.0, &["validate"]);
    task_template(&repo.0, "legacy_id: task-1\n");
    let made = ok(&repo.0, &["new", "task", "From", "the", "template"]);
    let new_id = made["data"]["node"]["id"].as_str().expect("id").to_string();
    let text = fs::read_to_string(file_of(&repo.0, &new_id)).expect("new node");
    assert!(!text.contains("legacy_id"), "{text}");
    ok(&repo.0, &["validate"]);
    let shown = ok(&repo.0, &["show", "task-1", "--fields", "id"]);
    assert_eq!(shown["data"]["node"]["id"], id.as_str(), "{shown}");
}

#[test]
fn a_refused_store_node_leaves_the_graph_readable() {
    // RC1 audit I2: what index::build refuses, `new` refuses before it
    // publishes the file; here, a template with v2 identity fields.
    let (repo, _) = c_repo("template-identity");
    let before = ok(&repo.0, &["list"]);
    task_template(
        &repo.0,
        "graph_id: 01890a5d-ac96-774b-bcce-b302099a8057\n\
         node_id: 01890a5d-ac96-774b-bcce-b302099a8058\n",
    );
    let (code, out) = run(&repo.0, &["new", "task", "Refused"]);
    assert_ne!(code, 0, "{out}");
    assert!(
        codes(&out).iter().any(|c| c == "node.identity_unsupported"),
        "{out}"
    );
    fs::remove_file(repo.0.join(".mdkg/templates/default/task.md")).expect("template");
    let after = ok(&repo.0, &["list"]);
    assert_eq!(after["data"], before["data"], "no node was published");
}

/// The titles of every node `list` shows.
fn titles(root: &Path) -> Vec<String> {
    let out = ok(root, &["list"]);
    out["data"]["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|n| n["title"].as_str().map(String::from))
        .collect()
}

#[test]
fn new_survives_a_crash_or_a_failed_write_at_each_step() {
    // RC1 audit O1: the store node is synced before it gets its name and
    // its folder after, and the graph stays readable at every step: a
    // stop before the name leaves no node, one after leaves the whole one.
    for (hook, step) in [
        ("MDKG_CRASH_AFTER", "staged"),
        ("MDKG_CRASH_AFTER", "linked"),
        ("MDKG_FAIL_AFTER", "staged"),
        ("MDKG_FAIL_AFTER", "linked"),
    ] {
        let (repo, _) = c_repo(&format!("new-{step}"));
        let (code, out) = run_env(
            &repo.0,
            "lane-a",
            &["new", "task", "Probe"],
            &[(hook, step)],
        );
        if hook == "MDKG_CRASH_AFTER" {
            assert_eq!(code, 86, "{step}: the hook stopped it");
        } else {
            assert_eq!(
                (code, codes(&out)),
                (4, vec!["io.write_failed".to_string()]),
                "{step}: {out}"
            );
        }
        ok(&repo.0, &["validate"]);
        let published = titles(&repo.0).iter().any(|t| t == "Probe");
        assert_eq!(published, step == "linked", "{hook} {step}");
        if published {
            let shown = ok(&repo.0, &["show", "Probe"]);
            let text = fs::read_to_string(
                repo.0
                    .join(shown["data"]["node"]["path"].as_str().expect("path")),
            )
            .expect("node");
            assert!(text.contains("# Overview"), "the whole file: {text}");
        } else {
            ok(&repo.0, &["new", "task", "Probe"]);
        }
    }
}

#[test]
fn committed_nodes_never_name_a_private_work_node() {
    // RC1 audit E1 (decided 2026-10-25: work refers to design, never the
    // other way round): a committed node naming a store work node breaks
    // every fresh clone, so `new` refuses it and `validate` reports it;
    // the work node naming the design is fine.
    let (repo, task) = c_repo("private-target");
    let (code, out) = run(
        &repo.0,
        &[
            "new", "dec", "Adopt", "importer", "--status", "accepted", "--refs", &task,
        ],
    );
    assert_eq!(code, 2, "{out}");
    assert_eq!(codes(&out), ["edge.private_target"], "{out}");
    let hint = out["diagnostics"][0]["hint"].as_str().unwrap_or_default();
    assert!(hint.contains("goal or epic"), "{out}");
    assert!(!titles(&repo.0).iter().any(|t| t == "Adopt importer"));

    // Written by hand, validate refuses it; reads still work here.
    let dec = ok(&repo.0, &["new", "dec", "Adopt", "importer"]);
    let dec_id = dec["data"]["node"]["id"].as_str().expect("id").to_string();
    let path = repo
        .0
        .join(dec["data"]["node"]["path"].as_str().expect("path"));
    let text = fs::read_to_string(&path).expect("dec");
    assert!(text.contains("refs: []"), "{text}");
    fs::write(
        &path,
        text.replacen("refs: []", &format!("refs: [{task}]"), 1),
    )
    .expect("refs");
    let (code, out) = run(&repo.0, &["validate"]);
    assert_eq!(code, 2, "{out}");
    assert!(
        codes(&out).contains(&"edge.private_target".to_string()),
        "{out}"
    );
    ok(&repo.0, &["show", &dec_id]);
    fs::write(&path, text).expect("put back");

    // Work refers to design: a store task under a committed epic, naming
    // the decision, and a committed goal scoped by that epic.
    let epic = ok(&repo.0, &["new", "epic", "Importer"]);
    let epic_id = epic["data"]["node"]["id"].as_str().expect("id").to_string();
    ok(
        &repo.0,
        &[
            "new", "task", "Build", "it", "--epic", &epic_id, "--refs", &dec_id,
        ],
    );
    ok(&repo.0, &["validate"]);
}

#[test]
fn a_fresh_clone_reads_a_goal_whose_active_task_it_lacks() {
    // RC1 audit E1 (orchestrator, 2026-10-25): a goal's active_node and
    // last_active_node are runtime pointers, so naming a store task is
    // allowed; a fresh clone, which has no store nodes, warns and reads
    // on, never errors. Written as bare ids, or as this graph's mdkg://
    // refs (Codex, 4fea836).
    fresh_clone_reads(false);
    fresh_clone_reads(true);
}

fn fresh_clone_reads(qualified: bool) {
    let (repo, _) = c_repo(if qualified {
        "fresh-clone-uri"
    } else {
        "fresh-clone"
    });
    let epic = ok(&repo.0, &["new", "epic", "Shipping"]);
    let epic_id = epic["data"]["node"]["id"].as_str().expect("id").to_string();
    let task = ok(&repo.0, &["new", "task", "Ship", "--epic", &epic_id]);
    let task_id = task["data"]["node"]["id"].as_str().expect("id").to_string();
    let goal = ok(&repo.0, &["new", "goal", "Ship", "it"]);
    let goal_id = goal["data"]["node"]["id"].as_str().expect("id").to_string();
    let goal_path = repo
        .0
        .join(goal["data"]["node"]["path"].as_str().expect("path"));
    let pointer = if qualified {
        let config: Value = serde_json::from_str(
            &fs::read_to_string(repo.0.join(".mdkg/config.json")).expect("config"),
        )
        .expect("config json");
        let graph = config["graph_id"].as_str().expect("graph_id");
        format!("mdkg://{graph}/{task_id}")
    } else {
        task_id.clone()
    };
    let text = fs::read_to_string(&goal_path).expect("goal");
    fs::write(
        &goal_path,
        text.replace(
            "scope_refs: []",
            &format!(
                "scope_refs: [{epic_id}]\nactive_node: {pointer}\nlast_active_node: {pointer}"
            ),
        ),
    )
    .expect("goal");
    sh(&repo.0, &["add", "."]);
    sh(&repo.0, &["commit", "-q", "-m", "goal"]);
    ok(&repo.0, &["goal", "activate", &goal_id]);
    let out = ok(&repo.0, &["validate"]);
    assert!(
        !codes(&out).contains(&"edge.target_not_on_branch".to_string()),
        "{out}"
    );

    let clone = repo.0.with_file_name(format!(
        "{}-clone",
        repo.0.file_name().expect("name").to_string_lossy()
    ));
    let out = isolated(
        Command::new("git")
            .arg("clone")
            .arg("-q")
            .arg(&repo.0)
            .arg(&clone),
    )
    .output()
    .expect("git clone");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let clone = Scratch(clone);
    let out = ok(&clone.0, &["validate"]);
    let warned = codes(&out)
        .iter()
        .filter(|c| *c == "edge.target_not_on_branch")
        .count();
    assert_eq!(warned, 2, "active_node and last_active_node: {out}");
    ok(&clone.0, &["list"]);
    ok(&clone.0, &["show", &goal_id]);
    ok(&clone.0, &["next"]);
    for args in [&["goal", "current"][..], &["goal", "next", &goal_id]] {
        let out = ok(&clone.0, args);
        let said = out["diagnostics"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|d| d["code"] == "edge.target_not_on_branch")
            .and_then(|d| d["message"].as_str())
            .unwrap_or_default()
            .to_string();
        assert!(
            said.contains(&task_id) && said.contains("isn't in this checkout"),
            "{args:?}: {out}"
        );
    }
}

#[test]
fn a_loops_scope_refs_are_text() {
    // Lane 1, format section 5: only a goal's scope_refs name nodes; a
    // loop's are uninterpreted, so naming a store task there is no
    // edge.private_target.
    let (repo, task) = c_repo("loop-scope");
    let made = ok(&repo.0, &["new", "loop", "Nightly"]);
    let path = repo
        .0
        .join(made["data"]["node"]["path"].as_str().expect("path"));
    let text = fs::read_to_string(&path).expect("loop");
    assert!(text.contains("scope_refs: []"), "{text}");
    // The bundled loop names a skill this repository doesn't have.
    let text = text.replacen("skills: [pursue-mdkg-loop]", "skills: []", 1);
    fs::write(
        &path,
        text.replacen("scope_refs: []", &format!("scope_refs: [{task}]"), 1),
    )
    .expect("scope");
    let out = ok(&repo.0, &["validate"]);
    assert!(
        !codes(&out).contains(&"edge.private_target".to_string()),
        "{out}"
    );
}
