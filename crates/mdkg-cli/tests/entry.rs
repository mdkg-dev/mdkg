//! `mdkg entry` and `done`'s gate through the CLI, against real git
//! (edd-01a1878e, A27 and A29): SHAs resolve through the repository's
//! common directory, classes come from agent definitions, and `done`
//! refuses while the gate fails.
use serde_json::{Value, json};
use std::{fs, path::PathBuf, process::Command};
mod common;

const NODE: &str = "task-01a1878e-3e6f-710b-9d01-68d002aed3e3";
const NODE_PATH: &str = ".mdkg/work/task-ledger.md";
const GRAPH: &str = "0192f3a0-0000-7000-8000-000000000000";

/// The lease length a store these tests open in-process is created with:
/// the CLI's own default, two hours. The first open creates the store with
/// it, and a claim made after that lasts that long, so a short one let a
/// claim expire before `done` on a loaded machine (the flake in
/// `a_settle_refused_after_the_commit_leaves_it_unsettled_until_a_rerun`).
const STORE_TTL_MS: u64 = 2 * 60 * 60 * 1_000;

struct Fixture {
    dir: PathBuf,
    repo: PathBuf,
    home: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.dir).ok();
    }
}

fn agent(role: &str) -> String {
    json!({
        "schema": "mdkg.agent.v1",
        "harness": "codex",
        "role": role,
        "reports_to": "orchestrator",
        "mode": "auto"
    })
    .to_string()
}

/// Anchoring's view for [`Fixture::open_round`]: no main ref, so a node's
/// first round anchors.
struct NoMain;

impl mdkg_coord_sqlite::entries::Env for NoMain {
    fn node(
        &self,
        h: &str,
    ) -> Result<mdkg_coord_sqlite::ledger::NodeLink, mdkg_coord_sqlite::entries::EntryError> {
        mdkg_coord_sqlite::ledger::NodeLink::of(h)
            .map_err(|_| mdkg_coord_sqlite::entries::EntryError::NodeNotFound(h.into()))
    }
    fn commit(&self, sha: &str) -> Result<String, mdkg_coord_sqlite::entries::EntryError> {
        Ok(sha.into())
    }
    fn contains(&self, _a: &str, _h: &str) -> Result<bool, mdkg_coord_sqlite::entries::EntryError> {
        Ok(false)
    }
    fn holds(&self, _actor: &str, _role: &str) -> bool {
        false
    }
    fn main_tips(&self) -> Result<Option<Vec<String>>, mdkg_coord_sqlite::entries::EntryError> {
        Ok(None)
    }
}

impl Fixture {
    /// A git repository with one task node (with `body` under its front
    /// matter), a reviewer `rev` and a worker `wrk`, both reporting to
    /// `orchestrator`, all committed on main.
    fn new(body: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("mdkg-entry-{}", mdkg_format::Uuid7::now()));
        fs::create_dir_all(&dir).expect("entry fixture");
        let dir = fs::canonicalize(dir).expect("entry fixture");
        let f = Self {
            repo: dir.join("repo"),
            home: dir.join("home"),
            dir,
        };
        fs::create_dir_all(f.repo.join(".mdkg/work")).expect("entry fixture");
        fs::create_dir_all(f.repo.join(".mdkg/agents")).expect("entry fixture");
        fs::create_dir_all(&f.home).expect("entry fixture");
        fs::write(
            f.repo.join(".mdkg/config.json"),
            json!({
                "schema_version": 3, "format_version": "0.7", "graph_id": GRAPH,
                "work": {"location": "git"}
            })
            .to_string(),
        )
        .expect("entry fixture");
        fs::write(f.repo.join(".mdkg/agents/rev.json"), agent("reviewer")).expect("agent");
        fs::write(f.repo.join(".mdkg/agents/wrk.json"), agent("worker")).expect("agent");
        f.write_node("todo", body);
        fs::write(f.repo.join(".gitignore"), ".worktrees/\n").expect("ignore");
        f.git(&["init", "-q", "-b", "main"]);
        f.git(&["add", "."]);
        f.git(&["commit", "-qm", "fixture"]);
        // The graph checkout, on mdkg-graph, which classes come from once
        // it's registered (checkout mode; the orchestrator on #10737).
        f.git(&[
            "worktree",
            "add",
            "-q",
            ".worktrees/graph",
            "-b",
            "mdkg-graph",
        ]);
        f.git(&["config", &format!("mdkg.graph.{GRAPH}.worktree"), "graph"]);
        f
    }

    fn graph(&self) -> PathBuf {
        self.repo.join(".worktrees/graph")
    }

    /// Commits `text` at `path` in the graph checkout.
    fn commit_graph(&self, path: &str, text: &str) {
        let graph = self.graph();
        fs::write(graph.join(path), text).expect("graph write");
        let g = graph.to_str().expect("graph");
        self.git(&["-C", g, "add", path]);
        self.git(&["-C", g, "commit", "-qm", path]);
    }

    fn write_node(&self, status: &str, body: &str) {
        fs::write(
            self.repo.join(NODE_PATH),
            format!(
                "---\nid: {NODE}\ntype: task\ntitle: Ledger fixture\nstatus: {status}\n\
                 created: 2026-10-29T00:00:00.000Z\nupdated: 2026-10-29T00:00:00.000Z\n---\n{body}"
            ),
        )
        .expect("node");
    }

    fn env<'a>(&self, cmd: &'a mut Command) -> &'a mut Command {
        common::without_launcher_env(cmd)
            .current_dir(&self.repo)
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("XDG_DATA_HOME", self.home.join(".local/share"))
            .env("CODEX_HOME", self.home.join(".codex"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .env_remove("MDKG_RUN_MDKG")
            .env("MDKG_RUN_SECURITY", "/usr/bin/false")
    }

    fn git(&self, args: &[&str]) -> String {
        let out = self
            .env(&mut Command::new("git"))
            .args(args)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A commit touching `path`, returning its full SHA.
    fn commit(&self, path: &str, text: &str) -> String {
        let file = self.repo.join(path);
        fs::create_dir_all(file.parent().expect("parent")).expect("dir");
        fs::write(&file, text).expect("write");
        self.git(&["add", path]);
        self.git(&["commit", "-qm", path]);
        self.git(&["rev-parse", "HEAD"])
    }

    /// Runs `mdkg --as <actor> <args> --json`: its exit code and envelope.
    fn run(&self, actor: &str, args: &[&str]) -> (i32, Value) {
        self.run_with(actor, args, &[])
    }

    /// [`Fixture::run`] with `vars` set in its environment.
    fn run_with(&self, actor: &str, args: &[&str], vars: &[(&str, &str)]) -> (i32, Value) {
        let mut c = Command::new(env!("CARGO_BIN_EXE_mdkg"));
        let out = self
            .env(&mut c)
            .envs(vars.iter().copied())
            .args(["--root", self.repo.to_str().expect("root"), "--as", actor])
            .args(args)
            .arg("--json")
            .output()
            .expect("mdkg");
        let envelope: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
            panic!(
                "mdkg {args:?} printed no envelope ({e}): {}\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
        });
        (out.status.code().unwrap_or(-1), envelope)
    }

    fn ok(&self, actor: &str, args: &[&str]) -> Value {
        let (code, envelope) = self.run(actor, args);
        assert_eq!(code, 0, "mdkg {args:?}: {envelope:#}");
        envelope["data"].clone()
    }

    /// The code a refused command gave, with its exit code.
    fn refused(&self, actor: &str, args: &[&str]) -> (i32, String) {
        let (code, envelope) = self.run(actor, args);
        assert_ne!(code, 0, "mdkg {args:?} succeeded: {envelope:#}");
        let first = envelope["diagnostics"]
            .as_array()
            .and_then(|d| d.iter().find(|d| d["severity"] == "error"))
            .and_then(|d| d["code"].as_str())
            .unwrap_or_default()
            .to_string();
        (code, first)
    }

    /// Opens a review round over `base..head` on NODE as a review request
    /// `id` would (the mail call site isn't wired yet), as rev's.
    fn open_round(&self, base: &str, head: &str, id: &str) {
        self.open_round_for(base, head, id, &["rev", "rev2"]);
    }

    fn open_round_for(&self, base: &str, head: &str, id: &str, reviewers: &[&str]) {
        let mut store =
            mdkg_coord_sqlite::Store::open_for_repo(&self.repo, STORE_TTL_MS).expect("store");
        let node = mdkg_coord_sqlite::ledger::NodeLink::of(NODE).expect("node");
        store
            .entry_open_round(
                &node,
                "review",
                (base, head),
                &reviewers.iter().map(|r| r.to_string()).collect::<Vec<_>>(),
                "orchestrator",
                id,
                1,
                false,
                &NoMain,
            )
            .expect("round");
    }

    fn state(&self, entry: &str) -> String {
        let data = self.ok("rev", &["entry", "show", NODE, entry]);
        data["entry"]["state"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }
}

#[test]
fn fix_and_verify_resolve_real_shas_through_the_common_directory() {
    let f = Fixture::new("Body.\n");
    f.ok(
        "rev",
        &[
            "entry",
            "add",
            NODE,
            "--kind",
            "finding",
            "--title",
            "Off by one",
        ],
    );
    let base = f.git(&["rev-parse", "HEAD"]);
    let fix = f.commit("src/lib.rs", "fixed\n");
    let short = &fix[..10];
    f.ok("wrk", &["entry", "fix", NODE, "F1", "--sha", short]);
    let data = f.ok("rev", &["entry", "show", NODE, "F1"]);
    assert_eq!(data["entry"]["state"], "fixed");
    assert_eq!(data["entry"]["fix"]["sha"], fix.as_str(), "{data:#}");
    let later = f.commit("src/other.rs", "later\n");
    f.open_round(&base, &later, "m1");
    f.ok("rev", &["entry", "verify", NODE, "F1", "--at", &later]);
    assert_eq!(f.state("F1"), "verified");
    // The verify recorded its fix commit set and paths, read through git:
    // the walk's base..fix and the fix commit's own paths.
    let data = f.ok("rev", &["entry", "show", NODE, "F1"]);
    let verified = &data["entry"]["verified"];
    assert_eq!(verified["fix_commits"], json!([fix]), "{data:#}");
    assert_eq!(verified["fix_paths"], json!(["src/lib.rs"]), "{data:#}");
    // A SHA that isn't in the repository is refused, never passed.
    f.ok(
        "rev",
        &[
            "entry", "add", NODE, "--kind", "finding", "--title", "Second",
        ],
    );
    let (_, code) = f.refused(
        "wrk",
        &[
            "entry",
            "fix",
            NODE,
            "F2",
            "--sha",
            "0123456789abcdef0123456789abcdef01234567",
        ],
    );
    assert_eq!(code, "entry.unknown_commit");
    // A verify at a SHA that doesn't hold the fix: not covered.
    let side_base = f.git(&["rev-parse", "HEAD~2"]);
    f.ok("wrk", &["entry", "fix", NODE, "F2", "--sha", &later]);
    let (_, code) = f.refused("rev", &["entry", "verify", NODE, "F2", "--at", &side_base]);
    assert_eq!(code, "entry.not_covered");
}

/// A29 condition 2 through git: a verified fix is contained while HEAD
/// holds the reviewed walk's paths as its final SHA does, the node file
/// compared without its status, updated and `# Ledger`; a later change
/// to a reviewed path fails until a round covers it.
#[test]
fn the_gate_compares_the_reviewed_walk_at_head() {
    let f = Fixture::new("Body.\n");
    f.ok(
        "rev",
        &[
            "entry",
            "add",
            NODE,
            "--kind",
            "finding",
            "--title",
            "Off by one",
        ],
    );
    let base = f.git(&["rev-parse", "HEAD"]);
    let fix = f.commit("src/lib.rs", "fixed\n");
    f.ok("wrk", &["entry", "fix", NODE, "F1", "--sha", &fix]);
    // The round also reviews an edit to the node's own body.
    f.write_node("todo", "Body, reviewed.\n");
    let head = f.commit(
        NODE_PATH,
        &std::fs::read_to_string(f.repo.join(NODE_PATH)).expect("node"),
    );
    f.open_round_for(&base, &head, "m1", &["rev"]);
    f.ok("rev", &["entry", "verify", NODE, "F1", "--at", &head]);
    let range = format!("{base}..{head}");
    f.ok(
        "rev",
        &[
            "entry",
            "complete",
            NODE,
            "V1",
            "--range",
            &range,
            "--verdict",
            "clean",
        ],
    );
    let data = f.ok("rev", &["entry", "gate", NODE]);
    assert_eq!(data["ok"], true, "{data:#}");
    // `done`'s own edits to the node file don't count.
    let done = std::fs::read_to_string(f.repo.join(NODE_PATH))
        .expect("node")
        .replace("status: todo", "status: done")
        .replace(
            "updated: 2026-10-29T00:00:00.000Z",
            "updated: 2026-10-30T00:00:00.000Z",
        )
        + "# Ledger\n\n<!-- mdkg:ledger begin upto=5 -->\n- F1 major verified@0123456789ab by rev: Broken\n<!-- mdkg:ledger end -->\n";
    f.commit(NODE_PATH, &done);
    let data = f.ok("rev", &["entry", "gate", NODE]);
    assert_eq!(data["ok"], true, "{data:#}");
    // A later change to a reviewed path fails on content.
    f.commit("src/lib.rs", "changed after review\n");
    // A failed gate is a check that found errors: exit 2, the envelope
    // not ok, its data still the answer (e2e UX; orchestrator #17713).
    let (code, envelope) = f.run("rev", &["entry", "gate", NODE]);
    assert_eq!(code, 2, "{envelope:#}");
    assert_eq!(envelope["ok"], false, "{envelope:#}");
    assert_eq!(
        envelope["diagnostics"][0]["code"], "entry.gate_failed",
        "{envelope:#}"
    );
    let data = envelope["data"].clone();
    assert_eq!(data["ok"], false, "{data:#}");
    let failure = &data["failures"][0];
    assert_eq!(failure["reason"], "not_covered", "{data:#}");
    assert_eq!(failure["data"]["paths"], json!(["src/lib.rs"]), "{data:#}");
    assert_eq!(failure["data"]["final"], head.as_str(), "{data:#}");
    // As text, its data prints one field a line, never JSON (review of
    // 4d8a588f).
    let mut c = Command::new(env!("CARGO_BIN_EXE_mdkg"));
    let out = f
        .env(&mut c)
        .args(["--root", f.repo.to_str().expect("root"), "--as", "rev"])
        .args(["entry", "gate", NODE])
        .output()
        .expect("gate");
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("\n    data:\n"), "{text}");
    assert!(
        text.contains("\n      paths:\n        - src/lib.rs\n"),
        "{text}"
    );
    assert!(text.contains(&format!("\n      final: {head}\n")), "{text}");
    assert!(!text.contains('{') && !text.contains('"'), "{text}");
}

