//! The tier A write commands end to end (task-12): two worktrees create
//! nodes without id collisions, claim and update work, finish it with
//! `done`, pick up work with `next`, and run goals, all through the binary.
//!
//! When `MDKG_ORACLE_NODE` and `MDKG_ORACLE_CLI` name a Node binary and the
//! 0.6.0 CLI (`scripts/check.sh` sets both), the test also checks that 0.6.0
//! still reads every file the commands wrote (dec-2 decision 12).

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

mod common;

const TTL: u64 = 60_000;

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        if std::env::var_os("MDKG_KEEP_SCRATCH").is_none() {
            fs::remove_dir_all(&self.0).ok();
        }
    }
}

fn scratch() -> Scratch {
    let dir = common::temp::dir("mdkg-cli-write");
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

fn run(worktree: &Path, actor: &str, now: u64, args: &[&str]) -> (i32, Value) {
    run_env(worktree, actor, now, args, &[])
}

fn run_env(
    worktree: &Path,
    actor: &str,
    now: u64,
    args: &[&str],
    env: &[(&str, &OsStr)],
) -> (i32, Value) {
    let out = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
        .arg("--root")
        .arg(worktree)
        .args(["--as", actor])
        .args(args)
        .arg("--json")
        .env("MDKG_NOW_MS", now.to_string())
        .env("MDKG_LEASE_TTL_MS", TTL.to_string())
        .envs(env.iter().copied())
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

fn ok(worktree: &Path, actor: &str, now: u64, args: &[&str]) -> Value {
    let (code, out) = run(worktree, actor, now, args);
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

fn status_of(worktree: &Path, actor: &str, now: u64, id: &str) -> String {
    let out = ok(worktree, actor, now, &["show", id, "--fields", "status"]);
    out["data"]["node"]["status"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// 0.6.0 must still read the graph, if the oracle is available.
fn oracle_validates(root: &Path) {
    let (Ok(node), Ok(cli)) = (
        std::env::var("MDKG_ORACLE_NODE"),
        std::env::var("MDKG_ORACLE_CLI"),
    ) else {
        return;
    };
    let out = Command::new(node)
        .arg(cli)
        .args(["validate", "--root"])
        .arg(root)
        .output()
        .expect("run the 0.6.0 CLI");
    assert!(
        out.status.success(),
        "0.6.0 can't read the graph: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn two_worktrees_write_without_collisions() {
    let dir = scratch();
    let repo = dir.0.join("repo");
    let conformance = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance");
    fs::create_dir_all(&repo).expect("repo");
    copy_tree(&conformance.join("base"), &repo);
    copy_tree(
        &conformance.join("cases/g01-valid-basic/.mdkg"),
        &repo.join(".mdkg"),
    );
    sh(&repo, &["init", "-q", "-b", "main"]);
    sh(&repo, &["add", "."]);
    sh(&repo, &["commit", "-q", "-m", "init"]);
    let wa = dir.0.join("wt-a");
    let wb = dir.0.join("wt-b");
    for (branch, wt) in [("a", &wa), ("b", &wb)] {
        let path = wt.to_str().expect("utf-8");
        sh(
            &repo,
            &["worktree", "add", "-q", "-b", branch, path, "main"],
        );
    }

    // Both worktrees create a task from the same base: the ids differ.
    let a_new = ok(&wa, "a", 0, &["new", "task", "Parser", "--priority", "1"]);
    let b_new = ok(&wb, "b", 1, &["new", "task", "Lexer", "--priority", "1"]);
    let (a_id, b_id) = (
        a_new["data"]["node"]["id"]
            .as_str()
            .expect("id")
            .to_string(),
        b_new["data"]["node"]["id"]
            .as_str()
            .expect("id")
            .to_string(),
    );
    assert_eq!((a_id.as_str(), b_id.as_str()), ("task-3", "task-4"));
    assert_eq!(a_new["data"]["node"]["status"], "backlog");
    sh(&wa, &["add", "."]);
    sh(&wa, &["commit", "-q", "-m", "a: new task"]);
    sh(&wb, &["add", "."]);
    sh(&wb, &["commit", "-q", "-m", "b: new task"]);

    // Lasting statuses only: `new` refuses temporary ones.
    let (code, out) = run(&wa, "a", 2, &["new", "task", "X", "--status", "progress"]);
    assert_eq!(
        (code, codes(&out)),
        (1, vec!["usage.bad_status".to_string()])
    );

    // A claims and marks review; the status is the coordinator's, not the file's.
    ok(&wa, "a", 3, &["claim", "task-1"]);
    assert_eq!(status_of(&wa, "a", 4, "task-1"), "progress");
    ok(&wa, "a", 5, &["update", "task-1", "--status", "review"]);
    assert_eq!(status_of(&wb, "b", 6, "task-1"), "review");
    let text = fs::read_to_string(wa.join(".mdkg/work/task-1-basic.md")).expect("read");
    assert!(
        text.contains("status: todo"),
        "temporary states never touch files"
    );
    let (code, out) = run(&wb, "b", 7, &["update", "task-1", "--status", "blocked"]);
    assert_eq!(
        (code, codes(&out)),
        (5, vec!["coord.not_owner".to_string()])
    );
    let (code, out) = run(&wa, "a", 7, &["update", "task-1", "--status", "done"]);
    assert_eq!(
        (code, codes(&out)),
        (1, vec!["usage.bad_status".to_string()])
    );

    // B's next skips A's claimed task and takes the highest priority.
    let next = ok(&wb, "b", 8, &["next"]);
    assert_eq!(next["data"]["node"]["id"], "task-4");
    assert!(
        next["data"]["node"]["body"].is_string(),
        "next returns the full record"
    );

    // B can't finish A's task; A finishes it with a checkpoint.
    let (code, out) = run(&wb, "b", 9, &["done", "task-1"]);
    assert_eq!(
        (code, codes(&out)),
        (5, vec!["coord.lease_held".to_string()])
    );
    let done = ok(&wa, "a", 10, &["done", "task-1", "--checkpoint"]);
    assert_eq!(done["data"]["node"]["status"], "done");
    let checkpoint = done["data"]["checkpoint"].as_str().expect("checkpoint");
    assert_eq!(checkpoint, "chk-1");
    let head = sh(&wa, &["log", "-1", "--format=%s"]);
    assert_eq!(head, "task-1: done");
    let files = sh(&wa, &["show", "--name-only", "--format=", "HEAD"]);
    assert!(
        files.contains("task-1-basic.md") && files.contains("chk-1-"),
        "{files}"
    );

    // B sees A's version waiting to merge; A's lease is gone.
    let status = ok(&wb, "b", 11, &["status"]);
    assert_eq!(status["data"]["queue"][0]["branch"], "a");
    assert_eq!(status["data"]["leases"], serde_json::json!([]));

    // B creates a node while A's chk-1 is unmerged: no collision either.
    let b_chk = ok(&wb, "b", 12, &["new", "checkpoint", "Lexer notes"]);
    assert_eq!(b_chk["data"]["node"]["id"], "chk-2");

    // Goals: one is active at a time, and next stays inside its scope.
    let goal = ok(&wa, "a", 13, &["new", "goal", "Ship"]);
    let goal_id = goal["data"]["node"]["id"]
        .as_str()
        .expect("goal id")
        .to_string();
    assert_eq!(goal["data"]["node"]["fields"]["goal_state"], "active");
    let current = ok(&wa, "a", 14, &["goal", "current"]);
    assert_eq!(current["data"]["goal"]["id"], goal_id.as_str());
    let other = ok(&wa, "a", 15, &["new", "goal", "Later"]);
    assert_eq!(other["data"]["node"]["fields"]["goal_state"], "paused");
    let other_id = other["data"]["node"]["id"]
        .as_str()
        .expect("goal id")
        .to_string();
    let activated = ok(&wa, "a", 16, &["goal", "activate", &other_id]);
    assert_eq!(activated["data"]["goal"]["fields"]["goal_state"], "active");
    let current = ok(&wa, "a", 17, &["goal", "current"]);
    assert_eq!(current["data"]["goal"]["id"], other_id.as_str());
    let (code, out) = run(&wa, "a", 18, &["goal", "next"]);
    assert_eq!(
        (code, codes(&out)),
        (0, vec!["node.none_ready".to_string()])
    );

    // The 0.6.0 spellings still work, with a hint on stderr.
    ok(&wb, "b", 19, &["task", "start", "task-4"]);
    assert_eq!(status_of(&wb, "b", 20, "task-4"), "progress");
    ok(&wb, "b", 21, &["task", "done", "task-4"]);
    assert_eq!(status_of(&wb, "b", 22, "task-4"), "done");

    // Merging both branches settles everything, and the graph still validates.
    sh(&repo, &["merge", "-q", "--no-ff", "-m", "merge a", "a"]);
    sh(&repo, &["merge", "-q", "--no-ff", "-m", "merge b", "b"]);
    let status = ok(&repo, "nick", 23, &["status"]);
    assert_eq!(status["data"]["queue"], serde_json::json!([]));
    let validate = ok(&repo, "nick", 24, &["validate"]);
    assert_eq!(validate["data"]["errors"], 0);
    oracle_validates(&repo);
}

#[test]
fn format_reports_then_rewrites() {
    let dir = scratch();
    let conformance = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance");
    copy_tree(&conformance.join("base"), &dir.0);
    copy_tree(
        &conformance.join("cases/g01-valid-basic/.mdkg"),
        &dir.0.join(".mdkg"),
    );
    let file = dir.0.join(".mdkg/work/task-2-other.md");
    let text = fs::read_to_string(&file).expect("read");
    // Out of canonical order: title after status.
    let shuffled = text.replace(
        "title: Other task\nstatus: backlog",
        "status: backlog\ntitle: Other task",
    );
    fs::write(&file, &shuffled).expect("write");
    let out = ok(&dir.0, "a", 0, &["format"]);
    assert_eq!(
        out["data"]["files"],
        serde_json::json!([".mdkg/work/task-2-other.md"])
    );
    assert_eq!(
        fs::read_to_string(&file).expect("read"),
        shuffled,
        "a dry run by default"
    );
    ok(&dir.0, "a", 1, &["format", "--write"]);
    assert_eq!(fs::read_to_string(&file).expect("read"), text);
    let out = ok(&dir.0, "a", 2, &["format"]);
    assert_eq!(out["data"]["files"], serde_json::json!([]));
}

/// The g01 fixture in a repository with worktrees `wt-a` and `wt-b`; task-1
/// refs task-2, and the chain runs task-1, task-2, bug-1 (`next` and `prev`
/// edges).
fn fixture() -> (Scratch, PathBuf, PathBuf, PathBuf) {
    let dir = scratch();
    let repo = dir.0.join("repo");
    let conformance = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance");
    fs::create_dir_all(&repo).expect("repo");
    copy_tree(&conformance.join("base"), &repo);
    copy_tree(
        &conformance.join("cases/g01-valid-basic/.mdkg"),
        &repo.join(".mdkg"),
    );
    let t1 = repo.join(".mdkg/work/task-1-basic.md");
    let text = fs::read_to_string(&t1).expect("read");
    fs::write(
        &t1,
        text.replacen("refs: [task-2]", "refs: [task-2]\nnext: task-2", 1),
    )
    .expect("write");
    let t2 = repo.join(".mdkg/work/task-2-other.md");
    let text = fs::read_to_string(&t2).expect("read");
    fs::write(
        &t2,
        text.replacen(
            "\nstatus: backlog",
            "\nstatus: backlog\nprev: task-1\nnext: bug-1",
            1,
        ),
    )
    .expect("write");
    fs::write(
        repo.join(".mdkg/work/bug-1-chain.md"),
        "---\nid: bug-1\ntype: bug\ntitle: Chain end\nstatus: todo\nprev: task-2\ncreated: 2026-10-18\nupdated: 2026-10-18\n---\n# Overview\n\nBody text.\n",
    )
    .expect("bug");
    sh(&repo, &["init", "-q", "-b", "main"]);
    sh(&repo, &["add", "."]);
    sh(&repo, &["commit", "-q", "-m", "init"]);
    let wa = dir.0.join("wt-a");
    let wb = dir.0.join("wt-b");
    for (branch, wt) in [("a", &wa), ("b", &wb)] {
        let path = wt.to_str().expect("utf-8");
        sh(
            &repo,
            &["worktree", "add", "-q", "-b", branch, path, "main"],
        );
    }
    (dir, repo, wa, wb)
}

#[test]
fn a_turn_or_a_path_under_one_never_opens_the_store() {
    let (_dir, repo, wa, _wb) = fixture();
    for typed in ["./run:x/a.md", "run:codex", "role:x"] {
        let (code, out) = run(&wa, "a", 0, &["update", typed, "--clear"]);
        assert_eq!(
            (code, codes(&out)),
            (1, vec!["msg.bad_id".to_string()]),
            "{typed}: {out}"
        );
    }
    assert!(!repo.join(".git/mdkg").exists(), "no store opened");
}

#[test]
fn review_regressions_for_the_write_commands() {
    let (_dir, _repo, wa, wb) = fixture();

    // A request that fails uses up no id.
    let (code, _) = run(&wa, "a", 0, &["new", "task", "X", "--parent", "nope"]);
    assert_ne!(code, 0);
    let (code, out) = run(&wa, "a", 1, &["new", "task", "Y", "--status", "progress"]);
    assert_eq!(code, 1, "{out}");
    let (code, out) = run(&wa, "a", 1, &["new", "task", "W", "--next", "task-1"]);
    assert_eq!(
        (code, codes(&out)),
        (2, vec!["edge.prev_next_asymmetric".to_string()])
    );
    let made = ok(&wa, "a", 2, &["new", "task", "Z"]);
    assert_eq!(made["data"]["node"]["id"], "task-3");

    // An item waiting for review isn't offered by `next`.
    ok(&wa, "a", 3, &["claim", "task-1"]);
    let set = ok(&wa, "a", 4, &["update", "task-1", "--status", "review"]);
    assert_eq!(
        set["data"]["status"], "review",
        "the effective status: {set}"
    );
    ok(&wa, "a", 5, &["release", "task-1"]);
    let next = ok(&wb, "b", 6, &["next"]);
    assert_eq!(next["data"]["node"]["id"], "bug-1", "{next}");
    ok(&wa, "a", 6, &["update", "bug-1", "--status", "blocked"]);
    let next = ok(&wb, "b", 6, &["next"]);
    assert_eq!(
        next["data"]["node"]["id"], "task-2",
        "blocked is skipped too: {next}"
    );
    // `status` shows temporary statuses, everyone's, from any worktree.
    let work = |out: &serde_json::Value| -> Vec<(String, String, String)> {
        out["data"]["work"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|w| {
                let scope = w["scope"].as_str().unwrap_or_default();
                let file = scope.rsplit('/').next().unwrap_or_default();
                let id = file.split('-').take(2).collect::<Vec<_>>().join("-");
                (id, w["status"].to_string(), w["actor"].to_string())
            })
            .collect()
    };
    let status = ok(&wb, "b", 6, &["status"]);
    assert_eq!(
        work(&status),
        [
            ("bug-1".into(), "\"blocked\"".into(), "\"a\"".into()),
            ("task-1".into(), "\"review\"".into(), "\"a\"".into()),
        ],
        "{status}"
    );
    ok(&wa, "a", 6, &["update", "bug-1", "--clear"]);
    let status = ok(&wb, "b", 6, &["status"]);
    assert_eq!(work(&status).len(), 1, "{status}");

    // A state whose node is gone is cleared by its file path (task-16).
    let made = ok(&wa, "a", 6, &["new", "task", "Soon gone"]);
    let id = made["data"]["node"]["id"].as_str().expect("id").to_string();
    let gone = made["data"]["node"]["path"]
        .as_str()
        .expect("path")
        .to_string();
    ok(&wa, "a", 6, &["update", &id, "--status", "review"]);
    fs::remove_file(wa.join(&gone)).expect("remove");
    let not_found = || vec!["node.not_found".to_string()];
    let (code, out) = run(&wa, "a", 6, &["update", &id, "--clear"]);
    assert_eq!((code, codes(&out)), (3, not_found()), "{out}");
    // Neither a node nor a recorded state, inside or outside the worktree;
    // the typed path isn't quoted.
    for typed in [".mdkg/work/none-sk_live.md", "../outside-sk_live.md"] {
        let (code, out) = run(&wa, "a", 6, &["update", typed, "--clear"]);
        assert_eq!((code, codes(&out)), (3, not_found()), "{out}");
        assert!(!out.to_string().contains("sk_live"), "{out}");
    }
    // Under a folder named like a turn: refused as that overlap, still
    // without quoting the path.
    let (code, out) = run(&wa, "a", 6, &["update", "./run:sk_live_x/a.md", "--clear"]);
    assert_eq!(
        (code, codes(&out)),
        (1, vec!["msg.bad_id".to_string()]),
        "{out}"
    );
    assert!(!out.to_string().contains("sk_live"), "{out}");
    let (code, out) = run(&wa, "a", 6, &["update", &gone, "--status", "review"]);
    assert_eq!(
        (code, codes(&out)),
        (3, not_found()),
        "setting a state needs a node: {out}"
    );
    let cleared = ok(&wa, "a", 6, &["update", &gone, "--clear"]);
    assert_eq!(cleared["data"]["node"], gone.as_str(), "{cleared}");
    let status = ok(&wb, "b", 6, &["status"]);
    assert_eq!(
        work(&status).len(),
        1,
        "only task-1's review is left: {status}"
    );

    // A node alive only in another worktree keeps its state: b sees a's
    // review in `status`, but only a may clear it by path.
    let made = ok(&wa, "a", 6, &["new", "task", "Lives in a"]);
    let id = made["data"]["node"]["id"].as_str().expect("id").to_string();
    let elsewhere = made["data"]["node"]["path"]
        .as_str()
        .expect("path")
        .to_string();
    ok(&wa, "a", 6, &["update", &id, "--status", "review"]);
    let (code, out) = run(&wb, "b", 6, &["update", &elsewhere, "--clear"]);
    assert_eq!(
        (code, codes(&out)),
        (5, vec!["coord.not_owner".to_string()]),
        "{out}"
    );
    let status = ok(&wb, "b", 6, &["status"]);
    assert_eq!(work(&status).len(), 2, "a's review is kept: {status}");
    // Clearing as a, deliberately, works from b, and the result names the
    // path as stored, not as typed.
    let shouted = elsewhere.to_uppercase();
    let cleared = ok(&wb, "a", 6, &["update", &shouted, "--clear"]);
    assert_eq!(cleared["data"]["node"], elsewhere.as_str(), "{cleared}");

    // A typed id that isn't a node isn't quoted when setting a state either.
    let (code, out) = run(
        &wa,
        "a",
        6,
        &["update", "sk_live_abc", "--status", "review"],
    );
    assert_eq!((code, codes(&out)), (3, not_found()), "{out}");
    assert!(!out.to_string().contains("sk_live"), "{out}");
    let (code, out) = run(
        &wa,
        "a",
        6,
        &["update", "task-1", "--status", "sk_live_xyz"],
    );
    assert_eq!(
        (code, codes(&out)),
        (1, vec!["usage.bad_status".to_string()]),
        "{out}"
    );
    assert!(!out.to_string().contains("sk_live"), "{out}");

    // A node in this worktree has no owner rule, by id or by path: b clears
    // a's review of task-1, which b can see too.
    let cleared = ok(
        &wb,
        "b",
        6,
        &["update", ".mdkg/work/task-1-basic.md", "--clear"],
    );
    assert_eq!(
        cleared["data"]["node"], "task-1",
        "a path naming a node is that node: {cleared}"
    );
    assert_eq!(cleared["data"]["status"], "todo", "{cleared}");
    ok(&wa, "a", 6, &["update", "task-1", "--status", "blocked"]);
    let cleared = ok(&wb, "b", 6, &["update", "task-1", "--clear"]);
    assert_eq!(cleared["data"]["status"], "todo", "{cleared}");
    let status = ok(&wb, "b", 6, &["status"]);
    assert!(work(&status).is_empty(), "{status}");

    // `next <id>` walks past a held item to the next available one.
    ok(&wa, "a", 7, &["claim", "task-2"]);
    let next = ok(&wb, "b", 8, &["next", "task-1"]);
    assert_eq!(next["data"]["node"]["id"], "bug-1", "{next}");

    // `done --checkpoint` that can't take the lease leaves no checkpoint.
    let (code, out) = run(&wb, "b", 9, &["done", "task-2", "--checkpoint"]);
    assert_eq!(
        (code, codes(&out)),
        (5, vec!["coord.lease_held".to_string()])
    );
    let dirty = sh(&wb, &["status", "--porcelain"]);
    assert!(!dirty.contains("chk-"), "{dirty}");
    ok(&wa, "a", 10, &["release", "task-2"]);

    // `goal claim --json` prints one envelope, with the lease.
    let goal = ok(&wb, "b", 11, &["new", "goal", "Ship"]);
    let path = wb.join(goal["data"]["node"]["path"].as_str().expect("path"));
    let text = fs::read_to_string(&path).expect("read");
    let scoped = if text.contains("scope_refs: []") {
        text.replacen("scope_refs: []", "scope_refs: [task-2]", 1)
    } else {
        text.replacen("\ntitle:", "\nscope_refs: [task-2]\ntitle:", 1)
    };
    fs::write(&path, scoped).expect("write");
    let claimed = ok(&wb, "b", 12, &["goal", "claim"]);
    assert_eq!(claimed["data"]["node"]["id"], "task-2", "{claimed}");
    assert_eq!(claimed["data"]["leases"][0]["owner"], "b", "{claimed}");

    // `validate --changed-only` still reports errors in unchanged files.
    sh(&wa, &["rm", "-q", ".mdkg/work/task-2-other.md"]);
    let (code, out) = run(&wa, "a", 13, &["validate", "--changed-only"]);
    assert_eq!(code, 2, "{out}");
    assert!(
        codes(&out).iter().any(|c| c == "edge.missing_target"),
        "{out}"
    );
}

#[test]
fn a_failed_done_puts_the_worktree_back_as_it_was() {
    let (_dir, repo, wa, _wb) = fixture();
    // Uncommitted work: a note in task-1, and a new node with notes.
    let t1 = wa.join(".mdkg/work/task-1-basic.md");
    let before = fs::read_to_string(&t1).expect("read") + "\nAn uncommitted note.\n";
    fs::write(&t1, &before).expect("write");
    // Staged on purpose, then edited again: a failed `done` must leave the
    // staged version staged and the newer one in the worktree.
    sh(&wa, &["add", ".mdkg/work/task-1-basic.md"]);
    let staged = before.clone();
    let before = before + "A second, unstaged note.\n";
    fs::write(&t1, &before).expect("write");
    let made = ok(&wa, "a", 0, &["new", "task", "Fresh one"]);
    let fresh = wa.join(made["data"]["node"]["path"].as_str().expect("path"));
    let fresh_id = made["data"]["node"]["id"].as_str().expect("id").to_string();
    let fresh_text = fs::read_to_string(&fresh).expect("read") + "\nNotes on the new one.\n";
    fs::write(&fresh, &fresh_text).expect("write");

    // Every commit fails after staging: the branch's ref is locked, so
    // `done` can't record. (Hooks never run: mdkg's git ignores
    // them, so a hook that rewrites or blocks can't be the failure.)
    fs::write(repo.join(".git/refs/heads/a.lock"), "").expect("lock");
    // Hooks that run even under --no-verify: only core.hooksPath stops them.
    let marker = repo.with_file_name("hook-ran");
    for name in [
        "pre-commit",
        "post-index-change",
        "prepare-commit-msg",
        "reference-transaction",
    ] {
        let hook = repo.join(".git/hooks").join(name);
        fs::create_dir_all(hook.parent().expect("hooks")).expect("hooks dir");
        fs::write(
            &hook,
            format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display()),
        )
        .expect("hook");
        let mut perms = fs::metadata(&hook).expect("meta").permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        fs::set_permissions(&hook, perms).expect("chmod");
    }

    let (code, out) = run(&wa, "a", 1, &["done", "task-1", "--checkpoint"]);
    assert_ne!(code, 0, "{out}");
    assert!(!marker.exists(), "no hook ran during checkpoint rollback");
    assert_eq!(
        fs::read_to_string(&t1).expect("read"),
        before,
        "uncommitted edits survive"
    );
    let (code, out) = run(&wa, "a", 2, &["done", &fresh_id]);
    assert_ne!(code, 0, "{out}");
    assert!(!marker.exists(), "no hook ran during new-node rollback");
    assert_eq!(
        fs::read_to_string(&fresh).expect("read"),
        fresh_text,
        "a new node survives"
    );

    // The index is as it was (only the note staged on purpose), nothing is
    // left behind, and no lease is held.
    assert_eq!(
        sh(&wa, &["diff", "--cached", "--name-only"]),
        ".mdkg/work/task-1-basic.md"
    );
    assert_eq!(
        sh(&wa, &["show", ":.mdkg/work/task-1-basic.md"]),
        staged.trim_end(),
        "the staged version is exactly what was staged"
    );
    // Force a refreshable stat entry without changing file contents. A plain
    // status may rewrite the index and run post-index-change, making this
    // observer itself trip the hooks intended to catch a broken done.
    fs::File::open(wa.join(".mdkg/work/task-2-other.md"))
        .expect("untouched node")
        .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1))
        .expect("make cached stat information stale");
    let observed = isolated(Command::new("git").arg("-C").arg(&wa))
        // Override even an explicit request for optional writes from the
        // caller: this command observes rollback and must not run a hook.
        .env("GIT_OPTIONAL_LOCKS", "1")
        .args(["--no-optional-locks", "status", "--porcelain"])
        .output()
        .expect("observe worktree");
    assert!(observed.status.success(), "{observed:?}");
    let untracked = String::from_utf8(observed.stdout).expect("status UTF-8");
    assert!(!untracked.contains("chk-"), "{untracked}");
    assert!(!marker.exists(), "the status observer must not run a hook");
    let status = ok(&wa, "a", 3, &["status"]);
    assert_eq!(status["data"]["leases"], serde_json::json!([]), "{status}");
    assert!(!repo.with_file_name("hook-ran").exists(), "no hook ran");
}

/// Round 1 of 486f2ef: repository config can't fake or block a `done`
/// commit: a textconv that makes every change look empty, and signing
/// through a program that fails.
#[test]
fn repository_config_cant_fake_or_block_a_commit() {
    let (_dir, repo, wa, wb) = fixture();
    sh(&repo, &["config", "diff.md.textconv", "true"]);
    sh(&repo, &["config", "diff.md.command", "true"]);
    sh(&repo, &["config", "diff.md.trustExitCode", "true"]);
    sh(&repo, &["config", "commit.gpgSign", "true"]);
    sh(&repo, &["config", "gpg.program", "false"]);
    sh(&repo, &["config", "merge.verifySignatures", "true"]);
    // The repository's own attributes are read (task-28); a checkout's
    // aren't.
    fs::create_dir_all(repo.join(".git/info")).expect("info");
    fs::write(repo.join(".git/info/attributes"), "*.md diff=md\n").expect("attributes");
    fs::write(wa.join(".gitattributes"), "*.md diff=md\n").expect("attributes");
    let t1 = wa.join(".mdkg/work/task-1-basic.md");
    let text = fs::read_to_string(&t1).expect("read") + "\nA note.\n";
    fs::write(&t1, text).expect("write");
    let before = sh(&wa, &["rev-parse", "HEAD"]);
    ok(&wa, "a", 0, &["claim", "task-1"]);
    ok(&wa, "a", 1, &["done", "task-1"]);
    assert_ne!(sh(&wa, &["rev-parse", "HEAD"]), before, "committed");

    // A sync merges an unsigned version though config asks to sign and to
    // verify signatures.
    let t2 = wa.join(".mdkg/work/task-2-other.md");
    ok(&wa, "a", 2, &["claim", "task-2"]);
    let text = fs::read_to_string(&t2).expect("read") + "\nEdited by a.\n";
    fs::write(&t2, text).expect("write");
    ok(&wa, "a", 3, &["done", "task-2", "--checkpoint"]);
    ok(&wa, "a", 4, &["release", "task-2"]);
    let claim = ok(&wb, "b", 5, &["claim", "task-2"]);
    assert!(
        codes(&claim).contains(&"coord.sync_needed".to_string()),
        "{claim}"
    );
    // Never merged onto a tag a rewritten HEAD names.
    sh(&repo, &["tag", "t1", "b"]);
    let wb_head = repo.join(".git/worktrees/wt-b/HEAD");
    let saved = fs::read_to_string(&wb_head).expect("HEAD");
    fs::write(&wb_head, "ref: refs/tags/t1\n").expect("HEAD");
    let (code, out) = run(&wb, "b", 6, &["sync", "task-2"]);
    assert_ne!(code, 0, "{out}");
    fs::write(&wb_head, saved).expect("restore");
    let sync = ok(&wb, "b", 6, &["sync", "task-2"]);
    assert_eq!(
        sync["data"]["stacked"].as_array().map(Vec::len),
        Some(1),
        "{sync}"
    );
    assert!(
        fs::read_to_string(wb.join(".mdkg/work/task-2-other.md"))
            .expect("read")
            .contains("Edited by a.")
    );
}

/// Round 1 of b5bde24: each worktree reaches only its own git folder, git's
/// own reading of `.git` decides, and the pin admits only real worktrees.
#[test]
fn a_worktree_reaches_only_its_own_git_folder() {
    let (dir, repo, wa, wb) = fixture();
    let common = repo.join(".git");
    let pin = [("MDKG_TRUSTED_COMMON_DIR", common.as_os_str())];
    let main_head = || sh(&repo, &["rev-parse", "main"]);
    let main_before = main_head();
    let (code, out) = run_env(&wa, "a", 0, &["claim", "task-1"], &pin);
    assert_eq!(code, 0, "pinned and genuine: {out}");
    let t1 = wa.join(".mdkg/work/task-1-basic.md");
    let text = fs::read_to_string(&t1).expect("read") + "\nA note.\n";
    fs::write(&t1, text).expect("write");
    let link = wa.join(".git");
    let real = fs::read_to_string(&link).expect("link");
    let refused = |what: &str, env: &[(&str, &OsStr)]| {
        let (code, out) = run_env(&wa, "a", 1, &["done", "task-1"], env);
        assert_ne!(code, 0, "{what}: {out}");
        assert_eq!(main_head(), main_before, "{what}: nothing on main");
        let (code, out) = run_env(&wb, "b", 1, &["next"], env);
        assert_eq!(code, 0, "{what}: wt-b is fine: {out}");
        let (code, out) = run_env(&wa, "b", 1, &["next"], env);
        assert_ne!(code, 0, "{what}: never 'nobody holds anything': {out}");
        assert!(
            codes(&out).contains(&"coord.store_failed".to_string()),
            "{what}: {out}"
        );
    };

    // A `.git` file naming the common dir would commit onto main, pinned
    // by the common dir or by the worktree alone.
    fs::write(&link, format!("gitdir: {}\n", common.display())).expect("write");
    refused("gitdir to the common dir", &pin);
    let (code, out) = run_env(
        &wa,
        "a",
        1,
        &["done", "task-1"],
        &[("MDKG_TRUSTED_WORKTREE", wa.as_os_str())],
    );
    assert_ne!(code, 0, "gitdir to the common dir, worktree pinned: {out}");
    assert_eq!(main_head(), main_before, "nothing on main");
    fs::write(&link, &real).expect("restore");

    // A `.git` linked to another worktree's would commit onto its branch.
    fs::remove_file(&link).expect("rm");
    std::os::unix::fs::symlink(wb.join(".git"), &link).expect("symlink");
    refused("linked to wt-b's .git", &pin);
    refused("linked to wt-b's .git, unpinned", &[]);
    fs::remove_file(&link).expect("rm");
    fs::write(&link, &real).expect("restore");

    // Git reads `gitdir:  <path>` as a folder named " <path>" inside the
    // checkout: one built there, claiming main, is refused.
    let admin = common.join("worktrees/wt-a");
    let decoy = wa.join(format!(" {}", admin.display()));
    fs::create_dir_all(&decoy).expect("decoy");
    fs::write(decoy.join("HEAD"), "ref: refs/heads/main\n").expect("HEAD");
    fs::write(decoy.join("commondir"), format!("{}\n", common.display())).expect("commondir");
    fs::write(decoy.join("gitdir"), format!("{}\n", link.display())).expect("gitdir");
    fs::write(&link, format!("gitdir:  {}\n", admin.display())).expect("write");
    refused("gitdir with two spaces", &pin);
    fs::write(&link, &real).expect("restore");
    fs::remove_dir_all(wa.join(" ")).expect("rm decoy");

    // A plain `.git` file copied from wt-b: its git folder names wt-b back.
    fs::copy(wb.join(".git"), &link).expect("copy");
    refused("wt-b's .git file copied in", &pin);
    fs::write(&link, &real).expect("restore");

    // A HEAD rewritten to a branch another worktree has checked out.
    let head_file = admin.join("HEAD");
    let head = fs::read_to_string(&head_file).expect("HEAD");
    let b_before = sh(&repo, &["rev-parse", "b"]);
    for branch in ["b", "main"] {
        fs::write(&head_file, format!("ref: refs/heads/{branch}\n")).expect("HEAD");
        let (code, out) = run_env(&wa, "a", 1, &["done", "task-1"], &pin);
        assert_ne!(code, 0, "HEAD on {branch}: {out}");
    }
    assert_eq!(sh(&repo, &["rev-parse", "b"]), b_before, "nothing on b");
    assert_eq!(main_head(), main_before, "nothing on main");
    // Refused before staging: the index is as it was.
    assert_eq!(sh(&wa, &["diff", "--cached", "--name-only"]), "");
    // Other letter case reaches the same ref on a case-insensitive disk;
    // and the main checkout's branch counts, pinned or not.
    for (spelled, env) in [("MAIN", &pin[..]), ("B", &[][..]), ("main", &[][..])] {
        fs::write(&head_file, format!("ref: refs/heads/{spelled}\n")).expect("HEAD");
        let (code, out) = run_env(&wa, "a", 1, &["done", "task-1"], env);
        assert_ne!(code, 0, "HEAD on {spelled}: {out}");
    }
    assert_eq!(sh(&repo, &["rev-parse", "b"]), b_before, "nothing on b");
    assert_eq!(main_head(), main_before, "nothing on main");
    // main is read from the repository's own config, never from a
    // worktree's config.worktree.
    sh(&repo, &["config", "core.repositoryformatversion", "1"]);
    sh(&repo, &["config", "extensions.worktreeConfig", "true"]);
    sh(&wa, &["config", "--worktree", "mdkg.mainRef", "a"]);
    sh(&repo, &["checkout", "-q", "--detach"]);
    fs::write(&head_file, "ref: refs/heads/main\n").expect("HEAD");
    let (code, out) = run_env(&wa, "a", 1, &["done", "task-1"], &pin);
    assert_ne!(code, 0, "main renamed in config.worktree: {out}");
    assert_eq!(main_head(), main_before, "nothing on main");
    fs::write(&head_file, &head).expect("restore");
    sh(&repo, &["checkout", "-q", "main"]);
    sh(&wa, &["config", "--worktree", "--unset", "mdkg.mainRef"]);
    sh(&repo, &["config", "--unset", "extensions.worktreeConfig"]);
    // A branch another worktree is bisecting, or the main checkout is
    // rebasing, is held though that HEAD is detached (git's own guard).
    sh(&wb, &["bisect", "start"]);
    sh(&wb, &["checkout", "-q", "--detach"]);
    assert!(
        fs::read_to_string(common.join("worktrees/wt-b/HEAD"))
            .expect("HEAD")
            .len()
            > 30,
        "wt-b's HEAD is detached while it bisects"
    );
    fs::write(&head_file, "ref: refs/heads/b\n").expect("HEAD");
    let (code, out) = run_env(&wa, "a", 1, &["done", "task-1"], &pin);
    assert_ne!(code, 0, "b is being bisected: {out}");
    sh(&wb, &["bisect", "reset"]);
    sh(&repo, &["branch", "x", "main"]);
    for state in ["rebase-merge", "rebase-apply"] {
        let folder = common.join(state);
        fs::create_dir_all(&folder).expect("state");
        fs::write(folder.join("head-name"), "refs/heads/x\n").expect("head-name");
        fs::write(&head_file, "ref: refs/heads/x\n").expect("HEAD");
        let (code, out) = run_env(&wa, "a", 1, &["done", "task-1"], &pin);
        assert_ne!(code, 0, "x is being rebased ({state}): {out}");
        fs::remove_dir_all(&folder).expect("rm");
    }
    assert_eq!(sh(&repo, &["rev-parse", "b"]), b_before, "nothing on b");
    assert_eq!(sh(&repo, &["rev-parse", "x"]), main_before, "nothing on x");
    // A branch another worktree's `rebase --update-refs` will move: a ref,
    // then its old and new values.
    sh(&repo, &["branch", "stack-1", "main"]);
    let rebasing = common.join("worktrees/wt-b/rebase-merge");
    fs::create_dir_all(&rebasing).expect("state");
    fs::write(
        rebasing.join("update-refs"),
        format!("refs/heads/stack-1\n{main_before}\n{main_before}\n"),
    )
    .expect("update-refs");
    fs::write(&head_file, "ref: refs/heads/stack-1\n").expect("HEAD");
    let (code, out) = run_env(&wa, "a", 1, &["done", "task-1"], &pin);
    assert_ne!(code, 0, "stack-1 is in a rebase's update-refs: {out}");
    assert_eq!(
        sh(&repo, &["rev-parse", "stack-1"]),
        main_before,
        "nothing on stack-1"
    );
    // A name that isn't UTF-8 in that list, and a list over 1 MiB whose cap
    // splits a character: both refuse, never skip the list.
    let mut odd = b"refs/heads/caf\xe9\n".to_vec();
    odd.extend(
        format!("{main_before}\n{main_before}\nrefs/heads/stack-1\n{main_before}\n{main_before}\n")
            .bytes(),
    );
    fs::write(rebasing.join("update-refs"), odd).expect("update-refs");
    let (code, out) = run_env(&wa, "a", 1, &["done", "task-1"], &pin);
    assert_ne!(code, 0, "a non-UTF-8 name in update-refs: {out}");
    // ASCII records, then one whose `é` straddles byte 1 MiB exactly.
    let record = format!("refs/heads/s\n{main_before}\n{main_before}\n");
    let mut long = String::new();
    while long.len() + record.len() + 64 < (1 << 20) {
        long.push_str(&record);
    }
    let pad = (1 << 20) - 1 - long.len() - "refs/heads/".len();
    long.push_str(&format!(
        "refs/heads/{}\u{e9}\n{main_before}\n{main_before}\n",
        "a".repeat(pad)
    ));
    assert_eq!(
        long.as_bytes()[(1 << 20) - 1],
        0xc3,
        "é starts at the last byte"
    );
    long.push_str(&format!(
        "refs/heads/stack-1\n{main_before}\n{main_before}\n"
    ));
    fs::write(rebasing.join("update-refs"), long).expect("update-refs");
    let (code, out) = run_env(&wa, "a", 1, &["done", "task-1"], &pin);
    assert_ne!(code, 0, "an update-refs list over 1 MiB: {out}");
    assert_eq!(
        sh(&repo, &["rev-parse", "stack-1"]),
        main_before,
        "nothing on stack-1"
    );
    fs::remove_dir_all(&rebasing).expect("rm");
    // Another worktree's HEAD too long to read whole refuses, never is
    // skipped as if it held nothing.
    let wb_head = common.join("worktrees/wt-b/HEAD");
    let wb_saved = fs::read_to_string(&wb_head).expect("HEAD");
    fs::write(&wb_head, format!("ref: refs/heads/{}\n", "n/".repeat(2100))).expect("HEAD");
    let (code, out) = run_env(&wa, "a", 1, &["done", "task-1"], &pin);
    assert_ne!(code, 0, "a HEAD elsewhere too long to read: {out}");
    fs::write(&wb_head, wb_saved).expect("restore");
    fs::write(&head_file, &head).expect("restore");

    // A tag, pinned or not, is never moved.
    sh(&repo, &["tag", "v1", "main"]);
    fs::write(&head_file, "ref: refs/tags/v1\n").expect("HEAD");
    for env in [&pin[..], &[]] {
        let (code, out) = run_env(&wa, "a", 1, &["done", "task-1"], env);
        assert_ne!(code, 0, "HEAD on a tag: {out}");
    }
    assert_eq!(
        sh(&repo, &["rev-parse", "v1"]),
        main_before,
        "the tag stays"
    );
    // Main, from a linked worktree under the pin, even when nobody has it
    // checked out.
    sh(&repo, &["checkout", "-q", "--detach"]);
    fs::write(&head_file, "ref: refs/heads/main\n").expect("HEAD");
    let (code, out) = run_env(&wa, "a", 1, &["done", "task-1"], &pin);
    assert_ne!(code, 0, "HEAD on main, pinned: {out}");
    assert_eq!(main_head(), main_before, "nothing on main");
    fs::write(&head_file, head).expect("restore");
    sh(&repo, &["checkout", "-q", "main"]);

    // A worktree inside the main checkout whose `.git` is gone falls
    // through to the main checkout: refused.
    let nested = repo.join(".worktrees/wt-n");
    sh(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "n",
            nested.to_str().expect("utf-8"),
            "main",
        ],
    );
    fs::remove_file(nested.join(".git")).expect("rm");
    for args in [&["status"][..], &["sync"][..], &["claim", "task-2"][..]] {
        let (code, out) = run_env(&nested, "n", 1, args, &pin);
        assert_ne!(code, 0, "{args:?}: {out}");
        assert!(
            codes(&out).contains(&"coord.store_failed".to_string()),
            "{out}"
        );
    }
    assert_eq!(main_head(), main_before, "nothing on main");
    // Its record rewritten to point elsewhere, the worktree pin still
    // refuses it.
    let record = common.join("worktrees/wt-n/gitdir");
    fs::write(&record, "/nonexistent/x/.git\n").expect("gitdir");
    let pinned_n = [
        ("MDKG_TRUSTED_COMMON_DIR", common.as_os_str()),
        ("MDKG_TRUSTED_WORKTREE", nested.as_os_str()),
    ];
    for args in [&["status"][..], &["sync"][..]] {
        let (code, out) = run_env(&nested, "n", 1, args, &pinned_n);
        assert_ne!(code, 0, "{args:?}: {out}");
    }
    assert_eq!(main_head(), main_before, "nothing on main");
    // The genuine worktree, pinned the same way, works.
    let pinned_a = [
        ("MDKG_TRUSTED_COMMON_DIR", common.as_os_str()),
        ("MDKG_TRUSTED_WORKTREE", wa.as_os_str()),
    ];
    let (code, out) = run_env(&wa, "a", 1, &["status"], &pinned_a);
    assert_eq!(code, 0, "{out}");

    // A pin must be an absolute path.
    for bad in ["", "repo/.git"] {
        let (code, out) = run_env(
            &wa,
            "a",
            1,
            &["status"],
            &[("MDKG_TRUSTED_COMMON_DIR", OsStr::new(bad))],
        );
        assert_ne!(code, 0, "pin {bad:?}: {out}");
    }

    // Another repository's config can't lend it this one's checkout.
    let other = dir.0.join("other");
    fs::create_dir_all(&other).expect("other");
    sh(&other, &["init", "-q", "-b", "main"]);
    sh(
        &other,
        &["config", "core.worktree", repo.to_str().expect("utf-8")],
    );
    let (code, out) = run(&other, "a", 1, &["claim", "task-2"]);
    assert_ne!(code, 0, "{out}");
    assert!(!other.join(".git/mdkg").exists(), "no store lent");

    // Worktrees git links with relative paths (git 2.48+) are genuine.
    let rel = dir.0.join("wt-rel");
    let made = isolated(Command::new("git").arg("-C").arg(&repo).args([
        "worktree",
        "add",
        "-q",
        "--relative-paths",
        "-b",
        "rel",
        rel.to_str().expect("utf-8"),
        "main",
    ]))
    .output()
    .expect("git");
    if made.status.success() {
        let (code, out) = run_env(&rel, "r", 2, &["status"], &pin);
        assert_eq!(code, 0, "relative links: {out}");
    }

    // The genuine link still works, pinned.
    let (code, out) = run_env(&wa, "a", 3, &["done", "task-1"], &pin);
    assert_eq!(code, 0, "{out}");
}

/// Round 1 of b5bde24: a SHA-256 repository's checkout can't run its
/// filters either (the empty tree is in the repository's own format).
#[test]
fn a_sha256_checkout_cant_run_its_filters() {
    let dir = scratch();
    let repo = dir.0.join("repo");
    let conformance = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance");
    fs::create_dir_all(&repo).expect("repo");
    copy_tree(&conformance.join("base"), &repo);
    copy_tree(
        &conformance.join("cases/g01-valid-basic/.mdkg"),
        &repo.join(".mdkg"),
    );
    sh(
        &repo,
        &["init", "-q", "-b", "main", "--object-format=sha256"],
    );
    sh(&repo, &["add", "."]);
    sh(&repo, &["commit", "-q", "-m", "init"]);
    let wa = dir.0.join("wt-a");
    sh(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "a",
            wa.to_str().expect("utf-8"),
            "main",
        ],
    );
    let marker = dir.0.join("filter-ran");
    let command = format!("touch '{}'; cat", marker.display());
    sh(&repo, &["config", "filter.x.clean", &command]);
    sh(&repo, &["config", "filter.x.smudge", &command]);
    fs::write(wa.join(".gitattributes"), "* filter=x\n").expect("attributes");
    let t1 = wa.join(".mdkg/work/task-1-basic.md");
    let text = fs::read_to_string(&t1).expect("read") + "\nA note.\n";
    fs::write(&t1, text).expect("write");
    ok(&wa, "a", 0, &["claim", "task-1"]);
    ok(&wa, "a", 1, &["done", "task-1"]);
    assert!(!marker.exists(), "the checkout's filter never ran");
}

/// Review of 8d0596c: a FIFO planted where git keeps a small file can't
/// hang a command past git's own time limit.
#[test]
fn a_fifo_in_the_git_folder_cant_hang_a_command() {
    let (_dir, repo, wa, _wb) = fixture();
    for name in ["commondir", "gitdir"] {
        let file = repo.join(".git/worktrees/wt-a").join(name);
        let saved = fs::read(&file).expect("read");
        fs::remove_file(&file).expect("rm");
        assert!(
            Command::new("mkfifo")
                .arg(&file)
                .status()
                .expect("mkfifo")
                .success()
        );
        for args in [&["list"][..], &["status"][..]] {
            let mut child = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
                .arg("--root")
                .arg(&wa)
                .args(["--as", "a"])
                .args(args)
                .arg("--json")
                .env("MDKG_GIT_TIMEOUT_MS", "1500")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn");
            let deadline = Instant::now() + Duration::from_secs(30);
            let finished = loop {
                if child.try_wait().expect("wait").is_some() {
                    break true;
                }
                if Instant::now() > deadline {
                    child.kill().ok();
                    child.wait().ok();
                    break false;
                }
                std::thread::sleep(Duration::from_millis(50));
            };
            assert!(finished, "{args:?} with a FIFO as {name} hung");
        }
        fs::remove_file(&file).expect("rm");
        fs::write(&file, saved).expect("restore");
    }
}

/// Round 2 of 2595b4f: a git command cut short at the time limit gets TERM
/// first, so it removes its index lock.
#[test]
fn a_git_command_cut_short_leaves_no_lock() {
    let (_dir, repo, wa, _wb) = fixture();
    sh(&repo, &["config", "filter.slow.clean", "sleep 30; cat"]);
    fs::create_dir_all(repo.join(".git/info")).expect("info");
    fs::write(repo.join(".git/info/attributes"), "*.md filter=slow\n").expect("attributes");
    let t1 = wa.join(".mdkg/work/task-1-basic.md");
    let text = fs::read_to_string(&t1).expect("read") + "\nA note.\n";
    fs::write(&t1, text).expect("write");
    ok(&wa, "a", 0, &["claim", "task-1"]);
    let started = Instant::now();
    let (code, out) = run_env(
        &wa,
        "a",
        1,
        &["done", "task-1"],
        &[("MDKG_GIT_TIMEOUT_MS", OsStr::new("1500"))],
    );
    assert_ne!(code, 0, "{out}");
    assert!(started.elapsed() < Duration::from_secs(25), "bounded");
    assert!(
        !repo.join(".git/worktrees/wt-a/index.lock").exists(),
        "no lock left"
    );
}

/// Round 1 of 486f2ef (orchestrator 1(a)): a filter a checkout names never
/// runs, even when the repository's config defines it, and a worktree
/// whose `.git` is redirected, or whose config moves its top, is refused.
#[test]
fn a_checkout_cant_make_the_coordinator_run_its_programs() {
    let (dir, repo, wa, _wb) = fixture();
    let marker = dir.0.join("filter-ran");
    let script = dir.0.join("filter.sh");
    fs::write(
        &script,
        format!("#!/bin/sh\ntouch '{}'\ncat\n", marker.display()),
    )
    .expect("script");
    let mut perms = fs::metadata(&script).expect("meta").permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    fs::set_permissions(&script, perms).expect("chmod");
    sh(
        &repo,
        &["config", "filter.x.clean", script.to_str().expect("utf-8")],
    );
    sh(
        &repo,
        &["config", "filter.x.smudge", script.to_str().expect("utf-8")],
    );
    fs::write(wa.join(".gitattributes"), "* filter=x\n").expect("attributes");
    // Hooks config defines, which git 2.54 and later runs whatever
    // core.hooksPath says.
    for event in ["post-index-change", "pre-commit", "post-commit"] {
        sh(&repo, &["config", &format!("hook.h-{event}.event"), event]);
        sh(
            &repo,
            &[
                "config",
                &format!("hook.h-{event}.command"),
                script.to_str().expect("utf-8"),
            ],
        );
    }
    let t1 = wa.join(".mdkg/work/task-1-basic.md");
    let text = fs::read_to_string(&t1).expect("read") + "\nA note.\n";
    fs::write(&t1, text).expect("write");
    ok(&wa, "a", 0, &["claim", "task-1"]);
    ok(&wa, "a", 1, &["done", "task-1"]);
    assert!(!marker.exists(), "neither the filter nor a config hook ran");

    // A pinned common dir refuses a worktree redirected to another one.
    let decoy = dir.0.join("decoy");
    fs::create_dir_all(&decoy).expect("decoy");
    sh(&decoy, &["init", "-q", "-b", "main"]);
    let fake = decoy.join(".git");
    let real_link = fs::read_to_string(wa.join(".git")).expect("link");
    fs::write(wa.join(".git"), format!("gitdir: {}\n", fake.display())).expect("redirect");
    let out = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
        .arg("--root")
        .arg(&wa)
        .args(["--as", "a", "status", "--json"])
        .env("MDKG_TRUSTED_COMMON_DIR", repo.join(".git"))
        .output()
        .expect("run");
    assert_ne!(out.status.code(), Some(0), "redirected");
    assert!(!fake.join("mdkg").exists(), "no store in the fake folder");
    fs::write(wa.join(".git"), real_link).expect("restore");

    // The repository's config can't move a worktree's top elsewhere.
    sh(
        &repo,
        &["config", "core.worktree", dir.0.to_str().expect("utf-8")],
    );
    let (code, out) = run(&repo, "a", 2, &["status"]);
    assert_ne!(code, 0, "{out}");
}

/// Round 4 of the git chain (review of ade12dc): a FIFO planted in one
/// worktree's git folder can't stall or fail another worktree's commands.
#[test]
fn one_worktree_cant_stall_another() {
    let (_dir, repo, wa, _wb) = fixture();
    let admin = repo.join(".git/worktrees/wt-b");
    for name in ["gitdir", "HEAD", "locked"] {
        let file = admin.join(name);
        let saved = fs::read(&file).ok();
        fs::remove_file(&file).ok();
        assert!(
            Command::new("mkfifo")
                .arg(&file)
                .status()
                .expect("mkfifo")
                .success()
        );
        // A status, and a claim that looks for the path in every worktree.
        for args in [&["status"][..], &["claim", "--new", "nothing-here.rs"][..]] {
            let started = Instant::now();
            let (code, out) = run_env(
                &wa,
                "a",
                0,
                args,
                &[("MDKG_GIT_TIMEOUT_MS", OsStr::new("3000"))],
            );
            assert_eq!(code, 0, "{name} {args:?}: {out}");
            assert!(
                started.elapsed() < Duration::from_secs(6),
                "{name} {args:?}: stalled"
            );
        }
        ok(&wa, "a", 0, &["release", "./nothing-here.rs"]);
        fs::remove_file(&file).expect("rm");
        if let Some(bytes) = saved {
            fs::write(&file, bytes).expect("restore");
        }
    }
}

/// Round 5 of the git chain (review of 48920a5): a reftable repository
/// keeps every HEAD file as a stub, so whose branch is checked out where
/// can't be read from them: mdkg doesn't write there yet.
#[test]
fn a_reftable_repository_is_not_written_to() {
    let dir = scratch();
    let repo = dir.0.join("repo");
    let conformance = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance");
    fs::create_dir_all(&repo).expect("repo");
    copy_tree(&conformance.join("base"), &repo);
    copy_tree(
        &conformance.join("cases/g01-valid-basic/.mdkg"),
        &repo.join(".mdkg"),
    );
    let made = isolated(Command::new("git").arg("-C").arg(&repo).args([
        "init",
        "-q",
        "-b",
        "main",
        "--ref-format=reftable",
    ]))
    .output()
    .expect("git");
    if !made.status.success() {
        return; // git before 2.45
    }
    sh(&repo, &["add", "."]);
    sh(&repo, &["commit", "-q", "-m", "init"]);
    let wa = dir.0.join("wt-a");
    sh(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "a",
            wa.to_str().expect("utf-8"),
            "main",
        ],
    );
    let t1 = wa.join(".mdkg/work/task-1-basic.md");
    let text = fs::read_to_string(&t1).expect("read") + "\nA note.\n";
    fs::write(&t1, text).expect("write");
    ok(&wa, "a", 0, &["claim", "task-1"]);
    let before = sh(&repo, &["rev-parse", "a"]);
    let (code, out) = run(&wa, "a", 1, &["done", "task-1"]);
    assert_ne!(code, 0, "{out}");
    assert_eq!(sh(&repo, &["rev-parse", "a"]), before);
}

