//! `work.committed: none` (edd-5 profile C, task-39): work-type nodes live
//! beside the coordinator store, which git never sees, and every command
//! reads them with the rest of the graph.

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
        "mdkg-cli-profile-c-{}-{}",
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

/// Sets `work.committed` in the repository's config, committed.
fn committed(dir: &Path, value: Value) {
    let path = dir.join(".mdkg/config.json");
    let mut config: Value =
        serde_json::from_str(&fs::read_to_string(&path).expect("config")).expect("json");
    config["work"]["committed"] = value;
    fs::write(&path, config.to_string()).expect("write config");
    sh(dir, &["commit", "-q", "-am", "work.committed"]);
}

#[test]
fn profile_c_keeps_work_nodes_beside_the_store() {
    let repo = repo(false, &[]);
    committed(&repo.0, "none".into());
    let task = ok(&repo.0, &["new", "task", "Ship", "it"]);
    // Beside the store: in a main checkout that's inside the graph root,
    // so the path is relative (`.git/mdkg/work/root/...`); from a linked
    // worktree it's absolute. Either joins onto the root.
    let path = task["data"]["node"]["path"]
        .as_str()
        .expect("path")
        .to_string();
    let file = fs::canonicalize(repo.0.join(&path)).expect("the file");
    let store_work = fs::canonicalize(repo.0.join(".git"))
        .expect("git")
        .join("mdkg/work/root");
    assert!(file.starts_with(&store_work), "{path}");
    assert_eq!(
        sh(&repo.0, &["status", "--porcelain"]),
        "",
        "git sees nothing"
    );
    let id = task["data"]["node"]["id"].as_str().expect("id").to_string();

    // A design node stays in the committed plane.
    let edd = ok(&repo.0, &["new", "edd", "Design"]);
    assert!(
        edd["data"]["node"]["path"]
            .as_str()
            .is_some_and(|p| p.starts_with(".mdkg/design/")),
        "{edd}"
    );

    // Every read sees the task.
    let shown = ok(&repo.0, &["show", &id]);
    assert_eq!(shown["data"]["node"]["path"], path.as_str(), "{shown}");
    let listed = ok(&repo.0, &["list", "--type", "task", "--fields", "id"]);
    assert!(listed.to_string().contains(&id), "{listed}");
    let (code, v) = run(&repo.0, &["validate"]);
    assert_eq!(code, 0, "{v}");
}

#[test]
fn profile_b_is_the_default_and_bad_values_are_refused() {
    let b = repo(false, &[]);
    let task = ok(&b.0, &["new", "task", "Committed"]);
    assert!(
        task["data"]["node"]["path"]
            .as_str()
            .is_some_and(|p| p.starts_with(".mdkg/work/")),
        "{task}"
    );
    committed(&b.0, "everything".into());
    let (code, v) = run(&b.0, &["list"]);
    assert_ne!(code, 0, "{v}");
    assert_eq!(v["diagnostics"][0]["code"], "config.invalid", "{v}");

    // A 0.6.0-format graph can't drop its work nodes out of git.
    let legacy = repo(true, &[]);
    committed(&legacy.0, "none".into());
    let (code, v) = run(&legacy.0, &["list"]);
    assert_ne!(code, 0, "{v}");
    assert_eq!(v["diagnostics"][0]["code"], "config.invalid", "{v}");
}

/// Every worktree of the repository shares the work folder: a task made in
/// a linked worktree has an absolute path there, and the main checkout
/// lists it.
#[test]
fn worktrees_share_the_work_folder() {
    let repo = repo(false, &[]);
    committed(&repo.0, "none".into());
    let linked = repo.0.with_file_name(format!(
        "{}-linked",
        repo.0.file_name().expect("name").to_string_lossy()
    ));
    sh(
        &repo.0,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "side",
            linked.to_str().expect("utf-8"),
        ],
    );
    let task = ok(&linked, &["new", "task", "From", "the", "side"]);
    let path = task["data"]["node"]["path"]
        .as_str()
        .expect("path")
        .to_string();
    assert!(Path::new(&path).is_absolute(), "{path}");
    let id = task["data"]["node"]["id"].as_str().expect("id").to_string();
    let listed = ok(&repo.0, &["list", "--type", "task", "--fields", "id"]);
    assert!(listed.to_string().contains(&id), "{listed}");
    fs::remove_dir_all(&linked).ok();
}

