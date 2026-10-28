//! RC1's minor findings (task-45), each reproduced on the RC binary first:
//! MCP's list filters take lists as the CLI does (U5); `store` usage
//! errors carry their own schema (U6); a short title's card fits 80
//! columns (U8); `--fields refs` reads as `edges` (F4); `goal current`
//! names the goal (F6).

mod common;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Value, json};

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).ok();
    }
}

fn isolated<'a>(cmd: &'a mut Command, home: &Path) -> &'a mut Command {
    common::without_launcher_env(cmd)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .env("MDKG_ACTOR", "tester")
}

fn git(dir: &Path, home: &Path, args: &[&str]) {
    let ok = isolated(Command::new("git").arg("-C").arg(dir).args(args), home)
        .status()
        .expect("git runs")
        .success();
    assert!(ok, "git {args:?}");
}

struct Graph {
    _scratch: Scratch,
    repo: PathBuf,
    home: PathBuf,
}

impl Graph {
    /// A committed graph with a goal and two tasks, one titled as the
    /// README's example ("Parse the CSV header").
    fn new() -> Graph {
        let dir = common::temp::dir("mdkg-cli-rc1-minors");
        let repo = dir.join("repo");
        fs::create_dir_all(&repo).expect("dir");
        let g = Graph {
            repo: fs::canonicalize(&repo).expect("canonical"),
            home: fs::canonicalize(&dir).expect("canonical"),
            _scratch: Scratch(dir),
        };
        git(&g.repo, &g.home, &["init", "-q", "-b", "main"]);
        g.ok(&["init", "--work", "shared"]);
        git(&g.repo, &g.home, &["add", "-A"]);
        git(&g.repo, &g.home, &["commit", "-qm", "init"]);
        g.ok(&["new", "goal", "Ship the parser", "--status", "todo"]);
        g.ok(&["new", "task", "Parse the CSV header", "--status", "todo"]);
        g.ok(&["new", "epic", "Imports", "--status", "todo"]);
        git(&g.repo, &g.home, &["add", "-A"]);
        git(&g.repo, &g.home, &["commit", "-qm", "graph"]);
        g
    }

    fn run(&self, args: &[&str]) -> (i32, String, String) {
        let out = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")), &self.home)
            .current_dir(&self.repo)
            .arg("--root")
            .arg(&self.repo)
            .args(args)
            .output()
            .expect("run mdkg");
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    fn ok(&self, args: &[&str]) -> String {
        let (code, out, err) = self.run(args);
        assert_eq!(code, 0, "{args:?}: {out}{err}");
        out
    }

    fn json(&self, args: &[&str]) -> Value {
        let (_, out, err) = self.run(&[args, &["--json"][..]].concat());
        serde_json::from_str(&out).unwrap_or_else(|_| panic!("{args:?}: {out}{err}"))
    }

    fn id_titled(&self, title: &str) -> String {
        let v = self.json(&["list"]);
        v["data"]["nodes"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|n| n["title"] == title)
            .and_then(|n| n["id"].as_str())
            .expect("node")
            .to_string()
    }

    /// The results of MCP tool calls, in order.
    fn mcp(&self, calls: &[(&str, Value)]) -> Vec<Value> {
        let mut child = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")), &self.home)
            .current_dir(&self.repo)
            .args(["--root", self.repo.to_str().expect("utf-8"), "mcp"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("mcp");
        let mut input = vec![
            json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {
                "protocolVersion": "2025-06-18", "capabilities": {},
                "clientInfo": {"name": "t", "version": "1"}}}),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        ];
        for (i, (name, args)) in calls.iter().enumerate() {
            input.push(
                json!({"jsonrpc": "2.0", "id": i + 1, "method": "tools/call",
                "params": {"name": name, "arguments": args}}),
            );
        }
        let text: String = input.iter().map(|m| format!("{m}\n")).collect();
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(text.as_bytes())
            .expect("write");
        let out = child.wait_with_output().expect("mcp ran");
        // Replies can come in any order: sorted by request id.
        let mut replies: Vec<Value> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|v| v["id"].as_u64().is_some_and(|id| id > 0))
            .collect();
        replies.sort_by_key(|v| v["id"].as_u64());
        replies
            .into_iter()
            .map(|v| {
                let text = v["result"]["content"]
                    .as_array()
                    .and_then(|c| c.last())
                    .and_then(|c| c["text"].as_str())
                    .unwrap_or_default()
                    .to_string();
                serde_json::from_str(&text).unwrap_or(Value::Null)
            })
            .collect()
    }
}

