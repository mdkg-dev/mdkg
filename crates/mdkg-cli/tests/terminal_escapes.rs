//! Node text in human-readable output can't drive the terminal (RC1
//! security finding SEC2): ESC, CSI, OSC and C1 controls in a title, a
//! field or a body print escaped in `show`, `list`, `next`, the graph
//! cards and `goal`. JSON output keeps the text as it is.

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

/// Runs mdkg, returning its exit code, stdout and stderr.
fn mdkg(dir: &Path, home: &Path, args: &[&str]) -> (i32, String, String) {
    let out = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")), home)
        .arg("--root")
        .arg(dir)
        .args(args)
        .output()
        .expect("run mdkg");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Every escape a hostile title or body could carry: a CSI cursor move,
/// an OSC title set ended by BEL, an OSC 8 link ended by ST, an OSC 52
/// clipboard write, and C1's CSI (U+009B).
const TITLE: &str = "Fix \u{1b}[2J\u{1b}]0;owned\u{7} it \u{9b}31m";
const BODY: &str = "line one\n\u{1b}]8;;https://example.invalid\u{1b}\\link\u{1b}]8;;\u{1b}\\\n\u{1b}]52;c;aGk=\u{7}\ttab kept\n";

const REF: &str = "https://example.invalid/a\u{1b}7\u{7}b\u{9b}2J";

/// A graph with one task whose title and body hold every escape above.
fn graph() -> (Scratch, PathBuf, String) {
    let dir = common::temp::dir("mdkg-cli-terminal-escapes");
    let repo = dir.join("repo");
    fs::create_dir_all(&repo).expect("dir");
    let repo = fs::canonicalize(repo).expect("canonical");
    let home = fs::canonicalize(&dir).expect("canonical");
    git(&repo, &home, &["init", "-q", "-b", "main"]);
    let (code, out, err) = mdkg(&repo, &home, &["init", "--work", "shared", "--json"]);
    assert_eq!(code, 0, "{out}{err}");
    git(&repo, &home, &["add", "-A"]);
    git(&repo, &home, &["commit", "-qm", "init"]);
    let (code, out, err) = mdkg(
        &repo,
        &home,
        &["new", "task", "Plain title", "--status", "todo", "--json"],
    );
    assert_eq!(code, 0, "{out}{err}");
    let v: Value = serde_json::from_str(&out).expect("json");
    let id = v["data"]["node"]["id"].as_str().expect("id").to_string();
    let path = repo.join(v["data"]["node"]["path"].as_str().expect("path"));
    let text = fs::read_to_string(&path).expect("node");
    // Raw in the file, as a hostile repository would write them.
    let text = text.replacen("title: Plain title", &format!("title: {TITLE}"), 1);
    // A ref is the node's own text too: a URI with ESC 7, BEL and C1 CSI
    // (no brackets, so the inline list still reads).
    assert!(text.contains("\nrefs: []\n"), "{text}");
    let text = text.replacen("\nrefs: []\n", &format!("\nrefs: [{REF}]\n"), 1);
    let text = format!("{}\n{BODY}", text.trim_end());
    fs::write(&path, text).expect("write");
    git(&repo, &home, &["add", "-A"]);
    git(&repo, &home, &["commit", "-qm", "hostile"]);
    (Scratch(dir), repo, id)
}

/// No raw control but newline and tab, and the escapes shown as text.
fn assert_escaped(what: &str, out: &str) {
    let raw: Vec<char> = out
        .chars()
        .filter(|c| c.is_control() && *c != '\n' && *c != '\t')
        .collect();
    assert!(
        raw.is_empty(),
        "{what} printed raw controls {raw:?}: {out:?}"
    );
}

#[test]
fn node_text_prints_escaped_in_every_text_view() {
    let (scratch, repo, id) = graph();
    let home = scratch.0.clone();
    let views: &[&[&str]] = &[
        &["show", &id],
        &["list"],
        &["next"],
        &["graph", "ready"],
        &["search", "Fix"],
        &["graph", "refs", &id],
    ];
    for args in views {
        let (code, out, err) = mdkg(&repo, &home, args);
        assert_eq!(code, 0, "{args:?}: {out}{err}");
        assert_escaped(&format!("{args:?}"), &out);
        assert_escaped(&format!("{args:?} stderr"), &err);
        assert!(
            out.contains("\\u{1b}"),
            "{args:?} shows ESC escaped: {out:?}"
        );
    }
    // show's body keeps its lines and tabs, and escapes the rest.
    let (_, out, _) = mdkg(&repo, &home, &["show", &id]);
    assert!(out.contains("line one\n"), "{out:?}");
    assert!(out.contains("\ttab kept"), "{out:?}");
    assert!(out.contains("\\u{9b}31m"), "C1 CSI escaped: {out:?}");
    assert!(out.contains("\\u{7}"), "BEL escaped: {out:?}");
    // JSON keeps the text as it is.
    let (_, out, _) = mdkg(&repo, &home, &["show", &id, "--json"]);
    let v: Value = serde_json::from_str(&out).expect("json");
    assert_eq!(v["data"]["node"]["title"], TITLE, "{out}");
    let (_, out, _) = mdkg(&repo, &home, &["graph", "refs", &id, "--json"]);
    assert!(
        out.contains("\\u001b7"),
        "graph refs JSON keeps the ref: {out}"
    );
}
