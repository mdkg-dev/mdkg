//! Install prepares a graph checkout without changing existing work.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

mod common;

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).ok();
    }
}

fn isolated(c: &mut Command) -> &mut Command {
    common::without_launcher_env(c)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .env("MDKG_RUN_SECURITY", "/usr/bin/false")
        .env("MDKG_RUN_CODEX", "/usr/bin/false")
}

fn git(root: &Path, args: &[&str]) -> String {
    let out = isolated(Command::new("git").arg("-C").arg(root).args(args))
        .output()
        .expect("git");
    assert!(out.status.success(), "{args:?}: {out:?}");
    String::from_utf8(out.stdout).expect("UTF-8").trim().into()
}

fn mdkg(root: &Path, args: &[&str]) -> Output {
    isolated(
        Command::new(env!("CARGO_BIN_EXE_mdkg"))
            .arg("--root")
            .arg(root)
            .args(args)
            .arg("--json"),
    )
    .output()
    .expect("mdkg")
}

fn ok(root: &Path, args: &[&str]) -> Value {
    let out = mdkg(root, args);
    assert!(out.status.success(), "{args:?}: {out:?}");
    let value = serde_json::from_slice(&out.stdout).expect("JSON");
    common::conforms_envelope(&value);
    value
}

fn repo(commit: bool) -> Scratch {
    let path = std::env::temp_dir().join(format!(
        "mdkg-install-worktree-{}-{}",
        std::process::id(),
        uuid::Uuid::now_v7()
    ));
    fs::create_dir_all(&path).expect("scratch");
    let path = fs::canonicalize(path).expect("canonical");
    git(&path, &["init", "-q", "-b", "main"]);
    ok(&path, &["init"]);
    if commit {
        git(&path, &["add", ".mdkg"]);
        git(&path, &["commit", "-q", "-m", "graph"]);
    }
    Scratch(path)
}

#[test]
fn install_creates_graph_checkout_once_and_dry_run_changes_nothing() {
    let r = repo(true);
    // Init's ignore behavior is covered by its own suite; start from none.
    fs::remove_file(r.0.join(".gitignore")).ok();
    let graph = r.0.join(".worktrees/graph");
    let local_before = fs::read(r.0.join(".git/config")).expect("local config");
    ok(&r.0, &["install", "--dry-run"]);
    assert_eq!(
        fs::read(r.0.join(".git/config")).expect("config"),
        local_before
    );
    assert!(!graph.exists());
    assert!(!r.0.join(".gitignore").exists());
    let out = ok(&r.0, &["install", "--enable", "agents-md"]);
    assert!(graph.join(".mdkg/config.json").is_file(), "{out}");
    assert_eq!(
        git(&graph, &["symbolic-ref", "--short", "HEAD"]),
        "mdkg-graph"
    );
    assert_eq!(
        git(&graph, &["rev-parse", "HEAD"]),
        git(&r.0, &["rev-parse", "main"])
    );
    assert_eq!(
        fs::read_to_string(r.0.join(".gitignore")).expect("ignore"),
        "/.worktrees/\n"
    );
    let key = identity_key(&r.0);
    assert_eq!(
        git(&r.0, &["config", "--local", "--get-all", &key]),
        "graph"
    );
    let local_installed = fs::read(r.0.join(".git/config")).expect("config");
    ok(&r.0, &["install"]);
    assert_eq!(
        fs::read(r.0.join(".git/config")).expect("config"),
        local_installed
    );
    assert_eq!(
        fs::read_to_string(r.0.join(".gitignore")).expect("ignore"),
        "/.worktrees/\n"
    );
    ok(&r.0, &["install", "--undo"]);
    assert!(graph.join(".git").is_file());
}

#[test]
fn installing_enabled_projections_in_the_clean_graph_checkout_succeeds() {
    let r = repo(true);
    ok(&r.0, &["install"]);
    let graph = r.0.join(".worktrees/graph");
    let before = fs::read(graph.join(".mdkg/config.json")).expect("config");
    ok(
        &graph,
        &["install", "--enable", "agents-md,mcp:claude", "--dry-run"],
    );
    assert_eq!(
        fs::read(graph.join(".mdkg/config.json")).expect("config"),
        before
    );
    assert!(!graph.join("AGENTS.md").exists());
    let result = ok(&graph, &["install", "--enable", "agents-md,mcp:claude"]);
    assert!(result.to_string().contains("graph worktree"), "{result}");
    assert!(graph.join("AGENTS.md").is_file());
    assert!(graph.join(".mcp.json").is_file());
    let config: Value =
        serde_json::from_slice(&fs::read(graph.join(".mdkg/config.json")).expect("config"))
            .expect("JSON");
    assert_eq!(
        config["projections"],
        serde_json::json!(["agents-md", "mcp:claude"])
    );
    assert_eq!(
        fs::read(r.0.join(".mdkg/config.json")).expect("primary config"),
        before
    );
    git(&graph, &["add", ".mdkg/config.json"]);
    git(&graph, &["commit", "-qm", "enable projections"]);
    ok(&graph, &["install", "--undo"]);
    assert_eq!(
        fs::read(graph.join(".mdkg/config.json")).expect("restored config"),
        before
    );
    assert!(!graph.join("AGENTS.md").exists());
    assert!(!graph.join(".mcp.json").exists());
}

fn directory_snapshot(root: &Path) -> std::collections::BTreeMap<PathBuf, Option<Vec<u8>>> {
    fn visit(
        root: &Path,
        relative: &Path,
        files: &mut std::collections::BTreeMap<PathBuf, Option<Vec<u8>>>,
    ) {
        let path = root.join(relative);
        if path.is_dir() {
            files.insert(relative.to_owned(), None);
            for entry in fs::read_dir(path).expect("snapshot directory") {
                visit(
                    root,
                    &relative.join(entry.expect("entry").file_name()),
                    files,
                );
            }
        } else {
            files.insert(
                relative.to_owned(),
                Some(fs::read(path).expect("snapshot file")),
            );
        }
    }
    let mut files = std::collections::BTreeMap::new();
    visit(root, Path::new(""), &mut files);
    files
}

