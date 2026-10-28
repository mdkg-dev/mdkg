//! The external lease authority: when the user configures one for a
//! repository, claim, release and renew ask it, pass its envelope through,
//! and refuse on anything else; the local store never grants a lease.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use mdkg_format::Digest;
use serde_json::{Value, json};

mod common;

/// The timeout for an authority a test expects to answer. Far above the
/// 10 s default: under load, macOS can hold a freshly written script for
/// seconds while it assesses it, and those tests aren't about timing. The
/// tests of the timeout itself set their own short ones.
const ANSWER_MS: u64 = 120_000;
const LOCAL_TURN_CHILD: &str = "MDKG_TEST_LOCAL_TURN_RENEW";
const LOCAL_TURN_STARTUP: std::time::Duration = std::time::Duration::from_secs(30);

fn local_turn_child(program: &Path, root: &Path, config: &Path) -> Command {
    let mut command = Command::new(program);
    isolated(&mut command)
        .args([
            "--exact",
            "receiver_local_turn_renewal_never_starts_configured_authority",
        ])
        .env(LOCAL_TURN_CHILD, root)
        .env("XDG_CONFIG_HOME", config)
        // Set before the child's first claim creates the store. The shared
        // helper's one-second fixture lease would expire during startup.
        .env("MDKG_LEASE_TTL_MS", ANSWER_MS.to_string())
        .env_remove("MDKG_NOW_MS");
    command
}

// This integration case checks authority routing, not how fast the OS starts
// three real CLI processes inside a renewal's unchanged one-second bound.
// A verified renewal is ready; only its bounded process-start timeout retries.
fn local_turn_ready(
    allowance: std::time::Duration,
    mut renew: impl FnMut() -> Result<u64, mdkg_run::RunError>,
) -> Result<u64, mdkg_run::RunError> {
    let start = std::time::Instant::now();
    loop {
        match renew() {
            Err(error)
                if error.code == mdkg_run::error::CODE_MAIL_FAILED
                    && error.message.contains("turn lease check timed out")
                    && start.elapsed() < allowance =>
            {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            result => return result,
        }
    }
}

#[test]
fn local_turn_readiness_waits_for_verified_renewal() {
    let mut attempts = 0;
    let deadline = local_turn_ready(LOCAL_TURN_STARTUP, || {
        attempts += 1;
        if attempts < 3 {
            Err(mdkg_run::RunError::failure(
                mdkg_run::error::CODE_MAIL_FAILED,
                "can't run fixture: turn lease check timed out",
            ))
        } else {
            Ok(123)
        }
    })
    .unwrap();
    assert_eq!(deadline, 123);
    assert_eq!(attempts, 3);
}

#[test]
fn local_turn_readiness_never_retries_other_failures() {
    for (code, message) in [
        (
            mdkg_run::error::CODE_MAIL_FAILED,
            "receiver turn lease expired or changed holder",
        ),
        ("coord.authority_failed", "turn lease check timed out"),
        (mdkg_run::error::CODE_MAIL_FAILED, "invalid JSON contract"),
    ] {
        let error = mdkg_run::RunError::failure(code, message);
        let mut attempts = 0;
        let result = local_turn_ready(LOCAL_TURN_STARTUP, || {
            attempts += 1;
            Err(error.clone())
        });
        assert_eq!(result, Err(error));
        assert_eq!(attempts, 1);
    }
}

#[test]
fn local_turn_readiness_stops_at_its_startup_allowance() {
    let error = mdkg_run::RunError::failure(
        mdkg_run::error::CODE_MAIL_FAILED,
        "can't run fixture: turn lease check timed out",
    );
    let mut attempts = 0;
    let result = local_turn_ready(std::time::Duration::ZERO, || {
        attempts += 1;
        Err(error.clone())
    });
    assert_eq!(result, Err(error));
    assert_eq!(attempts, 1);
}

#[test]
fn local_turn_readiness_child_lease_outlasts_startup_allowance() {
    let command = local_turn_child(
        Path::new("/fixture/test"),
        Path::new("/fixture/repo"),
        Path::new("/fixture/config"),
    );
    let ttl = command
        .get_envs()
        .find(|(name, _)| *name == "MDKG_LEASE_TTL_MS")
        .and_then(|(_, value)| value)
        .unwrap()
        .to_str()
        .unwrap()
        .parse::<u64>()
        .unwrap();
    assert!(std::time::Duration::from_millis(ttl) > LOCAL_TURN_STARTUP);
    assert_eq!(
        command
            .get_envs()
            .find(|(name, _)| *name == "MDKG_NOW_MS")
            .unwrap()
            .1,
        None
    );
}

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).ok();
    }
}

fn scratch() -> Scratch {
    let dir = common::temp::dir("mdkg-cli-authority");
    Scratch(fs::canonicalize(dir).expect("canonical"))
}

fn copy_tree(src: &Path, dest: &Path) {
    for entry in fs::read_dir(src).expect("read dir") {
        let entry = entry.expect("entry");
        let target = dest.join(entry.file_name());
        if entry.file_type().expect("type").is_dir() {
            fs::create_dir_all(&target).expect("mkdir");
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).expect("copy");
        }
    }
}

/// A repository, a config home, and a fake authority whose answer each
/// case sets.
struct Setup {
    dir: Scratch,
    repo: PathBuf,
    config: PathBuf,
    script: PathBuf,
}

impl Setup {
    fn new() -> Self {
        let dir = scratch();
        let repo = dir.0.join("repo");
        let conformance = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance");
        fs::create_dir_all(&repo).expect("repo");
        copy_tree(&conformance.join("base"), &repo);
        copy_tree(&conformance.join("cases/g01-valid-basic"), &repo);
        fs::remove_file(repo.join("commands")).ok();
        fs::remove_file(repo.join("README.md")).ok();
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["add", "."],
            &["commit", "-q", "-m", "init"],
        ] {
            let out = isolated(Command::new("git").arg("-C").arg(&repo).args(args))
                .output()
                .expect("git");
            assert!(out.status.success(), "git {args:?}");
        }
        let config = dir.0.join("config");
        let script = dir.0.join("authority.sh");
        let setup = Setup {
            dir,
            repo,
            config,
            script,
        };
        // It records its arguments, prints `answer.json` and exits with
        // the code in `code`.
        let d = setup.dir.0.display();
        fs::write(
            &setup.script,
            format!(
                "#!/bin/sh\necho \"$@\" >> {d}/args.log\ncat {d}/answer.json 2>/dev/null\nexit $(cat {d}/code 2>/dev/null || echo 0)\n"
            ),
        )
        .expect("script");
        fs::set_permissions(&setup.script, fs::Permissions::from_mode(0o755)).expect("chmod");
        setup
    }

    fn common_dir(&self) -> String {
        let out = isolated(Command::new("git").arg("-C").arg(&self.repo).args([
            "rev-parse",
            "--path-format=absolute",
            "--git-common-dir",
        ]))
        .output()
        .expect("git");
        let dir = String::from_utf8_lossy(&out.stdout).trim().to_string();
        fs::canonicalize(dir)
            .expect("canonical")
            .to_string_lossy()
            .into_owned()
    }

    fn folder(&self) -> PathBuf {
        self.config.join("mdkg").join("lease-authority")
    }

    fn file(&self) -> PathBuf {
        let key = Digest::of_bytes(self.common_dir().as_bytes());
        self.folder().join(format!("{}.json", key.hex()))
    }

    /// Configures `command` as the authority.
    fn configure(&self, command: Value, timeout_ms: u64) {
        fs::create_dir_all(self.folder()).expect("folder");
        for f in [self.config.join("mdkg"), self.folder()] {
            fs::set_permissions(&f, fs::Permissions::from_mode(0o700)).expect("chmod");
        }
        let file = self.file();
        fs::write(
            &file,
            json!({"common_dir": self.common_dir(), "command": command, "timeout_ms": timeout_ms})
                .to_string(),
        )
        .expect("file");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).expect("chmod");
    }

    fn use_script(&self) {
        self.configure(json!([self.script.to_string_lossy()]), ANSWER_MS);
    }

    fn answer(&self, envelope: Value, code: i32) {
        fs::write(self.dir.0.join("answer.json"), envelope.to_string()).expect("answer");
        fs::write(self.dir.0.join("code"), code.to_string()).expect("code");
    }

    fn args(&self) -> String {
        fs::read_to_string(self.dir.0.join("args.log")).unwrap_or_default()
    }

    fn run(&self, actor: &str, args: &[&str]) -> (i32, Value) {
        let out = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
            .arg("--root")
            .arg(&self.repo)
            .args(["--as", actor])
            .args(args)
            .arg("--json")
            .env("XDG_CONFIG_HOME", &self.config)
            .env("MDKG_NOW_MS", "1000")
            .output()
            .expect("run mdkg");
        let envelope: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|_| {
            panic!(
                "not JSON: {}\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
        });
        common::conforms_envelope(&envelope);
        (out.status.code().unwrap_or(-1), envelope)
    }

    /// Leases in the local store, read with the authority switched off.
    fn local_leases(&self) -> Vec<Value> {
        let saved = self.config.with_file_name("config-off");
        let moved = fs::rename(&self.config, &saved).is_ok();
        let (_, out) = self.run("a", &["status", "--all"]);
        if moved {
            fs::rename(&saved, &self.config).expect("restore");
        }
        out["data"]["leases"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }
}

