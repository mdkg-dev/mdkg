//! The fresh-agent recovery test (task-8), a release gate.
//!
//! On a throwaway clone of this repository, an agent finishes one task on
//! its branch without merging, then dies mid-way through another. Once its
//! lease has expired, a fresh agent with no context must recover, from the
//! repo and mdkg alone:
//! - the current work: nothing is held, and one version waits to merge, on
//!   the dead agent's branch;
//! - the accepted decisions;
//! - valid evidence: a graph that validates, and the record of finished
//!   work. A 0.6-era graph keeps that record as checkpoint nodes. A migrated
//!   one (format 0.7, its config names a `graph_id`) keeps it in the
//!   coordinator's record chain: migrate turns checkpoints into
//!   `legacy.checkpoint` records in the store it ran against, which no clone
//!   carries, so here the chain must hold the dead agent's finished work,
//!   exported and verified;
//! - the next safe action: a ready item nobody holds, which it can claim and
//!   bring up to date with `sync`.
//!
//! The answers are checked as properties, so the test keeps passing as this
//! repository's own graph changes. Missing goal and decision context is
//! seeded along with two practice tasks, so an empty exported graph also
//! provides the context and ready work the recovery exercises.
//!
//! Work that was never `done` lives only on its branch: a claim records no
//! version, so mdkg can't point a fresh agent at the dead agent's
//! half-way commit. The branch is still in git for a person to find.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

mod common;

const TTL: u64 = 60_000;

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).ok();
    }
}