#[test]
fn graph_install_previews_do_not_recreate_the_writer_lock() {
    let r = repo(true);
    ok(&r.0, &["install"]);
    let graph = r.0.join(".worktrees/graph");
    // Record an undoable change, then settle its tracked config before preview.
    ok(&graph, &["install", "--enable", "agents-md"]);
    git(&graph, &["add", ".mdkg/config.json"]);
    git(&graph, &["commit", "-qm", "settle installed projections"]);
    let lock = r.0.join(".git/mdkg/graph-worktree.lock");
    for args in [
        vec!["install", "--dry-run"],
        vec!["install", "--undo", "--dry-run"],
    ] {
        fs::remove_file(&lock).ok();
        let mdkg_before = directory_snapshot(&r.0.join(".git/mdkg"));
        let head_before = git(&graph, &["rev-parse", "HEAD"]);
        let status_before = git(
            &graph,
            &["status", "--porcelain=v1", "--untracked-files=all"],
        );
        let config_before = fs::read(r.0.join(".git/config")).expect("local config");
        ok(&graph, &args);
        assert!(!lock.exists(), "{args:?} created a writer lock");
        assert_eq!(
            directory_snapshot(&r.0.join(".git/mdkg")),
            mdkg_before,
            "{args:?} changed existing coordinator state"
        );
        assert_eq!(git(&graph, &["rev-parse", "HEAD"]), head_before);
        assert_eq!(
            git(
                &graph,
                &["status", "--porcelain=v1", "--untracked-files=all"]
            ),
            status_before
        );
        assert_eq!(
            fs::read(r.0.join(".git/config")).expect("local config"),
            config_before
        );
    }
}

#[test]
fn declared_config_parents_must_be_committed_directories() {
    for kind in ["symlink", "gitlink", "blob"] {
        let r = repo(true);
        declare_workspace(&r.0, "linked", "linked", ".mdkg");
        match kind {
            "symlink" => {
                std::os::unix::fs::symlink(".", r.0.join("linked")).expect("workspace link");
                git(&r.0, &["add", "linked"]);
            }
            "gitlink" => {
                let head = git(&r.0, &["rev-parse", "HEAD"]);
                git(
                    &r.0,
                    &[
                        "update-index",
                        "--add",
                        "--cacheinfo",
                        &format!("160000,{head},linked"),
                    ],
                );
            }
            _ => {
                fs::write(r.0.join("linked"), "not a directory").expect("blob parent");
                git(&r.0, &["add", "linked"]);
            }
        }
        git(&r.0, &["add", ".mdkg/config.json"]);
        git(&r.0, &["commit", "-qm", "declare non-directory workspace"]);
        let out = mdkg(&r.0, &["install"]);
        assert!(!out.status.success(), "{kind}: {out:?}");
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(
            text.contains("linked") && text.contains("directory"),
            "{kind}: {text}"
        );
    }
}

#[test]
fn invalid_projection_input_refuses_before_creating_the_graph_checkout() {
    let r = repo(true);
    let before = fs::read(r.0.join(".mdkg/config.json")).expect("config");
    let ignore_before = fs::read(r.0.join(".gitignore")).ok();
    let remote = r#"{"mcpServers":{"mdkg":{"type":"http","url":"https://example.invalid/mcp"}}}"#;
    fs::write(r.0.join(".mcp.json"), remote).expect("existing remote server");
    let out = mdkg(&r.0, &["install", "--enable", "mcp:claude"]);
    assert!(!out.status.success(), "{out:?}");
    assert!(!r.0.join(".worktrees/graph").exists());
    assert_eq!(fs::read(r.0.join(".gitignore")).ok(), ignore_before);
    assert_eq!(
        fs::read(r.0.join(".mdkg/config.json")).expect("config"),
        before
    );
    assert_eq!(
        fs::read_to_string(r.0.join(".mcp.json")).expect("user server"),
        remote
    );
}

#[test]
fn install_defers_an_unborn_main_without_creating_an_ignore_file() {
    let r = repo(false);
    fs::remove_file(r.0.join(".gitignore")).ok();
    let out = ok(&r.0, &["install"]);
    assert!(out.to_string().contains("graph worktree deferred"), "{out}");
    assert!(!r.0.join(".worktrees/graph").exists());
    assert!(!r.0.join(".gitignore").exists());
    git(&r.0, &["add", ".mdkg"]);
    git(&r.0, &["commit", "-qm", "graph"]);
    ok(&r.0, &["install"]);
    assert!(r.0.join(".worktrees/graph/.git").is_file());
}

#[test]
fn install_defers_until_the_initial_graph_is_committed_to_main() {
    let r = repo(false);
    fs::remove_file(r.0.join(".gitignore")).ok();
    fs::write(r.0.join("README.md"), "existing project").expect("file");
    git(&r.0, &["add", "README.md"]);
    git(&r.0, &["commit", "-qm", "project before graph"]);
    let out = ok(&r.0, &["install"]);
    assert!(out.to_string().contains("graph worktree deferred"), "{out}");
    assert!(!r.0.join(".worktrees/graph").exists());
    git(&r.0, &["add", ".mdkg"]);
    git(&r.0, &["commit", "-qm", "initial graph"]);
    ok(&r.0, &["install"]);
    assert!(r.0.join(".worktrees/graph/.mdkg/config.json").is_file());
}

#[test]
fn install_preserves_ignore_lines_and_fast_forwards_existing_graph_history() {
    let r = repo(true);
    fs::write(r.0.join(".gitignore"), "personal-cache/").expect("ignore");
    git(&r.0, &["branch", "mdkg-graph"]);
    let original = git(&r.0, &["rev-parse", "mdkg-graph"]);
    fs::write(r.0.join("new-main"), "main update").expect("new file");
    git(&r.0, &["add", "new-main"]);
    git(&r.0, &["commit", "-qm", "advance main"]);
    ok(&r.0, &["install"]);
    assert_eq!(
        git(&r.0.join(".worktrees/graph"), &["rev-parse", "HEAD"]),
        git(&r.0, &["rev-parse", "main"])
    );
    git(
        &r.0,
        &["merge-base", "--is-ancestor", &original, "mdkg-graph"],
    );
    assert_eq!(
        fs::read_to_string(r.0.join(".gitignore")).expect("ignore"),
        "personal-cache/\n/.worktrees/\n"
    );
}

#[test]
fn install_from_a_linked_checkout_creates_graph_in_the_primary_checkout() {
    let r = repo(true);
    let other = r.0.join("other");
    git(
        &r.0,
        &[
            "worktree",
            "add",
            "-qb",
            "other",
            other.to_str().expect("path"),
        ],
    );
    ok(&other, &["install"]);
    assert!(r.0.join(".worktrees/graph/.git").is_file());
    assert!(!other.join(".worktrees/graph").exists());
}

#[test]
fn unsafe_graph_paths_and_ignore_files_refuse_before_projection_writes() {
    use std::os::unix::fs::symlink;
    for kind in ["ignore-link", "ignore-hardlink", "folder-link", "occupied"] {
        let r = repo(true);
        fs::remove_file(r.0.join(".gitignore")).ok();
        let outside = r.0.join("unrelated");
        fs::write(&outside, "keep").expect("file");
        match kind {
            "ignore-link" => symlink(&outside, r.0.join(".gitignore")).expect("link"),
            "ignore-hardlink" => fs::hard_link(&outside, r.0.join(".gitignore")).expect("link"),
            "folder-link" => symlink(&r.0, r.0.join(".worktrees")).expect("link"),
            _ => fs::create_dir_all(r.0.join(".worktrees/graph")).expect("folder"),
        }
        let out = mdkg(&r.0, &["install", "--enable", "agents-md"]);
        assert!(!out.status.success(), "{kind}: {out:?}");
        assert!(!r.0.join("AGENTS.md").exists(), "{kind}");
        assert_eq!(fs::read_to_string(outside).expect("file"), "keep");
    }
}

