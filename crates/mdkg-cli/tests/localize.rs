//! End-to-end localize checks. These Git-writing fixtures are compiled in
//! restricted worker turns and executed by the central acceptance runner.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

mod common;

struct Fixture {
    root: PathBuf,
    id: String,
    path: String,
    goal: String,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).ok();
    }
}

fn isolated(command: &mut Command) -> &mut Command {
    common::without_launcher_env(command)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .env("MDKG_LEASE_TTL_MS", "120000")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("MDKG_NOW_MS")
}

fn git(root: &Path, args: &[&str]) -> String {
    let out = isolated(Command::new("git").current_dir(root).args(args))
        .output()
        .expect("valid localize fixture");
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8(out.stdout)
        .expect("valid localize fixture")
        .trim()
        .into()
}

fn run(root: &Path, actor: &str, args: &[&str]) -> (i32, Value) {
    let out = isolated(
        Command::new(env!("CARGO_BIN_EXE_mdkg"))
            .current_dir(root)
            .args(["--as", actor])
            .args(args)
            .arg("--json"),
    )
    .output()
    .expect("valid localize fixture");
    let value: Value =
        serde_json::from_slice(&out.stdout).unwrap_or_else(|_| panic!("{args:?}: {out:?}"));
    common::conforms_envelope(&value);
    (out.status.code().unwrap_or(-1), value)
}

fn ok(root: &Path, args: &[&str]) -> Value {
    let (code, out) = run(root, "localizer", args);
    assert_eq!(code, 0, "{args:?}: {out}");
    out
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("mdkg-localize-cli-{}", uuid::Uuid::now_v7()));
        fs::create_dir_all(&root).expect("valid localize fixture");
        let root = fs::canonicalize(root).expect("valid localize fixture");
        fs::write(root.join("README.md"), "fixture\n").expect("valid localize fixture");
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["add", "README.md"]);
        git(&root, &["commit", "-qm", "start"]);
        ok(&root, &["init", "--work", "shared", "--no-gitignore"]);
        let goal = ok(&root, &["new", "goal", "Keep design"]);
        let goal_id = goal["data"]["node"]["id"]
            .as_str()
            .expect("valid localize fixture");
        let task = ok(
            &root,
            &["new", "task", "Keep all bytes", "--parent", goal_id],
        );
        git(&root, &["add", ".mdkg"]);
        git(&root, &["commit", "-qm", "graph"]);
        git(&root, &["checkout", "-qb", "convert"]);
        Self {
            root,
            id: task["data"]["node"]["id"]
                .as_str()
                .expect("valid localize fixture")
                .into(),
            path: task["data"]["node"]["path"]
                .as_str()
                .expect("valid localize fixture")
                .into(),
            goal: goal["data"]["node"]["path"]
                .as_str()
                .expect("valid localize fixture")
                .into(),
        }
    }

    fn plan(&self) -> Value {
        ok(&self.root, &["work", "localize", "--dry-run"])
    }
}