fn isolated(cmd: &mut Command) -> &mut Command {
    common::without_launcher_env(cmd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .env("MDKG_LEASE_TTL_MS", "1000")
}

fn codes(envelope: &Value) -> Vec<String> {
    envelope["diagnostics"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|d| d["code"].as_str().map(String::from))
        .collect()
}

fn lease(scope: &str) -> Value {
    json!({
        "id": "lease-remote-1",
        "owner": "a",
        "scope": scope,
        "status": "active",
        "until": "1970-01-01T00:00:02Z",
        "deadline_ms": 2000,
        "mode": "exclusive"
    })
}

#[test]
fn claims_release_and_renew_go_through_the_authority() {
    let s = Setup::new();
    s.use_script();
    let answer = json!({
        "schema": "mdkg.claim.v1",
        "ok": true,
        "data": {"lease": lease(".mdkg/work/task-1-basic.md"), "renewed": false},
        "diagnostics": []
    });
    s.answer(answer.clone(), 0);
    let (code, out) = s.run("a", &["claim", "task-1"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["data"], answer["data"], "passed through");
    // Resolved here: the canonical scope, its mode and the node.
    assert_eq!(
        s.args().trim(),
        "claim .mdkg/work/task-1-basic.md --mode exclusive --node root:task-1 --as a --json"
    );

    s.answer(
        json!({"schema": "mdkg.release.v1", "ok": true,
               "data": {"lease": lease("x")}, "diagnostics": []}),
        0,
    );
    let (code, _) = s.run("a", &["release", "task-1"]);
    assert_eq!(code, 0);
    s.answer(
        json!({"schema": "mdkg.renew.v1", "ok": true,
               "data": {"renewed": [lease("x")]}, "diagnostics": []}),
        0,
    );
    let (code, out) = s.run("a", &["renew"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["data"]["renewed"][0]["id"], "lease-remote-1");
    let log = s.args();
    assert!(
        log.contains("release --scope .mdkg/work/task-1-basic.md --as a --json"),
        "{log}"
    );
    assert!(log.contains("renew --as a --json"), "{log}");

    // A refusal comes through as it is, exit code and all.
    s.answer(
        json!({"schema": "mdkg.claim.v1", "ok": false, "diagnostics": [
            {"code": "coord.lease_held", "severity": "error", "message": "held by b"}]}),
        5,
    );
    let (code, out) = s.run("a", &["claim", "task-1", "--new"]);
    assert_eq!(
        (code, codes(&out)),
        (5, vec!["coord.lease_held".to_string()])
    );
    // An ambiguous handle's refusal too (exit 7, task-41), its candidates
    // kept.
    s.answer(
        json!({"schema": "mdkg.claim.v1", "ok": false, "diagnostics": [
            {"code": "node.ambiguous", "severity": "error", "message": "ambiguous id",
             "hint": "matches: root:task-1  One; ws:task-1  Two"}]}),
        7,
    );
    let (code, out) = s.run("a", &["claim", "task-1", "--new"]);
    assert_eq!(
        (code, codes(&out)),
        (7, vec!["node.ambiguous".to_string()]),
        "{out}"
    );
    assert!(
        out["diagnostics"][0]["hint"]
            .as_str()
            .is_some_and(|h| h.contains("ws:task-1")),
        "{out}"
    );

    // status stays local and says so.
    let (code, out) = s.run("a", &["status"]);
    assert_eq!(code, 0);
    assert!(codes(&out).contains(&"coord.authority_remote".to_string()));

    assert!(s.local_leases().is_empty(), "nothing granted locally");
}

#[test]
fn an_authority_lease_id_is_never_read_as_a_title() {
    // Codex, 4925bd9: `release` takes node handles (RC1 audit U1), but a
    // name shaped like a lease id goes to the authority as that id, even
    // when a node's title contains it.
    let s = Setup::new();
    s.use_script();
    let file = s.repo.join(".mdkg/work/task-1-basic.md");
    let text = fs::read_to_string(&file).expect("node");
    fs::write(
        &file,
        text.replacen("title: Basic task", "title: Investigate lease-remote-1", 1),
    )
    .expect("title");
    s.answer(
        json!({"schema": "mdkg.release.v1", "ok": true,
               "data": {"lease": lease(".mdkg/work/task-1-basic.md")}, "diagnostics": []}),
        0,
    );
    let (code, out) = s.run("a", &["release", "lease-remote-1"]);
    assert_eq!(code, 0, "{out}");
    let log = s.args();
    assert_eq!(log.trim(), "release lease-remote-1 --as a --json", "{log}");
}

#[test]
fn every_failure_refuses_and_grants_nothing() {
    let s = Setup::new();
    let refuses = |why: &str| {
        let (code, out) = s.run("a", &["claim", "task-1"]);
        assert_eq!(
            (code, codes(&out)),
            (4, vec!["coord.authority_failed".to_string()]),
            "{why}: {out}"
        );
    };
    let good = json!({
        "schema": "mdkg.claim.v1",
        "ok": true,
        "data": {"lease": lease("x"), "renewed": false},
        "diagnostics": []
    });

    s.configure(
        json!([s.dir.0.join("missing").to_string_lossy()]),
        ANSWER_MS,
    );
    refuses("a missing command");

    s.use_script();
    s.answer(json!({}), 3);
    refuses("a non-zero exit without an envelope");
    fs::write(s.dir.0.join("answer.json"), "not json").expect("answer");
    fs::write(s.dir.0.join("code"), "0").expect("code");
    refuses("malformed output");
    let mut wrong = good.clone();
    wrong["schema"] = json!("mdkg.release.v1");
    s.answer(wrong, 0);
    refuses("another command's schema");
    s.answer(good.clone(), 4);
    refuses("ok with a failing exit");
    let mut warnings_only = good.clone();
    warnings_only["ok"] = json!(false);
    warnings_only["diagnostics"] =
        json!([{"code": "coord.lease_shared", "severity": "warning", "message": "m"}]);
    s.answer(warnings_only, 5);
    refuses("not ok without an error");

    let script = |name: &str, body: &str| {
        let path = s.dir.0.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("script");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");
        path.to_string_lossy().into_owned()
    };
    s.configure(json!([script("slow.sh", "sleep 5")]), 300);
    let started = std::time::Instant::now();
    refuses("a timeout");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(3),
        "killed"
    );
    // A child left holding the output open doesn't stretch the timeout.
    let answer = good.to_string();
    s.configure(
        json!([script(
            "held.sh",
            &format!("echo '{answer}'\nsleep 8 &\nexit 0")
        )]),
        500,
    );
    let started = std::time::Instant::now();
    refuses("output held open");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(3),
        "not waited out"
    );

    // The program is an absolute path outside the repository.
    s.configure(json!(["authority.sh"]), ANSWER_MS);
    refuses("a bare command found on PATH");
    let inside = s.repo.join("authority.sh");
    fs::copy(&s.script, &inside).expect("copy");
    fs::set_permissions(&inside, fs::Permissions::from_mode(0o755)).expect("chmod");
    s.configure(json!([inside.to_string_lossy()]), ANSWER_MS);
    refuses("a command inside the repository");
    fs::remove_file(&inside).expect("clean");

    // The file must be the user's own, unshared, and for this repository.
    s.use_script();
    s.answer(good, 0);
    let file = s.file();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o620)).expect("chmod");
    refuses("a group-writable file");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).expect("chmod");
    fs::set_permissions(s.folder(), fs::Permissions::from_mode(0o777)).expect("chmod");
    refuses("a world-writable folder");
    fs::set_permissions(s.folder(), fs::Permissions::from_mode(0o700)).expect("chmod");
    let real = file.with_extension("real");
    fs::rename(&file, &real).expect("move");
    std::os::unix::fs::symlink(&real, &file).expect("symlink");
    refuses("a symlinked file");
    fs::remove_file(&file).expect("unlink");
    fs::rename(&real, &file).expect("restore");
    let mut text: Value =
        serde_json::from_str(&fs::read_to_string(&file).expect("read")).expect("json");
    text["common_dir"] = json!("/somewhere/else/.git");
    fs::write(&file, text.to_string()).expect("write");
    refuses("another repository's file");
    text["common_dir"] = json!(s.common_dir());
    text["shell"] = json!(true);
    fs::write(&file, text.to_string()).expect("write");
    refuses("an unknown key");

    // Links on the way are refused, dangling ones too.
    s.use_script();
    let file = s.file();
    fs::remove_file(&file).expect("unlink");
    std::os::unix::fs::symlink(s.dir.0.join("gone.json"), &file).expect("dangling");
    refuses("a dangling file link");
    fs::remove_file(&file).expect("unlink");
    let folder = s.folder();
    let real = s.dir.0.join("real-folder");
    fs::rename(&folder, &real).expect("move");
    std::os::unix::fs::symlink(&real, &folder).expect("link");
    s.use_script();
    refuses("a symlinked folder");

    assert!(s.local_leases().is_empty(), "nothing granted locally");
}

