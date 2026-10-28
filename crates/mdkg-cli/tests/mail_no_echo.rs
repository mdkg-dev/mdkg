//! Mail and store refusals quote nothing the caller typed (edd-4: no id,
//! context, room or mailbox, and the actor is a mailbox too), and a store
//! copy that can't be read isn't reported as missing (task-45).

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
        .env("MDKG_WAIT_POLL_MS", "20")
}

fn git(dir: &Path, args: &[&str]) {
    let out = isolated(Command::new("git").arg("-C").arg(dir).args(args))
        .output()
        .expect("git runs");
    assert!(out.status.success(), "git {args:?}");
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

fn repo(case: &str) -> Scratch {
    let dir = common::temp::dir(&format!("mdkg-cli-no-echo-{case}"));
    let conformance = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance");
    copy_tree(&conformance.join("base"), &dir);
    copy_tree(&conformance.join("cases/g01-valid-basic"), &dir);
    fs::remove_file(dir.join("commands")).ok();
    git(&dir, &["init", "-q", "-b", "main"]);
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-q", "-m", "init"]);
    Scratch(fs::canonicalize(dir).expect("canonical"))
}

/// Runs mdkg as `actor`: (exit code, envelope).
fn run(dir: &Path, actor: &str, args: &[&str]) -> (i32, Value) {
    let out = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
        .arg("--root")
        .arg(dir)
        .args(["--as", actor])
        .args(args)
        .arg("--json")
        .output()
        .expect("run mdkg");
    let envelope = serde_json::from_slice(&out.stdout).unwrap_or_else(|_| {
        panic!(
            "not JSON: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    });
    (out.status.code().unwrap_or(-1), envelope)
}

/// Every diagnostic's message and hint.
fn texts(envelope: &Value) -> String {
    envelope["diagnostics"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|d| format!("{} {}", d["message"], d["hint"]))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn mail_refusals_quote_no_typed_mailbox_room_or_actor() {
    let repo = repo("mail");
    let actor = "markeractor7";
    let (code, out) = run(&repo.0, "owner-a", &["room", "create", "markerroom7"]);
    assert_eq!(code, 0, "{out}");
    let not_json = repo.0.join("markerjson7.json");
    fs::write(&not_json, "{ not json").expect("write");
    let not_json = not_json.to_str().expect("utf-8");
    let cases: [&[&str]; 7] = [
        // A mailbox the actor doesn't hold.
        &["msg", "inbox", "--mailbox", "role:markerrole7"],
        // Leaving a room the actor isn't in: a warning.
        &["room", "leave", "markerroom7"],
        // A wait that runs out.
        &["msg", "wait", "--timeout", "1"],
        // A malformed mailbox: the rule is in the hint, not the name.
        &[
            "msg",
            "send",
            "--to",
            "markerbad7 x",
            "--kind",
            "note",
            "--text",
            "hi",
        ],
        // Files named on the command line: no path is quoted.
        &[
            "msg",
            "send",
            "--to",
            "owner-a",
            "--kind",
            "note",
            "--text-file",
            "markerfile7",
        ],
        &[
            "msg",
            "send",
            "--to",
            "owner-a",
            "--kind",
            "note",
            "--data-file",
            not_json,
        ],
        &[
            "msg",
            "import",
            "--from",
            "codex:x",
            "--to",
            "owner-a",
            "--kind",
            "note",
            "markerfile7",
        ],
    ];
    for args in cases {
        let (_, out) = run(&repo.0, actor, args);
        let said = texts(&out);
        assert!(!said.is_empty(), "{args:?} said nothing: {out}");
        for marker in [
            "markeractor7",
            "markerroom7",
            "markerrole7",
            "markerbad7",
            "markerfile7",
            "markerjson7",
        ] {
            assert!(!said.contains(marker), "{args:?} quoted {marker}: {said}");
        }
    }
}

#[test]
#[cfg(unix)]
fn a_store_copy_that_cant_be_read_is_not_reported_missing() {
    use std::os::unix::fs::PermissionsExt;
    let repo = repo("restore");
    // A store, so there is one to restore over.
    let (code, out) = run(&repo.0, "owner-a", &["claim", "task-1"]);
    assert_eq!(code, 0, "{out}");
    let locked = repo.0.join("locked");
    fs::create_dir_all(&locked).expect("dir");
    fs::write(locked.join("copy.sqlite"), "not a store").expect("copy");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("chmod");
    let copy = locked.join("copy.sqlite");
    let (code, out) = run(
        &repo.0,
        "owner-a",
        &["store", "restore", copy.to_str().expect("utf-8")],
    );
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).expect("chmod back");
    assert_ne!(code, 0, "{out}");
    assert_ne!(code, 3, "an unreadable copy isn't a missing one: {out}");
    // A copy that really isn't there is not found.
    let gone = repo.0.join("gone.sqlite");
    let (code, out) = run(
        &repo.0,
        "owner-a",
        &["store", "restore", gone.to_str().expect("utf-8")],
    );
    assert_eq!(code, 3, "{out}");
    assert_eq!(out["diagnostics"][0]["code"], "io.read_failed", "{out}");
}

/// The audit's E1, E2, E4 and E17 (task-45): coordinator refusals say what
/// to do next, and none quotes what was typed.
#[test]
fn coordinator_refusals_have_hints_and_quote_nothing_typed() {
    let repo = repo("coord");
    let (code, out) = run(&repo.0, "owner-a", &["claim", "task-1"]);
    assert_eq!(code, 0, "{out}");
    // Held by someone else: a hint to look elsewhere.
    let (code, out) = run(&repo.0, "owner-b", &["claim", "task-1"]);
    assert_eq!(code, 5, "{out}");
    assert_eq!(out["diagnostics"][0]["code"], "coord.lease_held", "{out}");
    assert!(out["diagnostics"][0]["hint"].is_string(), "{out}");
    // Nothing held there (a path that exists, so it's a path).
    std::fs::write(repo.0.join("markerpath7.md"), "x").expect("file");
    let cases: [&[&str]; 3] = [
        &["release", "markerpath7.md"],
        // Not a changeset id; no changesets from a branch.
        &["abandon", "markercs7"],
        &["abandon", "--branch", "markerbranch7"],
    ];
    for args in cases {
        let (code, out) = run(&repo.0, "owner-b", args);
        assert_ne!(code, 0, "{args:?}: {out}");
        let said = texts(&out);
        assert!(
            out["diagnostics"][0]["hint"].is_string(),
            "{args:?} has no hint: {out}"
        );
        for marker in ["markerpath7", "markercs7", "markerbranch7", "markerhandle7"] {
            assert!(!said.contains(marker), "{args:?} quoted {marker}: {said}");
        }
    }
}

/// The orchestrator's E2/E4 ruling (2026-10-25): a handle that matches
/// nothing, or several nodes, is reported in one wording everywhere,
/// 0.6.0's aliases too, without quoting what was typed; the matches go in
/// the hint.
#[test]
fn unresolved_handles_use_one_wording_and_quote_nothing() {
    let repo = repo("handles");
    for args in [
        &["show", "markerhandle7"][..],
        &["claim", "markerhandle7"],
        &["task", "start", "markerhandle7"],
    ] {
        let (code, out) = run(&repo.0, "owner-a", args);
        assert_eq!(code, 3, "{args:?}: {out}");
        let d = &out["diagnostics"][0];
        assert_eq!(d["code"], "node.not_found", "{args:?}: {out}");
        assert_eq!(
            d["message"], "no node matches that handle",
            "{args:?}: {out}"
        );
        assert!(d["hint"].is_string(), "{args:?}: {out}");
        assert!(!texts(&out).contains("markerhandle7"), "{args:?}: {out}");
    }
    // "task" is in both titles: the count, and the matches in the hint.
    let (code, out) = run(&repo.0, "owner-a", &["show", "task"]);
    assert_eq!(code, 7, "{out}");
    let d = &out["diagnostics"][0];
    assert_eq!(d["message"], "that handle names 2 nodes", "{out}");
    let hint = d["hint"].as_str().unwrap_or_default();
    assert!(
        hint.contains("Basic task") && hint.contains("Other task"),
        "{out}"
    );
}

/// The write commands, under the same rulings (D2, D3, E2/E4): an edge
/// flag's unknown handle, an unknown type and a node that isn't a goal are
/// refused with their codes and nothing typed quoted, and each goal
/// subcommand has its own schema.
#[test]
fn write_commands_use_one_code_each_and_quote_nothing() {
    let repo = repo("writes");
    let (code, out) = run(
        &repo.0,
        "owner-a",
        &["new", "task", "x", "--parent", "markerparent7"],
    );
    assert_eq!(
        (code, out["diagnostics"][0]["code"].as_str()),
        (3, Some("node.not_found")),
        "{out}"
    );
    let hint = out["diagnostics"][0]["hint"].as_str().unwrap_or_default();
    assert!(hint.contains("--parent"), "{out}");
    assert!(!texts(&out).contains("markerparent7"), "{out}");
    let (code, out) = run(&repo.0, "owner-a", &["new", "markertype7", "x"]);
    assert_eq!(
        (code, out["diagnostics"][0]["code"].as_str()),
        (1, Some("usage.unknown_type")),
        "{out}"
    );
    assert!(!texts(&out).contains("markertype7"), "{out}");
    let (code, out) = run(&repo.0, "owner-a", &["goal", "next", "task-1"]);
    assert_eq!(
        (code, out["diagnostics"][0]["code"].as_str()),
        (1, Some("goal.not_goal")),
        "{out}"
    );
    assert_eq!(out["schema"], "mdkg.goal.next.v1", "{out}");
    assert!(
        !out["diagnostics"][0]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("task-1")
    );
    let (_, out) = run(&repo.0, "owner-a", &["goal", "current"]);
    assert_eq!(out["schema"], "mdkg.goal.current.v1", "{out}");
    assert!(out["data"]["leases"].is_array(), "{out}");
}
