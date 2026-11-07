//! graph.remote sync (edd-01a18867, section 4) between two clones and
//! local bare repositories: no network, file paths only. Each clone sets
//! its own `core.hooksPath`, so no hook of the user's runs.
use std::path::{Path, PathBuf};
use std::process::Command;

use mdkg_format::Uuid7;
use mdkg_graph::graphref::{self, GraphRef};
use mdkg_graph::refwrite::{Change, LockMode, Request};
use mdkg_graph::sync::guard::{ScanResult, Scanner};
use mdkg_graph::sync::{self, Approval, Guards, Options, Who, net};

const GRAPH_ID: &str = "0192f3a0-0000-7000-8000-0000000000aa";
const GRAPH: &str = "refs/heads/mdkg-graph";
/// What the test scanner treats as a credential: assembled, so no
/// credential-shaped string is in the source.
const MARKER: &str = concat!("PLANTED", "-", "SECRET");

struct Dir(PathBuf);
impl Drop for Dir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

fn git_in(dir: &Path, args: &[&str]) -> std::process::Output {
    let mut cmd = Command::new("git");
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            cmd.env_remove(key);
        }
    }
    cmd.current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .args(["-c", "core.hooksPath=/dev/null"])
        .args(args)
        .output()
        .expect("git")
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = git_in(dir, args);
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf8").trim().into()
}

/// A fixture: a root folder holding the code remote `code.git`, the graph
/// remote `graphs.git`, and clones.
struct World {
    root: Dir,
}

struct Clone {
    dir: PathBuf,
}

impl World {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("mdkg-sync-{}", Uuid7::now()));
        std::fs::create_dir_all(&root).expect("root");
        let root = Dir(root.canonicalize().expect("canonical"));
        for bare in ["code.git", "graphs.git", "other.git"] {
            git(&root.0, &["init", "-q", "--bare", "-b", "trunk", bare]);
        }
        Self { root }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.0.join(name)
    }

    /// The first clone: code on `trunk` (pushed to `code.git`), and a
    /// graph-only graph on `mdkg-graph` holding `.mdkg/config.json` and
    /// `.mdkg/notes.txt`. `origin` is the code remote and `mine` the
    /// graph remote.
    fn first(&self, name: &str) -> Clone {
        let dir = self.path(name);
        std::fs::create_dir_all(&dir).expect("clone");
        let c = Clone { dir };
        c.init();
        c.write("src.txt", "code\n");
        c.git(&["add", "."]);
        c.git(&["commit", "-qm", "code"]);
        c.git(&["push", "-q", "origin", "trunk"]);
        c.git(&["checkout", "-q", "--orphan", "seed"]);
        c.git(&["rm", "-rfq", "--cached", "."]);
        std::fs::remove_file(c.dir.join("src.txt")).expect("clear");
        c.write(
            ".mdkg/config.json",
            &format!(r#"{{"schema_version":3,"format_version":"0.7","graph_id":"{GRAPH_ID}"}}"#),
        );
        c.write(".mdkg/notes.txt", "one\ntwo\nthree\n");
        c.git(&["add", "."]);
        c.git(&["commit", "-qm", "graph"]);
        c.git(&["update-ref", GRAPH, "HEAD"]);
        c.git(&["checkout", "-qf", "trunk"]);
        c.git(&["branch", "-D", "-q", "seed"]);
        c.git(&["clean", "-fdxq"]);
        c.configure();
        c
    }

    /// Another clone of the same code and graph, its graph ref taken from
    /// `graphs.git`.
    fn second(&self, name: &str) -> Clone {
        let dir = self.path(name);
        std::fs::create_dir_all(&dir).expect("clone");
        let c = Clone { dir };
        c.init();
        c.git(&["fetch", "-q", "origin", "trunk"]);
        c.git(&["checkout", "-qf", "-B", "trunk", "FETCH_HEAD"]);
        c.git(&["fetch", "-q", "mine", &format!("{GRAPH}:{GRAPH}")]);
        c.configure();
        c
    }
}

impl Clone {
    fn init(&self) {
        git(&self.dir, &["init", "-q", "-b", "trunk"]);
        let world = self.dir.parent().expect("world");
        for (k, v) in [
            ("user.name", "Sync Test".to_owned()),
            ("user.email", "sync@example.invalid".to_owned()),
            ("commit.gpgsign", "false".to_owned()),
            // No hook of the user's: this clone's own folder.
            (
                "core.hooksPath",
                self.dir.join(".git/test-hooks").display().to_string(),
            ),
        ] {
            self.git(&["config", k, &v]);
        }
        std::fs::create_dir_all(self.dir.join(".git/test-hooks")).expect("hooks");
        self.git(&[
            "remote",
            "add",
            "origin",
            &world.join("code.git").display().to_string(),
        ]);
        self.git(&[
            "remote",
            "add",
            "mine",
            &world.join("graphs.git").display().to_string(),
        ]);
    }

    fn configure(&self) {
        self.git(&["config", "mdkg.graph.ref", "mdkg-graph"]);
        self.git(&[
            "config",
            &format!("mdkg.graph.{GRAPH_ID}.ref"),
            "mdkg-graph",
        ]);
        self.git(&["config", "mdkg.mainRef", "trunk"]);
        self.git(&["config", "mdkg.graph.remote", "mine"]);
    }

    fn git(&self, args: &[&str]) -> String {
        git(&self.dir, args)
    }

    fn write(&self, path: &str, text: &str) {
        let path = self.dir.join(path);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, text).expect("write");
    }

    fn graph(&self) -> GraphRef {
        graphref::resolve(&self.dir)
            .expect("resolve")
            .expect("ref mode")
    }

    fn tip(&self) -> String {
        self.git(&["rev-parse", GRAPH])
    }

    /// Publishes `text` at `path` on the graph ref.
    fn put(&self, path: &str, text: &str) -> String {
        self.change(
            vec![Change::Put {
                path: path.into(),
                bytes: text.as_bytes().to_vec(),
            }],
            &format!("put {path}"),
        )
    }

    /// One commit of `changes` on the local graph.
    fn change(&self, changes: Vec<Change>, message: &str) -> String {
        let writer = self
            .graph()
            .writer(&self.dir, LockMode::Local)
            .expect("writer");
        let base = writer.snapshot().expect("snapshot");
        writer
            .write(&Request {
                id: format!("t-{}", Uuid7::now()),
                base: Some(base),
                changes,
                message: message.into(),
                trailers: vec![],
            })
            .expect("write")
            .commit
    }

    fn approve(&self) {
        sync::approve(&self.dir, &self.graph(), &[]).expect("approve");
    }

    fn sync(&self, who: Who, push: bool) -> Result<sync::Outcome, graphref::Error> {
        self.sync_with(who, push, Some(&Marker), false)
    }

    fn sync_with(
        &self,
        who: Who,
        push: bool,
        scanner: Option<&dyn Scanner>,
        accept_rewrite: bool,
    ) -> Result<sync::Outcome, graphref::Error> {
        let graph = self.graph();
        let net = net::Git::new(&graph.common, &self.dir);
        sync::run(
            &self.dir,
            &graph,
            &net,
            &Guards {
                protected: &[],
                scanner,
                definitions: &|_, _| Ok(()),
                replay: None,
            },
            Options {
                who,
                push,
                accept_rewrite,
            },
        )
    }
}

