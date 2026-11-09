//! The derived index through the binary (task-41): `search`, `index`,
//! `graph`, `pack`, and `next` on the ready set, in throwaway repositories.

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
    let dir = common::temp::dir(&format!("mdkg-cli-index-{name}"));
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

fn node(id: &str, node_type: &str, title: &str, extra: &[&str], body: &str) -> String {
    let mut lines = vec![
        "---".to_string(),
        format!("id: {id}"),
        format!("type: {node_type}"),
        format!("title: {title}"),
    ];
    lines.push("status: todo".into());
    lines.extend(extra.iter().map(|s| s.to_string()));
    lines.extend([
        "created: 2026-10-24".into(),
        "updated: 2026-10-24".into(),
        "---".into(),
    ]);
    format!("{}\n{body}", lines.join("\n"))
}

fn write(root: &Path, file: &str, text: &str) {
    let p = root.join(".mdkg/work").join(file);
    fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
    fs::write(p, text).expect("write");
}

/// A repository with a goal and four tasks, committed.
fn repo(name: &str) -> Scratch {
    let s = scratch(name);
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance/base");
    copy_tree(&base, &s.0);
    write(
        &s.0,
        "goal-1.md",
        &node(
            "goal-1",
            "goal",
            "Ship the index",
            &[
                "goal_state: active",
                "goal_condition: the index ships",
                "scope_refs: []",
                "required_skills: []",
                "required_checks: []",
                "max_iterations: 25",
                "blocked_after_attempts: 3",
            ],
            "Goal.\n",
        ),
    );
    write(
        &s.0,
        "task-1.md",
        &node(
            "task-1",
            "task",
            "Parser for frontmatter",
            &["parent: goal-1", "tags: [format]", "priority: 1"],
            "# Overview\n\nReads keys.\n",
        ),
    );
    write(
        &s.0,
        "task-2.md",
        &node(
            "task-2",
            "task",
            "Coordinator leases",
            &[
                "parent: goal-1",
                "owners: [nick]",
                "blocked_by: [task-1]",
                "priority: 1",
            ],
            "# Overview\n\nThe word frontmatter appears only in this body.\n",
        ),
    );
    write(
        &s.0,
        "task-3.md",
        &node(
            "task-3",
            "task",
            "Merkle roots",
            &["parent: goal-1", "blocked_by: [task-2]"],
            "# Overview\n\nHashes.\n",
        ),
    );
    write(
        &s.0,
        "task-4.md",
        &node(
            "task-4",
            "task",
            "Merkle proofs",
            &["parent: goal-1", "priority: 5"],
            "Proofs.\n",
        ),
    );
    git(&s.0, &["init", "-q", "-b", "main"]);
    git(&s.0, &["add", "-A"]);
    git(&s.0, &["commit", "-qm", "init"]);
    s
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

fn ids(nodes: &Value) -> Vec<String> {
    nodes
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|n| n["id"].as_str().map(String::from))
        .collect()
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
fn search_ranks_bodies_too_and_follows_edits() {
    let r = repo("search");
    let out = ok(&r.0, &["search", "frontmatter"]);
    assert_eq!(
        ids(&out["data"]["nodes"]),
        ["task-1", "task-2"],
        "title above body"
    );
    assert_eq!(out["data"]["count"], 2);
    let first = &out["data"]["nodes"][0];
    let second = &out["data"]["nodes"][1];
    assert!(first["score"].as_f64() > second["score"].as_f64());
    assert!(
        second["snippet"]
            .as_str()
            .is_some_and(|s| s.contains("**frontmatter**")),
        "{second}"
    );
    assert!(codes(&out).is_empty(), "{out}");

    // Filters, as list's, plus owners.
    let out = ok(&r.0, &["search", "frontmatter", "--owners", "NICK"]);
    assert_eq!(ids(&out["data"]["nodes"]), ["task-2"]);
    let out = ok(&r.0, &["search", "merkle", "--limit", "1"]);
    assert_eq!(
        (
            out["data"]["count"].as_u64(),
            out["data"]["truncated"].as_bool()
        ),
        (Some(2), Some(true))
    );
    let out = ok(&r.0, &["search", "merkle", "--type", "goal"]);
    assert_eq!(out["data"]["count"], 0);

    // An edit shows up without a rebuild.
    write(
        &r.0,
        "task-4.md",
        &node(
            "task-4",
            "task",
            "Merkle proofs",
            &["parent: goal-1"],
            "Now about frontmatter.\n",
        ),
    );
    let out = ok(&r.0, &["search", "frontmatter"]);
    assert_eq!(out["data"]["count"], 3);

    let (code, out) = run(&r.0, &["search", "--owners", ",", "x"]);
    assert_eq!(code, 1, "{out}");
}

#[test]
fn the_index_lives_beside_the_store_one_per_worktree() {
    let r = repo("place");
    let out = ok(&r.0, &["index", "status"]);
    let path = out["data"]["path"].as_str().expect("a file").to_string();
    assert!(
        path.starts_with(&r.0.join(".git/mdkg/index").display().to_string()),
        "{path}"
    );
    assert!(Path::new(&path).is_file());
    assert_eq!(out["data"]["index"]["nodes"], 5);
    assert_eq!(out["data"]["sync"]["added"], 5);
    let out = ok(&r.0, &["index", "status"]);
    assert_eq!(out["data"]["sync"]["unchanged"], 5);

    let out = ok(&r.0, &["index", "rebuild"]);
    assert_eq!(out["data"]["sync"]["added"], 5);
    assert_eq!(out["data"]["sync"]["rebuilt"], true);

    // Another worktree keeps its own file in the same folder.
    let wt = r.0.join("wt2");
    git(
        &r.0,
        &[
            "worktree",
            "add",
            "-q",
            wt.to_str().expect("utf8"),
            "-b",
            "side",
        ],
    );
    let other = ok(&wt, &["index", "status"]);
    let other_path = other["data"]["path"].as_str().expect("a file");
    assert_ne!(other_path, path);
    assert_eq!(Path::new(other_path).parent(), Path::new(&path).parent());

    // The index is never committed: nothing in the worktree changed.
    let status = isolated(
        Command::new("git")
            .arg("-C")
            .arg(&r.0)
            .args(["status", "--porcelain"]),
    )
    .output()
    .expect("git status");
    let changed = String::from_utf8_lossy(&status.stdout).to_string();
    assert!(changed.lines().all(|l| l.ends_with("wt2/")), "{changed}");
}

#[test]
fn an_index_that_cant_be_kept_is_built_in_memory() {
    let r = repo("memory");
    fs::create_dir_all(r.0.join(".git/mdkg")).expect("mkdir");
    // A file where the folder goes: the folder can't be made.
    fs::write(r.0.join(".git/mdkg/index"), b"not a folder").expect("block");
    let out = ok(&r.0, &["search", "merkle"]);
    assert_eq!(out["data"]["count"], 2);
    assert_eq!(codes(&out), ["io.write_failed"]);

    // Outside a repository there's nowhere to keep it, and nothing to say.
    let plain = scratch("plain");
    copy_tree(&r.0.join(".mdkg"), &plain.0.join(".mdkg"));
    let out = ok(&plain.0, &["search", "merkle"]);
    assert_eq!(out["data"]["count"], 2);
    assert!(codes(&out).is_empty(), "{out}");
    let out = ok(&plain.0, &["index", "status"]);
    assert!(out["data"].get("path").is_none());
}

#[test]
fn graph_queries_and_next_use_the_ready_set() {
    let r = repo("graph");
    let out = ok(&r.0, &["graph", "ready"]);
    assert_eq!(ids(&out["data"]["nodes"]), ["task-1", "task-4"]);
    assert_eq!(out["data"]["count"], 2);

    let out = ok(&r.0, &["graph", "critical-path"]);
    assert_eq!(out["schema"], "mdkg.graph.critical-path.v1");
    assert_eq!(out["data"]["goal"]["id"], "goal-1");
    assert_eq!(ids(&out["data"]["nodes"]), ["task-1", "task-2", "task-3"]);
    assert_eq!(out["data"]["remaining"], 4);
    assert_eq!(out["data"]["cycle"], serde_json::json!([]));
    // A node that isn't a goal (D2): goal.not_goal, without its id.
    let (code, out) = run(&r.0, &["graph", "critical-path", "task-1"]);
    assert_eq!(
        (code, codes(&out)),
        (1, vec!["goal.not_goal".to_string()]),
        "{out}"
    );
    let message = out["diagnostics"][0]["message"]
        .as_str()
        .unwrap_or_default();
    assert!(!message.contains("task-1"), "{out}");
    // A limit that isn't positive (D2): usage.bad_limit.
    for limit in [&["--limit", "0"][..], &["--limit", "-1"], &["--limit=-1"]] {
        let args: Vec<&str> = ["graph", "ready"].iter().chain(limit).copied().collect();
        let (code, out) = run(&r.0, &args);
        assert_eq!(
            (code, codes(&out)),
            (1, vec!["usage.bad_limit".to_string()]),
            "{limit:?}: {out}"
        );
        assert_eq!(out["schema"], "mdkg.graph.ready.v1");
    }
    // The same for search's.
    let (code, out) = run(&r.0, &["search", "merkle", "--limit", "-1"]);
    assert_eq!(
        (code, codes(&out)),
        (1, vec!["usage.bad_limit".to_string()]),
        "{out}"
    );

    let out = ok(&r.0, &["graph", "backrefs", "task-1"]);
    let edges = out["data"]["edges"].as_array().expect("edges");
    assert_eq!(edges.len(), 1);
    assert_eq!(
        (edges[0]["kind"].as_str(), edges[0]["id"].as_str()),
        (Some("blocked_by"), Some("task-2"))
    );
    let out = ok(&r.0, &["graph", "refs", "coordinator"]);
    assert_eq!(out["data"]["node"]["id"], "task-2", "a title fragment");
    assert_eq!(out["data"]["edges"].as_array().map(Vec::len), Some(2));

    // `show` and `list --epic` take handles too.
    let out = ok(&r.0, &["show", "coordinator", "--fields", "id"]);
    assert_eq!(out["data"]["node"]["id"], "task-2");
    let (code, out) = run(&r.0, &["show", "merkle"]);
    assert_eq!(
        (code, codes(&out)),
        (7, vec!["node.ambiguous".to_string()]),
        "{out}"
    );
    let (code, out) = run(&r.0, &["show", "no such title"]);
    assert_eq!(
        (code, codes(&out)),
        (3, vec!["node.not_found".to_string()]),
        "{out}"
    );

    // An ambiguous handle lists its matches.
    let (code, out) = run(&r.0, &["graph", "refs", "merkle"]);
    assert_eq!(code, 7, "{out}");
    assert_eq!(codes(&out), ["node.ambiguous"]);
    let hint = out["diagnostics"][0]["hint"].as_str().unwrap_or_default();
    assert!(
        hint.contains("Merkle proofs") && hint.contains("Merkle roots"),
        "{hint}"
    );

    // `next` skips task-2 and task-3, blocked by unfinished work.
    let out = ok(&r.0, &["next"]);
    assert_eq!(out["data"]["node"]["id"], "task-1");
    write(
        &r.0,
        "task-1.md",
        &node(
            "task-1",
            "task",
            "Parser for frontmatter",
            &["parent: goal-1", "priority: 9"],
            "Reads.\n",
        ),
    );
    let out = ok(&r.0, &["next"]);
    assert_eq!(
        out["data"]["node"]["id"], "task-4",
        "task-2 still waits on task-1"
    );
}

#[test]
fn packs_fit_the_budget() {
    let r = repo("pack");
    let out = ok(&r.0, &["pack", "task-2", "--budget", "4000"]);
    let nodes = out["data"]["nodes"].as_array().expect("nodes");
    assert_eq!(nodes[0]["qid"], "root:task-2");
    assert!(nodes.len() >= 3, "{out}");
    let md = out["data"]["markdown"].as_str().expect("markdown");
    assert!(md.starts_with("# mdkg pack"));
    assert!(md.contains("## root:task-2"));
    assert!(out["data"]["tokens"].as_u64() <= Some(4000));

    let out = ok(&r.0, &["pack", "--query", "merkle", "--profile", "concise"]);
    let first = out["data"]["nodes"][0]["qid"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(first == "root:task-3" || first == "root:task-4", "{out}");

    let (code, _) = run(&r.0, &["pack"]);
    assert_eq!(code, 1);
    let (code, _) = run(&r.0, &["pack", "task-1", "--profile", "huge"]);
    assert_eq!(code, 1);
}

#[test]
fn a_common_word_still_scores_above_zero() {
    // RC1 F6: BM25 gives a word in most nodes an idf of 1e-6, which three
    // decimals showed as 0.0 for every hit. Six significant digits keep
    // the real score, and its order.
    let r = repo("common-word");
    let out = ok(&r.0, &["search", "overview"]);
    let scores: Vec<f64> = out["data"]["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|n| n["score"].as_f64())
        .collect();
    assert_eq!(scores.len(), 3, "{out}");
    assert!(scores.iter().all(|s| *s > 0.0), "{out}");
    assert!(scores.windows(2).all(|w| w[0] >= w[1]), "{out}");
}

#[test]
fn message_pack_reuses_budget_for_thread_bound_nodes_requirements_and_decisions() {
    let r = scratch("message-pack");
    fs::create_dir_all(r.0.join(".mdkg/work")).unwrap();
    fs::create_dir_all(r.0.join(".mdkg/design")).unwrap();
    fs::write(r.0.join(".mdkg/config.json"), serde_json::json!({
        "schema_version":3, "format_version":"0.7", "graph_id":mdkg_format::Uuid7::now().to_string(),
        "work":{"location":"git"}
    }).to_string()).unwrap();
    let task = format!("task-{}", mdkg_format::Uuid7::now());
    let other = format!("task-{}", mdkg_format::Uuid7::now());
    let accepted = format!("dec-{}", mdkg_format::Uuid7::now());
    let proposed = format!("dec-{}", mdkg_format::Uuid7::now());
    write(
        &r.0,
        "target.md",
        &node(
            &task,
            "task",
            "Target",
            &[],
            "# Overview\n\nTarget opening.\n\n# Requirements\n\n- R1: The output shall retain requirements. (verify: test)\n",
        ),
    );
    write(
        &r.0,
        "other.md",
        &node(&other, "task", "Unrelated", &[], "Unrelated body."),
    );
    for (id, status) in [(&accepted, "accepted"), (&proposed, "proposed")] {
        let text = node(
            id,
            "dec",
            "Choice",
            &[&format!("refs: [{task}]")],
            "Accepted decision body.",
        )
        .replace("status: todo", &format!("status: {status}"));
        fs::write(r.0.join(format!(".mdkg/design/{status}.md")), text).unwrap();
    }
    git(&r.0, &["init", "-q", "-b", "main"]);
    git(&r.0, &["add", "."]);
    git(&r.0, &["commit", "-qm", "fixture"]);
    ok(
        &r.0,
        &[
            "msg",
            "send",
            "--to",
            "worker",
            "--kind",
            "question",
            "--id",
            "trigger",
            "--node",
            &task,
            "--text",
            "Trigger body.",
        ],
    );
    ok(
        &r.0,
        &[
            "msg",
            "send",
            "--to",
            "worker",
            "--kind",
            "note",
            "--id",
            "followup",
            "--reply-to",
            "trigger",
            "--text",
            "Thread reply.",
        ],
    );
    let packed = ok(
        &r.0,
        &["pack", "--for-message", "trigger", "--budget", "4000"],
    );
    let d = &packed["data"];
    let qids: Vec<_> = d["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["qid"].as_str().unwrap())
        .collect();
    assert_eq!(qids.len(), 2, "{packed}");
    assert!(qids.iter().any(|q| q.ends_with(&task)));
    assert!(qids.iter().any(|q| q.ends_with(&accepted)));
    let md = d["markdown"].as_str().unwrap();
    for content in [
        "Trigger body.",
        "Thread reply.",
        "R1: The output shall retain requirements",
        "Accepted decision body.",
    ] {
        assert!(md.contains(content), "{md}");
    }
    assert!(!md.contains("Unrelated body."));
    assert_eq!(d["requirements"][0]["requirements"][0]["id"], "R1");
    assert_eq!(d["message"]["messages"], 2);
    // Neither pack nor pending silently marks mail shown.
    let store = mdkg_coord_sqlite::git::store_path(&r.0).unwrap();
    let conn = rusqlite::Connection::open(store).unwrap();
    let shown: i64 = conn
        .query_row(
            "SELECT count(*) FROM msg_deliveries WHERE shown_ms IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(shown, 0);
    // Expiry masks the body, but send-time UUID bindings still seed the pack.
    conn.execute(
        "UPDATE msg_messages SET retention='ttl:1', purge_after_ms=0 WHERE message_id='trigger'",
        [],
    )
    .unwrap();
    let expired = ok(&r.0, &["pack", "--for-message", "trigger"]);
    assert_eq!(expired["data"]["nodes"].as_array().unwrap().len(), 2);
    assert!(
        expired["data"]["markdown"]
            .as_str()
            .unwrap()
            .contains("(body purged)")
    );
    drop(conn);
    for budget in [64, 128, 256, 500, 1000] {
        let packed = ok(
            &r.0,
            &[
                "pack",
                "--for-message",
                "trigger",
                "--budget",
                &budget.to_string(),
                "--profile",
                "concise",
            ],
        );
        let d = &packed["data"];
        let md = d["markdown"].as_str().unwrap();
        assert!(md.len().div_ceil(4) <= budget, "{packed}");
        assert_eq!(d["tokens"].as_u64().unwrap() as usize, md.len().div_ceil(4));
    }
    fs::write(
        r.0.join("part.json"),
        serde_json::json!({"target":task}).to_string(),
    )
    .unwrap();
    ok(
        &r.0,
        &[
            "msg",
            "send",
            "--to",
            "worker",
            "--kind",
            "question",
            "--id",
            "data-ref",
            "--data-file",
            r.0.join("part.json").to_str().unwrap(),
        ],
    );
    let data = ok(&r.0, &["pack", "--for-message", "data-ref"]);
    assert_eq!(data["data"]["nodes"].as_array().unwrap().len(), 2, "{data}");
    // A message with no node bindings still produces a useful thread pack.
    ok(
        &r.0,
        &[
            "msg",
            "send",
            "--to",
            "worker",
            "--kind",
            "note",
            "--id",
            "plain",
            "--text",
            "Plain thread body.",
        ],
    );
    let plain = ok(&r.0, &["pack", "--for-message", "plain"]);
    assert!(plain["data"]["nodes"].as_array().unwrap().is_empty());
    assert!(
        plain["data"]["markdown"]
            .as_str()
            .unwrap()
            .contains("Plain thread body.")
    );
    ok(
        &r.0,
        &[
            "msg",
            "send",
            "--to",
            "worker",
            "--kind",
            "note",
            "--id",
            "long",
            "--text",
            &"é long thread line.\n".repeat(1000),
        ],
    );
    let long = ok(&r.0, &["pack", "--for-message", "long", "--budget", "128"]);
    assert_eq!(long["data"]["message"]["cut"], true);
    assert!(long["data"]["tokens"].as_u64().unwrap() <= 128);
    assert_ne!(run(&r.0, &["pack", "--for-message", "missing"]).0, 0);
}

#[test]
fn message_pack_resolves_text_and_markdown_references_with_concise_requirements() {
    let r = repo("message-refs");
    for id in ["task-1", "task-2"] {
        write(
            &r.0,
            &format!("{id}.md"),
            &node(
                id,
                "task",
                "Referenced",
                &[],
                "# Overview\n\nOpening.\n\n# Detail\n\nDetails.\n\n# Requirements\n\n- R1: The output shall include the tail requirement. (verify: test)\n",
            ),
        );
    }
    write(
        &r.0,
        "dec-1.md",
        &node("dec-1", "dec", "Accepted", &["refs: [task-2]"], "Decision.")
            .replace("status: todo", "status: accepted"),
    );
    ok(
        &r.0,
        &[
            "msg",
            "send",
            "--to",
            "worker",
            "--kind",
            "question",
            "--id",
            "text-refs",
            "--text",
            "See `task-1`. Also [the second task](.mdkg/work/task-2.md#R1).",
        ],
    );
    let packed = ok(
        &r.0,
        &["pack", "--for-message", "text-refs", "--profile", "concise"],
    );
    let d = &packed["data"];
    assert_eq!(d["nodes"].as_array().unwrap().len(), 3, "{packed}");
    assert_eq!(d["requirements"].as_array().unwrap().len(), 2, "{packed}");
    assert!(
        d["markdown"]
            .as_str()
            .unwrap()
            .matches("tail requirement")
            .count()
            >= 2,
        "{packed}"
    );
}

#[test]
fn message_pack_channel_scope_excludes_chatter_and_sibling_node_seeds() {
    let r = repo("channel-pack");
    ok(&r.0, &["channel", "create", "dev", "--member", "worker"]);
    for n in 0..20 {
        ok(
            &r.0,
            &[
                "msg",
                "send",
                "--channel",
                "dev",
                "--kind",
                "note",
                "--id",
                &format!("noise-{n}"),
                "--text",
                &"Old unrelated task-4 chatter.\n".repeat(100),
            ],
        );
    }
    for (id, parent, target) in [
        ("parent", None, "task-1"),
        ("trigger", Some("parent"), "task-2"),
        ("sibling", Some("parent"), "task-3"),
        ("child", Some("trigger"), "task-2"),
    ] {
        let mut args = vec![
            "msg",
            "send",
            "--channel",
            "dev",
            "--kind",
            "question",
            "--id",
            id,
            "--text",
            target,
        ];
        if let Some(parent) = parent {
            args.extend(["--reply-to", parent]);
        }
        ok(&r.0, &args);
    }
    ok(
        &r.0,
        &[
            "msg",
            "send",
            "--to",
            "worker",
            "--kind",
            "note",
            "--id",
            "cross-context",
            "--reply-to",
            "child",
            "--text",
            "Cross-context answer.",
        ],
    );
    let packed = ok(
        &r.0,
        &["pack", "--for-message", "trigger", "--budget", "4000"],
    );
    let d = &packed["data"];
    assert_eq!(d["message"]["messages"], 4, "{packed}");
    let mut qids: Vec<_> = d["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["qid"].as_str().unwrap())
        .collect();
    qids.sort_unstable();
    assert_eq!(qids, ["root:task-1", "root:task-2"], "{packed}");
    let md = d["markdown"].as_str().unwrap();
    assert!(md.contains("Cross-context answer."), "{md}");
    assert!(!md.contains("Old unrelated"), "{md}");
    assert!(!md.contains("sibling"), "{md}");
    // More than one read page, but the smallest pack still reads a bounded
    // body prefix and reports the complete selected header count.
    for n in 0..20 {
        ok(
            &r.0,
            &[
                "msg",
                "send",
                "--channel",
                "dev",
                "--kind",
                "note",
                "--id",
                &format!("reply-{n}"),
                "--reply-to",
                "trigger",
                "--text",
                &"Reply detail.\n".repeat(100),
            ],
        );
    }
    let small = ok(
        &r.0,
        &["pack", "--for-message", "trigger", "--budget", "128"],
    );
    assert_eq!(small["data"]["message"]["messages"], 24);
    assert_eq!(small["data"]["message"]["cut"], true);
    assert!(small["data"]["tokens"].as_u64().unwrap() <= 128);
}
