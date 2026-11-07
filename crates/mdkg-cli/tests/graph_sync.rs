//! `mdkg graph remote approve` and `mdkg graph sync` (edd-01a18867,
//! section 4, lane L2) against local bare repositories: no network, and
//! nothing reaches the user's own configuration.

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
        let dir = common::temp::dir("mdkg-graph-sync");
        let dir = fs::canonicalize(dir).expect("canonical");
        for d in ["home", "repo", "hooks"] {
            fs::create_dir_all(dir.join(d)).expect("folder");
        }
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

    /// git's exit code in the repository.
    fn git_status(&self, args: &[&str]) -> i32 {
        self.isolated(Command::new("git").arg("-C").arg(self.repo()).args(args))
            .output()
            .expect("git runs")
            .status
            .code()
            .unwrap_or(-1)
    }

    fn mdkg_env(&self, args: &[&str], env: &[(&str, &str)]) -> (i32, Value) {
        self.mdkg_exe(std::path::Path::new(env!("CARGO_BIN_EXE_mdkg")), args, env)
    }

    /// The built binary run as `exe` (a link to it under another name).
    fn mdkg_exe(&self, exe: &std::path::Path, args: &[&str], env: &[(&str, &str)]) -> (i32, Value) {
        let mut cmd = Command::new(exe);
        self.isolated(&mut cmd).current_dir(self.repo()).args(args);
        for (k, v) in env {
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

    fn mdkg(&self, args: &[&str]) -> (i32, Value) {
        self.mdkg_env(args, &[])
    }

    /// `mdkg` that must end within a minute: a wait on the graph lock this
    /// process itself holds would otherwise hang the test (Terra #17687).
    fn mdkg_timed(&self, args: &[&str]) -> (i32, Value) {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_mdkg"));
        self.isolated(&mut cmd)
            .current_dir(self.repo())
            .args(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = cmd.spawn().expect("mdkg runs");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while child.try_wait().expect("wait").is_none() {
            if std::time::Instant::now() > deadline {
                child.kill().ok();
                child.wait().ok();
                panic!("mdkg {args:?} didn't finish in 60 s");
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let out = child.wait_with_output().expect("output");
        let json: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
            panic!(
                "mdkg {args:?} printed no JSON ({e}): {}\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
        });
        (out.status.code().unwrap_or(-1), json)
    }

    /// Makes the remote's graph ref a child of the local tip holding
    /// `text` at `path`, as another clone's push would, and leaves the
    /// local graph ref where it was. The remote's new tip.
    fn advance_remote(&self, path: &str, text: &str) -> String {
        let local = self.git(&["rev-parse", GRAPH]);
        let theirs = self.plant(path, text);
        self.git(&["push", "-q", "mine", &format!("{theirs}:{GRAPH}")]);
        self.git(&["update-ref", GRAPH, &local, &theirs]);
        theirs
    }

    /// A ref-mode repository on `main` (code pushed to `origin`, the code
    /// remote) whose graph remote is `mine`, a private bare repository.
    fn setup() -> Self {
        let s = Self::new();
        for bare in ["code.git", "mine.git"] {
            s.git_in(&s.0, &["init", "-q", "--bare", "-b", "main", bare]);
        }
        s.git(&["init", "-q", "-b", "main"]);
        s.git(&["config", "user.name", "Sync Test"]);
        s.git(&["config", "user.email", "sync@example.invalid"]);
        // Hooks of the fixture's own, never the user's.
        s.git(&[
            "config",
            "core.hooksPath",
            &s.0.join("hooks").to_string_lossy(),
        ]);
        fs::write(s.repo().join("code.rs"), "fn main() {}\n").expect("code");
        s.git(&["add", "code.rs"]);
        s.git(&["commit", "-q", "-m", "code"]);
        s.git(&[
            "remote",
            "add",
            "origin",
            &s.0.join("code.git").to_string_lossy(),
        ]);
        s.git(&["push", "-q", "origin", "main"]);
        s.git(&[
            "remote",
            "add",
            "mine",
            &s.0.join("mine.git").to_string_lossy(),
        ]);
        s.git(&["config", "mdkg.graph.ref", GRAPH]);
        s.git(&["config", "mdkg.mainRef", "main"]);
        let (code, init) = s.mdkg(&["init", "--json"]);
        assert_eq!(code, 0, "{init}");
        s.git(&["config", "mdkg.graph.remote", "mine"]);
        s
    }

    fn remote_tip(&self) -> Option<String> {
        let out = self
            .isolated(
                Command::new("git")
                    .arg("-C")
                    .arg(self.0.join("mine.git"))
                    .args(["rev-parse", "--verify", "--quiet", GRAPH]),
            )
            .output()
            .expect("git");
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }

    /// Commits `text` at `path` straight onto the graph ref.
    fn plant(&self, path: &str, text: &str) -> String {
        let tip = self.git(&["rev-parse", GRAPH]);
        let index = self.0.join("plant-index");
        let run = |args: &[&str], input: Option<&str>| -> String {
            let mut cmd = Command::new("git");
            self.isolated(&mut cmd)
                .arg("-C")
                .arg(self.repo())
                .args(args)
                .env("GIT_INDEX_FILE", &index)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped());
            let mut child = cmd.spawn().expect("git");
            if let Some(i) = input {
                use std::io::Write as _;
                child
                    .stdin
                    .take()
                    .expect("stdin")
                    .write_all(i.as_bytes())
                    .expect("write");
            }
            drop(child.stdin.take());
            let out = child.wait_with_output().expect("git");
            assert!(out.status.success(), "{args:?}");
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        };
        run(&["read-tree", &tip], None);
        let blob = run(&["hash-object", "-w", "--stdin"], Some(text));
        run(
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("100644,{blob},{path}"),
            ],
            None,
        );
        let tree = run(&["write-tree"], None);
        let commit = run(&["commit-tree", &tree, "-p", &tip, "-m", "plant"], None);
        self.git(&["update-ref", GRAPH, &commit, &tip]);
        fs::remove_file(index).ok();
        commit
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

/// Doctor's check named `name`.
fn doctor_row(s: &Scratch, name: &str) -> Value {
    let (_, v) = s.mdkg(&["doctor", "--json"]);
    v["data"]["checks"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|c| c["name"] == name)
        .cloned()
        .unwrap_or_else(|| panic!("no {name} check: {v}"))
}

#[test]
fn approval_and_sync_are_refused_inside_an_agent_turn() {
    let s = Scratch::setup();
    let turn = [("MDKG_RUN_TURN", "1")];
    let (code, v) = s.mdkg_env(&["graph", "remote", "approve", "--json"], &turn);
    assert_ne!(code, 0, "{v}");
    assert_eq!(codes(&v), ["graph.remote_unapproved"], "{v}");
    assert!(!s.0.join("repo/.git/mdkg/graph/remote.json").exists());
    let (code, v) = s.mdkg_env(&["graph", "sync", "--json"], &turn);
    assert_ne!(code, 0, "{v}");
    assert_eq!(codes(&v), ["graph.sync_in_turn"], "{v}");
    assert_eq!(s.remote_tip(), None);
}

#[test]
fn a_person_approves_then_syncs() {
    let s = Scratch::setup();
    // Unapproved: a person's sync neither pulls nor pushes (DECISION 11)
    // and says how to approve.
    let (code, v) = s.mdkg(&["graph", "sync", "--json"]);
    assert_ne!(code, 0, "{v}");
    assert_eq!(codes(&v), ["graph.remote_unapproved"], "{v}");
    assert!(
        v["diagnostics"][0]["message"]
            .as_str()
            .is_some_and(|m| m.contains("graph remote approve")),
        "{v}"
    );
    assert_eq!(s.remote_tip(), None);
    let (_, status) = s.mdkg(&["status", "--json"]);
    assert!(
        codes(&status).contains(&"graph.remote_unapproved".to_owned()),
        "{status}"
    );

    // Doctor: never approved is a warning (DECISION 2), with the scanner row.
    let remote = doctor_row(&s, "graph.remote");
    assert_eq!(remote["status"], "warning", "{remote}");
    assert!(
        remote["hint"]
            .as_str()
            .unwrap_or("")
            .contains("graph remote approve"),
        "{remote}"
    );
    assert_eq!(doctor_row(&s, "graph.secret_scan")["status"], "ok");

    let (code, v) = s.mdkg(&["graph", "remote", "approve", "--dry-run", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["data"]["pinned"], false);
    assert!(!s.0.join("repo/.git/mdkg/graph/remote.json").exists());
    let (code, v) = s.mdkg(&["graph", "remote", "approve", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["data"]["remote"], "mine");
    assert_eq!(v["data"]["remote_ref"], GRAPH);
    assert_eq!(v["data"]["code_remote"], false);
    let mine = format!("{}", s.0.join("mine.git").display());
    assert_eq!(v["data"]["push"], serde_json::json!([mine]), "{v}");

    let (code, v) = s.mdkg(&["graph", "sync", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["data"]["pushed"], true, "{v}");
    assert_eq!(s.remote_tip(), Some(s.git(&["rev-parse", GRAPH])));
    // Only the graph ref crossed.
    assert_eq!(
        s.git_in(
            &s.0.join("mine.git"),
            &["for-each-ref", "--format=%(refname)"]
        ),
        GRAPH
    );
    assert_eq!(doctor_row(&s, "graph.remote")["status"], "ok");
    let (_, status) = s.mdkg(&["status", "--json"]);
    assert!(
        !codes(&status)
            .iter()
            .any(|c| c.starts_with("graph.remote") || c.starts_with("graph.sync")),
        "{status}"
    );
}

#[test]
fn a_conflict_listing_that_fails_is_shown_not_hidden() {
    // Terra #19873: in a shallow clone a conflict whose merge is missing
    // can't be judged; graph conflicts fails and doctor says so.
    let s = Scratch::setup();
    let (code, v) = s.mdkg(&["graph", "remote", "approve", "--json"]);
    assert_eq!(code, 0, "{v}");
    let (code, v) = s.mdkg(&["graph", "sync", "--json"]);
    assert_eq!(code, 0, "{v}");
    let tip = s.git(&["rev-parse", GRAPH]);
    let missing = "b".repeat(40);
    let graph = s.0.join("repo/.git/mdkg/graph");
    fs::create_dir_all(&graph).expect("graph dir");
    fs::write(
        graph.join("conflicts.json"),
        serde_json::json!({"conflicts": [{"merge": missing, "path": ".mdkg/n.txt", "local": null, "remote": null, "time": 1}]})
            .to_string(),
    )
    .expect("conflicts.json");
    fs::write(s.0.join("repo/.git/shallow"), format!("{tip}\n")).expect("shallow");
    let (code, v) = s.mdkg(&["graph", "conflicts", "--json"]);
    assert_ne!(code, 0, "{v}");
    assert!(v.to_string().contains("shallow"), "{v}");
    let (_, status) = s.mdkg(&["status", "--json"]);
    let listing = status["diagnostics"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|d| {
            d["message"]
                .as_str()
                .is_some_and(|m| m.contains("can't be listed"))
        })
        .cloned()
        .unwrap_or_default();
    assert_eq!(listing["code"], "graph.git", "{status}");
    assert!(
        !codes(&status).contains(&"graph.sync_conflict".to_owned()),
        "{status}"
    );
    let row = doctor_row(&s, "graph.remote");
    assert_eq!(row["status"], "warning", "{row}");
    assert!(
        row["summary"]
            .as_str()
            .unwrap_or_default()
            .contains("can't be listed"),
        "{row}"
    );
}

#[test]
fn a_saved_sync_error_names_the_binary_running_now() {
    // Terra #20017, #20048: an error saved by a build under another name
    // (one since removed) suggests its commands under the name running
    // now, even after a remote ref with a backtick of its own.
    let s = Scratch::setup();
    let (code, v) = s.mdkg(&["graph", "remote", "approve", "--json"]);
    assert_eq!(code, 0, "{v}");
    let (code, v) = s.mdkg(&["graph", "sync", "--json"]);
    assert_eq!(code, 0, "{v}");
    let state = s.0.join("repo/.git/mdkg/graph/sync.json");
    let mut v: Value = serde_json::from_slice(&fs::read(&state).expect("sync.json")).expect("json");
    let old = "mdkg-gone";
    v["last_error"] = serde_json::json!({
        "code": "graph.remote_rewritten",
        "message": format!(
            "refs/heads/graph`east moved; a person runs `{old} graph sync --accept-rewrite`"
        ),
        "time": 1,
        "bin": old
    });
    v["attempts"] = serde_json::json!(1);
    fs::write(&state, v.to_string()).expect("sync.json");
    let (_, status) = s.mdkg(&["status", "--json"]);
    let text = status.to_string();
    assert!(
        text.contains("`mdkg graph sync --accept-rewrite`"),
        "{status}"
    );
    assert!(!text.contains(old), "{status}");
    let row = doctor_row(&s, "graph.remote");
    assert!(
        row["summary"]
            .as_str()
            .unwrap_or_default()
            .contains("`mdkg graph sync --accept-rewrite`"),
        "{row}"
    );
    assert!(!row.to_string().contains(old), "{row}");
}

#[test]
#[cfg(unix)]
fn a_failure_synced_under_one_name_is_shown_under_the_next() {
    // Terra #20112: a real failed sync records its writer, so a later
    // build under another name suggests its own command.
    let s = Scratch::setup();
    let (code, v) = s.mdkg(&["graph", "remote", "approve", "--json"]);
    assert_eq!(code, 0, "{v}");
    let (code, v) = s.mdkg(&["graph", "sync", "--json"]);
    assert_eq!(code, 0, "{v}");
    s.git_in(&s.0.join("mine.git"), &["update-ref", "-d", GRAPH]);
    let bin = s.0.join("bin");
    fs::create_dir_all(&bin).expect("bin dir");
    let gone = bin.join("mdkg-gone");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_mdkg"), &gone).expect("link");
    let (code, v) = s.mdkg_exe(&gone, &["graph", "sync", "--json"], &[]);
    assert_ne!(code, 0, "{v}");
    let saved: Value = serde_json::from_slice(
        &fs::read(s.0.join("repo/.git/mdkg/graph/sync.json")).expect("sync.json"),
    )
    .expect("json");
    assert_eq!(saved["last_error"]["bin"], "mdkg-gone", "{saved}");
    assert!(
        saved["last_error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("`mdkg-gone graph remote approve`"),
        "{saved}"
    );
    fs::remove_file(&gone).expect("unlink");
    let (_, status) = s.mdkg(&["status", "--json"]);
    let text = status.to_string();
    assert!(text.contains("`mdkg graph remote approve`"), "{status}");
    assert!(!text.contains("mdkg-gone"), "{status}");
}

#[test]
fn doctor_reports_a_deleted_remote_ref_until_it_is_recreated() {
    // Terra #19640 minor: doctor reads the deleted state itself, not only
    // the last error, and tells approval from the pending recreation.
    let s = Scratch::setup();
    let (code, v) = s.mdkg(&["graph", "remote", "approve", "--json"]);
    assert_eq!(code, 0, "{v}");
    let (code, v) = s.mdkg(&["graph", "sync", "--json"]);
    assert_eq!(code, 0, "{v}");
    s.git_in(&s.0.join("mine.git"), &["update-ref", "-d", GRAPH]);
    let (code, v) = s.mdkg(&["graph", "sync", "--json"]);
    assert_ne!(code, 0, "{v}");
    let row = doctor_row(&s, "graph.remote");
    assert_eq!(row["status"], "warning", "{row}");
    assert!(
        row["summary"]
            .as_str()
            .unwrap_or_default()
            .contains("was deleted"),
        "{row}"
    );
    assert!(
        row["hint"]
            .as_str()
            .unwrap_or_default()
            .contains("graph remote approve"),
        "{row}"
    );
    // A push latch names the deletion and the next step too (#19852):
    // planted beside `missing`, then removed again.
    let state = s.0.join("repo/.git/mdkg/graph/sync.json");
    let latch = |on: bool| {
        let mut v: Value =
            serde_json::from_slice(&fs::read(&state).expect("sync.json")).expect("json");
        v["refused"] = if on {
            serde_json::json!({"code": "graph.push_refused", "message": "hook said no", "time": 1})
        } else {
            Value::Null
        };
        fs::write(&state, v.to_string()).expect("sync.json");
    };
    latch(true);
    let row = doctor_row(&s, "graph.remote");
    let summary = row["summary"].as_str().unwrap_or_default();
    assert!(summary.contains("hook said no"), "{row}");
    assert!(summary.contains("recreating it needs"), "{row}");
    latch(false);
    let (code, v) = s.mdkg(&["graph", "remote", "approve", "--json"]);
    assert_eq!(code, 0, "{v}");
    latch(true);
    let row = doctor_row(&s, "graph.remote");
    let summary = row["summary"].as_str().unwrap_or_default();
    assert!(summary.contains("hook said no"), "{row}");
    assert!(summary.contains("the next push recreates it"), "{row}");
    latch(false);
    // A pull-only sync succeeds and clears the last error; the ref is
    // still absent.
    let (code, v) = s.mdkg(&["graph", "sync", "--no-push", "--json"]);
    assert_eq!(code, 0, "{v}");
    let row = doctor_row(&s, "graph.remote");
    assert!(
        row["summary"]
            .as_str()
            .unwrap_or_default()
            .contains("approved again"),
        "{row}"
    );
    let (code, v) = s.mdkg(&["graph", "sync", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(s.remote_tip(), Some(s.git(&["rev-parse", GRAPH])));
    assert_ne!(doctor_row(&s, "graph.remote")["status"], "warning");
}

#[test]
fn doctor_shows_a_later_failure_while_a_deleted_ref_waits() {
    // Terra #19723: after approving a deleted ref again, a push the scan
    // refuses is what doctor reports, with the deletion named.
    let s = Scratch::setup();
    let (code, v) = s.mdkg(&["graph", "remote", "approve", "--json"]);
    assert_eq!(code, 0, "{v}");
    let (code, v) = s.mdkg(&["graph", "sync", "--json"]);
    assert_eq!(code, 0, "{v}");
    s.git_in(&s.0.join("mine.git"), &["update-ref", "-d", GRAPH]);
    let (code, v) = s.mdkg(&["graph", "sync", "--json"]);
    assert_ne!(code, 0, "{v}");
    let (code, v) = s.mdkg(&["graph", "remote", "approve", "--json"]);
    assert_eq!(code, 0, "{v}");
    let token = concat!("ghp_", "abcdefghijklmnopqrstuvwxyz0123456789");
    s.plant(".mdkg/leak.txt", &format!("token: {token}\n"));
    let (code, v) = s.mdkg(&["graph", "sync", "--json"]);
    assert_ne!(code, 0, "{v}");
    assert_eq!(codes(&v), ["graph.secret"], "{v}");
    let row = doctor_row(&s, "graph.remote");
    let summary = row["summary"].as_str().unwrap_or_default();
    assert!(summary.contains("graph.secret"), "{row}");
    assert!(summary.contains("is deleted"), "{row}");
    assert!(!row.to_string().contains(token), "{row}");
    assert_eq!(s.remote_tip(), None);
}

#[test]
fn the_real_secret_scanner_refuses_a_planted_credential() {
    let s = Scratch::setup();
    let (code, v) = s.mdkg(&["graph", "remote", "approve", "--json"]);
    assert_eq!(code, 0, "{v}");
    let token = concat!("ghp_", "abcdefghijklmnopqrstuvwxyz0123456789");
    let planted = s.plant(".mdkg/leak.txt", &format!("token: {token}\n"));
    let (code, v) = s.mdkg(&["graph", "sync", "--json"]);
    assert_ne!(code, 0, "{v}");
    assert_eq!(codes(&v), ["graph.secret"], "{v}");
    let text = v.to_string();
    assert!(
        text.contains(&planted[..12]) && text.contains(".mdkg/leak.txt"),
        "{v}"
    );
    assert!(!text.contains(token), "the match is never printed: {v}");
    assert_eq!(s.remote_tip(), None);
}

#[test]
fn a_changed_destination_is_refused_by_sync_and_doctor() {
    let s = Scratch::setup();
    let (code, v) = s.mdkg(&["graph", "remote", "approve", "--json"]);
    assert_eq!(code, 0, "{v}");
    s.git_in(&s.0, &["init", "-q", "--bare", "elsewhere.git"]);
    s.git(&[
        "remote",
        "set-url",
        "mine",
        &s.0.join("elsewhere.git").to_string_lossy(),
    ]);
    let (code, v) = s.mdkg(&["graph", "sync", "--json"]);
    assert_ne!(code, 0, "{v}");
    assert_eq!(codes(&v), ["graph.remote_changed"], "{v}");
    let (_, status) = s.mdkg(&["status", "--json"]);
    assert!(
        codes(&status).contains(&"graph.remote_changed".to_owned()),
        "{status}"
    );
    assert_eq!(doctor_row(&s, "graph.remote")["status"], "error");
    assert_eq!(
        s.git_in(&s.0.join("elsewhere.git"), &["for-each-ref"]),
        "",
        "nothing reached the new destination"
    );
}

#[test]
fn a_global_include_into_the_checkout_is_pinned() {
    // DECISION 16 (A'), Terra #19100: an included file takes its
    // includer's scope, so a global includeIf pointing into the checkout
    // lets an agent change the ssh command; every scope but `command` is
    // pinned.
    let s = Scratch::setup();
    let included = s.repo().join("included.gitconfig");
    fs::write(&included, "[core]\n\tsshCommand = ssh\n").expect("include");
    fs::write(
        s.0.join("home/.gitconfig"),
        format!("[include]\n\tpath = {}\n", included.display()),
    )
    .expect("global config");
    let (code, v) = s.mdkg(&["graph", "remote", "approve", "--json"]);
    assert_eq!(code, 0, "{v}");
    fs::write(&included, "[core]\n\tsshCommand = ssh -o IncludeMarker=1\n").expect("include");
    let (code, v) = s.mdkg(&["graph", "sync", "--json"]);
    assert_ne!(code, 0, "{v}");
    assert_eq!(codes(&v), ["graph.remote_changed"], "{v}");
    let text = v.to_string();
    assert!(text.contains("global core.sshcommand"), "{v}");
    assert!(!text.contains("IncludeMarker"), "{v}");
}

#[test]
fn a_person_pulls_a_newer_remote_tip_without_waiting_on_its_own_lock() {
    let s = Scratch::setup();
    let (code, v) = s.mdkg(&["graph", "remote", "approve", "--json"]);
    assert_eq!(code, 0, "{v}");
    let (code, v) = s.mdkg(&["graph", "sync", "--json"]);
    assert_eq!(code, 0, "{v}");
    // A command that binds the graph first, so its cache is at the tip.
    let (code, v) = s.mdkg(&["list", "--json"]);
    assert_eq!(code, 0, "{v}");
    let theirs = s.advance_remote(".mdkg/notes.txt", "from another clone\n");
    let (code, v) = s.mdkg_timed(&["graph", "sync", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["data"]["after"], theirs.as_str(), "{v}");
    assert_eq!(s.git(&["rev-parse", GRAPH]), theirs, "a fast-forward");
    let (code, v) = s.mdkg(&["list", "--json"]);
    assert_eq!(code, 0, "the next command binds at the new tip: {v}");
}

#[test]
fn a_conflict_is_pulled_then_resolved_without_waiting_on_its_own_lock() {
    let s = Scratch::setup();
    let (code, v) = s.mdkg(&["graph", "remote", "approve", "--json"]);
    assert_eq!(code, 0, "{v}");
    s.plant(".mdkg/notes.txt", "base\n");
    let (code, v) = s.mdkg(&["graph", "sync", "--json"]);
    assert_eq!(code, 0, "{v}");
    s.advance_remote(".mdkg/notes.txt", "remote\n");
    s.plant(".mdkg/notes.txt", "local\n");
    let (code, v) = s.mdkg_timed(&["graph", "sync", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert!(codes(&v).contains(&"graph.sync_conflict".to_owned()), "{v}");
    assert_eq!(
        s.git(&["show", &format!("{GRAPH}:.mdkg/notes.txt")]),
        "remote"
    );
    let (code, v) = s.mdkg(&["graph", "conflicts", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(
        v["data"]["conflicts"].as_array().map(Vec::len),
        Some(1),
        "{v}"
    );
    let (code, v) = s.mdkg_timed(&[
        "graph",
        "resolve",
        ".mdkg/notes.txt",
        "--take",
        "local",
        "--json",
    ]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(
        s.git(&["show", &format!("{GRAPH}:.mdkg/notes.txt")]),
        "local"
    );
    let (_, v) = s.mdkg(&["graph", "conflicts", "--json"]);
    assert_eq!(v["data"]["conflicts"], serde_json::json!([]), "{v}");
}

#[test]
fn a_conflicted_path_with_a_hidden_character_is_resolved_as_listed() {
    // Terra #19906, #19960: `graph conflicts` and the sync's note show
    // U+200D escaped; the suggested command, run as written, settles only
    // the conflict it names, and the raw and escaped spellings of one path
    // settle it once.
    let s = Scratch::setup();
    let (code, v) = s.mdkg(&["graph", "remote", "approve", "--json"]);
    assert_eq!(code, 0, "{v}");
    let path = ".mdkg/a\u{200d}b.txt";
    let shown = ".mdkg/a\\u{200d}b.txt";
    s.plant(path, "base\n");
    let (code, v) = s.mdkg(&["graph", "sync", "--json"]);
    assert_eq!(code, 0, "{v}");
    let mut notes = Vec::new();
    for round in ["1", "2"] {
        s.advance_remote(path, &format!("remote {round}\n"));
        s.plant(path, &format!("local {round}\n"));
        let (code, v) = s.mdkg_timed(&["graph", "sync", "--json"]);
        assert_eq!(code, 0, "{v}");
        let note = v["diagnostics"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|d| d["code"] == "graph.sync_conflict")
            .and_then(|d| d["message"].as_str())
            .unwrap_or_default()
            .to_owned();
        assert!(note.contains(&format!(" -- '{shown}'` keeps it")), "{v}");
        notes.push(note);
    }
    let (code, v) = s.mdkg(&["graph", "conflicts", "--json"]);
    assert_eq!(code, 0, "{v}");
    let open: Vec<String> = v["data"]["conflicts"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| {
            assert_eq!(c["path"], path, "{v}");
            c["merge"].as_str().unwrap_or_default().to_owned()
        })
        .collect();
    assert_eq!(open.len(), 2, "{v}");
    // The second round's suggestion, word for word: its options, then the
    // single-quoted path (no quote of its own in this one).
    let cmd = notes[1]
        .split(" graph resolve ")
        .nth(1)
        .and_then(|rest| rest.split("` keeps it").next())
        .expect("suggested command")
        .to_owned();
    let (opts, word) = cmd.split_once(" -- ").expect("-- before the path");
    let typed = word.trim_matches('\'');
    assert_eq!(typed, shown, "{cmd}");
    let mut args = vec!["graph", "resolve"];
    args.extend(opts.split_whitespace());
    args.extend(["--json", "--", typed]);
    let named = opts
        .split_whitespace()
        .skip_while(|w| *w != "--merge")
        .nth(1)
        .expect("--merge in the suggestion")
        .to_owned();
    let newer = open
        .iter()
        .find(|m| m.starts_with(&named))
        .expect("the suggestion names an open merge")
        .clone();
    let (code, v) = s.mdkg_timed(&args);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["data"]["paths"], serde_json::json!([path]), "{v}");
    assert_eq!(s.git(&["show", &format!("{GRAPH}:{path}")]), "local 2");
    let (_, v) = s.mdkg(&["graph", "conflicts", "--json"]);
    let left: Vec<&str> = v["data"]["conflicts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c["merge"].as_str())
        .collect();
    assert_eq!(left.len(), 1, "{v}");
    assert_ne!(left[0], newer, "only the named merge cleared: {v}");
    // Both spellings at once: one conflict, settled once.
    let older = left[0].to_owned();
    let (code, v) = s.mdkg_timed(&[
        "graph", "resolve", "--take", "remote", "--merge", &older, "--json", "--", path, shown,
    ]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["data"]["paths"], serde_json::json!([path]), "{v}");
    let (_, v) = s.mdkg(&["graph", "conflicts", "--json"]);
    assert_eq!(v["data"]["conflicts"], serde_json::json!([]), "{v}");
}

#[test]
fn a_path_with_two_open_conflicts_is_resolved_only_by_naming_its_merge() {
    // DECISION 20: with several open conflicts on a path, resolve needs
    // --merge, so repeating a resolution whose outcome was uncertain can't
    // settle the older conflict instead.
    let s = Scratch::setup();
    let (code, v) = s.mdkg(&["graph", "remote", "approve", "--json"]);
    assert_eq!(code, 0, "{v}");
    s.plant(".mdkg/notes.txt", "base\n");
    let (code, v) = s.mdkg(&["graph", "sync", "--json"]);
    assert_eq!(code, 0, "{v}");
    let merges = |s: &Scratch| -> Vec<String> {
        let (_, v) = s.mdkg(&["graph", "conflicts", "--json"]);
        v["data"]["conflicts"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| c["merge"].as_str().map(str::to_owned))
            .collect()
    };
    for round in ["1", "2"] {
        s.advance_remote(".mdkg/notes.txt", &format!("remote {round}\n"));
        s.plant(".mdkg/notes.txt", &format!("local {round}\n"));
        let (code, v) = s.mdkg_timed(&["graph", "sync", "--json"]);
        assert_eq!(code, 0, "{v}");
    }
    let open = merges(&s);
    assert_eq!(open.len(), 2, "{open:?}");
    let newest = s.git(&["rev-parse", GRAPH]);
    assert!(open.contains(&newest), "{open:?} {newest}");
    let older = open.iter().find(|m| **m != newest).cloned().expect("older");
    let resolve = |merge: Option<&str>| {
        let mut args = vec!["graph", "resolve", ".mdkg/notes.txt", "--take", "local"];
        if let Some(m) = merge {
            args.extend(["--merge", m]);
        }
        args.push("--json");
        s.mdkg_timed(&args)
    };
    let (code, v) = resolve(None);
    assert_ne!(code, 0, "{v}");
    assert_eq!(codes(&v), ["usage.invalid_arguments"], "{v}");
    assert!(v.to_string().contains(&older[..12]), "{v}");
    assert_eq!(merges(&s).len(), 2);
    let (code, v) = resolve(Some(&newest[..10]));
    assert_eq!(code, 0, "{v}");
    assert_eq!(
        s.git(&["show", &format!("{GRAPH}:.mdkg/notes.txt")]),
        "local 2"
    );
    // The same command again finds nothing to settle; the older conflict
    // stays open and the graph keeps the chosen version.
    let (code, v) = resolve(Some(&newest[..10]));
    assert_ne!(code, 0, "{v}");
    assert_eq!(merges(&s), [older]);
    assert_eq!(
        s.git(&["show", &format!("{GRAPH}:.mdkg/notes.txt")]),
        "local 2"
    );
}

fn node(id: &str, body: &str) -> String {
    format!(
        "---\nid: {id}\ntype: task\ntitle: Fixture\nstatus: todo\npriority: 2\n\
         created: 2024-06-09T00:00:00.000Z\nupdated: 2024-06-09T00:00:00.000Z\n---\n{body}\n"
    )
}

#[test]
fn a_resolution_that_breaks_the_graph_is_refused_and_paths_settle_together() {
    // DECISION 18 (V), Terra #19140: a resolution passes the same strict
    // validation a sync merge does. Taking the local version here gives
    // two nodes one id, so it's refused, nothing is published and the
    // conflict stays open.
    const X: &str = ".mdkg/work/task-x.md";
    const Z: &str = ".mdkg/work/task-z.md";
    const W: &str = ".mdkg/notes.txt";
    let a = "task-01900000-0000-7000-8000-00000000000a";
    let b = "task-01900000-0000-7000-8000-00000000000b";
    let s = Scratch::setup();
    let (code, v) = s.mdkg(&["graph", "remote", "approve", "--json"]);
    assert_eq!(code, 0, "{v}");
    s.plant(X, &node(a, "base"));
    s.plant(W, "base\n");
    let (code, v) = s.mdkg(&["graph", "sync", "--json"]);
    assert_eq!(code, 0, "{v}");
    // The remote edits X and W and adds Z with id b; locally, X takes id b
    // and W changes too: two conflicts, X and W.
    let local = s.git(&["rev-parse", GRAPH]);
    s.plant(X, &node(a, "remote"));
    s.plant(Z, &node(b, "other"));
    let theirs = s.plant(W, "remote\n");
    s.git(&["push", "-q", "mine", &format!("{theirs}:{GRAPH}")]);
    s.git(&["update-ref", GRAPH, &local, &theirs]);
    s.plant(X, &node(b, "local"));
    s.plant(W, "local\n");
    let (code, v) = s.mdkg_timed(&["graph", "sync", "--json"]);
    assert_eq!(code, 0, "{v}");
    let (_, v) = s.mdkg(&["graph", "conflicts", "--json"]);
    assert_eq!(
        v["data"]["conflicts"].as_array().map(Vec::len),
        Some(2),
        "{v}"
    );
    let before = s.git(&["rev-parse", GRAPH]);
    let (code, v) = s.mdkg_timed(&["graph", "resolve", X, "--take", "local", "--json"]);
    assert_ne!(code, 0, "{v}");
    assert_eq!(codes(&v), ["graph.sync_invalid"], "{v}");
    assert_eq!(s.git(&["rev-parse", GRAPH]), before, "nothing published");
    let (_, v) = s.mdkg(&["graph", "conflicts", "--json"]);
    assert_eq!(
        v["data"]["conflicts"].as_array().map(Vec::len),
        Some(2),
        "{v}"
    );
    // Several paths settle in one commit.
    let (code, v) = s.mdkg_timed(&["graph", "resolve", X, W, "--take", "remote", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["data"]["paths"], serde_json::json!([W, X]), "{v}");
    assert_eq!(v["data"]["cleared"], 2, "{v}");
    let (_, v) = s.mdkg(&["graph", "conflicts", "--json"]);
    assert_eq!(v["data"]["conflicts"], serde_json::json!([]), "{v}");
}

#[test]
fn a_resolution_is_refused_while_someone_else_leases_the_node() {
    let s = Scratch::setup();
    let (code, v) = s.mdkg(&["graph", "remote", "approve", "--json"]);
    assert_eq!(code, 0, "{v}");
    s.plant(".mdkg/notes.txt", "base\n");
    let (code, v) = s.mdkg(&["graph", "sync", "--json"]);
    assert_eq!(code, 0, "{v}");
    s.advance_remote(".mdkg/notes.txt", "remote\n");
    s.plant(".mdkg/notes.txt", "local\n");
    let (code, v) = s.mdkg_timed(&["graph", "sync", "--json"]);
    assert_eq!(code, 0, "{v}");
    let worker = [("MDKG_ACTOR", "worker")];
    let (code, v) = s.mdkg_env(&["claim", "--new", ".mdkg/notes.txt", "--json"], &worker);
    assert_eq!(code, 0, "{v}");
    let resolve = [
        "graph",
        "resolve",
        ".mdkg/notes.txt",
        "--take",
        "local",
        "--json",
    ];
    let (code, v) = s.mdkg_timed(&resolve);
    assert_eq!(code, 5, "{v}");
    assert_eq!(codes(&v), ["coord.lease_held"], "{v}");
    // `--as` names the resolver: the worker's own folder or MDKG_ACTOR
    // doesn't make the worker's lease the resolver's (Terra #17986).
    let mut as_reviewer = vec!["--as", "reviewer"];
    as_reviewer.extend(resolve);
    let (code, v) = s.mdkg_env(&as_reviewer, &worker);
    assert_eq!(code, 5, "{v}");
    assert_eq!(codes(&v), ["coord.lease_held"], "{v}");
    assert_eq!(
        s.git(&["show", &format!("{GRAPH}:.mdkg/notes.txt")]),
        "remote"
    );
    let (_, v) = s.mdkg(&["graph", "conflicts", "--json"]);
    assert_eq!(
        v["data"]["conflicts"].as_array().map(Vec::len),
        Some(1),
        "{v}"
    );
    let (code, v) = s.mdkg_env(&["release", ".mdkg/notes.txt", "--json"], &worker);
    assert_eq!(code, 0, "{v}");
    let (code, v) = s.mdkg_timed(&resolve);
    assert_eq!(code, 0, "{v}");
    assert_eq!(
        s.git(&["show", &format!("{GRAPH}:.mdkg/notes.txt")]),
        "local"
    );
}

#[test]
fn the_real_scanner_is_not_fooled_by_a_replace_ref() {
    let s = Scratch::setup();
    let (code, v) = s.mdkg(&["graph", "remote", "approve", "--json"]);
    assert_eq!(code, 0, "{v}");
    let token = concat!("ghp_", "abcdefghijklmnopqrstuvwxyz0123456789");
    s.plant(".mdkg/notes.txt", "harmless\n");
    let harmless = s.git(&["rev-parse", &format!("{GRAPH}:.mdkg/notes.txt")]);
    let planted = s.plant(".mdkg/leak.txt", &format!("token: {token}\n"));
    let secret = s.git(&["rev-parse", &format!("{planted}:.mdkg/leak.txt")]);
    s.git(&["replace", &secret, &harmless]);
    let (code, v) = s.mdkg(&["graph", "sync", "--json"]);
    assert_ne!(code, 0, "{v}");
    assert_eq!(codes(&v), ["graph.secret_scan_unavailable"], "{v}");
    assert!(!v.to_string().contains(token), "{v}");
    assert_eq!(s.remote_tip(), None);
}

impl Scratch {
    /// A worker `w` on the fake Claude harness, with one message waiting.
    fn worker(&self) {
        let (code, v) = self.mdkg(&[
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
        assert_eq!(code, 0, "{v}");
        let (code, v) = self.mdkg(&[
            "--as",
            "orchestrator",
            "msg",
            "send",
            "--to",
            "w",
            "--kind",
            "note",
            "--text",
            "go",
            "--json",
        ]);
        assert_eq!(code, 0, "{v}");
    }

    /// `agents up w --max-turns 1` as the launcher (not `--once`, so the
    /// sync timers run): its exit code and stderr.
    fn up(&self) -> (i32, String) {
        self.up_with("")
    }

    /// [`Scratch::up`], with `before` run by the harness (a shell script)
    /// at the start of the turn, before the fake harness.
    fn up_with(&self, before: &str) -> (i32, String) {
        self.up_as(before, &["w"], &[])
    }

    /// [`Scratch::up_with`] for `agents`, with `env` set.
    fn up_as(&self, before: &str, agents: &[&str], env: &[(&str, &str)]) -> (i32, String) {
        let fake =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../mdkg-run/tests/fixtures/fake-claude.sh");
        let fake = if before.is_empty() {
            fake
        } else {
            let wrapper = self.0.join("harness.sh");
            fs::write(
                &wrapper,
                format!("#!/bin/sh\n{before}\nexec '{}' \"$@\"\n", fake.display()),
            )
            .expect("wrapper");
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).expect("chmod");
            wrapper
        };
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_mdkg"));
        self.isolated(&mut cmd)
            .current_dir(self.repo())
            .env("MDKG_RUN_CLAUDE", fake)
            .env("MDKG_WAIT_POLL_MS", "20")
            .env("MDKG_RUN_SECURITY", "/usr/bin/false")
            .env_remove("MDKG_RUN_MDKG")
            .envs(env.iter().copied())
            .args(["--as", "launcher", "agents", "up"])
            .args(agents)
            .args(["--max-turns", "1", "--wait", "1"]);
        let out = cmd.output().expect("mdkg runs");
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }
}

#[test]
fn the_launcher_never_syncs_an_unapproved_remote() {
    let s = Scratch::setup();
    s.worker();
    let theirs = s.advance_remote(".mdkg/notes.txt", "remote\n");
    let before = s.git(&["rev-parse", GRAPH]);
    let (code, err) = s.up();
    assert_eq!(code, 0, "{err}");
    // Neither pulled (DECISION 4) nor pushed.
    assert_eq!(s.remote_tip(), Some(theirs), "{err}");
    assert_eq!(
        s.git(&["merge-base", "--is-ancestor", &before, GRAPH]),
        "",
        "{err}"
    );
    assert!(
        s.git(&["rev-list", &format!("{before}..{GRAPH}")])
            .lines()
            .all(|c| c != s.remote_tip().unwrap_or_default()),
        "the remote's commit wasn't pulled: {err}"
    );
}

#[test]
fn the_launcher_never_syncs_inside_an_agent_turn() {
    // DECISION 19 (T1): `agents up` started inside a turn runs without
    // sync, and says so once.
    let s = Scratch::setup();
    let (code, v) = s.mdkg(&["graph", "remote", "approve", "--json"]);
    assert_eq!(code, 0, "{v}");
    let (code, v) = s.mdkg(&["graph", "sync", "--json"]);
    assert_eq!(code, 0, "{v}");
    s.worker();
    let theirs = s.advance_remote(".mdkg/notes.txt", "remote\n");
    s.plant(".mdkg/mine.txt", "local\n");
    let mine = s.git(&["rev-parse", GRAPH]);
    let ran = s.0.join("turn-ran");
    let (code, err) = s.up_as(
        &format!("touch '{}'", ran.display()),
        &["w"],
        &[("MDKG_RUN_TURN", "1")],
    );
    // It runs, without sync (Terra #19617).
    assert_eq!(code, 0, "{err}");
    assert!(ran.exists(), "the worker's turn ran: {err}");
    assert_eq!(
        err.matches("graph sync is off for this run").count(),
        1,
        "{err}"
    );
    assert!(err.contains("graph.sync_in_turn"), "{err}");
    assert!(!err.contains("graph pulled"), "{err}");
    assert_eq!(
        s.remote_tip(),
        Some(theirs.clone()),
        "nothing pushed: {err}"
    );
    assert_ne!(
        s.git_status(&["merge-base", "--is-ancestor", &theirs, GRAPH]),
        0,
        "nothing pulled: {err}"
    );
    assert_eq!(s.git(&["rev-parse", GRAPH]), mine, "{err}");
}

#[test]
fn bad_arguments_fail_before_the_first_pull() {
    // DECISION 19 (T1): `ready`'s checks of the arguments alone run before
    // the start pull, so a typo never touches the network.
    let s = Scratch::setup();
    let (code, v) = s.mdkg(&["graph", "remote", "approve", "--json"]);
    assert_eq!(code, 0, "{v}");
    let (code, v) = s.mdkg(&["graph", "sync", "--json"]);
    assert_eq!(code, 0, "{v}");
    s.worker();
    let theirs = s.advance_remote(".mdkg/notes.txt", "remote\n");
    let (code, err) = s.up_as("", &["w", "w"], &[]);
    assert_ne!(code, 0, "{err}");
    assert!(err.contains("duplicate selected agent"), "{err}");
    assert!(!err.contains("graph pulled"), "{err}");
    assert_ne!(
        s.git_status(&["merge-base", "--is-ancestor", &theirs, GRAPH]),
        0,
        "nothing pulled: {err}"
    );
}

#[test]
fn the_launcher_pulls_before_turns_and_pushes_while_they_run() {
    // Terra #18237: the start pull finishes before a turn is admitted; the
    // timers run while turns do and read the settings on each tick.
    let s = Scratch::setup();
    s.git(&["config", "mdkg.graph.sync.pushDelay", "600"]);
    let (code, v) = s.mdkg(&["graph", "remote", "approve", "--json"]);
    assert_eq!(code, 0, "{v}");
    let (code, v) = s.mdkg(&["graph", "sync", "--json"]);
    assert_eq!(code, 0, "{v}");
    s.worker();
    let theirs = s.advance_remote(".mdkg/notes.txt", "remote\n");
    // A local change too, so the pull merges and leaves a push to make.
    s.plant(".mdkg/mine.txt", "local\n");
    let repo = s.repo().display().to_string();
    let seen = s.0.join("pulled-before-turn");
    // The turn checks the pull came first, shortens the push delay, and
    // runs long enough for the timers to push.
    let (code, err) = s.up_with(&format!(
        "git -C '{repo}' merge-base --is-ancestor {theirs} {GRAPH} && touch '{}'\n\
         git -C '{repo}' config mdkg.graph.sync.pushDelay 1\nsleep 4",
        seen.display()
    ));
    assert_eq!(code, 0, "{err}");
    assert!(seen.exists(), "the turn started before the pull: {err}");
    assert!(err.contains("graph pulled from mine"), "{err}");
    // The remote's commit came in, and everything local went out.
    s.git(&["merge-base", "--is-ancestor", &theirs, GRAPH]);
    assert_eq!(s.remote_tip(), Some(s.git(&["rev-parse", GRAPH])), "{err}");
    assert!(err.contains("graph pushed to mine"), "{err}");
}

#[test]
fn turns_are_prepared_from_the_pulled_definitions() {
    // Terra #18984: the start pull runs before `up` reads the definitions
    // turns are prepared from, so a remote change to a worker applies to
    // its first turn.
    let s = Scratch::setup();
    let (code, v) = s.mdkg(&["graph", "remote", "approve", "--json"]);
    assert_eq!(code, 0, "{v}");
    s.worker();
    let (code, v) = s.mdkg(&["graph", "sync", "--json"]);
    assert_eq!(code, 0, "{v}");
    let mut def: Value =
        serde_json::from_str(&s.git(&["show", &format!("{GRAPH}:.mdkg/agents/w.json")]))
            .expect("definition");
    def["model"] = Value::from("remote-model-x");
    s.advance_remote(
        ".mdkg/agents/w.json",
        &serde_json::to_string_pretty(&def).expect("json"),
    );
    let args = s.0.join("args.txt");
    let (code, err) = s.up_with(&format!("printf '%s\\n' \"$@\" > '{}'", args.display()));
    assert_eq!(code, 0, "{err}");
    let args = fs::read_to_string(&args).expect("the turn ran");
    assert!(args.lines().any(|l| l == "remote-model-x"), "{args}\n{err}");
}

#[test]
fn a_remote_configured_while_the_launcher_runs_is_synced() {
    // Terra #18984: `up` started local only keeps its timers, and syncs
    // once a (pinned) remote is configured.
    let s = Scratch::setup();
    s.git(&["config", "mdkg.graph.sync.pushDelay", "1"]);
    let (code, v) = s.mdkg(&["graph", "remote", "approve", "--json"]);
    assert_eq!(code, 0, "{v}");
    let (code, v) = s.mdkg(&["graph", "sync", "--json"]);
    assert_eq!(code, 0, "{v}");
    s.git(&["config", "--unset", "mdkg.graph.remote"]);
    s.worker();
    let repo = s.repo().display().to_string();
    let (code, err) = s.up_with(&format!(
        "git -C '{repo}' config mdkg.graph.remote mine\nsleep 4"
    ));
    assert_eq!(code, 0, "{err}");
    assert!(err.contains("graph pushed to mine"), "{err}");
    s.git(&[
        "merge-base",
        "--is-ancestor",
        &s.remote_tip().expect("pushed"),
        GRAPH,
    ]);
}

#[test]
fn a_waiting_push_is_dropped_when_the_launcher_stops() {
    // 2026-11-04: no push when `up` stops; the next `up` or a
    // person's sync sends it.
    let s = Scratch::setup();
    s.git(&["config", "mdkg.graph.sync.pushDelay", "600"]);
    let (code, v) = s.mdkg(&["graph", "remote", "approve", "--json"]);
    assert_eq!(code, 0, "{v}");
    let (code, v) = s.mdkg(&["graph", "sync", "--json"]);
    assert_eq!(code, 0, "{v}");
    let pushed = s.remote_tip();
    s.worker();
    s.plant(".mdkg/mine.txt", "local\n");
    let (code, err) = s.up();
    assert_eq!(code, 0, "{err}");
    assert!(!err.contains("graph pushed"), "{err}");
    assert_eq!(s.remote_tip(), pushed, "{err}");
}

#[test]
fn the_launcher_never_pushes_to_a_changed_destination() {
    let s = Scratch::setup();
    let (code, v) = s.mdkg(&["graph", "remote", "approve", "--json"]);
    assert_eq!(code, 0, "{v}");
    s.git_in(&s.0, &["init", "-q", "--bare", "elsewhere.git"]);
    s.git(&[
        "remote",
        "set-url",
        "mine",
        &s.0.join("elsewhere.git").to_string_lossy(),
    ]);
    s.worker();
    let (code, err) = s.up();
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        s.git_in(&s.0.join("elsewhere.git"), &["for-each-ref"]),
        "",
        "nothing reached the new destination: {err}"
    );
    assert_eq!(s.remote_tip(), None, "{err}");
}

#[test]
fn approval_reads_the_config_the_push_uses() {
    // Terra #18368: network git clears GIT_CONFIG_GLOBAL, so the default
    // global config (here, an insteadOf to `elsewhere.git`) decides where
    // it goes. Approval must read that same configuration, not the
    // alternate global file this process names (`/dev/null` here).
    let s = Scratch::setup();
    s.git_in(&s.0, &["init", "-q", "--bare", "elsewhere.git"]);
    let mine = s.0.join("mine.git").display().to_string();
    let elsewhere = s.0.join("elsewhere.git").display().to_string();
    fs::write(
        s.0.join("home/.gitconfig"),
        format!("[url \"{elsewhere}\"]\n\tinsteadOf = {mine}\n"),
    )
    .expect("global config");
    let (code, v) = s.mdkg(&["graph", "remote", "approve", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["data"]["push"], serde_json::json!([elsewhere]), "{v}");
    let (code, v) = s.mdkg(&["graph", "sync", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["data"]["pushed"], true, "{v}");
    assert_eq!(s.remote_tip(), None, "nothing went to the unapproved one");
    assert_eq!(
        s.git_in(
            &s.0.join("elsewhere.git"),
            &["for-each-ref", "--format=%(refname)"]
        ),
        GRAPH
    );
}
