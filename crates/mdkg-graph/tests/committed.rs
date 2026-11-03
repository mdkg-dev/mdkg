//! Object-reader parity, isolation and resource ceilings on throwaway repositories.
use std::path::{Path, PathBuf};
use std::process::Command;

use mdkg_format::{Code, Uuid7};
use mdkg_graph::{GraphError, Index, committed, config, index};

const GRAPH: &str = "0192f3a0-0000-7000-8000-000000000000";
const TASK: &str = "task-0192f3a4-0000-7000-8000-000000000002";
const GOAL: &str = "goal-0192f3a4-0000-7000-8000-000000000001";

struct Repo(PathBuf);
impl Drop for Repo {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}
impl Repo {
    fn new() -> Self {
        Self::new_format("sha1")
    }
    fn new_format(format: &str) -> Self {
        let repo = Self(std::env::temp_dir().join(format!("mdkg-committed-{}", Uuid7::now())));
        std::fs::create_dir_all(&repo.0).expect("fixture directory");
        repo.git(&["init", "-q", "--object-format", format]);
        repo.git(&["config", "user.name", "Snapshot test"]);
        repo.git(&["config", "user.email", "snapshot@example.invalid"]);
        repo.write(".mdkg/config.json", "{}");
        repo
    }
    fn git(&self, args: &[&str]) -> String {
        self.git_input(args, None)
    }
    fn git_input(&self, args: &[&str], input: Option<&[u8]>) -> String {
        use std::io::Write as _;
        use std::process::Stdio;
        let mut cmd = Command::new("git");
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("GIT_") {
                cmd.env_remove(key);
            }
        }
        let mut child = cmd
            .current_dir(&self.0)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("fixture git");
        if let Some(input) = input {
            child
                .stdin
                .take()
                .expect("stdin")
                .write_all(input)
                .expect("fixture input");
        } else {
            drop(child.stdin.take());
        }
        let output = child.wait_with_output().expect("fixture git output");
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("git UTF-8")
            .trim()
            .to_owned()
    }
    fn write(&self, path: &str, bytes: impl AsRef<[u8]>) {
        let path = self.0.join(path);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("parents");
        std::fs::write(path, bytes).expect("fixture write");
    }
    fn commit(&self) -> String {
        self.git(&["add", "."]);
        self.git(&["commit", "-qm", "Fixture"]);
        self.git(&["rev-parse", "HEAD"])
    }
    fn read(&self, oid: &str) -> Result<Index, GraphError> {
        read_at(&self.0, oid, "")
    }
    fn filesystem(&self, root: &str) -> Index {
        let root = self.0.join(root);
        let mut cfg = config::load(&root).expect("filesystem config");
        cfg.store_work_dir = None; // Compare committed sources only, including profile C.
        index::build(&root, &cfg).expect("filesystem index")
    }
}
fn read_at(repo: &Path, oid: &str, root: &str) -> Result<Index, GraphError> {
    mdkg_git::with_worktree_trust(&mdkg_git::WorktreeTrust::default(), || {
        committed::build_at(repo, oid, root)
    })
}
fn node(id: &str, kind: &str, title: &str, extra: &str) -> String {
    let stamp = if mdkg_format::NodeId::parse(id).is_ok() {
        "2026-10-24T18:03:11.482Z"
    } else {
        "2026-10-24"
    };
    let goal = if kind == "goal" {
        "goal_state: active\ngoal_condition: done\n"
    } else {
        ""
    };
    format!(
        "---\nid: {id}\ntype: {kind}\ntitle: {title}\nstatus: todo\n{goal}created: {stamp}\nupdated: {stamp}\n{extra}---\nBody.\n"
    )
}
fn v07() -> serde_json::Value {
    serde_json::json!({"schema_version":3,"format_version":"0.7","graph_id":GRAPH})
}
fn parity(actual: &Index, expected: &Index) {
    assert_eq!(actual.nodes, expected.nodes);
    assert_eq!(actual.reverse_edges, expected.reverse_edges);
    assert_eq!(actual.workspaces, expected.workspaces);
    assert_eq!(actual.graph_id, expected.graph_id);
    assert_eq!(actual.stale_refs, expected.stale_refs);
    assert_eq!(actual.private_work, expected.private_work);
    for node in &expected.nodes {
        assert_eq!(actual.get(&node.qid), expected.get(&node.qid));
        if let Ok(id) = mdkg_format::NodeId::parse(&node.id) {
            assert_eq!(actual.by_uuid(id.uuid()), expected.by_uuid(id.uuid()));
        }
    }
}