fn remote_tip(world: &World, bare: &str) -> Option<String> {
    let out = git_in(
        &world.path(bare),
        &["rev-parse", "--verify", "--quiet", GRAPH],
    );
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Flags any outgoing commit that adds a line holding [`MARKER`], against
/// its first parent, as the real scanner reads what a commit adds.
struct Marker;
impl Scanner for Marker {
    fn scan(&self, common: &Path, commits: &[String]) -> Result<ScanResult, String> {
        for c in commits {
            let out = git_in(
                common,
                &[
                    "--git-dir",
                    &common.display().to_string(),
                    "diff-tree",
                    "-r",
                    "-p",
                    "--root",
                    "-m",
                    "--first-parent",
                    "--format=",
                    c,
                ],
            );
            let text = String::from_utf8_lossy(&out.stdout);
            let mut path = "";
            for line in text.lines() {
                if let Some(p) = line.strip_prefix("+++ b/") {
                    path = p;
                } else if line.starts_with('+') && line.contains(MARKER) {
                    return Ok(ScanResult {
                        finding: Some((c.clone(), path.to_owned())),
                        unscanned: vec![],
                    });
                }
            }
        }
        Ok(ScanResult::default())
    }
}

#[test]
fn an_unapproved_remote_never_pushes() {
    let world = World::new();
    let a = world.first("a");
    a.put(".mdkg/notes.txt", "changed\n");
    assert_eq!(
        sync::approval(&a.dir, &a.graph()).expect("approval"),
        Approval::Unapproved
    );
    let e = a.sync(Who::Launcher, true).expect_err("launcher");
    assert_eq!(e.code, "graph.remote_unapproved");
    // A person's sync neither pulls nor pushes either (DECISION 11).
    let e = a.sync(Who::Person, true).expect_err("person");
    assert_eq!(e.code, "graph.remote_unapproved");
    assert_eq!(remote_tip(&world, "graphs.git"), None);
}

#[test]
fn an_approved_remote_pushes_and_another_clone_pulls() {
    let world = World::new();
    let a = world.first("a");
    a.approve();
    let out = a.sync(Who::Launcher, true).expect("first push");
    assert!(out.pushed);
    assert_eq!(remote_tip(&world, "graphs.git"), Some(a.tip()));
    // Only remoteRef crossed: no code branch, no tag.
    assert_eq!(
        git(
            &world.path("graphs.git"),
            &["for-each-ref", "--format=%(refname)"]
        ),
        GRAPH
    );

    let b = world.second("b");
    b.approve();
    let theirs = b.put(".mdkg/b.txt", "from b\n");
    assert!(b.sync(Who::Person, true).expect("b pushes").pushed);
    let out = a.sync(Who::Launcher, false).expect("a pulls");
    assert_eq!(out.after, theirs);
    assert_eq!(a.tip(), theirs, "a fast-forward");
    assert!(!out.pushed && !out.ahead);
}

#[test]
fn a_changed_destination_is_refused_until_approved_again() {
    let world = World::new();
    let a = world.first("a");
    a.approve();
    a.git(&[
        "remote",
        "set-url",
        "mine",
        &world.path("other.git").display().to_string(),
    ]);
    let e = a.sync(Who::Person, true).expect_err("changed");
    assert_eq!(e.code, "graph.remote_changed");
    assert_eq!(remote_tip(&world, "other.git"), None);
    // A pushurl alone moving is a change too.
    a.approve();
    a.git(&[
        "remote",
        "set-url",
        "--push",
        "mine",
        &world.path("graphs.git").display().to_string(),
    ]);
    assert_eq!(
        a.sync(Who::Launcher, true).expect_err("push url").code,
        "graph.remote_changed"
    );
    a.git(&["config", "--unset", "remote.mine.pushurl"]);
    // And another remoteRef.
    a.git(&["config", "mdkg.graph.remoteRef", "graphs/repo"]);
    assert_eq!(
        a.sync(Who::Launcher, true).expect_err("ref").code,
        "graph.remote_changed"
    );
    a.approve();
    assert!(a.sync(Who::Launcher, true).expect("approved again").pushed);
    assert_eq!(remote_tip(&world, "other.git"), None);
}

#[test]
fn a_changed_git_setup_is_refused_until_approved_again() {
    // DECISION 10 (2026-11-04): the agent-writable git setup a push
    // runs with is pinned, so changing it is a changed destination.
    let world = World::new();
    let a = world.first("a");
    a.approve();
    a.git(&["config", "core.sshCommand", "ssh -o SetupMarker=1"]);
    let e = a.sync(Who::Launcher, true).expect_err("ssh command");
    assert_eq!(e.code, "graph.remote_changed");
    assert!(e.message.contains("core.sshcommand"), "{}", e.message);
    assert!(!e.message.contains("SetupMarker"), "{}", e.message);
    assert_eq!(remote_tip(&world, "graphs.git"), None);
    a.approve();
    for (key, value) in [
        ("remote.mine.receivepack", "git-receive-pack"),
        ("credential.helper", "cache"),
        ("hook.audit.command", "true"),
    ] {
        a.git(&["config", key, value]);
        let e = a.sync(Who::Launcher, true).expect_err(key);
        assert_eq!(e.code, "graph.remote_changed", "{key}");
        a.approve();
    }
    // The pre-push hook git would run: added, then edited.
    let hook = a.dir.join(".git/test-hooks/pre-push");
    std::fs::create_dir_all(hook.parent().expect("hooks")).expect("hooks");
    std::fs::write(&hook, "#!/bin/sh\nexit 0\n").expect("hook");
    let mode = |m| {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(m)).expect("chmod");
    };
    mode(0o755);
    let e = a.sync(Who::Launcher, true).expect_err("hook added");
    assert!(e.message.contains("pre-push"), "{}", e.message);
    a.approve();
    std::fs::write(&hook, "#!/bin/sh\necho changed\nexit 0\n").expect("hook");
    assert_eq!(
        a.sync(Who::Launcher, true).expect_err("hook edited").code,
        "graph.remote_changed"
    );
    a.approve();
    mode(0o644);
    assert_eq!(
        a.sync(Who::Launcher, true).expect_err("hook mode").code,
        "graph.remote_changed"
    );
    // Terra #18946: an owner bit alone is a change too (0655 vs 0755).
    // As root, access(X_OK) passes on any execute bit (Terra #19334), so
    // git would still run it: no change there.
    mode(0o755);
    a.approve();
    mode(0o655);
    if rustix::process::geteuid().is_root() {
        // Pull-only, so the push the end of this test expects is still due
        // (Terra #19704).
        a.sync(Who::Launcher, false)
            .expect("root runs it either way");
    } else {
        assert_eq!(
            a.sync(Who::Launcher, true).expect_err("owner bit").code,
            "graph.remote_changed"
        );
    }
    mode(0o755);
    a.approve();
    // Two values are not one value holding a newline.
    a.git(&["config", "--unset-all", "core.sshCommand"]);
    a.git(&["config", "--add", "core.sshCommand", "ssh -o First=1"]);
    a.git(&["config", "--add", "core.sshCommand", "ssh"]);
    a.approve();
    a.git(&["config", "--unset-all", "core.sshCommand"]);
    a.git(&["config", "core.sshCommand", "ssh -o First=1\nssh"]);
    assert_eq!(
        a.sync(Who::Launcher, true).expect_err("joined").code,
        "graph.remote_changed"
    );
    a.git(&["config", "--unset-all", "core.sshCommand"]);
    a.approve();
    // DECISION 16 (A), Terra #19005: the order between keys is pinned. An
    // empty `credential.helper` after a URL's helper resets it; before it,
    // the helper runs.
    let config = a.dir.join(".git/config");
    let original = std::fs::read_to_string(&config).expect("config");
    let with = |extra: &str| {
        std::fs::write(&config, format!("{original}{extra}")).expect("config");
    };
    with("[credential \"https://example.invalid\"]\n\thelper = store\n[credential]\n\thelper =\n");
    a.approve();
    with("[credential]\n\thelper =\n[credential \"https://example.invalid\"]\n\thelper = store\n");
    let e = a.sync(Who::Launcher, true).expect_err("reordered");
    assert_eq!(e.code, "graph.remote_changed");
    assert!(
        e.message.contains("the order of these settings"),
        "{}",
        e.message
    );
    // A key with no value isn't one with an empty value.
    with("[credential]\n\tuseHttpPath\n");
    a.approve();
    with("[credential]\n\tuseHttpPath =\n");
    assert_eq!(
        a.sync(Who::Launcher, true).expect_err("bare vs empty").code,
        "graph.remote_changed"
    );
    std::fs::write(&config, &original).expect("config");
    a.approve();
    // DECISION 16 (B): the hook is pinned by git's own access(X_OK), so an
    // ACL that denies execute without touching the mode is a change.
    #[cfg(target_os = "macos")]
    {
        let me = Command::new("id").arg("-un").output().expect("id");
        let me = String::from_utf8_lossy(&me.stdout).trim().to_owned();
        let acl = |op: &str| {
            let ok = Command::new("chmod")
                .args([op, &format!("user:{me} deny execute")])
                .arg(&hook)
                .status()
                .expect("chmod")
                .success();
            assert!(ok, "chmod {op}");
        };
        acl("+a");
        let e = a.sync(Who::Launcher, true).expect_err("acl");
        assert_eq!(e.code, "graph.remote_changed");
        assert!(e.message.contains("pre-push"), "{}", e.message);
        acl("-a");
        a.approve();
    }
    // A URL in a key is never printed: only its digest.
    a.git(&[
        "config",
        "credential.https://userpart@example.invalid.helper",
        "cache",
    ]);
    let e = a.sync(Who::Launcher, true).expect_err("credential key");
    assert_eq!(e.code, "graph.remote_changed");
    assert!(!e.message.contains("userpart"), "{}", e.message);
    assert!(e.message.contains("credential.<"), "{}", e.message);
    a.git(&[
        "config",
        "--remove-section",
        "credential.https://userpart@example.invalid",
    ]);
    // A hooks path git can't expand is refused, never pinned as text.
    unresolvable_hooks_path_is_refused(&a);
    a.git(&[
        "config",
        "core.hooksPath",
        &a.dir.join(".git/test-hooks").display().to_string(),
    ]);
    a.approve();
    // A pin from before setup was pinned matches no current setup.
    let pin = a.dir.join(".git/mdkg/graph/remote.json");
    let mut v: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&pin).expect("pin")).expect("json");
    v.as_object_mut().expect("object").remove("setup");
    std::fs::write(&pin, serde_json::to_vec(&v).expect("json")).expect("pin");
    assert_eq!(
        a.sync(Who::Launcher, true).expect_err("old pin").code,
        "graph.remote_changed"
    );
    a.approve();
    assert!(a.sync(Who::Launcher, true).expect("approved again").pushed);
}

#[test]
fn an_unresolvable_hooks_path_cannot_be_approved() {
    let world = World::new();
    let a = world.first("a");
    unresolvable_hooks_path_is_refused(&a);
}

/// Sets `core.hooksPath` to a `~user` path no account has and checks that
/// approving refuses it and pins nothing. Some git versions (2.43, as on
/// Ubuntu) die in every command such a checkout runs, so approving fails
/// at its first git call (`graph.git`, with git's message naming the
/// path); others (Apple's 2.50) read it, and the hooks guard refuses it
/// (`graph.remote_invalid`). The setting stays set for the caller.
fn unresolvable_hooks_path_is_refused(a: &Clone) {
    const HOOKS: &str = "~nosuchuser-mdkg-test/hooks";
    let graph = a.graph();
    let pin = a.dir.join(".git/mdkg/graph/remote.json");
    let pinned = std::fs::read(&pin).ok();
    a.git(&["config", "core.hooksPath", HOOKS]);
    let error = sync::approve(&a.dir, &graph, &[]).expect_err("unresolvable");
    let git_refuses = !git_in(&a.dir, &["rev-parse", "--git-dir"]).status.success();
    if git_refuses {
        assert_eq!(error.code, "graph.git", "{}", error.message);
        assert!(error.message.contains(HOOKS), "{}", error.message);
    } else {
        assert_eq!(error.code, "graph.remote_invalid", "{}", error.message);
    }
    assert_eq!(std::fs::read(&pin).ok(), pinned, "nothing is pinned");
}

#[test]
fn a_code_remote_as_the_destination_is_refused() {
    let world = World::new();
    let a = world.first("a");
    // remoteRef naming a code branch.
    a.git(&["config", "mdkg.graph.remoteRef", "trunk"]);
    assert_eq!(
        sync::approve(&a.dir, &a.graph(), &[])
            .expect_err("trunk")
            .code,
        "graph.remote_is_code"
    );
    a.git(&["config", "mdkg.graph.remoteRef", "release"]);
    assert_eq!(
        sync::approve(&a.dir, &a.graph(), &["release".into()])
            .expect_err("protected")
            .code,
        "graph.remote_is_code"
    );
    a.git(&["config", "--unset", "mdkg.graph.remoteRef"]);
    // A graph remote that is the company's remote by another name.
    a.git(&[
        "remote",
        "add",
        "company",
        &world.path("graphs.git").display().to_string(),
    ]);
    assert_eq!(
        sync::approve(&a.dir, &a.graph(), &[])
            .expect_err("shared")
            .code,
        "graph.remote_is_code"
    );
    // Never approved, so sync stops before any network use (DECISION 11).
    assert_eq!(
        a.sync(Who::Person, true).expect_err("sync").code,
        "graph.remote_unapproved"
    );
    assert_eq!(remote_tip(&world, "graphs.git"), None);
}

#[test]
fn a_graph_remote_with_separate_fetch_and_push_repositories_is_refused() {
    let world = World::new();
    let a = world.first("a");
    a.git(&[
        "remote",
        "set-url",
        "--push",
        "mine",
        &world.path("other.git").display().to_string(),
    ]);
    let e = sync::approve(&a.dir, &a.graph(), &[]).expect_err("split");
    assert_eq!(e.code, "graph.remote_invalid", "{}", e.message);
    assert!(e.message.contains("other"), "{}", e.message);
    assert_eq!(
        a.sync(Who::Launcher, true).expect_err("launcher").code,
        "graph.remote_unapproved"
    );
    assert_eq!(
        a.sync(Who::Person, true).expect_err("person").code,
        "graph.remote_unapproved"
    );
    // Two push URLs (git pushes to each) are refused too.
    a.git(&[
        "remote",
        "set-url",
        "--add",
        "--push",
        "mine",
        &world.path("graphs.git").display().to_string(),
    ]);
    assert_eq!(
        sync::approve(&a.dir, &a.graph(), &[])
            .expect_err("two")
            .code,
        "graph.remote_invalid"
    );
    assert_eq!(remote_tip(&world, "other.git"), None);
    assert_eq!(remote_tip(&world, "graphs.git"), None);
}

#[test]
fn a_remote_that_only_fetches_from_the_code_repository_is_refused() {
    let world = World::new();
    let a = world.first("a");
    // The code remote fetches code.git and pushes to other.git; the graph
    // remote names code.git, which is the code's but not where the code
    // is pushed: no full-shape exception, and it shares a destination.
    a.git(&[
        "remote",
        "set-url",
        "--push",
        "origin",
        &world.path("other.git").display().to_string(),
    ]);
    a.git(&[
        "remote",
        "set-url",
        "mine",
        &world.path("code.git").display().to_string(),
    ]);
    assert_eq!(
        sync::approve(&a.dir, &a.graph(), &[])
            .expect_err("shared")
            .code,
        "graph.remote_is_code"
    );
    // Never approved, so sync stops before any network use (DECISION 11).
    assert_eq!(
        a.sync(Who::Person, true).expect_err("sync").code,
        "graph.remote_unapproved"
    );
    assert_eq!(remote_tip(&world, "code.git"), None);
}

#[test]
fn a_remote_ref_related_to_the_code_is_refused() {
    let world = World::new();
    let a = world.first("a");
    a.approve();
    // The private remote's graph ref holds code history.
    let trunk = a.git(&["rev-parse", "trunk"]);
    a.git(&["push", "-q", "mine", &format!("{trunk}:{GRAPH}")]);
    assert_eq!(
        a.sync(Who::Launcher, true).expect_err("related").code,
        "graph.remote_is_code"
    );
}