#[test]
fn the_gate_at_an_unknown_sha_is_refused_never_checked_at_head() {
    let f = Fixture::new("Body.\n");
    let head = f.git(&["rev-parse", "HEAD"]);
    let data = f.ok("rev", &["entry", "gate", NODE]);
    assert_eq!(data["at"], head.as_str());
    assert_eq!(data["ok"], true);
    let data = f.ok("rev", &["entry", "gate", NODE, "--at", &head[..12]]);
    assert_eq!(data["at"], head.as_str());
    for bad in [
        "0123456789abcdef0123456789abcdef01234567",
        "main",
        "deadbeefx",
    ] {
        let (_, code) = f.refused("rev", &["entry", "gate", NODE, "--at", bad]);
        assert_eq!(code, "entry.unknown_commit", "--at {bad}");
    }
}

#[test]
fn classes_come_from_the_graph_branch_not_the_checkout() {
    let f = Fixture::new("Body.\n");
    // The graph checkout defines a new reviewer this checkout lacks.
    f.commit_graph(".mdkg/agents/rev2.json", &agent("reviewer"));
    assert!(!f.repo.join(".mdkg/agents/rev2.json").exists());
    // And the checkout edits its worker into a reviewer, uncommitted.
    fs::write(f.repo.join(".mdkg/agents/wrk.json"), agent("reviewer")).expect("agent");
    let base = f.git(&["rev-parse", "HEAD"]);
    let fix = f.commit("src/lib.rs", "fixed\n");
    f.open_round(&base, &fix, "m1");
    for (actor, title) in [("rev2", "From rev2"), ("wrk", "From wrk")] {
        let (code, envelope) = f.run(
            actor,
            &["entry", "add", NODE, "--kind", "finding", "--title", title],
        );
        if actor == "rev2" {
            assert_eq!(
                code, 0,
                "a graph-branch reviewer adds findings: {envelope:#}"
            );
        } else {
            assert_ne!(
                code, 0,
                "the checkout's edit makes no reviewer: {envelope:#}"
            );
        }
    }
    f.ok("wrk", &["entry", "fix", NODE, "F1", "--sha", &fix]);
    let (_, code) = f.refused("wrk", &["entry", "verify", NODE, "F1", "--at", &fix]);
    assert_eq!(code, "entry.not_allowed");
    f.ok("rev2", &["entry", "verify", NODE, "F1", "--at", &fix]);
    assert_eq!(f.state("F1"), "verified");
}

#[test]
fn a_ref_named_like_a_hex_prefix_is_never_taken_for_a_sha() {
    let f = Fixture::new("Body.\n");
    let head = f.git(&["rev-parse", "HEAD"]);
    // A branch and a tag whose names look like SHA prefixes no object has.
    let fake = if head.starts_with("deadbeef") {
        "cafef00d"
    } else {
        "deadbeef"
    };
    f.git(&["branch", fake]);
    f.git(&["tag", "abcdef12"]);
    for name in [fake, "abcdef12"] {
        let (_, code) = f.refused("rev", &["entry", "gate", NODE, "--at", name]);
        assert_eq!(code, "entry.unknown_commit", "--at {name}");
    }
    // A real abbreviation still resolves, to the commit.
    let data = f.ok("rev", &["entry", "gate", NODE, "--at", &head[..9]]);
    assert_eq!(data["at"], head.as_str());
}

fn refused_class(f: &Fixture, actor: &str) -> String {
    let (_, code) = f.refused(
        actor,
        &[
            "entry", "add", NODE, "--kind", "finding", "--title", "Refused",
        ],
    );
    code
}

/// `entry add` without `--title` names the missing flag, never "not a
/// ledger op" (e2e N5).
#[test]
fn a_missing_title_is_named_with_its_flag() {
    let f = Fixture::new("Body.\n");
    let (code, envelope) = f.run(
        "rev",
        &["entry", "add", NODE, "--kind", "finding", "--text", "x"],
    );
    assert_eq!(code, 1, "{envelope:#}");
    let d = &envelope["diagnostics"][0];
    assert_eq!(d["code"], "entry.bad_op", "{envelope:#}");
    assert_eq!(
        d["message"], "the op's title is missing, not valid, or not one this op takes",
        "{envelope:#}"
    );
    let hint = d["hint"].as_str().unwrap_or_default();
    assert!(hint.starts_with("check `--title`"), "{envelope:#}");
    // A positional field names its argument, never a flag (review of
    // 6baee0b9).
    let (_, envelope) = f.run(
        "rev",
        &[
            "entry",
            "complete",
            NODE,
            "--range",
            "a..b",
            "--verdict",
            "clean",
        ],
    );
    let hint = envelope["diagnostics"][0]["hint"]
        .as_str()
        .unwrap_or_default();
    assert!(
        !hint.contains("--round")
            && hint.starts_with("check the [ENTRY] argument (`round` in an op file)"),
        "{envelope:#}"
    );
}

#[test]
fn classes_come_from_graph_ref_objects_and_fail_closed() {
    let f = Fixture::new("Body.\n");
    // An uncommitted edit in the graph checkout grants nothing: classes are
    // read as git objects at the graph ref, never from a working tree.
    fs::write(f.graph().join(".mdkg/agents/wrk.json"), agent("reviewer")).expect("agent");
    assert_eq!(refused_class(&f, "wrk"), "entry.not_allowed");
    f.ok(
        "rev",
        &[
            "entry", "add", NODE, "--kind", "finding", "--title", "Still",
        ],
    );
    // An invalid committed definition refuses everyone's class, with a hint.
    f.commit_graph(".mdkg/agents/wrk.json", "{\"schema\": \"mdkg.agent.v1\"}");
    let (_, envelope) = f.run(
        "rev",
        &[
            "entry", "add", NODE, "--kind", "finding", "--title", "Refused",
        ],
    );
    let d = &envelope["diagnostics"][0];
    assert_eq!(d["code"], "entry.not_allowed", "{envelope:#}");
    assert!(
        d["hint"].as_str().unwrap_or_default().contains("graph ref"),
        "{envelope:#}"
    );
    // With the registered graph checkout gone, nobody is classed.
    let g = f.graph();
    f.git(&["worktree", "remove", "--force", g.to_str().expect("graph")]);
    f.git(&["branch", "-D", "mdkg-graph"]);
    let no_ref = || {
        f.run(
            "rev",
            &[
                "entry", "add", NODE, "--kind", "finding", "--title", "No ref",
            ],
        )
        .1
    };
    let envelope = no_ref();
    assert_eq!(envelope["diagnostics"][0]["code"], "entry.not_allowed");
    assert!(
        envelope["diagnostics"][0]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("registered graph checkout"),
        "{envelope:#}"
    );
    // Nor with no record of one: never the invoking checkout's HEAD, which
    // commits rev as a reviewer too (the orchestrator on #10737, option 1).
    f.git(&["config", "--unset", &format!("mdkg.graph.{GRAPH}.worktree")]);
    let envelope = no_ref();
    let d = &envelope["diagnostics"][0];
    assert_eq!(d["code"], "entry.not_allowed", "{envelope:#}");
    assert!(
        d["message"]
            .as_str()
            .unwrap_or_default()
            .contains("no graph ref or registered graph checkout"),
        "{envelope:#}"
    );
    assert!(
        d["hint"]
            .as_str()
            .unwrap_or_default()
            .contains("mdkg install"),
        "{envelope:#}"
    );
}

/// The orchestrator on #10737, option 1 (graph.ref edd row 29): in ref
/// mode, classes come from the graph ref's tip, pinned through g-core,
/// never the registered graph checkout's HEAD.
#[test]
fn in_ref_mode_classes_come_from_the_graph_refs_tip() {
    let f = Fixture::new("Body.\n");
    // The graph ref: a branch no worktree has checked out, at the graph
    // checkout's tip, recorded for this graph's id.
    f.git(&["branch", "graph-tip", "mdkg-graph"]);
    f.git(&["config", "mdkg.graph.ref", "refs/heads/graph-tip"]);
    f.git(&[
        "config",
        &format!("mdkg.graph.{GRAPH}.ref"),
        "refs/heads/graph-tip",
    ]);
    let add = |title: &str| {
        f.run(
            "rev",
            &["entry", "add", NODE, "--kind", "finding", "--title", title],
        )
    };
    let (code, envelope) = add("From the ref");
    assert_eq!(code, 0, "{envelope:#}");
    // The checkout makes rev a worker; the graph ref still says reviewer.
    f.commit_graph(".mdkg/agents/rev.json", &agent("worker"));
    let (code, envelope) = add("Still the ref's");
    assert_eq!(code, 0, "{envelope:#}");
    // Once the graph ref has it, rev is a worker, which raises no finding.
    f.git(&["branch", "-f", "graph-tip", "mdkg-graph"]);
    let (code, envelope) = add("Moved");
    assert_ne!(code, 0, "{envelope:#}");
    assert_eq!(
        envelope["diagnostics"][0]["code"], "entry.not_allowed",
        "{envelope:#}"
    );
}

