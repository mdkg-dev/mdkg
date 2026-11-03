//! `migrate` end to end, in scratch repositories (task-40): dry run, apply
//! in one commit, rollback to a byte-identical tree, and a crash after
//! every step leaving the old graph or the new one, from which the same
//! command finishes or rolls back.

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

fn scratch() -> Scratch {
    let dir = common::temp::dir("mdkg-cli-migrate");
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
        // One date, so every fixture repository is the same commits.
        .env("GIT_AUTHOR_DATE", "2026-10-24T12:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-10-24T12:00:00Z")
        .env_remove("MDKG_MIGRATE_CRASH_AFTER")
        .env_remove("MDKG_MIGRATE_RACE_FILE")
        .env_remove("MDKG_MIGRATE_RACE_TEXT")
        .env_remove("MDKG_MIGRATE_BREAK_STAGED")
        .env_remove("MDKG_MIGRATE_RACE_STORE")
        .env_remove("MDKG_MIGRATE_RACE_GRAPH")
        .env_remove("MDKG_MIGRATE_RACE_INDEX")
        .env_remove("MDKG_MIGRATE_RACE_UNMERGED")
        .env_remove("MDKG_MIGRATE_CRASH_AFTER_SWAPS")
        .env_remove("MDKG_MIGRATE_RESTORE_CRASH_AFTER_WRITES")
        .env_remove("MDKG_MIGRATE_RESTORE_FAIL_INDEX")
}

fn git(repo: &Path, args: &[&str]) -> String {
    let out = isolated(Command::new("git").arg("-C").arg(repo).args(args))
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

const TASK_1: &str = "---
id: task-1
type: task
title: First task
status: todo
priority: 1
tags: []
relates: [task-2, test-2]
blocked_by: [test-1]
refs: [chk-1]
created: 2026-10-18
updated: 2026-10-19
---
# Overview

Builds on task-2; see `mdkg show task-2`.

# Progress (2026-10-19)

- step one
";

const TASK_2: &str = "---
id: task-2
type: task
title: Second task
status: backlog
created: 2026-10-18
updated: 2026-10-18
---
# Overview

Body.
";

const CHK_1: &str = "---
id: chk-1
type: checkpoint
title: First checkpoint
checkpoint_kind: implementation
status: done
scope: [task-1]
created: 2026-10-19
updated: 2026-10-19
---
# Summary

Did task-1.
";

const TEST_1: &str = "---
id: test-1
type: test
title: Proves task-1
status: done
relates: [task-1]
created: 2026-10-19
updated: 2026-10-19
---
Evidence.
";

const TEST_2: &str = "---
id: test-2
type: test
title: Later proof
status: todo
relates: [task-1]
created: 2026-10-19
updated: 2026-10-19
---
";

const EVENTS: &str = "{\"ts\":\"2026-10-18T01:02:03.004Z\",\"agent\":\"mdkg-cli\",\"kind\":\"NODE_CREATED\",\"refs\":[\"task-1\"]}\n";

const AGENTS: &str = "Start with `mdkg task start task-1`.\n";

/// A repository with a small 0.6.0 graph committed on main: two tasks, a
/// checkpoint, a finished and an open test, an event log, AGENTS.md, and
/// an ignored cache file inside `.mdkg`.
fn repo(dir: &Scratch, name: &str) -> PathBuf {
    repo_in(dir, name, "sha1")
}

/// [`repo`], in a repository of object format `format`.
fn repo_in(dir: &Scratch, name: &str, format: &str) -> PathBuf {
    let repo = dir.0.join(name);
    let conformance = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance");
    fs::create_dir_all(repo.join(".mdkg/work/events")).expect("repo");
    copy_tree(&conformance.join("base"), &repo);
    for (path, text) in [
        (".mdkg/work/task-1-first.md", TASK_1),
        (".mdkg/work/task-2-second.md", TASK_2),
        (".mdkg/work/chk-1-first.md", CHK_1),
        (".mdkg/work/test-1-proves.md", TEST_1),
        (".mdkg/work/test-2-later.md", TEST_2),
        (".mdkg/work/events/events.jsonl", EVENTS),
        ("AGENTS.md", AGENTS),
        (".gitignore", ".mdkg/index/\n"),
    ] {
        fs::write(repo.join(path), text).expect("write");
    }
    fs::create_dir_all(repo.join(".mdkg/index")).expect("index");
    fs::write(repo.join(".mdkg/index/cache.json"), "{\"cached\":true}").expect("cache");
    git(
        &repo,
        &[
            "init",
            "-q",
            "-b",
            "main",
            &format!("--object-format={format}"),
        ],
    );
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    repo
}

fn run_env(repo: &Path, args: &[&str], crash_after: Option<&str>) -> (i32, Option<Value>) {
    run_with(repo, args, crash_after, &[])
}

fn run_with(
    repo: &Path,
    args: &[&str],
    crash_after: Option<&str>,
    env: &[(&str, &str)],
) -> (i32, Option<Value>) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mdkg"));
    isolated(&mut cmd)
        .arg("--root")
        .arg(repo)
        .args(["--as", "a"])
        .args(args)
        .arg("--json")
        .env("MDKG_NOW_MS", "1000");
    if let Some(stage) = crash_after {
        cmd.env("MDKG_MIGRATE_CRASH_AFTER", stage);
    }
    cmd.envs(env.iter().copied());
    let out = cmd.output().expect("run mdkg");
    let envelope: Option<Value> = serde_json::from_slice(&out.stdout).ok();
    if let Some(e) = &envelope {
        common::conforms_envelope(e);
    }
    (out.status.code().unwrap_or(-1), envelope)
}

fn run(repo: &Path, args: &[&str]) -> (i32, Value) {
    let (code, envelope) = run_env(repo, args, None);
    (
        code,
        envelope.unwrap_or_else(|| panic!("{args:?}: no JSON")),
    )
}

fn codes(envelope: &Value) -> Vec<String> {
    envelope["diagnostics"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|d| d["code"].as_str().map(String::from))
        .collect()
}