#[test]
fn root_parity_ignores_dirty_checkout_and_reads_from_subdirectory() {
    let r = Repo::new();
    r.write(".mdkg/work/task-1.md", node("task-1", "task", "One", ""));
    r.write(
        ".mdkg/work/task-2.md",
        node("task-2", "task", "Two", "relates: [task-1]\n"),
    );
    let oid = r.commit();
    let expected = r.filesystem("");
    parity(&r.read(&oid).expect("snapshot"), &expected);
    let before_index = std::fs::read(r.0.join(".git/index")).expect("git index");
    r.write(".mdkg/config.json", "not JSON");
    std::fs::remove_file(r.0.join(".mdkg/work/task-1.md")).expect("dirty delete");
    r.write(".mdkg/work/task-2.md", "dirty invalid node");
    let before_status = r.git(&["status", "--porcelain=v1"]);
    parity(&r.read(&oid).expect("immutable snapshot"), &expected);
    parity(
        &read_at(&r.0.join(".mdkg"), &oid, "").expect("subdirectory"),
        &expected,
    );
    assert_eq!(r.git(&["status", "--porcelain=v1"]), before_status);
    assert_eq!(
        std::fs::read(r.0.join(".git/index")).expect("git index"),
        before_index
    );
    assert_eq!(r.git(&["rev-parse", "HEAD"]), oid);
}

#[test]
fn custom_templates_workspace_order_tolerant_duplicates_and_skipped_core_match() {
    let r = Repo::new();
    r.write(".mdkg/config.json", serde_json::json!({
        "templates":{"root_path":"schemas","default_set":"team"},
        "workspaces":{"root":{"path":"."},"a":{"path":"packages/a","mdkg_dir":"graph"},"z":{"path":"packages/z","enabled":false}},
        "index":{"tolerant":true}
    }).to_string());
    r.write(
        "schemas/team/task.md",
        node("task-1", "task", "Template", "colour: blue\n"),
    );
    r.write(
        "packages/a/graph/work/a/x.md",
        node("task-1", "task", "Loses", "colour: red\n"),
    );
    r.write(
        "packages/a/graph/work/a-b.md",
        node("task-1", "task", "Wins", "colour: blue\n"),
    );
    r.write(
        "packages/z/.mdkg/work/ignored.md",
        "invalid disabled workspace",
    );
    r.write(".mdkg/work/bad.md", "invalid tolerant node");
    r.write(".mdkg/core/core.md", "not a node; counted but not parsed");
    r.write(
        ".mdkg/work/ok.md",
        node("task-2", "task", "Root", "colour: green\n"),
    );
    let oid = r.commit();
    let actual = r.read(&oid).expect("snapshot");
    parity(&actual, &r.filesystem(""));
    assert_eq!(actual.nodes.len(), 2);
    assert_eq!(actual.nodes[0].title, "Wins");
}

#[test]
fn format07_uuid_edges_fixed_schemas_and_invalid_graphs_match() {
    let r = Repo::new();
    r.write(".mdkg/config.json", v07().to_string());
    r.write(
        ".mdkg/templates/default/task.md",
        "invalid ignored 0.7 template",
    );
    r.write(".mdkg/work/goal.md", node(GOAL, "goal", "Goal", ""));
    r.write(
        ".mdkg/work/task.md",
        node(
            TASK,
            "task",
            "Task",
            &format!("parent: {GOAL}\nrelates: [bug-0192f3a4-0000-7000-8000-000000000001]\n"),
        ),
    );
    let oid = r.commit();
    parity(&r.read(&oid).expect("snapshot"), &r.filesystem(""));
    assert!(!r.read(&oid).expect("snapshot").stale_refs.is_empty());
    r.write(
        ".mdkg/work/duplicate.md",
        node(
            "bug-0192f3a4-0000-7000-8000-000000000002",
            "bug",
            "Duplicate",
            "",
        ),
    );
    let bad = r.commit();
    assert_eq!(
        r.read(&bad).expect_err("duplicate UUID").code(),
        Code::NodeDuplicateId
    );
    std::fs::remove_file(r.0.join(".mdkg/work/duplicate.md")).expect("remove duplicate");
    r.write(
        ".mdkg/work/task.md",
        node(
            TASK,
            "task",
            "Task",
            "relates: [task-0192f3a4-0000-7000-8000-000000000099]\n",
        ),
    );
    let bad = r.commit();
    assert_eq!(
        r.read(&bad).expect_err("missing edge").code(),
        Code::EdgeMissingTarget
    );
}