fn sync(root: &Path, dry: bool) -> mdkg_run::install::worktree::GraphSync {
    mdkg_coord_sqlite::git::with_worktree_trust(&Default::default(), || {
        mdkg_run::install::worktree::sync_graph(root, "main", dry).expect("sync")
    })
}

fn advance_main(root: &Path) -> String {
    fs::write(root.join("new-main"), "main update").expect("file");
    git(root, &["add", "new-main"]);
    git(root, &["commit", "-qm", "advance main"]);
    git(root, &["rev-parse", "HEAD"])
}

#[test]
fn graph_sync_fast_forwards_only_and_dry_run_does_not_write() {
    let r = repo(true);
    ok(&r.0, &["install"]);
    let graph = r.0.join(".worktrees/graph");
    let before = git(&graph, &["rev-parse", "HEAD"]);
    let target = advance_main(&r.0);
    let preview = sync(&graph, true);
    assert!(!preview.updated);
    assert!(preview.reason.expect("plan").contains("would fast-forward"));
    assert_eq!(git(&graph, &["rev-parse", "HEAD"]), before);
    let updated = sync(&graph, false);
    assert!(updated.updated, "{updated:?}");
    assert_eq!(updated.head.as_deref(), Some(target.as_str()));
    assert!(graph.join("new-main").exists());
    let again = sync(&graph, false);
    assert!(!again.updated);
    assert!(again.reason.is_none());
    assert!(sync(&r.0, false).reason.is_none());
}

#[test]
fn dirty_and_diverged_graphs_are_reported_and_preserved() {
    let r = repo(true);
    ok(&r.0, &["install"]);
    let graph = r.0.join(".worktrees/graph");
    let before = git(&graph, &["rev-parse", "HEAD"]);
    advance_main(&r.0);
    fs::write(graph.join("local-work"), "keep").expect("file");
    let dirty = sync(&graph, false);
    assert_eq!(
        dirty.reason,
        Some(format!("graph not synced: dirty at {before}"))
    );
    assert_eq!(git(&graph, &["rev-parse", "HEAD"]), before);
    git(&graph, &["add", "local-work"]);
    git(&graph, &["commit", "-qm", "graph work"]);
    let before = git(&graph, &["rev-parse", "HEAD"]);
    let diverged = sync(&graph, false);
    assert_eq!(
        diverged.reason,
        Some(format!("graph not synced: diverged at {before}"))
    );
    assert_eq!(
        fs::read_to_string(graph.join("local-work")).expect("work"),
        "keep"
    );
    assert_eq!(git(&graph, &["rev-parse", "HEAD"]), before);
}

#[test]
fn fast_forward_does_not_overwrite_ignored_local_content() {
    let r = repo(true);
    fs::write(r.0.join(".gitignore"), "new-main\n").expect("ignore");
    git(&r.0, &["add", ".gitignore"]);
    git(&r.0, &["commit", "-qm", "ignore local content"]);
    ok(&r.0, &["install"]);
    let graph = r.0.join(".worktrees/graph");
    fs::write(graph.join("new-main"), "keep my ignored content").expect("file");
    fs::write(r.0.join("new-main"), "tracked on main").expect("file");
    git(&r.0, &["add", "-f", "new-main"]);
    git(&r.0, &["commit", "-qm", "track ignored file"]);
    let before = git(&graph, &["rev-parse", "HEAD"]);
    let status = sync(&graph, false);
    assert!(!status.updated, "{status:?}");
    assert!(
        status
            .reason
            .expect("reason")
            .contains("fast-forward unavailable")
    );
    assert_eq!(git(&graph, &["rev-parse", "HEAD"]), before);
    assert_eq!(
        fs::read_to_string(graph.join("new-main")).expect("content"),
        "keep my ignored content"
    );
}

#[test]
fn configured_main_branch_is_used_and_a_missing_main_is_explicit() {
    let r = repo(true);
    git(&r.0, &["branch", "-m", "trunk"]);
    let out = mdkg(&r.0, &["install", "--enable", "agents-md"]);
    assert!(!out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stdout).contains("main branch main does not exist"));
    assert!(!r.0.join("AGENTS.md").exists());
    git(&r.0, &["config", "mdkg.mainRef", "trunk"]);
    ok(&r.0, &["install"]);
    assert_eq!(
        git(&r.0.join(".worktrees/graph"), &["rev-parse", "HEAD"]),
        git(&r.0, &["rev-parse", "trunk"])
    );
}

#[test]
fn install_does_not_repurpose_another_branch_or_a_branch_in_another_worktree() {
    for other_place in [false, true] {
        let r = repo(true);
        let target = if other_place {
            r.0.join("other")
        } else {
            r.0.join(".worktrees/graph")
        };
        let branch = if other_place {
            "mdkg-graph"
        } else {
            "personal"
        };
        git(
            &r.0,
            &[
                "worktree",
                "add",
                "-qb",
                branch,
                target.to_str().expect("path"),
            ],
        );
        let before = git(&target, &["rev-parse", "HEAD"]);
        let out = mdkg(&r.0, &["install", "--enable", "agents-md"]);
        assert!(!out.status.success(), "{out:?}");
        assert!(!r.0.join("AGENTS.md").exists());
        assert_eq!(git(&target, &["symbolic-ref", "--short", "HEAD"]), branch);
        assert_eq!(git(&target, &["rev-parse", "HEAD"]), before);
    }
}

#[test]
fn graph_creation_and_fast_forward_do_not_run_checkout_or_merge_hooks() {
    use std::os::unix::fs::PermissionsExt as _;
    let r = repo(true);
    let sentinel = r.0.join("hook-ran");
    for hook in ["post-checkout", "post-merge"] {
        let file = r.0.join(".git/hooks").join(hook);
        fs::write(
            &file,
            format!("#!/bin/sh\ntouch '{}'\n", sentinel.display()),
        )
        .expect("hook");
        fs::set_permissions(file, fs::Permissions::from_mode(0o755)).expect("mode");
    }
    ok(&r.0, &["install"]);
    advance_main(&r.0);
    assert!(sync(&r.0.join(".worktrees/graph"), false).updated);
    assert!(!sentinel.exists());
}

#[test]
fn worker_worktree_creation_adds_the_ignore_rule_even_without_install() {
    let r = repo(true);
    let graph = r.0.join("launcher");
    git(
        &r.0,
        &[
            "worktree",
            "add",
            "-qb",
            "launcher",
            graph.to_str().expect("path"),
        ],
    );
    fs::remove_file(r.0.join(".gitignore")).ok();
    ok(
        &graph,
        &[
            "agents",
            "add",
            "worker",
            "--harness",
            "codex",
            "--role",
            "worker",
        ],
    );
    ok(&graph, &["agents", "up", "worker", "--once", "--dry-run"]);
    assert!(!r.0.join(".gitignore").exists());
    assert!(!r.0.join(".worktrees/worker").exists());
    // No mail: creates its checkout, then returns idle without a harness.
    ok(&graph, &["agents", "up", "worker", "--once"]);
    assert!(r.0.join(".worktrees/worker/.git").is_file());
    assert_eq!(
        fs::read_to_string(r.0.join(".gitignore")).expect("ignore"),
        "/.worktrees/\n"
    );
}

