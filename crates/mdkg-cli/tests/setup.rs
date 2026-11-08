//! The setup commands: `init`, `doctor` and `skills` (edd-6, lane 6), each
//! in a temporary repository.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

mod common;

const BIN: &str = env!("CARGO_BIN_EXE_mdkg");

/// A fresh folder, optionally a git repository.
fn temp(name: &str, git: bool) -> PathBuf {
    let dir = common::temp::dir(&format!("mdkg-setup-{name}"));
    let dir = std::fs::canonicalize(&dir).expect("canonical");
    if git {
        let ok = Command::new("git")
            .args(["init", "-q"])
            .current_dir(&dir)
            .status()
            .expect("git init")
            .success();
        assert!(ok);
    }
    dir
}

fn run(dir: &Path, args: &[&str]) -> Output {
    common::without_launcher_env(&mut Command::new(BIN))
        .args(args)
        .current_dir(dir)
        .env("MDKG_ACTOR", "tester")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .expect("run the binary")
}

fn json(dir: &Path, args: &[&str]) -> (i32, Value) {
    let out = run(dir, args);
    let v: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|_| {
        panic!(
            "no JSON from {args:?}: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    });
    (out.status.code().unwrap_or(-1), v)
}

fn codes(v: &Value) -> Vec<String> {
    v["diagnostics"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|d| d["code"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

fn config(dir: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(dir.join(".mdkg/config.json")).expect("config"))
        .expect("config is JSON")
}

fn is_uuid_v7(s: &str) -> bool {
    s.len() == 36
        && s.as_bytes()[14] == b'7'
        && s.bytes().all(|b| b == b'-' || b.is_ascii_hexdigit())
}

/// Every file under a folder, relative to it, sorted, leaving out `.git`.
fn tree(dir: &Path) -> Vec<String> {
    let mut out: Vec<String> = walk(dir)
        .into_iter()
        .filter_map(|p| {
            p.strip_prefix(dir)
                .ok()
                .map(|r| r.to_string_lossy().replace('\\', "/"))
        })
        .filter(|r| !r.starts_with(".git/"))
        .collect();
    out.sort();
    out
}

/// Every file's bytes, to show a command left them all alone.
fn snapshot(dir: &Path) -> Vec<(String, Vec<u8>)> {
    tree(dir)
        .into_iter()
        .map(|r| {
            let bytes = std::fs::read(dir.join(&r)).unwrap_or_default();
            (r, bytes)
        })
        .collect()
}

const AGENT_READY: &[&str] = &[
    ".agents/skills/.mdkg-managed.json",
    ".agents/skills/mdkg-workflow/SKILL.md",
    ".claude/skills/.mdkg-managed.json",
    ".claude/skills/mdkg-workflow/SKILL.md",
    ".codex/config.toml",
    ".mcp.json",
    ".mdkg/README.md",
    ".mdkg/config.json",
    ".mdkg/skills/mdkg-workflow/SKILL.md",
    "AGENTS.md",
    "opencode.json",
];

/// Every projection there was before they were opt-in.
const ALL_PROJECTIONS: &[&str] = &[
    "agents-md",
    "mcp:claude",
    "mcp:codex",
    "mcp:opencode",
    "skills:.agents/skills",
    "skills:.claude/skills",
];

/// Lists `names` as the config's projections, without syncing them.
fn declare(dir: &Path, names: &[&str]) {
    let mut cfg = config(dir);
    cfg["projections"] = serde_json::json!(names);
    let text = serde_json::to_string_pretty(&cfg).expect("json") + "\n";
    std::fs::write(dir.join(".mdkg/config.json"), text).expect("write");
}

/// Enables `names` with install, which lists them in the config and
/// syncs them.
fn enable(dir: &Path, names: &[&str]) {
    let (code, v) = json(dir, &["install", "--enable", &names.join(","), "--json"]);
    assert_eq!(code, 0, "{v}");
}

/// The skill mirrors alone.
const MIRRORS: &[&str] = &["skills:.agents/skills", "skills:.claude/skills"];

#[test]
fn init_writes_only_mdkg_until_projections_are_enabled() {
    let dir = temp("init-b", true);
    let (code, v) = json(&dir, &["init", "--json"]);
    assert_eq!(code, 0, "{v}");
    common::conforms("init", &v);
    // Init writes only its README, config and skill inside .mdkg.
    // No nodes, templates or harness integrations are created.
    assert_eq!(
        tree(&dir),
        [
            ".mdkg/README.md",
            ".mdkg/config.json",
            ".mdkg/skills/mdkg-workflow/SKILL.md"
        ],
        "{v}"
    );
    assert!(
        v["data"]["notes"].to_string().contains("install --enable"),
        "{v}"
    );
    // The config is the consult's, key for key, opting into nothing.
    let cfg = config(&dir);
    let keys: Vec<&String> = cfg.as_object().expect("object").keys().collect();
    assert_eq!(
        keys,
        [
            "schema_version",
            "format_version",
            "graph_id",
            "work",
            "orchestration",
            "projections"
        ]
    );
    assert_eq!(cfg["projections"], serde_json::json!([]));
    let (code, sv) = json(&dir, &["skills", "validate", "--json"]);
    assert_eq!(code, 0, "{sv}");
    assert_eq!(codes(&sv), Vec::<String>::new(), "{sv}");
    // Enabling every projection gives what init wrote before opt-in.
    enable(&dir, ALL_PROJECTIONS);
    assert_eq!(tree(&dir), AGENT_READY);
    assert_eq!(cfg["schema_version"], 3);
    assert_eq!(cfg["format_version"], "0.7");
    let id = cfg["graph_id"].as_str().expect("graph_id");
    assert!(is_uuid_v7(id), "{id}");
    assert_eq!(v["data"]["graph_id"], id);
    assert_eq!(cfg["work"]["location"], "local");
    assert_eq!(cfg["orchestration"]["branch"], "agent/{agent}");
    assert_eq!(cfg["orchestration"]["worktree"], ".worktrees/{agent}");
    assert!(!dir.join(".gitignore").exists(), "no worktree exists yet");
    // The agents' configs start this binary: the path it was run by, since
    // the test binary isn't `mdkg` on PATH.
    let mcp: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join(".mcp.json")).expect("mcp"))
            .expect("json");
    assert_eq!(mcp["mcpServers"]["mdkg"]["command"], BIN, "{mcp}");
    assert!(
        std::fs::read_to_string(dir.join("AGENTS.md"))
            .expect("agents")
            .contains("mdkg")
    );
    // The seeded skill is the shipped one, mirrored.
    let skill = std::fs::read(dir.join(".mdkg/skills/mdkg-workflow/SKILL.md")).expect("skill");
    assert_eq!(
        std::fs::read(dir.join(".claude/skills/mdkg-workflow/SKILL.md")).expect("mirror"),
        skill
    );
    let (code, sv) = json(&dir, &["skills", "validate", "--json"]);
    assert_eq!(code, 0, "{sv}");
    assert_eq!(codes(&sv), Vec::<String>::new(), "{sv}");
    // Two graphs never share an id.
    let other = temp("init-b2", true);
    json(&other, &["init", "--json"]);
    assert_ne!(config(&other)["graph_id"], cfg["graph_id"]);
}

#[test]
fn init_graph_only_writes_the_config_and_readme() {
    let dir = temp("init-graph-only", true);
    let (code, v) = json(&dir, &["init", "--graph-only", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(tree(&dir), [".mdkg/README.md", ".mdkg/config.json"], "{v}");
    assert_eq!(v["data"]["agents"], false);
    // `skills add` brings the shipped skill in later; it's mirrored once a
    // mirror is enabled.
    let (code, v) = json(&dir, &["skills", "add", "mdkg-workflow", "--json"]);
    assert_eq!(code, 0, "{v}");
    common::conforms("skills.add", &v);
    assert_eq!(v["data"]["action"], "created");
    assert!(
        !dir.join(".agents").exists(),
        "no mirror until one is enabled"
    );
    enable(&dir, &["skills:.agents/skills"]);
    assert!(dir.join(".agents/skills/mdkg-workflow/SKILL.md").is_file());
    assert!(!dir.join(".claude").exists());
    let (_, v) = json(&dir, &["skills", "add", "mdkg-workflow", "--json"]);
    assert_eq!(v["data"]["action"], "kept", "{v}");
    // The project's own edit is never replaced.
    std::fs::write(
        dir.join(".mdkg/skills/mdkg-workflow/SKILL.md"),
        "---\nname: mdkg-workflow\ndescription: ours\n---\n",
    )
    .expect("write");
    let (code, v) = json(&dir, &["skills", "add", "mdkg-workflow", "--json"]);
    assert_eq!(code, 1, "{v}");
    assert_eq!(codes(&v), ["skill.exists"]);
    // Unless --replace asks for the shipped copy: replaced and mirrored.
    let (code, v) = json(
        &dir,
        &["skills", "add", "mdkg-workflow", "--replace", "--json"],
    );
    assert_eq!(code, 0, "{v}");
    common::conforms("skills.add", &v);
    assert_eq!(v["data"]["action"], "replaced", "{v}");
    let shipped = std::fs::read(dir.join(".mdkg/skills/mdkg-workflow/SKILL.md")).expect("skill");
    assert!(!String::from_utf8_lossy(&shipped).contains("description: ours"));
    assert_eq!(
        std::fs::read(dir.join(".agents/skills/mdkg-workflow/SKILL.md")).expect("mirror"),
        shipped
    );
    let (code, v) = json(&dir, &["skills", "add", "something-else", "--json"]);
    assert_eq!(code, 3, "{v}");
    assert_eq!(codes(&v), ["skill.not_found"]);
}

#[test]
fn init_again_on_its_graph_changes_no_byte() {
    let dir = temp("init-again", true);
    json(&dir, &["init", "--json"]);
    let before = snapshot(&dir);
    let (code, v) = json(&dir, &["init", "--json"]);
    assert_eq!(code, 0, "{v}");
    common::conforms("init", &v);
    assert_eq!(v["data"]["already"], true);
    assert_eq!(v["data"]["graph_id"], config(&dir)["graph_id"]);
    // The same choices, given explicitly, are no conflict.
    let (code, v) = json(
        &dir,
        &[
            "init",
            "--work",
            "local",
            "--branch-pattern",
            "agent/{agent}",
            "--json",
        ],
    );
    assert_eq!(code, 0, "{v}");
    assert_eq!(snapshot(&dir), before);
}

#[test]
fn init_refuses_a_conflicting_choice_and_changes_no_byte() {
    let dir = temp("init-conflict", true);
    json(&dir, &["init", "--json"]);
    let before = snapshot(&dir);
    for args in [
        &["init", "--work", "shared", "--json"][..],
        &["init", "--branch-pattern", "lanes/{agent}", "--json"],
        &["init", "--worktree-pattern", "../wt-{agent}", "--json"],
    ] {
        let (code, v) = json(&dir, args);
        assert_eq!(code, 1, "{args:?}: {v}");
        assert_eq!(codes(&v), ["init.conflict"], "{args:?}");
    }
    assert_eq!(snapshot(&dir), before);
}

#[test]
fn init_refuses_a_0_6_graph_and_points_to_migrate() {
    let dir = temp("init-legacy", true);
    std::fs::create_dir_all(dir.join(".mdkg/work")).expect("mkdir");
    std::fs::write(
        dir.join(".mdkg/config.json"),
        r#"{"schema_version": 1, "tool": "mdkg", "workspaces": {"root": {"path": ".", "enabled": true, "mdkg_dir": ".mdkg"}}}"#,
    )
    .expect("write");
    std::fs::write(
        dir.join(".mdkg/work/task-1-a.md"),
        "---\nid: task-1\ntype: task\ntitle: a\nstatus: todo\n---\n",
    )
    .expect("write");
    let before = snapshot(&dir);
    let (code, v) = json(&dir, &["init", "--json"]);
    assert_eq!(code, 1, "{v}");
    assert_eq!(codes(&v), ["init.needs_migrate"]);
    assert!(
        v["diagnostics"][0]["hint"]
            .as_str()
            .unwrap_or("")
            .contains("migrate"),
        "{v}"
    );
    assert_eq!(snapshot(&dir), before);
}

#[test]
fn init_leaves_a_malformed_config_or_an_unexplained_mdkg_alone() {
    // A config that isn't JSON: diagnosed, kept.
    let dir = temp("init-malformed", true);
    std::fs::create_dir_all(dir.join(".mdkg")).expect("mkdir");
    std::fs::write(dir.join(".mdkg/config.json"), "{ not json").expect("write");
    let before = snapshot(&dir);
    let (code, v) = json(&dir, &["init", "--json"]);
    assert_ne!(code, 0, "{v}");
    assert_eq!(codes(&v), ["config.unreadable"]);
    assert_eq!(snapshot(&dir), before);
    // Files init didn't write, and no config: kept, and refused.
    let dir = temp("init-unexplained", true);
    std::fs::create_dir_all(dir.join(".mdkg/work")).expect("mkdir");
    std::fs::write(dir.join(".mdkg/work/notes.md"), "mine").expect("write");
    let before = snapshot(&dir);
    let (code, v) = json(&dir, &["init", "--json"]);
    assert_eq!(code, 1, "{v}");
    assert_eq!(codes(&v), ["init.unexplained"]);
    assert_eq!(snapshot(&dir), before);
    // A seeded skill someone edited isn't init's own output either.
    let dir = temp("init-edited-skill", true);
    std::fs::create_dir_all(dir.join(".mdkg/skills/mdkg-workflow")).expect("mkdir");
    std::fs::write(dir.join(".mdkg/skills/mdkg-workflow/SKILL.md"), "edited").expect("write");
    let (code, v) = json(&dir, &["init", "--json"]);
    assert_eq!(code, 1, "{v}");
    assert_eq!(codes(&v), ["init.unexplained"]);
}

#[test]
fn init_local_work_and_its_patterns() {
    let dir = temp("init-c", true);
    let (code, v) = json(
        &dir,
        &[
            "init",
            "--work",
            "local",
            "--branch-pattern",
            "lanes/{agent}",
            "--worktree-pattern",
            "../wt-{agent}",
            "--json",
        ],
    );
    assert_eq!(code, 0, "{v}");
    let cfg = config(&dir);
    assert_eq!(cfg["work"]["location"], "local");
    assert_eq!(cfg["orchestration"]["branch"], "lanes/{agent}");
    // A worktree folder outside the checkout needs no ignore line.
    assert!(!dir.join(".gitignore").exists());
    // Shared work uses the canonical git location in config.
    let other = temp("init-none", true);
    json(
        &other,
        &["init", "--work", "shared", "--graph-only", "--json"],
    );
    assert_eq!(config(&other)["work"]["location"], "git");
}

#[test]
fn init_dry_run_writes_nothing() {
    let dir = temp("init-dry", true);
    let (code, v) = json(&dir, &["init", "--dry-run", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["data"]["dry_run"], true);
    assert!(!v["data"]["changes"].as_array().expect("files").is_empty());
    assert_eq!(tree(&dir), Vec::<String>::new());
}

#[test]
fn init_refuses_bad_patterns_without_writing() {
    let dir = temp("init-bad", true);
    for args in [
        ["--branch-pattern", "agent"],
        ["--branch-pattern", "a b/{agent}"],
        ["--worktree-pattern", "/abs/{agent}"],
        ["--worktree-pattern", "x/../{agent}"],
        ["--worktree-pattern", "{agent}/{agent}"],
        ["--worktree-pattern", "wt/\"{agent}\""],
        ["--branch-pattern", "agent/\"{agent}\""],
        // What the launcher refuses, init refuses too.
        ["--worktree-pattern", "{agent}/wt"],
        // A `.` part, which an ignore line would keep but git wouldn't match.
        ["--worktree-pattern", "tools/./wt-{agent}"],
        ["--worktree-pattern", "./wt-{agent}"],
        // Further than beside the main checkout.
        ["--worktree-pattern", "../../tmp-escape/{agent}"],
        ["--branch-pattern", "refs/{agent}"],
    ] {
        let mut all = vec!["init"];
        all.extend(args);
        all.push("--json");
        let (code, v) = json(&dir, &all);
        assert_eq!(code, 1, "{args:?}: {v}");
        assert_eq!(codes(&v), ["usage.invalid_arguments"], "{args:?}");
        // Typed text isn't echoed.
        let message = v["diagnostics"][0]["message"].as_str().unwrap_or("");
        assert!(
            !message.contains("abs") && !message.contains("a b"),
            "{message}"
        );
    }
    assert_eq!(tree(&dir), Vec::<String>::new());
}

#[test]
fn init_leaves_gitignore_bytes_unchanged_in_both_work_modes() {
    for work in ["local", "shared"] {
        for original in [
            None,
            Some("target\n.worktrees\n"),
            Some("target"),
            Some("/.worktrees/\n!/.worktrees/\n"),
        ] {
            let dir = temp(
                &format!("init-ignore-{work}-{}", uuid::Uuid::now_v7()),
                false,
            );
            if let Some(bytes) = original {
                std::fs::write(dir.join(".gitignore"), bytes).expect("existing ignore");
            }
            let (code, v) = json(&dir, &["init", "--work", work, "--json"]);
            assert_eq!(code, 0, "{v}");
            assert_eq!(
                std::fs::read_to_string(dir.join(".gitignore"))
                    .ok()
                    .as_deref(),
                original
            );
            assert!(
                v["data"]["changes"]
                    .as_array()
                    .expect("changes")
                    .iter()
                    .all(|c| c["path"].as_str().is_some_and(|p| p.starts_with(".mdkg/")))
            );
            assert!(dir.join(".mdkg/README.md").is_file());
            std::fs::remove_dir_all(dir).expect("clean fixture");
        }
    }
}

#[test]
fn init_cli_readme_guides_opt_in_without_touching_user_files() {
    let dir = temp("init-guide-cli", false);
    std::fs::write(dir.join(".gitignore"), "target\r\n!keep\r\n").expect("ignore");
    std::fs::write(dir.join("AGENTS.md"), "project instructions\n").expect("instructions");
    let before = snapshot(&dir);
    let (code, preview) = json(&dir, &["init", "--dry-run", "--json"]);
    assert_eq!(code, 0, "{preview}");
    assert_eq!(snapshot(&dir), before);
    assert!(
        preview["data"]["changes"]
            .as_array()
            .expect("changes")
            .iter()
            .any(|c| c["path"] == ".mdkg/README.md")
    );
    let out = run(&dir, &["init"]);
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8(out.stdout).expect("init output");
    assert!(text.contains(".mdkg/README.md") && text.contains("install --enable"));
    assert_eq!(
        std::fs::read_to_string(dir.join(".gitignore")).expect("ignore"),
        "target\r\n!keep\r\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("AGENTS.md")).expect("instructions"),
        "project instructions\n"
    );
    let guide = std::fs::read_to_string(dir.join(".mdkg/README.md")).expect("guide");
    assert!(guide.contains("mdkg skills sync"));
    assert!(guide.lines().any(|line| {
        line == "mdkg --root .worktrees/graph agents add worker --harness claude --role worker"
    }));
    assert!(
        guide
            .lines()
            .any(|line| line == "mdkg --root .worktrees/graph agents up worker")
    );
    assert!(
        !dir.join(".claude").exists()
            && !dir.join(".codex").exists()
            && !dir.join(".agents").exists()
    );
    std::fs::write(dir.join(".mdkg/README.md"), "edited guide\n").expect("edit guide");
    let before = snapshot(&dir);
    let (code, v) = json(&dir, &["init", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["data"]["already"], true);
    assert_eq!(snapshot(&dir), before);
    std::fs::remove_dir_all(dir).expect("clean fixture");
}

#[test]
fn init_outside_git_warns_that_coordination_needs_it() {
    let dir = temp("init-nogit", false);
    let (code, v) = json(&dir, &["init", "--graph-only", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["data"]["git"], false);
    assert_eq!(codes(&v), ["io.git_failed"]);
}

/// Commits everything on main, so `new` and `claim` have a main.
fn commit_all(dir: &Path) {
    for args in [
        &["checkout", "-q", "-B", "main"][..],
        &["add", "-A"],
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-q",
            "-m",
            "init",
        ],
    ] {
        let ok = Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .expect("git")
            .success();
        assert!(ok, "git {args:?}");
    }
}

#[test]
fn init_new_validate_shared_work_commits_the_task_in_the_graph() {
    let dir = temp("e2e-b", true);
    json(&dir, &["init", "--work", "shared", "--json"]);
    commit_all(&dir);
    let (code, v) = json(&dir, &["new", "task", "Parse the CSV header", "--json"]);
    assert_eq!(code, 0, "{v}");
    let id = v["data"]["node"]["id"].as_str().expect("id");
    let uuid = id.strip_prefix("task-").expect("task- id");
    assert!(is_uuid_v7(uuid), "{id}");
    let path = v["data"]["node"]["path"].as_str().expect("path");
    assert_eq!(path, ".mdkg/work/task-parse-the-csv-header.md", "{v}");
    assert!(dir.join(path).is_file());
    let (code, v) = json(&dir, &["validate", "--json"]);
    assert_eq!(code, 0, "{v}");
    // A new task's Requirements section holds only the template's fenced
    // example, so the one warning is the lint asking for items (A28).
    assert_eq!(
        codes(&v),
        ["req.missing"],
        "a fresh graph validates clean but for the empty requirements: {v}"
    );
}

#[test]
fn new_nodes_from_local_and_bundled_templates_have_no_live_requirements() {
    let kinds = ["task", "bug", "feat", "spike", "edd", "prd"];
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.mdkg/templates/default");
    for local in [false, true] {
        // The local case copies this repository's own templates; the
        // public export's graph is a fresh `mdkg init` without them.
        if local && !source.is_dir() {
            eprintln!(
                "skipped the local templates: this repository has no .mdkg/templates/default"
            );
            continue;
        }
        let dir = temp(if local { "req-local" } else { "req-bundled" }, true);
        assert_eq!(json(&dir, &["init", "--work", "shared", "--json"]).0, 0);
        if local {
            let templates = dir.join(".mdkg/templates/default");
            std::fs::create_dir_all(&templates).expect("template directory");
            for kind in kinds {
                std::fs::copy(
                    source.join(format!("{kind}.md")),
                    templates.join(format!("{kind}.md")),
                )
                .expect("local template");
            }
        }
        commit_all(&dir);
        for kind in kinds {
            let title = format!("Example {kind}");
            let (code, out) = json(&dir, &["new", kind, &title, "--json"]);
            assert_eq!(code, 0, "{kind}, local={local}: {out}");
            let path = out["data"]["node"]["path"].as_str().expect("node path");
            let body = std::fs::read_to_string(dir.join(path)).expect("created node");
            let parsed = mdkg_graph::requirements::parse(&body);
            assert!(parsed.section_found, "{kind}, local={local}");
            assert_eq!(parsed.items, 0, "{kind}, local={local}");
            assert!(parsed.requirements.is_empty(), "{kind}, local={local}");
        }
        std::fs::remove_dir_all(dir).expect("remove fixture");
    }
}

#[test]
fn local_work_survives_install_and_all_three_start_hook_adapters() {
    let dir = temp("local-hooks", true);
    assert_eq!(json(&dir, &["init", "--work", "local", "--json"]).0, 0);
    configure_codex_hook_actor(&dir);
    commit_all(&dir);
    enable(&dir, &["hooks:claude", "hooks:codex", "hooks:opencode"]);
    assert_eq!(config(&dir)["work"]["location"], "local");
    let (code, node) = json(&dir, &["new", "task", "Local hook work", "--json"]);
    assert_eq!(code, 0, "{node}");
    let id = node["data"]["node"]["id"].as_str().unwrap();
    let private = c_node_file(&dir, id);
    assert!(private.is_file());
    assert!(!dir.join(".mdkg/work").exists());
    let (code, sent) = json(
        &dir,
        &[
            "--as",
            "sender",
            "msg",
            "send",
            "--to",
            "tester",
            "--kind",
            "note",
            "--text",
            "PRIVATE_LOCAL_BODY",
            "--json",
        ],
    );
    assert_eq!(code, 0, "{sent}");
    let header = format!("#{} note from sender", sent["data"]["message"]["seq"]);
    for (i, harness) in ["claude", "codex", "opencode"].into_iter().enumerate() {
        let sid = format!("0192f3a0-0000-7000-8000-{i:012}");
        let input = serde_json::json!({"cwd":dir,"session_id":sid,"sessionID":sid,
                    "hook_event_name":if harness == "opencode" {"prompt"} else {"SessionStart"},"source":"startup"});
        let output = start_hook(&dir, harness, &input);
        assert!(
            output.status.success(),
            "{harness}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(text.contains(&header), "{harness}: {text}");
        assert!(!text.contains("PRIVATE_LOCAL_BODY"), "{harness}: {text}");
    }
    assert!(private.is_file());
    assert!(!dir.join(".mdkg/work").exists());
}

/// An explicit actor flag does not establish a Codex receiver's worktree
/// identity. Configure the fixture location, as a real agent definition does.
fn configure_codex_hook_actor(dir: &Path) {
    let mut cfg = config(dir);
    cfg["agents"] = serde_json::json!({"tester":{"cwd":dir}});
    std::fs::write(
        dir.join(".mdkg/config.json"),
        serde_json::to_string_pretty(&cfg).expect("config JSON") + "\n",
    )
    .expect("fixture actor location");
}

fn start_hook(dir: &Path, harness: &str, input: &Value) -> Output {
    use std::io::Write;
    use std::process::Stdio;
    let mut command = Command::new(BIN);
    let mut child = common::without_launcher_env(&mut command)
        .current_dir(dir)
        .args(["--as", "tester", "hook", "start", "--harness", harness])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start hook");
    child
        .stdin
        .take()
        .expect("hook stdin")
        .write_all(input.to_string().as_bytes())
        .expect("hook input");
    child.wait_with_output().expect("hook output")
}

/// Mail hooks work without an agent mapping; only receiver registration
/// requires one. Identity reads only committed config, so the graph and its
/// mapped definition are committed.
#[test]
fn codex_hook_fixture_registers_only_with_actor_identity_in_local_and_shared_work() {
    for work in ["local", "shared"] {
        let dir = temp(&format!("hook-identity-{work}"), true);
        assert_eq!(
            json(&dir, &["init", "--work", work, "--no-gitignore", "--json"]).0,
            0
        );
        declare(&dir, &["hooks:codex"]);
        commit_all(&dir);
        let input = serde_json::json!({"cwd":dir,
            "session_id":"0192f3a0-0000-7000-8000-000000000001",
            "hook_event_name":"SessionStart","source":"startup"});
        let common = mdkg_coord_sqlite::git::common_dir(&dir).expect("common");
        let before = start_hook(&dir, "codex", &input);
        assert_eq!(before.status.code(), Some(0), "{work}: {before:?}");
        assert!(before.stdout.is_empty(), "{work}: {before:?}");
        assert!(before.stderr.is_empty(), "{work}: {before:?}");
        assert!(
            !mdkg_run::hooks::dir(&common).join("receiver").exists(),
            "a plain mail hook must not register a receiver"
        );

        let (code, sent) = json(
            &dir,
            &[
                "--as",
                "sender",
                "msg",
                "send",
                "--to",
                "tester",
                "--kind",
                "handoff",
                "--text",
                "PRIVATE_IDENTITY_FIXTURE_BODY",
                "--json",
            ],
        );
        assert_eq!(code, 0, "{work}: {sent}");
        let header = format!("#{} handoff from sender", sent["data"]["message"]["seq"]);
        let unmapped = start_hook(&dir, "codex", &input);
        assert_eq!(unmapped.status.code(), Some(0), "{work}: {unmapped:?}");
        assert!(unmapped.stderr.is_empty(), "{work}: {unmapped:?}");
        let context = String::from_utf8(unmapped.stdout).expect("hook context");
        assert!(context.contains(&header), "{work}: {context}");
        assert!(!context.contains("PRIVATE_IDENTITY_FIXTURE_BODY"));
        assert!(
            !mdkg_run::hooks::dir(&common).join("receiver").exists(),
            "mail and --as do not establish receiver identity"
        );

        configure_codex_hook_actor(&dir);
        commit_all(&dir);
        let after = start_hook(&dir, "codex", &input);
        assert_eq!(after.status.code(), Some(0), "{work}: {after:?}");
        assert!(after.stderr.is_empty(), "{work}: {after:?}");
        let context = String::from_utf8(after.stdout).expect("hook context");
        assert!(context.contains(&header), "{work}: {context}");
        assert!(!context.contains("PRIVATE_IDENTITY_FIXTURE_BODY"));
        let route = mdkg_run::receiver::route::read(&common, "tester")
            .expect("receiver route")
            .expect("configured actor registers its receiver");
        assert_eq!(route.session.as_deref(), input["session_id"].as_str());
        assert_eq!(route.cwd.as_deref(), Some(dir.as_path()));
        assert!(!route.suspended);
        std::fs::remove_dir_all(&dir).expect("remove fixture");
    }
}

#[test]
fn init_new_validate_local_work_keeps_the_task_out_of_the_checkout() {
    let dir = temp("e2e-c", true);
    json(&dir, &["init", "--work", "local", "--json"]);
    commit_all(&dir);
    let (code, v) = json(&dir, &["new", "task", "Parse the CSV header", "--json"]);
    assert_eq!(code, 0, "{v}");
    let id = v["data"]["node"]["id"].as_str().expect("id");
    assert!(
        is_uuid_v7(id.strip_prefix("task-").expect("task- id")),
        "{id}"
    );
    // The task's file is beside the store in the git common dir, not in
    // the checkout, and git sees no change.
    let file = c_node_file(&dir, id);
    let common = std::fs::canonicalize(dir.join(".git")).expect("git dir");
    let real = std::fs::canonicalize(&file).expect("the task's file");
    assert!(real.starts_with(&common), "{}", real.display());
    assert!(!dir.join(".mdkg/work").exists(), "{v}");
    // As text, the path outside the checkout says it's local work (e2e UX).
    let out = run(&dir, &["new", "task", "Another one"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains(" (local work at "), "{text}");
    assert_git_clean(&dir);
    let (code, v) = json(&dir, &["validate", "--json"]);
    assert_eq!(code, 0, "{v}");
    // Claimed and closed in place: the file says done, and git still sees
    // nothing.
    let (code, v) = json(&dir, &["claim", id, "--json"]);
    assert_eq!(code, 0, "{v}");
    let (code, v) = json(&dir, &["done", id, "--json"]);
    assert_eq!(code, 0, "{v}");
    let (code, v) = json(&dir, &["show", id, "--json"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["data"]["node"]["status"], "done", "{v}");
    let text = std::fs::read_to_string(&file).expect("the task's file");
    assert!(text.lines().any(|l| l == "status: done"), "{text}");
    assert_git_clean(&dir);
}

/// A git folder inside the checkout (`git init --separate-git-dir`) gives
/// local work a relative path; `new` still labels it (Terra #17973).
#[test]
fn new_labels_local_work_under_a_separate_git_dir_in_the_checkout() {
    let dir = temp("separate-git-dir", false);
    let ok = Command::new("git")
        .args(["init", "-q", "--separate-git-dir", "repository-data"])
        .current_dir(&dir)
        .status()
        .expect("git init")
        .success();
    assert!(ok);
    json(&dir, &["init", "--work", "local", "--json"]);
    std::fs::write(dir.join(".gitignore"), "/repository-data\n").expect("ignore");
    commit_all(&dir);
    let (code, v) = json(&dir, &["new", "task", "Local here", "--json"]);
    assert_eq!(code, 0, "{v}");
    let path = v["data"]["node"]["path"].as_str().expect("path");
    assert!(path.starts_with("repository-data/"), "{v}");
    let out = run(&dir, &["new", "task", "Another one"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains(" (local work at repository-data/"), "{text}");
}

/// Where a node's file is: `show` gives its path from the graph root
/// (`.git/mdkg/work/…` for a profile C node).
fn c_node_file(dir: &Path, id: &str) -> PathBuf {
    let (code, v) = json(dir, &["show", id, "--fields", "path", "--json"]);
    assert_eq!(code, 0, "{v}");
    let path = dir.join(v["data"]["node"]["path"].as_str().expect("path"));
    assert!(path.is_file(), "{}", path.display());
    path
}

fn assert_git_clean(dir: &Path) {
    let out = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(dir)
        .output()
        .expect("git");
    assert!(out.status.success(), "{out:?}");
    let changes = String::from_utf8_lossy(&out.stdout);
    assert!(changes.trim().is_empty(), "git sees changes:\n{changes}");
}

fn status_of<'a>(v: &'a Value, name: &str) -> &'a str {
    v["data"]["checks"]
        .as_array()
        .and_then(|a| a.iter().find(|c| c["name"] == name))
        .and_then(|c| c["status"].as_str())
        .unwrap_or("absent")
}

#[test]
fn install_enable_and_disable_change_the_list_and_only_their_files() {
    let dir = temp("install-enable", true);
    json(&dir, &["init", "--json"]);
    let before = snapshot(&dir);
    // A dry run writes nothing, the config included.
    let (code, v) = json(
        &dir,
        &["install", "--enable", "mcp:claude", "--dry-run", "--json"],
    );
    assert_eq!(code, 0, "{v}");
    assert_eq!(snapshot(&dir), before);
    let (code, v) = json(&dir, &["install", "--enable", "mcp:claude", "--json"]);
    assert_eq!(code, 0, "{v}");
    common::conforms("install", &v);
    assert_eq!(
        config(&dir)["projections"],
        serde_json::json!(["mcp:claude"])
    );
    let added: Vec<String> = tree(&dir)
        .into_iter()
        .filter(|f| !before.iter().any(|(b, _)| b == f))
        .collect();
    assert_eq!(added, [".mcp.json"]);
    // A plain sync keeps to the list.
    let (code, v) = json(&dir, &["install", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert!(!dir.join("AGENTS.md").exists() && !dir.join(".claude").exists());
    // Disabled: off the list, and the file only mdkg wrote is gone.
    let (code, v) = json(&dir, &["install", "--disable", "mcp:claude", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(config(&dir)["projections"], serde_json::json!([]));
    assert!(!dir.join(".mcp.json").exists());
    // Names are checked before anything is read; undo takes no list.
    let (code, v) = json(&dir, &["install", "--enable", "mcp:emacs", "--json"]);
    assert_eq!(code, 1, "{v}");
    assert_eq!(codes(&v), ["usage.invalid_arguments"]);
    let out = run(&dir, &["install", "--undo", "--enable", "agents-md"]);
    assert!(!out.status.success());
}

#[test]
fn fresh_init_installs_all_three_hook_defaults_and_projections() {
    let dir = temp("install-all-hook-defaults", true);
    let (code, v) = json(&dir, &["init", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(config(&dir)["projections"], serde_json::json!([]));
    assert!(!dir.join(".mdkg/hooks").exists());
    let hooks = ["hooks:claude", "hooks:codex", "hooks:opencode"];
    enable(&dir, &hooks);
    assert_eq!(config(&dir)["projections"], serde_json::json!(hooks));
    for (canonical, target, expected) in [
        (
            ".mdkg/hooks/claude/settings.json",
            ".claude/settings.local.json",
            mdkg_run::hooks::claude_canonical(BIN),
        ),
        (
            ".mdkg/hooks/codex/hooks.json",
            ".codex/hooks.json",
            mdkg_run::codex_hooks::codex_fragment(BIN),
        ),
        (
            ".mdkg/hooks/opencode/mdkg/server.ts",
            ".opencode/plugins/mdkg/server.ts",
            mdkg_run::hooks::opencode_plugin(BIN),
        ),
    ] {
        let definition = std::fs::read_to_string(dir.join(canonical)).expect("canonical");
        let projected = std::fs::read_to_string(dir.join(target)).expect("projection");
        assert!(!definition.is_empty(), "{canonical}");
        assert_eq!(definition, expected, "{canonical}");
        if canonical.ends_with(".json") {
            assert_eq!(
                serde_json::from_str::<Value>(&projected).expect("target JSON"),
                serde_json::from_str::<Value>(&definition).expect("canonical JSON"),
                "{target}"
            );
        } else {
            assert_eq!(projected, definition, "{target}");
        }
    }
    let (_, v) = json(&dir, &["doctor", "--json"]);
    assert_eq!(status_of(&v, "hooks"), "ok", "{v}");
    // Opting into hooks neither enables other projections nor writes trust.
    for path in [
        "AGENTS.md",
        ".mcp.json",
        ".codex/config.toml",
        "opencode.json",
    ] {
        assert!(!dir.join(path).exists(), "unexpected projection: {path}");
    }
    std::fs::remove_dir_all(&dir).expect("cleanup");
}

#[test]
fn install_ships_hooks_and_preserves_the_users_files() {
    for undo in [false, true] {
        let dir = temp(&format!("install-shipped-hooks-{undo}"), true);
        let (code, v) = json(&dir, &["init", "--json"]);
        assert_eq!(code, 0, "{v}");
        let user = serde_json::json!({
            "model": "user-model",
            "hooks": {
                "Stop": [{"hooks": [{"type": "command", "command": "./notify.sh"}]}],
                "Notification": [{"hooks": [{"type": "command", "command": "./alert.sh"}]}]
            }
        });
        let settings = dir.join(".claude/settings.local.json");
        std::fs::create_dir_all(settings.parent().expect("parent")).expect("mkdir");
        std::fs::write(&settings, serde_json::to_string(&user).expect("json")).expect("write");
        let before = snapshot(&dir);
        let (code, v) = json(
            &dir,
            &[
                "install",
                "--enable",
                "hooks:claude,hooks:opencode",
                "--dry-run",
                "--json",
            ],
        );
        assert_eq!(code, 0, "{v}");
        assert_eq!(snapshot(&dir), before, "dry run writes no canonical files");

        enable(&dir, &["hooks:claude"]);
        let canonical = dir.join(".mdkg/hooks/claude/settings.json");
        let shipped = mdkg_run::hooks::claude_canonical(BIN);
        assert_eq!(
            std::fs::read_to_string(&canonical).expect("canonical"),
            shipped
        );
        let merged: Value =
            serde_json::from_slice(&std::fs::read(&settings).expect("read")).expect("json");
        assert_eq!(merged["model"], user["model"]);
        assert_eq!(
            merged["hooks"]["Notification"],
            user["hooks"]["Notification"]
        );
        assert_eq!(merged["hooks"]["Stop"][0], user["hooks"]["Stop"][0]);
        let fragment: Value = serde_json::from_str(&shipped).expect("fragment");
        for (event, entries) in fragment["hooks"].as_object().expect("hooks") {
            let mut expected = user["hooks"][event].as_array().cloned().unwrap_or_default();
            expected.extend(entries.as_array().expect("entries").iter().cloned());
            assert_eq!(
                merged["hooks"][event],
                serde_json::json!(expected),
                "{event}"
            );
        }
        assert!(!dir.join(".opencode").exists());
        assert!(!dir.join(".codex").exists());
        assert!(!dir.join(".mcp.json").exists());

        enable(&dir, &["hooks:opencode"]);
        let plugin_canonical = dir.join(".mdkg/hooks/opencode/mdkg/server.ts");
        let plugin = dir.join(".opencode/plugins/mdkg/server.ts");
        let shipped_plugin = mdkg_run::hooks::opencode_plugin(BIN);
        assert_eq!(
            std::fs::read_to_string(&plugin_canonical).expect("canonical"),
            shipped_plugin
        );
        assert_eq!(
            std::fs::read_to_string(&plugin).expect("plugin"),
            shipped_plugin
        );
        assert!(!dir.join(".opencode/plugins/mdkg.ts").exists());
        let synced = snapshot(&dir);
        let (code, v) = json(&dir, &["install", "--json"]);
        assert_eq!(code, 0, "{v}");
        assert_eq!(
            snapshot(&dir),
            synced,
            "sync does not duplicate hook entries"
        );
        let (_, v) = json(&dir, &["doctor", "--json"]);
        assert_eq!(status_of(&v, "hooks"), "ok", "{v}");

        let args: &[&str] = if undo {
            &["install", "--undo", "--json"]
        } else {
            &[
                "install",
                "--disable",
                "hooks:claude,hooks:opencode",
                "--json",
            ]
        };
        let (code, v) = json(&dir, args);
        assert_eq!(code, 0, "{v}");
        let left: Value =
            serde_json::from_slice(&std::fs::read(&settings).expect("read")).expect("json");
        assert_eq!(left, user);
        assert_eq!(config(&dir)["projections"], serde_json::json!([]));
        assert!(!plugin.exists());
        if undo {
            assert_eq!(snapshot(&dir), before, "undo restores the original bytes");
        } else {
            assert_eq!(
                std::fs::read_to_string(&canonical).expect("canonical"),
                shipped
            );
            assert_eq!(
                std::fs::read_to_string(&plugin_canonical).expect("canonical"),
                shipped_plugin
            );
        }
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }
}

#[test]
fn doctor_passes_a_fresh_graph() {
    let dir = temp("doctor-ok", true);
    json(&dir, &["init", "--json"]);
    let (code, v) = json(&dir, &["doctor", "--json"]);
    common::conforms("doctor", &v);
    assert_eq!(code, 0, "{v}");
    for name in ["binary", "git", "config", "graph", "mirrors", "store"] {
        assert_eq!(status_of(&v, name), "ok", "{name}: {v}");
    }
    // Nothing outside .mdkg is enabled yet: the skill alone is checked.
    assert_eq!(status_of(&v, "mcp"), "skip", "{v}");
    assert_eq!(status_of(&v, "agents"), "ok", "{v}");
    enable(&dir, ALL_PROJECTIONS);
    let (code, v) = json(&dir, &["doctor", "--json"]);
    assert_eq!(code, 0, "{v}");
    for name in ["mirrors", "mcp", "agents"] {
        assert_eq!(status_of(&v, name), "ok", "{name}: {v}");
    }
    // Enabled but never synced is a warning, not a skip.
    std::fs::remove_file(dir.join(".mcp.json")).expect("rm");
    std::fs::remove_file(dir.join("AGENTS.md")).expect("rm");
    let (_, v) = json(&dir, &["doctor", "--json"]);
    assert_eq!(status_of(&v, "mcp"), "warning", "{v}");
    assert_eq!(status_of(&v, "agents"), "warning", "{v}");
    assert!(v.to_string().contains("mcp:claude is enabled"), "{v}");
    json(&dir, &["install", "--json"]);
    // From a subfolder too, as git finds its repository.
    std::fs::create_dir_all(dir.join("src/deep")).expect("mkdir");
    let (_, v) = json(&dir.join("src/deep"), &["doctor", "--json"]);
    assert_eq!(status_of(&v, "config"), "ok", "{v}");
}

fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git");
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .current_dir(dir)
        .status()
        .expect("git")
        .success();
    assert!(ok, "git {args:?}");
}

fn check_of<'a>(v: &'a Value, name: &str) -> &'a Value {
    v["data"]["checks"]
        .as_array()
        .and_then(|a| a.iter().find(|c| c["name"] == name))
        .unwrap_or(&Value::Null)
}

/// A graph with a nested workspace, optionally carrying private code
/// beside that workspace's dedicated graph folder (#14632).
fn nested_graph_shape(private_source: bool) -> PathBuf {
    let dir = temp("nested-graph-shape", false);
    git(&dir, &["init", "-q", "-b", "trunk"]);
    let (code, v) = json(&dir, &["init", "--graph-only", "--json"]);
    assert_eq!(code, 0, "{v}");
    let mut cfg = config(&dir);
    let mut nested = cfg.clone();
    nested["graph_id"] = serde_json::json!(uuid::Uuid::now_v7().to_string());
    std::fs::create_dir_all(dir.join("services/api/.mdkg")).expect("nested graph");
    std::fs::write(
        dir.join("services/api/.mdkg/config.json"),
        nested.to_string(),
    )
    .expect("nested config");
    cfg["workspaces"] = serde_json::json!({
        "root": {"path": ".", "enabled": true, "mdkg_dir": ".mdkg", "visibility": "private"},
        "api": {"path": "services/api", "enabled": true, "mdkg_dir": ".mdkg", "visibility": "private"}
    });
    std::fs::write(dir.join(".mdkg/config.json"), cfg.to_string()).expect("config");
    if private_source {
        std::fs::write(dir.join("services/api/private-source.rs"), "private code\n")
            .expect("source");
    }
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-q", "-m", "nested graph"]);
    git(&dir, &["branch", "mdkg-graph"]);
    git(&dir, &["config", "mdkg.graph.ref", "refs/heads/mdkg-graph"]);
    let identity = format!(
        "mdkg.graph.{}.ref",
        cfg["graph_id"].as_str().expect("graph id")
    );
    git(&dir, &["config", &identity, "refs/heads/mdkg-graph"]);
    // A fixture-only remote: install neither fetches nor pushes.
    let remote = dir.join("private.git");
    git(
        &dir,
        &["remote", "add", "mine", remote.to_str().expect("path")],
    );
    let (code, v) = json(&dir, &["install", "--setup", "local", "--json"]);
    assert_eq!(code, 0, "{v}");
    dir
}

#[test]
fn professional_setup_refuses_source_beside_a_nested_graph() {
    let dir = nested_graph_shape(true);
    let config_before = std::fs::read(dir.join(".git/config")).expect("git config");
    let (code, v) = json(
        &dir,
        &[
            "install",
            "--setup",
            "professional",
            "--graph-remote",
            "mine",
            "--json",
        ],
    );
    assert_ne!(code, 0, "{v}");
    assert_eq!(codes(&v), ["graph.shape"], "{v}");
    assert_eq!(
        std::fs::read(dir.join(".git/config")).expect("git config"),
        config_before
    );
    // Doctor applies the same proof to an already-configured remote.
    git(&dir, &["config", "mdkg.graph.remote", "mine"]);
    let (_, v) = json(&dir, &["doctor", "--json"]);
    assert_eq!(status_of(&v, "graph.shape"), "error", "{v}");
}

#[test]
fn professional_setup_and_doctor_accept_a_pure_nested_graph() {
    let dir = nested_graph_shape(false);
    let (code, v) = json(
        &dir,
        &[
            "install",
            "--setup",
            "professional",
            "--graph-remote",
            "mine",
            "--json",
        ],
    );
    assert_eq!(code, 0, "{v}");
    let (_, v) = json(&dir, &["doctor", "--json"]);
    assert_eq!(status_of(&v, "graph.shape"), "ok", "{v}");
    assert_eq!(check_of(&v, "graph.shape")["summary"], "graph-only", "{v}");
}

fn shape_git_input(dir: &Path, args: &[&str], input: &[u8]) -> String {
    let out = mdkg_git::command(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
        .args(args)
        .run(Some(input))
        .expect("git");
    assert!(out.status.success(), "{out:?}");
    String::from_utf8(out.stdout)
        .expect("git output")
        .trim()
        .to_owned()
}

#[test]
fn personal_doctor_checks_invalid_paths_after_an_outside_entry() {
    for nested in [false, true] {
        let dir = nested_graph_shape(false);
        let graph = "refs/heads/mdkg-graph";
        let blob = shape_git_input(&dir, &["rev-parse", "HEAD:.mdkg/README.md"], b"");
        let mut entries = shape_git_input(&dir, &["ls-tree", "-z", graph], b"").into_bytes();
        if nested {
            let mut children = format!("100644 blob {blob}\tz").into_bytes();
            children.extend_from_slice(b"\xff\0");
            let tree = shape_git_input(&dir, &["mktree", "-z"], &children);
            entries.extend_from_slice(format!("040000 tree {tree}\ta\0").as_bytes());
        } else {
            entries.extend_from_slice(format!("100644 blob {blob}\ta\0").as_bytes());
            entries.extend_from_slice(format!("100644 blob {blob}\tz").as_bytes());
            entries.extend_from_slice(b"\xff\0");
        }
        let tree = shape_git_input(&dir, &["mktree", "-z"], &entries);
        let commit = shape_git_input(
            &dir,
            &["commit-tree", &tree, "-p", graph, "-m", "mixed full shape"],
            b"",
        );
        git(&dir, &["update-ref", graph, &commit]);
        let remote = dir.join("origin.git");
        git(
            &dir,
            &["remote", "add", "origin", remote.to_str().expect("path")],
        );
        git(&dir, &["config", "mdkg.graph.remote", "origin"]);
        let (_, v) = json(&dir, &["doctor", "--json"]);
        assert_eq!(status_of(&v, "graph.shape"), "error", "{v}");
    }
    // Readable full shapes are still kept for a personal graph (G20).
    let dir = nested_graph_shape(true);
    let remote = dir.join("origin.git");
    git(
        &dir,
        &["remote", "add", "origin", remote.to_str().expect("path")],
    );
    git(&dir, &["config", "mdkg.graph.remote", "origin"]);
    let (_, v) = json(&dir, &["doctor", "--json"]);
    assert_eq!(status_of(&v, "graph.shape"), "ok", "{v}");
    assert_eq!(
        check_of(&v, "graph.shape")["summary"],
        "full shape (kept for a personal graph)",
        "{v}"
    );
}

/// A personal full shape whose remaining tree passes the walk's entry
/// limit warns (and fails `--strict`), with the code's remote; another
/// remote is still an error (Terra #14932, #18260). It lowers the limit
/// with MDKG_SHAPE_ENTRY_LIMIT, which only a debug build reads, so a
/// release-profile run skips it (Terra #20014); the shipped limit is
/// pinned by the bin's the_shipped_shape_entry_limit_is_100_000.
#[test]
#[cfg_attr(
    not(debug_assertions),
    ignore = "needs a debug build's MDKG_SHAPE_ENTRY_LIMIT"
)]
fn personal_doctor_warns_when_a_full_shape_passes_the_walk_budget() {
    let dir = nested_graph_shape(false);
    let graph = "refs/heads/mdkg-graph";
    let remote = dir.join("origin.git");
    git(
        &dir,
        &["remote", "add", "origin", remote.to_str().expect("path")],
    );
    git(&dir, &["config", "mdkg.graph.remote", "origin"]);
    // Doctor runs with this build first on PATH, so its `path` check is
    // ok, and with the remote approved and synced --strict passes before
    // the big tree (Terra #18403). After it, the shape warns, and so does
    // graph.remote for the commit not pushed yet.
    let bin_dir = Path::new(BIN).parent().expect("the binary's folder");
    let path = std::env::join_paths(std::iter::once(bin_dir.to_path_buf()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .expect("PATH");
    let doctor = |args: &[&str]| {
        let out = common::without_launcher_env(&mut Command::new(BIN))
            .args(args)
            .current_dir(&dir)
            .env("MDKG_ACTOR", "tester")
            .env("PATH", &path)
            // A debug build's lowered walk limit, so the big tree trips it
            // in seconds (F140); the shipped 100,000 is a unit test.
            .env("MDKG_SHAPE_ENTRY_LIMIT", "1000")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .output()
            .expect("run the binary");
        let v: Value = serde_json::from_slice(&out.stdout).expect("doctor's JSON");
        (out.status.code().unwrap_or(-1), v)
    };
    // A person approves the graph remote and syncs first (0.7.2 graph
    // sync), so its own row is no warning.
    git(
        &dir,
        &["init", "-q", "--bare", remote.to_str().expect("path")],
    );
    let (code, approved) = doctor(&["graph", "remote", "approve", "--json"]);
    assert_eq!(code, 0, "{approved}");
    let (code, synced) = doctor(&["graph", "sync", "--json"]);
    assert_eq!(code, 0, "{synced}");
    let (code, before) = doctor(&["doctor", "--strict", "--json"]);
    assert_eq!(status_of(&before, "graph.shape"), "ok", "{before}");
    assert_eq!(before["data"]["warnings"].as_u64(), Some(0), "{before}");
    assert_eq!(code, 0, "{before}");
    let blob = shape_git_input(&dir, &["rev-parse", "HEAD:.mdkg/README.md"], b"");
    // 12 small trees that a walk expands to 1,024 leaves and over 2,000
    // entries, past the lowered 1,000-entry limit.
    let mut big = shape_git_input(
        &dir,
        &["mktree", "-z"],
        format!("100644 blob {blob}\tleaf\0").as_bytes(),
    );
    for _ in 0..10 {
        big = shape_git_input(
            &dir,
            &["mktree", "-z"],
            format!("040000 tree {big}\ta\0040000 tree {big}\tb\0").as_bytes(),
        );
    }
    let mut entries = shape_git_input(&dir, &["ls-tree", "-z", graph], b"").into_bytes();
    // `README` (outside: the shape is full) sorts before `src`.
    entries.extend_from_slice(format!("100644 blob {blob}\tREADME\0").as_bytes());
    entries.extend_from_slice(format!("040000 tree {big}\tsrc\0").as_bytes());
    let tree = shape_git_input(&dir, &["mktree", "-z"], &entries);
    let commit = shape_git_input(
        &dir,
        &["commit-tree", &tree, "-p", graph, "-m", "a big full shape"],
        b"",
    );
    git(&dir, &["update-ref", graph, &commit]);
    // --strict fails on the shape's warning alone (Terra #19756), with the
    // personal remote still configured (Terra #19803): sync.json records
    // the big commit as synced, so graph.remote has nothing unpushed to
    // warn about. One doctor run on the big tree: each takes a minute.
    let state = dir.join(".git/mdkg/graph/sync.json");
    let mut synced: Value =
        serde_json::from_slice(&std::fs::read(&state).expect("sync.json")).expect("json");
    synced["accepted"] = commit.clone().into();
    std::fs::write(&state, synced.to_string()).expect("sync.json");
    let (code, v) = doctor(&["doctor", "--strict", "--json"]);
    assert_eq!(code, 2, "{v}");
    assert_eq!(status_of(&v, "graph.shape"), "warning", "{v}");
    assert_eq!(
        status_of(&v, "graph.remote"),
        "ok",
        "origin stays configured: {v}"
    );
    assert_eq!(v["data"]["warnings"].as_u64(), Some(1), "{v}");
    let summary = check_of(&v, "graph.shape")["summary"]
        .as_str()
        .expect("summary")
        .to_owned();
    assert!(summary.contains("wasn't checked"), "{v}");
    assert!(summary.contains("limit"), "{v}");
    let strict = v["diagnostics"]
        .as_array()
        .expect("diagnostics")
        .iter()
        .find(|d| d["code"] == "doctor.check_failed")
        .expect("the strict failure");
    assert_eq!(strict["message"], "--strict: 1 check warned", "{v}");
    // Another remote than the code's is refused whatever the walk did.
    let other = dir.join("other.git");
    git(
        &dir,
        &["remote", "add", "other", other.to_str().expect("path")],
    );
    git(&dir, &["config", "mdkg.graph.remote", "other"]);
    let (_, v) = json(&dir, &["doctor", "--json"]);
    assert_eq!(status_of(&v, "graph.shape"), "error", "{v}");
}

#[test]
fn outside_empty_trees_refuse_setup_and_fail_doctor() {
    for name in [b"outside".as_slice(), b"outside-\xff".as_slice()] {
        let dir = nested_graph_shape(false);
        let graph = "refs/heads/mdkg-graph";
        let empty = shape_git_input(&dir, &["mktree", "-z"], b"");
        let mut entries = shape_git_input(&dir, &["ls-tree", "-z", graph], b"").into_bytes();
        entries.extend_from_slice(format!("040000 tree {empty}\t").as_bytes());
        entries.extend_from_slice(name);
        entries.push(0);
        let tree = shape_git_input(&dir, &["mktree", "-z"], &entries);
        let commit = shape_git_input(
            &dir,
            &[
                "commit-tree",
                &tree,
                "-p",
                graph,
                "-m",
                "outside empty tree",
            ],
            b"",
        );
        git(&dir, &["update-ref", graph, &commit]);
        let (code, v) = json(
            &dir,
            &[
                "install",
                "--setup",
                "professional",
                "--graph-remote",
                "mine",
                "--json",
            ],
        );
        assert_ne!(code, 0, "{v}");
        assert_eq!(codes(&v), ["graph.shape"], "{v}");
        git(&dir, &["config", "mdkg.graph.remote", "mine"]);
        let (_, v) = json(&dir, &["doctor", "--json"]);
        assert_eq!(status_of(&v, "graph.shape"), "error", "{v}");
    }
}

#[cfg(unix)]
#[test]
fn unclassifiable_nested_paths_refuse_setup_and_fail_doctor() {
    use std::os::unix::ffi::OsStringExt as _;

    let dir = nested_graph_shape(false);
    // macOS can't create this filename on disk. Put it straight into the
    // fixture index, reusing a blob, so the committed tree still tests it.
    let out = Command::new("git")
        .args(["rev-parse", "HEAD:.mdkg/README.md"])
        .current_dir(&dir)
        .output()
        .expect("blob");
    assert!(out.status.success(), "{out:?}");
    let blob = String::from_utf8(out.stdout).expect("object id");
    let name = std::ffi::OsString::from_vec(b"services/api/private-\xff.rs".to_vec());
    let status = Command::new("git")
        .args([
            "update-index",
            "--add",
            "--cacheinfo",
            "100644",
            blob.trim(),
        ])
        .arg(name)
        .current_dir(&dir)
        .status()
        .expect("index");
    assert!(status.success());
    git(&dir, &["commit", "-q", "-m", "unclassifiable source path"]);
    git(&dir, &["update-ref", "refs/heads/mdkg-graph", "HEAD"]);
    let (code, v) = json(
        &dir,
        &[
            "install",
            "--setup",
            "professional",
            "--graph-remote",
            "mine",
            "--json",
        ],
    );
    assert_ne!(code, 0, "{v}");
    assert_eq!(codes(&v), ["graph.shape"], "{v}");
    git(&dir, &["config", "mdkg.graph.remote", "mine"]);
    let (_, v) = json(&dir, &["doctor", "--json"]);
    assert_eq!(status_of(&v, "graph.shape"), "error", "{v}");
}

#[test]
fn an_unborn_main_is_named_with_the_commit_that_makes_it() {
    let dir = temp("unborn", false);
    git(&dir, &["init", "-q", "-b", "main"]);
    let (code, v) = json(&dir, &["init", "--graph-only", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(codes(&v), ["io.git_failed"], "{v}");
    let first = v["data"]["next"][0].as_str().unwrap_or("");
    assert!(first.starts_with("git add -A && git commit"), "{v}");
    let (_, v) = json(&dir, &["doctor", "--json"]);
    assert_eq!(status_of(&v, "main"), "warning", "{v}");
    assert!(
        check_of(&v, "main")["hint"]
            .as_str()
            .unwrap_or("")
            .contains("git commit"),
        "{v}"
    );
    // The first commit makes it, and `new` works.
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-q", "-m", "Start the graph"]);
    let (_, v) = json(&dir, &["doctor", "--json"]);
    assert_eq!(status_of(&v, "main"), "ok", "{v}");
    let (code, v) = json(
        &dir,
        &["new", "task", "First task", "--status", "todo", "--json"],
    );
    assert_eq!(code, 0, "{v}");
}

#[test]
fn a_repository_on_master_is_told_to_name_its_main_branch() {
    let dir = temp("on-master", false);
    git(&dir, &["init", "-q", "-b", "master"]);
    std::fs::write(dir.join("README.md"), "x\n").expect("write");
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-q", "-m", "first"]);
    let (_, v) = json(&dir, &["init", "--graph-only", "--json"]);
    assert_eq!(
        v["data"]["next"][0], "git config mdkg.mainRef master",
        "{v}"
    );
    let (_, v) = json(&dir, &["doctor", "--json"]);
    assert_eq!(status_of(&v, "main"), "warning", "{v}");
    git(&dir, &["config", "mdkg.mainRef", "master"]);
    let (_, v) = json(&dir, &["doctor", "--json"]);
    assert_eq!(status_of(&v, "main"), "ok", "{v}");
}

/// `json`, with extra environment variables.
fn json_env(dir: &Path, env: &[(&str, &str)], args: &[&str]) -> (i32, Value) {
    let mut cmd = Command::new(BIN);
    common::without_launcher_env(&mut cmd)
        .args(args)
        .current_dir(dir)
        .env("MDKG_ACTOR", "tester")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("run the binary");
    let v: Value = serde_json::from_slice(&out.stdout).expect("JSON");
    (out.status.code().unwrap_or(-1), v)
}

/// Runs init's suggested repair in a shell, with `env`, then `mdkg new`.
fn repair_then_new(dir: &Path, env: &[(&str, &str)]) {
    let (_, v) = json_env(dir, env, &["init", "--graph-only", "--json"]);
    let steps: Vec<String> = v["data"]["next"]
        .as_array()
        .expect("next")
        .iter()
        .filter_map(|s| s.as_str())
        .take_while(|s| !s.starts_with("mdkg "))
        .map(str::to_string)
        .collect();
    assert!(!steps.is_empty(), "{v}");
    let mut sh = Command::new("sh");
    sh.arg("-c")
        .arg(steps.join(" && "))
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com");
    for (k, val) in env {
        sh.env(k, val);
    }
    assert!(sh.status().expect("sh").success(), "{steps:?}");
    let (code, v) = json_env(
        dir,
        env,
        &[
            "new",
            "task",
            "After the repair",
            "--status",
            "todo",
            "--json",
        ],
    );
    assert_eq!(code, 0, "{steps:?}: {v}");
}

#[test]
fn the_suggested_repair_makes_new_work() {
    // An override naming the checked-out branch, which has no commit yet.
    let dir = temp("repair-override", false);
    git(&dir, &["init", "-q", "-b", "main"]);
    repair_then_new(&dir, &[("MDKG_MAIN_REF", "main")]);
    // An inherited GIT_DIR, which the user's own git would follow.
    let dir = temp("repair-gitdir", false);
    git(&dir, &["init", "-q", "-b", "main"]);
    repair_then_new(&dir, &[("GIT_DIR", "/nonexistent")]);
    // And an inherited object folder.
    let dir = temp("repair-objects", false);
    git(&dir, &["init", "-q", "-b", "main"]);
    repair_then_new(&dir, &[("GIT_OBJECT_DIRECTORY", "/nonexistent")]);
    // Unborn on master.
    let dir = temp("repair-master", false);
    git(&dir, &["init", "-q", "-b", "master"]);
    repair_then_new(&dir, &[]);
}

#[test]
fn the_main_branch_fix_covers_each_way_it_can_be_missing() {
    // Unborn on master: name it as main, then make the first commit.
    let dir = temp("unborn-master", false);
    git(&dir, &["init", "-q", "-b", "master"]);
    let (_, v) = json(&dir, &["init", "--graph-only", "--json"]);
    assert_eq!(
        v["data"]["next"][0], "git config mdkg.mainRef master",
        "{v}"
    );
    let second = v["data"]["next"][1].as_str().unwrap_or("");
    assert!(second.starts_with("git add -A && git commit"), "{v}");
    // An inherited GIT_DIR doesn't hide the repository from doctor.
    let (_, v) = json_env(&dir, &[("GIT_DIR", "/nonexistent")], &["doctor", "--json"]);
    assert_eq!(status_of(&v, "main"), "warning", "{v}");
    // The environment's override wins over the repository's config.
    git(&dir, &["config", "mdkg.mainRef", "master"]);
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-q", "-m", "first"]);
    let (_, v) = json(&dir, &["doctor", "--json"]);
    assert_eq!(status_of(&v, "main"), "ok", "{v}");
    let (_, v) = json_env(&dir, &[("MDKG_MAIN_REF", "missing")], &["doctor", "--json"]);
    assert_eq!(status_of(&v, "main"), "warning", "{v}");
    assert!(
        check_of(&v, "main")["hint"]
            .as_str()
            .unwrap_or("")
            .contains("MDKG_MAIN_REF"),
        "{v}"
    );
    // A branch name a shell would read is quoted in the command.
    let odd = temp("odd-branch", false);
    git(&odd, &["init", "-q", "-b", "rel$x`y`"]);
    std::fs::write(odd.join("README.md"), "x\n").expect("write");
    git(&odd, &["add", "-A"]);
    git(&odd, &["commit", "-q", "-m", "first"]);
    let (_, v) = json(&odd, &["init", "--graph-only", "--json"]);
    assert_eq!(
        v["data"]["next"][0], "git config mdkg.mainRef 'rel$x`y`'",
        "{v}"
    );
}

#[test]
fn doctor_checks_the_agents_instructions_are_this_builds() {
    // Handwritten unborn metadata keeps this regression runnable without
    // any Git-writing commands. All Git calls below are read-only.
    let dir = temp("doctor-agents", false);
    for path in [".git/objects", ".git/refs"] {
        std::fs::create_dir_all(dir.join(path)).expect("git metadata folder");
    }
    std::fs::write(dir.join(".git/HEAD"), "ref: refs/heads/main\n").expect("HEAD");
    std::fs::write(
        dir.join(".git/config"),
        "[core]\nrepositoryformatversion = 0\nbare = false\n",
    )
    .expect("git config");
    let (code, v) = json(&dir, &["init", "--json"]);
    assert_eq!(code, 0, "{v}");
    enable(&dir, ALL_PROJECTIONS);
    let (_, v) = json(&dir, &["doctor", "--json"]);
    assert_eq!(status_of(&v, "agents"), "ok", "{v}");
    // An older block: install's fix.
    let agents = std::fs::read_to_string(dir.join("AGENTS.md")).expect("AGENTS.md");
    let marker = mdkg_run::install::BLOCK_START;
    let old = agents.replacen(marker, &format!("{marker}\nOlder fixture instructions."), 1);
    assert_ne!(
        old, agents,
        "the old-block fixture must change the managed block"
    );
    std::fs::write(dir.join("AGENTS.md"), &old).expect("write");
    let (_, v) = json(&dir, &["doctor", "--json"]);
    assert_eq!(status_of(&v, "agents"), "warning", "{v}");
    assert!(
        check_of(&v, "agents")["hint"]
            .as_str()
            .unwrap_or("")
            .contains("install"),
        "{v}"
    );
    let (code, v) = json(&dir, &["install", "--json"]);
    assert_eq!(code, 0, "{v}");
    let (_, v) = json(&dir, &["doctor", "--json"]);
    assert_eq!(status_of(&v, "agents"), "ok", "{v}");
    // A CRLF AGENTS.md, as install keeps one, is still this build's block.
    let agents = std::fs::read_to_string(dir.join("AGENTS.md")).expect("AGENTS.md");
    let crlf = agents.replace('\n', "\r\n");
    assert_ne!(
        crlf, agents,
        "the line-ending fixture must change the generated text"
    );
    std::fs::write(dir.join("AGENTS.md"), crlf).expect("write");
    let (_, v) = json(&dir, &["doctor", "--json"]);
    assert_eq!(status_of(&v, "agents"), "ok", "{v}");
    std::fs::write(dir.join("AGENTS.md"), &agents).expect("write");
    // The project's own additions, at this build's version: fine, and said.
    let skill = dir.join(".mdkg/skills/mdkg-workflow/SKILL.md");
    let shipped = std::fs::read_to_string(&skill).expect("skill");
    std::fs::write(
        &skill,
        format!("{shipped}\n## This project\n\n- Run `make lint`.\n"),
    )
    .expect("write");
    let (_, v) = json(&dir, &["doctor", "--json"]);
    assert_eq!(status_of(&v, "agents"), "ok", "{v}");
    assert!(v.to_string().contains("own changes"), "{v}");
    // Whitespace the frontmatter parser takes is the same version.
    let spaced = shipped.replace("version: 0.7.3", "version :  0.7.3");
    assert_ne!(
        spaced, shipped,
        "the whitespace fixture must change the skill"
    );
    std::fs::write(&skill, spaced).expect("write");
    let (_, v) = json(&dir, &["doctor", "--json"]);
    assert_eq!(status_of(&v, "agents"), "ok", "{v}");
    // Another build's skill: bring in this build's.
    let older = shipped.replace("version: 0.7.3", "version: 0.6.9");
    assert_ne!(older, shipped, "the version fixture must change the skill");
    std::fs::write(&skill, older).expect("write");
    let (_, v) = json(&dir, &["doctor", "--json"]);
    assert_eq!(status_of(&v, "agents"), "warning", "{v}");
    assert!(
        check_of(&v, "agents")["hint"]
            .as_str()
            .unwrap_or("")
            .contains("skills add"),
        "{v}"
    );
}

#[test]
fn doctor_fails_without_a_config() {
    let dir = temp("doctor-none", true);
    let (code, v) = json(&dir, &["doctor", "--json"]);
    assert_eq!(code, 2, "{v}");
    common::conforms("doctor", &v);
    assert_eq!(status_of(&v, "config"), "error");
    assert_eq!(status_of(&v, "graph"), "skip");
    assert!(codes(&v).contains(&"doctor.check_failed".to_string()));
}

#[test]
fn doctor_checks_the_0_7_config_keys() {
    let dir = temp("doctor-keys", true);
    json(&dir, &["init", "--graph-only", "--json"]);
    let path = dir.join(".mdkg/config.json");
    // One that leads more than a folder up is valid, but doctor warns.
    let mut cfg = config(&dir);
    cfg["orchestration"] = serde_json::json!({"worktree": "../../agents/{agent}"});
    std::fs::write(&path, cfg.to_string()).expect("write");
    let (_, v) = json(&dir, &["doctor", "--json"]);
    assert_eq!(status_of(&v, "config"), "warning", "{v}");
    // A worktree pattern the launcher would refuse is an error here too.
    let mut cfg = config(&dir);
    cfg["orchestration"] = serde_json::json!({"worktree": "{agent}/wt"});
    std::fs::write(&path, cfg.to_string()).expect("write");
    let (code, v) = json(&dir, &["doctor", "--json"]);
    assert_eq!(code, 2, "{v}");
    assert_eq!(status_of(&v, "config"), "error", "{v}");
    let mut cfg = config(&dir);
    cfg["orchestration"] = serde_json::Value::Null;
    cfg["work"]["location"] = "everything".into();
    std::fs::write(&path, cfg.to_string()).expect("write");
    let (code, v) = json(&dir, &["doctor", "--json"]);
    assert_eq!(code, 2, "{v}");
    assert_eq!(status_of(&v, "config"), "error");
    // A migrated identity-v2 graph keeps its v4 graph_id.
    let mut cfg = config(&dir);
    cfg["work"]["location"] = "git".into();
    cfg["graph_id"] = "6f1c2b3a-4d5e-4f60-8a9b-9c0d1e2f3a4b".into();
    std::fs::write(&path, cfg.to_string()).expect("write");
    let (_, v) = json(&dir, &["doctor", "--json"]);
    assert_eq!(status_of(&v, "config"), "ok", "{v}");
    cfg["graph_id"] = "6F1C2B3A-4D5E-4F60-8A9B-9C0D1E2F3A4B".into();
    std::fs::write(&path, cfg.to_string()).expect("write");
    let (code, v) = json(&dir, &["doctor", "--json"]);
    assert_eq!(code, 2, "{v}");
    assert_eq!(status_of(&v, "config"), "error");
    // A 0.6-era config only warns (migrate moves it), unless --strict.
    std::fs::write(
        &path,
        r#"{"schema_version": 1, "tool": "mdkg", "workspaces": {"root": {"path": ".", "enabled": true, "mdkg_dir": ".mdkg"}}}"#,
    )
    .expect("write");
    let (code, v) = json(&dir, &["doctor", "--json"]);
    assert_eq!(status_of(&v, "config"), "warning", "{v}");
    let warned_only = v["data"]["errors"] == 0;
    assert!(warned_only == (code == 0), "{v}");
    let (code, v) = json(&dir, &["doctor", "--strict", "--json"]);
    assert_eq!(code, 2);
    // ok is false exactly when an error diagnostic is there (edd-4).
    assert_eq!(v["ok"], false);
    assert!(
        v["diagnostics"]
            .as_array()
            .expect("diagnostics")
            .iter()
            .any(|d| d["severity"] == "error"),
        "{v}"
    );
}

/// Sets a store's `meta` schema, as an older or newer build would leave it.
fn set_schema(store: &Path, schema: &str) {
    let conn = rusqlite::Connection::open(store).expect("open store");
    conn.execute("UPDATE meta SET value = ?1 WHERE key = 'schema'", [schema])
        .expect("set schema");
}

#[test]
fn doctor_reads_the_store_without_upgrading_it() {
    let dir = temp("doctor-store", true);
    json(&dir, &["init", "--graph-only", "--json"]);
    std::fs::write(dir.join("a.txt"), "a").expect("write");
    // Claims settle on main, which needs a commit.
    for args in [
        &["checkout", "-q", "-b", "main"][..],
        &["add", "-A"],
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-q",
            "-m",
            "init",
        ],
    ] {
        let ok = Command::new("git")
            .args(args)
            .current_dir(&dir)
            .status()
            .expect("git")
            .success();
        assert!(ok, "git {args:?}");
    }
    // A claim creates the store.
    let out = run(&dir, &["claim", "a.txt"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let store = dir.join(".git/mdkg/coord.sqlite");
    assert!(store.is_file());
    let current: String = rusqlite::Connection::open(&store)
        .expect("open store")
        .query_row("SELECT value FROM meta WHERE key = 'schema'", [], |r| {
            r.get(0)
        })
        .expect("schema");
    let (_, v) = json(&dir, &["doctor", "--json"]);
    assert_eq!(status_of(&v, "store"), "ok", "{v}");

    // An older store is reported, and left exactly as it was.
    set_schema(&store, "1");
    let before = std::fs::read(&store).expect("store");
    let (code, v) = json(&dir, &["doctor", "--json"]);
    assert_eq!(status_of(&v, "store"), "warning", "{v}");
    assert_eq!(code, 0, "{v}");
    assert_eq!(std::fs::read(&store).expect("store"), before);
    let copies = std::fs::read_dir(store.parent().expect("dir"))
        .expect("list")
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains(".pre-"))
        .count();
    assert_eq!(copies, 0, "doctor took no upgrade copy");

    // A newer one is a warning with its version (orchestrator #20626):
    // this build's other commands refuse it, and --strict fails on it.
    set_schema(&store, "99");
    let (code, v) = json(&dir, &["doctor", "--json"]);
    assert_eq!(status_of(&v, "store"), "warning", "{v}");
    assert_eq!(code, 0, "{v}");
    let check = v["data"]["checks"]
        .as_array()
        .expect("checks")
        .iter()
        .find(|c| c["name"] == "store")
        .expect("store check");
    assert_eq!(check["store_schema"], 99, "{v}");
    assert_eq!(check["upgrade_needed"], false, "{v}");
    // --strict fails on exactly that warning: with the store check alone,
    // a current store passes it and the newer one doesn't (review of
    // b2ef39a7).
    let (code, v) = json(&dir, &["doctor", "--only", "store", "--strict", "--json"]);
    assert_ne!(code, 0, "--strict fails on the warning: {v}");
    set_schema(&store, &current);
    let (code, v) = json(&dir, &["doctor", "--only", "store", "--strict", "--json"]);
    assert_eq!(code, 0, "a current store passes --strict: {v}");
}

#[test]
fn doctor_warns_when_an_mcp_config_runs_a_missing_program() {
    let dir = temp("doctor-mcp", true);
    json(&dir, &["init", "--graph-only", "--json"]);
    declare(&dir, &["mcp:claude"]);
    std::fs::write(
        dir.join(".mcp.json"),
        r#"{"mcpServers":{"mdkg":{"type":"stdio","command":"mdkg-not-installed-anywhere","args":["mcp"]}}}"#,
    )
    .expect("write");
    let (_, v) = json(&dir, &["doctor", "--json"]);
    assert_eq!(status_of(&v, "mcp"), "warning", "{v}");
    // A config whose projection isn't enabled isn't mdkg's to check.
    declare(&dir, &[]);
    let (_, v) = json(&dir, &["doctor", "--json"]);
    assert_eq!(status_of(&v, "mcp"), "skip", "{v}");
}

#[test]
fn claudes_hooks_never_write_or_ignore_the_shared_settings() {
    // dec-01a18ee0 item 11: Claude's hooks go in its per-user
    // `.claude/settings.local.json`, ignored in the clone's own
    // info/exclude when install creates it (orchestrator #13444), never in
    // the shared .gitignore; the team's shared `.claude/settings.json`
    // stays as it was, and stays committable. Codex's and OpenCode's files
    // keep their .gitignore rule (G24).
    let dir = temp("claude-hooks-local", true);
    json(&dir, &["init", "--json"]);
    let shared = dir.join(".claude/settings.json");
    std::fs::create_dir_all(shared.parent().expect("parent")).expect("mkdir");
    let team = "{\"permissions\": {\"allow\": [\"Bash(cargo test)\"]}}\n";
    std::fs::write(&shared, team).expect("write");
    let (code, v) = json(
        &dir,
        &[
            "install",
            "--hooks",
            "--enable",
            "hooks:claude,hooks:codex,hooks:opencode",
            "--json",
        ],
    );
    assert_eq!(code, 0, "{v}");
    assert_eq!(std::fs::read_to_string(&shared).expect("shared"), team);
    let local: Value = serde_json::from_slice(
        &std::fs::read(dir.join(".claude/settings.local.json")).expect("local"),
    )
    .expect("json");
    assert!(local["hooks"]["SessionStart"].is_array(), "{local}");
    let ignore = std::fs::read_to_string(dir.join(".gitignore")).unwrap_or_default();
    assert!(!ignore.contains(".claude/"), "{ignore}");
    for target in ["/.codex/hooks.json", "/.opencode/plugins/mdkg/server.ts"] {
        assert!(ignore.lines().any(|l| l == target), "{target}: {ignore}");
    }
    let ignored = |path: &str| {
        Command::new("git")
            .args(["check-ignore", "-q", path])
            .current_dir(&dir)
            .status()
            .expect("git check-ignore")
            .success()
    };
    assert!(!ignored(".claude/settings.json"));
    assert!(ignored(".claude/settings.local.json"));
    let exclude = std::fs::read_to_string(dir.join(".git/info/exclude")).expect("exclude");
    assert!(
        exclude.lines().any(|l| l == "/.claude/settings.local.json"),
        "{exclude}"
    );
    assert!(!v.to_string().contains("settings.json is ignored"), "{v}");
    // Only the team's own file is there to commit under .claude/.
    let staged = |dir: &Path| {
        let out = Command::new("git")
            .args(["add", "-A", "--dry-run", "--", ".claude"])
            .current_dir(dir)
            .output()
            .expect("git add");
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    assert_eq!(staged(&dir), "add '.claude/settings.json'\n");
    // A sync again leaves both alone.
    let (code, v) = json(&dir, &["install", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(std::fs::read_to_string(&shared).expect("shared"), team);
    // Undo takes its exclude line out with the file.
    let (code, v) = json(&dir, &["install", "--undo", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert!(!dir.join(".claude/settings.local.json").exists(), "{v}");
    let exclude = std::fs::read_to_string(dir.join(".git/info/exclude")).expect("exclude");
    assert!(
        !exclude.lines().any(|l| l == "/.claude/settings.local.json"),
        "{exclude}"
    );
    std::fs::remove_dir_all(&dir).expect("cleanup");

    // A fresh install with no shared file: nothing under .claude/ to
    // commit. One the user already had keeps its ignore state.
    let dir = temp("claude-hooks-fresh", true);
    json(&dir, &["init", "--json"]);
    let (code, v) = json(
        &dir,
        &["install", "--hooks", "--enable", "hooks:claude", "--json"],
    );
    assert_eq!(code, 0, "{v}");
    assert!(dir.join(".claude/settings.local.json").is_file(), "{v}");
    assert_eq!(staged(&dir), "", "{v}");
    std::fs::remove_dir_all(&dir).expect("cleanup");
    let dir = temp("claude-hooks-theirs", true);
    json(&dir, &["init", "--json"]);
    std::fs::create_dir_all(dir.join(".claude")).expect("mkdir");
    std::fs::write(
        dir.join(".claude/settings.local.json"),
        "{\"model\": \"opus\"}\n",
    )
    .expect("write");
    let (code, v) = json(
        &dir,
        &["install", "--hooks", "--enable", "hooks:claude", "--json"],
    );
    assert_eq!(code, 0, "{v}");
    let exclude = std::fs::read_to_string(dir.join(".git/info/exclude")).unwrap_or_default();
    assert!(!exclude.contains("settings.local.json"), "{exclude}");
    std::fs::remove_dir_all(&dir).expect("cleanup");
}

/// #13893: the per-user hook file's exclusion follows install's record,
/// so a retry repairs one a failed install never wrote; and install says
/// when git still doesn't ignore the file (a .gitignore negation, or a
/// linked folder) instead of calling it ignored.
#[test]
fn claudes_per_user_exclusion_is_repaired_and_checked_against_git() {
    let exclude_holds = |dir: &Path| {
        std::fs::read_to_string(dir.join(".git/info/exclude"))
            .unwrap_or_default()
            .lines()
            .any(|l| l == "/.claude/settings.local.json")
    };
    let dir = temp("claude-hooks-retry", true);
    json(&dir, &["init", "--json"]);
    let (code, v) = json(
        &dir,
        &["install", "--hooks", "--enable", "hooks:claude", "--json"],
    );
    assert_eq!(code, 0, "{v}");
    assert!(exclude_holds(&dir), "{v}");
    // As if the exclude write had failed after the file was written.
    let file = dir.join(".git/info/exclude");
    let text = std::fs::read_to_string(&file).expect("exclude");
    std::fs::write(&file, text.replace("/.claude/settings.local.json\n", "")).expect("w");
    let (code, v) = json(&dir, &["install", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert!(exclude_holds(&dir), "a retry repairs it: {v}");
    std::fs::remove_dir_all(&dir).expect("cleanup");

    let dir = temp("claude-hooks-negated", true);
    json(&dir, &["init", "--json"]);
    std::fs::write(dir.join(".gitignore"), "!/.claude/settings.local.json\n").expect("w");
    let (code, v) = json(
        &dir,
        &["install", "--hooks", "--enable", "hooks:claude", "--json"],
    );
    // A new hook file git wouldn't ignore is refused before it's made,
    // naming the rule (#14381 E).
    assert_ne!(code, 0, "{v}");
    assert!(v.to_string().contains("git wouldn't ignore it"), "{v}");
    assert!(v.to_string().contains(".gitignore:1:"), "{v}");
    assert!(!dir.join(".claude/settings.local.json").exists(), "{v}");
    std::fs::remove_dir_all(&dir).expect("cleanup");

    #[cfg(unix)]
    {
        let dir = temp("claude-hooks-linked-folder", true);
        json(&dir, &["init", "--json"]);
        std::fs::create_dir_all(dir.join("config")).expect("mkdir");
        std::os::unix::fs::symlink("config", dir.join(".claude")).expect("ln");
        let (code, v) = json(
            &dir,
            &["install", "--hooks", "--enable", "hooks:claude", "--json"],
        );
        // Git never ignores a path under a link: a new hook file there is
        // refused too (#14381 E), and with no rule to name, says so
        // (#14492).
        assert_ne!(code, 0, "{v}");
        assert!(
            v.to_string().contains("git can't say it would ignore it"),
            "{v}"
        );
        assert!(!dir.join("config/settings.local.json").exists(), "{v}");
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }
}

#[test]
fn doctor_checks_each_enabled_hook_projection() {
    let dir = temp("doctor-hooks", true);
    json(&dir, &["init", "--json"]);
    let (_, v) = json(&dir, &["doctor", "--json"]);
    assert_eq!(status_of(&v, "hooks"), "skip", "{v}");
    enable(&dir, &["hooks:claude", "hooks:opencode"]);
    let (_, v) = json(&dir, &["doctor", "--json"]);
    assert_eq!(status_of(&v, "hooks"), "ok", "{v}");
    for (path, drift) in [
        (".claude/settings.local.json", "{\"hooks\": {}}\n"),
        (".opencode/plugins/mdkg/server.ts", "// drift\n"),
    ] {
        std::fs::write(dir.join(path), drift).expect("write");
        let (_, v) = json(&dir, &["doctor", "--json"]);
        assert_eq!(status_of(&v, "hooks"), "warning", "{v}");
        assert!(check_of(&v, "hooks").to_string().contains(path), "{v}");
        let (code, v) = json(&dir, &["install", "--json"]);
        assert_eq!(code, 0, "{v}");
        let (_, v) = json(&dir, &["doctor", "--json"]);
        assert_eq!(status_of(&v, "hooks"), "ok", "{v}");
    }
}

fn skill(dir: &Path, slug: &str, frontmatter: &str) {
    let d = dir.join(".mdkg/skills").join(slug);
    std::fs::create_dir_all(&d).expect("mkdir");
    std::fs::write(
        d.join("SKILL.md"),
        format!("---\n{frontmatter}\n---\n\n# Purpose\n\nBody.\n"),
    )
    .expect("write");
}

#[test]
fn skills_list_show_and_validate() {
    let dir = temp("skills", true);
    json(&dir, &["init", "--graph-only", "--json"]);
    skill(
        &dir,
        "review-code",
        "name: review-code\ndescription: Review a commit\ntags: [stage:review]\nversion: 0.1.0",
    );
    skill(
        &dir,
        "plan-work",
        "name: plan-work\ndescription: Plan\ntags: [stage:plan]",
    );
    let (code, v) = json(&dir, &["skills", "list", "--json"]);
    assert_eq!(code, 0, "{v}");
    common::conforms("skills.list", &v);
    let slugs: Vec<&str> = v["data"]["skills"]
        .as_array()
        .expect("skills")
        .iter()
        .filter_map(|s| s["slug"].as_str())
        .collect();
    assert_eq!(slugs, ["plan-work", "review-code"]);
    let (_, v) = json(
        &dir,
        &["skills", "list", "--tags", "stage:review", "--json"],
    );
    assert_eq!(v["data"]["skills"].as_array().expect("skills").len(), 1);

    let (code, v) = json(&dir, &["skills", "show", "review-code", "--json"]);
    assert_eq!(code, 0, "{v}");
    common::conforms("skills.show", &v);
    assert_eq!(v["data"]["skill"]["version"], "0.1.0");
    assert!(v["data"]["body"].as_str().expect("body").contains("Body."));
    let (_, v) = json(&dir, &["skills", "show", "review-code", "--meta", "--json"]);
    assert!(v["data"].get("body").is_none());
    let (code, v) = json(&dir, &["skills", "show", "nope", "--json"]);
    assert_eq!(code, 3);
    assert_eq!(codes(&v), ["skill.not_found"]);
    // 0.6.0's spelling still works.
    let out = run(&dir, &["skill", "list"]);
    assert!(out.status.success());

    // Bad skills fail validate, naming each problem.
    skill(&dir, "no-description", "name: no-description");
    skill(&dir, "Not_Kebab", "name: x\ndescription: y");
    let (code, v) = json(&dir, &["skills", "validate", "--json"]);
    assert_eq!(code, 2, "{v}");
    common::conforms("skills.validate", &v);
    let found = codes(&v);
    assert!(found.contains(&"skill.invalid".to_string()), "{v}");
    assert!(found.contains(&"node.bad_skill_slug".to_string()), "{v}");
    // One skill alone.
    let (code, v) = json(&dir, &["skills", "validate", "review-code", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["data"]["checked"], 1);
}

#[test]
fn skills_sync_mirrors_and_validate_sees_drift() {
    let dir = temp("skills-sync", true);
    json(&dir, &["init", "--graph-only", "--json"]);
    // Nothing is mirrored until a mirror is enabled.
    skill(&dir, "early", "name: early\ndescription: Early");
    let (code, v) = json(&dir, &["skills", "sync", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert!(!dir.join(".claude").exists() && !dir.join(".agents").exists());
    let (_, v) = json(&dir, &["skills", "validate", "--json"]);
    assert_eq!(codes(&v), Vec::<String>::new(), "{v}");
    std::fs::remove_dir_all(dir.join(".mdkg/skills/early")).expect("rm");
    declare(&dir, MIRRORS);
    skill(
        &dir,
        "review-code",
        "name: review-code\ndescription: Review",
    );
    let (_, v) = json(&dir, &["skills", "validate", "--json"]);
    assert!(
        codes(&v).contains(&"skill.mirror_missing".to_string()),
        "{v}"
    );

    let (code, v) = json(&dir, &["skills", "sync", "--dry-run", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert!(!dir.join(".claude/skills").exists());
    let (code, v) = json(&dir, &["skills", "sync", "--json"]);
    assert_eq!(code, 0, "{v}");
    common::conforms("skills.sync", &v);
    for mirror in [".agents/skills", ".claude/skills"] {
        assert!(dir.join(mirror).join("review-code/SKILL.md").is_file());
        let managed: Value = serde_json::from_str(
            &std::fs::read_to_string(dir.join(mirror).join(".mdkg-managed.json")).expect("managed"),
        )
        .expect("json");
        assert_eq!(managed["managed_slugs"], serde_json::json!(["review-code"]));
    }
    let (code, v) = json(&dir, &["skills", "validate", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(codes(&v), Vec::<String>::new(), "{v}");
    // The whole graph agrees.
    let out = run(&dir, &["validate", "--json"]);
    let v: Value = serde_json::from_slice(&out.stdout).expect("json");
    assert!(!codes(&v).iter().any(|c| c.starts_with("skill.")), "{v}");

    // An edit to a mirror is drift until the next sync.
    let mirrored = dir.join(".claude/skills/review-code/SKILL.md");
    std::fs::write(&mirrored, "edited\n").expect("write");
    let (_, v) = json(&dir, &["skills", "validate", "--json"]);
    assert_eq!(codes(&v), ["skill.mirror_drift"], "{v}");
    let (_, v) = json(&dir, &["doctor", "--json"]);
    assert_eq!(status_of(&v, "mirrors"), "warning", "{v}");
    json(&dir, &["skills", "sync", "--json"]);
    assert!(
        std::fs::read_to_string(&mirrored)
            .expect("read")
            .contains("Review")
    );

    // A removed skill's mirror is stale, and the next sync retires it.
    std::fs::remove_dir_all(dir.join(".mdkg/skills/review-code")).expect("rm");
    let (_, v) = json(&dir, &["skills", "validate", "--json"]);
    assert!(codes(&v).contains(&"skill.mirror_stale".to_string()), "{v}");
    let (code, v) = json(&dir, &["skills", "sync", "--json"]);
    assert_eq!(code, 0, "{v}");
    for mirror in [".agents/skills", ".claude/skills"] {
        assert!(!dir.join(mirror).join("review-code").exists(), "{mirror}");
        let managed: Value = serde_json::from_str(
            &std::fs::read_to_string(dir.join(mirror).join(".mdkg-managed.json")).expect("managed"),
        )
        .expect("json");
        assert_eq!(managed["managed_slugs"], serde_json::json!([]));
    }
    let (_, v) = json(&dir, &["skills", "validate", "--json"]);
    assert_eq!(codes(&v), Vec::<String>::new(), "{v}");
}

#[test]
fn skills_sync_after_install_keeps_install_undo_whole() {
    let dir = temp("skills-undo", true);
    json(&dir, &["init", "--graph-only", "--json"]);
    skill(&dir, "a", "name: a\ndescription: d");
    skill(&dir, "b", "name: b\ndescription: d");
    enable(&dir, ALL_PROJECTIONS);
    // Edit one skill and retire the other, then sync.
    std::fs::write(
        dir.join(".mdkg/skills/a/SKILL.md"),
        "---\nname: a\ndescription: e\n---\n",
    )
    .expect("write");
    std::fs::remove_dir_all(dir.join(".mdkg/skills/b")).expect("rm");
    let (code, v) = json(&dir, &["skills", "sync", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert!(!dir.join(".claude/skills/b").exists());
    // install --undo still takes back everything install and sync did.
    let out = run(&dir, &["install", "--undo", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(!dir.join(".claude/skills").exists());
    assert!(!dir.join(".agents/skills").exists());
    assert!(!dir.join(".mcp.json").exists());
}

#[test]
fn skills_sync_refuses_mirror_folders_it_cant_write() {
    let dir = temp("skills-targets", true);
    json(&dir, &["init", "--graph-only", "--json"]);
    skill(&dir, "a", "name: a\ndescription: d");
    let path = dir.join(".mdkg/config.json");
    let mut cfg = config(&dir);
    // A config from before projections: its own targets name the mirrors.
    cfg.as_object_mut().expect("object").remove("projections");
    cfg["customization"]["skill_mirrors"]["targets"] = serde_json::json!(["tools/skills"]);
    std::fs::write(&path, cfg.to_string()).expect("write");
    let (code, v) = json(&dir, &["skills", "sync", "--json"]);
    assert_eq!(code, 1, "{v}");
    assert_eq!(codes(&v), ["skill.mirror_targets_unsupported"]);
    assert!(!dir.join(".claude").exists() && !dir.join("tools").exists());
    // Once projections are declared, they name the mirrors, and doctor
    // says the old targets are ignored.
    declare(&dir, &["skills:.claude/skills"]);
    let (code, v) = json(&dir, &["skills", "sync", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert!(dir.join(".claude/skills/a/SKILL.md").is_file());
    assert!(!dir.join(".agents").exists() && !dir.join("tools").exists());
    let (_, v) = json(&dir, &["doctor", "--json"]);
    assert_eq!(status_of(&v, "config"), "warning", "{v}");
    assert!(v.to_string().contains("ignored"), "{v}");
}

#[test]
fn skills_never_echo_a_typed_slug() {
    let dir = temp("skills-echo", true);
    json(&dir, &["init", "--graph-only", "--json"]);
    for cmd in ["show", "validate"] {
        let (code, v) = json(&dir, &["skills", cmd, "correct-horse-battery", "--json"]);
        assert_eq!(code, 3, "{v}");
        assert!(!v.to_string().contains("correct-horse"), "{v}");
    }
}

#[test]
fn init_resumes_only_from_its_own_output() {
    let dir = temp("init-stop", true);
    // What a stop leaves: a config stage cut off part way through the
    // graph_id, and the seeded skill published, but no config yet.
    std::fs::create_dir_all(dir.join(".mdkg/skills/mdkg-workflow")).expect("mkdir");
    std::fs::write(
        dir.join(".mdkg/.config.json.0a1b2c3d4e5f.mdkg-init.tmp"),
        "{\n  \"schema_version\": 3,\n  \"format_version\": \"0.7\",\n  \"graph_id\": \"01a170d6-ab",
    )
    .expect("write");
    let skill = dir.join(".mdkg/skills/mdkg-workflow/SKILL.md");
    let shipped = SHIPPED_SKILL.as_bytes().to_vec();
    std::fs::write(&skill, &shipped).expect("write");
    let (code, v) = json(&dir, &["init", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert!(config(&dir)["graph_id"].is_string());
    // The stage is left where it is, and named in a note: nothing is
    // deleted.
    let leftovers: Vec<_> = walk(&dir.join(".mdkg"))
        .into_iter()
        .filter(|p| p.to_string_lossy().ends_with(".mdkg-init.tmp"))
        .collect();
    assert_eq!(leftovers.len(), 1, "{leftovers:?}");
    assert!(
        v["data"]["notes"].to_string().contains("staging file"),
        "{v}"
    );
    let files = v["data"]["changes"].as_array().expect("files");
    assert!(
        files
            .iter()
            .any(|f| f["path"] == ".mdkg/skills/mdkg-workflow/SKILL.md" && f["action"] == "kept"),
        "{v}"
    );
    // The published skill has one name, so enabling a mirror takes it.
    enable(&dir, MIRRORS);
    assert_mirrored(&dir);
}

/// Both mirrors hold the shipped skill, and the canonical copy has one
/// name (a second hard link would keep the installer from mirroring it).
fn assert_mirrored(dir: &Path) {
    use std::os::unix::fs::MetadataExt;
    let canonical = dir.join(".mdkg/skills/mdkg-workflow/SKILL.md");
    let bytes = std::fs::read(&canonical).expect("canonical skill");
    assert_eq!(std::fs::metadata(&canonical).expect("meta").nlink(), 1);
    for mirror in [".agents/skills", ".claude/skills"] {
        let copy = dir.join(mirror).join("mdkg-workflow/SKILL.md");
        assert_eq!(
            std::fs::read(&copy).ok().as_deref(),
            Some(&bytes[..]),
            "{mirror}"
        );
    }
    let stray: Vec<_> = walk(&dir.join(".mdkg/skills/mdkg-workflow"))
        .into_iter()
        .chain(walk(&dir.join(".claude/skills")))
        .chain(walk(&dir.join(".agents/skills")))
        .filter(|p| p.to_string_lossy().ends_with(".mdkg-init.tmp"))
        .collect();
    assert!(stray.is_empty(), "{stray:?}");
}

#[test]
fn init_stopped_before_or_after_publishing_the_skill_still_mirrors_it() {
    // Before: the skill staged a folder up, not yet renamed into place.
    let dir = temp("init-stop-staged", true);
    std::fs::create_dir_all(dir.join(".mdkg/skills")).expect("mkdir");
    let shipped = SHIPPED_SKILL.as_bytes().to_vec();
    std::fs::write(
        dir.join(".mdkg/skills/.mdkg-workflow.SKILL.md.0a1b2c3d4e5f.mdkg-init.tmp"),
        &shipped,
    )
    .expect("write");
    let (code, v) = json(&dir, &["init", "--json"]);
    assert_eq!(code, 0, "{v}");
    enable(&dir, MIRRORS);
    assert_mirrored(&dir);
    // After: published, then stopped before the installer ran.
    let dir = temp("init-stop-published", true);
    json(&dir, &["init", "--graph-only", "--json"]);
    std::fs::remove_file(dir.join(".mdkg/config.json")).expect("rm");
    std::fs::create_dir_all(dir.join(".mdkg/skills/mdkg-workflow")).expect("mkdir");
    std::fs::write(dir.join(".mdkg/skills/mdkg-workflow/SKILL.md"), &shipped).expect("write");
    let (code, v) = json(&dir, &["init", "--json"]);
    assert_eq!(code, 0, "{v}");
    enable(&dir, MIRRORS);
    assert_mirrored(&dir);
}

#[test]
fn init_refuses_anything_but_the_skill_at_the_skill_path() {
    let dir = temp("init-skill-dir", true);
    std::fs::create_dir_all(dir.join(".mdkg/skills/mdkg-workflow/SKILL.md")).expect("mkdir");
    let (code, v) = json(&dir, &["init", "--json"]);
    assert_eq!(code, 5, "{v}");
    assert_eq!(codes(&v), ["init.unexplained"]);
    // No agent setup and no config were written.
    assert_eq!(tree(&dir), Vec::<String>::new());
    assert!(!dir.join(".mcp.json").exists() && !dir.join(".mdkg/config.json").exists());
}

#[test]
fn skills_add_stopped_after_publishing_mirrors_on_the_next_run() {
    let dir = temp("skills-add-stop", true);
    json(&dir, &["init", "--graph-only", "--json"]);
    declare(&dir, MIRRORS);
    commit_all(&dir);
    // Published, then stopped before the sync.
    let shipped = SHIPPED_SKILL.as_bytes().to_vec();
    std::fs::create_dir_all(dir.join(".mdkg/skills/mdkg-workflow")).expect("mkdir");
    std::fs::write(dir.join(".mdkg/skills/mdkg-workflow/SKILL.md"), &shipped).expect("write");
    let (code, v) = json(&dir, &["skills", "add", "mdkg-workflow", "--json"]);
    assert_eq!(code, 0, "{v}");
    // The copy a stopped run left was never committed: this run commits it
    // (Terra #19620), so it's this run's addition.
    assert_eq!(v["data"]["action"], "created", "{v}");
    assert!(v["data"]["commit"].is_string(), "{v}");
    assert_eq!(
        git_out(&dir, &["status", "--porcelain", "--", ".mdkg/skills"]),
        "",
        "committed"
    );
    assert_mirrored(&dir);
    // A run with the skill already committed changes nothing.
    let (code, v) = json(&dir, &["skills", "add", "mdkg-workflow", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["data"]["action"], "kept", "{v}");
    assert!(v["data"]["commit"].is_null(), "{v}");
    // A staged change of the skill is never replaced (Terra #19620).
    let skill = dir.join(".mdkg/skills/mdkg-workflow/SKILL.md");
    std::fs::write(
        &skill,
        "---\nname: mdkg-workflow\ndescription: staged\n---\n",
    )
    .expect("a");
    git(&dir, &["add", ".mdkg/skills/mdkg-workflow/SKILL.md"]);
    std::fs::write(
        &skill,
        "---\nname: mdkg-workflow\ndescription: later\n---\n",
    )
    .expect("b");
    let (code, v) = json(
        &dir,
        &["skills", "add", "mdkg-workflow", "--replace", "--json"],
    );
    assert_ne!(code, 0, "{v}");
    assert!(
        v["diagnostics"][0]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("staged changes"),
        "{v}"
    );
    assert!(
        git_out(&dir, &["show", ":.mdkg/skills/mdkg-workflow/SKILL.md"]).contains("staged"),
        "the staged version is kept"
    );
    assert!(
        std::fs::read_to_string(&skill)
            .expect("b")
            .contains("later")
    );
    // No stray staging file is left in the skill's folder to be mirrored.
    let names: Vec<_> = std::fs::read_dir(dir.join(".mdkg/skills/mdkg-workflow"))
        .expect("folder")
        .map(|e| e.expect("entry").file_name())
        .collect();
    assert_eq!(names, ["SKILL.md"], "{names:?}");
    // And a clean add publishes with one name.
    let dir = temp("skills-add-clean", true);
    json(&dir, &["init", "--graph-only", "--json"]);
    declare(&dir, MIRRORS);
    commit_all(&dir);
    let (code, v) = json(&dir, &["skills", "add", "mdkg-workflow", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert_mirrored(&dir);
}

#[test]
fn init_never_takes_a_file_for_its_own_by_name() {
    let dir = temp("init-not-own", true);
    std::fs::create_dir_all(dir.join(".mdkg/skills/mdkg-workflow")).expect("mkdir");
    // Named like staging files, but not what init writes.
    std::fs::write(
        dir.join(".mdkg/skills/mdkg-workflow/.notes.0a1b2c3d4e5f.mdkg-init.tmp"),
        "mine",
    )
    .expect("write");
    std::fs::write(
        dir.join(".mdkg/.config.json.0a1b2c3d4e5f.mdkg-init.tmp"),
        "{\"half",
    )
    .expect("write");
    // Empty, but for a file init never stages.
    let before = snapshot(&dir);
    let (code, v) = json(&dir, &["init", "--json"]);
    assert_eq!(code, 1, "{v}");
    assert_eq!(codes(&v), ["init.unexplained"]);
    assert_eq!(snapshot(&dir), before);
}

#[test]
fn init_never_takes_an_empty_file_for_a_target_it_never_stages() {
    // Alone in a fresh folder, so nothing else refuses the init first.
    let dir = temp("init-empty-unknown", true);
    std::fs::create_dir_all(dir.join(".mdkg/skills/mdkg-workflow")).expect("mkdir");
    std::fs::write(
        dir.join(".mdkg/skills/mdkg-workflow/.extra.0a1b2c3d4e5f.mdkg-init.tmp"),
        "",
    )
    .expect("write");
    let before = snapshot(&dir);
    let (code, v) = json(&dir, &["init", "--json"]);
    assert_eq!(code, 1, "{v}");
    assert_eq!(codes(&v), ["init.unexplained"]);
    assert_eq!(snapshot(&dir), before);
}

#[test]
fn init_below_the_worktree_top_writes_the_graph_but_install_refuses() {
    let dir = temp("init-nested", true);
    let sub = dir.join("sub");
    std::fs::create_dir_all(&sub).expect("mkdir");
    // Init writes only the graph, which may sit anywhere.
    let (code, v) = json(&sub, &["init", "--json"]);
    assert_eq!(code, 0, "{v}");
    let files = tree(&dir);
    assert!(
        !files.is_empty() && files.iter().all(|f| f.starts_with("sub/")),
        "{files:?}"
    );
    // The agents' files need the graph at the top: install says so and
    // writes nothing.
    declare(&sub, ALL_PROJECTIONS);
    let before = snapshot(&dir);
    let (code, v) = json(&sub, &["install", "--json"]);
    assert_eq!(code, 1, "{v}");
    assert_eq!(codes(&v), ["install.unsupported_layout"]);
    assert_eq!(snapshot(&dir), before);
}

#[test]
fn init_can_create_a_missing_directory_beneath_a_repository() {
    let dir = temp("init-missing-nested", true);
    let sub = dir.join("missing/nested");
    let root = sub.to_str().expect("path");
    let (code, v) = json(&dir, &["--root", root, "init", "--dry-run", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert!(!dir.join("missing").exists());
    let (code, v) = json(&dir, &["--root", root, "init", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert!(sub.join(".mdkg/config.json").is_file());
    assert!(tree(&dir).iter().all(|p| p.starts_with("missing/nested/")));
}

#[test]
fn skills_add_never_writes_through_a_link_and_survives_a_stop() {
    let dir = temp("skills-add-link", true);
    json(&dir, &["init", "--graph-only", "--json"]);
    declare(&dir, MIRRORS);
    commit_all(&dir);
    let outside = temp("skills-add-outside", false);
    std::fs::create_dir_all(dir.join(".mdkg/skills")).expect("mkdir");
    std::os::unix::fs::symlink(&outside, dir.join(".mdkg/skills/mdkg-workflow")).expect("ln");
    // A file outside, named and filled like an add's own staging file.
    let planted = outside.join(".SKILL.md.0a1b2c3d4e5f.mdkg-init.tmp");
    std::fs::write(&planted, "---\nname: mdkg").expect("write");
    let (code, v) = json(&dir, &["skills", "add", "mdkg-workflow", "--json"]);
    assert_ne!(code, 0, "{v}");
    assert!(!outside.join("SKILL.md").exists());
    assert!(planted.exists(), "a file outside the project was removed");
    std::fs::remove_file(dir.join(".mdkg/skills/mdkg-workflow")).expect("rm");
    // A stopped add left a part-written staging file. It sits a folder
    // up, outside the skill, so the add completes, and the stray file is
    // never mirrored.
    let stage = dir.join(".mdkg/skills/.mdkg-workflow.SKILL.md.0a1b2c3d4e5f.mdkg-init.tmp");
    std::fs::write(&stage, "---\nname: mdkg-wor").expect("write");
    let (code, v) = json(&dir, &["skills", "add", "mdkg-workflow", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["data"]["action"], "created");
    let stray: Vec<_> = walk(&dir.join(".claude"))
        .into_iter()
        .chain(walk(&dir.join(".mdkg/skills/mdkg-workflow")))
        .filter(|p| p.to_string_lossy().ends_with(".mdkg-init.tmp"))
        .collect();
    assert!(stray.is_empty(), "{stray:?}");
}

/// The skill as the binary ships it.
const SHIPPED_SKILL: &str = include_str!("../src/workflow_skill.md");

#[test]
fn init_takes_a_lock_so_two_cant_interleave() {
    let dir = temp("init-lock", true);
    std::fs::create_dir_all(dir.join(".mdkg")).expect("mkdir");
    let held = std::fs::File::open(dir.join(".mdkg")).expect("open");
    held.try_lock().expect("lock");
    let (code, v) = json(&dir, &["init", "--json"]);
    assert_eq!(code, 5, "{v}");
    assert_eq!(codes(&v), ["init.exists"]);
    assert!(!dir.join(".mdkg/config.json").exists());
    drop(held);
    let (code, v) = json(&dir, &["init", "--json"]);
    assert_eq!(code, 0, "{v}");
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).expect("list").flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}