/// Review of 541f23b: creators in different worktrees take turns, so a
/// node is checked before anyone can see it and never taken back. With
/// room for only a few more files, exactly that many succeed, each one
/// stays, and the graph still loads.
#[test]
fn concurrent_creators_take_turns() {
    let repo = repo(false, &[]);
    committed(&repo.0, "none".into());
    let linked = repo.0.with_file_name(format!(
        "{}-turns",
        repo.0.file_name().expect("name").to_string_lossy()
    ));
    sh(
        &repo.0,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "turns",
            linked.to_str().expect("utf-8"),
        ],
    );
    // Count documents the way discovery does.
    let docs = walk_md(&repo.0.join(".mdkg"));
    let room = 3;
    for dir in [&repo.0, &linked] {
        let path = dir.join(".mdkg/config.json");
        let mut config: Value =
            serde_json::from_str(&fs::read_to_string(&path).expect("config")).expect("json");
        config["index"]["limits"]["max_files"] = (docs + room).into();
        fs::write(&path, config.to_string()).expect("write config");
    }
    let handles: Vec<_> = (0..8)
        .map(|i| {
            let dir = if i % 2 == 0 {
                repo.0.clone()
            } else {
                linked.clone()
            };
            std::thread::spawn(move || run(&dir, &["new", "task", &format!("Turn {i}")]))
        })
        .collect();
    let results: Vec<(i32, Value)> = handles
        .into_iter()
        .map(|h| h.join().expect("joined"))
        .collect();
    // Each result is a success or the limit's refusal, by its code; any
    // other failure is named, so a flake says what it was.
    let mut made: Vec<String> = Vec::new();
    let mut refused = 0u64;
    let mut other: Vec<String> = Vec::new();
    for (code, v) in &results {
        let first = v["diagnostics"][0]["code"].as_str().unwrap_or_default();
        if *code == 0 {
            made.push(v["data"]["node"]["id"].as_str().expect("id").to_string());
        } else if first == "index.limit_exceeded" {
            refused += 1;
        } else {
            other.push(format!("exit {code}: {}", v["diagnostics"]));
        }
    }
    assert!(other.is_empty(), "creators failed otherwise: {other:#?}");
    assert_eq!(made.len() as u64 + refused, 8, "{results:?}");
    assert_eq!(made.len() as u64, room, "{results:?}");
    let listed = ok(&linked, &["list", "--type", "task", "--fields", "id"]);
    for id in &made {
        assert!(listed.to_string().contains(id), "{id} stays: {listed}");
    }
    fs::remove_dir_all(&linked).ok();
}

fn walk_md(dir: &Path) -> u64 {
    let mut n = 0;
    for entry in fs::read_dir(dir).expect("dir").flatten() {
        let p = entry.path();
        if p.is_dir() {
            n += walk_md(&p);
        } else if p.extension().is_some_and(|e| e == "md")
            && ["core", "design", "work"]
                .iter()
                .any(|f| p.to_string_lossy().contains(&format!("/.mdkg/{f}/")))
        {
            n += 1;
        }
    }
    n
}