#[test]
fn install_refuses_a_frozen_graph_before_writing_any_projections() {
    for state in ["index.lock", "tracked edit", "wrong branch"] {
        let r = repo(true);
        ok(&r.0, &["install"]);
        let graph = r.0.join(".worktrees/graph");
        let original = fs::read(r.0.join(".mdkg/config.json")).expect("primary config");
        match state {
            "index.lock" => {
                let admin = git(&graph, &["rev-parse", "--absolute-git-dir"]);
                fs::write(
                    Path::new(&admin).join("index.lock"),
                    "interrupted Git write",
                )
                .expect("lock");
            }
            "tracked edit" => {
                fs::write(graph.join(".mdkg/config.json"), "partial config, not JSON")
                    .expect("partial write");
            }
            _ => {
                git(&graph, &["switch", "-qc", "other"]);
            }
        }
        let before = fs::read(graph.join(".mdkg/config.json")).expect("graph config");
        let out = mdkg(&r.0, &["install", "--enable", "agents-md"]);
        assert!(!out.status.success(), "{state}: {out:?}");
        let body = String::from_utf8_lossy(&out.stdout);
        assert!(
            body.contains("graph worktree needs repair"),
            "{state}: {out:?}"
        );
        assert!(body.contains("git -C") && body.contains("status"), "{body}");
        assert!(!r.0.join("AGENTS.md").exists());
        assert_eq!(
            fs::read(r.0.join(".mdkg/config.json")).expect("primary preserved"),
            original
        );
        assert_eq!(
            fs::read(graph.join(".mdkg/config.json")).expect("graph untouched"),
            before
        );
    }
}

fn identity_key(root: &Path) -> String {
    let config: Value =
        serde_json::from_slice(&fs::read(root.join(".mdkg/config.json")).expect("config"))
            .expect("JSON");
    format!(
        "mdkg.graph.{}.worktree",
        config["graph_id"].as_str().expect("id")
    )
}

fn declare_workspace(root: &Path, alias: &str, path: &str, mdkg_dir: &str) {
    let file = root.join(".mdkg/config.json");
    let mut config: Value =
        serde_json::from_slice(&fs::read(&file).expect("config")).expect("JSON");
    if config.get("workspaces").is_none() {
        config["workspaces"] = serde_json::json!({"root":{"path":"."}});
    }
    config["workspaces"][alias] = serde_json::json!({"path":path,"mdkg_dir":mdkg_dir});
    fs::write(file, serde_json::to_vec(&config).expect("JSON")).expect("declared workspace");
}

#[test]
fn identity_follows_a_git_worktree_move_on_reinstall_and_sync() {
    let r = repo(true);
    ok(&r.0, &["install"]);
    let original = r.0.join(".worktrees/graph");
    let moved = r.0.join("renamed-checkout");
    let config = fs::read(r.0.join(".git/config")).expect("config");
    git(
        &r.0,
        &[
            "worktree",
            "move",
            original.to_str().expect("path"),
            moved.to_str().expect("path"),
        ],
    );
    ok(&r.0, &["install", "--dry-run"]);
    ok(&r.0, &["install"]);
    assert!(!original.exists());
    assert_eq!(fs::read(r.0.join(".git/config")).expect("config"), config);
    let target = advance_main(&r.0);
    assert_eq!(sync(&moved, false).head.as_deref(), Some(target.as_str()));
    assert!(moved.join("new-main").is_file());
}

#[test]
fn install_records_each_declared_graph_id_and_refuses_a_conflicting_record() {
    let r = repo(true);
    let nested = r.0.join("packages/second");
    fs::create_dir_all(&nested).expect("nested graph");
    ok(&nested, &["init"]);
    declare_workspace(&r.0, "second", "packages/second", ".mdkg");
    git(&r.0, &["add", "packages/second/.mdkg", ".mdkg/config.json"]);
    git(&r.0, &["commit", "-qm", "second graph"]);
    ok(&r.0, &["install"]);
    let first = identity_key(&r.0);
    let second = identity_key(&nested);
    assert_ne!(first, second);
    for key in [&first, &second] {
        assert_eq!(git(&r.0, &["config", "--local", "--get-all", key]), "graph");
    }
    let worker = r.0.join("worker");
    git(
        &r.0,
        &[
            "worktree",
            "add",
            "-qb",
            "worker",
            worker.to_str().expect("path"),
        ],
    );
    git(&r.0, &["config", "--local", &second, "worker"]);
    let before = fs::read(r.0.join(".git/config")).expect("config");
    let out = mdkg(&r.0, &["install", "--enable", "agents-md"]);
    assert!(!out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stdout).contains("identity needs repair"));
    assert_eq!(fs::read(r.0.join(".git/config")).expect("config"), before);
    assert!(!r.0.join("AGENTS.md").exists());
    // Explicit operator repair of just the wrong record lets install refill it.
    git(&r.0, &["config", "--local", "--unset-all", &second]);
    ok(&r.0, &["install"]);
    assert_eq!(
        git(&r.0, &["config", "--local", "--get-all", &second]),
        "graph"
    );
}

#[test]
fn graph_identity_is_local_to_each_clone_and_uses_the_actual_admin_name() {
    let r = repo(true);
    ok(&r.0, &["install"]);
    let clone = r.0.join("engineer-clone");
    // Local throwaway clone: no network or live repository settings.
    git(
        &r.0,
        &[
            "clone",
            "--no-local",
            "--quiet",
            r.0.to_str().expect("path"),
            clone.to_str().expect("path"),
        ],
    );
    let key = identity_key(&r.0);
    assert_eq!(key, identity_key(&clone));
    assert!(
        !fs::read_to_string(clone.join(".git/config"))
            .expect("config")
            .contains(&key)
    );
    // Occupy admin name `graph` with an unrelated, subsequently moved checkout.
    let occupied = clone.join("temporary/graph");
    git(
        &clone,
        &[
            "worktree",
            "add",
            "-qb",
            "unrelated",
            occupied.to_str().expect("path"),
        ],
    );
    let moved = clone.join("unrelated");
    git(
        &clone,
        &[
            "worktree",
            "move",
            occupied.to_str().expect("path"),
            moved.to_str().expect("path"),
        ],
    );
    ok(&clone, &["install"]);
    assert_eq!(git(&r.0, &["config", "--local", "--get", &key]), "graph");
    assert_eq!(git(&clone, &["config", "--local", "--get", &key]), "graph1");
    ok(&clone, &["install"]);
}