#[test]
fn nested_graph_roots_have_repository_relative_paths() {
    let r = Repo::new();
    r.write(".mdkg/work/root.md", node("task-1", "task", "Root", ""));
    r.write(
        "subproject/.mdkg/config.json",
        serde_json::json!({"workspaces":{"root":{"path":"."},"child":{"path":"nested"}}})
            .to_string(),
    );
    r.write(
        "subproject/.mdkg/work/a.md",
        node("task-2", "task", "Nested", ""),
    );
    r.write(
        "subproject/nested/.mdkg/work/b.md",
        node("task-3", "task", "Child", ""),
    );
    let oid = r.commit();
    let mut expected = r.filesystem("subproject");
    for node in &mut expected.nodes {
        node.path = format!("subproject/{}", node.path);
    }
    let actual = read_at(&r.0, &oid, "./subproject/").expect("nested graph");
    assert_eq!(actual.nodes, expected.nodes);
    assert_eq!(actual.workspaces, expected.workspaces);
    parity(
        &read_at(&r.0, &oid, ".").expect("root dot"),
        &r.read(&oid).expect("root empty"),
    );
    for path in ["../escape", "/tmp", "a/../b", "C:\\outside", "a\\b"] {
        assert!(read_at(&r.0, &oid, path).is_err(), "{path}");
    }
}

#[test]
fn local_work_is_absent_and_checkout_symlinks_do_not_supply_content() {
    let r = Repo::new();
    let mut cfg = v07();
    cfg["work"] = serde_json::json!({"location":"local"});
    r.write(".mdkg/config.json", cfg.to_string());
    r.write(".mdkg/design/goal.md", node(GOAL, "goal", "Goal", ""));
    let oid = r.commit();
    let expected = r.filesystem("");
    let cfg = config::load(&r.0).expect("local config");
    let store = cfg.store_work_dir.expect("local work directory");
    std::fs::create_dir_all(&store).expect("local work");
    std::fs::write(store.join("task.md"), node(TASK, "task", "Private", "")).expect("private task");
    let actual = r.read(&oid).expect("snapshot");
    parity(&actual, &expected);
    assert!(actual.private_work);
    assert_eq!(actual.nodes.len(), 1);
    std::fs::remove_file(r.0.join(".mdkg/config.json")).expect("remove checkout config");
    std::os::unix::fs::symlink("/does/not/exist", r.0.join(".mdkg/config.json"))
        .expect("checkout symlink");
    parity(&r.read(&oid).expect("snapshot"), &expected);
}

#[test]
fn configured_limits_count_shared_blobs_skipped_core_and_nonmarkdown_depth() {
    for (limits, files) in [
        (
            serde_json::json!({"max_files":1}),
            vec![".mdkg/work/a.md", ".mdkg/work/b.md"],
        ),
        (
            serde_json::json!({"max_files":1}),
            vec![".mdkg/core/core.md", ".mdkg/work/a.md"],
        ),
        (
            serde_json::json!({"max_file_bytes":1}),
            vec![".mdkg/work/a.md"],
        ),
        (
            serde_json::json!({"max_total_bytes":200}),
            vec![".mdkg/work/a.md", ".mdkg/work/b.md"],
        ),
        (
            serde_json::json!({"max_depth":0}),
            vec![".mdkg/work/deep/readme.txt"],
        ),
    ] {
        let r = Repo::new();
        r.write(
            ".mdkg/config.json",
            serde_json::json!({"index":{"limits":limits}}).to_string(),
        );
        for path in files {
            r.write(path, node("task-1", "task", "Same blob", ""));
        }
        let oid = r.commit();
        let cfg = config::load(&r.0).expect("config");
        assert_eq!(
            index::build(&r.0, &cfg)
                .expect_err("filesystem limit")
                .code(),
            Code::IndexLimitExceeded
        );
        assert_eq!(
            r.read(&oid).expect_err("object limit").code(),
            Code::IndexLimitExceeded
        );
    }
}