#[test]
fn full_shape_history_is_refused_on_a_private_remote() {
    let world = World::new();
    let a = world.first("a");
    a.approve();
    // A graph commit that also holds a code path.
    let tip = a.tip();
    let tree = a.git(&["rev-parse", &format!("{tip}^{{tree}}")]);
    let blob = a.git(&["rev-parse", "trunk:src.txt"]);
    let listing = format!(
        "{}\n100644 blob {blob}\tsrc.txt\n",
        a.git(&["ls-tree", &tree])
    );
    let out = Command::new("git")
        .current_dir(&a.dir)
        .args(["mktree"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut c| {
            use std::io::Write as _;
            c.stdin
                .take()
                .expect("stdin")
                .write_all(listing.as_bytes())?;
            c.wait_with_output()
        })
        .expect("mktree");
    let full = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    let commit = a.git(&["commit-tree", &full, "-p", &tip, "-m", "full"]);
    a.git(&["update-ref", GRAPH, &commit, &tip]);
    assert_eq!(
        a.sync(Who::Launcher, true).expect_err("full").code,
        "graph.shape"
    );
    assert_eq!(remote_tip(&world, "graphs.git"), None);
}

#[test]
fn a_personal_first_push_skips_what_the_code_remote_already_holds() {
    // Personal setup: the graph syncs with the code's own remote, and the
    // graph ref is full shape (code history in it). The code's history
    // holds a secret-shaped fixture the code remote already has: the first
    // graph push neither re-scans nor re-sizes it (design review, Opus 1).
    let world = World::new();
    let a = world.first("a");
    a.write("fixture.txt", &format!("key {MARKER}\n"));
    a.git(&["add", "fixture.txt"]);
    a.git(&["commit", "-qm", "fixture"]);
    a.git(&["push", "-q", "origin", "trunk"]);
    // A full-shape graph: trunk's tree plus the graph folder, cut from
    // trunk (G20).
    let tip = a.tip();
    let mdkg = a.git(&["rev-parse", &format!("{tip}:.mdkg")]);
    let listing = format!(
        "{}\n040000 tree {mdkg}\t.mdkg\n",
        a.git(&["ls-tree", "trunk"])
    );
    let out = Command::new("git")
        .current_dir(&a.dir)
        .args(["mktree"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut c| {
            use std::io::Write as _;
            c.stdin
                .take()
                .expect("stdin")
                .write_all(listing.as_bytes())?;
            c.wait_with_output()
        })
        .expect("mktree");
    let tree = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    let full = a.git(&["commit-tree", &tree, "-p", "trunk", "-m", "full"]);
    a.git(&["update-ref", GRAPH, &full, &tip]);
    a.git(&["config", "mdkg.graph.remote", "origin"]);
    a.approve();
    let out = a.sync(Who::Person, true).expect("first push");
    assert!(out.pushed);
    assert_eq!(remote_tip(&world, "code.git"), Some(a.tip()));
    // A secret the code remote doesn't hold is still found.
    let planted = a.put(".mdkg/leak.txt", &format!("key {MARKER}\n"));
    let e = a.sync(Who::Person, true).expect_err("secret");
    assert_eq!(e.code, "graph.secret", "{}", e.message);
    assert!(e.message.contains(&planted[..12]), "{}", e.message);
}

#[test]
fn a_tracking_ref_the_destination_lacks_is_still_scanned() {
    // Terra #18297: a tracking ref may record another URL's history (a
    // fork fetching upstream) or be stale. Only what the destination
    // itself reports bounds the outgoing set.
    let world = World::new();
    let a = world.first("a");
    a.write("fixture.txt", &format!("key {MARKER}\n"));
    a.git(&["add", "fixture.txt"]);
    a.git(&["commit", "-qm", "fixture"]);
    // Recorded as origin's, never pushed there.
    a.git(&["update-ref", "refs/remotes/origin/trunk", "trunk"]);
    let tip = a.tip();
    let mdkg = a.git(&["rev-parse", &format!("{tip}:.mdkg")]);
    let listing = format!(
        "{}\n040000 tree {mdkg}\t.mdkg\n",
        a.git(&["ls-tree", "trunk"])
    );
    let out = Command::new("git")
        .current_dir(&a.dir)
        .args(["mktree"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut c| {
            use std::io::Write as _;
            c.stdin
                .take()
                .expect("stdin")
                .write_all(listing.as_bytes())?;
            c.wait_with_output()
        })
        .expect("mktree");
    let tree = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    let full = a.git(&["commit-tree", &tree, "-p", "trunk", "-m", "full"]);
    a.git(&["update-ref", GRAPH, &full, &tip]);
    a.git(&["config", "mdkg.graph.remote", "origin"]);
    a.approve();
    let e = a.sync(Who::Person, true).expect_err("unscanned history");
    assert_eq!(e.code, "graph.secret", "{}", e.message);
    assert_eq!(remote_tip(&world, "code.git"), None);
}

/// The real runner, which before its first push moves the destination as
/// another device would: `before_push` runs once.
struct Moving<'a> {
    inner: net::Git,
    before_push: std::cell::Cell<Option<Box<dyn FnOnce() + 'a>>>,
}

impl sync::net::Net for Moving<'_> {
    fn remote_tip(
        &self,
        remote: &str,
        remote_ref: &str,
    ) -> Result<Option<String>, graphref::Error> {
        self.inner.remote_tip(remote, remote_ref)
    }
    fn heads(&self, remote: &str) -> Result<Vec<String>, graphref::Error> {
        self.inner.heads(remote)
    }
    fn fetch(&self, remote: &str, remote_ref: &str, into: &str) -> Result<String, graphref::Error> {
        self.inner.fetch(remote, remote_ref, into)
    }
    fn push(
        &self,
        remote: &str,
        commit: &str,
        remote_ref: &str,
        expect: Option<&str>,
    ) -> Result<net::Pushed, graphref::Error> {
        if let Some(f) = self.before_push.take() {
            f();
        }
        self.inner.push(remote, commit, remote_ref, expect)
    }
}

#[test]
fn a_url_changed_only_by_trailing_space_is_refused() {
    // Terra #18470: `<path> ` is a different folder to git; it must not
    // read as the approved `<path>`.
    let world = World::new();
    let a = world.first("a");
    a.approve();
    let spaced = format!("{} ", world.path("graphs.git").display());
    git(&world.path(""), &["init", "-q", "--bare", &spaced]);
    a.git(&["remote", "set-url", "mine", &spaced]);
    let e = a.sync(Who::Launcher, true).expect_err("ambiguous URL");
    assert_eq!(e.code, "graph.remote_invalid", "{}", e.message);
    assert_eq!(git(Path::new(&spaced), &["for-each-ref"]), "");
    assert_eq!(remote_tip(&world, "graphs.git"), None);
}

#[test]
fn a_url_or_push_url_ending_in_a_newline_is_refused() {
    // Terra #18503: git keeps the newline, so `<path>\n` is another
    // folder; dropping the empty line made it read as the approved path.
    for key in ["remote.mine.url", "remote.mine.pushurl"] {
        let world = World::new();
        let a = world.first("a");
        a.approve();
        let other = format!("{}\n", world.path("graphs.git").display());
        git(&world.path(""), &["init", "-q", "--bare", &other]);
        a.git(&["config", key, &other]);
        let e = a.sync(Who::Launcher, true).expect_err("ambiguous URL");
        assert_eq!(e.code, "graph.remote_invalid", "{key}: {}", e.message);
        assert_eq!(git(Path::new(&other), &["for-each-ref"]), "", "{key}");
        assert_eq!(remote_tip(&world, "graphs.git"), None, "{key}");
    }
}

#[test]
fn a_local_path_that_isnt_utf8_is_refused() {
    // Terra #18503: `g\x80.git` and `g\x81.git` are two repositories, but
    // a lossy conversion made them one pin.
    use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
    let world = World::new();
    let a = world.first("a");
    let odd = |b: u8| {
        let mut bytes = world.path("g").into_os_string().into_vec();
        bytes.extend_from_slice(&[b, b'.', b'g', b'i', b't']);
        PathBuf::from(std::ffi::OsStr::from_bytes(&bytes))
    };
    let first = odd(0x80);
    // Created where the file system allows it (macOS refuses such names);
    // the setting alone is refused either way.
    Command::new("git")
        .args(["init", "-q", "--bare"])
        .arg(&first)
        .output()
        .expect("git");
    let status = Command::new("git")
        .current_dir(&a.dir)
        .args(["config", "remote.mine.url"])
        .arg(&first)
        .status()
        .expect("git");
    assert!(status.success());
    let e = sync::approve(&a.dir, &a.graph(), &[]).expect_err("not UTF-8");
    assert_eq!(e.code, "graph.remote_invalid", "{}", e.message);
    // Never approved, so sync stops before any network use (DECISION 11).
    assert_eq!(
        a.sync(Who::Person, true).expect_err("not UTF-8").code,
        "graph.remote_unapproved"
    );
}

#[test]
fn a_rewrite_prefix_need_not_be_a_whole_url() {
    // Terra #18527: `[url "file://localhost"] insteadOf = cache:` is a
    // valid prefix rewrite; only the URL git prints after it is checked
    // as a URL. An unrelated rewrite doesn't block sync either.
    let world = World::new();
    let a = world.first("a");
    a.git(&["config", "url.file://localhost.insteadOf", "cache:"]);
    a.git(&[
        "config",
        "url.https://example.invalid/.insteadOf",
        "elsewhere:",
    ]);
    a.git(&[
        "remote",
        "set-url",
        "mine",
        &format!("cache:{}", world.path("graphs.git").display()),
    ]);
    a.approve();
    assert!(a.sync(Who::Person, true).expect("push").pushed);
    assert_eq!(remote_tip(&world, "graphs.git"), Some(a.tip()));
}

#[test]
fn file_urls_whose_escapes_decode_alike_are_refused() {
    // Terra #18602: `%+0` and `%00` are distinct literal paths to git but
    // decoded alike (to NUL); both are refused, so neither can pass as
    // the other's pin.
    // `%+1` and `%01` decode alike without the hex-digit rule but aren't
    // NUL, so they test that rule alone (Terra #18617); the reversed order
    // has a valid fetch URL and an invalid push URL.
    for (fetch, push) in [
        ("%+0", "%00"),
        ("%00", "%00"),
        ("%+0", "%+0"),
        ("%+1", "%01"),
        ("%01", "%+1"),
    ] {
        let world = World::new();
        let a = world.first("a");
        a.git(&["config", "url.file://localhost.insteadOf", "cache:"]);
        let at = |esc: &str| format!("cache:{}/repo{esc}.git", world.path("").display());
        a.git(&["config", "remote.mine.url", &at(fetch)]);
        a.git(&["config", "remote.mine.pushurl", &at(push)]);
        let e = sync::approve(&a.dir, &a.graph(), &[]).expect_err("malformed escape");
        assert_eq!(
            e.code, "graph.remote_invalid",
            "{fetch} {push}: {}",
            e.message
        );
        // Refused for the escape itself, not by the later one-destination
        // check (Terra #18624), and the same through `current`.
        assert!(
            e.message.contains("malformed %-escape"),
            "{fetch} {push}: {}",
            e.message
        );
        let e = sync::current(&a.dir, &a.graph()).expect_err("malformed escape");
        assert!(
            e.message.contains("malformed %-escape"),
            "{fetch} {push}: {}",
            e.message
        );
    }
}

#[cfg(target_os = "linux")]
#[test]
fn links_to_two_non_utf8_repositories_are_not_one_pin() {
    // Terra #18527: plain paths aren't percent-decoded, and one resolver
    // serves the check and the pin, so `read%2egit` and `write%2egit`
    // linking to `g\x80.git` and `g\x81.git` can't fold into one pin.
    use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
    let world = World::new();
    let a = world.first("a");
    let odd = |b: u8| {
        let mut bytes = world.path("g").into_os_string().into_vec();
        bytes.extend_from_slice(&[b, b'.', b'g', b'i', b't']);
        PathBuf::from(std::ffi::OsStr::from_bytes(&bytes))
    };
    for (b, link) in [(0x80, "read%2egit"), (0x81, "write%2egit")] {
        let target = odd(b);
        let status = Command::new("git")
            .args(["init", "-q", "--bare"])
            .arg(&target)
            .status()
            .expect("git");
        assert!(status.success());
        std::os::unix::fs::symlink(&target, world.path(link)).expect("link");
    }
    a.git(&[
        "config",
        "remote.mine.url",
        &world.path("read%2egit").display().to_string(),
    ]);
    a.git(&[
        "config",
        "remote.mine.pushurl",
        &world.path("write%2egit").display().to_string(),
    ]);
    let e = sync::approve(&a.dir, &a.graph(), &[]).expect_err("not UTF-8");
    assert_eq!(e.code, "graph.remote_invalid", "{}", e.message);
}

/// A runner that fails the test on any network use.
struct NoNet;

impl sync::net::Net for NoNet {
    fn remote_tip(&self, _: &str, _: &str) -> Result<Option<String>, graphref::Error> {
        panic!("remote_tip on an unapproved remote")
    }
    fn heads(&self, _: &str) -> Result<Vec<String>, graphref::Error> {
        panic!("heads on an unapproved remote")
    }
    fn fetch(&self, _: &str, _: &str, _: &str) -> Result<String, graphref::Error> {
        panic!("fetch from an unapproved remote")
    }
    fn push(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: Option<&str>,
    ) -> Result<net::Pushed, graphref::Error> {
        panic!("push to an unapproved remote")
    }
}

#[test]
fn an_unapproved_remote_is_never_contacted() {
    // Terra #18946: neither caller makes any network call (DECISIONS 4
    // and 11), not only no push.
    let world = World::new();
    let a = world.first("a");
    a.put(".mdkg/notes.txt", "changed\n");
    let graph = a.graph();
    for who in [Who::Person, Who::Launcher] {
        let e = sync::run(
            &a.dir,
            &graph,
            &NoNet,
            &Guards {
                protected: &[],
                scanner: Some(&Marker),
                definitions: &|_, _| Ok(()),
                replay: None,
            },
            Options {
                who,
                push: true,
                accept_rewrite: false,
            },
        )
        .expect_err("unapproved");
        assert_eq!(e.code, "graph.remote_unapproved", "{who:?}");
    }
}

/// The real runner, counting `heads` calls.
struct Counting {
    inner: net::Git,
    heads: std::cell::Cell<u32>,
}

impl sync::net::Net for Counting {
    fn remote_tip(
        &self,
        remote: &str,
        remote_ref: &str,
    ) -> Result<Option<String>, graphref::Error> {
        self.inner.remote_tip(remote, remote_ref)
    }
    fn heads(&self, remote: &str) -> Result<Vec<String>, graphref::Error> {
        self.heads.set(self.heads.get() + 1);
        self.inner.heads(remote)
    }
    fn fetch(&self, remote: &str, remote_ref: &str, into: &str) -> Result<String, graphref::Error> {
        self.inner.fetch(remote, remote_ref, into)
    }
    fn push(
        &self,
        remote: &str,
        commit: &str,
        remote_ref: &str,
        expect: Option<&str>,
    ) -> Result<net::Pushed, graphref::Error> {
        self.inner.push(remote, commit, remote_ref, expect)
    }
}

#[test]
fn only_a_sync_that_pushes_asks_for_the_destinations_heads() {
    // Terra #18368, #18470: pull-only and up-to-date syncs make no
    // `ls-remote` of every branch.
    let world = World::new();
    let a = world.first("a");
    a.git(&["config", "mdkg.graph.remote", "origin"]);
    a.approve();
    let graph = a.graph();
    let counting = Counting {
        inner: net::Git::new(&graph.common, &a.dir),
        heads: std::cell::Cell::new(0),
    };
    let run = |push: bool| {
        sync::run(
            &a.dir,
            &graph,
            &counting,
            &Guards {
                protected: &[],
                scanner: Some(&Marker),
                definitions: &|_, _| Ok(()),
                replay: None,
            },
            Options {
                who: Who::Person,
                push,
                accept_rewrite: false,
            },
        )
        .expect("sync")
    };
    assert!(!run(false).pushed);
    assert_eq!(counting.heads.get(), 0, "a pull-only sync");
    assert!(run(true).pushed);
    assert_eq!(counting.heads.get(), 1, "the push asked once");
    assert!(!run(true).pushed);
    assert_eq!(counting.heads.get(), 1, "an up-to-date sync");
}

#[test]
fn a_retry_reads_the_destinations_heads_again() {
    // Terra #18368: a head the first attempt saw is deleted before its push
    // (whose lease then fails); the retry must not keep trusting it.
    let world = World::new();
    let a = world.first("a");
    let c0 = a.git(&["rev-parse", "trunk"]);
    a.write("fixture.txt", &format!("key {MARKER}\n"));
    a.git(&["add", "fixture.txt"]);
    a.git(&["commit", "-qm", "fixture"]);
    // The secret-bearing commit is on the destination's `topic` only.
    a.git(&["push", "-q", "origin", "trunk:refs/heads/topic"]);
    let tip = a.tip();
    let mdkg = a.git(&["rev-parse", &format!("{tip}:.mdkg")]);
    let listing = format!(
        "{}\n040000 tree {mdkg}\t.mdkg\n",
        a.git(&["ls-tree", "trunk"])
    );
    let out = Command::new("git")
        .current_dir(&a.dir)
        .args(["mktree"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut c| {
            use std::io::Write as _;
            c.stdin
                .take()
                .expect("stdin")
                .write_all(listing.as_bytes())?;
            c.wait_with_output()
        })
        .expect("mktree");
    let tree = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    let full = a.git(&["commit-tree", &tree, "-p", "trunk", "-m", "full"]);
    a.git(&["update-ref", GRAPH, &full, &tip]);
    a.git(&["config", "mdkg.graph.remote", "origin"]);
    a.approve();
    let code = world.path("code.git");
    let graph = a.graph();
    let moving = Moving {
        inner: net::Git::new(&graph.common, &a.dir),
        before_push: std::cell::Cell::new(Some(Box::new(|| {
            let bare = |args: &[&str]| {
                let mut all = vec!["--git-dir", code.to_str().expect("path")];
                all.extend_from_slice(args);
                git(&world.path(""), &all);
            };
            bare(&["update-ref", "-d", "refs/heads/topic"]);
            bare(&["update-ref", GRAPH, &c0]);
        }))),
    };
    let e = sync::run(
        &a.dir,
        &graph,
        &moving,
        &Guards {
            protected: &[],
            scanner: Some(&Marker),
            definitions: &|_, _| Ok(()),
            replay: None,
        },
        Options {
            who: Who::Person,
            push: true,
            accept_rewrite: false,
        },
    )
    .expect_err("the retry scans what the destination no longer holds");
    assert_eq!(e.code, "graph.secret", "{}", e.message);
    drop(moving);
    assert_eq!(remote_tip(&world, "code.git"), Some(c0));
}

#[test]
fn syncs_and_resolutions_take_turns_on_the_sync_lock() {
    // A timer's sync and a person's never interleave their metadata
    // writes (design review, Astra 2): one waits for the other's lock.
    let world = World::new();
    let a = world.first("a");
    a.approve();
    let held = sync::lock(&a.graph().common).expect("lock");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::scope(|s| {
        s.spawn(|| {
            let out = a.sync(Who::Person, true);
            tx.send(out.map(|o| o.pushed)).expect("send");
        });
        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(500))
                .is_err(),
            "the sync waited for the lock"
        );
        drop(held);
        let pushed = rx
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("the sync ran once the lock was free");
        assert!(pushed.expect("sync"));
    });
}