#[test]
fn a_committed_authority_is_ignored_with_a_warning() {
    let s = Setup::new();
    let path = s.repo.join(".mdkg/config.json");
    let mut config: Value =
        serde_json::from_str(&fs::read_to_string(&path).expect("config")).expect("json");
    config["coordination"] = json!({"lease_authority": {"command": [s.script.to_string_lossy()]}});
    fs::write(&path, serde_json::to_string_pretty(&config).expect("json")).expect("write");
    let (code, out) = s.run("a", &["claim", "task-1"]);
    assert_eq!(code, 0, "{out}");
    assert!(codes(&out).contains(&"coord.authority_ignored".to_string()));
    assert_eq!(s.args(), "", "never run");
    assert_eq!(s.local_leases().len(), 1, "claimed locally");
}

#[test]
fn unknown_codes_pass_through_and_options_are_refused() {
    let s = Setup::new();
    s.use_script();
    s.answer(
        json!({
            "schema": "mdkg.claim.v1",
            "ok": true,
            "data": {"lease": lease("x"), "renewed": false},
            "diagnostics": [{"code": "coord.lease_expiring", "severity": "warning", "message": "soon"}]
        }),
        0,
    );
    let (code, out) = s.run("a", &["claim", "task-1"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(codes(&out), vec!["coord.authority_note".to_string()]);
    assert!(
        out["diagnostics"][0]["message"]
            .as_str()
            .is_some_and(|m| m.starts_with("coord.lease_expiring"))
    );

    // An actor that looks like an option never reaches the authority.
    let before = s.args();
    let out = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
        .arg("--root")
        .arg(&s.repo)
        .args(["--as=-v", "claim", "task-1", "--json"])
        .env("XDG_CONFIG_HOME", &s.config)
        .output()
        .expect("run");
    let envelope: Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(out.status.code(), Some(1), "{envelope}");
    assert_eq!(s.args(), before, "never run with it");
}

/// Roles and turns never go to the authority (orchestrator, 2026-10-21):
/// mailboxes and turns are per node and per store.
#[test]
fn roles_and_turns_stay_local() {
    let s = Setup::new();
    s.use_script();
    for scope in ["role:reviewer", "run:codex"] {
        let (code, out) = s.run("a", &["claim", scope]);
        assert_eq!(code, 0, "{out}");
        assert_eq!(out["data"]["lease"]["scope"], scope);
        let (code, out) = s.run("a", &["release", scope]);
        assert_eq!(code, 0, "{out}");
    }
    assert_eq!(s.args(), "", "never asked");
    // renew renews them here, and the rest through the authority.
    s.run("a", &["claim", "run:codex"]);
    s.answer(
        json!({"schema": "mdkg.renew.v1", "ok": true,
               "data": {"renewed": [lease("x")]}, "diagnostics": []}),
        0,
    );
    let (code, out) = s.run("a", &["renew"]);
    assert_eq!(code, 0, "{out}");
    let scopes: Vec<_> = out["data"]["renewed"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|l| l["scope"].as_str())
        .collect();
    assert_eq!(scopes, vec!["x", "run:codex"]);
    // With the authority unreachable, the turn still renews.
    s.configure(
        json!([s.dir.0.join("missing").to_string_lossy()]),
        ANSWER_MS,
    );
    let (code, out) = s.run("a", &["renew"]);
    assert_eq!(code, 4, "{out}");
    assert!(
        codes(&out).contains(&"coord.authority_remote".to_string()),
        "{out}"
    );
}

/// done, update and goal edits ask the authority whether the actor holds
/// the node (orchestrator, 2026-10-21, `holds`); the edit stays local.
#[test]
fn edits_need_the_authoritys_hold() {
    let s = Setup::new();
    s.use_script();
    let holds = |held: bool| {
        json!({"schema": "mdkg.holds.v1", "ok": true,
               "data": if held { json!({"holds": true, "lease": lease(".mdkg/work/task-1-basic.md")}) }
                       else { json!({"holds": false}) },
               "diagnostics": []})
    };
    s.answer(holds(false), 0);
    let (code, out) = s.run("a", &["update", "task-1", "--status", "review"]);
    assert_eq!(
        (code, codes(&out)),
        (4, vec!["coord.authority_failed".to_string()]),
        "{out}"
    );
    assert!(
        s.args()
            .contains("holds .mdkg/work/task-1-basic.md --as a --node root:task-1 --json")
    );
    s.answer(holds(true), 0);
    let (code, out) = s.run("a", &["update", "task-1", "--status", "review"]);
    assert_eq!(code, 0, "{out}");
    // A state whose node is gone is cleared by its path, and that needs
    // the hold too.
    let file = s.repo.join(".mdkg/work/task-1-basic.md");
    let kept = fs::read_to_string(&file).expect("node");
    fs::remove_file(&file).expect("remove");
    s.answer(holds(false), 0);
    // A fresh log, so the scope asked about is this call's.
    fs::remove_file(s.dir.0.join("args.log")).ok();
    let (code, out) = s.run("a", &["update", ".mdkg/work/task-1-basic.md", "--clear"]);
    assert_eq!(
        (code, codes(&out)),
        (4, vec!["coord.authority_failed".to_string()]),
        "by path: {out}"
    );
    // The node is gone, so no `--node`.
    assert!(
        s.args()
            .contains("holds .mdkg/work/task-1-basic.md --as a --json")
    );
    s.answer(holds(true), 0);
    let (code, out) = s.run("a", &["update", ".mdkg/work/task-1-basic.md", "--clear"]);
    assert_eq!(code, 0, "held, by path: {out}");
    fs::write(&file, kept).expect("restore");
    s.configure(
        json!([s.dir.0.join("missing").to_string_lossy()]),
        ANSWER_MS,
    );
    let (code, out) = s.run("a", &["update", "task-1", "--clear"]);
    assert_eq!(
        (code, codes(&out)),
        (4, vec!["coord.authority_failed".to_string()]),
        "unreachable refuses: {out}"
    );
}

/// A state an older build stored under a turn's folder (for a graph that
/// sat inside `run:x/`) is never cleared by path from a graph in a
/// subfolder, where the typed path's stored spelling would name it: the
/// lease authority is never asked about such scopes.
#[test]
fn a_state_under_a_turn_s_folder_is_never_cleared_by_path() {
    if common::rerun_without_launcher_env("a_state_under_a_turn_s_folder_is_never_cleared_by_path")
    {
        return;
    }
    let s = Setup::new();
    let conformance = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance");
    let sub = s.repo.join("sub");
    fs::create_dir_all(&sub).expect("sub");
    copy_tree(&conformance.join("base"), &sub);
    copy_tree(&conformance.join("cases/g01-valid-basic"), &sub);
    let leaf = mdkg_coord::Scope::new("run:x/.mdkg/work/a.md");
    let mut store = mdkg_coord_sqlite::Store::open_for_repo(&s.repo, 1000).expect("store");
    store
        .execute(&mdkg_coord::Command::SetWork {
            leaf: leaf.clone(),
            status: Some("review".into()),
            changeset: None,
            actor: "a".into(),
            now: 0,
        })
        .expect("seed");
    drop(store);
    s.use_script();
    s.answer(
        json!({"schema": "mdkg.holds.v1", "ok": true, "data": {"holds": true}, "diagnostics": []}),
        0,
    );
    let out = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
        .arg("--root")
        .arg(&sub)
        .args([
            "--as",
            "a",
            "update",
            "./run:x/.mdkg/work/a.md",
            "--clear",
            "--json",
        ])
        .env("XDG_CONFIG_HOME", &s.config)
        .env("MDKG_NOW_MS", "1000")
        .output()
        .expect("run");
    let envelope: Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(
        (out.status.code(), codes(&envelope)),
        (Some(3), vec!["node.not_found".to_string()]),
        "{envelope}"
    );
    assert!(
        s.args().is_empty(),
        "the authority wasn't asked: {}",
        s.args()
    );
    let state = mdkg_coord_sqlite::Store::open_for_repo(&s.repo, 1000)
        .expect("store")
        .coordinator()
        .expect("state");
    assert!(state.work(&leaf).is_some(), "the state is kept");
}

/// The host's side: a canonical scope with a mode, `holds`, and release by
/// scope, all local (no authority there).
#[test]
fn a_host_grants_what_a_worker_resolved() {
    let s = Setup::new();
    // `--mode` only makes a lease stricter: shared only where the config
    // shares the path.
    let (code, out) = s.run("a", &["claim", "src/", "--mode", "shared"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        out["data"]["lease"]["mode"], "exclusive",
        "not shared by the config"
    );
    s.run("a", &["release", "--scope", "src"]);
    let path = s.repo.join(".mdkg/config.json");
    let mut config: Value =
        serde_json::from_str(&fs::read_to_string(&path).expect("config")).expect("json");
    config["coordination"] = json!({"leases": {"paths": [{"glob": "src/**", "mode": "shared"}]}});
    fs::write(&path, serde_json::to_string_pretty(&config).expect("json")).expect("write");
    let (code, out) = s.run("a", &["claim", "src/", "--mode", "shared"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["data"]["lease"]["mode"], "shared");
    let (code, out) = s.run("a", &["claim", "role:x", "--mode", "shared"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        out["data"]["lease"]["mode"], "exclusive",
        "a role never shared"
    );
    let (code, _) = s.run("a", &["holds", "task-1"]);
    assert_eq!(code, 1, "a bare node id isn't a scope");
    let (code, out) = s.run(
        "a",
        &[
            "claim",
            ".mdkg/work/task-1-basic.md",
            "--mode",
            "shared",
            "--node",
            "task-1",
        ],
    );
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        out["data"]["lease"]["mode"], "exclusive",
        "a node is exclusive"
    );
    let (code, out) = s.run("a", &["holds", ".mdkg/work/task-1-basic.md"]);
    assert_eq!((code, &out["data"]["holds"]), (0, &json!(true)), "{out}");
    let (code, out) = s.run("b", &["holds", ".mdkg/work/task-1-basic.md"]);
    assert_eq!((code, &out["data"]["holds"]), (0, &json!(false)), "{out}");
    let (code, out) = s.run("a", &["release", "--scope", ".mdkg/work/task-1-basic.md"]);
    assert_eq!(code, 0, "{out}");
    for bad in [
        "../x",
        "/etc",
        "role:x/y",
        "a/./b",
        "a\\b",
        ".git",
        ".GIT/config",
    ] {
        let (code, out) = s.run("a", &["claim", bad, "--mode", "exclusive"]);
        assert_ne!(code, 0, "{bad}: {out}");
    }
}

/// Only this worktree can tell what it lacks: a delegated claim of a path
/// nobody has gets the local warning appended.
#[test]
fn local_notes_ride_along() {
    let s = Setup::new();
    s.use_script();
    s.answer(
        json!({"schema": "mdkg.claim.v1", "ok": true,
               "data": {"lease": lease("nowhere.txt"), "renewed": false}, "diagnostics": []}),
        0,
    );
    let (code, out) = s.run("a", &["claim", "gen/nowhere.txt"]);
    assert_eq!(code, 0, "{out}");
    assert!(
        codes(&out).contains(&"coord.path_missing".to_string()),
        "{out}"
    );
}

/// Round 2 of 1af426f: nothing the authority leaves behind keeps running,
/// and a program inside the repository is refused however it's reached.
#[test]
fn the_authority_leaves_nothing_behind_and_lives_outside() {
    let s = Setup::new();
    let marker = s.dir.0.join("late");
    let answer = json!({"schema": "mdkg.claim.v1", "ok": true,
        "data": {"lease": lease("x"), "renewed": false}, "diagnostics": []});
    let late = s.dir.0.join("late.sh");
    fs::write(
        &late,
        format!(
            "#!/bin/sh\necho '{answer}'\n(sleep 1; touch '{}') >/dev/null 2>&1 &\nexit 0\n",
            marker.display()
        ),
    )
    .expect("script");
    fs::set_permissions(&late, fs::Permissions::from_mode(0o755)).expect("chmod");
    s.configure(json!([late.to_string_lossy()]), ANSWER_MS);
    let (code, out) = s.run("a", &["claim", "task-1"]);
    assert_eq!(code, 0, "{out}");
    std::thread::sleep(std::time::Duration::from_millis(1500));
    assert!(!marker.exists(), "the background job was killed");

    // A link outside the repository to a program inside it.
    let inside = s.repo.join("auth.sh");
    fs::copy(&s.script, &inside).expect("copy");
    fs::set_permissions(&inside, fs::Permissions::from_mode(0o755)).expect("chmod");
    let link = s.dir.0.join("outside-link.sh");
    std::os::unix::fs::symlink(&inside, &link).expect("link");
    s.configure(json!([link.to_string_lossy()]), ANSWER_MS);
    let (code, out) = s.run("a", &["claim", "task-1"]);
    assert_eq!(
        (code, codes(&out)),
        (4, vec!["coord.authority_failed".to_string()]),
        "{out}"
    );
    // A program in another worktree of the repository.
    let wt = s.dir.0.join("wt2");
    let made = isolated(
        Command::new("git")
            .arg("-C")
            .arg(&s.repo)
            .args(["worktree", "add", "-q", "-b", "w2"])
            .arg(&wt),
    )
    .status()
    .expect("worktree");
    assert!(made.success());
    let there = wt.join("auth.sh");
    fs::copy(&s.script, &there).expect("copy");
    fs::set_permissions(&there, fs::Permissions::from_mode(0o755)).expect("chmod");
    s.configure(json!([there.to_string_lossy()]), ANSWER_MS);
    let (code, out) = s.run("a", &["claim", "task-1"]);
    assert_eq!(
        (code, codes(&out)),
        (4, vec!["coord.authority_failed".to_string()]),
        "{out}"
    );
}

/// A worker and a host, the authority being the host's own mdkg (as
/// a harness runs it): the host decides with its own rules.
struct Pair {
    worker: Setup,
    host: PathBuf,
}

impl Pair {
    fn new() -> Self {
        let worker = Setup::new();
        let host = worker.dir.0.join("host");
        let out = isolated(
            Command::new("git")
                .arg("clone")
                .arg("-q")
                .arg(&worker.repo)
                .arg(&host),
        )
        .output()
        .expect("clone");
        assert!(out.status.success());
        let wrapper = worker.dir.0.join("host.sh");
        fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nexec '{}' --root '{}' \"$@\"\n",
                env!("CARGO_BIN_EXE_mdkg"),
                host.display()
            ),
        )
        .expect("wrapper");
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).expect("chmod");
        worker.configure(json!([wrapper.to_string_lossy()]), ANSWER_MS);
        Pair { worker, host }
    }
}

#[test]
fn a_worker_and_its_host_agree() {
    let p = Pair::new();
    let s = &p.worker;
    // Two actors can't share a node through the host.
    let (code, out) = s.run("a", &["claim", "task-1"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["data"]["lease"]["mode"], "exclusive");
    let (code, out) = s.run("b", &["claim", "task-1"]);
    assert_eq!(
        (code, codes(&out)),
        (5, vec!["coord.lease_held".to_string()]),
        "{out}"
    );
    // An id the host minted goes back to it.
    let id = {
        let (_, out) = s.run("a", &["claim", "src/a.rs", "--new"]);
        out["data"]["lease"]["id"].as_str().expect("id").to_string()
    };
    let (code, out) = s.run("a", &["release", &id]);
    assert_eq!(
        (code, &out["data"]["lease"]["scope"]),
        (0, &json!("src/a.rs")),
        "{out}"
    );
    // A local role's id is ambiguous with the host's ids.
    let (_, out) = s.run("a", &["claim", "role:x"]);
    let local = out["data"]["lease"]["id"].as_str().expect("id").to_string();
    let (code, out) = s.run("a", &["release", &local]);
    assert_eq!(code, 1, "{out}");
    assert!(
        out["diagnostics"][0]["hint"]
            .as_str()
            .is_some_and(|h| h.contains("release role:x"))
    );
    // --exclusive over your own shared folder is refused there too.
    let path = p.host.join(".mdkg/config.json");
    let mut config: Value =
        serde_json::from_str(&fs::read_to_string(&path).expect("config")).expect("json");
    config["coordination"] = json!({"leases": {"paths": [{"glob": "gen/**", "mode": "shared"}]}});
    fs::write(&path, serde_json::to_string_pretty(&config).expect("json")).expect("write");
    let wpath = s.repo.join(".mdkg/config.json");
    fs::write(&wpath, serde_json::to_string_pretty(&config).expect("json")).expect("write");
    let (code, out) = s.run("a", &["claim", "--new", "gen/"]);
    assert_eq!(
        (code, &out["data"]["lease"]["mode"]),
        (0, &json!("shared")),
        "{out}"
    );
    let (code, out) = s.run("a", &["claim", "--exclusive", "gen/"]);
    assert_eq!(
        (code, codes(&out)),
        (5, vec!["coord.lease_held".to_string()]),
        "{out}"
    );
    // The host's notes don't come back; the worker's do, once.
    let (code, out) = s.run("a", &["claim", "--new", "src/brand_new.rs"]);
    assert_eq!(code, 0, "{out}");
    assert!(
        !codes(&out).contains(&"coord.path_missing".to_string()),
        "{out}"
    );
    // done needs the host's exclusive hold, and releases it there after.
    let t1 = s.repo.join(".mdkg/work/task-1-basic.md");
    let text = fs::read_to_string(&t1).expect("read") + "\nNote.\n";
    fs::write(&t1, text).expect("write");
    let (code, out) = s.run("b", &["done", "task-1"]);
    assert_eq!(
        (code, codes(&out)),
        (4, vec!["coord.authority_failed".to_string()]),
        "b doesn't hold it: {out}"
    );
    let (code, out) = s.run("a", &["done", "task-1"]);
    assert_eq!(code, 0, "{out}");
    let (_, out) = s.run("a", &["holds", ".mdkg/work/task-1-basic.md"]);
    assert_eq!(
        out["data"]["holds"], false,
        "released at the host after done"
    );
}

/// With an authority, renew renews only roles and turns here: a file lease
/// left in the local store from before isn't kept alive by it.
#[test]
fn renew_keeps_only_roles_and_turns_here() {
    let s = Setup::new();
    let (code, _) = s.run("a", &["claim", "src/old.rs", "--new"]);
    assert_eq!(code, 0);
    s.use_script();
    s.answer(
        json!({"schema": "mdkg.renew.v1", "ok": true, "data": {"renewed": []}, "diagnostics": []}),
        0,
    );
    let (code, out) = s.run("a", &["renew"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["data"]["renewed"], json!([]), "{out}");
}

/// Review of 404001a and 5bddc38: ids collide across stores, so only your
/// own active lease here makes one ambiguous; `done` and goal edits release
/// only the node's own lease at the host, never a folder covering it.
#[test]
fn release_by_id_and_after_done_take_only_whats_meant() {
    let p = Pair::new();
    let s = &p.worker;
    let off = s.config.with_file_name("config-off");
    // Your own lease here, from before the authority: its id is ambiguous.
    fs::rename(&s.config, &off).expect("off");
    let (code, out) = s.run("a", &["claim", "src/old.rs", "--new"]);
    assert_eq!(code, 0, "{out}");
    let old = out["data"]["lease"]["id"].as_str().expect("id").to_string();
    fs::rename(&off, &s.config).expect("on");
    // Someone else's role here can't be what you meant.
    let (code, out) = s.run("b", &["claim", "role:x"]);
    assert_eq!(code, 0, "{out}");
    let role = out["data"]["lease"]["id"].as_str().expect("id").to_string();
    let mut minted = Vec::new();
    for path in ["src/a.rs", "src/b.rs", "src/c.rs"] {
        let (code, out) = s.run("a", &["claim", path, "--new"]);
        assert_eq!(code, 0, "{out}");
        minted.push((
            out["data"]["lease"]["id"].as_str().expect("id").to_string(),
            path,
        ));
    }
    // Both stores number from the same start, so the ids meet.
    {
        let (id, path) = minted
            .iter()
            .find(|(id, _)| *id == role)
            .expect("an id the host minted is b's role's here");
        let (code, out) = s.run("a", &["release", id]);
        assert_eq!(
            (code, &out["data"]["lease"]["scope"]),
            (0, &json!(path)),
            "b's role doesn't block a's release at the host: {out}"
        );
    }
    {
        assert!(minted.iter().any(|(id, _)| *id == old), "{minted:?} {old}");
        let (code, out) = s.run("a", &["release", &old]);
        assert_eq!(code, 1, "your own lease here is ambiguous: {out}");
        assert!(
            out["diagnostics"][0]["message"]
                .as_str()
                .is_some_and(|m| m.contains("src/old.rs")),
            "{out}"
        );
        // Once it ended here, the id is the host's again.
        fs::rename(&s.config, &off).expect("off");
        let (code, out) = s.run("a", &["release", "./src/old.rs"]);
        assert_eq!(code, 0, "{out}");
        fs::rename(&off, &s.config).expect("on");
        let path = minted.iter().find(|(id, _)| *id == old).map(|(_, p)| *p);
        let (code, out) = s.run("a", &["release", &old]);
        assert_eq!(
            (code, out["data"]["lease"]["scope"].as_str()),
            (0, path),
            "{out}"
        );
    }

    // No folder lease holds node folders wholesale (task-36, MCP round
    // 2), so `done` works on the node's own lease.
    let (code, out) = s.run("a", &["claim", ".mdkg/work/"]);
    assert_eq!(
        (code, codes(&out)),
        (1, vec!["usage.invalid_arguments".to_string()]),
        "{out}"
    );
    let (code, out) = s.run("a", &["claim", "task-1"]);
    assert_eq!(code, 0, "{out}");
    let t1 = s.repo.join(".mdkg/work/task-1-basic.md");
    let text = fs::read_to_string(&t1).expect("read") + "\nNote.\n";
    fs::write(&t1, text).expect("write");
    let (code, out) = s.run("a", &["done", "task-1"]);
    assert_eq!(code, 0, "{out}");

    // A goal edit ends its lease there too.
    let goal = ".mdkg/work/goal-1-g.md";
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../conformance/cases/e01-edges")
            .join(goal),
        s.repo.join(goal),
    )
    .expect("goal");
    let git = |dir: &Path, args: &[&str]| {
        let out = isolated(Command::new("git").arg("-C").arg(dir).args(args))
            .output()
            .expect("git");
        assert!(out.status.success(), "git {args:?}");
    };
    git(&s.repo, &["add", goal]);
    git(&s.repo, &["commit", "-q", "-m", "goal"]);
    git(&p.host, &["pull", "-q"]);
    let (code, out) = s.run("a", &["claim", "goal-1"]);
    assert_eq!(code, 0, "{out}");
    let (code, out) = s.run("a", &["goal", "pause", "goal-1"]);
    assert_eq!(code, 0, "{out}");
    let (_, out) = s.run("a", &["holds", goal]);
    assert_eq!(out["data"]["holds"], false, "ended there too: {out}");
}

/// Review of 404001a: a shared hold isn't enough for an edit, and says so.
#[test]
fn a_shared_hold_at_the_authority_doesnt_cover_an_edit() {
    let s = Setup::new();
    s.use_script();
    let mut shared = lease(".mdkg/work/");
    shared["mode"] = json!("shared");
    s.answer(
        json!({"schema": "mdkg.holds.v1", "ok": true,
               "data": {"holds": true, "lease": shared}, "diagnostics": []}),
        0,
    );
    let (code, out) = s.run("a", &["update", "task-1", "--status", "review"]);
    assert_eq!(
        (code, codes(&out)),
        (4, vec!["coord.authority_failed".to_string()]),
        "{out}"
    );
    let d = &out["diagnostics"][0];
    assert!(
        d["message"]
            .as_str()
            .is_some_and(|m| m.contains("only shared")),
        "{out}"
    );
    assert!(
        d["hint"].as_str().is_some_and(
            |h| h.contains("--exclusive") && h.contains("`mdkg release ./.mdkg/work`")
        ),
        "the hint names the path, not the authority's id: {out}"
    );
}

/// Review of 5bddc38: one folder lease covering several goal edits is
/// committed once, so editing two goals under it succeeds.
#[test]
fn goal_edits_under_one_folder_lease_record_once() {
    let s = Setup::new();
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance/cases/e01-edges");
    let goal = ".mdkg/work/goal-1-g.md";
    fs::copy(base.join(goal), s.repo.join(goal)).expect("goal");
    let text = fs::read_to_string(s.repo.join(goal)).expect("read");
    fs::write(
        s.repo.join(".mdkg/work/goal-2-h.md"),
        text.replace("id: goal-1", "id: goal-2")
            .replace("goal_state: active", "goal_state: paused"),
    )
    .expect("goal-2");
    let git = |args: &[&str]| {
        let out = isolated(Command::new("git").arg("-C").arg(&s.repo).args(args))
            .output()
            .expect("git");
        assert!(out.status.success(), "git {args:?}");
    };
    git(&["add", ".mdkg/work"]);
    git(&["commit", "-q", "-m", "goals"]);
    // The node folder can't be leased wholesale (task-36, MCP round 2);
    // the goals are claimed one by one.
    let (code, out) = s.run("a", &["claim", ".mdkg/work/"]);
    assert_eq!(code, 1, "{out}");
    for goal in ["goal-1", "goal-2"] {
        let (code, out) = s.run("a", &["claim", goal]);
        assert_eq!(code, 0, "{out}");
    }
    let (code, out) = s.run("a", &["goal", "activate", "goal-2"]);
    assert_eq!(code, 0, "{out}");
}

/// Review of 404001a and 8d0596c: renew fails when your roles and turns
/// weren't renewed here, and work isn't handed out past an authority that
/// can't be trusted.
#[test]
fn renew_and_next_fail_closed_around_the_authority() {
    let s = Setup::new();
    let (code, out) = s.run("a", &["claim", "run:codex"]);
    assert_eq!(code, 0, "{out}");
    s.use_script();
    s.answer(
        json!({"schema": "mdkg.renew.v1", "ok": true,
               "data": {"renewed": [lease("src/a.rs")]}, "diagnostics": []}),
        0,
    );
    let git = |args: &[&str]| {
        isolated(Command::new("git").arg("-C").arg(&s.repo).args(args))
            .output()
            .expect("git")
    };
    // The store can't be read: the turn isn't renewed, and renew says so.
    let mdkg = s.repo.join(".git/mdkg");
    fs::set_permissions(&mdkg, fs::Permissions::from_mode(0o000)).expect("chmod");
    let (code, out) = s.run("a", &["renew"]);
    fs::set_permissions(&mdkg, fs::Permissions::from_mode(0o755)).expect("chmod");
    assert_ne!(code, 0, "the turn wasn't renewed: {out}");
    assert_eq!(out["ok"], false, "{out}");
    assert!(
        codes(&out).contains(&"coord.authority_remote".to_string()),
        "what the authority renewed is still said: {out}"
    );
    // Roles and turns don't depend on main: a missing main doesn't stop
    // them, or an actor with none.
    assert!(git(&["config", "mdkg.mainRef", "nosuch"]).status.success());
    for actor in ["a", "c"] {
        let (code, out) = s.run(actor, &["renew"]);
        assert_eq!(code, 0, "{actor}: {out}");
    }
    assert!(git(&["config", "--unset", "mdkg.mainRef"]).status.success());

    // A goal to hand out.
    let goal = ".mdkg/work/goal-1-g.md";
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../conformance/cases/e01-edges")
            .join(goal),
        s.repo.join(goal),
    )
    .expect("goal");
    assert!(git(&["add", goal]).status.success());
    assert!(git(&["commit", "-q", "-m", "goal"]).status.success());

    // A configured authority that can't be trusted stops `next`.
    fs::set_permissions(s.file(), fs::Permissions::from_mode(0o666)).expect("chmod");
    let (code, out) = s.run("a", &["next"]);
    assert_eq!(
        (code, codes(&out)),
        (4, vec!["coord.authority_failed".to_string()]),
        "{out}"
    );
    let (code, out) = s.run("a", &["goal", "next"]);
    assert_eq!(
        (code, codes(&out)),
        (4, vec!["coord.authority_failed".to_string()]),
        "{out}"
    );
}

/// Orchestrator, 2026-10-21: a `done` that fails after recording still
/// says the authority may hold the node, so it isn't renewed forever
/// unnoticed.
#[test]
fn a_done_that_fails_after_recording_still_warns_about_the_authority() {
    let s = Setup::new();
    let d = s.dir.0.display();
    fs::write(
        s.dir.0.join("holds.json"),
        json!({"schema": "mdkg.holds.v1", "ok": true,
               "data": {"holds": true, "lease": lease(".mdkg/work/task-1-basic.md")},
               "diagnostics": []})
        .to_string(),
    )
    .expect("holds");
    // It grants the hold, then fails the release, and the node read that
    // follows fails too (the file is gone).
    let script = s.dir.0.join("split.sh");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\ncase \"$1\" in\n  holds) cat {d}/holds.json ;;\n  release) rm -f .mdkg/work/task-1-basic.md; exit 1 ;;\nesac\n"
        ),
    )
    .expect("script");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod");
    s.configure(json!([script.to_string_lossy()]), ANSWER_MS);
    let t1 = s.repo.join(".mdkg/work/task-1-basic.md");
    let text = fs::read_to_string(&t1).expect("read") + "\nNote.\n";
    fs::write(&t1, text).expect("write");
    let (code, out) = s.run("a", &["done", "task-1"]);
    assert_ne!(code, 0, "the node read failed: {out}");
    let warned = out["diagnostics"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|d| d["code"] == "coord.authority_remote" && d["severity"] == "warning");
    assert!(warned, "the release warning survives the failure: {out}");
}