#[test]
fn only_full_commit_objects_are_accepted() {
    let r = Repo::new();
    r.write(".mdkg/work/task.md", node("task-1", "task", "One", ""));
    let oid = r.commit();
    r.git(&["tag", "-am", "Tag", "fixture-tag"]);
    for candidate in [
        "HEAD".to_owned(),
        oid[..12].to_owned(),
        oid.to_uppercase(),
        "f".repeat(40),
        r.git(&["rev-parse", "HEAD^{tree}"]),
        r.git(&["rev-parse", "HEAD:.mdkg/config.json"]),
        r.git(&["rev-parse", "fixture-tag"]),
    ] {
        assert!(r.read(&candidate).is_err(), "accepted {candidate}");
    }
    assert!(r.read(&oid).is_ok());
}

#[test]
fn committed_escapes_and_selected_symlinks_are_rejected_but_walk_links_are_skipped() {
    for cfg in [
        serde_json::json!({"workspaces":{"bad":{"path":"../escape"}}}),
        serde_json::json!({"templates":{"root_path":"../escape"}}),
        serde_json::json!({"templates":{"default_set":"../escape"}}),
    ] {
        let r = Repo::new();
        r.write(".mdkg/config.json", cfg.to_string());
        let oid = r.commit();
        assert!(r.read(&oid).is_err());
    }
    let r = Repo::new();
    r.write(".mdkg/work/task.md", node("task-1", "task", "One", ""));
    std::os::unix::fs::symlink("/outside", r.0.join(".mdkg/work/link.md")).expect("walk symlink");
    let oid = r.commit();
    parity(&r.read(&oid).expect("skip walk symlink"), &r.filesystem(""));
    std::os::unix::fs::symlink("/outside", r.0.join(".mdkg/design")).expect("selected symlink");
    let oid = r.commit();
    assert!(r.read(&oid).is_err());
}

#[test]
fn independent_blob_and_config_caps_override_untrusted_config() {
    let r = Repo::new();
    r.write(".mdkg/work/task.md", node("task-1", "task", "One", ""));
    let good = r.commit();
    r.write(
        ".mdkg/config.json",
        serde_json::json!({"index":{"limits":{"max_file_bytes":-1,"max_total_bytes":-1}}})
            .to_string(),
    );
    r.write(".mdkg/work/huge.md", vec![b' '; 16 * 1024 * 1024 + 1]);
    let huge = r.commit();
    assert_eq!(
        r.read(&huge).expect_err("hard blob cap").code(),
        Code::IndexLimitExceeded
    );
    r.write(
        ".mdkg/config.json",
        format!("{{}}{}", " ".repeat(1024 * 1024)),
    );
    let huge = r.commit();
    assert_eq!(
        r.read(&huge).expect_err("hard config cap").code(),
        Code::IndexLimitExceeded
    );
    assert!(r.read(&good).is_ok());
}

#[test]
fn invalid_utf8_node_body_is_lossy_and_v2_manifest_is_rejected() {
    let r = Repo::new();
    let mut bytes = node("task-1", "task", "One", "").into_bytes();
    bytes.push(0xff);
    r.write(".mdkg/work/task.md", bytes);
    let oid = r.commit();
    parity(&r.read(&oid).expect("lossy body"), &r.filesystem(""));
    r.write(".mdkg/graph.json", "{}");
    let oid = r.commit();
    assert_eq!(
        r.read(&oid).expect_err("v2").code(),
        Code::ConfigGraphV2Unsupported
    );
}