/// Review of 344ab92 (orchestrator, 2026-10-21): a branch git lists
/// nowhere is refused, a branch with no commits yet included, because a
/// look-alike name (APFS folds Unicode case and normalization) writes
/// through to another branch. Each probe must stay refused.
#[test]
fn branch_look_alikes_never_write_through() {
    let (dir, repo, wa, _wb) = fixture();
    let common = repo.join(".git");
    let pin = [("MDKG_TRUSTED_COMMON_DIR", common.as_os_str())];
    sh(&repo, &["branch", "master", "main"]);
    sh(&repo, &["branch", "caf\u{e9}", "main"]);
    let wc = dir.0.join("wt-c");
    sh(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "task-9",
            wc.to_str().expect("utf-8"),
            "main",
        ],
    );
    let t1 = wa.join(".mdkg/work/task-1-basic.md");
    let text = fs::read_to_string(&t1).expect("read") + "\nA note.\n";
    fs::write(&t1, text).expect("write");
    ok(&wa, "a", 0, &["claim", "task-1"]);
    let head_file = common.join("worktrees/wt-a/HEAD");
    let head = fs::read_to_string(&head_file).expect("HEAD");
    let tips = |names: &[&str]| -> Vec<String> {
        names.iter().map(|n| sh(&repo, &["rev-parse", n])).collect()
    };
    let names = ["task-9", "master", "caf\u{e9}", "main"];
    let before = tips(&names);
    // The long s, the Kelvin sign, and `é` written decomposed (NFD).
    for probe in [
        "ta\u{17f}k-9",
        "ma\u{17f}ter",
        "tas\u{212a}-9",
        "cafe\u{301}",
    ] {
        fs::write(&head_file, format!("ref: refs/heads/{probe}\n")).expect("HEAD");
        for env in [&pin[..], &[]] {
            let (code, out) = run_env(&wa, "a", 1, &["done", "task-1"], env);
            assert_ne!(code, 0, "{probe}: {out}");
        }
        sh(&repo, &["pack-refs", "--all"]);
    }
    assert_eq!(tips(&names), before, "no branch moved");
    // A branch with no commits yet says what to do.
    fs::write(&head_file, "ref: refs/heads/fresh-one\n").expect("HEAD");
    let (code, out) = run(&wa, "a", 1, &["done", "task-1"]);
    assert_ne!(code, 0, "{out}");
    assert!(
        out["diagnostics"][0]["message"]
            .as_str()
            .is_some_and(|m| m.contains("needs a commit first")),
        "{out}"
    );
    fs::write(&head_file, head).expect("restore");
}

