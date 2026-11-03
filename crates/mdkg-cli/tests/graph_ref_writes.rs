//! Graph writers on the graph ref (edd-01a18867, lane L3): ref-mode
//! `init` creates the graph as an orphan graph ref by a create
//! compare-and-swap, with no file in any checkout and no code branch
//! moved (R10, R30, C1).

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;

mod common;

const GRAPH: &str = "refs/heads/mdkg-graph";

/// A scratch folder and its checkout's folder name.
struct Scratch(PathBuf, &'static str);

impl Drop for Scratch {
    fn drop(&mut self) {
        // The graph cache is read-only by design.
        Command::new("chmod")
            .args(["-R", "u+w"])
            .arg(&self.0)
            .status()
            .ok();
        fs::remove_dir_all(&self.0).ok();
    }
}

impl Scratch {
    fn new() -> Self {
        Self::named("repo")
    }

    /// A scratch whose checkout folder is `name`, such as one a shell
    /// must quote.
    fn named(name: &'static str) -> Self {
        let dir = common::temp::dir("mdkg-graph-ref-writes");
        let dir = fs::canonicalize(dir).expect("canonical");
        fs::create_dir_all(dir.join("home")).expect("home");
        fs::create_dir_all(dir.join(name)).expect("repo");
        Self(dir, name)
    }

    fn repo(&self) -> PathBuf {
        self.0.join(self.1)
    }

    /// Nothing reaches the user's own configuration: HOME and the XDG
    /// folders are the scratch's, and git reads no global or system file.
    fn isolated<'a>(&self, cmd: &'a mut Command) -> &'a mut Command {
        let home = self.0.join("home");
        common::without_launcher_env(cmd)
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .env("CODEX_HOME", home.join(".codex"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("MDKG_ACTOR")
            .env_remove("MDKG_MAIN_REF")
    }

    fn git(&self, args: &[&str]) -> String {
        let out = self
            .isolated(Command::new("git").arg("-C").arg(self.repo()).args(args))
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    fn try_git(&self, args: &[&str]) -> Option<String> {
        let out = self
            .isolated(Command::new("git").arg("-C").arg(self.repo()).args(args))
            .output()
            .expect("git runs");
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }

    fn mdkg(&self, args: &[&str]) -> (i32, Value) {
        self.mdkg_with(args, &[])
    }

    /// `mdkg` with `input` on stdin.
    fn mdkg_stdin(&self, args: &[&str], input: &[u8]) -> (i32, Value) {
        use std::io::Write as _;
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_mdkg"));
        self.isolated(&mut cmd)
            .current_dir(self.repo())
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = cmd.spawn().expect("mdkg runs");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(input)
            .expect("write stdin");
        let out = child.wait_with_output().expect("mdkg finishes");
        let json: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
            panic!(
                "mdkg {args:?} printed no JSON ({e}): {}\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
        });
        (out.status.code().unwrap_or(-1), json)
    }

    fn mdkg_with(&self, args: &[&str], vars: &[(&str, &str)]) -> (i32, Value) {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_mdkg"));
        self.isolated(&mut cmd).current_dir(self.repo()).args(args);
        for (k, v) in vars {
            cmd.env(k, v);
        }
        let out = cmd.output().expect("mdkg runs");
        let json: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
            panic!(
                "mdkg {args:?} printed no JSON ({e}): {}\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
        });
        (out.status.code().unwrap_or(-1), json)
    }

    fn bounded_status(&self) -> (i32, Value) {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_mdkg"));
        self.isolated(&mut cmd)
            .current_dir(self.repo())
            .args(["status", "--json"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = cmd.spawn().expect("spawn status");
        let started = std::time::Instant::now();
        while child.try_wait().expect("poll status").is_none() {
            if started.elapsed() > std::time::Duration::from_secs(15) {
                child.kill().ok();
                child.wait().ok();
                panic!("status exceeded the 15-second test deadline");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let result = child.wait_with_output().expect("status output");
        let status: Value = serde_json::from_slice(&result.stdout).expect("status JSON");
        (result.status.code().unwrap_or(-1), status)
    }

    /// A repository with its own identity for commits, in ref mode.
    fn ref_mode(&self, commit: bool) {
        self.git(&["init", "-q", "-b", "trunk"]);
        self.git(&["config", "user.name", "Graph Test"]);
        self.git(&["config", "user.email", "graph@example.invalid"]);
        if commit {
            fs::write(self.repo().join("code.rs"), "fn main() {}\n").expect("code");
            self.git(&["add", "code.rs"]);
            self.git(&["commit", "-q", "-m", "code"]);
        }
        self.git(&["config", "mdkg.graph.ref", GRAPH]);
    }

    fn refs(&self) -> String {
        self.git(&["for-each-ref", "--format=%(refname) %(objectname)"])
    }
}

#[test]
fn init_creates_the_graph_on_an_orphan_graph_ref_and_touches_no_checkout() {
    let s = Scratch::new();
    s.ref_mode(true);
    let head = s.git(&["rev-parse", "HEAD"]);
    let (code, out) = s.mdkg(&["init", "--work", "shared", "--json"]);
    assert_eq!(code, 0, "{out}");
    let graph_id = out["data"]["graph_id"]
        .as_str()
        .expect("graph id")
        .to_owned();
    assert_eq!(out["data"]["already"], false);
    // The graph is the ref's first commit: no parent, graph files only,
    // and no identity-v2 manifest.
    let tip = s.git(&["rev-parse", GRAPH]);
    assert!(s.try_git(&["rev-parse", &format!("{tip}^")]).is_none());
    let files = s.git(&["ls-tree", "-r", "--name-only", GRAPH]);
    let files: Vec<&str> = files.lines().collect();
    assert!(files.contains(&".mdkg/config.json"), "{files:?}");
    assert!(files.contains(&".mdkg/README.md"), "{files:?}");
    assert!(files.iter().all(|f| f.starts_with(".mdkg/")), "{files:?}");
    assert!(!files.contains(&".mdkg/graph.json"));
    let config: Value =
        serde_json::from_str(&s.git(&["show", &format!("{GRAPH}:.mdkg/config.json")]))
            .expect("config");
    assert_eq!(config["graph_id"], graph_id.as_str());
    let message = s.git(&["log", "-1", "--format=%B", GRAPH]);
    assert!(
        message.starts_with(&format!("mdkg: init {graph_id}")),
        "{message}"
    );
    assert!(message.contains("Mdkg-Actor: "), "{message}");
    assert!(message.contains("Mdkg-Clone: "), "{message}");
    // The clone records the graph's identity, in the common config.
    assert_eq!(
        s.git(&["config", "--get", &format!("mdkg.graph.{graph_id}.ref")]),
        GRAPH
    );
    // No checkout changed: no .mdkg here, nothing to commit, HEAD as it
    // was, and no ref other than the graph ref.
    assert!(!s.repo().join(".mdkg").exists());
    assert!(s.git(&["status", "--porcelain"]).is_empty());
    assert_eq!(s.git(&["rev-parse", "HEAD"]), head);
    let refs = s.refs();
    let names: Vec<&str> = refs
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .collect();
    assert_eq!(names, vec![GRAPH, "refs/heads/trunk"], "{refs}");
}

#[test]
fn init_again_reports_the_existing_graph_and_writes_nothing() {
    let s = Scratch::new();
    s.ref_mode(true);
    let (code, first) = s.mdkg(&["init", "--work", "shared", "--json"]);
    assert_eq!(code, 0, "{first}");
    let refs = s.refs();
    let (code, again) = s.mdkg(&["init", "--json"]);
    assert_eq!(code, 0, "{again}");
    assert_eq!(again["data"]["already"], true);
    assert_eq!(again["data"]["graph_id"], first["data"]["graph_id"]);
    assert_eq!(again["data"]["work"], "shared");
    assert_eq!(s.refs(), refs);
    // A choice the graph already made differently is refused.
    let (code, conflict) = s.mdkg(&["init", "--work", "local", "--json"]);
    assert_ne!(code, 0, "{conflict}");
    assert_eq!(conflict["diagnostics"][0]["code"], "init.conflict");
}

#[test]
fn init_leaves_adopting_a_fetched_graph_ref_to_install() {
    let s = Scratch::new();
    s.ref_mode(true);
    let (code, first) = s.mdkg(&["init", "--json"]);
    assert_eq!(code, 0, "{first}");
    let graph_id = first["data"]["graph_id"].as_str().expect("id").to_owned();
    // As a fresh clone that fetched the graph ref: no identity record yet.
    // Recording it is adoption, which is install's.
    s.git(&["config", "--unset", &format!("mdkg.graph.{graph_id}.ref")]);
    let refs = s.refs();
    let (code, again) = s.mdkg(&["init", "--json"]);
    assert_ne!(code, 0, "{again}");
    assert!(
        again.to_string().contains("graph.identity_unrecorded"),
        "{again}"
    );
    assert!(again.to_string().contains("mdkg install"), "{again}");
    assert_eq!(s.refs(), refs);
    assert!(
        s.try_git(&["config", "--get", &format!("mdkg.graph.{graph_id}.ref")])
            .is_none()
    );
}

#[test]
fn init_dry_run_writes_nothing() {
    let s = Scratch::new();
    s.ref_mode(true);
    let refs = s.refs();
    let (code, out) = s.mdkg(&["init", "--dry-run", "--json"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["data"]["dry_run"], true);
    assert_eq!(s.refs(), refs);
    assert!(
        s.try_git(&["rev-parse", "--verify", "--quiet", GRAPH])
            .is_none()
    );
    assert!(!s.repo().join(".mdkg").exists());
}

#[test]
fn init_works_in_a_repository_with_no_commit() {
    let s = Scratch::new();
    s.ref_mode(false);
    let (code, out) = s.mdkg(&["init", "--json"]);
    assert_eq!(code, 0, "{out}");
    // Only the graph ref exists; HEAD's branch is still unborn.
    let refs = s.refs();
    assert_eq!(refs.lines().count(), 1, "{refs}");
    assert!(refs.starts_with(GRAPH), "{refs}");
    assert!(
        s.try_git(&["rev-parse", "--verify", "--quiet", "HEAD"])
            .is_none()
    );
}

#[test]
fn init_in_checkout_keeps_the_0_7_0_layout() {
    let s = Scratch::new();
    s.ref_mode(true);
    let (code, out) = s.mdkg(&["init", "--in-checkout", "--json"]);
    assert_eq!(code, 0, "{out}");
    assert!(s.repo().join(".mdkg/config.json").is_file());
    assert!(
        s.try_git(&["rev-parse", "--verify", "--quiet", GRAPH])
            .is_none()
    );
}

#[test]
fn init_refuses_a_graph_ref_naming_another_clone_s_graph() {
    let s = Scratch::new();
    s.ref_mode(true);
    let (code, out) = s.mdkg(&["init", "--json"]);
    assert_eq!(code, 0, "{out}");
    let graph_id = out["data"]["graph_id"].as_str().expect("id").to_owned();
    s.git(&["config", "--unset", &format!("mdkg.graph.{graph_id}.ref")]);
    s.git(&[
        "config",
        "mdkg.graph.01a18900-0000-7000-8000-0000000000ff.ref",
        GRAPH,
    ]);
    let (code, refused) = s.mdkg(&["init", "--json"]);
    assert_ne!(code, 0, "{refused}");
    assert!(
        refused.to_string().contains("graph.identity_mismatch"),
        "{refused}"
    );
}

/// A ref-mode clone whose graph ref holds one task, published as another
/// clone's sync would (plain git plumbing on the graph ref).
fn with_task(s: &Scratch) -> String {
    s.ref_mode(true);
    let (code, out) = s.mdkg(&["init", "--work", "shared", "--json"]);
    assert_eq!(code, 0, "{out}");
    let task = "task-01a18900-0000-7000-8000-0000000000aa";
    let body = format!(
        "---\nid: {task}\ntype: task\ntitle: Graph task\nstatus: todo\ncreated: \
         2026-10-24T00:00:00.000Z\nupdated: 2026-10-24T00:00:00.000Z\n---\n# Overview\n"
    );
    put_on_graph(s, &format!(".mdkg/work/{task}.md"), &body);
    task.to_owned()
}

/// Commits `body` at `rel` onto the graph ref, as a sync from another
/// clone would.
fn put_on_graph(s: &Scratch, rel: &str, body: &str) {
    let blob = {
        let out = s
            .isolated(Command::new("git").arg("-C").arg(s.repo()))
            .args(["hash-object", "-w", "--stdin"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut c| {
                use std::io::Write as _;
                c.stdin.take().expect("stdin").write_all(body.as_bytes())?;
                c.wait_with_output()
            })
            .expect("hash-object");
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    };
    let tip = s.git(&["rev-parse", GRAPH]);
    let index = s.0.join("graph-index");
    let with_index = |args: &[&str]| {
        let out = s
            .isolated(Command::new("git").arg("-C").arg(s.repo()))
            .env("GIT_INDEX_FILE", &index)
            .args(args)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    };
    with_index(&["read-tree", &tip]);
    with_index(&[
        "update-index",
        "--add",
        "--cacheinfo",
        &format!("100644,{blob},{rel}"),
    ]);
    let tree = with_index(&["write-tree"]);
    let commit = s.git(&["commit-tree", &tree, "-p", &tip, "-m", "sync"]);
    s.git(&["update-ref", GRAPH, &commit, &tip]);
}

#[test]
fn readers_show_the_graph_tip_and_status_reports_the_graph_ref() {
    let s = Scratch::new();
    let task = with_task(&s);
    let (code, list) = s.mdkg(&["list", "--json"]);
    assert_eq!(code, 0, "{list}");
    assert!(list.to_string().contains(&task), "{list}");
    let (code, show) = s.mdkg(&["show", &task, "--json"]);
    assert_eq!(code, 0, "{show}");
    assert_eq!(show["data"]["node"]["title"], "Graph task");
    let (code, status) = s.mdkg(&["status", "--json"]);
    assert_eq!(code, 0, "{status}");
    assert_eq!(status["data"]["graph"]["ref"], GRAPH);
    assert_eq!(
        status["data"]["graph"]["tip"],
        s.git(&["rev-parse", GRAPH]).as_str()
    );
    // Settlement is on the graph ref, and `main` (schema-required) says so.
    assert_eq!(status["data"]["main"], GRAPH, "{status}");
    // Still nothing in the checkout.
    assert!(!s.repo().join(".mdkg").exists());
    assert!(s.git(&["status", "--porcelain"]).is_empty());
}

#[test]
fn a_stale_mdkg_in_the_checkout_is_reported_and_never_read() {
    let s = Scratch::new();
    let task = with_task(&s);
    // main's stale copy, as a merged code branch might carry it.
    fs::create_dir_all(s.repo().join(".mdkg/work")).expect("dir");
    fs::write(
        s.repo().join(format!(".mdkg/work/{task}.md")),
        format!(
            "---\nid: {task}\ntype: task\ntitle: Stale title\nstatus: done\ncreated: \
             2026-10-24T00:00:00.000Z\nupdated: 2026-10-24T00:00:00.000Z\n---\n# Overview\n"
        ),
    )
    .expect("stale");
    let (code, show) = s.mdkg(&["show", &task, "--json"]);
    assert_eq!(code, 0, "{show}");
    assert_eq!(show["data"]["node"]["title"], "Graph task");
    let (code, status) = s.mdkg(&["status", "--json"]);
    assert_eq!(code, 0, "{status}");
    let edits = status["data"]["graph"]["hand_edits"]
        .as_array()
        .expect("edits");
    assert_eq!(edits.len(), 1, "{status}");
    assert_eq!(edits[0]["source"], "checkout");
}

#[test]
fn sync_has_nothing_to_bring_in() {
    let s = Scratch::new();
    let task = with_task(&s);
    let head = s.git(&["rev-parse", "HEAD"]);
    let (code, out) = s.mdkg(&["sync", &task, "--json"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["data"]["up_to_date"], true);
    assert_eq!(
        s.git(&["rev-parse", "HEAD"]),
        head,
        "no graph commit enters a code branch"
    );
}

#[test]
fn a_missing_graph_ref_is_refused_with_no_fallback_to_the_checkout() {
    let s = Scratch::new();
    s.ref_mode(true);
    // A checkout copy that must not be read as the graph.
    let (code, out) = s.mdkg(&["init", "--in-checkout", "--json"]);
    assert_eq!(code, 0, "{out}");
    let (code, list) = s.mdkg(&["list", "--json"]);
    assert_ne!(code, 0, "{list}");
    assert!(list.to_string().contains("graph.ref_missing"), "{list}");
}

#[test]
fn anchor_publishes_on_the_graph_ref_and_leaves_no_cache_edit() {
    let s = Scratch::new();
    with_task(&s);
    let before = s.git(&["rev-parse", GRAPH]);
    let (code, out) = s.mdkg(&["anchor", "--json"]);
    assert_eq!(code, 0, "{out}");
    let path = out["data"]["path"].as_str().expect("path").to_owned();
    let tip = s.git(&["rev-parse", GRAPH]);
    assert_ne!(tip, before);
    assert!(
        s.git(&["show", &format!("{tip}:{path}")])
            .contains("\"head\"")
    );
    assert!(!s.repo().join(".mdkg").exists());
    // The next command finds nothing edited outside a session.
    let (code, status) = s.mdkg(&["status", "--json"]);
    assert_eq!(code, 0, "{status}");
    assert!(
        status["data"]["graph"].get("hand_edits").is_none(),
        "{status}"
    );
    assert!(!status.to_string().contains("graph.cache_edit"), "{status}");
}

#[test]
fn work_localize_is_refused_in_ref_mode() {
    let s = Scratch::new();
    with_task(&s);
    let tip = s.git(&["rev-parse", GRAPH]);
    for args in [
        vec!["work", "localize", "--dry-run", "--json"],
        vec![
            "work",
            "localize",
            "--apply",
            "sha256:0000000000000000000000000000000000000000000000000000000000000000",
            "--json",
        ],
    ] {
        let (code, out) = s.mdkg(&args);
        assert_ne!(code, 0, "{out}");
        assert!(out.to_string().contains("graph.unsupported"), "{out}");
    }
    assert_eq!(s.git(&["rev-parse", GRAPH]), tip);
}

#[test]
fn validate_changed_compares_the_code_base_with_the_graph_tip() {
    let s = Scratch::new();
    with_task(&s);
    let (code, none) = s.mdkg(&["validate", "--changed-only", "--json"]);
    assert_eq!(code, 0, "{none}");
    assert_eq!(none["data"]["changed"], 0, "no code base: {none}");
    s.git(&["config", "mdkg.mainRef", "trunk"]);
    let (code, out) = s.mdkg(&["validate", "--changed-only", "--json"]);
    assert_eq!(code, 0, "{out}");
    assert!(out["data"]["changed"].as_u64().unwrap_or(0) >= 2, "{out}");
}

#[test]
fn a_refused_open_in_a_ref_mode_clone_never_reads_the_checkout() {
    let s = Scratch::new();
    with_task(&s);
    // A stale graph copy in a subfolder, beside a `.git` there that git
    // skips but that may hold a store: the open is refused.
    let sub = s.repo().join("sub");
    fs::create_dir_all(&sub).expect("sub");
    let sub_arg = sub.display().to_string();
    let (code, out) = s.mdkg(&["--root", &sub_arg, "init", "--in-checkout", "--json"]);
    assert_eq!(code, 0, "{out}");
    fs::create_dir_all(sub.join(".git/mdkg")).expect("skipped .git");
    fs::write(sub.join(".git/mdkg/coord.sqlite"), b"").expect("a store that may be there");
    let (code, out) = s.mdkg(&["--root", &sub_arg, "list", "--json"]);
    assert_ne!(code, 0, "the stale checkout copy must not be read: {out}");
    assert!(out.to_string().contains("coord.store_failed"), "{out}");
}

#[test]
fn an_anchor_whose_publish_failed_is_published_by_the_next_command() {
    let s = Scratch::new();
    with_task(&s);
    let before = s.git(&["rev-parse", GRAPH]);
    let (code, out) = s.mdkg_with(&["anchor", "--json"], &[("MDKG_FAIL_AFTER", "recorded")]);
    assert_ne!(code, 0, "{out}");
    assert_eq!(s.git(&["rev-parse", GRAPH]), before, "not published yet");
    // The next writer replays the intent: an anchor holds no lease, so
    // its journal entry is its commit point.
    let (code, status) = s.mdkg(&["renew", "--json"]);
    assert_eq!(code, 0, "{status}");
    let tip = s.git(&["rev-parse", GRAPH]);
    assert_ne!(tip, before, "{status}");
    let files = s.git(&["ls-tree", "-r", "--name-only", &tip, "--", ".mdkg/anchors"]);
    assert_eq!(files.lines().count(), 1, "{files}");
    assert!(
        !status.to_string().contains("graph.session_discarded"),
        "{status}"
    );
}

#[test]
fn status_reports_a_pending_intent_without_replay_beside_a_writer() {
    let s = Scratch::new();
    with_task(&s);
    let before = s.git(&["rev-parse", GRAPH]);
    let (code, out) = s.mdkg_with(&["anchor", "--json"], &[("MDKG_FAIL_AFTER", "recorded")]);
    assert_ne!(code, 0, "{out}");
    let common = s.repo().join(".git");
    let journal = mdkg_graph::intent::Journal::open(&common).unwrap();
    let pending = journal.pending().unwrap();
    assert_eq!(pending.len(), 1);
    let cache = mdkg_graph::cache::Cache::open(&s.repo()).unwrap();
    let session = cache.session_record().unwrap();
    let conn = rusqlite::Connection::open(common.join("mdkg/coord.sqlite")).unwrap();
    conn.busy_timeout(std::time::Duration::ZERO).unwrap();
    conn.execute_batch("BEGIN IMMEDIATE;").unwrap();
    // Keep a genuine coordinator writer's reservation while status reads.
    // Its read-only open must not wait for this writer or replay the intent.
    let (code, status) = s.bounded_status();
    assert_eq!(code, 4, "{status}");
    assert!(
        status["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["code"] == "graph.intents"),
        "{status}"
    );
    conn.execute_batch("INSERT INTO meta (key, value) VALUES ('f125_probe', 'committed'); COMMIT;")
        .unwrap();
    assert_eq!(
        s.git(&["rev-parse", GRAPH]),
        before,
        "status must not publish"
    );
    assert_eq!(
        journal.pending().unwrap(),
        pending,
        "status must not replay"
    );
    assert_eq!(
        cache.session_record().unwrap(),
        session,
        "status must not repair the session"
    );
    assert_eq!(s.mdkg(&["renew", "--json"]).0, 0, "a writer still recovers");
    assert!(journal.pending().unwrap().is_empty());
    assert_ne!(s.git(&["rev-parse", GRAPH]), before);
}

#[test]
fn status_reads_leased_pending_reservations_without_replaying_them() {
    use mdkg_graph::intent::Intent;
    use mdkg_graph::refwrite::{Change, LockKind, LockMode, Request};
    for recorded in [0, 1] {
        let s = Scratch::new();
        let task = with_task(&s);
        let (code, claimed) = s.mdkg(&["--as", "a", "claim", &task, "--json"]);
        assert_eq!(code, 0, "{claimed}");
        let before = s.git(&["rev-parse", GRAPH]);
        let common = s.repo().join(".git");
        let journal = mdkg_graph::intent::Journal::open(&common).unwrap();
        if recorded == 1 {
            let (code, out) = s.mdkg_with(
                &["--as", "a", "done", &task, "--json"],
                &[("MDKG_FAIL_AFTER", "recorded")],
            );
            assert_ne!(code, 0, "{out}");
        } else {
            // Model a crash after journaling, before the store records it.
            // The normal prepared fault rolls back instead of crashing.
            let graph = mdkg_graph::graphref::resolve(&s.repo()).unwrap().unwrap();
            let writer = graph.writer(&s.repo(), LockMode::Local).unwrap();
            let lock = writer.lock(LockKind::Exclusive).unwrap();
            let path = format!(".mdkg/work/{task}.md");
            let bytes = s
                .git(&["show", &format!("{GRAPH}:{path}")])
                .replace("status: todo", "status: done")
                .into_bytes();
            let prepared = writer
                .prepare(
                    &lock,
                    &Request {
                        id: "f125-unrecorded".into(),
                        base: Some(writer.snapshot().unwrap()),
                        changes: vec![Change::Put { path, bytes }],
                        message: format!("done {task}"),
                        trailers: vec![],
                    },
                )
                .unwrap();
            journal
                .record(&Intent {
                    prepared,
                    message: format!("done {task}"),
                    actor: "a".into(),
                    leases: vec![claimed["data"]["lease"]["id"].as_str().unwrap().into()],
                    op: "done".into(),
                })
                .unwrap();
        }
        let pending = journal.pending().unwrap();
        assert_eq!(pending.len(), 1);
        assert!(!pending[0].leases.is_empty());
        let conn = rusqlite::Connection::open(common.join("mdkg/coord.sqlite")).unwrap();
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM journal", [], |r| r.get(0))
            .unwrap();
        conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        let (code, status) = s.bounded_status();
        assert_eq!(code, 4, "{status}");
        assert!(
            status["diagnostics"]
                .as_array()
                .unwrap()
                .iter()
                .any(|d| d["code"] == "graph.intents"
                    && d["message"]
                        .as_str()
                        .is_some_and(|m| m.contains(&format!("({recorded} recorded)")))),
            "{status}"
        );
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM journal", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            rows
        );
        conn.execute_batch("COMMIT").unwrap();
        assert_eq!(s.git(&["rev-parse", GRAPH]), before);
        assert_eq!(journal.pending().unwrap(), pending);
        assert_eq!(s.mdkg(&["renew", "--json"]).0, 0);
        assert!(journal.pending().unwrap().is_empty());
        assert_eq!(s.git(&["rev-parse", GRAPH]) != before, recorded == 1);
    }
}

#[test]
fn status_projects_the_bound_tip_when_the_graph_moves_after_refresh() {
    use std::os::unix::fs::PermissionsExt;
    let s = Scratch::new();
    let task = with_task(&s);
    assert_eq!(s.mdkg(&["claim", &task, "--json"]).0, 0);
    let before = s.git(&["rev-parse", GRAPH]);
    let path = format!(".mdkg/work/{task}.md");
    let cache = mdkg_graph::cache::Cache::open(&s.repo()).unwrap();
    let cached = cache.root().join(&path);
    let original = fs::read(&cached).unwrap();
    // Force the refresh path whose lock lifetime is the pre-existing gap.
    fs::set_permissions(cached.parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();
    fs::remove_file(&cached).unwrap();
    let hooks = s.0.join("git-race");
    fs::create_dir(&hooks).unwrap();
    let ready = s.0.join("projection-ready");
    let release = s.0.join("projection-release");
    let script = hooks.join("git");
    fs::write(&script, "#!/bin/sh\ncase \" $* \" in *\" ls-tree -r -l -z \"*)\n : > \"$F125_READY\"\n count=0\n while [ ! -e \"$F125_RELEASE\" ] && [ $count -lt 750 ]; do sleep 0.02; count=$((count + 1)); done;;\nesac\nexec \"$F125_REAL_GIT\" \"$@\"\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    let mut paths = vec![hooks];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let search = std::env::join_paths(paths).unwrap();
    let real = mdkg_coord_sqlite::git::git_program().0.clone();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mdkg"));
    s.isolated(&mut cmd)
        .current_dir(s.repo())
        .args(["status", "--json"])
        .env("PATH", search)
        .env("F125_READY", &ready)
        .env("F125_RELEASE", &release)
        .env("F125_REAL_GIT", real)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while !ready.exists()
        && child.try_wait().unwrap().is_none()
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    if !ready.exists() {
        child.kill().ok();
        child.wait().ok();
        panic!("status never reached its projection");
    }
    // Another clone's publication, like with_task's plain Git fixture.
    let changed = String::from_utf8(original.clone())
        .unwrap()
        .replace("Graph task", "Moved graph");
    put_on_graph(&s, &path, &changed);
    fs::write(&release, "release").unwrap();
    let result = child.wait_with_output().unwrap();
    let status: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(result.status.code(), Some(0), "{status}");
    assert_eq!(status["data"]["graph"]["tip"], before);
    assert_ne!(s.git(&["rev-parse", GRAPH]), before);
    let store = mdkg_coord_sqlite::Store::open_read_only(&s.repo().join(".git/mdkg/coord.sqlite"))
        .unwrap()
        .unwrap();
    let state = store.coordinator().unwrap();
    let expected = state.expected(&mdkg_coord::Scope::new(path));
    assert_eq!(
        expected[0].content,
        Some(mdkg_format::Digest::of_bytes(&original).to_string()),
        "only the bound tip was observed"
    );
}

#[test]
fn status_defers_a_bound_tip_that_predates_the_coordinator_snapshot() {
    use mdkg_coord::{Changes, Command as CoordCommand, LeaseId, Mode, Outcome, Scope};
    use mdkg_coord_sqlite::Store;
    use std::os::unix::fs::PermissionsExt;
    let s = Scratch::new();
    let task = with_task(&s);
    let (code, claimed) = s.mdkg(&["--as", "a", "claim", &task, "--json"]);
    assert_eq!(code, 0, "{claimed}");
    let tip_a = s.git(&["rev-parse", GRAPH]);
    let path = format!(".mdkg/work/{task}.md");
    let original = s.git(&["show", &format!("{GRAPH}:{path}")]);
    let cache = mdkg_graph::cache::Cache::open(&s.repo()).unwrap();
    let cached = cache.root().join(&path);
    fs::set_permissions(cached.parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();
    fs::remove_file(&cached).unwrap();
    let hooks = s.0.join("git-before-snapshot");
    fs::create_dir(&hooks).unwrap();
    let ready = s.0.join("refresh-ready");
    let release = s.0.join("refresh-release");
    let script = hooks.join("git");
    // This cache-only probe runs after refresh pinned A, before status loads
    // its store snapshot. Parent writes model later publication/coordination
    // without rewriting the refreshing cache.
    fs::write(&script, "#!/bin/sh\ncase \" $* \" in *\" cat-file --batch-check=%(objecttype) %(objectsize) \"*)\n : > \"$F125_READY\"\n count=0\n while [ ! -e \"$F125_RELEASE\" ] && [ $count -lt 750 ]; do sleep 0.02; count=$((count + 1)); done;;\nesac\nexec \"$F125_REAL_GIT\" \"$@\"\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    let mut paths = vec![hooks];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mdkg"));
    s.isolated(&mut cmd)
        .current_dir(s.repo())
        .args(["--as", "a", "status", "--json"])
        .env("PATH", std::env::join_paths(paths).unwrap())
        .env("F125_READY", &ready)
        .env("F125_RELEASE", &release)
        .env("F125_REAL_GIT", &mdkg_coord_sqlite::git::git_program().0)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while !ready.exists()
        && child.try_wait().unwrap().is_none()
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    if !ready.exists() {
        child.kill().ok();
        child.wait().ok();
        panic!("status never reached its refresh before the snapshot");
    }
    let body_b = original.replace("Graph task", "Newer graph B");
    put_on_graph(&s, &path, &body_b);
    let mut store = Store::open_for_repo(&s.repo(), 7_200_000).unwrap();
    store.observe_main(&s.repo(), GRAPH, ".mdkg").unwrap();
    store
        .execute(&CoordCommand::Release {
            owner: "a".into(),
            lease: LeaseId(claimed["data"]["lease"]["id"].as_str().unwrap().into()),
            now: 0,
        })
        .unwrap();
    let lease = match store
        .execute(&CoordCommand::Claim {
            owner: "c".into(),
            scope: Scope::new(&path),
            mode: Mode::Exclusive,
            now: 0,
            request: None,
            node: None,
        })
        .unwrap()
    {
        Outcome::Lease { lease } => lease,
        other => panic!("{other:?}"),
    };
    let body_c = body_b.replace("Newer graph B", "Pending change C");
    let pending = store
        .execute(&CoordCommand::Commit {
            owner: "c".into(),
            lease,
            changes: Changes::from([(
                Scope::new(&path),
                Some(mdkg_format::Digest::of_bytes(body_c.as_bytes()).to_string()),
            )]),
            source: None,
            now: 1,
            landed: false,
        })
        .unwrap();
    assert!(matches!(pending, Outcome::Committed { .. }), "{pending:?}");
    let before = store.coordinator().unwrap();
    assert_eq!(before.settlement_queue().len(), 1);
    let journal = store.journal().unwrap();
    let log = store.log_head().unwrap();
    fs::write(&release, "release").unwrap();
    let result = child.wait_with_output().unwrap();
    let status: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(result.status.code(), Some(0), "{status}");
    assert_eq!(status["data"]["graph"]["tip"], tip_a);
    assert_eq!(status["data"]["queue"][0]["state"], "sequenced");
    assert!(
        status["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["code"] == "coord.store_failed"
                && d["message"]
                    .as_str()
                    .is_some_and(|m| m.contains("bound graph tip"))),
        "{status}"
    );
    assert_eq!(store.coordinator().unwrap(), before);
    assert_eq!(store.journal().unwrap(), journal);
    assert_eq!(store.log_head().unwrap(), log);
}

#[test]
fn status_refreshes_a_stale_cache_without_blocking_a_coordinator_writer() {
    let s = Scratch::new();
    let task = with_task(&s);
    assert_eq!(s.mdkg(&["renew", "--json"]).0, 0);
    let cache = mdkg_graph::cache::Cache::open(&s.repo()).unwrap();
    let manifest = cache.manifest().unwrap();
    let path = format!(".mdkg/work/{task}.md");
    let body = s.git(&["show", &format!("{GRAPH}:{path}")]);
    put_on_graph(&s, &path, &body.replace("Graph task", "New title"));
    let conn = rusqlite::Connection::open(s.repo().join(".git/mdkg/coord.sqlite")).unwrap();
    conn.busy_timeout(std::time::Duration::ZERO).unwrap();
    conn.execute_batch("BEGIN IMMEDIATE;").unwrap();
    let (code, status) = s.bounded_status();
    assert_eq!(code, 0, "{status}");
    assert_ne!(
        cache.manifest().unwrap(),
        manifest,
        "derived cache refreshed"
    );
    conn.execute_batch("INSERT INTO meta (key, value) VALUES ('f125_cache', 'committed'); COMMIT;")
        .unwrap();
    assert_eq!(status["data"]["graph"]["tip"], s.git(&["rev-parse", GRAPH]));
}

#[test]
fn status_bounds_both_graph_lock_waits_and_reports_the_unknown_holder() {
    use mdkg_graph::refwrite::{GraphLock, LockMode};
    let s = Scratch::new();
    let task = with_task(&s);
    assert_eq!(s.mdkg(&["renew", "--json"]).0, 0);
    let path = format!(".mdkg/work/{task}.md");
    let body = s.git(&["show", &format!("{GRAPH}:{path}")]);
    put_on_graph(&s, &path, &body.replace("Graph task", "New title"));
    let common = s.repo().join(".git");
    // An exclusive holder blocks status's read lock. A shared holder
    // permits that read but blocks the stale cache's exclusive refresh.
    for lock in [
        GraphLock::exclusive as fn(&std::path::Path, LockMode) -> _,
        GraphLock::shared,
    ] {
        let held = lock(&common, LockMode::Local).unwrap();
        let started = std::time::Instant::now();
        let (code, status) = s.bounded_status();
        assert_eq!(code, 4, "{status}");
        let diag = &status["diagnostics"][0];
        assert_eq!(diag["code"], "graph.lock_required", "{status}");
        let message = diag["message"].as_str().unwrap();
        assert!(
            message.contains("holder unknown") && message.contains("mdkg/graph/lock"),
            "{status}"
        );
        assert!(started.elapsed() >= std::time::Duration::from_secs(4));
        drop(held);
    }
    assert_eq!(
        s.bounded_status().0,
        0,
        "status refreshes once the lock is free"
    );
}

#[test]
fn init_in_ref_mode_accepts_a_root_that_does_not_exist_yet() {
    let s = Scratch::new();
    s.ref_mode(true);
    let missing = s.repo().join("missing/nested").display().to_string();
    let (code, out) = s.mdkg(&["--root", &missing, "init", "--json"]);
    assert_eq!(code, 0, "{out}");
    assert!(
        s.try_git(&["rev-parse", "--verify", "--quiet", GRAPH])
            .is_some()
    );
    assert!(
        !s.repo().join("missing").exists(),
        "nothing is written there"
    );
}

#[test]
fn a_refused_open_with_a_relative_include_still_fails_closed() {
    let s = Scratch::new();
    with_task(&s);
    // A relative include, which a config read through stdin can't take.
    fs::write(s.repo().join(".git/extra.gitconfig"), "[core]\n").expect("extra");
    s.git(&["config", "include.path", "extra.gitconfig"]);
    let sub = s.repo().join("sub");
    fs::create_dir_all(&sub).expect("sub");
    let sub_arg = sub.display().to_string();
    let (code, out) = s.mdkg(&["--root", &sub_arg, "init", "--in-checkout", "--json"]);
    assert_eq!(code, 0, "{out}");
    fs::create_dir_all(sub.join(".git/mdkg")).expect("skipped .git");
    fs::write(sub.join(".git/mdkg/coord.sqlite"), b"").expect("a store that may be there");
    let (code, out) = s.mdkg(&["--root", &sub_arg, "list", "--json"]);
    assert_ne!(code, 0, "the stale checkout copy must not be read: {out}");
    assert!(out.to_string().contains("coord.store_failed"), "{out}");
}

#[test]
fn init_completes_a_creation_that_published_before_it_finished() {
    let s = Scratch::new();
    s.ref_mode(true);
    let (code, first) = s.mdkg(&["init", "--json"]);
    assert_eq!(code, 0, "{first}");
    let graph_id = first["data"]["graph_id"].as_str().expect("id").to_owned();
    // As a crash after the publish and before the identity record: the
    // creation intent is still there, the record isn't.
    s.git(&["config", "--unset", &format!("mdkg.graph.{graph_id}.ref")]);
    fs::write(
        s.repo().join(".git/mdkg/graph/creating.json"),
        format!(r#"{{"graph_id":"{graph_id}","root":"","ref":"{GRAPH}","dir":".mdkg"}}"#),
    )
    .expect("intent");
    // A dry run says it would complete it, and writes nothing.
    let (code, dry) = s.mdkg(&["init", "--dry-run", "--json"]);
    assert_eq!(code, 0, "{dry}");
    assert!(dry.to_string().contains("would complete"), "{dry}");
    assert!(
        s.try_git(&["config", "--get", &format!("mdkg.graph.{graph_id}.ref")])
            .is_none()
    );
    // The real run completes it.
    let (code, again) = s.mdkg(&["init", "--json"]);
    assert_eq!(code, 0, "{again}");
    assert_eq!(again["data"]["graph_id"], graph_id.as_str());
    assert_eq!(
        s.git(&["config", "--get", &format!("mdkg.graph.{graph_id}.ref")]),
        GRAPH
    );
}

#[test]
fn node_edit_and_save_publish_a_body_edit_through_the_graph_ref() {
    let s = Scratch::new();
    let task = with_task(&s);
    let (code, edit) = s.mdkg(&["node", "edit", &task, "--json"]);
    assert_eq!(code, 0, "{edit}");
    let copy = PathBuf::from(edit["data"]["copy"].as_str().expect("copy"));
    assert!(copy.starts_with(s.repo().join(".mdkg-edit")), "{edit}");
    assert!(!copy.starts_with(s.repo().join(".git")));
    assert!(edit["data"]["preimage"].is_string(), "{edit}");
    // The copy is excluded: the checkout has nothing to commit.
    assert!(s.git(&["status", "--porcelain"]).is_empty());
    let body = fs::read_to_string(&copy).expect("copy");
    fs::write(&copy, format!("{body}\nProgress notes.\n")).expect("edit");
    let before = s.git(&["rev-parse", GRAPH]);
    let (code, saved) = s.mdkg(&["node", "save", &task, "--request-id", "save-a", "--json"]);
    assert_eq!(code, 0, "{saved}");
    let tip = s.git(&["rev-parse", GRAPH]);
    assert_ne!(tip, before);
    assert_eq!(saved["data"]["commit"], tip.as_str());
    assert_eq!(saved["data"]["request_id"], "save-a");
    assert!(
        s.git(&["show", &format!("{tip}:.mdkg/work/{task}.md")])
            .contains("Progress notes.")
    );
    assert!(!copy.exists(), "the saved copy is removed");
    // Readers see it at once.
    let (code, show) = s.mdkg(&["show", &task, "--json"]);
    assert_eq!(code, 0, "{show}");
    assert!(show.to_string().contains("Progress notes."), "{show}");
    // A retry with the same request id is the same receipt.
    let (code, again) = s.mdkg_stdin(
        &[
            "node",
            "save",
            &task,
            "--stdin",
            "--preimage",
            edit["data"]["preimage"].as_str().expect("preimage"),
            "--request-id",
            "save-a",
            "--json",
        ],
        format!("{body}\nProgress notes.\n").as_bytes(),
    );
    assert_eq!(code, 0, "{again}");
    assert_eq!(again["data"]["commit"], tip.as_str());
    assert_eq!(s.git(&["rev-parse", GRAPH]), tip);
}

#[test]
fn node_save_from_stdin_needs_its_preimage_and_refuses_a_stale_one() {
    let s = Scratch::new();
    let task = with_task(&s);
    let path = format!(".mdkg/work/{task}.md");
    let pre = s.git(&["rev-parse", &format!("{GRAPH}:{path}")]);
    let body = s.git(&["show", &format!("{GRAPH}:{path}")]);
    let content = format!("{body}\nFrom an MCP client.\n");
    // No preimage, and no `node edit` record: refused.
    let (code, out) = s.mdkg_stdin(
        &["node", "save", &task, "--stdin", "--json"],
        content.as_bytes(),
    );
    assert_ne!(code, 0, "{out}");
    assert!(out.to_string().contains("preimage"), "{out}");
    let (code, out) = s.mdkg_stdin(
        &[
            "node",
            "save",
            &task,
            "--stdin",
            "--preimage",
            &pre,
            "--json",
        ],
        content.as_bytes(),
    );
    assert_eq!(code, 0, "{out}");
    let tip = s.git(&["rev-parse", GRAPH]);
    // The same preimage again: the graph has moved past it.
    let (code, out) = s.mdkg_stdin(
        &[
            "node",
            "save",
            &task,
            "--stdin",
            "--preimage",
            &pre,
            "--json",
        ],
        format!("{body}\nLater.\n").as_bytes(),
    );
    assert_ne!(code, 0, "{out}");
    assert!(out.to_string().contains("graph.write_stale"), "{out}");
    assert_eq!(s.git(&["rev-parse", GRAPH]), tip);
}

#[test]
fn node_edit_is_for_ref_mode_only() {
    let s = Scratch::new();
    s.ref_mode(true);
    s.git(&["config", "--unset", "mdkg.graph.ref"]);
    // Checkout mode still settles on a main branch.
    s.git(&["config", "mdkg.mainRef", "trunk"]);
    let (code, out) = s.mdkg(&["init", "--work", "shared", "--json"]);
    assert_eq!(code, 0, "{out}");
    let (code, new) = s.mdkg(&["new", "task", "A checkout task", "--json"]);
    assert_eq!(code, 0, "{new}");
    let id = new["data"]["node"]["id"]
        .as_str()
        .or_else(|| new["data"]["id"].as_str())
        .expect("id")
        .to_owned();
    for verb in ["edit", "save"] {
        let (code, out) = s.mdkg(&["node", verb, &id, "--json"]);
        assert_ne!(code, 0, "{out}");
        assert!(
            out.to_string().contains("edit the node's file in place"),
            "{verb}: {out}"
        );
    }
}

#[test]
fn a_copy_save_retried_after_it_published_returns_its_receipt() {
    // A checkout folder a shell must quote, so the hint below runs only
    // if its quoting is right (review of 38095201).
    let s = Scratch::named("my repo's");
    let task = with_task(&s);
    let (code, edit) = s.mdkg(&["node", "edit", &task, "--json"]);
    assert_eq!(code, 0, "{edit}");
    let copy = PathBuf::from(edit["data"]["copy"].as_str().expect("copy"));
    let body = fs::read_to_string(&copy).expect("copy");
    fs::write(&copy, format!("{body}\nSaved once.\n")).expect("edit");
    let (code, first) = s.mdkg(&["node", "save", &task, "--request-id", "r1", "--json"]);
    assert_eq!(code, 0, "{first}");
    let tip = s.git(&["rev-parse", GRAPH]);
    // The output was lost; the same command again: the same receipt.
    let (code, again) = s.mdkg(&["node", "save", &task, "--request-id", "r1", "--json"]);
    assert_eq!(code, 0, "{again}");
    assert_eq!(again["data"]["commit"], tip.as_str());
    assert_eq!(s.git(&["rev-parse", GRAPH]), tip);
    // An id with no earlier save is still refused: there's no copy.
    // Nothing to act on exits 3 (e2e B16), with a hint that runs as shown
    // (Terra #15584).
    let (code, other) = s.mdkg(&["node", "save", &task, "--request-id", "r2", "--json"]);
    assert_eq!(code, 3, "{other}");
    assert!(other.to_string().contains("no copy"), "{other}");
    // The hint names this save's graph and actor, so it runs from
    // anywhere as the same actor (Terra #15805).
    let hint = other["diagnostics"][0]["hint"].as_str().unwrap_or_default();
    assert!(hint.starts_with("run `mdkg --root="), "{other}");
    // The checkout's name holds a space and a quote, so --root is quoted.
    assert!(!hint.starts_with("run `mdkg --root=/"), "{other}");
    assert!(hint.contains(" --as="), "{other}");
    assert!(
        hint.ends_with(&format!(" node edit {task}` first")),
        "{other}"
    );
    // It runs as shown, through a shell, from outside the checkout
    // (e2e N1): its --root names the checkout, never ref mode's cache.
    // The shell reads its quoting (review of e1045284).
    let command = hint
        .strip_prefix("run `mdkg ")
        .and_then(|h| h.strip_suffix("` first"))
        .unwrap_or_else(|| panic!("{hint}"));
    let out = s
        .isolated(&mut Command::new("sh"))
        .current_dir(&s.0)
        .arg("-c")
        .arg(format!("exec \"$0\" {command} --json"))
        .arg(env!("CARGO_BIN_EXE_mdkg"))
        .output()
        .expect("hint runs");
    assert!(out.status.success(), "{hint}: {out:?}");
    let (code, out) = s.mdkg(&["node", "edit", &task, "--discard", "--json"]);
    assert_eq!(code, 0, "{out}");
    // A record whose copy is gone: exit 3 too, and the hint keeps `--dir`,
    // attached, so a folder starting with `-` stays a value (#15805).
    fs::create_dir_all(s.repo().join("-temp")).expect("dir");
    let (code, edit) = s.mdkg(&["node", "edit", &task, "--dir=-temp", "--json"]);
    assert_eq!(code, 0, "{edit}");
    fs::remove_file(s.repo().join(edit["data"]["copy"].as_str().expect("copy"))).expect("rm copy");
    let (code, gone) = s.mdkg(&[
        "node",
        "save",
        &task,
        "--dir=-temp",
        "--request-id",
        "r3",
        "--json",
    ]);
    assert_eq!(code, 3, "{gone}");
    assert!(gone.to_string().contains("is gone"), "{gone}");
    let hint = gone["diagnostics"][0]["hint"].as_str().unwrap_or_default();
    assert!(
        hint.ends_with(&format!(" node edit {task} --dir=-temp` again")),
        "{gone}"
    );
    assert_eq!(s.git(&["rev-parse", GRAPH]), tip);
}

#[test]
fn a_used_request_id_never_hides_an_edit_under_another_folder() {
    let s = Scratch::new();
    let task = with_task(&s);
    let (code, edit) = s.mdkg(&["node", "edit", &task, "--json"]);
    assert_eq!(code, 0, "{edit}");
    let copy = PathBuf::from(edit["data"]["copy"].as_str().expect("copy"));
    let body = fs::read_to_string(&copy).expect("copy");
    fs::write(&copy, format!("{body}\nFirst.\n")).expect("edit");
    let (code, first) = s.mdkg(&["node", "save", &task, "--request-id", "r1", "--json"]);
    assert_eq!(code, 0, "{first}");
    // A new edit under another folder.
    let elsewhere = s.0.join("elsewhere");
    fs::create_dir_all(&elsewhere).expect("dir");
    let dir = elsewhere.display().to_string();
    let (code, second) = s.mdkg(&["node", "edit", &task, "--dir", &dir, "--json"]);
    assert_eq!(code, 0, "{second}");
    let other = PathBuf::from(second["data"]["copy"].as_str().expect("copy"));
    let body = fs::read_to_string(&other).expect("copy");
    fs::write(&other, format!("{body}\nSecond.\n")).expect("edit");
    // Reusing r1 without --dir: refused, never r1's old receipt.
    let tip = s.git(&["rev-parse", GRAPH]);
    let (code, wrong) = s.mdkg(&["node", "save", &task, "--request-id", "r1", "--json"]);
    assert_ne!(code, 0, "{wrong}");
    assert!(wrong.to_string().contains("no copy"), "{wrong}");
    assert_eq!(s.git(&["rev-parse", GRAPH]), tip);
    // The other copy's record survived: saving it there publishes it.
    let (code, saved) = s.mdkg(&["node", "save", &task, "--dir", &dir, "--json"]);
    assert_eq!(code, 0, "{saved}");
    assert!(
        s.git(&["show", &format!("{GRAPH}:.mdkg/work/{task}.md")])
            .contains("Second.")
    );
}

/// The graph files the graph ref's tip holds under `.mdkg/work`.
fn tip_work_files(s: &Scratch) -> Vec<String> {
    s.git(&["ls-tree", "-r", "--name-only", GRAPH, "--", ".mdkg/work"])
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn new_publishes_its_node_on_the_graph_ref_and_writes_no_checkout() {
    let s = Scratch::new();
    with_task(&s);
    let head = s.git(&["rev-parse", "HEAD"]);
    let before = s.git(&["rev-parse", GRAPH]);
    let (code, out) = s.mdkg(&["new", "task", "Made on the ref", "--json"]);
    assert_eq!(code, 0, "{out}");
    let id = out["data"]["node"]["id"].as_str().expect("id").to_owned();
    let tip = s.git(&["rev-parse", GRAPH]);
    assert_ne!(tip, before, "the node is published");
    assert_eq!(s.git(&["rev-parse", &format!("{tip}^")]), before);
    let file = out["data"]["node"]["path"]
        .as_str()
        .expect("path")
        .to_owned();
    assert!(tip_work_files(&s).contains(&file), "{file}");
    assert!(
        s.git(&["show", &format!("{tip}:{file}")])
            .contains("title: Made on the ref")
    );
    // No checkout file and no code commit.
    assert!(!s.repo().join(".mdkg").exists());
    assert_eq!(s.git(&["rev-parse", "HEAD"]), head);
    assert!(s.git(&["status", "--porcelain"]).is_empty());
    // Readers see it at once, and nothing is left as a hand edit.
    let (code, show) = s.mdkg(&["show", &id, "--json"]);
    assert_eq!(code, 0, "{show}");
    let (code, status) = s.mdkg(&["status", "--json"]);
    assert_eq!(code, 0, "{status}");
    assert!(
        status["data"]["graph"].get("hand_edits").is_none(),
        "{status}"
    );
}

#[test]
fn done_publishes_the_done_node_on_the_graph_ref() {
    let s = Scratch::new();
    let task = with_task(&s);
    let head = s.git(&["rev-parse", "HEAD"]);
    let (code, out) = s.mdkg(&["claim", &task, "--as", "worker", "--json"]);
    assert_eq!(code, 0, "{out}");
    let before = s.git(&["rev-parse", GRAPH]);
    let (code, out) = s.mdkg(&["done", &task, "--as", "worker", "--json"]);
    assert_eq!(code, 0, "{out}");
    let tip = s.git(&["rev-parse", GRAPH]);
    assert_ne!(tip, before, "{out}");
    let file = format!(".mdkg/work/{task}.md");
    assert!(
        s.git(&["show", &format!("{tip}:{file}")])
            .contains("status: done")
    );
    assert_eq!(out["data"]["commit"], tip.as_str(), "{out}");
    assert!(!s.repo().join(".mdkg").exists());
    assert_eq!(s.git(&["rev-parse", "HEAD"]), head);
    // A second `done` finds it done on the graph ref: nothing published.
    let (code, again) = s.mdkg(&["done", &task, "--as", "worker", "--json"]);
    assert_eq!(code, 0, "{again}");
    assert_eq!(again["data"]["already_done"], true, "{again}");
    assert_eq!(s.git(&["rev-parse", GRAPH]), tip);
}

#[test]
fn format_write_publishes_the_fix_on_the_graph_ref() {
    let s = Scratch::new();
    with_task(&s);
    // Keys out of their canonical order.
    let task = "task-01a18900-0000-7000-8000-0000000000ab";
    put_on_graph(
        &s,
        &format!(".mdkg/work/{task}.md"),
        &format!(
            "---\ntitle: Unformatted\nid: {task}\ntype: task\nstatus: todo\ncreated: \
             2026-10-24T00:00:00.000Z\nupdated: 2026-10-24T00:00:00.000Z\n---\n# Overview\n"
        ),
    );
    let (code, out) = s.mdkg(&["format", "--json"]);
    assert_eq!(code, 0, "{out}");
    let file = format!(".mdkg/work/{task}.md");
    assert!(
        out.to_string().contains(&file),
        "the fixture isn't canonical: {out}"
    );
    let before = s.git(&["rev-parse", GRAPH]);
    let (code, out) = s.mdkg(&["format", "--write", "--json"]);
    assert_eq!(code, 0, "{out}");
    let tip = s.git(&["rev-parse", GRAPH]);
    assert_ne!(tip, before, "{out}");
    assert!(!s.repo().join(".mdkg").exists());
    // Canonical now, as the graph tip has it.
    let (code, out) = s.mdkg(&["format", "--json"]);
    assert_eq!(code, 0, "{out}");
    assert!(!out.to_string().contains(&file), "{out}");
}

#[test]
fn goal_activate_publishes_on_the_graph_ref() {
    let s = Scratch::new();
    with_task(&s);
    let (code, out) = s.mdkg(&["new", "goal", "Ship the ref", "--json"]);
    assert_eq!(code, 0, "{out}");
    let goal = out["data"]["node"]["id"].as_str().expect("id").to_owned();
    let before = s.git(&["rev-parse", GRAPH]);
    let (code, out) = s.mdkg(&["goal", "activate", &goal, "--json"]);
    assert_eq!(code, 0, "{out}");
    let tip = s.git(&["rev-parse", GRAPH]);
    assert_ne!(tip, before, "{out}");
    let (code, current) = s.mdkg(&["goal", "current", "--json"]);
    assert_eq!(code, 0, "{current}");
    assert!(current.to_string().contains(&goal), "{current}");
    assert!(!s.repo().join(".mdkg").exists());
}

#[test]
fn done_settles_a_lease_the_graph_ref_moved_under_and_keeps_that_change() {
    let s = Scratch::new();
    let task = with_task(&s);
    let (code, out) = s.mdkg(&["claim", &task, "--as", "worker", "--json"]);
    assert_eq!(code, 0, "{out}");
    // Another clone's change to the node reaches the graph ref.
    put_on_graph(
        &s,
        &format!(".mdkg/work/{task}.md"),
        &format!(
            "---\nid: {task}\ntype: task\ntitle: Graph task\nstatus: todo\ncreated: \
             2026-10-24T00:00:00.000Z\nupdated: 2026-10-24T00:00:00.000Z\n---\n# Overview\n\n\
             Synced from elsewhere.\n"
        ),
    );
    let (code, out) = s.mdkg(&["done", &task, "--as", "worker", "--json"]);
    assert_eq!(code, 0, "{out}");
    let tip = s.git(&["rev-parse", GRAPH]);
    let file = s.git(&["show", &format!("{tip}:.mdkg/work/{task}.md")]);
    assert!(file.contains("status: done"), "{file}");
    assert!(file.contains("Synced from elsewhere."), "{file}");
    assert!(!out.to_string().contains("git merge"), "{out}");
}

#[test]
fn concurrent_news_with_one_title_each_keep_their_own_file() {
    let s = Scratch::new();
    with_task(&s);
    // Several `new`s with the same title at once: each picks its name at
    // the graph tip under the write lock, so none replaces another's file.
    let runs: Vec<_> = (0..4)
        .map(|_| {
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_mdkg"));
            s.isolated(&mut cmd).current_dir(s.repo()).args([
                "new",
                "task",
                "Fix flaky tests",
                "--json",
            ]);
            cmd.stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .expect("mdkg runs")
        })
        .collect();
    let mut made = Vec::new();
    for run in runs {
        let out = run.wait_with_output().expect("mdkg finishes");
        let json: Value = serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|e| panic!("no JSON ({e}): {}", String::from_utf8_lossy(&out.stderr)));
        assert_eq!(out.status.code(), Some(0), "{json}");
        let id = json["data"]["node"]["id"].as_str().expect("id").to_owned();
        let path = json["data"]["node"]["path"]
            .as_str()
            .expect("path")
            .to_owned();
        made.push((id, path));
    }
    let paths: std::collections::BTreeSet<_> = made.iter().map(|(_, p)| p.clone()).collect();
    assert_eq!(paths.len(), made.len(), "a name was used twice: {made:?}");
    for (id, path) in &made {
        let file = s.git(&["show", &format!("{GRAPH}:{path}")]);
        assert!(
            file.contains(&format!("id: {id}")),
            "{path} lost {id}: {file}"
        );
    }
}

fn hint_of(v: &Value, code: &str) -> String {
    v["diagnostics"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|d| d["code"] == code)
        .and_then(|d| d["hint"].as_str())
        .unwrap_or_else(|| panic!("no {code} hint: {v}"))
        .to_owned()
}

/// Terra #16136: files set aside as unclassified are listed under
/// graph.hand_edits (an op's own unfinished copies aren't), and the hint
/// tells each source how it goes: a rescued copy is deleted, a checkout
/// file restored, a branch commit reverted.
#[test]
fn unclassified_rescues_are_listed_and_the_hint_fits_each_source() {
    let s = Scratch::new();
    let _task = with_task(&s);
    let rescued = s.repo().join(".git/mdkg/graph/rescued");
    let marker = |kind: &str, id: &str| {
        let folder = rescued.join(format!("20261103T000000Z-0a1b-{kind}-{id}"));
        fs::create_dir_all(folder.join("files/.mdkg/work")).unwrap();
        fs::write(folder.join("files/.mdkg/work/stray.md"), "stray\n").unwrap();
        let record = serde_json::json!({"v": 1, "id": id, "op": "update", "actor": "a",
            "paths": [], "kind": kind});
        fs::write(folder.join("UNFINISHED.json"), record.to_string()).unwrap();
        folder
    };
    let unclassified = marker("unclassified", "req-1");
    let unfinished = marker("unfinished", "req-2");
    let (code, status) = s.mdkg(&["status", "--json"]);
    assert_eq!(code, 0, "{status}");
    let listed: Vec<String> = status["data"]["graph"]["hand_edits"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| e["at"].as_str().map(str::to_owned))
        .collect();
    let named = |f: &PathBuf| {
        let name = f.file_name().unwrap().to_string_lossy().into_owned();
        listed.iter().any(|l| l.ends_with(&name))
    };
    assert!(named(&unclassified), "{status}");
    assert!(!named(&unfinished), "{status}");
    let hint = hint_of(&status, "graph.hand_edits");
    for part in [
        "only after the save succeeds",
        "a rescued copy is deleted",
        "`git restore --staged --worktree -- <path>`",
        "and only those, are taken back to the code base in a new commit",
        "status keeps listing that commit while it's on the branch and not in the local code base",
        "after a squash or rebase merge until the branch is rebased onto it",
        "`-unclassified-` folder, whose files no importer takes",
        "remove a rescue folder once nothing in it is wanted",
    ] {
        assert!(
            hint.to_lowercase().contains(&part.to_lowercase()),
            "{part}: {hint}"
        );
    }
    // Doctor classifies each folder and gives each kind its own advice:
    // an unfinished write is rerun, never saved by hand (Terra #16139).
    let (_, doctor) = s.mdkg(&["doctor", "--json"]);
    let check = doctor["data"]["checks"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|c| c["name"] == "graph.cache")
        .unwrap_or_else(|| panic!("graph.cache check: {doctor}"));
    let message = check["summary"].as_str().unwrap_or_default();
    assert!(
        message.contains("1 unclassified") && message.contains("1 from an unfinished write"),
        "{doctor}"
    );
    let doctored = check["hint"].as_str().unwrap_or_default();
    let lines: Vec<&str> = doctored.lines().collect();
    assert_eq!(lines.len(), 2, "{doctored}");
    assert!(lines[0].contains("no importer takes"), "{doctored}");
    assert!(!lines[0].contains("node save"), "{doctored}");
    assert!(lines[1].contains("Run that command again"), "{doctored}");
    assert!(!lines[1].contains("node save"), "{doctored}");
}

/// Terra #16139: a tracked node edited in the checkout gets the same full
/// advice from status and doctor, and it puts the tracked file back with
/// `git restore`, never a deletion.
#[test]
fn a_checkout_edit_gets_the_same_advice_from_status_and_doctor() {
    let s = Scratch::new();
    let task = with_task(&s);
    let leaf = format!(".mdkg/work/{task}.md");
    let text = s.git(&["show", &format!("{GRAPH}:{leaf}")]);
    fs::create_dir_all(s.repo().join(".mdkg/work")).unwrap();
    fs::write(s.repo().join(&leaf), format!("{text}\n")).unwrap();
    s.git(&["add", &leaf]);
    s.git(&["commit", "-q", "-m", "a tracked node"]);
    fs::write(s.repo().join(&leaf), format!("{text}\na checkout edit\n")).unwrap();
    let (code, status) = s.mdkg(&["status", "--json"]);
    assert_eq!(code, 0, "{status}");
    let listed = hint_of(&status, "graph.hand_edits");
    let (_, doctor) = s.mdkg(&["doctor", "--json"]);
    let check = doctor["data"]["checks"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|c| c["name"] == "graph.hand_edits")
        .unwrap_or_else(|| panic!("graph.hand_edits check: {doctor}"));
    assert_eq!(check["status"], "warning", "{doctor}");
    assert_eq!(check["hint"].as_str().unwrap_or_default(), listed);
    assert!(
        listed.contains("`git restore --staged --worktree -- <path>`"),
        "{listed}"
    );
    assert!(!listed.contains("delete the edited file"), "{listed}");
}

/// Review #16176: `node edit` reads a non-UTF-8 `info/exclude` by bytes,
/// finds the rule already there, and adds no second copy.
#[test]
fn node_edit_keeps_a_non_utf8_exclude_with_the_rule() {
    let s = Scratch::new();
    let task = with_task(&s);
    let exclude = s.repo().join(".git/info/exclude");
    fs::create_dir_all(exclude.parent().unwrap()).unwrap();
    let original: &[u8] = b"# caf\xe9\n/.mdkg-edit/\n";
    fs::write(&exclude, original).unwrap();
    let (code, edit) = s.mdkg(&["node", "edit", &task, "--json"]);
    assert_eq!(code, 0, "{edit}");
    assert_eq!(fs::read(&exclude).unwrap(), original);
}

/// Review #16445: `node edit` uses install's writer, so a read-only
/// exclude whose rule a later negation undoes is left alone, with the
/// warning, never replaced or given another mode.
#[test]
fn node_edit_leaves_a_read_only_exclude_alone() {
    use std::os::unix::fs::PermissionsExt;
    let s = Scratch::new();
    let task = with_task(&s);
    let exclude = s.repo().join(".git/info/exclude");
    fs::create_dir_all(exclude.parent().unwrap()).unwrap();
    let negated = "/.mdkg-edit/\n!.*\n";
    fs::write(&exclude, negated).unwrap();
    fs::set_permissions(&exclude, fs::Permissions::from_mode(0o444)).unwrap();
    let (code, edit) = s.mdkg(&["node", "edit", &task, "--json"]);
    let mode = fs::metadata(&exclude).unwrap().permissions().mode() & 0o777;
    fs::set_permissions(&exclude, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(code, 0, "{edit}");
    assert!(edit.to_string().contains("graph.edit_unexcluded"), "{edit}");
    assert_eq!(fs::read_to_string(&exclude).unwrap(), negated);
    assert_eq!(mode, 0o444);
}

/// B15: a hand edit in the graph cache is set aside, `status` reports it
/// under graph.hand_edits, status, the read that rescued it and doctor
/// give the same hint, and following it publishes the change.
#[test]
fn hand_edit_reported_by_status() {
    use std::os::unix::fs::PermissionsExt;
    let s = Scratch::new();
    let task = with_task(&s);
    let (code, out) = s.mdkg(&["show", &task, "--json"]);
    assert_eq!(code, 0, "{out}");
    let cache = mdkg_graph::cache::Cache::open(&s.repo()).unwrap();
    let cached = cache.root().join(format!(".mdkg/work/{task}.md"));
    fs::set_permissions(&cached, fs::Permissions::from_mode(0o600)).unwrap();
    let mut body = fs::read_to_string(&cached).unwrap();
    body.push_str("a hand edit\n");
    fs::write(&cached, body).unwrap();

    let (code, status) = s.mdkg(&["status", "--json"]);
    assert_eq!(code, 0, "{status}");
    let edits = status["data"]["graph"]["hand_edits"]
        .as_array()
        .unwrap_or_else(|| panic!("hand_edits listed: {status}"));
    assert!(edits.iter().any(|e| e["source"] == "rescued"), "{status}");
    let rescued = hint_of(&status, "graph.cache_edit");
    let listed = hint_of(&status, "graph.hand_edits");
    let (_, doctor) = s.mdkg(&["doctor", "--json"]);
    let check = doctor["data"]["checks"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|c| c["name"] == "graph.cache")
        .unwrap_or_else(|| panic!("graph.cache check: {doctor}"));
    assert_eq!(check["status"], "warning", "{doctor}");
    let doctored = check["hint"].as_str().unwrap_or_default().to_owned();
    // One advice, wherever it's given: the whole text after the folder
    // prefix (Terra #16139).
    let advice = |h: &str| {
        h.strip_prefix("the files are at ")
            .and_then(|r| r.split_once("; ").map(|(_, a)| a))
            .unwrap_or(h)
            .to_owned()
    };
    assert!(listed.contains("never reads them"), "{listed}");
    assert!(listed.contains("node save <id>"), "{listed}");
    assert!(listed.contains("status --json"), "{listed}");
    assert_eq!(advice(&rescued), advice(&listed));
    assert_eq!(advice(&doctored), advice(&listed));

    // Following it publishes the change: edit, apply, save.
    let (code, edit) = s.mdkg(&["node", "edit", &task, "--json"]);
    assert_eq!(code, 0, "{edit}");
    let copy = PathBuf::from(edit["data"]["copy"].as_str().expect("copy"));
    let mut body = fs::read_to_string(&copy).expect("copy");
    body.push_str("a hand edit\n");
    fs::write(&copy, body).expect("edit");
    let (code, saved) = s.mdkg(&["node", "save", &task, "--json"]);
    assert_eq!(code, 0, "{saved}");
    assert!(
        s.git(&["show", &format!("{GRAPH}:.mdkg/work/{task}.md")])
            .contains("a hand edit")
    );
}