#[test]
fn a_pulled_remote_tip_is_accepted_before_the_local_graph_moves() {
    // Astra 3, Terra #18297: interrupted at the local CAS (a stale git ref
    // lock makes it fail), the remote tip is already durably accepted.
    let world = World::new();
    let a = world.first("a");
    a.approve();
    assert!(a.sync(Who::Person, true).expect("push").pushed);
    let b = world.second("b");
    b.approve();
    b.sync(Who::Person, true).expect("pull");
    b.put(".mdkg/notes.txt", "from b\n");
    assert!(b.sync(Who::Person, true).expect("push").pushed);
    let theirs = b.tip();
    let before = a.tip();
    let common = a.graph().common;
    let lock = common.join(format!("{GRAPH}.lock"));
    std::fs::write(&lock, "").expect("stale lock");
    a.sync(Who::Person, false).expect_err("the local CAS fails");
    assert_eq!(a.tip(), before, "the local graph didn't move");
    let state = sync::State::load(&common).expect("state");
    assert_eq!(state.accepted.as_deref(), Some(theirs.as_str()));
    std::fs::remove_file(&lock).expect("unlock");
    a.sync(Who::Person, false).expect("pull");
    assert_eq!(a.tip(), theirs);
}

#[test]
fn a_worktree_only_push_url_is_seen_by_the_guard() {
    // Terra #18297: a pushurl in a linked worktree's config.worktree is
    // where that worktree's push goes, so the guard reads it there too.
    let world = World::new();
    let a = world.first("a");
    a.approve();
    a.git(&["config", "extensions.worktreeConfig", "true"]);
    let linked = world.path("linked");
    a.git(&[
        "worktree",
        "add",
        "-q",
        "--detach",
        &linked.display().to_string(),
    ]);
    git(&world.path(""), &["init", "-q", "--bare", "other.git"]);
    git(
        &linked,
        &[
            "config",
            "--worktree",
            "remote.mine.pushurl",
            &world.path("other.git").display().to_string(),
        ],
    );
    let w = Clone { dir: linked };
    let e = w.sync(Who::Person, true).expect_err("another destination");
    assert!(
        ["graph.remote_invalid", "graph.remote_changed"].contains(&e.code),
        "{}: {}",
        e.code,
        e.message
    );
    assert_eq!(git(&world.path("other.git"), &["for-each-ref"]), "");
    assert_eq!(remote_tip(&world, "graphs.git"), None);
}

#[test]
fn a_planted_secret_is_refused_and_so_is_a_missing_scanner() {
    let world = World::new();
    let a = world.first("a");
    a.approve();
    a.put(".mdkg/notes.txt", "clean\n");
    assert_eq!(
        a.sync_with(Who::Person, true, None, false)
            .expect_err("no scanner")
            .code,
        "graph.secret_scan_unavailable"
    );
    let planted = a.put(".mdkg/leak.txt", &format!("key {MARKER}\n"));
    // Overwritten at the tip, still in the outgoing history.
    a.put(".mdkg/leak.txt", "gone\n");
    let e = a.sync(Who::Person, true).expect_err("secret");
    assert_eq!(e.code, "graph.secret");
    assert!(
        e.message.contains(&planted[..12]) && e.message.contains(".mdkg/leak.txt"),
        "{}",
        e.message
    );
    assert!(!e.message.contains(MARKER), "the match is never printed");
    assert_eq!(remote_tip(&world, "graphs.git"), None);
}

#[test]
fn a_replace_ref_hiding_a_secret_refuses_the_push() {
    let world = World::new();
    let a = world.first("a");
    a.approve();
    let planted = a.put(".mdkg/leak.txt", &format!("key {MARKER}\n"));
    let secret = a.git(&["rev-parse", &format!("{planted}:.mdkg/leak.txt")]);
    let harmless = a.git(&["rev-parse", &format!("{planted}:.mdkg/notes.txt")]);
    let _ = a.git(&["replace", &secret, &harmless]);
    let e = a.sync(Who::Person, true).expect_err("replaced");
    assert_eq!(e.code, "graph.secret_scan_unavailable", "{}", e.message);
    assert!(e.message.contains("refs/replace/"), "{}", e.message);
    assert_eq!(remote_tip(&world, "graphs.git"), None);
    // Without the replacement, the scan finds what the push would send.
    a.git(&["replace", "-d", &secret]);
    assert_eq!(
        a.sync(Who::Person, true).expect_err("secret").code,
        "graph.secret"
    );
    // A grafts file is refused the same way.
    let parent = a.git(&["rev-parse", &format!("{planted}^")]);
    std::fs::write(
        a.dir.join(".git/info/grafts"),
        format!("{planted} {parent}\n"),
    )
    .expect("grafts");
    assert_eq!(
        a.sync(Who::Person, true).expect_err("grafts").code,
        "graph.secret_scan_unavailable"
    );
    assert_eq!(remote_tip(&world, "graphs.git"), None);
}