/// Review of 6b7c10a (orchestrator, 2026-10-21): a look-alike git does
/// list (packed) is refused too, as is any non-ASCII branch on either side
/// of a comparison, and a branch that differs from another only in letter
/// case where the disk may fold it, whatever core.ignorecase says.
#[test]
fn packed_look_alikes_and_case_collisions_are_refused() {
    let (dir, repo, wa, _wb) = fixture();
    let common = repo.join(".git");
    let t1 = wa.join(".mdkg/work/task-1-basic.md");
    let text = fs::read_to_string(&t1).expect("read") + "\nA note.\n";
    fs::write(&t1, text).expect("write");
    ok(&wa, "a", 0, &["claim", "task-1"]);
    let head_file = common.join("worktrees/wt-a/HEAD");
    let head = fs::read_to_string(&head_file).expect("HEAD");
    let refused = |env: &[(&str, &OsStr)], why: &str| {
        let (code, out) = run_env(&wa, "a", 1, &["done", "task-1"], env);
        assert_ne!(code, 0, "{out}");
        assert!(
            out["diagnostics"][0]["message"]
                .as_str()
                .is_some_and(|m| m.contains(why)),
            "{why}: {out}"
        );
    };

    // (a) The look-alike packed, the real branch checked out elsewhere.
    sh(&repo, &["branch", "ta\u{17f}k-9", "main"]);
    sh(&repo, &["pack-refs", "--all"]);
    let wc = dir.0.join("wt-c");
    sh(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "task-9",
            wc.to_str().expect("utf-8"),
            "main",
        ],
    );
    let task9 = sh(&repo, &["rev-parse", "task-9"]);
    fs::write(&head_file, "ref: refs/heads/ta\u{17f}k-9\n").expect("HEAD");
    refused(&[], "must be ASCII");

    // (b) main named with a look-alike, HEAD on the ASCII branch it folds
    // onto, both packed, pinned: main's own name must be ASCII.
    sh(&repo, &["branch", "ma\u{17f}ter", "main"]);
    sh(&repo, &["pack-refs", "--all"]);
    sh(&repo, &["branch", "master", "main"]);
    sh(&repo, &["pack-refs", "--all"]);
    let look_alike = sh(&repo, &["rev-parse", "ma\u{17f}ter"]);
    fs::write(&head_file, "ref: refs/heads/master\n").expect("HEAD");
    refused(
        &[
            ("MDKG_TRUSTED_COMMON_DIR", common.as_os_str()),
            ("MDKG_MAIN_REF", OsStr::new("ma\u{17f}ter")),
        ],
        "must be ASCII",
    );
    assert_eq!(sh(&repo, &["rev-parse", "task-9"]), task9, "task-9 stays");
    assert_eq!(
        sh(&repo, &["rev-parse", "ma\u{17f}ter"]),
        look_alike,
        "main stays"
    );

    // (c) Another worktree holds the look-alike, HEAD is on the ASCII
    // branch it folds onto: the held name must be ASCII, and says whose.
    sh(&repo, &["branch", "ta\u{17f}k-q", "main"]);
    sh(&repo, &["pack-refs", "--all"]);
    sh(&repo, &["branch", "task-q", "main"]);
    sh(&repo, &["pack-refs", "--all"]);
    let wd = dir.0.join("wt-d");
    sh(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            wd.to_str().expect("utf-8"),
            "ta\u{17f}k-q",
        ],
    );
    let held = sh(&repo, &["rev-parse", "ta\u{17f}k-q"]);
    fs::write(&head_file, "ref: refs/heads/task-q\n").expect("HEAD");
    refused(&[], "held by the worktree wt-d");
    assert_eq!(
        sh(&repo, &["rev-parse", "ta\u{17f}k-q"]),
        held,
        "wt-d's branch stays"
    );

    // Letter case alone, with core.ignorecase off: on macOS and Windows the
    // disk folds anyway, so `feature` beside a packed `Feature` is refused.
    if cfg!(any(target_vendor = "apple", target_os = "windows")) {
        sh(&repo, &["config", "core.ignorecase", "false"]);
        sh(&repo, &["branch", "Feature", "main"]);
        sh(&repo, &["pack-refs", "--all"]);
        sh(&repo, &["branch", "feature", "main"]);
        let feature = sh(&repo, &["rev-parse", "Feature"]);
        fs::write(&head_file, "ref: refs/heads/feature\n").expect("HEAD");
        refused(&[], "collides with refs/heads/Feature");
        assert_eq!(sh(&repo, &["rev-parse", "Feature"]), feature);
    }
    fs::write(&head_file, head).expect("restore");
}