#[test]
fn replacement_refs_and_inherited_git_environment_cannot_redirect_snapshot() {
    let r = Repo::new();
    r.write(".mdkg/work/task.md", node("task-1", "task", "Original", ""));
    let original = r.commit();
    let expected = r.filesystem("");
    r.write(
        ".mdkg/work/task.md",
        node("task-1", "task", "Replacement", ""),
    );
    let replacement = r.commit();
    r.git(&["replace", &original, &replacement]);
    parity(
        &r.read(&original).expect("replace refs disabled"),
        &expected,
    );
    let hostile = Repo::new();
    hostile.write(".mdkg/work/task.md", node("task-9", "task", "Hostile", ""));
    hostile.commit();
    let output = Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "hostile_git_environment_child",
            "--test-threads=1",
        ])
        .env("MDKG_COMMITTED_TEST_REPO", &r.0)
        .env("MDKG_COMMITTED_TEST_OID", &original)
        .env("GIT_DIR", hostile.0.join(".git"))
        .env("GIT_WORK_TREE", &hostile.0)
        .env("GIT_OBJECT_DIRECTORY", hostile.0.join(".git/objects"))
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "core.repositoryformatversion")
        .env("GIT_CONFIG_VALUE_0", "999")
        .output()
        .expect("child test");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn hostile_git_environment_child() {
    let Some(repo) = std::env::var_os("MDKG_COMMITTED_TEST_REPO") else {
        return;
    };
    let oid = std::env::var("MDKG_COMMITTED_TEST_OID").expect("child OID");
    let snapshot = read_at(Path::new(&repo), &oid, "").expect("hardened reader");
    assert_eq!(snapshot.nodes.len(), 1);
    assert_eq!(snapshot.nodes[0].title, "Original");
}

#[test]
fn oversized_templates_are_rejected_before_parsing() {
    let r = Repo::new();
    r.write(
        ".mdkg/templates/default/task.md",
        vec![b' '; 16 * 1024 * 1024 + 1],
    );
    let oid = r.commit();
    assert_eq!(
        r.read(&oid).expect_err("hard template cap").code(),
        Code::IndexLimitExceeded
    );
}

#[test]
fn tree_metadata_is_capped_before_listing_and_gitlink_roots_are_rejected() {
    let r = Repo::new();
    let good = r.commit();
    let config_blob = r.git(&["rev-parse", "HEAD:.mdkg/config.json"]);
    let mut input = String::new();
    for i in 0..40_000 {
        input.push_str(&format!(
            "100644 blob {config_blob}\t{i:06}{}\n",
            "x".repeat(90)
        ));
    }
    let huge_tree = r.git_input(&["mktree"], Some(input.as_bytes()));
    let metadata =
        format!("100644 blob {config_blob}\tconfig.json\n040000 tree {huge_tree}\twork\n");
    let mdkg_tree = r.git_input(&["mktree"], Some(metadata.as_bytes()));
    let root_tree = r.git_input(
        &["mktree"],
        Some(format!("040000 tree {mdkg_tree}\t.mdkg\n").as_bytes()),
    );
    let oid = r.git(&["commit-tree", &root_tree, "-p", &good, "-m", "Large tree"]);
    assert_eq!(
        r.read(&oid).expect_err("tree hard cap").code(),
        Code::IndexLimitExceeded
    );
    assert!(r.read(&good).is_ok());
    r.git(&[
        "update-index",
        "--add",
        "--cacheinfo",
        &format!("160000,{good},.mdkg/work"),
    ]);
    let root_tree = r.git(&["write-tree"]);
    let oid = r.git(&["commit-tree", &root_tree, "-p", &good, "-m", "Gitlink root"]);
    assert!(r.read(&oid).is_err());
}

#[test]
fn sha256_and_snapshot_config_are_read_from_the_same_commit() {
    let r = Repo::new_format("sha256");
    r.write(
        ".mdkg/config.json",
        serde_json::json!({
            "workspaces":{"root":{"path":"."},"empty":{"path":"empty"}}
        })
        .to_string(),
    );
    r.write(".mdkg/work/task.md", node("task-1", "task", "One", ""));
    let oid = r.commit();
    assert_eq!(oid.len(), 64);
    let expected = r.filesystem("");
    let config = config::load(&r.0).expect("config");
    r.write(".mdkg/config.json", "invalid dirty config");
    let snapshot = mdkg_git::with_worktree_trust(&mdkg_git::WorktreeTrust::default(), || {
        committed::load_at(&r.0, &oid, "")
    })
    .expect("snapshot");
    parity(&snapshot.index, &expected);
    assert_eq!(snapshot.config, config);
    assert!(snapshot.index.workspaces.contains(&"empty".to_owned()));
}

