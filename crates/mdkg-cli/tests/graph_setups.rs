//! Install, the launcher and doctor across the graph setups (edd-01a18867,
//! lane L4a): checkout mode, ref mode, and a clone of a ref-mode
//! repository, each also with no main branch at all (R2: mdkg never
//! requires main).

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
        let dir = common::temp::dir("mdkg-graph-setups");
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

    fn git_in(&self, dir: &Path, args: &[&str]) -> String {
        let out = self
            .isolated(Command::new("git").arg("-C").arg(dir).args(args))
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    fn git(&self, args: &[&str]) -> String {
        self.git_in(&self.repo(), args)
    }

    fn mdkg_in(&self, dir: &Path, args: &[&str]) -> (i32, Value) {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_mdkg"));
        self.isolated(&mut cmd).current_dir(dir).args(args);
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

    fn mdkg(&self, args: &[&str]) -> (i32, Value) {
        self.mdkg_in(&self.repo(), args)
    }

    /// A repository on `branch` with one code commit and its own identity
    /// for commits.
    fn code(&self, branch: &str) {
        self.git(&["init", "-q", "-b", branch]);
        self.git(&["config", "user.name", "Graph Test"]);
        self.git(&["config", "user.email", "graph@example.invalid"]);
        fs::write(self.repo().join("code.rs"), "fn main() {}\n").expect("code");
        self.git(&["add", "code.rs"]);
        self.git(&["commit", "-q", "-m", "code"]);
    }

    /// [`Scratch::code`] in ref mode, with the graph created on the graph
    /// ref by `init`.
    fn ref_mode(&self, branch: &str) {
        self.code(branch);
        self.git(&["config", "mdkg.graph.ref", GRAPH]);
        let (code, init) = self.mdkg(&["init", "--json"]);
        assert_eq!(code, 0, "{init}");
    }
}

fn codes(v: &Value) -> Vec<String> {
    v["diagnostics"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|d| d["code"].as_str().map(str::to_owned))
        .collect()
}

/// The launcher's dry-run turn of reviewer `r`, as a person runs it from
/// `dir`.
fn dry_turn(s: &Scratch, dir: &Path) -> (i32, Value) {
    s.mdkg_in(
        dir,
        &[
            "--as",
            "launcher",
            "agents",
            "up",
            "r",
            "--once",
            "--prompt",
            "x",
            "--dry-run",
            "--json",
        ],
    )
}

const REVIEWER: &str =
    "{\"schema\": \"mdkg.agent.v1\", \"harness\": \"claude\", \"role\": \"reviewer\"}\n";

/// Checkout mode keeps 0.7.0's refusal: there, mdkg commits graph writes
/// to the checkout's branch. (Ref mode's side is run.rs's unit test:
/// `agents up` in ref mode is L5b's.)
#[test]
fn checkout_mode_still_refuses_the_launcher_on_main() {
    let s = Scratch::new();
    s.code("main");
    let (code, init) = s.mdkg(&["init", "--json"]);
    assert_eq!(code, 0, "{init}");
    fs::create_dir_all(s.repo().join(".mdkg/agents")).expect("agents");
    fs::write(s.repo().join(".mdkg/agents/r.json"), REVIEWER).expect("definition");
    s.git(&["add", ".mdkg"]);
    s.git(&["commit", "-q", "-m", "graph"]);
    let (code, out) = dry_turn(&s, &s.repo());
    assert_eq!(code, 1, "{out}");
    assert_eq!(codes(&out), ["run.branch_refused"], "{out}");
}

/// Local work in ref mode (g-docs' finding; R1, C25): `init`'s default
/// `work.location: "local"` keeps work nodes beside the coordinator, so
/// `new` and `next` work, and the graph ref never receives the task.
#[test]
fn ref_mode_keeps_local_work_beside_the_coordinator() {
    let s = Scratch::new();
    s.ref_mode("main");
    let tip = s.git(&["rev-parse", GRAPH]);
    let (code, made) = s.mdkg(&["new", "task", "Local", "work", "--status", "todo", "--json"]);
    assert_eq!(code, 0, "{made}");
    let path = made["data"]["node"]["path"].as_str().expect("path");
    let common = PathBuf::from(s.git(&["rev-parse", "--path-format=absolute", "--git-common-dir"]));
    assert!(
        Path::new(path).starts_with(common.join("mdkg/work/root"))
            || common.join("mdkg/work/root").join(path).is_file(),
        "{made}"
    );
    let (code, next) = s.mdkg(&["next", "--json"]);
    assert_eq!(code, 0, "{next}");
    assert_eq!(next["data"]["node"]["title"], "Local work", "{next}");
    assert_eq!(
        s.git(&["rev-parse", GRAPH]),
        tip,
        "the graph ref is untouched"
    );
}

fn check_of<'a>(v: &'a Value, name: &str) -> &'a Value {
    v["data"]["checks"]
        .as_array()
        .and_then(|c| c.iter().find(|c| c["name"] == name))
        .unwrap_or_else(|| panic!("no {name} check: {v}"))
}

fn has_check(v: &Value, name: &str) -> bool {
    v["data"]["checks"]
        .as_array()
        .is_some_and(|c| c.iter().any(|c| c["name"] == name))
}

/// Row 14: in ref mode doctor has no `main` check, an info-only
/// `code_base` one, and the graph checks; nothing needs a main branch
/// (R2, R37). The graph is read from the graph ref, not the checkout.
#[test]
fn ref_mode_doctor_needs_no_main() {
    for branch in ["main", "trunk"] {
        let s = Scratch::new();
        s.ref_mode(branch);
        let (code, v) = s.mdkg(&["doctor", "--json"]);
        assert_eq!(code, 0, "{v}");
        assert!(!has_check(&v, "main"), "{v}");
        assert_eq!(check_of(&v, "code_base")["status"], "ok", "{v}");
        assert_eq!(check_of(&v, "graph.ref")["status"], "ok", "{v}");
        assert_eq!(check_of(&v, "config")["status"], "ok", "{v}");
        assert_eq!(check_of(&v, "graph")["status"], "ok", "{v}");
        for name in [
            "graph.shape",
            "graph.remote",
            "graph.intents",
            "graph.cache",
            "graph.view",
            "graph.hand_edits",
        ] {
            assert_eq!(check_of(&v, name)["status"], "ok", "{name}: {v}");
        }
        // `init` adds node edit's exclusion too (B18).
        assert_eq!(check_of(&v, "graph.exclude")["status"], "ok", "{v}");
    }
}