#[test]
fn an_inline_agent_is_classed_by_the_launcher_rules_and_a_saved_spec_must_agree() {
    let f = Fixture::new("Body.\n");
    // A legacy inline reviewer in the graph ref's config.json.
    let config = |extra: Value| {
        let mut agents =
            json!({"inl": {"harness": "codex", "role": "reviewer", "reports_to": "orchestrator"}});
        if let (Some(a), Value::Object(more)) = (agents.as_object_mut(), extra) {
            a.extend(more);
        }
        json!({
            "schema_version": 3, "format_version": "0.7", "graph_id": GRAPH,
            "work": {"location": "git"}, "agents": agents
        })
        .to_string()
    };
    f.commit_graph(".mdkg/config.json", &config(json!({})));
    f.ok(
        "inl",
        &[
            "entry", "add", NODE, "--kind", "finding", "--title", "Inline",
        ],
    );
    // An inline agent the launcher's execution rules refuse refuses classes.
    f.commit_graph(
        ".mdkg/config.json",
        &config(json!({"bad": {"harness": "codex", "role": "worker", "mode": "fast"}})),
    );
    assert_eq!(refused_class(&f, "inl"), "entry.not_allowed");
    f.commit_graph(".mdkg/config.json", &config(json!({})));
    // The launcher saved rev's spec as a worker (a user-layer override it
    // applied): an op relying on rev's class is refused, never classed
    // either way (Terra's code review #5973, finding 4).
    let run = f.repo.join(".git/mdkg/run");
    fs::create_dir_all(&run).expect("run dir");
    fs::write(
        run.join("rev.json"),
        json!({"spec": {"name": "rev", "harness": "codex", "role": "worker", "cwd": "/x", "reports_to": "orchestrator"}})
            .to_string(),
    )
    .expect("saved spec");
    let (_, envelope) = f.run(
        "rev",
        &[
            "entry",
            "add",
            NODE,
            "--kind",
            "finding",
            "--title",
            "Overridden",
        ],
    );
    let d = &envelope["diagnostics"][0];
    assert_eq!(d["code"], "entry.not_allowed", "{envelope:#}");
    assert!(
        d["message"]
            .as_str()
            .unwrap_or_default()
            .contains("rev.role"),
        "{envelope:#}"
    );
    // In step with the committed definition, it's a reviewer again.
    fs::write(
        run.join("rev.json"),
        json!({"spec": {"name": "rev", "harness": "codex", "role": "reviewer", "cwd": "/x", "reports_to": "orchestrator"}})
            .to_string(),
    )
    .expect("saved spec");
    f.ok(
        "rev",
        &[
            "entry", "add", NODE, "--kind", "finding", "--title", "In step",
        ],
    );
}

/// Terra's code review #6278, finding 4: a committed config the launcher
/// would reject grants no class, even beside a valid definition file.
#[test]
fn a_malformed_committed_config_refuses_every_class() {
    let f = Fixture::new("Body.\n");
    let base = || {
        json!({
            "schema_version": 3, "format_version": "0.7", "graph_id": GRAPH,
            "work": {"location": "git"}
        })
    };
    let mut config = base();
    config["agents"] = json!(["inl"]);
    f.commit_graph(".mdkg/config.json", &config.to_string());
    assert_eq!(refused_class(&f, "rev"), "entry.not_allowed");
    f.commit_graph(".mdkg/config.json", "[]");
    assert_eq!(refused_class(&f, "rev"), "entry.not_allowed");
    f.commit_graph(".mdkg/config.json", &base().to_string());
    f.ok(
        "rev",
        &["entry", "add", NODE, "--kind", "finding", "--title", "Fine"],
    );
}

/// Terra's code review #6278, finding 1, with the orchestrator's call: a
/// process with a live turn mark is in a turn whatever actor it names, so
/// a worker's turn can't act as its coordinator with `--as`.
#[test]
fn a_marked_turn_never_acts_as_its_coordinator() {
    let f = Fixture::new("Body.\n");
    f.ok("wrk", &["claim", NODE]);
    let add = |title: &str| {
        vec![
            "entry".to_string(),
            "add".into(),
            NODE.into(),
            "--kind".into(),
            "finding".into(),
            "--title".into(),
            title.into(),
        ]
    };
    let args = add("Coordinated");
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    // Unmarked, the orchestrator is the coordinator of wrk's node.
    f.ok("orchestrator", &args);
    // Marked as a turn (any actor it names), it's only its own class.
    for actor in ["wrk", "orchestrator", "rev2"] {
        let args = add("Forged");
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let (code, envelope) = f.run_with(
            "orchestrator",
            &args,
            &[("MDKG_RUN_TURN", "t-1"), ("MDKG_ACTOR", actor)],
        );
        assert_ne!(code, 0, "{actor}: {envelope:#}");
        assert_eq!(
            envelope["diagnostics"][0]["code"], "entry.not_allowed",
            "{actor}: {envelope:#}"
        );
    }
}

/// The orchestrator on Terra's #6919, call 2: leases match by the node's
/// folder as the graph ref holds it, never the invoking checkout's, so a
/// node the graph ref doesn't hold has no worker and no coordinator.
#[test]
fn a_node_off_the_graph_ref_has_no_coordinator() {
    const OFF: &str = "task-01a1878e-3e6f-710b-9d01-68d002aed3e4";
    let f = Fixture::new("Body.\n");
    f.commit(
        ".mdkg/work/task-off.md",
        &format!(
            "---\nid: {OFF}\ntype: task\ntitle: Off the graph ref\nstatus: todo\n\
             created: 2026-10-29T00:00:00.000Z\nupdated: 2026-10-29T00:00:00.000Z\n---\nBody.\n"
        ),
    );
    f.ok("wrk", &["claim", NODE]);
    f.ok("wrk", &["claim", OFF]);
    let add = |node: &str| {
        let args = [
            "entry",
            "add",
            node,
            "--kind",
            "finding",
            "--title",
            "Coordinated",
        ];
        f.run("orchestrator", &args)
    };
    let (code, envelope) = add(NODE);
    assert_eq!(code, 0, "{envelope:#}");
    let (code, envelope) = add(OFF);
    assert_ne!(code, 0, "{envelope:#}");
    assert_eq!(
        envelope["diagnostics"][0]["code"], "entry.not_allowed",
        "{envelope:#}"
    );
    // The id in a file outside the graph's document folders (a fixture)
    // is never a node: it grants nothing (Terra's #8466, the blocker).
    f.commit_graph(
        "fixture-off.md",
        &format!("---\nid: {OFF}\ntype: task\ntitle: A fixture\nstatus: todo\n---\n"),
    );
    let (code, envelope) = add(OFF);
    assert_ne!(code, 0, "{envelope:#}");
    assert_eq!(
        envelope["diagnostics"][0]["code"], "entry.not_allowed",
        "{envelope:#}"
    );
    // Nor under `.mdkg` outside the document folders (a template, a
    // fixture), nor an id-only file inside them (Terra's #8791).
    for (path, text) in [
        (
            ".mdkg/templates/off.md",
            format!("---\nid: {OFF}\ntype: task\ntitle: T\n---\n"),
        ),
        (
            ".mdkg/fixtures/off.md",
            format!("---\nid: {OFF}\ntype: task\ntitle: T\n---\n"),
        ),
        (
            ".mdkg/work/off-id-only.md",
            format!("---\nid: {OFF}\n---\n"),
        ),
        // Nor a file the indexer skips: an upper-case `.MD`, or a core
        // folder's `core.md` page (Terra's #9504, blocker 2).
        (
            ".mdkg/work/off-upper.MD",
            format!(
                "---\nid: {OFF}\ntype: task\ntitle: Upper\nstatus: todo\n\
                 created: 2026-10-29T00:00:00.000Z\nupdated: 2026-10-29T00:00:00.000Z\n---\n"
            ),
        ),
        (
            ".mdkg/core/core.md",
            format!(
                "---\nid: {OFF}\ntype: task\ntitle: Core page\nstatus: todo\n\
                 created: 2026-10-29T00:00:00.000Z\nupdated: 2026-10-29T00:00:00.000Z\n---\n"
            ),
        ),
    ] {
        let full = f.graph().join(path);
        std::fs::create_dir_all(full.parent().expect("dir")).expect("dir");
        f.commit_graph(path, &text);
        let (code, envelope) = add(OFF);
        assert_ne!(code, 0, "{path}: {envelope:#}");
        assert_eq!(
            envelope["diagnostics"][0]["code"], "entry.not_allowed",
            "{path}: {envelope:#}"
        );
    }
    // A node the graph ref holds retyped, with space around its key, is
    // still the node by its UUID (Terra's #8466).
    let retyped = NODE.replacen("task-", "bug-", 1);
    f.commit_graph(
        NODE_PATH,
        &format!(
            "---\n  id :  {retyped}\ntype: bug\ntitle: Ledger fixture\nstatus: todo\n\
             created: 2026-10-29T00:00:00.000Z\nupdated: 2026-10-29T00:00:00.000Z\n---\nBody.\n"
        ),
    );
    let (code, envelope) = add(NODE);
    assert_eq!(code, 0, "{envelope:#}");
}

/// Terra's code review #10220, blocker 1, with the orchestrator's call: a
/// root node whose file sits in an enabled workspace's folder
/// (`.mdkg/work/nested`) is leased in that folder, and the gate reads its
/// folder by the same rule. A subordinate's old lease, keyed in `.mdkg`
/// before the workspace existed, never makes its coordinator a
/// coordinator on a node the coordinator holds itself.
#[test]
fn a_node_in_a_nested_workspace_folder_is_read_where_its_lease_is() {
    const NESTED: &str = "task-01a1878e-3e6f-710b-9d01-68d002aed3e5";
    const NESTED_PATH: &str = ".mdkg/work/nested/task-nested.md";
    let f = Fixture::new("Body.\n");
    let text = format!(
        "---\nid: {NESTED}\ntype: task\ntitle: Nested\nstatus: todo\n\
         created: 2026-10-29T00:00:00.000Z\nupdated: 2026-10-29T00:00:00.000Z\n---\nBody.\n"
    );
    f.commit(NESTED_PATH, &text);
    fs::create_dir_all(f.graph().join(".mdkg/work/nested")).expect("dir");
    f.commit_graph(NESTED_PATH, &text);
    // The subordinate's lease, keyed `.mdkg#<id>` with no nested
    // workspace yet, now released: its most recent worker.
    f.ok("wrk", &["claim", NESTED]);
    f.ok("wrk", &["release", NESTED]);
    let config = json!({
        "schema_version": 3, "format_version": "0.7", "graph_id": GRAPH,
        "work": {"location": "git"},
        "workspaces": {
            "root": {"path": ".", "mdkg_dir": ".mdkg", "enabled": true},
            "nested": {"path": ".mdkg/work", "mdkg_dir": "nested", "enabled": true}
        }
    })
    .to_string();
    f.commit(".mdkg/config.json", &config);
    f.commit_graph(".mdkg/config.json", &config);
    // The coordinator's own lease, keyed in `.mdkg/work/nested`.
    f.ok("orchestrator", &["claim", NESTED]);
    let store = mdkg_coord_sqlite::Store::open_for_repo(&f.repo, STORE_TTL_MS).expect("store");
    let keys: Vec<(String, String)> = store
        .coordinator()
        .expect("state")
        .leases()
        .map(|l| (l.owner.clone(), l.node.clone().unwrap_or_default()))
        .collect();
    assert_eq!(
        keys,
        [
            ("wrk".to_string(), format!(".mdkg#{NESTED}")),
            (
                "orchestrator".to_string(),
                format!(".mdkg/work/nested#{NESTED}")
            ),
        ],
        "the old lease and the coordinator's own, keyed apart"
    );
    drop(store);
    let (code, envelope) = f.run(
        "orchestrator",
        &[
            "entry",
            "add",
            NESTED,
            "--kind",
            "finding",
            "--title",
            "On my own work",
        ],
    );
    assert_ne!(code, 0, "{envelope:#}");
    assert_eq!(
        envelope["diagnostics"][0]["code"], "entry.not_allowed",
        "{envelope:#}"
    );
    // Released, the coordinator's lease is the most recent there, so the
    // subordinate's old one still makes no one a coordinator.
    f.ok("orchestrator", &["release", NESTED]);
    let (code, envelope) = f.run(
        "orchestrator",
        &[
            "entry",
            "add",
            NESTED,
            "--kind",
            "finding",
            "--title",
            "Still mine",
        ],
    );
    assert_ne!(code, 0, "{envelope:#}");
}

/// The leases on `node` as `(owner, key)`, in the order they were made.
fn lease_keys(f: &Fixture) -> Vec<(String, String)> {
    let store = mdkg_coord_sqlite::Store::open_for_repo(&f.repo, STORE_TTL_MS).expect("store");
    store
        .coordinator()
        .expect("state")
        .leases()
        .map(|l| (l.owner.clone(), l.node.clone().unwrap_or_default()))
        .collect()
}