/// Orchestrator ruling (2026-10-25): a shared work node naming knowledge
/// only another branch holds is a warning there, never a broken graph.
#[test]
fn a_shared_node_naming_another_branch_s_knowledge_warns() {
    let repo = repo(false, &[]);
    committed(&repo.0, "none".into());
    let a = repo.0.with_file_name(format!(
        "{}-a",
        repo.0.file_name().expect("name").to_string_lossy()
    ));
    sh(
        &repo.0,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature",
            a.to_str().expect("utf-8"),
        ],
    );
    let edd = ok(&a, &["new", "edd", "Only", "here"]);
    let edd_id = edd["data"]["node"]["id"].as_str().expect("id").to_string();
    let task = ok(
        &a,
        &["new", "task", "Needs", "the", "design", "--refs", &edd_id],
    );
    let task_id = task["data"]["node"]["id"].as_str().expect("id").to_string();
    // And a body link to it.
    let file = a.join(task["data"]["node"]["path"].as_str().expect("path"));
    let text = fs::read_to_string(&file).expect("task");
    fs::write(&file, format!("{text}\nSee [the design]({edd_id}).\n")).expect("link");

    // The main checkout (another branch) has the task but not the edd.
    let b = &repo.0;
    let listed = ok(b, &["list", "--type", "task", "--fields", "id"]);
    assert!(listed.to_string().contains(&task_id), "{listed}");
    let shown = ok(b, &["show", &task_id]);
    assert_eq!(
        shown["diagnostics"][0]["code"], "edge.target_not_on_branch",
        "{shown}"
    );
    ok(b, &["search", "design"]);
    ok(b, &["new", "task", "Unrelated"]);
    let (code, v) = run(b, &["validate"]);
    assert_eq!(code, 0, "{v}");
    assert!(
        v.to_string().contains("edge.target_not_on_branch"),
        "validate lists it: {v}"
    );
    assert!(
        v.to_string().contains("a body link"),
        "the body link too: {v}"
    );
    assert!(!v.to_string().contains("node.bad_body_link"), "{v}");
    // A missing target from a committed node is still an error.
    let (code, v) = run(
        &a,
        &[
            "new",
            "edd",
            "Broken",
            "--refs",
            "edd-01a1710b-0000-7000-8000-000000000000",
        ],
    );
    assert_ne!(code, 0, "{v}");
    fs::remove_dir_all(&a).ok();
}

/// Orchestrator, 2026-10-25: commands running at once wait for the store,
/// never failing with `database is locked`. Every open used to run
/// `PRAGMA journal_mode = WAL`, which SQLite refuses at once (it never
/// waits on its busy handler for a journal-mode change) while another
/// process commits or closes. Three rounds of eight `new` from two
/// worktrees, all of which must land, and a moment where another
/// connection holds the store exclusively, which `new` must wait out.
#[test]
fn concurrent_commands_wait_for_the_store() {
    let repo = repo(false, &[]);
    committed(&repo.0, "none".into());
    let linked = repo.0.with_file_name(format!(
        "{}-busy",
        repo.0.file_name().expect("name").to_string_lossy()
    ));
    sh(
        &repo.0,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "busy",
            linked.to_str().expect("utf-8"),
        ],
    );
    let mut failed = Vec::new();
    for round in 0..3 {
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let dir = if i % 2 == 0 {
                    repo.0.clone()
                } else {
                    linked.clone()
                };
                std::thread::spawn(move || run(&dir, &["new", "task", &format!("R{round} n{i}")]))
            })
            .collect();
        for h in handles {
            let (code, v) = h.join().expect("joined");
            if code != 0 {
                failed.push(v["diagnostics"].to_string());
            }
        }
    }
    let db = repo.0.join(".git/mdkg/coord.sqlite");
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        let c = rusqlite::Connection::open(&db).expect("open");
        c.execute_batch("PRAGMA locking_mode = EXCLUSIVE; BEGIN EXCLUSIVE;")
            .expect("lock");
        held_tx.send(()).expect("signal");
        std::thread::sleep(std::time::Duration::from_millis(1000));
        c.execute_batch("COMMIT;").expect("commit");
    });
    held_rx.recv().expect("held");
    let (code, v) = run(&repo.0, &["new", "task", "Held"]);
    if code != 0 {
        failed.push(v["diagnostics"].to_string());
    }
    holder.join().expect("holder");
    fs::remove_dir_all(&linked).ok();
    assert!(
        failed.is_empty(),
        "commands failed instead of waiting: {failed:#?}"
    );
}