#[test]
fn update_names_an_ambiguous_id_s_candidates_and_reports_progress() {
    let dir = scratch();
    let repo = dir.0.join("repo");
    let conformance = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance");
    fs::create_dir_all(&repo).expect("repo");
    copy_tree(&conformance.join("base"), &repo);
    copy_tree(&conformance.join("cases/s02-workspaces"), &repo);
    fs::remove_file(repo.join("commands")).ok();
    sh(&repo, &["init", "-q", "-b", "main"]);
    sh(&repo, &["add", "."]);
    sh(&repo, &["commit", "-q", "-m", "init"]);
    // task-1 lives in several workspaces: the hint names them (from the
    // graph), and nothing typed is quoted.
    // Typed in upper case (ids match case-insensitively), so an echo of
    // what was typed would show.
    let (code, out) = run(&repo, "a", 1, &["update", "TASK-1", "--status", "review"]);
    assert_eq!(
        (code, codes(&out)),
        (7, vec!["node.ambiguous".to_string()]),
        "{out}"
    );
    assert!(!out.to_string().contains("TASK-1"), "{out}");
    // The whole diagnostic, so even a lowercased echo would show. The
    // hint names every enabled workspace's task-1, in byte order, and not
    // the disabled one.
    assert_eq!(
        out["diagnostics"],
        serde_json::json!([{
            "code": "node.ambiguous",
            "severity": "error",
            "message": "that handle names 5 nodes",
            "hint": "matches: a2:task-1  Letter-digit workspace; a_b:task-1  Underscore workspace; ab2:task-1  Digit workspace; ab:task-1  Plain workspace; root:task-1  Root task; give more of the id, or the full id",
        }]),
        "{out}"
    );
    // Clearing a state while you hold the node's lease leaves `progress`.
    ok(&repo, "a", 2, &["claim", "root:task-1"]);
    ok(
        &repo,
        "a",
        3,
        &["update", "root:task-1", "--status", "blocked"],
    );
    let cleared = ok(&repo, "a", 4, &["update", "root:task-1", "--clear"]);
    assert_eq!(cleared["data"]["status"], "progress", "{cleared}");
}

