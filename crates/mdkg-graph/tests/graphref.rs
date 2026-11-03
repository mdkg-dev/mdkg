//! graph.ref config, authority reads and the graph-owned path set
//! (edd-01a18867), on throwaway repositories.
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use mdkg_format::Uuid7;
use mdkg_graph::config;
use mdkg_graph::graphref::{self, design_paths, graph_paths};
use mdkg_graph::refwrite::{Change, Error, LockMode, RefWriter, Request};

const GRAPH_ID: &str = "0192f3a0-0000-7000-8000-000000000000";
const OTHER_ID: &str = "0192f3a0-0000-7000-8000-0000000000ff";
const GRAPH: &str = "refs/heads/mdkg-graph";

struct Repo(PathBuf);
impl Drop for Repo {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}
impl Repo {
    /// A code repository on `trunk` with one commit, and a graph on
    /// `mdkg-graph` that no checkout has.
    fn new() -> Self {
        let repo = Self(
            std::env::temp_dir()
                .join(format!("mdkg-graphref-{}", Uuid7::now()))
                .canonicalize_or_self(),
        );
        std::fs::create_dir_all(&repo.0).expect("fixture directory");
        repo.git(&["init", "-q", "-b", "trunk"]);
        repo.git(&["config", "user.name", "Graphref Test"]);
        repo.git(&["config", "user.email", "graphref@example.invalid"]);
        repo.git(&["config", "commit.gpgsign", "false"]);
        repo.write("source.txt", "code\n");
        repo.git(&["add", "."]);
        repo.git(&["commit", "-qm", "code"]);
        repo.git(&["checkout", "-q", "--orphan", "seed"]);
        repo.git(&["rm", "-rfq", "--cached", "."]);
        std::fs::remove_file(repo.0.join("source.txt")).expect("clear");
        repo.write(
            ".mdkg/config.json",
            &format!(
                r#"{{"schema_version":3,"format_version":"0.7","graph_id":"{GRAPH_ID}",
                "agents":{{"defaults":{{"model":"x"}},"inline":{{"role":"worker"}},"astra":{{"role":"old"}}}}}}"#
            ),
        );
        repo.write(
            ".mdkg/agents/astra.json",
            r#"{"role":"worker","harness":"codex"}"#,
        );
        repo.git(&["add", "."]);
        repo.git(&["commit", "-qm", "graph"]);
        repo.git(&["update-ref", GRAPH, "HEAD"]);
        repo.git(&["checkout", "-qf", "trunk"]);
        repo.git(&["branch", "-D", "-q", "seed"]);
        repo.git(&["clean", "-fdxq"]);
        repo.git(&["config", "mdkg.graph.ref", "mdkg-graph"]);
        repo.git(&[
            "config",
            &format!("mdkg.graph.{GRAPH_ID}.ref"),
            "mdkg-graph",
        ]);
        repo
    }
    fn git(&self, args: &[&str]) -> String {
        let out = git_command(&self.0).args(args).output().expect("git");
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).expect("utf8").trim().into()
    }
    fn write(&self, path: &str, text: &str) {
        let path = self.0.join(path);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, text).expect("write");
    }
}

trait Canon {
    fn canonicalize_or_self(self) -> PathBuf;
}
impl Canon for PathBuf {
    fn canonicalize_or_self(self) -> PathBuf {
        std::fs::create_dir_all(&self).ok();
        self.canonicalize().unwrap_or(self)
    }
}

fn graph_writer(repo: &Path) -> RefWriter {
    graphref::resolve(repo)
        .expect("resolve")
        .expect("ref mode")
        .writer(repo, LockMode::Local)
        .expect("writer")
}

fn git_command(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            cmd.env_remove(key);
        }
    }
    cmd.current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .args(["-c", "core.hooksPath=/dev/null"]);
    cmd
}

#[test]
fn checkout_mode_without_the_key_and_defaults_with_it() {
    let repo = Repo::new();
    repo.git(&["config", "--unset", "mdkg.graph.ref"]);
    assert_eq!(graphref::resolve(&repo.0).expect("resolve"), None);
    repo.git(&["config", "mdkg.graph.ref", "mdkg-graph"]);
    let graph = graphref::resolve(&repo.0)
        .expect("resolve")
        .expect("ref mode");
    assert_eq!(graph.name, GRAPH);
    assert_eq!(graph.main_ref, None);
    assert_eq!(graph.main_ref_base, None);
    assert_eq!(graph.remote, None);
    assert_eq!(graph.push_delay, Duration::from_secs(120));
    assert_eq!(graph.pull_interval, Duration::from_secs(900));
    assert!(!graph.publish_design);
    assert!(!graph.follow_fetch);
}

#[test]
fn keys_come_from_the_common_config_file_only() {
    let repo = Repo::new();
    repo.git(&["update-ref", "refs/remotes/origin/main", "trunk"]);
    repo.git(&["config", "mdkg.mainRef", "origin/main"]);
    repo.git(&["config", "mdkg.graph.remote", "mine"]);
    repo.git(&["config", "mdkg.graph.remoteRef", "graphs/repo"]);
    repo.git(&["config", "mdkg.graph.sync.pushDelay", "2"]);
    repo.git(&["config", "mdkg.graph.sync.pullInterval", "0"]);
    repo.git(&["config", "mdkg.graph.publishDesign", "on"]);
    repo.git(&["config", "mdkg.follow.fetch", "on"]);
    repo.git(&[
        "config",
        &format!("mdkg.graph.{GRAPH_ID}.ref"),
        "mdkg-graph",
    ]);
    // A worktree's own config file never redirects authority.
    repo.git(&["config", "extensions.worktreeConfig", "true"]);
    repo.git(&["config", "--worktree", "mdkg.graph.ref", "elsewhere"]);
    let graph = graphref::resolve(&repo.0)
        .expect("resolve")
        .expect("ref mode");
    assert_eq!(graph.name, GRAPH);
    assert_eq!(graph.main_ref.as_deref(), Some("refs/remotes/origin/main"));
    let remote = graph.remote.expect("remote");
    assert_eq!(remote.name, "mine");
    assert_eq!(remote.remote_ref, "refs/heads/graphs/repo");
    assert_eq!(remote.pinned, None);
    assert_eq!(graph.push_delay, Duration::from_secs(2));
    assert_eq!(graph.pull_interval, Duration::ZERO);
    assert!(graph.publish_design && graph.follow_fetch);
    assert_eq!(graph.graph_ids, vec![GRAPH_ID.to_owned()]);

    graphref::pin_destination(
        &graph.common,
        &graphref::Destination {
            fetch: vec!["example.invalid/me/graph".into()],
            push: vec!["example.invalid/me/graph".into()],
        },
    )
    .expect("pin");
    let pinned = graphref::resolve(&repo.0)
        .expect("resolve")
        .expect("ref mode");
    assert_eq!(
        pinned.remote.expect("remote").pinned.expect("pinned").push,
        vec!["example.invalid/me/graph".to_owned()]
    );

    repo.git(&["config", "mdkg.mainRef", "mdkg-graph"]);
    assert_eq!(
        graphref::resolve(&repo.0).expect_err("graph == main").code,
        "graph.ref_invalid"
    );
    repo.git(&["config", "mdkg.mainRef", "trunk"]);
    repo.git(&["config", "mdkg.graph.sync.pushDelay", "soon"]);
    assert_eq!(
        graphref::resolve(&repo.0).expect_err("bad delay").code,
        "graph.config_invalid"
    );
}

/// Run as a child with `$MDKG_MAIN_REF` set: only the base moves.
#[test]
fn main_ref_env_child() {
    let Some(root) = std::env::var_os("MDKG_GRAPHREF_CHILD") else {
        return;
    };
    let graph = graphref::resolve(Path::new(&root))
        .expect("resolve")
        .expect("ref mode");
    assert_eq!(graph.main_ref.as_deref(), Some("refs/heads/trunk"));
    assert_eq!(graph.main_ref_base.as_deref(), Some("refs/heads/agent/x"));
}

#[test]
fn main_ref_env_changes_only_the_base() {
    let repo = Repo::new();
    repo.git(&["config", "mdkg.mainRef", "trunk"]);
    let out = Command::new(std::env::current_exe().expect("test binary"))
        .args(["--exact", "main_ref_env_child", "--test-threads=1"])
        .env("MDKG_GRAPHREF_CHILD", &repo.0)
        .env("MDKG_MAIN_REF", "refs/heads/agent/x")
        .output()
        .expect("child");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("1 passed"));
}