/// Every file under `dir` (but `.git`), with its bytes.
fn snapshot(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for entry in fs::read_dir(dir).expect("read dir") {
            let entry = entry.expect("entry");
            let path = entry.path();
            let rel = path
                .strip_prefix(base)
                .expect("rel")
                .to_string_lossy()
                .into_owned();
            if rel == ".git" {
                continue;
            }
            if entry.file_type().expect("type").is_dir() {
                walk(base, &path, out);
            } else {
                out.insert(rel, fs::read(&path).expect("read"));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

fn dry_run(repo: &Path) -> String {
    let (code, out) = run(repo, &["migrate", "--dry-run"]);
    assert_eq!(code, 0, "{out}");
    out["data"]["plan"].as_str().expect("plan").to_string()
}

/// A dry run that moves the work history to the store, as 0.7.0 did.
fn dry_run_store(repo: &Path) -> String {
    let (code, out) = run(repo, &["migrate", "--dry-run", "--work-history", "store"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["data"]["work_history"], "store", "{out}");
    out["data"]["plan"].as_str().expect("plan").to_string()
}

#[test]
fn dry_run_apply_and_rollback_round_trip() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    // A coordinator store, so the snapshot runs.
    let (code, out) = run(&repo, &["claim", "task-2"]);
    assert_eq!(code, 0, "{out}");
    let before = snapshot(&repo);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    let tracked = |repo: &Path| git(repo, &["ls-files", "--", ".mdkg/work"]);
    let tracked_before = tracked(&repo);

    let (code, out) = run(&repo, &["migrate", "--dry-run"]);
    assert_eq!(code, 0, "{out}");
    let hash = out["data"]["plan"].as_str().expect("plan").to_string();
    // The graph commits its work (its config leaves work.location
    // implicit, which is git), so its history stays there.
    assert_eq!(out["data"]["work_history"], "git", "{out}");
    let counts = &out["data"]["counts"];
    assert_eq!(counts["records"], 0, "{out}");
    assert_eq!(
        counts["kept_in_git"], 2,
        "the checkpoint and the finished test"
    );
    assert_eq!(counts["nodes"], 5, "every node stays");
    assert_eq!(counts["files_deleted"], 0);
    // A dry run changes nothing, and gives the same hash again.
    assert_eq!(snapshot(&repo), before);
    assert_eq!(dry_run(&repo), hash);
    assert!(Path::new(out["data"]["patch_file"].as_str().expect("patch")).exists());

    let (code, out) = run(&repo, &["migrate", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["data"]["stage"], "done");
    // One commit on the base, a clean tree, and the backups.
    assert_eq!(git(&repo, &["rev-parse", "HEAD^"]), base);
    assert_eq!(git(&repo, &["status", "--porcelain"]), "");
    assert_eq!(git(&repo, &["rev-parse", "mdkg-pre-0.7"]), base);
    let backups = &out["data"]["backups"];
    assert!(
        Path::new(backups["graph_copy"].as_str().expect("copy"))
            .join("config.json")
            .exists()
    );
    assert!(Path::new(backups["store_snapshot"].as_str().expect("snapshot")).exists());
    // The graph is on 0.7.0, with every work file still tracked: the
    // checkpoint and the finished test as nodes, the event log as it was.
    assert_eq!(tracked(&repo), tracked_before);
    assert_eq!(tracked_before.lines().count(), 6, "{tracked_before}");
    let chk = fs::read_to_string(repo.join(".mdkg/work/chk-1-first.md")).expect("chk-1");
    assert!(chk.starts_with("---\nid: checkpoint-"), "{chk}");
    assert!(chk.contains("\nlegacy_id: chk-1\n"), "{chk}");
    assert!(chk.contains("\nscope: [task-"), "{chk}");
    let test1 = fs::read_to_string(repo.join(".mdkg/work/test-1-proves.md")).expect("test-1");
    assert!(test1.contains("\nlegacy_id: test-1\n"), "{test1}");
    assert!(test1.contains("\nstatus: done\n"), "{test1}");
    assert_eq!(
        fs::read_to_string(repo.join(".mdkg/work/events/events.jsonl")).expect("events"),
        EVENTS
    );
    let task1 = fs::read_to_string(repo.join(".mdkg/work/task-1-first.md")).expect("task-1");
    assert!(task1.contains("\nlegacy_id: task-1\n"), "{task1}");
    assert!(
        task1.contains("\ncreated: 2026-10-18T00:00:00.001Z\n"),
        "{task1}"
    );
    assert!(
        task1.contains("\nupdated: 2026-10-19T00:00:00.000Z\n"),
        "{task1}"
    );
    assert!(task1.contains("blocked_by: [test-"), "{task1}");
    assert!(task1.contains("refs: [checkpoint-"), "{task1}");
    assert!(
        task1.contains("# Progress (2026-10-19)\n\n- step one\n"),
        "{task1}"
    );
    assert!(!task1.contains("# Migrated records"), "{task1}");
    let config = fs::read_to_string(repo.join(".mdkg/config.json")).expect("config");
    assert!(config.contains("\"graph_id\""));
    let config: Value = serde_json::from_str(&config).expect("config json");
    assert_eq!(
        config["work"]["location"], "git",
        "the setting is kept, said"
    );
    // The ignored cache came along; AGENTS.md waits for --rewrite-refs.
    assert!(repo.join(".mdkg/index/cache.json").exists());
    assert_eq!(
        fs::read_to_string(repo.join("AGENTS.md")).expect("agents"),
        AGENTS
    );
    // The migrated graph is one 0.7.0 reads.
    let (code, out) = run(&repo, &["validate"]);
    assert_eq!(code, 0, "{out}");
    let (code, out) = run(&repo, &["show", "task-1"]);
    assert_eq!(code, 0, "a legacy id still finds its node: {out}");
    let (code, out) = run(&repo, &["show", "chk-1"]);
    assert_eq!(code, 0, "the checkpoint is a node: {out}");
    // Applying again is a no-op that says so.
    let (code, again) = run(&repo, &["migrate", "--apply", &hash]);
    assert_eq!(code, 0, "{again}");
    assert_eq!(again["data"]["stage"], "done");
    // A migrated graph has nothing to plan.
    let (code, out) = run(&repo, &["migrate", "--dry-run"]);
    assert_ne!(code, 0);
    assert!(
        codes(&out).contains(&"migrate.already_migrated".into()),
        "{out}"
    );

    let (code, out) = run(&repo, &["migrate", "--rollback"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["data"]["stage"], "rolled-back");
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), base);
    assert_eq!(git(&repo, &["status", "--porcelain"]), "");
    assert_eq!(
        snapshot(&repo),
        before,
        "rollback leaves a byte-identical tree"
    );
    // And it can be migrated again, to the same plan.
    assert_eq!(dry_run(&repo), hash);
    let (code, out) = run(&repo, &["migrate", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
}

#[test]
fn with_work_history_store_the_history_leaves_git_as_in_0_7_0() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    // A coordinator store, so the snapshot runs.
    let (code, out) = run(&repo, &["claim", "task-2"]);
    assert_eq!(code, 0, "{out}");
    let before = snapshot(&repo);
    let base = git(&repo, &["rev-parse", "HEAD"]);

    let (code, out) = run(&repo, &["migrate", "--dry-run", "--work-history", "store"]);
    assert_eq!(code, 0, "{out}");
    let hash = out["data"]["plan"].as_str().expect("plan").to_string();
    let counts = &out["data"]["counts"];
    assert_eq!(counts["checkpoints"], 1, "{out}");
    assert_eq!(counts["check_results"], 1);
    assert_eq!(counts["legacy_events"], 1);
    assert_eq!(counts["review_records"], 1);
    assert_eq!(counts["nodes"], 3, "two tasks and the open test stay");
    // A dry run changes nothing, and gives the same hash again.
    assert_eq!(snapshot(&repo), before);
    assert_eq!(dry_run_store(&repo), hash);
    assert!(Path::new(out["data"]["patch_file"].as_str().expect("patch")).exists());

    let (code, out) = run(
        &repo,
        &["migrate", "--apply", &hash, "--work-history", "store"],
    );
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["data"]["stage"], "done");
    // One commit on the base, a clean tree, and the backups.
    assert_eq!(git(&repo, &["rev-parse", "HEAD^"]), base);
    assert_eq!(git(&repo, &["status", "--porcelain"]), "");
    assert_eq!(git(&repo, &["rev-parse", "mdkg-pre-0.7"]), base);
    let backups = &out["data"]["backups"];
    assert!(
        Path::new(backups["graph_copy"].as_str().expect("copy"))
            .join("config.json")
            .exists()
    );
    assert!(Path::new(backups["store_snapshot"].as_str().expect("snapshot")).exists());
    // The graph is on 0.7.0.
    assert!(!repo.join(".mdkg/work/chk-1-first.md").exists());
    assert!(!repo.join(".mdkg/work/test-1-proves.md").exists());
    assert!(!repo.join(".mdkg/work/events/events.jsonl").exists());
    let task1 = fs::read_to_string(repo.join(".mdkg/work/task-1-first.md")).expect("task-1");
    assert!(task1.contains("\nlegacy_id: task-1\n"), "{task1}");
    assert!(
        task1.contains("\ncreated: 2026-10-18T00:00:00.001Z\n"),
        "{task1}"
    );
    assert!(
        task1.contains("\nupdated: 2026-10-19T00:00:00.000Z\n"),
        "{task1}"
    );
    assert!(task1.contains("blocked_by: []\n"), "{task1}");
    assert!(
        task1.contains("Progress notes moved to the work log"),
        "{task1}"
    );
    assert!(
        task1.contains("# Migrated records\n\n- chk-1 (2026-10-19): First checkpoint"),
        "{task1}"
    );
    assert!(
        task1.contains("- test-1 (done, 2026-10-19): Proves task-1"),
        "{task1}"
    );
    let config = fs::read_to_string(repo.join(".mdkg/config.json")).expect("config");
    assert!(config.contains("\"graph_id\""));
    // The ignored cache came along; AGENTS.md waits for --rewrite-refs.
    assert!(repo.join(".mdkg/index/cache.json").exists());
    assert_eq!(
        fs::read_to_string(repo.join("AGENTS.md")).expect("agents"),
        AGENTS
    );
    // The migrated graph is one 0.7.0 reads.
    let (code, out) = run(&repo, &["validate"]);
    assert_eq!(code, 0, "{out}");
    let (code, out) = run(&repo, &["show", "task-1"]);
    assert_eq!(code, 0, "a legacy id still finds its node: {out}");
    // Applying again is a no-op that says so.
    let (code, again) = run(&repo, &["migrate", "--apply", &hash]);
    assert_eq!(code, 0, "{again}");
    assert_eq!(again["data"]["stage"], "done");
    // A migrated graph has nothing to plan.
    let (code, out) = run(&repo, &["migrate", "--dry-run", "--work-history", "store"]);
    assert_ne!(code, 0);
    assert!(
        codes(&out).contains(&"migrate.already_migrated".into()),
        "{out}"
    );

    let (code, out) = run(&repo, &["migrate", "--rollback"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["data"]["stage"], "rolled-back");
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), base);
    assert_eq!(git(&repo, &["status", "--porcelain"]), "");
    assert_eq!(
        snapshot(&repo),
        before,
        "rollback leaves a byte-identical tree"
    );
    // And it can be migrated again, to the same plan.
    assert_eq!(dry_run_store(&repo), hash);
    let (code, out) = run(
        &repo,
        &["migrate", "--apply", &hash, "--work-history", "store"],
    );
    assert_eq!(code, 0, "{out}");
}

#[test]
fn a_migrated_repository_keeps_its_agent_files_and_doctor_is_clean() {
    let dir = scratch();
    let repo = repo(&dir, "agents");
    // What an earlier install left: the AGENTS.md block, the three MCP
    // configs and a managed mirror.
    let agents = format!(
        "{AGENTS}\n<!-- mdkg:harness:start -->\nRun `mdkg status`.\n<!-- mdkg:harness:end -->\n"
    );
    fs::create_dir_all(repo.join(".codex")).expect("mkdir");
    fs::create_dir_all(repo.join(".claude/skills")).expect("mkdir");
    for (path, text) in [
        ("AGENTS.md", agents.as_str()),
        (
            ".mcp.json",
            "{\"mcpServers\": {\"mdkg\": {\"type\": \"stdio\", \"command\": \"mdkg\", \"args\": [\"mcp\"]}}}\n",
        ),
        (
            ".codex/config.toml",
            "[mcp_servers.mdkg]\ncommand = \"mdkg\"\nargs = [\"mcp\"]\n",
        ),
        (
            "opencode.json",
            "{\"mcp\": {\"mdkg\": {\"type\": \"local\", \"command\": [\"mdkg\", \"mcp\"]}}}\n",
        ),
        (
            ".claude/skills/.mdkg-managed.json",
            "{\"managed_slugs\": []}\n",
        ),
    ] {
        fs::write(repo.join(path), text).expect("write");
    }
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "agents"]);
    let hash = dry_run(&repo);
    let (code, out) = run(&repo, &["migrate", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
    // Everything it had is listed, the old targets' mirrors included.
    let cfg: Value =
        serde_json::from_str(&fs::read_to_string(repo.join(".mdkg/config.json")).expect("cfg"))
            .expect("json");
    assert_eq!(
        cfg["projections"],
        serde_json::json!([
            "agents-md",
            "mcp:claude",
            "mcp:codex",
            "mcp:opencode",
            "skills:.agents/skills",
            "skills:.claude/skills"
        ]),
        "{cfg}"
    );
    // A sync rewrites them as this build does, and keeps every one.
    let (code, out) = run(&repo, &["install"]);
    assert_eq!(code, 0, "{out}");
    for f in [
        "AGENTS.md",
        ".mcp.json",
        ".codex/config.toml",
        "opencode.json",
    ] {
        assert!(repo.join(f).is_file(), "{f}");
    }
    let text = fs::read_to_string(repo.join("AGENTS.md")).expect("AGENTS.md");
    assert!(
        text.starts_with(AGENTS) && text.contains("mdkg:instructions"),
        "{text}"
    );
    let (_, out) = run(&repo, &["doctor"]);
    let checks = out["data"]["checks"].as_array().expect("checks");
    for name in ["config", "graph", "mirrors", "mcp", "agents"] {
        let c = checks.iter().find(|c| c["name"] == name).expect(name);
        assert_eq!(c["status"], "ok", "{name}: {out}");
    }
}

#[test]
fn a_nested_graph_records_its_own_projections_not_the_repositorys() {
    let dir = scratch();
    let top = dir.0.join("nested");
    let graph = top.join("packages/foo");
    let conformance = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance");
    fs::create_dir_all(graph.join(".mdkg/work")).expect("mkdir");
    copy_tree(&conformance.join("base"), &graph);
    fs::write(
        graph.join(".mdkg/work/task-1-only.md"),
        "---\nid: task-1\ntype: task\ntitle: Only\nstatus: todo\ncreated: 2026-10-20\nupdated: 2026-10-20\n---\n",
    )
    .expect("write");
    // The repository's top has the AGENTS.md block; the graph's folder an
    // MCP config.
    fs::write(
        top.join("AGENTS.md"),
        "<!-- mdkg:harness:start -->\nx\n<!-- mdkg:harness:end -->\n",
    )
    .expect("write");
    fs::write(
        graph.join(".mcp.json"),
        "{\"mcpServers\": {\"mdkg\": {\"command\": \"mdkg\", \"args\": [\"mcp\"]}}}\n",
    )
    .expect("write");
    git(&top, &["init", "-q", "-b", "main"]);
    git(&top, &["add", "."]);
    git(&top, &["commit", "-q", "-m", "init"]);
    let hash = dry_run(&graph);
    let (code, out) = run(&graph, &["migrate", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
    let cfg: Value =
        serde_json::from_str(&fs::read_to_string(graph.join(".mdkg/config.json")).expect("cfg"))
            .expect("json");
    // Its own MCP config, and the mirrors its old targets named.
    assert_eq!(
        cfg["projections"],
        serde_json::json!([
            "mcp:claude",
            "skills:.agents/skills",
            "skills:.claude/skills"
        ]),
        "{cfg}"
    );
}

#[test]
fn apply_refuses_a_stale_hash_and_a_dirty_tree() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    let hash = dry_run(&repo);
    let (code, out) = run(&repo, &["migrate", "--apply", "sha256:00"]);
    assert_ne!(code, 0);
    assert!(
        codes(&out).contains(&"migrate.plan_mismatch".into()),
        "{out}"
    );
    // A change after the dry run is a different plan.
    fs::write(
        repo.join(".mdkg/work/task-2-second.md"),
        TASK_2.replace("Body.", "Edited."),
    )
    .expect("edit");
    let (code, out) = run(&repo, &["migrate", "--apply", &hash]);
    assert_ne!(code, 0);
    assert!(
        codes(&out).contains(&"migrate.plan_mismatch".into()),
        "{out}"
    );
    // Its own plan, but uncommitted.
    let edited = dry_run(&repo);
    let (code, out) = run(&repo, &["migrate", "--apply", &edited]);
    assert_ne!(code, 0);
    assert!(codes(&out).contains(&"migrate.dirty".into()), "{out}");
    assert!(repo.join(".mdkg/work/chk-1-first.md").exists());
}

#[test]
fn rewrite_refs_applies_the_patch_and_rollback_undoes_it() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    let before = snapshot(&repo);
    let hash = dry_run(&repo);
    let (code, out) = run(&repo, &["migrate", "--apply", &hash, "--rewrite-refs"]);
    assert_eq!(code, 0, "{out}");
    let agents = fs::read_to_string(repo.join("AGENTS.md")).expect("agents");
    assert!(
        agents.starts_with("Start with `mdkg claim task-"),
        "{agents}"
    );
    assert_eq!(git(&repo, &["status", "--porcelain"]), "");
    assert!(git(&repo, &["show", "--stat", "HEAD"]).contains("AGENTS.md"));
    let (code, out) = run(&repo, &["migrate", "--rollback"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(snapshot(&repo), before);
}

#[test]
fn rollback_refuses_once_the_branch_moved_on() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    let hash = dry_run(&repo);
    let (code, out) = run(&repo, &["migrate", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
    fs::write(repo.join("NOTES.md"), "later\n").expect("notes");
    git(&repo, &["add", "NOTES.md"]);
    git(&repo, &["commit", "-q", "-m", "later"]);
    let (code, out) = run(&repo, &["migrate", "--rollback"]);
    assert_ne!(code, 0);
    assert!(codes(&out).contains(&"migrate.moved_on".into()), "{out}");
    let task1 = fs::read_to_string(repo.join(".mdkg/work/task-1-first.md")).expect("task-1");
    assert!(
        task1.contains("\nlegacy_id: task-1\n"),
        "nothing was rolled back"
    );
}

/// The tracked files under `.mdkg`, as the worktree has them.
fn graph_files(repo: &Path) -> BTreeMap<String, Vec<u8>> {
    snapshot(repo)
        .into_iter()
        .filter(|(p, _)| p.starts_with(".mdkg/") && !p.starts_with(".mdkg/index/"))
        .collect()
}

#[test]
fn a_crash_after_any_step_leaves_one_graph_and_finishes_or_rolls_back() {
    let dir = scratch();
    // The reference: a migration that ran through.
    let reference = repo(&dir, "reference");
    let old_graph = graph_files(&reference);
    let hash = dry_run(&reference);
    let (code, out) = run(&reference, &["migrate", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
    let new_graph = graph_files(&reference);
    let new_tree = git(&reference, &["rev-parse", "HEAD^{tree}"]);

    for stage in [
        "backed-up",
        "recorded",
        "built",
        "swapped",
        "committed",
        "done",
    ] {
        for finish in ["apply", "rollback"] {
            let repo = repo(&dir, &format!("{stage}-{finish}"));
            let before = snapshot(&repo);
            let base = git(&repo, &["rev-parse", "HEAD"]);
            assert_eq!(dry_run(&repo), hash, "the same graph, the same plan");
            let (code, _) = run_env(&repo, &["migrate", "--apply", &hash], Some(stage));
            assert_eq!(code, 86, "{stage}: the crash hook fired");
            // Readers see the old graph or the new one, never a mix.
            let now = graph_files(&repo);
            // (The staging folder beside it, and the new graph's swap marker,
            // are the migration's own and go at the end.)
            let staging = format!(".mdkg.migrate-{}", &hash[7..19]);
            let mark = format!(".mdkg/.migrate-{}", &hash[7..19]);
            let visible: BTreeMap<String, Vec<u8>> = now
                .into_iter()
                .filter(|(p, _)| !p.starts_with(&staging) && *p != mark)
                .collect();
            assert!(
                visible == old_graph || visible == new_graph,
                "{stage}: the graph is neither the old one nor the new one"
            );
            let (code, out) = run(&repo, &["migrate", "--status"]);
            let stopped = out["data"]["stage"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            assert_eq!(code, 0, "{out}");
            assert_eq!(stopped, stage, "{out}");
            if finish == "apply" {
                let (code, out) = run(&repo, &["migrate", "--apply", &hash]);
                assert_eq!(code, 0, "{stage}: {out}");
                assert_eq!(out["data"]["stage"], "done");
                assert_eq!(
                    git(&repo, &["rev-parse", "HEAD^{tree}"]),
                    new_tree,
                    "{stage}"
                );
                assert_eq!(git(&repo, &["rev-parse", "HEAD^"]), base);
                assert_eq!(graph_files(&repo), new_graph, "{stage}");
                assert_eq!(git(&repo, &["status", "--porcelain"]), "", "{stage}");
            } else {
                let (code, out) = run(&repo, &["migrate", "--rollback"]);
                assert_eq!(code, 0, "{stage}: {out}");
                assert_eq!(git(&repo, &["rev-parse", "HEAD"]), base, "{stage}");
                assert_eq!(
                    snapshot(&repo),
                    before,
                    "{stage}: byte-identical after rollback"
                );
            }
        }
    }
}

#[test]
fn an_unfinished_migration_blocks_another_plan() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    let hash = dry_run(&repo);
    let (code, _) = run_env(&repo, &["migrate", "--apply", &hash], Some("recorded"));
    assert_eq!(code, 86);
    let (code, out) = run(&repo, &["migrate", "--apply", "sha256:other"]);
    assert_ne!(code, 0);
    assert!(codes(&out).contains(&"migrate.in_progress".into()), "{out}");
    let (code, out) = run(&repo, &["migrate", "--status"]);
    assert_eq!(code, 0);
    assert!(codes(&out).contains(&"migrate.in_progress".into()), "{out}");
}

#[test]
fn rollback_never_installs_a_partial_copy() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    let before = snapshot(&repo);
    let hash = dry_run(&repo);
    let (code, out) = run(&repo, &["migrate", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
    // A rollback that stopped while copying the backup in left a staging
    // folder holding only the old config: it must not be swapped in.
    let (code, _) = run_env(&repo, &["migrate", "--rollback"], Some("rolling-back"));
    assert_eq!(code, 86);
    let staging = repo.join(format!(".mdkg.migrate-{}/.mdkg", &hash[7..19]));
    fs::create_dir_all(&staging).expect("staging");
    fs::write(staging.join("config.json"), &before[".mdkg/config.json"]).expect("partial");
    let (code, out) = run(&repo, &["migrate", "--rollback", "--force"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(snapshot(&repo), before);
}

#[test]
fn a_graph_folder_holding_links_is_refused() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    std::os::unix::fs::symlink("../../AGENTS.md", repo.join(".mdkg/work/linked.md")).expect("link");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "link"]);
    let (code, out) = run(&repo, &["migrate", "--dry-run"]);
    assert_ne!(code, 0);
    assert!(codes(&out).contains(&"migrate.unsupported".into()), "{out}");
}

#[test]
fn rewrite_refs_keeps_an_executable_script_executable() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch();
    let repo = repo(&dir, "repo");
    fs::create_dir_all(repo.join("scripts")).expect("scripts");
    let script = repo.join("scripts/start.sh");
    fs::write(&script, "#!/bin/sh\nmdkg task start task-1\n").expect("script");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "script"]);
    let before = snapshot(&repo);
    let hash = dry_run(&repo);
    let (code, out) = run(&repo, &["migrate", "--apply", &hash, "--rewrite-refs"]);
    assert_eq!(code, 0, "{out}");
    let text = fs::read_to_string(&script).expect("script");
    assert!(text.contains("mdkg claim task-"), "{text}");
    assert_eq!(
        fs::metadata(&script).expect("meta").permissions().mode() & 0o777,
        0o755
    );
    assert_eq!(git(&repo, &["status", "--porcelain"]), "");
    let (code, out) = run(&repo, &["migrate", "--rollback"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        fs::metadata(&script).expect("meta").permissions().mode() & 0o777,
        0o755
    );
    assert_eq!(snapshot(&repo), before);
}

#[test]
fn subgraph_refs_resolve_whether_or_not_the_child_migrated_first() {
    let dir = scratch();
    let parent = repo(&dir, "parent");
    // The child: its own repository, inside the parent's folder.
    let child = parent.join("child");
    let conformance = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance");
    fs::create_dir_all(child.join(".mdkg/work")).expect("child");
    copy_tree(&conformance.join("base"), &child);
    fs::write(
        child.join(".mdkg/work/task-7-child.md"),
        TASK_2.replace("task-2", "task-7"),
    )
    .expect("child task");
    git(&child, &["init", "-q", "-b", "main"]);
    git(&child, &["add", "."]);
    git(&child, &["commit", "-q", "-m", "child"]);
    // The parent names it as a subgraph, ignores its folder, and refers to
    // one of its nodes.
    let config_path = parent.join(".mdkg/config.json");
    let mut config: Value =
        serde_json::from_str(&fs::read_to_string(&config_path).expect("config")).expect("json");
    config["subgraphs"] =
        serde_json::json!({ "child": { "enabled": true, "source_path": "child" } });
    fs::write(
        &config_path,
        serde_json::to_string_pretty(&config).expect("json"),
    )
    .expect("config");
    fs::write(parent.join(".gitignore"), ".mdkg/index/\nchild/\n").expect("ignore");
    fs::write(
        parent.join(".mdkg/work/task-2-second.md"),
        TASK_2.replace(
            "status: backlog",
            "status: backlog\nrelates: [child:task-7]",
        ),
    )
    .expect("ref");
    git(&parent, &["add", "."]);
    git(&parent, &["commit", "-q", "-m", "subgraph"]);

    let planned = |repo: &Path| -> String {
        let (code, out) = run(repo, &["migrate", "--dry-run"]);
        assert_eq!(code, 0, "{out}");
        let plan = Path::new(out["data"]["plan_file"].as_str().expect("plan")).to_path_buf();
        let stored: Value = serde_json::from_slice(&fs::read(plan).expect("plan")).expect("json");
        stored["contents"][".mdkg/work/task-2-second.md"]
            .as_str()
            .expect("task-2")
            .to_string()
    };
    let before_child = planned(&parent);
    assert!(before_child.contains("relates: [mdkg://"), "{before_child}");
    // Migrate the child, then plan the parent again: the same ref.
    let hash = dry_run(&child);
    let (code, out) = run(&child, &["migrate", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(planned(&parent), before_child);
}

#[test]
fn an_edit_after_the_backup_is_refused_not_lost() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    let hash = dry_run(&repo);
    let (code, _) = run_env(&repo, &["migrate", "--apply", &hash], Some("built"));
    assert_eq!(code, 86);
    // Someone edits a planned node before the swap.
    let task2 = repo.join(".mdkg/work/task-2-second.md");
    let edited = TASK_2.replace("Body.", "Edited during the migration.");
    fs::write(&task2, &edited).expect("edit");
    let (code, out) = run(&repo, &["migrate", "--apply", &hash]);
    assert_ne!(code, 0);
    assert!(codes(&out).contains(&"migrate.dirty".into()), "{out}");
    assert_eq!(
        fs::read_to_string(&task2).expect("read"),
        edited,
        "the edit survives"
    );
    assert!(
        repo.join(".mdkg/work/chk-1-first.md").exists(),
        "still the old graph"
    );
    // Rolling back now leaves the live graph, with the edit, alone.
    let (code, out) = run(&repo, &["migrate", "--rollback"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        fs::read_to_string(&task2).expect("read"),
        edited,
        "rollback keeps the edit"
    );
    assert!(repo.join(".mdkg/work/chk-1-first.md").exists());
}

#[test]
fn rollback_refuses_to_discard_edits_to_the_migrated_graph() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    let hash = dry_run(&repo);
    // Crash after the swap, then someone adds a node to the new graph.
    let (code, _) = run_env(&repo, &["migrate", "--apply", &hash], Some("swapped"));
    assert_eq!(code, 86);
    let added = repo.join(".mdkg/work/task-9-new.md");
    fs::write(&added, "---\nid: task-9\n---\n").expect("add");
    let (code, out) = run(&repo, &["migrate", "--rollback"]);
    assert_ne!(code, 0);
    assert!(codes(&out).contains(&"migrate.dirty".into()), "{out}");
    assert!(added.exists(), "nothing was discarded");
    // --force rolls back anyway.
    let (code, out) = run(&repo, &["migrate", "--rollback", "--force"]);
    assert_eq!(code, 0, "{out}");
    assert!(repo.join(".mdkg/work/chk-1-first.md").exists());
}

#[test]
fn rewrite_refs_rewrites_a_skill_inside_the_graph_folder() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    let skill = repo.join(".mdkg/skills/example/SKILL.md");
    fs::create_dir_all(skill.parent().expect("dir")).expect("skills");
    fs::write(
        &skill,
        "---\nname: example\ndescription: x\n---\nRun `mdkg task start task-1`.\n",
    )
    .expect("skill");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "skill"]);
    let before = snapshot(&repo);
    let hash = dry_run(&repo);
    let (code, out) = run(&repo, &["migrate", "--apply", &hash, "--rewrite-refs"]);
    assert_eq!(code, 0, "{out}");
    let text = fs::read_to_string(&skill).expect("skill");
    assert!(text.contains("Run `mdkg claim task-"), "{text}");
    assert_eq!(git(&repo, &["status", "--porcelain"]), "");
    let (code, out) = run(&repo, &["migrate", "--rollback"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(snapshot(&repo), before);
}

#[test]
fn a_ref_to_a_node_on_another_branch_gets_that_branchs_id() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    // Branch `other` adds spike-1; main refers to it before it merges.
    let other = dir.0.join("other");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "other",
            other.to_str().expect("utf-8"),
            "main",
        ],
    );
    let spike = "---\nid: spike-1\ntype: spike\ntitle: Look into it\nstatus: todo\ncreated: 2026-10-20\nupdated: 2026-10-20\n---\n";
    fs::write(other.join(".mdkg/work/spike-1-look.md"), spike).expect("spike");
    git(&other, &["add", "."]);
    git(&other, &["commit", "-q", "-m", "spike"]);
    fs::write(
        repo.join(".mdkg/work/task-2-second.md"),
        TASK_2.replace("status: backlog", "status: backlog\nrefs: [spike-1]"),
    )
    .expect("ref");
    git(&repo, &["commit", "-q", "-am", "ref"]);

    let content = |worktree: &Path, path: &str| -> String {
        let (code, out) = run(worktree, &["migrate", "--dry-run"]);
        assert_eq!(code, 0, "{out}");
        let plan = Path::new(out["data"]["plan_file"].as_str().expect("plan")).to_path_buf();
        let stored: Value = serde_json::from_slice(&fs::read(plan).expect("plan")).expect("json");
        stored["contents"][path].as_str().expect(path).to_string()
    };
    let on_main = content(&repo, ".mdkg/work/task-2-second.md");
    let spike_id = content(&other, ".mdkg/work/spike-1-look.md")
        .lines()
        .find_map(|l| l.strip_prefix("id: ").map(str::to_string))
        .expect("id");
    assert!(spike_id.starts_with("spike-"), "{spike_id}");
    // 0.7.0 refuses a ref to a node the branch lacks, so the body keeps it,
    // by the id spike-1's own branch gives it.
    assert!(on_main.contains("refs: []"), "{on_main}");
    assert!(
        on_main.contains(&format!("refs: {spike_id} (spike-1, on another branch)")),
        "{on_main}"
    );
}

/// A worktree on a new branch `name`, holding `files` (graph folder
/// relative) committed on top of main.
fn branch_with(dir: &Scratch, repo: &Path, name: &str, files: &[(&str, &str)]) -> PathBuf {
    let wt = dir.0.join(name);
    git(
        repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            name,
            wt.to_str().expect("utf-8"),
            "main",
        ],
    );
    for (path, text) in files {
        fs::write(wt.join(".mdkg").join(path), text).expect("write");
    }
    git(&wt, &["add", "."]);
    git(&wt, &["commit", "-q", "-m", name]);
    wt
}

/// task-2's content in the plan of `worktree`, with `refs` set to `refs`
/// on main first.
fn planned_task2(repo: &Path, refs: &str) -> String {
    planned_task2_with(repo, refs, &[])
}

fn planned_task2_with(repo: &Path, refs: &str, extra: &[&str]) -> String {
    fs::write(
        repo.join(".mdkg/work/task-2-second.md"),
        TASK_2.replace(
            "status: backlog",
            &format!("status: backlog\nrefs: [{refs}]"),
        ),
    )
    .expect("ref");
    git(repo, &["commit", "-q", "-am", "ref"]);
    let mut args = vec!["migrate", "--dry-run"];
    args.extend_from_slice(extra);
    let (code, out) = run(repo, &args);
    assert_eq!(code, 0, "{out}");
    let plan = Path::new(out["data"]["plan_file"].as_str().expect("plan")).to_path_buf();
    let stored: Value = serde_json::from_slice(&fs::read(plan).expect("plan")).expect("json");
    stored["contents"][".mdkg/work/task-2-second.md"]
        .as_str()
        .expect("task-2")
        .to_string()
}

const SPIKE: &str = "---\nid: spike-1\ntype: spike\ntitle: Look into it\nstatus: todo\nx_acme_owner: nick\ncreated: 2026-10-20\nupdated: 2026-10-20\n---\n";

#[test]
fn a_ref_to_a_finished_test_on_another_branch_is_kept() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    let done_test = TEST_1.replace("test-1", "test-9");
    branch_with(
        &dir,
        &repo,
        "other",
        &[("work/test-9-proof.md", &done_test)],
    );
    // Kept in git there too, it's a node with an id once that branch
    // migrates: the body keeps the ref by that id.
    let task2 = planned_task2(&repo, "test-9");
    assert!(task2.contains("refs: []"), "{task2}");
    assert!(task2.contains("refs: test-0"), "{task2}");
    assert!(task2.contains(" (test-9, on another branch)"), "{task2}");
    // Moved to the store there, it has no id to point at.
    git(&repo, &["reset", "-q", "--hard", "HEAD^"]);
    let task2 = planned_task2_with(&repo, "test-9", &["--work-history", "store"]);
    assert!(
        task2.contains("refs: test-9 (a checkpoint or finished test on another branch)"),
        "{task2}"
    );
}

