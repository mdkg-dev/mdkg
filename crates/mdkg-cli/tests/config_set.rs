//! `config set` and `config unset` change the committed project config
//! through mdkg's own writer: a publish on the graph ref in ref mode, a
//! commit where the committed config is read in checkout mode. Disposable
//! repositories and homes only.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;

mod common;

const GRAPH: &str = "refs/heads/mdkg-graph";
const KEY: &str = "coordination.messages.ack_grace_days";

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
        let dir = common::temp::dir("mdkg-config-set");
        let dir = fs::canonicalize(dir).expect("canonical");
        fs::create_dir_all(dir.join("home")).expect("home");
        fs::create_dir_all(dir.join("repo")).expect("repo");
        Self(dir)
    }

    fn repo(&self) -> PathBuf {
        self.0.join("repo")
    }

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

    fn mdkg(&self, args: &[&str]) -> (i32, Value) {
        self.mdkg_with(args, &[])
    }

    fn mdkg_with(&self, args: &[&str], vars: &[(&str, &str)]) -> (i32, Value) {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_mdkg"));
        self.isolated(&mut cmd).current_dir(self.repo()).args(args);
        for (k, v) in vars {
            cmd.env(k, v);
        }
        let out = cmd.output().expect("mdkg runs");
        let json: Value = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
        (out.status.code().unwrap_or(-1), json)
    }

    fn repository(&self) {
        self.git(&["init", "-q", "-b", "trunk"]);
        self.git(&["config", "user.name", "Config Test"]);
        self.git(&["config", "user.email", "config@example.invalid"]);
        fs::write(self.repo().join("code.rs"), "fn main() {}\n").expect("code");
        self.git(&["add", "code.rs"]);
        self.git(&["commit", "-q", "-m", "code"]);
    }

    /// A ref-mode graph on the graph ref.
    fn ref_mode(&self) {
        self.repository();
        self.git(&["config", "mdkg.graph.ref", GRAPH]);
        let (code, out) = self.mdkg(&["init", "--work", "shared", "--json"]);
        assert_eq!(code, 0, "{out}");
    }

    /// A checkout-mode graph, committed on the checkout's branch.
    fn checkout_mode(&self) {
        self.repository();
        self.git(&["config", "mdkg.mainRef", "trunk"]);
        let (code, out) = self.mdkg(&["init", "--json"]);
        assert_eq!(code, 0, "{out}");
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", "graph"]);
    }

    fn committed(&self, rev: &str) -> Value {
        serde_json::from_str(&self.git(&["show", &format!("{rev}:.mdkg/config.json")]))
            .expect("committed config is JSON")
    }

    fn refs(&self) -> String {
        self.git(&["for-each-ref", "--format=%(refname) %(objectname)"])
    }

    /// Commits `body` at `rel` onto the graph ref outside mdkg, as a sync
    /// from another clone would.
    fn put_on_graph(&self, rel: &str, body: &str) {
        let blob = {
            let out = self
                .isolated(Command::new("git").arg("-C").arg(self.repo()))
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
        let tip = self.git(&["rev-parse", GRAPH]);
        let index = self.0.join("graph-index");
        let with_index = |args: &[&str]| {
            let out = self
                .isolated(Command::new("git").arg("-C").arg(self.repo()))
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
        let commit = self.git(&["commit-tree", &tree, "-p", &tip, "-m", "sync"]);
        self.git(&["update-ref", GRAPH, &commit, &tip]);
    }

    /// Every active lease, whoever owns it.
    fn all_leases(&self) -> usize {
        let (code, status) = self.mdkg(&["status", "--json"]);
        assert_eq!(code, 0, "{status}");
        status["data"]["leases"].as_array().map_or(0, Vec::len)
    }

    fn my_leases(&self) -> usize {
        let (code, status) = self.mdkg(&["status", "--mine", "--json"]);
        assert_eq!(code, 0, "{status}");
        status["data"]["leases"]
            .as_array()
            .map_or(0, |l| l.iter().filter(|l| l["mine"] == true).count())
    }
}

#[test]
fn ref_mode_set_and_unset_publish_on_the_graph_ref_and_touch_no_checkout() {
    let s = Scratch::new();
    s.ref_mode();
    let head = s.git(&["rev-parse", "HEAD"]);
    let before = s.git(&["rev-parse", GRAPH]);

    let (code, out) = s.mdkg(&["config", "set", KEY, "0", "--json"]);
    assert_eq!(code, 0, "{out}");
    let data = &out["data"];
    let tip = s.git(&["rev-parse", GRAPH]);
    assert_ne!(tip, before, "{out}");
    assert_eq!(s.git(&["rev-parse", &format!("{tip}^")]), before);
    assert_eq!(data["commit"], tip.as_str(), "{out}");
    assert_eq!(data["base"], before.as_str(), "{out}");
    assert_eq!(data["graph_ref"], GRAPH);
    assert_eq!(data["path"], ".mdkg/config.json");
    assert_eq!(data["pointer"], "/coordination/messages/ack_grace_days");
    assert_eq!(data["old"], Value::Null);
    assert_eq!(data["new"], 0);
    assert_eq!(data["changed"], true);
    assert!(
        data["diff"]
            .as_str()
            .unwrap()
            .contains("+      \"ack_grace_days\": 0"),
        "{out}"
    );
    let committed = s.committed(&tip);
    assert_eq!(committed["coordination"]["messages"]["ack_grace_days"], 0);
    assert!(committed["graph_id"].is_string(), "identity kept");
    let subject = s.git(&["log", "-1", "--format=%s", &tip]);
    assert!(subject.starts_with("mdkg: config set "), "{subject}");
    // No checkout file, no code commit, and the lease it took is gone.
    assert!(!s.repo().join(".mdkg").exists());
    assert_eq!(s.git(&["rev-parse", "HEAD"]), head);
    assert!(s.git(&["status", "--porcelain"]).is_empty());
    assert_eq!(s.my_leases(), 0);
    // Every command reads it at once.
    let (code, show) = s.mdkg(&["config", "show", "--json"]);
    assert_eq!(code, 0, "{show}");
    assert_eq!(
        show["data"]["values"]["coordination"]["messages"]["ack_grace_days"],
        0
    );
    assert_eq!(show["data"]["graph"]["commit"], tip.as_str());

    // The same value again commits nothing.
    let (code, same) = s.mdkg(&["config", "set", KEY, "0", "--json"]);
    assert_eq!(code, 0, "{same}");
    assert_eq!(same["data"]["changed"], false);
    assert_eq!(same["data"]["commit"], Value::Null);
    assert_eq!(s.git(&["rev-parse", GRAPH]), tip);

    let (code, out) = s.mdkg(&["config", "unset", KEY, "--json"]);
    assert_eq!(code, 0, "{out}");
    let unset = s.git(&["rev-parse", GRAPH]);
    assert_ne!(unset, tip);
    assert_eq!(out["data"]["old"], 0);
    assert_eq!(out["data"]["new"], Value::Null);
    assert!(
        s.committed(&unset)["coordination"]["messages"]
            .get("ack_grace_days")
            .is_none()
    );
    assert!(
        s.git(&["log", "-1", "--format=%s", &unset])
            .starts_with("mdkg: config unset ")
    );
    assert_eq!(s.my_leases(), 0);
}

#[test]
fn dry_run_shows_the_diff_and_changes_nothing_in_either_mode() {
    for checkout in [false, true] {
        let s = Scratch::new();
        if checkout {
            s.checkout_mode();
        } else {
            s.ref_mode();
        }
        let refs = s.refs();
        let (code, out) = s.mdkg(&["config", "set", KEY, "5", "--dry-run", "--json"]);
        assert_eq!(code, 0, "{out}");
        assert_eq!(out["data"]["dry_run"], true);
        assert_eq!(out["data"]["changed"], true);
        assert_eq!(out["data"]["commit"], Value::Null);
        assert_eq!(out["data"]["new"], 5);
        let diff = out["data"]["diff"].as_str().unwrap();
        assert!(diff.starts_with("--- a/.mdkg/config.json\n"), "{diff}");
        assert!(diff.contains("+      \"ack_grace_days\": 5"), "{diff}");
        assert_eq!(s.refs(), refs, "checkout mode {checkout}");
        assert!(s.git(&["status", "--porcelain"]).is_empty());
        if !checkout {
            assert_eq!(
                out["data"]["graph_ref"], GRAPH,
                "the graph ref it would publish on"
            );
        }
    }
}

#[test]
fn an_invalid_value_or_unknown_key_is_refused_and_changes_nothing() {
    for checkout in [false, true] {
        let s = Scratch::new();
        if checkout {
            s.checkout_mode();
        } else {
            s.ref_mode();
        }
        let refs = s.refs();
        for (args, code_name) in [
            (vec!["set", KEY, "3651"], "config.invalid"),
            (vec!["set", KEY, "-1"], "config.invalid"),
            (
                vec!["set", "coordination.leases.default", "advisory"],
                "config.invalid",
            ),
            (
                vec!["set", "coordination.mesages.ack_grace_days", "1"],
                "config.invalid",
            ),
            (vec!["unset", "coordination.mesages"], "config.invalid"),
            (vec!["set", "graph_id", "\"x\""], "config.invalid"),
            (vec!["set", KEY, "{"], "usage.bad_value"),
        ] {
            let mut argv = vec!["config"];
            argv.extend(&args);
            argv.push("--json");
            let (code, out) = s.mdkg(&argv);
            assert_ne!(code, 0, "{args:?}: {out}");
            assert_eq!(out["ok"], false, "{args:?}: {out}");
            assert_eq!(out["diagnostics"][0]["code"], code_name, "{args:?}: {out}");
            assert_eq!(s.refs(), refs, "{args:?} in checkout mode {checkout}");
            assert!(s.git(&["status", "--porcelain"]).is_empty(), "{args:?}");
            assert_eq!(s.my_leases(), 0, "{args:?}");
        }
    }
}

#[test]
fn checkout_mode_commits_the_config_where_it_is_read() {
    let s = Scratch::new();
    s.checkout_mode();
    let head = s.git(&["rev-parse", "HEAD"]);
    let (code, out) = s.mdkg(&["config", "set", KEY, "2", "--json"]);
    assert_eq!(code, 0, "{out}");
    let tip = s.git(&["rev-parse", "HEAD"]);
    assert_ne!(tip, head);
    assert_eq!(s.git(&["rev-parse", "HEAD^"]), head);
    assert_eq!(out["data"]["commit"], tip.as_str());
    assert_eq!(out["data"]["base"], head.as_str());
    assert_eq!(out["data"]["graph_ref"], Value::Null);
    assert_eq!(
        s.git(&["diff", "--name-only", &head, &tip]),
        ".mdkg/config.json"
    );
    assert_eq!(
        s.committed("HEAD")["coordination"]["messages"]["ack_grace_days"],
        2
    );
    assert!(s.git(&["status", "--porcelain"]).is_empty());
    assert_eq!(s.my_leases(), 0);

    // An uncommitted change to the config is never carried along.
    let path = s.repo().join(".mdkg/config.json");
    let edited = fs::read_to_string(&path)
        .unwrap()
        .replace("\"ack_grace_days\": 2", "\"ack_grace_days\": 4");
    assert!(edited.contains("\"ack_grace_days\": 4"), "{edited}");
    fs::write(&path, &edited).unwrap();
    let (code, out) = s.mdkg(&["config", "set", KEY, "3", "--json"]);
    assert_ne!(code, 0, "{out}");
    assert!(
        out["diagnostics"][0]["message"]
            .as_str()
            .unwrap()
            .contains("uncommitted"),
        "{out}"
    );
    assert_eq!(s.git(&["rev-parse", "HEAD"]), tip);
    assert_eq!(fs::read_to_string(&path).unwrap(), edited);
    assert_eq!(s.my_leases(), 0);

    // Nor is a staged one whose working file was put back to HEAD.
    s.git(&["add", ".mdkg/config.json"]);
    s.git(&[
        "restore",
        "--source=HEAD",
        "--worktree",
        ".mdkg/config.json",
    ]);
    let staged = s.git(&["diff", "--cached", "--name-only"]);
    assert_eq!(staged, ".mdkg/config.json");
    let (code, out) = s.mdkg(&["config", "set", KEY, "3", "--json"]);
    assert_ne!(code, 0, "{out}");
    assert!(
        out["diagnostics"][0]["message"]
            .as_str()
            .unwrap()
            .contains("uncommitted"),
        "{out}"
    );
    assert_eq!(s.git(&["rev-parse", "HEAD"]), tip);
    assert_eq!(s.git(&["show", ":.mdkg/config.json"]), edited.trim_end());
    assert_eq!(s.my_leases(), 0);

    // Nor one git reports clean because the file is marked
    // assume-unchanged.
    s.git(&["reset", "-q", "--", ".mdkg/config.json"]);
    s.git(&["update-index", "--assume-unchanged", ".mdkg/config.json"]);
    fs::write(&path, &edited).unwrap();
    assert!(s.git(&["status", "--porcelain"]).is_empty());
    let (code, out) = s.mdkg(&["config", "set", KEY, "4", "--json"]);
    assert_ne!(code, 0, "{out}");
    assert!(
        out["diagnostics"][0]["message"]
            .as_str()
            .unwrap()
            .contains("uncommitted"),
        "{out}"
    );
    assert_eq!(s.git(&["rev-parse", "HEAD"]), tip);
    assert_eq!(fs::read_to_string(&path).unwrap(), edited);
    assert_eq!(s.my_leases(), 0);

    // A clean file under either index flag is refused too: git wouldn't
    // stage the change, so a commit of it would commit nothing.
    s.git(&["checkout", "--", ".mdkg/config.json"]);
    fs::write(&path, s.git(&["show", "HEAD:.mdkg/config.json"]) + "\n").unwrap();
    for (on, off) in [
        ("--assume-unchanged", "--no-assume-unchanged"),
        ("--skip-worktree", "--no-skip-worktree"),
    ] {
        s.git(&["update-index", "--no-assume-unchanged", ".mdkg/config.json"]);
        s.git(&["update-index", on, ".mdkg/config.json"]);
        let (code, out) = s.mdkg(&["config", "set", KEY, "5", "--json"]);
        assert_ne!(code, 0, "{on}: {out}");
        let hint = out["diagnostics"][0]["hint"].as_str().unwrap_or_default();
        assert!(
            hint.contains(&format!("`git update-index {off} -- .mdkg/config.json`")),
            "{on}: {out}"
        );
        assert_eq!(s.git(&["rev-parse", "HEAD"]), tip, "{on}");
        assert_eq!(s.my_leases(), 0, "{on}");
        s.git(&["update-index", off, ".mdkg/config.json"]);
    }
    assert!(s.git(&["status", "--porcelain"]).is_empty());

    // With both flags set, following each hint in turn clears them and
    // the retry goes through.
    s.git(&["update-index", "--assume-unchanged", ".mdkg/config.json"]);
    s.git(&["update-index", "--skip-worktree", ".mdkg/config.json"]);
    let mut cleared: Vec<String> = Vec::new();
    let out = loop {
        let (code, out) = s.mdkg(&["config", "set", KEY, "5", "--json"]);
        if code == 0 {
            break out;
        }
        // Run the hint's command as shown, from the repository's top.
        let hint = out["diagnostics"][0]["hint"].as_str().unwrap_or_default();
        let shown = hint
            .split('`')
            .nth(1)
            .unwrap_or_else(|| panic!("a hint gives a command: {out}"));
        let words: Vec<&str> = shown.split_whitespace().collect();
        assert_eq!(words[..2], ["git", "update-index"], "{out}");
        assert!(
            !cleared.iter().any(|c| c == words[2]) && cleared.len() < 2,
            "{out}"
        );
        cleared.push(words[2].to_string());
        s.git(&words[1..]);
    };
    assert_eq!(cleared.len(), 2, "both flags were named: {cleared:?}");
    assert_eq!(out["data"]["changed"], true, "{out}");
}

/// A write outside mdkg moves the graph ref while a `config set` is
/// recorded but not yet published (the process stopped there). The
/// replay loses its compare-and-swap, rebuilds the change on the newer
/// tip and publishes it, keeping both changes.
#[test]
fn a_change_that_lost_its_compare_and_swap_is_rebuilt_on_the_newer_tip() {
    let s = Scratch::new();
    s.ref_mode();
    let before = s.git(&["rev-parse", GRAPH]);
    let (code, out) = s.mdkg_with(
        &["config", "set", KEY, "1", "--json"],
        &[("MDKG_CRASH_AFTER", "recorded")],
    );
    assert_eq!(code, 86, "the process stops after recording: {out}");
    assert_eq!(s.git(&["rev-parse", GRAPH]), before, "not published yet");
    // Another writer moves the ref meanwhile.
    s.put_on_graph(".mdkg/notes.md", "from another clone\n");
    let moved = s.git(&["rev-parse", GRAPH]);
    // The next graph command replays the pending intent.
    let (code, renew) = s.mdkg(&["renew", "--json"]);
    assert_eq!(code, 0, "{renew}");
    let tip = s.git(&["rev-parse", GRAPH]);
    assert_ne!(tip, moved, "the change is published: {renew}");
    assert_eq!(
        s.git(&["rev-parse", &format!("{tip}^")]),
        moved,
        "rebuilt on the newer tip"
    );
    assert_eq!(
        s.committed(&tip)["coordination"]["messages"]["ack_grace_days"],
        1
    );
    assert_eq!(
        s.git(&["show", &format!("{tip}:.mdkg/notes.md")]),
        "from another clone"
    );
    let (code, show) = s.mdkg(&["config", "show", "--json"]);
    assert_eq!(code, 0, "{show}");
    assert_eq!(
        show["data"]["values"]["coordination"]["messages"]["ack_grace_days"],
        1
    );
}

/// When the other writer changed the config itself, the recorded change
/// can't be rebuilt over it: it's reported stale, and the ref keeps the
/// other change.
#[test]
fn a_change_whose_config_moved_underneath_is_never_published_over_it() {
    let s = Scratch::new();
    s.ref_mode();
    let (code, out) = s.mdkg_with(
        &["config", "set", KEY, "1", "--json"],
        &[("MDKG_CRASH_AFTER", "recorded")],
    );
    assert_eq!(code, 86, "{out}");
    let mut other = s.committed(GRAPH);
    other["coordination"] = serde_json::json!({"messages": {"ack_grace_days": 9}});
    s.put_on_graph(
        ".mdkg/config.json",
        &format!("{}\n", serde_json::to_string_pretty(&other).unwrap()),
    );
    let moved = s.git(&["rev-parse", GRAPH]);
    let (_, renew) = s.mdkg(&["renew", "--json"]);
    assert_eq!(s.git(&["rev-parse", GRAPH]), moved, "{renew}");
    assert_eq!(
        s.committed(GRAPH)["coordination"]["messages"]["ack_grace_days"],
        9
    );
}

/// Config dispatch runs before the CLI's actor gate, so the setter refuses a
/// human, role, turn or mdkg actor itself, before anything is written.
#[test]
fn a_reserved_actor_is_refused_and_changes_nothing() {
    for checkout in [false, true] {
        let s = Scratch::new();
        if checkout {
            s.checkout_mode();
        } else {
            s.ref_mode();
        }
        let refs = s.refs();
        for actor in ["human:nick", "role:ops", "run:lane", "mdkg:launcher"] {
            let (code, out) = s.mdkg(&["config", "set", KEY, "2", "--as", actor, "--json"]);
            assert_ne!(code, 0, "{actor}: {out}");
            let code_name = out["diagnostics"][0]["code"].as_str().unwrap_or_default();
            assert!(
                matches!(
                    code_name,
                    "msg.human_actor" | "msg.reserved_actor" | "msg.bad_id"
                ),
                "{actor}: {out}"
            );
            assert_eq!(s.refs(), refs, "{actor} in checkout mode {checkout}");
            assert!(s.git(&["status", "--porcelain"]).is_empty(), "{actor}");
            // No lease for anyone, the refused actor included.
            assert_eq!(s.all_leases(), 0, "{actor} in checkout mode {checkout}");
        }
    }
}

/// Checkout mode with a registered graph checkout: a sibling worktree at
/// the same commit isn't where the config is read, so the change is
/// refused there and made in the registered checkout.
#[test]
fn a_sibling_at_the_same_commit_is_not_the_registered_graph_checkout() {
    let s = Scratch::new();
    s.checkout_mode();
    let graph = s.repo().join(".worktrees/graph");
    let sibling = s.repo().join(".worktrees/sibling");
    let path = |p: &PathBuf| p.to_str().expect("utf-8").to_owned();
    s.git(&[
        "worktree",
        "add",
        "-q",
        "-b",
        "mdkg-graph",
        &path(&graph),
        "trunk",
    ]);
    let (code, out) = s.mdkg(&["install", "--json"]);
    assert_eq!(code, 0, "{out}");
    s.git(&[
        "worktree",
        "add",
        "-q",
        "--detach",
        &path(&sibling),
        "mdkg-graph",
    ]);
    let refs = s.refs();
    let (code, out) = s.mdkg(&["-r", &path(&sibling), "config", "set", KEY, "2", "--json"]);
    assert_ne!(code, 0, "{out}");
    assert!(
        out["diagnostics"][0]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("not this checkout"),
        "{out}"
    );
    assert_eq!(s.refs(), refs);
    let (code, out) = s.mdkg(&["-r", &path(&graph), "config", "set", KEY, "2", "--json"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        s.committed("mdkg-graph")["coordination"]["messages"]["ack_grace_days"],
        2
    );
}

/// A change to the value the key already has commits nothing and leaves a
/// lease the actor held before the command in place.
#[test]
fn a_held_lease_is_kept_by_a_no_op_and_used_up_by_a_change() {
    for checkout in [false, true] {
        let s = Scratch::new();
        if checkout {
            s.checkout_mode();
        } else {
            s.ref_mode();
        }
        let (code, out) = s.mdkg(&["config", "set", KEY, "2", "--json"]);
        assert_eq!(code, 0, "{out}");
        let (code, out) = s.mdkg(&["claim", ".mdkg/config.json", "--json"]);
        assert_eq!(code, 0, "{out}");
        assert_eq!(s.my_leases(), 1);
        let (code, out) = s.mdkg(&["config", "set", KEY, "2", "--json"]);
        assert_eq!(code, 0, "{out}");
        assert_eq!(out["data"]["changed"], false, "{out}");
        assert_eq!(s.my_leases(), 1, "checkout mode {checkout}");
        // A committed change uses the held lease up, as `done` does.
        let (code, out) = s.mdkg(&["config", "set", KEY, "3", "--json"]);
        assert_eq!(code, 0, "{out}");
        assert_eq!(out["data"]["changed"], true, "{out}");
        assert_eq!(s.my_leases(), 0, "checkout mode {checkout}");
    }
}

/// A checkout-mode change that fails after git staged the config leaves
/// nothing staged, the working file as committed, and HEAD where it was.
#[test]
fn a_checkout_change_that_fails_after_staging_leaves_nothing_staged() {
    let s = Scratch::new();
    s.checkout_mode();
    let head = s.git(&["rev-parse", "HEAD"]);
    let path = s.repo().join(".mdkg/config.json");
    let before = fs::read(&path).unwrap();
    let (code, out) = s.mdkg_with(
        &["config", "set", KEY, "6", "--json"],
        &[("MDKG_FAIL_AFTER", "config-staged")],
    );
    assert_ne!(code, 0, "{out}");
    assert!(out.to_string().contains("injected"), "{out}");
    assert_eq!(s.git(&["rev-parse", "HEAD"]), head);
    assert!(
        s.git(&["diff", "--cached", "--name-only"]).is_empty(),
        "nothing staged"
    );
    assert_eq!(fs::read(&path).unwrap(), before);
    assert!(s.git(&["status", "--porcelain"]).is_empty());
    assert_eq!(s.my_leases(), 0);
    // The next change goes through.
    let (code, out) = s.mdkg(&["config", "set", KEY, "6", "--json"]);
    assert_eq!(code, 0, "{out}");
}

/// When unstaging fails too (git can't write the index), the working file
/// is still put back and both failures are reported.
#[test]
fn a_failed_unstage_still_restores_the_working_config() {
    let s = Scratch::new();
    s.checkout_mode();
    let head = s.git(&["rev-parse", "HEAD"]);
    let path = s.repo().join(".mdkg/config.json");
    let before = fs::read(&path).unwrap();
    let (code, out) = s.mdkg_with(
        &["config", "set", KEY, "6", "--json"],
        &[("MDKG_FAIL_AFTER", "config-staged-locked")],
    );
    assert_ne!(code, 0, "{out}");
    let message = out["diagnostics"][0]["message"]
        .as_str()
        .unwrap_or_default();
    assert!(message.contains("injected"), "{out}");
    assert!(message.contains("unstaging the config failed"), "{out}");
    assert!(!message.contains("restoring the config failed"), "{out}");
    assert_eq!(fs::read(&path).unwrap(), before, "working file restored");
    assert_eq!(s.git(&["rev-parse", "HEAD"]), head);
    // The index still holds the new bytes until the lock is cleared.
    let lock = s.git(&[
        "rev-parse",
        "--path-format=absolute",
        "--git-path",
        "index.lock",
    ]);
    fs::remove_file(lock.trim()).unwrap();
    s.git(&["reset", "--quiet", "HEAD", "--", ".mdkg/config.json"]);
    assert!(s.git(&["status", "--porcelain"]).is_empty());
}

/// A lease the command took and couldn't release is named in the error,
/// since it may still be held.
#[test]
fn a_lease_that_fails_to_release_is_named() {
    let s = Scratch::new();
    s.checkout_mode();
    let (code, out) = s.mdkg(&["config", "set", KEY, "2", "--json"]);
    assert_eq!(code, 0, "{out}");
    // The same value again: a no-op that releases the lease it took.
    let (code, out) = s.mdkg_with(
        &["config", "set", KEY, "2", "--json"],
        &[("MDKG_FAIL_AFTER", "config-release")],
    );
    assert_ne!(code, 0, "{out}");
    let message = out["diagnostics"][0]["message"]
        .as_str()
        .unwrap_or_default();
    assert!(message.contains("may still be held"), "{out}");
    assert!(message.contains("lease-"), "{out}");
    assert!(message.contains("until it expires"), "{out}");
    // No release command: one by path would reach a lease authority's
    // lease, and a fixed binary name can be the wrong one.
    assert!(!message.contains("release ."), "{out}");
    assert_eq!(s.my_leases(), 1);
    // The id named is the lease `status` still lists.
    let (_, status) = s.mdkg(&["status", "--mine", "--json"]);
    let id = status["data"]["leases"]
        .as_array()
        .and_then(|l| l.iter().find(|l| l["mine"] == true))
        .and_then(|l| l["id"].as_str())
        .unwrap_or_default()
        .to_owned();
    assert!(!id.is_empty() && message.contains(&id), "{status} / {out}");
}

/// An agent policy the supervisor would refuse is refused at set time, in
/// both modes, and changes nothing.
#[test]
fn an_agent_policy_value_is_refused_at_set_time() {
    for checkout in [false, true] {
        let s = Scratch::new();
        if checkout {
            s.checkout_mode();
        } else {
            s.ref_mode();
        }
        let refs = s.refs();
        let (code, out) = s.mdkg(&[
            "config",
            "set",
            "agents.defaults.env.inherit",
            "sometimes",
            "--json",
        ]);
        assert_ne!(code, 0, "{out}");
        assert_eq!(out["diagnostics"][0]["code"], "config.invalid", "{out}");
        assert!(out.to_string().contains("env.inherit"), "{out}");
        assert_eq!(s.refs(), refs, "checkout mode {checkout}");
        assert!(s.git(&["status", "--porcelain"]).is_empty());
        assert_eq!(s.my_leases(), 0);
    }
}

/// The committed layer's `agents` and each entry in it must be objects,
/// as the runtime's authority check requires, so a `null` or other
/// non-object is refused at set time in both modes. A `null` in the user
/// and local layers still hides an inherited agent, so with both present
/// a valid change still lands (orchestrator ruling on #21055).
#[test]
fn a_committed_agent_that_isnt_an_object_is_refused_at_set_time() {
    for checkout in [false, true] {
        let s = Scratch::new();
        if checkout {
            s.checkout_mode();
        } else {
            s.ref_mode();
        }
        let refs = s.refs();
        for (key, value, reason) in [
            (
                "agents.ghost",
                "null",
                "agent ghost: definition must be an object",
            ),
            (
                "agents.ghost",
                "\"worker\"",
                "agent ghost: definition must be an object",
            ),
            ("agents", "[]", "agents must be an object"),
        ] {
            let (code, out) = s.mdkg(&["config", "set", key, value, "--json"]);
            assert_ne!(code, 0, "{key} {value}: {out}");
            assert_eq!(out["diagnostics"][0]["code"], "config.invalid", "{out}");
            assert!(out.to_string().contains(reason), "{out}");
            assert_eq!(s.refs(), refs, "checkout mode {checkout}");
            assert!(s.git(&["status", "--porcelain"]).is_empty());
            assert_eq!(s.my_leases(), 0);
        }
        // The user and local layers may still hide an agent with `null`.
        let user = s.0.join("home/.config/mdkg/config.json");
        fs::create_dir_all(user.parent().unwrap()).unwrap();
        fs::write(&user, r#"{"agents":{"ghost":null}}"#).unwrap();
        let local = s.repo().join(".git/mdkg/config.local.json");
        fs::create_dir_all(local.parent().unwrap()).unwrap();
        fs::write(&local, r#"{"agents":{"other":null}}"#).unwrap();
        let (code, out) = s.mdkg(&["config", "set", KEY, "0", "--json"]);
        assert_eq!(code, 0, "checkout mode {checkout}: {out}");
        let (code, show) = s.mdkg(&["config", "show", "--json"]);
        assert_eq!(code, 0, "{show}");
    }
}

/// `work.location` and its alias are refused in both modes, changing
/// nothing; `work localize` moves work with its nodes.
#[test]
fn the_work_location_is_refused_and_changes_nothing() {
    for checkout in [false, true] {
        let s = Scratch::new();
        if checkout {
            s.checkout_mode();
        } else {
            s.ref_mode();
        }
        let refs = s.refs();
        for args in [
            &["config", "set", "work.location", "git", "--json"][..],
            &["config", "unset", "work.committed", "--json"],
            &["config", "unset", "work", "--json"],
        ] {
            let (code, out) = s.mdkg(args);
            assert_ne!(code, 0, "{args:?}: {out}");
            let hint = out["diagnostics"][0]["hint"].as_str().unwrap_or_default();
            // `work localize` runs only in a checkout and only out of git;
            // the hint says so in both modes.
            assert!(hint.contains("work localize"), "{args:?}: {out}");
            assert!(
                hint.contains("back into git") && hint.contains("graph ref"),
                "{out}"
            );
        }
        assert_eq!(s.refs(), refs, "checkout mode {checkout}");
        assert!(s.git(&["status", "--porcelain"]).is_empty());
        assert_eq!(s.my_leases(), 0);
    }
}

/// In ref mode, a `.mdkg/config.json` the checkout still commits (a
/// stale copy from before the move to a graph ref, or one a merged pull
/// request brought in) is never written: the change goes only to the
/// graph ref.
#[test]
fn ref_mode_never_writes_a_config_the_checkout_commits() {
    let s = Scratch::new();
    s.ref_mode();
    let stale = s.git(&["show", &format!("{GRAPH}:.mdkg/config.json")]) + "\n";
    fs::create_dir_all(s.repo().join(".mdkg")).unwrap();
    fs::write(s.repo().join(".mdkg/config.json"), &stale).unwrap();
    s.git(&["add", ".mdkg/config.json"]);
    s.git(&["commit", "-q", "-m", "stale config"]);
    let head = s.git(&["rev-parse", "HEAD"]);
    // The graph then moves on, so the checkout's copy is stale.
    let hold = "coordination.messages.human_hold_days";
    let (code, out) = s.mdkg(&["config", "set", hold, "9", "--json"]);
    assert_eq!(code, 0, "{out}");
    let before = s.git(&["rev-parse", GRAPH]);

    let (code, out) = s.mdkg(&["config", "set", KEY, "4", "--json"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        out["data"]["base"],
        before.as_str(),
        "read from the graph ref: {out}"
    );
    let tip = s.git(&["rev-parse", GRAPH]);
    assert_ne!(tip, before, "{out}");
    assert_eq!(out["data"]["commit"], tip.as_str(), "{out}");
    let messages = &s.committed(&tip)["coordination"]["messages"];
    assert_eq!(messages["ack_grace_days"], 4);
    assert_eq!(
        messages["human_hold_days"], 9,
        "the graph's newer setting is kept"
    );
    assert_eq!(s.git(&["rev-parse", "HEAD"]), head);
    assert_eq!(
        fs::read_to_string(s.repo().join(".mdkg/config.json")).unwrap(),
        stale
    );
    assert!(s.git(&["status", "--porcelain"]).is_empty());
}

/// `--json` hides a value for its key's name or a parent's, in `old`,
/// `new` and `diff`, for set and unset, in both modes.
#[test]
fn json_output_hides_values_of_sensitive_keys() {
    for checkout in [false, true] {
        let s = Scratch::new();
        if checkout {
            s.checkout_mode();
        } else {
            s.ref_mode();
        }
        for key in ["customization.password", "customization.secrets.x"] {
            for args in [
                vec!["config", "set", key, "\"plain words\"", "--json"],
                vec!["config", "set", key, "\"other words\"", "--json"],
                vec!["config", "unset", key, "--json"],
            ] {
                let (code, out) = s.mdkg(&args);
                assert_eq!(code, 0, "{args:?}: {out}");
                assert_eq!(out["data"]["changed"], true, "{args:?}: {out}");
                let text = out.to_string();
                assert!(
                    !text.contains("words"),
                    "{args:?} in checkout mode {checkout}: {out}"
                );
                for side in ["old", "new"] {
                    let v = &out["data"][side];
                    assert!(v.is_null() || *v == "[redacted]", "{side} {args:?}: {out}");
                }
            }
        }
    }
}

/// A checkout-mode write that fails after its rename (as a failed folder
/// sync would) puts the committed bytes back: the config, HEAD and the
/// index are as they were, and the lease the command took is released.
#[test]
fn a_write_failing_after_its_rename_restores_the_config() {
    let s = Scratch::new();
    s.checkout_mode();
    let path = s.repo().join(".mdkg/config.json");
    let before = fs::read(&path).unwrap();
    let head = s.git(&["rev-parse", "HEAD"]);
    let (code, out) = s.mdkg_with(
        &["config", "set", KEY, "2", "--json"],
        &[("MDKG_FAIL_AFTER", "config-renamed")],
    );
    assert_ne!(code, 0, "{out}");
    assert_eq!(out["diagnostics"][0]["code"], "io.write_failed", "{out}");
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(s.git(&["rev-parse", "HEAD"]), head);
    assert!(s.git(&["status", "--porcelain"]).is_empty());
    assert_eq!(s.all_leases(), 0);
}

/// A clean checkout whose config git converted to CRLF (`core.autocrlf`)
/// isn't an uncommitted change: dry run and set both work.
#[test]
fn a_clean_crlf_checkout_of_the_config_is_accepted() {
    let s = Scratch::new();
    s.checkout_mode();
    s.git(&["config", "core.autocrlf", "true"]);
    let path = s.repo().join(".mdkg/config.json");
    fs::remove_file(&path).unwrap();
    s.git(&["checkout", "--", ".mdkg/config.json"]);
    assert!(fs::read(&path).unwrap().windows(2).any(|w| w == b"\r\n"));
    assert!(s.git(&["status", "--porcelain"]).is_empty());
    let (code, out) = s.mdkg(&["config", "set", KEY, "2", "--dry-run", "--json"]);
    assert_eq!(code, 0, "{out}");
    let (code, out) = s.mdkg(&["config", "set", KEY, "2", "--json"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        s.committed("HEAD")["coordination"]["messages"]["ack_grace_days"],
        2
    );
}

/// With a lease authority configured, a ref-mode change needs the
/// authority's hold; a no-op leaves that lease with the actor, and a
/// committed change releases it there.
#[test]
fn a_no_op_keeps_the_authority_lease_and_a_change_releases_it() {
    use std::os::unix::fs::PermissionsExt;
    let s = Scratch::new();
    s.ref_mode();
    let common = fs::canonicalize(s.repo().join(".git")).expect("common dir");
    let common = common.to_string_lossy().into_owned();
    let config = s.0.join("home/.config");
    let folder = config.join("mdkg/lease-authority");
    fs::create_dir_all(&folder).unwrap();
    for f in [config.join("mdkg"), folder.clone()] {
        fs::set_permissions(&f, fs::Permissions::from_mode(0o700)).unwrap();
    }
    // The script finds its files beside itself, so no path is
    // interpolated into it, and answers each command with its own schema.
    let script = s.0.join("authority.sh");
    fs::write(
        &script,
        "#!/bin/sh\nd=$(dirname \"$0\")\necho \"$@\" >> \"$d/args.log\"\n\
         cat \"$d/$1.json\"\n",
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let held = serde_json::json!({
        "schema": "mdkg.holds.v1", "ok": true, "diagnostics": [],
        "data": {"holds": true, "lease": {
            "id": "lease-remote-1", "owner": "repo", "scope": ".mdkg/config.json",
            "status": "active", "until": "2999-01-01T00:00:00Z",
            "deadline_ms": 32_503_680_000_000u64, "mode": "exclusive"}}
    });
    fs::write(s.0.join("holds.json"), held.to_string()).unwrap();
    let mut released = held.clone();
    released["schema"] = serde_json::json!("mdkg.release.v1");
    released["data"] = serde_json::json!({"lease": held["data"]["lease"]});
    released["data"]["lease"]["status"] = serde_json::json!("released");
    fs::write(s.0.join("release.json"), released.to_string()).unwrap();
    let key = mdkg_format::Digest::of_bytes(common.as_bytes());
    let file = folder.join(format!("{}.json", key.hex()));
    fs::write(
        &file,
        serde_json::json!({"common_dir": common, "command": [script], "timeout_ms": 120_000})
            .to_string(),
    )
    .unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
    let log = || fs::read_to_string(s.0.join("args.log")).unwrap_or_default();

    let (code, out) = s.mdkg(&["config", "set", KEY, "2", "--json"]);
    assert_eq!(code, 0, "{out}");
    assert!(log().contains("holds .mdkg/config.json"), "{}", log());
    assert!(
        log().contains("release lease-remote-1"),
        "a change releases: {}",
        log()
    );
    let warned = out["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .any(|d| d["code"] == "coord.authority_remote");
    assert!(!warned, "the release succeeded: {out}");

    fs::remove_file(s.0.join("args.log")).unwrap();
    let (code, out) = s.mdkg(&["config", "set", KEY, "2", "--json"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["data"]["changed"], false, "{out}");
    assert!(log().contains("holds .mdkg/config.json"), "{}", log());
    assert!(!log().contains("release"), "a no-op keeps it: {}", log());
}

/// H-M6 (orchestrator #20070): the command `install` and `doctor` name for
/// a config with no `projections` key is `config set projections <today's
/// list>`, then `install`. It's durable in either mode: a publish on the
/// graph ref, or a commit in the registered graph checkout. A later graph
/// write isn't frozen by a dirty config, and neither command warns again
/// where the config is read.
#[test]
fn the_legacy_projections_hint_writes_the_key_in_either_mode() {
    const CODE: &str = "install.legacy_projections";
    let legacy = |out: &Value| {
        out["diagnostics"]
            .as_array()
            .map(|ds| ds.iter().filter(|d| d["code"] == CODE).count())
            .unwrap_or_default()
    };
    let list = serde_json::json!([
        "agents-md",
        "mcp:claude",
        "mcp:codex",
        "mcp:opencode",
        "skills:.agents/skills",
        "skills:.claude/skills"
    ]);
    for checkout in [false, true] {
        let s = Scratch::new();
        if checkout {
            s.checkout_mode();
        } else {
            s.ref_mode();
        }
        // A config from before the key existed.
        let (code, out) = s.mdkg(&["config", "unset", "projections", "--json"]);
        assert_eq!(code, 0, "{out}");
        let (code, out) = s.mdkg(&["install", "--json"]);
        assert_eq!(code, 0, "checkout {checkout}: {out}");
        assert_eq!(legacy(&out), 1, "checkout {checkout}: {out}");
        let hint = out["diagnostics"]
            .as_array()
            .and_then(|ds| ds.iter().find(|d| d["code"] == CODE))
            .and_then(|d| d["hint"].as_str())
            .expect("hint")
            .to_owned();
        assert!(
            hint.contains(&format!("config set projections '{list}'`")),
            "{hint}"
        );
        // Where the committed config is read: the graph ref, or the
        // checkout install registered.
        let graph = s.repo().join(".worktrees/graph");
        let at: Vec<String> = if checkout {
            assert!(graph.is_dir(), "install registers the graph checkout");
            vec!["-r".into(), graph.to_str().expect("utf-8").into()]
        } else {
            Vec::new()
        };
        let run = |args: &[&str]| {
            let mut all: Vec<&str> = at.iter().map(String::as_str).collect();
            all.extend(args);
            s.mdkg(&all)
        };
        let (_, out) = run(&["doctor", "--json"]);
        assert_eq!(legacy(&out), 1, "checkout {checkout}: {out}");
        // The hint's command, as given.
        let (code, out) = run(&["config", "set", "projections", &list.to_string(), "--json"]);
        assert_eq!(code, 0, "checkout {checkout}: {out}");
        // The graph ref in ref mode, the graph checkout's branch otherwise.
        assert_eq!(
            s.committed("mdkg-graph")["projections"],
            list,
            "checkout {checkout}"
        );
        if checkout {
            let status = s.git(&[
                "-C",
                graph.to_str().expect("utf-8"),
                "status",
                "--porcelain",
            ]);
            assert_eq!(status, "", "the graph checkout is left clean");
        }
        // A later graph write isn't frozen.
        let (code, out) = run(&["new", "task", "After", "the", "key", "--json"]);
        assert_eq!(code, 0, "checkout {checkout}: {out}");
        let (code, out) = run(&["config", "set", KEY, "2", "--json"]);
        assert_eq!(code, 0, "checkout {checkout}: {out}");
        // Then install, where the config is read: no warning.
        let (code, out) = run(&["install", "--json"]);
        assert_eq!(code, 0, "checkout {checkout}: {out}");
        assert_eq!(legacy(&out), 0, "checkout {checkout}: {out}");
        let (_, out) = run(&["doctor", "--json"]);
        assert_eq!(legacy(&out), 0, "checkout {checkout}: {out}");
    }
}
