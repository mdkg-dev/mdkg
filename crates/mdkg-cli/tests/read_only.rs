//! Reads that write nothing (Omni's read audit, 0.7.2): `--read-only`,
//! `status --no-observe` and purge-free `--no-mark` mail reads. Each test
//! proves no write by the store's event head and its bytes.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

mod common;

const DAY: u64 = 24 * 60 * 60 * 1000;

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
        .env("MDKG_WAIT_POLL_MS", "20")
}

fn sh(dir: &Path, args: &[&str]) {
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

/// A repository on main whose mail keeps bodies for a day
/// (`retention: ttl:1`), with worktrees `wt-a` and `wt-b`.
fn repo() -> (Scratch, PathBuf, PathBuf, PathBuf) {
    repo_with_graph_in("")
}

/// [`repo`] with the graph in the folder `sub` of the checkout (`""` for
/// its top): the paths returned for `wt-a` and `wt-b` are their graph
/// roots.
fn repo_with_graph_in(sub: &str) -> (Scratch, PathBuf, PathBuf, PathBuf) {
    let dir =
        Scratch(fs::canonicalize(common::temp::dir("mdkg-cli-read-only")).expect("canonical"));
    let repo = dir.0.join("repo");
    let graph = if sub.is_empty() {
        repo.clone()
    } else {
        repo.join(sub)
    };
    let conformance = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance");
    fs::create_dir_all(&graph).expect("graph");
    copy_tree(&conformance.join("base"), &graph);
    copy_tree(&conformance.join("cases/g01-valid-basic"), &graph);
    fs::remove_file(graph.join("commands")).ok();
    fs::remove_file(graph.join("README.md")).ok();
    let config = graph.join(".mdkg/config.json");
    let mut value: Value =
        serde_json::from_slice(&fs::read(&config).expect("config")).expect("config JSON");
    value["coordination"]["messages"]["retention"] = "ttl:1".into();
    fs::write(&config, serde_json::to_vec_pretty(&value).expect("JSON")).expect("config");
    sh(&repo, &["init", "-q", "-b", "main"]);
    sh(&repo, &["add", "."]);
    sh(&repo, &["commit", "-q", "-m", "init"]);
    let wa = dir.0.join("wt-a");
    let wb = dir.0.join("wt-b");
    for (branch, wt) in [("a", &wa), ("b", &wb)] {
        let path = wt.to_str().expect("utf-8");
        sh(
            &repo,
            &["worktree", "add", "-q", "-b", branch, path, "main"],
        );
    }
    if sub.is_empty() {
        (dir, repo, wa, wb)
    } else {
        (dir, repo, wa.join(sub), wb.join(sub))
    }
}

/// Runs mdkg as `actor` at `now` with `flags` before the command: (exit
/// code, envelope).
fn run(worktree: &Path, actor: &str, now: u64, args: &[&str]) -> (i32, Value) {
    run_from(worktree, worktree, actor, now, args)
}

/// [`run`] from the folder `cwd`, with `--root worktree`.
fn run_from(cwd: &Path, worktree: &Path, actor: &str, now: u64, args: &[&str]) -> (i32, Value) {
    let out = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
        .current_dir(cwd)
        .arg("--root")
        .arg(worktree)
        .args(["--as", actor])
        .args(args)
        .arg("--json")
        .env("MDKG_NOW_MS", now.to_string())
        .output()
        .expect("run mdkg");
    let envelope: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|_| {
        panic!(
            "not JSON: {}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    });
    common::conforms_envelope(&envelope);
    (out.status.code().unwrap_or(-1), envelope)
}

fn ok(worktree: &Path, actor: &str, now: u64, args: &[&str]) -> Value {
    let (code, out) = run(worktree, actor, now, args);
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

/// Each listed message's id and body state (`present` or `purged`).
fn bodies(envelope: &Value) -> Vec<(String, String)> {
    envelope["data"]["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|m| {
            (
                m["messageId"].as_str().unwrap_or_default().to_string(),
                m["body"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

fn store(repo: &Path) -> PathBuf {
    repo.join(".git/mdkg/coord.sqlite")
}

/// What a read must leave alone: the event head, the store's bytes and
/// its WAL's content (a missing WAL reads as empty: SQLite's read-only
/// open may create an empty one, which holds nothing), and how many
/// bodies it holds.
#[derive(Debug, PartialEq, Eq)]
struct Untouched {
    head: String,
    bytes: Vec<u8>,
    wal: Vec<u8>,
    bodies: i64,
}

fn untouched(repo: &Path) -> Untouched {
    let path = store(repo);
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    let bodies = mdkg_coord_sqlite::Store::open_raw_read_only(&path)
        .expect("read-only open")
        .expect("a store")
        .query_row("SELECT COUNT(*) FROM msg_bodies", [], |r| r.get(0))
        .expect("bodies");
    Untouched {
        // A store on an older schema has no head this build reads: its
        // bytes still prove it unchanged.
        head: mdkg_coord_sqlite::events::head(&path).unwrap_or_else(|e| format!("{e:?}")),
        bytes: fs::read(&path).expect("store bytes"),
        wal: fs::read(wal).unwrap_or_default(),
        bodies,
    }
}

fn same(before: &Untouched, after: &Untouched, what: &str) {
    assert_eq!(before.head, after.head, "{what}: event head moved");
    assert_eq!(before.bodies, after.bodies, "{what}: a body was purged");
    assert!(before.bytes == after.bytes, "{what}: store bytes changed");
    assert!(before.wal == after.wal, "{what}: WAL changed");
}

/// `b` gets `old` at 0 and `new` just before a day later; at `DAY + 1` `old`'s body
/// is past its TTL but still in the store.
fn two_messages(wa: &Path, wb: &Path) {
    for (id, now) in [("old", 0), ("new", DAY - 1)] {
        ok(
            wa,
            "a",
            now,
            &[
                "msg", "send", "--to", "b", "--kind", "note", "--id", id, "--text", "for b",
            ],
        );
    }
    // Shown, so a marking read later has something it would mark.
    ok(wb, "b", 1, &["msg", "inbox"]);
}

#[test]
fn read_only_mail_reads_and_status_leave_the_store_byte_identical() {
    let (_dir, repo, wa, wb) = repo();
    two_messages(&wa, &wb);
    let now = DAY + 1;
    let before = untouched(&repo);
    assert_eq!(before.bodies, 2);
    let expected = vec![
        ("old".to_string(), "purged".to_string()),
        ("new".to_string(), "present".to_string()),
    ];

    let inbox = ok(&wb, "b", now, &["--read-only", "msg", "inbox", "--all"]);
    assert_eq!(bodies(&inbox), expected, "{inbox}");
    same(&before, &untouched(&repo), "--read-only msg inbox");

    let wait = ok(
        &wb,
        "b",
        now,
        &["--read-only", "msg", "wait", "--timeout", "1"],
    );
    assert_eq!(bodies(&wait), expected, "{wait}");
    same(&before, &untouched(&repo), "--read-only msg wait");

    let thread = ok(&wb, "b", now, &["--read-only", "msg", "thread", "old"]);
    assert_eq!(bodies(&thread), vec![expected[0].clone()], "{thread}");
    same(&before, &untouched(&repo), "--read-only msg thread");

    let status = ok(&wb, "b", now, &["--read-only", "status"]);
    assert_eq!(status["schema"], "mdkg.status.v1");
    same(&before, &untouched(&repo), "--read-only status");

    // A plain marking read purges the due body: the reads above had work
    // they could have done, and didn't.
    let marked = ok(&wb, "b", now, &["msg", "inbox", "--all"]);
    assert_eq!(bodies(&marked), expected, "{marked}");
    let after = untouched(&repo);
    assert_eq!(after.bodies, 1);
    assert_ne!(after.head, before.head, "the purge is a record");
}

#[test]
fn no_mark_reads_never_purge_or_record() {
    let (_dir, repo, wa, wb) = repo();
    two_messages(&wa, &wb);
    let now = DAY + 1;
    // A writing open may itself touch the file (journal mode, the open's
    // transaction), so this checks the head and the bodies, not bytes.
    let head = untouched(&repo);
    for args in [
        &["msg", "inbox", "--all", "--no-mark"][..],
        &["msg", "wait", "--timeout", "1", "--no-mark"],
        &["msg", "thread", "old", "--no-mark"],
    ] {
        let out = ok(&wb, "b", now, args);
        assert!(
            bodies(&out).contains(&("old".to_string(), "purged".to_string())) || args[1] == "wait",
            "{args:?}: {out}"
        );
        let after = untouched(&repo);
        assert_eq!(after.head, head.head, "{args:?}: event head moved");
        assert_eq!(after.bodies, 2, "{args:?}: a body was purged");
    }
}

#[test]
fn status_no_observe_records_nothing() {
    let (_dir, repo, wa, _wb) = repo();
    // A claim makes the store; status then records main as it is.
    ok(&wa, "a", 0, &["claim", ".mdkg/config.json"]);
    ok(&wa, "a", 0, &["status"]);
    // main moves on, so status has an observation it would record.
    let node = fs::read_dir(repo.join(".mdkg/work"))
        .expect("work folder")
        .map(|e| e.expect("entry").path())
        .find(|p| p.extension().is_some_and(|e| e == "md"))
        .expect("a node");
    let mut text = fs::read_to_string(&node).expect("node");
    text.push_str("\nMoved on main.\n");
    fs::write(&node, text).expect("write");
    sh(&repo, &["add", "."]);
    sh(&repo, &["commit", "-q", "-m", "move main"]);
    let before = untouched(&repo);
    let out = ok(&wa, "a", 1, &["status", "--no-observe"]);
    assert_eq!(out["schema"], "mdkg.status.v1");
    assert!(
        !codes(&out).contains(&"coord.store_failed".to_string()),
        "{out}"
    );
    same(&before, &untouched(&repo), "status --no-observe");
    ok(&wa, "a", 1, &["--read-only", "status"]);
    same(&before, &untouched(&repo), "--read-only status");
    ok(&wa, "a", 2, &["status"]);
    assert_ne!(
        untouched(&repo).head,
        before.head,
        "a plain status records what it observed"
    );
}

#[test]
fn read_only_refuses_writes_before_writing() {
    let (_dir, repo, wa, wb) = repo();
    // No store yet: reads that need one refuse, status reads none, and
    // nothing is created.
    let (code, out) = run(&wb, "b", 0, &["--read-only", "msg", "inbox"]);
    assert_eq!(code, 1, "{out}");
    assert_eq!(codes(&out), vec!["store.read_only"], "{out}");
    ok(&wb, "b", 0, &["--read-only", "status"]);
    assert!(!store(&repo).exists(), "no store was created");

    two_messages(&wa, &wb);
    let before = untouched(&repo);
    for args in [
        &["--read-only", "claim", ".mdkg/config.json"][..],
        &[
            "--read-only",
            "msg",
            "send",
            "--to",
            "a",
            "--kind",
            "note",
            "--text",
            "hi",
        ],
        &["--read-only", "msg", "ack", "old"],
        &[
            "--read-only",
            "msg",
            "inbox",
            "--mark-shown",
            "1/sha256:x/old",
        ],
    ] {
        let (code, out) = run(&wb, "b", DAY + 1, args);
        assert_eq!(code, 1, "{args:?}: {out}");
        assert!(
            codes(&out).contains(&"store.read_only".to_string()),
            "{args:?}: {out}"
        );
        same(&before, &untouched(&repo), &format!("{args:?}"));
    }
}

/// One MCP `tools/call` of `tool` as `actor` at `now`, the server started
/// with `flags` before `mcp`: the envelope it replied with. The server is
/// then shut by closing its input and waited for, so whatever it does
/// after the reply (marking what it showed) is over.
fn mcp_call(
    worktree: &Path,
    actor: &str,
    now: u64,
    flags: &[&str],
    tool: &str,
    arguments: Value,
) -> Value {
    use std::io::{BufRead, BufReader, Write};
    use std::process::Stdio;
    let mut child = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
        .arg("--root")
        .arg(worktree)
        .args(["--as", actor])
        .args(flags)
        .arg("mcp")
        .env("MDKG_NOW_MS", now.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("start mcp");
    let request = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": tool, "arguments": arguments },
    });
    let mut stdin = child.stdin.take().expect("stdin");
    writeln!(stdin, "{request}").expect("write");
    stdin.flush().expect("flush");
    let mut line = String::new();
    BufReader::new(child.stdout.take().expect("stdout"))
        .read_line(&mut line)
        .expect("a reply");
    drop(stdin);
    child.wait().expect("mcp exits");
    let reply: Value = serde_json::from_str(&line).unwrap_or_else(|_| panic!("not JSON: {line}"));
    let text = reply["result"]["content"]
        .as_array()
        .and_then(|c| c.last())
        .and_then(|c| c["text"].as_str())
        .unwrap_or_else(|| panic!("no envelope: {reply}"));
    serde_json::from_str(text).expect("the envelope")
}

/// MCP's mail listings defer marking to after their reply, but stay normal
/// reads: a body past its retention is purged, on an empty page too
/// (#17186). Only `--no-mark` and `--read-only` read purely.
#[test]
fn mcp_mail_reads_still_purge_due_bodies() {
    let (_dir, repo, wa, wb) = repo();
    two_messages(&wa, &wb);
    let now = DAY + 1;
    // `a` has no mail: its empty page still purges `b`'s due body.
    let before = untouched(&repo);
    let empty = mcp_call(&wa, "a", now, &[], "inbox", serde_json::json!({}));
    assert_eq!(empty["ok"], true, "{empty}");
    assert!(bodies(&empty).is_empty(), "{empty}");
    let after = untouched(&repo);
    assert_eq!(after.bodies, 1, "the empty page purged the due body");
    assert_ne!(after.head, before.head, "the purge is a record");

    // And a listing that carries mail purges too.
    let (_dir, repo, wa, wb) = self::repo();
    two_messages(&wa, &wb);
    let listed = mcp_call(&wb, "b", now, &[], "inbox", serde_json::json!({}));
    assert_eq!(listed["ok"], true, "{listed}");
    assert_eq!(
        bodies(&listed),
        vec![
            ("old".to_string(), "purged".to_string()),
            ("new".to_string(), "present".to_string()),
        ],
        "{listed}"
    );
    assert_eq!(
        untouched(&repo).bodies,
        1,
        "the listing purged the due body"
    );
}

/// `--read-only` is an allowlist (#17282): every command but `status`,
/// `msg inbox`, `msg wait` and `msg thread` is refused with
/// `store.read_only` before it runs. Servers too, which would otherwise run
/// writing commands for their clients. Nothing in the store, the graph or
/// git changes.
#[test]
fn read_only_runs_only_the_allowlisted_reads() {
    let (_dir, repo, wa, wb) = repo();
    two_messages(&wa, &wb);
    let before = untouched(&repo);
    let git_state = |dir: &Path| {
        let out = Command::new("git")
            .current_dir(dir)
            .args(["status", "--porcelain", "--ignored"])
            .output()
            .expect("git status");
        let head = Command::new("git")
            .current_dir(dir)
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("git rev-parse");
        (out.stdout, head.stdout)
    };
    let graph = git_state(&wb);
    // Commands that take --json: refused with a JSON envelope.
    for args in [
        &["--read-only", "list"][..],
        &["--read-only", "show", "task-1"],
        &["--read-only", "search", "x"],
        &["--read-only", "next"],
        &["--read-only", "renew"],
        &["--read-only", "msg", "watch"],
        &["--read-only", "msg", "ack", "old"],
        &["--read-only", "migrate", "--rollback"],
        &["--read-only", "agents", "status"],
    ] {
        let (code, out) = run(&wb, "b", DAY + 1, args);
        assert_eq!(code, 1, "{args:?}: {out}");
        assert_eq!(codes(&out), vec!["store.read_only"], "{args:?}: {out}");
        same(&before, &untouched(&repo), &format!("{args:?}"));
    }
    // Servers, refused before they read a request or bind a port.
    for args in [&["--read-only", "mcp"][..], &["--read-only", "serve"]] {
        let out = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
            .current_dir(&wb)
            .args(["--as", "b"])
            .args(args)
            .output()
            .expect("run mdkg");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(out.status.code(), Some(1), "{args:?}: {text}");
        assert!(text.contains("store.read_only"), "{args:?}: {text}");
        same(&before, &untouched(&repo), &format!("{args:?}"));
    }
    assert!(git_state(&wb) == graph, "the worktree or HEAD changed");
    // The allowlisted reads still run.
    ok(&wb, "b", DAY + 1, &["--read-only", "status"]);
    ok(&wb, "b", DAY + 1, &["--read-only", "msg", "inbox"]);
    ok(&wb, "b", DAY + 1, &["--read-only", "msg", "thread", "new"]);
    ok(
        &wb,
        "b",
        DAY + 1,
        &["--read-only", "msg", "wait", "--timeout", "0"],
    );
    same(&before, &untouched(&repo), "the allowlisted reads");
}

/// A store that needs an upgrade is refused under `--read-only` with
/// `store.read_only` (exit 1), as the flag documents, and left as it was.
#[test]
fn read_only_refuses_a_store_that_needs_an_upgrade() {
    let (_dir, repo, wa, wb) = repo();
    let (_other, _, elsewhere, _) = self::repo();
    two_messages(&wa, &wb);
    rusqlite::Connection::open(store(&repo))
        .expect("store")
        .execute("UPDATE meta SET value = '1' WHERE key = 'schema'", [])
        .expect("an older schema");
    let before = untouched(&repo);
    for args in [
        &["--read-only", "status"][..],
        &["--read-only", "msg", "inbox"],
        &["--read-only", "msg", "thread", "old"],
    ] {
        // Run from another repository's folder: the hint names the
        // repository --root reads, not the folder it was run from.
        let (code, out) = run_from(&elsewhere, &wb, "b", DAY + 1, args);
        assert_eq!(code, 1, "{args:?}: {out}");
        assert_eq!(codes(&out), vec!["store.read_only"], "{args:?}: {out}");
        let hint = out["diagnostics"][0]["hint"].as_str().unwrap_or_default();
        assert!(hint.contains("renew"), "{args:?}: {out}");
        assert!(hint.contains(&*wb.to_string_lossy()), "{args:?}: {out}");
        assert!(
            !hint.contains(&*elsewhere.to_string_lossy()),
            "{args:?}: {out}"
        );
        same(&before, &untouched(&repo), &format!("{args:?}"));
    }
}

/// `msg wait` and `msg thread` with the MCP server's `--defer-mark` purge a
/// due body as a normal read does, but mark nothing: mail they listed
/// can't be acked until it is listed normally (#17242). MCP's
/// `inbox_wait` purges the same way.
#[test]
fn deferred_wait_and_thread_reads_purge_but_mark_nothing() {
    let (_dir, repo, wa, wb) = repo();
    for (id, now) in [("old", 0), ("new", DAY - 1)] {
        ok(
            &wa,
            "a",
            now,
            &[
                "msg", "send", "--to", "b", "--kind", "note", "--id", id, "--text", "for b",
            ],
        );
    }
    let now = DAY + 1;
    let not_acked = |what: &str| {
        let (_, refused) = run(&wb, "b", now, &["msg", "ack", "new"]);
        assert!(
            codes(&refused).contains(&"msg.not_shown".to_string()),
            "{what} marked `new` shown: {refused}"
        );
    };
    let thread = ok(&wb, "b", now, &["msg", "thread", "new", "--defer-mark"]);
    assert_eq!(
        bodies(&thread),
        vec![("new".to_string(), "present".to_string())],
        "{thread}"
    );
    assert_eq!(
        untouched(&repo).bodies,
        1,
        "the thread read purged the due body"
    );
    not_acked("msg thread --defer-mark");
    let waited = ok(
        &wb,
        "b",
        now,
        &["msg", "wait", "--timeout", "1", "--defer-mark"],
    );
    assert!(
        bodies(&waited).contains(&("new".to_string(), "present".to_string())),
        "{waited}"
    );
    not_acked("msg wait --defer-mark");
    // A normal listing marks it, and the ack goes through.
    ok(&wb, "b", now, &["msg", "inbox"]);
    let acked = ok(&wb, "b", now, &["msg", "ack", "new"]);
    assert_eq!(
        acked["data"]["acked"].as_array().map(Vec::len),
        Some(1),
        "{acked}"
    );

    // MCP's inbox_wait purges a due body too.
    let (_dir, repo, wa, wb) = self::repo();
    two_messages(&wa, &wb);
    let listed = mcp_call(
        &wb,
        "b",
        now,
        &[],
        "inbox_wait",
        serde_json::json!({ "timeout": 1 }),
    );
    assert_eq!(listed["ok"], true, "{listed}");
    assert_eq!(untouched(&repo).bodies, 1, "inbox_wait purged the due body");
}

/// A store left with a hot rollback journal by a crash needs recovery,
/// which is a write: `--read-only` refuses with `store.read_only`, names a
/// writing command, and leaves the database and journal bytes alone.
#[test]
fn read_only_refuses_a_store_that_needs_crash_recovery() {
    let (_dir, repo, wa, wb) = repo();
    two_messages(&wa, &wb);
    let path = store(&repo);
    let (journal, database_bytes, journal_bytes) = crash(&path);
    for args in [
        &["--read-only", "msg", "inbox"][..],
        &["--read-only", "msg", "thread", "old"],
    ] {
        let (code, out) = run(&wb, "b", DAY + 1, args);
        assert_eq!(code, 1, "{args:?}: {out}");
        assert_eq!(codes(&out), vec!["store.read_only"], "{args:?}: {out}");
        let hint = out["diagnostics"][0]["hint"].as_str().unwrap_or_default();
        assert!(hint.contains("msg inbox"), "{args:?}: {out}");
        assert!(hint.contains(&*wb.to_string_lossy()), "{args:?}: {out}");
        assert!(
            fs::read(&path).expect("database") == database_bytes,
            "{args:?}"
        );
        assert!(
            fs::read(&journal).expect("journal") == journal_bytes,
            "{args:?}"
        );
    }
}

/// Leaves the store at `path` as a writer that spilled pages and then
/// "crashed" left it: the database and its hot journal as they were
/// mid-transaction. Returns the journal's path and both files' bytes.
fn crash(path: &Path) -> (PathBuf, Vec<u8>, Vec<u8>) {
    let writer = rusqlite::Connection::open(path).expect("writer");
    writer
        .execute_batch(
            "PRAGMA journal_mode=DELETE; PRAGMA cache_size=1; BEGIN IMMEDIATE;
             CREATE TABLE spill(x TEXT);",
        )
        .expect("rollback mode");
    for _ in 0..200 {
        writer
            .execute("INSERT INTO spill(x) VALUES (?1)", ["x".repeat(4000)])
            .expect("spill pages");
    }
    let journal = path.with_file_name("coord.sqlite-journal");
    let database_bytes = fs::read(path).expect("hot database");
    let journal_bytes = fs::read(&journal).expect("hot journal");
    writer.execute_batch("ROLLBACK;").expect("rollback");
    drop(writer);
    fs::write(path, &database_bytes).expect("restore hot database");
    fs::write(&journal, &journal_bytes).expect("restore hot journal");
    (journal, database_bytes, journal_bytes)
}

/// A graph in a folder inside its checkout: a refusal names that folder
/// as the --root to write with, not the checkout's top, which has no graph
/// (#17326). Both the upgrade and the recovery hints.
#[test]
fn refusal_hints_name_a_nested_graph_root() {
    let (_dir, repo, wa, wb) = repo_with_graph_in("graph");
    let (_other, _, elsewhere, _) = self::repo();
    two_messages(&wa, &wb);
    let named = format!("--root {}", wb.display());
    let top = format!("--root {} ", wb.parent().expect("top").display());
    let check = |args: &[&str], writer: &str| {
        let (code, out) = run_from(&elsewhere, &wb, "b", DAY + 1, args);
        assert_eq!(code, 1, "{args:?}: {out}");
        assert_eq!(codes(&out), vec!["store.read_only"], "{args:?}: {out}");
        let hint = out["diagnostics"][0]["hint"].as_str().unwrap_or_default();
        assert!(hint.contains(writer), "{args:?}: {out}");
        assert!(hint.contains(&named), "{args:?}: {out}");
        assert!(!hint.contains(&top), "{args:?}: {out}");
        // Only the hint says what to run (#17360).
        let message = out["diagnostics"][0]["message"]
            .as_str()
            .unwrap_or_default();
        assert!(!message.contains(writer), "{args:?}: {out}");
        assert!(!message.contains("--root"), "{args:?}: {out}");
    };
    let path = store(&repo);
    let (journal, database_bytes, journal_bytes) = crash(&path);
    for args in [
        &["--read-only", "msg", "inbox"][..],
        &["--read-only", "msg", "thread", "old"],
    ] {
        check(args, "msg inbox");
        assert!(fs::read(&path).expect("database") == database_bytes);
        assert!(fs::read(&journal).expect("journal") == journal_bytes);
    }
    // A writer recovers it, as the hint says; then it needs an upgrade.
    ok(&wb, "b", DAY + 1, &["msg", "inbox"]);
    rusqlite::Connection::open(&path)
        .expect("store")
        .execute("UPDATE meta SET value = '1' WHERE key = 'schema'", [])
        .expect("an older schema");
    let before = untouched(&repo);
    for args in [
        &["--read-only", "status"][..],
        &["--read-only", "msg", "inbox"],
    ] {
        check(args, "renew");
        same(&before, &untouched(&repo), &format!("{args:?}"));
    }
    // Without --read-only the read is refused the same way, as a store
    // failure, and its hint names the nested root too.
    let (code, out) = run_from(&elsewhere, &wb, "b", DAY + 1, &["status"]);
    assert_eq!(code, 4, "{out}");
    assert_eq!(codes(&out), vec!["coord.store_failed"], "{out}");
    let hint = out["diagnostics"][0]["hint"].as_str().unwrap_or_default();
    assert!(hint.contains("renew"), "{out}");
    assert!(hint.contains(&named), "{out}");
    assert!(!hint.contains("--read-only"), "{out}");
    same(&before, &untouched(&repo), "status");
}