/// `orchestrator`'s `entry add` of a finding on `node`: a coordinator's
/// op here, which only a node another agent works for it allows.
fn coordinator_add(f: &Fixture, node: &str, title: &str) -> (i32, Value) {
    f.run(
        "orchestrator",
        &["entry", "add", node, "--kind", "finding", "--title", title],
    )
}

/// Terra's code review #10490, the major, with the orchestrator's call:
/// lease keys come from the graph ref's committed workspaces, the list
/// the gate reads, so a checkout's uncommitted workspace whose folder
/// holds the coordinator's own node keys its lease where the gate looks,
/// and the coordinator class on that node stays refused.
#[test]
fn an_uncommitted_workspace_edit_never_splits_a_lease_from_the_gate() {
    const NESTED: &str = "task-01a1878e-3e6f-710b-9d01-68d002aed3e6";
    const NESTED_PATH: &str = ".mdkg/work/nested/task-nested.md";
    let f = Fixture::new("Body.\n");
    let text = format!(
        "---\nid: {NESTED}\ntype: task\ntitle: Nested\nstatus: todo\n\
         created: 2026-10-29T00:00:00.000Z\nupdated: 2026-10-29T00:00:00.000Z\n---\nBody.\n"
    );
    f.commit(NESTED_PATH, &text);
    fs::create_dir_all(f.graph().join(".mdkg/work/nested")).expect("dir");
    f.commit_graph(NESTED_PATH, &text);
    // The subordinate's lease, now released: the node's most recent worker.
    f.ok("wrk", &["claim", NESTED]);
    f.ok("wrk", &["release", NESTED]);
    // The coordinator's own checkout adds a workspace holding the node's
    // file, left uncommitted, then claims the node.
    fs::write(
        f.repo.join(".mdkg/config.json"),
        json!({
            "schema_version": 3, "format_version": "0.7", "graph_id": GRAPH,
            "work": {"location": "git"},
            "workspaces": {
                "root": {"path": ".", "mdkg_dir": ".mdkg", "enabled": true},
                "nested": {"path": ".mdkg/work", "mdkg_dir": "nested", "enabled": true}
            }
        })
        .to_string(),
    )
    .expect("config");
    f.ok("orchestrator", &["claim", NESTED]);
    assert_eq!(
        lease_keys(&f),
        [
            ("wrk".to_string(), format!(".mdkg#{NESTED}")),
            ("orchestrator".to_string(), format!(".mdkg#{NESTED}")),
        ],
        "both keyed by the graph ref's workspaces, never the checkout's edit"
    );
    let (code, envelope) = coordinator_add(&f, NESTED, "On my own work");
    assert_ne!(code, 0, "{envelope:#}");
    assert_eq!(
        envelope["diagnostics"][0]["code"], "entry.not_allowed",
        "{envelope:#}"
    );
    // Released, its lease is the node's most recent: still its own work.
    f.ok("orchestrator", &["release", NESTED]);
    let (code, envelope) = coordinator_add(&f, NESTED, "Still mine");
    assert_ne!(code, 0, "{envelope:#}");
    assert_eq!(
        envelope["diagnostics"][0]["code"], "entry.not_allowed",
        "{envelope:#}"
    );
}

/// Terra's code review #10490: a node file the coordinator moved in its
/// own checkout into another workspace's folder keys its lease apart from
/// the folder the graph ref's file gives the gate; its own lease, matched
/// by the node's id wherever it's keyed, still makes the node its own
/// work, live or as its most recent.
#[test]
fn a_node_file_moved_in_the_checkout_is_still_its_own_work() {
    let f = Fixture::new("Body.\n");
    let config = json!({
        "schema_version": 3, "format_version": "0.7", "graph_id": GRAPH,
        "work": {"location": "git"},
        "workspaces": {
            "root": {"path": ".", "mdkg_dir": ".mdkg", "enabled": true},
            "nested": {"path": ".mdkg/work", "mdkg_dir": "nested", "enabled": true}
        }
    })
    .to_string();
    f.commit(".mdkg/config.json", &config);
    f.commit_graph(".mdkg/config.json", &config);
    f.ok("wrk", &["claim", NODE]);
    f.ok("wrk", &["release", NODE]);
    // Moved, uncommitted, into the nested workspace's folder.
    fs::create_dir_all(f.repo.join(".mdkg/work/nested")).expect("dir");
    fs::rename(
        f.repo.join(NODE_PATH),
        f.repo.join(".mdkg/work/nested/task-ledger.md"),
    )
    .expect("move");
    f.ok("orchestrator", &["claim", NODE]);
    assert_eq!(
        lease_keys(&f),
        [
            ("wrk".to_string(), format!(".mdkg#{NODE}")),
            (
                "orchestrator".to_string(),
                format!(".mdkg/work/nested#{NODE}")
            ),
        ],
        "the moved file keys apart from the graph ref's"
    );
    let (code, envelope) = coordinator_add(&f, NODE, "On my own work");
    assert_ne!(code, 0, "{envelope:#}");
    assert_eq!(
        envelope["diagnostics"][0]["code"], "entry.not_allowed",
        "{envelope:#}"
    );
    f.ok("orchestrator", &["release", NODE]);
    let (code, envelope) = coordinator_add(&f, NODE, "Still mine");
    assert_ne!(code, 0, "{envelope:#}");
    // The subordinate's next lease makes it its node again.
    fs::rename(
        f.repo.join(".mdkg/work/nested/task-ledger.md"),
        f.repo.join(NODE_PATH),
    )
    .expect("move back");
    f.ok("wrk", &["claim", NODE]);
    let (code, envelope) = coordinator_add(&f, NODE, "Coordinated");
    assert_eq!(code, 0, "{envelope:#}");
}

/// Terra's code review #6278, finding 3, with the orchestrator's call: an
/// entry op reads classes under the graph lock, shared, taken before its
/// store transaction, so it waits while a ref writer holds it exclusive.
#[test]
fn an_entry_op_waits_for_the_graph_lock() {
    let f = Fixture::new("Body.\n");
    let dir = f.repo.join(".git/mdkg/graph");
    fs::create_dir_all(&dir).expect("graph dir");
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join("lock"))
        .expect("lock");
    lock.lock().expect("exclusive");
    let mut c = Command::new(env!("CARGO_BIN_EXE_mdkg"));
    let mut child = f
        .env(&mut c)
        .args(["--root", f.repo.to_str().expect("root"), "--as", "rev"])
        .args([
            "entry", "add", NODE, "--kind", "finding", "--title", "Waited", "--json",
        ])
        .stdout(std::process::Stdio::null())
        .spawn()
        .expect("mdkg");
    std::thread::sleep(std::time::Duration::from_millis(1_500));
    assert!(
        child.try_wait().expect("wait").is_none(),
        "the op ran under a writer's exclusive graph lock"
    );
    drop(lock);
    let status = child.wait().expect("wait");
    assert!(status.success(), "{status:?}");
    assert_eq!(f.state("F1"), "open");
}

/// The orchestrator on Terra's #6548, call 6 (and Opus's #6482, finding
/// 2): a merge declared as a fix is judged against its first parent only,
/// so a revert of what it fixed is caught even when a path it differs in
/// from its second parent changes later. `--commit` declares another
/// commit of the fix.
#[test]
fn a_merge_fix_is_judged_against_its_first_parent() {
    let f = Fixture::new("Body.\n");
    let base = f.git(&["rev-parse", "HEAD"]);
    f.git(&["checkout", "-qb", "side"]);
    let side = f.commit("b.txt", "side\n");
    f.git(&["checkout", "-q", "main"]);
    f.commit("c.txt", "p1\n");
    // M: the side merged with main's tree kept, then a.txt, the fix.
    f.git(&["merge", "-q", "-s", "ours", "--no-edit", "side"]);
    fs::write(f.repo.join("a.txt"), "fix\n").expect("fix");
    f.git(&["add", "a.txt"]);
    f.git(&["commit", "-q", "--amend", "--no-edit"]);
    let merge = f.git(&["rev-parse", "HEAD"]);
    let other = f.commit("d.txt", "more\n");
    // The fix undone, and b.txt (where M differs from `side`) edited.
    f.git(&["rm", "-q", "a.txt"]);
    f.git(&["commit", "-qm", "revert"]);
    let at = f.commit("b.txt", "later\n");
    f.open_round(&base, &at, "m1");
    f.ok(
        "rev",
        &["entry", "add", NODE, "--kind", "finding", "--title", "F"],
    );
    f.ok(
        "wrk",
        &[
            "entry", "fix", NODE, "F1", "--sha", &merge, "--commit", &other,
        ],
    );
    let shown = f.ok("rev", &["entry", "show", NODE, "F1"]);
    assert_eq!(
        shown["entry"]["fix_commits"],
        json!([merge.clone(), other]),
        "{shown:#}"
    );
    let (_, envelope) = f.run("rev", &["entry", "verify", NODE, "F1", "--at", &at]);
    let message = envelope["diagnostics"][0]["message"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        message.contains("undoes") && message.contains(&merge),
        "{envelope:#}"
    );
    let _ = side;
}

/// A29, "`done` refuses while the gate fails": the pre-check refuses with
/// `entry.gate_failed` (exit 5) and nothing written; once a coordinator's
/// residual settles the failure, `done` commits, and the settling transaction
/// records the mark and its `ledger.settle` record together (A29 step 4).
#[test]
fn done_refuses_while_the_gate_fails_and_settles_when_it_passes() {
    let f = Fixture::new("Body.\n");
    f.ok("wrk", &["claim", NODE]);
    f.ok(
        "rev",
        &[
            "entry",
            "add",
            NODE,
            "--kind",
            "finding",
            "--severity",
            "blocker",
            "--title",
            "Broken",
        ],
    );
    let head = f.git(&["rev-parse", "HEAD"]);
    let before = fs::read(f.repo.join(NODE_PATH)).expect("node");
    let (code, refused) = f.refused("wrk", &["done", NODE]);
    assert_eq!((code, refused.as_str()), (5, "entry.gate_failed"));
    assert_eq!(f.git(&["rev-parse", "HEAD"]), head, "nothing committed");
    assert_eq!(
        fs::read(f.repo.join(NODE_PATH)).expect("node"),
        before,
        "nothing written"
    );
    let node = mdkg_coord_sqlite::ledger::NodeLink::of(NODE).expect("node");
    let settles = || {
        mdkg_coord_sqlite::Store::open_for_repo(&f.repo, STORE_TTL_MS)
            .expect("store")
            .ledger_settles(&node)
            .expect("settles")
    };
    assert!(settles().is_empty());
    // Recorded by the worker's coordinator, which needs no lease (A27
    // rule 4); a worker's `done` then passes.
    f.ok(
        "orchestrator",
        &["entry", "residual", NODE, "F1", "--reason", "shipped as is"],
    );
    let done = f.ok("wrk", &["done", NODE]);
    let commit = done["commit"].as_str().expect("commit").to_string();
    assert_eq!(f.git(&["rev-parse", "HEAD"]), commit);
    let records = settles();
    assert_eq!(records.len(), 1, "{records:#?}");
    assert_eq!(records[0]["node"], json!(NODE));
    assert_eq!(records[0]["at"], json!(commit));
    assert_eq!(records[0]["changeset"], done["changesets"][0]);
    assert_eq!(records[0]["entries"][0]["state"], json!("residual"));
    // The section `done` committed names the residual, with its reason.
    let text = f.git(&["show", &format!("{commit}:{NODE_PATH}")]);
    assert!(
        text.contains("\n- F1 blocker residual by orchestrator: Broken; reason: shipped as is\n"),
        "{text}"
    );
}