#[test]
fn authority_reads_committed_objects_only() {
    let repo = Repo::new();
    // A checkout's files, committed elsewhere or not, change nothing.
    repo.write(".mdkg/agents/rogue.json", r#"{"role":"orchestrator"}"#);
    repo.write(".mdkg/config.json", r#"{"graph_id":"nope"}"#);
    let tip = repo.git(&["rev-parse", GRAPH]);
    let found = graphref::authority(&repo.0).expect("authority");
    assert_eq!(found.commit, tip);
    assert_eq!(found.graph_id, GRAPH_ID);
    let names: Vec<&str> = found.definitions.keys().map(String::as_str).collect();
    assert_eq!(names, ["astra", "inline"]);
    let astra = &found.definitions["astra"];
    assert!(!astra.inline);
    assert_eq!(astra.value["harness"], "codex");
    assert_eq!(
        astra.blob,
        repo.git(&["rev-parse", &format!("{GRAPH}:.mdkg/agents/astra.json")])
    );
    assert!(found.definitions["inline"].inline);
    let pinned = graphref::authority_at(&repo.0, &tip).expect("pinned");
    assert_eq!(
        (&pinned.commit, &pinned.config, &pinned.definitions),
        (&found.commit, &found.config, &found.definitions)
    );
    assert!(pinned.source.starts_with("commit:"));
}

#[test]
fn authority_refuses_links_and_oversized_definitions() {
    let repo = Repo::new();
    let tip = repo.git(&["rev-parse", GRAPH]);
    let link = repo.git(&["hash-object", "-w", "--stdin"]); // empty blob
    let tree = repo.git(&["rev-parse", &format!("{tip}:.mdkg/agents")]);
    let listing = repo.git(&["ls-tree", &tree]);
    let mktree = |extra: &str| {
        use std::io::Write as _;
        let mut child = git_command(&repo.0)
            .args(["mktree"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("mktree");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(format!("{listing}\n{extra}\n").as_bytes())
            .expect("input");
        let out = child.wait_with_output().expect("mktree");
        String::from_utf8(out.stdout)
            .expect("utf8")
            .trim()
            .to_owned()
    };
    let with_tree = |agents: &str| {
        let root = repo.git(&["rev-parse", &format!("{tip}^{{tree}}")]);
        let mdkg = repo.git(&["rev-parse", &format!("{root}:.mdkg")]);
        let config = repo.git(&["rev-parse", &format!("{mdkg}:config.json")]);
        let input = format!("040000 tree {agents}\tagents\n100644 blob {config}\tconfig.json\n");
        let mdkg_tree = {
            use std::io::Write as _;
            let mut child = git_command(&repo.0)
                .args(["mktree"])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .expect("mktree");
            child
                .stdin
                .take()
                .expect("stdin")
                .write_all(input.as_bytes())
                .expect("in");
            String::from_utf8(child.wait_with_output().expect("out").stdout)
                .expect("utf8")
                .trim()
                .to_owned()
        };
        let root_tree = {
            use std::io::Write as _;
            let mut child = git_command(&repo.0)
                .args(["mktree"])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .expect("mktree");
            child
                .stdin
                .take()
                .expect("stdin")
                .write_all(format!("040000 tree {mdkg_tree}\t.mdkg\n").as_bytes())
                .expect("in");
            String::from_utf8(child.wait_with_output().expect("out").stdout)
                .expect("utf8")
                .trim()
                .to_owned()
        };
        repo.git(&["commit-tree", &root_tree, "-p", &tip, "-m", "bad agents"])
    };
    let linked = with_tree(&mktree(&format!("120000 blob {link}\tlinked.json")));
    let err = graphref::authority_at(&repo.0, &linked).expect_err("symlink refused");
    assert_eq!(err.code, "graph.authority_invalid");
    let big = {
        use std::io::Write as _;
        let mut child = git_command(&repo.0)
            .args(["hash-object", "-w", "--stdin"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("hash");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(&vec![b' '; 1024 * 1024 + 1])
            .expect("in");
        String::from_utf8(child.wait_with_output().expect("out").stdout)
            .expect("utf8")
            .trim()
            .to_owned()
    };
    let oversized = with_tree(&mktree(&format!("100644 blob {big}\tbig.json")));
    let err = graphref::authority_at(&repo.0, &oversized).expect_err("oversized refused");
    assert_eq!(err.code, "graph.authority_invalid");
}

#[test]
fn another_graphs_ref_is_refused_by_identity() {
    let repo = Repo::new();
    graphref::authority(&repo.0).expect("own graph");
    let own = format!("mdkg.graph.{GRAPH_ID}.ref");
    repo.git(&["config", "--unset", &own]);
    // No record: refused, except for init and install's own check.
    assert_eq!(
        graphref::authority(&repo.0).expect_err("unrecorded").code,
        "graph.identity_unrecorded"
    );
    let graph = graphref::resolve(&repo.0)
        .expect("resolve")
        .expect("ref mode");
    graph
        .check_identity_or_unrecorded(GRAPH_ID, "")
        .expect("init and install may adopt");
    repo.git(&[
        "config",
        &format!("mdkg.graph.{OTHER_ID}.ref"),
        "mdkg-graph",
    ]);
    assert_eq!(
        graphref::authority(&repo.0).expect_err("other graph").code,
        "graph.identity_mismatch"
    );
    let graph = graphref::resolve(&repo.0)
        .expect("resolve")
        .expect("ref mode");
    assert_eq!(
        graph
            .check_identity_or_unrecorded(GRAPH_ID, "")
            .expect_err("a recorded other graph")
            .code,
        "graph.identity_mismatch"
    );
}

#[test]
fn a_missing_or_symbolic_graph_ref_is_refused() {
    let repo = Repo::new();
    repo.git(&["update-ref", "-d", GRAPH]);
    assert_eq!(
        graphref::authority(&repo.0).expect_err("missing").code,
        "graph.ref_missing"
    );
    repo.git(&["symbolic-ref", GRAPH, "refs/heads/trunk"]);
    assert_eq!(
        graphref::authority(&repo.0).expect_err("symbolic").code,
        "graph.ref_invalid"
    );
}

fn workspaces(value: serde_json::Value) -> config::Config {
    config::from_snapshot(serde_json::json!({
        "schema_version": 3, "format_version": "0.7", "graph_id": GRAPH_ID,
        "workspaces": value,
    }))
    .expect("config")
}

#[test]
fn graph_paths_refuse_shared_and_overlapping_folders() {
    let ok = workspaces(serde_json::json!({
        "root": {"path": ".", "mdkg_dir": ".mdkg"},
        "web": {"path": "apps/web", "mdkg_dir": ".mdkg"},
    }));
    assert_eq!(
        graph_paths(&ok).expect("paths"),
        [".mdkg", "apps/web/.mdkg"]
    );
    assert_eq!(
        design_paths(&ok).expect("design"),
        [".mdkg/design", "apps/web/.mdkg/design"]
    );
    let mut reached = 0;
    for bad in [
        serde_json::json!({"root": {"path": "."}, "src": {"path": "src", "mdkg_dir": "."}}),
        serde_json::json!({"root": {"path": "."}, "inner": {"path": ".mdkg", "mdkg_dir": "sub"}}),
    ] {
        let Ok(config) = config::from_snapshot(serde_json::json!({
            "schema_version": 3, "format_version": "0.7", "graph_id": GRAPH_ID,
            "workspaces": bad,
        })) else {
            continue; // refused by config validation already
        };
        reached += 1;
        assert_eq!(
            graph_paths(&config).expect_err("overlap").code,
            "graph.overlapping_workspace"
        );
    }
    assert!(reached > 0, "config validation refused every case first");
}

fn put(path: &str, bytes: &[u8]) -> Change {
    Change::Put {
        path: path.into(),
        bytes: bytes.into(),
    }
}

#[test]
fn a_missing_graph_ref_is_created_by_create_cas() {
    let repo = Repo::new();
    repo.git(&["update-ref", "-d", GRAPH]);
    repo.git(&["config", "--unset", &format!("mdkg.graph.{GRAPH_ID}.ref")]);
    repo.git(&["config", "mdkg.mainRef", "trunk"]);
    let graph = graphref::resolve(&repo.0)
        .expect("resolve")
        .expect("ref mode");
    let writer = graph
        .creator(&repo.0, LockMode::Local, GRAPH_ID, "")
        .expect("creator");
    let request = Request {
        id: "init-1".into(),
        base: None,
        changes: vec![put(
            ".mdkg/config.json",
            format!(r#"{{"schema_version":3,"format_version":"0.7","graph_id":"{GRAPH_ID}"}}"#)
                .as_bytes(),
        )],
        message: "init".into(),
        trailers: vec![("Mdkg-Actor".into(), "nick".into())],
    };
    let receipt = writer.write(&request).expect("create");
    graph.complete_creation(&repo.0).expect("records written");
    assert_eq!(
        repo.git(&["config", &format!("mdkg.graph.{GRAPH_ID}.ref")]),
        GRAPH
    );
    assert!(!receipt.already_applied);
    assert_eq!(repo.git(&["rev-parse", GRAPH]), receipt.commit);
    // An orphan, graph-only first commit: no parent, only .mdkg.
    assert_eq!(repo.git(&["rev-list", "--count", GRAPH]), "1");
    assert_eq!(repo.git(&["ls-tree", "--name-only", GRAPH]), ".mdkg");
    let message = repo.git(&["log", "-1", "--format=%B", GRAPH]);
    assert!(message.starts_with("mdkg: init [init-1 "), "{message}");
    assert!(message.ends_with("Mdkg-Actor: nick"), "{message}");
    // The pending ref is gone after the publish.
    assert!(writer.pending().expect("pending").is_empty());
    // A rerun returns the original receipt; another create can't publish.
    let again = writer.write(&request).expect("rerun");
    assert!(again.already_applied);
    assert_eq!(again.commit, receipt.commit);
    let mut other = request.clone();
    other.id = "init-2".into();
    assert!(matches!(writer.write(&other), Err(Error::Exists(_))));
    // trunk never moved.
    assert_eq!(repo.git(&["log", "-1", "--format=%s", "trunk"]), "code");
}

#[test]
fn writes_outside_the_graph_folders_are_refused() {
    let repo = Repo::new();
    let writer = graph_writer(&repo.0)
        .with_paths(&[".mdkg".into(), "apps/web/.mdkg".into()])
        .expect("paths");
    let base = writer.snapshot().expect("tip");
    for path in ["source.txt", "apps/web/src/x.md", ".mdkgx/a.md"] {
        let request = Request {
            id: "outside".into(),
            base: Some(base.clone()),
            changes: vec![put(path, b"x")],
            message: "outside".into(),
            trailers: vec![],
        };
        assert!(
            matches!(writer.write(&request), Err(Error::Invalid(_))),
            "{path}"
        );
    }
    let request = Request {
        id: "inside".into(),
        base: Some(base),
        changes: vec![put("apps/web/.mdkg/design/a.md", b"x")],
        message: "inside".into(),
        trailers: vec![],
    };
    writer.write(&request).expect("graph-owned path");
}

/// A child with an empty HOME: no identity anywhere.
#[test]
fn identity_child() {
    let Some(root) = std::env::var_os("MDKG_GRAPHREF_IDENTITY") else {
        return;
    };
    let writer = graph_writer(Path::new(&root));
    let request = Request {
        id: "no-identity".into(),
        base: Some(writer.snapshot().expect("tip")),
        changes: vec![put(".mdkg/design/a.md", b"x")],
        message: "x".into(),
        trailers: vec![],
    };
    let err = writer.write(&request).expect_err("no identity");
    assert_eq!(err.code(), "graph.identity_unset");
    // git's own advice is dropped: one line, with mdkg's fix.
    let message = err.to_string();
    assert!(!message.contains('\n'), "{message}");
    if let Some(hint) = std::env::var_os("MDKG_GRAPHREF_HINT") {
        let hint = hint.to_str().expect("utf-8 hint");
        assert!(message.contains(hint), "{message}");
    }
}

#[test]
fn the_users_identity_is_used_and_a_missing_one_refuses() {
    let repo = Repo::new();
    let tip = repo.git(&["rev-parse", GRAPH]);
    repo.git(&["config", "--unset", "user.name"]);
    repo.git(&["config", "--unset", "user.email"]);
    let home = repo.0.join("home");
    std::fs::create_dir_all(&home).expect("home");
    let run = |home: &Path| {
        Command::new(std::env::current_exe().expect("test binary"))
            .args(["--exact", "identity_child", "--test-threads=1"])
            .env("MDKG_GRAPHREF_IDENTITY", &repo.0)
            .env("HOME", home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env_remove("GIT_AUTHOR_NAME")
            .env_remove("GIT_AUTHOR_EMAIL")
            .env_remove("GIT_COMMITTER_NAME")
            .env_remove("GIT_COMMITTER_EMAIL")
            .env_remove("EMAIL")
            .env_remove("GIT_CONFIG_GLOBAL")
            .output()
            .expect("child")
    };
    let out = run(&home);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(repo.git(&["rev-parse", GRAPH]), tip);

    // A global identity (the professional case) is honoured.
    std::fs::write(
        home.join(".gitconfig"),
        "[user]\n\tname = Global Person\n\temail = global@example.invalid\n",
    )
    .expect("global config");
    let writer = graph_writer(&repo.0);
    let _ = writer;
    let out = Command::new(std::env::current_exe().expect("test binary"))
        .args(["--exact", "global_identity_child", "--test-threads=1"])
        .env("MDKG_GRAPHREF_IDENTITY", &repo.0)
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env_remove("GIT_CONFIG_GLOBAL")
        .output()
        .expect("child");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(
        repo.git(&["log", "-1", "--format=%an <%ae>", GRAPH]),
        "Global Person <global@example.invalid>"
    );
}

#[test]
fn global_identity_child() {
    let Some(root) = std::env::var_os("MDKG_GRAPHREF_IDENTITY") else {
        return;
    };
    let writer = graph_writer(Path::new(&root));
    let request = Request {
        id: "global-identity".into(),
        base: Some(writer.snapshot().expect("tip")),
        changes: vec![put(".mdkg/design/a.md", b"x")],
        message: "x".into(),
        trailers: vec![],
    };
    writer.write(&request).expect("global identity");
}

/// `GIT_CONFIG_GLOBAL` naming a file is the user's global config (Nix,
/// home-manager, CI), so its identity is used and the `--global` hint
/// writes where mdkg reads. A `/dev/null` there is ignored: `$HOME`'s
/// config applies, and with none the hint names that file (e2e B7).
#[test]
fn git_config_global_holds_the_identity_and_dev_null_is_ignored() {
    // A trust pin the child runs under, when one is set.
    let pin: std::cell::RefCell<Option<PathBuf>> = std::cell::RefCell::new(None);
    let child_in = |cwd: Option<&Path>,
                    repo: &Repo,
                    name: &str,
                    home: &Path,
                    global: &Path,
                    hint: Option<&str>| {
        let mut command = Command::new(std::env::current_exe().expect("test binary"));
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        command
            .args(["--exact", name, "--test-threads=1"])
            .env("MDKG_GRAPHREF_IDENTITY", &repo.0)
            .env("HOME", home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("GIT_CONFIG_GLOBAL", global)
            .env_remove("GIT_AUTHOR_NAME")
            .env_remove("GIT_AUTHOR_EMAIL")
            .env_remove("GIT_COMMITTER_NAME")
            .env_remove("GIT_COMMITTER_EMAIL")
            .env_remove("EMAIL")
            .env_remove("MDKG_GRAPHREF_HINT");
        if let Some(hint) = hint {
            command.env("MDKG_GRAPHREF_HINT", hint);
        }
        match pin.borrow().as_ref() {
            Some(top) => command.env("MDKG_TRUSTED_WORKTREE", top),
            None => command.env_remove("MDKG_TRUSTED_WORKTREE"),
        };
        let out = command.output().expect("child");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stdout)
        );
    };
    let child = |repo: &Repo, name: &str, home: &Path, global: &Path, hint: Option<&str>| {
        child_in(None, repo, name, home, global, hint)
    };
    let fresh = || {
        let repo = Repo::new();
        repo.git(&["config", "--unset", "user.name"]);
        repo.git(&["config", "--unset", "user.email"]);
        let home = repo.0.join("home");
        std::fs::create_dir_all(&home).expect("home");
        (repo, home)
    };
    let identity = "[user]\n\tname = Nix Person\n\temail = nix@example.invalid\n";

    // The file GIT_CONFIG_GLOBAL names holds the identity.
    let (repo, home) = fresh();
    let global = repo.0.join("nix-gitconfig");
    std::fs::write(&global, identity).expect("global file");
    child(&repo, "global_identity_child", &home, &global, None);
    assert_eq!(
        repo.git(&["log", "-1", "--format=%an <%ae>", GRAPH]),
        "Nix Person <nix@example.invalid>"
    );

    // It names a file with no identity yet: the hint's `--global` writes it.
    let (repo, home) = fresh();
    let missing = repo.0.join("empty-gitconfig");
    let hint = format!(
        "`git config --global user.email <you@example.com>` and `user.name` (it writes {})",
        missing.display()
    );
    child(&repo, "identity_child", &home, &missing, Some(&hint));

    // /dev/null is ignored: with no $HOME config the hint names that file.
    let (repo, home) = fresh();
    let dev_null = Path::new("/dev/null");
    child(
        &repo,
        "identity_child",
        &home,
        dev_null,
        Some("`git config --file ~/.gitconfig user.email"),
    );
    // ...and with one, $HOME's identity is used.
    std::fs::write(home.join(".gitconfig"), identity).expect("home config");
    child(&repo, "global_identity_child", &home, dev_null, None);
    assert_eq!(
        repo.git(&["log", "-1", "--format=%an <%ae>", GRAPH]),
        "Nix Person <nix@example.invalid>"
    );

    // A relative path is read as git reads it (reviews of 5fa1bd43 and
    // f58c2058): inside a worktree from its top, here Bob, not the
    // subfolder's Carol or $HOME's Alice; outside one from the folder.
    let carol = "[user]\n\tname = Carol\n\temail = carol@example.invalid\n";
    let relative = |repo: &Repo, home: &Path, cwd: &Path| {
        std::fs::write(
            home.join(".gitconfig"),
            "[user]\n\tname = Alice\n\temail = alice@example.invalid\n",
        )
        .expect("home config");
        child_in(
            Some(cwd),
            repo,
            "global_identity_child",
            home,
            Path::new("rel-gitconfig"),
            None,
        );
        repo.git(&["log", "-1", "--format=%an <%ae>", GRAPH])
    };
    let (repo, home) = fresh();
    std::fs::write(
        repo.0.join("rel-gitconfig"),
        "[user]\n\tname = Bob\n\temail = bob@example.invalid\n",
    )
    .expect("top's relative global");
    let sub = repo.0.join("sub");
    std::fs::create_dir_all(&sub).expect("sub");
    std::fs::write(sub.join("rel-gitconfig"), carol).expect("sub's");
    assert_eq!(relative(&repo, &home, &sub), "Bob <bob@example.invalid>");
    // git itself agrees, run in the subfolder.
    let native = Command::new("git")
        .args(["var", "GIT_AUTHOR_IDENT"])
        .current_dir(&sub)
        .env("HOME", &home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "rel-gitconfig")
        .env_remove("GIT_AUTHOR_NAME")
        .env_remove("GIT_AUTHOR_EMAIL")
        .env_remove("EMAIL")
        .output()
        .expect("git var");
    assert!(
        String::from_utf8_lossy(&native.stdout).starts_with("Bob <bob@example.invalid>"),
        "{native:?}"
    );
    // Run in another repository's subfolder while a pin names the one
    // mdkg writes: git reads the file from the folder's own worktree, so
    // that one's Dave, not the pinned one's Bob (review of a5a90190).
    let (repo, home) = fresh();
    std::fs::write(
        repo.0.join("rel-gitconfig"),
        "[user]\n\tname = Bob\n\temail = bob@example.invalid\n",
    )
    .expect("pinned's");
    let other = Repo::new();
    std::fs::write(
        other.0.join("rel-gitconfig"),
        "[user]\n\tname = Dave\n\temail = dave@example.invalid\n",
    )
    .expect("other's");
    let other_sub = other.0.join("sub");
    std::fs::create_dir_all(&other_sub).expect("sub");
    *pin.borrow_mut() = Some(std::fs::canonicalize(&repo.0).expect("canonical"));
    let found = relative(&repo, &home, &other_sub);
    *pin.borrow_mut() = None;
    assert_eq!(found, "Dave <dave@example.invalid>");

    // Outside any worktree: the folder's own file.
    let (repo, home) = fresh();
    let outside = std::env::temp_dir().join(format!(
        "mdkg-graphref-outside-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    ));
    std::fs::create_dir_all(&outside).expect("outside");
    std::fs::write(outside.join("rel-gitconfig"), carol).expect("outside's");
    // Only when the temp folder really is outside every worktree: git
    // itself reads it from an enclosing worktree's top otherwise (review
    // of a5a90190).
    let enclosed = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(&outside)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .is_ok_and(|o| o.status.success());
    // The child runs only there: inside an enclosing worktree with no
    // file at its top, it would find no identity at all (review of
    // f02a3c6a).
    if !enclosed {
        let found = relative(&repo, &home, &outside);
        assert_eq!(found, "Carol <carol@example.invalid>");
    }
    std::fs::remove_dir_all(&outside).ok();
}

#[test]
fn graphref_writer_binds_the_recorded_identity() {
    let repo = Repo::new();
    let graph = graphref::resolve(&repo.0)
        .expect("resolve")
        .expect("ref mode");
    let writer = graph.writer(&repo.0, LockMode::Local).expect("own graph");
    let request = Request {
        id: "bound".into(),
        base: Some(writer.snapshot().expect("tip")),
        changes: vec![put(".mdkg/design/b.md", b"b")],
        message: "bound".into(),
        trailers: vec![],
    };
    writer.write(&request).expect("write");
    // No record: refused for an existing graph.
    repo.git(&["config", "--unset", &format!("mdkg.graph.{GRAPH_ID}.ref")]);
    let graph = graphref::resolve(&repo.0)
        .expect("resolve")
        .expect("ref mode");
    assert_eq!(
        graph
            .writer(&repo.0, LockMode::Local)
            .expect_err("unrecorded")
            .code,
        "graph.identity_unrecorded"
    );
    // A record naming another ref isn't a record of this one.
    repo.git(&[
        "config",
        &format!("mdkg.graph.{GRAPH_ID}.ref"),
        "other-graph",
    ]);
    let graph = graphref::resolve(&repo.0)
        .expect("resolve")
        .expect("ref mode");
    assert!(graph.graph_ids.is_empty());
    // A key of another shape isn't a record either.
    repo.git(&[
        "config",
        &format!("mdkg.graph.{GRAPH_ID}.worktree"),
        "graph",
    ]);
    let graph = graphref::resolve(&repo.0)
        .expect("resolve")
        .expect("ref mode");
    assert!(graph.graph_ids.is_empty());
}

fn commit_graph(repo: &Repo, id: &str, changes: Vec<Change>) -> String {
    let graph = graphref::resolve(&repo.0)
        .expect("resolve")
        .expect("ref mode");
    let writer = graph.writer(&repo.0, LockMode::Local).expect("writer");
    let request = Request {
        id: id.into(),
        base: Some(writer.snapshot().expect("tip")),
        changes,
        message: id.into(),
        trailers: vec![],
    };
    writer.write(&request).expect("write").commit
}

#[test]
fn authority_refuses_duplicate_keys_and_a_v2_marker() {
    let repo = Repo::new();
    let found = graphref::authority(&repo.0).expect("authority");
    assert!(
        found.source.starts_with("graph:refs/heads/mdkg-graph@"),
        "{}",
        found.source
    );
    assert_eq!(found.graph_ref.as_deref(), Some(GRAPH));
    let dup = commit_graph(
        &repo,
        "dup",
        vec![put(
            ".mdkg/agents/dup.json",
            br#"{"role":"worker","role":"orchestrator"}"#,
        )],
    );
    let err = graphref::authority_at(&repo.0, &dup).expect_err("duplicate keys");
    assert_eq!(err.code, "graph.authority_invalid");
    // Strict admission refuses the same candidate.
    let err = mdkg_git::with_worktree_trust(&mdkg_git::WorktreeTrust::default(), || {
        mdkg_graph::committed::load_strict(&repo.0, &dup, "", GRAPH_ID)
    })
    .expect_err("strict refuses the agent file");
    assert!(err.to_string().contains("graph.authority_invalid"), "{err}");
    let v2 = commit_graph(
        &repo,
        "v2",
        vec![
            Change::Delete {
                path: ".mdkg/agents/dup.json".into(),
            },
            put(".mdkg/graph.json", b"{}"),
        ],
    );
    assert!(graphref::authority_at(&repo.0, &v2).is_err());
}

#[test]
fn checkout_mode_reads_the_checkout_holding_the_graph_or_refuses() {
    let repo = Repo::new();
    repo.git(&["config", "--unset", "mdkg.graph.ref"]);
    repo.git(&["config", "--unset", &format!("mdkg.graph.{GRAPH_ID}.ref")]);
    // No record: the invoking checkout's HEAD (trunk has no .mdkg).
    assert!(graphref::authority(&repo.0).is_err());
    repo.git(&["worktree", "add", "-q", "--detach", "view", GRAPH]);
    repo.git(&["config", &format!("mdkg.graph.{GRAPH_ID}.worktree"), "view"]);
    // A record of another graph naming a checkout that doesn't hold it
    // isn't a candidate.
    repo.git(&["config", &format!("mdkg.graph.{OTHER_ID}.worktree"), "view"]);
    let found = graphref::authority(&repo.0).expect("the checkout holding the graph");
    assert!(
        found.source.starts_with("checkout:worktrees/view/HEAD@"),
        "{}",
        found.source
    );
    // A second checkout holding the other graph at the same root: two
    // candidates, so it fails closed.
    repo.git(&["worktree", "add", "-q", "--detach", "view2", GRAPH]);
    let view2 = repo.0.join("view2");
    std::fs::write(
        view2.join(".mdkg/config.json"),
        format!(r#"{{"schema_version":3,"format_version":"0.7","graph_id":"{OTHER_ID}"}}"#),
    )
    .expect("config");
    let out = git_command(&view2)
        .args(["commit", "-qam", "other graph"])
        .output()
        .expect("commit");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    repo.git(&[
        "config",
        &format!("mdkg.graph.{OTHER_ID}.worktree"),
        "view2",
    ]);
    assert_eq!(
        graphref::authority(&repo.0)
            .expect_err("two checkouts hold a graph at the root")
            .code,
        "graph.identity_mismatch"
    );
}

/// A worktree on an unborn branch with nothing in it, built without `git
/// worktree add --orphan` (git 2.42), so older git runs these tests too
/// (Opus #7843).
fn orphan_worktree(repo: &Repo, name: &str) {
    repo.git(&["worktree", "add", "-q", "--detach", name, GRAPH]);
    let dir = repo.0.join(name);
    for args in [
        &["checkout", "-q", "--orphan", "orphan-branch"][..],
        &["rm", "-rfq", "."][..],
    ] {
        let out = git_command(&dir).args(args).output().expect("git");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// Records OTHER_ID as the graph at `sub` on the graph ref.
fn record_nested(repo: &Repo) {
    repo.git(&[
        "config",
        &format!("mdkg.graph.{OTHER_ID}.ref"),
        "mdkg-graph",
    ]);
    repo.git(&["config", &format!("mdkg.graph.{OTHER_ID}.root"), "sub"]);
}

#[test]
fn authority_reads_a_graph_under_a_subfolder() {
    let repo = Repo::new();
    // Install records a nested graph before writing it (orchestrator #6947).
    record_nested(&repo);
    let graph = graphref::resolve(&repo.0)
        .expect("resolve")
        .expect("ref mode");
    let writer = graph
        .writer(&repo.0, LockMode::Local)
        .expect("writer")
        .with_paths(&[".mdkg".into(), "sub/.mdkg".into()])
        .expect("paths");
    let request = Request {
        id: "nested".into(),
        base: Some(writer.snapshot().expect("tip")),
        changes: vec![
            put(
                "sub/.mdkg/config.json",
                format!(r#"{{"schema_version":3,"format_version":"0.7","graph_id":"{OTHER_ID}"}}"#)
                    .as_bytes(),
            ),
            put("sub/.mdkg/agents/nested.json", br#"{"role":"worker"}"#),
        ],
        message: "nested".into(),
        trailers: vec![],
    };
    let tip = writer.write(&request).expect("write").commit;
    let nested = graphref::authority_at_in(&repo.0, &tip, "sub").expect("nested");
    assert_eq!(nested.graph_id, OTHER_ID);
    assert_eq!(nested.definitions.keys().collect::<Vec<_>>(), ["nested"]);
    // The nested graph needs its own record.
    repo.git(&["config", "--unset", &format!("mdkg.graph.{OTHER_ID}.ref")]);
    repo.git(&["config", "--unset", &format!("mdkg.graph.{OTHER_ID}.root")]);
    assert_eq!(
        graphref::authority_in(&repo.0, "sub")
            .expect_err("unrecorded")
            .code,
        "graph.identity_mismatch"
    );
    repo.git(&[
        "config",
        &format!("mdkg.graph.{OTHER_ID}.ref"),
        "mdkg-graph",
    ]);
    // Recorded, but bound to the repository root: still refused at sub.
    assert_eq!(
        graphref::authority_in(&repo.0, "sub")
            .expect_err("recorded for another root")
            .code,
        "graph.identity_mismatch"
    );
    repo.git(&["config", &format!("mdkg.graph.{OTHER_ID}.root"), "sub"]);
    assert_eq!(
        graphref::authority_in(&repo.0, "sub")
            .expect("recorded")
            .graph_id,
        OTHER_ID
    );
}

#[test]
fn a_linked_common_config_is_refused() {
    let repo = Repo::new();
    let config = repo.0.join(".git/config");
    let moved = repo.0.join("real-config");
    std::fs::rename(&config, &moved).expect("move");
    std::os::unix::fs::symlink(&moved, &config).expect("link");
    let err = graphref::resolve(&repo.0).expect_err("linked config");
    assert!(
        err.code == "graph.config_invalid" || err.code == "graph.io",
        "{err}"
    );
}

#[test]
fn graph_paths_include_declared_subgraphs() {
    let config = config::from_snapshot(serde_json::json!({
        "schema_version": 3, "format_version": "0.7", "graph_id": GRAPH_ID,
        "subgraphs": {"lib": {"source_path": "libs/core"}, "far": {}},
    }))
    .expect("config");
    assert_eq!(
        graph_paths(&config).expect("paths"),
        [".mdkg", "libs/core/.mdkg"]
    );
}

#[test]
fn a_project_layer_comes_from_the_pinned_authority() {
    let repo = Repo::new();
    let found = graphref::authority(&repo.0).expect("authority");
    // The checkout holds no .mdkg at all.
    assert!(!repo.0.join(".mdkg").exists());
    let project = config::layers::Project::from_authority(
        &found,
        &repo.0,
        mdkg_format::settings::Environment::default(),
    )
    .expect("project");
    assert_eq!(project.project_input().value, found.config);
    assert_eq!(project.graph_source(), Some(found.source.as_str()));
    assert_eq!(
        project.project_input().origin.path,
        Some(std::path::PathBuf::from(&found.source))
    );
    // No file backs the layer: a write to "the project file" fails rather
    // than creating one named after the label.
    assert!(std::fs::write(&project.paths().project, b"{}").is_err());
    assert!(!repo.0.join(&found.source).exists());
    // A scoped project replacement names the authority's origin.
    let origin = mdkg_format::settings::Origin::file(
        mdkg_format::settings::Layer::Project,
        std::path::PathBuf::from(&found.source),
    );
    let mut value = found.config.clone();
    value["index"] = serde_json::json!({"tolerant": true});
    let replacement = mdkg_format::settings::Input::new(origin, value).expect("input");
    project
        .resolve_replacing(&replacement, &[], None)
        .expect("replacement named by the graph origin");
    let elsewhere = mdkg_format::settings::Input::new(
        mdkg_format::settings::Origin::file(
            mdkg_format::settings::Layer::Project,
            repo.0.join(".mdkg/config.json"),
        ),
        found.config.clone(),
    )
    .expect("input");
    assert!(project.resolve_replacing(&elsewhere, &[], None).is_err());
    let resolved = project.resolve(&[], None).expect("resolve");
    assert_eq!(resolved.effective.values["graph_id"], GRAPH_ID);
}

#[test]
fn checkout_mode_reports_the_real_cause_and_reads_unregistered_nested_graphs() {
    let repo = Repo::new();
    repo.git(&["config", "--unset", "mdkg.graph.ref"]);
    repo.git(&["config", "--unset", &format!("mdkg.graph.{GRAPH_ID}.ref")]);
    repo.git(&["worktree", "add", "-q", "--detach", "view", GRAPH]);
    repo.git(&["config", &format!("mdkg.graph.{GRAPH_ID}.worktree"), "view"]);
    let view = repo.0.join("view");
    let commit = |message: &str| {
        let out = git_command(&view).args(["add", "."]).output().expect("add");
        assert!(out.status.success());
        let out = git_command(&view)
            .args(["commit", "-qm", message])
            .output()
            .expect("commit");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    // A nested graph with no record of its own: the one registered
    // checkout holds it.
    std::fs::create_dir_all(view.join("sub/.mdkg")).expect("mkdir");
    std::fs::write(
        view.join("sub/.mdkg/config.json"),
        format!(r#"{{"schema_version":3,"format_version":"0.7","graph_id":"{OTHER_ID}"}}"#),
    )
    .expect("nested config");
    commit("nested graph");
    // No record of its own: refused, never guessed (no fallback).
    assert_eq!(
        graphref::authority_in(&repo.0, "sub")
            .expect_err("unregistered nested graph")
            .code,
        "graph.ref_missing"
    );
    repo.git(&["config", &format!("mdkg.graph.{OTHER_ID}.worktree"), "view"]);
    repo.git(&["config", &format!("mdkg.graph.{OTHER_ID}.root"), "sub"]);
    assert_eq!(
        graphref::authority_in(&repo.0, "sub")
            .expect("nested")
            .graph_id,
        OTHER_ID
    );
    // A broken committed definition is reported as itself, not as a
    // missing checkout.
    std::fs::write(view.join(".mdkg/agents/broken.json"), b"{not json").expect("broken");
    commit("broken definition");
    assert_eq!(
        graphref::authority(&repo.0).expect_err("broken").code,
        "graph.authority_invalid"
    );
    // A registered checkout that's gone is named.
    repo.git(&["worktree", "remove", "--force", "view"]);
    let err = graphref::authority(&repo.0).expect_err("gone");
    assert_eq!(err.code, "graph.ref_missing");
    assert!(err.message.contains("view"), "{}", err.message);
}

#[test]
fn checkout_mode_refuses_a_replaced_id_and_a_deleted_checkout() {
    let repo = Repo::new();
    repo.git(&["config", "--unset", "mdkg.graph.ref"]);
    repo.git(&["config", "--unset", &format!("mdkg.graph.{GRAPH_ID}.ref")]);
    repo.git(&["worktree", "add", "-q", "--detach", "view", GRAPH]);
    repo.git(&["config", &format!("mdkg.graph.{GRAPH_ID}.worktree"), "view"]);
    graphref::authority(&repo.0).expect("registered");
    // The registered checkout now holds another graph: refused.
    let view = repo.0.join("view");
    std::fs::write(
        view.join(".mdkg/config.json"),
        format!(r#"{{"schema_version":3,"format_version":"0.7","graph_id":"{OTHER_ID}"}}"#),
    )
    .expect("config");
    let out = git_command(&view)
        .args(["commit", "-qam", "another graph"])
        .output()
        .expect("commit");
    assert!(out.status.success());
    assert_eq!(
        graphref::authority(&repo.0).expect_err("replaced id").code,
        "graph.ref_missing"
    );
    // A checkout folder deleted without pruning its admin record.
    std::fs::remove_dir_all(&view).expect("rm view");
    let err = graphref::authority(&repo.0).expect_err("deleted");
    assert_eq!(err.code, "graph.ref_missing");
    assert!(err.message.contains("view"), "{}", err.message);
}

#[test]
fn the_common_config_is_read_without_includes_or_extra_links() {
    let repo = Repo::new();
    let included = repo.0.join("included.config");
    std::fs::write(
        &included,
        "[mdkg \"graph\"]\n\tref = refs/heads/elsewhere\n",
    )
    .expect("inc");
    repo.git(&["config", "include.path", included.to_str().expect("utf8")]);
    let graph = graphref::resolve(&repo.0)
        .expect("resolve")
        .expect("ref mode");
    assert_eq!(
        graph.name, GRAPH,
        "an included file can't redirect authority"
    );
    std::fs::hard_link(repo.0.join(".git/config"), repo.0.join("alias.config")).expect("link");
    assert_eq!(
        graphref::resolve(&repo.0).expect_err("two links").code,
        "graph.config_invalid"
    );
}

#[test]
fn strict_admission_checks_a_nested_graphs_definitions() {
    let repo = Repo::new();
    // Install records a nested graph before writing it (orchestrator #6947).
    record_nested(&repo);
    let graph = graphref::resolve(&repo.0)
        .expect("resolve")
        .expect("ref mode");
    let writer = graph
        .writer(&repo.0, LockMode::Local)
        .expect("writer")
        .with_paths(&[".mdkg".into(), "sub/.mdkg".into()])
        .expect("paths");
    let request = Request {
        id: "nested-dup".into(),
        base: Some(writer.snapshot().expect("tip")),
        changes: vec![
            put(
                "sub/.mdkg/config.json",
                format!(r#"{{"schema_version":3,"format_version":"0.7","graph_id":"{OTHER_ID}"}}"#)
                    .as_bytes(),
            ),
            put("sub/.mdkg/agents/dup.json", br#"{"role":"a","role":"b"}"#),
        ],
        message: "nested".into(),
        trailers: vec![],
    };
    let tip = writer.write(&request).expect("write").commit;
    let err = mdkg_git::with_worktree_trust(&mdkg_git::WorktreeTrust::default(), || {
        mdkg_graph::committed::load_strict(&repo.0, &tip, "sub", OTHER_ID)
    })
    .expect_err("nested duplicate keys");
    assert!(err.to_string().contains("graph.authority_invalid"), "{err}");
}

#[test]
fn a_writer_rechecks_the_clone_config_at_every_write() {
    let repo = Repo::new();
    let graph = graphref::resolve(&repo.0)
        .expect("resolve")
        .expect("ref mode");
    assert_eq!(
        graph
            .creator(&repo.0, LockMode::Local, OTHER_ID, "")
            .expect_err("exists")
            .code,
        "graph.ref_exists"
    );
    let writer = graph_writer(&repo.0);
    let request = |id: &str| Request {
        id: id.into(),
        base: Some(writer.snapshot().expect("tip")),
        changes: vec![put(&format!(".mdkg/design/{id}.md"), b"x")],
        message: id.into(),
        trailers: vec![],
    };
    writer.write(&request("first")).expect("write");
    // The main ref changes under a long-lived writer.
    repo.git(&["config", "mdkg.mainRef", "trunk"]);
    assert!(writer.write(&request("second")).is_err());
    repo.git(&["config", "--unset", "mdkg.mainRef"]);
    // The identity record goes.
    repo.git(&["config", "--unset", &format!("mdkg.graph.{GRAPH_ID}.ref")]);
    assert!(writer.write(&request("third")).is_err());
    repo.git(&[
        "config",
        &format!("mdkg.graph.{GRAPH_ID}.ref"),
        "mdkg-graph",
    ]);
    writer.write(&request("fourth")).expect("restored");
}

#[test]
fn only_an_unregistered_checkout_with_no_graph_is_authority_absent() {
    use graphref::{Absent, Lookup};
    // Unborn, in checkout mode.
    let dir = std::env::temp_dir()
        .join(format!("mdkg-graphref-unborn-{}", Uuid7::now()))
        .canonicalize_or_self();
    let unborn = Repo(dir);
    unborn.git(&["init", "-q", "-b", "trunk"]);
    assert_eq!(
        graphref::lookup_in(&unborn.0, "").expect("lookup"),
        Lookup::Absent(Absent::Unborn)
    );
    // A code commit with no .mdkg, in checkout mode.
    let repo = Repo::new();
    repo.git(&["config", "--unset", "mdkg.graph.ref"]);
    assert_eq!(
        graphref::lookup_in(&repo.0, "").expect("lookup"),
        Lookup::Absent(Absent::NoGraphConfig)
    );
    // A broken committed config is an error, never absent.
    repo.write(".mdkg/config.json", "{not json");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-qm", "broken graph"]);
    assert!(graphref::lookup_in(&repo.0, "").is_err());
    // Ref mode is never absent: a missing graph ref is an error.
    let ref_mode = Repo::new();
    assert!(matches!(
        graphref::lookup_in(&ref_mode.0, "").expect("found"),
        Lookup::Found(_)
    ));
    ref_mode.git(&["update-ref", "-d", GRAPH]);
    assert_eq!(
        graphref::lookup_in(&ref_mode.0, "")
            .expect_err("missing")
            .code,
        "graph.ref_missing"
    );
    // A registered checkout that's gone is an error, never absent.
    let registered = Repo::new();
    registered.git(&["config", "--unset", "mdkg.graph.ref"]);
    registered.git(&["config", &format!("mdkg.graph.{GRAPH_ID}.worktree"), "gone"]);
    assert!(graphref::lookup_in(&registered.0, "").is_err());
}

/// Terra #15629: a config-only lookup never reads `agents/`, so an
/// oversized, linked, folder or malformed definition refuses the whole
/// read but not the config one, and it returns no definitions at all,
/// inline ones included.
#[test]
fn a_config_only_lookup_ignores_every_definition() {
    use graphref::Lookup;
    let config_only = |repo: &Repo| match graphref::lookup_config_in(&repo.0, "") {
        Ok(Lookup::Found(found)) => found,
        other => panic!("config-only lookup: {other:?}"),
    };
    let repo = Repo::new();
    let found = config_only(&repo);
    assert_eq!(found.graph_id, GRAPH_ID);
    assert!(found.definitions.is_empty(), "{:?}", found.definitions);
    for what in ["malformed", "oversized", "folder", "link"] {
        let repo = Repo::new();
        repo.git(&["checkout", "-q", "mdkg-graph"]);
        let bad = ".mdkg/agents/bad.json";
        match what {
            "malformed" => repo.write(bad, "{ not json"),
            "oversized" => {
                repo.write(bad, &format!("{{\"x\":\"{}\"}}", "a".repeat(1024 * 1024)));
            }
            "folder" => repo.write(&format!("{bad}/inner"), "x"),
            _ => std::os::unix::fs::symlink("astra.json", repo.0.join(bad)).expect("link"),
        }
        repo.git(&["add", "."]);
        repo.git(&["commit", "-qm", what]);
        repo.git(&["checkout", "-qf", "trunk"]);
        repo.git(&["clean", "-fdxq"]);
        assert!(
            graphref::lookup_in(&repo.0, "").is_err(),
            "{what}: whole read"
        );
        let found = config_only(&repo);
        assert!(found.definitions.is_empty(), "{what}");
    }
}

/// Terra #15782: the folder a root's records name, as authority reads use
/// it; disagreeing records are an error.
#[test]
fn folder_follows_the_root_records() {
    let repo = Repo::new();
    let folder = |repo: &Repo| {
        graphref::resolve(&repo.0)
            .expect("resolve")
            .expect("ref mode")
            .folder("")
    };
    assert_eq!(folder(&repo).expect("default"), ".mdkg");
    repo.git(&["config", &format!("mdkg.graph.{GRAPH_ID}.dir"), "knowledge"]);
    assert_eq!(folder(&repo).expect("recorded"), "knowledge");
    repo.git(&["config", &format!("mdkg.graph.{OTHER_ID}.root"), ""]);
    assert!(folder(&repo).is_err(), "two records disagree");
}

#[test]
fn a_recorded_graph_moved_to_another_root_is_refused() {
    // Root graph A and nested graph B both recorded, each at its root.
    let repo = Repo::new();
    repo.git(&[
        "config",
        &format!("mdkg.graph.{OTHER_ID}.ref"),
        "mdkg-graph",
    ]);
    repo.git(&["config", &format!("mdkg.graph.{OTHER_ID}.root"), "sub"]);
    let graph = graphref::resolve(&repo.0)
        .expect("resolve")
        .expect("ref mode");
    let writer = graph
        .writer(&repo.0, LockMode::Local)
        .expect("writer")
        .with_paths(&[".mdkg".into(), "sub/.mdkg".into()])
        .expect("paths");
    // A's config placed at B's root.
    let request = Request {
        id: "moved".into(),
        base: Some(writer.snapshot().expect("tip")),
        changes: vec![put(
            "sub/.mdkg/config.json",
            format!(r#"{{"schema_version":3,"format_version":"0.7","graph_id":"{GRAPH_ID}"}}"#)
                .as_bytes(),
        )],
        message: "moved".into(),
        trailers: vec![],
    };
    // The writer refuses to publish A's id at B's root (orchestrator
    // #6947), and nothing is published.
    let tip = writer.snapshot().expect("tip");
    let err = writer.write(&request).expect_err("A at B's root");
    assert_eq!(err.code(), "graph.identity_mismatch", "{err}");
    assert!(err.to_string().contains("mdkg install"), "{err}");
    assert_eq!(writer.snapshot().expect("tip"), tip);
    assert_eq!(
        graphref::authority_in(&repo.0, "sub")
            .expect_err("no graph at sub")
            .code,
        "graph.authority_invalid"
    );
    // And B's id at the repository root is refused for the root too.
    let graph = graphref::resolve(&repo.0)
        .expect("resolve")
        .expect("ref mode");
    assert!(graph.check_identity(OTHER_ID).is_err());
    graph.check_identity(GRAPH_ID).expect("A at its root");
    // Install's own check is bound to a root as well (Opus #6894 minor 3).
    graph
        .check_identity_or_unrecorded(OTHER_ID, "./sub")
        .expect("B adopted at its root");
    assert_eq!(
        graph
            .check_identity_or_unrecorded(OTHER_ID, "")
            .expect_err("B at the root")
            .code,
        "graph.identity_mismatch"
    );
}

#[test]
fn an_unreadable_registered_checkout_refuses_instead_of_falling_through() {
    let repo = Repo::new();
    repo.git(&["config", "--unset", "mdkg.graph.ref"]);
    repo.git(&["config", "--unset", &format!("mdkg.graph.{GRAPH_ID}.ref")]);
    // Two registered checkouts at the root: A (GRAPH_ID) and B (OTHER_ID).
    repo.git(&["worktree", "add", "-q", "--detach", "a", GRAPH]);
    repo.git(&["worktree", "add", "-q", "--detach", "b", GRAPH]);
    let b = repo.0.join("b");
    std::fs::write(
        b.join(".mdkg/config.json"),
        format!(r#"{{"schema_version":3,"format_version":"0.7","graph_id":"{OTHER_ID}"}}"#),
    )
    .expect("config");
    let commit = |dir: &Path, message: &str| {
        let out = git_command(dir).args(["add", "."]).output().expect("add");
        assert!(out.status.success());
        let out = git_command(dir)
            .args(["commit", "-qm", message])
            .output()
            .expect("commit");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    commit(&b, "graph B");
    repo.git(&["config", &format!("mdkg.graph.{GRAPH_ID}.worktree"), "a"]);
    repo.git(&["config", &format!("mdkg.graph.{OTHER_ID}.worktree"), "b"]);
    // Both readable: two candidates at the root, refused.
    assert_eq!(
        graphref::authority(&repo.0).expect_err("ambiguous").code,
        "graph.identity_mismatch"
    );
    // A's config breaks: the lookup refuses, never B's authority.
    let a = repo.0.join("a");
    std::fs::write(a.join(".mdkg/config.json"), b"{not json").expect("broken");
    commit(&a, "broken config");
    assert!(graphref::authority(&repo.0).is_err());
    // B is the single candidate once A is no longer recorded; A's broken
    // config isn't read at all.
    repo.git(&[
        "config",
        "--unset",
        &format!("mdkg.graph.{GRAPH_ID}.worktree"),
    ]);
    assert_eq!(
        graphref::authority(&repo.0).expect("B alone").graph_id,
        OTHER_ID
    );
    // Definitions load only for the single match, and an error there is
    // reported as itself.
    std::fs::write(b.join(".mdkg/agents/broken.json"), b"{not json").expect("broken");
    commit(&b, "broken definition");
    assert_eq!(
        graphref::authority(&repo.0)
            .expect_err("B's definition")
            .code,
        "graph.authority_invalid"
    );
}

/// Opus #6894 major 1: a checkout recorded only for another root is no
/// candidate, so graphs at two roots in two checkouts each resolve.
#[test]
fn checkouts_of_graphs_at_two_roots_each_resolve_their_own() {
    let repo = Repo::new();
    repo.git(&["config", "--unset", "mdkg.graph.ref"]);
    repo.git(&["config", "--unset", &format!("mdkg.graph.{GRAPH_ID}.ref")]);
    // X holds the root graph (A); Y holds only a nested graph (B) at sub.
    repo.git(&["worktree", "add", "-q", "--detach", "x", GRAPH]);
    orphan_worktree(&repo, "y");
    let y = repo.0.join("y");
    std::fs::create_dir_all(y.join("sub/.mdkg")).expect("mkdir");
    std::fs::write(
        y.join("sub/.mdkg/config.json"),
        format!(r#"{{"schema_version":3,"format_version":"0.7","graph_id":"{OTHER_ID}"}}"#),
    )
    .expect("config");
    let out = git_command(&y).args(["add", "."]).output().expect("add");
    assert!(out.status.success());
    let out = git_command(&y)
        .args(["commit", "-qm", "nested graph B"])
        .output()
        .expect("commit");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    repo.git(&["config", &format!("mdkg.graph.{GRAPH_ID}.worktree"), "x"]);
    repo.git(&["config", &format!("mdkg.graph.{OTHER_ID}.worktree"), "y"]);
    repo.git(&["config", &format!("mdkg.graph.{OTHER_ID}.root"), "sub"]);
    let root = graphref::authority_in(&repo.0, "").expect("A at the root");
    assert_eq!(root.graph_id, GRAPH_ID);
    assert!(root.source.starts_with("checkout:worktrees/x/HEAD@"));
    let nested = graphref::authority_in(&repo.0, "sub").expect("B at sub");
    assert_eq!(nested.graph_id, OTHER_ID);
    assert!(nested.source.starts_with("checkout:worktrees/y/HEAD@"));
    // Legacy 0.7.0 records (no `.root`) bind the root their checkout holds
    // their id at, in checkout mode (orchestrator #6947, G19).
    repo.git(&["config", "--unset", &format!("mdkg.graph.{OTHER_ID}.root")]);
    assert_eq!(
        graphref::authority_in(&repo.0, "sub")
            .expect("legacy B at sub")
            .graph_id,
        OTHER_ID
    );
    assert_eq!(
        graphref::authority_in(&repo.0, "")
            .expect("A at the root")
            .graph_id,
        GRAPH_ID
    );
    // A legacy root record never binds a nested copy of its config.
    let x = repo.0.join("x");
    std::fs::create_dir_all(x.join("sub/.mdkg")).expect("mkdir");
    std::fs::copy(x.join(".mdkg/config.json"), x.join("sub/.mdkg/config.json")).expect("copy");
    let out = git_command(&x).args(["add", "."]).output().expect("add");
    assert!(out.status.success());
    let out = git_command(&x)
        .args(["commit", "-qm", "A copied to sub"])
        .output()
        .expect("commit");
    assert!(out.status.success());
    assert_eq!(
        graphref::authority_in(&repo.0, "sub")
            .expect("still only B at sub")
            .graph_id,
        OTHER_ID
    );
    // A `.root` record with no checkout record is partial: refused.
    repo.git(&[
        "config",
        "--unset",
        &format!("mdkg.graph.{OTHER_ID}.worktree"),
    ]);
    repo.git(&["config", &format!("mdkg.graph.{OTHER_ID}.root"), "sub"]);
    assert_eq!(
        graphref::authority_in(&repo.0, "sub")
            .expect_err("partial records")
            .code,
        "graph.identity_mismatch"
    );
    // It refuses only lookups at its own root (Opus #7736): the root graph,
    // whose records are complete, still resolves.
    assert_eq!(
        graphref::authority(&repo.0)
            .expect("the root graph is unaffected")
            .graph_id,
        GRAPH_ID
    );
    // With no checkout record at all, a lookup at the partial record's root
    // still refuses rather than reading the invoking HEAD.
    repo.git(&[
        "config",
        "--unset",
        &format!("mdkg.graph.{GRAPH_ID}.worktree"),
    ]);
    assert_eq!(
        graphref::authority_in(&repo.0, "sub")
            .expect_err("never the invoking HEAD")
            .code,
        "graph.identity_mismatch"
    );
}

/// Terra #6917: two ids bound to one root (two records with no `.root`)
/// are refused, so one can't replace the other.
#[test]
fn two_graphs_recorded_at_one_root_are_refused() {
    let repo = Repo::new();
    graphref::authority(&repo.0).expect("own graph");
    repo.git(&[
        "config",
        &format!("mdkg.graph.{OTHER_ID}.ref"),
        "mdkg-graph",
    ]);
    let err = graphref::authority(&repo.0).expect_err("two at the root");
    assert_eq!(err.code, "graph.identity_mismatch");
    assert!(err.message.contains("several graphs"), "{}", err.message);
    repo.git(&["config", &format!("mdkg.graph.{OTHER_ID}.root"), "sub"]);
    graphref::authority(&repo.0).expect("each at its own root");
}

/// orchestrator #6947: a writer rechecks the other graphs' bindings at
/// prepare and publish.
#[test]
fn a_writer_refuses_once_the_identity_records_change() {
    let repo = Repo::new();
    let graph = graphref::resolve(&repo.0)
        .expect("resolve")
        .expect("ref mode");
    let writer = graph.writer(&repo.0, LockMode::Local).expect("writer");
    repo.git(&[
        "config",
        &format!("mdkg.graph.{OTHER_ID}.ref"),
        "mdkg-graph",
    ]);
    repo.git(&["config", &format!("mdkg.graph.{OTHER_ID}.root"), "sub"]);
    let request = Request {
        id: "records-changed".into(),
        base: Some(writer.snapshot().expect("tip")),
        changes: vec![put(".mdkg/design/r.md", b"r")],
        message: "r".into(),
        trailers: vec![],
    };
    let err = writer.write(&request).expect_err("records changed");
    assert_eq!(err.code(), "graph.identity_mismatch", "{err}");
}

#[test]
fn a_checkout_with_relative_worktree_links_is_registered() {
    let repo = Repo::new();
    repo.git(&["config", "--unset", "mdkg.graph.ref"]);
    repo.git(&["config", "--unset", &format!("mdkg.graph.{GRAPH_ID}.ref")]);
    let out = git_command(&repo.0)
        .args([
            "worktree",
            "add",
            "-q",
            "--relative-paths",
            "--detach",
            "view",
            GRAPH,
        ])
        .output()
        .expect("worktree");
    if !out.status.success() {
        // Only git older than 2.48, which has no relative worktree links,
        // may skip; any other failure fails the test.
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            // git's own diagnostic for an option it doesn't know (Terra
            // #7983), not any usage text that mentions it.
            stderr.contains("unknown option") && stderr.contains("relative-paths"),
            "worktree add failed: {stderr}"
        );
        return;
    }
    let link = std::fs::read_to_string(repo.0.join("view/.git")).expect(".git");
    assert!(
        !link.contains(&*repo.0.to_string_lossy()),
        "relative: {link}"
    );
    repo.git(&["config", &format!("mdkg.graph.{GRAPH_ID}.worktree"), "view"]);
    graphref::authority(&repo.0).expect("a live checkout with relative links");
}

#[test]
fn a_registered_nested_graph_alone_gets_a_writer() {
    let repo = Repo::new();
    // Install records a nested graph before writing it (orchestrator #6947).
    record_nested(&repo);
    let graph = graphref::resolve(&repo.0)
        .expect("resolve")
        .expect("ref mode");
    let root_writer = graph
        .writer(&repo.0, LockMode::Local)
        .expect("writer")
        .with_paths(&[".mdkg".into(), "sub/.mdkg".into()])
        .expect("paths");
    let seed = Request {
        id: "nested-seed".into(),
        base: Some(root_writer.snapshot().expect("tip")),
        changes: vec![put(
            "sub/.mdkg/config.json",
            format!(r#"{{"schema_version":3,"format_version":"0.7","graph_id":"{OTHER_ID}"}}"#)
                .as_bytes(),
        )],
        message: "seed".into(),
        trailers: vec![],
    };
    root_writer.write(&seed).expect("seed");
    // Only the nested graph is registered now.
    repo.git(&["config", "--unset", &format!("mdkg.graph.{GRAPH_ID}.ref")]);
    repo.git(&[
        "config",
        &format!("mdkg.graph.{OTHER_ID}.ref"),
        "mdkg-graph",
    ]);
    repo.git(&["config", &format!("mdkg.graph.{OTHER_ID}.root"), "sub"]);
    let graph = graphref::resolve(&repo.0)
        .expect("resolve")
        .expect("ref mode");
    assert!(graph.writer(&repo.0, LockMode::Local).is_err());
    let nested = graph
        .writer_in(&repo.0, LockMode::Local, "sub")
        .expect("nested writer")
        .with_paths(&["sub/.mdkg".into()])
        .expect("paths");
    let write = Request {
        id: "nested-write".into(),
        base: Some(nested.snapshot().expect("tip")),
        changes: vec![put("sub/.mdkg/design/n.md", b"n")],
        message: "nested".into(),
        trailers: vec![],
    };
    nested.write(&write).expect("nested write");
}

#[test]
fn a_poisoned_checkout_record_never_opens_a_file_outside_a_checkout() {
    let repo = Repo::new();
    repo.git(&["config", "--unset", "mdkg.graph.ref"]);
    repo.git(&["config", "--unset", &format!("mdkg.graph.{GRAPH_ID}.ref")]);
    repo.git(&["worktree", "add", "-q", "--detach", "view", GRAPH]);
    repo.git(&["config", &format!("mdkg.graph.{GRAPH_ID}.worktree"), "view"]);
    // The admin record names an arbitrary file instead of a `.git`.
    let secret = repo.0.join("secret.txt");
    std::fs::write(&secret, "gitdir: whatever").expect("secret");
    std::fs::write(
        repo.0.join(".git/worktrees/view/gitdir"),
        secret.to_string_lossy().as_bytes(),
    )
    .expect("poison");
    assert_eq!(
        graphref::authority(&repo.0).expect_err("poisoned").code,
        "graph.ref_missing"
    );
}

#[test]
fn a_crash_between_the_create_and_its_records_is_adopted() {
    let repo = Repo::new();
    repo.git(&["update-ref", "-d", GRAPH]);
    repo.git(&["config", "--unset", &format!("mdkg.graph.{GRAPH_ID}.ref")]);
    let graph = graphref::resolve(&repo.0)
        .expect("resolve")
        .expect("ref mode");
    // A bad id and an already-registered one are refused.
    assert!(
        graph
            .creator(&repo.0, LockMode::Local, "not-a-uuid", "")
            .is_err()
    );
    let request = Request {
        id: "init".into(),
        base: None,
        changes: vec![put(
            ".mdkg/config.json",
            format!(r#"{{"schema_version":3,"format_version":"0.7","graph_id":"{GRAPH_ID}"}}"#)
                .as_bytes(),
        )],
        message: "init".into(),
        trailers: vec![],
    };
    // The created graph must carry the intended id.
    let wrong = graph
        .creator(&repo.0, LockMode::Local, OTHER_ID, "")
        .expect("creator");
    assert!(wrong.write(&request).is_err());
    // One creation at a time (Opus #7225): its pending intent refuses a
    // second creator rather than being replaced.
    let err = graph
        .creator(&repo.0, LockMode::Local, GRAPH_ID, "")
        .expect_err("another creation is pending");
    assert_eq!(err.code, "graph.ref_exists");
    assert!(err.message.contains("creating.json"), "{}", err.message);
    std::fs::remove_file(repo.0.join(".git/mdkg/graph/creating.json")).expect("abandon it");
    let creator = graph
        .creator(&repo.0, LockMode::Local, GRAPH_ID, "")
        .expect("creator");
    let receipt = creator.write(&request).expect("create");
    // Crash here: the records were never written.
    assert!(
        graphref::resolve(&repo.0)
            .expect("resolve")
            .expect("ref")
            .graph_ids
            .is_empty()
    );
    let graph = graphref::resolve(&repo.0)
        .expect("resolve")
        .expect("ref mode");
    let adopted = graph
        .creator(&repo.0, LockMode::Local, GRAPH_ID, "")
        .expect("adopted");
    let again = adopted.write(&request).expect("rerun");
    assert!(again.already_applied);
    assert_eq!(again.commit, receipt.commit);
    let graph = graphref::resolve(&repo.0)
        .expect("resolve")
        .expect("ref mode");
    assert_eq!(graph.graph_ids, vec![GRAPH_ID.to_owned()]);
    graphref::authority(&repo.0).expect("registered and readable");
    // Now registered: a second creator for it is refused.
    assert!(
        graph
            .creator(&repo.0, LockMode::Local, GRAPH_ID, "")
            .is_err()
    );
}

#[test]
fn a_bound_writer_cannot_change_a_registered_nested_graphs_id() {
    let repo = Repo::new();
    // Install records a nested graph before writing it (orchestrator #6947).
    record_nested(&repo);
    let graph = graphref::resolve(&repo.0)
        .expect("resolve")
        .expect("ref mode");
    let writer = graph
        .writer(&repo.0, LockMode::Local)
        .expect("writer")
        .with_paths(&[".mdkg".into(), "sub/.mdkg".into()])
        .expect("paths");
    let config =
        |id: &str| format!(r#"{{"schema_version":3,"format_version":"0.7","graph_id":"{id}"}}"#);
    let seed = Request {
        id: "nested".into(),
        base: Some(writer.snapshot().expect("tip")),
        changes: vec![put("sub/.mdkg/config.json", config(OTHER_ID).as_bytes())],
        message: "nested".into(),
        trailers: vec![],
    };
    writer.write(&seed).expect("seed");
    repo.git(&[
        "config",
        &format!("mdkg.graph.{OTHER_ID}.ref"),
        "mdkg-graph",
    ]);
    repo.git(&["config", &format!("mdkg.graph.{OTHER_ID}.root"), "sub"]);
    let graph = graphref::resolve(&repo.0)
        .expect("resolve")
        .expect("ref mode");
    let writer = graph
        .writer(&repo.0, LockMode::Local)
        .expect("writer")
        .with_paths(&[".mdkg".into(), "sub/.mdkg".into()])
        .expect("paths");
    let third = "0192f3a0-0000-7000-8000-0000000000aa";
    let change = Request {
        id: "transfer".into(),
        base: Some(writer.snapshot().expect("tip")),
        changes: vec![put("sub/.mdkg/config.json", config(third).as_bytes())],
        message: "transfer".into(),
        trailers: vec![],
    };
    assert_eq!(
        writer
            .write(&change)
            .expect_err("no identity transfer")
            .code(),
        "graph.identity_mismatch"
    );
}

#[test]
fn a_git_failure_is_an_error_never_authority_absent() {
    use graphref::Lookup;
    let repo = Repo::new();
    repo.git(&["config", "--unset", "mdkg.graph.ref"]);
    repo.git(&["config", "--unset", &format!("mdkg.graph.{GRAPH_ID}.ref")]);
    // A normalized root still finds nothing at a code-only HEAD.
    assert!(matches!(
        graphref::lookup_in(&repo.0, "./sub/.").expect("lookup"),
        Lookup::Absent(_)
    ));
    // HEAD's tree object goes missing: a failure, not "no graph".
    let tree = repo.git(&["rev-parse", "HEAD^{tree}"]);
    let object = repo
        .0
        .join(".git/objects")
        .join(&tree[..2])
        .join(&tree[2..]);
    let mut perms = std::fs::metadata(&object).expect("object").permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(&object, perms).ok();
    std::fs::remove_file(&object).expect("remove the tree object");
    assert!(graphref::lookup_in(&repo.0, "").is_err());
}

/// Terra #6964: an unborn branch is proven, a broken HEAD is an error, and
/// the root is read from the repository root wherever the call is made.
#[test]
fn lookup_proves_unborn_and_reads_the_root_from_any_subdirectory() {
    use graphref::{Absent, Lookup};
    let repo = Repo::new();
    repo.git(&["config", "--unset", "mdkg.graph.ref"]);
    repo.git(&["config", "--unset", &format!("mdkg.graph.{GRAPH_ID}.ref")]);
    // A checkout of the graph, called from a subfolder with "." and "./".
    repo.git(&["worktree", "add", "-q", "--detach", "x", GRAPH]);
    let x = repo.0.join("x");
    for root in [".", "./", ""] {
        assert!(
            matches!(
                graphref::lookup_in(&x.join(".mdkg"), root).expect("lookup"),
                Lookup::Found(_)
            ),
            "{root:?}"
        );
    }
    // A root that leaves the repository is an error, never absence.
    assert_eq!(
        graphref::lookup_in(&x, "../x").expect_err("..").code,
        "graph.config_invalid"
    );
    // An unborn branch is proven unborn.
    orphan_worktree(&repo, "y");
    assert!(matches!(
        graphref::lookup_in(&repo.0.join("y"), "").expect("unborn"),
        Lookup::Absent(Absent::Unborn)
    ));
    // A detached HEAD naming a missing commit is an error, not unborn.
    let missing = "0123456789abcdef0123456789abcdef01234567";
    std::fs::write(repo.0.join(".git/worktrees/x/HEAD"), format!("{missing}\n")).expect("HEAD");
    assert!(graphref::lookup_in(&x, "").is_err());
}

/// Terra #6994: links are read with line endings stripped, nothing else,
/// so a checkout folder ending in a space is still registered.
#[test]
fn a_checkout_folder_ending_in_a_space_is_registered() {
    let repo = Repo::new();
    repo.git(&["config", "--unset", "mdkg.graph.ref"]);
    repo.git(&["config", "--unset", &format!("mdkg.graph.{GRAPH_ID}.ref")]);
    repo.git(&["worktree", "add", "-q", "--detach", "sp ", GRAPH]);
    let admin = std::fs::read_dir(repo.0.join(".git/worktrees"))
        .expect("admin folders")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .find(|n| n.starts_with("sp"))
        .expect("the checkout's admin folder");
    repo.git(&["config", &format!("mdkg.graph.{GRAPH_ID}.worktree"), &admin]);
    assert_eq!(
        graphref::authority(&repo.0).expect("registered").graph_id,
        GRAPH_ID
    );
}

/// orchestrator #7181: the upstream guard sees tracking config as ordinary
/// git does, an inherited GIT_CONFIG_GLOBAL included.
#[test]
fn an_upstream_in_an_inherited_global_config_is_refused() {
    let repo = Repo::new();
    let global = repo.0.join("alt-global");
    std::fs::write(
        &global,
        "[branch \"mdkg-graph\"]\n\tremote = origin\n\tmerge = refs/heads/mdkg-graph\n",
    )
    .expect("global");
    let out = Command::new(std::env::current_exe().expect("test binary"))
        .args(["--exact", "inherited_upstream_child", "--test-threads=1"])
        .env("MDKG_GRAPHREF_IDENTITY", &repo.0)
        .env("GIT_CONFIG_GLOBAL", &global)
        .output()
        .expect("child");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn inherited_upstream_child() {
    let Some(root) = std::env::var_os("MDKG_GRAPHREF_IDENTITY") else {
        return;
    };
    let graph = graphref::resolve(Path::new(&root))
        .expect("resolve")
        .expect("ref mode");
    let err = graph
        .writer(Path::new(&root), LockMode::Local)
        .expect_err("a tracking graph branch");
    assert!(err.message.contains("upstream"), "{}", err.message);
}

/// Terra #7173: a nested writer's default scope is its own `.mdkg/`, not
/// the root graph's.
#[test]
fn a_nested_writer_writes_its_own_folder_by_default() {
    let repo = Repo::new();
    record_nested(&repo);
    let graph = graphref::resolve(&repo.0)
        .expect("resolve")
        .expect("ref mode");
    let root_writer = graph
        .writer(&repo.0, LockMode::Local)
        .expect("writer")
        .with_paths(&[".mdkg".into(), "sub/.mdkg".into()])
        .expect("paths");
    let seed = Request {
        id: "nested-default-seed".into(),
        base: Some(root_writer.snapshot().expect("tip")),
        changes: vec![put(
            "sub/.mdkg/config.json",
            format!(r#"{{"schema_version":3,"format_version":"0.7","graph_id":"{OTHER_ID}"}}"#)
                .as_bytes(),
        )],
        message: "seed".into(),
        trailers: vec![],
    };
    root_writer.write(&seed).expect("seed");
    let graph = graphref::resolve(&repo.0)
        .expect("resolve")
        .expect("ref mode");
    let nested = graph
        .writer_in(&repo.0, LockMode::Local, "sub")
        .expect("nested writer");
    let write = |id: &str, path: &str| {
        nested.write(&Request {
            id: id.into(),
            base: Some(nested.snapshot().expect("tip")),
            changes: vec![put(path, b"x")],
            message: id.into(),
            trailers: vec![],
        })
    };
    write("nested-own", "sub/.mdkg/design/n.md").expect("its own folder");
    assert!(write("nested-root", ".mdkg/design/n.md").is_err());
}

/// Terra #7173: another tool's non-UTF-8 config value doesn't block
/// resolution.
#[test]
fn a_non_utf8_value_outside_mdkg_does_not_block_resolution() {
    use std::io::Write as _;
    let repo = Repo::new();
    let mut config = std::fs::OpenOptions::new()
        .append(true)
        .open(repo.0.join(".git/config"))
        .expect("config");
    config
        .write_all(b"[user]\n\tlegacy = caf\xe9\n")
        .expect("legacy value");
    drop(config);
    graphref::resolve(&repo.0)
        .expect("resolve")
        .expect("ref mode");
    graphref::authority(&repo.0).expect("authority");
}

/// Opus #7225: a nested creation records `.root` before `.ref`, so a crash
/// between the two never registers the id at the repository root.
#[test]
fn a_crash_between_a_nested_creations_records_never_binds_the_root() {
    let repo = Repo::new();
    repo.git(&["update-ref", "-d", GRAPH]);
    repo.git(&["config", "--unset", &format!("mdkg.graph.{GRAPH_ID}.ref")]);
    let graph = graphref::resolve(&repo.0)
        .expect("resolve")
        .expect("ref mode");
    let creator = graph
        .creator(&repo.0, LockMode::Local, OTHER_ID, "sub")
        .expect("creator");
    let request = Request {
        id: "nested-init".into(),
        base: None,
        changes: vec![put(
            "sub/.mdkg/config.json",
            format!(r#"{{"schema_version":3,"format_version":"0.7","graph_id":"{OTHER_ID}"}}"#)
                .as_bytes(),
        )],
        message: "init".into(),
        trailers: vec![],
    };
    creator.write(&request).expect("create");
    // Crash after the first record: only `.root` is written.
    repo.git(&["config", &format!("mdkg.graph.{OTHER_ID}.root"), "sub"]);
    let graph = graphref::resolve(&repo.0)
        .expect("resolve")
        .expect("ref mode");
    assert!(graph.graph_ids.is_empty(), "not registered yet");
    assert!(graph.check_identity_at(OTHER_ID, "").is_err());
    // The rerun adopts it, bound to its root.
    graph
        .creator(&repo.0, LockMode::Local, OTHER_ID, "sub")
        .expect("adopted");
    let graph = graphref::resolve(&repo.0)
        .expect("resolve")
        .expect("ref mode");
    graph.check_identity_at(OTHER_ID, "sub").expect("at sub");
    assert!(graph.check_identity_at(OTHER_ID, "").is_err());
    assert!(!repo.0.join(".git/mdkg/graph/creating.json").exists());
}

/// orchestrator #7290 item 3: a new id must be absent from every record,
/// whatever ref or checkout it names.
#[test]
fn a_creator_refuses_an_id_recorded_for_anything_else() {
    let repo = Repo::new();
    repo.git(&["update-ref", "-d", GRAPH]);
    repo.git(&["config", "--unset", &format!("mdkg.graph.{GRAPH_ID}.ref")]);
    // Recorded for a checkout only, never for this graph ref.
    repo.git(&[
        "config",
        &format!("mdkg.graph.{OTHER_ID}.worktree"),
        "elsewhere",
    ]);
    let graph = graphref::resolve(&repo.0)
        .expect("resolve")
        .expect("ref mode");
    let err = graph
        .creator(&repo.0, LockMode::Local, OTHER_ID, "")
        .expect_err("recorded elsewhere");
    assert_eq!(err.code, "graph.identity_mismatch");
}

/// orchestrator #7290 items 2 and 5: completion rechecks the current
/// config under the lock and keeps the intent when the graph ref changed;
/// an abandoned creation can be retired.
#[test]
fn completion_rechecks_the_config_and_abandoned_creations_retire() {
    let repo = Repo::new();
    repo.git(&["update-ref", "-d", GRAPH]);
    repo.git(&["config", "--unset", &format!("mdkg.graph.{GRAPH_ID}.ref")]);
    let graph = graphref::resolve(&repo.0)
        .expect("resolve")
        .expect("ref mode");
    let creator = graph
        .creator(&repo.0, LockMode::Local, GRAPH_ID, "")
        .expect("creator");
    let request = Request {
        id: "init-config".into(),
        base: None,
        changes: vec![put(
            ".mdkg/config.json",
            format!(r#"{{"schema_version":3,"format_version":"0.7","graph_id":"{GRAPH_ID}"}}"#)
                .as_bytes(),
        )],
        message: "init".into(),
        trailers: vec![],
    };
    creator.write(&request).expect("create");
    let intent = repo.0.join(".git/mdkg/graph/creating.json");
    // The graph ref is reconfigured before completion: the intent stays.
    repo.git(&["config", "mdkg.graph.ref", "refs/heads/other-graph"]);
    let err = graph
        .complete_creation(&repo.0)
        .expect_err("config changed");
    assert_eq!(err.code, "graph.identity_mismatch");
    assert!(intent.exists(), "kept");
    repo.git(&["config", "mdkg.graph.ref", GRAPH]);
    graph.complete_creation(&repo.0).expect("completed");
    assert!(!intent.exists());
    // An abandoned creation (never published) is retired on request.
    let other = Repo::new();
    other.git(&["update-ref", "-d", GRAPH]);
    other.git(&["config", "--unset", &format!("mdkg.graph.{GRAPH_ID}.ref")]);
    let graph = graphref::resolve(&other.0)
        .expect("resolve")
        .expect("ref mode");
    graph
        .creator(&other.0, LockMode::Local, GRAPH_ID, "")
        .expect("creator");
    assert!(
        graph
            .creator(&other.0, LockMode::Local, OTHER_ID, "")
            .is_err()
    );
    graph.abandon_creation(&other.0).expect("retired");
    graph
        .creator(&other.0, LockMode::Local, OTHER_ID, "")
        .expect("a new creation");
}

/// orchestrator #7290 item 8: a graph ref naming a missing object is an
/// error, never a missing ref.
#[test]
fn a_graph_ref_naming_a_missing_object_is_an_error() {
    let repo = Repo::new();
    let missing = "0123456789abcdef0123456789abcdef01234567";
    std::fs::create_dir_all(repo.0.join(".git/refs/heads")).expect("refs");
    std::fs::write(
        repo.0.join(".git/refs/heads/mdkg-graph"),
        format!("{missing}\n"),
    )
    .expect("broken ref");
    std::fs::remove_file(repo.0.join(".git/packed-refs")).ok();
    let err = graphref::authority(&repo.0).expect_err("broken");
    assert_ne!(err.code, "graph.ref_missing", "{}", err.message);
}

/// orchestrator #7376 and #7556: a 0.7.0-style clone (a nested graph
/// recorded only by `.worktree`, no `.root`) keeps its nested graph in
/// checkout mode and in ref mode.
#[test]
fn a_0_7_0_clone_keeps_its_nested_graph_in_both_modes() {
    let repo = Repo::new();
    record_nested(&repo);
    let writer = graph_writer(&repo.0)
        .with_paths(&[".mdkg".into(), "sub/.mdkg".into()])
        .expect("paths");
    writer
        .write(&Request {
            id: "legacy-nested".into(),
            base: Some(writer.snapshot().expect("tip")),
            changes: vec![put(
                "sub/.mdkg/config.json",
                format!(r#"{{"schema_version":3,"format_version":"0.7","graph_id":"{OTHER_ID}"}}"#)
                    .as_bytes(),
            )],
            message: "nested".into(),
            trailers: vec![],
        })
        .expect("nested graph");
    // As 0.7.0 recorded it: `.worktree` only.
    repo.git(&["config", "--unset", &format!("mdkg.graph.{OTHER_ID}.ref")]);
    repo.git(&["config", "--unset", &format!("mdkg.graph.{OTHER_ID}.root")]);
    repo.git(&["worktree", "add", "-q", "--detach", "x", GRAPH]);
    repo.git(&["config", &format!("mdkg.graph.{OTHER_ID}.worktree"), "x"]);
    // Ref mode.
    assert_eq!(
        graphref::authority_in(&repo.0, "sub")
            .expect("ref mode")
            .graph_id,
        OTHER_ID
    );
    assert_eq!(
        graphref::authority_in(&repo.0, "").expect("root").graph_id,
        GRAPH_ID
    );
    // Checkout mode: both graphs in one recorded checkout.
    repo.git(&["config", "--unset", "mdkg.graph.ref"]);
    repo.git(&["config", "--unset", &format!("mdkg.graph.{GRAPH_ID}.ref")]);
    repo.git(&["config", &format!("mdkg.graph.{GRAPH_ID}.worktree"), "x"]);
    assert_eq!(
        graphref::authority_in(&repo.0, "sub")
            .expect("checkout mode")
            .graph_id,
        OTHER_ID
    );
    assert_eq!(
        graphref::authority_in(&repo.0, "").expect("root").graph_id,
        GRAPH_ID
    );
}

/// orchestrator #7628: candidates are picked by graph id alone, so an
/// unrelated malformed field in a candidate that doesn't match never blocks
/// the right one; the match is still validated in full.
#[test]
fn an_unrelated_bad_field_in_another_candidate_never_blocks_the_match() {
    let repo = Repo::new();
    repo.git(&["config", "--unset", "mdkg.graph.ref"]);
    repo.git(&["config", "--unset", &format!("mdkg.graph.{GRAPH_ID}.ref")]);
    repo.git(&["worktree", "add", "-q", "--detach", "a", GRAPH]);
    repo.git(&["worktree", "add", "-q", "--detach", "b", GRAPH]);
    let commit = |dir: &Path, config: String| {
        std::fs::write(dir.join(".mdkg/config.json"), config).expect("config");
        let out = git_command(dir).args(["add", "."]).output().expect("add");
        assert!(out.status.success());
        let out = git_command(dir)
            .args(["commit", "-qm", "config"])
            .output()
            .expect("commit");
        assert!(out.status.success());
    };
    // a is a root candidate (OTHER_ID's 0.7.0 record names it) whose
    // committed id is a third graph's, with an invalid unrelated field.
    let third = "0192f3a0-0000-7000-8000-0000000000aa";
    commit(
        &repo.0.join("a"),
        format!(
            r#"{{"schema_version":3,"format_version":"0.7","graph_id":"{third}","work":{{"location":42}}}}"#
        ),
    );
    let a_head = repo.git(&["rev-parse", "worktrees/a/HEAD"]);
    assert!(
        graphref::authority_at_in(&repo.0, &a_head, "").is_err(),
        "the unrelated field really is invalid"
    );
    repo.git(&["config", &format!("mdkg.graph.{OTHER_ID}.worktree"), "a"]);
    repo.git(&["config", &format!("mdkg.graph.{GRAPH_ID}.worktree"), "b"]);
    let found = graphref::authority(&repo.0).expect("b is chosen");
    assert_eq!(found.graph_id, GRAPH_ID);
    assert!(found.source.starts_with("checkout:worktrees/b/HEAD@"));
    // The match itself is validated in full.
    commit(
        &repo.0.join("b"),
        format!(
            r#"{{"schema_version":3,"format_version":"0.7","graph_id":"{GRAPH_ID}","work":{{"location":42}}}}"#
        ),
    );
    assert!(graphref::authority(&repo.0).is_err());
}

/// Terra #7776: four identity gaps.
#[test]
fn identity_gaps_from_terra_7776_are_closed() {
    use graphref::Lookup;
    // 1. A creator rechecks the other bindings: a graph recorded at its
    // root after it opened refuses the create.
    let repo = Repo::new();
    repo.git(&["update-ref", "-d", GRAPH]);
    repo.git(&["config", "--unset", &format!("mdkg.graph.{GRAPH_ID}.ref")]);
    let graph = graphref::resolve(&repo.0)
        .expect("resolve")
        .expect("ref mode");
    let creator = graph
        .creator(&repo.0, LockMode::Local, GRAPH_ID, "")
        .expect("creator");
    repo.git(&[
        "config",
        &format!("mdkg.graph.{OTHER_ID}.ref"),
        "mdkg-graph",
    ]);
    let create = Request {
        id: "late-binding".into(),
        base: None,
        changes: vec![put(
            ".mdkg/config.json",
            format!(r#"{{"schema_version":3,"format_version":"0.7","graph_id":"{GRAPH_ID}"}}"#)
                .as_bytes(),
        )],
        message: "init".into(),
        trailers: vec![],
    };
    assert!(creator.write(&create).is_err(), "records changed under it");

    // 2. Two ids explicitly bound to one root are refused in checkout mode
    // and by install's own check.
    let repo = Repo::new();
    repo.git(&[
        "config",
        &format!("mdkg.graph.{OTHER_ID}.ref"),
        "mdkg-graph",
    ]);
    repo.git(&["config", &format!("mdkg.graph.{OTHER_ID}.root"), "sub"]);
    let third = "0192f3a0-0000-7000-8000-0000000000aa";
    repo.git(&["config", &format!("mdkg.graph.{third}.ref"), "mdkg-graph"]);
    repo.git(&["config", &format!("mdkg.graph.{third}.root"), "sub"]);
    let graph = graphref::resolve(&repo.0)
        .expect("resolve")
        .expect("ref mode");
    assert!(graph.check_identity_or_unrecorded(OTHER_ID, "sub").is_err());
    repo.git(&["config", "--unset", "mdkg.graph.ref"]);
    repo.git(&["worktree", "add", "-q", "--detach", "x", GRAPH]);
    repo.git(&["config", &format!("mdkg.graph.{OTHER_ID}.worktree"), "x"]);
    repo.git(&["config", &format!("mdkg.graph.{third}.worktree"), "x"]);
    assert_eq!(
        graphref::authority_in(&repo.0, "sub")
            .expect_err("two at sub")
            .code,
        "graph.identity_mismatch"
    );

    // 3. A graph nested inside another graph's folder is checked too.
    let repo = Repo::new();
    let writer = graph_writer(&repo.0);
    let nested = Request {
        id: "inner-graph".into(),
        base: Some(writer.snapshot().expect("tip")),
        changes: vec![put(
            ".mdkg/subprojects/foo/.mdkg/config.json",
            format!(r#"{{"schema_version":3,"format_version":"0.7","graph_id":"{OTHER_ID}"}}"#)
                .as_bytes(),
        )],
        message: "inner".into(),
        trailers: vec![],
    };
    let err = writer.write(&nested).expect_err("unrecorded inner graph");
    assert_eq!(err.code(), "graph.identity_mismatch", "{err}");

    // 4. A partial registration is never reported as a graphless checkout.
    let repo = Repo::new();
    repo.git(&["config", "--unset", "mdkg.graph.ref"]);
    repo.git(&["config", "--unset", &format!("mdkg.graph.{GRAPH_ID}.ref")]);
    repo.git(&["config", &format!("mdkg.graph.{OTHER_ID}.root"), "sub"]);
    assert!(
        !matches!(graphref::lookup_in(&repo.0, "sub"), Ok(Lookup::Absent(_))),
        "a partial record isn't absence"
    );
}

/// Terra #7807 (routed by orchestrator #7846): conflicting duplicate
/// records refuse, backslash roots refuse with a rename hint, and strict
/// admission runs the consumer's definition check.
#[test]
fn duplicate_records_backslash_roots_and_definition_checks_refuse() {
    use std::io::Write as _;
    let repo = Repo::new();
    // Two different values for one record key.
    let mut config = std::fs::OpenOptions::new()
        .append(true)
        .open(repo.0.join(".git/config"))
        .expect("config");
    write!(
        config,
        "[mdkg \"graph.{OTHER_ID}\"]\n\tworktree = a\n\tworktree = b\n"
    )
    .expect("dup");
    drop(config);
    assert_eq!(
        graphref::resolve(&repo.0).expect_err("conflict").code,
        "graph.config_invalid"
    );
    let repo = Repo::new();
    // A backslash in a graph-root folder name.
    let err = graphref::authority_in(&repo.0, "pack\\ages").expect_err("backslash");
    assert_eq!(err.code, "graph.config_invalid");
    assert!(err.message.contains("rename the folder"), "{}", err.message);
    // Strict admission with the consumer's check.
    let writer = graph_writer(&repo.0);
    let tip = writer
        .write(&Request {
            id: "empty-def".into(),
            base: Some(writer.snapshot().expect("tip")),
            changes: vec![put(".mdkg/agents/empty.json", b"{}")],
            message: "empty".into(),
            trailers: vec![],
        })
        .expect("write")
        .commit;
    mdkg_graph::committed::load_strict(&repo.0, &tip, "", GRAPH_ID).expect("no check");
    let check = |name: &str, value: &serde_json::Value| {
        if value.as_object().is_some_and(|o| o.is_empty()) {
            Err(format!("{name} is empty"))
        } else {
            Ok(())
        }
    };
    let err = mdkg_graph::committed::load_strict_with(&repo.0, &tip, "", GRAPH_ID, &check)
        .expect_err("the consumer's check refuses");
    assert!(err.to_string().contains("empty"), "{err}");
}

/// Opus #7843 and Terra #7885: unborn is proven exactly; a torn branch
/// ref, a missing non-branch target, an absolute root and a committed link
/// on the way to the graph folder are errors, never absence. A descendant
/// ref doesn't stop an absent branch being unborn.
#[test]
fn lookup_never_reads_a_broken_checkout_as_absent() {
    use graphref::{Absent, Lookup};
    let repo = Repo::new();
    repo.git(&["config", "--unset", "mdkg.graph.ref"]);
    repo.git(&["config", "--unset", &format!("mdkg.graph.{GRAPH_ID}.ref")]);
    repo.git(&["worktree", "add", "-q", "--detach", "w", GRAPH]);
    let w = repo.0.join("w");
    let head = repo.0.join(".git/worktrees/w/HEAD");
    // A symbolic HEAD to a torn (empty) branch ref.
    std::fs::write(&head, "ref: refs/heads/torn\n").expect("HEAD");
    std::fs::write(repo.0.join(".git/refs/heads/torn"), b"").expect("torn ref");
    assert!(graphref::lookup_in(&w, "").is_err(), "torn ref");
    std::fs::remove_file(repo.0.join(".git/refs/heads/torn")).expect("rm");
    // An absent branch with a descendant ref is still unborn.
    repo.git(&["update-ref", "refs/heads/torn/child", GRAPH]);
    assert!(matches!(
        graphref::lookup_in(&w, "").expect("unborn"),
        Lookup::Absent(Absent::Unborn)
    ));
    // HEAD naming a missing tag is broken, not unborn.
    std::fs::write(&head, "ref: refs/tags/missing\n").expect("HEAD");
    assert!(graphref::lookup_in(&w, "").is_err(), "non-branch target");
    // An absolute root is refused.
    assert_eq!(
        graphref::lookup_in(&repo.0, "/missing")
            .expect_err("absolute")
            .code,
        "graph.config_invalid"
    );
    // A committed link where a folder belongs is an error.
    let code = repo.0.join("code");
    std::fs::create_dir(&code).expect("code");
    let out = git_command(&code)
        .args(["init", "-q"])
        .output()
        .expect("init");
    assert!(out.status.success());
    std::os::unix::fs::symlink("elsewhere", code.join(".mdkg")).expect("link");
    let out = git_command(&code).args(["add", "."]).output().expect("add");
    assert!(out.status.success());
    let out = git_command(&code)
        .args(["-c", "user.name=Graphref Test"])
        .args(["-c", "user.email=graphref@example.invalid"])
        .args(["commit", "-qm", "link"])
        .output()
        .expect("commit");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(graphref::lookup_in(&code, "").is_err(), "committed link");
}

/// Opus #8729, Terra #8754 and #8516: in checkout mode a partial `.root`
/// record refuses only lookups at its own root, also in `lookup_in`.
#[test]
fn lookup_scopes_a_partial_record_to_its_root() {
    use graphref::{Absent, Lookup};
    let repo = Repo::new();
    repo.git(&["config", "--unset", "mdkg.graph.ref"]);
    repo.git(&["config", "--unset", &format!("mdkg.graph.{GRAPH_ID}.ref")]);
    repo.git(&["config", &format!("mdkg.graph.{OTHER_ID}.root"), "sub"]);
    // The repository root (HEAD commits no graph there) is plainly absent.
    assert!(matches!(
        graphref::lookup_in(&repo.0, "").expect("root"),
        Lookup::Absent(Absent::NoGraphConfig)
    ));
    // The partial record's own root refuses.
    assert!(graphref::lookup_in(&repo.0, "sub").is_err());
}

/// orchestrator #8573 and Terra #8551/#8754/#8686: an explicit binding
/// always wins over the legacy fallback, a modern `.ref`-only record isn't
/// legacy, and the HEAD fallback refuses an id bound elsewhere.
#[test]
fn explicit_bindings_win_over_the_legacy_fallback() {
    // Ref mode: C explicitly bound to `sub`, legacy B's config placed there.
    let repo = Repo::new();
    record_nested(&repo);
    let third = "0192f3a0-0000-7000-8000-0000000000aa";
    let writer = graph_writer(&repo.0)
        .with_paths(&[".mdkg".into(), "sub/.mdkg".into()])
        .expect("paths");
    writer
        .write(&Request {
            id: "legacy-at-bound".into(),
            base: Some(writer.snapshot().expect("tip")),
            changes: vec![put(
                "sub/.mdkg/config.json",
                format!(r#"{{"schema_version":3,"format_version":"0.7","graph_id":"{OTHER_ID}"}}"#)
                    .as_bytes(),
            )],
            message: "nested".into(),
            trailers: vec![],
        })
        .expect("seed");
    // Swap: OTHER_ID becomes legacy (`.worktree` only); `third` takes `sub`.
    repo.git(&["config", "--unset", &format!("mdkg.graph.{OTHER_ID}.ref")]);
    repo.git(&["config", "--unset", &format!("mdkg.graph.{OTHER_ID}.root")]);
    repo.git(&["worktree", "add", "-q", "--detach", "x", GRAPH]);
    repo.git(&["config", &format!("mdkg.graph.{OTHER_ID}.worktree"), "x"]);
    repo.git(&["config", &format!("mdkg.graph.{third}.ref"), "mdkg-graph"]);
    repo.git(&["config", &format!("mdkg.graph.{third}.root"), "sub"]);
    assert_eq!(
        graphref::authority_in(&repo.0, "sub")
            .expect_err("explicit binding wins")
            .code,
        "graph.identity_mismatch"
    );
    // Checkout mode, same records: the legacy record isn't a candidate.
    repo.git(&["config", "--unset", "mdkg.graph.ref"]);
    repo.git(&["config", &format!("mdkg.graph.{third}.worktree"), "x"]);
    assert!(graphref::authority_in(&repo.0, "sub").is_err());

    // The HEAD fallback refuses an id explicitly bound to another root.
    let repo = Repo::new();
    repo.git(&["config", "--unset", "mdkg.graph.ref"]);
    repo.git(&[
        "config",
        &format!("mdkg.graph.{GRAPH_ID}.root"),
        "elsewhere",
    ]);
    repo.git(&["worktree", "add", "-q", "--detach", "h", GRAPH]);
    // With its `.ref` record, the clone used a graph ref and has no graph
    // checkout: no HEAD is read at all (#16207).
    let err = graphref::authority(&repo.0.join("h")).expect_err("no checkout");
    assert_eq!(err.code, "graph.checkout_missing", "{}", err.message);
    // Without it, the HEAD fallback still refuses the id bound elsewhere.
    repo.git(&["config", "--unset", &format!("mdkg.graph.{GRAPH_ID}.ref")]);
    let err = graphref::authority(&repo.0.join("h")).expect_err("bound elsewhere");
    assert_eq!(err.code, "graph.identity_mismatch", "{}", err.message);
}

/// #16207: the checkout-mode guard fires only for a clone with graph
/// identity records. A plain checkout-mode clone that never used ref mode
/// (no `mdkg.graph.<id>.ref`, no `.worktree`) still reads the invoking
/// checkout's HEAD, as 0.7.0 did; the same clone with a `.ref` record
/// refuses with graph.checkout_missing.
#[test]
fn only_a_clone_that_used_a_graph_ref_refuses_the_head_fallback() {
    let repo = Repo::new();
    repo.git(&["config", "--unset", "mdkg.graph.ref"]);
    repo.git(&["config", "--unset", &format!("mdkg.graph.{GRAPH_ID}.ref")]);
    repo.git(&["worktree", "add", "-q", "--detach", "h", GRAPH]);
    let found = graphref::authority(&repo.0.join("h")).expect("plain checkout mode");
    assert!(
        found.source.starts_with("checkout:HEAD@"),
        "{}",
        found.source
    );
    assert_eq!(found.graph_id, GRAPH_ID);
    assert!(found.graph_ref.is_none());
    repo.git(&[
        "config",
        &format!("mdkg.graph.{GRAPH_ID}.ref"),
        "mdkg-graph",
    ]);
    let err = graphref::authority(&repo.0.join("h")).expect_err("used a graph ref");
    assert_eq!(err.code, "graph.checkout_missing", "{}", err.message);
}

/// orchestrator #8608: three cases for a candidate's graph id.
#[test]
fn candidate_ids_legacy_skip_malformed_refuse() {
    let repo = Repo::new();
    repo.git(&["config", "--unset", "mdkg.graph.ref"]);
    repo.git(&["config", "--unset", &format!("mdkg.graph.{GRAPH_ID}.ref")]);
    repo.git(&["worktree", "add", "-q", "--detach", "a", GRAPH]);
    repo.git(&["worktree", "add", "-q", "--detach", "b", GRAPH]);
    let commit = |dir: &Path, config: &str| {
        std::fs::write(dir.join(".mdkg/config.json"), config).expect("config");
        let out = git_command(dir).args(["add", "."]).output().expect("add");
        assert!(out.status.success());
        let out = git_command(dir)
            .args(["commit", "-qm", "config"])
            .output()
            .expect("commit");
        assert!(out.status.success());
    };
    // a: a pre-migration branch, a 0.6-shaped config with no graph_id,
    // under a 0.7.0 legacy record: no candidate, never a refusal.
    commit(&repo.0.join("a"), r#"{"schema_version":2}"#);
    repo.git(&["config", &format!("mdkg.graph.{OTHER_ID}.worktree"), "a"]);
    repo.git(&["config", &format!("mdkg.graph.{GRAPH_ID}.worktree"), "b"]);
    assert_eq!(
        graphref::authority(&repo.0).expect("b chosen").graph_id,
        GRAPH_ID
    );
    // a's id present but malformed: fails closed.
    commit(
        &repo.0.join("a"),
        r#"{"schema_version":3,"format_version":"0.7","graph_id":"not-a-uuid"}"#,
    );
    assert_eq!(
        graphref::authority(&repo.0).expect_err("malformed id").code,
        "graph.config_invalid"
    );
    // a's config unreadable: fails closed.
    commit(&repo.0.join("a"), "{not json");
    assert!(graphref::authority(&repo.0).is_err());
}

/// orchestrator #8573, Opus #8328, #8345, Terra #8377 and #8754: creation
/// is fenced by its intent, completion fails closed, a failed creation can
/// resume, a creator stays in its clone, and a bound root is refused.
#[test]
fn creation_is_fenced_and_resumable() {
    let repo = Repo::new();
    repo.git(&["update-ref", "-d", GRAPH]);
    repo.git(&["config", "--unset", &format!("mdkg.graph.{GRAPH_ID}.ref")]);
    let graph = graphref::resolve(&repo.0)
        .expect("resolve")
        .expect("ref mode");
    let request = |id: &str| Request {
        id: id.into(),
        base: None,
        changes: vec![put(
            ".mdkg/config.json",
            format!(r#"{{"schema_version":3,"format_version":"0.7","graph_id":"{GRAPH_ID}"}}"#)
                .as_bytes(),
        )],
        message: "init".into(),
        trailers: vec![],
    };
    // An abandoned creation's writer never publishes.
    let creator = graph
        .creator(&repo.0, LockMode::Local, GRAPH_ID, "")
        .expect("creator");
    assert_eq!(
        graph.pending_creation().expect("pending"),
        Some((GRAPH_ID.to_owned(), String::new(), ".mdkg".to_owned()))
    );
    graph.abandon_creation(&repo.0).expect("abandon");
    assert!(creator.write(&request("fenced")).is_err());
    assert!(!repo.0.join(".git/refs/heads/mdkg-graph").exists());
    // A failed creation resumes with its pending id.
    let creator = graph
        .creator(&repo.0, LockMode::Local, GRAPH_ID, "")
        .expect("creator");
    drop(creator);
    let (id, root, dir) = graph.pending_creation().expect("pending").expect("some");
    let creator = graph
        .creator_dir(&repo.0, LockMode::Local, &id, &root, &dir)
        .expect("resumed");
    creator.write(&request("resumed")).expect("create");
    graph.complete_creation(&repo.0).expect("complete");
    // Completion with no intent and an unrecorded graph fails closed.
    repo.git(&["config", "--unset", &format!("mdkg.graph.{GRAPH_ID}.ref")]);
    let graph = graphref::resolve(&repo.0)
        .expect("resolve")
        .expect("ref mode");
    assert_eq!(
        graph
            .complete_creation(&repo.0)
            .expect_err("unrecorded")
            .code,
        "graph.identity_unrecorded"
    );
    // A creator in another clone is refused.
    let other = Repo::new();
    assert!(
        graph
            .creator(&other.0, LockMode::Local, OTHER_ID, "sub")
            .is_err()
    );
}

/// orchestrator #8234: the upstream guard sees inline config inherited
/// through GIT_CONFIG_COUNT.
#[test]
fn an_upstream_set_inline_through_the_environment_is_refused() {
    let repo = Repo::new();
    let out = Command::new(std::env::current_exe().expect("test binary"))
        .args(["--exact", "inherited_upstream_child", "--test-threads=1"])
        .env("MDKG_GRAPHREF_IDENTITY", &repo.0)
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "branch.mdkg-graph.remote")
        .env("GIT_CONFIG_VALUE_0", "origin")
        .output()
        .expect("child");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
}

/// Terra #8551: a graph readable only through its 0.7.0 record is kept by
/// writers: a root writer can't delete or replace its config.
#[test]
fn writers_keep_a_0_7_0_nested_graph() {
    let repo = Repo::new();
    record_nested(&repo);
    let writer = graph_writer(&repo.0)
        .with_paths(&[".mdkg".into(), "sub/.mdkg".into()])
        .expect("paths");
    writer
        .write(&Request {
            id: "legacy-seed".into(),
            base: Some(writer.snapshot().expect("tip")),
            changes: vec![put(
                "sub/.mdkg/config.json",
                format!(r#"{{"schema_version":3,"format_version":"0.7","graph_id":"{OTHER_ID}"}}"#)
                    .as_bytes(),
            )],
            message: "nested".into(),
            trailers: vec![],
        })
        .expect("seed");
    // As 0.7.0 recorded it: `.worktree` only.
    repo.git(&["config", "--unset", &format!("mdkg.graph.{OTHER_ID}.ref")]);
    repo.git(&["config", "--unset", &format!("mdkg.graph.{OTHER_ID}.root")]);
    repo.git(&["config", &format!("mdkg.graph.{OTHER_ID}.worktree"), "x"]);
    let writer = graph_writer(&repo.0)
        .with_paths(&[".mdkg".into(), "sub/.mdkg".into()])
        .expect("paths");
    let err = writer
        .write(&Request {
            id: "legacy-delete".into(),
            base: Some(writer.snapshot().expect("tip")),
            changes: vec![Change::Delete {
                path: "sub/.mdkg/config.json".into(),
            }],
            message: "delete".into(),
            trailers: vec![],
        })
        .expect_err("the 0.7.0 graph is kept");
    assert_eq!(err.code(), "graph.identity_mismatch", "{err}");
}

/// orchestrator #8321: a 0.7.0 workspace whose graph folder isn't `.mdkg`
/// is found through `mdkg.graph.<id>.dir`, in ref mode and checkout mode,
/// and a writer bound to it writes its own folder.
#[test]
fn a_custom_graph_folder_is_found_in_both_modes() {
    let repo = Repo::new();
    let root = "packages/custom";
    repo.git(&[
        "config",
        &format!("mdkg.graph.{OTHER_ID}.ref"),
        "mdkg-graph",
    ]);
    repo.git(&["config", &format!("mdkg.graph.{OTHER_ID}.root"), root]);
    repo.git(&[
        "config",
        &format!("mdkg.graph.{OTHER_ID}.dir"),
        ".knowledge",
    ]);
    let writer = graph_writer(&repo.0)
        .with_paths(&[
            ".mdkg".into(),
            format!("{root}/.knowledge"),
            format!("{root}/.mdkg"),
        ])
        .expect("paths");
    writer
        .write(&Request {
            id: "custom-seed".into(),
            base: Some(writer.snapshot().expect("tip")),
            changes: vec![
                put(
                    &format!("{root}/.knowledge/config.json"),
                    format!(
                        r#"{{"schema_version":3,"format_version":"0.7","graph_id":"{OTHER_ID}"}}"#
                    )
                    .as_bytes(),
                ),
                put(
                    &format!("{root}/.knowledge/agents/w.json"),
                    br#"{"role":"worker"}"#,
                ),
            ],
            message: "custom".into(),
            trailers: vec![],
        })
        .expect("seed");
    // Ref mode.
    let found = graphref::authority_in(&repo.0, root).expect("ref mode");
    assert_eq!(found.graph_id, OTHER_ID);
    assert!(found.definitions.contains_key("w"));
    let graph = graphref::resolve(&repo.0)
        .expect("resolve")
        .expect("ref mode");
    let nested = graph
        .writer_in(&repo.0, LockMode::Local, root)
        .expect("nested writer");
    nested
        .write(&Request {
            id: "custom-own".into(),
            base: Some(nested.snapshot().expect("tip")),
            changes: vec![put(&format!("{root}/.knowledge/design/n.md"), b"n")],
            message: "own".into(),
            trailers: vec![],
        })
        .expect("its own folder");
    // Checkout mode: one recorded checkout holds both graphs.
    repo.git(&["config", "--unset", "mdkg.graph.ref"]);
    repo.git(&["config", "--unset", &format!("mdkg.graph.{GRAPH_ID}.ref")]);
    repo.git(&["config", "--unset", &format!("mdkg.graph.{OTHER_ID}.ref")]);
    repo.git(&["worktree", "add", "-q", "--detach", "x", GRAPH]);
    repo.git(&["config", &format!("mdkg.graph.{GRAPH_ID}.worktree"), "x"]);
    repo.git(&["config", &format!("mdkg.graph.{OTHER_ID}.worktree"), "x"]);
    assert_eq!(
        graphref::authority_in(&repo.0, root)
            .expect("checkout mode")
            .graph_id,
        OTHER_ID
    );
    assert!(matches!(
        graphref::lookup_in(&repo.0, root).expect("lookup"),
        graphref::Lookup::Found(_)
    ));
}

/// Opus #9270: two graphs bound to one root with different `.dir` records
/// refuse, naming both, rather than one winning by key order.
#[test]
fn conflicting_graph_folders_at_one_root_refuse() {
    let repo = Repo::new();
    let third = "0192f3a0-0000-7000-8000-0000000000aa";
    for (id, dir) in [(OTHER_ID, ".graph-a"), (third, ".graph-b")] {
        repo.git(&["config", &format!("mdkg.graph.{id}.ref"), "mdkg-graph"]);
        repo.git(&["config", &format!("mdkg.graph.{id}.root"), "sub"]);
        repo.git(&["config", &format!("mdkg.graph.{id}.dir"), dir]);
    }
    let err = graphref::authority_in(&repo.0, "sub").expect_err("conflict");
    assert_eq!(err.code, "graph.identity_mismatch");
    assert!(
        err.message.contains(".graph-a") && err.message.contains(".graph-b"),
        "{}",
        err.message
    );
    assert!(graphref::lookup_in(&repo.0, "sub").is_err());
}

/// Terra #9366: bindings, folders and creation identity.
#[test]
fn terra_9366_bindings_folders_and_creation() {
    // A writer refuses once a `.dir` record changes after it opened.
    let repo = Repo::new();
    let writer = graph_writer(&repo.0);
    repo.git(&[
        "config",
        &format!("mdkg.graph.{GRAPH_ID}.dir"),
        ".knowledge",
    ]);
    assert!(
        writer
            .write(&Request {
                id: "dir-changed".into(),
                base: Some(writer.snapshot().expect("tip")),
                changes: vec![put(".mdkg/design/d.md", b"d")],
                message: "d".into(),
                trailers: vec![],
            })
            .is_err()
    );
    repo.git(&["config", "--unset", &format!("mdkg.graph.{GRAPH_ID}.dir")]);
    // ... and once a 0.7.0 record is added.
    let writer = graph_writer(&repo.0);
    repo.git(&["config", &format!("mdkg.graph.{OTHER_ID}.worktree"), "x"]);
    assert!(
        writer
            .write(&Request {
                id: "legacy-added".into(),
                base: Some(writer.snapshot().expect("tip")),
                changes: vec![put(".mdkg/design/l.md", b"l")],
                message: "l".into(),
                trailers: vec![],
            })
            .is_err()
    );

    // A `.dir` record whose `.ref` names another graph ref isn't used.
    let repo = Repo::new();
    let third = "0192f3a0-0000-7000-8000-0000000000aa";
    repo.git(&[
        "config",
        &format!("mdkg.graph.{third}.ref"),
        "refs/heads/old-graph",
    ]);
    repo.git(&["config", &format!("mdkg.graph.{third}.dir"), ".old"]);
    assert_eq!(
        graphref::authority(&repo.0).expect("active graph").graph_id,
        GRAPH_ID
    );

    // A partial `.root` at `sub` blocks a 0.7.0 graph there in ref mode.
    let repo = Repo::new();
    record_nested(&repo);
    let writer = graph_writer(&repo.0)
        .with_paths(&[".mdkg".into(), "sub/.mdkg".into()])
        .expect("paths");
    writer
        .write(&Request {
            id: "partial-seed".into(),
            base: Some(writer.snapshot().expect("tip")),
            changes: vec![put(
                "sub/.mdkg/config.json",
                format!(r#"{{"schema_version":3,"format_version":"0.7","graph_id":"{OTHER_ID}"}}"#)
                    .as_bytes(),
            )],
            message: "nested".into(),
            trailers: vec![],
        })
        .expect("seed");
    repo.git(&["config", "--unset", &format!("mdkg.graph.{OTHER_ID}.ref")]);
    repo.git(&["config", "--unset", &format!("mdkg.graph.{OTHER_ID}.root")]);
    repo.git(&["config", &format!("mdkg.graph.{OTHER_ID}.worktree"), "x"]);
    repo.git(&["config", &format!("mdkg.graph.{third}.root"), "sub"]);
    assert!(graphref::authority_in(&repo.0, "sub").is_err());

    // Creation: the folder is part of its identity; recovery stays in its
    // clone; a folder name the reader rejects is refused up front.
    let repo = Repo::new();
    repo.git(&["update-ref", "-d", GRAPH]);
    repo.git(&["config", "--unset", &format!("mdkg.graph.{GRAPH_ID}.ref")]);
    let graph = graphref::resolve(&repo.0)
        .expect("resolve")
        .expect("ref mode");
    assert!(
        graph
            .creator_dir(&repo.0, LockMode::Local, GRAPH_ID, "", "a:graph")
            .is_err()
    );
    graph
        .creator_dir(&repo.0, LockMode::Local, GRAPH_ID, "", ".old")
        .expect("creator");
    assert!(
        graph
            .creator_dir(&repo.0, LockMode::Local, GRAPH_ID, "", ".new")
            .is_err(),
        "another folder is another creation"
    );
    let other = Repo::new();
    assert!(graph.complete_creation(&other.0).is_err());
    assert!(graph.abandon_creation(&other.0).is_err());
    assert!(repo.0.join(".git/mdkg/graph/creating.json").exists());
}

fn node(n: u8, title: &str) -> Vec<u8> {
    let stamp = "2026-10-24T18:03:11.482Z";
    format!(
        "---\nid: task-0192f3a0-0000-7000-8000-0000000000b{n}\ntype: task\ntitle: {title}\nstatus: todo\ncreated: {stamp}\nupdated: {stamp}\n---\nBody.\n"
    )
    .into_bytes()
}

/// Terra #9366: the committed reader and strict admission read a custom
/// graph folder through their `_dir` variants.
#[test]
fn the_committed_reader_reads_a_custom_graph_folder() {
    let repo = Repo::new();
    let root = "packages/custom";
    repo.git(&[
        "config",
        &format!("mdkg.graph.{OTHER_ID}.ref"),
        "mdkg-graph",
    ]);
    repo.git(&["config", &format!("mdkg.graph.{OTHER_ID}.root"), root]);
    repo.git(&[
        "config",
        &format!("mdkg.graph.{OTHER_ID}.dir"),
        ".knowledge",
    ]);
    let writer = graph_writer(&repo.0)
        .with_paths(&[
            ".mdkg".into(),
            format!("{root}/.knowledge"),
            format!("{root}/.mdkg"),
        ])
        .expect("paths");
    let tip =
        writer
            .write(&Request {
                id: "custom-reader".into(),
                base: Some(writer.snapshot().expect("tip")),
                changes: vec![
                put(
                    &format!("{root}/.knowledge/config.json"),
                    format!(
                        r#"{{"schema_version":3,"format_version":"0.7","graph_id":"{OTHER_ID}"}}"#
                    )
                    .as_bytes(),
                ),
                // Nodes in the graph's folder and in an unrelated `.mdkg`
                // beside it (Terra #10216): only the first are read.
                put(&format!("{root}/.knowledge/work/task-1.md"), &node(1, "Kept")),
                put(&format!("{root}/.mdkg/work/task-2.md"), &node(2, "Unrelated")),
            ],
                message: "custom".into(),
                trailers: vec![],
            })
            .expect("seed")
            .commit;
    let snapshot = mdkg_git::with_worktree_trust(&mdkg_git::WorktreeTrust::default(), || {
        mdkg_graph::committed::load_at_dir(&repo.0, &tip, root, ".knowledge")
    })
    .expect("load_at_dir");
    let titles: Vec<&str> = snapshot
        .index
        .nodes
        .iter()
        .map(|n| n.title.as_str())
        .collect();
    assert_eq!(titles, ["Kept"]);
    // The graph's paths are its own folder, not an implicit `.mdkg`.
    assert_eq!(
        graphref::graph_paths_dir(&snapshot.config, ".knowledge").expect("paths"),
        [".knowledge"]
    );
    assert_eq!(
        graphref::graph_paths_dir(&snapshot.config, ".mdkg/custom").expect("nested"),
        [".mdkg/custom"]
    );
    mdkg_git::with_worktree_trust(&mdkg_git::WorktreeTrust::default(), || {
        mdkg_graph::committed::load_strict_dir_with(
            &repo.0,
            &tip,
            root,
            ".knowledge",
            OTHER_ID,
            &|_, _| Ok(()),
        )
    })
    .expect("strict admission");
}

/// orchestrator #9719 (Terra #9705, Opus #9671): a missing or empty `.dir`
/// is `.mdkg` when folders are compared; a writer refuses while any root
/// has conflicting folders; an unborn lookup sees the conflict too.
#[test]
fn effective_folders_conflict_everywhere_they_matter() {
    let third = "0192f3a0-0000-7000-8000-0000000000aa";
    // A default-folder record (no `.dir`, or an empty one) and an explicit
    // `.dir` at one root conflict.
    for empty in [None, Some(""), Some(".")] {
        let repo = Repo::new();
        repo.git(&[
            "config",
            &format!("mdkg.graph.{OTHER_ID}.ref"),
            "mdkg-graph",
        ]);
        repo.git(&["config", &format!("mdkg.graph.{OTHER_ID}.root"), "sub"]);
        if let Some(value) = empty {
            repo.git(&["config", &format!("mdkg.graph.{OTHER_ID}.dir"), value]);
        }
        repo.git(&["config", &format!("mdkg.graph.{third}.ref"), "mdkg-graph"]);
        repo.git(&["config", &format!("mdkg.graph.{third}.root"), "sub"]);
        repo.git(&["config", &format!("mdkg.graph.{third}.dir"), ".foo"]);
        let err = graphref::authority_in(&repo.0, "sub").expect_err("conflict");
        assert!(
            err.message.contains(".mdkg") && err.message.contains(".foo"),
            "{}",
            err.message
        );
        assert!(
            err.message.contains("git config --remove-section"),
            "{}",
            err.message
        );
        // A writer at the repository root refuses too: the conflict is at
        // another root it could reach.
        let graph = graphref::resolve(&repo.0)
            .expect("resolve")
            .expect("ref mode");
        let refused = graph
            .writer(&repo.0, LockMode::Local)
            .expect_err("writer refused");
        assert_eq!(refused.code, "graph.identity_mismatch", "{empty:?}");
    }
    // An unborn checkout-mode lookup refuses the conflict rather than
    // reporting absence.
    let repo = Repo::new();
    repo.git(&["config", "--unset", "mdkg.graph.ref"]);
    repo.git(&["config", &format!("mdkg.graph.{third}.ref"), "mdkg-graph"]);
    repo.git(&["config", &format!("mdkg.graph.{third}.dir"), ".foo"]);
    orphan_worktree(&repo, "u");
    assert_eq!(
        graphref::lookup_in(&repo.0.join("u"), "")
            .expect_err("conflict")
            .code,
        "graph.identity_mismatch"
    );
}

/// Terra #10216: a creator whose intent was replaced by one for the same
/// id, root and ref in another folder never publishes; a writer refuses
/// once another id's partial `.root` record gives its root another folder.
#[test]
fn writers_are_fenced_by_folders_recorded_after_they_open() {
    let repo = Repo::new();
    repo.git(&["update-ref", "-d", GRAPH]);
    repo.git(&["config", "--unset", &format!("mdkg.graph.{GRAPH_ID}.ref")]);
    let graph = graphref::resolve(&repo.0)
        .expect("resolve")
        .expect("ref mode");
    let old = graph
        .creator_dir(&repo.0, LockMode::Local, GRAPH_ID, "", ".old")
        .expect("creator");
    graph.abandon_creation(&repo.0).expect("abandon");
    graph
        .creator_dir(&repo.0, LockMode::Local, GRAPH_ID, "", ".new")
        .expect("replacement");
    let err = old
        .write(&Request {
            id: "init-old".into(),
            base: None,
            changes: vec![put(
                ".old/config.json",
                format!(r#"{{"schema_version":3,"format_version":"0.7","graph_id":"{GRAPH_ID}"}}"#)
                    .as_bytes(),
            )],
            message: "init".into(),
            trailers: vec![],
        })
        .expect_err("the replaced creator is fenced");
    assert_eq!(err.code(), "graph.identity_mismatch", "{err}");
    assert!(
        !git_command(&repo.0)
            .args(["rev-parse", "--verify", "--quiet", GRAPH])
            .status()
            .expect("git")
            .success(),
        "nothing published"
    );

    // A partial `.root` record, added after the writer opened, giving the
    // root another folder.
    let repo = Repo::new();
    let writer = graph_writer(&repo.0);
    repo.git(&["config", &format!("mdkg.graph.{OTHER_ID}.root"), "."]);
    repo.git(&[
        "config",
        &format!("mdkg.graph.{OTHER_ID}.dir"),
        ".knowledge",
    ]);
    let err = writer
        .write(&Request {
            id: "late-folder".into(),
            base: Some(writer.snapshot().expect("tip")),
            changes: vec![put(".mdkg/design/late.md", b"late\n")],
            message: "late".into(),
            trailers: vec![],
        })
        .expect_err("the root's folder changed");
    assert_eq!(err.code(), "graph.identity_mismatch", "{err}");
}
