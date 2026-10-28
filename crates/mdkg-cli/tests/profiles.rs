//! A whole session in each work profile (edd-5, task-39): init, new,
//! claim, update, done, then reads, through the binary in throwaway
//! repositories. Under profile C nothing of the work types reaches git.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

mod common;

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

/// An empty repository with one commit, so main exists.
fn empty_repo(name: &str) -> Scratch {
    let dir = std::env::temp_dir().join(format!(
        "mdkg-cli-profiles-{name}-{}-{}",
        std::process::id(),
        mdkg_format::Uuid7::now()
    ));
    fs::create_dir_all(&dir).expect("scratch");
    let dir = fs::canonicalize(dir).expect("canonical");
    fs::write(dir.join("README.md"), "scratch\n").expect("readme");
    sh(&dir, &["init", "-q", "-b", "main"]);
    sh(&dir, &["add", "."]);
    sh(&dir, &["commit", "-q", "-m", "start"]);
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

fn run_as(root: &Path, actor: &str, args: &[&str]) -> (i32, Value) {
    let out = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
        .arg("--root")
        .arg(root)
        .args(["--as", actor])
        .args(args)
        .arg("--json")
        .output()
        .expect("run mdkg");
    let envelope: Value = serde_json::from_slice(&out.stdout).expect("an envelope");
    common::conforms_envelope(&envelope);
    (out.status.code().unwrap_or(-1), envelope)
}

fn ok(root: &Path, args: &[&str]) -> Value {
    let (code, out) = run(root, args);
    assert_eq!(code, 0, "{args:?}: {out}");
    out
}

/// Every path git has ever held, in any commit, plus the worktree's
/// untracked files.
fn everything_git_saw(dir: &Path) -> String {
    let history = sh(dir, &["log", "--all", "--name-only", "--format="]);
    let status = sh(dir, &["status", "--porcelain", "--untracked-files=all"]);
    format!("{history}\n{status}")
}

fn session(profile: &str) -> (Scratch, String, String) {
    let repo = empty_repo(profile);
    ok(&repo.0, &["init", "--work", profile]);
    sh(&repo.0, &["add", "."]);
    sh(&repo.0, &["commit", "-q", "-m", "init"]);
    let edd = ok(&repo.0, &["new", "edd", "The", "design"]);
    let edd_path = edd["data"]["node"]["path"]
        .as_str()
        .expect("path")
        .to_string();
    sh(&repo.0, &["add", "."]);
    sh(&repo.0, &["commit", "-q", "-m", "design"]);
    let task = ok(&repo.0, &["new", "task", "Ship", "the", "thing"]);
    let id = task["data"]["node"]["id"].as_str().expect("id").to_string();
    if profile == "shared" {
        sh(&repo.0, &["add", "."]);
        sh(&repo.0, &["commit", "-q", "-m", "task"]);
    }
    ok(&repo.0, &["claim", &id]);
    ok(&repo.0, &["release", &id]);
    ok(&repo.0, &["claim", &id]);
    // Another actor can't take it meanwhile.
    let (code, v) = run_as(&repo.0, "lane-b", &["claim", &id]);
    assert_eq!(code, 5, "{v}");
    ok(&repo.0, &["update", &id, "--status", "review"]);
    let shown = ok(&repo.0, &["show", &id]);
    assert!(shown.to_string().contains("review"), "{shown}");
    ok(&repo.0, &["update", &id, "--clear"]);
    ok(&repo.0, &["done", &id]);
    let shown = ok(&repo.0, &["show", &id]);
    assert_eq!(shown["data"]["node"]["status"], "done", "{shown}");
    let listed = ok(
        &repo.0,
        &["list", "--type", "task", "--fields", "id,status"],
    );
    assert!(listed.to_string().contains(&id), "{listed}");
    let found = ok(&repo.0, &["search", "thing"]);
    assert!(found.to_string().contains(&id), "{found}");
    let (code, v) = run(&repo.0, &["validate"]);
    assert_eq!(code, 0, "{v}");
    (repo, id, edd_path)
}

#[test]
fn profile_b_commits_work_definitions() {
    let (repo, _, edd) = session("shared");
    let seen = everything_git_saw(&repo.0);
    assert!(seen.contains(".mdkg/work/task-ship-the-thing.md"), "{seen}");
    assert!(seen.contains(&edd), "{seen}");
}

#[test]
fn profile_c_keeps_every_work_node_out_of_git() {
    let (repo, _, edd) = session("local");
    // A checkpoint and a retyped node stay out of git too.
    let second = ok(&repo.0, &["new", "task", "Second"]);
    let second = second["data"]["node"]["id"]
        .as_str()
        .expect("id")
        .to_string();
    let done = ok(&repo.0, &["done", &second, "--checkpoint"]);
    assert!(done["data"]["checkpoint"].is_string(), "{done}");
    let third = ok(&repo.0, &["new", "task", "Third"]);
    let third = third["data"]["node"]["id"]
        .as_str()
        .expect("id")
        .to_string();
    ok(&repo.0, &["retype", &third, "bug"]);
    // Never across planes.
    let (code, v) = run(&repo.0, &["retype", &third, "edd"]);
    assert_eq!(code, 1, "{v}");
    let (code, v) = run(&repo.0, &["validate"]);
    assert_eq!(code, 0, "{v}");
    let seen = everything_git_saw(&repo.0);
    assert!(
        !seen.contains("task-"),
        "no task file ever reached git: {seen}"
    );
    assert!(seen.contains(&edd), "knowledge stays in git: {seen}");
}

/// Under C, a task made in one worktree is claimed in another and done
/// from a third place, with one lease between them all.
#[test]
fn profile_c_work_moves_between_worktrees() {
    let repo = empty_repo("c-worktrees");
    ok(&repo.0, &["init", "--work", "local"]);
    sh(&repo.0, &["add", "."]);
    sh(&repo.0, &["commit", "-q", "-m", "init"]);
    let linked = repo.0.with_file_name(format!(
        "{}-wt",
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
    let task = ok(&repo.0, &["new", "task", "Shared"]);
    let id = task["data"]["node"]["id"].as_str().expect("id").to_string();
    ok(&linked, &["claim", &id]);
    // The same lease from the main checkout: held, by the same scope.
    let (code, v) = run_as(&repo.0, "lane-b", &["claim", &id]);
    assert_eq!(code, 5, "{v}");
    // Every worktree sees it held: in progress, never handed out.
    let shown = ok(&repo.0, &["show", &id]);
    assert_eq!(shown["data"]["node"]["status"], "progress", "{shown}");
    let (_, next) = run_as(&repo.0, "lane-b", &["next"]);
    assert!(!next.to_string().contains(&id), "next skips it: {next}");
    // Its records name the node: scope (format 9.1).
    let scope = format!("node:{}", id.split_once('-').map_or("", |(_, u)| u));
    let bundle = repo.0.join("work.jsonl");
    ok(
        &repo.0,
        &["export", "--work", "--out", bundle.to_str().expect("utf-8")],
    );
    let text = fs::read_to_string(&bundle).expect("bundle");
    assert!(text.contains(&format!("\"{scope}\"")), "{text}");
    assert!(!text.contains("path:node:"), "{text}");
    fs::remove_file(&bundle).ok();
    ok(&linked, &["update", &id, "--status", "review"]);
    ok(&linked, &["update", &id, "--clear"]);
    ok(&linked, &["done", &id]);
    // Status and done records name the node: scope too.
    let bundle = repo.0.join("after.jsonl");
    ok(
        &repo.0,
        &["export", "--work", "--out", bundle.to_str().expect("utf-8")],
    );
    let text = fs::read_to_string(&bundle).expect("bundle");
    assert!(text.contains("\"kind\":\"work.status\""), "{text}");
    assert!(!text.contains("path:node:"), "{text}");
    fs::remove_file(&bundle).ok();
    let shown = ok(&repo.0, &["show", &id]);
    assert_eq!(shown["data"]["node"]["status"], "done", "{shown}");
    assert!(!everything_git_saw(&repo.0).contains("task-"));
    fs::remove_dir_all(&linked).ok();
}

/// Review of 8e48575: a C edit that fails to write puts nothing back over
/// anyone, and its lease goes only after.
#[test]
fn a_failed_c_edit_leaves_the_file_and_frees_the_lease() {
    use std::os::unix::fs::PermissionsExt;
    let repo = empty_repo("c-fail");
    ok(&repo.0, &["init", "--work", "local"]);
    sh(&repo.0, &["add", "."]);
    sh(&repo.0, &["commit", "-q", "-m", "init"]);
    let task = ok(&repo.0, &["new", "task", "Fragile"]);
    let id = task["data"]["node"]["id"].as_str().expect("id").to_string();
    let file = repo
        .0
        .join(task["data"]["node"]["path"].as_str().expect("path"));
    let folder = file.parent().expect("folder").to_path_buf();
    let before = fs::read(&file).expect("file");
    fs::set_permissions(&folder, fs::Permissions::from_mode(0o555)).expect("chmod");
    let (code, v) = run(&repo.0, &["done", &id]);
    fs::set_permissions(&folder, fs::Permissions::from_mode(0o755)).expect("chmod");
    assert_ne!(code, 0, "{v}");
    assert_eq!(fs::read(&file).expect("file"), before, "unchanged");
    // The lease it took is gone: someone else can claim.
    let (code, v) = run_as(&repo.0, "lane-b", &["claim", &id]);
    assert_eq!(code, 0, "{v}");
}

/// Lane 4's note: `status` lists a C node's recorded state by its node:
/// name, and `update <that> --clear` clears it, as with a path.
#[test]
fn a_c_state_clears_by_the_name_status_lists() {
    let repo = empty_repo("c-clear");
    ok(&repo.0, &["init", "--work", "local"]);
    sh(&repo.0, &["add", "."]);
    sh(&repo.0, &["commit", "-q", "-m", "init"]);
    let task = ok(&repo.0, &["new", "task", "Blocked", "one"]);
    let id = task["data"]["node"]["id"].as_str().expect("id").to_string();
    let name = format!("node:{}", id.split_once('-').map_or("", |(_, u)| u));
    ok(&repo.0, &["update", &id, "--status", "blocked"]);
    let status = ok(&repo.0, &["status"]);
    let text = status.to_string();
    assert!(text.contains(&name), "{text}");
    ok(&repo.0, &["update", &name, "--clear"]);
    let shown = ok(&repo.0, &["show", &id]);
    assert_ne!(shown["data"]["node"]["status"], "blocked", "{shown}");
    // Also once the node's file is gone: the state clears by its name.
    ok(&repo.0, &["update", &id, "--status", "blocked"]);
    fs::remove_file(
        repo.0
            .join(task["data"]["node"]["path"].as_str().expect("path")),
    )
    .expect("remove");
    ok(&repo.0, &["update", &name, "--clear"]);
    let status = ok(&repo.0, &["status"]);
    assert!(!status.to_string().contains("blocked"), "{status}");
}

/// Task-47: format --write rewrites a shared C node under its lease,
/// whole; one another actor holds is reported and left as is.
#[test]
fn format_rewrites_shared_c_nodes_under_their_lease() {
    let repo = empty_repo("c-format");
    ok(&repo.0, &["init", "--work", "local"]);
    sh(&repo.0, &["add", "."]);
    sh(&repo.0, &["commit", "-q", "-m", "init"]);
    let task = ok(&repo.0, &["new", "task", "Messy"]);
    let id = task["data"]["node"]["id"].as_str().expect("id").to_string();
    let file = repo
        .0
        .join(task["data"]["node"]["path"].as_str().expect("path"));
    // Swap the first two frontmatter lines: valid, but not canonical.
    let text = fs::read_to_string(&file).expect("task");
    let mut lines: Vec<&str> = text.lines().collect();
    lines.swap(1, 2);
    let messy = format!("{}\n", lines.join("\n"));
    fs::write(&file, &messy).expect("write");
    // Held by someone else: left as is.
    let (code, v) = run_as(&repo.0, "lane-b", &["claim", &id]);
    assert_eq!(code, 0, "{v}");
    let out = ok(&repo.0, &["format", "--write"]);
    assert!(
        out["data"]["skipped"].to_string().contains("task-messy"),
        "{out}"
    );
    assert_eq!(fs::read_to_string(&file).expect("task"), messy, "untouched");
    let (code, v) = run_as(&repo.0, "lane-b", &["release", &id]);
    assert_eq!(code, 0, "{v}");
    // Free: rewritten, and its lease ends with the write.
    let out = ok(&repo.0, &["format", "--write"]);
    assert!(out["data"]["skipped"].is_null(), "{out}");
    assert_eq!(
        fs::read_to_string(&file).expect("task"),
        text,
        "canonical again"
    );
    let (code, v) = run_as(&repo.0, "lane-b", &["claim", &id]);
    assert_eq!(code, 0, "nobody holds it after: {v}");
}