/// A31: git work's `done` writes the generated `# Ledger` section into the
/// node, in the same commit as `status: done`, and the settle still passes,
/// since the strip takes the section back out. A later `done`, after the
/// node is reopened, replaces it whole.
#[test]
fn done_writes_the_ledger_section_in_its_own_commit() {
    let f = Fixture::new("Body.\n\n# Requirements\n\n- R1: The writer shall render the ledger.\n");
    let fix = f.reviewed_fix();
    f.ok(
        "rev",
        &["entry", "req", NODE, "R1", "--state", "met", "--at", &fix],
    );
    f.ok("wrk", &["claim", NODE]);
    let done = f.ok("wrk", &["done", NODE]);
    let commit = done["commit"].as_str().expect("commit").to_string();
    let text = f.git(&["show", &format!("{commit}:{NODE_PATH}")]);
    let tail = text
        .split_once("Body.\n\n# Requirements\n\n- R1: The writer shall render the ledger.\n")
        .map(|(_, t)| t)
        .unwrap_or_else(|| panic!("the body is kept: {text}"));
    let lines: Vec<&str> = tail.lines().collect();
    assert_eq!(lines[0], "# Ledger", "{text}");
    assert!(
        lines[2].starts_with("<!-- mdkg:ledger begin upto="),
        "{text}"
    );
    assert_eq!(
        lines[3],
        format!("- F1 major verified@{} by rev: Off by one", &fix[..12])
    );
    assert_eq!(lines[4], format!("- R1 met@{} by rev", &fix[..12]));
    assert!(lines[5].starts_with("- V1 closed by rev: "), "{text}");
    assert_eq!(lines[6], "<!-- mdkg:ledger end -->");
    assert_eq!(lines.len(), 7, "{text}");
    // Settled once, at that commit, and read as done.
    let node = mdkg_coord_sqlite::ledger::NodeLink::of(NODE).expect("node");
    let store = || mdkg_coord_sqlite::Store::open_for_repo(&f.repo, STORE_TTL_MS).expect("store");
    let settles = store().ledger_settles(&node).expect("settles");
    assert_eq!(settles.len(), 1, "{settles:#?}");
    assert_eq!(settles[0]["at"], json!(commit));
    assert_eq!(f.read_status(), (json!("done"), Value::Null));
    // Reopened with a new issue, the next done replaces the section.
    f.write_node(
        "todo",
        &format!("Body.\n\n# Requirements\n\n- R1: The writer shall render the ledger.\n{tail}\n"),
    );
    let reopened = f.commit(
        NODE_PATH,
        &fs::read_to_string(f.repo.join(NODE_PATH)).expect("node"),
    );
    f.ok("wrk", &["claim", NODE]);
    f.ok(
        "wrk",
        &["entry", "add", NODE, "--kind", "issue", "--title", "Later"],
    );
    f.ok(
        "orchestrator",
        &["entry", "residual", NODE, "I1", "--reason", "next release"],
    );
    let again = f.ok("wrk", &["done", NODE]);
    let second = again["commit"].as_str().expect("commit").to_string();
    assert_ne!(second, reopened);
    let text = f.git(&["show", &format!("{second}:{NODE_PATH}")]);
    assert_eq!(text.matches("# Ledger").count(), 1, "{text}");
    assert!(
        text.contains("\n- I1 residual by orchestrator: Later; reason: next release\n"),
        "{text}"
    );
    assert_eq!(store().ledger_settles(&node).expect("settles").len(), 2);
}

impl Fixture {
    /// The node's effective status and `settled` field, as `show` reads.
    fn read_status(&self) -> (Value, Value) {
        let d = self.ok("rev", &["show", NODE]);
        (d["node"]["status"].clone(), d["node"]["settled"].clone())
    }

    /// A finding fixed in one commit on the current branch and verified
    /// in a closed round `base..fix` (rev's), returning the fix SHA.
    fn reviewed_fix(&self) -> String {
        self.ok(
            "rev",
            &[
                "entry",
                "add",
                NODE,
                "--kind",
                "finding",
                "--title",
                "Off by one",
            ],
        );
        let base = self.git(&["rev-parse", "HEAD"]);
        let fix = self.commit("src/lib.rs", "fixed\n");
        self.ok("wrk", &["entry", "fix", NODE, "F1", "--sha", &fix]);
        self.open_round_for(&base, &fix, "m1", &["rev"]);
        self.ok("rev", &["entry", "verify", NODE, "F1", "--at", &fix]);
        let range = format!("{base}..{fix}");
        self.ok(
            "rev",
            &[
                "entry",
                "complete",
                NODE,
                "V1",
                "--range",
                &range,
                "--verdict",
                "clean",
            ],
        );
        fix
    }
}

/// A29, "A done file that didn't settle reads as unsettled", with the
/// orchestrator's call on #10251: a settled node reads done through later
/// commits, one on a reviewed path included (the record's `at` is in
/// HEAD's history); a done file committed by hand, never settled, reads
/// `done (unsettled)` with `settled: false`, and isn't done for `list
/// --status done`.
#[test]
fn a_settled_node_stays_done_and_a_hand_committed_done_reads_unsettled() {
    let f = Fixture::new("Body.\n");
    f.reviewed_fix();
    f.ok("wrk", &["claim", NODE]);
    f.ok("wrk", &["done", NODE]);
    assert_eq!(f.read_status(), (json!("done"), Value::Null));
    f.commit("src/other.rs", "elsewhere\n");
    f.commit("src/lib.rs", "changed after review\n");
    assert_eq!(f.read_status(), (json!("done"), Value::Null));

    let g = Fixture::new("Body.\n");
    g.ok(
        "rev",
        &[
            "entry",
            "add",
            NODE,
            "--kind",
            "finding",
            "--severity",
            "blocker",
            "--title",
            "Broken",
        ],
    );
    g.write_node("done", "Body.\n");
    g.commit(
        NODE_PATH,
        &fs::read_to_string(g.repo.join(NODE_PATH)).expect("node"),
    );
    assert_eq!(g.read_status(), (json!("done (unsettled)"), json!(false)));
    let listed = g.ok("rev", &["list", "--status", "done"]);
    assert_eq!(listed["count"], 0, "{listed:#}");
    let listed = g.ok("rev", &["list", "--status", "done (unsettled)"]);
    assert_eq!(listed["count"], 1, "{listed:#}");
    // A node without a ledger reads as in 0.7.0.
    let h = Fixture::new("Body.\n");
    h.write_node("done", "Body.\n");
    h.commit(
        NODE_PATH,
        &fs::read_to_string(h.repo.join(NODE_PATH)).expect("node"),
    );
    assert_eq!(h.read_status(), (json!("done"), Value::Null));
    // Without the coordinator (a store that can't be read), a git-work
    // done file with a `# Ledger` section reads `done (unknown)`; one
    // without reads as in 0.7.0.
    let store = h.repo.join(".git/mdkg");
    fs::create_dir_all(&store).expect("store dir");
    for name in ["coord.sqlite-wal", "coord.sqlite-shm"] {
        fs::remove_file(store.join(name)).ok();
    }
    fs::write(store.join("coord.sqlite"), "not a database").expect("damage");
    assert_eq!(h.read_status(), (json!("done"), Value::Null));
    h.write_node(
        "done",
        "Body.\n# Ledger\n\n<!-- mdkg:ledger begin upto=1 -->\n- F1 major verified\n<!-- mdkg:ledger end -->\n",
    );
    assert_eq!(h.read_status(), (json!("done (unknown)"), Value::Null));
}

/// The other direction of #10251's call: the done revision brought onto
/// another branch is checked where it arrived. A squash that brings the
/// reviewed content with it reads settled; a cherry-pick of the done
/// commit alone, onto a tree without the fix, reads `done (unsettled)`.
#[test]
fn a_done_revision_off_its_settle_history_is_checked_where_it_arrived() {
    let f = Fixture::new("Body.\n");
    let start = f.git(&["rev-parse", "HEAD"]);
    f.git(&["checkout", "-q", "-b", "work"]);
    f.reviewed_fix();
    f.ok("wrk", &["claim", NODE]);
    let done = f.ok("wrk", &["done", NODE]);
    let done_commit = done["commit"].as_str().expect("commit").to_string();
    assert_eq!(f.read_status(), (json!("done"), Value::Null));
    // A squash of the branch onto main: the fix comes with the done file.
    f.git(&["checkout", "-q", "main"]);
    f.git(&["merge", "-q", "--squash", "work"]);
    f.git(&["commit", "-qm", "squashed"]);
    f.commit("src/other.rs", "later on main\n");
    assert_eq!(f.read_status(), (json!("done"), Value::Null));
    // The done commit alone, on a branch without the fix.
    f.git(&["checkout", "-q", "-b", "bare", &start]);
    f.git(&["cherry-pick", "--allow-empty", &done_commit]);
    assert_eq!(f.read_status(), (json!("done (unsettled)"), json!(false)));
}

/// Terra's #10357, minor 1 (the fault bar's path): a settle refused after
/// `done`'s git commit leaves that commit unsettled: exit 5 and
/// `entry.gate_failed` saying so, no changeset, mark or settle record, and
/// the lease still held. Once the gate passes, a rerun settles exactly
/// once, at that commit, with no second commit. The gate passes at the
/// pre-check (HEAD holds the reviewed body) and fails at the settle,
/// because the done commit carries an unreviewed edit to a reviewed path,
/// the node's own body.
#[test]
fn a_settle_refused_after_the_commit_leaves_it_unsettled_until_a_rerun() {
    let f = Fixture::new("Body.\n");
    let base = f.git(&["rev-parse", "HEAD"]);
    f.write_node("todo", "Body, reviewed.\n");
    let head = f.commit(
        NODE_PATH,
        &fs::read_to_string(f.repo.join(NODE_PATH)).expect("node"),
    );
    f.open_round_for(&base, &head, "m1", &["rev"]);
    let range = format!("{base}..{head}");
    f.ok(
        "rev",
        &[
            "entry",
            "complete",
            NODE,
            "V1",
            "--range",
            &range,
            "--verdict",
            "clean",
        ],
    );
    f.ok("wrk", &["claim", NODE]);
    // Unreviewed and uncommitted: `done` commits it with the status.
    f.write_node("todo", "Body, edited after review.\n");
    let node = mdkg_coord_sqlite::ledger::NodeLink::of(NODE).expect("node");
    let store = || mdkg_coord_sqlite::Store::open_for_repo(&f.repo, STORE_TTL_MS).expect("store");
    let changesets = || store().coordinator().expect("state").changesets().count();
    let before = changesets();
    let (code, envelope) = f.run("wrk", &["done", NODE]);
    assert_eq!(code, 5, "{envelope:#}");
    let error = &envelope["diagnostics"][0];
    assert_eq!(error["code"], "entry.gate_failed", "{envelope:#}");
    assert!(
        error["message"]
            .as_str()
            .is_some_and(|m| m.contains("committed but unsettled")),
        "{envelope:#}"
    );
    let done_commit = f.git(&["rev-parse", "HEAD"]);
    assert_ne!(done_commit, head, "the commit stays on the branch");
    assert_eq!(changesets(), before, "no changeset recorded");
    assert!(store().ledger_settles(&node).expect("settles").is_empty());
    let state = store().coordinator().expect("state");
    assert!(
        state
            .leases()
            .any(|l| l.owner == "wrk" && l.status == mdkg_coord::lease::Status::Active),
        "the lease is still held"
    );
    let (status, _) = f.read_status();
    assert_eq!(status, json!("done (unsettled)"));
    // The edit reviewed: a round over it, closed clean. The rerun settles
    // that commit and makes no new one.
    f.open_round_for(&head, &done_commit, "m2", &["rev"]);
    let range = format!("{head}..{done_commit}");
    f.ok(
        "rev",
        &[
            "entry",
            "complete",
            NODE,
            "V2",
            "--range",
            &range,
            "--verdict",
            "clean",
        ],
    );
    let rerun = f.ok("wrk", &["done", NODE]);
    assert_eq!(f.git(&["rev-parse", "HEAD"]), done_commit, "{rerun:#}");
    let settles = store().ledger_settles(&node).expect("settles");
    assert_eq!(settles.len(), 1, "{settles:#?}");
    assert_eq!(settles[0]["at"], json!(done_commit));
    assert_eq!(f.read_status(), (json!("done"), Value::Null));
    // The settle alone: no work state written for the node, so nothing
    // stands over a later status (Terra's #10631, findings 1 and 3).
    let state = store().coordinator().expect("state");
    let leaf = mdkg_coord::Scope::new(NODE_PATH);
    assert!(state.work(&leaf).is_none());
    assert!(
        state.work_states().all(|(k, _)| *k != leaf),
        "no work row: {:?}",
        state.work_states().collect::<Vec<_>>()
    );
    // Reopened on main later: it reads as the file says.
    if state
        .leases()
        .any(|l| l.owner == "wrk" && l.status == mdkg_coord::lease::Status::Active)
    {
        f.ok("wrk", &["release", NODE]);
    }
    f.write_node("todo", "Body, edited after review.\n");
    f.commit(
        NODE_PATH,
        &fs::read_to_string(f.repo.join(NODE_PATH)).expect("node"),
    );
    assert_eq!(f.read_status().0, json!("todo"));
}

