//! `new`, `done` and `retype` in a format 0.7 graph (docs/format-0.7.md),
//! through the binary, in throwaway repositories.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

mod common;

const GRAPH: &str = "0192f3a0-0000-7000-8000-000000000000";

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        if std::env::var_os("MDKG_KEEP_SCRATCH").is_none() {
            fs::remove_dir_all(&self.0).ok();
        }
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

fn sh(dir: &Path, args: &[&str]) -> String {
    let out = isolated(Command::new("git").arg("-C").arg(dir).args(args))
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
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

/// A committed repository holding the conformance base, as format 0.7
/// unless `legacy`, plus `files`.
fn repo(legacy: bool, files: &[(&str, String)]) -> Scratch {
    let dir = std::env::temp_dir().join(format!(
        "mdkg-cli-format07-{}-{}",
        std::process::id(),
        mdkg_format::Uuid7::now()
    ));
    fs::create_dir_all(&dir).expect("scratch");
    let dir = fs::canonicalize(dir).expect("canonical");
    copy_tree(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance/base"),
        &dir,
    );
    let path = dir.join(".mdkg/config.json");
    let mut config: Value =
        serde_json::from_str(&fs::read_to_string(&path).expect("config")).expect("json");
    if !legacy {
        config["schema_version"] = 3.into();
        config["format_version"] = "0.7".into();
        config["graph_id"] = GRAPH.into();
    }
    if config.get("workspaces").is_none() {
        config["workspaces"] =
            serde_json::json!({ "root": { "path": ".", "enabled": true, "mdkg_dir": ".mdkg" } });
    }
    fs::write(&path, config.to_string()).expect("write config");
    for (rel, text) in files {
        let p = dir.join(rel);
        fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
        fs::write(p, text).expect("write");
    }
    sh(&dir, &["init", "-q", "-b", "main"]);
    sh(&dir, &["add", "."]);
    sh(&dir, &["commit", "-q", "-m", "init"]);
    Scratch(dir)
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or_default())
        .unwrap_or_default()
}