/// The g01 fixture (format 0.6) as a committed repository at `dir/repo`,
/// beside a folder `dir/outside` a link in it may point at.
fn linkable() -> (Scratch, PathBuf, PathBuf) {
    let dir = scratch();
    let repo = dir.0.join("repo");
    let outside = dir.0.join("outside");
    let conformance = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance");
    fs::create_dir_all(&repo).expect("repo");
    fs::create_dir_all(&outside).expect("outside");
    copy_tree(&conformance.join("base"), &repo);
    copy_tree(
        &conformance.join("cases/g01-valid-basic/.mdkg"),
        &repo.join(".mdkg"),
    );
    (dir, repo, outside)
}

fn commit_all(repo: &Path) {
    sh(repo, &["init", "-q", "-b", "main"]);
    sh(repo, &["add", "."]);
    sh(repo, &["commit", "-q", "-m", "init"]);
}

#[test]
fn nothing_is_written_through_a_linked_graph_folder() {
    // RC1 SEC3: a cloned repository can ship `.mdkg/work` as a link out of
    // itself; `new` and `format --write` refuse to write through it.
    let (_dir, repo, outside) = linkable();
    fs::rename(repo.join(".mdkg/work"), outside.join("work")).expect("move");
    std::os::unix::fs::symlink("../../outside/work", repo.join(".mdkg/work")).expect("link");
    commit_all(&repo);
    let before: Vec<_> = fs::read_dir(outside.join("work"))
        .expect("outside")
        .map(|e| e.expect("entry").file_name())
        .collect();
    let (code, out) = run(&repo, "a", 0, &["new", "task", "escaped"]);
    assert_ne!(code, 0, "{out}");
    assert_eq!(codes(&out), ["io.write_failed"], "{out}");
    // format --write would rewrite a node out there.
    let file = outside.join("work/task-2-other.md");
    let text = fs::read_to_string(&file).expect("read");
    let shuffled = text.replace(
        "title: Other task\nstatus: backlog",
        "status: backlog\ntitle: Other task",
    );
    fs::write(&file, &shuffled).expect("write");
    let (code, out) = run(&repo, "a", 1, &["format", "--write"]);
    assert_ne!(code, 0, "{out}");
    assert_eq!(fs::read_to_string(&file).expect("read"), shuffled);
    let after: Vec<_> = fs::read_dir(outside.join("work"))
        .expect("outside")
        .map(|e| e.expect("entry").file_name())
        .collect();
    assert_eq!(after, before, "nothing new outside the repository");
}