/// The orchestrator on #10776, option 1: a done commit on a branch whose
/// settle was refused, with its lease gone and main not holding it, gets
/// the gate at HEAD first, writing nothing, and then the claim-then-done
/// hint; the claimed rerun settles it once, with its changeset.
#[test]
fn an_unleased_unsettled_done_on_a_branch_gates_then_hints() {
    let f = Fixture::new("Body.\n");
    f.git(&["checkout", "-q", "-b", "work"]);
    let base = f.git(&["rev-parse", "HEAD"]);
    f.write_node("todo", "Body, reviewed.\n");
    let head = f.commit(
        NODE_PATH,
        &fs::read_to_string(f.repo.join(NODE_PATH)).expect("node"),
    );
    f.open_round_for(&base, &head, "m1", &["rev"]);
    let clean = |v: &str, from: &str, to: &str| {
        let range = format!("{from}..{to}");
        f.ok(
            "rev",
            &[
                "entry",
                "complete",
                NODE,
                v,
                "--range",
                &range,
                "--verdict",
                "clean",
            ],
        );
    };
    clean("V1", &base, &head);
    f.ok("wrk", &["claim", NODE]);
    f.write_node("todo", "Body, edited after review.\n");
    let (code, envelope) = f.run("wrk", &["done", NODE]);
    assert_eq!(code, 5, "{envelope:#}");
    let done_commit = f.git(&["rev-parse", "HEAD"]);
    f.ok("wrk", &["release", NODE]);
    let node = mdkg_coord_sqlite::ledger::NodeLink::of(NODE).expect("node");
    let settles = || {
        mdkg_coord_sqlite::Store::open_for_repo(&f.repo, STORE_TTL_MS)
            .expect("store")
            .ledger_settles(&node)
            .expect("settles")
    };
    // Unleased and not on main: the gate at HEAD refuses with its
    // failures, and nothing is written.
    let (code, envelope) = f.run("wrk", &["done", NODE]);
    assert_eq!(code, 5, "{envelope:#}");
    assert_eq!(
        envelope["diagnostics"][0]["code"], "entry.gate_failed",
        "{envelope:#}"
    );
    assert_eq!(f.git(&["rev-parse", "HEAD"]), done_commit);
    assert!(settles().is_empty());
    // The edit reviewed: the gate passes, and only a lease can record it.
    f.open_round_for(&head, &done_commit, "m2", &["rev"]);
    clean("V2", &head, &done_commit);
    let (code, envelope) = f.run("wrk", &["done", NODE]);
    assert_ne!(code, 0, "{envelope:#}");
    assert_eq!(
        envelope["diagnostics"][0]["code"], "coord.no_lease",
        "{envelope:#}"
    );
    assert!(settles().is_empty(), "nothing written");
    // The claimed rerun records it and settles it once.
    f.ok("wrk", &["claim", NODE]);
    f.ok("wrk", &["done", NODE]);
    let after = settles();
    assert_eq!(after.len(), 1, "{after:#?}");
    assert!(after[0]["changeset"].is_string(), "{after:#?}");
    f.ok("wrk", &["done", NODE]);
    assert_eq!(settles().len(), 1, "a further rerun adds none");
}

/// The orchestrator on #10776: a recorded changeset and its mark with no
/// settle record, as an earlier build's `done` left them (injected in
/// debug builds with `MDKG_FAIL_AFTER=unsettled`), are settled by the
/// rerun, with that changeset, once.
#[test]
fn a_legacy_mark_without_a_settle_is_settled_by_the_rerun() {
    let f = Fixture::new("Body.\n");
    f.git(&["checkout", "-q", "-b", "work"]);
    f.ok("wrk", &["claim", NODE]);
    let (code, envelope) = f.run_with("wrk", &["done", NODE], &[("MDKG_FAIL_AFTER", "unsettled")]);
    assert_eq!(code, 0, "{envelope:#}");
    let node = mdkg_coord_sqlite::ledger::NodeLink::of(NODE).expect("node");
    let store = || mdkg_coord_sqlite::Store::open_for_repo(&f.repo, STORE_TTL_MS).expect("store");
    assert!(store().ledger_settles(&node).expect("settles").is_empty());
    let leaf = mdkg_coord::Scope::new(NODE_PATH);
    let mark = store()
        .coordinator()
        .expect("state")
        .done_mark(&leaf)
        .expect("the mark")
        .to_string();
    let rerun = f.ok("wrk", &["done", NODE]);
    assert_eq!(rerun["already_done"], true, "{rerun:#}");
    let settles = store().ledger_settles(&node).expect("settles");
    assert_eq!(settles.len(), 1, "{settles:#?}");
    assert_eq!(settles[0]["changeset"], json!(mark));
    f.ok("wrk", &["done", NODE]);
    assert_eq!(store().ledger_settles(&node).expect("settles").len(), 1);
}

/// Terra's #10948, minor 1: the `unsettled` injection drops only the settle
/// record, never the gate. A legacy mark's rerun under it, with a blocker
/// open, refuses as the real settle would, and writes no record.
#[test]
fn the_unsettled_injection_still_evaluates_the_gate() {
    let f = Fixture::new("Body.\n");
    f.git(&["checkout", "-q", "-b", "work"]);
    f.ok("wrk", &["claim", NODE]);
    let unsettled = [("MDKG_FAIL_AFTER", "unsettled")];
    let (code, envelope) = f.run_with("wrk", &["done", NODE], &unsettled);
    assert_eq!(code, 0, "{envelope:#}");
    f.ok(
        "rev",
        &[
            "entry",
            "add",
            NODE,
            "--kind",
            "finding",
            "--severity",
            "blocker",
            "--title",
            "Broken",
        ],
    );
    let (code, envelope) = f.run_with("wrk", &["done", NODE], &unsettled);
    assert_eq!(code, 5, "{envelope:#}");
    assert_eq!(
        envelope["diagnostics"][0]["code"], "entry.gate_failed",
        "{envelope:#}"
    );
    let node = mdkg_coord_sqlite::ledger::NodeLink::of(NODE).expect("node");
    let store = mdkg_coord_sqlite::Store::open_for_repo(&f.repo, STORE_TTL_MS).expect("store");
    assert!(store.ledger_settles(&node).expect("settles").is_empty());
}

impl Fixture {
    /// `msg send --kind review_request --node NODE --range <range> --to
    /// rev` as the orchestrator, with `extra` arguments.
    fn request(&self, range: &str, extra: &[&str]) -> (i32, Value) {
        self.request_with(range, extra, &[])
    }

    fn request_with(&self, range: &str, extra: &[&str], vars: &[(&str, &str)]) -> (i32, Value) {
        let mut args = vec![
            "msg",
            "send",
            "--kind",
            "review_request",
            "--to",
            "rev",
            "--node",
            NODE,
            "--range",
            range,
            "--text",
            "Review the fix\nwith care",
        ];
        args.extend_from_slice(extra);
        self.run_with("orchestrator", &args, vars)
    }

    /// The review rounds on NODE, as `entry list` reads them.
    fn rounds(&self) -> Vec<Value> {
        let data = self.ok("rev", &["entry", "list", NODE, "--kind", "review"]);
        data["entries"].as_array().cloned().unwrap_or_default()
    }
}

/// Terra's #6548, "the verify guard has no production round-opening
/// path": a `review_request` sent with `--node` and `--range` opens the
/// node's review round in the send's own transaction, with its recipients
/// as reviewers, so a fixed finding can be verified through the CLI
/// alone. A retry of the same send opens nothing more; the same id with
/// another binding is another request.
#[test]
fn a_review_request_with_a_node_opens_the_round_a_verify_needs() {
    let f = Fixture::new("Body.\n");
    f.ok(
        "rev",
        &[
            "entry",
            "add",
            NODE,
            "--kind",
            "finding",
            "--title",
            "Off by one",
        ],
    );
    let base = f.git(&["rev-parse", "HEAD"]);
    let fix = f.commit("src/lib.rs", "fixed\n");
    f.ok("wrk", &["entry", "fix", NODE, "F1", "--sha", &fix]);
    let (_, code) = f.refused("rev", &["entry", "verify", NODE, "F1", "--at", &fix]);
    assert_eq!(code, "entry.not_covered", "no round yet");
    // Abbreviated SHAs resolve to full ids before anything is sent.
    let range = format!("{}..{}", &base[..10], &fix[..10]);
    let (code, envelope) = f.request(&range, &["--id", "rr-1"]);
    assert_eq!(code, 0, "{envelope:#}");
    let data = &envelope["data"];
    assert_eq!(data["round"]["entry"], format!("{NODE}#V1"), "{envelope:#}");
    assert_eq!(data["round"]["anchored"], "main", "{envelope:#}");
    let rounds = f.rounds();
    assert_eq!(rounds.len(), 1, "{rounds:#?}");
    assert_eq!(rounds[0]["title"], "Review the fix");
    assert_eq!(rounds[0]["range"], json!({"base": base, "sha": fix}));
    assert_eq!(rounds[0]["reviewers"], json!(["rev"]));
    assert_eq!(rounds[0]["request"], "rr-1");
    f.ok("rev", &["entry", "verify", NODE, "F1", "--at", &fix]);
    assert_eq!(f.state("F1"), "verified");
    // The same send again: a duplicate, and no second round.
    let (code, envelope) = f.request(&range, &["--id", "rr-1"]);
    assert_eq!(code, 0, "{envelope:#}");
    assert_eq!(envelope["data"]["duplicate"], true, "{envelope:#}");
    assert!(envelope["data"]["round"].is_null(), "{envelope:#}");
    assert_eq!(f.rounds().len(), 1);
    // The same id with another range is another request, never a silent
    // duplicate keeping the old range (Terra's #11150).
    let (code, envelope) = f.request(&format!("{base}..{base}"), &["--id", "rr-1"]);
    assert_ne!(code, 0, "{envelope:#}");
    assert_eq!(envelope["diagnostics"][0]["code"], "msg.id_reused");
    assert_eq!(f.rounds().len(), 1);
    // The same id without the binding is a different request.
    let (code, envelope) = f.run(
        "orchestrator",
        &[
            "msg",
            "send",
            "--kind",
            "review_request",
            "--to",
            "rev",
            "--id",
            "rr-1",
            "--text",
            "Review the fix\nwith care",
        ],
    );
    assert_ne!(code, 0, "{envelope:#}");
    assert_eq!(envelope["diagnostics"][0]["code"], "msg.id_reused");
}

/// A27 rule 8 and A23: a review request with `--node` names exactly one
/// node (`msg.review_multi_node`) and needs `--range`
/// (`msg.review_unranged`), each refused whole with nothing sent; one
/// node named twice is one node; a range that doesn't resolve sends
/// nothing; without `--node` a review request sends as before, and
/// `--range` belongs to review requests alone.
#[test]
fn review_requests_bind_one_node_with_a_range_or_send_nothing() {
    const OTHER: &str = "task-01a1878e-3e6f-710b-9d01-68d002aed3e5";
    let f = Fixture::new("Body.\n");
    f.commit(
        ".mdkg/work/task-other.md",
        &format!(
            "---\nid: {OTHER}\ntype: task\ntitle: Other\nstatus: todo\n\
             created: 2026-10-29T00:00:00.000Z\nupdated: 2026-10-29T00:00:00.000Z\n---\nOther.\n"
        ),
    );
    let base = f.git(&["rev-parse", "HEAD~1"]);
    let head = f.git(&["rev-parse", "HEAD"]);
    let range = format!("{base}..{head}");
    let inbox = || {
        f.ok("rev", &["msg", "inbox", "--no-mark"])["messages"]
            .as_array()
            .map_or(0, Vec::len)
    };
    let refused = |envelope: &Value| {
        envelope["diagnostics"]
            .as_array()
            .and_then(|d| d.iter().find(|d| d["severity"] == "error"))
            .map(|d| d["code"].clone())
            .unwrap_or_default()
    };
    let (code, envelope) = f.request(&range, &["--node", OTHER]);
    assert_eq!(code, 1, "{envelope:#}");
    assert_eq!(refused(&envelope), "msg.review_multi_node");
    let (code, envelope) = f.run(
        "orchestrator",
        &[
            "msg",
            "send",
            "--kind",
            "review_request",
            "--to",
            "rev",
            "--node",
            NODE,
            "--text",
            "Review",
        ],
    );
    assert_eq!(code, 1, "{envelope:#}");
    assert_eq!(refused(&envelope), "msg.review_unranged");
    let (_, envelope) = f.request(&format!("{base}..0123456789abcdef"), &[]);
    assert_eq!(refused(&envelope), "entry.unknown_commit", "{envelope:#}");
    let (_, envelope) = f.request(&range, &["--node", "task-nope"]);
    assert_eq!(refused(&envelope), "node.not_found", "{envelope:#}");
    assert_eq!(inbox(), 0, "nothing sent");
    assert!(f.rounds().is_empty(), "no round opened");
    // The same node twice is one node.
    let (code, envelope) = f.request(&range, &["--node", NODE]);
    assert_eq!(code, 0, "{envelope:#}");
    assert_eq!(f.rounds().len(), 1);
    // Without --node, as before; --range alone, or on another kind, isn't.
    let plain = [
        "msg",
        "send",
        "--kind",
        "review_request",
        "--to",
        "rev",
        "--text",
        "Review",
    ];
    f.ok("orchestrator", &plain);
    assert_eq!(f.rounds().len(), 1);
    let (code, _) = f.run(
        "orchestrator",
        &[plain.as_slice(), &["--range", &range]].concat(),
    );
    assert_ne!(code, 0);
    let (code, envelope) = f.run(
        "orchestrator",
        &[
            "msg", "send", "--kind", "note", "--to", "rev", "--node", NODE, "--range", &range,
            "--text", "Note",
        ],
    );
    assert_eq!(code, 1, "{envelope:#}");
    assert_eq!(refused(&envelope), "usage.invalid_arguments");
    assert_eq!(inbox(), 2);
}