#[test]
fn one_node_at_two_paths_on_two_branches_stays_unresolved() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    branch_with(&dir, &repo, "one", &[("work/spike-1-a.md", SPIKE)]);
    branch_with(&dir, &repo, "two", &[("work/spike-1-b.md", SPIKE)]);
    let task2 = planned_task2(&repo, "spike-1");
    assert!(task2.contains("refs: []"), "{task2}");
    assert!(task2.contains("frontmatter: refs: spike-1\n"), "{task2}");
    let (_, out) = run(&repo, &["migrate", "--dry-run"]);
    let warned = out["data"]["warnings"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|w| w["code"] == "migrate.ambiguous_ref");
    assert!(warned, "{out}");
}

#[test]
fn a_ref_to_a_node_another_branch_already_migrated_gets_its_id() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    let other = branch_with(&dir, &repo, "other", &[("work/spike-1-look.md", SPIKE)]);
    // The other branch migrates first; its spike-1 now has x- keys.
    let hash = dry_run(&other);
    let (code, out) = run(&other, &["migrate", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
    let migrated = fs::read_to_string(other.join(".mdkg/work/spike-1-look.md")).expect("spike");
    assert!(migrated.contains("x-acme.owner: nick"), "{migrated}");
    let spike_id = migrated
        .lines()
        .find_map(|l| l.strip_prefix("id: ").map(str::to_string))
        .expect("id");
    let task2 = planned_task2(&repo, "spike-1");
    assert!(
        task2.contains(&format!("refs: {spike_id} (spike-1, on another branch)")),
        "{task2}"
    );
}

#[test]
fn rollback_refuses_to_discard_a_staged_addition() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    let hash = dry_run(&repo);
    let (code, _) = run_env(&repo, &["migrate", "--apply", &hash], Some("swapped"));
    assert_eq!(code, 86);
    let added = repo.join(".mdkg/work/task-9-new.md");
    fs::write(&added, "---\nid: task-9\n---\n").expect("add");
    git(&repo, &["add", ".mdkg/work/task-9-new.md"]);
    let (code, out) = run(&repo, &["migrate", "--rollback"]);
    assert_ne!(code, 0);
    assert!(codes(&out).contains(&"migrate.dirty".into()), "{out}");
    assert!(added.exists());
}

#[test]
fn a_resumed_rollback_still_refuses_to_discard_edits() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    let hash = dry_run(&repo);
    let (code, out) = run(&repo, &["migrate", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
    let (code, _) = run_env(&repo, &["migrate", "--rollback"], Some("rolling-back"));
    assert_eq!(code, 86);
    let task2 = repo.join(".mdkg/work/task-2-second.md");
    let edited = fs::read_to_string(&task2)
        .expect("read")
        .replace("Body.", "Edited.");
    fs::write(&task2, &edited).expect("edit");
    let (code, out) = run(&repo, &["migrate", "--rollback"]);
    assert_ne!(code, 0);
    assert!(codes(&out).contains(&"migrate.dirty".into()), "{out}");
    assert_eq!(fs::read_to_string(&task2).expect("read"), edited);
}

#[test]
fn restoring_one_file_after_the_swap_doesnt_fool_rollback() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    let before = snapshot(&repo);
    let hash = dry_run(&repo);
    let (code, _) = run_env(&repo, &["migrate", "--apply", &hash], Some("swapped"));
    assert_eq!(code, 86);
    // Only the config goes back to the base.
    git(&repo, &["restore", ".mdkg/config.json"]);
    let (code, out) = run(&repo, &["migrate", "--rollback"]);
    assert_ne!(code, 0, "a mixed graph isn't the old one: {out}");
    assert!(codes(&out).contains(&"migrate.dirty".into()), "{out}");
    // Forced, the whole old graph comes back.
    let (code, out) = run(&repo, &["migrate", "--rollback", "--force"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(snapshot(&repo), before);
}

#[test]
fn rollback_refuses_to_drop_work_staged_only_in_the_index() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    let hash = dry_run(&repo);
    let (code, out) = run(&repo, &["migrate", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
    let (code, _) = run_env(&repo, &["migrate", "--rollback"], Some("rolling-back"));
    assert_eq!(code, 86);
    // A node staged, then its file removed: it lives only in the index.
    let added = repo.join(".mdkg/work/task-9-new.md");
    fs::write(&added, "---\nid: task-9\n---\n").expect("add");
    git(&repo, &["add", ".mdkg/work/task-9-new.md"]);
    fs::remove_file(&added).expect("rm");
    let (code, out) = run(&repo, &["migrate", "--rollback"]);
    assert_ne!(code, 0);
    assert!(codes(&out).contains(&"migrate.dirty".into()), "{out}");
    assert!(git(&repo, &["ls-files", "--", ".mdkg/work/task-9-new.md"]).contains("task-9"));
}

#[test]
fn a_tracked_file_an_ignore_rule_matches_still_migrates() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    let script = repo.join(".mdkg/skills/tool/run.sh");
    fs::create_dir_all(script.parent().expect("dir")).expect("dir");
    fs::write(&script, "echo hi\n").expect("script");
    fs::write(
        script.with_file_name("SKILL.md"),
        "---\nname: tool\ndescription: A tool.\n---\nRuns it.\n",
    )
    .expect("skill");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "script"]);
    fs::write(repo.join(".gitignore"), ".mdkg/index/\n*.sh\n").expect("ignore");
    git(&repo, &["commit", "-q", "-am", "ignore scripts"]);
    let before = snapshot(&repo);
    let hash = dry_run(&repo);
    let (code, out) = run(&repo, &["migrate", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
    let (code, out) = run(&repo, &["migrate", "--rollback"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(snapshot(&repo), before);
}

#[test]
fn a_resumed_apply_keeps_an_edit_to_a_patched_file() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    let hash = dry_run(&repo);
    let (code, _) = run_env(
        &repo,
        &["migrate", "--apply", &hash, "--rewrite-refs"],
        Some("swapped"),
    );
    assert_eq!(code, 86);
    let agents = repo.join("AGENTS.md");
    fs::write(&agents, "Edited by hand.\n").expect("edit");
    let (code, out) = run(&repo, &["migrate", "--apply", &hash, "--rewrite-refs"]);
    assert_ne!(code, 0);
    assert!(codes(&out).contains(&"migrate.dirty".into()), "{out}");
    assert_eq!(
        fs::read_to_string(&agents).expect("read"),
        "Edited by hand.\n"
    );
}

#[test]
fn an_edit_racing_a_patch_write_is_kept() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    let agents = repo.join("AGENTS.md");
    let race = [
        ("MDKG_MIGRATE_RACE_FILE", agents.to_str().expect("utf-8")),
        ("MDKG_MIGRATE_RACE_TEXT", "Saved in the moment.\n"),
    ];
    // The editor saves between the check and the replacement.
    let hash = dry_run(&repo);
    let (code, out) = run_with(
        &repo,
        &["migrate", "--apply", &hash, "--rewrite-refs"],
        None,
        &race,
    );
    let out = out.expect("json");
    assert_ne!(code, 0, "{out}");
    assert!(codes(&out).contains(&"migrate.dirty".into()), "{out}");
    assert_eq!(
        fs::read_to_string(&agents).expect("read"),
        "Saved in the moment.\n"
    );
    // Undone, the same apply finishes.
    fs::write(&agents, AGENTS).expect("undo");
    let (code, out) = run(&repo, &["migrate", "--apply", &hash, "--rewrite-refs"]);
    assert_eq!(code, 0, "{out}");
    // Rollback's undo of the patch: the same guard.
    let (code, out) = run_with(&repo, &["migrate", "--rollback"], None, &race);
    let out = out.expect("json");
    assert_ne!(code, 0, "{out}");
    assert!(codes(&out).contains(&"migrate.dirty".into()), "{out}");
    assert_eq!(
        fs::read_to_string(&agents).expect("read"),
        "Saved in the moment.\n"
    );
}

/// The kinds of the records in the coordinator store's log.
fn log_kinds(repo: &Path) -> Vec<String> {
    let db = repo.join(".git/mdkg/coord.sqlite");
    let conn =
        rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("store");
    let mut stmt = conn
        .prepare("SELECT record FROM log ORDER BY seq")
        .expect("log");
    stmt.query_map([], |r| r.get::<_, String>(0))
        .expect("rows")
        .map(|t| {
            let v: Value = serde_json::from_str(&t.expect("row")).expect("json");
            v["kind"].as_str().unwrap_or_default().to_string()
        })
        .collect()
}

#[test]
fn records_reach_the_store_once_and_rollback_takes_them_out() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    let hash = dry_run_store(&repo);
    let (code, out) = run(
        &repo,
        &["migrate", "--apply", &hash, "--work-history", "store"],
    );
    assert_eq!(code, 0, "{out}");
    let kinds = log_kinds(&repo);
    let count = |k: &str| kinds.iter().filter(|x| *x == k).count();
    assert_eq!(count("log.import"), 1, "{kinds:?}");
    assert_eq!(count("legacy.checkpoint"), 1);
    assert_eq!(count("check.result"), 1);
    assert_eq!(count("legacy.event"), 1);
    assert_eq!(count("review.record"), 1);
    // A crash after the import and a resumed apply import nothing again.
    let (code, out) = run(&repo, &["migrate", "--rollback"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        log_kinds(&repo)
            .iter()
            .filter(|k| *k == "log.import")
            .count(),
        0
    );
    let (code, _) = run_env(
        &repo,
        &["migrate", "--apply", &hash, "--work-history", "store"],
        Some("recorded"),
    );
    assert_eq!(code, 86);
    let (code, out) = run(
        &repo,
        &["migrate", "--apply", &hash, "--work-history", "store"],
    );
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        log_kinds(&repo)
            .iter()
            .filter(|k| *k == "log.import")
            .count(),
        1
    );
}

#[test]
fn a_store_this_build_cant_open_stops_the_migration_first() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    let (code, out) = run(&repo, &["claim", "task-2"]);
    assert_eq!(code, 0, "{out}");
    // An older schema: a build-folder binary may not upgrade it.
    rusqlite::Connection::open(repo.join(".git/mdkg/coord.sqlite"))
        .expect("store")
        .execute("UPDATE meta SET value = '2' WHERE key = 'schema'", [])
        .expect("schema");
    let before = snapshot(&repo);
    let hash = dry_run_store(&repo);
    let (code, out) = run(
        &repo,
        &["migrate", "--apply", &hash, "--work-history", "store"],
    );
    assert_ne!(code, 0, "{out}");
    assert_eq!(snapshot(&repo), before, "nothing changed");
    let (code, _) = run(&repo, &["migrate", "--status"]);
    assert_ne!(code, 0, "no migration started");
}

#[test]
fn every_summary_digest_names_a_record_in_the_store() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    let hash = dry_run_store(&repo);
    let (code, out) = run(
        &repo,
        &["migrate", "--apply", &hash, "--work-history", "store"],
    );
    assert_eq!(code, 0, "{out}");
    let db = repo.join(".git/mdkg/coord.sqlite");
    let conn =
        rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("store");
    let mut stmt = conn.prepare("SELECT record FROM log").expect("log");
    let digests: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .expect("rows")
        .filter_map(|t| {
            let v: Value = serde_json::from_str(&t.ok()?).ok()?;
            v["body"]["migrate_digest"].as_str().map(str::to_string)
        })
        .collect();
    let mut cited = 0;
    for (_, bytes) in snapshot(&repo)
        .iter()
        .filter(|(p, _)| p.starts_with(".mdkg/work/"))
    {
        let text = String::from_utf8_lossy(bytes);
        for part in text.split("migration digest ").skip(1) {
            let digest: String = part.chars().take(7 + 64).collect();
            assert!(digests.contains(&digest), "{digest} isn't in the store");
            cited += 1;
        }
    }
    assert_eq!(
        cited, 3,
        "the checkpoint, the check result and the review record"
    );
}

#[test]
fn a_write_between_the_snapshot_and_the_import_refuses_rollback() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    let (code, out) = run(&repo, &["claim", "task-2"]);
    assert_eq!(code, 0, "{out}");
    let hash = dry_run_store(&repo);
    // A crash after the backups (the snapshot is taken), then an agent
    // writes before the import.
    let (code, _) = run_env(
        &repo,
        &["migrate", "--apply", &hash, "--work-history", "store"],
        Some("backed-up"),
    );
    assert_eq!(code, 86);
    let (code, out) = run(&repo, &["release", "task-2"]);
    assert_eq!(code, 0, "{out}");
    let (code, out) = run(
        &repo,
        &["migrate", "--apply", &hash, "--work-history", "store"],
    );
    assert_eq!(code, 0, "{out}");
    let (code, out) = run(&repo, &["migrate", "--rollback"]);
    assert_ne!(code, 0);
    assert!(
        codes(&out).contains(&"migrate.store_changed".into()),
        "{out}"
    );
    let message = out["diagnostics"][0]["message"]
        .as_str()
        .unwrap_or_default();
    assert!(message.contains("lease.release"), "{message}");
    // --force restores anyway.
    let (code, out) = run(&repo, &["migrate", "--rollback", "--force"]);
    assert_eq!(code, 0, "{out}");
}

#[test]
fn a_crash_after_the_import_still_guards_rollback() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    let hash = dry_run_store(&repo);
    let (code, _) = run_env(
        &repo,
        &["migrate", "--apply", &hash, "--work-history", "store"],
        Some("recorded"),
    );
    assert_eq!(code, 86);
    // An agent writes after the import committed, before the swap.
    let (code, out) = run(&repo, &["claim", "task-2"]);
    assert_eq!(code, 0, "{out}");
    let (code, out) = run(&repo, &["migrate", "--rollback"]);
    assert_ne!(code, 0);
    assert!(
        codes(&out).contains(&"migrate.store_changed".into()),
        "{out}"
    );
    let message = out["diagnostics"][0]["message"]
        .as_str()
        .unwrap_or_default();
    assert!(message.contains("lease.claim"), "{message}");
}

#[test]
fn a_migration_that_wouldnt_validate_stops_before_the_swap() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    let before = snapshot(&repo);
    let hash = dry_run(&repo);
    // The staged graph breaks (the test hook), as a planner bug would.
    let (code, out) = run_with(
        &repo,
        &["migrate", "--apply", &hash],
        None,
        &[("MDKG_MIGRATE_BREAK_STAGED", "1")],
    );
    let out = out.expect("json");
    assert_ne!(code, 0, "{out}");
    assert!(codes(&out).contains(&"migrate.step_failed".into()), "{out}");
    let message = out["diagnostics"][0]["message"]
        .as_str()
        .unwrap_or_default();
    assert!(message.contains("doesn't validate"), "{message}");
    assert!(
        repo.join(".mdkg/work/chk-1-first.md").exists(),
        "still the old graph"
    );
    let (code, out) = run(&repo, &["migrate", "--rollback"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(snapshot(&repo), before);
}

#[test]
fn a_store_write_racing_the_restore_is_refused_not_lost() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    let hash = dry_run_store(&repo);
    let (code, out) = run(
        &repo,
        &["migrate", "--apply", &hash, "--work-history", "store"],
    );
    assert_eq!(code, 0, "{out}");
    // Another writer appends between rollback's check and its restore.
    let (code, out) = run_with(
        &repo,
        &["migrate", "--rollback"],
        None,
        &[("MDKG_MIGRATE_RACE_STORE", "1")],
    );
    let out = out.expect("json");
    assert_ne!(code, 0, "{out}");
    assert!(
        codes(&out).contains(&"migrate.store_changed".into()),
        "{out}"
    );
    // The racing write survives: the log still holds it, and the import.
    let kinds = log_kinds(&repo);
    assert_eq!(
        kinds.iter().filter(|k| *k == "log.import").count(),
        2,
        "{kinds:?}"
    );
    // Run again: refused up front, naming the write.
    let (code, out) = run(&repo, &["migrate", "--rollback"]);
    assert_ne!(code, 0);
    let message = out["diagnostics"][0]["message"]
        .as_str()
        .unwrap_or_default();
    assert!(message.contains("log.import"), "{message}");
}