#[test]
fn included_and_worktree_config_cannot_override_clone_local_identity() {
    let r = repo(true);
    ok(&r.0, &["install"]);
    let graph = r.0.join(".worktrees/graph");
    let key = identity_key(&r.0);
    let included = r.0.join(".git/included-config");
    fs::write(&included, "").expect("included file");
    git(
        &r.0,
        &[
            "config",
            "--file",
            included.to_str().expect("path"),
            &key,
            "wrong",
        ],
    );
    git(
        &r.0,
        &[
            "config",
            "--local",
            "include.path",
            included.to_str().expect("path"),
        ],
    );
    git(
        &r.0,
        &["config", "--local", "extensions.worktreeConfig", "true"],
    );
    git(&graph, &["config", "--worktree", &key, "also-wrong"]);
    let local = fs::read(r.0.join(".git/config")).expect("local config");
    ok(&r.0, &["install"]);
    let target = advance_main(&r.0);
    assert_eq!(sync(&graph, false).head.as_deref(), Some(target.as_str()));
    assert_eq!(fs::read(r.0.join(".git/config")).expect("config"), local);
}

#[test]
fn agents_up_start_syncs_before_loading_new_definitions() {
    let r = repo(true);
    ok(&r.0, &["install"]);
    let graph = r.0.join(".worktrees/graph");
    ok(
        &r.0,
        &[
            "agents",
            "add",
            "new-worker",
            "--harness",
            "codex",
            "--role",
            "worker",
        ],
    );
    // agents add commits the definition itself (bug-01a1807c).
    let target = git(&r.0, &["rev-parse", "HEAD"]);
    assert!(!graph.join(".mdkg/agents/new-worker.json").exists());
    let out = mdkg(&graph, &["agents", "up", "new-worker", "--once"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(git(&graph, &["rev-parse", "HEAD"]), target);
    assert!(graph.join(".mdkg/agents/new-worker.json").is_file());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("graph synced"),
        "{out:?}"
    );
}

#[test]
fn reinstall_syncs_existing_graph_and_dry_run_only_reports_it() {
    let r = repo(true);
    ok(&r.0, &["install"]);
    let graph = r.0.join(".worktrees/graph");
    let original = git(&graph, &["rev-parse", "HEAD"]);
    let target = advance_main(&r.0);
    let preview = ok(&r.0, &["install", "--dry-run"]);
    assert_eq!(git(&graph, &["rev-parse", "HEAD"]), original);
    assert!(
        preview.to_string().contains("would fast-forward"),
        "{preview}"
    );
    let result = ok(&r.0, &["install"]);
    assert_eq!(git(&graph, &["rev-parse", "HEAD"]), target);
    assert!(result.to_string().contains("graph synced"), "{result}");
}

#[test]
fn identity_review_ignores_all_undeclared_configs_before_reading_blobs() {
    let r = repo(true);
    for (path, text) in [
        ("conformance/base/.mdkg/config.json", "{}".to_owned()),
        (
            "conformance/identity-v2/.mdkg/config.json",
            r#"{"schema_version":2,"workspaces":[]}"#.to_owned(),
        ),
        (
            "examples/copied/.mdkg/config.json",
            fs::read_to_string(r.0.join(".mdkg/config.json")).expect("config"),
        ),
        ("examples/independent/.mdkg/config.json", serde_json::json!({"schema_version":3,"format_version":"0.7","graph_id":uuid::Uuid::now_v7().to_string()}).to_string()),
        ("examples/malformed/.mdkg/config.json", "{invalid".into()),
        ("examples/oversized/.mdkg/config.json", " ".repeat((1 << 20) + 1)),
    ] {
        let path = r.0.join(path);
        fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
        fs::write(path, text).expect("fixture config");
    }
    git(&r.0, &["add", "conformance", "examples"]);
    git(&r.0, &["commit", "-qm", "committed examples"]);
    ok(&r.0, &["install"]);
    assert_eq!(
        git(&r.0, &["config", "--local", "--get-regexp", "^mdkg.graph."])
            .lines()
            .count(),
        1
    );
    git(&r.0, &["rm", "-r", "examples"]);
    git(&r.0, &["commit", "-qm", "remove unrelated fixtures"]);
    ok(&r.0, &["install"]);
    assert_eq!(
        git(
            &r.0,
            &["config", "--local", "--get-all", &identity_key(&r.0)]
        ),
        "graph"
    );
    ok(&r.0, &["install"]);
}

#[test]
fn identity_review_refuses_linked_common_config_before_record_use_or_publication() {
    for symlink in [true, false] {
        let r = repo(true);
        ok(&r.0, &["install"]);
        let source = r.0.join("worker-controlled-config");
        let local = r.0.join(".git/config");
        fs::rename(&local, &source).expect("move config");
        if symlink {
            std::os::unix::fs::symlink(&source, &local).expect("symlink config");
        } else {
            fs::hard_link(&source, &local).expect("hardlink config");
        }
        let original = fs::read(&source).expect("source");
        for args in [vec!["install"], vec!["status"]] {
            let out = mdkg(&r.0, &args);
            assert!(!out.status.success(), "{symlink} {args:?}: {out:?}");
            assert!(
                String::from_utf8_lossy(&out.stdout).contains("local Git config"),
                "{out:?}"
            );
        }
        assert_eq!(fs::read(&source).expect("source preserved"), original);
    }
}

#[test]
fn identity_review_refuses_an_oversized_committed_config_blob() {
    let r = repo(true);
    let nested = r.0.join("nested/.mdkg");
    fs::create_dir_all(&nested).expect("nested");
    fs::write(nested.join("config.json"), " ".repeat((1 << 20) + 1)).expect("oversized blob");
    declare_workspace(&r.0, "nested", "nested", ".mdkg");
    git(&r.0, &["add", "nested", ".mdkg/config.json"]);
    git(&r.0, &["commit", "-qm", "oversized fixture"]);
    let out = mdkg(&r.0, &["install"]);
    assert!(!out.status.success(), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("1 MiB"),
        "{out:?}"
    );
}

#[test]
fn startup_dry_run_preserves_graph_and_reports_the_pending_update() {
    let r = repo(true);
    ok(
        &r.0,
        &[
            "agents",
            "add",
            "worker",
            "--harness",
            "codex",
            "--role",
            "worker",
        ],
    );
    // agents add commits the definition itself (bug-01a1807c).
    ok(&r.0, &["install"]);
    let graph = r.0.join(".worktrees/graph");
    let before = git(&graph, &["rev-parse", "HEAD"]);
    let local = fs::read(r.0.join(".git/config")).expect("config");
    advance_main(&r.0);
    let out = mdkg(&graph, &["agents", "up", "worker", "--once", "--dry-run"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(git(&graph, &["rev-parse", "HEAD"]), before);
    assert!(!graph.join("new-main").exists());
    assert_eq!(fs::read(r.0.join(".git/config")).expect("config"), local);
    assert!(String::from_utf8_lossy(&out.stderr).contains("would fast-forward"));
}

#[test]
fn startup_and_install_recheck_incoming_config_version_before_publication() {
    for install in [false, true] {
        let r = repo(true);
        ok(&r.0, &["install"]);
        let graph = r.0.join(".worktrees/graph");
        let path = r.0.join(".mdkg/config.json");
        let mut config: Value =
            serde_json::from_slice(&fs::read(&path).expect("config")).expect("JSON");
        config["schema_version"] = 4.into();
        fs::write(
            &path,
            serde_json::to_vec_pretty(&config).expect("serialize"),
        )
        .expect("config");
        git(&r.0, &["add", ".mdkg/config.json"]);
        git(&r.0, &["commit", "-qm", "unsupported incoming format"]);
        let args = if install {
            vec!["install", "--enable", "agents-md"]
        } else {
            vec!["agents", "up", "worker", "--once"]
        };
        let out = mdkg(&graph, &args);
        assert!(!out.status.success(), "{args:?}: {out:?}");
        assert!(!graph.join("AGENTS.md").exists());
        assert!(!r.0.join(".worktrees/worker").exists());
    }
}

#[test]
fn refresh_reports_freeze_and_graph_membership_changes_need_install() {
    let r = repo(true);
    ok(&r.0, &["install"]);
    let graph = r.0.join(".worktrees/graph");
    let nested = r.0.join("packages/second");
    fs::create_dir_all(&nested).expect("nested");
    ok(&nested, &["init"]);
    let key = identity_key(&nested);
    declare_workspace(&r.0, "second", "packages/second", ".mdkg");
    git(&r.0, &["add", "packages/second/.mdkg", ".mdkg/config.json"]);
    git(&r.0, &["commit", "-qm", "add graph"]);
    let startup = mdkg(&graph, &["agents", "up", "worker", "--once"]);
    assert!(!startup.status.success(), "{startup:?}");
    let failure: Value = serde_json::from_slice(&startup.stdout).expect("startup failure");
    let hint = failure["diagnostics"]
        .as_array()
        .expect("diagnostics")
        .iter()
        .filter_map(|d| d["hint"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        hint.contains("mdkg --root ")
            && hint.contains(r.0.to_str().expect("path"))
            && hint.contains(" install"),
        "{hint}"
    );
    assert!(!r.0.join(".worktrees/worker").exists());
    mdkg_coord_sqlite::git::with_worktree_trust(&Default::default(), || {
        let report = mdkg_run::install::worktree::refresh_graph(&graph, "main", false);
        assert!(report.note().expect("reported refusal").contains("missing"));
    });
    ok(&r.0, &["install"]);
    assert_eq!(git(&r.0, &["config", "--local", "--get", &key]), "graph");
    git(&r.0, &["rm", "-r", "packages/second/.mdkg"]);
    git(&r.0, &["commit", "-qm", "remove graph"]);
    mdkg_coord_sqlite::git::with_worktree_trust(&Default::default(), || {
        let report = mdkg_run::install::worktree::refresh_graph(&graph, "main", false);
        let note = report.note().expect("stale record guidance");
        assert!(
            note.contains(&format!("config --local --unset-all {key}"))
                && note.contains("mdkg --root "),
            "{note}"
        );
    });
    assert!(!mdkg(&r.0, &["install"]).status.success());
    git(&r.0, &["config", "--local", "--unset-all", &key]);
    ok(&r.0, &["install"]);
    let admin = git(&graph, &["rev-parse", "--absolute-git-dir"]);
    fs::write(Path::new(&admin).join("index.lock"), "busy").expect("lock");
    mdkg_coord_sqlite::git::with_worktree_trust(&Default::default(), || {
        let report = mdkg_run::install::worktree::refresh_graph(&graph, "main", false);
        let note = report.note().expect("freeze reported");
        assert!(
            note.contains("graph worktree needs repair") && note.contains("git -C"),
            "{note}"
        );
    });
}

#[test]
fn install_upgrades_a_pre_record_checkout_without_recreating_it() {
    let r = repo(true);
    let graph = r.0.join(".worktrees/graph");
    git(
        &r.0,
        &[
            "worktree",
            "add",
            "-qb",
            "mdkg-graph",
            graph.to_str().expect("path"),
        ],
    );
    let head = git(&graph, &["rev-parse", "HEAD"]);
    assert!(!mdkg(&graph, &["status"]).status.success());
    ok(&r.0, &["install"]);
    assert_eq!(git(&graph, &["rev-parse", "HEAD"]), head);
    assert_eq!(
        git(&r.0, &["config", "--local", "--get", &identity_key(&r.0)]),
        "graph"
    );
    ok(&graph, &["status"]);
}

#[test]
fn unrelated_byte_values_in_local_git_config_do_not_affect_identity() {
    let r = repo(true);
    ok(&r.0, &["install"]);
    let path = r.0.join(".git/config");
    let mut config = fs::read(&path).expect("config");
    config.extend_from_slice(b"\n[unrelated]\npath = /tmp/\xff\n");
    fs::write(&path, &config).expect("byte-valued setting");
    ok(&r.0, &["install"]);
    assert_eq!(fs::read(&path).expect("config"), config);
}

#[test]
fn identity_accepts_the_existing_integral_float_schema_version() {
    let r = repo(true);
    let path = r.0.join(".mdkg/config.json");
    let mut config: Value =
        serde_json::from_slice(&fs::read(&path).expect("config")).expect("JSON");
    config["schema_version"] = serde_json::json!(3.0);
    fs::write(&path, serde_json::to_vec(&config).expect("JSON")).expect("config");
    git(&r.0, &["add", ".mdkg/config.json"]);
    git(&r.0, &["commit", "-qm", "equivalent version number"]);
    ok(&r.0, &["install"]);
    ok(&r.0, &["install"]);
}

#[test]
fn identity_review_refuses_malformed_nested_configs() {
    for config in [
        "{invalid",
        "null",
        "[]",
        r#"{"schema_version":4}"#,
        r#"{"schema_version":3,"format_version":"0.7"}"#,
    ] {
        let r = repo(true);
        let nested = r.0.join("packages/second/.mdkg");
        fs::create_dir_all(&nested).expect("nested");
        fs::write(nested.join("config.json"), config).expect("malformed config");
        let root_config = r.0.join(".mdkg/config.json");
        let mut root: Value =
            serde_json::from_slice(&fs::read(&root_config).expect("root config")).expect("JSON");
        root["workspaces"]["second"] =
            serde_json::json!({"path":"packages/second","mdkg_dir":".mdkg","enabled":true});
        fs::write(&root_config, serde_json::to_vec(&root).expect("JSON")).expect("declared graph");
        git(&r.0, &["add", "packages", ".mdkg/config.json"]);
        git(&r.0, &["commit", "-qm", "invalid nested graph"]);
        let out = mdkg(&r.0, &["install"]);
        assert!(!out.status.success(), "{config}: {out:?}");
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("packages/second/.mdkg/config.json"),
            "{out:?}"
        );
    }
}

#[test]
fn identity_review_refuses_valueless_and_empty_records_at_parse_time() {
    for value in ["", " = \"\""] {
        let r = repo(true);
        ok(&r.0, &["install"]);
        let key = identity_key(&r.0);
        git(&r.0, &["config", "--local", "--unset-all", &key]);
        let id = key
            .strip_prefix("mdkg.graph.")
            .expect("prefix")
            .strip_suffix(".worktree")
            .expect("suffix");
        let path = r.0.join(".git/config");
        let mut bytes = fs::read(&path).expect("config");
        bytes.extend_from_slice(format!("\n[mdkg \"graph.{id}\"]\nworktree{value}\n").as_bytes());
        fs::write(&path, bytes).expect("record");
        let out = mdkg(&r.0, &["install"]);
        assert!(!out.status.success(), "{out:?}");
        assert!(
            String::from_utf8_lossy(&out.stdout)
                .contains("requires a non-empty worktree admin name"),
            "{out:?}"
        );
    }
}

#[test]
fn agents_up_starts_after_reporting_a_diverged_graph() {
    let r = repo(true);
    ok(
        &r.0,
        &[
            "agents",
            "add",
            "worker",
            "--harness",
            "codex",
            "--role",
            "worker",
        ],
    );
    // agents add commits the definition itself (bug-01a1807c).
    ok(&r.0, &["install"]);
    let graph = r.0.join(".worktrees/graph");
    fs::write(graph.join("graph-only"), "graph work").expect("graph work");
    git(&graph, &["add", "graph-only"]);
    git(&graph, &["commit", "-qm", "graph work"]);
    let head = git(&graph, &["rev-parse", "HEAD"]);
    advance_main(&r.0);
    let out = mdkg(&graph, &["agents", "up", "worker", "--once"]);
    assert!(out.status.success(), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("diverged"),
        "{out:?}"
    );
    assert_eq!(git(&graph, &["rev-parse", "HEAD"]), head);
    assert!(r.0.join(".worktrees/worker").is_dir());
}

#[test]
fn declared_custom_workspaces_and_local_subgraphs_share_the_checkout() {
    let r = repo(true);
    let child = r.0.join("packages/custom");
    let subgraph = r.0.join("nested/subgraph");
    let literal = r.0.join("packages\\literal");
    for path in [&child, &subgraph, &literal] {
        fs::create_dir_all(path).expect("child graph");
        ok(path, &["init"]);
    }
    let child_key = identity_key(&child);
    let subgraph_key = identity_key(&subgraph);
    let literal_key = identity_key(&literal);
    fs::rename(child.join(".mdkg"), child.join(".knowledge")).expect("custom document root");
    declare_workspace(&r.0, "custom", " ./packages/custom/.", ".knowledge");
    declare_workspace(&r.0, "literal", "packages\\literal", ".mdkg");
    declare_workspace(&r.0, "documents", "docs", ".mdkg");
    let path = r.0.join(".mdkg/config.json");
    let mut config: Value =
        serde_json::from_slice(&fs::read(&path).expect("config")).expect("JSON");
    config["subgraphs"] = serde_json::json!({
        "local":{"source_path":"nested/subgraph"},
        "alias":{}, "absent":{"source_path":"not-here"},
        "external":{"source_path":"../outside"}
    });
    fs::write(path, serde_json::to_vec(&config).expect("JSON")).expect("subgraphs");
    git(
        &r.0,
        &["add", ".mdkg", "packages", "nested", "packages\\literal"],
    );
    git(&r.0, &["commit", "-qm", "declared graph roots"]);
    ok(&r.0, &["install"]);
    for key in [identity_key(&r.0), child_key, subgraph_key, literal_key] {
        assert_eq!(git(&r.0, &["config", "--local", "--get", &key]), "graph");
    }
    assert_eq!(
        git(&r.0, &["config", "--local", "--get-regexp", "^mdkg.graph."])
            .lines()
            .count(),
        4
    );
    // Discovery reads committed declarations, not uncommitted working bytes.
    fs::write(r.0.join(".mdkg/config.json"), "damaged caller config").expect("working damage");
    assert!(sync(&r.0.join(".worktrees/graph"), false).reason.is_none());
}

#[test]
fn install_without_committed_identity_explains_migration_before_reinstall() {
    use std::os::unix::process::CommandExt as _;
    for migrated in [false, true] {
        let r = repo(true);
        let path = r.0.join(".mdkg/config.json");
        let valid_config = fs::read(&path).expect("config");
        let mut config: Value = serde_json::from_slice(&valid_config).expect("JSON");
        if migrated {
            config["graph_id"] = serde_json::json!("invalid-id");
        } else {
            let object = config.as_object_mut().expect("config object");
            object.remove("graph_id");
            object.remove("format_version");
            object.insert("schema_version".into(), serde_json::json!(1));
        }
        fs::write(&path, serde_json::to_vec(&config).expect("JSON")).expect("config");
        git(&r.0, &["add", ".mdkg/config.json"]);
        git(&r.0, &["commit", "-qm", "config without valid identity"]);
        // An existing checkout exercises committed identity validation even
        // during a preview, before install can publish anything.
        let graph = r.0.join(".worktrees/graph");
        git(
            &r.0,
            &[
                "worktree",
                "add",
                "-qb",
                "mdkg-graph",
                graph.to_str().expect("path"),
                "main",
            ],
        );
        if migrated {
            // The caller must pass the early config gate so install checks
            // the damaged committed identity in the existing graph checkout.
            fs::write(&path, valid_config).expect("restore primary identity");
            git(&r.0, &["add", ".mdkg/config.json"]);
            git(&r.0, &["commit", "-qm", "restore primary identity"]);
        }
        let local = fs::read(r.0.join(".git/config")).expect("local config");
        let head = git(&graph, &["rev-parse", "HEAD"]);
        let status = git(
            &graph,
            &["status", "--porcelain=v1", "--untracked-files=all"],
        );
        for binary in ["mdkg", "mdkg-next"] {
            let out = isolated(
                Command::new(env!("CARGO_BIN_EXE_mdkg"))
                    .arg0(binary)
                    .arg("--root")
                    .arg(&r.0)
                    .args(["install", "--dry-run", "--json"]),
            )
            .output()
            .expect("install preview");
            let body: Value = serde_json::from_slice(&out.stdout).expect("JSON");
            assert_eq!(out.status.code(), Some(1), "{body}");
            let diagnostic = &body["diagnostics"][0];
            assert_eq!(diagnostic["code"], "install.unsupported_layout", "{body}");
            assert_eq!(
                diagnostic["message"],
                "graph checkout identity needs repair: committed .mdkg/config.json has no valid 0.7 graph identity",
                "{body}"
            );
            assert_eq!(
                diagnostic["hint"],
                format!(
                    "If this graph is unmigrated, run {binary} migrate --dry-run from its primary \
                     checkout, review the plan, then {binary} migrate --apply <plan-hash>. If it \
                     was already migrated, restore its valid committed graph identity. Ensure \
                     any existing graph checkout contains that valid identity commit before \
                     rerunning {binary} install from the primary checkout. No checkout or \
                     identity record is reset automatically."
                ),
                "{body}"
            );
            assert_eq!(fs::read(r.0.join(".git/config")).expect("config"), local);
            assert_eq!(git(&graph, &["rev-parse", "HEAD"]), head);
            assert_eq!(
                git(
                    &graph,
                    &["status", "--porcelain=v1", "--untracked-files=all"]
                ),
                status
            );
        }
    }
}

#[test]
fn install_declared_child_without_committed_identity_requires_child_repair() {
    use std::os::unix::process::CommandExt as _;
    for workspace in [true, false] {
        let r = repo(true);
        let child = r.0.join("packages/child");
        fs::create_dir_all(&child).expect("child");
        ok(&child, &["init"]);
        let relative = if workspace {
            fs::rename(child.join(".mdkg"), child.join(".knowledge")).expect("custom graph root");
            declare_workspace(&r.0, "child", "packages/child", ".knowledge");
            "packages/child/.knowledge/config.json"
        } else {
            let path = r.0.join(".mdkg/config.json");
            let mut config: Value =
                serde_json::from_slice(&fs::read(&path).expect("config")).expect("JSON");
            config["subgraphs"] = serde_json::json!({"child":{"source_path":"packages/child"}});
            fs::write(path, serde_json::to_vec(&config).expect("JSON")).expect("subgraphs");
            "packages/child/.mdkg/config.json"
        };
        git(&r.0, &["add", ".mdkg", "packages"]);
        git(&r.0, &["commit", "-qm", "declare child graph"]);
        ok(&r.0, &["install"]);
        let graph = r.0.join(".worktrees/graph");
        // Keep the primary valid; only the existing graph checkout's committed
        // child identity needs repair. Root migration cannot repair that child.
        let path = graph.join(relative);
        let mut config: Value =
            serde_json::from_slice(&fs::read(&path).expect("child config")).expect("JSON");
        config
            .as_object_mut()
            .expect("config object")
            .remove("graph_id");
        fs::write(path, serde_json::to_vec(&config).expect("JSON")).expect("child config");
        git(&graph, &["add", relative]);
        git(&graph, &["commit", "-qm", "missing child identity"]);
        let local = fs::read(r.0.join(".git/config")).expect("local config");
        let head = git(&graph, &["rev-parse", "HEAD"]);
        let status = git(
            &graph,
            &["status", "--porcelain=v1", "--untracked-files=all"],
        );
        for binary in ["mdkg", "mdkg-next"] {
            let out = isolated(
                Command::new(env!("CARGO_BIN_EXE_mdkg"))
                    .arg0(binary)
                    .arg("--root")
                    .arg(&r.0)
                    .args(["install", "--dry-run", "--json"]),
            )
            .output()
            .expect("install preview");
            let body: Value = serde_json::from_slice(&out.stdout).expect("JSON");
            assert_eq!(out.status.code(), Some(1), "{body}");
            let diagnostic = &body["diagnostics"][0];
            assert_eq!(diagnostic["code"], "install.unsupported_layout", "{body}");
            assert_eq!(
                diagnostic["message"],
                format!(
                    "graph checkout identity needs repair: committed {relative} has no valid 0.7 graph identity"
                ),
                "{body}"
            );
            assert_eq!(
                diagnostic["hint"],
                format!(
                    "Restore the correct 0.7 graph identity in {relative} and commit any correction. \
                     Ensure any existing graph checkout contains that valid identity commit before \
                     rerunning {binary} install from the primary checkout. No checkout or \
                     identity record is reset automatically."
                ),
                "{body}"
            );
            assert_eq!(fs::read(r.0.join(".git/config")).expect("config"), local);
            assert_eq!(git(&graph, &["rev-parse", "HEAD"]), head);
            assert_eq!(
                git(
                    &graph,
                    &["status", "--porcelain=v1", "--untracked-files=all"]
                ),
                status
            );
        }
    }
}

#[test]
fn repair_commands_use_the_invoked_binary_and_quote_checkout_paths() {
    use std::os::unix::process::CommandExt as _;
    let mut r = repo(true);
    let quoted = r.0.with_file_name(format!(
        "{} with ' quote",
        r.0.file_name().expect("name").to_string_lossy()
    ));
    fs::rename(&r.0, &quoted).expect("quoted primary path");
    r.0 = quoted;
    let invoke = |root: &Path, args: &[&str]| {
        isolated(
            Command::new(env!("CARGO_BIN_EXE_mdkg"))
                .arg0("mdkg-next")
                .arg("--root")
                .arg(root)
                .args(args)
                .arg("--json"),
        )
        .output()
        .expect("renamed binary")
    };
    let installed = invoke(&r.0, &["install"]);
    assert!(installed.status.success(), "{installed:?}");
    assert!(String::from_utf8_lossy(&installed.stdout).contains("launch with mdkg-next --root"));
    let graph = r.0.join(".worktrees/graph");
    let child = r.0.join("child");
    fs::create_dir(&child).expect("child");
    ok(&child, &["init"]);
    declare_workspace(&r.0, "child", "child", ".mdkg");
    git(&r.0, &["add", ".mdkg", "child/.mdkg"]);
    git(&r.0, &["commit", "-qm", "add declared graph"]);
    let failure = invoke(&graph, &["agents", "up", "worker", "--once"]);
    assert!(!failure.status.success(), "{failure:?}");
    let body: Value = serde_json::from_slice(&failure.stdout).expect("JSON");
    let word = format!("'{}'", r.0.to_string_lossy().replace('\'', "'\\''"));
    assert_eq!(
        body["diagnostics"][0]["hint"],
        format!(
            "Register the committed graphs from the primary checkout: mdkg-next --root {word} install. Existing conflicting records are never overwritten."
        )
    );
    assert!(invoke(&r.0, &["install"]).status.success());
    let key = identity_key(&child);
    git(&r.0, &["rm", "-r", "child/.mdkg"]);
    git(&r.0, &["commit", "-qm", "remove declared graph"]);
    let failure = invoke(&graph, &["install"]);
    assert!(!failure.status.success(), "{failure:?}");
    let body: Value = serde_json::from_slice(&failure.stdout).expect("JSON");
    let graph_word = format!("'{}'", graph.to_string_lossy().replace('\'', "'\\''"));
    assert_eq!(
        body["diagnostics"][0]["hint"],
        format!(
            "After verifying this record is incorrect or its graph was removed, remove only this key: git -C {graph_word} config --local --unset-all {key}; then run mdkg-next --root {word} install. No checkout or record is reset automatically."
        )
    );
}