#[test]
fn pending_intents_are_replayed_before_sync_moves_the_local_graph() {
    // R13 (Terra #18237): the caller's replay runs under the exclusive
    // lock before the local compare-and-swap; a failing replay stops the
    // sync with the local graph unmoved.
    let world = World::new();
    let a = world.first("a");
    a.approve();
    a.sync(Who::Person, true).expect("seed");
    let b = world.second("b");
    b.approve();
    a.put(".mdkg/notes.txt", "from a\n");
    a.sync(Who::Person, true).expect("a pushes");
    let run = |replay: &dyn Fn(
        &mdkg_graph::refwrite::RefWriter,
        &mdkg_graph::refwrite::GraphLock,
    ) -> Result<(), graphref::Error>| {
        let graph = b.graph();
        let net = sync::net::Git::new(&graph.common, &b.dir);
        sync::run(
            &b.dir,
            &graph,
            &net,
            &Guards {
                protected: &[],
                scanner: Some(&Marker),
                definitions: &|_, _| Ok(()),
                replay: Some(replay),
            },
            Options {
                who: Who::Person,
                push: true,
                accept_rewrite: false,
            },
        )
    };
    let before = b.tip();
    let e = run(&|_, _| Err(graphref::Error::new("graph.intents", "store busy")))
        .expect_err("replay failed");
    assert_eq!(e.code, "graph.intents");
    assert_eq!(b.tip(), before, "the local graph didn't move");
    let calls = std::cell::Cell::new(0);
    let out = run(&|writer, lock| {
        lock.require_exclusive(writer.common(), "test")
            .expect("exclusive");
        calls.set(calls.get() + 1);
        Ok(())
    })
    .expect("pulled");
    assert_eq!(calls.get(), 1);
    assert_eq!(out.after, a.tip());
}

#[test]
fn a_conflict_recorded_twice_for_one_merge_is_one_entry() {
    // Terra #19736: a sync that crashed after recording can make the same
    // merge again and append the same record; it lists and clears once.
    let world = World::new();
    let a = world.first("a");
    a.approve();
    a.sync(Who::Person, true).expect("seed");
    let b = world.second("b");
    b.approve();
    b.put(".mdkg/notes.txt", "one\nB\n");
    b.sync(Who::Person, true).expect("b pushes");
    a.put(".mdkg/notes.txt", "one\nA\n");
    let out = a.sync(Who::Launcher, true).expect("merge");
    assert_eq!(out.conflicts.len(), 1);
    let merge = a.tip();
    let common = a.graph().common;
    let file = common.join("mdkg/graph/conflicts.json");
    let mut v: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&file).expect("read")).expect("json");
    let entry = v["conflicts"][0].clone();
    v["conflicts"].as_array_mut().expect("list").push(entry);
    std::fs::write(&file, v.to_string()).expect("write");
    let listed = sync::conflicts(&common, Some(&merge)).expect("conflicts");
    assert_eq!(listed.len(), 1, "{listed:?}");
    let cleared = sync::clear_conflict(
        &common,
        &sync::lock(&common).expect("lock"),
        ".mdkg/notes.txt",
        &merge,
    )
    .expect("clear");
    assert_eq!(cleared, 1);
    assert!(
        sync::conflicts(&common, Some(&merge))
            .expect("none")
            .is_empty()
    );
}

#[test]
fn a_pruned_merge_s_entry_does_not_break_the_listing() {
    // Terra #19800: an entry recorded for a merge whose CAS lost, once git
    // prunes that commit, is skipped, not an error for every listing.
    let world = World::new();
    let a = world.first("a");
    a.approve();
    a.sync(Who::Person, true).expect("seed");
    let b = world.second("b");
    b.approve();
    b.put(".mdkg/notes.txt", "one\nB\n");
    b.sync(Who::Person, true).expect("b pushes");
    a.put(".mdkg/notes.txt", "one\nA\n");
    let out = a.sync(Who::Launcher, true).expect("merge");
    assert_eq!(out.conflicts.len(), 1);
    let merge = a.tip();
    let common = a.graph().common;
    let file = common.join("mdkg/graph/conflicts.json");
    let mut v: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&file).expect("read")).expect("json");
    let mut gone = v["conflicts"][0].clone();
    gone["merge"] = "b".repeat(40).into();
    gone["path"] = ".mdkg/other.txt".into();
    v["conflicts"].as_array_mut().expect("list").insert(0, gone);
    std::fs::write(&file, v.to_string()).expect("write");
    let listed = sync::conflicts(&common, Some(&merge)).expect("listing still works");
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0].merge, merge);
    // In a shallow clone a missing merge may be behind the boundary, so it
    // is an error, never silently dropped (Terra #19820).
    std::fs::write(common.join("shallow"), format!("{merge}\n")).expect("shallow");
    let e = sync::conflicts(&common, Some(&merge)).expect_err("shallow");
    assert_eq!(e.code, "graph.git");
    assert!(e.message.contains("shallow"), "{}", e.message);
}

#[test]
fn one_resolution_clears_all_its_entries_in_one_write() {
    // Terra #19565: `graph resolve a b` clears its selected entries
    // together, so no retry finds `a`'s older conflict selectable while
    // `b`'s is still open.
    let world = World::new();
    let a = world.first("a");
    a.approve();
    a.sync(Who::Person, true).expect("seed");
    let b = world.second("b");
    b.approve();
    b.put(".mdkg/notes.txt", "one\nB\n");
    b.sync(Who::Person, true).expect("b pushes");
    a.put(".mdkg/notes.txt", "one\nA\n");
    let first = a.sync(Who::Launcher, true).expect("first merge");
    assert_eq!(first.conflicts.len(), 1);
    let merge1 = a.tip();
    b.sync(Who::Person, true).expect("b pulls");
    b.put(".mdkg/notes.txt", "two\nB\n");
    b.put(".mdkg/x.txt", "x B\n");
    b.sync(Who::Person, true).expect("b pushes again");
    a.put(".mdkg/notes.txt", "two\nA\n");
    a.put(".mdkg/x.txt", "x A\n");
    let second = a.sync(Who::Launcher, true).expect("second merge");
    assert_eq!(second.conflicts.len(), 2, "{second:?}");
    let merge2 = a.tip();
    let common = a.graph().common;
    let cleared = sync::clear_conflicts(
        &common,
        &sync::lock(&common).expect("lock"),
        &[
            (".mdkg/notes.txt".to_owned(), merge2.clone()),
            (".mdkg/x.txt".to_owned(), merge2.clone()),
        ],
    )
    .expect("clear");
    assert_eq!(cleared, 2);
    let left = sync::conflicts(&common, Some(&merge2)).expect("conflicts");
    assert_eq!(left.len(), 1, "{left:?}");
    assert_eq!(
        (left[0].path.as_str(), left[0].merge.as_str()),
        (".mdkg/notes.txt", merge1.as_str())
    );
}

#[test]
fn a_conflict_takes_the_remote_version_and_is_recorded() {
    let world = World::new();
    let a = world.first("a");
    a.approve();
    a.sync(Who::Person, true).expect("seed");
    let b = world.second("b");
    b.approve();
    let theirs = b.put(".mdkg/notes.txt", "one\nB\nthree\n");
    b.put(".mdkg/b-only.txt", "b\n");
    b.sync(Who::Person, true).expect("b pushes");
    let ours = a.put(".mdkg/notes.txt", "one\nA\nthree\n");
    a.put(".mdkg/a-only.txt", "a\n");
    let out = a.sync(Who::Launcher, true).expect("a merges and pushes");
    assert!(out.pushed);
    assert_eq!(out.conflicts.len(), 1);
    let c = &out.conflicts[0];
    assert_eq!(c.path, ".mdkg/notes.txt");
    let merge = a.tip();
    assert_eq!(c.merge, merge);
    assert_eq!(remote_tip(&world, "graphs.git"), Some(merge.clone()));
    // The remote's version wins; both sides' other changes are kept; the
    // local version stays in history as the first parent.
    assert_eq!(
        a.git(&["show", &format!("{merge}:.mdkg/notes.txt")]),
        "one\nB\nthree"
    );
    assert_eq!(a.git(&["show", &format!("{merge}:.mdkg/a-only.txt")]), "a");
    assert_eq!(a.git(&["show", &format!("{merge}:.mdkg/b-only.txt")]), "b");
    assert_eq!(a.git(&["rev-parse", &format!("{merge}^2")]), b.tip());
    assert_eq!(
        a.git(&["rev-parse", &format!("{merge}^1:.mdkg/notes.txt")]),
        a.git(&["rev-parse", &format!("{ours}:.mdkg/notes.txt")])
    );
    let message = a.git(&["log", "-1", "--format=%B", &merge]);
    assert!(
        message.contains("Mdkg-Conflict: .mdkg/notes.txt local="),
        "{message}"
    );
    let _ = theirs;
    let recorded = sync::conflicts(&a.graph().common, Some(&merge)).expect("conflicts");
    assert_eq!(recorded, out.conflicts);
    // Resolution clears it; b pulls the merge as a fast-forward.
    assert_eq!(
        sync::clear_conflict(
            &a.graph().common,
            &sync::lock(&a.graph().common).expect("lock"),
            ".mdkg/notes.txt",
            &merge
        )
        .expect("clear"),
        1
    );
    assert!(
        sync::conflicts(&a.graph().common, Some(&merge))
            .expect("none")
            .is_empty()
    );
    assert_eq!(b.sync(Who::Launcher, true).expect("b pulls").after, merge);
}

#[test]
fn a_merge_only_the_remote_tree_makes_valid_records_every_local_change() {
    // DECISION 6 (2026-11-04): a rename on one side and an edit on
    // the other leave a duplicate id that neither the conflicted paths nor
    // the paths both sides changed fix; the third try takes the remote's
    // version of everything the local side changed.
    let id = "task-0192f3a4-0000-7000-8000-000000000001";
    let node = |title: &str| {
        format!(
            "---\nid: {id}\ntype: task\ntitle: {title}\nstatus: todo\ncreated: 2026-10-24\nupdated: 2026-10-24\n---\n"
        )
    };
    let world = World::new();
    let a = world.first("a");
    a.approve();
    a.put(&format!(".mdkg/work/{id}.md"), &node("One"));
    a.sync(Who::Person, true).expect("seed");
    let b = world.second("b");
    b.approve();
    b.put(&format!(".mdkg/work/{id}.md"), &node("Edited"));
    b.sync(Who::Person, true).expect("b pushes");
    // a moves the node to another file, keeping its id.
    a.change(
        vec![
            // Rewritten enough that git sees no rename.
            Change::Put {
                path: ".mdkg/work/moved.md".into(),
                bytes: format!("{}\n{}", node("Moved"), "A longer body.\n".repeat(40)).into_bytes(),
            },
            Change::Delete {
                path: format!(".mdkg/work/{id}.md"),
            },
        ],
        "move the node",
    );
    let out = a.sync(Who::Launcher, true).expect("the third try");
    let merge = a.tip();
    let paths: Vec<&str> = out.conflicts.iter().map(|c| c.path.as_str()).collect();
    assert!(paths.contains(&".mdkg/work/moved.md"), "{paths:?}");
    assert!(
        !git_in(
            &a.dir,
            &["cat-file", "-e", &format!("{merge}:.mdkg/work/moved.md")]
        )
        .status
        .success(),
        "the local copy is a recorded conflict, not a duplicate"
    );
    assert!(
        a.git(&["show", &format!("{merge}:.mdkg/work/{id}.md")])
            .contains("title: Edited")
    );
    assert_eq!(remote_tip(&world, "graphs.git"), Some(merge));
}

