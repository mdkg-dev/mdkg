//! The 0.6.0 spellings (task-40): each runs its 0.7.0 equivalent, in that
//! command's envelope, with a `usage.deprecated` notice; options with no
//! 0.7.0 meaning are named, and field edits no 0.7.0 command makes are
//! refused rather than dropped.

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
    let dir = common::temp::dir("mdkg-cli-compat");
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

fn git(dir: &Path, args: &[&str]) {
    let out = isolated(Command::new("git").arg("-C").arg(dir).args(args))
        .output()
        .expect("git");
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

const GOAL: &str = "---
id: goal-1
type: goal
title: Ship
status: todo
goal_state: active
goal_condition: both tasks done
scope_refs: [task-1, task-2]
created: 2026-10-18
updated: 2026-10-18
---
";

/// A worktree on branch `a` of a repository holding g01's two tasks and a
/// goal over them.
fn worktree(dir: &Scratch) -> PathBuf {
    let repo = dir.0.join("repo");
    let conformance = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance");
    fs::create_dir_all(&repo).expect("repo");
    copy_tree(&conformance.join("base"), &repo);
    copy_tree(
        &conformance.join("cases/g01-valid-basic/.mdkg"),
        &repo.join(".mdkg"),
    );
    fs::write(repo.join(".mdkg/work/goal-1-ship.md"), GOAL).expect("goal");
    // Outside the goal's scope.
    fs::write(
        repo.join(".mdkg/work/task-3-aside.md"),
        "---\nid: task-3\ntype: task\ntitle: Aside\nstatus: todo\ncreated: 2026-10-18\nupdated: 2026-10-18\n---\n",
    )
    .expect("task-3");
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    let wt = dir.0.join("wt-a");
    git(
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
    wt
}

/// Runs mdkg as `a`; returns the exit code, the envelope (with
/// `--json`) and stderr.
fn run(wt: &Path, now: u64, args: &[&str]) -> (i32, Value, String) {
    let out = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
        .arg("--root")
        .arg(wt)
        .args(["--as", "a"])
        .args(args)
        .env("MDKG_NOW_MS", now.to_string())
        .output()
        .expect("run mdkg");
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let envelope: Value = if args.contains(&"--json") {
        let v: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|_| {
            panic!(
                "{args:?}: not JSON: {}\n{stderr}",
                String::from_utf8_lossy(&out.stdout)
            )
        });
        common::conforms_envelope(&v);
        v
    } else {
        Value::Null
    };
    (out.status.code().unwrap_or(-1), envelope, stderr)
}

fn deprecation(envelope: &Value) -> String {
    envelope["diagnostics"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|d| d["code"] == "usage.deprecated")
        .and_then(|d| d["message"].as_str())
        .unwrap_or_else(|| panic!("no usage.deprecated notice: {envelope}"))
        .to_string()
}

fn codes(envelope: &Value) -> Vec<String> {
    envelope["diagnostics"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|d| d["code"].as_str().map(String::from))
        .collect()
}

#[test]
fn task_start_update_and_done_run_their_new_commands() {
    let dir = scratch();
    let wt = worktree(&dir);

    let (code, out, _) = run(
        &wt,
        1,
        &[
            "task", "start", "task-1", "--run-id", "r1", "--note", "go", "--json",
        ],
    );
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["schema"], "mdkg.claim.v1");
    let notice = deprecation(&out);
    assert!(
        notice.contains("`mdkg task start` is now `mdkg claim`"),
        "{notice}"
    );
    assert!(notice.contains("--run-id, --note"), "{notice}");

    let (code, out, _) = run(
        &wt,
        2,
        &["task", "update", "task-1", "--status", "review", "--json"],
    );
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["schema"], "mdkg.update.v1");
    assert!(deprecation(&out).contains("is now `mdkg update`"));

    // `--status progress` is a claim; `done` finishes.
    let (code, out, _) = run(
        &wt,
        3,
        &["task", "update", "task-2", "--status", "progress", "--json"],
    );
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["schema"], "mdkg.claim.v1");

    // Field edits are refused, and change nothing.
    let before = fs::read_to_string(wt.join(".mdkg/work/task-1-basic.md")).expect("read");
    let (code, out, _) = run(
        &wt,
        4,
        &[
            "task",
            "update",
            "task-1",
            "--priority",
            "2",
            "--add-tags",
            "x",
            "--json",
        ],
    );
    assert_eq!(code, 1, "{out}");
    assert_eq!(out["schema"], "mdkg.update.v1");
    assert_eq!(codes(&out), ["usage.bad_value"]);
    assert!(
        out["diagnostics"][0]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("--priority, --add-tags")
    );
    assert_eq!(
        fs::read_to_string(wt.join(".mdkg/work/task-1-basic.md")).expect("read"),
        before
    );
    let (code, out, _) = run(
        &wt,
        5,
        &["task", "update", "task-1", "--status", "todo", "--json"],
    );
    assert_eq!(
        (code, codes(&out)),
        (1, vec!["usage.bad_status".to_string()])
    );

    // 0.6.0's `--checkpoint <title>`: the checkpoint is made, the title named.
    let (code, out, _) = run(
        &wt,
        6,
        &[
            "task",
            "done",
            "task-1",
            "--checkpoint",
            "Parser works",
            "--json",
        ],
    );
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["schema"], "mdkg.done.v1");
    assert!(out["data"]["checkpoint"].is_string(), "{out}");
    assert!(deprecation(&out).contains("the --checkpoint title"));
    let (code, out, _) = run(
        &wt,
        7,
        &["task", "done", "task-2", "--add-refs", "task-1", "--json"],
    );
    assert_eq!(
        (code, codes(&out)),
        (1, vec!["usage.bad_value".to_string()])
    );
}