fn graph_bytes(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    fn walk(dir: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
        for entry in fs::read_dir(dir).expect("valid localize fixture") {
            let entry = entry.expect("valid localize fixture");
            if entry.file_type().expect("valid localize fixture").is_dir() {
                walk(&entry.path(), out)
            } else {
                out.push((
                    entry.path(),
                    fs::read(entry.path()).expect("valid localize fixture"),
                ));
            }
        }
    }
    let mut out = Vec::new();
    walk(&root.join(".mdkg"), &mut out);
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

#[test]
fn localize_dry_run_is_read_only_and_the_plan_is_repeatable() {
    let f = Fixture::new();
    let graph = graph_bytes(&f.root);
    let head = git(&f.root, &["rev-parse", "HEAD"]);
    let plan = f.plan();
    assert_eq!(plan["data"]["plan"], f.plan()["data"]["plan"]);
    assert_eq!(plan["data"]["applied"], false);
    assert_eq!(plan["data"]["nodes"][0]["id"], f.id);
    assert_eq!(graph_bytes(&f.root), graph);
    assert_eq!(git(&f.root, &["rev-parse", "HEAD"]), head);
    assert!(!f.root.join(".git/mdkg/localize").exists());
    assert!(!f.root.join(".git/mdkg/work/root").exists());
}

#[test]
fn localize_commits_only_the_config_and_removals_and_keeps_private_bytes() {
    let f = Fixture::new();
    let bytes = fs::read(f.root.join(&f.path)).expect("valid localize fixture");
    ok(&f.root, &["claim", &f.id]);
    ok(&f.root, &["update", &f.id, "--status", "blocked"]);
    ok(&f.root, &["release", &f.id]);
    fs::write(f.root.join("unrelated.txt"), "keep staged\n").expect("valid localize fixture");
    git(&f.root, &["add", "unrelated.txt"]);
    let p = f.plan();
    let hash = p["data"]["plan"].as_str().expect("valid localize fixture");
    let applied = ok(&f.root, &["work", "localize", "--apply", hash]);
    common::conforms("work.localize", &applied);
    let target = applied["data"]["nodes"][0]["to"]
        .as_str()
        .expect("valid localize fixture");
    assert_eq!(fs::read(target).expect("valid localize fixture"), bytes);
    assert!(!f.root.join(&f.path).exists());
    let cfg: Value = serde_json::from_slice(
        &fs::read(f.root.join(".mdkg/config.json")).expect("valid localize fixture"),
    )
    .expect("valid localize fixture");
    assert_eq!(cfg["work"]["location"], "local");
    assert!(cfg["work"].get("committed").is_none());
    let paths = git(
        &f.root,
        &["diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"],
    );
    let mut paths: Vec<_> = paths.lines().collect();
    paths.sort();
    let mut expected = vec![".mdkg/config.json", f.path.as_str()];
    expected.sort();
    assert_eq!(paths, expected);
    assert_eq!(
        git(&f.root, &["diff", "--cached", "--name-only"]),
        "unrelated.txt"
    );
    assert_eq!(
        ok(&f.root, &["show", &f.id])["data"]["node"]["status"],
        "blocked"
    );
    assert!(
        Path::new(
            applied["data"]["backup"]
                .as_str()
                .expect("valid localize fixture")
        )
        .join("config.json")
        .is_file()
    );
    let again = ok(&f.root, &["work", "localize", "--apply", hash]);
    assert_eq!(again["data"]["commit"], applied["data"]["commit"]);
    assert_eq!(f.plan()["data"]["already_local"], true);
}

#[test]
fn localize_refuses_a_stale_plan_and_a_conflicting_lease() {
    let f = Fixture::new();
    let p = f.plan();
    let hash = p["data"]["plan"].as_str().expect("valid localize fixture");
    let before = graph_bytes(&f.root);
    fs::write(
        f.root.join(&f.path),
        fs::read_to_string(f.root.join(&f.path)).expect("valid localize fixture")
            + "\nMore work.\n",
    )
    .expect("valid localize fixture");
    git(&f.root, &["add", ".mdkg"]);
    git(&f.root, &["commit", "-qm", "new work"]);
    assert_ne!(
        run(&f.root, "localizer", &["work", "localize", "--apply", hash]).0,
        0
    );
    let current = graph_bytes(&f.root);
    assert_ne!(before, current);
    let p = f.plan();
    let hash = p["data"]["plan"].as_str().expect("valid localize fixture");
    let (code, claim) = run(&f.root, "someone-else", &["claim", &f.id]);
    assert_eq!(code, 0, "{claim}");
    assert_ne!(
        run(&f.root, "localizer", &["work", "localize", "--apply", hash]).0,
        0
    );
    assert_eq!(current, graph_bytes(&f.root));
    assert!(
        !f.root
            .join(".git/mdkg/work/root")
            .join(format!("{}.md", f.id))
            .exists()
    );
}

#[test]
fn localize_refuses_committed_references_into_work_and_existing_private_work() {
    let f = Fixture::new();
    let text = fs::read_to_string(f.root.join(&f.goal)).expect("valid localize fixture");
    let changed = text.replacen("\nrefs: []", &format!("\nrefs: [{}]", f.id), 1);
    assert_ne!(changed, text);
    fs::write(f.root.join(&f.goal), changed).expect("valid localize fixture");
    git(&f.root, &["add", ".mdkg"]);
    git(&f.root, &["commit", "-qm", "incoming ref"]);
    let (code, error) = run(&f.root, "localizer", &["work", "localize", "--dry-run"]);
    assert_ne!(code, 0);
    assert!(error.to_string().contains("edge.private_target"));
    fs::write(f.root.join(&f.goal), text).expect("valid localize fixture");
    git(&f.root, &["add", ".mdkg"]);
    git(&f.root, &["commit", "-qm", "clear ref"]);
    let dir = f.root.join(".git/mdkg/work/root");
    fs::create_dir_all(&dir).expect("valid localize fixture");
    fs::write(dir.join("other.md"), "existing private work").expect("valid localize fixture");
    let before = graph_bytes(&f.root);
    assert_ne!(
        run(&f.root, "localizer", &["work", "localize", "--dry-run"]).0,
        0
    );
    assert_eq!(before, graph_bytes(&f.root));
    assert_eq!(
        fs::read_to_string(dir.join("other.md")).expect("valid localize fixture"),
        "existing private work"
    );
}

#[test]
fn localize_refuses_committed_body_links_into_moved_work() {
    let f = Fixture::new();
    let text = fs::read_to_string(f.root.join(&f.goal)).expect("goal");
    fs::write(
        f.root.join(&f.goal),
        format!("{text}\n[work being moved]({})\n", f.id),
    )
    .expect("incoming body link");
    git(&f.root, &["add", ".mdkg"]);
    git(&f.root, &["commit", "-qm", "incoming body link"]);
    let before = graph_bytes(&f.root);
    let head = git(&f.root, &["rev-parse", "HEAD"]);
    let (code, out) = run(&f.root, "localizer", &["work", "localize", "--dry-run"]);
    assert_ne!(code, 0, "{out}");
    let diagnostic = out["diagnostics"]
        .as_array()
        .expect("diagnostics")
        .iter()
        .find(|d| d["code"] == "edge.private_target")
        .expect("body link refusal");
    assert_eq!(diagnostic["path"], f.goal);
    assert!(
        diagnostic["message"]
            .as_str()
            .expect("message")
            .contains(&f.id)
    );
    assert_eq!(graph_bytes(&f.root), before);
    assert_eq!(git(&f.root, &["rev-parse", "HEAD"]), head);
    assert!(!f.root.join(".git/mdkg/localize").exists());
}