#[test]
fn resolving_the_newest_conflict_on_a_path_keeps_the_older_one() {
    let world = World::new();
    let a = world.first("a");
    a.approve();
    a.sync(Who::Person, true).expect("seed");
    let b = world.second("b");
    b.approve();
    let mut merges = Vec::new();
    for round in ["1", "2"] {
        b.put(".mdkg/notes.txt", &format!("one\nB{round}\nthree\n"));
        b.sync(Who::Person, true).expect("b pushes");
        a.put(".mdkg/notes.txt", &format!("one\nA{round}\nthree\n"));
        let out = a.sync(Who::Person, true).expect("a merges");
        assert_eq!(out.conflicts.len(), 1, "round {round}");
        merges.push(a.tip());
        b.sync(Who::Person, false).expect("b pulls the merge");
    }
    let common = a.graph().common;
    let listed = sync::conflicts(&common, Some(&a.tip())).expect("conflicts");
    assert_eq!(listed.len(), 2, "{listed:?}");
    assert_eq!(
        sync::clear_conflict(
            &common,
            &sync::lock(&common).expect("lock"),
            ".mdkg/notes.txt",
            &merges[1]
        )
        .expect("clear"),
        1
    );
    let left = sync::conflicts(&common, Some(&a.tip())).expect("conflicts");
    assert_eq!(left.len(), 1, "{left:?}");
    assert_eq!(
        left[0].merge, merges[0],
        "the older conflict is still listed"
    );
}

#[test]
fn a_remote_deletion_wins_over_a_local_edit() {
    let world = World::new();
    let a = world.first("a");
    a.approve();
    a.sync(Who::Person, true).expect("seed");
    let b = world.second("b");
    b.approve();
    let writer = b.graph().writer(&b.dir, LockMode::Local).expect("writer");
    writer
        .write(&Request {
            id: "del".into(),
            base: Some(writer.snapshot().expect("snap")),
            changes: vec![Change::Delete {
                path: ".mdkg/notes.txt".into(),
            }],
            message: "delete notes".into(),
            trailers: vec![],
        })
        .expect("delete");
    b.sync(Who::Person, true).expect("b pushes");
    a.put(".mdkg/notes.txt", "edited\n");
    let out = a.sync(Who::Person, true).expect("merge");
    assert_eq!(out.conflicts.len(), 1);
    assert_eq!(out.conflicts[0].remote, None);
    let tip = a.tip();
    assert!(
        !git_in(
            &a.dir,
            &["cat-file", "-e", &format!("{tip}:.mdkg/notes.txt")]
        )
        .status
        .success()
    );
}

#[test]
fn an_oversized_pin_is_refused_and_the_old_pin_stays() {
    // Terra #19005 m4: remote.json over the reader's 64 KiB cap is never
    // written, so the approval in place stays readable.
    let world = World::new();
    let a = world.first("a");
    a.approve();
    let graph = a.graph();
    let before = graph.remote.as_ref().and_then(|r| r.pinned.clone());
    assert!(before.is_some());
    let mut big = before.clone().expect("pinned");
    big.setup.insert("huge".into(), "x".repeat(70 * 1024));
    let e = graphref::pin_destination(&graph.common, &big).expect_err("over the cap");
    assert_eq!(e.code, "graph.remote_invalid", "{}", e.message);
    let after = a.graph().remote.and_then(|r| r.pinned);
    assert_eq!(after, before);
}

#[test]
fn a_deleted_remote_ref_is_recreated_only_after_approving_again() {
    // DECISION 17 (orchestrator, 2026-11-04): accepting can't adopt a tip
    // that doesn't exist, and a push would recreate the ref with all the
    // history its deletion removed, so it waits for a person's approval.
    let world = World::new();
    let a = world.first("a");
    a.approve();
    a.sync(Who::Person, true).expect("seed");
    git(&world.path("graphs.git"), &["update-ref", "-d", GRAPH]);
    for (who, accept) in [
        (Who::Launcher, false),
        (Who::Person, false),
        (Who::Person, true),
    ] {
        let e = a
            .sync_with(who, true, Some(&Marker), accept)
            .expect_err("deleted");
        assert_eq!(e.code, "graph.remote_rewritten", "{who:?} {accept}");
        assert!(e.message.contains("graph remote approve"), "{}", e.message);
        assert_eq!(remote_tip(&world, "graphs.git"), None, "{who:?} {accept}");
    }
    // Approving again is the person's choice to publish there anew.
    a.approve();
    let out = a.sync(Who::Launcher, true).expect("first push again");
    assert!(out.pushed);
    assert_eq!(remote_tip(&world, "graphs.git"), Some(a.tip()));
    // A remote moved back is still a rewrite after a plain re-approval.
    let first = a.tip();
    a.put(".mdkg/notes.txt", "two\n");
    a.sync(Who::Person, true).expect("second");
    git(&world.path("graphs.git"), &["update-ref", GRAPH, &first]);
    a.approve();
    assert_eq!(
        a.sync(Who::Launcher, true).expect_err("rewritten").code,
        "graph.remote_rewritten"
    );
}

#[test]
fn a_deleted_ref_restored_before_approval_keeps_the_rewrite_guard() {
    // Terra #19459 M1: approving while the ref is deleted permits creating
    // it, never forgets the accepted tip, so a ref restored to an older tip
    // before the next sync is still a rewrite.
    let world = World::new();
    let a = world.first("a");
    a.approve();
    let first = a.tip();
    a.sync(Who::Person, true).expect("seed");
    a.put(".mdkg/notes.txt", "two\n");
    a.sync(Who::Person, true).expect("second");
    git(&world.path("graphs.git"), &["update-ref", "-d", GRAPH]);
    assert_eq!(
        a.sync(Who::Launcher, true).expect_err("deleted").code,
        "graph.remote_rewritten"
    );
    git(&world.path("graphs.git"), &["update-ref", GRAPH, &first]);
    a.approve();
    for who in [Who::Launcher, Who::Person] {
        assert_eq!(
            a.sync(who, true).expect_err("still a rewrite").code,
            "graph.remote_rewritten",
            "{who:?}"
        );
        assert_eq!(remote_tip(&world, "graphs.git"), Some(first.clone()));
    }
    // Seen again, the permission is gone: deleting it once more needs
    // another approval.
    git(&world.path("graphs.git"), &["update-ref", "-d", GRAPH]);
    let e = a.sync(Who::Person, true).expect_err("deleted again");
    assert!(e.message.contains("graph remote approve"), "{}", e.message);
    assert_eq!(remote_tip(&world, "graphs.git"), None);
}

/// A remote whose ref `ls-remote` sees but whose fetch fails, after
/// reading what `sync.json` held when the fetch started.
struct FetchFails(net::Git, PathBuf, std::cell::Cell<Option<bool>>);

impl sync::net::Net for FetchFails {
    fn remote_tip(
        &self,
        remote: &str,
        remote_ref: &str,
    ) -> Result<Option<String>, graphref::Error> {
        self.0.remote_tip(remote, remote_ref)
    }
    fn heads(&self, remote: &str) -> Result<Vec<String>, graphref::Error> {
        self.0.heads(remote)
    }
    fn fetch(&self, _: &str, _: &str, _: &str) -> Result<String, graphref::Error> {
        let state = sync::State::load(&self.1).expect("sync.json");
        self.2.set(Some(state.recreate || state.missing));
        Err(graphref::Error::new(
            "graph.remote_unreachable",
            "the fetch failed",
        ))
    }
    fn push(
        &self,
        remote: &str,
        commit: &str,
        remote_ref: &str,
        expect: Option<&str>,
    ) -> Result<net::Pushed, graphref::Error> {
        self.0.push(remote, commit, remote_ref, expect)
    }
}

#[test]
fn a_ref_seen_again_ends_the_recreate_approval_even_if_its_fetch_fails() {
    // Terra #19640: the permission to recreate a deleted ref is consumed
    // when the ref is seen, before the fetch, so a deletion after a failed
    // fetch needs another approval.
    let world = World::new();
    let a = world.first("a");
    a.approve();
    a.sync(Who::Person, true).expect("seed");
    let tip = a.tip();
    git(&world.path("graphs.git"), &["update-ref", "-d", GRAPH]);
    assert_eq!(
        a.sync(Who::Launcher, true).expect_err("deleted").code,
        "graph.remote_rewritten"
    );
    a.approve();
    git(&world.path("graphs.git"), &["update-ref", GRAPH, &tip]);
    let graph = a.graph();
    let fails = FetchFails(
        net::Git::new(&graph.common, &a.dir),
        graph.common.clone(),
        std::cell::Cell::new(None),
    );
    let e = sync::run(
        &a.dir,
        &graph,
        &fails,
        &Guards {
            protected: &[],
            scanner: Some(&Marker),
            definitions: &|_, _| Ok(()),
            replay: None,
        },
        Options {
            who: Who::Launcher,
            push: true,
            accept_rewrite: false,
        },
    )
    .expect_err("the fetch fails");
    assert_eq!(e.code, "graph.remote_unreachable");
    // Durable before the fetch began, not only by the failure's write
    // (Terra #19713).
    assert_eq!(fails.2.get(), Some(false), "cleared before the fetch");
    git(&world.path("graphs.git"), &["update-ref", "-d", GRAPH]);
    let e = a.sync(Who::Launcher, true).expect_err("deleted again");
    assert_eq!(e.code, "graph.remote_rewritten");
    assert!(e.message.contains("graph remote approve"), "{}", e.message);
    assert_eq!(remote_tip(&world, "graphs.git"), None);
}

#[test]
fn a_ref_deleted_during_an_accepted_rewrite_is_not_recreated() {
    // Terra #19459 M2: the deleted-ref guard holds on every attempt, an
    // adoption's retry included.
    let world = World::new();
    let a = world.first("a");
    a.approve();
    let first = a.tip();
    a.sync(Who::Person, true).expect("seed");
    let c = world.second("c");
    a.put(".mdkg/notes.txt", "two\n");
    a.sync(Who::Person, true).expect("second");
    rewritten_to_r1(&world, &a, &c, &first);
    a.put(".mdkg/other.txt", "local\n");
    let graph = a.graph();
    let moving = Moving {
        inner: net::Git::new(&graph.common, &a.dir),
        before_push: std::cell::Cell::new(Some(Box::new(|| {
            git(&world.path("graphs.git"), &["update-ref", "-d", GRAPH]);
        }))),
    };
    let e = sync::run(
        &a.dir,
        &graph,
        &moving,
        &Guards {
            protected: &[],
            scanner: Some(&Marker),
            definitions: &|_, _| Ok(()),
            replay: None,
        },
        Options {
            who: Who::Person,
            push: true,
            accept_rewrite: true,
        },
    )
    .expect_err("deleted mid-sync");
    assert_eq!(e.code, "graph.remote_rewritten");
    assert!(e.message.contains("graph remote approve"), "{}", e.message);
    assert_eq!(remote_tip(&world, "graphs.git"), None);
}

#[test]
fn a_rewritten_remote_stays_refused_until_a_person_accepts_it() {
    let world = World::new();
    let a = world.first("a");
    a.approve();
    let first = a.tip();
    a.sync(Who::Person, true).expect("seed");
    a.put(".mdkg/notes.txt", "two\n");
    a.sync(Who::Person, true).expect("second");
    // Someone moves the remote back.
    git(&world.path("graphs.git"), &["update-ref", GRAPH, &first]);
    assert_eq!(
        a.sync(Who::Launcher, true).expect_err("rewritten").code,
        "graph.remote_rewritten"
    );
    // Again after a restart: the accepted tip is persisted.
    assert_eq!(
        a.sync(Who::Launcher, false).expect_err("still").code,
        "graph.remote_rewritten"
    );
    assert_eq!(
        a.sync_with(Who::Launcher, true, Some(&Marker), true)
            .expect_err("not a person")
            .code,
        "graph.remote_rewritten"
    );
    // A local change made since, in a path the rewrite didn't touch.
    let before = a.put(".mdkg/other.txt", "three\n");
    let out = a
        .sync_with(Who::Person, true, Some(&Marker), true)
        .expect("accepted");
    // DECISION 9 (a): the rewritten tip is adopted, the local change is
    // re-applied on it alone, and the old local graph is kept.
    let backup = out.backup.clone().expect("a backup ref");
    assert_eq!(a.git(&["rev-parse", &backup]), before);
    assert!(out.pushed && out.conflicts.is_empty(), "{out:?}");
    assert_eq!(remote_tip(&world, "graphs.git"), Some(out.after.clone()));
    assert_eq!(a.tip(), out.after);
    assert_eq!(
        a.git(&["rev-parse", &format!("{}^@", out.after)]),
        first,
        "parented on the rewritten tip alone"
    );
    let removed = git_in(
        &a.dir,
        &["merge-base", "--is-ancestor", &before, &out.after],
    );
    assert!(!removed.status.success(), "the removed history stays out");
    assert_eq!(
        a.git(&["show", &format!("{}:.mdkg/other.txt", out.after)]),
        "three"
    );
    assert_eq!(
        git_in(
            &a.dir,
            &["cat-file", "-e", &format!("{}:.mdkg/notes.txt", out.after)]
        )
        .status
        .success(),
        git_in(
            &a.dir,
            &["cat-file", "-e", &format!("{first}:.mdkg/notes.txt")]
        )
        .status
        .success(),
        "the rewrite's version of the path it reverted"
    );
    // Accepted: background sync goes on from it.
    a.put(".mdkg/other.txt", "four\n");
    assert!(a.sync(Who::Launcher, true).expect("launcher").pushed);
}

