//! `msg list` end to end (edd-01a1a1f1): a person reads mail across
//! mailboxes by kind, outside agent turns, and the store doesn't change.
//! Also the lead read an agent's turn takes from its own mailbox: a
//! no-mark sender-filtered read, then `--mark-shown` of exactly what it
//! delivers (dec-01a18ee0 items 14 and 26).

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

fn isolated(cmd: &mut Command) -> &mut Command {
    common::without_launcher_env(cmd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
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

/// A committed graph in a fresh repository.
fn repo() -> (Scratch, PathBuf) {
    let dir = Scratch(fs::canonicalize(common::temp::dir("mdkg-cli-msg-list")).expect("canonical"));
    let repo = dir.0.join("repo");
    let conformance = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance");
    fs::create_dir_all(&repo).expect("repo");
    copy_tree(&conformance.join("base"), &repo);
    copy_tree(&conformance.join("cases/g01-valid-basic"), &repo);
    fs::remove_file(repo.join("commands")).ok();
    fs::remove_file(repo.join("README.md")).ok();
    sh(&repo, &["init", "-q", "-b", "main"]);
    sh(&repo, &["add", "."]);
    sh(&repo, &["commit", "-q", "-m", "init"]);
    (dir, repo)
}

/// Runs mdkg as `actor`, with `env` set: (exit code, envelope).
fn run_env(root: &Path, actor: &str, env: &[(&str, &str)], args: &[&str]) -> (i32, Value) {
    run_in(root, root, actor, env, args)
}

/// [`run_env`] from the working folder `cwd`, with `--root root`.
fn run_in(
    cwd: &Path,
    root: &Path,
    actor: &str,
    env: &[(&str, &str)],
    args: &[&str],
) -> (i32, Value) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mdkg"));
    // From the fixture, as a person's shell would be: the turn check also
    // looks at the working folder's repository, and the test's own folder
    // may be one where an agent's turn is running.
    isolated(&mut cmd)
        .current_dir(cwd)
        .arg("--root")
        .arg(root)
        .args(["--as", actor])
        .args(args)
        .arg("--json");
    for (key, value) in env {
        cmd.env(key, value);
    }
    let out = cmd.output().expect("run mdkg");
    let envelope: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|_| {
        panic!(
            "not JSON: {}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    });
    // `schemas/` is generated at integration: before then this branch
    // has no file for `msg list`, and its envelopes are checked once
    // there is (the schemas test makes sure there is).
    let schema = envelope["schema"].as_str().unwrap_or_default();
    let file = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../schemas")
        .join(format!("{schema}.json"));
    if file.exists() {
        common::conforms_envelope(&envelope);
    }
    (out.status.code().unwrap_or(-1), envelope)
}

fn run(root: &Path, actor: &str, args: &[&str]) -> (i32, Value) {
    run_env(root, actor, &[], args)
}

fn ok(root: &Path, actor: &str, args: &[&str]) -> Value {
    let (code, out) = run(root, actor, args);
    assert_eq!(code, 0, "{args:?}: {out}");
    out
}

fn code_of(envelope: &Value) -> String {
    envelope["diagnostics"][0]["code"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

fn ids(envelope: &Value) -> Vec<String> {
    envelope["data"]["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m["messageId"].as_str().map(String::from))
        .collect()
}

fn send(root: &Path, from: &str, to: &str, kind: &str, id: &str) -> Value {
    ok(
        root,
        from,
        &[
            "msg", "send", "--to", to, "--kind", kind, "--id", id, "--text", "body",
        ],
    )["data"]["message"]
        .clone()
}

/// The store's database and WAL: a read-only open may create an empty
/// WAL where none was, and nothing else may change.
fn store_files(repo: &Path) -> Vec<(String, Vec<u8>)> {
    let dir = repo.join(".git").join("mdkg");
    let mut files: Vec<(String, Vec<u8>)> = ["coord.sqlite", "coord.sqlite-wal"]
        .iter()
        .filter_map(|name| {
            let bytes = fs::read(dir.join(name)).ok()?;
            Some((name.to_string(), bytes))
        })
        .filter(|(name, bytes)| !(name.ends_with("-wal") && bytes.is_empty()))
        .collect();
    files.sort();
    files
}

#[test]
fn a_person_lists_mail_across_mailboxes_by_kind() {
    let (_dir, repo) = repo();
    send(&repo, "lane-a", "rev-1,rev-2", "question", "q1");
    send(&repo, "rev-1", "lane-a", "findings", "f1");
    send(&repo, "lane-b", "orchestrator", "note", "n1");
    let out = ok(
        &repo,
        "human:nick",
        &["msg", "list", "--kind", "question,findings"],
    );
    assert_eq!(out["schema"], "mdkg.msg.list.v1");
    assert_eq!(ids(&out), ["q1", "f1"]);
    assert_eq!(out["data"]["more"], false);
    assert!(out["data"]["event_cursor"].is_string());
    assert_eq!(out["data"]["messages"][0]["kind"], "question");
    assert!(out["data"]["messages"][0]["parts"].is_array());
    // Scope and sender.
    let out = ok(
        &repo,
        "human:nick",
        &["msg", "list", "--to", "rev-2", "--from", "lane-a"],
    );
    assert_eq!(ids(&out), ["q1"]);
    assert_eq!(out["data"]["mailboxes"], serde_json::json!(["rev-2"]));
    // Paging.
    let one = ok(&repo, "human:nick", &["msg", "list", "--limit", "2"]);
    assert_eq!(ids(&one), ["q1", "f1"]);
    assert_eq!(one["data"]["more"], true);
    let cursor = one["data"]["cursor"].to_string();
    let two = ok(
        &repo,
        "human:nick",
        &["msg", "list", "--limit", "2", "--after", &cursor],
    );
    assert_eq!(ids(&two), ["n1"]);
    // Bounds and arguments.
    let (code, out) = run(&repo, "human:nick", &["msg", "list", "--limit", "201"]);
    assert_eq!((code, code_of(&out).as_str()), (1, "usage.bad_limit"));
    let (code, out) = run(&repo, "human:nick", &["msg", "list", "--state", "open"]);
    assert_eq!(
        (code, code_of(&out).as_str()),
        (1, "usage.invalid_arguments")
    );
    let (code, out) = run(&repo, "human:nick", &["msg", "list", "--channel", "nope"]);
    assert_eq!((code, code_of(&out).as_str()), (3, "channel.unknown"));
    // A kind no build sends is refused as `msg inbox` refuses it, alone or
    // beside a known one, rather than reading as no open mail; so is a
    // sender that isn't a name.
    for kinds in ["fualt", "fault,fualt"] {
        let (code, out) = run(&repo, "human:nick", &["msg", "list", "--kind", kinds]);
        assert_eq!(
            (code, code_of(&out).as_str()),
            (1, "msg.bad_kind"),
            "{kinds}: {out}"
        );
        assert!(out["data"].is_null(), "{out}");
    }
    let (code, out) = run(&repo, "human:nick", &["msg", "list", "--from", "lane a"]);
    assert_eq!((code, code_of(&out).as_str()), (1, "msg.bad_id"), "{out}");
}

#[test]
fn only_a_person_outside_an_agent_turn_lists() {
    let (_dir, repo) = repo();
    send(&repo, "lane-a", "orchestrator", "question", "q1");
    // A well-formed actor that isn't a person: msg list's own refusal.
    // Names no command takes are refused first, with the codes every
    // command gives them: a role or a turn is never an actor, and a
    // human name must be a valid mailbox.
    for (actor, code) in [
        ("lane-a", "api.actor_not_allowed"),
        ("orchestrator", "api.actor_not_allowed"),
        ("HUMAN:nick", "msg.bad_id"),
        ("role:reviewer", "msg.reserved_actor"),
    ] {
        let (exit, out) = run(&repo, actor, &["msg", "list"]);
        assert_eq!((exit, code_of(&out).as_str()), (1, code), "{actor}: {out}");
        assert!(out["data"].is_null(), "{actor} read nothing: {out}");
    }
    // A person inside an agent's turn (the launcher's turn mark; the same
    // for Claude, Codex and OpenCode turns, since the check doesn't depend
    // on the harness) is refused by the live-turn guard, with its own code.
    let before = store_files(&repo);
    let (code, out) = run_env(
        &repo,
        "human:nick",
        &[("MDKG_RUN_TURN", "1")],
        &["msg", "list"],
    );
    assert_eq!((code, code_of(&out).as_str()), (1, "run.turn_active"));
    assert!(out["data"].is_null());
    assert_eq!(store_files(&repo), before);
    // A person under a live launcher turn with no mark (edd test plan
    // item 9): the rule a person's `msg send` uses, the turn's actor and
    // the launcher ancestry, never the checkout the turn works in.
    let refused = |cwd: &Path, why: &str| {
        let (code, out) = run_in(cwd, &repo, "human:nick", &[], &["msg", "list"]);
        assert_eq!(
            (code, code_of(&out).as_str()),
            (1, "run.turn_active"),
            "{why}: {out}"
        );
        assert!(out["data"].is_null(), "{why}: {out}");
        assert_eq!(store_files(&repo), before, "{why}");
    };
    let lists = |cwd: &Path, why: &str| {
        let (code, out) = run_in(cwd, &repo, "human:nick", &[], &["msg", "list"]);
        assert_eq!(code, 0, "{why}: {out}");
        assert_eq!(ids(&out), ["q1"], "{why}: {out}");
    };
    let unrelated = Reaped(Command::new("sleep").arg("120").spawn().expect("sleep"));
    // 1. A turn another process launched, working in the checkout the
    // list targets and runs in: an agent at work there doesn't hold up a
    // person's read.
    let turn = live_turn(&repo, &repo, "lane-x", unrelated.id());
    lists(
        &repo,
        "a turn in the same checkout that didn't launch this process",
    );
    drop(turn);
    // 2. Ancestry: a turn launched by this test process, so the mdkg it
    // runs is the turn's descendant, in another worktree and in this one.
    let wt = repo.parent().expect("scratch").join("wt");
    sh(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "wt",
            wt.to_str().expect("utf-8"),
        ],
    );
    let turn = live_turn(&repo, &wt, "lane-x", std::process::id());
    refused(&repo, "a descendant of a turn in another worktree");
    drop(turn);
    let turn = live_turn(&repo, &repo, "lane-x", std::process::id());
    refused(&repo, "a descendant of a turn in this checkout");
    drop(turn);
    let turn = live_turn(&repo, &wt, "lane-x", unrelated.id());
    lists(&repo, "a turn elsewhere that didn't launch this process");
    drop(turn);
    // 3. The person's own live turn refuses, wherever it works.
    let turn = live_turn(&repo, &wt, "human:nick", unrelated.id());
    refused(&repo, "a live turn whose actor is the person");
    drop(turn);
    // 4. The working folder's repository is read too: run from another
    // repository, a descendant of a turn there is refused, and a turn
    // there that another process launched isn't.
    let other = repo.parent().expect("scratch").join("other");
    fs::create_dir_all(&other).expect("other");
    sh(&other, &["init", "-q", "-b", "main"]);
    let turn = live_turn(&other, &other, "lane-y", std::process::id());
    refused(
        &other,
        "run from a repository where this process launched a turn",
    );
    drop(turn);
    let turn = live_turn(&other, &other, "lane-y", unrelated.id());
    lists(&other, "run from a repository with another process's turn");
    drop(turn);
}

/// A child process killed and reaped when dropped, a failed assertion's
/// unwinding included, so it never outlives its test.
struct Reaped(std::process::Child);

impl Reaped {
    fn id(&self) -> u32 {
        self.0.id()
    }
}

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A launcher turn record in `repo`'s common folder for `actor`, working
/// in `cwd`, held live by its lock, whose launcher is the process
/// `launcher`, as the turn check reads it (the fixture `tests/serve.rs`
/// uses).
fn live_turn(repo: &Path, cwd: &Path, actor: &str, launcher: u32) -> fs::File {
    let out = Command::new("ps")
        .env("TZ", "UTC")
        .env("LC_ALL", "C")
        .args(["-o", "lstart=", "-p", &launcher.to_string()])
        .output()
        .expect("ps");
    let started = String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let dir = mdkg_run::hooks::dir(&repo.join(".git")).join("turns");
    fs::create_dir_all(&dir).expect("turns dir");
    let path = dir.join(format!("{actor}.json"));
    let record = serde_json::json!({
        "actor": actor,
        "harness": "claude",
        "cwd": cwd,
        "launcher_process": {"pid": launcher, "started": started},
    });
    fs::write(&path, record.to_string()).expect("record");
    let file = fs::OpenOptions::new().read(true).open(&path).expect("open");
    file.lock().expect("lock");
    file
}

/// The actor comes from `$MDKG_ACTOR` when there's no `--as`, as for
/// every command.
#[test]
fn a_person_named_by_mdkg_actor_lists() {
    let (_dir, repo) = repo();
    send(&repo, "lane-a", "orchestrator", "question", "q1");
    let run_as_env = |actor: &str| -> (i32, Value) {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_mdkg"));
        isolated(&mut cmd)
            .current_dir(&repo)
            .env("MDKG_ACTOR", actor)
            .arg("--root")
            .arg(&repo)
            .args(["msg", "list", "--json"]);
        let out = cmd.output().expect("run mdkg");
        let envelope: Value = serde_json::from_slice(&out.stdout).expect("JSON");
        (out.status.code().unwrap_or(-1), envelope)
    };
    let (code, out) = run_as_env("human:nick");
    assert_eq!(code, 0, "{out}");
    assert_eq!(ids(&out), ["q1"]);
    let (code, out) = run_as_env("lane-a");
    assert_eq!((code, code_of(&out).as_str()), (1, "api.actor_not_allowed"));
}

#[test]
fn a_list_leaves_the_store_bytes_unchanged() {
    let (_dir, repo) = repo();
    send(&repo, "lane-a", "orchestrator", "question", "q1");
    send(&repo, "lane-b", "orchestrator", "question", "q2");
    let before = store_files(&repo);
    assert!(!before.is_empty(), "the store exists");
    for args in [
        &["msg", "list"][..],
        &["msg", "list", "--state", "any", "--kind", "question"][..],
        &["msg", "list", "--to", "orchestrator"][..],
    ] {
        ok(&repo, "human:nick", args);
        assert_eq!(store_files(&repo), before, "{args:?}");
    }
    // Nothing was marked shown: the recipient can't ack what it never saw.
    let out = ok(
        &repo,
        "orchestrator",
        &["msg", "ack", "--through", "1000", "--kind", "question"],
    );
    assert_eq!(out["data"]["acked"], serde_json::json!([]));
    assert_eq!(out["data"]["skipped"][0]["reason"], "not_shown");
}

/// Item 14's lead read: the oldest mail from `reports_to`, wherever it is
/// in the mailbox, without marking what it passes; then exactly the
/// delivered messages are marked, so only they can be acked.
#[test]
fn a_lead_read_marks_only_what_it_delivers() {
    let (_dir, repo) = repo();
    for i in 1..=3 {
        send(&repo, "other", "lane", "question", &format!("o{i}"));
    }
    let lead = send(&repo, "lead", "lane", "question", "l1");
    send(&repo, "other", "lane", "question", "o4");
    let found = ok(
        &repo,
        "lane",
        &[
            "msg",
            "inbox",
            "--from",
            "lead",
            "--no-mark",
            "--limit",
            "1",
        ],
    );
    assert_eq!(ids(&found), ["l1"], "found behind older mail");
    let shown = format!(
        "{}/{}/{}",
        lead["seq"],
        lead["digest"].as_str().expect("digest"),
        lead["messageId"].as_str().expect("id")
    );
    let marked = ok(&repo, "lane", &["msg", "inbox", "--mark-shown", &shown]);
    assert!(ids(&marked).is_empty(), "a mark lists nothing");
    let out = ok(
        &repo,
        "lane",
        &["msg", "ack", "--through", "1000", "--kind", "question"],
    );
    let acked: Vec<&str> = out["data"]["acked"]
        .as_array()
        .expect("acked")
        .iter()
        .filter_map(|a| a["messageId"].as_str())
        .collect();
    assert_eq!(acked, ["l1"]);
    let skipped: Vec<&str> = out["data"]["skipped"]
        .as_array()
        .expect("skipped")
        .iter()
        .filter(|s| s["reason"] == "not_shown")
        .filter_map(|s| s["messageId"].as_str())
        .collect();
    assert_eq!(skipped, ["o1", "o2", "o3", "o4"]);
}

/// A person's client that shows mail it read itself (`msg list` marks
/// nothing) records what it showed with `msg inbox --mark-shown`, then
/// acks by id. Ack never skips the shown check (#20613's ruling): before
/// the mark, or with a mark that doesn't match the message, the ack is
/// refused and nothing is acked.
#[test]
fn a_person_marks_what_their_client_showed_then_acks_by_id() {
    let (_dir, repo) = repo();
    send(&repo, "lane-a", "human:nick", "question", "p1");
    let listed = ok(&repo, "human:nick", &["msg", "list", "--to", "human:nick"]);
    assert_eq!(ids(&listed), ["p1"]);
    let m = &listed["data"]["messages"][0];
    let seq = m["seq"].as_u64().expect("seq");
    let digest = m["digest"].as_str().expect("digest");
    // Read without a mark: the ack is refused.
    let (code, out) = run(&repo, "human:nick", &["msg", "ack", "p1"]);
    assert_eq!(
        (code, code_of(&out).as_str()),
        (5, "msg.not_shown"),
        "{out}"
    );
    assert!(
        out["diagnostics"][0]["hint"]
            .as_str()
            .is_some_and(|h| h.contains("--mark-shown")),
        "{out}"
    );
    // A mark naming another digest marks nothing.
    let wrong = format!("{seq}/sha256:{}/p1", "0".repeat(64));
    let marked = ok(
        &repo,
        "human:nick",
        &["msg", "inbox", "--mark-shown", &wrong],
    );
    assert!(ids(&marked).is_empty(), "{marked}");
    let (code, out) = run(&repo, "human:nick", &["msg", "ack", "p1"]);
    assert_eq!(
        (code, code_of(&out).as_str()),
        (5, "msg.not_shown"),
        "{out}"
    );
    // The mark the listing gives: the ack goes through.
    let shown = format!("{seq}/{digest}/p1");
    ok(
        &repo,
        "human:nick",
        &["msg", "inbox", "--mark-shown", &shown],
    );
    let acked = ok(&repo, "human:nick", &["msg", "ack", "p1"]);
    assert_eq!(acked["data"]["acked"][0]["messageId"], "p1", "{acked}");
}