/// A configured code base that doesn't resolve only warns.
#[test]
fn a_missing_code_base_warns_and_a_missing_graph_ref_fails() {
    let s = Scratch::new();
    s.ref_mode("trunk");
    s.git(&["config", "mdkg.mainRef", "refs/remotes/origin/main"]);
    let (code, v) = s.mdkg(&["doctor", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(check_of(&v, "code_base")["status"], "warning", "{v}");
    assert_eq!(check_of(&v, "graph.design_drift")["status"], "skip", "{v}");
    s.git(&["update-ref", "-d", GRAPH]);
    let (code, v) = s.mdkg(&["doctor", "--json"]);
    assert_eq!(code, 2, "{v}");
    assert_eq!(check_of(&v, "graph.ref")["status"], "error", "{v}");
    assert_eq!(check_of(&v, "config")["status"], "skip", "{v}");
}

/// Checkout mode keeps 0.7.0's `main` check and says how to move to the
/// graph ref.
#[test]
fn checkout_mode_doctor_names_the_move_to_the_graph_ref() {
    let s = Scratch::new();
    s.code("main");
    let (code, init) = s.mdkg(&["init", "--json"]);
    assert_eq!(code, 0, "{init}");
    let (_, v) = s.mdkg(&["doctor", "--json"]);
    // Info, so `--strict` passes a supported checkout-mode clone
    // (dec-01a18ee0 item 10).
    assert_eq!(check_of(&v, "graph.ref")["status"], "ok", "{v}");
    assert!(
        check_of(&v, "graph.ref")["summary"]
            .as_str()
            .is_some_and(|t| t.starts_with("info: checkout mode")),
        "{v}"
    );
    assert!(
        check_of(&v, "graph.ref")["hint"]
            .as_str()
            .is_some_and(|h| h.contains("install --setup")),
        "{v}"
    );
    assert!(has_check(&v, "main"), "{v}");
    assert!(!has_check(&v, "code_base"), "{v}");
}

/// The edd's doctor table: `graph.hand_edits` warns when the invoking
/// checkout holds graph-folder changes that differ from the graph.
#[test]
fn hand_edits_are_uncommitted_changes_that_differ_from_the_graph() {
    let s = Scratch::new();
    s.ref_mode("main");
    // A committed copy is main's, never read: design drift's business.
    // An uncommitted copy equal to the tip's isn't a hand edit either.
    let config = s.git(&["show", &format!("{GRAPH}:.mdkg/config.json")]);
    fs::create_dir_all(s.repo().join(".mdkg/work")).expect("dir");
    fs::write(s.repo().join(".mdkg/config.json"), format!("{config}\n")).expect("same");
    let (_, v) = s.mdkg(&["doctor", "--json"]);
    assert_eq!(check_of(&v, "graph.hand_edits")["status"], "ok", "{v}");
    fs::write(s.repo().join(".mdkg/work/task-x.md"), "hand edit\n").expect("edit");
    let (_, v) = s.mdkg(&["doctor", "--json"]);
    let edits = check_of(&v, "graph.hand_edits");
    assert_eq!(edits["status"], "warning", "{v}");
    assert!(
        edits["summary"]
            .as_str()
            .is_some_and(|t| t.starts_with("1 path uncommitted")),
        "{v}"
    );
}

/// R33: graph.remote's protected names are the turn's and the hook's.
#[test]
fn a_remote_ref_on_a_slashed_code_branch_is_protected() {
    let s = Scratch::new();
    s.ref_mode("main");
    let bare = s.0.join("graph.git");
    s.git(&["init", "-q", "--bare", &bare.to_string_lossy()]);
    s.git(&["remote", "add", "origin", &bare.to_string_lossy()]);
    s.git(&["config", "mdkg.graph.remote", "origin"]);
    s.git(&["config", "mdkg.graph.remoteRef", "refs/heads/release/1.0"]);
    s.git(&["config", "mdkg.mainRef", "refs/remotes/origin/release/1.0"]);
    let (_, v) = s.mdkg(&["doctor", "--json"]);
    let remote = check_of(&v, "graph.remote");
    assert_eq!(remote["status"], "error", "{v}");
    assert!(
        remote["summary"]
            .as_str()
            .is_some_and(|t| t.contains("protected code branch")),
        "{v}"
    );
}

/// R33: graph.remote compares protected names without case, as the hook
/// does, so `refs/heads/Main` is the protected `main`.
#[test]
fn a_remote_ref_differing_only_in_case_is_protected() {
    let s = Scratch::new();
    s.ref_mode("main");
    let bare = s.0.join("graph.git");
    s.git(&["init", "-q", "--bare", &bare.to_string_lossy()]);
    s.git(&["remote", "add", "origin", &bare.to_string_lossy()]);
    s.git(&["config", "mdkg.graph.remote", "origin"]);
    s.git(&["config", "mdkg.graph.remoteRef", "refs/heads/Main"]);
    let (_, v) = s.mdkg(&["doctor", "--json"]);
    let remote = check_of(&v, "graph.remote");
    assert_eq!(remote["status"], "error", "{v}");
    assert!(
        remote["summary"]
            .as_str()
            .is_some_and(|t| t.contains("protected code branch")),
        "{v}"
    );
    // Non-ASCII too, as the hook lowercases (#13646).
    s.git(&["config", "mdkg.mainRef", "refs/heads/ÉCOLE"]);
    s.git(&["config", "mdkg.graph.remoteRef", "refs/heads/école"]);
    let (_, v) = s.mdkg(&["doctor", "--json"]);
    assert_eq!(check_of(&v, "graph.remote")["status"], "error", "{v}");
}

/// G16, R43: the code base's design folders against the graph tip's, as
/// info.
#[test]
fn design_drift_is_reported_as_info() {
    let s = Scratch::new();
    s.ref_mode("main");
    s.git(&["config", "mdkg.mainRef", "refs/heads/main"]);
    fs::create_dir_all(s.repo().join(".mdkg/design")).expect("dir");
    fs::write(s.repo().join(".mdkg/design/edd-1.md"), "old copy\n").expect("design");
    s.git(&["add", ".mdkg/design/edd-1.md"]);
    s.git(&["commit", "-q", "-m", "published design"]);
    let (code, v) = s.mdkg(&["doctor", "--json"]);
    assert_eq!(code, 0, "{v}");
    let drift = check_of(&v, "graph.design_drift");
    assert_eq!(drift["status"], "ok", "{v}");
    assert!(
        drift["summary"]
            .as_str()
            .is_some_and(|t| t.starts_with("info:") && t.contains(".mdkg/design/edd-1.md")),
        "{v}"
    );
}

impl Scratch {
    fn config(&self, key: &str) -> Option<String> {
        let out = self
            .isolated(Command::new("git").arg("-C").arg(self.repo()))
            .args(["config", "--file", ".git/config", "--get", key])
            .output()
            .expect("git");
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }

    /// Checkout mode as 0.7.0 has it: the graph committed on `branch`.
    fn checkout_graph(&self, branch: &str) {
        self.code(branch);
        let (code, init) = self.mdkg(&["init", "--json"]);
        assert_eq!(code, 0, "{init}");
        self.git(&["add", ".mdkg"]);
        self.git(&["commit", "-q", "-m", "graph"]);
    }
}

fn notes(v: &Value) -> String {
    v["data"]["notes"].to_string()
}

/// R30, C1: `--setup local` creates the graph ref as an orphan, graph-only
/// commit of the checkout's graph by a create compare-and-swap, records
/// the identity and the keys, and moves no code branch. With no main at
/// all, mainRef stays unset (R2).
#[test]
fn local_setup_creates_a_graph_only_ref_from_the_checkout_graph() {
    for branch in ["main", "trunk"] {
        let s = Scratch::new();
        s.checkout_graph(branch);
        let head = s.git(&["rev-parse", "HEAD"]);
        let (code, v) = s.mdkg(&["install", "--setup", "local", "--json"]);
        assert_eq!(code, 0, "{v}");
        assert_eq!(s.git(&["rev-parse", "HEAD"]), head, "{v}");
        assert_eq!(s.config("mdkg.graph.ref").as_deref(), Some(GRAPH));
        let tree = s.git(&["ls-tree", "--name-only", GRAPH]);
        assert_eq!(tree, ".mdkg", "graph-only");
        assert!(
            s.git(&["rev-list", "--parents", "-n1", GRAPH])
                .split(' ')
                .count()
                == 1,
            "an orphan"
        );
        let graph_id: Value =
            serde_json::from_str(&s.git(&["show", &format!("{GRAPH}:.mdkg/config.json")]))
                .expect("config");
        let id = graph_id["graph_id"].as_str().expect("id");
        assert_eq!(
            s.config(&format!("mdkg.graph.{id}.ref")).as_deref(),
            Some(GRAPH)
        );
        let main = (branch == "main").then(|| "refs/heads/main".to_owned());
        assert_eq!(s.config("mdkg.mainRef"), main, "{v}");
        assert_eq!(s.config("mdkg.graph.remote"), None);
        assert_eq!(s.config("mdkg.follow.fetch").as_deref(), Some("off"));
        assert_eq!(s.config("mdkg.graph.publishDesign").as_deref(), Some("off"));
        let exclude = fs::read_to_string(s.repo().join(".git/info/exclude")).expect("exclude");
        assert!(exclude.lines().any(|l| l == "/.mdkg-edit/"), "{exclude}");
        // The clone reads the graph ref now, and doctor agrees.
        let (code, list) = s.mdkg(&["list", "--json"]);
        assert_eq!(code, 0, "{list}");
        let (code, doctor) = s.mdkg(&["doctor", "--json"]);
        assert_eq!(code, 0, "{doctor}");
        assert_eq!(
            check_of(&doctor, "graph.exclude")["status"],
            "ok",
            "{doctor}"
        );
        // Again: nothing more to do.
        let tip = s.git(&["rev-parse", GRAPH]);
        let (code, again) = s.mdkg(&["install", "--json"]);
        assert_eq!(code, 0, "{again}");
        assert!(notes(&again).contains("(inferred)"), "{again}");
        assert_eq!(s.git(&["rev-parse", GRAPH]), tip);
    }
}

#[test]
fn a_setup_dry_run_writes_nothing() {
    let s = Scratch::new();
    s.checkout_graph("main");
    let refs = s.git(&["for-each-ref"]);
    let config = fs::read_to_string(s.repo().join(".git/config")).expect("config");
    let (code, v) = s.mdkg(&["install", "--setup", "local", "--dry-run", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert!(notes(&v).contains("dry run"), "{v}");
    assert_eq!(s.git(&["for-each-ref"]), refs);
    assert_eq!(
        fs::read_to_string(s.repo().join(".git/config")).expect("config"),
        config
    );
}

/// A clone of a ref-mode repository: install adopts the remote-tracking
/// graph ref `git clone` made, fetching nothing, and pins no destination
/// (G23). The origin has no main branch at all.
#[test]
fn personal_setup_adopts_a_cloned_graph_ref() {
    let s = Scratch::new();
    s.ref_mode("trunk");
    let tip = s.git(&["rev-parse", GRAPH]);
    let clone = s.0.join("clone");
    s.git_in(
        &s.0,
        &[
            "clone",
            "-q",
            &s.repo().to_string_lossy(),
            &clone.to_string_lossy(),
        ],
    );
    s.git_in(&clone, &["config", "user.name", "Graph Test"]);
    s.git_in(&clone, &["config", "user.email", "graph@example.invalid"]);
    let (code, v) = s.mdkg_in(&clone, &["install", "--setup", "personal", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(s.git_in(&clone, &["rev-parse", GRAPH]), tip);
    let get = |k: &str| s.git_in(&clone, &["config", "--file", ".git/config", "--get", k]);
    assert_eq!(get("mdkg.graph.ref"), GRAPH);
    assert_eq!(get("mdkg.graph.remote"), "origin");
    assert_eq!(get("mdkg.follow.fetch"), "on");
    assert_eq!(get("mdkg.graph.publishDesign"), "on");
    assert_eq!(get("mdkg.mainRef"), "refs/remotes/origin/trunk");
    assert!(notes(&v).contains("approves"), "{v}");
    assert!(
        !notes(&v).contains("remote approve"),
        "never a missing command: {v}"
    );
    assert!(
        !clone.join(".git/mdkg/graph/remote.json").exists(),
        "nothing pinned"
    );
    let (code, list) = s.mdkg_in(&clone, &["list", "--json"]);
    assert_eq!(code, 0, "{list}");
}

/// Professional: the graph remote is required, must exist, and shares no
/// URL with the company remote; a full-shape graph is refused.
#[test]
fn professional_setup_guards_the_company_remote() {
    let s = Scratch::new();
    s.checkout_graph("main");
    let company = s.0.join("company.git");
    let mine = s.0.join("mine.git");
    for bare in [&company, &mine] {
        s.git_in(&s.0, &["init", "-q", "--bare", &bare.to_string_lossy()]);
    }
    s.git(&["remote", "add", "origin", &company.to_string_lossy()]);
    let (code, v) = s.mdkg(&["install", "--setup", "professional", "--json"]);
    assert_ne!(code, 0, "{v}");
    assert!(v.to_string().contains("--graph-remote"), "{v}");
    s.git(&["remote", "add", "alias", &company.to_string_lossy()]);
    let (code, v) = s.mdkg(&[
        "install",
        "--setup",
        "professional",
        "--graph-remote",
        "alias",
        "--json",
    ]);
    assert_ne!(code, 0, "{v}");
    assert_eq!(codes(&v), ["graph.remote_is_code"], "{v}");
    s.git(&["remote", "add", "mine", &mine.to_string_lossy()]);
    let (code, v) = s.mdkg(&[
        "install",
        "--setup",
        "professional",
        "--graph-remote",
        "mine",
        "--json",
    ]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(s.config("mdkg.graph.remote").as_deref(), Some("mine"));
    assert_eq!(s.config("mdkg.follow.fetch").as_deref(), Some("off"));
    assert_eq!(s.config("mdkg.graph.publishDesign").as_deref(), Some("off"));
    assert_eq!(s.git(&["ls-tree", "--name-only", GRAPH]), ".mdkg");
    // Nothing reached either remote.
    for bare in [&company, &mine] {
        assert_eq!(s.git_in(bare, &["for-each-ref"]), "");
    }
}

/// Rollback (Terra #6198 major 18): `--checkout-mode` unsets
/// `mdkg.graph.ref` and keeps the graph ref.
/// Claude-4 #13116, major 1: the clone's own `mdkg.graph.ref` is the one
/// kept and checked, never a leftover `mdkg-graph`.
#[test]
fn a_setup_keeps_a_custom_named_graph_ref() {
    let s = Scratch::new();
    s.code("main");
    s.git(&["config", "mdkg.graph.ref", "refs/heads/team-graph"]);
    let (code, init) = s.mdkg(&["init", "--json"]);
    assert_eq!(code, 0, "{init}");
    let team = s.git(&["rev-parse", "refs/heads/team-graph"]);
    // A leftover default-named ref holding code, which a professional
    // setup would refuse as full shape if it were the one checked.
    s.git(&["update-ref", GRAPH, "refs/heads/main"]);
    let (code, v) = s.mdkg(&["install", "--setup", "local", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(
        s.config("mdkg.graph.ref").as_deref(),
        Some("refs/heads/team-graph")
    );
    assert_eq!(s.git(&["rev-parse", "refs/heads/team-graph"]), team);
    assert_eq!(
        s.git(&["rev-parse", GRAPH]),
        s.git(&["rev-parse", "refs/heads/main"])
    );
    assert!(notes(&v).contains("refs/heads/team-graph (kept)"), "{v}");
}

/// Claude-4 #13116, minor 1: the professional guard compares where the
/// remotes lead, not their URL strings.
#[test]
fn professional_setup_sees_one_destination_through_other_spellings() {
    let s = Scratch::new();
    s.checkout_graph("main");
    s.git(&[
        "remote",
        "add",
        "origin",
        "git@example.invalid:acme/app.git",
    ]);
    for (name, url) in [
        ("https", "https://example.invalid/acme/app"),
        ("ssh", "ssh://git@Example.invalid:22/acme/app.git/"),
        ("short", "gh:acme/app"),
    ] {
        s.git(&["remote", "add", name, url]);
    }
    s.git(&["config", "url.https://example.invalid/.insteadOf", "gh:"]);
    for name in ["https", "ssh", "short"] {
        let (code, v) = s.mdkg(&[
            "install",
            "--setup",
            "professional",
            "--graph-remote",
            name,
            "--json",
        ]);
        assert_ne!(code, 0, "{name}: {v}");
        assert_eq!(codes(&v), ["graph.remote_is_code"], "{name}: {v}");
    }
    assert_eq!(s.config("mdkg.graph.ref"), None);
}

/// The key order's crash path: the graph ref made, `mdkg.graph.ref` not
/// yet written. Installing again keeps the ref and finishes the move.
#[test]
fn a_setup_interrupted_before_its_last_key_finishes_on_a_rerun() {
    let s = Scratch::new();
    s.checkout_graph("main");
    let (code, v) = s.mdkg(&["install", "--setup", "local", "--json"]);
    assert_eq!(code, 0, "{v}");
    let tip = s.git(&["rev-parse", GRAPH]);
    s.git(&["config", "--unset", "mdkg.graph.ref"]);
    let (code, v) = s.mdkg(&["install", "--setup", "local", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(s.git(&["rev-parse", GRAPH]), tip, "kept, not made again");
    assert_eq!(s.config("mdkg.graph.ref").as_deref(), Some(GRAPH));
}

#[test]
fn checkout_mode_rolls_a_clone_back() {
    let s = Scratch::new();
    s.checkout_graph("main");
    let (code, v) = s.mdkg(&["install", "--setup", "local", "--json"]);
    assert_eq!(code, 0, "{v}");
    let tip = s.git(&["rev-parse", GRAPH]);
    let (code, v) = s.mdkg(&["install", "--checkout-mode", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(s.config("mdkg.graph.ref"), None);
    assert_eq!(s.git(&["rev-parse", GRAPH]), tip);
    let (_, doctor) = s.mdkg(&["doctor", "--json"]);
    assert!(
        check_of(&doctor, "graph.ref")["summary"]
            .as_str()
            .is_some_and(|t| t.starts_with("info: checkout mode")),
        "{doctor}"
    );
}

/// Omni's ask (#12874): the store check carries its schema versions and
/// whether this build would upgrade it, as fields, not only in the label.
#[test]
fn doctor_reports_the_store_schema_as_fields() {
    let s = Scratch::new();
    s.ref_mode("main");
    let (code, made) = s.mdkg(&["new", "task", "Store", "--json"]);
    assert_eq!(code, 0, "{made}");
    let (_, v) = s.mdkg(&["doctor", "--json"]);
    let store = check_of(&v, "store");
    let schema = store["store_schema"].as_u64().expect("store_schema");
    assert!(store["mail_schema"].as_u64().is_some(), "{v}");
    assert_eq!(store["upgrade_needed"], false, "{v}");
    assert!(
        store["summary"]
            .as_str()
            .is_some_and(|t| t.contains(&format!("schema {schema}"))),
        "{v}"
    );
    let path = s.repo().join(".git/mdkg/coord.sqlite");
    let conn = rusqlite::Connection::open(&path).expect("open store");
    conn.execute("UPDATE meta SET value = '1' WHERE key = 'schema'", [])
        .expect("set schema");
    drop(conn);
    let (_, v) = s.mdkg(&["doctor", "--json"]);
    let store = check_of(&v, "store");
    assert_eq!(store["store_schema"], 1, "{v}");
    assert_eq!(store["upgrade_needed"], true, "{v}");
    assert_eq!(store["status"], "warning", "{v}");
}

/// Q7 (a): in ref mode the projections come from the graph tip. `--enable`
/// publishes the config's new list to the graph ref (one compare-and-swap),
/// and a missing canonical hook file with it; the projection files land
/// in the checkout, for Claude, Codex and OpenCode alike. The checkout
/// holds no `.mdkg` and gets none.
#[test]
fn ref_mode_projections_come_from_the_graph_tip() {
    let s = Scratch::new();
    s.ref_mode("main");
    let before = s.git(&["rev-parse", GRAPH]);
    let (code, v) = s.mdkg(&[
        "install",
        "--enable",
        "mcp:claude,mcp:codex,mcp:opencode,skills:.claude/skills,hooks:claude",
        "--json",
    ]);
    assert_eq!(code, 0, "{v}");
    assert_ne!(s.git(&["rev-parse", GRAPH]), before, "published: {v}");
    let listed: Value =
        serde_json::from_str(&s.git(&["show", &format!("{GRAPH}:.mdkg/config.json")]))
            .expect("config");
    let names = listed["projections"].to_string();
    for p in [
        "mcp:claude",
        "mcp:codex",
        "mcp:opencode",
        "skills:.claude/skills",
        "hooks:claude",
    ] {
        assert!(names.contains(p), "{p}: {names}");
    }
    s.git(&[
        "cat-file",
        "-e",
        &format!("{GRAPH}:.mdkg/hooks/claude/settings.json"),
    ]);
    for file in [
        ".mcp.json",
        ".codex/config.toml",
        "opencode.json",
        ".claude/skills/mdkg-workflow/SKILL.md",
        ".claude/settings.local.json",
    ] {
        assert!(s.repo().join(file).is_file(), "{file}: {v}");
    }
    assert!(!s.repo().join(".mdkg").exists(), "no checkout copy");
    // Again: nothing to publish, nothing changes.
    let tip = s.git(&["rev-parse", GRAPH]);
    let (code, again) = s.mdkg(&["install", "--json"]);
    assert_eq!(code, 0, "{again}");
    assert_eq!(s.git(&["rev-parse", GRAPH]), tip);
    // Disabling publishes again and removes mdkg's part.
    let (code, off) = s.mdkg(&["install", "--disable", "mcp:opencode", "--json"]);
    assert_eq!(code, 0, "{off}");
    assert!(!s.repo().join("opencode.json").exists(), "{off}");
    assert!(
        !s.git(&["show", &format!("{GRAPH}:.mdkg/config.json")])
            .contains("mcp:opencode")
    );
}

/// The professional setup writes only files the repository doesn't
/// track, each excluded in the common info/exclude, and no AGENTS.md
/// block unless this install enables it.
/// Claude-6 #13122, major: install's publish is based on the tip its
/// config was read at, so a config change published in between is refused
/// as stale, never overwritten. A `git` shim first on PATH publishes that
/// change (a prepared child commit) when install reads the config.
#[cfg(unix)]
#[test]
fn an_enable_racing_another_publish_is_refused_not_lost() {
    use std::os::unix::fs::PermissionsExt;
    let s = Scratch::new();
    s.ref_mode("main");
    let t0 = s.git(&["rev-parse", GRAPH]);
    // The concurrent change: T0's config listing mcp:opencode.
    let mut config: Value =
        serde_json::from_str(&s.git(&["show", &format!("{t0}:.mdkg/config.json")]))
            .expect("config");
    config["projections"] = serde_json::json!(["mcp:opencode"]);
    let file = s.0.join("config.json");
    fs::write(
        &file,
        serde_json::to_string_pretty(&config).expect("json") + "\n",
    )
    .expect("w");
    let blob = s.git(&["hash-object", "-w", &file.to_string_lossy()]);
    let index = s.0.join("index");
    let with_index = |args: &[&str]| {
        let out = s
            .isolated(Command::new("git").arg("-C").arg(s.repo()).args(args))
            .env("GIT_INDEX_FILE", &index)
            .output()
            .expect("git");
        assert!(out.status.success(), "{args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    };
    with_index(&["read-tree", &t0]);
    with_index(&[
        "update-index",
        "--cacheinfo",
        &format!("100644,{blob},.mdkg/config.json"),
    ]);
    let tree = with_index(&["write-tree"]);
    let t1 = s.git(&["commit-tree", &tree, "-p", &t0, "-m", "concurrent"]);
    let real = String::from_utf8_lossy(
        &Command::new("sh")
            .args(["-c", "command -v git"])
            .output()
            .expect("which")
            .stdout,
    )
    .trim()
    .to_owned();
    let shim = s.0.join("shim");
    fs::create_dir_all(&shim).expect("shim");
    let mark = s.0.join("raced");
    fs::write(
        shim.join("git"),
        format!(
            "#!/bin/sh\ncase \"$*\" in *show*:.mdkg/config.json*) if [ ! -e '{mark}' ]; then : > '{mark}'; '{real}' -C '{repo}' update-ref {GRAPH} {t1} {t0} || exit 99; fi;; esac\nexec '{real}' \"$@\"\n",
            mark = mark.display(),
            repo = s.repo().display(),
        ),
    )
    .expect("shim");
    fs::set_permissions(shim.join("git"), fs::Permissions::from_mode(0o755)).expect("chmod");
    let path = format!(
        "{}:{}",
        shim.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mdkg"));
    s.isolated(&mut cmd)
        .current_dir(s.repo())
        .env("PATH", path)
        .args(["install", "--enable", "mcp:claude", "--json"]);
    let out = cmd.output().expect("mdkg");
    let v: Value = serde_json::from_slice(&out.stdout).expect("json");
    assert!(mark.exists(), "the shim never raced: {v}");
    assert_ne!(out.status.code(), Some(0), "{v}");
    assert_eq!(codes(&v), ["graph.write_stale"], "{v}");
    assert_eq!(s.git(&["rev-parse", GRAPH]), t1, "the other publish stands");
}

/// A child of the graph commit `parent` whose config lists `listed`.
fn config_commit(s: &Scratch, parent: &str, listed: &[&str]) -> String {
    let mut config: Value =
        serde_json::from_str(&s.git(&["show", &format!("{parent}:.mdkg/config.json")]))
            .expect("config");
    config["projections"] = serde_json::json!(listed);
    let file = s.0.join(format!(
        "config-{}.json",
        listed.join("-").replace(':', "_")
    ));
    fs::write(
        &file,
        serde_json::to_string_pretty(&config).expect("json") + "\n",
    )
    .expect("w");
    let blob = s.git(&["hash-object", "-w", &file.to_string_lossy()]);
    let index = s.0.join(format!("index-{parent}"));
    let with_index = |args: &[&str]| {
        let out = s
            .isolated(Command::new("git").arg("-C").arg(s.repo()).args(args))
            .env("GIT_INDEX_FILE", &index)
            .output()
            .expect("git");
        assert!(out.status.success(), "{args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    };
    with_index(&["read-tree", parent]);
    with_index(&[
        "update-index",
        "--cacheinfo",
        &format!("100644,{blob},.mdkg/config.json"),
    ]);
    let tree = with_index(&["write-tree"]);
    s.git(&["commit-tree", &tree, "-p", parent, "-m", "listed"])
}

/// #13251 finding 5: a publish that leaves the config alone still
/// checks it after. Install reads a list naming `hooks:codex`, whose
/// canonical file the graph lacks; a `git` shim moves the graph to a
/// child listing nothing as install reads the config. The canonical
/// file's publish stands (it changes no other path), but install
/// refuses to sync the list the graph no longer holds.
#[cfg(unix)]
#[test]
fn a_projection_list_changed_during_a_publish_isnt_synced() {
    use std::os::unix::fs::PermissionsExt;
    let s = Scratch::new();
    s.ref_mode("main");
    let t0 = s.git(&["rev-parse", GRAPH]);
    let listed = config_commit(&s, &t0, &["hooks:codex"]);
    s.git(&["update-ref", GRAPH, &listed, &t0]);
    let emptied = config_commit(&s, &listed, &[]);
    let real = String::from_utf8_lossy(
        &Command::new("sh")
            .args(["-c", "command -v git"])
            .output()
            .expect("which")
            .stdout,
    )
    .trim()
    .to_owned();
    let shim = s.0.join("shim");
    fs::create_dir_all(&shim).expect("shim");
    let mark = s.0.join("raced");
    fs::write(
        shim.join("git"),
        format!(
            "#!/bin/sh\ncase \"$*\" in *show*:.mdkg/config.json*) if [ ! -e '{mark}' ]; then : > '{mark}'; '{real}' -C '{repo}' update-ref {GRAPH} {emptied} {listed} || exit 99; fi;; esac\nexec '{real}' \"$@\"\n",
            mark = mark.display(),
            repo = s.repo().display(),
        ),
    )
    .expect("shim");
    fs::set_permissions(shim.join("git"), fs::Permissions::from_mode(0o755)).expect("chmod");
    let path = format!(
        "{}:{}",
        shim.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mdkg"));
    s.isolated(&mut cmd)
        .current_dir(s.repo())
        .env("PATH", path)
        .args(["install", "--hooks", "--json"]);
    let out = cmd.output().expect("mdkg");
    let v: Value = serde_json::from_slice(&out.stdout).expect("json");
    assert!(mark.exists(), "the shim never raced: {v}");
    assert_ne!(out.status.code(), Some(0), "{v}");
    assert_eq!(codes(&v), ["graph.write_stale"], "{v}");
    assert!(!s.repo().join(".codex/hooks.json").exists(), "{v}");
}

/// #13251 finding 4: the professional setup's tracking check sees what a
/// name lands on: a link is left alone (it may lead to a tracked file),
/// and so is a tracked file spelled in another case where git compares
/// names without case.
#[cfg(unix)]
#[test]
fn professional_projections_leave_links_and_case_aliases_alone() {
    let s = Scratch::new();
    s.checkout_graph("main");
    fs::write(s.repo().join("company.json"), "{\"mcpServers\": {}}\n").expect("tracked");
    s.git(&["add", "company.json"]);
    s.git(&["commit", "-q", "-m", "company file"]);
    std::os::unix::fs::symlink("company.json", s.repo().join(".mcp.json")).expect("link");
    let mine = s.0.join("mine.git");
    s.git_in(&s.0, &["init", "-q", "--bare", &mine.to_string_lossy()]);
    s.git(&["remote", "add", "mine", &mine.to_string_lossy()]);
    let (code, v) = s.mdkg(&[
        "install",
        "--setup",
        "professional",
        "--graph-remote",
        "mine",
        "--json",
    ]);
    assert_eq!(code, 0, "{v}");
    let (code, v) = s.mdkg(&["install", "--enable", "mcp:claude", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(
        fs::read_to_string(s.repo().join("company.json")).expect("company"),
        "{\"mcpServers\": {}}\n",
        "{v}"
    );
    assert!(notes(&v).contains("left alone"), "{v}");
    if s.config("core.ignorecase").as_deref() == Some("true") {
        fs::remove_file(s.repo().join(".mcp.json")).expect("rm");
        s.git(&["mv", "company.json", ".MCP.JSON"]);
        s.git(&["commit", "-q", "-m", "renamed"]);
        let (code, v) = s.mdkg(&["install", "--json"]);
        assert_eq!(code, 0, "{v}");
        assert_eq!(
            fs::read_to_string(s.repo().join(".MCP.JSON")).expect("company"),
            "{\"mcpServers\": {}}\n",
            "{v}"
        );
    }
}

/// #13434 finding 1, the professional setup: Claude's per-user settings
/// linked to the tracked shared file are left alone, never written.
#[cfg(unix)]
#[test]
fn professional_hooks_never_write_the_shared_settings_through_a_link() {
    let s = Scratch::new();
    s.checkout_graph("main");
    let team = "{\"model\": \"opus\"}\n";
    fs::create_dir_all(s.repo().join(".claude")).expect("dir");
    fs::write(s.repo().join(".claude/settings.json"), team).expect("tracked");
    s.git(&["add", ".claude/settings.json"]);
    s.git(&["commit", "-q", "-m", "team settings"]);
    std::os::unix::fs::symlink(
        "settings.json",
        s.repo().join(".claude/settings.local.json"),
    )
    .expect("link");
    let mine = s.0.join("mine.git");
    s.git_in(&s.0, &["init", "-q", "--bare", &mine.to_string_lossy()]);
    s.git(&["remote", "add", "mine", &mine.to_string_lossy()]);
    let (code, v) = s.mdkg(&[
        "install",
        "--setup",
        "professional",
        "--graph-remote",
        "mine",
        "--json",
    ]);
    assert_eq!(code, 0, "{v}");
    let (code, v) = s.mdkg(&["install", "--hooks", "--enable", "hooks:claude", "--json"]);
    // Left out, not failed (#13893).
    assert_eq!(code, 0, "{v}");
    assert!(notes(&v).contains("left alone, since they're links"), "{v}");
    assert_eq!(
        fs::read_to_string(s.repo().join(".claude/settings.json")).expect("team"),
        team,
        "{v}"
    );
    // Disabling it leaves the link alone too, and doesn't fail after the
    // list change is published (#14098).
    let (code, v) = s.mdkg(&["install", "--disable", "hooks:claude", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert!(notes(&v).contains("left alone, since they're links"), "{v}");
    // The disable is published as asked (#14242).
    assert!(
        !s.git(&["show", &format!("{GRAPH}:.mdkg/config.json")])
            .contains("hooks:claude"),
        "{v}"
    );
    // And recorded here as an opt-out, though the link is left alone
    // (#14344).
    let marked = fs::read_dir(s.repo().join(".git/mdkg/install"))
        .expect("records")
        .flatten()
        .any(|e| {
            fs::read_to_string(e.path().join("hooks-off.json"))
                .is_ok_and(|t| t.contains("hooks:claude"))
        });
    assert!(marked, "{v}");
    assert!(
        fs::symlink_metadata(s.repo().join(".claude/settings.local.json"))
            .expect("link")
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        fs::read_to_string(s.repo().join(".claude/settings.json")).expect("team"),
        team,
        "{v}"
    );
}

/// Orchestrator #13845: a worktree removed without undo and made again at
/// the same path gets a fresh install record; the earlier checkout's is
/// set aside, never applied or deleted.
#[test]
fn a_recreated_worktree_sets_the_earlier_install_record_aside() {
    let s = Scratch::new();
    s.ref_mode("main");
    let wt = s.0.join("wt");
    s.git(&["worktree", "add", "-q", "-b", "wt", &wt.to_string_lossy()]);
    let (code, v) = s.mdkg_in(
        &wt,
        &["install", "--hooks", "--enable", "hooks:codex", "--json"],
    );
    assert_eq!(code, 0, "{v}");
    s.git(&["worktree", "remove", "--force", &wt.to_string_lossy()]);
    s.git(&["worktree", "add", "-q", &wt.to_string_lossy(), "wt"]);
    // A dry run doesn't check, as this checkout's, records a real run
    // would set aside (#14381 minors).
    let (code, v) = s.mdkg_in(&wt, &["install", "--dry-run", "--json"]);
    assert_eq!(code, 0, "{v}");
    // The rollback names the earlier record as such, not as broken, and
    // leaves it for the next install to set aside (#14153).
    let (code, v) = s.mdkg_in(&wt, &["install", "--checkout-mode", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert!(
        notes(&v).contains("an earlier checkout's at this path"),
        "{v}"
    );
    assert!(!notes(&v).contains("unusable"), "{v}");
    let (code, v) = s.mdkg_in(&wt, &["install", "--setup", "local", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert!(notes(&v).contains("set aside"), "{v}");
    let install = s.repo().join(".git/mdkg/install");
    let aside: Vec<_> = fs::read_dir(&install)
        .expect("records")
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains(".stale-"))
        .collect();
    assert_eq!(aside.len(), 1, "the earlier record is kept: {v}");
    assert!(aside[0].path().join("gitignore.json").is_file());
}

/// Orchestrator #13910: the rollback to checkout mode isn't blocked by an
/// unusable install record; it warns, names the folder and leaves it.
#[test]
fn a_checkout_mode_rollback_warns_of_an_unusable_record_and_goes_ahead() {
    let s = Scratch::new();
    s.ref_mode("main");
    let (code, v) = s.mdkg(&["install", "--hooks", "--enable", "hooks:codex", "--json"]);
    assert_eq!(code, 0, "{v}");
    let records: Vec<_> = fs::read_dir(s.repo().join(".git/mdkg/install"))
        .expect("records")
        .flatten()
        .map(|e| e.path().join("gitignore.json"))
        .filter(|p| p.is_file())
        .collect();
    assert_eq!(records.len(), 1, "{v}");
    fs::write(&records[0], "not json").expect("w");
    let (code, v) = s.mdkg(&["install", "--checkout-mode", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert!(notes(&v).contains("is unusable"), "{v}");
    assert_eq!(s.config("mdkg.graph.ref"), None, "{v}");
    assert_eq!(
        fs::read_to_string(&records[0]).expect("left in place"),
        "not json"
    );
}

/// #13910: a clone set up in ref mode from the start has no `.mdkg` in its
/// checkout; `install --undo` still puts back what install wrote.
#[test]
fn a_ref_mode_install_is_undone() {
    let s = Scratch::new();
    s.ref_mode("main");
    let (code, v) = s.mdkg(&["install", "--hooks", "--enable", "hooks:codex", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert!(s.repo().join(".codex/hooks.json").is_file(), "{v}");
    let (code, v) = s.mdkg(&["install", "--undo", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert!(!s.repo().join(".codex/hooks.json").exists(), "{v}");
    // The graph's list is left, and named (#14149).
    assert!(
        notes(&v).contains("projection list is left as it is") && notes(&v).contains("hooks:codex"),
        "{v}"
    );
}

/// #13686: install reads its undo records before it writes anything, so
/// one it can't use refuses the install whole, in either mode.
#[test]
fn an_unusable_undo_record_refuses_install_before_any_write() {
    for ref_mode in [false, true] {
        let s = Scratch::new();
        if ref_mode {
            s.ref_mode("main");
        } else {
            s.checkout_graph("main");
        }
        let (code, v) = s.mdkg(&["install", "--hooks", "--enable", "hooks:codex", "--json"]);
        assert_eq!(code, 0, "{v}");
        let records: Vec<_> = fs::read_dir(s.repo().join(".git/mdkg/install"))
            .expect("records")
            .flatten()
            .map(|e| e.path().join("gitignore.json"))
            .filter(|p| p.is_file())
            .collect();
        assert_eq!(records.len(), 1, "{v}");
        let mut record: Value =
            serde_json::from_str(&fs::read_to_string(&records[0]).expect("r")).expect("json");
        record["root"] = "/elsewhere".into();
        fs::write(&records[0], record.to_string()).expect("w");
        let (code, v) = s.mdkg(&["install", "--enable", "mcp:claude", "--json"]);
        assert_ne!(code, 0, "{v}");
        assert!(v.to_string().contains("record of another project"), "{v}");
        assert!(
            !s.repo().join(".mcp.json").exists(),
            "ref mode {ref_mode}: {v}"
        );
    }
}

/// #13895: a path under a linked folder is left alone, as a linked file
/// is: `.codex` leading to a tracked `company/` folder never gets its
/// tracked config.toml written.
#[cfg(unix)]
#[test]
fn professional_projections_leave_paths_under_a_linked_folder_alone() {
    let s = Scratch::new();
    s.checkout_graph("main");
    fs::create_dir_all(s.repo().join("company")).expect("dir");
    fs::write(s.repo().join("company/config.toml"), "model = \"x\"\n").expect("tracked");
    s.git(&["add", "company/config.toml"]);
    s.git(&["commit", "-q", "-m", "company config"]);
    std::os::unix::fs::symlink("company", s.repo().join(".codex")).expect("link");
    let mine = s.0.join("mine.git");
    s.git_in(&s.0, &["init", "-q", "--bare", &mine.to_string_lossy()]);
    s.git(&["remote", "add", "mine", &mine.to_string_lossy()]);
    let (code, v) = s.mdkg(&[
        "install",
        "--setup",
        "professional",
        "--graph-remote",
        "mine",
        "--json",
    ]);
    assert_eq!(code, 0, "{v}");
    let (code, v) = s.mdkg(&["install", "--enable", "mcp:codex", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(
        fs::read_to_string(s.repo().join("company/config.toml")).expect("company"),
        "model = \"x\"\n",
        "{v}"
    );
    assert!(notes(&v).contains("left alone"), "{v}");
}

#[test]
fn professional_projections_touch_no_tracked_file() {
    let s = Scratch::new();
    s.checkout_graph("main");
    fs::write(s.repo().join(".mcp.json"), "{\"mcpServers\": {}}\n").expect("tracked");
    fs::write(s.repo().join("AGENTS.md"), "# Company\n").expect("tracked");
    s.git(&["add", ".mcp.json", "AGENTS.md"]);
    s.git(&["commit", "-q", "-m", "company files"]);
    let mine = s.0.join("mine.git");
    s.git_in(&s.0, &["init", "-q", "--bare", &mine.to_string_lossy()]);
    s.git(&["remote", "add", "mine", &mine.to_string_lossy()]);
    let (code, v) = s.mdkg(&[
        "install",
        "--setup",
        "professional",
        "--graph-remote",
        "mine",
        "--json",
    ]);
    assert_eq!(code, 0, "{v}");
    let (code, v) = s.mdkg(&[
        "install",
        "--enable",
        "mcp:claude,mcp:codex,agents-md",
        "--json",
    ]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(
        fs::read_to_string(s.repo().join(".mcp.json")).expect("mcp"),
        "{\"mcpServers\": {}}\n",
        "{v}"
    );
    assert_eq!(
        fs::read_to_string(s.repo().join("AGENTS.md")).expect("agents"),
        "# Company\n"
    );
    assert!(notes(&v).contains("left alone"), "{v}");
    assert!(s.repo().join(".codex/config.toml").is_file(), "{v}");
    let exclude = fs::read_to_string(s.repo().join(".git/info/exclude")).expect("exclude");
    assert!(
        exclude.lines().any(|l| l == "/.codex/config.toml"),
        "{exclude}"
    );
    assert_eq!(s.git(&["status", "--porcelain"]), "", "nothing to commit");
    // Install owns the lines it added there (#13251): undo takes them out
    // with the files, and leaves the setup's own.
    let (code, v) = s.mdkg(&["install", "--undo", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert!(!s.repo().join(".codex/config.toml").exists(), "{v}");
    let exclude = fs::read_to_string(s.repo().join(".git/info/exclude")).expect("exclude");
    assert!(
        !exclude.lines().any(|l| l == "/.codex/config.toml"),
        "{exclude}"
    );
    assert!(exclude.lines().any(|l| l == "/.mdkg-edit/"), "{exclude}");
}

/// #13251: an exclusion added for a write that then fails is taken back,
/// so a file install never wrote isn't hidden.
#[cfg(unix)]
#[test]
fn a_failed_professional_write_leaves_no_exclusion() {
    use std::os::unix::fs::PermissionsExt as _;
    let s = Scratch::new();
    s.checkout_graph("main");
    let mine = s.0.join("mine.git");
    s.git_in(&s.0, &["init", "-q", "--bare", &mine.to_string_lossy()]);
    s.git(&["remote", "add", "mine", &mine.to_string_lossy()]);
    let (code, v) = s.mdkg(&[
        "install",
        "--setup",
        "professional",
        "--graph-remote",
        "mine",
        "--json",
    ]);
    assert_eq!(code, 0, "{v}");
    let codex = s.repo().join(".codex");
    fs::create_dir_all(&codex).expect("dir");
    fs::write(codex.join("config.toml"), "model = \"x\"\n").expect("user's");
    fs::set_permissions(&codex, fs::Permissions::from_mode(0o555)).expect("chmod");
    let (code, v) = s.mdkg(&["install", "--enable", "mcp:codex", "--json"]);
    fs::set_permissions(&codex, fs::Permissions::from_mode(0o755)).expect("chmod");
    assert_ne!(code, 0, "{v}");
    assert_eq!(
        fs::read_to_string(codex.join("config.toml")).expect("kept"),
        "model = \"x\"\n"
    );
    let exclude = fs::read_to_string(s.repo().join(".git/info/exclude")).expect("exclude");
    assert!(
        !exclude.lines().any(|l| l == "/.codex/config.toml"),
        "{exclude}"
    );
}

/// C13, R49 (G24): a hook projection the config lists isn't installed
/// without `--hooks`, and install says how to opt in. With it, the hook
/// files are written and ignored: `.gitignore` in checkout mode and the
/// local setup, the common info/exclude in the professional one.
#[test]
fn hooks_install_only_when_asked_and_their_files_are_ignored() {
    let s = Scratch::new();
    s.checkout_graph("main");
    // Someone else enabled them in the committed config.
    let path = s.repo().join(".mdkg/config.json");
    let mut config: Value =
        serde_json::from_str(&fs::read_to_string(&path).expect("config")).expect("json");
    config["projections"] = serde_json::json!(["hooks:git", "hooks:codex"]);
    fs::write(&path, config.to_string()).expect("config");
    s.git(&["commit", "-q", "-am", "hooks listed"]);
    let (code, v) = s.mdkg(&["install", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert!(notes(&v).contains("install --hooks"), "{v}");
    assert!(!s.repo().join(".codex/hooks.json").exists(), "{v}");
    assert_eq!(s.config("core.hooksPath"), None, "no git hook: {v}");
    let (code, v) = s.mdkg(&["install", "--hooks", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert!(s.repo().join(".codex/hooks.json").is_file(), "{v}");
    assert!(s.config("core.hooksPath").is_some(), "{v}");
    let ignore = fs::read_to_string(s.repo().join(".gitignore")).expect("gitignore");
    assert!(
        ignore.lines().any(|l| l == "/.codex/hooks.json"),
        "{ignore}"
    );
    // Installed here, so a plain install keeps it in sync.
    let (code, v) = s.mdkg(&["install", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert!(!notes(&v).contains("install --hooks"), "{v}");
}

#[test]
fn professional_hook_files_go_in_info_exclude() {
    let s = Scratch::new();
    s.checkout_graph("main");
    let mine = s.0.join("mine.git");
    s.git_in(&s.0, &["init", "-q", "--bare", &mine.to_string_lossy()]);
    s.git(&["remote", "add", "mine", &mine.to_string_lossy()]);
    let (code, v) = s.mdkg(&[
        "install",
        "--setup",
        "professional",
        "--graph-remote",
        "mine",
        "--json",
    ]);
    assert_eq!(code, 0, "{v}");
    let (code, v) = s.mdkg(&["install", "--enable", "hooks:opencode", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert!(
        s.repo().join(".opencode/plugins/mdkg/server.ts").is_file(),
        "{v}"
    );
    let exclude = fs::read_to_string(s.repo().join(".git/info/exclude")).expect("exclude");
    assert!(
        exclude
            .lines()
            .any(|l| l == "/.opencode/plugins/mdkg/server.ts"),
        "{exclude}"
    );
    assert!(!s.repo().join(".gitignore").exists(), "no tracked file");
}

/// Orchestrator #12831 Q2, #14076 major 5: the move to the graph ref with
/// changesets still open is refused, not drained, in 0.7.1; once they're
/// settled or abandoned it goes ahead.
#[test]
fn the_move_to_the_graph_ref_waits_for_open_changesets() {
    let s = Scratch::new();
    s.checkout_graph("main");
    // Work in the repository, so `done` on a branch opens a changeset.
    let config = s.repo().join(".mdkg/config.json");
    let mut c: Value =
        serde_json::from_str(&fs::read_to_string(&config).expect("config")).expect("json");
    c["work"] = serde_json::json!({"location": "git"});
    fs::write(&config, c.to_string()).expect("w");
    s.git(&["commit", "-q", "-am", "work in git"]);
    let (code, new) = s.mdkg(&["new", "task", "Open work", "--json"]);
    assert_eq!(code, 0, "{new}");
    let id = new["data"]["node"]["id"].as_str().expect("id").to_owned();
    if !s.git(&["status", "--porcelain", ".mdkg"]).is_empty() {
        s.git(&["add", ".mdkg"]);
        s.git(&["commit", "-q", "-m", "task"]);
    }
    let wt = s.0.join("wt");
    s.git(&[
        "worktree",
        "add",
        "-q",
        "-b",
        "b",
        wt.to_str().expect("utf-8"),
        "main",
    ]);
    let as_agent = ["--as", "agent1"];
    let (code, claim) = s.mdkg_in(&wt, &[&["claim", &id, "--json"][..], &as_agent].concat());
    assert_eq!(code, 0, "{claim}");
    let (code, done) = s.mdkg_in(&wt, &[&["done", &id, "--json"][..], &as_agent].concat());
    assert_eq!(code, 0, "{done}");
    // A dry run reads no coordinator, so writes none (#14205).
    let store = s.repo().join(".git/mdkg/coord.sqlite");
    let before = fs::read(&store).ok();
    assert!(before.is_some(), "the store is there");
    let (code, v) = s.mdkg(&["install", "--setup", "local", "--dry-run", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert!(notes(&v).contains("coordinator isn't read"), "{v}");
    assert_eq!(fs::read(&store).ok(), before, "store untouched");
    let (code, v) = s.mdkg(&["install", "--setup", "local", "--json"]);
    assert_ne!(code, 0, "{v}");
    assert_eq!(v["diagnostics"][0]["code"], "graph.open_changesets", "{v}");
    assert_eq!(s.config("mdkg.graph.ref"), None, "still checkout mode");
    // From a checkout of a commit before the graph too: the store is the
    // clone's (#14205).
    let old = s.0.join("old");
    let first = s.git(&["rev-list", "--max-parents=0", "HEAD"]);
    s.git(&[
        "worktree",
        "add",
        "-q",
        "--detach",
        old.to_str().expect("utf-8"),
        &first,
    ]);
    let (code, v) = s.mdkg_in(&old, &["install", "--setup", "local", "--json"]);
    assert_ne!(code, 0, "{v}");
    assert!(v.to_string().contains("graph.open_changesets"), "{v}");
    assert_eq!(s.config("mdkg.graph.ref"), None, "still checkout mode");
    assert!(
        !s.git(&["for-each-ref", GRAPH]).contains(GRAPH),
        "no graph ref"
    );
    let (code, abandoned) = s.mdkg(&["abandon", "--branch", "b", "--json"]);
    assert_eq!(code, 0, "{abandoned}");
    let (code, v) = s.mdkg(&["install", "--setup", "local", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(s.config("mdkg.graph.ref").as_deref(), Some(GRAPH));
}

/// Orchestrator #14098: a hook file is ignored before it's written, so a
/// crash between the two leaves an ignored path, never a file an agent's
/// `git add -A` would take. The write failing after the exclusion stands
/// in for the crash.
#[test]
fn a_hook_file_is_ignored_before_it_is_written() {
    let s = Scratch::new();
    s.checkout_graph("main");
    let path = s.repo().join(".mdkg/config.json");
    let mut config: Value =
        serde_json::from_str(&fs::read_to_string(&path).expect("config")).expect("json");
    config["projections"] = serde_json::json!(["hooks:codex"]);
    fs::write(&path, config.to_string()).expect("config");
    s.git(&["commit", "-q", "-am", "hooks listed"]);
    // The hook file can't be written: `.codex` is read-only.
    let codex = s.repo().join(".codex");
    fs::create_dir(&codex).expect("dir");
    let mode = |m: &str| {
        Command::new("chmod")
            .arg(m)
            .arg(&codex)
            .status()
            .expect("chmod");
    };
    mode("555");
    let (code, v) = s.mdkg(&["install", "--hooks", "--json"]);
    assert_ne!(code, 0, "{v}");
    let ignore =
        fs::read_to_string(s.repo().join(".gitignore")).unwrap_or_else(|e| panic!("{e}: {v}"));
    assert!(
        ignore.lines().any(|l| l == "/.codex/hooks.json"),
        "ignored first: {ignore}"
    );
    // Out of the way, install finishes, and undo takes the line out.
    mode("755");
    let (code, v) = s.mdkg(&["install", "--hooks", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert!(s.repo().join(".codex/hooks.json").is_file(), "{v}");
    let (code, v) = s.mdkg(&["install", "--undo", "--json"]);
    assert_eq!(code, 0, "{v}");
    let ignore = fs::read_to_string(s.repo().join(".gitignore")).unwrap_or_default();
    assert!(
        !ignore.lines().any(|l| l == "/.codex/hooks.json"),
        "{ignore}"
    );
}

/// #14098 major 6: hooks merged into a tracked per-user settings file
/// would reach teammates with the next commit; install says so.
#[test]
fn hooks_in_a_tracked_per_user_file_are_named() {
    let s = Scratch::new();
    s.checkout_graph("main");
    fs::create_dir_all(s.repo().join(".claude")).expect("dir");
    fs::write(s.repo().join(".claude/settings.local.json"), "{}\n").expect("w");
    s.git(&["add", ".claude/settings.local.json"]);
    s.git(&["commit", "-q", "-m", "tracked local settings"]);
    let (code, v) = s.mdkg(&["install", "--hooks", "--enable", "hooks:claude", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert!(
        notes(&v).contains(".claude/settings.local.json is tracked"),
        "{v}"
    );
}

/// #14119: the professional setup leaves a tracked file alone under any
/// name the filesystem takes for it, also where git's case folding
/// differs (APFS takes `ſ` for `s`; git doesn't).
#[test]
fn professional_projections_leave_a_unicode_alias_of_a_tracked_file_alone() {
    let s = Scratch::new();
    s.checkout_graph("main");
    let probe = s.repo().join("probe-\u{17f}");
    fs::write(&probe, "").expect("probe");
    let folds = s.repo().join("probe-s").exists();
    fs::remove_file(&probe).expect("rm");
    if !folds {
        return;
    }
    let skill = s.repo().join(".mdkg/skills/extra");
    fs::create_dir_all(&skill).expect("skill");
    let workflow = fs::read_dir(s.repo().join(".mdkg/skills"))
        .expect("skills")
        .flatten()
        .map(|e| e.path().join("SKILL.md"))
        .find(|p| p.is_file())
        .expect("a skill");
    let text = fs::read_to_string(&workflow).expect("skill text");
    fs::write(
        skill.join("SKILL.md"),
        text.replacen("name: ", "name: extra-", 1),
    )
    .expect("w");
    fs::write(skill.join("te\u{17f}t.md"), "mdkg's\n").expect("w");
    let team = s.repo().join(".claude/skills/extra");
    fs::create_dir_all(&team).expect("team");
    fs::write(team.join("test.md"), "team\n").expect("w");
    s.git(&["add", ".mdkg", ".claude"]);
    s.git(&["commit", "-q", "-m", "skill and team file"]);
    let mine = s.0.join("mine.git");
    s.git_in(&s.0, &["init", "-q", "--bare", &mine.to_string_lossy()]);
    s.git(&["remote", "add", "mine", &mine.to_string_lossy()]);
    let (code, v) = s.mdkg(&[
        "install",
        "--setup",
        "professional",
        "--graph-remote",
        "mine",
        "--json",
    ]);
    assert_eq!(code, 0, "{v}");
    let (code, v) = s.mdkg(&["install", "--enable", "skills:.claude/skills", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(
        fs::read_to_string(team.join("test.md")).expect("team"),
        "team\n",
        "{v}"
    );
}

/// Orchestrator #14187, the move keeps the graph: uncommitted graph
/// changes refuse it; a checkout whose graph is in the graph ref's
/// first-parent history (only behind it) adopts the ref; one with graph
/// changes the ref lacks is refused. No main is read.
#[test]
fn the_move_keeps_the_checkouts_graph() {
    let s = Scratch::new();
    s.checkout_graph("trunk");
    fs::write(s.repo().join(".mdkg/draft.md"), "not committed\n").expect("w");
    let (code, v) = s.mdkg(&["install", "--setup", "local", "--json"]);
    assert_ne!(code, 0, "{v}");
    assert_eq!(v["diagnostics"][0]["code"], "graph.uncommitted", "{v}");
    assert_eq!(s.config("mdkg.graph.ref"), None);
    fs::remove_file(s.repo().join(".mdkg/draft.md")).expect("rm");
    let (code, v) = s.mdkg(&["install", "--setup", "local", "--json"]);
    assert_eq!(code, 0, "{v}");
    // The graph ref moves on; back in checkout mode, this checkout is
    // only behind it.
    let (code, v) = s.mdkg(&["install", "--enable", "mcp:claude", "--json"]);
    assert_eq!(code, 0, "{v}");
    let tip = s.git(&["rev-parse", GRAPH]);
    let (code, v) = s.mdkg(&["install", "--checkout-mode", "--json"]);
    assert_eq!(code, 0, "{v}");
    let (code, v) = s.mdkg(&["install", "--setup", "local", "--json"]);
    assert_eq!(code, 0, "behind: {v}");
    assert_eq!(s.git(&["rev-parse", GRAPH]), tip, "adopted as it is");
    // Graph changes of its own: refused.
    let (code, v) = s.mdkg(&["install", "--checkout-mode", "--json"]);
    assert_eq!(code, 0, "{v}");
    fs::write(s.repo().join(".mdkg/mine.md"), "only here\n").expect("w");
    s.git(&["add", ".mdkg/mine.md"]);
    s.git(&["commit", "-q", "-m", "graph change of its own"]);
    let (code, v) = s.mdkg(&["install", "--setup", "local", "--json"]);
    assert_ne!(code, 0, "{v}");
    assert_eq!(v["diagnostics"][0]["code"], "graph.diverged", "{v}");
    assert_eq!(s.config("mdkg.graph.ref"), None);
}

/// #14223: a professional hook file a `.gitignore` negation re-includes
/// isn't reported as ignored: git's own answer decides.
#[test]
fn a_professional_hook_file_a_negation_re_includes_is_named() {
    let s = Scratch::new();
    s.checkout_graph("main");
    fs::write(
        s.repo().join(".gitignore"),
        "!/.opencode/plugins/mdkg/server.ts\n",
    )
    .expect("w");
    s.git(&["add", ".gitignore"]);
    s.git(&["commit", "-q", "-m", "negation"]);
    let mine = s.0.join("mine.git");
    s.git_in(&s.0, &["init", "-q", "--bare", &mine.to_string_lossy()]);
    s.git(&["remote", "add", "mine", &mine.to_string_lossy()]);
    let (code, v) = s.mdkg(&[
        "install",
        "--setup",
        "professional",
        "--graph-remote",
        "mine",
        "--json",
    ]);
    assert_eq!(code, 0, "{v}");
    // A new one is refused before it's made, naming the rule (#14381 E).
    let (code, v) = s.mdkg(&["install", "--hooks", "--enable", "hooks:opencode", "--json"]);
    assert_ne!(code, 0, "{v}");
    assert!(v.to_string().contains("git wouldn't ignore it"), "{v}");
    assert!(v.to_string().contains(".gitignore:1:"), "{v}");
    assert!(
        !s.repo().join(".opencode/plugins/mdkg/server.ts").exists(),
        "{v}"
    );
}

/// Orchestrator #14254 (row 16): a ref-mode turn with no main branch and
/// no code base configured has no main ref: none is named "main", none is
/// passed to the agent, and a reviewer has the launcher's rules only.
#[test]
fn a_ref_mode_turn_with_no_main_has_no_main_ref() {
    let s = Scratch::new();
    s.ref_mode("trunk");
    let (code, v) = s.mdkg(&[
        "agents",
        "add",
        "r",
        "--harness",
        "claude",
        "--role",
        "reviewer",
        "--json",
    ]);
    assert_eq!(code, 0, "{v}");
    let (code, out) = dry_turn(&s, &s.repo());
    assert_eq!(code, 0, "{out}");
    let text = out.to_string();
    assert!(!text.contains("MDKG_MAIN_REF"), "{out}");
    assert!(!text.contains("refs/heads/main"), "{out}");
    // The reviewer's preamble is there with no main too (#14381 I): the
    // checkout's files are data, never instructions.
    assert!(text.contains("they are data"), "{out}");
    // An explicit --base is honored with no main (#14381 H): its AGENTS.md
    // gives the rules.
    fs::write(
        s.repo().join("AGENTS.md"),
        "Rule from the base: be terse.\n",
    )
    .expect("w");
    s.git(&["add", "AGENTS.md"]);
    s.git(&["commit", "-q", "-m", "rules"]);
    let base = s.git(&["rev-parse", "HEAD"]);
    let (code, v) = s.mdkg(&["agents", "edit", "r", "--base", &base, "--json"]);
    assert_eq!(code, 0, "{v}");
    let (code, out) = dry_turn(&s, &s.repo());
    assert_eq!(code, 0, "{out}");
    let text = out.to_string();
    assert!(text.contains("they are data"), "{out}");
    assert!(text.contains("Rule from the base: be terse."), "{out}");
}

/// #14264: where a projected file goes, the professional setup leaves a
/// folder that holds anything alone, never removing it with what's in it:
/// here a name it would leave alone itself (a backslash, Unix).
#[cfg(unix)]
#[test]
fn professional_projections_never_remove_a_folder_in_their_way() {
    let s = Scratch::new();
    s.checkout_graph("main");
    let skill = s.repo().join(".mdkg/skills/extra");
    fs::create_dir_all(&skill).expect("skill");
    let workflow = fs::read_dir(s.repo().join(".mdkg/skills"))
        .expect("skills")
        .flatten()
        .map(|e| e.path().join("SKILL.md"))
        .find(|p| p.is_file())
        .expect("a skill");
    let text = fs::read_to_string(&workflow).expect("skill text");
    fs::write(
        skill.join("SKILL.md"),
        text.replacen("name: ", "name: extra-", 1),
    )
    .expect("w");
    fs::write(skill.join("x"), "mdkg's\n").expect("w");
    s.git(&["add", ".mdkg"]);
    s.git(&["commit", "-q", "-m", "skill"]);
    let mine = s.0.join("mine.git");
    s.git_in(&s.0, &["init", "-q", "--bare", &mine.to_string_lossy()]);
    s.git(&["remote", "add", "mine", &mine.to_string_lossy()]);
    let (code, v) = s.mdkg(&[
        "install",
        "--setup",
        "professional",
        "--graph-remote",
        "mine",
        "--json",
    ]);
    assert_eq!(code, 0, "{v}");
    let folder = s.repo().join(".claude/skills/extra/x");
    fs::create_dir_all(&folder).expect("folder");
    fs::write(folder.join("a\\b.md"), "the user's\n").expect("w");
    let (code, v) = s.mdkg(&["install", "--enable", "skills:.claude/skills", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(
        fs::read_to_string(folder.join("a\\b.md")).unwrap_or_else(|e| panic!("{e}: {v}")),
        "the user's\n",
        "{v}"
    );
}

/// #14282: checking install's records writes nothing; a lost copy whose
/// file holds its bytes again is stored anew only by an install that
/// writes: not a dry run, not the rollback to checkout mode.
#[test]
fn a_lost_copy_is_repaired_only_by_an_install_that_writes() {
    let s = Scratch::new();
    s.ref_mode("main");
    fs::write(s.repo().join("AGENTS.md"), "# Ours\n").expect("w");
    s.git(&["add", "AGENTS.md"]);
    s.git(&["commit", "-q", "-m", "agents"]);
    let (code, v) = s.mdkg(&["install", "--enable", "agents-md", "--json"]);
    assert_eq!(code, 0, "{v}");
    let objects = s.repo().join(".git/mdkg/install");
    let copy = fs::read_dir(&objects)
        .expect("records")
        .flatten()
        .map(|e| e.path().join("objects"))
        .find(|p| p.is_dir())
        .and_then(|d| fs::read_dir(d).ok()?.flatten().next().map(|e| e.path()))
        .expect("a copy");
    fs::remove_file(&copy).expect("lost");
    fs::write(s.repo().join("AGENTS.md"), "# Ours\n").expect("put back");
    let (code, v) = s.mdkg(&["install", "--dry-run", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert!(!copy.exists(), "a dry run writes nothing: {v}");
    let (code, v) = s.mdkg(&["install", "--checkout-mode", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert!(!copy.exists(), "nor the rollback: {v}");
    let (code, v) = s.mdkg(&["install", "--setup", "local", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert!(copy.is_file(), "repaired: {v}");
}

fn copy_tree(from: &Path, to: &Path) {
    for entry in fs::read_dir(from).expect("read") {
        let entry = entry.expect("entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("type").is_dir() {
            fs::create_dir_all(&target).expect("dir");
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).expect("copy");
        }
    }
}

/// #14296: the move reads HEAD's own graph paths (a commit id, not the
/// name HEAD), so an uncommitted change in a nested workspace's graph
/// refuses it too.
#[test]
fn an_uncommitted_nested_graph_change_refuses_the_move() {
    let s = Scratch::new();
    s.checkout_graph("main");
    let conformance = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance");
    let w = s.repo().join("w");
    fs::create_dir_all(&w).expect("w");
    copy_tree(&conformance.join("base"), &w);
    copy_tree(&conformance.join("cases/g01-valid-basic"), &w);
    fs::remove_file(w.join("commands")).ok();
    fs::remove_file(w.join("README.md")).ok();
    let path = s.repo().join(".mdkg/config.json");
    let mut config: Value =
        serde_json::from_str(&fs::read_to_string(&path).expect("config")).expect("json");
    config["workspaces"] = serde_json::json!({
        "root": {"path": ".", "enabled": true, "mdkg_dir": ".mdkg", "visibility": "private"},
        "w": {"path": "w", "enabled": true, "mdkg_dir": ".mdkg", "visibility": "private"}
    });
    fs::write(&path, config.to_string()).expect("w");
    s.git(&["add", "."]);
    s.git(&["commit", "-q", "-m", "workspace"]);
    fs::write(w.join(".mdkg/draft.md"), "not committed\n").expect("w");
    let (code, v) = s.mdkg(&["install", "--setup", "local", "--json"]);
    assert_ne!(code, 0, "{v}");
    assert_eq!(v["diagnostics"][0]["code"], "graph.uncommitted", "{v}");
    assert!(v.to_string().contains("w/.mdkg"), "{v}");
}

/// #14296: a clone holding only published design files commits no graph
/// config: it has no graph of its own, and adopts the graph ref.
#[test]
fn a_design_only_checkout_adopts_the_graph_ref() {
    let s = Scratch::new();
    s.ref_mode("main");
    let tip = s.git(&["rev-parse", GRAPH]);
    let clone = s.0.join("clone");
    s.git_in(
        &s.0,
        &[
            "clone",
            "-q",
            &s.repo().to_string_lossy(),
            &clone.to_string_lossy(),
        ],
    );
    s.git_in(&clone, &["config", "user.name", "Graph Test"]);
    s.git_in(&clone, &["config", "user.email", "graph@example.invalid"]);
    fs::create_dir_all(clone.join(".mdkg/design")).expect("dir");
    fs::write(clone.join(".mdkg/design/edd-x.md"), "published design\n").expect("w");
    s.git_in(&clone, &["add", ".mdkg/design"]);
    s.git_in(&clone, &["commit", "-q", "-m", "design"]);
    let (code, v) = s.mdkg_in(&clone, &["install", "--setup", "personal", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(s.git_in(&clone, &["rev-parse", GRAPH]), tip);
}

/// #14319 D1 (a): an existing untracked shared hook file the sync merges
/// into is ignored before the merge too, so a failed or interrupted write
/// never leaves mdkg's hook lines in a file `git add -A` takes.
#[cfg(unix)]
#[test]
fn an_existing_shared_hook_file_is_ignored_before_the_merge() {
    let s = Scratch::new();
    s.checkout_graph("main");
    let path = s.repo().join(".mdkg/config.json");
    let mut config: Value =
        serde_json::from_str(&fs::read_to_string(&path).expect("config")).expect("json");
    config["projections"] = serde_json::json!(["hooks:codex"]);
    fs::write(&path, config.to_string()).expect("config");
    s.git(&["commit", "-q", "-am", "hooks listed"]);
    let codex = s.repo().join(".codex");
    fs::create_dir(&codex).expect("dir");
    fs::write(codex.join("hooks.json"), "{\"hooks\": {}}\n").expect("the user's");
    let mode = |m: &str| {
        Command::new("chmod")
            .arg(m)
            .arg(&codex)
            .status()
            .expect("chmod");
    };
    mode("555");
    let (code, v) = s.mdkg(&["install", "--hooks", "--json"]);
    mode("755");
    assert_ne!(code, 0, "{v}");
    let ignore =
        fs::read_to_string(s.repo().join(".gitignore")).unwrap_or_else(|e| panic!("{e}: {v}"));
    assert!(
        ignore.lines().any(|l| l == "/.codex/hooks.json"),
        "ignored first: {ignore}"
    );
}

/// #14328, #14381 E: in checkout mode too, git's own answer decides. A
/// nested .gitignore's negation re-including an existing hook file is
/// warned about, never reported as ignored.
#[test]
fn a_nested_negation_over_an_existing_hook_file_is_named() {
    let s = Scratch::new();
    s.checkout_graph("main");
    let path = s.repo().join(".mdkg/config.json");
    let mut config: Value =
        serde_json::from_str(&fs::read_to_string(&path).expect("config")).expect("json");
    config["projections"] = serde_json::json!(["hooks:codex"]);
    fs::write(&path, config.to_string()).expect("config");
    fs::create_dir_all(s.repo().join(".codex")).expect("dir");
    fs::write(s.repo().join(".codex/.gitignore"), "!/hooks.json\n").expect("w");
    s.git(&["add", ".mdkg", ".codex/.gitignore"]);
    s.git(&["commit", "-q", "-m", "hooks listed, negation"]);
    fs::write(s.repo().join(".codex/hooks.json"), "{\"hooks\": {}}\n").expect("the user's");
    let (code, v) = s.mdkg(&["install", "--hooks", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert!(notes(&v).contains("git doesn't ignore it"), "{v}");
    assert!(!notes(&v).contains("hooks.json is ignored"), "{v}");
}

/// Orchestrator #14381 D: every refusal comes before the shared projection
/// list or the graph ref changes. An --enable the professional setup must
/// refuse (a negation re-includes the file it would write) leaves the
/// graph ref as it was.
#[test]
fn a_refused_enable_publishes_nothing() {
    let s = Scratch::new();
    s.checkout_graph("main");
    fs::write(s.repo().join(".gitignore"), "!/.mcp.json\n").expect("w");
    s.git(&["add", ".gitignore"]);
    s.git(&["commit", "-q", "-m", "negation"]);
    let mine = s.0.join("mine.git");
    s.git_in(&s.0, &["init", "-q", "--bare", &mine.to_string_lossy()]);
    s.git(&["remote", "add", "mine", &mine.to_string_lossy()]);
    let (code, v) = s.mdkg(&[
        "install",
        "--setup",
        "professional",
        "--graph-remote",
        "mine",
        "--json",
    ]);
    assert_eq!(code, 0, "{v}");
    let tip = s.git(&["rev-parse", GRAPH]);
    let (code, v) = s.mdkg(&["install", "--enable", "mcp:claude", "--json"]);
    assert_ne!(code, 0, "{v}");
    assert!(v.to_string().contains("git wouldn't ignore it"), "{v}");
    assert!(v.to_string().contains(".gitignore:1:"), "{v}");
    assert_eq!(s.git(&["rev-parse", GRAPH]), tip, "nothing published: {v}");
    assert!(
        !s.git(&["show", &format!("{GRAPH}:.mdkg/config.json")])
            .contains("mcp:claude"),
        "{v}"
    );
    assert!(!s.repo().join(".mcp.json").exists(), "{v}");
}

/// #14500: outside professional too, a sync that would refuse refuses
/// before the publish, so the graph ref and its projection list stay as
/// they were.
#[test]
fn a_refused_sync_publishes_nothing_in_local_and_personal() {
    for setup in ["local", "personal"] {
        let s = Scratch::new();
        s.checkout_graph("main");
        let origin = s.0.join("origin.git");
        s.git_in(&s.0, &["init", "-q", "--bare", &origin.to_string_lossy()]);
        s.git(&["remote", "add", "origin", &origin.to_string_lossy()]);
        let (code, v) = s.mdkg(&["install", "--setup", setup, "--json"]);
        assert_eq!(code, 0, "{setup}: {v}");
        fs::create_dir_all(s.repo().join(".codex")).expect("dir");
        fs::write(s.repo().join(".codex/config.toml"), "model = [\n").expect("bad toml");
        let tip = s.git(&["rev-parse", GRAPH]);
        let (code, v) = s.mdkg(&["install", "--enable", "mcp:codex", "--json"]);
        assert_ne!(code, 0, "{setup}: {v}");
        assert!(v.to_string().contains("valid TOML"), "{setup}: {v}");
        assert_eq!(
            s.git(&["rev-parse", GRAPH]),
            tip,
            "{setup}: nothing published: {v}"
        );
        assert!(
            !s.git(&["show", &format!("{GRAPH}:.mdkg/config.json")])
                .contains("mcp:codex"),
            "{setup}: {v}"
        );
    }
}

/// #14588: a first hooks enable, whose canonical file the graph doesn't
/// have yet, checks the hook target against the shipped text before the
/// publish: a target it can't merge publishes nothing.
#[test]
fn a_first_hooks_enable_with_an_unmergeable_target_publishes_nothing() {
    for setup in ["local", "personal"] {
        let s = Scratch::new();
        s.checkout_graph("main");
        let origin = s.0.join("origin.git");
        s.git_in(&s.0, &["init", "-q", "--bare", &origin.to_string_lossy()]);
        s.git(&["remote", "add", "origin", &origin.to_string_lossy()]);
        let (code, v) = s.mdkg(&["install", "--setup", setup, "--json"]);
        assert_eq!(code, 0, "{setup}: {v}");
        fs::create_dir_all(s.repo().join(".codex")).expect("dir");
        fs::write(s.repo().join(".codex/hooks.json"), "{\"hooks\": [\n").expect("bad json");
        let tip = s.git(&["rev-parse", GRAPH]);
        assert!(
            !s.git(&["ls-tree", "-r", "--name-only", GRAPH])
                .contains(".mdkg/hooks/codex/hooks.json"),
            "{setup}: no canonical file on the graph yet"
        );
        let (code, v) = s.mdkg(&["install", "--hooks", "--enable", "hooks:codex", "--json"]);
        assert_ne!(code, 0, "{setup}: {v}");
        assert!(
            v.to_string().contains("install.unmergeable")
                && v.to_string().contains(".codex/hooks.json"),
            "{setup}: {v}"
        );
        assert_eq!(
            s.git(&["rev-parse", GRAPH]),
            tip,
            "{setup}: nothing published: {v}"
        );
        assert!(
            !s.git(&["show", &format!("{GRAPH}:.mdkg/config.json")])
                .contains("hooks:codex"),
            "{setup}: {v}"
        );
    }
}

/// Setup must validate requested hooks before creating or adopting a ref.
fn checkout_ignore_refusal_preserves_graph_setup(dry: bool) {
    for (harness, target) in [
        ("claude", ".claude/settings.local.json"),
        ("codex", ".codex/hooks.json"),
        ("opencode", ".opencode/plugins/mdkg/server.ts"),
    ] {
        let s = Scratch::new();
        s.checkout_graph("main");
        fs::write(s.repo().join(".gitignore"), format!("!/{target}\n")).expect("negation");
        s.git(&["add", ".gitignore"]);
        s.git(&["commit", "-qm", "negation"]);
        let refs = s.git(&["for-each-ref", "--format=%(refname) %(objectname)"]);
        let worktrees = s.git(&["worktree", "list", "--porcelain"]);
        let config = fs::read(s.repo().join(".git/config")).expect("git config");
        let graph_config = fs::read(s.repo().join(".mdkg/config.json")).expect("graph config");
        let ignore = fs::read(s.repo().join(".gitignore")).expect("gitignore");
        let exclude = fs::read(s.repo().join(".git/info/exclude")).ok();
        assert!(!s.repo().join(".worktrees").exists());
        let enabling = format!("hooks:{harness}");
        let mut args = vec!["install", "--hooks", "--enable", &enabling, "--json"];
        if dry {
            args.push("--dry-run");
        }
        let (code, v) = s.mdkg(&args);
        assert_ne!(code, 0, "{dry}: {v}");
        assert!(v.to_string().contains("git wouldn't ignore it"), "{v}");
        assert!(v.to_string().contains(".gitignore:1:"), "{v}");
        assert_eq!(
            s.git(&["for-each-ref", "--format=%(refname) %(objectname)"]),
            refs
        );
        assert_eq!(s.git(&["worktree", "list", "--porcelain"]), worktrees);
        assert_eq!(
            fs::read(s.repo().join(".git/config")).expect("git config"),
            config
        );
        assert_eq!(
            fs::read(s.repo().join(".mdkg/config.json")).expect("graph config"),
            graph_config
        );
        assert_eq!(
            fs::read(s.repo().join(".gitignore")).expect("gitignore"),
            ignore
        );
        assert_eq!(fs::read(s.repo().join(".git/info/exclude")).ok(), exclude);
        assert!(
            !s.repo().join(".worktrees").exists(),
            "no graph worktree created: {v}"
        );
        assert!(!s.repo().join(target).exists());
    }
}

#[test]
fn checkout_ignore_refusal_precedes_graph_creation() {
    checkout_ignore_refusal_preserves_graph_setup(false);
}

#[test]
fn checkout_ignore_refusal_precedes_dry_run_success() {
    checkout_ignore_refusal_preserves_graph_setup(true);
}

fn hook_enable_refuses_before_graph_refresh(dry: bool) {
    for (harness, target) in [
        ("claude", ".claude/settings.local.json"),
        ("codex", ".codex/hooks.json"),
        ("opencode", ".opencode/plugins/mdkg/server.ts"),
    ] {
        let s = Scratch::new();
        s.checkout_graph("main");
        let (code, v) = s.mdkg(&["install", "--json"]);
        assert_eq!(code, 0, "{v}");
        let graph = s.repo().join(".worktrees/graph");
        let old = s.git_in(&graph, &["rev-parse", "HEAD"]);
        let mut ignore = fs::read_to_string(s.repo().join(".gitignore")).expect("gitignore");
        ignore.push_str(&format!("!/{target}\n"));
        fs::write(s.repo().join(".gitignore"), ignore).expect("negation");
        s.git(&["add", ".gitignore"]);
        s.git(&["commit", "-qm", "main adds an ignore negation"]);
        let main = s.git(&["rev-parse", "main"]);
        assert_ne!(main, old);
        assert_eq!(s.git_in(&graph, &["status", "--porcelain"]), "");
        let refs = s.git(&["for-each-ref", "--format=%(refname) %(objectname)"]);
        let config = fs::read(s.repo().join(".git/config")).expect("git config");
        let graph_config = fs::read(graph.join(".mdkg/config.json")).expect("graph config");
        let graph_ignore = fs::read(graph.join(".gitignore")).ok();
        let enabling = format!("hooks:{harness}");
        let mut args = vec!["install", "--hooks", "--enable", &enabling, "--json"];
        if dry {
            args.push("--dry-run");
        }
        let (code, v) = s.mdkg_in(&graph, &args);
        assert_ne!(code, 0, "{harness}/{dry}: {v}");
        assert_eq!(s.git_in(&graph, &["rev-parse", "HEAD"]), old, "{v}");
        assert_eq!(
            s.git(&["for-each-ref", "--format=%(refname) %(objectname)"]),
            refs
        );
        assert_eq!(
            fs::read(s.repo().join(".git/config")).expect("git config"),
            config
        );
        assert_eq!(
            fs::read(graph.join(".mdkg/config.json")).expect("graph config"),
            graph_config
        );
        assert_eq!(fs::read(graph.join(".gitignore")).ok(), graph_ignore);
        assert!(!graph.join(target).exists());
        assert!(v.to_string().contains("refresh"), "{v}");
        assert!(v.to_string().contains(".worktrees/graph"), "{v}");
        assert!(
            v.to_string().contains("merge the configured main branch"),
            "{v}"
        );
        // Refresh alone still works. The subsequent hook request checks
        // the new ignore rule, rather than refusing a refresh again.
        let (code, v) = s.mdkg_in(&graph, &["install", "--json"]);
        assert_eq!(code, 0, "{v}");
        assert_eq!(s.git_in(&graph, &["rev-parse", "HEAD"]), main);
        let (code, v) = s.mdkg_in(
            &graph,
            &["install", "--hooks", "--enable", &enabling, "--json"],
        );
        assert_ne!(code, 0, "{v}");
        assert!(v.to_string().contains("git wouldn't ignore it"), "{v}");
        assert_eq!(s.git_in(&graph, &["rev-parse", "HEAD"]), main);
    }
}

#[test]
fn hook_enable_precedes_existing_graph_refresh() {
    hook_enable_refuses_before_graph_refresh(false);
}

#[test]
fn hook_enable_precedes_existing_graph_refresh_dry_run() {
    hook_enable_refuses_before_graph_refresh(true);
}

#[test]
fn hooks_allow_graph_refresh_when_their_ignore_files_are_unchanged() {
    for (harness, target) in [
        ("claude", ".claude/settings.local.json"),
        ("codex", ".codex/hooks.json"),
        ("opencode", ".opencode/plugins/mdkg/server.ts"),
    ] {
        let s = Scratch::new();
        s.checkout_graph("main");
        let (code, v) = s.mdkg(&["install", "--json"]);
        assert_eq!(code, 0, "{v}");
        let graph = s.repo().join(".worktrees/graph");
        let old = s.git_in(&graph, &["rev-parse", "HEAD"]);
        fs::create_dir(s.repo().join("unrelated")).expect("folder");
        fs::write(s.repo().join("unrelated/.gitignore"), "*.log\n").expect("unrelated ignore");
        s.git(&["add", "unrelated"]);
        s.git(&["commit", "-qm", "change an unrelated ignore file"]);
        let main = s.git(&["rev-parse", "HEAD"]);
        assert_ne!(main, old);
        let hook = format!("hooks:{harness}");
        let (code, v) = s.mdkg_in(
            &graph,
            &["install", "--enable", &hook, "--dry-run", "--json"],
        );
        assert_eq!(code, 0, "{harness}: {v}");
        assert_eq!(s.git_in(&graph, &["rev-parse", "HEAD"]), old);
        assert!(!graph.join(target).exists());
        let (code, v) = s.mdkg_in(&graph, &["install", "--enable", &hook, "--json"]);
        assert_eq!(code, 0, "{harness}: {v}");
        assert_eq!(s.git_in(&graph, &["rev-parse", "HEAD"]), main);
        assert!(graph.join(target).is_file());
    }
}

fn installed_hooks_refuse_before_graph_refresh(dry: bool) {
    for (harness, target) in [
        ("claude", Some(".claude/settings.local.json")),
        ("codex", Some(".codex/hooks.json")),
        ("opencode", Some(".opencode/plugins/mdkg/server.ts")),
        ("git", None),
    ] {
        let s = Scratch::new();
        s.checkout_graph("main");
        // Fixture commits must not execute the Git hooks installed below.
        let git = |dir: &Path, args: &[&str]| {
            let mut safe = vec!["-c", "core.hooksPath=/dev/null"];
            safe.extend_from_slice(args);
            s.git_in(dir, &safe)
        };
        let (code, v) = s.mdkg(&["install", "--json"]);
        assert_eq!(code, 0, "{v}");
        git(&s.repo(), &["add", ".gitignore"]);
        git(&s.repo(), &["commit", "-qm", "track worktree ignore"]);
        let graph = s.repo().join(".worktrees/graph");
        let (code, v) = s.mdkg_in(&graph, &["install", "--json"]);
        assert_eq!(code, 0, "{v}");
        let hook = format!("hooks:{harness}");
        let (code, v) = s.mdkg_in(&graph, &["install", "--enable", &hook, "--json"]);
        assert_eq!(code, 0, "{v}");
        git(&graph, &["add", ".mdkg", ".gitignore"]);
        git(&graph, &["commit", "-qm", "install hook"]);
        git(&s.repo(), &["merge", "--ff-only", "mdkg-graph"]);
        if let Some(target) = target {
            // Remove only the declared projection and ignored file: the
            // install record remains an opt-in, without --disable.
            let path = graph.join(".mdkg/config.json");
            let mut config: Value =
                serde_json::from_slice(&fs::read(&path).expect("config")).expect("json");
            config["projections"] = serde_json::json!([]);
            fs::write(path, serde_json::to_vec(&config).expect("json")).expect("omit hook");
            git(&graph, &["commit", "-qam", "omit hook without disabling"]);
            git(&s.repo(), &["merge", "--ff-only", "mdkg-graph"]);
            fs::remove_file(graph.join(target)).expect("remove ignored hook");
        }
        let path = s.repo().join(".mdkg/config.json");
        let mut config: Value =
            serde_json::from_slice(&fs::read(&path).expect("config")).expect("json");
        config["projections"] = serde_json::json!([hook]);
        fs::write(path, serde_json::to_vec(&config).expect("json")).expect("restore hook");
        if let Some(target) = target {
            let path = s.repo().join(".gitignore");
            let mut ignore = fs::read_to_string(&path).expect("ignore");
            ignore.push_str(&format!("!/{target}\n"));
            fs::write(path, ignore).expect("negation");
        } else {
            // The conservative Git-hook check watches the root ignore too;
            // unrelated main changes alone no longer block a refresh.
            let path = s.repo().join(".gitignore");
            let mut ignore = fs::read_to_string(&path).expect("ignore");
            ignore.push_str("# incoming hook ignore change\n");
            fs::write(path, ignore).expect("changed root ignore");
        }
        fs::write(s.repo().join("main-advance"), "main content\n").expect("advance");
        git(&s.repo(), &["add", ".mdkg", ".gitignore", "main-advance"]);
        git(&s.repo(), &["commit", "-qm", "restore hook on main"]);
        let main = git(&s.repo(), &["rev-parse", "HEAD"]);
        assert_eq!(git(&graph, &["status", "--porcelain"]), "");
        let refs = git(
            &s.repo(),
            &["for-each-ref", "--format=%(refname) %(objectname)"],
        );
        let config = fs::read(s.repo().join(".git/config")).expect("git config");
        let graph_config = fs::read(graph.join(".mdkg/config.json")).expect("graph config");
        let mut args = vec!["install", "--json"];
        if dry {
            args.push("--dry-run");
        }
        let (code, v) = s.mdkg_in(&graph, &args);
        assert_ne!(code, 0, "{harness}/{dry}: {v}");
        assert_eq!(
            git(
                &s.repo(),
                &["for-each-ref", "--format=%(refname) %(objectname)"]
            ),
            refs,
            "{v}"
        );
        assert_eq!(
            fs::read(s.repo().join(".git/config")).expect("git config"),
            config
        );
        assert_eq!(
            fs::read(graph.join(".mdkg/config.json")).expect("graph config"),
            graph_config
        );
        assert!(
            v.to_string().contains("merge the configured main branch"),
            "{v}"
        );
        assert!(
            !v.to_string().contains("run install without --hooks"),
            "{v}"
        );
        // An explicit disable excludes the kept hook from the guard and
        // the eventual sync, so refreshing is allowed without recreating it.
        let (code, v) = s.mdkg_in(&graph, &["install", "--disable", &hook, "--json"]);
        assert_eq!(code, 0, "{harness}: {v}");
        assert_eq!(git(&graph, &["rev-parse", "HEAD"]), main);
        if let Some(target) = target {
            assert!(!graph.join(target).exists());
        }
    }
}

#[test]
fn previously_installed_hooks_refuse_graph_refresh() {
    installed_hooks_refuse_before_graph_refresh(false);
}

#[test]
fn previously_installed_hooks_refuse_graph_refresh_dry_run() {
    installed_hooks_refuse_before_graph_refresh(true);
}

fn initial_ignore_negation_refuses_before_publication(professional: bool, dry: bool) {
    let s = Scratch::new();
    s.checkout_graph("main");
    let (setup, target, enable) = if professional {
        let mine = s.0.join("mine.git");
        s.git_in(&s.0, &["init", "-q", "--bare", &mine.to_string_lossy()]);
        s.git(&["remote", "add", "mine", &mine.to_string_lossy()]);
        ("professional", ".mcp.json", "mcp:claude")
    } else {
        ("local", ".claude/settings.local.json", "hooks:claude")
    };
    fs::write(s.repo().join(".gitignore"), format!("!/{target}\n")).expect("ignore negation");
    s.git(&["add", ".gitignore"]);
    s.git(&["commit", "-qm", "reinclude the projection"]);
    let config = fs::read(s.repo().join(".git/config")).expect("git config");
    let refs = s.git(&["for-each-ref", "--format=%(refname) %(objectname)"]);
    let exclude = fs::read(s.repo().join(".git/info/exclude")).ok();
    let ignore = fs::read(s.repo().join(".gitignore")).expect("gitignore");
    let mut args = vec!["install", "--setup", setup, "--enable", enable, "--json"];
    if professional {
        args.extend(["--graph-remote", "mine"]);
    } else {
        args.push("--hooks");
    }
    if dry {
        args.push("--dry-run");
    }
    let (code, v) = s.mdkg(&args);
    assert_ne!(code, 0, "{setup}/{dry}: {v}");
    assert_eq!(codes(&v), ["install.unmergeable"], "{v}");
    assert!(v.to_string().contains("git wouldn't ignore it"), "{v}");
    assert!(v.to_string().contains(target), "{v}");
    assert!(v.to_string().contains(".gitignore:1:"), "{v}");
    assert_eq!(
        s.git(&["for-each-ref", "--format=%(refname) %(objectname)"]),
        refs,
        "refusal must precede creating the graph ref: {v}"
    );
    assert_eq!(
        fs::read(s.repo().join(".git/config")).expect("git config"),
        config
    );
    assert_eq!(fs::read(s.repo().join(".git/info/exclude")).ok(), exclude);
    assert_eq!(
        fs::read(s.repo().join(".gitignore")).expect("gitignore"),
        ignore
    );
    assert!(!s.repo().join(target).exists());
}

#[test]
fn initial_local_hook_ignore_refusal_precedes_publication() {
    initial_ignore_negation_refuses_before_publication(false, false);
}

#[test]
fn initial_local_hook_ignore_refusal_precedes_dry_run_success() {
    initial_ignore_negation_refuses_before_publication(false, true);
}

#[test]
fn initial_professional_ignore_refusal_precedes_publication() {
    initial_ignore_negation_refuses_before_publication(true, false);
}

#[test]
fn initial_professional_ignore_refusal_precedes_dry_run_success() {
    initial_ignore_negation_refuses_before_publication(true, true);
}

#[test]
fn conflicting_projection_flags_refuse_before_setup_even_without_a_graph() {
    for graph in [false, true] {
        let s = Scratch::new();
        if graph {
            s.checkout_graph("main");
        } else {
            s.code("main");
        }
        let config = fs::read(s.repo().join(".git/config")).unwrap();
        let refs = s.git(&["for-each-ref", "--format=%(refname) %(objectname)"]);
        for dry in [true, false] {
            let mut args = vec![
                "install",
                "--setup",
                "local",
                "--enable",
                "mcp:codex",
                "--disable",
                "mcp:codex",
                "--json",
            ];
            if dry {
                args.push("--dry-run");
            }
            let (code, v) = s.mdkg(&args);
            assert_ne!(code, 0, "{graph}/{dry}: {v}");
            assert!(v.to_string().contains("both enabled and disabled"), "{v}");
            assert_eq!(fs::read(s.repo().join(".git/config")).unwrap(), config);
            assert_eq!(
                s.git(&["for-each-ref", "--format=%(refname) %(objectname)"]),
                refs
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn non_regular_committed_canonicals_are_not_missing_defaults() {
    for (h, canonical, kind) in [
        ("claude", ".mdkg/hooks/claude/settings.json", "leaf"),
        ("codex", ".mdkg/hooks/codex/hooks.json", "leaf"),
        ("opencode", ".mdkg/hooks/opencode/mdkg/server.ts", "leaf"),
        ("codex", ".mdkg/hooks/codex/hooks.json", "parent"),
        ("codex", ".mdkg/hooks/codex/hooks.json", "gitlink"),
    ] {
        let s = Scratch::new();
        s.checkout_graph("main");
        let path = s.repo().join(canonical);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        if kind == "parent" {
            fs::remove_dir_all(path.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink("other-hooks", path.parent().unwrap()).unwrap();
        } else {
            fs::remove_file(&path).ok();
            if kind == "gitlink" {
                fs::create_dir(&path).unwrap();
            } else {
                std::os::unix::fs::symlink("other-hook", &path).unwrap();
            }
        }
        s.git(&["add", ".mdkg"]);
        if kind == "gitlink" {
            let oid = s.git(&["rev-parse", "HEAD"]);
            s.git(&[
                "update-index",
                "--add",
                "--cacheinfo",
                "160000",
                &oid,
                canonical,
            ]);
        }
        s.git(&["commit", "-qm", "non-regular canonical"]);
        let config = fs::read(s.repo().join(".git/config")).unwrap();
        let refs = s.git(&["for-each-ref", "--format=%(refname) %(objectname)"]);
        let enabling = format!("hooks:{h}");
        for dry in [true, false] {
            let mut args = vec![
                "install", "--setup", "local", "--enable", &enabling, "--json",
            ];
            if dry {
                args.push("--dry-run");
            }
            let (code, v) = s.mdkg(&args);
            assert_ne!(code, 0, "{h}/{kind}/{dry}: {v}");
            assert!(v.to_string().contains("symlink or gitlink"), "{v}");
            assert_eq!(fs::read(s.repo().join(".git/config")).unwrap(), config);
            assert_eq!(
                s.git(&["for-each-ref", "--format=%(refname) %(objectname)"]),
                refs
            );
        }
        let (code, v) = s.mdkg(&["install", "--setup", "local", "--json"]);
        assert_eq!(code, 0, "unrequested {h}/{kind} is left alone: {v}");
    }
}

#[cfg(target_os = "macos")]
#[test]
fn case_alias_of_absent_projected_hooks_folder_refuses_before_publication() {
    use std::os::unix::fs::PermissionsExt as _;
    let s = Scratch::new();
    s.checkout_graph("main");
    let probe = s.repo().join("CaseProbe");
    fs::create_dir(&probe).unwrap();
    let folds = s.repo().join("caseprobe").exists();
    fs::remove_dir(probe).unwrap();
    if !folds {
        return;
    }
    let skill = s.repo().join(".mdkg/skills/demo");
    fs::create_dir_all(&skill).unwrap();
    fs::write(
        skill.join("SKILL.md"),
        "---\nname: demo\ndescription: fixture\n---\nDemo\n",
    )
    .unwrap();
    fs::write(skill.join("previous-hooks-path"), "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(
        skill.join("previous-hooks-path"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    s.git(&["add", ".mdkg"]);
    s.git(&["commit", "-qm", "case alias fixture"]);
    let (code, v) = s.mdkg(&["install", "--setup", "local", "--json"]);
    assert_eq!(code, 0, "{v}");
    s.git(&["config", "core.hooksPath", ".AGENTS/SKILLS/demo"]);
    let tip = s.git(&["rev-parse", GRAPH]);
    let config = s.git(&["show", &format!("{GRAPH}:.mdkg/config.json")]);
    for dry in [true, false] {
        let mut args = vec![
            "install",
            "--enable",
            "skills:.agents/skills,hooks:git",
            "--json",
        ];
        if dry {
            args.push("--dry-run");
        }
        let (code, v) = s.mdkg(&args);
        assert_ne!(code, 0, "{dry}: {v}");
        assert!(v.to_string().contains("projected destination"), "{v}");
        assert_eq!(s.git(&["rev-parse", GRAPH]), tip);
        assert_eq!(
            s.git(&["show", &format!("{GRAPH}:.mdkg/config.json")]),
            config
        );
        assert!(!s.repo().join(".agents/skills/demo").exists());
    }
}

/// Setup must validate requested hooks before creating or adopting a ref.
#[test]
fn initial_setup_previews_every_requested_harness_before_publishing() {
    for (harness, target) in [
        ("claude", ".claude/settings.local.json"),
        ("codex", ".codex/hooks.json"),
        ("opencode", ".opencode/plugins/mdkg/server.ts"),
    ] {
        let s = Scratch::new();
        s.checkout_graph("main");
        let path = s.repo().join(target);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        if harness == "opencode" {
            fs::create_dir(&path).unwrap();
        } else {
            fs::write(&path, "{broken-json\n").unwrap();
        }
        let config = fs::read(s.repo().join(".git/config")).unwrap();
        let refs = s.git(&["for-each-ref", "--format=%(refname) %(objectname)"]);
        let enabling = format!("hooks:{harness}");
        for dry in [true, false] {
            let mut args = vec![
                "install", "--setup", "local", "--enable", &enabling, "--json",
            ];
            if dry {
                args.push("--dry-run");
            }
            let (code, v) = s.mdkg(&args);
            assert_ne!(code, 0, "{harness}/{dry}: {v}");
            assert!(v.to_string().contains("install.unmergeable"), "{v}");
            assert_eq!(fs::read(s.repo().join(".git/config")).unwrap(), config);
            assert_eq!(
                s.git(&["for-each-ref", "--format=%(refname) %(objectname)"]),
                refs
            );
            assert_eq!(s.config("mdkg.graph.ref"), None);
        }
    }
}

#[test]
fn a_hooks_path_inside_a_future_skill_mirror_refuses_before_publication() {
    use std::os::unix::fs::PermissionsExt as _;
    let s = Scratch::new();
    s.checkout_graph("main");
    let skill = s.repo().join(".mdkg/skills/demo");
    fs::create_dir_all(&skill).unwrap();
    fs::write(
        skill.join("SKILL.md"),
        "---\nname: demo\ndescription: fixture\n---\nDemo\n",
    )
    .unwrap();
    fs::write(skill.join("post-commit"), "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(skill.join("post-commit"), fs::Permissions::from_mode(0o755)).unwrap();
    s.git(&["add", ".mdkg"]);
    s.git(&["commit", "-qm", "executable skill fixture"]);
    let (code, v) = s.mdkg(&["install", "--setup", "local", "--json"]);
    assert_eq!(code, 0, "{v}");
    s.git(&["config", "core.hooksPath", ".agents/skills/demo"]);
    let destination =
        mdkg_run::install::hook_files::git_hooks_dir(&s.repo().join(".git")).join("post-commit");
    fs::create_dir_all(&destination).unwrap();
    let tip = s.git(&["rev-parse", GRAPH]);
    let config = s.git(&["show", &format!("{GRAPH}:.mdkg/config.json")]);
    for dry in [true, false] {
        let mut args = vec![
            "install",
            "--enable",
            "skills:.agents/skills,hooks:git",
            "--json",
        ];
        if dry {
            args.push("--dry-run");
        }
        let (code, v) = s.mdkg(&args);
        assert_ne!(code, 0, "{dry}: {v}");
        let message = v.to_string();
        assert!(
            message.contains("projected destination")
                && message.contains(".agents/skills/demo")
                && message.contains(".agents/skills"),
            "{v}"
        );
        assert_eq!(s.git(&["rev-parse", GRAPH]), tip);
        assert_eq!(
            s.git(&["show", &format!("{GRAPH}:.mdkg/config.json")]),
            config
        );
        assert!(!s.repo().join(".agents/skills/demo").exists());
    }
}

#[test]
fn reserved_chained_hook_names_refuse_and_uninstall_restores_the_previous_path() {
    use std::os::unix::fs::PermissionsExt as _;
    for name in [
        "previous-hooks-path",
        "previous-local-hooks-path",
        "mdkg-binary",
        "installed-from",
    ] {
        let s = Scratch::new();
        s.ref_mode("main");
        let previous = s.repo().join("company-hooks");
        fs::create_dir(&previous).unwrap();
        let script = "#!/bin/sh\nexit 0\n";
        fs::write(previous.join(name), script).unwrap();
        fs::set_permissions(previous.join(name), fs::Permissions::from_mode(0o755)).unwrap();
        s.git(&["config", "core.hooksPath", "company-hooks"]);
        let tip = s.git(&["rev-parse", GRAPH]);
        for dry in [true, false] {
            let mut args = vec!["install", "--enable", "hooks:git", "--json"];
            if dry {
                args.push("--dry-run");
            }
            let (code, v) = s.mdkg(&args);
            assert_ne!(code, 0, "{name}/{dry}: {v}");
            assert!(
                v.to_string().contains("reserved") && v.to_string().contains("rename"),
                "{v}"
            );
            assert_eq!(s.git(&["rev-parse", GRAPH]), tip);
            assert_eq!(s.config("core.hooksPath").as_deref(), Some("company-hooks"));
        }
        fs::rename(previous.join(name), previous.join("company-helper")).unwrap();
        let (code, v) = s.mdkg(&["install", "--enable", "hooks:git", "--json"]);
        assert_eq!(code, 0, "{v}");
        let (code, v) = s.mdkg(&["install", "--disable", "hooks:git", "--json"]);
        assert_eq!(code, 0, "{v}");
        assert_eq!(s.config("core.hooksPath").as_deref(), Some("company-hooks"));
        assert_eq!(
            fs::read_to_string(previous.join("company-helper")).unwrap(),
            script
        );
    }
}

/// Setup must validate requested hooks before creating or adopting a ref.
#[test]
fn git_hook_refusals_leave_graph_creation_and_adoption_unchanged() {
    for adopt in [false, true] {
        let s = Scratch::new();
        let (root, setup) = if adopt {
            s.ref_mode("main");
            let (code, v) = s.mdkg(&["install", "--enable", "hooks:git,hooks:opencode", "--json"]);
            assert_eq!(code, 0, "{v}");
            let clone = s.0.join("clone");
            s.git_in(
                &s.0,
                &[
                    "clone",
                    "-q",
                    s.repo().to_str().unwrap(),
                    clone.to_str().unwrap(),
                ],
            );
            s.git_in(&clone, &["config", "user.name", "Graph Test"]);
            s.git_in(&clone, &["config", "user.email", "graph@example.invalid"]);
            assert!(
                !clone.join(".mdkg").exists(),
                "listed hooks come from the adopted ref"
            );
            (clone, "personal")
        } else {
            s.checkout_graph("main");
            (s.repo(), "local")
        };
        s.git_in(&root, &["config", "extensions.worktreeConfig", "true"]);
        s.git_in(
            &root,
            &["config", "--worktree", "core.hooksPath", "company-hooks"],
        );
        let config = fs::read(root.join(".git/config")).unwrap();
        let exclude = fs::read(root.join(".git/info/exclude")).ok();
        let refs = s.git_in(
            &root,
            &["for-each-ref", "--format=%(refname) %(objectname)"],
        );
        for dry in [true, false] {
            let mut args = vec!["install", "--setup", setup, "--hooks", "--json"];
            if !adopt {
                args.extend(["--enable", "hooks:git,hooks:opencode"]);
            }
            if dry {
                args.push("--dry-run");
            }
            let (code, v) = s.mdkg_in(&root, &args);
            assert_ne!(code, 0, "{adopt}/{dry}: {v}");
            assert!(v.to_string().contains("config.worktree"), "{v}");
            assert_eq!(fs::read(root.join(".git/config")).unwrap(), config);
            assert_eq!(fs::read(root.join(".git/info/exclude")).ok(), exclude);
            assert_eq!(
                s.git_in(
                    &root,
                    &["for-each-ref", "--format=%(refname) %(objectname)"]
                ),
                refs
            );
            assert!(!root.join(".opencode").exists());
        }
        if !adopt {
            // A Claude-only request must not be refused for Git's path.
            let (code, v) = s.mdkg_in(
                &root,
                &[
                    "install",
                    "--setup",
                    "local",
                    "--hooks",
                    "--enable",
                    "hooks:claude",
                    "--json",
                ],
            );
            assert_eq!(code, 0, "{v}");
        }
    }
}

#[test]
fn git_hook_destination_directories_refuse_before_publication() {
    for name in ["previous-hooks-path", "pre-commit"] {
        let s = Scratch::new();
        s.ref_mode("main");
        let target =
            mdkg_run::install::hook_files::git_hooks_dir(&s.repo().join(".git")).join(name);
        fs::create_dir_all(&target).unwrap();
        let tip = s.git(&["rev-parse", GRAPH]);
        let config = s.git(&["show", &format!("{GRAPH}:.mdkg/config.json")]);
        for dry in [true, false] {
            let mut args = vec![
                "install",
                "--hooks",
                "--enable",
                "hooks:git,hooks:opencode",
                "--json",
            ];
            if dry {
                args.push("--dry-run");
            }
            let (code, v) = s.mdkg(&args);
            assert_ne!(code, 0, "{name}/{dry}: {v}");
            assert!(
                v.to_string().contains(name) && v.to_string().contains("plain file"),
                "{v}"
            );
            assert_eq!(s.git(&["rev-parse", GRAPH]), tip, "{v}");
            assert_eq!(
                s.git(&["show", &format!("{GRAPH}:.mdkg/config.json")]),
                config
            );
            assert!(target.is_dir());
        }
    }
}

/// A future-plan preview must not create a fresh checkout's lock or record.
#[test]
fn a_git_hook_worktree_override_refuses_before_graph_publication() {
    let s = Scratch::new();
    s.ref_mode("main");
    let wt = s.0.join("override");
    s.git(&["worktree", "add", "-qb", "override", wt.to_str().unwrap()]);
    s.git(&["config", "extensions.worktreeConfig", "true"]);
    s.git_in(
        &wt,
        &["config", "--worktree", "core.hooksPath", "company-hooks"],
    );
    let tip = s.git(&["rev-parse", GRAPH]);
    let config = s.git(&["show", &format!("{GRAPH}:.mdkg/config.json")]);
    for dry in [true, false] {
        let mut args = vec![
            "install",
            "--hooks",
            "--enable",
            "hooks:git,hooks:opencode",
            "--json",
        ];
        if dry {
            args.push("--dry-run");
        }
        let (code, v) = s.mdkg_in(&wt, &args);
        assert_ne!(code, 0, "{dry}: {v}");
        assert!(v.to_string().contains("config.worktree"), "{v}");
        assert_eq!(s.git(&["rev-parse", GRAPH]), tip, "{v}");
        assert_eq!(
            s.git(&["show", &format!("{GRAPH}:.mdkg/config.json")]),
            config
        );
        assert!(!wt.join(".opencode").exists());
    }
}

/// A future-plan preview must not create a fresh checkout's lock or record.
#[test]
fn a_fresh_linked_ref_preview_creates_no_install_metadata() {
    fn snapshot(root: &Path) -> Vec<(PathBuf, Option<Vec<u8>>)> {
        fn visit(root: &Path, path: &Path, out: &mut Vec<(PathBuf, Option<Vec<u8>>)>) {
            let Ok(meta) = fs::symlink_metadata(path) else {
                return;
            };
            out.push((
                path.strip_prefix(root).unwrap().into(),
                (!meta.is_dir()).then(|| fs::read(path).unwrap()),
            ));
            if meta.is_dir() {
                for entry in fs::read_dir(path).unwrap() {
                    visit(root, &entry.unwrap().path(), out);
                }
            }
        }
        let mut out = Vec::new();
        visit(root, root, &mut out);
        out.sort();
        out
    }
    let s = Scratch::new();
    s.ref_mode("main");
    let wt = s.0.join("fresh");
    s.git(&["worktree", "add", "-qb", "fresh", wt.to_str().unwrap()]);
    let records = s.repo().join(".git/mdkg/install");
    let before = snapshot(&records);
    let tip = s.git(&["rev-parse", GRAPH]);
    let (code, v) = s.mdkg_in(
        &wt,
        &[
            "install",
            "--dry-run",
            "--hooks",
            "--enable",
            "hooks:opencode",
            "--json",
        ],
    );
    assert_eq!(code, 0, "{v}");
    assert_eq!(
        snapshot(&records),
        before,
        "preview created install metadata: {v}"
    );
    assert_eq!(s.git(&["rev-parse", GRAPH]), tip);
    assert!(!wt.join(".opencode").exists());
}

/// The complete plan must reject projections that resolve to one file.
#[cfg(unix)]
#[test]
fn pending_hooks_and_mcp_check_shared_landings_before_publication() {
    let s = Scratch::new();
    s.checkout_graph("main");
    let (code, v) = s.mdkg(&["install", "--setup", "local", "--json"]);
    assert_eq!(code, 0, "{v}");
    let target = s.repo().join(".opencode/plugins/mdkg/server.ts");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(s.repo().join(".mcp.json"), "{\"mcpServers\":{}}\n").unwrap();
    std::os::unix::fs::symlink("../../../.mcp.json", &target).unwrap();
    let tip = s.git(&["rev-parse", GRAPH]);
    let original = fs::read(s.repo().join(".mcp.json")).unwrap();
    for dry in [true, false] {
        let mut args = vec![
            "install",
            "--hooks",
            "--enable",
            "hooks:opencode,mcp:claude",
            "--json",
        ];
        if dry {
            args.push("--dry-run");
        }
        let (code, v) = s.mdkg(&args);
        assert_ne!(code, 0, "{dry}: {v}");
        assert!(v.to_string().contains("install.unmergeable"), "{v}");
        assert_eq!(s.git(&["rev-parse", GRAPH]), tip, "{v}");
        assert_eq!(fs::read(s.repo().join(".mcp.json")).unwrap(), original);
        assert!(
            !s.git(&["ls-tree", "-r", "--name-only", GRAPH])
                .contains(".mdkg/hooks/opencode/mdkg/server.ts")
        );
    }
}

#[test]
fn tracked_malformed_pending_hooks_refuse_before_professional_publication() {
    let s = Scratch::new();
    s.checkout_graph("main");
    let remote = s.0.join("mine.git");
    s.git_in(&s.0, &["init", "-q", "--bare", &remote.to_string_lossy()]);
    s.git(&["remote", "add", "mine", &remote.to_string_lossy()]);
    let (code, v) = s.mdkg(&[
        "install",
        "--setup",
        "professional",
        "--graph-remote",
        "mine",
        "--json",
    ]);
    assert_eq!(code, 0, "{v}");
    fs::create_dir_all(s.repo().join(".codex")).unwrap();
    let malformed = "{\"hooks\": [\n";
    fs::write(s.repo().join(".codex/hooks.json"), malformed).unwrap();
    s.git(&["add", "-f", ".codex/hooks.json"]);
    s.git(&["commit", "-qm", "tracked malformed hooks"]);
    let tip = s.git(&["rev-parse", GRAPH]);
    for dry in [true, false] {
        let mut args = vec!["install", "--hooks", "--enable", "hooks:codex", "--json"];
        if dry {
            args.push("--dry-run");
        }
        let (code, v) = s.mdkg(&args);
        assert_ne!(code, 0, "{dry}: {v}");
        assert!(v.to_string().contains("install.unmergeable"), "{v}");
        assert_eq!(s.git(&["rev-parse", GRAPH]), tip, "{v}");
        assert_eq!(
            fs::read_to_string(s.repo().join(".codex/hooks.json")).unwrap(),
            malformed
        );
        assert!(
            !s.git(&["ls-tree", "-r", "--name-only", GRAPH])
                .contains(".mdkg/hooks/codex/hooks.json")
        );
    }
}

#[test]
fn doctor_warns_for_an_oversized_opencode_canonical_on_the_graph_ref() {
    let s = Scratch::new();
    s.checkout_graph("main");
    let canonical = s.repo().join(".mdkg/hooks/opencode/mdkg/server.ts");
    fs::create_dir_all(canonical.parent().unwrap()).unwrap();
    let huge = vec![b' '; mdkg_run::install::hook_files::HOOK_FILE_CAP as usize + 1];
    fs::write(&canonical, huge).unwrap();
    let path = s.repo().join(".mdkg/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["projections"] = serde_json::json!(["hooks:opencode"]);
    fs::write(path, serde_json::to_vec(&config).unwrap()).unwrap();
    s.git(&["add", ".mdkg"]);
    s.git(&["commit", "-qm", "oversized canonical fixture"]);
    let (code, v) = s.mdkg(&["install", "--setup", "local", "--json"]);
    assert_eq!(code, 0, "{v}");
    let (_, doctor) = s.mdkg(&["doctor", "--json"]);
    let hooks = check_of(&doctor, "hooks");
    assert_eq!(hooks["status"], "warning", "{doctor}");
    assert!(
        hooks.to_string().contains("missing or unreadable"),
        "{doctor}"
    );
}

/// An existing canonical default is a graph update, not a write into the
/// read-only cache. Refusals still precede that publish (L4a).
#[test]
fn initial_setup_previews_the_upgraded_opencode_default() {
    let s = Scratch::new();
    s.checkout_graph("main");
    let canonical = ".mdkg/hooks/opencode/mdkg/server.ts";
    let target = ".opencode/plugins/mdkg/server.ts";
    let current = mdkg_run::hooks::opencode_plugin("/opt/mdkg-original");
    let old = current.replace("[\"bash\", \"shell\"]", "[\"bash\"]");
    assert_eq!(
        mdkg_run::hooks::opencode_plugin_upgrade(&old),
        Some(current.clone())
    );
    fs::create_dir_all(s.repo().join(canonical).parent().unwrap()).unwrap();
    fs::write(s.repo().join(canonical), &old).unwrap();
    s.git(&["add", ".mdkg"]);
    s.git(&["commit", "-qm", "previous OpenCode plugin"]);
    let refs = s.git(&["for-each-ref", "--format=%(refname) %(objectname)"]);
    let config = fs::read(s.repo().join(".git/config")).unwrap();
    for dry in [true, false] {
        let mut args = vec![
            "install",
            "--setup",
            "local",
            "--hooks",
            "--enable",
            "hooks:opencode",
            "--json",
        ];
        if dry {
            args.push("--dry-run");
        }
        let (code, v) = s.mdkg(&args);
        assert_eq!(code, 0, "{dry}: {v}");
        if dry {
            assert_eq!(
                s.git(&["for-each-ref", "--format=%(refname) %(objectname)"]),
                refs
            );
            assert_eq!(fs::read(s.repo().join(".git/config")).unwrap(), config);
            assert!(!s.repo().join(target).exists());
        }
    }
    assert_eq!(
        s.git(&["show", &format!("{GRAPH}:{canonical}")]),
        current.trim()
    );
    assert_eq!(fs::read_to_string(s.repo().join(target)).unwrap(), current);
    assert_eq!(fs::read_to_string(s.repo().join(canonical)).unwrap(), old);
}

#[test]
fn opencode_default_upgrade_reaches_the_graph_ref_and_projection() {
    let s = Scratch::new();
    s.checkout_graph("trunk");
    let canonical = ".mdkg/hooks/opencode/mdkg/server.ts";
    let target = ".opencode/plugins/mdkg/server.ts";
    let current = mdkg_run::hooks::opencode_plugin("/opt/mdkg-original");
    let old = current.replace("[\"bash\", \"shell\"]", "[\"bash\"]");
    fs::create_dir_all(s.repo().join(canonical).parent().unwrap()).unwrap();
    fs::write(s.repo().join(canonical), &old).unwrap();
    let config_path = s.repo().join(".mdkg/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    config["projections"] = serde_json::json!(["hooks:opencode"]);
    fs::write(config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    s.git(&["add", ".mdkg"]);
    s.git(&["commit", "-qm", "previous OpenCode plugin"]);
    let (code, v) = s.mdkg(&["install", "--setup", "local", "--json"]);
    assert_eq!(code, 0, "{v}");
    let tip = s.git(&["rev-parse", GRAPH]);
    assert_eq!(
        s.git(&["show", &format!("{GRAPH}:{canonical}")]),
        old.trim()
    );
    assert!(!s.repo().join(target).exists(), "no --hooks: {v}");
    let (code, v) = s.mdkg(&["install", "--hooks", "--dry-run", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert!(notes(&v).contains(canonical), "{v}");
    assert!(
        v["data"]["changes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|change| change["path"] == target && change["action"] == "created"),
        "{v}"
    );
    assert_eq!(s.git(&["rev-parse", GRAPH]), tip);
    assert!(!s.repo().join(target).exists());
    // Reading a hard-linked target succeeds, but the eventual write
    // would split a user's file. Refuse before changing the graph tip.
    fs::create_dir_all(s.repo().join(target).parent().unwrap()).unwrap();
    let other = s.repo().join("other-plugin.txt");
    fs::write(&other, &old).unwrap();
    fs::hard_link(&other, s.repo().join(target)).unwrap();
    let (code, v) = s.mdkg(&["install", "--hooks", "--json"]);
    assert_ne!(code, 0, "{v}");
    assert!(v.to_string().contains("hard links"), "{v}");
    assert_eq!(s.git(&["rev-parse", GRAPH]), tip, "nothing published: {v}");
    assert_eq!(fs::read_to_string(&other).unwrap(), old);
    assert_eq!(fs::read_to_string(s.repo().join(target)).unwrap(), old);
    fs::remove_file(s.repo().join(target)).unwrap();
    // A directory where the target file belongs must refuse before the
    // canonical file is published, even though only that file changes.
    fs::create_dir_all(s.repo().join(target)).unwrap();
    let (code, v) = s.mdkg(&["install", "--hooks", "--dry-run", "--json"]);
    assert_ne!(code, 0, "a preview must make the same target refusal: {v}");
    assert_eq!(s.git(&["rev-parse", GRAPH]), tip);
    let (code, v) = s.mdkg(&["install", "--hooks", "--json"]);
    assert_ne!(code, 0, "{v}");
    assert_eq!(s.git(&["rev-parse", GRAPH]), tip, "nothing published: {v}");
    fs::remove_dir(s.repo().join(target)).unwrap();
    let (code, v) = s.mdkg(&["install", "--hooks", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(
        s.git(&["show", &format!("{GRAPH}:{canonical}")]),
        current.trim()
    );
    assert_eq!(fs::read_to_string(s.repo().join(target)).unwrap(), current);
    assert_eq!(
        fs::read_to_string(s.repo().join(canonical)).unwrap(),
        old,
        "the original checkout graph is left alone"
    );
    let updated = s.git(&["rev-parse", GRAPH]);
    let (code, v) = s.mdkg(&["install", "--hooks", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(s.git(&["rev-parse", GRAPH]), updated, "idempotent: {v}");
    let (_, doctor) = s.mdkg(&["doctor", "--json"]);
    assert_eq!(check_of(&doctor, "hooks")["status"], "ok", "{doctor}");
}

/// #14492: a hook file under a linked folder in professional is left
/// alone like the folder's other files, never refused for.
#[cfg(unix)]
#[test]
fn a_professional_hook_file_under_a_linked_folder_is_left_alone() {
    let s = Scratch::new();
    s.checkout_graph("main");
    fs::create_dir_all(s.repo().join("company")).expect("dir");
    fs::write(s.repo().join("company/config.toml"), "model = \"x\"\n").expect("tracked");
    s.git(&["add", "company/config.toml"]);
    s.git(&["commit", "-q", "-m", "company config"]);
    std::os::unix::fs::symlink("company", s.repo().join(".codex")).expect("link");
    let mine = s.0.join("mine.git");
    s.git_in(&s.0, &["init", "-q", "--bare", &mine.to_string_lossy()]);
    s.git(&["remote", "add", "mine", &mine.to_string_lossy()]);
    let (code, v) = s.mdkg(&[
        "install",
        "--setup",
        "professional",
        "--graph-remote",
        "mine",
        "--json",
    ]);
    assert_eq!(code, 0, "{v}");
    let (code, v) = s.mdkg(&["install", "--hooks", "--enable", "hooks:codex", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert!(!s.repo().join("company/hooks.json").exists(), "{v}");
    assert!(notes(&v).contains("left alone"), "{v}");
}

/// #14320: the open-changeset guard never opens a store through a link to
/// one that isn't there, where SQLite would start an empty one.
#[cfg(unix)]
#[test]
fn a_dangling_store_link_refuses_the_move() {
    let s = Scratch::new();
    s.checkout_graph("main");
    let store = s.repo().join(".git/mdkg/coord.sqlite");
    let _ = fs::remove_file(&store);
    fs::create_dir_all(store.parent().expect("parent")).expect("dir");
    // Its folder is there, so SQLite could start an empty store in it.
    let gone = s.0.join("unmounted/coord.sqlite");
    fs::create_dir_all(gone.parent().expect("parent")).expect("dir");
    std::os::unix::fs::symlink(&gone, &store).expect("ln");
    let (code, v) = s.mdkg(&["install", "--setup", "local", "--json"]);
    assert_ne!(code, 0, "{v}");
    assert!(!gone.exists(), "no store made there: {v}");
    assert_eq!(s.config("mdkg.graph.ref"), None, "{v}");
}

/// #14381 minors: the professional setup with no mainRef at all, and the
/// personal setup on a clone with no code base, install and pass doctor's
/// graph checks: mdkg never requires main.
#[test]
fn professional_and_personal_need_no_code_base() {
    let s = Scratch::new();
    s.checkout_graph("trunk");
    let mine = s.0.join("mine.git");
    s.git_in(&s.0, &["init", "-q", "--bare", &mine.to_string_lossy()]);
    s.git(&["remote", "add", "mine", &mine.to_string_lossy()]);
    let (code, v) = s.mdkg(&[
        "install",
        "--setup",
        "professional",
        "--graph-remote",
        "mine",
        "--json",
    ]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(s.config("mdkg.mainRef"), None, "{v}");
    let (code, v) = s.mdkg(&["install", "--enable", "mcp:claude", "--json"]);
    assert_eq!(code, 0, "{v}");
    let (code, doctor) = s.mdkg(&["doctor", "--json"]);
    assert_eq!(code, 0, "{doctor}");

    let p = Scratch::new();
    p.ref_mode("trunk");
    let clone = p.0.join("clone");
    p.git_in(
        &p.0,
        &[
            "clone",
            "-q",
            &p.repo().to_string_lossy(),
            &clone.to_string_lossy(),
        ],
    );
    p.git_in(&clone, &["config", "user.name", "Graph Test"]);
    p.git_in(&clone, &["config", "user.email", "graph@example.invalid"]);
    let (code, v) = p.mdkg_in(&clone, &["install", "--setup", "personal", "--json"]);
    assert_eq!(code, 0, "{v}");
    let (code, doctor) = p.mdkg_in(&clone, &["doctor", "--json"]);
    assert_eq!(code, 0, "{doctor}");
}

impl Scratch {
    /// The clone's `mdkg.graph.<id>.worktree` records.
    fn worktree_records(&self) -> String {
        let out = self
            .isolated(Command::new("git").arg("-C").arg(self.repo()))
            .args([
                "config",
                "--file",
                ".git/config",
                "--get-regexp",
                r"^mdkg\.graph\..*\.worktree$",
            ])
            .output()
            .expect("git");
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// `status` and `agents status` both answer.
    fn status_works(&self) {
        for args in [&["status", "--json"][..], &["agents", "status", "--json"]] {
            let (code, v) = self.mdkg(args);
            assert_eq!(code, 0, "{args:?}: {v}");
            assert_eq!(v["ok"], true, "{args:?}: {v}");
        }
    }
}

/// B2: in ref mode a `.worktrees/graph` view is a read-only convenience;
/// status works with it, and doctor agrees.
#[test]
fn ref_mode_view_keeps_status_working() {
    let s = Scratch::new();
    s.code("main");
    let (code, v) = s.mdkg(&["install", "--setup", "local", "--json"]);
    assert_eq!(code, 0, "{v}");
    let (code, v) = s.mdkg(&["init", "--json"]);
    assert_eq!(code, 0, "{v}");
    let (code, v) = s.mdkg(&["install", "--view", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert!(s.repo().join(".worktrees/graph/.git").exists());
    s.status_works();
    // Ref-mode writes go to the graph ref, past the view.
    let (code, v) = s.mdkg(&["new", "task", "beside the view", "--json"]);
    assert_eq!(code, 0, "{v}");
    s.status_works();
    let (code, doctor) = s.mdkg(&["doctor", "--json"]);
    assert_eq!(code, 0, "{doctor}");
}

/// Review of 785f4c4a: in ref mode a view, the primary checkout's or a
/// linked worktree's, takes ref-backed writes, and migrate, which writes
/// the checkout it runs in, refuses in either.
#[test]
fn ref_mode_views_take_ref_writes_and_refuse_migrate() {
    let s = Scratch::new();
    s.code("main");
    let (code, v) = s.mdkg(&["install", "--setup", "local", "--json"]);
    assert_eq!(code, 0, "{v}");
    let (code, v) = s.mdkg(&["init", "--work", "shared", "--json"]);
    assert_eq!(code, 0, "{v}");
    let (code, v) = s.mdkg(&["install", "--view", "--json"]);
    assert_eq!(code, 0, "{v}");
    let linked = s.0.join("linked");
    s.git(&[
        "worktree",
        "add",
        "-q",
        "-b",
        "side",
        &linked.to_string_lossy(),
    ]);
    let (code, v) = s.mdkg_in(&linked, &["install", "--view", "--json"]);
    assert_eq!(code, 0, "{v}");
    for view in [
        s.repo().join(".worktrees/graph"),
        linked.join(".worktrees/graph"),
    ] {
        assert!(view.join(".git").exists(), "{}", view.display());
        let before = s.git(&["rev-parse", GRAPH]);
        let (code, v) = s.mdkg_in(&view, &["new", "task", "from a view", "--json"]);
        assert_eq!(code, 0, "{}: {v}", view.display());
        // Shared work: the node is published on the graph ref.
        let path = v["data"]["node"]["path"].as_str().expect("path");
        assert_ne!(s.git(&["rev-parse", GRAPH]), before, "{}", view.display());
        let tree = s.git(&["ls-tree", "-r", "--name-only", GRAPH]);
        assert!(tree.lines().any(|l| l == path), "{path} on {GRAPH}: {tree}");
        let (code, v) = s.mdkg_in(
            &view,
            &["migrate", "--restore-history", "--dry-run", "--json"],
        );
        assert_ne!(code, 0, "{}: {v}", view.display());
        assert!(
            codes(&v).contains(&"graph.view".to_owned()),
            "{}: {v}",
            view.display()
        );
    }
    // A linked worktree whose `.worktrees` is a link to another folder.
    let elsewhere = s.0.join("elsewhere");
    fs::create_dir_all(&elsewhere).expect("elsewhere");
    let other = s.0.join("other");
    s.git(&[
        "worktree",
        "add",
        "-q",
        "-b",
        "other",
        &other.to_string_lossy(),
    ]);
    std::os::unix::fs::symlink(&elsewhere, other.join(".worktrees")).expect("link");
    let (code, v) = s.mdkg_in(&other, &["install", "--view", "--json"]);
    assert_eq!(code, 0, "{v}");
    let linked_view = other.join(".worktrees/graph");
    let (code, v) = s.mdkg_in(
        &linked_view,
        &["migrate", "--restore-history", "--dry-run", "--json"],
    );
    assert_ne!(code, 0, "{v}");
    assert!(codes(&v).contains(&"graph.view".to_owned()), "{v}");
    // Review #16130: the refusal names git's condition and a way out that
    // fits a detached code checkout too.
    let refusal = v["diagnostics"].to_string();
    assert!(
        refusal.contains("linked worktree with a detached HEAD")
            && refusal.contains("primary checkout"),
        "{v}"
    );
    // Review #16311: the hint keeps the graph view detached and offers
    // `git switch` only to a detached code worktree.
    assert!(
        refusal.contains("in the graph view (.worktrees/graph) leave it detached")
            && refusal.contains("only a detached code worktree should `git switch <branch>`"),
        "{v}"
    );
    // A ref setting that doesn't read: migrate still refuses in a view.
    s.git(&["config", "mdkg.graph.sync.pushDelay", "0"]);
    let (code, v) = s.mdkg_in(
        &linked.join(".worktrees/graph"),
        &["migrate", "--restore-history", "--dry-run", "--json"],
    );
    assert_ne!(code, 0, "{v}");
    assert!(codes(&v).contains(&"graph.view".to_owned()), "{v}");
    s.git(&["config", "--unset", "mdkg.graph.sync.pushDelay"]);
    // Review of 54e0b480: a damaged owning checkout drops out of the
    // worktree list, and its valid view still refuses.
    let owner = linked.join(".git");
    let record = fs::read(&owner).expect("record");
    fs::write(&owner, "damaged\n").expect("damage");
    let view = linked.join(".worktrees/graph");
    let before = s.git(&["rev-parse", GRAPH]);
    for mode in [&["--dry-run"][..], &["--apply", "sha256:00"]] {
        let args = [&["migrate", "--restore-history"][..], mode, &["--json"]].concat();
        let (code, v) = s.mdkg_in(&view, &args);
        assert_ne!(code, 0, "{mode:?}: {v}");
        assert!(
            codes(&v).contains(&"graph.view".to_owned()),
            "{mode:?}: {v}"
        );
    }
    assert_eq!(s.git(&["rev-parse", GRAPH]), before);
    fs::write(&owner, record).expect("repair");
    // Review #15801: a symlinked `.worktrees` and a damaged owner at once.
    let owner = other.join(".git");
    let record = fs::read(&owner).expect("record");
    fs::write(&owner, "damaged\n").expect("damage");
    let (code, v) = s.mdkg_in(
        &linked_view,
        &["migrate", "--restore-history", "--dry-run", "--json"],
    );
    assert_ne!(code, 0, "{v}");
    assert!(codes(&v).contains(&"graph.view".to_owned()), "{v}");
    fs::write(&owner, record).expect("repair");
    s.status_works();
}

/// #15804, #15983: a primary repository whose path ends in
/// `.worktrees/graph` isn't a view; migrate's view gate lets it through.
#[test]
fn a_primary_repository_at_a_graph_slot_path_is_not_a_view() {
    let s = Scratch::new();
    let top = s.repo().join(".worktrees/graph");
    fs::create_dir_all(&top).expect("dir");
    s.git_in(&top, &["init", "-q", "-b", "main"]);
    s.git_in(&top, &["config", "user.name", "Graph Test"]);
    s.git_in(&top, &["config", "user.email", "graph@example.invalid"]);
    s.git_in(&top, &["config", "mdkg.graph.ref", GRAPH]);
    fs::write(top.join("code.rs"), "fn main() {}\n").expect("code");
    s.git_in(&top, &["add", "code.rs"]);
    s.git_in(&top, &["commit", "-q", "-m", "code"]);
    let (code, v) = s.mdkg_in(&top, &["init", "--json"]);
    assert_eq!(code, 0, "{v}");
    let (code, v) = s.mdkg_in(
        &top,
        &["migrate", "--restore-history", "--dry-run", "--json"],
    );
    // Past the gate: migrate's own next check (a ref-mode root has no
    // checkout config) answers, not git or the view gate.
    assert_ne!(code, 0, "{v}");
    assert_eq!(codes(&v), vec!["config.missing".to_owned()], "{v}");
}

/// B2, B9: the documented move from checkout mode. `--setup` detaches the
/// graph checkout plain `install` made, drops its `.worktree` record, and
/// status works with the view, without it, and after a rollback.
#[test]
fn checkout_to_ref_migration_leaves_status_usable() {
    let s = Scratch::new();
    s.checkout_graph("main");
    let (code, v) = s.mdkg(&["install", "--json"]);
    assert_eq!(code, 0, "{v}");
    let view = s.repo().join(".worktrees/graph");
    assert_eq!(
        s.git_in(&view, &["symbolic-ref", "HEAD"]),
        GRAPH,
        "install puts the graph checkout on the graph branch"
    );
    assert!(!s.worktree_records().is_empty());
    s.status_works();

    let (code, v) = s.mdkg(&["install", "--setup", "local", "--dry-run", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert!(notes(&v).contains("detached from"), "{v}");
    assert_eq!(s.config("mdkg.graph.ref"), None);

    let (code, v) = s.mdkg(&["install", "--setup", "local", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(s.config("mdkg.graph.ref").as_deref(), Some(GRAPH));
    assert_eq!(s.worktree_records(), "", "no stale .worktree record");
    let out = s
        .isolated(Command::new("git").arg("-C").arg(&view))
        .args(["symbolic-ref", "-q", "HEAD"])
        .output()
        .expect("git");
    assert!(!out.status.success(), "the view is detached");
    s.status_works();
    let (code, v) = s.mdkg(&["new", "task", "after the move", "--json"]);
    assert_eq!(code, 0, "{v}");
    s.status_works();

    // Back to checkout mode: the view is reattached, and install
    // registers it again.
    let (code, v) = s.mdkg(&["install", "--checkout-mode", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(s.git_in(&view, &["symbolic-ref", "HEAD"]), GRAPH);
    let (code, v) = s.mdkg(&["install", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert!(!s.worktree_records().is_empty());
    s.status_works();

    // And forward again, then without the view.
    let (code, v) = s.mdkg(&["install", "--setup", "local", "--json"]);
    assert_eq!(code, 0, "{v}");
    s.git(&["worktree", "remove", ".worktrees/graph"]);
    s.status_works();
}

/// B9: uncommitted changes in the graph checkout would be lost to ref
/// mode, so the move refuses and writes nothing.
#[test]
fn migration_refuses_a_graph_checkout_with_changes() {
    let s = Scratch::new();
    s.checkout_graph("main");
    let (code, v) = s.mdkg(&["install", "--json"]);
    assert_eq!(code, 0, "{v}");
    let view = s.repo().join(".worktrees/graph");
    let refused = |what: &str| {
        let (code, v) = s.mdkg(&["install", "--setup", "local", "--json"]);
        assert_ne!(code, 0, "{what}: {v}");
        assert!(codes(&v).contains(&"graph.view".to_owned()), "{what}: {v}");
        assert_eq!(s.config("mdkg.graph.ref"), None, "{what}");
        assert_eq!(s.git_in(&view, &["symbolic-ref", "HEAD"]), GRAPH, "{what}");
        assert!(!s.worktree_records().is_empty(), "{what}");
    };
    // An untracked draft, even with status set to hide untracked files.
    s.git_in(&view, &["config", "status.showUntrackedFiles", "no"]);
    fs::write(view.join(".mdkg/scratch.md"), "edit\n").expect("edit");
    refused("untracked");
    fs::remove_file(view.join(".mdkg/scratch.md")).expect("rm");
    // An ignored draft.
    fs::write(view.join(".git-ignored-draft"), "draft\n").expect("draft");
    fs::write(s.repo().join(".git/info/exclude"), ".git-ignored-draft\n").expect("exclude");
    refused("ignored");
    fs::remove_file(view.join(".git-ignored-draft")).expect("rm");
    // A tracked edit hidden by assume-unchanged.
    let config = view.join(".mdkg/config.json");
    let mut text = fs::read_to_string(&config).expect("config");
    text.push('\n');
    fs::write(&config, text).expect("edit");
    s.git_in(
        &view,
        &["update-index", "--assume-unchanged", ".mdkg/config.json"],
    );
    refused("assume-unchanged");
}

/// Review #15360: the move drops only the moved graph's `.worktree`
/// record; another graph's (a 0.7.0 nested graph's) is its identity and
/// stays.
#[test]
fn migration_keeps_another_graph_s_worktree_record() {
    // A real nested graph's record stays: `Scratch::nested_moved`.
    let s = Scratch::new();
    s.nested_moved(false);
    // Records install can't resolve (two admins for one checkout) refuse
    // the move, with a repair hint, instead of being guessed at (#15983).
    let s = Scratch::new();
    s.checkout_graph("main");
    let (code, v) = s.mdkg(&["install", "--json"]);
    assert_eq!(code, 0, "{v}");
    let other = "mdkg.graph.01a19fa0-0000-7000-8000-000000000001.worktree";
    s.git(&["config", other, "nested"]);
    let (code, v) = s.mdkg(&["install", "--setup", "local", "--json"]);
    assert_ne!(code, 0, "{v}");
    assert!(codes(&v).contains(&"graph.view".to_owned()), "{v}");
    assert!(v.to_string().contains("doctor --repair"), "{v}");
    assert_eq!(s.config("mdkg.graph.ref"), None);
}

impl Scratch {
    /// Checkout mode with a 0.7.0 nested graph (workspace `w`, its own
    /// 0.7 identity) installed, then moved to the graph ref: returns the
    /// two `.worktree` records install made.
    fn nested_moved(&self, relative: bool) -> String {
        self.checkout_graph("main");
        if relative {
            self.git(&["config", "worktree.useRelativePaths", "true"]);
        }
        let w = self.repo().join("w");
        fs::create_dir_all(w.join(".mdkg")).expect("w");
        let path = self.repo().join(".mdkg/config.json");
        let mut config: Value =
            serde_json::from_str(&fs::read_to_string(&path).expect("config")).expect("json");
        config["workspaces"] = serde_json::json!({
            "root": {"path": ".", "enabled": true, "mdkg_dir": ".mdkg", "visibility": "private"},
            "w": {"path": "w", "enabled": true, "mdkg_dir": ".mdkg", "visibility": "private"}
        });
        fs::write(&path, config.to_string()).expect("w");
        let mut nested = config.clone();
        nested["graph_id"] = Value::from("01a19fa0-0000-7000-8000-000000000002");
        nested.as_object_mut().expect("object").remove("workspaces");
        fs::write(w.join(".mdkg/config.json"), nested.to_string()).expect("w");
        self.git(&["add", "."]);
        self.git(&["commit", "-q", "-m", "workspace"]);
        let (code, v) = self.mdkg(&["install", "--json"]);
        assert_eq!(code, 0, "{v}");
        let both = self.worktree_records();
        assert_eq!(both.lines().count(), 2, "{both}");
        let (code, v) = self.mdkg(&["install", "--setup", "local", "--json"]);
        assert_eq!(code, 0, "{v}");
        let kept = self.worktree_records();
        assert_eq!(kept.lines().count(), 1, "the nested record stays: {kept}");
        both
    }
}

fn record_ids(records: &str) -> Vec<String> {
    let mut ids: Vec<String> = records
        .lines()
        .map(|l| l.split(' ').next().unwrap_or("").to_owned())
        .collect();
    ids.sort();
    ids
}

/// Reviews #15478, #15944: after the view is removed, the way back to
/// checkout mode drops the records naming it and makes and registers a new
/// graph checkout itself, so status is right at once and both graphs are
/// recorded again.
#[test]
fn rollback_after_removing_the_view_records_both_graphs_again() {
    let s = Scratch::new();
    let both = s.nested_moved(false);
    s.git(&["worktree", "remove", "--force", ".worktrees/graph"]);
    s.status_works();
    let (code, v) = s.mdkg(&["install", "--checkout-mode", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert!(notes(&v).contains("the view) is gone"), "{v}");
    assert_eq!(record_ids(&s.worktree_records()), record_ids(&both));
    assert!(s.repo().join(".worktrees/graph/.git").exists(), "{v}");
    s.status_works();
    let (code, v) = s.mdkg(&["doctor", "--json"]);
    assert_eq!(code, 0, "{v}");
    let (code, v) = s.mdkg(&["install", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(record_ids(&s.worktree_records()), record_ids(&both));
}

/// A checkout-mode clone moved to ref mode, its view detached in the slot.
fn moved(s: &Scratch) {
    s.checkout_graph("main");
    let (code, v) = s.mdkg(&["install", "--json"]);
    assert_eq!(code, 0, "{v}");
    let (code, v) = s.mdkg(&["install", "--setup", "local", "--json"]);
    assert_eq!(code, 0, "{v}");
}

/// #16207: a short `mdkg.graph.ref` is the branch the reader names
/// (`refs/heads/mdkg-graph`): rollback after the view was removed
/// registers the replacement, and a dry run names it first.
#[test]
fn rollback_with_a_short_ref_name_registers_the_replacement() {
    let s = Scratch::new();
    moved(&s);
    s.git(&["config", "mdkg.graph.ref", "mdkg-graph"]);
    s.git(&["worktree", "remove", "--force", ".worktrees/graph"]);
    let (code, v) = s.mdkg(&["install", "--checkout-mode", "--dry-run", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert!(
        notes(&v).contains("would create graph worktree"),
        "the dry run names the replacement: {v}"
    );
    let (code, v) = s.mdkg(&["install", "--checkout-mode", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert!(s.repo().join(".worktrees/graph/.git").exists(), "{v}");
    assert!(!s.worktree_records().is_empty(), "{v}");
    s.status_works();
}

/// #16207: rollback run from a linked worktree finds the primary
/// checkout's view, as install places it, and reattaches it.
#[test]
fn rollback_from_a_linked_worktree_reattaches_the_primary_view() {
    let s = Scratch::new();
    moved(&s);
    let worker = s.0.join("worker");
    s.git(&[
        "worktree",
        "add",
        "-q",
        "-b",
        "worker",
        worker.to_str().expect("utf-8"),
    ]);
    let (code, v) = s.mdkg_in(&worker, &["install", "--checkout-mode", "--json"]);
    assert_eq!(code, 0, "{v}");
    let view = s.repo().join(".worktrees/graph");
    assert_eq!(s.git_in(&view, &["symbolic-ref", "HEAD"]), GRAPH, "{v}");
    assert!(!worker.join(".worktrees/graph").exists(), "{v}");
}

/// #16358: with a custom graph ref, rollback refuses up front, in ref mode,
/// with nothing changed (install makes a graph checkout only on
/// mdkg-graph), naming the rename route with the ref quoted for a shell;
/// a dry run refuses the same way, and ref mode keeps working.
#[test]
fn rollback_with_a_custom_ref_refuses_and_changes_nothing() {
    let s = Scratch::new();
    s.code("main");
    s.git(&["config", "mdkg.graph.ref", "refs/heads/team-graph"]);
    let (code, v) = s.mdkg(&["init", "--json"]);
    assert_eq!(code, 0, "{v}");
    let config = |s: &Scratch| s.git(&["config", "--list", "--local"]);
    let before = config(&s);
    for args in [
        &["install", "--checkout-mode", "--dry-run", "--json"][..],
        &["install", "--checkout-mode", "--json"],
    ] {
        let (code, v) = s.mdkg(args);
        assert_ne!(code, 0, "{args:?}: {v}");
        assert!(codes(&v).contains(&"graph.ref_invalid".to_owned()), "{v}");
        let hint = v["diagnostics"].to_string();
        assert!(
            hint.contains("git branch -m -- team-graph mdkg-graph"),
            "{v}"
        );
        assert_eq!(config(&s), before, "nothing changed");
    }
    s.status_works();
}

/// #16562: an option-looking graph ref (`refs/heads/-f`) is named after
/// `--` in the rename hint, so following it renames that ref and never
/// reads it as `git branch`'s force flag.
#[test]
fn rollback_hint_ends_options_before_an_option_looking_ref() {
    let s = Scratch::new();
    s.code("main");
    s.git(&["config", "mdkg.graph.ref", "refs/heads/-f"]);
    let (code, v) = s.mdkg(&["init", "--json"]);
    assert_eq!(code, 0, "{v}");
    let (code, v) = s.mdkg(&["install", "--checkout-mode", "--json"]);
    assert_ne!(code, 0, "{v}");
    let hint = v["diagnostics"].to_string();
    assert!(hint.contains("git branch -m -- -f mdkg-graph"), "{v}");
    let main = s.git(&["rev-parse", "refs/heads/main"]);
    s.git(&["branch", "-m", "--", "-f", "mdkg-graph"]);
    assert_eq!(
        s.git(&["rev-parse", "refs/heads/main"]),
        main,
        "main untouched"
    );
    s.git(&["rev-parse", "--verify", "refs/heads/mdkg-graph"]);
    assert!(
        s.git(&["for-each-ref", "--format=%(refname)", "refs/heads/-f"])
            .is_empty(),
        "the option-looking ref was the one renamed"
    );
}

/// Review #15944: a rollback interrupted after `mdkg.graph.ref` went is
/// finished by running it again; another graph's record (not committed on
/// the graph ref) is kept for explicit repair.
#[test]
fn an_interrupted_rollback_is_finished_by_a_rerun() {
    let s = Scratch::new();
    let both = s.nested_moved(false);
    s.git(&["worktree", "remove", "--force", ".worktrees/graph"]);
    // As if a rollback stopped right after unsetting the ref.
    s.git(&["config", "--unset", "mdkg.graph.ref"]);
    let (code, v) = s.mdkg(&["install", "--checkout-mode", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert!(notes(&v).contains("already in checkout mode"), "{v}");
    assert_eq!(record_ids(&s.worktree_records()), record_ids(&both));
    s.status_works();

    // Another graph's record, whose checkout is also gone, stays.
    let s = Scratch::new();
    s.nested_moved(false);
    let other = "mdkg.graph.01a19fa0-0000-7000-8000-000000000009.worktree";
    s.git(&["config", other, "elsewhere"]);
    s.git(&["worktree", "remove", "--force", ".worktrees/graph"]);
    let (code, v) = s.mdkg(&["install", "--checkout-mode", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(s.worktree_records(), format!("{other} elsewhere"), "{v}");
}

/// Review #15944: with relative worktree links (`worktree.useRelativePaths`)
/// a live graph checkout's record is read as git reads it, from the admin
/// folder, and kept.
#[test]
fn rollback_keeps_a_live_record_with_relative_links() {
    let s = Scratch::new();
    s.nested_moved(true);
    let kept = s.worktree_records();
    let (code, v) = s.mdkg(&["install", "--checkout-mode", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(s.worktree_records(), kept, "{v}");
    let (code, v) = s.mdkg(&["install", "--json"]);
    assert_eq!(code, 0, "{v}");
    s.status_works();
}

/// Review #15360: the move finds the graph checkout from a linked
/// worktree too, and never detaches a checkout reached through a link at
/// `.worktrees/graph`.
#[test]
fn migration_detaches_only_the_real_graph_slot() {
    let s = Scratch::new();
    s.checkout_graph("main");
    let (code, v) = s.mdkg(&["install", "--json"]);
    assert_eq!(code, 0, "{v}");
    let wt = s.0.join("linked");
    s.git(&[
        "worktree",
        "add",
        "-q",
        "-b",
        "linked",
        wt.to_str().expect("utf-8"),
    ]);
    let (code, v) = s.mdkg_in(&wt, &["install", "--setup", "local", "--json"]);
    assert_eq!(code, 0, "{v}");
    let view = s.repo().join(".worktrees/graph");
    let out = s
        .isolated(Command::new("git").arg("-C").arg(&view))
        .args(["symbolic-ref", "-q", "HEAD"])
        .output()
        .expect("git");
    assert!(!out.status.success(), "detached from the linked worktree");

    // A slot that is a link to the graph checkout moved elsewhere.
    let s = Scratch::new();
    s.checkout_graph("main");
    let (code, v) = s.mdkg(&["install", "--json"]);
    assert_eq!(code, 0, "{v}");
    let moved = s.0.join("moved-graph");
    s.git(&[
        "worktree",
        "move",
        ".worktrees/graph",
        moved.to_str().expect("utf-8"),
    ]);
    std::os::unix::fs::symlink(&moved, s.repo().join(".worktrees/graph")).expect("link");
    let (code, v) = s.mdkg(&["install", "--setup", "local", "--json"]);
    assert_ne!(code, 0, "{v}");
    assert!(codes(&v).contains(&"graph.view".to_owned()), "{v}");
    assert_eq!(s.git_in(&moved, &["symbolic-ref", "HEAD"]), GRAPH);
    assert_eq!(s.config("mdkg.graph.ref"), None);
}

/// Review #15480: a graph slot whose `.git` is redirected to the common
/// folder is never taken for the graph checkout: the move refuses, and
/// neither HEAD nor the config moves.
#[test]
fn a_redirected_graph_slot_is_never_detached() {
    let s = Scratch::new();
    s.checkout_graph("main");
    let (code, v) = s.mdkg(&["install", "--json"]);
    assert_eq!(code, 0, "{v}");
    let wt = s.0.join("linked");
    s.git(&[
        "worktree",
        "add",
        "-q",
        "-b",
        "linked",
        wt.to_str().expect("utf-8"),
    ]);
    let slot = s.repo().join(".worktrees/graph");
    let admin = s.repo().join(".git/worktrees/graph/HEAD");
    let before = fs::read_to_string(&admin).expect("admin HEAD");
    fs::write(
        slot.join(".git"),
        format!("gitdir: {}\n", s.repo().join(".git").display()),
    )
    .expect("redirect");
    let (code, v) = s.mdkg_in(&wt, &["install", "--setup", "local", "--json"]);
    assert_ne!(code, 0, "{v}");
    assert!(codes(&v).contains(&"graph.view".to_owned()), "{v}");
    assert_eq!(s.git(&["symbolic-ref", "HEAD"]), "refs/heads/main");
    assert_eq!(fs::read_to_string(&admin).expect("admin HEAD"), before);
    assert_eq!(s.config("mdkg.graph.ref"), None);
}

/// Review #15486: an already detached graph checkout with an edit still
/// refuses the move, and an edit git's stat cache calls clean (same size,
/// mtime put back, ctime not trusted) is found by content.
#[test]
fn hidden_graph_checkout_edits_refuse_the_move() {
    // Detached by hand, with an uncommitted edit.
    let s = Scratch::new();
    s.checkout_graph("main");
    let (code, v) = s.mdkg(&["install", "--json"]);
    assert_eq!(code, 0, "{v}");
    let view = s.repo().join(".worktrees/graph");
    s.git_in(&view, &["switch", "-q", "--detach"]);
    fs::write(view.join(".mdkg/draft.md"), "hand edit\n").expect("w");
    let (code, v) = s.mdkg(&["install", "--setup", "local", "--json"]);
    assert_ne!(code, 0, "{v}");
    assert!(codes(&v).contains(&"graph.view".to_owned()), "{v}");
    assert_eq!(s.config("mdkg.graph.ref"), None);

    // A same-size edit with its mtime put back.
    let s = Scratch::new();
    s.checkout_graph("main");
    let (code, v) = s.mdkg(&["install", "--json"]);
    assert_eq!(code, 0, "{v}");
    s.git(&["config", "core.trustctime", "false"]);
    let view = s.repo().join(".worktrees/graph");
    let file = view.join(".mdkg/config.json");
    let original = fs::read(&file).expect("config");
    // An mtime well before the index's, recorded, so the entry isn't racy
    // and git trusts its stat data.
    let mtime = std::time::SystemTime::now() - std::time::Duration::from_secs(100);
    let set = |when| {
        fs::File::options()
            .write(true)
            .open(&file)
            .and_then(|f| f.set_modified(when))
            .expect("mtime")
    };
    set(mtime);
    s.git_in(&view, &["update-index", "-q", "--refresh"]);
    assert_eq!(s.git_in(&view, &["status", "--porcelain"]), "");
    // Same size: the first quote swapped for an apostrophe.
    let mut edited = original.clone();
    let at = edited.iter().position(|b| *b == b'"').expect("quote");
    edited[at] = b'\'';
    assert_ne!(edited, original);
    fs::write(&file, &edited).expect("w");
    set(mtime);
    assert_eq!(
        s.git_in(&view, &["status", "--porcelain"]),
        "",
        "stat cache hides it"
    );
    let (code, v) = s.mdkg(&["install", "--setup", "local", "--json"]);
    assert_ne!(code, 0, "{v}");
    assert!(codes(&v).contains(&"graph.view".to_owned()), "{v}");
    assert_eq!(s.config("mdkg.graph.ref"), None);
}

/// Review #15486 minor: refreshing a view never overwrites an ignored
/// draft at a path the tip now has: the refresh refuses, and the draft and
/// the view's HEAD stay.
#[test]
fn view_refresh_keeps_an_ignored_draft() {
    let s = Scratch::new();
    s.code("main");
    let (code, v) = s.mdkg(&["install", "--setup", "local", "--json"]);
    assert_eq!(code, 0, "{v}");
    let (code, v) = s.mdkg(&["init", "--work", "shared", "--json"]);
    assert_eq!(code, 0, "{v}");
    let (code, v) = s.mdkg(&["install", "--view", "--json"]);
    assert_eq!(code, 0, "{v}");
    let view = s.repo().join(".worktrees/graph");
    let head = s.git_in(&view, &["rev-parse", "HEAD"]);
    let (code, v) = s.mdkg(&["new", "task", "published later", "--json"]);
    assert_eq!(code, 0, "{v}");
    let path = v["data"]["node"]["path"].as_str().expect("path").to_owned();
    let exclude = s.repo().join(".git/info/exclude");
    let mut rules = fs::read_to_string(&exclude).unwrap_or_default();
    rules.push_str(&format!("/{path}\n"));
    fs::write(&exclude, rules).expect("exclude");
    fs::create_dir_all(view.join(&path).parent().expect("parent")).expect("dir");
    fs::write(view.join(&path), "an ignored draft\n").expect("draft");
    let (code, v) = s.mdkg(&["install", "--view", "--json"]);
    assert_ne!(code, 0, "{v}");
    assert_eq!(
        fs::read_to_string(view.join(&path)).expect("draft"),
        "an ignored draft\n"
    );
    assert_eq!(s.git_in(&view, &["rev-parse", "HEAD"]), head);
}

/// B18: a fresh ref-mode `init` (the documented `git config`, then `init`)
/// adds `/.mdkg-edit/` to the common `info/exclude`, so doctor has no
/// graph.exclude warning; a dry run writes nothing.
#[test]
fn ref_mode_init_excludes_the_edit_folder() {
    let s = Scratch::new();
    s.code("main");
    s.git(&["config", "mdkg.graph.ref", GRAPH]);
    let exclude = s.repo().join(".git/info/exclude");
    let before = fs::read_to_string(&exclude).unwrap_or_default();
    let (code, v) = s.mdkg(&["init", "--dry-run", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(fs::read_to_string(&exclude).unwrap_or_default(), before);
    let (code, v) = s.mdkg(&["init", "--json"]);
    assert_eq!(code, 0, "{v}");
    let rules = fs::read_to_string(&exclude).expect("exclude");
    assert!(rules.lines().any(|l| l == "/.mdkg-edit/"), "{rules}");
    let (_, doctor) = s.mdkg(&["doctor", "--json"]);
    let check = doctor["data"]["checks"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|c| c["name"] == "graph.exclude")
        .unwrap_or_else(|| panic!("graph.exclude check: {doctor}"));
    assert_eq!(check["status"], "ok", "{doctor}");
}

/// Review #16128: the exclusion is read as bytes and never changes a rule
/// already there (a non-UTF-8 comment, a final `.env` with no newline);
/// a dry run names the write; a rerun after the line was lost (a crash
/// between publication and the exclusion) adds it again.
#[test]
fn ref_mode_init_keeps_existing_ignore_rules_and_finishes_on_rerun() {
    let s = Scratch::new();
    s.code("main");
    s.git(&["config", "mdkg.graph.ref", GRAPH]);
    let exclude = s.repo().join(".git/info/exclude");
    fs::create_dir_all(exclude.parent().expect("info")).expect("info");
    let original: &[u8] = b"# caf\xe9 notes\n.env";
    fs::write(&exclude, original).expect("exclude");
    fs::write(s.repo().join(".env"), "x\n").expect("env");
    s.git(&["check-ignore", "-q", ".env"]);
    let (code, v) = s.mdkg(&["init", "--dry-run", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(fs::read(&exclude).expect("exclude"), original);
    let notes = v["data"]["notes"].to_string();
    assert!(notes.contains("would add /.mdkg-edit/"), "{v}");
    let (code, v) = s.mdkg(&["init", "--json"]);
    assert_eq!(code, 0, "{v}");
    let bytes = fs::read(&exclude).expect("exclude");
    assert!(bytes.starts_with(original), "{bytes:?}");
    assert!(bytes.ends_with(b".env\n/.mdkg-edit/\n"), "{bytes:?}");
    s.git(&["check-ignore", "-q", ".env"]);
    // The line lost after publication: a rerun finishes it.
    fs::write(&exclude, original).expect("exclude");
    let (code, v) = s.mdkg(&["init", "--json"]);
    assert_eq!(code, 0, "{v}");
    let bytes = fs::read(&exclude).expect("exclude");
    assert!(bytes.ends_with(b".env\n/.mdkg-edit/\n"), "{bytes:?}");
    let (code, v) = s.mdkg(&["init", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(fs::read(&exclude).expect("exclude"), bytes, "once");

    // Review #16176: doctor reads the same non-UTF-8 file by bytes.
    let (_, doctor) = s.mdkg(&["doctor", "--json"]);
    assert_eq!(
        check_of(&doctor, "graph.exclude")["status"],
        "ok",
        "{doctor}"
    );

    // A rerun's text output says what it changes, not "nothing to do".
    fs::write(&exclude, original).expect("exclude");
    let out = s
        .isolated(Command::new(env!("CARGO_BIN_EXE_mdkg")).current_dir(s.repo()))
        .args(["init", "--dry-run"])
        .output()
        .expect("init");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("would add /.mdkg-edit/"), "{text}");
    assert!(!text.contains("nothing to do"), "{text}");

    // A write that fails leaves the file as it was, never a fragment.
    use std::os::unix::fs::PermissionsExt;
    let info = exclude.parent().expect("info").to_path_buf();
    fs::set_permissions(&info, fs::Permissions::from_mode(0o555)).expect("ro");
    let (code, v) = s.mdkg(&["init", "--json"]);
    fs::set_permissions(&info, fs::Permissions::from_mode(0o755)).expect("rw");
    assert_ne!(code, 0, "{v}");
    assert_eq!(fs::read(&exclude).expect("exclude"), original, "unchanged");
}

/// Review #16163: a sparse invoking checkout whose `.mdkg` files are
/// skip-worktree and absent moves (they aren't deletions), and a present
/// skip-worktree file that differs still refuses.
#[test]
fn a_sparse_checkout_moves_and_a_present_flagged_edit_refuses() {
    let s = Scratch::new();
    s.checkout_graph("main");
    let files = s.git(&["ls-files", ".mdkg"]);
    let files: Vec<&str> = files.lines().collect();
    let mut args = vec!["update-index", "--skip-worktree"];
    args.extend(&files);
    s.git(&args);
    // A present, flagged, edited file refuses.
    let config = s.repo().join(".mdkg/config.json");
    let mut text = fs::read_to_string(&config).expect("config");
    text.push('\n');
    fs::write(&config, &text).expect("edit");
    let (code, v) = s.mdkg(&["install", "--setup", "local", "--json"]);
    assert_ne!(code, 0, "{v}");
    assert!(codes(&v).contains(&"graph.uncommitted".to_owned()), "{v}");
    // Absent ones, as sparse checkout leaves them, move.
    fs::remove_dir_all(s.repo().join(".mdkg")).expect("rm");
    assert_eq!(s.git(&["status", "--porcelain"]), "", "sparse and clean");
    let (code, v) = s.mdkg(&["install", "--setup", "local", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(s.config("mdkg.graph.ref").as_deref(), Some(GRAPH));
}

/// Review #16232: a clean graph checkout that sparsely leaves out a path
/// (skip-worktree, absent) moves; a present skip-worktree file edited
/// there still refuses.
#[test]
fn a_sparse_graph_checkout_moves_and_its_present_flagged_edit_refuses() {
    let s = Scratch::new();
    s.checkout_graph("main");
    let (code, v) = s.mdkg(&["install", "--json"]);
    assert_eq!(code, 0, "{v}");
    let view = s.repo().join(".worktrees/graph");
    let listed = s.git_in(&view, &["ls-files"]);
    let files: Vec<&str> = listed.lines().collect();
    let mut args = vec!["update-index", "--skip-worktree"];
    args.extend(&files);
    s.git_in(&view, &args);
    // A present, flagged, edited graph file refuses.
    let config = view.join(".mdkg/config.json");
    let mut text = fs::read_to_string(&config).expect("config");
    text.push('\n');
    fs::write(&config, &text).expect("edit");
    let (code, v) = s.mdkg(&["install", "--setup", "local", "--json"]);
    assert_ne!(code, 0, "{v}");
    assert_eq!(s.config("mdkg.graph.ref"), None);
    // Absent, as sparse checkout leaves them: the move goes ahead.
    for f in &files {
        fs::remove_file(view.join(f)).ok();
    }
    assert_eq!(s.git_in(&view, &["status", "--porcelain"]), "", "sparse");
    let (code, v) = s.mdkg(&["install", "--setup", "local", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(s.config("mdkg.graph.ref").as_deref(), Some(GRAPH));
}

/// Review #16374: a sparse graph checkout leaving out a file whose name
/// isn't UTF-8 moves; the path's bytes reach git unchanged. Linux only:
/// macOS filesystems refuse such names.
#[cfg(target_os = "linux")]
#[test]
fn a_sparse_graph_checkout_with_a_non_utf8_name_moves() {
    use std::os::unix::ffi::OsStrExt;
    let s = Scratch::new();
    s.checkout_graph("main");
    let name = std::ffi::OsStr::from_bytes(b"legacy-caf\xe9.txt");
    fs::write(s.repo().join(name), "code\n").expect("legacy");
    s.git(&["add", "-A"]);
    s.git(&["commit", "-q", "-m", "legacy name"]);
    let (code, v) = s.mdkg(&["install", "--json"]);
    assert_eq!(code, 0, "{v}");
    let view = s.repo().join(".worktrees/graph");
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(&view)
        .args(["update-index", "--skip-worktree", "--"])
        .arg(name)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .status()
        .expect("git");
    assert!(status.success());
    fs::remove_file(view.join(name)).expect("rm");
    assert_eq!(s.git_in(&view, &["status", "--porcelain"]), "", "sparse");
    let (code, v) = s.mdkg(&["install", "--setup", "local", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(s.config("mdkg.graph.ref").as_deref(), Some(GRAPH));
}

/// Review #16163: a worktree git lists on the graph branch whose path
/// can't be resolved (a disconnected drive) isn't the identified
/// checkout, so the move refuses rather than leave the branch checked out.
#[test]
fn an_unresolvable_worktree_on_the_graph_branch_refuses_the_move() {
    let s = Scratch::new();
    s.checkout_graph("main");
    s.git(&["branch", "mdkg-graph"]);
    let away = s.0.join("away");
    s.git(&[
        "worktree",
        "add",
        "-q",
        away.to_str().expect("utf-8"),
        "mdkg-graph",
    ]);
    fs::remove_dir_all(&away).expect("rm");
    let (code, v) = s.mdkg(&["install", "--setup", "local", "--json"]);
    assert_ne!(code, 0, "{v}");
    assert!(codes(&v).contains(&"graph.view".to_owned()), "{v}");
    assert_eq!(s.config("mdkg.graph.ref"), None);
}

/// Review #16291: whether the edit folder is excluded is asked of git, so
/// a rule a later negation undoes gets the rule appended last; the file
/// keeps its mode, and a read-only one is refused, not replaced.
#[test]
fn ref_mode_init_asks_git_whether_the_edit_folder_is_ignored() {
    use std::os::unix::fs::PermissionsExt;
    let s = Scratch::new();
    s.code("main");
    s.git(&["config", "mdkg.graph.ref", GRAPH]);
    let exclude = s.repo().join(".git/info/exclude");
    fs::create_dir_all(exclude.parent().expect("info")).expect("info");
    let negated = "/.mdkg-edit/  \n!.*\n";
    fs::write(&exclude, negated).expect("exclude");
    fs::set_permissions(&exclude, fs::Permissions::from_mode(0o600)).expect("mode");
    let (code, v) = s.mdkg(&["init", "--json"]);
    assert_eq!(code, 0, "{v}");
    let text = fs::read_to_string(&exclude).expect("exclude");
    assert_eq!(text, format!("{negated}/.mdkg-edit/\n"), "appended last");
    s.git(&["check-ignore", "-q", "--no-index", ".mdkg-edit/probe"]);
    let mode = fs::metadata(&exclude).expect("meta").permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "mode kept");
    let (_, doctor) = s.mdkg(&["doctor", "--json"]);
    assert_eq!(
        check_of(&doctor, "graph.exclude")["status"],
        "ok",
        "{doctor}"
    );

    // A read-only file without an effective rule is refused, unchanged.
    fs::write(&exclude, negated).expect("exclude");
    fs::set_permissions(&exclude, fs::Permissions::from_mode(0o444)).expect("ro");
    let (code, v) = s.mdkg(&["init", "--json"]);
    fs::set_permissions(&exclude, fs::Permissions::from_mode(0o644)).expect("rw");
    assert_ne!(code, 0, "{v}");
    assert_eq!(fs::read_to_string(&exclude).expect("exclude"), negated);
    let (_, doctor) = s.mdkg(&["doctor", "--json"]);
    assert_eq!(
        check_of(&doctor, "graph.exclude")["status"],
        "warning",
        "{doctor}"
    );
}

/// Review #16499: a new info/exclude keeps the umask's mode (077 gives
/// 0600), never a fixed 0644.
#[test]
fn a_new_exclude_file_keeps_the_umask() {
    use std::os::unix::fs::PermissionsExt;
    let s = Scratch::new();
    s.code("main");
    s.git(&["config", "mdkg.graph.ref", GRAPH]);
    let exclude = s.repo().join(".git/info/exclude");
    fs::remove_file(&exclude).ok();
    let out = s
        .isolated(Command::new("/bin/sh").current_dir(s.repo()))
        .arg("-c")
        .arg("umask 077; exec \"$0\" init --json")
        .arg(env!("CARGO_BIN_EXE_mdkg"))
        .output()
        .expect("init");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let mode = fs::metadata(&exclude)
        .expect("exclude")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
}

/// Review #15961, one rule (#15983): the move inspects the registered
/// graph checkout wherever it is, and refuses anything the graph branch
/// lacks: an edit in a moved, detached checkout, a commit off the branch,
/// and an edit in the invoking checkout that git's stat cache hides.
#[test]
fn the_move_refuses_whatever_the_graph_branch_lacks() {
    // Moved with `git worktree move`, detached, with an edit.
    let s = Scratch::new();
    s.checkout_graph("main");
    let (code, v) = s.mdkg(&["install", "--json"]);
    assert_eq!(code, 0, "{v}");
    let moved = s.0.join("moved-graph");
    s.git(&[
        "worktree",
        "move",
        ".worktrees/graph",
        moved.to_str().expect("utf-8"),
    ]);
    s.git_in(&moved, &["switch", "-q", "--detach"]);
    fs::write(moved.join(".mdkg/draft.md"), "hand edit\n").expect("w");
    let (code, v) = s.mdkg(&["install", "--setup", "local", "--json"]);
    assert_ne!(code, 0, "{v}");
    assert!(codes(&v).contains(&"graph.view".to_owned()), "{v}");
    assert_eq!(s.config("mdkg.graph.ref"), None);

    // Detached, with a commit the graph branch doesn't hold.
    let s = Scratch::new();
    s.checkout_graph("main");
    let (code, v) = s.mdkg(&["install", "--json"]);
    assert_eq!(code, 0, "{v}");
    let view = s.repo().join(".worktrees/graph");
    s.git_in(&view, &["switch", "-q", "--detach"]);
    fs::write(view.join(".mdkg/extra.md"), "committed off the branch\n").expect("w");
    s.git_in(&view, &["add", ".mdkg/extra.md"]);
    s.git_in(&view, &["commit", "-q", "-m", "off the branch"]);
    let (code, v) = s.mdkg(&["install", "--setup", "local", "--json"]);
    assert_ne!(code, 0, "{v}");
    assert!(v.to_string().contains("doesn't hold"), "{v}");
    assert_eq!(s.config("mdkg.graph.ref"), None);

    // The invoking checkout: a same-size edit with its mtime put back.
    let s = Scratch::new();
    s.checkout_graph("main");
    s.git(&["config", "core.trustctime", "false"]);
    let file = s.repo().join(".mdkg/config.json");
    let original = fs::read(&file).expect("config");
    let mtime = std::time::SystemTime::now() - std::time::Duration::from_secs(100);
    let set = |when| {
        fs::File::options()
            .write(true)
            .open(&file)
            .and_then(|f| f.set_modified(when))
            .expect("mtime")
    };
    set(mtime);
    s.git(&["update-index", "-q", "--refresh"]);
    let mut edited = original.clone();
    let at = edited.iter().position(|b| *b == b'"').expect("quote");
    edited[at] = b'\'';
    fs::write(&file, &edited).expect("w");
    set(mtime);
    assert_eq!(s.git(&["status", "--porcelain"]), "", "stat cache hides it");
    let (code, v) = s.mdkg(&["install", "--setup", "local", "--json"]);
    assert_ne!(code, 0, "{v}");
    assert!(codes(&v).contains(&"graph.uncommitted".to_owned()), "{v}");
    assert_eq!(s.config("mdkg.graph.ref"), None);

    // The invoking checkout: an edit hidden by assume-unchanged (Terra
    // #16144: the docs say index flags don't hide an edit).
    let s = Scratch::new();
    s.checkout_graph("main");
    let file = s.repo().join(".mdkg/config.json");
    let mut text = fs::read_to_string(&file).expect("config");
    text.push('\n');
    fs::write(&file, text).expect("edit");
    s.git(&["update-index", "--assume-unchanged", ".mdkg/config.json"]);
    assert_eq!(s.git(&["status", "--porcelain"]), "", "the flag hides it");
    let (code, v) = s.mdkg(&["install", "--setup", "local", "--json"]);
    assert_ne!(code, 0, "{v}");
    assert!(codes(&v).contains(&"graph.uncommitted".to_owned()), "{v}");
    assert_eq!(s.config("mdkg.graph.ref"), None);
}
