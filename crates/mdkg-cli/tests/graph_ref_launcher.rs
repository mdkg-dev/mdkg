//! The launcher on the graph ref (edd-01a18867, lane L5b): definition
//! writes published on the graph ref, never the checkout, and `agents up`
//! with no main and no `.worktrees/graph`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

mod common;

const GRAPH: &str = "refs/heads/mdkg-graph";

struct Scratch(PathBuf);

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
        let dir = common::temp::dir("mdkg-graph-ref-launcher");
        let dir = fs::canonicalize(dir).expect("canonical");
        fs::create_dir_all(dir.join("home")).expect("home");
        fs::create_dir_all(dir.join("repo")).expect("repo");
        Self(dir)
    }

    fn repo(&self) -> PathBuf {
        self.0.join("repo")
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
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_mdkg"));
        self.isolated(&mut cmd).current_dir(self.repo()).args(args);
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

    /// `agents up <agent> --once --json` as the launcher: its events.
    fn up(&self, agent: &str) -> Vec<Value> {
        let (code, events, err) = self.try_up(agent);
        assert_eq!(code, 0, "{events:?}\n{err}");
        events
    }

    /// `agents up <agent> --once --json` as the launcher, with every
    /// harness's fake: exit code, events, stderr.
    fn try_up(&self, agent: &str) -> (i32, Vec<Value>, String) {
        let fake = |name: &str| {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../mdkg-run/tests/fixtures")
                .join(name)
        };
        fs::create_dir_all(self.0.join("home/.codex")).expect("codex home");
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_mdkg"));
        self.isolated(&mut cmd)
            .current_dir(self.repo())
            .env("MDKG_RUN_CLAUDE", fake("fake-claude.sh"))
            .env("MDKG_RUN_CODEX", fake("fake-codex.sh"))
            .env("MDKG_RUN_OPENCODE", fake("fake-opencode.sh"))
            .env("MDKG_WAIT_POLL_MS", "20")
            .env("MDKG_RUN_SECURITY", "/usr/bin/false")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .env_remove("MDKG_RUN_MDKG")
            .args([
                "--as", "launcher", "agents", "up", agent, "--once", "--json",
            ]);
        let out = cmd.output().expect("mdkg runs");
        let text = String::from_utf8_lossy(&out.stdout);
        let events = text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{e}: {l}")))
            .collect();
        (
            out.status.code().unwrap_or(-1),
            events,
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    /// A clone in ref mode on `trunk` (no main), with one code commit and
    /// a graph made by `init` on the graph ref.
    fn ref_mode(&self) {
        self.ref_mode_with(true);
    }

    /// [`Scratch::ref_mode`], with no code commit unless `commit`.
    fn ref_mode_with(&self, commit: bool) {
        self.git(&["init", "-q", "-b", "trunk"]);
        self.git(&["config", "user.name", "Graph Test"]);
        self.git(&["config", "user.email", "graph@example.invalid"]);
        if commit {
            self.code_commit();
        }
        self.git(&["config", "mdkg.graph.ref", GRAPH]);
        let (code, out) = self.mdkg(&["init", "--work", "shared", "--json"]);
        assert_eq!(code, 0, "{out}");
    }

    fn code_commit(&self) {
        self.code_commit_named("code.rs");
    }

    fn code_commit_named(&self, file: &str) {
        fs::write(self.repo().join(file), "fn main() {}\n").expect("code");
        self.git(&["add", file]);
        self.git(&["commit", "-q", "-m", file]);
    }

    fn refs(&self) -> String {
        self.git(&["for-each-ref", "--format=%(refname) %(objectname)"])
    }

    /// The definition as the graph ref's tip holds it.
    fn definition(&self, name: &str) -> Option<Value> {
        self.try_git(&["show", &format!("{GRAPH}:.mdkg/agents/{name}.json")])
            .map(|text| serde_json::from_str(&text).expect("definition json"))
    }
}

#[test]
fn definition_writes_publish_on_the_graph_ref_and_touch_no_checkout() {
    let s = Scratch::new();
    s.ref_mode();
    let head = s.git(&["rev-parse", "HEAD"]);
    let (code, out) = s.mdkg(&[
        "agents",
        "add",
        "w",
        "--harness",
        "claude",
        "--role",
        "worker",
        "--worktree",
        "--json",
    ]);
    assert_eq!(code, 0, "{out}");
    let tip = s.git(&["rev-parse", GRAPH]);
    assert_eq!(out["data"]["commit"], tip.as_str(), "{out}");
    assert_eq!(out["data"]["changesets"].as_array().map(Vec::len), Some(1));
    let added = s.definition("w").expect("published");
    assert_eq!(added["harness"], "claude");
    assert_eq!(added["role"], "worker");
    assert!(
        s.git(&["log", "-1", "--format=%s", GRAPH])
            .starts_with("mdkg: Add agent w ["),
    );
    // Edit keeps what it isn't given.
    let (code, out) = s.mdkg(&["agents", "edit", "w", "--model", "sonnet", "--json"]);
    assert_eq!(code, 0, "{out}");
    let edited = s.definition("w").expect("still there");
    assert_eq!(edited["model"], "sonnet");
    assert_eq!(edited["harness"], "claude");
    // A no-op edit publishes nothing.
    let tip = s.git(&["rev-parse", GRAPH]);
    let (code, out) = s.mdkg(&["agents", "edit", "w", "--model", "sonnet", "--json"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(s.git(&["rev-parse", GRAPH]), tip);
    assert_eq!(out["data"]["commit"], tip.as_str());
    // Remove takes it off the graph ref.
    let (code, out) = s.mdkg(&["agents", "rm", "w", "--json"]);
    assert_eq!(code, 0, "{out}");
    assert!(s.definition("w").is_none());
    // No checkout changed: no .mdkg here, nothing to commit, HEAD as it
    // was, and only the graph ref and trunk.
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
fn a_refused_definition_publishes_nothing() {
    let s = Scratch::new();
    s.ref_mode();
    let (code, out) = s.mdkg(&[
        "agents",
        "add",
        "w",
        "--harness",
        "claude",
        "--role",
        "worker",
        "--json",
    ]);
    assert_eq!(code, 0, "{out}");
    let tip = s.git(&["rev-parse", GRAPH]);
    // Adding it again is refused, and so is removing one that isn't there.
    let (code, out) = s.mdkg(&[
        "agents",
        "add",
        "w",
        "--harness",
        "claude",
        "--role",
        "worker",
        "--json",
    ]);
    assert_ne!(code, 0, "{out}");
    let (code, out) = s.mdkg(&["agents", "rm", "nobody", "--json"]);
    assert_ne!(code, 0, "{out}");
    assert_eq!(s.git(&["rev-parse", GRAPH]), tip);
    // No lease is left behind by either.
    let (code, status) = s.mdkg(&["status", "--json"]);
    assert_eq!(code, 0, "{status}");
    let leases = status["data"]["leases"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(leases.is_empty(), "{leases:?}");
}

/// A worker in ref mode, with a note waiting for it.
fn worker_with_mail(s: &Scratch) {
    harness_worker_with_mail(s, "w", "claude");
}

fn harness_worker_with_mail(s: &Scratch, agent: &str, harness: &str) {
    let (code, out) = s.mdkg(&[
        "agents",
        "add",
        agent,
        "--harness",
        harness,
        "--role",
        "worker",
        "--worktree",
        "--json",
    ]);
    assert_eq!(code, 0, "{out}");
    let (code, out) = s.mdkg(&[
        "--as",
        "orchestrator",
        "msg",
        "send",
        "--to",
        agent,
        "--kind",
        "note",
        "--text",
        "go",
        "--json",
    ]);
    assert_eq!(code, 0, "{out}");
}

fn replied(events: &[Value]) -> bool {
    events
        .iter()
        .any(|e| e["data"]["turn"]["outcome"] == "replied")
}

fn saved_state(s: &Scratch, agent: &str) -> Value {
    let path = s.repo().join(format!(".git/mdkg/run/{agent}.json"));
    serde_json::from_slice(&fs::read(path).expect("state")).expect("json")
}

#[test]
fn agents_up_runs_with_no_main_and_no_graph_worktree() {
    let s = Scratch::new();
    s.ref_mode();
    worker_with_mail(&s);
    let trunk = s.git(&["rev-parse", "trunk"]);
    let events = s.up("w");
    assert!(replied(&events), "{events:?}");
    // No `.worktrees/graph`, no main, and the worker's worktree is cut
    // from the main checkout's branch, recorded as its work base.
    assert!(!s.repo().join(".worktrees/graph").exists());
    assert!(
        s.try_git(&["rev-parse", "--verify", "--quiet", "refs/heads/main"])
            .is_none()
    );
    let state = saved_state(&s, "w");
    assert_eq!(state["work_base"]["commit"], trunk.as_str(), "{state}");
    assert_eq!(state["work_base"]["from"], "refs/heads/trunk");
    assert_eq!(state["work_base"]["source"], "checkout");
    assert!(
        state["work_base"].get("notes").is_none(),
        "no mainRef, nothing to note"
    );
    assert!(!s.repo().join(".mdkg").exists());
    // A record lost (a crash before the state was saved) is read back from
    // the branch's reflog at the next start.
    let path = s.repo().join(".git/mdkg/run/w.json");
    let mut lost = state.clone();
    lost.as_object_mut().expect("object").remove("work_base");
    fs::write(&path, lost.to_string()).expect("write state");
    let (code, out) = s.mdkg(&[
        "--as",
        "orchestrator",
        "msg",
        "send",
        "--to",
        "w",
        "--kind",
        "note",
        "--text",
        "again",
        "--json",
    ]);
    assert_eq!(code, 0, "{out}");
    // trunk has moved on, so the resolver no longer names that commit:
    // the reflog's record stands, as recovered.
    s.code_commit_named("later.rs");
    let events = s.up("w");
    assert!(replied(&events), "{events:?}");
    let state = saved_state(&s, "w");
    assert_eq!(state["work_base"]["commit"], trunk.as_str(), "{state}");
    assert_eq!(state["work_base"]["source"], "recovered");
    // Lost again with trunk back at that commit: the resolver agrees, and
    // its source is the record's.
    s.git(&["reset", "-q", "--hard", &trunk]);
    let mut lost = saved_state(&s, "w");
    lost.as_object_mut().expect("object").remove("work_base");
    fs::write(&path, lost.to_string()).expect("write state");
    let (code, out) = s.mdkg(&[
        "--as",
        "orchestrator",
        "msg",
        "send",
        "--to",
        "w",
        "--kind",
        "note",
        "--text",
        "third",
        "--json",
    ]);
    assert_eq!(code, 0, "{out}");
    assert!(replied(&s.up("w")));
    let state = saved_state(&s, "w");
    assert_eq!(state["work_base"]["commit"], trunk.as_str(), "{state}");
    assert_eq!(state["work_base"]["source"], "checkout");
}

#[test]
fn a_main_ref_not_here_is_noted_in_the_saved_state() {
    let s = Scratch::new();
    s.ref_mode();
    s.git(&["config", "mdkg.mainRef", "origin/main"]);
    worker_with_mail(&s);
    let events = s.up("w");
    assert!(replied(&events), "{events:?}");
    let state = saved_state(&s, "w");
    assert_eq!(state["work_base"]["source"], "checkout", "{state}");
    let notes = state["work_base"]["notes"].to_string();
    assert!(notes.contains("isn't in this clone"), "{state}");
    assert_eq!(state["work_base"]["turns"], 0);
}

#[test]
fn agents_up_cuts_from_origin_main_and_moves_no_other_ref() {
    let s = Scratch::new();
    s.ref_mode();
    let first = s.git(&["rev-parse", "HEAD"]);
    s.git(&["update-ref", "refs/remotes/origin/main", &first]);
    s.git(&["config", "mdkg.mainRef", "origin/main"]);
    fs::write(s.repo().join("later.rs"), "// later\n").expect("w");
    s.git(&["add", "later.rs"]);
    s.git(&["commit", "-q", "-m", "later"]);
    worker_with_mail(&s);
    let before = s.refs();
    let events = s.up("w");
    assert!(replied(&events), "{events:?}");
    let state = saved_state(&s, "w");
    assert_eq!(state["work_base"]["commit"], first.as_str(), "{state}");
    assert_eq!(state["work_base"]["from"], "refs/remotes/origin/main");
    assert_eq!(state["work_base"]["source"], "main_ref");
    assert_eq!(
        s.git(&["-C", ".worktrees/w", "rev-parse", "HEAD"]),
        first,
        "the fake makes no commit"
    );
    // Only the agent's branch is new, and the graph ref may move (the
    // turn's mail and receipt). origin/main and trunk are as they were.
    let after = s.refs();
    for line in before.lines() {
        if !line.starts_with(GRAPH) {
            assert!(after.contains(line), "{line} moved:\n{after}");
        }
    }
    for line in after.lines() {
        assert!(
            before.contains(line)
                || line.starts_with(GRAPH)
                || line.starts_with("refs/heads/agent/w "),
            "new ref {line}"
        );
    }
    assert!(
        s.git(&["reflog", "show", "--format=%H", "refs/remotes/origin/main"])
            .lines()
            .count()
            <= 1
    );
}

/// C22: `agents up` from the primary checkout, with no `.worktrees/graph`
/// and no main, on every harness.
#[test]
fn agents_up_from_the_primary_checkout_on_every_harness() {
    let s = Scratch::new();
    s.ref_mode();
    let trunk = s.git(&["rev-parse", "trunk"]);
    for harness in ["claude", "codex", "opencode"] {
        let agent = format!("w-{harness}");
        harness_worker_with_mail(&s, &agent, harness);
        let events = s.up(&agent);
        assert!(replied(&events), "{harness}: {events:?}");
        let state = saved_state(&s, &agent);
        assert_eq!(state["work_base"]["commit"], trunk.as_str(), "{state}");
    }
    assert!(!s.repo().join(".worktrees/graph").exists());
    assert!(!s.repo().join(".mdkg").exists());
}

/// C1: an unborn code base refuses a worker's turn with `run.no_base`
/// until the first code commit, then runs.
#[test]
fn an_unborn_code_base_refuses_the_worker_until_the_first_commit() {
    let s = Scratch::new();
    s.ref_mode_with(false);
    worker_with_mail(&s);
    let (code, events, err) = s.try_up("w");
    let all = format!("{events:?}{err}");
    assert!(all.contains("run.no_base"), "{code}: {all}");
    assert!(!s.repo().join(".worktrees/w").exists());
    assert!(
        s.try_git(&["rev-parse", "--verify", "--quiet", "refs/heads/agent/w"])
            .is_none()
    );
    s.code_commit();
    let events = s.up("w");
    assert!(replied(&events), "{events:?}");
}

/// G15: `follow: "merge"` in a committed definition runs under `agents up`
/// and follows by notes: the prompt says how far behind mainRef the branch
/// is and that automatic merge is unavailable, nothing is merged, and
/// `agents status` shows the lag. `--unset follow` takes it off.
#[test]
fn follow_merge_is_notes_in_this_release() {
    let s = Scratch::new();
    s.ref_mode();
    let first = s.git(&["rev-parse", "HEAD"]);
    s.git(&["update-ref", "refs/remotes/origin/main", &first]);
    s.git(&["config", "mdkg.mainRef", "origin/main"]);
    let file = s.0.join("w.json");
    fs::write(
        &file,
        r#"{"schema":"mdkg.agent.v1","harness":"claude","role":"worker","worktree":true,"follow":"merge"}"#,
    )
    .expect("definition");
    let (code, out) = s.mdkg(&[
        "agents",
        "add",
        "w",
        "--file",
        file.to_str().expect("utf-8"),
        "--json",
    ]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(s.definition("w").expect("added")["follow"], "merge");
    let (code, out) = s.mdkg(&[
        "--as",
        "orchestrator",
        "msg",
        "send",
        "--to",
        "w",
        "--kind",
        "note",
        "--text",
        "one",
        "--json",
    ]);
    assert_eq!(code, 0, "{out}");
    let events = s.up("w");
    assert!(replied(&events), "{events:?}");
    // Upstream moves on (as a fetch would move origin/main).
    fs::write(s.repo().join("upstream.rs"), "// upstream\n").expect("w");
    s.git(&["add", "upstream.rs"]);
    s.git(&["commit", "-q", "-m", "upstream"]);
    let upstream = s.git(&["rev-parse", "HEAD"]);
    s.git(&["update-ref", "refs/remotes/origin/main", &upstream]);
    let (code, status) = s.mdkg(&["agents", "status", "--json"]);
    assert_eq!(code, 0, "{status}");
    let w = status["data"]["agents"]
        .as_array()
        .and_then(|a| a.iter().find(|x| x["name"] == "w"))
        .cloned()
        .expect("w in status");
    assert_eq!(
        w["code_base"]["main_ref"], "refs/remotes/origin/main",
        "{w}"
    );
    assert_eq!(w["code_base"]["behind"], 1, "{w}");
    // And in the text a person reads.
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mdkg"));
    s.isolated(&mut cmd)
        .current_dir(s.repo())
        .args(["agents", "status"]);
    let text = String::from_utf8_lossy(&cmd.output().expect("status").stdout).into_owned();
    assert!(
        text.contains("  code base: 0 ahead, 1 behind refs/remotes/origin/main"),
        "{text}"
    );
    let (code, out) = s.mdkg(&[
        "--as",
        "orchestrator",
        "msg",
        "send",
        "--to",
        "w",
        "--kind",
        "note",
        "--text",
        "two",
        "--json",
    ]);
    assert_eq!(code, 0, "{out}");
    let head = s.git(&["-C", ".worktrees/w", "rev-parse", "HEAD"]);
    let events = s.up("w");
    assert!(replied(&events), "{events:?}");
    let prompt = fs::read_to_string(s.repo().join(".worktrees/w/claude.prompt")).expect("prompt");
    assert!(
        prompt.contains(
            "your branch is 1 commit behind origin/main and 0 ahead; merge it when it suits \
             the work (`git merge 'refs/remotes/origin/main'`)"
        ),
        "{prompt}"
    );
    assert!(
        prompt.contains("is not available in this release"),
        "{prompt}"
    );
    // Nothing merged, and origin/main and trunk are as they were.
    assert_eq!(s.git(&["-C", ".worktrees/w", "rev-parse", "HEAD"]), head);
    assert_eq!(s.git(&["rev-parse", "refs/remotes/origin/main"]), upstream);
    assert_eq!(s.git(&["rev-parse", "trunk"]), upstream);
    // The setting can be taken off.
    let (code, out) = s.mdkg(&["agents", "edit", "w", "--unset", "follow", "--json"]);
    assert_eq!(code, 0, "{out}");
    assert!(
        s.definition("w")
            .expect("still there")
            .get("follow")
            .is_none()
    );
}