/// Review of 84d906a: an agent rewriting its own worktree's record to
/// claim the whole disk can't make every authority call refuse: a record
/// counts only when the worktree's `.git` names it back.
#[test]
fn a_rewritten_worktree_record_cant_block_the_authority() {
    let s = Setup::new();
    s.use_script();
    s.answer(
        json!({"schema": "mdkg.claim.v1", "ok": true,
               "data": {"lease": lease(".mdkg/work/task-1-basic.md"), "renewed": false},
               "diagnostics": []}),
        0,
    );
    let wt = s.dir.0.join("wt-x");
    let made = isolated(Command::new("git").arg("-C").arg(&s.repo).args([
        "worktree",
        "add",
        "-q",
        "-b",
        "x",
        wt.to_str().expect("utf-8"),
    ]))
    .output()
    .expect("git");
    assert!(made.status.success());
    fs::write(s.repo.join(".git/worktrees/wt-x/gitdir"), "/.git\n").expect("record");
    let (code, out) = s.run("a", &["claim", "task-1"]);
    assert_eq!(code, 0, "{out}");
}

/// Audit task-36: through a host, a renamed node is still the node the
/// host knows, and the host refuses a path that isn't where it has it.
#[test]
fn a_host_refuses_a_renamed_node() {
    let p = Pair::new();
    let s = &p.worker;
    let (code, out) = s.run("a", &["claim", "task-1"]);
    assert_eq!(code, 0, "{out}");
    let renamed = ".mdkg/work/task-1-renamed.md";
    fs::rename(
        s.repo.join(".mdkg/work/task-1-basic.md"),
        s.repo.join(renamed),
    )
    .expect("rename");
    for target in [renamed, "task-1"] {
        let (code, out) = s.run("b", &["claim", target]);
        assert_eq!(
            (code, codes(&out)),
            (5, vec!["coord.lease_held".to_string()]),
            "{target}: {out}"
        );
    }
    // The honest move, unheld: the host refuses it until it has the move.
    let (code, out) = s.run("a", &["release", "task-1"]);
    assert_eq!(
        (code, codes(&out)),
        (3, vec!["coord.no_lease".to_string()]),
        "a's lease is on the old name: {out}"
    );
    fs::rename(
        s.repo.join(renamed),
        s.repo.join(".mdkg/work/task-1-basic.md"),
    )
    .expect("rename back");
    let (code, out) = s.run("a", &["release", "task-1"]);
    assert_eq!(code, 0, "{out}");
    fs::rename(
        s.repo.join(".mdkg/work/task-1-basic.md"),
        s.repo.join(renamed),
    )
    .expect("rename");
    let (code, out) = s.run("b", &["claim", "task-1"]);
    assert_eq!(code, 5, "{out}");
    assert!(
        out["diagnostics"][0]["hint"]
            .as_str()
            .is_some_and(|h| h.contains("merge")),
        "{out}"
    );
    let host_file = p.host.join(".mdkg/work/task-1-basic.md");
    fs::rename(&host_file, p.host.join(renamed)).expect("host has the move");
    let (code, out) = s.run("b", &["claim", "task-1"]);
    assert_eq!(
        (code, &out["data"]["lease"]["scope"]),
        (0, &json!(renamed)),
        "{out}"
    );
}