#[test]
fn a_new_v07_node_isnt_written_through_a_linked_folder() {
    // RC1 SEC3, format 0.7: the same for create_v07.
    let dir = scratch();
    let repo = dir.0.join("repo");
    let outside = dir.0.join("outside");
    fs::create_dir_all(&repo).expect("repo");
    fs::create_dir_all(outside.join("work")).expect("outside");
    fs::write(repo.join("README.md"), "x\n").expect("readme");
    sh(&repo, &["init", "-q", "-b", "main"]);
    ok(&repo, "a", 0, &["init", "--work", "shared"]);
    fs::remove_dir_all(repo.join(".mdkg/work")).ok();
    std::os::unix::fs::symlink("../../outside/work", repo.join(".mdkg/work")).expect("link");
    sh(&repo, &["add", "."]);
    sh(&repo, &["commit", "-q", "-m", "init"]);
    let (code, out) = run(&repo, "a", 1, &["new", "task", "escaped"]);
    assert_ne!(code, 0, "{out}");
    assert_eq!(codes(&out), ["io.write_failed"], "{out}");
    assert_eq!(
        fs::read_dir(outside.join("work")).expect("outside").count(),
        0
    );
}

#[test]
fn a_dangling_link_at_the_next_name_is_never_followed() {
    // RC1 SEC8: in a 0.6 graph the next node's name is predictable; a link
    // there (uncommitted: a committed name already raises the next alias)
    // pointing out of the repository must not be written through.
    let (_dir, repo, outside) = linkable();
    commit_all(&repo);
    let target = outside.join("target.txt");
    std::os::unix::fs::symlink(&target, repo.join(".mdkg/work/task-3-escaped.md")).expect("link");
    let (code, out) = run(&repo, "a", 0, &["new", "task", "escaped"]);
    assert_ne!(code, 0, "{out}");
    assert!(!target.exists(), "the link's target was never created");
}