#[test]
fn checkpoint_new_and_goal_claim_run_their_new_commands() {
    let dir = scratch();
    let wt = worktree(&dir);
    let (code, out, _) = run(
        &wt,
        1,
        &[
            "checkpoint",
            "new",
            "Lexer",
            "notes",
            "--kind",
            "audit",
            "--run-id",
            "r",
            "--json",
        ],
    );
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["schema"], "mdkg.new.v1");
    assert_eq!(out["data"]["node"]["type"], "checkpoint");
    assert!(deprecation(&out).contains("is now `mdkg new checkpoint`"));

    // A work item: claimed.
    let (code, out, _) = run(&wt, 2, &["goal", "claim", "task-2", "--json"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["schema"], "mdkg.claim.v1");
    assert!(
        out["data"]["lease"]["scope"]
            .as_str()
            .unwrap_or_default()
            .contains("task-2"),
        "{out}"
    );
    assert!(deprecation(&out).contains("`mdkg goal claim <work>` is now `mdkg claim <work>`"));
    // A goal and a work item: the work item.
    let (code, out, _) = run(&wt, 3, &["goal", "claim", "goal-1", "task-1", "--json"]);
    assert_eq!(code, 0, "{out}");
    assert!(
        out["data"]["lease"]["scope"]
            .as_str()
            .unwrap_or_default()
            .contains("task-1"),
        "{out}"
    );
}