#[test]
fn a_graph_with_no_records_migrates_and_rolls_back_without_a_store() {
    let dir = scratch();
    let repo = dir.0.join("plain");
    let conformance = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance");
    fs::create_dir_all(repo.join(".mdkg/work")).expect("repo");
    copy_tree(&conformance.join("base"), &repo);
    fs::write(repo.join(".mdkg/work/task-2-second.md"), TASK_2).expect("task");
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    let before = snapshot(&repo);
    let hash = dry_run(&repo);
    let (code, out) = run(&repo, &["migrate", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["data"]["counts"]["records"], 0);
    assert!(
        !repo.join(".git/mdkg/coord.sqlite").exists(),
        "no store was made"
    );
    let (code, out) = run(&repo, &["migrate", "--rollback"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(snapshot(&repo), before);
}

#[test]
fn a_broken_body_link_is_caught_before_head_moves() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    // A 0.7-shaped link to a node nobody has: 0.6.0 reads it as text, 0.7.0
    // refuses it.
    fs::write(
        repo.join(".mdkg/work/task-2-second.md"),
        TASK_2.replace(
            "Body.",
            "See [later](task-0192f3a4-7b2c-7def-8a9b-1c2d3e4f5a6b).",
        ),
    )
    .expect("link");
    git(&repo, &["commit", "-q", "-am", "link"]);
    let before = snapshot(&repo);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    let hash = dry_run(&repo);
    let (code, out) = run(&repo, &["migrate", "--apply", &hash]);
    assert_ne!(code, 0, "{out}");
    let message = out["diagnostics"][0]["message"]
        .as_str()
        .unwrap_or_default();
    assert!(message.contains("node.bad_body_link"), "{message}");
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), base);
    assert!(
        repo.join(".mdkg/work/chk-1-first.md").exists(),
        "the old graph is back"
    );
    let (code, out) = run(&repo, &["migrate", "--rollback"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(snapshot(&repo), before);
}

#[test]
fn a_key_only_a_local_template_allows_becomes_an_extension_key() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    // The templates live in schemas/, and a task template allows `colour`.
    fs::create_dir_all(repo.join("schemas")).expect("schemas");
    copy_tree(&repo.join(".mdkg/templates"), &repo.join("schemas"));
    fs::remove_dir_all(repo.join(".mdkg/templates")).expect("rm");
    let task_template = repo.join("schemas/default/task.md");
    let text = fs::read_to_string(&task_template).expect("template");
    fs::write(
        &task_template,
        text.replacen("tags: []", "colour: {{colour}}\ntags: []", 1),
    )
    .expect("template");
    let config_path = repo.join(".mdkg/config.json");
    let mut config: Value =
        serde_json::from_str(&fs::read_to_string(&config_path).expect("config")).expect("json");
    config["templates"]["root_path"] = Value::String("schemas".into());
    fs::write(
        &config_path,
        serde_json::to_string_pretty(&config).expect("json"),
    )
    .expect("config");
    fs::write(
        repo.join(".mdkg/work/task-2-second.md"),
        TASK_2.replace("status: backlog", "status: backlog\ncolour: blue"),
    )
    .expect("colour");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "outside templates"]);
    // 0.6.0 reads it (its templates allow `colour`); format 0.7's node
    // schema has no such key, so it becomes x-local.colour, value unchanged.
    let (code, out) = run(&repo, &["migrate", "--dry-run"]);
    assert_eq!(code, 0, "{out}");
    let converted: Vec<String> = out["data"]["warnings"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|w| w["code"] == "migrate.key_converted")
        .filter_map(|w| w["message"].as_str().map(String::from))
        .collect();
    assert_eq!(converted.len(), 1, "{converted:?}");
    assert!(
        converted[0]
            .contains("colour isn't in format 0.7's node schema; it becomes x-local.colour"),
        "{converted:?}"
    );
    let hash = out["data"]["plan"].as_str().expect("plan").to_string();
    let (code, out) = run(&repo, &["migrate", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
    let task2 = fs::read_to_string(repo.join(".mdkg/work/task-2-second.md")).expect("read");
    assert!(task2.contains("\nx-local.colour: blue\n"), "{task2}");
    let (code, out) = run(&repo, &["validate"]);
    assert_eq!(code, 0, "{out}");
}

#[test]
fn an_edit_saved_into_the_new_graph_before_validation_is_kept() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    let hash = dry_run(&repo);
    // An editor saves a 0.6.0 node into the new graph while it's in place,
    // and that's what fails validation.
    let raced = TASK_2.replace("task-2", "task-90");
    let (code, out) = run_with(
        &repo,
        &["migrate", "--apply", &hash],
        None,
        &[("MDKG_MIGRATE_RACE_GRAPH", raced.as_str())],
    );
    let out = out.expect("json");
    assert_ne!(code, 0, "{out}");
    let message = out["diagnostics"][0]["message"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(message.contains("is kept in"), "{message}");
    let kept = message.rsplit(" in ").next().expect("path").to_string();
    let saved = Path::new(&kept).join("work/task-90-raced.md");
    assert_eq!(fs::read_to_string(&saved).expect("kept"), raced);
    // Neither a rollback nor a retry deletes it.
    let (code, out) = run(&repo, &["migrate", "--rollback"]);
    assert_eq!(code, 0, "{out}");
    assert!(saved.exists());
    assert!(repo.join(".mdkg/work/chk-1-first.md").exists());
}

#[test]
fn an_ignored_node_saved_into_the_new_graph_is_kept_too() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    fs::write(
        repo.join(".gitignore"),
        ".mdkg/index/\n.mdkg/work/task-90-raced.md\n",
    )
    .expect("ignore");
    git(&repo, &["commit", "-q", "-am", "ignore"]);
    let hash = dry_run(&repo);
    let raced = TASK_2.replace("task-2", "task-90");
    let (code, out) = run_with(
        &repo,
        &["migrate", "--apply", &hash],
        None,
        &[("MDKG_MIGRATE_RACE_GRAPH", raced.as_str())],
    );
    let out = out.expect("json");
    assert_ne!(code, 0, "{out}");
    let message = out["diagnostics"][0]["message"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let kept = message.rsplit(" in ").next().expect("path").to_string();
    let saved = Path::new(&kept).join("work/task-90-raced.md");
    // A retry, then a rollback, and it's still there.
    let (_, _) = run(&repo, &["migrate", "--apply", &hash]);
    let (code, out) = run(&repo, &["migrate", "--rollback"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(fs::read_to_string(&saved).expect("kept"), raced);
}

/// The `log.import` markers' bodies, in order.
fn log_markers(repo: &Path) -> Vec<Value> {
    let db = repo.join(".git/mdkg/coord.sqlite");
    let conn =
        rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("store");
    let mut stmt = conn
        .prepare("SELECT record FROM log ORDER BY seq")
        .expect("log");
    stmt.query_map([], |r| r.get::<_, String>(0))
        .expect("rows")
        .filter_map(|t| serde_json::from_str::<Value>(&t.ok()?).ok())
        .filter(|v| v["kind"] == "log.import")
        .map(|v| v["body"].clone())
        .collect()
}

#[test]
#[ignore = "the branch-against-main mode is cut from 0.7.1 (back in 0.7.2)"]
fn a_branch_migrated_after_main_merges_cleanly() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    // An unmerged branch with one node of its own.
    let other = branch_with(&dir, &repo, "other", &[("work/spike-1-look.md", SPIKE)]);
    // main migrates, then the branch migrates on its own, before merging.
    let hash = dry_run_store(&repo);
    let (code, out) = run(
        &repo,
        &["migrate", "--apply", &hash, "--work-history", "store"],
    );
    assert_eq!(code, 0, "{out}");
    let hash = dry_run_store(&other);
    let (code, out) = run(
        &other,
        &["migrate", "--apply", &hash, "--work-history", "store"],
    );
    assert_eq!(code, 0, "{out}");
    // Both imports ran, and the branch's skipped what main's already
    // imported: the store judges that by migrate_digest inside the
    // import's own transaction (lane 2), so each shared record is there
    // once.
    // Planned against the migrated main, the branch takes main's files for
    // all it shares with main, and this store already has main's records,
    // so it has none to import: each shared record is in the store once,
    // from main's import.
    let kinds = log_kinds(&repo);
    let count = |k: &str| kinds.iter().filter(|x| *x == k).count();
    assert_eq!(count("log.import"), 1, "{kinds:?}");
    let markers = log_markers(&repo);
    assert_eq!(markers[0]["skipped"], 0, "main's import skips nothing");
    for shared in [
        "legacy.checkpoint",
        "check.result",
        "legacy.event",
        "review.record",
    ] {
        assert_eq!(count(shared), 1, "{shared}: {kinds:?}");
    }
    let db = repo.join(".git/mdkg/coord.sqlite");
    let conn =
        rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("store");
    let mut stmt = conn.prepare("SELECT record FROM log").expect("log");
    let mut digests: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .expect("rows")
        .filter_map(|t| {
            let v: Value = serde_json::from_str(&t.ok()?).ok()?;
            v["body"]["migrate_digest"].as_str().map(str::to_string)
        })
        .collect();
    digests.sort();
    let mut unique = digests.clone();
    unique.dedup();
    assert_eq!(digests.len(), unique.len(), "each shared digest once");
    // The shared files came out byte for byte the same: the merge is clean.
    git(&repo, &["merge", "-q", "--no-edit", "other"]);
    assert!(repo.join(".mdkg/work/spike-1-look.md").exists());
    let (code, out) = run(&repo, &["validate"]);
    assert_eq!(code, 0, "{out}");
}

#[test]
fn a_staged_submodule_pointer_is_refused_up_front() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    // A gitlink staged in the index: git status, ignoring submodules, hides it.
    let head = git(&repo, &["rev-parse", "HEAD"]);
    git(
        &repo,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{head},vendor/sub"),
        ],
    );
    let hash = dry_run(&repo);
    let (code, out) = run(&repo, &["migrate", "--apply", &hash]);
    assert_ne!(code, 0, "{out}");
    assert!(codes(&out).contains(&"migrate.dirty".into()), "{out}");
    assert!(git(&repo, &["ls-files", "--stage", "--", "vendor/sub"]).starts_with("160000"));
    assert!(repo.join(".mdkg/work/chk-1-first.md").exists());
}

#[test]
fn a_forced_rollback_keeps_a_staged_submodule_pointer() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    let hash = dry_run(&repo);
    let (code, out) = run(&repo, &["migrate", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
    let head = git(&repo, &["rev-parse", "HEAD"]);
    git(
        &repo,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{head},vendor/sub"),
        ],
    );
    let (code, out) = run(&repo, &["migrate", "--rollback", "--force"]);
    assert_eq!(code, 0, "{out}");
    let staged = git(&repo, &["ls-files", "--stage", "--", "vendor/sub"]);
    assert!(staged.starts_with(&format!("160000 {head}")), "{staged}");
    assert!(repo.join(".mdkg/work/chk-1-first.md").exists());
}

#[test]
fn a_pointer_staged_during_the_apply_is_kept() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    let head = git(&repo, &["rev-parse", "HEAD"]);
    let hash = dry_run(&repo);
    let entry = format!("160000,{head},vendor/race");
    let (code, out) = run_with(
        &repo,
        &["migrate", "--apply", &hash],
        None,
        &[("MDKG_MIGRATE_RACE_INDEX", entry.as_str())],
    );
    assert_eq!(code, 0, "{out:?}");
    let staged = git(&repo, &["ls-files", "--stage", "--", "vendor/race"]);
    assert!(staged.starts_with(&format!("160000 {head}")), "{staged}");
    // The migration itself is the index's, too.
    let staged = git(&repo, &["show", ":.mdkg/work/chk-1-first.md"]);
    assert!(staged.contains("\nlegacy_id: chk-1\n"), "{staged}");
}

#[test]
fn an_unmerged_entry_appearing_during_the_apply_is_refused_and_kept() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    let hash = dry_run(&repo);
    let (code, out) = run_with(
        &repo,
        &["migrate", "--apply", &hash],
        None,
        &[("MDKG_MIGRATE_RACE_UNMERGED", "AGENTS.md")],
    );
    let out = out.expect("json");
    assert_ne!(code, 0, "{out}");
    // The conflict stages are still there, for their owner to resolve.
    let stages = git(&repo, &["ls-files", "--stage", "--", "AGENTS.md"]);
    assert!(
        stages.contains(" 1\tAGENTS.md") && stages.contains(" 2\tAGENTS.md"),
        "{stages}"
    );
    // A rollback while the conflict stands is refused before it starts, so
    // the run is still the apply's to finish.
    let (code, out) = run(&repo, &["migrate", "--rollback"]);
    assert_ne!(code, 0, "{out}");
    assert!(codes(&out).contains(&"migrate.dirty".into()), "{out}");
    // Resolved, the same apply finishes.
    git(&repo, &["reset", "-q", "--", "AGENTS.md"]);
    let (code, out) = run(&repo, &["migrate", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["data"]["stage"], "done");
    assert_eq!(git(&repo, &["status", "--porcelain"]), "");
}