#[test]
fn regular_files_at_discovery_roots_and_ancestors_are_absent_like_a_checkout() {
    for template_file in ["schemas", "schemas/team"] {
        let r = Repo::new();
        r.write(
            ".mdkg/config.json",
            serde_json::json!({
                "templates":{"root_path":"schemas","default_set":"team"},
                "workspaces":{
                    "root":{"path":"."},
                    "file_path":{"path":"packages/file"},
                    "file_mdkg":{"path":"packages/child","mdkg_dir":"graph"}
                }
            })
            .to_string(),
        );
        r.write(
            template_file,
            "ordinary file instead of a template directory",
        );
        r.write(
            "packages/file",
            "ordinary file instead of workspace directory",
        );
        r.write(
            "packages/child/graph",
            "ordinary file instead of metadata directory",
        );
        r.write(".mdkg/core", "ordinary file instead of document directory");
        r.write(
            ".mdkg/design",
            "ordinary file instead of document directory",
        );
        r.write(
            ".mdkg/work/task.md",
            node("task-1", "task", "Still indexed", ""),
        );
        let oid = r.commit();
        let expected = r.filesystem("");
        assert_eq!(expected.nodes.len(), 1);
        assert_eq!(expected.workspaces.len(), 3);
        parity(
            &r.read(&oid).expect("plain files are absent directories"),
            &expected,
        );
    }
}

#[test]
fn document_batch_process_count_does_not_grow_with_node_count() {
    use std::os::unix::fs::PermissionsExt as _;
    let r = Repo::new();
    r.write(".mdkg/work/task-1.md", node("task-1", "task", "One", ""));
    let small = r.commit();
    for i in 2..=2_000 {
        r.write(
            &format!(".mdkg/work/task-{i}.md"),
            node(&format!("task-{i}"), "task", &format!("Task {i}"), ""),
        );
    }
    let large = r.commit();
    let shim_dir = r.0.join(".git/count-shim");
    std::fs::create_dir(&shim_dir).expect("shim directory");
    let shim = shim_dir.join("git");
    std::fs::write(&shim, "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$MDKG_TEST_SPAWN_LOG\"\nexec \"$MDKG_TEST_REAL_GIT\" \"$@\"\n").expect("shim");
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755))
        .expect("shim executable");
    let path = std::env::join_paths(std::iter::once(shim_dir).chain(std::env::split_paths(
        &std::env::var_os("PATH").expect("PATH"),
    )))
    .expect("PATH join");
    let mut calls = Vec::new();
    for (oid, count) in [(&small, 1), (&large, 2_000)] {
        let log = r.0.join(format!(".git/calls-{count}"));
        let output = Command::new(std::env::current_exe().expect("test binary"))
            .args(["--exact", "batch_count_child", "--test-threads=1"])
            .env("PATH", &path)
            .env("MDKG_TEST_REAL_GIT", &mdkg_git::git_program().0)
            .env("MDKG_TEST_SPAWN_LOG", &log)
            .env("MDKG_COMMITTED_TEST_REPO", &r.0)
            .env("MDKG_COMMITTED_TEST_OID", oid)
            .env("MDKG_COMMITTED_TEST_COUNT", count.to_string())
            .output()
            .expect("child test");
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let log = std::fs::read_to_string(log).expect("spawn log");
        // Every object and tree read goes through one cat-file reader.
        // Each line is git's whole argument list, the global options first.
        let readers: Vec<_> = log
            .lines()
            .filter_map(|l| {
                [" cat-file ", " ls-tree "]
                    .iter()
                    .find_map(|verb| l.find(verb).map(|at| &l[at + 1..]))
            })
            .collect();
        assert_eq!(readers, ["cat-file --batch-command"], "{log}");
        calls.push(log.lines().count());
    }
    eprintln!("Git calls for 1 and 2,000 documents: {calls:?}");
    assert_eq!(
        calls[0], calls[1],
        "Git process count must depend on tree shape, not node count"
    );
}

#[test]
fn names_holding_a_newline_are_walked_by_object_id() {
    // One cat-file reader serves every read, one request per line: a
    // name with a newline must never reach it as a request.
    let r = Repo::new();
    r.write(".mdkg/work/task-1.md", node("task-1", "task", "One", ""));
    r.write(
        ".mdkg/work/line\nbreak/task-2.md",
        node("task-2", "task", "Two", ""),
    );
    r.write(".mdkg/work/notes\n.md", node("task-3", "task", "Three", ""));
    let oid = r.commit();
    let expected = r.filesystem("");
    assert_eq!(expected.nodes.len(), 3);
    parity(&r.read(&oid).expect("newline names"), &expected);
}