#[test]
fn text_output_prints_the_notice_on_stderr() {
    let dir = scratch();
    let wt = worktree(&dir);
    let (code, _, stderr) = run(&wt, 1, &["task", "start", "task-1"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains("usage.deprecated"), "{stderr}");
    assert!(stderr.contains("run `mdkg claim` instead"), "{stderr}");
}

fn leases(wt: &Path) -> Vec<String> {
    let (_, out, _) = run(wt, 99, &["status", "--json"]);
    out["data"]["leases"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|l| l["scope"].as_str().map(String::from))
        .collect()
}

#[test]
fn ws_selects_the_node_and_checkpoint_new_refuses_it() {
    let dir = scratch();
    let wt = worktree(&dir);
    // An unknown workspace: refused, and nothing is done.
    let (code, out, _) = run(
        &wt,
        1,
        &["task", "done", "task-1", "--ws", "nosuch", "--json"],
    );
    assert_ne!(code, 0, "{out}");
    assert_eq!(out["schema"], "mdkg.done.v1");
    assert!(
        !fs::read_to_string(wt.join(".mdkg/work/task-1-basic.md"))
            .expect("read")
            .contains("status: done")
    );
    // The graph's own workspace: it runs.
    let (code, out, _) = run(
        &wt,
        2,
        &["task", "start", "task-1", "--ws", "root", "--json"],
    );
    assert_eq!(code, 0, "{out}");
    let (code, out, _) = run(
        &wt,
        3,
        &["checkpoint", "new", "Notes", "--ws", "root", "--json"],
    );
    assert_eq!(
        (code, codes(&out)),
        (1, vec!["usage.bad_value".to_string()]),
        "{out}"
    );
    assert_eq!(out["schema"], "mdkg.new.v1");
}

#[test]
fn goal_claim_checks_the_named_goal() {
    let dir = scratch();
    let wt = worktree(&dir);
    // Work outside the goal, and a goal that doesn't exist: refused, no lease.
    let (code, out, _) = run(&wt, 1, &["goal", "claim", "goal-1", "task-3", "--json"]);
    assert_eq!(
        (code, codes(&out)),
        (1, vec!["usage.bad_value".to_string()]),
        "{out}"
    );
    assert_eq!(out["schema"], "mdkg.claim.v1");
    let (code, out, _) = run(&wt, 2, &["goal", "claim", "goal-9", "task-1", "--json"]);
    assert_ne!(code, 0, "{out}");
    assert!(leases(&wt).is_empty(), "{:?}", leases(&wt));
    // Any case names the goal: its next item is claimed, not the goal.
    let (code, out, _) = run(&wt, 3, &["goal", "claim", "GOAL-1", "--json"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["schema"], "mdkg.goal.claim.v1");
    assert!(
        leases(&wt).iter().all(|l| !l.contains("goal-1")),
        "{:?}",
        leases(&wt)
    );
}

#[test]
fn early_failures_name_the_new_commands_schema() {
    let dir = scratch();
    let wt = worktree(&dir);
    let (code, out, _) = run(
        &wt,
        1,
        &[
            "task", "update", "task-1", "--status", "done", "--bogus", "--json",
        ],
    );
    assert_eq!(code, 1);
    assert_eq!(out["schema"], "mdkg.done.v1");
    let (code, out, _) = run(&wt, 1, &["goal", "claim", "task-1", "--bogus", "--json"]);
    assert_eq!(code, 1);
    assert_eq!(out["schema"], "mdkg.claim.v1");
    // A missing config, after parsing.
    fs::remove_file(wt.join(".mdkg/config.json")).expect("rm");
    let (code, out, _) = run(&wt, 1, &["goal", "claim", "task-1", "--json"]);
    assert_eq!(code, 1);
    assert_eq!(out["schema"], "mdkg.claim.v1");
}

#[test]
fn a_ws_that_names_a_role_or_a_turn_takes_no_lease() {
    let dir = scratch();
    let wt = worktree(&dir);
    for ws in ["run", "role"] {
        let (code, out, _) = run(&wt, 1, &["task", "start", "task-1", "--ws", ws, "--json"]);
        assert_ne!(code, 0, "{ws}: {out}");
        assert_eq!(out["schema"], "mdkg.claim.v1");
        let (code, out, _) = run(
            &wt,
            2,
            &[
                "task", "update", "task-1", "--status", "progress", "--ws", ws, "--json",
            ],
        );
        assert_ne!(code, 0, "{ws}: {out}");
    }
    assert!(leases(&wt).is_empty(), "{:?}", leases(&wt));
}

#[test]
fn attached_global_options_still_name_the_new_schema() {
    let dir = scratch();
    let wt = worktree(&dir);
    let out = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
        .arg(format!("--root={}", wt.display()))
        .args(["--as=a", "task", "start", "task-1", "--bogus", "--json"])
        .output()
        .expect("run");
    let v: Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(v["schema"], "mdkg.claim.v1", "{v}");
}

#[test]
fn goal_claim_finds_the_work_in_the_goals_workspace() {
    let dir = scratch();
    let wt = worktree(&dir);
    // A second workspace, `app`, with its own task-1 and a goal over it.
    let config_path = wt.join(".mdkg/config.json");
    let mut config: Value =
        serde_json::from_str(&fs::read_to_string(&config_path).expect("config")).expect("json");
    config["workspaces"]["app"] =
        serde_json::json!({ "path": "app", "enabled": true, "mdkg_dir": ".mdkg" });
    fs::write(
        &config_path,
        serde_json::to_string_pretty(&config).expect("json"),
    )
    .expect("config");
    fs::create_dir_all(wt.join("app/.mdkg/work")).expect("app");
    fs::write(
        wt.join("app/.mdkg/work/task-1-app.md"),
        "---\nid: task-1\ntype: task\ntitle: App task\nstatus: todo\ncreated: 2026-10-18\nupdated: 2026-10-18\n---\n",
    )
    .expect("task");
    fs::write(
        wt.join("app/.mdkg/work/goal-1-app.md"),
        GOAL.replace("scope_refs: [task-1, task-2]", "scope_refs: [task-1]"),
    )
    .expect("goal");
    git(&wt, &["add", "."]);
    git(&wt, &["commit", "-q", "-m", "app"]);
    let (code, out, _) = run(&wt, 1, &["goal", "claim", "app:goal-1", "task-1", "--json"]);
    assert_eq!(code, 0, "{out}");
    assert!(
        out["data"]["lease"]["scope"]
            .as_str()
            .unwrap_or_default()
            .starts_with("app/"),
        "{out}"
    );
}