#[test]
fn a_rollback_that_stopped_is_finished_by_rollback_not_apply() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    let hash = dry_run(&repo);
    let (code, out) = run(&repo, &["migrate", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
    let (code, _) = run_env(&repo, &["migrate", "--rollback"], Some("rolling-back"));
    assert_eq!(code, 86);
    let (code, out) = run(&repo, &["migrate", "--apply", &hash]);
    assert_ne!(code, 0, "{out}");
    assert!(codes(&out).contains(&"migrate.in_progress".into()), "{out}");
    let (code, out) = run(&repo, &["migrate", "--rollback"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["data"]["stage"], "rolled-back");
}

const V2_GRAPH: &str = "6f1c2b3a-4d5e-4f60-8a7b-9c0d1e2f3a4b";
const V2_TASK_1: &str = "1b2c3d4e-5f60-4718-9a2b-3c4d5e6f7a8b";
const V2_GOAL_1: &str = "3a2b1c0d-9e8f-4a7b-b6c5-d4e3f2a1b0c9";
const V2_CHK_1: &str = "4d5e6f70-8192-4a3b-8c4d-5e6f708192a3";

/// `conformance/identity-v2` in a repository, with a checkpoint on task-1,
/// a body naming the goal by identity ref in prose and in code, and
/// 0.6.0's identity migration receipt.
fn v2_repo(dir: &Scratch) -> PathBuf {
    let repo = dir.0.join("v2");
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance/identity-v2");
    fs::create_dir_all(repo.join(".mdkg/identity/migrations")).expect("repo");
    copy_tree(&fixture.join(".mdkg"), &repo.join(".mdkg"));
    let goal = format!("mdkg://{V2_GRAPH}/{V2_GOAL_1}");
    let chk = format!(
        "---\nid: chk-1\ngraph_id: {V2_GRAPH}\nnode_id: {V2_CHK_1}\ntype: checkpoint\n\
         title: Started\ncheckpoint_kind: implementation\nstatus: done\n\
         scope: [mdkg://{V2_GRAPH}/{V2_TASK_1}]\ncreated: 2026-01-17\nupdated: 2026-01-17\n---\n\
         # Summary\n\nStarted.\n"
    );
    fs::write(repo.join(".mdkg/work/chk-1-started.md"), chk).expect("chk");
    let task1 = repo.join(".mdkg/work/task-1-start.md");
    let text = fs::read_to_string(&task1).expect("task-1");
    let text = text.replace(
        "Start here.\n",
        &format!("Start here, for {goal} (`mdkg show {goal}`).\n"),
    );
    fs::write(&task1, text).expect("task-1");
    fs::write(
        repo.join(".mdkg/identity/migrations/receipt.json"),
        "{\"receipt\":true}\n",
    )
    .expect("receipt");
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    repo
}

#[test]
#[ignore = "the branch-against-main mode is cut from 0.7.1 (back in 0.7.2)"]
fn an_identity_v2_node_main_got_by_a_cherry_pick_keeps_its_one_id() {
    let dir = scratch();
    let repo = v2_repo(&dir);
    // A node the branch made after the fork, which main got by a
    // cherry-pick: no shared history, the same UUID.
    let t3 = format!(
        "---\nid: task-3\ngraph_id: {V2_GRAPH}\nnode_id: 5e6f7081-92a3-4b4c-9d5e-6f708192a3b4\n\
         type: task\ntitle: Later\nstatus: todo\ncreated: 2026-01-20\nupdated: 2026-01-20\n---\n"
    );
    let other = branch_with(&dir, &repo, "other", &[("work/task-3-later.md", &t3)]);
    fs::write(repo.join(".mdkg/work/task-3-later.md"), &t3).expect("t3");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "cherry-pick task-3"]);
    let hash = dry_run(&repo);
    let (code, out) = run(&repo, &["migrate", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
    let main_t3 = id_of(&fs::read_to_string(repo.join(".mdkg/work/task-3-later.md")).expect("t3"));
    assert!(
        main_t3.ends_with("5e6f7081-92a3-4b4c-9d5e-6f708192a3b4"),
        "{main_t3}"
    );
    // Main renames it after its migration: the branch's copy is still the
    // one node, not added again from main's new path.
    git(
        &repo,
        &[
            "mv",
            ".mdkg/work/task-3-later.md",
            ".mdkg/work/task-3-renamed.md",
        ],
    );
    git(&repo, &["commit", "-qm", "rename task-3"]);
    let (code, out) = run(&other, &["migrate", "--dry-run"]);
    assert_eq!(code, 0, "{out}");
    assert!(
        !codes_in(&out["data"]["warnings"]).contains(&"migrate.legacy_id_collision".to_string()),
        "{out}"
    );
    let ours = planned(&out, ".mdkg/work/task-3-later.md");
    assert_eq!(id_of(&ours), main_t3, "{ours}");
    assert!(ours.contains("\nlegacy_id: task-3\n"), "{ours}");
    assert_eq!(planned(&out, ".mdkg/work/task-3-renamed.md"), "", "{out}");
    let hash = out["data"]["plan"].as_str().expect("plan").to_string();
    let (code, out) = run(&other, &["migrate", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
}

#[test]
fn an_identity_v2_created_naming_no_real_day_is_refused_at_plan_time() {
    // 0.6.0 checked only the digits, and a v2 node keeps its `created`, so
    // the plan must say so, not the migrated graph's validation.
    let dir = scratch();
    let repo = v2_repo(&dir);
    let path = repo.join(".mdkg/work/task-1-start.md");
    let text = fs::read_to_string(&path).expect("task-1");
    fs::write(
        &path,
        text.replace("created: 2026-01-15", "created: 2026-02-30"),
    )
    .expect("task-1");
    git(&repo, &["commit", "-qam", "a day that doesn't exist"]);
    let (code, out) = run(&repo, &["migrate", "--dry-run"]);
    assert_ne!(code, 0, "{out}");
    assert!(
        out.to_string()
            .contains("task-1's created isn't a YYYY-MM-DD date that exists"),
        "{out}"
    );
}

#[test]
fn an_identity_v2_graph_keeps_its_uuids() {
    let dir = scratch();
    let repo = v2_repo(&dir);
    let before = snapshot(&repo);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    let (code, out) = run(&repo, &["migrate", "--dry-run"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["data"]["graph_id"], V2_GRAPH, "{out}");
    assert_eq!(out["data"]["work_history"], "git", "{out}");
    assert_eq!(out["data"]["counts"]["checkpoints"], 0, "{out}");
    assert_eq!(out["data"]["counts"]["kept_in_git"], 1, "{out}");
    // The ref into a graph nobody here knows can't get its type.
    let warnings = out["data"]["warnings"].to_string();
    assert!(
        warnings.contains("migrate.unresolved_ref")
            && warnings.contains("mdkg://7e8d9c0b-1a2f-4e3d-8c4b-5a6f7e8d9c0b/"),
        "{out}"
    );
    assert!(!warnings.contains("x-local"), "{out}");
    let hash = out["data"]["plan"].as_str().expect("plan").to_string();

    let (code, out) = run(&repo, &["migrate", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(git(&repo, &["status", "--porcelain"]), "");
    // The manifest goes; the config holds the graph's own id.
    assert!(!repo.join(".mdkg/graph.json").exists());
    let config = fs::read_to_string(repo.join(".mdkg/config.json")).expect("config");
    assert!(
        config.contains(&format!("\"graph_id\": \"{V2_GRAPH}\"")),
        "{config}"
    );
    assert!(config.contains("\"schema_version\": 3"), "{config}");
    assert!(repo.join(".mdkg/identity/migrations/receipt.json").exists());
    // The checkpoint stays, keeping its UUID too.
    let chk = fs::read_to_string(repo.join(".mdkg/work/chk-1-started.md")).expect("chk-1");
    assert!(
        chk.starts_with(&format!(
            "---\nid: checkpoint-{V2_CHK_1}\nlegacy_id: chk-1\n"
        )),
        "{chk}"
    );
    let task1 = fs::read_to_string(repo.join(".mdkg/work/task-1-start.md")).expect("task-1");
    let (t1, g1) = (format!("task-{V2_TASK_1}"), format!("goal-{V2_GOAL_1}"));
    assert!(
        task1.starts_with(&format!("---\nid: {t1}\nlegacy_id: task-1\n")),
        "{task1}"
    );
    assert!(
        !task1.contains("graph_id:") && !task1.contains("node_id:"),
        "{task1}"
    );
    assert!(task1.contains(&format!("\nparent: {g1}\n")), "{task1}");
    assert!(
        task1.contains("\ncreated: 2026-01-15T00:00:00.000Z\n"),
        "{task1}"
    );
    assert!(
        task1.contains(&format!("Start here, for {g1} (`mdkg show {g1}`).")),
        "{task1}"
    );
    assert!(!task1.contains("# Migrated records"), "{task1}");
    let task2 = fs::read_to_string(repo.join(".mdkg/work/task-2-finish.md")).expect("task-2");
    assert!(task2.contains(&format!("\nrelates: [{t1}]\n")), "{task2}");
    assert!(
        task2.contains(&format!("After [the start]({t1}).")),
        "{task2}"
    );
    assert!(task2.contains("\nrefs: []\n"), "{task2}");
    assert!(
        task2.contains("moved here from the frontmatter: refs: mdkg://7e8d9c0b-"),
        "{task2}"
    );
    let goal = fs::read_to_string(repo.join(".mdkg/work/goal-1-ship.md")).expect("goal");
    assert!(goal.contains(&format!("\nscope_refs: [{t1}]\n")), "{goal}");
    let (code, out) = run(&repo, &["validate"]);
    assert_eq!(code, 0, "{out}");
    let (code, out) = run(&repo, &["show", &t1]);
    assert_eq!(code, 0, "{out}");

    let (code, out) = run(&repo, &["migrate", "--rollback"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), base);
    assert_eq!(
        snapshot(&repo),
        before,
        "rollback leaves a byte-identical tree"
    );
    assert_eq!(dry_run(&repo), hash);
}

#[test]
fn a_v2_node_without_its_identity_stops_the_plan() {
    let dir = scratch();
    let repo = v2_repo(&dir);
    let path = repo.join(".mdkg/work/task-2-finish.md");
    let text = fs::read_to_string(&path).expect("task-2");
    let without: String = text
        .lines()
        .filter(|l| !l.starts_with("node_id:"))
        .map(|l| format!("{l}\n"))
        .collect();
    fs::write(&path, without).expect("write");
    git(&repo, &["commit", "-qam", "drop node_id"]);
    let (code, out) = run(&repo, &["migrate", "--dry-run"]);
    assert_ne!(code, 0, "{out}");
    assert!(codes(&out).contains(&"node.bad_identity".into()), "{out}");
}

#[test]
fn rollback_reads_an_empty_store_file_as_no_store() {
    let dir = scratch();
    let repo = v2_repo(&dir);
    // No records, so the apply touches no store.
    git(&repo, &["rm", "-q", ".mdkg/work/chk-1-started.md"]);
    git(&repo, &["commit", "-qm", "no checkpoint"]);
    let before = snapshot(&repo);
    let hash = dry_run(&repo);
    let (code, out) = run(&repo, &["migrate", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
    let store = repo.join(".git/mdkg/coord.sqlite");
    assert!(!store.exists());
    // An empty file there (an sqlite3 shell opened it) holds no import.
    fs::write(&store, "").expect("empty store");
    let wal = store.with_file_name("coord.sqlite-wal");
    fs::write(&wal, "preserve rollback sidecar").expect("wal");
    let (code, out) = run(&repo, &["migrate", "--rollback"]);
    assert_eq!(code, 0, "{out}");
    assert!(fs::read(&store).expect("store remains").is_empty());
    assert_eq!(
        fs::read(&wal).expect("wal remains"),
        b"preserve rollback sidecar"
    );
    assert_eq!(snapshot(&repo), before);
}

#[test]
fn titles_keep_old_ids_as_written_and_they_still_resolve() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    let path = repo.join(".mdkg/work/task-2-second.md");
    let text = fs::read_to_string(&path).expect("task-2");
    let text = text.replace(
        "title: Second task\n",
        "title: Close task-1 and prepare the handoff\nrelates: [task-1]\n",
    );
    fs::write(&path, text).expect("write");
    git(&repo, &["commit", "-qam", "a title naming task-1"]);
    let hash = dry_run(&repo);
    let (code, out) = run(&repo, &["migrate", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
    let task2 = fs::read_to_string(&path).expect("task-2");
    assert!(
        task2.contains("\ntitle: Close task-1 and prepare the handoff\n"),
        "{task2}"
    );
    // The edge names task-1 by its new id.
    let task1 = fs::read_to_string(repo.join(".mdkg/work/task-1-first.md")).expect("task-1");
    let new1 = task1
        .lines()
        .find_map(|l| l.strip_prefix("id: "))
        .expect("id")
        .to_string();
    assert!(new1.starts_with("task-0"), "{new1}");
    assert!(task2.contains(&format!("\nrelates: [{new1}]\n")), "{task2}");
    // The id the title names still finds its node.
    let (code, out) = run(&repo, &["show", "task-1"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.to_string().contains(&new1), "{out}");
}

#[test]
fn a_v2_ref_to_another_branchs_node_resolves_by_uuid_despite_a_colliding_id() {
    const MINE: &str = "5e6f7081-92a3-4b4c-8d5e-6f708192a3b4";
    const THEIRS: &str = "6f708192-a3b4-4c5d-9e6f-708192a3b4c5";
    let dir = scratch();
    let repo = v2_repo(&dir);
    let task3 = |uuid: &str, title: &str| {
        format!(
            "---\nid: task-3\ngraph_id: {V2_GRAPH}\nnode_id: {uuid}\ntype: task\ntitle: {title}\n\
             status: todo\ncreated: 2026-01-18\nupdated: 2026-01-18\n---\n# Overview\n\nBody.\n"
        )
    };
    // Each branch made its own task-3.
    git(&repo, &["switch", "-q", "-c", "side"]);
    fs::write(
        repo.join(".mdkg/work/task-3-theirs.md"),
        task3(THEIRS, "Theirs"),
    )
    .expect("write");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "their task-3"]);
    git(&repo, &["switch", "-q", "main"]);
    fs::write(repo.join(".mdkg/work/task-3-mine.md"), task3(MINE, "Mine")).expect("write");
    let theirs = format!("mdkg://{V2_GRAPH}/{THEIRS}");
    let path = repo.join(".mdkg/work/task-1-start.md");
    let text = fs::read_to_string(&path).expect("task-1");
    let text = text
        .replace(
            "status: todo\n",
            &format!("status: todo\nrelates: [{theirs}]\n"),
        )
        .replace(
            "Start here,",
            &format!("Start here (`mdkg show {theirs}`),"),
        );
    fs::write(&path, text).expect("write");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "mine, and a ref to theirs"]);
    let hash = dry_run(&repo);
    let (code, out) = run(&repo, &["migrate", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
    let task1 = fs::read_to_string(&path).expect("task-1");
    let id = format!("task-{THEIRS}");
    assert!(task1.contains(&format!("(`mdkg show {id}`)")), "{task1}");
    assert!(
        task1.contains(&format!("{id} ({theirs}, on another branch)")),
        "{task1}"
    );
    let (code, out) = run(&repo, &["validate"]);
    assert_eq!(code, 0, "{out}");
}

#[test]
fn a_shallow_clone_is_refused() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    // A second commit, so a depth-1 clone lacks the first.
    fs::write(repo.join("NOTES.md"), "notes\n").expect("write");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "second"]);
    let clone = dir.0.join("shallow");
    git(
        &dir.0,
        &[
            "clone",
            "-q",
            "--depth",
            "1",
            &format!("file://{}", repo.display()),
            clone.to_str().expect("path"),
        ],
    );
    let (code, out) = run(&clone, &["migrate", "--dry-run"]);
    assert_ne!(code, 0, "{out}");
    assert!(codes(&out).contains(&"migrate.unsupported".into()), "{out}");
    assert!(out.to_string().contains("--unshallow"), "{out}");
    // The full history plans.
    git(&clone, &["fetch", "-q", "--unshallow"]);
    let (code, out) = run(&clone, &["migrate", "--dry-run"]);
    assert_eq!(code, 0, "{out}");
}

#[test]
fn the_s02_workspaces_case_migrates_in_one_commit_and_validates() {
    let dir = scratch();
    let repo = dir.0.join("ws");
    let case = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance/cases/s02-workspaces");
    fs::create_dir_all(&repo).expect("repo");
    copy_tree(&case, &repo);
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    let (code, out) = run(&repo, &["migrate", "--dry-run"]);
    assert_eq!(code, 0, "{out}");
    // Every enabled workspace, in place; the disabled one isn't read.
    let listed: Vec<&str> = out["data"]["workspaces"]
        .as_array()
        .expect("workspaces")
        .iter()
        .filter_map(|w| w["alias"].as_str())
        .collect();
    assert_eq!(listed, ["a2", "a_b", "ab", "ab2", "root"], "{out}");
    let hash = out["data"]["plan"].as_str().expect("plan").to_string();
    let (code, out) = run(&repo, &["migrate", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(git(&repo, &["rev-parse", "HEAD^"]), base, "one commit");
    assert_eq!(git(&repo, &["status", "--porcelain"]), "");
    // Six task-1s, six ids.
    let mut ids = std::collections::BTreeSet::new();
    for f in [
        ".mdkg/work/task-1-r.md",
        "ws1/.mdkg/work/task-1.md",
        "ws2/.mdkg/work/task-1.md",
        "ws3/.mdkg/work/task-1.md",
        "ws5/.mdkg/work/task-1.md",
    ] {
        let text = fs::read_to_string(repo.join(f)).expect(f);
        assert!(text.contains("\nlegacy_id: task-1\n"), "{f}: {text}");
        ids.insert(text.lines().nth(1).unwrap_or_default().to_string());
    }
    assert_eq!(ids.len(), 5, "{ids:?}");
    // The disabled workspace stays as it was.
    let off = fs::read_to_string(repo.join("ws4/.mdkg/work/task-1.md")).expect("ws4");
    assert!(off.starts_with("---\nid: task-1\n"), "{off}");
    let (code, out) = run(&repo, &["validate"]);
    assert_eq!(code, 0, "{out}");
}

/// A two-workspace service-repo graph: the root workspace and a second one at
/// `services/api`, each with its own task-1 and chk-1 (0.6.0 ids are
/// unique only in a workspace), refs across them both ways, committed
/// work nodes, and an event log in each.
fn two_ws_repo(dir: &Scratch, name: &str) -> PathBuf {
    let repo = dir.0.join(name);
    let conformance = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance");
    fs::create_dir_all(repo.join(".mdkg/work/events")).expect("repo");
    fs::create_dir_all(repo.join("services/api/.mdkg/work/events")).expect("api");
    copy_tree(&conformance.join("base"), &repo);
    let config = repo.join(".mdkg/config.json");
    let mut cfg: Value =
        serde_json::from_str(&fs::read_to_string(&config).expect("config")).expect("json");
    cfg["workspaces"]["api"] = serde_json::json!({
        "path": "services/api", "enabled": true, "mdkg_dir": ".mdkg", "visibility": "private"
    });
    fs::write(
        &config,
        serde_json::to_string_pretty(&cfg).expect("json") + "\n",
    )
    .expect("config");
    let api_task = "---
id: task-1
type: task
title: API task
status: todo
relates: [root:task-1, task-2]
created: 2026-10-20
updated: 2026-10-20
---
# Overview

Serves root:task-1, then task-2.
";
    let api_task2 = "---
id: task-2
type: task
title: API follow-up
status: backlog
created: 2026-10-20
updated: 2026-10-20
---
";
    let api_chk = "---
id: chk-1
type: checkpoint
title: API checkpoint
status: done
scope: [task-1]
created: 2026-10-21
updated: 2026-10-21
---
Did the API's task-1.
";
    let api_test = "---
id: test-1
type: test
title: Proves the API
status: done
relates: [task-1]
created: 2026-10-21
updated: 2026-10-21
---
";
    for (path, text) in [
        (
            ".mdkg/work/task-1-first.md",
            TASK_1.replace(
                "relates: [task-2, test-2]",
                "relates: [task-2, test-2, api:task-1]",
            ),
        ),
        (".mdkg/work/task-2-second.md", TASK_2.into()),
        (".mdkg/work/chk-1-first.md", CHK_1.into()),
        (".mdkg/work/test-1-proves.md", TEST_1.into()),
        (".mdkg/work/test-2-later.md", TEST_2.into()),
        (".mdkg/work/events/events.jsonl", EVENTS.into()),
        ("services/api/.mdkg/work/task-1-api.md", api_task.into()),
        ("services/api/.mdkg/work/task-2-api.md", api_task2.into()),
        ("services/api/.mdkg/work/chk-1-api.md", api_chk.into()),
        ("services/api/.mdkg/work/test-1-api.md", api_test.into()),
        ("services/api/.mdkg/work/events/events.jsonl", EVENTS.into()),
        ("AGENTS.md", AGENTS.into()),
        (".gitignore", ".mdkg/index/\n".into()),
    ] {
        fs::write(repo.join(path), text).expect("write");
    }
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    repo
}

/// Every file under the graph's folders (indexes aside).
fn ws_graph_files(repo: &Path) -> BTreeMap<String, Vec<u8>> {
    snapshot(repo)
        .into_iter()
        .filter(|(p, _)| {
            (p.starts_with(".mdkg/") || p.starts_with("services/api/.mdkg/"))
                && !p.contains(".mdkg/index/")
        })
        .collect()
}

fn id_of(text: &str) -> String {
    text.lines()
        .find_map(|l| l.strip_prefix("id: "))
        .expect("id")
        .to_string()
}

#[test]
fn a_two_workspace_graph_migrates_from_a_linked_worktree_rolls_back_and_migrates_again() {
    let dir = scratch();
    let main = two_ws_repo(&dir, "main");
    // The main checkout has uncommitted work, which must stay untouched.
    fs::write(main.join("NOTES.md"), "in progress\n").expect("notes");
    let main_before = snapshot(&main);
    let main_head = git(&main, &["rev-parse", "HEAD"]);
    let wt = dir.0.join("wt");
    git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "migrate-0.7",
            wt.to_str().expect("utf-8"),
            "main",
        ],
    );
    let tracked = |repo: &Path| {
        git(
            repo,
            &["ls-files", "--", ".mdkg/work", "services/api/.mdkg/work"],
        )
    };
    let tracked_before = tracked(&wt);
    let before = snapshot(&wt);
    let base = git(&wt, &["rev-parse", "HEAD"]);

    let (code, out) = run(&wt, &["migrate", "--dry-run"]);
    assert_eq!(code, 0, "{out}");
    let hash = out["data"]["plan"].as_str().expect("plan").to_string();
    assert_eq!(out["data"]["work_history"], "git", "{out}");
    let ws: Vec<(&str, &str)> = out["data"]["workspaces"]
        .as_array()
        .expect("workspaces")
        .iter()
        .map(|w| {
            (
                w["alias"].as_str().unwrap_or(""),
                w["folder"].as_str().unwrap_or(""),
            )
        })
        .collect();
    assert_eq!(
        ws,
        [("api", "services/api/.mdkg/"), ("root", ".mdkg/")],
        "{out}"
    );
    assert_eq!(out["data"]["counts"]["nodes"], 9, "{out}");
    assert_eq!(out["data"]["counts"]["kept_in_git"], 4, "{out}");

    let (code, out) = run(&wt, &["migrate", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(git(&wt, &["rev-parse", "HEAD^"]), base);
    assert_eq!(git(&wt, &["status", "--porcelain"]), "");
    // Every work file in both workspaces is still tracked.
    assert_eq!(tracked(&wt), tracked_before);
    let read = |p: &str| fs::read_to_string(wt.join(p)).expect(p);
    let (root1, api1, api2) = (
        read(".mdkg/work/task-1-first.md"),
        read("services/api/.mdkg/work/task-1-api.md"),
        read("services/api/.mdkg/work/task-2-api.md"),
    );
    let (root1_id, api1_id, api2_id) = (id_of(&root1), id_of(&api1), id_of(&api2));
    assert_ne!(root1_id, api1_id, "two task-1s, two ids");
    // Refs name the right workspace's node, qualified or not.
    assert!(root1.contains(&format!(", {api1_id}]\n")), "{root1}");
    assert!(
        api1.contains(&format!("\nrelates: [{root1_id}, {api2_id}]\n")),
        "{api1}"
    );
    assert!(
        api1.contains(&format!(
            "Serves [root:task-1]({root1_id}), then [task-2]({api2_id})."
        )),
        "{api1}"
    );
    let api_chk = read("services/api/.mdkg/work/chk-1-api.md");
    assert!(
        api_chk.contains(&format!("\nscope: [{api1_id}]\n")),
        "{api_chk}"
    );
    let root_chk = read(".mdkg/work/chk-1-first.md");
    assert_ne!(id_of(&root_chk), id_of(&api_chk));
    // Both workspaces' config entries stay.
    let cfg: Value = serde_json::from_str(&read(".mdkg/config.json")).expect("config");
    assert_eq!(cfg["workspaces"]["api"]["path"], "services/api", "{cfg}");
    assert_eq!(cfg["workspaces"]["root"]["path"], ".", "{cfg}");
    let (code, out) = run(&wt, &["validate"]);
    assert_eq!(code, 0, "{out}");
    // The main checkout, its uncommitted work included, never moved.
    assert_eq!(snapshot(&main), main_before);
    assert_eq!(git(&main, &["rev-parse", "HEAD"]), main_head);

    let (code, out) = run(&wt, &["migrate", "--rollback"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(git(&wt, &["rev-parse", "HEAD"]), base);
    assert_eq!(snapshot(&wt), before, "byte-identical after rollback");
    // Again, to the same plan.
    let (code, out) = run(&wt, &["migrate", "--dry-run"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["data"]["plan"], hash.as_str());
    let (code, out) = run(&wt, &["migrate", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(read("services/api/.mdkg/work/task-1-api.md"), api1);
    assert_eq!(snapshot(&main), main_before);
}

#[test]
fn a_crash_between_two_workspaces_swaps_finishes_or_rolls_back() {
    let dir = scratch();
    let reference = two_ws_repo(&dir, "reference");
    let old_graph = ws_graph_files(&reference);
    let hash = dry_run(&reference);
    let (code, out) = run(&reference, &["migrate", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
    let new_graph = ws_graph_files(&reference);
    let new_tree = git(&reference, &["rev-parse", "HEAD^{tree}"]);
    let short = &hash[7..19];
    for (swaps, finish) in [
        ("1", "apply"),
        ("1", "rollback"),
        ("2", "apply"),
        ("2", "rollback"),
    ] {
        let repo = two_ws_repo(&dir, &format!("{swaps}-{finish}"));
        let before = snapshot(&repo);
        let base = git(&repo, &["rev-parse", "HEAD"]);
        assert_eq!(dry_run(&repo), hash);
        // A crash after the first folder (the second workspace's) swapped.
        let (code, _) = run_with(
            &repo,
            &["migrate", "--apply", &hash],
            None,
            &[("MDKG_MIGRATE_CRASH_AFTER_SWAPS", swaps)],
        );
        assert_eq!(code, 86, "the crash hook fired");
        let now = ws_graph_files(&repo);
        let api = |g: &BTreeMap<String, Vec<u8>>| -> BTreeMap<String, Vec<u8>> {
            g.iter()
                .filter(|(p, _)| p.starts_with("services/api/.mdkg/") && !p.contains(".migrate-"))
                .map(|(p, b)| (p.clone(), b.clone()))
                .collect()
        };
        let root = |g: &BTreeMap<String, Vec<u8>>| -> BTreeMap<String, Vec<u8>> {
            g.iter()
                .filter(|(p, _)| p.starts_with(".mdkg/"))
                .map(|(p, b)| (p.clone(), b.clone()))
                .collect()
        };
        // Each folder is whole: the second one new, the config's new only
        // once it swapped too (before HEAD moved).
        assert_eq!(api(&now), api(&new_graph));
        let mark = format!(".mdkg/.migrate-{short}");
        let root_now: BTreeMap<String, Vec<u8>> =
            root(&now).into_iter().filter(|(p, _)| *p != mark).collect();
        if swaps == "1" {
            assert_eq!(root_now, root(&old_graph));
        } else {
            assert_eq!(root_now, root(&new_graph));
        }
        // The journal lists the second folder in `extra`, which a
        // one-workspace journal (as 0.7.0 wrote) leaves out.
        let journal: Value = serde_json::from_slice(
            &fs::read(
                Path::new(out_status_folder(&repo).as_str())
                    .parent()
                    .expect("root")
                    .join("journal.json"),
            )
            .expect("journal"),
        )
        .expect("json");
        assert_eq!(
            journal["extra"].as_array().map(Vec::len),
            Some(1),
            "{journal}"
        );
        assert!(
            repo.join(format!("services/api/.mdkg/.migrate-{short}"))
                .exists(),
            "the swapped folder carries its marker"
        );
        let (code, out) = run(&repo, &["migrate", "--status"]);
        assert_eq!(code, 0, "{out}");
        assert_eq!(out["data"]["stage"], "built", "{out}");
        if finish == "apply" {
            let (code, out) = run(&repo, &["migrate", "--apply", &hash]);
            assert_eq!(code, 0, "{out}");
            assert_eq!(git(&repo, &["rev-parse", "HEAD^{tree}"]), new_tree);
            assert_eq!(ws_graph_files(&repo), new_graph);
            assert_eq!(git(&repo, &["status", "--porcelain"]), "");
        } else {
            let (code, out) = run(&repo, &["migrate", "--rollback"]);
            assert_eq!(code, 0, "{out}");
            assert_eq!(git(&repo, &["rev-parse", "HEAD"]), base);
            assert_eq!(snapshot(&repo), before, "byte-identical after rollback");
        }
    }
}

#[test]
fn a_v2_graph_of_several_workspaces_is_told_to_stay_on_0_6() {
    let dir = scratch();
    let repo = v2_repo(&dir);
    // A second workspace, empty.
    let path = repo.join(".mdkg/config.json");
    let config = fs::read_to_string(&path).expect("config");
    let config = config.replace(
        "\"workspaces\": { \"root\": { \"path\": \".\", \"enabled\": true, \"mdkg_dir\": \".mdkg\" } }",
        "\"workspaces\": { \"root\": { \"path\": \".\", \"enabled\": true, \"mdkg_dir\": \".mdkg\" }, \
         \"aux\": { \"path\": \"aux\", \"enabled\": true, \"mdkg_dir\": \".mdkg\" } }",
    );
    assert!(config.contains("\"aux\""), "{config}");
    fs::write(&path, config).expect("write");
    fs::create_dir_all(repo.join("aux/.mdkg/work")).expect("aux");
    fs::write(repo.join("aux/.mdkg/work/.keep"), "").expect("keep");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "a second workspace"]);
    let (code, out) = run(&repo, &["migrate", "--dry-run"]);
    assert_ne!(code, 0, "{out}");
    assert!(codes(&out).contains(&"migrate.unsupported".into()), "{out}");
    let hint = out["diagnostics"][0]["hint"].as_str().expect("hint");
    assert!(
        hint.contains("0.6.x") && !hint.contains("0.6 form"),
        "{hint}"
    );
}

#[test]
fn restore_history_brings_back_what_a_store_migration_took_out_of_git() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    let tracked = |repo: &Path| git(repo, &["ls-files", "--", ".mdkg/work"]);
    let tracked_before = tracked(&repo);
    // What a migration that keeps the history in git writes for them.
    let hash = dry_run(&repo);
    let (code, out) = run(&repo, &["migrate", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
    let kept: Vec<(&str, String)> = [
        ".mdkg/work/chk-1-first.md",
        ".mdkg/work/test-1-proves.md",
        ".mdkg/work/events/events.jsonl",
    ]
    .into_iter()
    .map(|p| (p, fs::read_to_string(repo.join(p)).expect("kept")))
    .collect();
    let (code, out) = run(&repo, &["migrate", "--rollback"]);
    assert_eq!(code, 0, "{out}");
    // A graph that isn't migrated has nothing to restore.
    let (code, out) = run(&repo, &["migrate", "--restore-history", "--dry-run"]);
    assert_ne!(code, 0);
    assert!(
        codes(&out).contains(&"migrate.not_migrated".into()),
        "{out}"
    );

    // Migrated as 0.7.0 did: the history left git.
    let hash = dry_run_store(&repo);
    let (code, out) = run(
        &repo,
        &["migrate", "--apply", &hash, "--work-history", "store"],
    );
    assert_eq!(code, 0, "{out}");
    assert_ne!(tracked(&repo), tracked_before);
    // Found by its config, not its message: a squash or a reword keeps it.
    git(
        &repo,
        &["commit", "-q", "--amend", "-m", "squashed on GitHub"],
    );
    let migration = git(&repo, &["rev-parse", "HEAD"]);

    let before = snapshot(&repo);
    let (code, out) = run(&repo, &["migrate", "--restore-history", "--dry-run"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["data"]["stage"], "planned", "{out}");
    assert_eq!(snapshot(&repo), before, "a dry run changes nothing");
    let restore = &out["data"]["restore"];
    assert_eq!(
        restore["migrations"],
        serde_json::json!([migration]),
        "{out}"
    );
    assert_eq!(restore["checkpoints"], 1, "{out}");
    assert_eq!(restore["check_results"], 1, "{out}");
    assert_eq!(restore["event_logs"], 1, "{out}");
    let hash = out["data"]["plan"].as_str().expect("plan").to_string();
    // The same graph, the same plan.
    let (_, again) = run(&repo, &["migrate", "--restore-history", "--dry-run"]);
    assert_eq!(again["data"]["plan"], hash.as_str());
    // A wrong hash is refused, changing nothing.
    let (code, out) = run(
        &repo,
        &["migrate", "--restore-history", "--apply", "sha256:00"],
    );
    assert_ne!(code, 0);
    assert!(
        codes(&out).contains(&"migrate.plan_mismatch".into()),
        "{out}"
    );
    assert_eq!(snapshot(&repo), before);

    let (code, out) = run(&repo, &["migrate", "--restore-history", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["data"]["stage"], "done", "{out}");
    // One commit on the migration, a clean tree, and every work file
    // tracked again, each as a migration that kept them would have it.
    assert_eq!(git(&repo, &["rev-parse", "HEAD^"]), migration);
    assert_eq!(git(&repo, &["status", "--porcelain"]), "");
    assert_eq!(tracked(&repo), tracked_before);
    for (path, text) in &kept {
        assert_eq!(
            &fs::read_to_string(repo.join(path)).expect("restored"),
            text,
            "{path}"
        );
    }
    let (code, out) = run(&repo, &["validate"]);
    assert_eq!(code, 0, "{out}");
    let (code, out) = run(&repo, &["show", "chk-1"]);
    assert_eq!(code, 0, "the checkpoint is a node again: {out}");
    // Done: there's nothing left to restore, and a second apply says so.
    let (code, out) = run(&repo, &["migrate", "--restore-history", "--dry-run"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["data"]["stage"], "nothing-to-restore", "{out}");
    let (code, out) = run(&repo, &["migrate", "--restore-history", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["data"]["stage"], "nothing-to-restore", "{out}");
}

#[test]
fn restore_history_refuses_a_dirty_tree_and_skips_a_node_the_graph_has_again() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    let hash = dry_run_store(&repo);
    let (code, out) = run(
        &repo,
        &["migrate", "--apply", &hash, "--work-history", "store"],
    );
    assert_eq!(code, 0, "{out}");
    let (_, out) = run(&repo, &["migrate", "--restore-history", "--dry-run"]);
    let hash = out["data"]["plan"].as_str().expect("plan").to_string();
    fs::write(repo.join("NOTES.md"), "draft\n").expect("notes");
    let before = snapshot(&repo);
    let (code, out) = run(&repo, &["migrate", "--restore-history", "--apply", &hash]);
    assert_ne!(code, 0);
    assert!(codes(&out).contains(&"migrate.dirty".into()), "{out}");
    assert_eq!(snapshot(&repo), before);
    fs::remove_file(repo.join("NOTES.md")).expect("notes");
    // A test someone recreated under the old id since stays theirs.
    let (code, out) = run(&repo, &["new", "test", "Proves task-1 again"]);
    assert_eq!(code, 0, "{out}");
    let path = out["data"]["node"]["path"]
        .as_str()
        .expect("path")
        .to_string();
    let text = fs::read_to_string(repo.join(&path)).expect("new test");
    let text = text.replacen("\ntype: test\n", "\nlegacy_id: test-1\ntype: test\n", 1);
    fs::write(repo.join(&path), text).expect("legacy id");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "recreate test-1"]);
    let (code, out) = run(&repo, &["migrate", "--restore-history", "--dry-run"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["data"]["restore"]["check_results"], 0, "{out}");
    assert_eq!(out["data"]["restore"]["checkpoints"], 1, "{out}");
    assert!(
        codes_in(&out["data"]["warnings"]).contains(&"migrate.history_present".to_string()),
        "{out}"
    );
}

fn codes_in(warnings: &Value) -> Vec<String> {
    warnings
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|w| w["code"].as_str().map(String::from))
        .collect()
}

#[test]
#[ignore = "the branch-against-main mode is cut from 0.7.1 (back in 0.7.2)"]
fn a_branch_migrated_against_a_migrated_main_conflicts_only_in_its_own_edits() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    // A branch cut before main migrated: its own spike, an edit to a node
    // main has, and a node it moved.
    let other = branch_with(&dir, &repo, "other", &[("work/spike-1-look.md", SPIKE)]);
    let task2 = fs::read_to_string(other.join(".mdkg/work/task-2-second.md")).expect("task-2");
    fs::write(
        other.join(".mdkg/work/task-2-second.md"),
        task2.replace("Body.", "Body, edited on the branch."),
    )
    .expect("edit");
    git(
        &other,
        &[
            "mv",
            ".mdkg/work/task-1-first.md",
            ".mdkg/work/task-1-moved.md",
        ],
    );
    git(&other, &["commit", "-qam", "branch work"]);
    // main migrates, then changes a node the branch never touched.
    let hash = dry_run(&repo);
    let (code, out) = run(&repo, &["migrate", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
    let test2 = repo.join(".mdkg/work/test-2-later.md");
    let edited = fs::read_to_string(&test2).expect("test-2") + "\nLater, on main.\n";
    fs::write(&test2, &edited).expect("main edit");
    git(&repo, &["commit", "-qam", "main moves on"]);
    let main_task1 = fs::read_to_string(repo.join(".mdkg/work/task-1-first.md")).expect("t1");

    let (code, out) = run(&other, &["migrate", "--dry-run"]);
    assert_eq!(code, 0, "{out}");
    let against = &out["data"]["against"];
    assert_eq!(against["ref"], "refs/heads/main", "{out}");
    assert_eq!(against["shared"], 2, "the edited and the moved node: {out}");
    assert!(against["adopted"].as_u64() > Some(0), "{out}");
    assert_eq!(out["data"]["work_history"], "git", "{out}");
    assert!(
        out["data"]["work_history_reason"]
            .as_str()
            .is_some_and(|r| r.contains("refs/heads/main")),
        "{out}"
    );
    let hash = out["data"]["plan"].as_str().expect("plan").to_string();
    let (code, out) = run(&other, &["migrate", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
    // The moved node keeps main's id; a file it didn't change is main's.
    let moved = fs::read_to_string(other.join(".mdkg/work/task-1-moved.md")).expect("moved");
    assert_eq!(id_of(&moved), id_of(&main_task1));
    assert_eq!(
        fs::read_to_string(other.join(".mdkg/work/test-2-later.md")).expect("t2"),
        edited
    );
    let spike = fs::read_to_string(other.join(".mdkg/work/spike-1-look.md")).expect("spike");
    assert!(spike.contains("\nlegacy_id: spike-1\n"), "{spike}");

    // The merge conflicts only where the branch made its own change: the
    // node it moved (git can't pair the old 0.6 file with its 0.7 rewrite).
    // The edit to task-2, and every file it didn't touch, merge cleanly.
    let out = isolated(Command::new("git").arg("-C").arg(&other))
        .args(["merge", "-q", "--no-edit", "main"])
        .output()
        .expect("merge");
    assert!(!out.status.success());
    assert_eq!(
        git(&other, &["diff", "--name-only", "--diff-filter=U"]),
        ".mdkg/work/task-1-first.md"
    );
    // Keep the branch's move.
    git(&other, &["rm", "-q", ".mdkg/work/task-1-first.md"]);
    git(&other, &["commit", "-q", "--no-edit"]);
    assert_eq!(git(&other, &["status", "--porcelain"]), "");
    let (code, out) = run(&other, &["validate"]);
    assert_eq!(code, 0, "{out}");
    let t2 = fs::read_to_string(other.join(".mdkg/work/task-2-second.md")).expect("task-2");
    assert!(t2.contains("Body, edited on the branch."), "{t2}");
    // Each node once.
    let id = id_of(&main_task1);
    let holders = snapshot(&other)
        .into_iter()
        .filter(|(p, b)| {
            p.starts_with(".mdkg/work/")
                && String::from_utf8_lossy(b).contains(&format!("\nid: {id}\n"))
        })
        .count();
    assert_eq!(holders, 1);
    // And main merges the branch as cleanly.
    git(&repo, &["merge", "-q", "--no-edit", "other"]);
    let (code, out) = run(&repo, &["validate"]);
    assert_eq!(code, 0, "{out}");
}

/// Main's migrated copies that have diverged (local and fetched) are a
/// migrated main all the same: the same refusal, naming no removed flag.
#[test]
fn a_branch_whose_migrated_mains_diverged_is_refused_the_same_way() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    let other = branch_with(&dir, &repo, "other", &[("work/spike-1-look.md", SPIKE)]);
    let hash = dry_run(&repo);
    let (code, out) = run(&repo, &["migrate", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
    // origin/main has another migration commit of the same base.
    let migrated = git(&repo, &["rev-parse", "HEAD"]);
    git(
        &repo,
        &["commit", "-q", "--amend", "-m", "migrated elsewhere"],
    );
    let elsewhere = git(&repo, &["rev-parse", "HEAD"]);
    git(
        &repo,
        &["update-ref", "refs/remotes/origin/main", &elsewhere],
    );
    git(&repo, &["reset", "-q", "--hard", &migrated]);
    let (code, out) = run(&other, &["migrate", "--dry-run"]);
    assert_ne!(code, 0, "{out}");
    assert!(codes(&out).contains(&"migrate.unsupported".into()), "{out}");
    let hint = out["diagnostics"][0]["hint"].as_str().unwrap_or_default();
    assert!(hint.contains("merge main into this branch first"), "{out}");
    assert!(!out.to_string().contains("--against"), "{out}");
}

/// A branch cut before main migrated is refused (the branch-against-main
/// mode is cut from 0.7.1): main migrated here, or on GitHub and fetched.
#[test]
fn a_branch_whose_main_is_already_migrated_is_refused() {
    let dir = scratch();
    for remote_only in [false, true] {
        let repo = repo(&dir, if remote_only { "remote" } else { "local" });
        let other = branch_with(
            &dir,
            &repo,
            if remote_only { "other-r" } else { "other-l" },
            &[("work/spike-1-look.md", SPIKE)],
        );
        let hash = dry_run(&repo);
        let (code, out) = run(&repo, &["migrate", "--apply", &hash]);
        assert_eq!(code, 0, "{out}");
        if remote_only {
            let migrated = git(&repo, &["rev-parse", "HEAD"]);
            git(
                &repo,
                &["update-ref", "refs/remotes/origin/main", &migrated],
            );
            git(&repo, &["reset", "-q", "--hard", "HEAD^"]);
        }
        let before = snapshot(&other);
        for args in [
            vec!["migrate", "--dry-run"],
            vec!["migrate", "--apply", &hash],
        ] {
            let (code, out) = run(&other, &args);
            assert_ne!(code, 0, "{out}");
            assert!(codes(&out).contains(&"migrate.unsupported".into()), "{out}");
            let hint = out["diagnostics"][0]["hint"].as_str().unwrap_or_default();
            assert!(hint.contains("merge main into this branch first"), "{out}");
        }
        assert_eq!(snapshot(&other), before, "nothing changes");
    }
}

#[test]
#[ignore = "the branch-against-main mode is cut from 0.7.1 (back in 0.7.2)"]
fn against_names_a_ref_that_isnt_migrated_is_refused() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    let (code, out) = run(&repo, &["migrate", "--dry-run", "--against", "main"]);
    assert_ne!(code, 0, "{out}");
    assert!(codes(&out).contains(&"migrate.unsupported".into()), "{out}");
}

#[test]
#[ignore = "the branch-against-main mode is cut from 0.7.1 (back in 0.7.2)"]
fn a_main_migrated_on_the_remote_only_is_followed_once_fetched() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    let other = branch_with(&dir, &repo, "other", &[("work/spike-1-look.md", SPIKE)]);
    // main was migrated through a pull request on GitHub: the fetched
    // origin/main has the migration, the local main doesn't yet.
    let hash = dry_run(&repo);
    let (code, out) = run(&repo, &["migrate", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
    let migrated = git(&repo, &["rev-parse", "HEAD"]);
    git(
        &repo,
        &["update-ref", "refs/remotes/origin/main", &migrated],
    );
    git(&repo, &["reset", "-q", "--hard", "HEAD^"]);
    let (code, out) = run(&other, &["migrate", "--dry-run"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        out["data"]["against"]["ref"], "refs/remotes/origin/main",
        "{out}"
    );
    assert_eq!(
        out["data"]["against"]["migration"],
        migrated.as_str(),
        "{out}"
    );
}

const TASK_3_MAIN: &str = "---\nid: task-3\ntype: task\ntitle: Fix the parser\nstatus: todo\ncreated: 2026-10-21\nupdated: 2026-10-21\n---\n";
const TASK_3_BRANCH: &str = "---\nid: task-3\ntype: task\ntitle: Add the docs\nstatus: todo\nrelates: [task-2]\ncreated: 2026-10-22\nupdated: 2026-10-22\n---\n";
const TASK_9: &str = "---\nid: task-9\ntype: task\ntitle: Ninth\nstatus: todo\ncreated: 2026-10-21\nupdated: 2026-10-21\n---\n";

/// The plan's content for `path`, from a dry run's stored plan.
fn planned(out: &Value, path: &str) -> String {
    let plan = Path::new(out["data"]["plan_file"].as_str().expect("plan")).to_path_buf();
    let stored: Value = serde_json::from_slice(&fs::read(plan).expect("plan")).expect("json");
    stored["contents"][path]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

#[test]
#[ignore = "the branch-against-main mode is cut from 0.7.1 (back in 0.7.2)"]
fn a_node_both_sides_made_after_the_fork_keeps_its_own_id() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    // Both make their own task-3 after the fork.
    let other = branch_with(
        &dir,
        &repo,
        "other",
        &[("work/task-3-docs.md", TASK_3_BRANCH)],
    );
    // main makes task-3 and task-9, points task-1 at task-9, and migrates.
    fs::write(repo.join(".mdkg/work/task-3-parser.md"), TASK_3_MAIN).expect("t3");
    fs::write(repo.join(".mdkg/work/task-9-ninth.md"), TASK_9).expect("t9");
    let t1 = fs::read_to_string(repo.join(".mdkg/work/task-1-first.md")).expect("t1");
    fs::write(
        repo.join(".mdkg/work/task-1-first.md"),
        t1.replace("refs: [chk-1]", "refs: [chk-1, task-9]"),
    )
    .expect("t1");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "main's own task-3 and task-9"]);
    let hash = dry_run(&repo);
    let (code, out) = run(&repo, &["migrate", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
    let main_t3 = fs::read_to_string(repo.join(".mdkg/work/task-3-parser.md")).expect("t3");
    let main_t9 = fs::read_to_string(repo.join(".mdkg/work/task-9-ninth.md")).expect("t9");

    let (code, out) = run(&other, &["migrate", "--dry-run"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["data"]["against"]["shared"], 0, "{out}");
    assert_eq!(out["data"]["against"]["added"], 2, "{out}");
    let warning = out["data"]["warnings"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|w| w["code"] == "migrate.legacy_id_collision")
        .unwrap_or_else(|| panic!("a collision warning: {out}"));
    assert_eq!(warning["path"], ".mdkg/work/task-3-docs.md", "{out}");
    let ours = planned(&out, ".mdkg/work/task-3-docs.md");
    assert_ne!(id_of(&ours), id_of(&main_t3), "{ours}");
    // It leaves the handle task-3 to main's: two legacy ids alike in one
    // workspace wouldn't validate.
    assert!(!ours.contains("legacy_id:"), "{ours}");
    // main's task-9 comes along, so main's task-1 (taken as main has it)
    // finds what it refers to.
    assert_eq!(planned(&out, ".mdkg/work/task-9-ninth.md"), main_t9);
    let hash = out["data"]["plan"].as_str().expect("plan").to_string();
    let (code, out) = run(&other, &["migrate", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
    let (code, out) = run(&other, &["validate"]);
    assert_eq!(code, 0, "{out}");
    // The two task-3s stay two nodes through the merge.
    git(&other, &["merge", "-q", "--no-edit", "main"]);
    let ids: Vec<String> = [".mdkg/work/task-3-docs.md", ".mdkg/work/task-3-parser.md"]
        .iter()
        .map(|p| id_of(&fs::read_to_string(other.join(p)).expect("task-3")))
        .collect();
    assert_ne!(ids[0], ids[1]);
    assert_eq!(ids[1], id_of(&main_t3));
    let (code, out) = run(&other, &["validate"]);
    assert_eq!(code, 0, "{out}");
    let (code, out) = run(&other, &["show", "task-3"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["data"]["node"]["id"], ids[1].as_str(), "{out}");
}

#[test]
#[ignore = "the branch-against-main mode is cut from 0.7.1 (back in 0.7.2)"]
fn a_ref_to_a_node_main_moved_names_main_s_id() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    // The branch changes task-1 only, pointing it at task-2, which it
    // leaves alone.
    let t1 = TASK_1.replace("blocked_by: [test-1]", "blocked_by: [test-1, task-2]");
    let other = branch_with(&dir, &repo, "other", &[("work/task-1-first.md", &t1)]);
    // main migrates, then moves task-2.
    let hash = dry_run(&repo);
    let (code, out) = run(&repo, &["migrate", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
    let main_t2 = id_of(&fs::read_to_string(repo.join(".mdkg/work/task-2-second.md")).expect("t2"));
    git(
        &repo,
        &[
            "mv",
            ".mdkg/work/task-2-second.md",
            ".mdkg/work/task-2-renamed.md",
        ],
    );
    git(&repo, &["commit", "-qm", "move task-2"]);
    let (code, out) = run(&other, &["migrate", "--dry-run"]);
    assert_eq!(code, 0, "{out}");
    let ours = planned(&out, ".mdkg/work/task-1-first.md");
    let blocked = ours
        .lines()
        .find(|l| l.starts_with("blocked_by:"))
        .unwrap_or_default();
    assert!(blocked.contains(&main_t2), "{ours}");
    let hash = out["data"]["plan"].as_str().expect("plan").to_string();
    let (code, out) = run(&other, &["migrate", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
    // The branch's own edit to task-1 is the only conflict (its lines sit
    // beside the ones main's migration rewrote); keeping the branch's
    // side keeps a ref main's graph resolves.
    let out = isolated(Command::new("git").arg("-C").arg(&other))
        .args(["merge", "-q", "--no-edit", "main"])
        .output()
        .expect("merge");
    if !out.status.success() {
        assert_eq!(
            git(&other, &["diff", "--name-only", "--diff-filter=U"]),
            ".mdkg/work/task-1-first.md"
        );
        git(
            &other,
            &["checkout", "-q", "--ours", ".mdkg/work/task-1-first.md"],
        );
        git(&other, &["add", ".mdkg/work/task-1-first.md"]);
        git(&other, &["commit", "-q", "--no-edit"]);
    }
    assert!(other.join(".mdkg/work/task-2-renamed.md").exists());
    assert!(!other.join(".mdkg/work/task-2-second.md").exists());
    let (code, out) = run(&other, &["validate"]);
    assert_eq!(code, 0, "{out}");
}

#[test]
#[ignore = "the branch-against-main mode is cut from 0.7.1 (back in 0.7.2)"]
fn a_branch_against_a_main_that_moved_its_history_out_follows_it() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    // A graph whose only work history is a progress section: no
    // checkpoint, finished test or event log for its migration to delete.
    for path in [
        ".mdkg/work/chk-1-first.md",
        ".mdkg/work/test-1-proves.md",
        ".mdkg/work/test-2-later.md",
        ".mdkg/work/events/events.jsonl",
    ] {
        git(&repo, &["rm", "-q", path]);
    }
    let t1 = fs::read_to_string(repo.join(".mdkg/work/task-1-first.md")).expect("t1");
    fs::write(
        repo.join(".mdkg/work/task-1-first.md"),
        t1.replace(
            "relates: [task-2, test-2]\nblocked_by: [test-1]\nrefs: [chk-1]\n",
            "relates: [task-2]\n",
        ),
    )
    .expect("t1");
    git(&repo, &["commit", "-qam", "history only in progress notes"]);
    let other = branch_with(&dir, &repo, "other", &[("work/spike-1-look.md", SPIKE)]);
    let hash = dry_run_store(&repo);
    let (code, out) = run(
        &repo,
        &["migrate", "--apply", &hash, "--work-history", "store"],
    );
    assert_eq!(code, 0, "{out}");
    let (code, out) = run(&other, &["migrate", "--dry-run"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["data"]["work_history"], "store", "{out}");
    // A branch can't send its history elsewhere than main's went.
    let (code, out) = run(&other, &["migrate", "--dry-run", "--work-history", "git"]);
    assert_ne!(code, 0, "{out}");
    assert!(codes(&out).contains(&"migrate.unsupported".into()), "{out}");
}

#[test]
#[ignore = "the branch-against-main mode is cut from 0.7.1 (back in 0.7.2)"]
fn a_fresh_clone_of_a_branch_imports_the_history_main_moved_out() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    let _other = branch_with(&dir, &repo, "other", &[("work/spike-1-look.md", SPIKE)]);
    // History main made after the fork, which the branch never saw: an
    // event and a checkpoint.
    let events = repo.join(".mdkg/work/events/events.jsonl");
    let more = fs::read_to_string(&events).expect("events")
        + "{\"ts\":\"2026-10-22T01:02:03.004Z\",\"agent\":\"mdkg-cli\",\"kind\":\"NODE_UPDATED\",\"refs\":[\"task-2\"]}\n";
    fs::write(&events, more).expect("events");
    fs::write(
        repo.join(".mdkg/work/chk-2-later.md"),
        CHK_1
            .replace("id: chk-1", "id: chk-2")
            .replace("First checkpoint", "Later"),
    )
    .expect("chk-2");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "main's history since the fork"]);
    let hash = dry_run_store(&repo);
    let (code, out) = run(
        &repo,
        &["migrate", "--apply", &hash, "--work-history", "store"],
    );
    assert_eq!(code, 0, "{out}");
    let main_counts = out["data"]["counts"].clone();
    // A clone on another machine: main's migration is in git, its records
    // aren't in this store.
    let clone = dir.0.join("clone");
    let out = isolated(Command::new("git").arg("clone").arg("-q"))
        .arg(&repo)
        .arg(&clone)
        .output()
        .expect("clone");
    assert!(out.status.success(), "{out:?}");
    git(&clone, &["checkout", "-q", "-b", "other", "origin/other"]);
    // Run as a launcher turn or the MCP server runs it: pinned to this
    // checkout, which the replay's own repository isn't.
    let top = fs::canonicalize(&clone).expect("clone");
    let pins = [
        ("MDKG_TRUSTED_WORKTREE", top.to_string_lossy().into_owned()),
        (
            "MDKG_TRUSTED_COMMON_DIR",
            top.join(".git").to_string_lossy().into_owned(),
        ),
    ];
    let pins: Vec<(&str, &str)> = pins.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let pinned = |args: &[&str]| {
        let (code, out) = run_with(&clone, args, None, &pins);
        (code, out.expect("json"))
    };
    let (code, out) = pinned(&["migrate", "--dry-run"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["data"]["work_history"], "store", "{out}");
    for k in [
        "records",
        "checkpoints",
        "check_results",
        "legacy_events",
        "review_records",
    ] {
        assert_eq!(out["data"]["counts"][k], main_counts[k], "{k}: {out}");
    }
    // Main's own records, as main's migration made them.
    assert_eq!(
        out["data"]["against"]["replayed"], main_counts["records"],
        "{out}"
    );
    let hash = out["data"]["plan"].as_str().expect("plan").to_string();
    let (code, out) = pinned(&["migrate", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
    let kinds = log_kinds(&clone);
    let count = |k: &str| kinds.iter().filter(|x| *x == k).count();
    for k in [
        "legacy.checkpoint",
        "check.result",
        "legacy.event",
        "review.record",
    ] {
        assert!(count(k) > 0, "{k}: {kinds:?}");
    }
    assert_eq!(count("legacy.checkpoint"), 2, "{kinds:?}");
    assert_eq!(log_markers(&clone)[0]["skipped"], 0);
    // Every digest main's nodes cite is in this store.
    let db = clone.join(".git/mdkg/coord.sqlite");
    let conn =
        rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("store");
    let mut stmt = conn.prepare("SELECT record FROM log").expect("log");
    let digests: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .expect("rows")
        .filter_map(|t| {
            let v: Value = serde_json::from_str(&t.ok()?).ok()?;
            v["body"]["migrate_digest"].as_str().map(str::to_string)
        })
        .collect();
    for (path, bytes) in snapshot(&clone)
        .iter()
        .filter(|(p, _)| p.starts_with(".mdkg/work/"))
    {
        let text = String::from_utf8_lossy(bytes);
        for part in text.split("migration digest ").skip(1) {
            let digest: String = part.chars().take(7 + 64).collect();
            assert!(
                digests.contains(&digest),
                "{path}: {digest} isn't in the store"
            );
        }
    }
    // No temporary worktree is left behind.
    assert_eq!(
        git(&clone, &["worktree", "list", "--porcelain"])
            .lines()
            .filter(|l| l.starts_with("worktree "))
            .count(),
        1
    );
}

#[test]
#[ignore = "the branch-against-main mode is cut from 0.7.1 (back in 0.7.2)"]
fn a_store_that_ran_main_s_migration_imports_nothing_twice_without_its_plan_hash() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    let other = branch_with(&dir, &repo, "other", &[("work/spike-1-look.md", SPIKE)]);
    let hash = dry_run_store(&repo);
    let (code, out) = run(
        &repo,
        &["migrate", "--apply", &hash, "--work-history", "store"],
    );
    assert_eq!(code, 0, "{out}");
    // A squash on GitHub that dropped the plan hash from the message.
    git(&repo, &["commit", "-q", "--amend", "-m", "migrate"]);
    let worktrees = git(&repo, &["worktree", "list", "--porcelain"]);
    let (code, out) = run(&other, &["migrate", "--dry-run"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["data"]["work_history"], "store", "{out}");
    // Without the plan hash, main's records are replayed whole...
    assert!(
        out["data"]["against"]["replayed"].as_u64() > Some(0),
        "{out}"
    );
    // A store-mode dry run leaves the repository's worktrees as they were.
    assert_eq!(git(&repo, &["worktree", "list", "--porcelain"]), worktrees);
    let hash = out["data"]["plan"].as_str().expect("plan").to_string();
    let (code, out) = run(&other, &["migrate", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
    // ...and each one this store holds is skipped.
    let kinds = log_kinds(&repo);
    assert_eq!(
        kinds.iter().filter(|k| *k == "legacy.checkpoint").count(),
        1,
        "{kinds:?}"
    );
    let markers = log_markers(&repo);
    assert_eq!(markers.last().expect("marker")["count"], 0, "{markers:?}");
}

#[test]
#[ignore = "the branch-against-main mode is cut from 0.7.1 (back in 0.7.2)"]
fn an_incomplete_replay_is_refused_even_after_main_restores_its_history() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    let other = branch_with(&dir, &repo, "other", &[("work/spike-1-look.md", SPIKE)]);
    let hash = dry_run_store(&repo);
    let (code, out) = run(
        &repo,
        &["migrate", "--apply", &hash, "--work-history", "store"],
    );
    assert_eq!(code, 0, "{out}");
    // Main's nodes cite a record no replay makes, as a child graph outside
    // git would cause.
    let t1 = repo.join(".mdkg/work/task-1-first.md");
    let text = fs::read_to_string(&t1).expect("task-1");
    let at = text
        .find("migration digest sha256:")
        .expect("a cited digest")
        + "migration digest sha256:".len();
    let text = format!("{}{}{}", &text[..at], "f".repeat(64), &text[at + 64..]);
    fs::write(&t1, text).expect("task-1");
    git(&repo, &["commit", "-qam", "cite a record no replay makes"]);
    // Another machine's store, which never saw main's migration.
    let store = repo.join(".git/mdkg/coord.sqlite");
    let aside = dir.0.join("aside");
    fs::create_dir_all(&aside).expect("aside");
    let swap = |from: &Path, to: &Path| {
        for ext in ["", "-wal", "-shm"] {
            let name = format!("coord.sqlite{ext}");
            if from.join(&name).exists() {
                fs::rename(from.join(&name), to.join(&name)).expect("move store");
            }
        }
    };
    let store_dir = store.parent().expect("store folder").to_path_buf();
    swap(&store_dir, &aside);
    let (code, out) = run(&other, &["migrate", "--dry-run"]);
    assert_ne!(code, 0, "{out}");
    assert!(codes(&out).contains(&"migrate.unsupported".into()), "{out}");
    let hint = out["diagnostics"][0]["hint"].as_str().unwrap_or_default();
    assert!(hint.contains("where main's migration ran"), "{out}");
    assert!(!hint.contains("--restore-history"), "{out}");
    // The way forward: where main's migration ran, whose store has its
    // records.
    swap(&aside, &store_dir);
    let (code, out) = run(&other, &["migrate", "--dry-run"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["data"]["against"]["replayed"], 0, "{out}");
    // Main restoring its history doesn't get round it: its nodes still
    // cite the moved sections' records, which only a store holds.
    let (code, out) = run(&repo, &["migrate", "--restore-history", "--dry-run"]);
    assert_eq!(code, 0, "{out}");
    let restore = out["data"]["plan"].as_str().expect("plan").to_string();
    let (code, out) = run(
        &repo,
        &["migrate", "--restore-history", "--apply", &restore],
    );
    assert_eq!(code, 0, "{out}");
    swap(&store_dir, &aside);
    let (code, out) = run(&other, &["migrate", "--dry-run"]);
    assert_ne!(code, 0, "{out}");
    assert!(codes(&out).contains(&"migrate.unsupported".into()), "{out}");
}

#[test]
#[ignore = "the branch-against-main mode is cut from 0.7.1 (back in 0.7.2)"]
fn a_main_that_restored_its_history_is_still_followed_in_store_mode() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    let other = branch_with(&dir, &repo, "other", &[("work/spike-1-look.md", SPIKE)]);
    let hash = dry_run_store(&repo);
    let (code, out) = run(
        &repo,
        &["migrate", "--apply", &hash, "--work-history", "store"],
    );
    assert_eq!(code, 0, "{out}");
    let (code, out) = run(&repo, &["migrate", "--restore-history", "--dry-run"]);
    assert_eq!(code, 0, "{out}");
    let restore = out["data"]["plan"].as_str().expect("plan").to_string();
    let (code, out) = run(
        &repo,
        &["migrate", "--restore-history", "--apply", &restore],
    );
    assert_eq!(code, 0, "{out}");
    // A clean store, as on another machine: main's moved progress section
    // is cited by its node and only replaying gets it here.
    let store_dir = repo.join(".git/mdkg");
    for ext in ["", "-wal", "-shm"] {
        fs::remove_file(store_dir.join(format!("coord.sqlite{ext}"))).ok();
    }
    let (code, out) = run(&other, &["migrate", "--dry-run"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["data"]["work_history"], "store", "{out}");
    assert!(
        out["data"]["against"]["replayed"].as_u64() > Some(0),
        "{out}"
    );
    let hash = out["data"]["plan"].as_str().expect("plan").to_string();
    let (code, out) = run(&other, &["migrate", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
    let kinds = log_kinds(&repo);
    assert!(kinds.iter().any(|k| k == "review.record"), "{kinds:?}");
}

#[test]
#[ignore = "the branch-against-main mode is cut from 0.7.1 (back in 0.7.2)"]
fn a_replay_works_in_a_sha256_repository() {
    let dir = scratch();
    let repo = repo_in(&dir, "repo", "sha256");
    let other = branch_with(&dir, &repo, "other", &[("work/spike-1-look.md", SPIKE)]);
    let hash = dry_run_store(&repo);
    let (code, out) = run(
        &repo,
        &["migrate", "--apply", &hash, "--work-history", "store"],
    );
    assert_eq!(code, 0, "{out}");
    let records = out["data"]["counts"]["records"].clone();
    // A store that never saw main's migration, as on another machine.
    fs::remove_file(repo.join(".git/mdkg/coord.sqlite")).expect("store");
    let (code, out) = run(&other, &["migrate", "--dry-run"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["data"]["against"]["replayed"], records, "{out}");
}

#[test]
#[ignore = "the branch-against-main mode is cut from 0.7.1 (back in 0.7.2)"]
fn a_node_both_sides_made_at_one_path_on_one_day_gets_its_own_id() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    let ours = TASK_3_MAIN.replace("Fix the parser", "Add the docs");
    let other = branch_with(&dir, &repo, "other", &[("work/task-3-parser.md", &ours)]);
    fs::write(repo.join(".mdkg/work/task-3-parser.md"), TASK_3_MAIN).expect("t3");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "main's task-3"]);
    let hash = dry_run(&repo);
    let (code, out) = run(&repo, &["migrate", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
    let main_t3 = fs::read_to_string(repo.join(".mdkg/work/task-3-parser.md")).expect("t3");
    let (code, out) = run(&other, &["migrate", "--dry-run"]);
    assert_eq!(code, 0, "{out}");
    let planned_t3 = planned(&out, ".mdkg/work/task-3-parser.md");
    assert_ne!(id_of(&planned_t3), id_of(&main_t3), "{planned_t3}");
    assert!(!planned_t3.contains("legacy_id:"), "{planned_t3}");
}

#[test]
#[ignore = "the branch-against-main mode is cut from 0.7.1 (back in 0.7.2)"]
fn a_node_the_branch_edited_that_main_moved_is_one_node() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    let t2 = TASK_2.replace("Body.", "Body, edited on the branch.");
    // The branch also points task-1 at task-9, which only main will make.
    let t1 = TASK_1.replace("refs: [chk-1]", "refs: [chk-1, task-9]");
    let other = branch_with(
        &dir,
        &repo,
        "other",
        &[
            ("work/task-2-second.md", &t2),
            ("work/task-1-first.md", &t1),
        ],
    );
    fs::write(repo.join(".mdkg/work/task-9-ninth.md"), TASK_9).expect("t9");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "main's task-9"]);
    let hash = dry_run(&repo);
    let (code, out) = run(&repo, &["migrate", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
    let main_t2 = id_of(&fs::read_to_string(repo.join(".mdkg/work/task-2-second.md")).expect("t2"));
    let main_t9 = id_of(&fs::read_to_string(repo.join(".mdkg/work/task-9-ninth.md")).expect("t9"));
    git(
        &repo,
        &[
            "mv",
            ".mdkg/work/task-2-second.md",
            ".mdkg/work/task-2-renamed.md",
        ],
    );
    git(&repo, &["commit", "-qm", "move task-2"]);
    let (code, out) = run(&other, &["migrate", "--dry-run"]);
    assert_eq!(code, 0, "{out}");
    // The branch's own task-2 has main's id, and main's moved copy isn't
    // added beside it.
    assert_eq!(
        id_of(&planned(&out, ".mdkg/work/task-2-second.md")),
        main_t2
    );
    assert_eq!(planned(&out, ".mdkg/work/task-2-renamed.md"), "", "{out}");
    // A ref to a node only main made names it, in the frontmatter.
    let t1 = planned(&out, ".mdkg/work/task-1-first.md");
    let refs = t1
        .lines()
        .find(|l| l.starts_with("refs:"))
        .unwrap_or_default();
    assert!(refs.contains(&main_t9), "{t1}");
    let hash = out["data"]["plan"].as_str().expect("plan").to_string();
    let (code, out) = run(&other, &["migrate", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
}

#[test]
#[ignore = "the branch-against-main mode is cut from 0.7.1 (back in 0.7.2)"]
fn a_branch_that_added_events_imports_main_s_lines_once() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    // The branch appends to the event log main's migration moves out.
    let more = format!(
        "{EVENTS}{}",
        "{\"ts\":\"2026-10-22T01:02:03.004Z\",\"agent\":\"mdkg-cli\",\"kind\":\"NODE_UPDATED\",\"refs\":[\"task-2\"]}\n"
    );
    let other = branch_with(&dir, &repo, "other", &[("work/events/events.jsonl", &more)]);
    let hash = dry_run_store(&repo);
    let (code, out) = run(
        &repo,
        &["migrate", "--apply", &hash, "--work-history", "store"],
    );
    assert_eq!(code, 0, "{out}");
    let events = |r: &Path| log_kinds(r).iter().filter(|k| *k == "legacy.event").count();
    let on_main = events(&repo);
    let (code, out) = run(&other, &["migrate", "--dry-run"]);
    assert_eq!(code, 0, "{out}");
    let hash = out["data"]["plan"].as_str().expect("plan").to_string();
    let (code, out) = run(&other, &["migrate", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
    // The line from before the fork is in the store once; the branch's
    // own line is added.
    assert_eq!(events(&repo), on_main + 1);
}

#[test]
#[ignore = "the branch-against-main mode is cut from 0.7.1 (back in 0.7.2)"]
fn a_remote_main_with_a_slash_in_its_name_is_followed() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    git(&repo, &["branch", "release/0.7"]);
    git(&repo, &["config", "mdkg.mainRef", "release/0.7"]);
    let other = branch_with(&dir, &repo, "other", &[("work/spike-1-look.md", SPIKE)]);
    let hash = dry_run(&repo);
    let (code, out) = run(&repo, &["migrate", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
    let migrated = git(&repo, &["rev-parse", "HEAD"]);
    git(
        &repo,
        &["update-ref", "refs/remotes/origin/release/0.7", &migrated],
    );
    git(&repo, &["reset", "-q", "--hard", "HEAD^"]);
    let (code, out) = run(&other, &["migrate", "--dry-run"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        out["data"]["against"]["ref"], "refs/remotes/origin/release/0.7",
        "{out}"
    );
}

/// A repository migrated as 0.7.0 did, history in the store, and its
/// restore plan's hash.
fn store_migrated(repo: &Path) -> String {
    let hash = dry_run_store(repo);
    let (code, out) = run(
        repo,
        &["migrate", "--apply", &hash, "--work-history", "store"],
    );
    assert_eq!(code, 0, "{out}");
    let (code, out) = run(repo, &["migrate", "--restore-history", "--dry-run"]);
    assert_eq!(code, 0, "{out}");
    out["data"]["plan"].as_str().expect("plan").to_string()
}

#[test]
fn a_restore_that_stopped_while_writing_finishes_on_the_same_hash() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    let tracked_before = git(&repo, &["ls-files", "--", ".mdkg/work"]);
    let hash = store_migrated(&repo);
    let (code, _) = run_with(
        &repo,
        &["migrate", "--restore-history", "--apply", &hash],
        None,
        &[("MDKG_MIGRATE_RESTORE_CRASH_AFTER_WRITES", "1")],
    );
    assert_eq!(code, 86, "the crash hook fired");
    // The file it wrote isn't taken for history the graph already has.
    let (code, out) = run(&repo, &["migrate", "--restore-history", "--dry-run"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["data"]["plan"], hash.as_str(), "{out}");
    let (code, out) = run(&repo, &["migrate", "--restore-history", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["data"]["stage"], "done", "{out}");
    assert_eq!(git(&repo, &["status", "--porcelain"]), "");
    assert_eq!(
        git(&repo, &["ls-files", "--", ".mdkg/work"]),
        tracked_before
    );
}

#[test]
fn a_restore_whose_index_update_failed_is_finished_by_the_next_apply() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    let hash = store_migrated(&repo);
    let (code, _) = run_with(
        &repo,
        &["migrate", "--restore-history", "--apply", &hash],
        None,
        &[("MDKG_MIGRATE_RESTORE_FAIL_INDEX", "1")],
    );
    assert_ne!(code, 0);
    // HEAD has the restore; the index doesn't, so the files look deleted.
    assert!(git(&repo, &["status", "--porcelain"]).contains("D  .mdkg/work/chk-1-first.md"));
    // A dry run says so.
    let (code, out) = run(&repo, &["migrate", "--restore-history", "--dry-run"]);
    assert_eq!(code, 0, "{out}");
    assert!(
        codes_in(&out["data"]["warnings"]).contains(&"migrate.restore_unfinished".to_string()),
        "{out}"
    );
    let (code, out) = run(&repo, &["migrate", "--restore-history", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["data"]["stage"], "done", "{out}");
    assert_eq!(git(&repo, &["status", "--porcelain"]), "");
}

#[test]
fn a_finished_restore_leaves_later_unstaging_alone() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    let hash = store_migrated(&repo);
    let (code, out) = run(&repo, &["migrate", "--restore-history", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
    // Someone unstages everything the restore added, on purpose.
    git(&repo, &["read-tree", "HEAD^"]);
    let parent_tree = git(&repo, &["rev-parse", "HEAD^^{tree}"]);
    assert_eq!(git(&repo, &["write-tree"]), parent_tree);
    let (code, out) = run(&repo, &["migrate", "--restore-history", "--dry-run"]);
    assert_eq!(code, 0, "{out}");
    assert!(
        !codes_in(&out["data"]["warnings"]).contains(&"migrate.restore_unfinished".to_string()),
        "{out}"
    );
    let (code, out) = run(&repo, &["migrate", "--restore-history", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["data"]["stage"], "nothing-to-restore", "{out}");
    assert_eq!(
        git(&repo, &["write-tree"]),
        parent_tree,
        "the index stays theirs"
    );
}

#[test]
fn a_restore_that_stopped_mid_file_finishes_on_the_same_hash() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    let hash = store_migrated(&repo);
    // A stop part way through writing a file leaves only its staged copy,
    // never part of the file in its place.
    let (code, _) = run_with(
        &repo,
        &["migrate", "--restore-history", "--apply", &hash],
        None,
        &[("MDKG_MIGRATE_RESTORE_CRASH_MID_FILE", "1")],
    );
    assert_eq!(code, 86, "the crash hook fired");
    let left = git(&repo, &["status", "--porcelain", "--untracked-files=all"]);
    assert!(left.contains(".mdkg-restore-"), "{left}");
    assert!(!left.contains(" .mdkg/work/chk-1-first.md"), "{left}");
    let (code, out) = run(&repo, &["migrate", "--restore-history", "--dry-run"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["data"]["plan"], hash.as_str(), "{out}");
    let (code, out) = run(&repo, &["migrate", "--restore-history", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["data"]["stage"], "done", "{out}");
    // The file in place is HEAD's, whole, and the staged copy is gone.
    assert_eq!(
        git(&repo, &["status", "--porcelain", "--untracked-files=all"]),
        ""
    );
}

#[test]
fn a_restore_s_staged_copy_left_after_it_finished_goes_next_time() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    let hash = store_migrated(&repo);
    let (code, out) = run(&repo, &["migrate", "--restore-history", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
    // As if removing its staged copy had failed: the copy is still there,
    // and still listed as the restore's own.
    let short = &hash["sha256:".len().."sha256:".len() + 12];
    let staged = format!(".mdkg/work/.chk-1-first.md.mdkg-restore-{short}");
    fs::write(repo.join(&staged), "copy\n").expect("copy");
    let migrate = repo.join(".git/mdkg/migrate");
    let folder = fs::read_dir(&migrate)
        .expect("migrate folder")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.is_dir())
        .expect("its folder");
    fs::write(
        folder.join("restore-staged"),
        serde_json::json!({ "plan": hash, "staged": [staged] }).to_string(),
    )
    .expect("list");
    let (code, out) = run(&repo, &["migrate", "--restore-history", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["data"]["stage"], "nothing-to-restore", "{out}");
    assert!(!repo.join(&staged).exists());
    assert!(!folder.join("restore-staged").exists());
    assert_eq!(
        git(&repo, &["status", "--porcelain", "--untracked-files=all"]),
        ""
    );
}

#[cfg(unix)]
#[test]
fn a_restore_s_cleanup_never_deletes_through_a_linked_folder() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    let hash = store_migrated(&repo);
    let (code, out) = run(&repo, &["migrate", "--restore-history", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
    let short = &hash["sha256:".len().."sha256:".len() + 12];
    let staged = format!(".mdkg/work/.chk-1-first.md.mdkg-restore-{short}");
    let migrate = repo.join(".git/mdkg/migrate");
    let folder = fs::read_dir(&migrate)
        .expect("migrate folder")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.is_dir())
        .expect("its folder");
    fs::write(
        folder.join("restore-staged"),
        serde_json::json!({ "plan": hash, "staged": [staged] }).to_string(),
    )
    .expect("list");
    // `.mdkg/work` is now a link to a folder outside the checkout that
    // holds a file of the listed name: it isn't the restore's to delete.
    let outside = dir.0.join("outside");
    fs::create_dir_all(&outside).expect("outside");
    copy_tree(&repo.join(".mdkg/work"), &outside);
    let theirs = outside.join(format!(".chk-1-first.md.mdkg-restore-{short}"));
    fs::write(&theirs, "theirs\n").expect("theirs");
    fs::rename(repo.join(".mdkg/work"), dir.0.join("work-was")).expect("move");
    std::os::unix::fs::symlink(&outside, repo.join(".mdkg/work")).expect("link");
    let _ = run(&repo, &["migrate", "--restore-history", "--apply", &hash]);
    assert_eq!(fs::read_to_string(&theirs).expect("kept"), "theirs\n");
}

#[test]
fn a_restore_leaves_a_file_it_didnt_stage_alone() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    let hash = store_migrated(&repo);
    // Someone's file where the restore would stage a copy: it isn't one
    // the restore listed as its own, so it's refused, not replaced.
    let short = &hash["sha256:".len().."sha256:".len() + 12];
    let theirs = repo.join(format!(".mdkg/work/.chk-1-first.md.mdkg-restore-{short}"));
    fs::write(&theirs, "mine\n").expect("theirs");
    let (code, out) = run(&repo, &["migrate", "--restore-history", "--apply", &hash]);
    assert_ne!(code, 0, "{out}");
    assert!(codes(&out).contains(&"migrate.dirty".into()), "{out}");
    assert_eq!(fs::read_to_string(&theirs).expect("kept"), "mine\n");
}

#[test]
fn a_restore_with_changes_staged_on_its_unfinished_index_is_refused() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    let hash = store_migrated(&repo);
    let (code, _) = run_with(
        &repo,
        &["migrate", "--restore-history", "--apply", &hash],
        None,
        &[("MDKG_MIGRATE_RESTORE_FAIL_INDEX", "1")],
    );
    assert_ne!(code, 0);
    // Something staged on top of the parent's index.
    fs::write(repo.join("NOTES.md"), "draft\n").expect("notes");
    git(&repo, &["add", "NOTES.md"]);
    let (code, out) = run(&repo, &["migrate", "--restore-history", "--dry-run"]);
    assert_eq!(code, 0, "{out}");
    assert!(
        codes_in(&out["data"]["warnings"]).contains(&"migrate.restore_unfinished".to_string()),
        "{out}"
    );
    let staged = git(&repo, &["write-tree"]);
    let (code, out) = run(&repo, &["migrate", "--restore-history", "--apply", &hash]);
    assert_ne!(code, 0, "{out}");
    assert!(codes(&out).contains(&"migrate.dirty".into()), "{out}");
    assert_eq!(
        git(&repo, &["write-tree"]),
        staged,
        "the index stays theirs"
    );
    // The way out the hint gives.
    git(&repo, &["reset", "-q"]);
    let (code, out) = run(&repo, &["migrate", "--restore-history", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["data"]["stage"], "nothing-to-restore", "{out}");
}

#[test]
fn a_restore_dry_run_changes_nothing_in_the_migration_folder() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    let hash = store_migrated(&repo);
    let (code, _) = run_with(
        &repo,
        &["migrate", "--restore-history", "--apply", &hash],
        None,
        &[("MDKG_MIGRATE_RESTORE_FAIL_INDEX", "1")],
    );
    assert_ne!(code, 0);
    let folder = git(&repo, &["rev-parse", "--git-common-dir"]);
    let folder = repo.join(folder).join("mdkg/migrate");
    let listing = |dir: &Path| -> BTreeMap<String, Vec<u8>> { snapshot(dir) };
    let before = listing(&folder);
    // HEAD moves on, so the pending file is stale: a dry run still leaves
    // it for an apply, under the lock, to clear.
    git(&repo, &["reset", "-q"]);
    git(&repo, &["commit", "-q", "--allow-empty", "-m", "later"]);
    let (code, out) = run(&repo, &["migrate", "--restore-history", "--dry-run"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(listing(&folder), before);
}

#[cfg(unix)]
#[test]
fn a_restore_refuses_a_link_where_a_file_goes() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    let hash = store_migrated(&repo);
    // A link whose target holds exactly the bytes the restore would write
    // isn't the file: git would commit a link.
    let text = isolated(Command::new("git").arg("-C").arg(&repo))
        .args(["show", "HEAD^:.mdkg/work/chk-1-first.md"])
        .output()
        .expect("show")
        .stdout;
    let target = dir.0.join("elsewhere.md");
    fs::write(&target, &text).expect("target");
    std::os::unix::fs::symlink(&target, repo.join(".mdkg/work/chk-1-first.md")).expect("link");
    let (code, out) = run(&repo, &["migrate", "--restore-history", "--apply", &hash]);
    assert_ne!(code, 0, "{out}");
    assert!(codes(&out).contains(&"migrate.dirty".into()), "{out}");
    assert!(
        fs::symlink_metadata(repo.join(".mdkg/work/chk-1-first.md"))
            .expect("link")
            .file_type()
            .is_symlink()
    );
}

#[test]
fn a_restore_never_writes_through_a_link() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    let hash = store_migrated(&repo);
    // The event log's folder, gone with the migration, is now a link out.
    let outside = dir.0.join("outside");
    fs::create_dir_all(&outside).expect("outside");
    fs::remove_dir_all(repo.join(".mdkg/work/events")).ok();
    std::os::unix::fs::symlink(&outside, repo.join(".mdkg/work/events")).expect("link");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "a link"]);
    let (_, out) = run(&repo, &["migrate", "--restore-history", "--dry-run"]);
    let hash2 = out["data"]["plan"].as_str().expect("plan").to_string();
    assert_ne!(hash2, hash, "the base moved");
    let (code, out) = run(&repo, &["migrate", "--restore-history", "--apply", &hash2]);
    assert_ne!(code, 0, "{out}");
    assert!(codes(&out).contains(&"migrate.dirty".into()), "{out}");
    assert_eq!(fs::read_dir(&outside).expect("outside").count(), 0);
    assert_eq!(git(&repo, &["status", "--porcelain"]), "");
}

#[test]
fn a_restore_finds_a_migration_merged_with_no_ff_and_takes_main_lines_copy() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    // The migration happens on a branch...
    let wt = dir.0.join("mig");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "mig",
            wt.to_str().expect("utf-8"),
            "main",
        ],
    );
    let hash = dry_run_store(&wt);
    let (code, out) = run(
        &wt,
        &["migrate", "--apply", &hash, "--work-history", "store"],
    );
    assert_eq!(code, 0, "{out}");
    // ...while main edits the checkpoint, then merges it with --no-ff,
    // resolving the checkpoint as the migration did: gone.
    let chk = repo.join(".mdkg/work/chk-1-first.md");
    let edited = fs::read_to_string(&chk).expect("chk") + "\nEdited on main.\n";
    fs::write(&chk, &edited).expect("edit");
    git(&repo, &["commit", "-qam", "main edits the checkpoint"]);
    let merged = isolated(Command::new("git").arg("-C").arg(&repo))
        .args(["merge", "-q", "--no-ff", "--no-edit", "mig"])
        .output()
        .expect("merge");
    assert!(
        !merged.status.success(),
        "the edit conflicts with the deletion"
    );
    git(&repo, &["rm", "-q", ".mdkg/work/chk-1-first.md"]);
    git(&repo, &["commit", "-q", "--no-edit"]);
    let (code, out) = run(&repo, &["migrate", "--restore-history", "--dry-run"]);
    assert_eq!(code, 0, "{out}");
    let hash = out["data"]["plan"].as_str().expect("plan").to_string();
    let (code, out) = run(&repo, &["migrate", "--restore-history", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
    let back = fs::read_to_string(&chk).expect("restored");
    assert!(back.contains("Edited on main."), "main's own copy: {back}");
    let (code, out) = run(&repo, &["validate"]);
    assert_eq!(code, 0, "{out}");
}

#[test]
fn a_restore_in_a_graph_of_two_workspaces_names_the_right_nodes() {
    let dir = scratch();
    let repo = two_ws_repo(&dir, "repo");
    let hash = store_migrated(&repo);
    let (code, out) = run(&repo, &["migrate", "--restore-history", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
    let api1 = fs::read_to_string(repo.join("services/api/.mdkg/work/task-1-api.md")).expect("t1");
    let chk = fs::read_to_string(repo.join("services/api/.mdkg/work/chk-1-api.md")).expect("chk");
    assert!(
        chk.contains(&format!("\nscope: [{}]\n", id_of(&api1))),
        "{chk}"
    );
    let root_chk = fs::read_to_string(repo.join(".mdkg/work/chk-1-first.md")).expect("chk");
    assert_ne!(id_of(&root_chk), id_of(&chk));
    let (code, out) = run(&repo, &["validate"]);
    assert_eq!(code, 0, "{out}");
}

#[test]
fn a_restore_of_an_identity_v2_graph_keeps_its_refs() {
    let dir = scratch();
    let repo = v2_repo(&dir);
    let hash = store_migrated(&repo);
    let (code, out) = run(&repo, &["migrate", "--restore-history", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
    let chk = fs::read_to_string(repo.join(".mdkg/work/chk-1-started.md")).expect("chk");
    assert!(chk.contains(&format!("task-{V2_TASK_1}")), "{chk}");
    assert!(!chk.contains("scope: []"), "{chk}");
    let (code, out) = run(&repo, &["validate"]);
    assert_eq!(code, 0, "{out}");
}

/// The migration's own folder, from `--status`'s backup path.
fn out_status_folder(repo: &Path) -> String {
    let (_, out) = run(repo, &["migrate", "--status"]);
    let copy = out["data"]["backups"]["graph_copy"].as_str().expect("copy");
    Path::new(copy)
        .parent()
        .expect("folder")
        .display()
        .to_string()
}

#[test]
fn a_resumed_apply_says_where_a_rejected_graph_was_kept() {
    let dir = scratch();
    let repo = two_ws_repo(&dir, "repo");
    let hash = dry_run(&repo);
    let (code, _) = run_with(
        &repo,
        &["migrate", "--apply", &hash],
        None,
        &[
            ("MDKG_MIGRATE_BREAK_STAGED", "1"),
            ("MDKG_MIGRATE_CRASH_IN_SWAP_BACK", "1"),
        ],
    );
    assert_eq!(code, 86);
    // Run again: the rejected graphs are kept before anything is rebuilt,
    // the migration finishes, and its report names them.
    let (code, out) = run(&repo, &["migrate", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["data"]["stage"], "done", "{out}");
    let kept: Vec<&Value> = out["data"]["warnings"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|w| w["code"] == "migrate.rejected_kept")
        .collect();
    assert_eq!(kept.len(), 2, "{out}");
    // So does --status, later.
    let (_, out) = run(&repo, &["migrate", "--status"]);
    assert!(
        codes_in(&out["data"]["warnings"]).contains(&"migrate.rejected_kept".to_string()),
        "{out}"
    );
}

#[test]
fn a_kept_graph_is_reported_after_a_crash_while_keeping() {
    let dir = scratch();
    let repo = two_ws_repo(&dir, "repo");
    let hash = dry_run(&repo);
    // A crash after the first rejected folder was kept, before the second.
    let (code, _) = run_with(
        &repo,
        &["migrate", "--apply", &hash],
        None,
        &[
            ("MDKG_MIGRATE_BREAK_STAGED", "1"),
            ("MDKG_MIGRATE_CRASH_AFTER_KEEP", "1"),
        ],
    );
    assert_eq!(code, 86);
    let (code, out) = run(&repo, &["migrate", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
    let kept: Vec<String> = out["data"]["warnings"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|w| w["code"] == "migrate.rejected_kept")
        .filter_map(|w| w["message"].as_str().map(String::from))
        .collect();
    assert_eq!(kept.len(), 2, "both kept folders are named: {out}");
}

#[test]
fn a_rejected_graph_of_two_workspaces_is_swapped_back_whole_and_kept() {
    let dir = scratch();
    for crash in [false, true] {
        let repo = two_ws_repo(&dir, if crash { "crash" } else { "plain" });
        let before = snapshot(&repo);
        let hash = dry_run(&repo);
        let mut env = vec![("MDKG_MIGRATE_BREAK_STAGED", "1")];
        if crash {
            // A crash after both folders swapped back, before the rejected
            // graphs were moved out of the way.
            env.push(("MDKG_MIGRATE_CRASH_IN_SWAP_BACK", "1"));
        }
        let (code, out) = run_with(&repo, &["migrate", "--apply", &hash], None, &env);
        if crash {
            assert_eq!(code, 86);
        } else {
            let out = out.expect("json");
            assert!(codes(&out).contains(&"migrate.step_failed".into()), "{out}");
        }
        // Both folders are the old graph again; HEAD never moved.
        let visible: BTreeMap<String, Vec<u8>> = snapshot(&repo)
            .into_iter()
            .filter(|(p, _)| !p.contains(".mdkg.migrate-"))
            .collect();
        assert_eq!(visible, before);
        let folder = PathBuf::from(out_status_folder(&repo));
        if crash {
            // The rollback keeps them before it clears the staging paths,
            // and says where.
            let (code, out) = run(&repo, &["migrate", "--rollback"]);
            assert_eq!(code, 0, "{out}");
            let said = codes_in(&out["data"]["warnings"])
                .iter()
                .filter(|c| *c == "migrate.rejected_kept")
                .count();
            assert_eq!(said, 2, "{out}");
        }
        let kept: Vec<String> = fs::read_dir(&folder)
            .expect("folder")
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|n| n.starts_with("kept-"))
            .collect();
        assert_eq!(kept.len(), 2, "both rejected folders are kept: {kept:?}");
        if !crash {
            let (code, out) = run(&repo, &["migrate", "--rollback"]);
            assert_eq!(code, 0, "{out}");
        }
        assert_eq!(snapshot(&repo), before);
    }
}

/// Text `migrate --dry-run` escapes what the repository supplies: a
/// workspace whose folder holds ESC prints it as `\u{1b}`, never raw
/// (Terra #15894).
#[test]
fn a_dry_run_s_text_escapes_workspace_folders() {
    let dir = scratch();
    let repo = repo(&dir, "escapes");
    let folder = "services/a\u{1b}[2Jpi";
    fs::create_dir_all(repo.join(folder).join(".mdkg/work")).expect("workspace");
    fs::write(
        repo.join(folder).join(".mdkg/work/task-1-api.md"),
        "---\nid: task-1\ntype: task\ntitle: API task\nstatus: todo\ncreated: 2026-10-20\nupdated: 2026-10-20\n---\n",
    )
    .expect("task");
    let config = repo.join(".mdkg/config.json");
    let mut cfg: Value =
        serde_json::from_str(&fs::read_to_string(&config).expect("config")).expect("json");
    cfg["workspaces"]["api"] = serde_json::json!({
        "path": folder, "enabled": true, "mdkg_dir": ".mdkg", "visibility": "private"
    });
    fs::write(
        &config,
        serde_json::to_string_pretty(&cfg).expect("json") + "\n",
    )
    .expect("config");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "hostile workspace"]);
    let out = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
        .arg("--root")
        .arg(&repo)
        .args(["--as", "a", "migrate", "--dry-run"])
        .env("MDKG_NOW_MS", "1000")
        .output()
        .expect("run mdkg");
    let text = String::from_utf8_lossy(&out.stdout);
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{text}{err}");
    assert!(text.contains("workspaces, each kept in place"), "{text}");
    assert!(text.contains("a\\u{1b}[2Jpi"), "{text:?}");
    assert!(!text.contains('\u{1b}'), "{text:?}");
    assert!(!err.contains('\u{1b}'), "{err:?}");
}

/// Text `migrate --status` escapes what the journal holds: a worker can
/// edit `journal.json`, and its `commit` isn't covered by the plan's hash
/// (Terra #15997).
#[test]
fn a_status_s_text_escapes_the_journal() {
    let dir = scratch();
    let repo = repo(&dir, "journal-escapes");
    let hash = dry_run(&repo);
    let (code, _) = run_env(&repo, &["migrate", "--apply", &hash], Some("committed"));
    assert_eq!(code, 86);
    let journal = Path::new(out_status_folder(&repo).as_str())
        .parent()
        .expect("root")
        .join("journal.json");
    let mut v: Value = serde_json::from_slice(&fs::read(&journal).expect("journal")).expect("json");
    v["commit"] = Value::String("abc\u{1b}[2Jforged".into());
    fs::write(&journal, serde_json::to_vec(&v).expect("json")).expect("journal");
    let out = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
        .arg("--root")
        .arg(&repo)
        .args(["--as", "a", "migrate", "--status"])
        .env("MDKG_NOW_MS", "1000")
        .output()
        .expect("run mdkg");
    let text = String::from_utf8_lossy(&out.stdout);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(text.contains("abc\\u{1b}[2Jforged"), "{text:?}{err:?}");
    assert!(!text.contains('\u{1b}'), "{text:?}");
    assert!(!err.contains('\u{1b}'), "{err:?}");
}