#[test]
fn a_path_that_also_names_a_06_node_still_warns() {
    // RC1 F3 made this a note in a 0.7 graph; a 0.6 graph's short ids are
    // easy to mistake, so it stays a warning there.
    let (_dir, repo, _) = linkable();
    fs::write(repo.join("basic"), "x\n").expect("file");
    commit_all(&repo);
    let out = ok(&repo, "a", 0, &["claim", "basic"]);
    assert_eq!(out["data"]["lease"]["scope"], "basic", "{out}");
    let note = out["diagnostics"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|d| d["code"] == "node.ambiguous")
        .cloned()
        .unwrap_or_default();
    assert_eq!(note["severity"], "warning", "{out}");
}

#[test]
fn an_anchor_isnt_written_through_a_linked_folder() {
    // RC1 SEC3: `.mdkg/anchors` as a link out of the repository.
    let dir = scratch();
    let repo = dir.0.join("repo");
    let outside = dir.0.join("outside");
    fs::create_dir_all(&repo).expect("repo");
    fs::create_dir_all(outside.join("anchors")).expect("outside");
    fs::write(repo.join("README.md"), "x\n").expect("readme");
    sh(&repo, &["init", "-q", "-b", "main"]);
    ok(&repo, "a", 0, &["init", "--work", "shared"]);
    sh(&repo, &["add", "."]);
    sh(&repo, &["commit", "-q", "-m", "init"]);
    ok(&repo, "a", 1, &["claim", "README.md"]);
    std::os::unix::fs::symlink("../../outside/anchors", repo.join(".mdkg/anchors")).expect("link");
    let (code, out) = run(&repo, "a", 2, &["anchor"]);
    assert_ne!(code, 0, "{out}");
    assert_eq!(codes(&out), ["io.write_failed"], "{out}");
    assert_eq!(
        fs::read_dir(outside.join("anchors"))
            .expect("outside")
            .count(),
        0
    );
}

