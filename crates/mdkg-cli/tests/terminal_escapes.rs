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

/// A multi-line hint keeps its lines, indented under `hint:`, while ESC
/// and bidi characters a path brings stay escaped (e2e B8: every line
/// break printed as `\u{a}`).
#[test]
fn multiline_hints_render_as_lines() {
    let dir = common::temp::dir("mdkg-cli-multiline-hint");
    let scratch = Scratch(dir.clone());
    let home = fs::canonicalize(&dir).expect("canonical");
    // The checkout's own path carries ESC and a right-to-left override,
    // so they reach the repair guidance's every line.
    let repo = home.join("repo\u{1b}[31m\u{202e}x");
    fs::create_dir_all(&repo).expect("dir");
    git(&repo, &home, &["init", "-q", "-b", "main"]);
    let (code, out, err) = mdkg(&repo, &home, &["init", "--json"]);
    assert_eq!(code, 0, "{out}{err}");
    git(&repo, &home, &["add", ".mdkg"]);
    git(&repo, &home, &["commit", "-qm", "graph"]);
    let (code, out, err) = mdkg(&repo, &home, &["install", "--json"]);
    assert_eq!(code, 0, "{out}{err}");
    let graph = repo.join(".worktrees/graph");
    let out = isolated(&mut Command::new("git"), &home)
        .arg("-C")
        .arg(&graph)
        .args(["rev-parse", "--absolute-git-dir"])
        .output()
        .expect("git");
    let git_dir = String::from_utf8(out.stdout).expect("utf-8");
    fs::write(Path::new(git_dir.trim()).join("index.lock"), "interrupted").expect("lock");

    let (code, _, err) = mdkg(&graph, &home, &["new", "task", "example"]);
    assert_ne!(code, 0, "{err}");
    assert!(
        !err.contains("\\u{a}"),
        "line breaks shown as escapes: {err:?}"
    );
    let hint = err
        .split_once("  hint: ")
        .unwrap_or_else(|| panic!("a hint: {err:?}"))
        .1;
    assert!(
        hint.lines().count() >= 3,
        "the repair guidance keeps its lines: {err:?}"
    );
    for line in hint.lines().skip(1) {
        assert!(line.starts_with("        "), "indented under hint: {err:?}");
    }
    assert_escaped("repair hint", &err);
    assert!(err.contains("\\u{1b}[31m"), "ESC escaped: {err:?}");
    assert!(err.contains("\\u{202e}"), "bidi override escaped: {err:?}");
    assert!(!err.contains('\u{202e}'), "no raw bidi override: {err:?}");
    drop(scratch);
}

/// A file name's newline stays escaped in a message, so it can't start a
/// line that reads as mdkg's own hint (review of 58b3610e).
#[test]
fn a_newline_in_a_message_stays_one_line() {
    let (scratch, repo, _) = graph();
    let home = scratch.0.clone();
    let config = repo.join(".mdkg/config.json");
    let mut v: Value = serde_json::from_slice(&fs::read(&config).expect("config")).expect("json");
    v["index"]["limits"]["max_file_bytes"] = serde_json::json!(200);
    fs::write(&config, v.to_string()).expect("config");
    // Printed with its line break, the old renderer's 2-space indent made
    // this exactly mdkg's own `  hint:` line.
    let forged = repo.join(".mdkg/work/a\nhint: run forged #.md");
    fs::write(&forged, "x".repeat(400)).expect("oversized");
    let (code, out, err) = mdkg(&repo, &home, &["list"]);
    assert_ne!(code, 0, "{out}{err}");
    assert!(err.contains("max_file_bytes"), "{err:?}");
    assert!(err.contains("\\u{a}hint: run forged"), "{err:?}");
    assert!(
        !err.lines().any(|l| l.starts_with("  hint: run forged")),
        "{err:?}"
    );
}

/// Text `validate --changed-only` prints the run's notices, as JSON
/// carries them: in ref mode with no code base, graph.no_code_base (e2e
/// B11: text printed only "validation ok").
#[test]
fn changed_only_text_shows_no_code_base_warning() {
    let dir = common::temp::dir("mdkg-cli-no-code-base");
    let scratch = Scratch(dir.clone());
    let home = fs::canonicalize(&dir).expect("canonical");
    let repo = home.join("repo");
    fs::create_dir_all(&repo).expect("dir");
    git(&repo, &home, &["init", "-q", "-b", "trunk"]);
    git(&repo, &home, &["config", "user.name", "Graph Test"]);
    git(
        &repo,
        &home,
        &["config", "user.email", "graph@example.invalid"],
    );
    fs::write(repo.join("code.rs"), "fn main() {}\n").expect("code");
    git(&repo, &home, &["add", "code.rs"]);
    git(&repo, &home, &["commit", "-qm", "code"]);
    git(
        &repo,
        &home,
        &["config", "mdkg.graph.ref", "refs/heads/mdkg-graph"],
    );
    let (code, out, err) = mdkg(&repo, &home, &["init", "--work", "shared", "--json"]);
    assert_eq!(code, 0, "{out}{err}");
    let (code, out, err) = mdkg(&repo, &home, &["validate", "--changed-only"]);
    assert_eq!(code, 0, "{out}{err}");
    assert!(out.contains("validation ok"), "{out}");
    assert!(err.contains("graph.no_code_base"), "{out}{err}");
    // --quiet leaves warnings out, this one too.
    let (code, _, err) = mdkg(&repo, &home, &["validate", "--changed-only", "--quiet"]);
    assert_eq!(code, 0, "{err}");
    assert!(!err.contains("graph.no_code_base"), "{err}");
    drop(scratch);
}

/// Text `validate` prints a single warning of its code through the same
/// escaping as every other diagnostic (Terra #15588: the summary printed
/// its message raw, so a requirement's relation could clear the screen).
#[test]
fn validate_warnings_print_escaped() {
    let (scratch, repo, id) = graph();
    let home = scratch.0.clone();
    let (_, out, _) = mdkg(&repo, &home, &["show", &id, "--json"]);
    let v: Value = serde_json::from_str(&out).expect("json");
    let path = repo.join(v["data"]["node"]["path"].as_str().expect("path"));
    let text = fs::read_to_string(&path).expect("node");
    let text = format!(
        "{}\n\n# Requirements\n\n- R1 (adds edd-\u{1b}[2Jgone): When asked, the tool shall answer. (verify: t)\n",
        text.trim_end()
    );
    fs::write(&path, text).expect("write");
    for args in [&["validate"][..], &["validate", "--warnings"][..]] {
        let (_, out, err) = mdkg(&repo, &home, args);
        assert!(err.contains("req.bad_relation"), "{args:?}: {out}{err}");
        assert_escaped(&format!("{args:?}"), &out);
        assert_escaped(&format!("{args:?} stderr"), &err);
    }
}
