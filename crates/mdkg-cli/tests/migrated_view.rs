//! What a migrated graph shows (the dogfood pass, task-45): a node's 0.6.0
//! id stays visible beside its full id, in text and JSON, and `list`'s
//! text leaves the path to `show`.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).ok();
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
        .env("MDKG_ACTOR", "tester")
}

fn git(dir: &Path, args: &[&str]) {
    let ok = isolated(Command::new("git").arg("-C").arg(dir).args(args))
        .status()
        .expect("git runs")
        .success();
    assert!(ok, "git {args:?}");
}

fn mdkg(dir: &Path, args: &[&str]) -> (i32, String) {
    let out = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
        .arg("--root")
        .arg(dir)
        .args(args)
        .env("HOME", dir)
        .output()
        .expect("run mdkg");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

/// A 0.7 graph with one task that carries a legacy id.
fn graph() -> (Scratch, String) {
    let dir = common::temp::dir("mdkg-cli-migrated-view");
    let dir = fs::canonicalize(dir).expect("canonical");
    git(&dir, &["init", "-q", "-b", "main"]);
    let (code, out) = mdkg(&dir, &["init", "--work", "shared", "--json"]);
    assert_eq!(code, 0, "{out}");
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "init"]);
    // Two tasks titled "Audit …", with legacy ids task-4 and task-5.
    let mut ids = Vec::new();
    for (title, legacy) in [
        ("Audit the runtime", "task-4"),
        ("Audit the store", "task-5"),
    ] {
        let (code, out) = mdkg(&dir, &["new", "task", title, "--status", "todo", "--json"]);
        assert_eq!(code, 0, "{out}");
        let v: Value = serde_json::from_str(&out).expect("json");
        let id = v["data"]["node"]["id"].as_str().expect("id").to_string();
        let path = dir.join(v["data"]["node"]["path"].as_str().expect("path"));
        let text = fs::read_to_string(&path).expect("node");
        let text = text.replacen(
            &format!("id: {id}\n"),
            &format!("id: {id}\nlegacy_id: {legacy}\n"),
            1,
        );
        fs::write(&path, text).expect("write");
        ids.push(id);
    }
    let id = ids.remove(0);
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "graph"]);
    (Scratch(dir), id)
}

#[test]
fn a_legacy_id_shows_beside_the_full_id_and_list_drops_the_path() {
    let (g, id) = graph();
    let (code, out) = mdkg(&g.0, &["list"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains(&format!("{id} (task-4) | todo/")), "{out}");
    assert!(!out.contains(".mdkg/"), "list's text names no path: {out}");
    let (code, out) = mdkg(&g.0, &["show", "task-4"]);
    assert_eq!(code, 0, "{out}");
    let first = out.lines().next().unwrap_or_default();
    assert!(first.starts_with(&format!("{id} (task-4) |")), "{out}");
    assert!(
        out.lines()
            .nth(1)
            .is_some_and(|l| l.starts_with("path: .mdkg/")),
        "show keeps the path, on its own line: {out}"
    );
    for args in [&["show", "task-4", "--json"][..], &["list", "--json"]] {
        let (_, out) = mdkg(&g.0, args);
        let v: Value = serde_json::from_str(&out).expect("json");
        let node = if args[0] == "show" {
            v["data"]["node"].clone()
        } else {
            v["data"]["nodes"][0].clone()
        };
        assert_eq!(node["legacy_id"], "task-4", "{args:?}: {out}");
    }
}

/// The Codex round on ef66d3a: graph's cards and pack's sections show the
/// legacy id too; an ambiguous handle lists its matches with their legacy
/// ids, and says a legacy id works.
#[test]
fn graph_pack_and_ambiguity_hints_show_legacy_ids() {
    let (g, id) = graph();
    let (code, out) = mdkg(&g.0, &["graph", "ready", "--json"]);
    assert_eq!(code, 0, "{out}");
    let v: Value = serde_json::from_str(&out).expect("json");
    let legacy: Vec<&str> = v["data"]["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|n| n["legacy_id"].as_str())
        .collect();
    assert!(legacy.contains(&"task-4"), "{out}");
    let (code, out) = mdkg(&g.0, &["pack", "task-4"]);
    assert_eq!(code, 0, "{out}");
    // Pack's sections name nodes by qid (`root:…`), as before.
    assert!(out.contains(&format!("{id} (task-4)")), "{out}");
    let (code, out) = mdkg(&g.0, &["show", "Audit", "--json"]);
    assert_eq!(code, 7, "{out}");
    let v: Value = serde_json::from_str(&out).expect("json");
    let hint = v["diagnostics"][0]["hint"].as_str().unwrap_or_default();
    assert!(
        hint.contains("(task-4)") && hint.contains("(task-5)"),
        "{out}"
    );
    assert!(hint.contains("a legacy id like task-"), "{out}");
}