fn run(root: &Path, args: &[&str]) -> (i32, Value) {
    let out = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
        .arg("--root")
        .arg(root)
        .args(["--as", "lane-a"])
        .args(args)
        .arg("--json")
        .env("MDKG_NOW_MS", now().to_string())
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

fn goal(id: &str) -> String {
    format!(
        "---\nid: {id}\ntype: goal\ntitle: Ship it\nstatus: todo\ngoal_state: active\ngoal_condition: done\nlegacy_id: goal-4\ncreated: 2026-10-24T00:00:00.000Z\nupdated: 2026-10-24T00:00:00.000Z\n---\n# Objective\n"
    )
}

const GOAL: &str = "goal-0192f3a4-0000-7000-8000-000000000001";

#[test]
fn new_mints_ids_and_names_files_by_type_and_slug() {
    let r = repo(false, &[(".mdkg/work/goal-ship.md", goal(GOAL))]);
    let a = ok(&r.0, &["new", "task", "Do it", "--parent", "goal-4"]);
    let id = a["data"]["node"]["id"].as_str().expect("id").to_string();
    let parsed = mdkg_format::NodeId::parse(&id).expect("a format 0.7 id");
    assert_eq!(parsed.node_type(), "task");
    assert_eq!(a["data"]["node"]["path"], ".mdkg/work/task-do-it.md");
    let text = fs::read_to_string(r.0.join(".mdkg/work/task-do-it.md")).expect("file");
    // The parent is written as its full id, whatever handle named it.
    assert!(text.contains(&format!("parent: {GOAL}\n")), "{text}");
    let created = text
        .lines()
        .find_map(|l| l.strip_prefix("created: "))
        .expect("created");
    assert!(mdkg_format::node::is_timestamp(created), "{created}");
    // `created` is the UUID's own millisecond.
    let ms: u64 = created[20..23].parse().expect("ms");
    assert_eq!(Some(ms), parsed.uuid().unix_ms().map(|t| t % 1000));
    assert!(text.contains(&format!("updated: {created}\n")));

    // The same title again: the next free name, and another id.
    let b = ok(&r.0, &["new", "task", "Do it"]);
    assert_eq!(b["data"]["node"]["path"], ".mdkg/work/task-do-it-2.md");
    assert_ne!(b["data"]["node"]["id"], a["data"]["node"]["id"]);
    let c = ok(&r.0, &["new", "checkpoint", "Did it"]);
    assert!(
        c["data"]["node"]["id"]
            .as_str()
            .is_some_and(|i| i.starts_with("checkpoint-"))
    );
    assert_eq!(c["data"]["node"]["path"], ".mdkg/work/checkpoint-did-it.md");
    let v = ok(&r.0, &["validate"]);
    assert!(!codes(&v).contains(&"node.stale_type".to_string()), "{v}");
    // Handles in scope and refs are written as full ids too; a URI stays.
    let d = ok(
        &r.0,
        &[
            "new",
            "checkpoint",
            "Scoped",
            "--scope",
            "goal-4",
            "--refs",
            "goal-4,https://x.dev/a",
        ],
    );
    let path = d["data"]["node"]["path"].as_str().expect("path");
    let text = fs::read_to_string(r.0.join(path)).expect("file");
    assert!(text.contains(&format!("scope: [{GOAL}]\n")), "{text}");
    assert!(
        text.contains(&format!("refs: [{GOAL}, https://x.dev/a]\n")),
        "{text}"
    );
    let (code, _) = run(&r.0, &["new", "checkpoint", "Bad", "--scope", "task-404"]);
    assert_eq!(code, 3);
}

#[test]
fn updated_is_never_before_created() {
    // created a minute in the future, as a UUID's time may run ahead.
    let task = "task-0192f3a4-0000-7000-8000-000000000002";
    let ahead = {
        let ms = now() + 60_000;
        let secs = ms / 1000;
        let (days, rem) = (secs / 86_400, secs % 86_400);
        // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
        let z = i64::try_from(days).expect("days") + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = yoe + era * 400 + i64::from(m <= 2);
        format!(
            "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
            rem / 3600,
            rem % 3600 / 60,
            rem % 60,
            ms % 1000
        )
    };
    let r = repo(
        false,
        &[(
            ".mdkg/work/task-a.md",
            format!(
                "---\nid: {task}\ntype: task\ntitle: A\nstatus: todo\ncreated: {ahead}\nupdated: {ahead}\n---\n# Overview\n"
            ),
        )],
    );
    ok(&r.0, &["done", task]);
    let text = fs::read_to_string(r.0.join(".mdkg/work/task-a.md")).expect("file");
    assert!(text.contains(&format!("updated: {ahead}\n")), "{text}");
}

#[test]
fn done_on_a_checkpoint_never_creates_another_checkpoint() {
    for (legacy, profile_c) in [(true, false), (false, false), (false, true)] {
        for with_checkpoint in [false, true] {
            let r = repo(legacy, &[]);
            if profile_c {
                let path = r.0.join(".mdkg/config.json");
                let mut config: Value =
                    serde_json::from_str(&fs::read_to_string(&path).expect("config"))
                        .expect("json");
                config["work"]["committed"] = "none".into();
                fs::write(path, config.to_string()).expect("config");
                sh(&r.0, &["commit", "-q", "-am", "profile C"]);
            }
            let new = ok(&r.0, &["new", "checkpoint", "Milestone"]);
            let id = new["data"]["node"]["id"].as_str().expect("id");
            assert_eq!(new["data"]["node"]["status"], "done");
            ok(&r.0, &["claim", id]);
            let before = sh(&r.0, &["rev-parse", "HEAD"]);
            let args = if with_checkpoint {
                vec!["done", id, "--checkpoint"]
            } else {
                vec!["done", id]
            };
            let done = ok(&r.0, &args);
            assert!(
                done["data"].get("checkpoint").is_none(),
                "legacy={legacy}, profile_c={profile_c}: {done}"
            );
            assert_eq!(done["data"]["node"]["id"], id);
            assert_eq!(done["data"]["node"]["status"], "done");
            let head = sh(&r.0, &["rev-parse", "HEAD"]);
            if profile_c {
                assert_eq!(head, before, "store work creates no git commit");
                assert_eq!(done["data"]["changesets"], serde_json::json!([]));
            } else {
                assert_ne!(head, before, "the original checkpoint is committed");
                assert_eq!(done["data"]["commit"], head);
                assert_eq!(done["data"]["changesets"].as_array().map(Vec::len), Some(1));
            }
            let status = ok(&r.0, &["status"]);
            assert_eq!(status["data"]["leases"], serde_json::json!([]), "{status}");
            let listed = ok(&r.0, &["list", "--type", "checkpoint", "--fields", "id"]);
            assert_eq!(listed["data"]["nodes"], serde_json::json!([{"id": id}]));

            // The recorded checkpoint is still a no-op on a later done,
            // including a retry that asks for another checkpoint.
            let again = ok(&r.0, &["done", id, "--checkpoint"]);
            assert_eq!(again["data"]["already_done"], true, "{again}");
            assert!(again["data"].get("checkpoint").is_none(), "{again}");
            assert_eq!(again["data"]["changesets"], serde_json::json!([]));
            assert_eq!(sh(&r.0, &["rev-parse", "HEAD"]), head);
            let listed_again = ok(&r.0, &["list", "--type", "checkpoint", "--fields", "id"]);
            assert_eq!(listed_again["data"]["nodes"], listed["data"]["nodes"]);

            if with_checkpoint {
                // A task still gets its requested checkpoint in every
                // storage profile; only checkpoints themselves are skipped.
                let task = ok(&r.0, &["new", "task", "Work"]);
                let task_id = task["data"]["node"]["id"].as_str().expect("task id");
                ok(&r.0, &["claim", task_id]);
                let done = ok(&r.0, &["done", task_id, "--checkpoint"]);
                let extra = done["data"]["checkpoint"]
                    .as_str()
                    .expect("task checkpoint");
                assert_ne!(extra, id);
                let listed = ok(&r.0, &["list", "--type", "checkpoint"]);
                assert_eq!(listed["data"]["nodes"].as_array().map(Vec::len), Some(2));
            }
        }
    }
}

#[test]
fn done_writes_a_timestamp() {
    let task = "task-0192f3a4-0000-7000-8000-000000000002";
    let r = repo(
        false,
        &[(
            ".mdkg/work/task-a.md",
            format!(
                "---\nid: {task}\ntype: task\ntitle: A\nstatus: todo\ncreated: 2026-10-24T00:00:00.000Z\nupdated: 2026-10-24T00:00:00.000Z\n---\n# Overview\n"
            ),
        )],
    );
    ok(&r.0, &["done", task]);
    let text = fs::read_to_string(r.0.join(".mdkg/work/task-a.md")).expect("file");
    let updated = text
        .lines()
        .find_map(|l| l.strip_prefix("updated: "))
        .expect("updated");
    assert!(mdkg_format::node::is_timestamp(updated), "{updated}");
    assert!(text.contains("status: done"));
}

#[test]
fn retype_rewrites_the_id_and_every_ref() {
    let task = "task-0192f3a4-0000-7000-8000-000000000002";
    let other = "task-0192f3a4-0000-7000-8000-000000000003";
    let r = repo(
        false,
        &[
            (".mdkg/work/goal-ship.md", goal(GOAL)),
            (
                ".mdkg/work/task-a.md",
                format!(
                    "---\nid: {task}\ntype: task\ntitle: A\nstatus: todo\nparent: {GOAL}\ncreated: 2026-10-24T00:00:00.000Z\nupdated: 2026-10-24T00:00:00.000Z\n---\n# Overview\n"
                ),
            ),
            (
                ".mdkg/work/task-b.md",
                format!(
                    "---\nid: {other}\ntype: task\ntitle: B\nstatus: todo\nrelates: [{task}]\nblocked_by: [{task}]\ncreated: 2026-10-24T00:00:00.000Z\nupdated: 2026-10-24T00:00:00.000Z\n---\n# Overview\n\nAfter [A]({task}).\n"
                ),
            ),
        ],
    );
    let head = sh(&r.0, &["rev-parse", "HEAD"]);
    let out = ok(
        &r.0,
        &["retype", "task-0192f3a4-0000-7000-8000-000000000002", "bug"],
    );
    let bug = task.replacen("task", "bug", 1);
    assert_eq!(out["data"]["node"]["id"], bug.as_str());
    assert_eq!(out["data"]["from"], task);
    assert_eq!(
        out["data"]["rewritten"],
        serde_json::json!([".mdkg/work/task-b.md", ".mdkg/work/task-a.md"])
    );
    let a = fs::read_to_string(r.0.join(".mdkg/work/task-a.md")).expect("a");
    assert!(a.contains(&format!("id: {bug}\ntype: bug\n")), "{a}");
    let b = fs::read_to_string(r.0.join(".mdkg/work/task-b.md")).expect("b");
    assert!(!b.contains(task), "{b}");
    assert!(b.contains(&format!("relates: [{bug}]")) && b.contains(&format!("[A]({bug})")));
    // One commit holds it all.
    assert_ne!(sh(&r.0, &["rev-parse", "HEAD"]), head);
    assert_eq!(sh(&r.0, &["status", "--porcelain"]), "");
    let v = ok(&r.0, &["validate"]);
    assert!(!codes(&v).contains(&"node.stale_type".to_string()), "{v}");
    // Again: nothing left to rewrite. The plan, kept a day, has the files
    // recorded once more, which changes nothing.
    let again = ok(&r.0, &["retype", &bug, "bug"]);
    assert_eq!(again["data"]["rewritten"], serde_json::json!([]));
    assert_eq!(sh(&r.0, &["status", "--porcelain"]), "");
}

#[test]
fn retype_refuses_what_it_cant_do() {
    let task = "task-0192f3a4-0000-7000-8000-000000000002";
    let text = format!(
        "---\nid: {task}\ntype: task\ntitle: A\nstatus: todo\npriority: 1\ncreated: 2026-10-24T00:00:00.000Z\nupdated: 2026-10-24T00:00:00.000Z\n---\n# Overview\n"
    );
    let r = repo(false, &[(".mdkg/work/task-a.md", text.clone())]);
    // An edd can't hold a status or priority: nothing is written.
    let (code, out) = run(&r.0, &["retype", task, "edd"]);
    assert_ne!(code, 0, "{out}");
    assert_eq!(
        fs::read_to_string(r.0.join(".mdkg/work/task-a.md")).expect("a"),
        text
    );
    // A goal's active node must stay actionable: a graph-level rule.
    let goal_text = goal(GOAL).replace(
        "legacy_id: goal-4\n",
        &format!("legacy_id: goal-4\nscope_refs: [{task}]\nactive_node: {task}\n"),
    );
    let r2 = repo(
        false,
        &[
            (".mdkg/work/goal-ship.md", goal_text),
            (".mdkg/work/task-a.md", text.clone()),
        ],
    );
    let (code, out) = run(&r2.0, &["retype", task, "epic"]);
    assert_eq!(code, 2, "{out}");
    assert_eq!(
        fs::read_to_string(r2.0.join(".mdkg/work/task-a.md")).expect("a"),
        text
    );
    assert_eq!(sh(&r2.0, &["status", "--porcelain"]), "");
    let (code, out) = run(&r.0, &["retype", task, "widget"]);
    assert_eq!(code, 1, "{out}");
    // A type typed on the command line (D2), not quoted.
    assert_eq!(codes(&out), ["usage.unknown_type"]);
    assert!(
        !out["diagnostics"][0]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("widget")
    );
    // A legacy graph has no UUIDs to keep.
    let legacy = repo(true, &[]);
    let (code, out) = run(&legacy.0, &["retype", "task-1", "bug"]);
    assert_eq!(code, 1, "{out}");
    assert_eq!(codes(&out), ["usage.bad_value"]);
}

// Review of 9ce31f5.

fn two_tasks() -> (Scratch, &'static str, &'static str) {
    let task = "task-0192f3a4-0000-7000-8000-000000000002";
    let other = "task-0192f3a4-0000-7000-8000-000000000003";
    let r = repo(
        false,
        &[
            (
                ".mdkg/work/task-a.md",
                format!(
                    "---\nid: {task}\ntype: task\ntitle: A\nstatus: todo\ncreated: 2026-10-24T00:00:00.000Z\nupdated: 2026-10-24T00:00:00.000Z\n---\n# Overview\n"
                ),
            ),
            (
                ".mdkg/work/task-b.md",
                format!(
                    "---\nid: {other}\ntype: task\ntitle: B\nstatus: todo\nrelates: [{task}]\ncreated: 2026-10-24T00:00:00.000Z\nupdated: 2026-10-24T00:00:00.000Z\n---\n# Overview\n"
                ),
            ),
        ],
    );
    (r, task, other)
}

/// Where a stopped retype of `uuid` left its plan in the worktree `r`.
fn journal_path(r: &Path, uuid: &str) -> PathBuf {
    let common = PathBuf::from(sh(
        r,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    ));
    let top = sh(r, &["rev-parse", "--show-toplevel"]);
    let key = mdkg_format::Digest::of_bytes(top.as_bytes()).to_string();
    let key = &key["sha256:".len()..][..16];
    common
        .join("mdkg/retype")
        .join(format!("{uuid}-{key}.json"))
}

/// What a retype that stopped after writing `files` (graph-relative) leaves:
/// those files rewritten, uncommitted, and its plan naming them.
fn stop_after(r: &Path, uuid: &str, files: &[(&str, String)]) {
    let mut plan = serde_json::Map::new();
    for (rel, text) in files {
        fs::write(r.join(rel), text).expect("write");
        plan.insert(
            (*rel).to_string(),
            mdkg_format::Digest::of_bytes(text.as_bytes())
                .to_string()
                .into(),
        );
    }
    let path = journal_path(r, uuid);
    fs::create_dir_all(path.parent().expect("dir")).expect("mkdir");
    fs::write(
        path,
        serde_json::json!({ "files": Value::Object(plan), "recorded": false }).to_string(),
    )
    .expect("plan");
}

#[test]
fn an_old_unfinished_plan_still_recovers() {
    // Stopped two days ago after every write: the rerun commits and
    // records it, and plans are never swept.
    let uuid = "0192f3a4-0000-7000-8000-000000000002";
    let (r, task, _) = two_tasks();
    let bug = task.replacen("task", "bug", 1);
    let files: Vec<(&str, String)> = [".mdkg/work/task-a.md", ".mdkg/work/task-b.md"]
        .into_iter()
        .map(|rel| {
            let text = fs::read_to_string(r.0.join(rel)).expect("read");
            (
                rel,
                text.replace(task, &bug)
                    .replace("type: task\ntitle: A", "type: bug\ntitle: A"),
            )
        })
        .collect();
    stop_after(&r.0, uuid, &files);
    let plan = journal_path(&r.0, uuid);
    let aged = Command::new("touch")
        .args(["-t", "202001010000"])
        .arg(&plan)
        .status()
        .expect("touch");
    assert!(aged.success());
    let head = sh(&r.0, &["rev-parse", "HEAD"]);
    let out = ok(&r.0, &["retype", &bug, "bug"]);
    assert_eq!(
        out["data"]["recovered"].as_array().map(Vec::len),
        Some(2),
        "{out}"
    );
    assert_ne!(sh(&r.0, &["rev-parse", "HEAD"]), head);
    assert_eq!(sh(&r.0, &["status", "--porcelain"]), "");
    let text = fs::read_to_string(&plan).expect("plan");
    assert!(text.contains("\"recorded\":true"), "{text}");
}

#[test]
fn a_stopped_retype_is_finished_and_committed_by_the_next() {
    let uuid = "0192f3a4-0000-7000-8000-000000000002";
    // Stopped after rewriting B, before the node and the commit.
    let (r, task, _) = two_tasks();
    let bug = task.replacen("task", "bug", 1);
    let b = fs::read_to_string(r.0.join(".mdkg/work/task-b.md")).expect("b");
    stop_after(
        &r.0,
        uuid,
        &[(".mdkg/work/task-b.md", b.replace(task, &bug))],
    );
    let out = ok(&r.0, &["retype", task, "bug"]);
    assert_eq!(
        out["data"]["rewritten"],
        serde_json::json!([".mdkg/work/task-a.md"])
    );
    assert_eq!(
        out["data"]["recovered"],
        serde_json::json!([".mdkg/work/task-b.md"])
    );
    assert_eq!(sh(&r.0, &["status", "--porcelain"]), "");
    // Kept a day after it's recorded, for a rerun after a power loss.
    assert!(journal_path(&r.0, uuid).exists());

    // Stopped after every write, before the commit: the rerun commits.
    let (r, task, _) = two_tasks();
    let bug = task.replacen("task", "bug", 1);
    let files: Vec<(&str, String)> = ["task-a.md", "task-b.md"]
        .iter()
        .map(|f| {
            let rel = if *f == "task-a.md" {
                ".mdkg/work/task-a.md"
            } else {
                ".mdkg/work/task-b.md"
            };
            let text = fs::read_to_string(r.0.join(rel)).expect("read");
            (
                rel,
                text.replace(task, &bug)
                    .replace("type: task\ntitle: A", "type: bug\ntitle: A"),
            )
        })
        .collect();
    stop_after(&r.0, uuid, &files);
    let head = sh(&r.0, &["rev-parse", "HEAD"]);
    let out = ok(&r.0, &["retype", &bug, "bug"]);
    assert_eq!(
        out["data"]["recovered"].as_array().map(Vec::len),
        Some(2),
        "{out}"
    );
    assert_ne!(sh(&r.0, &["rev-parse", "HEAD"]), head);
    assert_eq!(sh(&r.0, &["status", "--porcelain"]), "");
}

#[test]
fn a_rerun_never_takes_unrelated_edits() {
    // B names A and was edited by hand; no retype stopped. Retyping A to
    // its own type changes nothing, and B's edit stays uncommitted.
    let (r, task, _) = two_tasks();
    let b = r.0.join(".mdkg/work/task-b.md");
    let text = fs::read_to_string(&b).expect("b");
    fs::write(&b, text.replace("title: B", "title: B edited")).expect("write");
    let head = sh(&r.0, &["rev-parse", "HEAD"]);
    let out = ok(&r.0, &["retype", task, "task"]);
    assert_eq!(out["data"]["rewritten"], serde_json::json!([]));
    assert_eq!(sh(&r.0, &["rev-parse", "HEAD"]), head);
    assert_eq!(
        sh(&r.0, &["status", "--porcelain"]),
        "M .mdkg/work/task-b.md"
    );
}

#[test]
fn new_keeps_pins_and_edits_keep_permissions() {
    let r = repo(false, &[(".mdkg/work/goal-ship.md", goal(GOAL))]);
    let pinned = format!("{GOAL}@sha256:{}", "a".repeat(64));
    let out = ok(&r.0, &["new", "task", "Pinned", "--refs", &pinned]);
    let path = out["data"]["node"]["path"]
        .as_str()
        .expect("path")
        .to_string();
    let text = fs::read_to_string(r.0.join(&path)).expect("file");
    assert!(text.contains(&format!("refs: [{pinned}]\n")), "{text}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        sh(&r.0, &["add", "."]);
        sh(&r.0, &["commit", "-q", "-m", "pinned"]);
        let id = out["data"]["node"]["id"].as_str().expect("id").to_string();
        let file = r.0.join(&path);
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).expect("chmod");
        ok(&r.0, &["done", &id]);
        let mode = fs::metadata(&file).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}

#[test]
fn new_checks_the_graph_before_placing_the_file() {
    let r = repo(false, &[]);
    let missing = "goal-0192f3a4-0000-7000-8000-0000000000ff";
    // A stale temporary from a crashed `new`, ten minutes and more ago.
    let stale =
        r.0.join(".mdkg/work/.mdkg-new-0192f3a4-0000-7000-8000-000000000009.tmp");
    fs::create_dir_all(stale.parent().expect("dir")).expect("mkdir");
    fs::write(&stale, "partial").expect("write");
    let out = Command::new("touch")
        .args(["-t", "202001010000"])
        .arg(&stale)
        .status()
        .expect("touch");
    assert!(out.success());
    let (code, out) = run(&r.0, &["new", "task", "Bad", "--refs", missing]);
    assert_ne!(code, 0, "{out}");
    assert_eq!(codes(&out), ["node.not_found"]);
    let left: Vec<String> = fs::read_dir(r.0.join(".mdkg/work"))
        .expect("dir")
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect();
    // Nothing placed, and no temporary left (a stale one is only swept
    // once a node gets that far).
    assert!(!left.iter().any(|n| n.starts_with("task-")), "{left:?}");
    ok(&r.0, &["new", "task", "Good"]);
    let left: Vec<String> = fs::read_dir(r.0.join(".mdkg/work"))
        .expect("dir")
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect();
    assert_eq!(left, ["task-good.md"], "{left:?}");
}

#[test]
fn a_date_only_created_is_a_floor_too() {
    let task = "task-0192f3a4-0000-7000-8000-000000000002";
    let r = repo(
        false,
        &[(
            ".mdkg/work/task-a.md",
            format!(
                "---\nid: {task}\ntype: task\ntitle: A\nstatus: todo\ncreated: 2999-01-01\nupdated: 2999-01-02\n---\n# Overview\n"
            ),
        )],
    );
    ok(&r.0, &["done", task]);
    let text = fs::read_to_string(r.0.join(".mdkg/work/task-a.md")).expect("file");
    assert!(
        text.contains("updated: 2999-01-02T00:00:00.000Z\n"),
        "{text}"
    );
}

#[test]
fn new_refuses_a_node_over_the_index_limits_before_placing_it() {
    let r = repo(false, &[(".mdkg/work/goal-ship.md", goal(GOAL))]);
    let path = r.0.join(".mdkg/config.json");
    let mut config: Value =
        serde_json::from_str(&fs::read_to_string(&path).expect("config")).expect("json");
    // The goal is the one document; one more would exceed the limit.
    config["index"]["limits"]["max_files"] = 1.into();
    fs::write(&path, config.to_string()).expect("write");
    let (code, out) = run(&r.0, &["new", "task", "Over"]);
    assert_ne!(code, 0, "{out}");
    assert_eq!(codes(&out), ["index.limit_exceeded"]);
    assert!(!r.0.join(".mdkg/work/task-over.md").exists());
}

fn run_as(root: &Path, actor: &str, args: &[&str]) -> (i32, Value) {
    let out = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
        .arg("--root")
        .arg(root)
        .args(["--as", actor])
        .args(args)
        .arg("--json")
        .env("MDKG_NOW_MS", now().to_string())
        .output()
        .expect("run mdkg");
    let envelope: Value = serde_json::from_slice(&out.stdout).expect("json");
    (out.status.code().unwrap_or(-1), envelope)
}

#[test]
fn a_refused_recovery_keeps_the_plan_for_the_next_try() {
    let uuid = "0192f3a4-0000-7000-8000-000000000002";
    let (r, task, _) = two_tasks();
    let bug = task.replacen("task", "bug", 1);
    let b = fs::read_to_string(r.0.join(".mdkg/work/task-b.md")).expect("b");
    stop_after(
        &r.0,
        uuid,
        &[(".mdkg/work/task-b.md", b.replace(task, &bug))],
    );
    // Someone else holds B: the recovery is refused, and the plan stays.
    let (code, out) = run_as(&r.0, "lane-b", &["claim", ".mdkg/work/task-b.md"]);
    assert_eq!(code, 0, "{out}");
    let (code, out) = run(&r.0, &["retype", task, "bug"]);
    assert_eq!(code, 5, "{out}");
    assert!(journal_path(&r.0, uuid).exists());
    let (code, out) = run_as(&r.0, "lane-b", &["release", ".mdkg/work/task-b.md"]);
    assert_eq!(code, 0, "{out}");
    let out = ok(&r.0, &["retype", task, "bug"]);
    assert_eq!(
        out["data"]["rewritten"].as_array().map(Vec::len),
        Some(1),
        "{out}"
    );
    assert_eq!(
        out["data"]["recovered"].as_array().map(Vec::len),
        Some(1),
        "{out}"
    );
    assert_eq!(sh(&r.0, &["status", "--porcelain"]), "");
    // Kept a day after it's recorded, for a rerun after a power loss.
    assert!(journal_path(&r.0, uuid).exists());
}

#[test]
fn a_plan_whose_files_are_committed_is_recorded_again() {
    // Stopped after the git commit, before the coordinator recorded it:
    // the files are clean and hold the plan's content.
    let uuid = "0192f3a4-0000-7000-8000-000000000002";
    let (r, task, _) = two_tasks();
    let bug = task.replacen("task", "bug", 1);
    ok(&r.0, &["retype", task, "bug"]);
    let files: Vec<(&str, String)> = [".mdkg/work/task-a.md", ".mdkg/work/task-b.md"]
        .into_iter()
        .map(|rel| (rel, fs::read_to_string(r.0.join(rel)).expect("read")))
        .collect();
    stop_after(&r.0, uuid, &files);
    let head = sh(&r.0, &["rev-parse", "HEAD"]);
    let out = ok(&r.0, &["retype", &bug, "bug"]);
    assert_eq!(
        out["data"]["recovered"].as_array().map(Vec::len),
        Some(2),
        "{out}"
    );
    assert_eq!(sh(&r.0, &["rev-parse", "HEAD"]), head);
    assert_eq!(sh(&r.0, &["status", "--porcelain"]), "");
    // Kept a day after it's recorded, for a rerun after a power loss.
    assert!(journal_path(&r.0, uuid).exists());
}

#[test]
fn retype_leaves_extension_values_alone() {
    let (r, task, _) = two_tasks();
    let b = r.0.join(".mdkg/work/task-b.md");
    let text = fs::read_to_string(&b).expect("b");
    fs::write(
        &b,
        text.replace(
            "status: todo\n",
            &format!("status: todo\nx-acme.snapshot: {task}\n"),
        ),
    )
    .expect("write");
    sh(&r.0, &["commit", "-qam", "extension"]);
    ok(&r.0, &["retype", task, "bug"]);
    let after = fs::read_to_string(&b).expect("b");
    let bug = task.replacen("task", "bug", 1);
    assert!(
        after.contains(&format!("x-acme.snapshot: {task}\n")),
        "{after}"
    );
    assert!(after.contains(&format!("relates: [{bug}]")), "{after}");
}

#[test]
fn edits_and_format_read_0_7_files_strictly() {
    // A title of one U+00A0 and an extension value padded with U+00A0: text
    // in a 0.7 file, which done and format --write keep.
    let task = "task-0192f3a4-0000-7000-8000-000000000002";
    let text = format!(
        "---\nid: {task}\ntype: task\ntitle: \u{A0}\nstatus: todo\ncreated: 2026-10-24T00:00:00.000Z\nupdated: 2026-10-24T00:00:00.000Z\nx-acme.note: \u{A0}kept\u{A0}\n---\n# Overview\n"
    );
    let r = repo(false, &[(".mdkg/work/task-a.md", text)]);
    ok(&r.0, &["format", "--write"]);
    ok(&r.0, &["done", task]);
    let after = fs::read_to_string(r.0.join(".mdkg/work/task-a.md")).expect("file");
    assert!(after.contains("title: \u{A0}\n"), "{after:?}");
    assert!(
        after.contains("x-acme.note: \u{A0}kept\u{A0}\n"),
        "{after:?}"
    );
    assert!(after.contains("status: done\n"), "{after:?}");
}

#[test]
fn a_stray_cr_never_reaches_format() {
    // `title: \r\r\n` would read as a CR title and write back as an empty
    // one; a 0.7 reader refuses it first, and format leaves the file.
    let task = "task-0192f3a4-0000-7000-8000-000000000002";
    let text = format!(
        "---\nid: {task}\ntype: task\ntitle: \r\r\nstatus: todo\ncreated: 2026-10-24T00:00:00.000Z\nupdated: 2026-10-24T00:00:00.000Z\n---\n# Overview\n"
    );
    let r = repo(false, &[(".mdkg/work/task-a.md", text.clone())]);
    let (code, out) = run(&r.0, &["format", "--write"]);
    assert_ne!(code, 0, "{out}");
    assert_eq!(codes(&out), ["frontmatter.stray_cr"]);
    assert_eq!(
        fs::read_to_string(r.0.join(".mdkg/work/task-a.md")).expect("file"),
        text
    );
}

#[test]
fn a_node_new_writes_never_lacks_a_recommended_heading() {
    // A 0.7 type recommends its template's headings (RC1 F5), so every
    // node `new` makes, of every type it makes, validates without
    // node.heading_missing.
    let r = repo(false, &[]);
    for t in [
        "task",
        "bug",
        "feat",
        "spike",
        "test",
        "epic",
        "goal",
        "checkpoint",
        "edd",
        "dec",
        "prd",
        "prop",
        "rule",
    ] {
        ok(&r.0, &["new", t, &format!("A {t}")]);
    }
    let out = ok(&r.0, &["validate", "--warnings"]);
    let missing: Vec<String> = codes(&out)
        .into_iter()
        .filter(|c| c == "node.heading_missing")
        .collect();
    assert!(missing.is_empty(), "{out}");
}