/// Anchoring reads only the configured main ref, never `MDKG_MAIN_REF`
/// (the orchestrator on Terra's #6548, call 2): a round from a base only
/// the work branch holds stays unanchored with the override naming that
/// branch, and the send says so.
#[test]
fn a_send_anchors_on_the_configured_main_never_mdkg_main_ref() {
    let f = Fixture::new("Body.\n");
    f.git(&["checkout", "-q", "-b", "work"]);
    let x1 = f.commit("src/lib.rs", "one\n");
    let x2 = f.commit("src/lib.rs", "two\n");
    let range = format!("{x1}..{x2}");
    let (code, envelope) = f.request_with(&range, &[], &[("MDKG_MAIN_REF", "work")]);
    assert_eq!(code, 0, "{envelope:#}");
    assert!(
        envelope["data"]["round"]["anchored"].is_null(),
        "{envelope:#}"
    );
    let warned = envelope["diagnostics"]
        .as_array()
        .is_some_and(|d| d.iter().any(|d| d["code"] == "entry.round_unanchored"));
    assert!(warned, "{envelope:#}");
    assert!(f.rounds()[0]["anchored"].is_null());
}

/// The orchestrator on #11041, DECISION 2: a range whose head the graph
/// commit holds and no main tip does is a graph range, anchored as the
/// node's first round whatever main is; a range whose head main holds
/// anchors on main, even when the graph shares that history (checkout
/// mode, where mdkg-graph forked from main).
#[test]
fn graph_ranges_anchor_first_and_shared_history_anchors_on_main() {
    let f = Fixture::new("Body.\n");
    let c0 = f.git(&["rev-parse", "HEAD"]);
    let c1 = f.commit("src/lib.rs", "one\n");
    // The graph takes main's c1, then a design edit of its own.
    let graph = f.graph();
    let g = graph.to_str().expect("graph");
    f.git(&["-C", g, "merge", "-q", "--ff-only", "main"]);
    f.commit_graph("design.md", "Design.\n");
    let g1 = f.git(&["-C", g, "rev-parse", "HEAD"]);
    let anchored = |range: String, id: &str| {
        let (code, envelope) = f.request(&range, &["--id", id]);
        assert_eq!(code, 0, "{envelope:#}");
        envelope["data"]["round"]["anchored"].clone()
    };
    // A graph range: the node's first round anchors "first", though main
    // resolves and holds its base.
    assert_eq!(anchored(format!("{c0}..{g1}"), "rr-g"), "first");
    // c1 is shared: main holds it, so it's code, anchored on main.
    assert_eq!(anchored(format!("{c0}..{c1}"), "rr-shared"), "main");
    // A later graph round from another base anchors nothing.
    assert!(anchored(format!("{c1}..{g1}"), "rr-g2").is_null());
}

/// Terra's #11075, minor 1: the `# Ledger` section never pushes a node
/// file over the graph's `index.limits.max_file_bytes`, which would fail
/// every index read: `done` writes none, warns `entry.ledger_unwritten`,
/// and settles; the graph still indexes.
#[test]
fn the_ledger_section_never_pushes_a_file_over_the_index_limit() {
    let f = Fixture::new("Body.\n");
    let size = fs::metadata(f.repo.join(NODE_PATH)).expect("node").len();
    let path = f.repo.join(".mdkg/config.json");
    let mut config: Value =
        serde_json::from_str(&fs::read_to_string(&path).expect("config")).expect("json");
    config["index"] = json!({"limits": {"max_file_bytes": size + 64}});
    f.commit(".mdkg/config.json", &config.to_string());
    f.ok("wrk", &["claim", NODE]);
    f.ok(
        "wrk",
        &["entry", "add", NODE, "--kind", "issue", "--title", "Later"],
    );
    f.ok(
        "orchestrator",
        &["entry", "residual", NODE, "I1", "--reason", "next release"],
    );
    let (code, envelope) = f.run("wrk", &["done", NODE]);
    assert_eq!(code, 0, "{envelope:#}");
    let warned = envelope["diagnostics"]
        .as_array()
        .is_some_and(|d| d.iter().any(|d| d["code"] == "entry.ledger_unwritten"));
    assert!(warned, "{envelope:#}");
    let text = fs::read_to_string(f.repo.join(NODE_PATH)).expect("node");
    assert!(!text.contains("# Ledger"), "{text}");
    assert!(text.len() as u64 <= size + 64);
    assert_eq!(f.read_status(), (json!("done"), Value::Null));
}

impl Fixture {
    /// A blocker finding F1 on NODE, open, as rev's.
    fn blocker(&self) {
        self.ok(
            "rev",
            &[
                "entry",
                "add",
                NODE,
                "--kind",
                "finding",
                "--severity",
                "blocker",
                "--title",
                "Broken",
            ],
        );
    }
}

/// A29, "Residuals" (F8): a coordinator's `done --residual` on a node no
/// live lease holds records a residual for each failure, a finding's and
/// a requirement's, all in one transaction, then completes and settles; a
/// worker's is refused with nothing recorded.
#[test]
fn done_residual_is_a_coordinators_and_records_every_failure() {
    let f = Fixture::new("Body.\n\n# Requirements\n\n- R1: The thing shall work.\n");
    f.ok("wrk", &["claim", NODE]);
    f.blocker();
    let (code, refused) = f.refused("wrk", &["done", NODE, "--residual", "ship it"]);
    assert_eq!((code, refused.as_str()), (5, "entry.not_allowed"));
    assert_eq!(f.state("F1"), "open", "nothing recorded");
    f.ok("wrk", &["release", NODE]);
    let done = f.ok("orchestrator", &["done", NODE, "--residual", "ship it"]);
    assert_eq!(
        done["residuals"],
        json!([format!("{NODE}#F1"), format!("{NODE}#R1")]),
        "{done:#}"
    );
    assert_eq!(f.state("F1"), "residual");
    assert_eq!(f.state("R1"), "waived");
    let node = mdkg_coord_sqlite::ledger::NodeLink::of(NODE).expect("node");
    let store = mdkg_coord_sqlite::Store::open_for_repo(&f.repo, STORE_TTL_MS).expect("store");
    assert_eq!(store.ledger_settles(&node).expect("settles").len(), 1);
}

/// S1 (e2e 0.7.1): a `done --residual` refused because someone else holds
/// the node records no residual, so the holder's work isn't waived under
/// them: neither a person's nor the holder's own coordinator's (`wrk`
/// reports to `orchestrator`), each of whom may record one.
#[test]
fn done_residual_is_atomic_with_lease_refusal() {
    let f = Fixture::new("Body.\n");
    f.blocker();
    f.ok("wrk", &["claim", NODE]);
    let (code, refused) = f.refused("human:nick", &["done", NODE, "--residual", "accepted"]);
    assert_eq!((code, refused.as_str()), (5, "coord.lease_held"));
    assert_eq!(f.state("F1"), "open", "nothing recorded");
    let (code, refused) = f.refused("orchestrator", &["done", NODE, "--residual", "accepted"]);
    assert_eq!((code, refused.as_str()), (5, "coord.lease_held"));
    assert_eq!(f.state("F1"), "open", "nothing recorded");
}

/// A29: condition 2's out-of-range verified findings are never waived:
/// `--residual` refuses with the gate's failure and records nothing.
#[test]
fn done_residual_never_waives_an_uncontained_verified_finding() {
    let f = Fixture::new("Body.\n");
    f.git(&["checkout", "-q", "-b", "work"]);
    f.reviewed_fix();
    f.git(&["checkout", "-q", "main"]);
    let (code, envelope) = f.run("orchestrator", &["done", NODE, "--residual", "ship it"]);
    assert_eq!(code, 5, "{envelope:#}");
    assert_eq!(envelope["diagnostics"][0]["code"], "entry.gate_failed");
    let issues = f.ok("rev", &["entry", "list", NODE, "--kind", "issue"]);
    assert_eq!(issues["entries"], json!([]), "nothing recorded");
}

/// The orchestrator on #11043: a person may run `done --residual` outside
/// a turn; inside one it's refused, gate passing or not, and plain `done`
/// as a person stays refused. A person's `entry residual` follows the same
/// rule (Command::Entry in the human:* allowance).
#[test]
fn a_persons_residuals_run_outside_turns_only() {
    let f = Fixture::new("Body.\n");
    f.blocker();
    let (code, refused) = f.refused("human:nick", &["done", NODE]);
    assert_eq!((code, refused.as_str()), (1, "msg.human_actor"));
    let turn = [("MDKG_RUN_TURN", "t-1")];
    for args in [
        &["done", NODE, "--residual", "ship it"][..],
        &["entry", "residual", NODE, "F1", "--reason", "ship it"][..],
    ] {
        let (code, envelope) = f.run_with("human:nick", args, &turn);
        assert_ne!(code, 0, "{args:?}: {envelope:#}");
        assert_eq!(
            envelope["diagnostics"][0]["code"], "entry.not_allowed",
            "{args:?}: {envelope:#}"
        );
    }
    assert_eq!(f.state("F1"), "open", "nothing recorded in a turn");
    f.ok(
        "human:nick",
        &["entry", "residual", NODE, "F1", "--reason", "ship it"],
    );
    assert_eq!(f.state("F1"), "residual");
    // A passing gate, a person, a turn: still refused.
    let (code, envelope) = f.run_with("human:nick", &["done", NODE, "--residual", "x"], &turn);
    assert_eq!(code, 5, "{envelope:#}");
    // Outside a turn, with nothing left to accept, it records none and
    // finishes.
    let done = f.ok("human:nick", &["done", NODE, "--residual", "nothing left"]);
    assert!(done["residuals"].is_null(), "{done:#}");
    assert!(done["commit"].is_string(), "{done:#}");
    assert_eq!(f.read_status(), (json!("done"), Value::Null));
}

/// Terra's #11125: a findings verdict with no backing finding, and a
/// verify whose SHA undoes the fix, are refusals on the ledger's state:
/// their own codes, exit 5, never a usage error.
#[test]
fn ledger_state_refusals_exit_as_conflicts() {
    let f = Fixture::new("Body.\n");
    f.ok(
        "rev",
        &[
            "entry",
            "add",
            NODE,
            "--kind",
            "finding",
            "--title",
            "Off by one",
        ],
    );
    let base = f.git(&["rev-parse", "HEAD"]);
    let fix = f.commit("src/lib.rs", "fixed\n");
    // The fix undone: src/lib.rs is back to absent, as at fix's parent.
    f.git(&["rm", "-q", "src/lib.rs"]);
    f.git(&["commit", "-qm", "revert the fix"]);
    let reverted = f.git(&["rev-parse", "HEAD"]);
    f.ok("wrk", &["entry", "fix", NODE, "F1", "--sha", &fix]);
    f.open_round_for(&base, &reverted, "m1", &["rev"]);
    let (code, refused) = f.refused("rev", &["entry", "verify", NODE, "F1", "--at", &reverted]);
    assert_eq!((code, refused.as_str()), (5, "entry.fix_absent"));
    // rev's finding came before the round opened, so it backs nothing.
    let range = format!("{base}..{reverted}");
    let (code, refused) = f.refused(
        "rev",
        &[
            "entry",
            "complete",
            NODE,
            "V1",
            "--range",
            &range,
            "--verdict",
            "findings",
        ],
    );
    assert_eq!((code, refused.as_str()), (5, "entry.verdict_unbacked"));
}