/// A rewrite of `a`'s remote by another device `c`: `r1` is `first` plus
/// `.mdkg/f.txt`, force-pushed. `a` has accepted a later tip.
fn rewritten_to_r1(world: &World, a: &Clone, c: &Clone, first: &str) -> String {
    c.git(&["update-ref", GRAPH, first]);
    let r1 = c.put(".mdkg/f.txt", "from the rewrite\n");
    c.git(&["push", "-q", "--force", "mine", &format!("{r1}:{GRAPH}")]);
    assert_eq!(remote_tip(world, "graphs.git"), Some(r1.clone()));
    let _ = a;
    r1
}

fn has(clone: &Clone, commit: &str, path: &str) -> bool {
    git_in(&clone.dir, &["cat-file", "-e", &format!("{commit}:{path}")])
        .status
        .success()
}

#[test]
fn a_stale_push_after_an_adoption_syncs_from_the_adopted_tip() {
    // DECISION 18 (R), Terra #19140 M2: once a rewrite is adopted, a
    // retry after a lost lease is a plain sync from it, so a deletion the
    // other device made in between stays deleted.
    let world = World::new();
    let a = world.first("a");
    a.approve();
    let first = a.tip();
    a.sync(Who::Person, true).expect("seed");
    let c = world.second("c");
    a.put(".mdkg/notes.txt", "two\n");
    a.sync(Who::Person, true).expect("second");
    let r1 = rewritten_to_r1(&world, &a, &c, &first);
    a.put(".mdkg/other.txt", "local\n");
    let graph = a.graph();
    let moving = Moving {
        inner: net::Git::new(&graph.common, &a.dir),
        before_push: std::cell::Cell::new(Some(Box::new(|| {
            // Another device advances r1 to r2, deleting f.txt.
            let r2 = c.change(
                vec![Change::Delete {
                    path: ".mdkg/f.txt".into(),
                }],
                "drop f",
            );
            c.git(&["push", "-q", "mine", &format!("{r2}:{GRAPH}")]);
        }))),
    };
    let out = sync::run(
        &a.dir,
        &graph,
        &moving,
        &Guards {
            protected: &[],
            scanner: Some(&Marker),
            definitions: &|_, _| Ok(()),
            replay: None,
        },
        Options {
            who: Who::Person,
            push: true,
            accept_rewrite: true,
        },
    )
    .expect("adopted, then synced");
    assert!(out.pushed, "{out:?}");
    assert_eq!(remote_tip(&world, "graphs.git"), Some(out.after.clone()));
    assert!(!has(&a, &out.after, ".mdkg/f.txt"), "r2's deletion stays");
    assert!(
        has(&a, &out.after, ".mdkg/other.txt"),
        "the local change stays"
    );
    let r2 = c.tip();
    assert_ne!(r1, r2);
    assert!(
        git_in(&a.dir, &["merge-base", "--is-ancestor", &r2, &out.after])
            .status
            .success()
    );
}

#[test]
fn an_adoption_whose_state_write_was_lost_is_recognized() {
    // DECISION 18 (R), Terra #19140 M3: the adoption is read from git
    // history, so a lost sync.json write after the local move neither
    // refuses the adopted remote again nor lets a second acceptance
    // replace the adopted graph.
    let world = World::new();
    let a = world.first("a");
    a.approve();
    let first = a.tip();
    a.sync(Who::Person, true).expect("seed");
    let c = world.second("c");
    a.put(".mdkg/notes.txt", "two\n");
    a.sync(Who::Person, true).expect("second");
    let old = a.tip();
    rewritten_to_r1(&world, &a, &c, &first);
    a.put(".mdkg/other.txt", "local\n");
    let adopted = a
        .sync_with(Who::Person, true, Some(&Marker), true)
        .expect("adopted");
    // The write after the move is lost: sync.json still holds the old tip.
    let state = a.dir.join(".git/mdkg/graph/sync.json");
    let mut v: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&state).expect("state")).expect("json");
    v["accepted"] = serde_json::Value::String(old);
    std::fs::write(&state, serde_json::to_vec(&v).expect("json")).expect("state");
    let out = a
        .sync(Who::Launcher, true)
        .expect("recognized, not refused");
    assert_eq!(out.after, adopted.after);
    let again = a
        .sync_with(Who::Person, true, Some(&Marker), true)
        .expect("accepting again changes nothing");
    assert_eq!(again.after, adopted.after);
    assert!(has(&a, &again.after, ".mdkg/other.txt"));
}

#[test]
fn an_accepted_rewrite_with_no_local_change_takes_the_remote() {
    let world = World::new();
    let a = world.first("a");
    a.approve();
    let first = a.tip();
    a.sync(Who::Person, true).expect("seed");
    let second = a.put(".mdkg/notes.txt", "two\n");
    a.sync(Who::Person, true).expect("second");
    // The remote moved back to an ancestor of the local graph: pushing
    // the local graph would re-publish what the rewrite removed.
    git(&world.path("graphs.git"), &["update-ref", GRAPH, &first]);
    let out = a
        .sync_with(Who::Person, true, Some(&Marker), true)
        .expect("accepted");
    assert_eq!(out.after, first);
    assert!(!out.pushed);
    assert_eq!(remote_tip(&world, "graphs.git"), Some(first.clone()));
    assert_eq!(
        a.git(&["rev-parse", out.backup.as_deref().expect("backup")]),
        second
    );
    // And it stays accepted.
    assert!(!a.sync(Who::Launcher, true).expect("launcher").pushed);
}

#[test]
fn a_failed_adoption_keeps_the_rewrite_refused() {
    // Terra #18960: the old accepted tip stays until the local graph
    // moves, so a backup that can't be written leaves the rewrite refused.
    let world = World::new();
    let a = world.first("a");
    a.approve();
    let first = a.tip();
    a.sync(Who::Person, true).expect("seed");
    a.put(".mdkg/notes.txt", "two\n");
    a.sync(Who::Person, true).expect("second");
    git(&world.path("graphs.git"), &["update-ref", GRAPH, &first]);
    // A ref where the backup folder would go: update-ref can't create it.
    a.git(&["update-ref", "refs/mdkg/backup/mine", &first]);
    let before = a.tip();
    a.sync_with(Who::Person, true, Some(&Marker), true)
        .expect_err("no backup");
    assert_eq!(a.tip(), before);
    assert_eq!(
        a.sync(Who::Launcher, true).expect_err("still refused").code,
        "graph.remote_rewritten"
    );
    assert_eq!(remote_tip(&world, "graphs.git"), Some(first.clone()));
    a.git(&["update-ref", "-d", "refs/mdkg/backup/mine"]);
    let out = a
        .sync_with(Who::Person, true, Some(&Marker), true)
        .expect("adopted");
    assert_eq!(out.after, first);
}

#[test]
fn an_accepted_rewrite_waits_for_open_conflicts() {
    // Terra #18960: adopting would leave the merge that recorded a
    // conflict, hiding it; it's settled first.
    let world = World::new();
    let a = world.first("a");
    a.approve();
    a.sync(Who::Person, true).expect("seed");
    let b = world.second("b");
    b.approve();
    b.put(".mdkg/notes.txt", "one\nB\n");
    b.sync(Who::Person, true).expect("b pushes");
    a.put(".mdkg/notes.txt", "one\nA\n");
    let out = a.sync(Who::Launcher, true).expect("merge");
    assert_eq!(out.conflicts.len(), 1);
    let merge = a.tip();
    // The remote is reset to b's tip, the merge's remote parent.
    git(&world.path("graphs.git"), &["update-ref", GRAPH, &b.tip()]);
    let e = a
        .sync_with(Who::Person, true, Some(&Marker), true)
        .expect_err("open conflict");
    assert_eq!(e.code, "graph.sync_conflict", "{}", e.message);
    assert_eq!(a.tip(), merge);
    sync::clear_conflict(
        &a.graph().common,
        &sync::lock(&a.graph().common).expect("lock"),
        ".mdkg/notes.txt",
        &merge,
    )
    .expect("clear");
    a.sync_with(Who::Person, true, Some(&Marker), true)
        .expect("adopted once settled");
}

#[test]
fn the_third_try_takes_the_remote_tree_exactly() {
    // Terra #18960: a remote rename that git carries a local edit into
    // is undone too; the third try's tree is the remote's.
    let node = |id: &str, title: &str, body: &str| {
        format!(
            "---\nid: {id}\ntype: task\ntitle: {title}\nstatus: todo\ncreated: 2026-10-24\nupdated: 2026-10-24\n---\n{body}"
        )
    };
    let x = "task-0192f3a4-0000-7000-8000-000000000002";
    let y = "task-0192f3a4-0000-7000-8000-000000000003";
    let body = "A body long enough for rename detection to match.\n".repeat(8);
    let world = World::new();
    let a = world.first("a");
    a.approve();
    a.put(".mdkg/work/old.md", &node(x, "X", &body));
    a.put(&format!(".mdkg/work/{y}.md"), &node(y, "Y", ""));
    a.sync(Who::Person, true).expect("seed");
    let b = world.second("b");
    b.approve();
    // The remote renames old.md to new.md and edits Y.
    b.change(
        vec![
            Change::Put {
                path: ".mdkg/work/new.md".into(),
                bytes: node(x, "X", &body).into_bytes(),
            },
            Change::Delete {
                path: ".mdkg/work/old.md".into(),
            },
        ],
        "rename",
    );
    b.put(&format!(".mdkg/work/{y}.md"), &node(y, "Y edited", ""));
    b.sync(Who::Person, true).expect("b pushes");
    // Locally: a small edit to old.md (git carries it to new.md), and Y
    // moved with its body rewritten (no rename seen: a duplicate id).
    a.put(
        ".mdkg/work/old.md",
        &node(x, "X", &format!("{body}local line\n")),
    );
    a.change(
        vec![
            Change::Put {
                path: ".mdkg/work/y-moved.md".into(),
                bytes: format!("{}{}", node(y, "Y", ""), "Rewritten.\n".repeat(40)).into_bytes(),
            },
            Change::Delete {
                path: format!(".mdkg/work/{y}.md"),
            },
        ],
        "move Y",
    );
    let out = a.sync(Who::Launcher, true).expect("third try");
    let merge = a.tip();
    assert_eq!(
        a.git(&["rev-parse", &format!("{merge}^{{tree}}")]),
        a.git(&["rev-parse", &format!("{}^{{tree}}", b.tip())]),
        "the remote's tree, the carried edit included"
    );
    let paths: Vec<&str> = out.conflicts.iter().map(|c| c.path.as_str()).collect();
    assert!(paths.contains(&".mdkg/work/old.md"), "{paths:?}");
    assert!(paths.contains(&".mdkg/work/y-moved.md"), "{paths:?}");
}