#[test]
fn mcp_list_and_search_take_lists_of_types_and_statuses() {
    let g = Graph::new();
    let results = g.mcp(&[
        ("list", json!({"type": ["task", "epic"]})),
        ("list", json!({"type": "task,epic", "status": "todo,done"})),
        ("search", json!({"query": "CSV", "status": "todo,done"})),
    ]);
    assert_eq!(results.len(), 3, "{results:?}");
    for r in &results[..2] {
        assert_eq!(r["ok"], true, "{r}");
        assert_eq!(r["data"]["count"], 2, "task and epic: {r}");
    }
    assert_eq!(results[2]["ok"], true, "{}", results[2]);
    assert_eq!(results[2]["data"]["count"], 1, "{}", results[2]);
}

#[test]
fn store_usage_errors_carry_their_own_schema() {
    let g = Graph::new();
    for (args, schema) in [
        (&["store", "restore", "--json"][..], "mdkg.store.restore.v1"),
        (
            &["store", "snapshots", "--typo", "--json"],
            "mdkg.store.snapshots.v1",
        ),
    ] {
        let (code, out, _) = g.run(args);
        assert_ne!(code, 0, "{args:?}");
        let v: Value = serde_json::from_str(&out).expect("json");
        assert_eq!(v["schema"], schema, "{args:?}: {out}");
    }
}

#[test]
fn a_short_titles_card_fits_80_columns() {
    let g = Graph::new();
    let out = g.ok(&["list", "--type", "task"]);
    let row = out.lines().next().expect("a row");
    assert!(row.ends_with("| todo/p9 | Parse the CSV header"), "{row}");
    assert!(
        row.chars().count() <= 80,
        "{} columns: {row}",
        row.chars().count()
    );
    let id = g.id_titled("Parse the CSV header");
    let out = g.ok(&["show", &id]);
    let mut lines = out.lines();
    let first = lines.next().expect("header");
    assert!(first.chars().count() <= 80, "{first}");
    assert_eq!(
        lines.next(),
        Some("path: .mdkg/work/task-parse-the-csv-header.md")
    );
    // Only the fields asked for, no placeholders.
    let out = g.ok(&["list", "--type", "task", "--fields", "title"]);
    assert_eq!(out.trim_end(), format!("{id} | Parse the CSV header"));
    // JSON keeps the type.
    let v = g.json(&["show", &id]);
    assert_eq!(v["data"]["node"]["type"], "task");
}

#[test]
fn fields_refs_reads_as_edges() {
    let g = Graph::new();
    let epic = g.id_titled("Imports");
    g.ok(&[
        "new",
        "task",
        "Refers to the epic",
        "--status",
        "todo",
        "--refs",
        &epic,
    ]);
    let id = g.id_titled("Refers to the epic");
    for args in [
        &[
            "list", "--type", "task", "--status", "todo", "--fields", "id,refs",
        ][..],
        &["show", &id, "--fields", "refs"],
    ] {
        let v = g.json(args);
        let node = if args[0] == "list" {
            v["data"]["nodes"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|n| n["id"] == id.as_str())
                .cloned()
                .unwrap_or_default()
        } else {
            v["data"]["node"].clone()
        };
        assert!(node["edges"].is_object(), "{args:?}: {v}");
        assert!(node["edges"].to_string().contains(&epic), "{args:?}: {v}");
    }
    let v = g.json(&["list", "--fields", "refs,edges"]);
    assert_eq!(v["ok"], true, "refs and edges together: {v}");
}

#[test]
fn goal_current_names_the_goal() {
    let g = Graph::new();
    let goal = g.id_titled("Ship the parser");
    g.ok(&["goal", "activate", &goal]);
    let out = g.ok(&["goal", "current"]);
    let first = out.lines().next().unwrap_or_default();
    assert!(
        first.contains(&goal) && first.contains("Ship the parser"),
        "{out}"
    );
}
