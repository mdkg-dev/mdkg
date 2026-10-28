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
    git(&repo, &["init", "-q", "-b", "main"]);
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

#[test]
fn dry_run_apply_and_rollback_round_trip() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    // A coordinator store, so the snapshot runs.
    let (code, out) = run(&repo, &["claim", "task-2"]);
    assert_eq!(code, 0, "{out}");
    let before = snapshot(&repo);
    let base = git(&repo, &["rev-parse", "HEAD"]);

    let (code, out) = run(&repo, &["migrate", "--dry-run"]);
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
    assert!(
        !repo.join(".mdkg/work/chk-1-first.md").exists(),
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
    fs::write(
        repo.join(".mdkg/work/task-2-second.md"),
        TASK_2.replace(
            "status: backlog",
            &format!("status: backlog\nrefs: [{refs}]"),
        ),
    )
    .expect("ref");
    git(repo, &["commit", "-q", "-am", "ref"]);
    let (code, out) = run(repo, &["migrate", "--dry-run"]);
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
    let task2 = planned_task2(&repo, "test-9");
    assert!(task2.contains("refs: []"), "{task2}");
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
    let hash = dry_run(&repo);
    let (code, out) = run(&repo, &["migrate", "--apply", &hash]);
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
    let (code, _) = run_env(&repo, &["migrate", "--apply", &hash], Some("recorded"));
    assert_eq!(code, 86);
    let (code, out) = run(&repo, &["migrate", "--apply", &hash]);
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
    let hash = dry_run(&repo);
    let (code, out) = run(&repo, &["migrate", "--apply", &hash]);
    assert_ne!(code, 0, "{out}");
    assert_eq!(snapshot(&repo), before, "nothing changed");
    let (code, _) = run(&repo, &["migrate", "--status"]);
    assert_ne!(code, 0, "no migration started");
}

#[test]
fn every_summary_digest_names_a_record_in_the_store() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    let hash = dry_run(&repo);
    let (code, out) = run(&repo, &["migrate", "--apply", &hash]);
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
    let hash = dry_run(&repo);
    // A crash after the backups (the snapshot is taken), then an agent
    // writes before the import.
    let (code, _) = run_env(&repo, &["migrate", "--apply", &hash], Some("backed-up"));
    assert_eq!(code, 86);
    let (code, out) = run(&repo, &["release", "task-2"]);
    assert_eq!(code, 0, "{out}");
    let (code, out) = run(&repo, &["migrate", "--apply", &hash]);
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
    let hash = dry_run(&repo);
    let (code, _) = run_env(&repo, &["migrate", "--apply", &hash], Some("recorded"));
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
    let hash = dry_run(&repo);
    let (code, out) = run(&repo, &["migrate", "--apply", &hash]);
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
fn a_branch_migrated_after_main_merges_cleanly() {
    let dir = scratch();
    let repo = repo(&dir, "repo");
    // An unmerged branch with one node of its own.
    let other = branch_with(&dir, &repo, "other", &[("work/spike-1-look.md", SPIKE)]);
    // main migrates, then the branch migrates on its own, before merging.
    let hash = dry_run(&repo);
    let (code, out) = run(&repo, &["migrate", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
    let hash = dry_run(&other);
    let (code, out) = run(&other, &["migrate", "--apply", &hash]);
    assert_eq!(code, 0, "{out}");
    // Both imports ran, and the branch's skipped what main's already
    // imported: the store judges that by migrate_digest inside the
    // import's own transaction (lane 2), so each shared record is there
    // once.
    let kinds = log_kinds(&repo);
    let count = |k: &str| kinds.iter().filter(|x| *x == k).count();
    assert_eq!(count("log.import"), 2, "{kinds:?}");
    let markers = log_markers(&repo);
    assert_eq!(markers[0]["skipped"], 0, "main's import skips nothing");
    assert!(
        markers[1]["skipped"].as_u64() > Some(0),
        "the branch's skips main's shared records: {markers:?}"
    );
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
    assert!(git(&repo, &["ls-files", "--", ".mdkg/work/chk-1-first.md"]).is_empty());
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
    assert_eq!(out["data"]["counts"]["checkpoints"], 1, "{out}");
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
    assert!(!repo.join(".mdkg/work/chk-1-started.md").exists());
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
    assert!(task1.contains("- chk-1 (2026-01-17): Started"), "{task1}");
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
    let (code, out) = run(&repo, &["migrate", "--rollback"]);
    assert_eq!(code, 0, "{out}");
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
fn a_graph_of_several_workspaces_is_refused_with_the_way_forward() {
    let dir = scratch();
    let repo = dir.0.join("ws");
    let case = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance/cases/s02-workspaces");
    fs::create_dir_all(&repo).expect("repo");
    copy_tree(&case, &repo);
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    // 0.7.0 reads it as it is.
    let (code, out) = run(&repo, &["validate"]);
    assert_eq!(code, 0, "{out}");
    let (code, out) = run(&repo, &["migrate", "--dry-run"]);
    assert_ne!(code, 0, "{out}");
    assert!(codes(&out).contains(&"migrate.unsupported".into()), "{out}");
    let hint = out["diagnostics"][0]["hint"].as_str().expect("hint");
    assert!(
        hint.contains("0.7.1") && hint.contains("0.6 form"),
        "{hint}"
    );
    assert!(!hint.contains("has no workspaces"), "{hint}");
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
