//! Graph freeze gates, including diagnostics with a damaged config.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::time::{Duration, Instant};

use mdkg_run::builds::{self, Build, Kind};
use serde_json::Value;

mod common;

const BIN: &str = env!("CARGO_BIN_EXE_mdkg");
const DEADLINE: Duration = Duration::from_secs(30);

struct Fixture {
    top: PathBuf,
    graph: PathBuf,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.top).ok();
    }
}

fn isolated(command: &mut Command, home: &Path) {
    let path = std::env::var_os("PATH").unwrap_or_default();
    command
        .env_clear()
        .env("PATH", path)
        .env("HOME", home)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .env("MDKG_RUN_SECURITY", "/usr/bin/false")
        .env("MDKG_RUN_CODEX", "/usr/bin/false")
        .env("MDKG_RUN_CLAUDE", "/usr/bin/false")
        .env("MDKG_RUN_OPENCODE", "/usr/bin/false");
    common::without_launcher_env(command);
}

struct Running(Child);
impl Drop for Running {
    fn drop(&mut self) {
        self.0.kill().ok();
        self.0.wait().ok();
    }
}

fn bounded(command: &mut Command) -> Output {
    // Files avoid stdout/stderr pipe capacity turning a timeout into a hang.
    let stem = std::env::temp_dir().join(format!("mdkg-repair-output-{}", uuid::Uuid::now_v7()));
    let out = stem.with_extension("out");
    let err = stem.with_extension("err");
    command
        .stdout(File::create(&out).expect("stdout"))
        .stderr(File::create(&err).expect("stderr"));
    let mut child = Running(command.spawn().expect("start command"));
    let deadline = Instant::now() + DEADLINE;
    let status = loop {
        if let Some(status) = child.0.try_wait().expect("poll child") {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "command did not return within {DEADLINE:?}: {command:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    let stdout = fs::read(&out).expect("stdout bytes");
    let stderr = fs::read(&err).expect("stderr bytes");
    fs::remove_file(out).ok();
    fs::remove_file(err).ok();
    Output {
        status,
        stdout,
        stderr,
    }
}

fn git(root: &Path, args: &[&str]) -> String {
    let mut command = Command::new("git");
    isolated(&mut command, root);
    command.arg("-C").arg(root).args(args);
    let out = bounded(&mut command);
    assert!(out.status.success(), "{args:?}: {out:?}");
    String::from_utf8(out.stdout)
        .expect("UTF-8")
        .trim()
        .to_string()
}

fn command(root: &Path, args: &[&str], json: bool) -> Command {
    let mut command = Command::new(BIN);
    isolated(&mut command, root);
    command
        .current_dir(root)
        .arg("--root")
        .arg(root)
        .args(["--as", "fixture-reader"])
        .args(args);
    if json {
        command.arg("--json");
    }
    command
}

fn run(root: &Path, args: &[&str]) -> (Output, Value) {
    let out = bounded(&mut command(root, args, true));
    let body: Value =
        serde_json::from_slice(&out.stdout).unwrap_or_else(|_| panic!("{args:?}: {out:?}"));
    common::conforms_envelope(&body);
    (out, body)
}

fn ok(root: &Path, args: &[&str]) -> Value {
    let (out, body) = run(root, args);
    assert!(out.status.success(), "{args:?}: {out:?}");
    body
}

fn fixture() -> Fixture {
    let top = std::env::temp_dir().join(format!("mdkg-graph-repair-{}", uuid::Uuid::now_v7()));
    fs::create_dir_all(&top).expect("fixture");
    let top = fs::canonicalize(top).expect("canonical fixture");
    git(&top, &["init", "-qb", "main"]);
    ok(&top, &["init"]);
    git(&top, &["add", ".mdkg"]);
    git(&top, &["commit", "-qm", "fixture graph"]);
    ok(&top, &["install"]);
    let graph = top.join(".worktrees/graph");
    Fixture { top, graph }
}

#[test]
fn graph_writer_waits_before_reading_and_preserves_a_sync_edit() {
    let f = fixture();
    let config_path = f.top.join(".mdkg/config.json");
    let mut config: Value =
        serde_json::from_slice(&fs::read(&config_path).expect("config")).expect("JSON");
    config["work"]["location"] = "git".into();
    fs::write(&config_path, serde_json::to_vec(&config).expect("JSON")).expect("shared work");
    git(&f.top, &["add", ".mdkg/config.json"]);
    git(&f.top, &["commit", "-qm", "shared work"]);
    let created = ok(&f.top, &["new", "task", "serialize graph writes"]);
    let id = created["data"]["node"]["id"].as_str().expect("id");
    let path = created["data"]["node"]["path"].as_str().expect("path");
    git(&f.top, &["add", ".mdkg"]);
    git(&f.top, &["commit", "-qm", "task"]);
    ok(&f.top, &["install"]);
    ok(&f.graph, &["claim", id]);
    let lock = File::options()
        .read(true)
        .write(true)
        .open(f.top.join(".git/mdkg/graph-worktree.lock"))
        .expect("graph lock");
    lock.lock().expect("hold sync lock");
    let mut child = Running(
        command(&f.graph, &["done", id], true)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("writer"),
    );
    // A writer must not even read its node while a sync owns the checkout.
    let until = Instant::now() + Duration::from_secs(2);
    while Instant::now() < until {
        assert!(
            child.0.try_wait().expect("writer status").is_none(),
            "writer escaped graph lock"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let file = f.top.join(path);
    let mut content = fs::read_to_string(&file).expect("task");
    content.push_str("\nMain edit must survive the waiting writer.\n");
    fs::write(&file, content).expect("main edit");
    git(&f.top, &["add", path]);
    git(&f.top, &["commit", "-qm", "main edits same node"]);
    git(&f.graph, &["merge", "--ff-only", "main"]);
    drop(lock);
    let deadline = Instant::now() + DEADLINE;
    loop {
        if let Some(status) = child.0.try_wait().expect("writer status") {
            assert!(status.success(), "writer failed: {status}");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "writer did not finish after sync"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let final_node = fs::read_to_string(f.graph.join(path)).expect("finished task");
    assert!(final_node.contains("Main edit must survive the waiting writer."));
    assert!(final_node.contains("status: done"));
}

fn index_lock(graph: &Path) -> PathBuf {
    Path::new(&git(graph, &["rev-parse", "--absolute-git-dir"])).join("index.lock")
}

fn freeze(graph: &Path) {
    fs::write(
        graph.join(".mdkg/config.json"),
        "interrupted config write; invalid JSON",
    )
    .expect("damage");
}

fn repair_diagnostic(body: &Value) -> &Value {
    body["diagnostics"]
        .as_array()
        .expect("diagnostics")
        .iter()
        .find(|d| {
            d["message"]
                .as_str()
                .is_some_and(|m| m.contains("graph worktree needs repair"))
        })
        .unwrap_or_else(|| panic!("missing repair diagnostic: {body}"))
}

fn snapshot(path: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn visit(base: &Path, at: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(at).expect("snapshot folder") {
            let entry = entry.expect("entry");
            let kind = entry.file_type().expect("type");
            if kind.is_dir() {
                visit(base, &entry.path(), out);
            } else if kind.is_file() {
                out.insert(
                    entry
                        .path()
                        .strip_prefix(base)
                        .expect("relative")
                        .to_path_buf(),
                    fs::read(entry.path()).expect("read"),
                );
            } else {
                panic!("unexpected fixture type");
            }
        }
    }
    let mut out = BTreeMap::new();
    visit(path, path, &mut out);
    out
}

#[test]
fn frozen_writers_refuse_before_parsing_a_damaged_config_or_creating_state() {
    let f = fixture();
    freeze(&f.graph);
    let before = snapshot(&f.graph);
    let local = snapshot(&f.top.join(".git/mdkg"));
    for args in [
        vec!["new", "task", "example"],
        vec!["update", "task-0", "--status", "review"],
        vec!["done", "task-0", "--checkpoint"],
        vec!["sync"],
        vec!["install", "--enable", "agents-md"],
        vec!["agents", "up", "worker", "--once"],
        vec![
            "agents",
            "add",
            "worker",
            "--harness",
            "codex",
            "--role",
            "worker",
        ],
        vec!["agents", "edit", "worker", "--harness", "codex"],
        vec!["agents", "rm", "worker"],
        vec!["format", "--write"],
        vec!["retype", "task-0", "bug"],
        vec!["anchor"],
        vec!["skills", "add", "mdkg-workflow"],
        vec!["init", "--dry-run"],
        vec!["goal", "done", "goal-0"],
        vec!["migrate", "--dry-run"],
    ] {
        let (out, body) = run(&f.graph, &args);
        assert!(!out.status.success(), "{args:?}: {body}");
        assert_eq!(
            repair_diagnostic(&body)["code"],
            "io.write_failed",
            "{args:?}"
        );
        assert!(
            repair_diagnostic(&body)["hint"]
                .as_str()
                .expect("guidance")
                .contains(" status")
        );
    }
    assert_eq!(snapshot(&f.graph), before);
    assert_eq!(snapshot(&f.top.join(".git/mdkg")), local);
}

#[test]
fn frozen_localize_refuses_before_config_reads_or_journal_creation() {
    for damaged_config in [false, true] {
        let f = fixture();
        let planned = ok(&f.graph, &["work", "localize", "--dry-run"]);
        let hash = planned["data"]["plan"].as_str().expect("localize plan");
        let journal = f.top.join(".git/mdkg/localize");
        assert!(!journal.exists(), "dry-run must not create a journal");
        if damaged_config {
            freeze(&f.graph);
        } else {
            fs::write(index_lock(&f.graph), "interrupted checkout").expect("index lock");
        }
        let before = snapshot(&f.graph);
        let metadata = snapshot(&f.top.join(".git/mdkg"));
        let index = index_lock(&f.graph).with_file_name("index");
        let index_before = fs::read(&index).expect("index bytes");
        let head = git(&f.graph, &["rev-parse", "HEAD"]);
        for args in [
            vec!["work", "localize", "--dry-run"],
            vec!["work", "localize", "--apply", hash],
        ] {
            let (out, body) = run(&f.graph, &args);
            assert!(!out.status.success(), "{args:?}: {body}");
            assert_eq!(body["schema"], "mdkg.work.localize.v1");
            assert_eq!(repair_diagnostic(&body)["code"], "io.write_failed");
            assert!(!journal.exists(), "{args:?} created localize metadata");
            assert_eq!(snapshot(&f.graph), before, "{args:?}");
            assert_eq!(snapshot(&f.top.join(".git/mdkg")), metadata, "{args:?}");
            assert_eq!(fs::read(&index).expect("index untouched"), index_before);
            assert_eq!(git(&f.graph, &["rev-parse", "HEAD"]), head);
        }
    }
}

#[test]
fn missing_config_is_reported_as_a_frozen_checkout_before_init_can_replace_it() {
    let f = fixture();
    fs::remove_file(f.graph.join(".mdkg/config.json")).expect("partial checkout deletion");
    for args in [vec!["new", "task", "example"], vec!["init"]] {
        let (out, body) = run(&f.graph, &args);
        assert!(!out.status.success());
        repair_diagnostic(&body);
        assert!(!f.graph.join(".mdkg/config.json").exists());
    }
}

#[test]
fn doctor_and_both_status_commands_show_paths_and_recovery_with_a_broken_config() {
    let f = fixture();
    freeze(&f.graph);
    let before = snapshot(&f.graph);
    for args in [vec!["doctor"], vec!["status"], vec!["agents", "status"]] {
        let (out, body) = run(&f.graph, &args);
        assert!(!out.status.success(), "{args:?}: {body}");
        let repair = repair_diagnostic(&body);
        assert!(
            repair["message"]
                .as_str()
                .expect("message")
                .contains(".mdkg/config.json")
        );
        let hint = repair["hint"].as_str().expect("commands");
        assert!(
            hint.starts_with("git -C") && hint.contains(" status"),
            "{hint}"
        );
        assert!(
            hint.contains("restore --source=HEAD")
                && !hint.contains("reset --hard")
                && !hint.contains("clean -")
        );
        if args != ["doctor"] {
            assert!(
                body.get("data").is_none(),
                "an incomplete inventory must not look empty"
            );
        }
        let text = bounded(&mut command(&f.graph, &args, false));
        assert!(String::from_utf8_lossy(&text.stderr).contains("graph worktree needs repair"));
        assert!(String::from_utf8_lossy(&text.stderr).contains("git -C"));
    }
    assert_eq!(snapshot(&f.graph), before);
}

#[test]
fn a_lock_or_wrong_branch_freezes_writers_with_an_otherwise_valid_config() {
    for state in ["index.lock", "branch"] {
        let f = fixture();
        if state == "index.lock" {
            fs::write(index_lock(&f.graph), "interrupted").expect("lock");
        } else {
            git(&f.graph, &["switch", "-qc", "other"]);
        }
        let before = snapshot(&f.graph);
        let (out, body) = run(&f.graph, &["new", "task", "example"]);
        assert!(!out.status.success());
        assert!(
            repair_diagnostic(&body)["message"]
                .as_str()
                .expect("state")
                .contains(if state == "index.lock" {
                    "index.lock"
                } else {
                    "HEAD is not"
                })
        );
        for args in [vec!["status"], vec!["agents", "status"], vec!["doctor"]] {
            let (_, body) = run(&f.graph, &args);
            let hint = repair_diagnostic(&body)["hint"].as_str().expect("guidance");
            if state == "index.lock" {
                assert!(hint.contains("no Git process is running"));
            } else {
                assert!(hint.contains("switch --no-overwrite-ignore mdkg-graph"));
            }
        }
        assert_eq!(snapshot(&f.graph), before);
    }
}

#[test]
fn hidden_index_changes_freeze_writers_and_read_only_diagnostics() {
    for flag in ["--assume-unchanged", "--skip-worktree"] {
        let f = fixture();
        git(&f.graph, &["update-index", flag, "--", ".mdkg/config.json"]);
        if flag == "--assume-unchanged" {
            freeze(&f.graph);
        } else {
            fs::remove_file(f.graph.join(".mdkg/config.json")).expect("hidden missing config");
        }
        assert!(git(&f.graph, &["diff", "--name-only"]).is_empty());
        let before = snapshot(&f.graph);
        let index = index_lock(&f.graph).with_file_name("index");
        let index_before = fs::read(&index).expect("index");
        for args in [
            vec!["new", "task", "example"],
            vec!["agents", "up", "worker", "--once"],
            vec!["install"],
            vec!["sync"],
            vec!["status"],
            vec!["agents", "status"],
            vec!["doctor"],
        ] {
            let (out, body) = run(&f.graph, &args);
            assert!(!out.status.success(), "{args:?}: {body}");
            let diagnostic = repair_diagnostic(&body);
            let message = diagnostic["message"].as_str().expect("state");
            assert!(
                message.contains("index flags") && message.contains(".mdkg/config.json"),
                "{message}"
            );
            let hint = diagnostic["hint"].as_str().expect("guidance");
            assert!(
                hint.contains("--no-assume-unchanged") && hint.contains("--no-skip-worktree"),
                "{hint}"
            );
            assert!(
                !hint.contains(" restore "),
                "unhide changes before suggesting a restore: {hint}"
            );
        }
        for args in [vec!["status"], vec!["agents", "status"], vec!["doctor"]] {
            let (out, body) = run(&f.top, &args);
            assert!(!out.status.success(), "{args:?}: {body}");
            repair_diagnostic(&body);
        }
        let text = bounded(&mut command(&f.graph, &["doctor"], false));
        assert!(String::from_utf8_lossy(&text.stderr).contains("index flags"));
        assert_eq!(snapshot(&f.graph), before);
        assert_eq!(fs::read(index).expect("index untouched"), index_before);
    }
}

#[test]
fn graph_writers_cannot_bypass_freeze_through_a_linked_parent_or_graph_folder() {
    for parent in [true, false] {
        let f = fixture();
        let (entry, moved, actual) = if parent {
            (
                f.top.join(".worktrees"),
                f.top.join("saved-worktrees"),
                f.top.join("saved-worktrees/graph"),
            )
        } else {
            (
                f.graph.clone(),
                f.top.join("saved-graph"),
                f.top.join("saved-graph"),
            )
        };
        fs::rename(&entry, &moved).expect("move graph");
        std::os::unix::fs::symlink(&moved, &entry).expect("link");
        let before = snapshot(&actual);
        let local = snapshot(&f.top.join(".git/mdkg"));
        for caller in [&f.graph, &actual] {
            for args in [
                vec!["new", "task", "example"],
                vec!["update", "task-0", "--status", "done"],
                vec!["done", "task-0"],
                vec!["install", "--enable", "agents-md"],
                vec!["sync"],
                vec!["agents", "up", "worker", "--once"],
            ] {
                let (out, body) = run(caller, &args);
                assert!(!out.status.success(), "{args:?} through {caller:?}: {body}");
                assert!(
                    body["diagnostics"]
                        .to_string()
                        .contains("must be a plain folder"),
                    "{body}"
                );
            }
        }
        assert_eq!(snapshot(&actual), before);
        assert_eq!(snapshot(&f.top.join(".git/mdkg")), local);
    }
}

#[test]
fn an_unresolvable_graph_link_leaves_primary_and_sibling_writers_usable() {
    let f = fixture();
    let worker = f.top.join("worker");
    git(
        &f.top,
        &[
            "worktree",
            "add",
            "-qb",
            "worker",
            worker.to_str().expect("path"),
        ],
    );
    let moved = f.top.join("saved-graph");
    fs::rename(&f.graph, &moved).expect("move graph");
    std::os::unix::fs::symlink(&f.graph, &f.graph).expect("unresolvable graph link");
    let before = snapshot(&moved);
    for caller in [&f.top, &worker] {
        ok(caller, &["new", "task", "independent checkout"]);
    }
    let (out, _) = run(&moved, &["new", "task", "must refuse"]);
    assert!(!out.status.success());
    assert_eq!(snapshot(&moved), before);
}

#[test]
fn graph_links_to_independent_checkouts_do_not_make_their_writers_graph_writers() {
    for target_primary in [true, false] {
        let f = fixture();
        let worker = f.top.join("worker");
        git(
            &f.top,
            &[
                "worktree",
                "add",
                "-qb",
                "worker",
                worker.to_str().expect("path"),
            ],
        );
        let moved = f.top.join("saved-graph");
        fs::rename(&f.graph, &moved).expect("move graph");
        let target = if target_primary { &f.top } else { &worker };
        std::os::unix::fs::symlink(target, &f.graph).expect("graph alias to independent checkout");
        let before = snapshot(&moved);
        for caller in [&f.top, &worker] {
            ok(caller, &["new", "task", "independent checkout"]);
        }
        let (out, _) = run(&moved, &["new", "task", "must refuse"]);
        assert!(!out.status.success());
        assert_eq!(snapshot(&moved), before);
    }
}

#[test]
fn a_graph_git_pointer_to_common_refuses_before_any_writer_or_startup_changes() {
    for link in [false, true] {
        let f = fixture();
        let entry = f.graph.join(".git");
        if link {
            fs::remove_file(&entry).expect("remove pointer");
            std::os::unix::fs::symlink(f.top.join(".git"), &entry).expect("link to common");
        } else {
            fs::write(
                &entry,
                format!("gitdir: {}\n", f.top.join(".git").display()),
            )
            .expect("pointer to common");
        }
        let config = fs::read(f.graph.join(".mdkg/config.json")).expect("config");
        let index = fs::read(f.top.join(".git/index")).expect("primary index");
        let local = snapshot(&f.top.join(".git/mdkg"));
        for args in [
            vec!["new", "task", "example"],
            vec!["install", "--enable", "agents-md"],
            vec!["agents", "up", "worker", "--once"],
            vec!["sync"],
        ] {
            let (out, _) = run(&f.graph, &args);
            assert!(!out.status.success(), "{args:?}: {out:?}");
        }
        assert_eq!(
            fs::read(f.graph.join(".mdkg/config.json")).expect("config"),
            config
        );
        assert_eq!(
            fs::read(f.top.join(".git/index")).expect("primary index"),
            index
        );
        assert_eq!(snapshot(&f.top.join(".git/mdkg")), local);
        assert!(!f.graph.join("AGENTS.md").exists());
    }
}

#[test]
fn a_graph_registration_through_an_alias_still_freezes_writers() {
    for moved in [false, true] {
        let f = fixture();
        let admin = PathBuf::from(git(&f.graph, &["rev-parse", "--absolute-git-dir"]));
        let alias = f.top.join("graph-alias");
        std::os::unix::fs::symlink(&f.graph, &alias).expect("registration alias");
        fs::write(
            admin.join("gitdir"),
            format!("{}\n", alias.join(".git").display()),
        )
        .expect("alias backlink");
        fs::write(admin.join("index.lock"), "interrupted").expect("lock");
        let actual = if moved {
            let actual = f.top.join("saved-graph");
            fs::rename(&f.graph, &actual).expect("move graph");
            std::os::unix::fs::symlink(&actual, &f.graph).expect("graph slot link");
            actual
        } else {
            f.graph.clone()
        };
        let before = snapshot(&actual);
        for args in [
            vec!["new", "task", "example"],
            vec!["install", "--enable", "agents-md"],
            vec!["sync"],
            vec!["agents", "up", "worker", "--once"],
        ] {
            let (out, _) = run(&actual, &args);
            assert!(!out.status.success(), "{args:?}: {out:?}");
        }
        assert_eq!(snapshot(&actual), before);
    }
}

#[test]
fn case_only_git_worktree_moves_keep_the_freeze_and_repair_gates() {
    let f = fixture();
    let upper = f.top.join(".worktrees/Graph");
    if !upper.exists() {
        eprintln!("not run: filesystem is case-sensitive");
        return;
    }
    let temporary = f.top.join(".worktrees/moving-graph");
    git(
        &f.top,
        &[
            "worktree",
            "move",
            f.graph.to_str().expect("path"),
            temporary.to_str().expect("path"),
        ],
    );
    git(
        &f.top,
        &[
            "worktree",
            "move",
            temporary.to_str().expect("path"),
            upper.to_str().expect("path"),
        ],
    );
    ok(&upper, &["install", "--enable", "agents-md", "--dry-run"]);
    let lock = index_lock(&upper);
    fs::write(&lock, "interrupted").expect("lock");
    let before = snapshot(&upper);
    let (out, body) = run(&upper, &["new", "task", "example"]);
    assert!(!out.status.success(), "{body}");
    repair_diagnostic(&body);
    for args in [vec!["status"], vec!["agents", "status"], vec!["doctor"]] {
        let (_, body) = run(&upper, &args);
        assert!(
            repair_diagnostic(&body)["message"]
                .as_str()
                .expect("state")
                .contains("index.lock")
        );
    }
    assert_eq!(snapshot(&upper), before);
    fs::remove_file(lock).expect("repair");
    ok(&upper, &["new", "task", "repaired case variant"]);
}

#[test]
fn a_separate_git_directory_allows_init_and_primary_and_linked_writers() {
    for common_name in ["repository-data", ".git"] {
        let top =
            std::env::temp_dir().join(format!("mdkg-separate-git-repair-{}", uuid::Uuid::now_v7()));
        let project = top.join("checkout");
        fs::create_dir_all(&project).expect("project");
        let f = Fixture {
            top: fs::canonicalize(&top).expect("top"),
            graph: fs::canonicalize(&project).expect("project"),
        };
        let common = f.top.join(common_name);
        git(
            &f.graph,
            &[
                "init",
                "-qb",
                "main",
                "--separate-git-dir",
                common.to_str().expect("path"),
            ],
        );
        ok(&f.graph, &["init"]);
        ok(&f.graph, &["new", "task", "primary checkout"]);
        git(&f.graph, &["add", ".mdkg"]);
        git(&f.graph, &["commit", "-qm", "initial graph"]);
        let worker = f.top.join("worker");
        git(
            &f.graph,
            &[
                "worktree",
                "add",
                "-qb",
                "worker",
                worker.to_str().expect("path"),
            ],
        );
        ok(&worker, &["new", "task", "linked checkout"]);
        ok(&f.graph, &["new", "task", "primary with linked checkout"]);
    }
}

#[test]
fn init_beneath_a_frozen_or_damaged_checkout_refuses_without_creating_the_directory() {
    for damaged_git in [false, true] {
        let f = fixture();
        if damaged_git {
            fs::write(f.graph.join(".git"), "broken Git entry").expect("damaged topology");
        } else {
            freeze(&f.graph);
        }
        let missing = f.graph.join("missing/nested");
        let before = snapshot(&f.graph);
        // Use the existing checkout as cwd but the missing directory as root.
        let mut command = Command::new(BIN);
        isolated(&mut command, &f.top);
        command
            .current_dir(&f.graph)
            .arg("--root")
            .arg(&missing)
            .args(["init", "--json"]);
        let out = bounded(&mut command);
        assert!(!out.status.success(), "{out:?}");
        if !damaged_git {
            let body: Value = serde_json::from_slice(&out.stdout).expect("JSON");
            repair_diagnostic(&body);
        }
        assert!(!f.graph.join("missing").exists());
        assert_eq!(snapshot(&f.graph), before);
    }
}

#[test]
fn frozen_diagnostics_return_with_build_warnings_while_receiver_and_graph_locks_are_held() {
    let f = fixture();
    let dir = mdkg_run::state::run_dir(&f.graph).expect("run dir");
    let version = ok(&f.graph, &["version"]);
    let build = Build {
        version: version["data"]["version"].as_str().expect("version").into(),
        commit: "sample-older-build".into(),
        dirty: Some(false),
        built: version["data"]["built"].as_str().expect("built").into(),
    };
    let _build =
        builds::publish(&dir, Kind::Mcp, Path::new(BIN), build, vec![]).expect("build receipt");
    let common = f.top.join(".git");
    let hooks = mdkg_run::hooks::dir(&common);
    fs::create_dir_all(hooks.join("receiver")).expect("receiver folder");
    let mut locks = Vec::new();
    for path in [
        hooks.join("threads.lock"),
        hooks.join("receiver/observer.lock"),
        common.join("mdkg/graph-worktree.lock"),
    ] {
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .expect("lock file");
        lock.lock().expect("exclusive lock");
        locks.push(lock);
    }
    freeze(&f.graph);
    let before = snapshot(&f.graph);
    for args in [vec!["status"], vec!["agents", "status"], vec!["doctor"]] {
        let (out, body) = run(&f.graph, &args);
        assert!(!out.status.success(), "{args:?}: {body}");
        repair_diagnostic(&body);
        assert!(
            body["diagnostics"]
                .as_array()
                .expect("diagnostics")
                .iter()
                .any(|d| d["message"]
                    .as_str()
                    .is_some_and(|m| m.contains("mixed mdkg builds"))),
            "{body}"
        );
    }
    assert_eq!(snapshot(&f.graph), before);
    drop(locks);
}

#[test]
fn printed_repair_commands_thaw_writers_and_preserve_untracked_and_ignored_files() {
    let f = fixture();
    freeze(&f.graph);
    fs::write(f.graph.join("local-notes"), "wanted untracked content").expect("notes");
    fs::OpenOptions::new()
        .append(true)
        .open(f.top.join(".git/info/exclude"))
        .expect("exclude")
        .write_all(b"\nlocal-cache\n")
        .expect("ignore cache");
    fs::write(f.graph.join("local-cache"), "wanted ignored content").expect("cache");
    let (_, body) = run(&f.graph, &["doctor"]);
    let hint = repair_diagnostic(&body)["hint"].as_str().expect("hint");
    for line in hint.lines().filter(|l| l.contains("restore --source=HEAD")) {
        let cmd = line.split_once(": git ").expect("exact command").1;
        let mut command = Command::new("/bin/sh");
        isolated(&mut command, &f.top);
        command.args(["-c", &format!("git {cmd}")]);
        assert!(bounded(&mut command).status.success());
    }
    ok(&f.graph, &["new", "task", "after repair"]);
    assert_eq!(
        fs::read_to_string(f.graph.join("local-notes")).expect("notes"),
        "wanted untracked content"
    );
    assert_eq!(
        fs::read_to_string(f.graph.join("local-cache")).expect("cache"),
        "wanted ignored content"
    );
    let (_, doctor) = run(&f.graph, &["doctor"]);
    assert!(
        doctor["data"]["checks"]
            .as_array()
            .expect("checks")
            .iter()
            .any(|c| c["name"] == "graph_worktree" && c["status"] == "ok")
    );
}

#[test]
fn an_unaffected_checkout_can_create_nodes_while_the_shared_graph_is_frozen() {
    let f = fixture();
    freeze(&f.graph);
    let before = snapshot(&f.graph);
    ok(&f.top, &["new", "task", "outside the frozen checkout"]);
    assert_eq!(snapshot(&f.graph), before);
}

#[test]
fn pinned_primary_and_sibling_diagnostics_still_show_the_shared_graph_repair() {
    let f = fixture();
    let sibling = f.top.join("sibling");
    git(
        &f.top,
        &[
            "worktree",
            "add",
            "-qb",
            "sibling",
            sibling.to_str().expect("path"),
            "main",
        ],
    );
    freeze(&f.graph);
    for caller in [&f.top, &sibling] {
        for args in [vec!["status"], vec!["agents", "status"], vec!["doctor"]] {
            let mut cmd = command(caller, &args, true);
            cmd.env("MDKG_TRUSTED_COMMON_DIR", f.top.join(".git"))
                .env("MDKG_TRUSTED_WORKTREE", caller);
            let out = bounded(&mut cmd);
            let body: Value = serde_json::from_slice(&out.stdout).expect("diagnostic envelope");
            assert!(!out.status.success(), "{args:?}: {body}");
            let repair = repair_diagnostic(&body);
            assert!(
                repair["message"]
                    .as_str()
                    .expect("state")
                    .contains(".mdkg/config.json")
            );
            assert!(
                repair["hint"]
                    .as_str()
                    .expect("guidance")
                    .contains(" status")
            );
        }
    }
}

fn identity_key(root: &Path) -> String {
    let config: Value =
        serde_json::from_slice(&fs::read(root.join(".mdkg/config.json")).expect("config"))
            .expect("JSON");
    format!(
        "mdkg.graph.{}.worktree",
        config["graph_id"].as_str().expect("graph ID")
    )
}

#[test]
fn missing_duplicate_and_wrong_identity_records_refuse_without_repairing_them() {
    for state in ["missing", "duplicate", "wrong", "unsafe-admin"] {
        let f = fixture();
        let key = identity_key(&f.top);
        let worker = f.top.join("worker");
        git(
            &f.top,
            &[
                "worktree",
                "add",
                "-qb",
                "worker",
                worker.to_str().expect("path"),
            ],
        );
        match state {
            "missing" => {
                git(&f.top, &["config", "--local", "--unset-all", &key]);
            }
            "duplicate" => {
                git(&f.top, &["config", "--local", "--add", &key, "graph"]);
            }
            "wrong" => {
                git(&f.top, &["config", "--local", &key, "worker"]);
            }
            _ => {
                git(&f.top, &["config", "--local", &key, "../graph"]);
            }
        }
        let before = snapshot(&f.graph);
        let local = fs::read(f.top.join(".git/config")).expect("local config");
        for args in [
            vec!["new", "task", "must refuse"],
            vec!["install", "--enable", "agents-md"],
            vec!["sync"],
            vec!["agents", "up", "worker", "--once"],
        ] {
            let (out, body) = run(&f.graph, &args);
            assert!(!out.status.success(), "{state} {args:?}: {body}");
            assert!(body.to_string().contains("identity needs repair"), "{body}");
            assert!(body.to_string().contains("install"), "{body}");
        }
        let (_, doctor) = run(&f.top, &["doctor"]);
        assert!(
            doctor["data"]["checks"]
                .as_array()
                .expect("checks")
                .iter()
                .any(|check| check["name"] == "graph_worktree" && check["status"] == "error"),
            "{doctor}"
        );
        assert_eq!(snapshot(&f.graph), before);
        assert_eq!(
            fs::read(f.top.join(".git/config")).expect("local config"),
            local
        );
        // The ordinary primary remains a usable writer despite bad registration.
        ok(&f.top, &["new", "task", "primary remains independent"]);
    }
}

#[test]
fn moved_graph_identity_survives_branch_changes_and_keeps_observation_only_freeze() {
    let f = fixture();
    let moved = f.top.join("renamed-graph");
    git(
        &f.top,
        &[
            "worktree",
            "move",
            f.graph.to_str().expect("path"),
            moved.to_str().expect("path"),
        ],
    );
    git(&moved, &["switch", "-qc", "wrong-branch"]);
    let before = snapshot(&moved);
    let local = fs::read(f.top.join(".git/config")).expect("local config");
    for args in [
        vec!["new", "task", "must refuse"],
        vec!["sync"],
        vec!["agents", "up", "worker", "--once"],
    ] {
        let (out, body) = run(&moved, &args);
        assert!(!out.status.success(), "{args:?}: {body}");
        let diagnostic = repair_diagnostic(&body);
        assert!(
            diagnostic["message"]
                .as_str()
                .expect("reason")
                .contains("HEAD is not")
        );
        assert!(diagnostic.to_string().contains("renamed-graph"));
    }
    let (_, doctor) = run(&f.top, &["doctor"]);
    assert!(doctor.to_string().contains("renamed-graph"), "{doctor}");
    assert_eq!(snapshot(&moved), before);
    assert_eq!(
        fs::read(f.top.join(".git/config")).expect("local config"),
        local
    );
    git(&moved, &["switch", "mdkg-graph"]);
    ok(&f.top, &["install"]);
    ok(&moved, &["new", "task", "repaired graph"]);
}

#[test]
fn identity_review_moved_checkout_missing_record_is_visible_to_all_diagnostics() {
    let f = fixture();
    let key = identity_key(&f.top);
    let moved = f.top.join("relocated");
    git(
        &f.top,
        &[
            "worktree",
            "move",
            f.graph.to_str().expect("path"),
            moved.to_str().expect("path"),
        ],
    );
    git(&f.top, &["config", "--local", "--unset-all", &key]);
    let config = fs::read(f.top.join(".git/config")).expect("local config");
    for caller in [&f.top, &moved] {
        for args in [vec!["status"], vec!["agents", "status"]] {
            let (out, body) = run(caller, &args);
            assert!(!out.status.success(), "{caller:?} {args:?}: {body}");
            assert!(body.to_string().contains("missing"), "{body}");
        }
        let (_, doctor) = run(caller, &["doctor"]);
        assert!(
            doctor["data"]["checks"]
                .as_array()
                .expect("checks")
                .iter()
                .any(|check| check["name"] == "graph_worktree" && check["status"] == "error"),
            "{doctor}"
        );
    }
    assert_eq!(fs::read(f.top.join(".git/config")).expect("config"), config);
}
