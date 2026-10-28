//! Handles in every command that takes a node (edd-6; task-41): a full
//! id, a unique id prefix, an exact legacy id and a title fragment, on a
//! format 0.7 graph, through the binary in throwaway repositories.

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

fn scratch(name: &str) -> Scratch {
    let dir = common::temp::dir(&format!("mdkg-cli-handles-{name}"));
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
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn run(root: &Path, args: &[&str]) -> (i32, Value) {
    let out = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
        .arg("--root")
        .arg(root)
        .args(["--as", "tester"])
        .args(args)
        .arg("--json")
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

const GOAL: &str = "goal-01910000-0000-7000-8000-000000000001";
const PARSER: &str = "task-0192f3a4-7b2c-7def-8a9b-1c2d3e4f5a6b";
const LEASES: &str = "task-01930000-0000-7000-8000-000000000002";
const MERKLE: &str = "task-01940000-0000-7000-8000-000000000003";

/// A format 0.7 node file.
fn node(id: &str, title: &str, extra: &[&str]) -> String {
    let node_type = id.split('-').next().unwrap_or_default();
    let mut lines = vec![
        "---".to_string(),
        format!("id: {id}"),
        format!("type: {node_type}"),
        format!("title: {title}"),
        "status: todo".into(),
    ];
    lines.extend(extra.iter().map(|s| s.to_string()));
    lines.extend([
        "created: 2026-10-24T00:00:00.000Z".into(),
        "updated: 2026-10-24T00:00:00.000Z".into(),
        "---".into(),
    ]);
    format!("{}\n# Overview\n\n{title}.\n", lines.join("\n"))
}

/// A 0.7 graph (from `init`) with a goal and three tasks: the parser task
/// migrated from task-19, and next after it the leases task.
fn repo(name: &str) -> Scratch {
    let s = scratch(name);
    git(&s.0, &["init", "-q", "-b", "main"]);
    ok(&s.0, &["init", "--work", "shared"]);
    let work = s.0.join(".mdkg/work");
    fs::create_dir_all(&work).expect("work dir");
    let scope = format!("scope_refs: [{PARSER}, {LEASES}, {MERKLE}]");
    let goal_fields = [
        "goal_state: active",
        "goal_condition: the handles work",
        &scope,
        "required_skills: []",
        "required_checks: []",
        "max_iterations: 25",
        "blocked_after_attempts: 3",
    ];
    let files = [
        ("goal-ship.md", node(GOAL, "Ship the handles", &goal_fields)),
        (
            "task-parser.md",
            node(
                PARSER,
                "Parser for frontmatter",
                &[
                    "legacy_id: task-19",
                    &format!("parent: {GOAL}"),
                    &format!("next: {LEASES}"),
                ],
            ),
        ),
        (
            "task-leases.md",
            node(
                LEASES,
                "Coordinator leases",
                &[&format!("parent: {GOAL}"), &format!("prev: {PARSER}")],
            ),
        ),
        (
            "task-merkle.md",
            node(MERKLE, "Merkle roots", &[&format!("parent: {GOAL}")]),
        ),
    ];
    for (file, text) in files {
        fs::write(work.join(file), text).expect("write node");
    }
    git(&s.0, &["add", "-A"]);
    git(&s.0, &["commit", "-qm", "init"]);
    s
}

/// The parser task's four handles: full id, prefix, legacy id, title.
const HANDLES: [&str; 4] = [PARSER, "0192f3a4", "task-19", "parser front"];

fn notes_name_the_node(out: &Value, handle: &str, id: &str) {
    let named = out["diagnostics"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|d| d["code"] == "node.resolved");
    if handle == id {
        assert!(named.is_none(), "a full id needs no note: {out}");
    } else {
        let message = named
            .and_then(|d| d["message"].as_str())
            .unwrap_or_default();
        assert!(message.contains(id), "{handle}: {out}");
        assert!(
            message.contains("Parser for frontmatter"),
            "{handle}: {out}"
        );
    }
}

#[test]
fn claim_release_and_sync_take_every_handle() {
    let r = repo("claim");
    for h in HANDLES {
        let out = ok(&r.0, &["claim", h]);
        assert_eq!(
            out["data"]["lease"]["scope"], ".mdkg/work/task-parser.md",
            "{h}: {out}"
        );
        notes_name_the_node(&out, h, PARSER);
        ok(&r.0, &["sync", h]);
        // Released by the same handle it was claimed with (RC1 audit U1).
        let out = ok(&r.0, &["release", h]);
        assert_eq!(out["data"]["lease"]["status"], "released", "{h}: {out}");
    }
    // Release refuses an ambiguous handle as claim does, holding on.
    ok(&r.0, &["claim", PARSER]);
    let (code, out) = run(&r.0, &["release", "er"]);
    assert_eq!(
        (code, codes(&out)),
        (7, vec!["node.ambiguous".to_string()]),
        "{out}"
    );
    ok(&r.0, &["release", "parser front"]);
    // A fragment naming several nodes refuses and lists them.
    let (code, out) = run(&r.0, &["claim", "er"]);
    // A 0.7 graph's ids are unique: matches are listed bare (lane 1 #138).
    assert!(!out["diagnostics"].to_string().contains("root:"), "{out}");
    assert!(out["diagnostics"].to_string().contains(PARSER), "{out}");
    assert_eq!(
        (code, codes(&out)),
        (7, vec!["node.ambiguous".to_string()]),
        "{out}"
    );
    let (code, out) = run(&r.0, &["sync", "o"]);
    assert_eq!(code, 7, "{out}");
    // A title nobody has is no node (and no path).
    let (code, _) = run(&r.0, &["claim", "nothing like this"]);
    assert_eq!(code, 3);
}

#[test]
fn an_existing_path_stays_a_path_and_says_what_else_it_names() {
    let r = repo("paths");
    fs::create_dir_all(r.0.join("coordinator")).expect("mkdir");
    fs::write(r.0.join("coordinator/notes.txt"), "x").expect("write");
    git(&r.0, &["add", "-A"]);
    git(&r.0, &["commit", "-qm", "folder"]);
    let out = ok(&r.0, &["claim", "coordinator"]);
    assert_eq!(out["data"]["lease"]["scope"], "coordinator", "{out}");
    let warning = out["diagnostics"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|d| d["code"] == "node.ambiguous")
        .unwrap_or_else(|| panic!("a warning: {out}"));
    // A note in a 0.7 graph (RC1 F3); a 0.6 graph keeps the warning
    // (write_commands.rs).
    assert_eq!(warning["severity"], "info");
    assert!(
        warning["message"]
            .as_str()
            .unwrap_or_default()
            .contains(LEASES),
        "{out}"
    );
}

#[test]
fn write_commands_take_every_handle() {
    let r = repo("write");
    for h in HANDLES {
        // update sets and clears a temporary status.
        let out = ok(&r.0, &["update", h, "--status", "review"]);
        notes_name_the_node(&out, h, PARSER);
        ok(&r.0, &["update", h, "--clear"]);
        // next <id> follows the parser task's `next` edge.
        let out = ok(&r.0, &["next", h]);
        assert_eq!(out["data"]["node"]["id"], LEASES, "{h}: {out}");
        notes_name_the_node(&out, h, PARSER);
        let out = ok(&r.0, &["format", h]);
        notes_name_the_node(&out, h, PARSER);
    }
    // goal next by the goal's handles.
    for g in [GOAL, "0191", "ship the handles"] {
        let out = ok(&r.0, &["goal", "next", g]);
        assert_eq!(out["data"]["goal"]["id"], GOAL, "{g}: {out}");
    }
    let (code, out) = run(&r.0, &["update", "o", "--status", "review"]);
    assert_eq!(
        (code, codes(&out)),
        (7, vec!["node.ambiguous".to_string()]),
        "{out}"
    );
}

#[test]
fn done_takes_every_handle() {
    for (i, h) in HANDLES.iter().enumerate() {
        let r = repo(&format!("done-{i}"));
        let out = ok(&r.0, &["done", h]);
        assert_eq!(out["data"]["node"]["id"], PARSER, "{h}: {out}");
        assert_eq!(out["data"]["node"]["status"], "done", "{h}: {out}");
        notes_name_the_node(&out, h, PARSER);
    }
}

#[test]
fn an_internal_pick_is_claimed_exactly_whatever_files_are_named_like_it() {
    // goal claim picks a task and claims it by qid: a top-level file named
    // like that qid must not take the lease (Codex, Tier 1, 43cee7c).
    let r = repo("pick");
    for id in [PARSER, LEASES, MERKLE] {
        fs::write(r.0.join(format!("root:{id}")), "decoy").expect("decoy");
    }
    git(&r.0, &["add", "-A"]);
    git(&r.0, &["commit", "-qm", "decoys"]);
    let out = ok(&r.0, &["goal", "claim", GOAL]);
    let leases = out["data"]["leases"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let scopes: Vec<&str> = leases.iter().filter_map(|l| l["scope"].as_str()).collect();
    assert_eq!(scopes.len(), 1, "{out}");
    assert!(scopes[0].starts_with(".mdkg/work/task-"), "{out}");
}

#[test]
fn a_title_with_a_slash_sets_and_clears_a_status() {
    let r = repo("slash");
    fs::write(
        r.0.join(".mdkg/work/task-http2.md"),
        node(
            "task-01960000-0000-7000-8000-000000000006",
            "HTTP/2 support",
            &[&format!("parent: {GOAL}")],
        ),
    )
    .expect("node");
    git(&r.0, &["add", "-A"]);
    git(&r.0, &["commit", "-qm", "http2"]);
    ok(&r.0, &["update", "HTTP/2", "--status", "review"]);
    ok(&r.0, &["update", "HTTP/2", "--clear"]);
    let out = ok(&r.0, &["show", "HTTP/2", "--fields", "status"]);
    assert_eq!(out["data"]["node"]["status"], "todo", "{out}");
}

#[test]
fn retype_takes_handles_and_a_stale_id_still_names_the_node() {
    let r = repo("retype");
    let out = ok(&r.0, &["retype", "coordinator", "bug"]);
    let new_id = LEASES.replacen("task", "bug", 1);
    let named = out["diagnostics"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|d| d["code"] == "node.resolved");
    assert!(named, "{out}");
    // The old id still finds the node, by its UUID, and says which it is.
    let out = ok(&r.0, &["format", LEASES]);
    let message = out["diagnostics"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|d| d["code"] == "node.resolved")
        .and_then(|d| d["message"].as_str())
        .unwrap_or_default()
        .to_string();
    assert!(message.contains(&new_id), "{out}");
}

#[test]
fn a_legacy_id_wins_over_a_prefix_through_the_parser() {
    let r = repo("legacy");
    // A task migrated from task-1934, and a fresh one whose UUID starts
    // with 1934.
    let fresh = "task-1934abcd-0000-7000-8000-000000000007";
    let migrated = "task-01970000-0000-7000-8000-000000000008";
    let work = r.0.join(".mdkg/work");
    fs::write(
        work.join("task-fresh.md"),
        node(fresh, "Fresh work", &[&format!("parent: {GOAL}")]),
    )
    .expect("node");
    fs::write(
        work.join("task-migrated.md"),
        node(
            migrated,
            "Migrated work",
            &["legacy_id: task-1934", &format!("parent: {GOAL}")],
        ),
    )
    .expect("node");
    git(&r.0, &["add", "-A"]);
    git(&r.0, &["commit", "-qm", "legacy"]);
    let out = ok(&r.0, &["show", "task-1934", "--fields", "id"]);
    assert_eq!(out["data"]["node"]["id"], migrated, "{out}");
    let warning = out["diagnostics"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|d| d["code"] == "node.ambiguous")
        .unwrap_or_else(|| panic!("a warning: {out}"));
    assert_eq!(warning["severity"], "warning");
    assert!(
        warning["hint"].as_str().unwrap_or_default().contains(fresh),
        "{out}"
    );
    let out = ok(&r.0, &["claim", "task-1934"]);
    assert_eq!(
        out["data"]["lease"]["scope"], ".mdkg/work/task-migrated.md",
        "{out}"
    );
    let out = ok(&r.0, &["show", "task-1934a", "--fields", "id"]);
    assert_eq!(out["data"]["node"]["id"], fresh);
}

#[test]
fn pinned_and_graph_qualified_refs_resolve_in_graph_refs() {
    let r = repo("pins");
    let config: Value =
        serde_json::from_slice(&fs::read(r.0.join(".mdkg/config.json")).expect("config"))
            .expect("json");
    let graph = config["graph_id"].as_str().expect("graph_id").to_string();
    let pin = "b".repeat(64);
    fs::write(
        r.0.join(".mdkg/work/task-merkle.md"),
        node(
            MERKLE,
            "Merkle roots",
            &[
                &format!("parent: {GOAL}"),
                &format!("refs: [{PARSER}@sha256:{pin}, mdkg://{graph}/{LEASES}]"),
            ],
        ),
    )
    .expect("node");
    git(&r.0, &["add", "-A"]);
    git(&r.0, &["commit", "-qm", "pins"]);
    let out = ok(&r.0, &["graph", "refs", MERKLE]);
    let refs: Vec<&Value> = out["data"]["edges"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|e| e["kind"] == "refs")
        .collect();
    assert_eq!(refs.len(), 2, "{out}");
    assert!(refs.iter().all(|e| e["exists"] == true), "{out}");
    assert!(refs.iter().any(|e| e["pin"] == pin.as_str()), "{out}");
    let out = ok(&r.0, &["graph", "backrefs", PARSER]);
    assert!(
        out["data"]["edges"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|e| e["kind"] == "refs" && e["id"].as_str().is_some_and(|i| i.ends_with(MERKLE))),
        "{out}"
    );
}

#[test]
fn new_takes_handles_in_its_edge_flags_and_writes_full_ids() {
    let r = repo("new");
    let out = ok(
        &r.0,
        &[
            "new",
            "task",
            "Edges by handle",
            "--parent",
            "ship the handles",
            "--relates",
            "task-19,merkle",
            "--blocked-by",
            "0193",
            "--refs",
            "coordinator,https://example.com/doc",
        ],
    );
    let path = out["data"]["node"]["path"]
        .as_str()
        .expect("path")
        .to_string();
    let text = fs::read_to_string(r.0.join(&path)).expect("new file");
    assert!(text.contains(&format!("parent: {GOAL}")), "{text}");
    assert!(
        text.contains(&format!("relates: [{PARSER}, {MERKLE}]")),
        "{text}"
    );
    assert!(text.contains(&format!("blocked_by: [{LEASES}]")), "{text}");
    assert!(
        text.contains(&format!("refs: [{LEASES}, https://example.com/doc]")),
        "{text}"
    );
    // An ambiguous handle refuses, and nothing is written.
    let before = fs::read_dir(r.0.join(".mdkg/work")).expect("dir").count();
    for args in [["--parent", "er"], ["--relates", "er"], ["--refs", "er"]] {
        let mut cmd = vec!["new", "task", "Refused"];
        cmd.extend(args);
        let (code, out) = run(&r.0, &cmd);
        assert_eq!(
            (code, codes(&out)),
            (7, vec!["node.ambiguous".to_string()]),
            "{args:?}: {out}"
        );
    }
    assert_eq!(
        fs::read_dir(r.0.join(".mdkg/work")).expect("dir").count(),
        before
    );
    let (code, _) = run(
        &r.0,
        &["new", "task", "Nowhere", "--parent", "no such title"],
    );
    assert_eq!(code, 3);
}

#[test]
fn clearing_a_recorded_path_beats_a_title_that_spells_it() {
    // task-old's state is recorded at its path; then its file goes, and a
    // live node's title quotes that path (Codex, 4de5a8b).
    let r = repo("recorded");
    let old_id = "task-01980000-0000-7000-8000-000000000009";
    let old_path = ".mdkg/work/task-old.md";
    fs::write(
        r.0.join(old_path),
        node(old_id, "Old work", &[&format!("parent: {GOAL}")]),
    )
    .expect("node");
    git(&r.0, &["add", "-A"]);
    git(&r.0, &["commit", "-qm", "old"]);
    ok(&r.0, &["update", old_id, "--status", "review"]);
    fs::remove_file(r.0.join(old_path)).expect("rm");
    fs::write(
        r.0.join(".mdkg/work/task-cleanup.md"),
        node(
            "task-01990000-0000-7000-8000-00000000000a",
            "Clean up .mdkg/work/task-old.md references",
            &[&format!("parent: {GOAL}")],
        ),
    )
    .expect("node");
    git(&r.0, &["add", "-A"]);
    git(&r.0, &["commit", "-qm", "gone"]);
    ok(&r.0, &["update", "clean up", "--status", "review"]);
    ok(&r.0, &["update", old_path, "--clear"]);
    // The live node keeps its state; the recorded one is gone.
    let out = ok(&r.0, &["show", "clean up", "--fields", "status"]);
    assert_eq!(out["data"]["node"]["status"], "review", "{out}");
    let (code, out) = run(&r.0, &["update", old_path, "--clear"]);
    // Now no state is recorded there, so the title names the live node.
    assert_eq!(code, 0, "{out}");
    let out = ok(&r.0, &["show", "clean up", "--fields", "status"]);
    assert_eq!(out["data"]["node"]["status"], "todo", "{out}");
}

/// A 0.6.0 graph (the conformance base) with two tasks whose titles
/// overlap, committed.
fn legacy_repo(name: &str) -> Scratch {
    let s = scratch(name);
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance/base");
    copy_tree(&base, &s.0);
    let work = s.0.join(".mdkg/work");
    fs::create_dir_all(&work).expect("work dir");
    let legacy = |id: &str, title: &str| {
        format!(
            "---\nid: {id}\ntype: task\ntitle: {title}\nstatus: todo\ncreated: 2026-10-24\nupdated: 2026-10-24\n---\n# Overview\n\n{title}.\n"
        )
    };
    fs::write(work.join("task-1-parser.md"), legacy("task-1", "Parser")).expect("node");
    fs::write(
        work.join("task-2-tests.md"),
        legacy("task-2", "Parser tests"),
    )
    .expect("node");
    git(&s.0, &["init", "-q", "-b", "main"]);
    git(&s.0, &["add", "-A"]);
    git(&s.0, &["commit", "-qm", "init"]);
    s
}

fn copy_tree(src: &Path, dest: &Path) {
    fs::create_dir_all(dest).expect("mkdir");
    for entry in fs::read_dir(src).expect("read dir") {
        let entry = entry.expect("entry");
        let target = dest.join(entry.file_name());
        if entry.file_type().expect("type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).expect("copy");
        }
    }
}

#[test]
fn refs_and_scope_take_handles_on_a_0_6_0_graph_too() {
    let r = legacy_repo("legacy-refs");
    let (code, out) = run(&r.0, &["new", "task", "X", "--refs", "par"]);
    assert_eq!(
        (code, codes(&out)),
        (7, vec!["node.ambiguous".to_string()]),
        "{out}"
    );
    let out = ok(
        &r.0,
        &[
            "new",
            "task",
            "Y",
            "--refs",
            "parser tests,https://example.com/a",
        ],
    );
    let path = out["data"]["node"]["path"]
        .as_str()
        .expect("path")
        .to_string();
    let text = fs::read_to_string(r.0.join(&path)).expect("file");
    assert!(
        text.contains("refs: [root:task-2, https://example.com/a]"),
        "{text}"
    );
    let resolved: Vec<&Value> = out["diagnostics"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|d| d["code"] == "node.resolved")
        .collect();
    assert_eq!(resolved.len(), 1, "said once: {out}");
    let out = ok(&r.0, &["new", "checkpoint", "Z", "--scope", "parser tests"]);
    let path = out["data"]["node"]["path"]
        .as_str()
        .expect("path")
        .to_string();
    let text = fs::read_to_string(r.0.join(&path)).expect("file");
    assert!(text.contains("scope: [task-2]"), "{text}");
}

#[test]
fn a_title_holding_uri_syntax_is_still_a_handle() {
    let r = repo("uri-title");
    fs::write(
        r.0.join(".mdkg/work/task-links.md"),
        node(
            "task-019a0000-0000-7000-8000-00000000000b",
            "Support https:// links",
            &[&format!("parent: {GOAL}")],
        ),
    )
    .expect("node");
    git(&r.0, &["add", "-A"]);
    git(&r.0, &["commit", "-qm", "links"]);
    let out = ok(
        &r.0,
        &[
            "new",
            "checkpoint",
            "C",
            "--scope",
            "Support https:// links",
        ],
    );
    let path = out["data"]["node"]["path"]
        .as_str()
        .expect("path")
        .to_string();
    let text = fs::read_to_string(r.0.join(&path)).expect("file");
    assert!(
        text.contains("task-019a0000-0000-7000-8000-00000000000b"),
        "{text}"
    );
    let out = ok(
        &r.0,
        &["new", "task", "D", "--refs", "Support https:// links"],
    );
    let path = out["data"]["node"]["path"]
        .as_str()
        .expect("path")
        .to_string();
    let text = fs::read_to_string(r.0.join(&path)).expect("file");
    assert!(
        text.contains("refs: [task-019a0000-0000-7000-8000-00000000000b]"),
        "{text}"
    );
}

#[test]
fn a_recorded_path_beats_two_titles_and_a_refused_path_falls_back_to_the_title() {
    let r = repo("recorded-two");
    let old_id = "task-019b0000-0000-7000-8000-00000000000c";
    let old_path = ".mdkg/work/task-old.md";
    fs::write(
        r.0.join(old_path),
        node(old_id, "Old work", &[&format!("parent: {GOAL}")]),
    )
    .expect("node");
    git(&r.0, &["add", "-A"]);
    git(&r.0, &["commit", "-qm", "old"]);
    ok(&r.0, &["update", old_id, "--status", "review"]);
    fs::remove_file(r.0.join(old_path)).expect("rm");
    for (i, title) in [
        "Clean up .mdkg/work/task-old.md references",
        "Audit .mdkg/work/task-old.md history",
    ]
    .iter()
    .enumerate()
    {
        fs::write(
            r.0.join(format!(".mdkg/work/task-quote-{i}.md")),
            node(
                &format!("task-019c0000-0000-7000-8000-00000000000{i}"),
                title,
                &[&format!("parent: {GOAL}")],
            ),
        )
        .expect("node");
    }
    // A title the path refuses to be (a turn's folder).
    fs::write(
        r.0.join(".mdkg/work/task-runlog.md"),
        node(
            "task-019d0000-0000-7000-8000-00000000000d",
            "run:worker/log",
            &[&format!("parent: {GOAL}")],
        ),
    )
    .expect("node");
    git(&r.0, &["add", "-A"]);
    git(&r.0, &["commit", "-qm", "quotes"]);
    // The recorded state wins over both titles; then, with none left,
    // the two titles are ambiguous.
    ok(&r.0, &["update", old_path, "--clear"]);
    let (code, out) = run(&r.0, &["update", old_path, "--clear"]);
    assert_eq!(
        (code, codes(&out)),
        (7, vec!["node.ambiguous".to_string()]),
        "{out}"
    );
    // A title the path reading refuses still names its node.
    ok(&r.0, &["update", "run:worker/log", "--status", "review"]);
    ok(&r.0, &["update", "run:worker/log", "--clear"]);
    let out = ok(&r.0, &["show", "run:worker/log", "--fields", "status"]);
    assert_eq!(out["data"]["node"]["status"], "todo", "{out}");
}

#[test]
fn a_goal_blocking_its_own_task_neither_hides_it_nor_hides_the_edge() {
    // ORR's shape (task-41 check on the migrated graph): the goal's
    // `blocks` lists its own child task. `next` still offers the task,
    // validate warns about the edge, and with two active goals
    // critical-path names them by bare id.
    let r = repo("ancestor");
    let goal2 = "goal-019e0000-0000-7000-8000-00000000000e";
    let lone = "task-019f0000-0000-7000-8000-00000000000f";
    let goal_fields = |scope: &str, extra: &str| {
        vec![
            "goal_state: active".to_string(),
            "goal_condition: the lone task ships".to_string(),
            format!("scope_refs: [{scope}]"),
            "required_skills: []".to_string(),
            "required_checks: []".to_string(),
            "max_iterations: 25".to_string(),
            "blocked_after_attempts: 3".to_string(),
            extra.to_string(),
        ]
    };
    let fields = goal_fields(lone, &format!("blocks: [{lone}]"));
    let refs: Vec<&str> = fields.iter().map(String::as_str).collect();
    let work = r.0.join(".mdkg/work");
    fs::write(
        work.join("goal-lone.md"),
        node(goal2, "Ship the lone task", &refs),
    )
    .expect("goal");
    fs::write(
        work.join("task-lone.md"),
        node(
            lone,
            "The lone task",
            &[&format!("parent: {goal2}"), "priority: 0"],
        ),
    )
    .expect("task");
    git(&r.0, &["add", "-A"]);
    git(&r.0, &["commit", "-qm", "lone"]);
    let out = ok(&r.0, &["next"]);
    assert_eq!(out["data"]["node"]["id"], lone, "{out}");
    let (code, out) = run(&r.0, &["validate"]);
    assert_eq!(code, 0, "{out}");
    let warning = out["diagnostics"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|d| d["code"] == "edge.blocks_ancestor")
        .unwrap_or_else(|| panic!("a warning: {out}"));
    assert_eq!(warning["severity"], "warning");
    assert!(
        warning["message"]
            .as_str()
            .unwrap_or_default()
            .contains(lone),
        "{out}"
    );
    let (code, out) = run(&r.0, &["graph", "critical-path"]);
    assert_eq!(code, 1, "{out}");
    let hint = out["diagnostics"][0]["hint"].as_str().unwrap_or_default();
    assert!(hint.contains(GOAL) && hint.contains(goal2), "{out}");
    assert!(!hint.contains("root:"), "bare ids: {out}");
}

#[test]
fn a_path_that_also_names_a_node_is_only_a_note() {
    // A fresh agent (RC1 F3): `claim <path>` whose text also matches a
    // title warned node.ambiguous with a `root:` id. A 0.7 id can't be
    // mistaken, so it's a note naming the bare id; the path is leased.
    let r = repo("path-note");
    fs::write(r.0.join("merkle"), "notes\n").expect("file");
    let out = ok(&r.0, &["claim", "merkle"]);
    assert_eq!(out["data"]["lease"]["scope"], "merkle", "{out}");
    let note = out["diagnostics"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|d| d["code"] == "node.ambiguous")
        .cloned()
        .unwrap_or_default();
    assert_eq!(note["severity"], "info", "{out}");
    let text = format!("{} {}", note["message"], note["hint"]);
    assert!(text.contains(MERKLE) && !text.contains("root:"), "{out}");
}

#[test]
fn pack_shows_bare_ids() {
    // RC1 F3: pack's markdown names nodes as the rest of a 0.7 graph's
    // output does, without the workspace.
    let r = repo("pack-ids");
    let out = ok(&r.0, &["pack", PARSER]);
    let markdown = out["data"]["markdown"].as_str().unwrap_or_default();
    assert!(
        markdown.contains(&format!("## {PARSER} (task-19)")),
        "{markdown}"
    );
    assert!(markdown.contains(&format!("parent: {GOAL}")), "{markdown}");
    assert!(!markdown.contains("root:"), "{markdown}");
}