/// Codex, task-36: with an authority, `format --write` writes only what the
/// authority says the actor holds.
#[test]
fn format_writes_only_what_the_authority_says_you_hold() {
    let s = Setup::new();
    let file = s.repo.join(".mdkg/work/task-2-other.md");
    let text = fs::read_to_string(&file).expect("read");
    let shuffled = text.replace(
        "title: Other task\nstatus: backlog",
        "status: backlog\ntitle: Other task",
    );
    assert_ne!(text, shuffled);
    fs::write(&file, &shuffled).expect("write");
    s.use_script();
    s.answer(
        json!({"schema": "mdkg.holds.v1", "ok": true, "data": {"holds": false}, "diagnostics": []}),
        0,
    );
    let (code, out) = s.run("a", &["format", "--write"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(fs::read_to_string(&file).expect("read"), shuffled, "{out}");
    assert_eq!(out["diagnostics"][0]["severity"], "warning", "{out}");
    assert_eq!(
        out["data"]["skipped"],
        json!([".mdkg/work/task-2-other.md"]),
        "{out}"
    );
    assert!(
        s.args()
            .contains("holds .mdkg/work/task-2-other.md --as a --node root:task-2 --json")
    );
    s.answer(
        json!({"schema": "mdkg.holds.v1", "ok": true,
               "data": {"holds": true, "lease": lease(".mdkg/work/task-2-other.md")},
               "diagnostics": []}),
        0,
    );
    let (code, out) = s.run("a", &["format", "--write"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(fs::read_to_string(&file).expect("read"), text, "{out}");
}

/// Review round 1 of audit-core: through a host, an edit asks `holds` with
/// the node, so a lease taken on the new name before the move (or a
/// folder) doesn't cover a node another actor holds; the host's refusal
/// names its holder as data; and no id is squatted on a path that isn't
/// a node file.
#[test]
fn a_host_guards_edits_of_a_renamed_node() {
    let p = Pair::new();
    let s = &p.worker;
    let (code, out) = s.run("a", &["claim", "task-1"]);
    assert_eq!(code, 0, "{out}");
    let (code, out) = s.run("b", &["claim", "task-1"]);
    assert_eq!(code, 5, "{out}");
    assert_eq!(out["diagnostics"][0]["holders"], json!(["a"]), "{out}");
    let renamed = ".mdkg/work/task-1-zz.md";
    let (code, out) = s.run("b", &["claim", "--new", renamed]);
    assert_eq!(code, 0, "{out}");
    fs::rename(
        s.repo.join(".mdkg/work/task-1-basic.md"),
        s.repo.join(renamed),
    )
    .expect("rename");
    let (code, out) = s.run("b", &["done", "task-1"]);
    assert_ne!(code, 0, "{out}");
    assert!(
        !fs::read_to_string(s.repo.join(renamed))
            .expect("read")
            .contains("status: done"),
        "{out}"
    );
    // Straight to the host: an id the graph lacks keys nothing but a node
    // file, and a malformed one nothing at all.
    let host = s.dir.0.join("host.sh");
    for args in [
        &[
            "claim",
            "--mode",
            "exclusive",
            "--node",
            "root:task-9",
            "src/x.rs",
        ][..],
        &[
            "claim",
            "--mode",
            "exclusive",
            "--node",
            "root:task-2",
            ".mdkg/work/task-1-basic.md",
        ],
        &[
            "claim",
            "--mode",
            "exclusive",
            "--node",
            "a\nb",
            ".mdkg/work/task-9-x.md",
        ],
    ] {
        let out = isolated(&mut Command::new(&host))
            .args(args)
            .args(["--as", "m", "--json"])
            .env("XDG_CONFIG_HOME", s.dir.0.join("host-config"))
            .output()
            .expect("host");
        assert!(
            !out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stdout)
        );
        let v: Value = serde_json::from_slice(&out.stdout).expect("json");
        assert!(
            matches!(
                v["diagnostics"][0]["code"].as_str(),
                Some("coord.lease_held" | "usage.invalid_arguments")
            ),
            "{args:?}: {v}"
        );
    }
    // The control: the node's own id on its own file is granted.
    let out = isolated(&mut Command::new(&host))
        .args(["claim", "--mode", "exclusive", "--node", "root:task-2"])
        .arg(".mdkg/work/task-2-other.md")
        .args(["--as", "m", "--json"])
        .env("XDG_CONFIG_HOME", s.dir.0.join("host-config"))
        .output()
        .expect("host");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
}

/// Review round 1 of audit-core: a second graph in the repository leases
/// its nodes through the same host, keyed in its own graph.
#[test]
fn a_subfolder_graph_claims_through_the_host() {
    let p = Pair::new();
    let s = &p.worker;
    let conformance = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance");
    let sub = s.repo.join("sub");
    fs::create_dir_all(&sub).expect("sub");
    copy_tree(&conformance.join("base"), &sub);
    copy_tree(&conformance.join("cases/g01-valid-basic"), &sub);
    fs::remove_file(sub.join("commands")).ok();
    fs::remove_file(sub.join("README.md")).ok();
    for args in [&["add", "."][..], &["commit", "-q", "-m", "sub"]] {
        let out = isolated(Command::new("git").arg("-C").arg(&s.repo).args(args))
            .output()
            .expect("git");
        assert!(out.status.success());
    }
    let out = isolated(
        Command::new("git")
            .arg("-C")
            .arg(&p.host)
            .args(["pull", "-q"]),
    )
    .output()
    .expect("pull");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let run = |actor: &str, args: &[&str]| {
        let out = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
            .arg("--root")
            .arg(&sub)
            .args(["--as", actor])
            .args(args)
            .arg("--json")
            .env("XDG_CONFIG_HOME", &s.config)
            .env("MDKG_NOW_MS", "1000")
            .output()
            .expect("run");
        let v: Value = serde_json::from_slice(&out.stdout).expect("json");
        (out.status.code().unwrap_or(-1), v)
    };
    let (code, out) = run("a", &["claim", "task-1"]);
    assert_eq!(code, 0, "{out}");
    let (code, out) = run("b", &["claim", "task-1"]);
    assert_eq!(code, 5, "{out}");
    // The root graph's task-1 is another node.
    let (code, out) = s.run("b", &["claim", "task-1"]);
    assert_eq!(code, 0, "{out}");
}

/// Review round 3 of audit-core: through a host, a workspace that's also a
/// graph of its own leases its nodes by their workspace id, and a second
/// actor is held off.
#[test]
fn a_workspace_that_is_a_graph_claims_through_the_host() {
    let p = Pair::new();
    let s = &p.worker;
    let conformance = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance");
    let w = s.repo.join("w");
    fs::create_dir_all(&w).expect("w");
    copy_tree(&conformance.join("base"), &w);
    copy_tree(&conformance.join("cases/g01-valid-basic"), &w);
    fs::remove_file(w.join("commands")).ok();
    fs::remove_file(w.join("README.md")).ok();
    let path = s.repo.join(".mdkg/config.json");
    let mut config: Value =
        serde_json::from_str(&fs::read_to_string(&path).expect("config")).expect("json");
    config["workspaces"] = json!({
        "root": {"path": ".", "enabled": true, "mdkg_dir": ".mdkg", "visibility": "private"},
        "w": {"path": "w", "enabled": true, "mdkg_dir": ".mdkg", "visibility": "private"}
    });
    fs::write(&path, serde_json::to_string_pretty(&config).expect("json")).expect("write");
    for args in [&["add", "."][..], &["commit", "-q", "-m", "w"]] {
        let out = isolated(Command::new("git").arg("-C").arg(&s.repo).args(args))
            .output()
            .expect("git");
        assert!(out.status.success());
    }
    let out = isolated(
        Command::new("git")
            .arg("-C")
            .arg(&p.host)
            .args(["pull", "-q"]),
    )
    .output()
    .expect("pull");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let (code, out) = s.run("a", &["claim", "w:task-1"]);
    assert_eq!(code, 0, "{out}");
    let (code, out) = s.run("b", &["claim", "w:task-1"]);
    assert_eq!(code, 5, "{out}");
}

/// Review round 4 of audit-core: through a host, a new node whose id
/// repeats in another workspace is another node, so it's granted; a node
/// moved within its own node folder is still refused.
#[test]
fn a_host_grants_a_new_node_whose_id_repeats_elsewhere() {
    let p = Pair::new();
    let s = &p.worker;
    let path = s.repo.join(".mdkg/config.json");
    let mut config: Value =
        serde_json::from_str(&fs::read_to_string(&path).expect("config")).expect("json");
    config["workspaces"] = json!({
        "root": {"path": ".", "enabled": true, "mdkg_dir": ".mdkg", "visibility": "private"},
        "w2": {"path": "w2", "enabled": true, "mdkg_dir": ".mdkg", "visibility": "private"}
    });
    fs::write(&path, serde_json::to_string_pretty(&config).expect("json")).expect("write");
    fs::create_dir_all(s.repo.join("w2/.mdkg/work")).expect("w2");
    fs::write(s.repo.join("w2/.mdkg/work/.keep"), "").expect("keep");
    for args in [&["add", "."][..], &["commit", "-q", "-m", "w2"]] {
        let out = isolated(Command::new("git").arg("-C").arg(&s.repo).args(args))
            .output()
            .expect("git");
        assert!(out.status.success());
    }
    let out = isolated(
        Command::new("git")
            .arg("-C")
            .arg(&p.host)
            .args(["pull", "-q"]),
    )
    .output()
    .expect("pull");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Only on the worker's branch: w2's own task-1.
    let text = fs::read_to_string(s.repo.join(".mdkg/work/task-1-basic.md")).expect("read");
    fs::write(s.repo.join("w2/.mdkg/work/task-1-w2.md"), text).expect("write");
    let (code, out) = s.run("a", &["claim", "w2:task-1"]);
    assert_eq!(code, 0, "{out}");
    let (code, out) = s.run("b", &["claim", "w2:task-1"]);
    assert_eq!(code, 5, "{out}");
    // root:task-1 is its own node, free.
    let (code, out) = s.run("b", &["claim", "root:task-1"]);
    assert_eq!(code, 0, "{out}");
}

/// Review of 4d301e4: a profile C node's node: lease is local (no authority
/// holds the store's folder), so renew renews it here with roles and turns.
#[test]
fn profile_c_work_leases_renew_here() {
    let s = Setup::new();
    let path = s.repo.join(".mdkg/config.json");
    let mut config: Value =
        serde_json::from_str(&fs::read_to_string(&path).expect("config")).expect("json");
    config["schema_version"] = 3.into();
    config["format_version"] = "0.7".into();
    config["graph_id"] = "0192f3a0-0000-7000-8000-000000000000".into();
    config["work"]["committed"] = "none".into();
    fs::write(&path, config.to_string()).expect("write config");
    // The fixture's nodes have 0.6.0 ids; a 0.7 graph starts without them.
    for entry in fs::read_dir(s.repo.join(".mdkg/work"))
        .expect("work")
        .flatten()
    {
        fs::remove_file(entry.path()).ok();
    }
    for args in [&["add", "-A"][..], &["commit", "-q", "-m", "profile c"]] {
        let out = isolated(Command::new("git").arg("-C").arg(&s.repo).args(args))
            .output()
            .expect("git");
        assert!(out.status.success(), "git {args:?}");
    }
    let (code, out) = s.run("a", &["new", "task", "Local", "one"]);
    assert_eq!(code, 0, "{out}");
    let id = out["data"]["node"]["id"].as_str().expect("id").to_string();
    s.use_script();
    let (code, out) = s.run("a", &["claim", &id]);
    assert_eq!(code, 0, "{out}");
    let scope = format!("node:{}", id.split_once('-').map_or("", |(_, u)| u));
    assert_eq!(out["data"]["lease"]["scope"], scope.as_str(), "{out}");
    s.answer(
        json!({"schema": "mdkg.renew.v1", "ok": true, "data": {"renewed": []}, "diagnostics": []}),
        0,
    );
    let (code, out) = s.run("a", &["renew"]);
    assert_eq!(code, 0, "{out}");
    let scopes: Vec<_> = out["data"]["renewed"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|l| l["scope"].as_str())
        .collect();
    assert!(scopes.contains(&scope.as_str()), "{out}");
}

#[test]
fn receiver_local_turn_renewal_never_starts_configured_authority() {
    use mdkg_run::Mail;
    if let Some(root) = std::env::var_os(LOCAL_TURN_CHILD) {
        let root = PathBuf::from(root);
        let invoked = root.parent().unwrap().join("authority-invoked");
        let mut mail = mdkg_run::CliMail::new(
            PathBuf::from(env!("CARGO_BIN_EXE_mdkg")),
            root,
            "receiver".into(),
        );
        mail.claim("run:alice").unwrap();
        let deadline = local_turn_ready(LOCAL_TURN_STARTUP, || {
            let result = mail.renew_held("run:alice");
            assert!(!invoked.exists(), "local renewal started the authority");
            result
        })
        .unwrap();
        assert!(deadline > mdkg_run::state::now_ms());
        mail.release("run:alice").unwrap();
        return;
    }
    let s = Setup::new();
    fs::write(&s.script, format!("#!/bin/sh\nprintf invoked > '{}/authority-invoked'\nsleep 3\nprintf escaped > '{}/authority-escaped'\n", s.dir.0.display(), s.dir.0.display())).unwrap();
    s.configure(json!([s.script.to_string_lossy()]), 1500);
    let out = local_turn_child(&std::env::current_exe().unwrap(), &s.repo, &s.config)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{} {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!s.dir.0.join("authority-invoked").exists());
    assert!(!s.dir.0.join("authority-escaped").exists());
    for scope in ["run:first", "run:second"] {
        assert_eq!(s.run("scoped", &["claim", scope]).0, 0);
    }
    let (code, renewed) = s.run("scoped", &["renew", "--local-turn", "run:first"]);
    assert_eq!(code, 0, "{renewed}");
    let rows = renewed["data"]["renewed"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{renewed}");
    assert_eq!(rows[0]["scope"], "run:first");
    assert_eq!(
        s.run("stranger", &["renew", "--local-turn", "run:first"]).1["data"]["renewed"],
        json!([])
    );
    // Scope restriction cannot turn this into bypassing file/role authority.
    for scope in ["role:alice", "./README.md", "run:../alice"] {
        let (code, _) = s.run("receiver", &["renew", "--local-turn", scope]);
        assert_ne!(code, 0);
    }
    assert!(!s.dir.0.join("authority-invoked").exists());
}