fn scratch() -> Scratch {
    let dir = common::temp::dir("mdkg-recovery");
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

fn run(worktree: &Path, actor: &str, now: u64, args: &[&str]) -> (i32, Value) {
    let out = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
        .arg("--root")
        .arg(worktree)
        .args(["--as", actor])
        .args(args)
        .arg("--json")
        .env("MDKG_NOW_MS", now.to_string())
        .env("MDKG_LEASE_TTL_MS", TTL.to_string())
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

fn ids(nodes: &Value) -> Vec<String> {
    nodes
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|n| n["id"].as_str().map(String::from))
        .collect()
}

/// Whether the graph is on format 0.7: migrate (or a 0.7 init) names a
/// `graph_id` in its config; a 0.6-era config has none.
fn migrated(repo: &Path) -> bool {
    let text = fs::read_to_string(repo.join(".mdkg/config.json")).expect("config");
    let config: Value = serde_json::from_str(&text).expect("config JSON");
    config.get("graph_id").is_some()
}

/// A public export starts with an empty graph. Supply only missing context
/// before the baseline commit, retaining the source graph when it has some.
fn seed_context(repo: &Path) {
    let current = ok(repo, "setup", 0, &["goal", "current"]);
    if current["data"]["goal"].is_null() {
        ok(repo, "setup", 0, &["new", "goal", "Recovery baseline"]);
    }
    let decisions = ok(
        repo,
        "setup",
        0,
        &["list", "--type", "dec", "--status", "accepted"],
    );
    if ids(&decisions["data"]["nodes"]).is_empty() {
        ok(
            repo,
            "setup",
            0,
            &[
                "new",
                "dec",
                "Recovery baseline decision",
                "--status",
                "accepted",
            ],
        );
    }
}

#[test]
fn recovery_context_is_seeded_once_in_a_fresh_graph() {
    // Handwritten unborn metadata keeps context preparation testable without
    // Git-writing commands; the complete recovery cases below exercise Git.
    let dir = scratch();
    let repo = &dir.0;
    for path in [".git/objects", ".git/refs"] {
        fs::create_dir_all(repo.join(path)).expect("Git metadata");
    }
    fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").expect("HEAD");
    fs::write(
        repo.join(".git/config"),
        "[core]\nrepositoryformatversion = 0\nbare = false\n",
    )
    .expect("Git config");
    ok(
        repo,
        "setup",
        0,
        &["init", "--work", "shared", "--graph-only"],
    );
    assert!(ok(repo, "setup", 0, &["goal", "current"])["data"]["goal"].is_null());
    seed_context(repo);
    let goal = ok(repo, "setup", 0, &["goal", "current"]);
    assert_eq!(
        goal["data"]["goal"]["fields"]["goal_state"], "active",
        "{goal}"
    );
    let decisions = ok(
        repo,
        "setup",
        0,
        &["list", "--type", "dec", "--status", "accepted"],
    );
    assert_eq!(ids(&decisions["data"]["nodes"]).len(), 1, "{decisions}");
    let nodes = ok(repo, "setup", 0, &["list", "--fields", "all"]);
    let bytes: Vec<_> = nodes["data"]["nodes"]
        .as_array()
        .expect("nodes")
        .iter()
        .map(|node| {
            let path = repo.join(node["path"].as_str().expect("node path"));
            (path.clone(), fs::read(path).expect("node bytes"))
        })
        .collect();
    assert_eq!(bytes.len(), 2, "{nodes}");
    seed_context(repo);
    assert_eq!(
        ok(repo, "setup", 0, &["list", "--fields", "all"])["data"],
        nodes["data"]
    );
    for (path, original) in bytes {
        assert_eq!(fs::read(path).expect("preserved node"), original);
    }
}

#[test]
fn a_fresh_agent_recovers_the_state_of_this_repository() {
    let dir = scratch();
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let repo = dir.0.join("repo");
    sh(
        &workspace,
        &[
            "clone",
            "--quiet",
            "--local",
            "--no-hardlinks",
            ".",
            repo.to_str().expect("utf-8"),
        ],
    );
    recover(dir, repo);
}

#[test]
fn a_fresh_agent_recovers_an_empty_exported_graph() {
    let dir = scratch();
    let repo = dir.0.join("repo");
    fs::create_dir(&repo).expect("empty export repository");
    sh(&repo, &["init", "-q", "-b", "main"]);
    ok(
        &repo,
        "setup",
        0,
        &["init", "--work", "shared", "--graph-only"],
    );
    let goal = ok(&repo, "setup", 0, &["goal", "current"]);
    assert!(goal["data"]["goal"].is_null(), "{goal}");
    let decisions = ok(
        &repo,
        "setup",
        0,
        &["list", "--type", "dec", "--status", "accepted"],
    );
    assert!(ids(&decisions["data"]["nodes"]).is_empty(), "{decisions}");
    sh(&repo, &["add", "."]);
    sh(&repo, &["commit", "-q", "-m", "empty exported graph"]);
    recover(dir, repo);
}

fn recover(dir: Scratch, repo: PathBuf) {
    // The clone's main is whatever branch this checkout is on.
    let branch = sh(&repo, &["rev-parse", "--abbrev-ref", "HEAD"]);
    if branch != "main" {
        sh(&repo, &["branch", "-f", "main", "HEAD"]);
        sh(&repo, &["switch", "-q", "main"]);
    }
    seed_context(&repo);
    for title in ["Recovery practice one", "Recovery practice two"] {
        ok(
            &repo,
            "setup",
            0,
            &["new", "task", title, "--status", "todo"],
        );
    }
    sh(&repo, &["add", "."]);
    sh(&repo, &["commit", "-q", "-m", "practice tasks"]);
    let dead = dir.0.join("wt-dead");
    let fresh = dir.0.join("wt-fresh");
    sh(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "dead",
            dead.to_str().expect("utf-8"),
            "main",
        ],
    );

    // The agent that will die finishes one item on its branch (unmerged)…
    let first = ok(&dead, "dead", 0, &["next"]);
    let finished = first["data"]["node"]["id"]
        .as_str()
        .expect("a ready item")
        .to_string();
    ok(&dead, "dead", 1, &["done", &finished]);
    // …then claims the next one, edits it, commits, and dies before `done`.
    let second = ok(&dead, "dead", 2, &["next"]);
    let abandoned = second["data"]["node"]["id"]
        .as_str()
        .expect("a second item")
        .to_string();
    assert_ne!(abandoned, finished);
    ok(&dead, "dead", 3, &["claim", &abandoned]);
    let path = dead.join(second["data"]["node"]["path"].as_str().expect("path"));
    let mut text = fs::read_to_string(&path).expect("read");
    text.push_str("\nHalf-way notes from an agent that crashed.\n");
    fs::write(&path, text).expect("write");
    sh(&dead, &["commit", "-q", "-am", "dead: half-way"]);

    // Long after its lease expired, a fresh agent starts in a new worktree.
    let later = 10 * TTL;
    sh(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "fresh",
            fresh.to_str().expect("utf-8"),
            "main",
        ],
    );

    // The abandoned item reads as ready again, and its expired lease doesn't
    // block a claim, even before anyone runs `status`.
    let shown = ok(
        &fresh,
        "fresh",
        later,
        &["show", &abandoned, "--fields", "status"],
    );
    let status_now = shown["data"]["node"]["status"].as_str().unwrap_or_default();
    assert!(
        !["progress", "done"].contains(&status_now),
        "the abandoned item is ready again: {status_now}"
    );
    ok(&fresh, "fresh", later, &["claim", &abandoned]);
    ok(&fresh, "fresh", later, &["release", &abandoned]);

    // Current work: nothing is held; one version waits to merge.
    let status = ok(&fresh, "fresh", later, &["status"]);
    assert_eq!(
        status["data"]["leases"],
        serde_json::json!([]),
        "the dead lease expired"
    );
    let queue = status["data"]["queue"].as_array().expect("queue");
    assert_eq!(queue.len(), 1, "{status}");
    assert_eq!(queue[0]["branch"], "dead");
    assert_eq!(queue[0]["state"], "sequenced");

    // The goal and the accepted decisions.
    let goal = ok(&fresh, "fresh", later, &["goal", "current"]);
    assert_eq!(
        goal["data"]["goal"]["fields"]["goal_state"], "active",
        "{goal}"
    );
    let decisions = ok(
        &fresh,
        "fresh",
        later,
        &["list", "--type", "dec", "--status", "accepted"],
    );
    assert!(
        !ids(&decisions["data"]["nodes"]).is_empty(),
        "accepted decisions are findable"
    );

    // Valid evidence: the graph validates, and the record of finished work
    // is there, as this graph's format keeps it.
    let validate = ok(&fresh, "fresh", later, &["validate"]);
    assert_eq!(validate["data"]["errors"], 0);
    if migrated(&repo) {
        let bundle = dir.0.join("work.jsonl");
        let bundle_arg = bundle.to_str().expect("utf-8");
        ok(
            &fresh,
            "fresh",
            later,
            &["export", "--work", "--out", bundle_arg],
        );
        let verified = ok(&fresh, "fresh", later, &["verify", bundle_arg]);
        assert_eq!(verified["data"]["source"], "bundle", "{verified}");
        let records = fs::read_to_string(&bundle).expect("bundle");
        let committed = records
            .lines()
            .skip(1)
            .map(|l| serde_json::from_str::<Value>(l).expect("a record"))
            .any(|r| {
                r["kind"] == "changeset.commit"
                    && r["actor"] == "dead"
                    && r["body"]["command"]["source"]["branch"] == "dead"
                    && r["subject"]
                        .as_array()
                        .is_some_and(|s| s.iter().any(|v| v == finished.as_str()))
            });
        assert!(
            committed,
            "the chain records the dead agent's finished work:\n{records}"
        );
    } else {
        let checkpoints = ok(&fresh, "fresh", later, &["list", "--type", "checkpoint"]);
        assert!(!ids(&checkpoints["data"]["nodes"]).is_empty());
    }

    // The next safe action: a ready item nobody holds. The abandoned item is
    // ready again; the finished one is not.
    let next = ok(&fresh, "fresh", later, &["next"]);
    let picked = next["data"]["node"]["id"]
        .as_str()
        .expect("next")
        .to_string();
    assert_ne!(
        picked, finished,
        "the dead agent's finished item isn't offered again"
    );
    let shown = ok(
        &fresh,
        "fresh",
        later,
        &["show", &picked, "--fields", "status"],
    );
    let picked_status = shown["data"]["node"]["status"].as_str().unwrap_or_default();
    assert!(
        !["progress", "done", "review", "blocked"].contains(&picked_status),
        "next offers ready work: {picked_status}"
    );
    ok(&fresh, "fresh", later, &["claim", &picked]);
    ok(&fresh, "fresh", later, &["release", &picked]);
    let list = ok(&fresh, "fresh", later, &["list", "--status", "progress"]);
    assert!(
        !ids(&list["data"]["nodes"]).contains(&abandoned),
        "an expired lease no longer shows as progress"
    );

    // Picking up the dead agent's finished work: claiming it warns that it's
    // done, and sync stacks this worktree on the unmerged version.
    let claim = ok(&fresh, "fresh", later, &["claim", &finished]);
    assert_eq!(claim["data"]["sync"][0]["branch"], "dead", "{claim}");
    assert_eq!(
        claim["diagnostics"][0]["code"], "coord.node_done",
        "{claim}"
    );
    let sync = ok(&fresh, "fresh", later, &["sync", &finished]);
    assert_eq!(sync["data"]["up_to_date"], true);
    let shown = ok(
        &fresh,
        "fresh",
        later,
        &["show", &finished, "--fields", "status"],
    );
    // Holding a lease on it doesn't make finished work in progress again.
    assert_eq!(shown["data"]["node"]["status"], "done");
}