/// Terra's #11173, major: only the exact lowercase `human:` is a person
/// for ledger ops. `HUMAN:nick` is refused as `msg.human_actor`, inside a
/// turn and out of one, for `done --residual` and `entry add`, and
/// records nothing; so is a malformed name after the prefix (#11205).
#[test]
fn a_mixed_case_human_name_is_never_a_person() {
    let f = Fixture::new("Body.\n");
    let head = f.git(&["rev-parse", "HEAD"]);
    for (actor, vars) in [
        ("HUMAN:nick", &[][..]),
        ("HUMAN:nick", &[("MDKG_RUN_TURN", "t-1")][..]),
        ("human:", &[][..]),
        ("human:nick ", &[][..]),
        ("human:Ana", &[][..]),
        ("human:ni\nck", &[][..]),
    ] {
        for args in [
            &["done", NODE, "--residual", "x"][..],
            &["entry", "add", NODE, "--kind", "issue", "--title", "Forged"][..],
        ] {
            let (code, envelope) = f.run_with(actor, args, vars);
            assert_eq!(code, 1, "{actor:?} {args:?} {vars:?}: {envelope:#}");
            assert_eq!(
                envelope["diagnostics"][0]["code"], "msg.human_actor",
                "{actor:?} {args:?} {vars:?}: {envelope:#}"
            );
        }
    }
    assert_eq!(f.git(&["rev-parse", "HEAD"]), head, "nothing committed");
    let issues = f.ok("rev", &["entry", "list", NODE]);
    assert_eq!(issues["entries"], json!([]), "nothing recorded");
}

/// The typed `entry list` and `entry show` output (`EntryRecord`,
/// `EntryEvent`) prints the names and values the untyped one did,
/// `Entry::to_json` and `Event::to_json` on the same store, for a sample
/// ledger with every kind and the ops that write each context key. Every
/// envelope it prints conforms to its `mdkg.entry.<sub>.v1` schema.
#[test]
fn typed_list_and_show_print_what_the_store_records_gave() {
    let f = Fixture::new("Body.\n\n# Requirements\n\n- R1: The thing shall work.\n");
    let checked = |actor: &str, args: &[&str]| -> Value {
        let (code, envelope) = f.run(actor, args);
        // A failed gate exits 2 with its data (orchestrator #17713).
        let failed_gate = args[..2] == ["entry", "gate"] && envelope["data"]["ok"] == false;
        assert_eq!(
            code,
            if failed_gate { 2 } else { 0 },
            "mdkg {args:?}: {envelope:#}"
        );
        common::conforms_envelope(&envelope);
        // The producer check too: a nested key its type doesn't declare
        // fails, so a context key that fell back to the record's `other`
        // (its stored value not its type's) fails here, never passes as
        // typed (Terra's review of bfc039d0, minor 1).
        let command = args[..2].join(".");
        let schema = common::schema(&command);
        let mut errors = Vec::new();
        common::api_schema::check_emitted(&schema, &schema, &envelope, "$", &mut errors);
        assert!(errors.is_empty(), "{command}: {errors:?}");
        envelope["data"].clone()
    };
    let add = |actor: &str, args: &[&str]| {
        let mut all = vec!["entry"];
        all.extend_from_slice(args);
        checked(actor, &all);
    };
    add(
        "rev",
        &[
            "add",
            NODE,
            "--kind",
            "finding",
            "--severity",
            "major",
            "--title",
            "Off by one",
            "--anchor",
            "src/lib.rs:3",
            "--requirement",
            "R1",
        ],
    );
    add(
        "rev",
        &[
            "add",
            NODE,
            "--kind",
            "finding",
            "--title",
            "Off by one, again",
            "--anchor",
            "src/lib.rs",
        ],
    );
    add("rev", &["duplicate", NODE, "F2", "--of", "F1"]);
    add(
        "rev",
        &[
            "ask",
            NODE,
            "--title",
            "Which schema?",
            "--asked-of",
            "wrk",
            "--text",
            "Store or mail?",
        ],
    );
    add("wrk", &["answer", NODE, "Q1", "--text", "Store."]);
    add(
        "wrk",
        &[
            "add",
            NODE,
            "--kind",
            "issue",
            "--title",
            "Flaky test",
            "--blocking",
        ],
    );
    add(
        "rev",
        &[
            "add",
            NODE,
            "--kind",
            "decision",
            "--title",
            "Keep mail at 3",
            "--ref",
            "Q1",
        ],
    );
    add(
        "rev",
        &[
            "propose",
            NODE,
            "--title",
            "Mail at 4",
            "--slot",
            "mail",
            "--supersedes",
            "D1",
        ],
    );
    add("rev", &["decide", NODE, "D2"]);
    let base = f.git(&["rev-parse", "HEAD"]);
    let fix = f.commit("src/lib.rs", "fixed\n");
    add("wrk", &["fix", NODE, "F1", "--sha", &fix]);
    let head = f.commit("src/other.rs", "later\n");
    f.open_round(&base, &head, "m1");
    add("rev", &["verify", NODE, "F1", "--at", &head]);
    let range = format!("{base}..{head}");
    add(
        "rev",
        &[
            "complete",
            NODE,
            "V1",
            "--range",
            &range,
            "--verdict",
            "clean",
        ],
    );
    add("rev", &["req", NODE, "R1", "--state", "met", "--at", &fix]);
    // `update`: one line applied, one refused with its code.
    let ops = f.dir.join("ops.jsonl");
    fs::write(
        &ops,
        "{\"op\": \"add\", \"kind\": \"issue\", \"title\": \"Docs\"}\n{\"op\": \"nope\"}\n",
    )
    .expect("ops");
    let updated = checked(
        "wrk",
        &[
            "entry",
            "update",
            NODE,
            "--op-file",
            ops.to_str().expect("ops"),
        ],
    );
    assert_eq!(updated["results"][1]["ok"], false, "{updated:#}");
    checked("rev", &["entry", "gate", NODE]);

    let node = mdkg_coord_sqlite::ledger::NodeLink::of(NODE).expect("node");
    let store = mdkg_coord_sqlite::Store::open_for_repo(&f.repo, STORE_TTL_MS).expect("store");
    let before = store
        .entries(&node.uuid, &Default::default())
        .expect("entries");
    let listed = checked("rev", &["entry", "list", NODE]);
    let expected: Vec<Value> = before.iter().map(|e| e.to_json()).collect();
    assert_eq!(listed["entries"], json!(expected), "{listed:#}");
    let kinds: std::collections::BTreeSet<&str> = before.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(kinds.len(), 6, "every kind: {kinds:?}");
    let mut keys = std::collections::BTreeSet::new();
    for e in &before {
        keys.extend(e.data.keys().cloned());
        let shown = checked("rev", &["entry", "show", NODE, &e.id]);
        assert_eq!(shown["entry"], e.to_json(), "{}", e.id);
        let history: Vec<Value> = store
            .entry_history(&node.uuid, &e.id)
            .expect("history")
            .iter()
            .map(|h| h.to_json())
            .collect();
        assert!(!history.is_empty(), "{}", e.id);
        assert_eq!(shown["history"], json!(history), "{}", e.id);
    }
    // The sample reaches the context keys the record types.
    for key in [
        "opened",
        "anchor",
        "requirement",
        "asked_of",
        "refs",
        "range",
        "reviewers",
        "completed",
        "request",
        "anchored",
        "answer",
        "answered_by",
        "fix",
        "fix_commits",
        "fixers",
        "verified",
        "duplicate_of",
        "supersedes",
        "superseded_by",
        "graph_folder",
    ] {
        assert!(keys.contains(key), "no entry holds {key}: {keys:?}");
    }
}

#[test]
fn digest_import_binds_nodes_and_all_review_references() {
    let f = Fixture::new("# Overview\n\nTest.\n");
    for id in ["request-one", "request-two"] {
        f.ok(
            "orchestrator",
            &[
                "msg", "send", "--to", "rev", "--kind", "note", "--node", NODE, "--id", id,
                "--text", "review",
            ],
        );
    }
    let file = f.dir.join("reply.txt");
    fs::write(&file, "finished").unwrap();
    let reply = f.ok(
        "launcher",
        &[
            "msg",
            "import",
            "--from",
            "rev",
            "--to",
            "orchestrator",
            "--kind",
            "findings",
            "--node",
            NODE,
            "--reply-to",
            "request-one",
            "--reply-to",
            "request-two",
            file.to_str().unwrap(),
        ],
    );
    let id = reply["message"]["messageId"].as_str().unwrap();
    assert_eq!(
        reply["message"]["referenceIds"],
        json!(["request-one", "request-two"])
    );
    let store = mdkg_coord_sqlite::Store::open_for_repo(&f.repo, STORE_TTL_MS).unwrap();
    assert_eq!(store.message_flow(id).unwrap().nodes[0].id, NODE);
}

#[test]
fn digest_import_retains_cross_branch_node_and_starts_its_own_conversation() {
    let f = Fixture::new("# Overview\n\nTest.\n");
    let sent = f.ok(
        "another-reviewer",
        &[
            "msg",
            "send",
            "--to",
            "rev",
            "--kind",
            "note",
            "--node",
            NODE,
            "--id",
            "other-request",
            "--text",
            "review",
        ],
    );
    let old_context = sent["message"]["contextId"].as_str().unwrap().to_owned();
    fs::remove_file(f.repo.join(NODE_PATH)).unwrap();
    let file = f.dir.join("reply.txt");
    fs::write(&file, "finished").unwrap();
    let args = [
        "msg",
        "import",
        "--from",
        "rev",
        "--to",
        "orchestrator",
        "--kind",
        "findings",
        "--node-id",
        NODE,
        "--standalone",
        "--reply-to",
        "other-request",
        file.to_str().unwrap(),
    ];
    let reply = f.ok("launcher", &args);
    let id = reply["message"]["messageId"].as_str().unwrap();
    assert_ne!(reply["message"]["contextId"], old_context);
    assert_eq!(reply["message"]["contextId"], id);
    assert_eq!(reply["message"]["referenceIds"], json!(["other-request"]));
    let store = mdkg_coord_sqlite::Store::open_for_repo(&f.repo, STORE_TTL_MS).unwrap();
    assert_eq!(store.message_flow(id).unwrap().nodes[0].id, NODE);
    assert_eq!(f.ok("launcher", &args)["duplicate"], true);
}

#[test]
fn digest_mail_transport_marks_only_delivered_bodies() {
    let f = Fixture::new("# Overview\n\nTest.\n");
    f.ok(
        "orchestrator",
        &[
            "msg", "send", "--to", "rev", "--kind", "note", "--node", NODE, "--id", "first",
            "--text", "one",
        ],
    );
    f.ok(
        "orchestrator",
        &[
            "msg", "send", "--to", "rev", "--kind", "note", "--id", "second", "--text", "two",
        ],
    );
    let inbox = f.ok("rev", &["msg", "inbox", "--no-mark"]);
    let messages = inbox["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["nodes"], json!([NODE]));
    let (_, code) = f.refused("rev", &["msg", "ack", "first"]);
    assert_eq!(code, "msg.not_shown");
    let mark = format!(
        "{}/{}/{}",
        messages[0]["seq"].as_u64().unwrap(),
        messages[0]["digest"].as_str().unwrap(),
        messages[0]["messageId"].as_str().unwrap()
    );
    f.ok("rev", &["msg", "inbox", "--mark-shown", &mark]);
    f.ok("rev", &["msg", "ack", "first"]);
    let (_, code) = f.refused("rev", &["msg", "ack", "second"]);
    assert_eq!(code, "msg.not_shown");
}