#[test]
fn old_style_tree_modes_read_like_git_reads_them() {
    // Old git wrote `040000` for a folder and modes like `100664` for a
    // file; git still reads them by their type bits.
    let r = Repo::new();
    r.write(".mdkg/work/task-1.md", node("task-1", "task", "One", ""));
    let good = r.commit();
    let expected = r.read(&good).expect("normal modes");
    let oid = |path: &str| {
        let hex = r.git(&["rev-parse", &format!("HEAD:{path}")]);
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect::<Vec<u8>>()
    };
    let raw = |entries: &[(&str, &str, Vec<u8>)]| {
        let mut tree = Vec::new();
        for (mode, name, oid) in entries {
            tree.extend_from_slice(format!("{mode} {name}\0").as_bytes());
            tree.extend_from_slice(oid);
        }
        let hex = r.git_input(
            &["hash-object", "-t", "tree", "-w", "--literally", "--stdin"],
            Some(&tree),
        );
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect::<Vec<u8>>()
    };
    let mdkg = raw(&[
        ("100664", "config.json", oid(".mdkg/config.json")),
        ("040000", "work", oid(".mdkg/work")),
    ]);
    let root = raw(&[("040000", ".mdkg", mdkg.clone())]);
    let root: String = root.iter().map(|b| format!("{b:02x}")).collect();
    let old = r.git(&["commit-tree", &root, "-p", &good, "-m", "Old modes"]);
    parity(&r.read(&old).expect("old-style modes"), &expected);
    // An unknown type, or a mode too large to be one, is refused, never
    // guessed as a gitlink as git's canon_mode would.
    for mode in ["170000", "77777777777"] {
        let root = raw(&[
            ("040000", ".mdkg", mdkg.clone()),
            (mode, "x", oid(".mdkg/config.json")),
        ]);
        let root: String = root.iter().map(|b| format!("{b:02x}")).collect();
        let odd = r.git(&["commit-tree", &root, "-p", &good, "-m", "Odd mode"]);
        let refused = r.read(&odd).expect_err(mode);
        assert!(
            format!("{refused:?}").contains("unsupported tree entry mode"),
            "{mode}: {refused:?}"
        );
    }
}

#[test]
fn batch_count_child() {
    let Some(count) = std::env::var_os("MDKG_COMMITTED_TEST_COUNT") else {
        return;
    };
    let repo = std::env::var_os("MDKG_COMMITTED_TEST_REPO").expect("child repo");
    let oid = std::env::var("MDKG_COMMITTED_TEST_OID").expect("child OID");
    let snapshot = read_at(Path::new(&repo), &oid, "").expect("reader");
    assert_eq!(
        snapshot.nodes.len(),
        count
            .to_str()
            .expect("count")
            .parse::<usize>()
            .expect("number")
    );
}

#[test]
fn strict_load_refuses_what_tolerant_mode_skips_and_another_graph_id() {
    let r = Repo::new();
    let mut config = v07();
    config["index"] = serde_json::json!({"tolerant": true});
    r.write(".mdkg/config.json", config.to_string());
    r.write(".mdkg/work/task-1.md", node(TASK, "task", "One", ""));
    let good = r.commit();
    let strict = |oid: &str, id: &str| {
        mdkg_git::with_worktree_trust(&mdkg_git::WorktreeTrust::default(), || {
            committed::load_strict(&r.0, oid, "", id)
        })
    };
    strict(&good, GRAPH).expect("a valid candidate");
    let other = "0192f3a0-0000-7000-8000-0000000000ff";
    let err = strict(&good, other).expect_err("another graph");
    assert!(err.to_string().contains("graph.identity_mismatch"), "{err}");
    r.write(".mdkg/work/bad.md", "not a node");
    let bad = r.commit();
    read_at(&r.0, &bad, "").expect("tolerant reader skips it");
    strict(&bad, GRAPH).expect_err("strict reader refuses it");
    std::fs::remove_file(r.0.join(".mdkg/work/bad.md")).expect("rm");
    std::os::unix::fs::symlink("task-1.md", r.0.join(".mdkg/work/link.md")).expect("link");
    let linked = r.commit();
    read_at(&r.0, &linked, "").expect("tolerant reader skips the link");
    strict(&linked, GRAPH).expect_err("strict reader refuses the link");
}