#[test]
fn done_never_touches_a_node_through_a_linked_folder() {
    // RC1 SEC3: discovery reads a node through a linked `.mdkg/work`;
    // `done` refuses it before writing, or even putting back, anything
    // there (its content alone would hide a write and an undo).
    let (_dir, repo, outside) = linkable();
    fs::rename(repo.join(".mdkg/work"), outside.join("work")).expect("move");
    std::os::unix::fs::symlink("../../outside/work", repo.join(".mdkg/work")).expect("link");
    commit_all(&repo);
    let file = outside.join("work/task-2-other.md");
    let then = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
    fs::File::options()
        .write(true)
        .open(&file)
        .expect("open")
        .set_modified(then)
        .expect("mtime");
    ok(&repo, "a", 0, &["claim", "task-2"]);
    let (code, out) = run(&repo, "a", 1, &["done", "task-2"]);
    assert_ne!(code, 0, "{out}");
    assert_eq!(codes(&out), ["io.write_failed"], "{out}");
    assert_eq!(
        fs::metadata(&file)
            .expect("meta")
            .modified()
            .expect("mtime"),
        then,
        "the file out there was never written"
    );
}

#[test]
fn a_refused_done_leaves_no_checkpoint() {
    // Codex on 0810f2e: a node under a linked `.mdkg/design`, a real
    // `.mdkg/work`. `done --checkpoint` is refused, and the checkpoint it
    // made for the unrecorded work goes too.
    let dir = scratch();
    let repo = dir.0.join("repo");
    let outside = dir.0.join("outside");
    fs::create_dir_all(&repo).expect("repo");
    fs::create_dir_all(outside.join("design")).expect("outside");
    fs::write(repo.join("README.md"), "x\n").expect("readme");
    sh(&repo, &["init", "-q", "-b", "main"]);
    ok(&repo, "a", 0, &["init", "--work", "shared"]);
    sh(&repo, &["add", "."]);
    sh(&repo, &["commit", "-q", "-m", "init"]);
    let made = ok(&repo, "a", 1, &["new", "task", "Linked"]);
    let id = made["data"]["node"]["id"].as_str().expect("id").to_string();
    let file = repo.join(made["data"]["node"]["path"].as_str().expect("path"));
    fs::rename(&file, outside.join("design/task-linked.md")).expect("move");
    fs::remove_dir_all(repo.join(".mdkg/design")).ok();
    std::os::unix::fs::symlink("../../outside/design", repo.join(".mdkg/design")).expect("link");
    let work_before: Vec<_> = fs::read_dir(repo.join(".mdkg/work"))
        .expect("work")
        .map(|e| e.expect("entry").file_name())
        .collect();
    ok(&repo, "a", 2, &["claim", &id]);
    let (code, out) = run(&repo, "a", 3, &["done", &id, "--checkpoint"]);
    assert_ne!(code, 0, "{out}");
    assert_eq!(codes(&out), ["io.write_failed"], "{out}");
    let work_after: Vec<_> = fs::read_dir(repo.join(".mdkg/work"))
        .expect("work")
        .map(|e| e.expect("entry").file_name())
        .collect();
    assert_eq!(work_after, work_before, "no orphan checkpoint");
}