#[test]
fn a_hook_refusal_stops_background_sync_until_a_person_syncs() {
    let world = World::new();
    let a = world.first("a");
    a.approve();
    let hook = a.dir.join(".git/test-hooks/pre-push");
    std::fs::write(&hook, "#!/bin/sh\necho refused by policy >&2\nexit 1\n").expect("hook");
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    // The hook is part of the pinned setup (DECISION 10).
    a.approve();
    assert_eq!(
        a.sync(Who::Launcher, true).expect_err("hook").code,
        "graph.push_refused"
    );
    std::fs::remove_file(&hook).expect("unhook");
    a.approve();
    assert_eq!(
        a.sync(Who::Launcher, true)
            .expect_err("waits for a person")
            .code,
        "graph.push_refused"
    );
    assert_eq!(remote_tip(&world, "graphs.git"), None);
    assert!(a.sync(Who::Person, true).expect("person").pushed);
    let state = sync::State::load(&a.graph().common).expect("state");
    assert!(state.refused.is_none() && state.last_error.is_none());
}

#[test]
fn a_relative_hooks_path_and_remote_url_resolve_from_the_checkout() {
    let world = World::new();
    let a = world.first("a");
    a.approve();
    // The spelling git reads from the checkout: one destination, so the
    // approval still holds.
    a.git(&["remote", "set-url", "mine", "../graphs.git"]);
    assert_eq!(
        sync::approval(&a.dir, &a.graph()).expect("approval"),
        Approval::Approved
    );
    // A committed-hooks layout: `.githooks` in the checkout, not `.git`.
    a.git(&["config", "core.hooksPath", ".githooks"]);
    let hook = a.dir.join(".githooks/pre-push");
    std::fs::create_dir_all(hook.parent().expect("hooks")).expect("mkdir");
    std::fs::write(&hook, "#!/bin/sh\necho refused by policy >&2\nexit 1\n").expect("hook");
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    // The hook is part of the pinned setup (DECISION 10).
    a.approve();
    assert_eq!(
        a.sync(Who::Person, true).expect_err("the hook runs").code,
        "graph.push_refused"
    );
    assert_eq!(remote_tip(&world, "graphs.git"), None);
    std::fs::remove_file(&hook).expect("unhook");
    a.approve();
    assert!(a.sync(Who::Person, true).expect("person").pushed);
    assert_eq!(remote_tip(&world, "graphs.git"), Some(a.tip()));
}

#[test]
fn a_checkout_that_isnt_the_clones_refuses_network_commands() {
    let world = World::new();
    let a = world.first("a");
    a.approve();
    let common = a.graph().common;
    let elsewhere = world.path("elsewhere");
    std::fs::create_dir_all(&elsewhere).expect("mkdir");
    git(&elsewhere, &["init", "-q"]);
    let e = net::checkout(&common, &elsewhere).expect_err("another repository");
    assert_eq!(e.code, "graph.git", "{}", e.message);
    assert!(net::network_command(&common, &elsewhere).is_err());
    std::fs::create_dir_all(a.dir.join("sub")).expect("mkdir");
    assert_eq!(
        net::checkout(&common, &a.dir.join("sub")).expect("verified"),
        a.dir.canonicalize().expect("canonical"),
        "a folder inside the checkout finds its top"
    );
}

#[test]
fn a_separate_git_folder_runs_the_checkouts_own_hooks() {
    // `git init --separate-git-dir=<p>/.git <p>/checkout`: the git folder's
    // parent isn't the checkout (Terra #17986).
    let world = World::new();
    let parent = world.path("sep");
    let checkout = parent.join("checkout");
    std::fs::create_dir_all(&parent).expect("mkdir");
    git(
        &parent,
        &[
            "init",
            "-q",
            "--separate-git-dir",
            &parent.join(".git").display().to_string(),
            &checkout.display().to_string(),
        ],
    );
    git(&checkout, &["config", "user.name", "Sync Test"]);
    git(&checkout, &["config", "user.email", "sync@example.invalid"]);
    git(&checkout, &["config", "core.hooksPath", ".githooks"]);
    git(&checkout, &["commit", "-q", "--allow-empty", "-m", "one"]);
    let hook = checkout.join(".githooks/pre-push");
    std::fs::create_dir_all(hook.parent().expect("hooks")).expect("mkdir");
    std::fs::write(&hook, "#!/bin/sh\nexit 1\n").expect("hook");
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    let common = parent.join(".git");
    assert_eq!(
        net::checkout(&common, &checkout).expect("verified"),
        checkout.canonicalize().expect("canonical")
    );
    // A relative remote URL resolves from the checkout, as git's would.
    let push = |args: &[&str]| {
        net::network_command(&common, &checkout)
            .expect("runner")
            .args(args)
            .run(None)
            .expect("git")
    };
    let out = push(&["push", "-q", "../../other.git", "HEAD:refs/heads/x"]);
    assert!(!out.status.success(), "the checkout's pre-push hook ran");
    assert_eq!(git(&world.path("other.git"), &["for-each-ref"]), "");
    std::fs::remove_file(&hook).expect("unhook");
    let out = push(&["push", "-q", "../../other.git", "HEAD:refs/heads/x"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!git(&world.path("other.git"), &["for-each-ref"]).is_empty());
}

#[test]
fn a_linked_worktrees_own_hooks_path_runs_on_its_pushes() {
    // `extensions.worktreeConfig` and a hooks path in the worktree's own
    // `config.worktree` only (Terra #18102): git reads it there, so the
    // runner must too.
    let world = World::new();
    let a = world.first("a");
    a.git(&["config", "extensions.worktreeConfig", "true"]);
    let linked = world.path("linked");
    a.git(&[
        "worktree",
        "add",
        "-q",
        "--detach",
        &linked.display().to_string(),
    ]);
    git(
        &linked,
        &["config", "--worktree", "core.hooksPath", ".wthooks"],
    );
    let hook = linked.join(".wthooks/pre-push");
    std::fs::create_dir_all(hook.parent().expect("hooks")).expect("mkdir");
    std::fs::write(&hook, "#!/bin/sh\nexit 1\n").expect("hook");
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    let common = a.graph().common;
    let push = |from: &Path| {
        net::network_command(&common, from)
            .expect("runner")
            .args([
                "push",
                "-q",
                &world.path("other.git").display().to_string(),
                "HEAD:refs/heads/x",
            ])
            .run(None)
            .expect("git")
    };
    git(&world.path(""), &["init", "-q", "--bare", "other.git"]);
    assert!(!push(&linked).status.success(), "the worktree's hook ran");
    assert_eq!(git(&world.path("other.git"), &["for-each-ref"]), "");
    // The main checkout has no such hook.
    let out = push(&a.dir);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn a_mirror_fetch_refspec_moves_no_ref_but_the_tracking_one() {
    let world = World::new();
    let a = world.first("a");
    a.approve();
    assert!(a.sync(Who::Person, true).expect("push").pushed);
    let b = world.second("b");
    b.approve();
    b.sync(Who::Person, true).expect("pull");
    b.put(".mdkg/notes.txt", "from b\n");
    assert!(b.sync(Who::Person, true).expect("push").pushed);
    // A mirror-style fetch refspec on a's remote: a plain fetch of the
    // graph ref would write a's local graph ref past every check.
    let before = a.tip();
    a.git(&[
        "config",
        "--add",
        "remote.mine.fetch",
        "+refs/heads/*:refs/heads/*",
    ]);
    let net = net::Git::new(&a.graph().common, &a.dir);
    let graph = a.graph();
    let remote = graph.remote.as_ref().expect("remote");
    let into = net::tracking_ref(&remote.name, &remote.remote_ref);
    use sync::net::Net as _;
    net.fetch(&remote.name, &remote.remote_ref, &into)
        .expect("fetch");
    assert_eq!(a.tip(), before, "the graph ref moved only through sync");
}

#[test]
fn a_refusal_whose_state_write_fails_still_stops_background_sync() {
    let world = World::new();
    let a = world.first("a");
    a.approve();
    let state_dir = a.graph().common.join("mdkg/graph");
    assert!(state_dir.join("remote.json").exists(), "the state folder");
    // The hook refuses, and makes the state folder unwritable first, so
    // the write recording the refusal fails.
    let hook = a.dir.join(".git/test-hooks/pre-push");
    std::fs::write(
        &hook,
        format!(
            "#!/bin/sh\nchmod a-w '{}'\necho refused by policy >&2\nexit 1\n",
            state_dir.display()
        ),
    )
    .expect("hook");
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    // The hook is part of the pinned setup (DECISION 10).
    a.approve();
    let e = a.sync(Who::Launcher, true).expect_err("hook");
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o755))
            .expect("writable again");
    }
    assert_eq!(e.code, "graph.push_refused", "{}", e.message);
    assert!(
        e.message.contains("sync.json"),
        "the write failed: {}",
        e.message
    );
    std::fs::remove_file(&hook).expect("unhook");
    a.approve();
    // The latch written before the push holds.
    let state = sync::State::load(&a.graph().common).expect("state");
    assert_eq!(
        state.refused.as_ref().map(|r| r.code.as_str()),
        Some(sync::UNRECORDED)
    );
    let e = a.sync(Who::Launcher, true).expect_err("waits for a person");
    assert_eq!(e.code, "graph.push_refused");
    assert_eq!(remote_tip(&world, "graphs.git"), None);
    assert!(a.sync(Who::Person, true).expect("person").pushed);
    let state = sync::State::load(&a.graph().common).expect("state");
    assert!(state.refused.is_none(), "{state:?}");
    // An accepted push clears the latch it wrote.
    a.put(".mdkg/notes.txt", "more\n");
    assert!(a.sync(Who::Launcher, true).expect("launcher").pushed);
    let state = sync::State::load(&a.graph().common).expect("state");
    assert!(state.refused.is_none(), "{state:?}");
}

#[test]
fn a_hook_refusal_git_reports_as_fatal_stays_latched() {
    let world = World::new();
    let a = world.first("a");
    a.approve();
    let hook = a.dir.join(".git/test-hooks/pre-push");
    std::fs::write(
        &hook,
        "#!/bin/sh\necho 'fatal: refused by policy' >&2\nexit 1\n",
    )
    .expect("hook");
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    // The hook is part of the pinned setup (DECISION 10).
    a.approve();
    a.sync(Who::Launcher, true).expect_err("the hook refuses");
    std::fs::remove_file(&hook).expect("unhook");
    a.approve();
    let state = sync::State::load(&a.graph().common).expect("state");
    assert_eq!(
        state.refused.as_ref().map(|r| r.code.as_str()),
        Some(sync::UNRECORDED),
        "{state:?}"
    );
    assert_eq!(
        a.sync(Who::Launcher, true)
            .expect_err("waits for a person")
            .code,
        "graph.push_refused"
    );
    assert_eq!(remote_tip(&world, "graphs.git"), None);
    assert!(a.sync(Who::Person, true).expect("person").pushed);
    let state = sync::State::load(&a.graph().common).expect("state");
    assert!(state.refused.is_none(), "{state:?}");
}

#[test]
fn an_unreachable_remote_is_a_classified_failure_and_recovers() {
    let world = World::new();
    let a = world.first("a");
    a.approve();
    let moved = world.path("graphs-moved.git");
    std::fs::rename(world.path("graphs.git"), &moved).expect("move away");
    let e = a.sync(Who::Launcher, true).expect_err("offline");
    assert_eq!(e.code, "graph.remote_unreachable", "{}", e.message);
    let state = sync::State::load(&a.graph().common).expect("state");
    assert_eq!(
        state.last_error.as_ref().map(|f| f.code.as_str()),
        Some("graph.remote_unreachable")
    );
    assert_eq!(state.attempts, 1);
    std::fs::rename(&moved, world.path("graphs.git")).expect("back");
    assert!(a.sync(Who::Launcher, true).expect("back online").pushed);
    let state = sync::State::load(&a.graph().common).expect("state");
    assert!(state.last_error.is_none() && state.last_ok.is_some());
}
