//! `mdkg req list` and the requirements in `pack` (a-flow edd, A28), in a
//! temporary repository.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

mod common;

const BIN: &str = env!("CARGO_BIN_EXE_mdkg");

fn repo(name: &str) -> PathBuf {
    let dir = common::temp::dir(&format!("mdkg-req-{name}"));
    let dir = std::fs::canonicalize(&dir).expect("canonical");
    git(&dir, &["init", "-q", "-b", "main"]);
    dir
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

fn json(dir: &Path, args: &[&str]) -> (i32, Value) {
    let out = common::without_launcher_env(&mut Command::new(BIN))
        .args(args)
        .current_dir(dir)
        .env("MDKG_ACTOR", "tester")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .expect("run the binary");
    let v: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|_| {
        panic!(
            "no JSON from {args:?}: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    });
    (out.status.code().unwrap_or(-1), v)
}

fn codes(v: &Value) -> Vec<&str> {
    v["diagnostics"]
        .as_array()
        .map(|a| a.iter().filter_map(|d| d["code"].as_str()).collect())
        .unwrap_or_default()
}

const BODY: &str = "# Overview\n\nA task.\n\n# Requirements\n\n\
- R1 (changes edd-0192f3a4-0000-7000-8000-000000000010#R3) (adds prd-0192f3a4-0000-7000-8000-000000000011): \
When a client asks, the service shall answer within 2 seconds. (verify: a timed request)\n\
- R2 (retired): The old cache shall stay warm.\n";

/// A task whose body holds two requirements, committed; its id.
fn task_with_requirements(dir: &Path) -> String {
    task_with_body(dir, BODY)
}

/// A task with `body` after its frontmatter, committed; its id.
fn task_with_body(dir: &Path, body: &str) -> String {
    assert_eq!(json(dir, &["init", "--work", "shared", "--json"]).0, 0);
    let (code, out) = json(dir, &["new", "task", "Answer quickly", "--json"]);
    assert_eq!(code, 0, "{out}");
    let id = out["data"]["node"]["id"].as_str().expect("id").to_string();
    let path = dir.join(out["data"]["node"]["path"].as_str().expect("path"));
    let text = std::fs::read_to_string(&path).expect("node");
    let end = text[4..].find("\n---\n").expect("frontmatter") + 4 + 5;
    std::fs::write(&path, format!("{}{body}", &text[..end])).expect("write node");
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", "init"]);
    id
}

fn check_items(items: &Value, state: Option<&str>) {
    let items = items.as_array().expect("requirements");
    assert_eq!(items.len(), 2, "{items:?}");
    let r1 = &items[0];
    assert_eq!(r1["id"], "R1");
    assert_eq!(r1["ears"], "event_driven");
    assert_eq!(
        r1["relations"],
        serde_json::json!([
            {"kind": "changes", "node": "edd-0192f3a4-0000-7000-8000-000000000010", "requirement": "R3"},
            {"kind": "adds", "node": "prd-0192f3a4-0000-7000-8000-000000000011", "requirement": null},
        ])
    );
    assert_eq!(
        r1["statement"],
        "When a client asks, the service shall answer within 2 seconds."
    );
    assert_eq!(r1["verify"], "a timed request");
    assert_eq!(r1["retired"], false);
    assert_eq!(r1["line"], 7);
    assert_eq!(r1["state"].as_str(), state);
    let r2 = &items[1];
    assert_eq!(r2["id"], "R2");
    assert_eq!(r2["retired"], true);
    assert_eq!(r2["relations"], serde_json::json!([]));
    assert_eq!(r2["state"], Value::Null);
}

#[test]
fn req_list_and_pack_report_requirements_with_known_or_unknown_states() {
    let dir = repo("list");
    let id = task_with_requirements(&dir);
    let store = dir.join(".git/mdkg/coord.sqlite");

    // Without a coordinator the states are unknown, never "unverified".
    if !store.exists() {
        let (code, out) = json(&dir, &["req", "list", &id, "--json"]);
        assert_eq!(code, 0, "{out}");
        assert_eq!(out["ok"], true);
        assert!(codes(&out).contains(&"coord.store_failed"), "{out}");
        check_items(&out["data"]["requirements"], None);
        assert!(!store.exists(), "a read must not create the coordinator");
    }

    // With one, a requirement without an entry reads as unverified.
    assert_eq!(json(&dir, &["claim", &id, "--json"]).0, 0);
    let (code, out) = json(&dir, &["req", "list", &id, "--json"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["schema"], "mdkg.req.list.v1");
    assert!(codes(&out).is_empty(), "{out}");
    check_items(&out["data"]["requirements"], Some("unverified"));

    let (code, out) = json(&dir, &["pack", &id, "--json"]);
    assert_eq!(code, 0, "{out}");
    let packed = out["data"]["requirements"]
        .as_array()
        .expect("requirements");
    assert_eq!(packed.len(), 1, "{out}");
    assert!(packed[0]["qid"].as_str().expect("qid").ends_with(&id));
    check_items(&packed[0]["requirements"], Some("unverified"));

    std::fs::remove_dir_all(dir).expect("remove fixture");
}

#[test]
fn pack_without_requirements_has_an_empty_list_and_no_warning() {
    let dir = repo("plain");
    assert_eq!(json(&dir, &["init", "--work", "shared", "--json"]).0, 0);
    let (code, out) = json(&dir, &["new", "feat", "Plain", "--json"]);
    assert_eq!(code, 0, "{out}");
    let id = out["data"]["node"]["id"].as_str().expect("id").to_string();
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-q", "-m", "init"]);
    let (code, out) = json(&dir, &["pack", &id, "--json"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["data"]["requirements"], serde_json::json!([]));
    assert!(!codes(&out).contains(&"coord.store_failed"), "{out}");
    std::fs::remove_dir_all(dir).expect("remove fixture");
}

/// The ledger's own fixture, small: a git work graph with a reviewer `rev`
/// and a worker `wrk`, both reporting to `orchestrator`, whose classes come
/// from the registered graph checkout, as `entry` reads classes (A27 rule
/// 4).
struct Ledger {
    dir: PathBuf,
    repo: PathBuf,
    home: PathBuf,
}

impl Drop for Ledger {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

const LEDGER_NODE: &str = "task-01a1878e-3e6f-710b-9d01-68d002aed3e3";
const LEDGER_GRAPH: &str = "0192f3a0-0000-7000-8000-000000000000";

impl Ledger {
    fn new(body: &str) -> Self {
        Self::in_workspace(body, None)
    }

    /// The fixture with its node in a second workspace at `path` (node
    /// folder `<path>/.mdkg`) instead of the root's.
    fn in_workspace(body: &str, path: Option<&str>) -> Self {
        let dir = std::fs::canonicalize(common::temp::dir("mdkg-req-ledger")).expect("canonical");
        let f = Self {
            repo: dir.join("repo"),
            home: dir.join("home"),
            dir,
        };
        let mdkg = path.map_or(".mdkg".to_string(), |p| format!("{p}/.mdkg"));
        std::fs::create_dir_all(f.repo.join(&mdkg).join("work")).expect("fixture");
        std::fs::create_dir_all(f.repo.join(".mdkg/agents")).expect("fixture");
        std::fs::create_dir_all(&f.home).expect("fixture");
        let mut config = serde_json::json!({
            "schema_version": 3, "format_version": "0.7", "graph_id": LEDGER_GRAPH,
            "work": {"location": "git"}
        });
        if let Some(path) = path {
            config["workspaces"] = serde_json::json!({
                "root": {"path": ".", "mdkg_dir": ".mdkg", "enabled": true},
                "side": {"path": path, "mdkg_dir": ".mdkg", "enabled": true}
            });
        }
        std::fs::write(f.repo.join(".mdkg/config.json"), config.to_string()).expect("config");
        for (name, role) in [("rev", "reviewer"), ("wrk", "worker")] {
            std::fs::write(
                f.repo.join(format!(".mdkg/agents/{name}.json")),
                serde_json::json!({
                    "schema": "mdkg.agent.v1", "harness": "codex", "role": role,
                    "reports_to": "orchestrator", "mode": "auto"
                })
                .to_string(),
            )
            .expect("agent");
        }
        std::fs::write(
            f.repo.join(&mdkg).join("work/task-ledger.md"),
            format!(
                "---\nid: {LEDGER_NODE}\ntype: task\ntitle: Ledger fixture\nstatus: todo\n\
                 created: 2026-10-29T00:00:00.000Z\nupdated: 2026-10-29T00:00:00.000Z\n---\n{body}"
            ),
        )
        .expect("node");
        std::fs::write(f.repo.join(".gitignore"), ".worktrees/\n").expect("ignore");
        f.git(&["init", "-q", "-b", "main"]);
        f.git(&["add", "."]);
        f.git(&["commit", "-qm", "fixture"]);
        f.git(&[
            "worktree",
            "add",
            "-q",
            ".worktrees/graph",
            "-b",
            "mdkg-graph",
        ]);
        f.git(&[
            "config",
            &format!("mdkg.graph.{LEDGER_GRAPH}.worktree"),
            "graph",
        ]);
        f
    }

    fn env<'a>(&self, cmd: &'a mut Command) -> &'a mut Command {
        common::without_launcher_env(cmd)
            .current_dir(&self.repo)
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("XDG_DATA_HOME", self.home.join(".local/share"))
            .env("CODEX_HOME", self.home.join(".codex"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("MDKG_RUN_MDKG")
            .env("MDKG_RUN_SECURITY", "/usr/bin/false")
    }

    fn git(&self, args: &[&str]) -> String {
        let out = self
            .env(&mut Command::new("git"))
            .args(args)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// `mdkg --as <actor> <args> --json`, which must succeed: its envelope.
    fn ok(&self, actor: &str, args: &[&str]) -> Value {
        let out = self
            .env(&mut Command::new(BIN))
            .args(["--root", self.repo.to_str().expect("root"), "--as", actor])
            .args(args)
            .arg("--json")
            .output()
            .expect("mdkg");
        let v: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
            panic!(
                "mdkg {args:?} printed no envelope ({e}): {}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
        });
        assert_eq!(out.status.code(), Some(0), "mdkg {args:?}: {v:#}");
        v
    }
}

/// States recorded in an existing store reach `req list` and `pack`: met
/// and unmet as the ledger holds them, `unverified` for an item with no
/// entry, and none for a retired item.
#[test]
fn req_list_and_pack_show_the_ledgers_recorded_states() {
    let f = Ledger::new(
        "Body.\n\n# Requirements\n\n\
         - R1: The writer shall render the ledger.\n\
         - R2: The writer shall keep each line under 240 bytes.\n\
         - R3: The writer shall sort entries by id.\n\
         - R4: The writer shall name each entry's actor.\n\
         - R5 (retired): The writer shall be fast.\n",
    );
    let head = f.git(&["rev-parse", "HEAD"]);
    // A reviewer settles R1 and R2; with its worker holding the node,
    // `orchestrator` is the coordinator and waives R3.
    f.ok("wrk", &["claim", LEDGER_NODE]);
    for (actor, id, state) in [
        ("rev", "R1", "met"),
        ("rev", "R2", "unmet"),
        ("orchestrator", "R3", "waived"),
    ] {
        f.ok(
            actor,
            &[
                "entry",
                "req",
                LEDGER_NODE,
                id,
                "--state",
                state,
                "--at",
                &head,
            ],
        );
    }
    let expected = [
        ("R1", Some("met")),
        ("R2", Some("unmet")),
        ("R3", Some("waived")),
        ("R4", Some("unverified")),
        ("R5", None),
    ];
    let check = |items: &Value| {
        let items = items.as_array().expect("requirements");
        let got: Vec<(&str, Option<&str>)> = items
            .iter()
            .map(|r| (r["id"].as_str().expect("id"), r["state"].as_str()))
            .collect();
        assert_eq!(got, expected, "{items:?}");
    };

    let out = f.ok("rev", &["req", "list", LEDGER_NODE]);
    assert!(codes(&out).is_empty(), "{out}");
    check(&out["data"]["requirements"]);

    let out = f.ok("rev", &["pack", LEDGER_NODE]);
    assert!(!codes(&out).contains(&"coord.store_failed"), "{out}");
    let packed = out["data"]["requirements"].as_array().expect("packed");
    assert_eq!(packed.len(), 1, "{out}");
    check(&packed[0]["requirements"]);
}

/// A workspace folder may hold `#`, and the lease key's folder is
/// everything before its last `#` (a-flow's #6919): the state recorded in
/// `a#b/.mdkg` reads back as recorded.
#[test]
fn req_list_reads_states_in_a_workspace_folder_holding_a_hash() {
    let f = Ledger::in_workspace(
        "# Requirements\n\n- R1: The writer shall render the ledger.\n",
        Some("a#b"),
    );
    let head = f.git(&["rev-parse", "HEAD"]);
    f.ok(
        "rev",
        &[
            "entry",
            "req",
            LEDGER_NODE,
            "R1",
            "--state",
            "met",
            "--at",
            &head,
        ],
    );
    let out = f.ok("rev", &["req", "list", LEDGER_NODE]);
    assert!(codes(&out).is_empty(), "{out}");
    let items = out["data"]["requirements"]
        .as_array()
        .expect("requirements");
    assert_eq!(items.len(), 1, "{out}");
    assert_eq!(items[0]["state"], "met", "{out}");
}

/// e2e S6, by design (docs/format-0.7.md 3.6): an indented continuation
/// line belongs to its item, and `(verify: ...)` counts only when it ends
/// the whole item. One before a continuation line is part of the
/// statement, and the item has no check, unless the item still ends in
/// `)` and the fallback applies.
#[test]
fn requirement_continuation_after_verify() {
    let dir = repo("continuation");
    let body = "# Overview\n\nA task.\n\n# Requirements\n\n\
- R1: When a send fails, the client shall retry. (verify: test backoff)\n  \
continuation line of R1\n\
- R2: When a send fails, the client shall retry\n  \
with backoff. (verify: test backoff)\n\
- R3: The client shall retry. (verify: test backoff)\n  \
up to 3 times (default)\n\
- R4: The client shall retry. (verify: first)\n  \
up to 3 times (verify: second)\n";
    let id = task_with_body(&dir, body);
    let (code, out) = json(&dir, &["req", "list", &id, "--json"]);
    assert_eq!(code, 0, "{out}");
    let items = out["data"]["requirements"].as_array().expect("items");
    assert_eq!(items.len(), 4, "{out}");
    assert_eq!(items[0]["verify"], Value::Null, "{out}");
    let statement = items[0]["statement"].as_str().expect("statement");
    assert!(
        statement.contains("(verify: test backoff)")
            && statement.ends_with("continuation line of R1"),
        "{out}"
    );
    assert_eq!(items[1]["verify"], "test backoff", "{out}");
    assert_eq!(
        items[1]["statement"], "When a send fails, the client shall retry with backoff.",
        "{out}"
    );
    // Still ending in `)`, the item takes the fallback: the earliest
    // marker starts a check that runs to the item's end (review of
    // 787cff45).
    assert_eq!(items[2]["statement"], "The client shall retry.", "{out}");
    assert_eq!(
        items[2]["verify"], "test backoff) up to 3 times (default",
        "{out}"
    );
    // A balanced check ending the continuation is the check; the earlier
    // marker stays in the statement (review of 66f6ea73).
    assert_eq!(items[3]["verify"], "second", "{out}");
    let statement = items[3]["statement"].as_str().expect("statement");
    assert!(statement.contains("(verify: first)"), "{out}");
    std::fs::remove_dir_all(dir).expect("remove fixture");
}
