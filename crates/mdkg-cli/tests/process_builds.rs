//! Process build diagnostics through the CLI, with isolated repositories.

use std::fs;
use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Output, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use mdkg_run::builds::{self, Build, Kind};
use serde_json::{Value, json};

mod common;

const BIN: &str = env!("CARGO_BIN_EXE_mdkg");
const STARTUP: Duration = Duration::from_secs(30);

struct Fixture {
    top: PathBuf,
    graph: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.top).ok();
    }
}

fn isolated<'a>(command: &'a mut Command, home: &Path) -> &'a mut Command {
    let path = std::env::var_os("PATH").unwrap_or_default();
    command
        .env_clear()
        .env("PATH", path)
        .env("HOME", home)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .env("MDKG_RUN_SECURITY", "/usr/bin/false")
        .env(
            "MDKG_RUN_CLAUDE",
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../mdkg-run/tests/fixtures/fake-claude.sh"),
        );
    common::without_launcher_env(command)
}

fn git(root: &Path, args: &[&str]) {
    let output = isolated(Command::new("git").current_dir(root).args(args), root)
        .output()
        .expect("git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("folder");
    for entry in fs::read_dir(from).expect("fixture folder") {
        let entry = entry.expect("fixture entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).expect("fixture file");
        }
    }
}

fn fixture() -> Fixture {
    let top = std::env::temp_dir().join(format!("mdkg-process-builds-{}", uuid::Uuid::now_v7()));
    let conformance = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance");
    copy_tree(&conformance.join("base"), &top);
    copy_tree(&conformance.join("cases/g01-valid-basic"), &top);
    fs::remove_file(top.join("commands")).ok();
    fs::write(
        top.join(".gitignore"),
        "/.worktrees/\n*.environ\n*.args\n*.prompt\n",
    )
    .expect("ignore");
    git(&top, &["init", "-q", "-b", "main"]);
    git(&top, &["add", "."]);
    git(&top, &["commit", "-q", "-m", "fixture"]);
    let top = fs::canonicalize(top).expect("canonical fixture");
    let migration = envelope(&top, &["migrate", "--dry-run", "--json"]);
    assert_eq!(migration["ok"], true, "{migration}");
    let plan = migration["data"]["plan"].as_str().expect("migration plan");
    let migrated = envelope(&top, &["migrate", "--apply", plan, "--json"]);
    assert_eq!(migrated["ok"], true, "{migrated}");
    let graph = top.join(".worktrees/graph");
    git(
        &top,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "mdkg-graph",
            graph.to_str().expect("path"),
            "main",
        ],
    );
    let installed = envelope(&top, &["install", "--json"]);
    assert_eq!(installed["ok"], true, "{installed}");
    // Migration journals and the install lock belong to fixture setup. Park
    // them after those commands exit so the original tests still exercise a
    // missing store, or deliberately create a private one in inventory_fixture.
    // Durable graph identity remains in Git config and worktree registration.
    fs::rename(
        top.join(".git/mdkg"),
        top.join(".git/process-builds-setup-mdkg"),
    )
    .expect("park completed migration/install metadata");
    assert!(
        !top.join(".git/mdkg").exists(),
        "fixture starts without a store"
    );
    Fixture { top, graph }
}

fn inventory_fixture() -> Fixture {
    let f = fixture();
    fs::DirBuilder::new()
        .mode(0o700)
        .create(f.top.join(".git/mdkg"))
        .expect("existing store folder");
    f
}

fn command(root: &Path) -> Command {
    command_as(root, "fixture-reader")
}

fn command_as(root: &Path, actor: &str) -> Command {
    let mut command = Command::new(BIN);
    isolated(&mut command, root)
        .current_dir(root)
        .arg("--root")
        .arg(root)
        .args(["--as", actor]);
    command
}

fn output(root: &Path, args: &[&str]) -> Output {
    command(root).args(args).output().expect("mdkg")
}

fn envelope(root: &Path, args: &[&str]) -> Value {
    let output = output(root, args);
    let value = serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "not JSON for {args:?}: {} / {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    common::conforms_envelope(&value);
    value
}

fn current_build(root: &Path) -> Build {
    let value = envelope(root, &["version", "--json"]);
    let data = &value["data"];
    Build {
        version: data["version"].as_str().expect("version").into(),
        commit: data["commit"].as_str().expect("commit").into(),
        dirty: data["dirty"].as_bool(),
        built: data["built"].as_str().expect("build date").into(),
    }
}

fn build_check(root: &Path) -> Value {
    envelope(root, &["doctor", "--json"])["data"]["checks"]
        .as_array()
        .expect("checks")
        .iter()
        .find(|c| c["name"] == "builds")
        .expect("builds check")
        .clone()
}

fn status(root: &Path) -> Value {
    let value = envelope(root, &["agents", "status", "--json"]);
    assert_eq!(value["ok"], true, "{value}");
    value["data"].clone()
}

#[test]
fn inspecting_missing_process_records_creates_nothing() {
    let f = fixture();
    let dir = mdkg_run::state::run_dir(&f.graph).expect("run dir");
    assert!(!dir.exists());
    let check = build_check(&f.graph);
    assert_eq!(check["status"], "ok");
    assert_eq!(check["summary"], "no build inventory yet");
    let data = status(&f.graph);
    assert!(data["processes"].as_array().unwrap().is_empty());
    assert!(data["build_warnings"].as_array().unwrap().is_empty());
    let text = output(&f.graph, &["agents", "status"]);
    assert!(String::from_utf8_lossy(&text.stdout).contains("no build inventory yet"));
    assert!(!dir.exists(), "diagnostics must not create the run folder");
    assert!(!f.top.join(".git/mdkg").exists());
}

#[test]
fn mixed_live_builds_warn_in_doctor_and_status_text_and_json() {
    let f = inventory_fixture();
    let dir = mdkg_run::state::run_dir(&f.graph).unwrap();
    let mut old = current_build(&f.graph);
    old.commit = "sample-old-build".into();
    let _launcher = builds::publish(
        &dir,
        Kind::Launcher,
        Path::new(BIN),
        old.clone(),
        vec!["worker".into()],
    )
    .unwrap();
    let _mcp = builds::publish(&dir, Kind::Mcp, Path::new(BIN), old, vec![]).unwrap();
    let check = build_check(&f.graph);
    assert_eq!(check["status"], "warning");
    assert!(
        check["summary"]
            .as_str()
            .unwrap()
            .contains("mixed mdkg builds")
    );
    let data = status(&f.graph);
    assert_eq!(data["processes"].as_array().unwrap().len(), 2);
    assert_eq!(data["build_warnings"].as_array().unwrap().len(), 2);
    let text = output(&f.graph, &["agents", "status"]);
    assert!(String::from_utf8_lossy(&text.stdout).contains("warning: mixed mdkg builds"));
    assert!(
        !output(&f.graph, &["doctor", "--strict", "--json"])
            .status
            .success()
    );
}

#[test]
fn matching_records_pass_and_stale_records_are_ignored_without_cleanup() {
    let f = inventory_fixture();
    let dir = mdkg_run::state::run_dir(&f.graph).unwrap();
    let held = builds::publish(
        &dir,
        Kind::Mcp,
        Path::new(BIN),
        current_build(&f.graph),
        vec![],
    )
    .unwrap();
    assert_eq!(build_check(&f.graph)["status"], "ok");
    assert!(
        status(&f.graph)["build_warnings"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let path = fs::read_dir(dir.join("builds"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let stale = dir.join("builds/stale.json");
    fs::copy(path, &stale).unwrap();
    let bytes = fs::read(&stale).unwrap();
    drop(held);
    assert!(status(&f.graph)["processes"].as_array().unwrap().is_empty());
    assert_eq!(build_check(&f.graph)["status"], "ok");
    assert_eq!(fs::read(stale).unwrap(), bytes);
}

#[test]
fn malformed_live_records_warn_without_hiding_a_healthy_process() {
    let f = inventory_fixture();
    let dir = mdkg_run::state::run_dir(&f.graph).unwrap();
    let _held = builds::publish(
        &dir,
        Kind::Mcp,
        Path::new(BIN),
        current_build(&f.graph),
        vec![],
    )
    .unwrap();
    let mut bad = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(dir.join("builds/bad.json"))
        .unwrap();
    bad.lock().unwrap();
    bad.write_all(b"{").unwrap();
    assert_eq!(build_check(&f.graph)["status"], "warning");
    let data = status(&f.graph);
    assert_eq!(data["processes"].as_array().unwrap().len(), 1);
    assert!(
        data["build_warnings"][0]
            .as_str()
            .unwrap()
            .contains("invalid process build record")
    );
}

#[test]
fn replacing_a_registered_executable_warns() {
    let f = inventory_fixture();
    let bin = f.top.join("sample-executable");
    fs::write(&bin, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
    let dir = mdkg_run::state::run_dir(&f.graph).unwrap();
    let _held =
        builds::publish(&dir, Kind::Launcher, &bin, current_build(&f.graph), vec![]).unwrap();
    fs::write(&bin, "#!/bin/sh\nexit 1\n").unwrap();
    assert_eq!(build_check(&f.graph)["status"], "warning");
    assert!(
        status(&f.graph)["build_warnings"][0]
            .as_str()
            .unwrap()
            .contains("executable changed on disk")
    );
}

struct Running {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
}

impl Running {
    fn start(command: &mut Command) -> Self {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .process_group(0)
            .spawn()
            .expect("process");
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().expect("piped stdout");
        let (send, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if send.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            stdin,
            lines,
        }
    }

    fn finish(&mut self) {
        let status = self.wait();
        assert!(status.success(), "{status}");
    }

    fn wait(&mut self) -> std::process::ExitStatus {
        self.stdin.take();
        let deadline = Instant::now() + STARTUP;
        loop {
            if let Some(status) = self.child.try_wait().expect("process status") {
                return status;
            }
            assert!(Instant::now() < deadline, "process did not finish");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn json(mut self) -> Value {
        self.wait();
        let mut text = String::new();
        loop {
            match self.lines.recv_timeout(STARTUP) {
                Ok(line) => {
                    text.push_str(&line);
                    text.push('\n');
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                Err(e) => panic!("output did not close: {e}"),
            }
        }
        serde_json::from_str(&text).expect("JSON response")
    }
    fn initialize_mcp(&mut self) {
        writeln!(
            self.stdin.as_mut().expect("MCP stdin"),
            "{}",
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
                "protocolVersion":"2025-11-25", "capabilities":{},
                "clientInfo":{"name":"fixture","version":"0"}
            }})
        )
        .expect("initialize MCP");
        let ready: Value =
            serde_json::from_str(&self.lines.recv_timeout(STARTUP).expect("MCP ready"))
                .expect("MCP response JSON");
        assert_eq!(ready["result"]["serverInfo"]["name"], "mdkg", "{ready}");
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        // Each fixture process has its own group; cancel mail-wait children
        // too if an assertion fails before the normal bounded shutdown.
        if self.child.try_wait().ok().flatten().is_none() {
            if let Some(pid) = rustix::process::Pid::from_raw(self.child.id() as i32) {
                let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
            }
            self.child.kill().ok();
        }
        self.child.wait().ok();
    }
}

#[test]
fn mcp_registers_while_running_and_removes_its_receipt_on_eof() {
    let f = inventory_fixture();
    let mut mcp = Running::start(command(&f.graph).arg("mcp"));
    mcp.initialize_mcp();
    let data = status(&f.graph);
    let process = data["processes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["pid"] == mcp.child.id())
        .expect("MCP build receipt");
    assert_eq!(process["kind"], "mcp");
    assert!(data["build_warnings"].as_array().unwrap().is_empty());
    mcp.finish();
    assert!(status(&f.graph)["processes"].as_array().unwrap().is_empty());
    let dir = mdkg_run::state::run_dir(&f.graph).unwrap();
    assert_eq!(fs::read_dir(dir.join("builds")).unwrap().count(), 0);
}

#[test]
fn mcp_reads_without_a_store_folder_create_no_metadata() {
    let f = fixture();
    let store = f.top.join(".git/mdkg");
    for root in [&f.top, &f.graph] {
        assert!(!store.exists());
        let mut mcp = Running::start(command(root).arg("mcp"));
        mcp.initialize_mcp();
        writeln!(
            mcp.stdin.as_mut().expect("MCP stdin"),
            "{}",
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{"_meta":{
                "io.modelcontextprotocol/protocolVersion":"2026-07-28",
                "io.modelcontextprotocol/clientCapabilities":{}
            }}})
        )
        .expect("read request");
        let reply: Value =
            serde_json::from_str(&mcp.lines.recv_timeout(STARTUP).expect("read response"))
                .expect("read response JSON");
        assert!(reply["result"]["tools"].is_array(), "{reply}");
        assert!(!store.exists(), "MCP registration initialized the store");
        mcp.finish();
        assert!(!store.exists(), "MCP shutdown left metadata");
    }
}

#[test]
fn a_blocked_registry_warns_without_preventing_an_mcp_session() {
    let f = fixture();
    let dir = mdkg_run::state::run_dir(&f.graph).unwrap();
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("builds"), "preserve this file").unwrap();
    let mut mcp = Running::start(command(&f.graph).arg("mcp"));
    mcp.initialize_mcp();
    assert_eq!(build_check(&f.graph)["status"], "warning");
    let data = status(&f.graph);
    assert!(data["processes"].as_array().unwrap().is_empty());
    assert!(
        data["build_warnings"][0]
            .as_str()
            .unwrap()
            .contains("isn't a plain build record folder")
    );
    mcp.finish();
    assert_eq!(
        fs::read_to_string(dir.join("builds")).unwrap(),
        "preserve this file"
    );
}

fn reviewer(f: &Fixture) {
    let path = f.graph.join(".mdkg/config.json");
    let mut config: Value =
        serde_json::from_slice(&fs::read(&path).expect("config")).expect("config JSON");
    config["agents"] = json!({"watcher":{"harness":"claude","role":"reviewer","cwd":f.graph}});
    fs::write(path, serde_json::to_vec(&config).expect("encode config")).expect("write config");
    // Intentional setup must leave the shared graph clean before launch.
    git(&f.graph, &["add", ".mdkg/config.json"]);
    git(&f.graph, &["commit", "-qm", "configure fixture reviewer"]);
}

#[test]
fn launcher_registers_its_agents_until_the_watch_finishes() {
    let f = fixture();
    reviewer(&f);
    let mut launcher = Running::start(command_as(&f.graph, "fixture-launcher").args([
        "agents",
        "up",
        "watcher",
        "--wait",
        "1",
        "--max-turns",
        "1",
    ]));
    assert!(
        launcher
            .lines
            .recv_timeout(STARTUP)
            .expect("launcher ready")
            .contains("agents up:")
    );
    let data = status(&f.graph);
    let process = data["processes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["pid"] == launcher.child.id())
        .expect("launcher build receipt");
    assert_eq!(process["kind"], "launcher");
    assert_eq!(process["agents"], json!(["watcher"]));
    let mail = envelope(
        &f.graph,
        &[
            "msg",
            "send",
            "--to",
            "watcher",
            "--kind",
            "handoff",
            "--text",
            "fixture turn",
            "--json",
        ],
    );
    assert_eq!(mail["ok"], true, "{mail}");
    launcher.finish();
    assert!(
        f.graph.join("claude.args").is_file(),
        "fake turn must have run"
    );
    assert!(status(&f.graph)["processes"].as_array().unwrap().is_empty());
    let dir = mdkg_run::state::run_dir(&f.graph).unwrap();
    assert_eq!(fs::read_dir(dir.join("builds")).unwrap().count(), 0);
}

#[test]
fn dry_run_does_not_register_a_process() {
    let f = fixture();
    reviewer(&f);
    let dir = mdkg_run::state::run_dir(&f.graph).unwrap();
    let dry = output(
        &f.graph,
        &[
            "agents",
            "up",
            "watcher",
            "--once",
            "--prompt",
            "fixture",
            "--dry-run",
            "--json",
        ],
    );
    assert!(
        dry.status.success(),
        "{}",
        String::from_utf8_lossy(&dry.stderr)
    );
    assert!(!dir.join("builds").exists());
}

#[test]
fn held_receiver_locks_do_not_delay_build_diagnostics() {
    let f = fixture();
    let common = mdkg_coord_sqlite::git::common_dir(&f.graph).unwrap();
    let hooks = mdkg_run::hooks::dir(&common);
    fs::create_dir_all(hooks.join("receiver")).unwrap();
    fs::create_dir_all(hooks.join("threads")).unwrap();
    fs::write(
        hooks.join("receiver/observer.json"),
        serde_json::to_vec(&json!({
            "actor":"observer", "session":null, "cwd":null, "suspended":false,
            "working":true, "at_ms":0, "reset_at_ms":0, "generation":1,
            "observation_pending":true, "last_attempt":null, "primaries":[], "subagents":[]
        }))
        .unwrap(),
    )
    .unwrap();
    let dir = mdkg_run::state::run_dir(&f.graph).unwrap();
    let mut old = current_build(&f.graph);
    old.commit = "sample-old-build".into();
    let _build = builds::publish(&dir, Kind::Mcp, Path::new(BIN), old, vec![]).unwrap();
    for path in [
        hooks.join("receiver/observer.lock"),
        hooks.join("threads.lock"),
    ] {
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .unwrap();
        lock.lock().unwrap();
        // Both children must finish while this lock is still held. The
        // Running guard kills/reaps a blocked regression at its deadline.
        let status = Running::start(command(&f.graph).args(["agents", "status", "--json"])).json();
        assert!(
            status["data"]["build_warnings"][0]
                .as_str()
                .unwrap()
                .contains("mixed mdkg builds")
        );
        assert!(
            status["data"]["receivers"][0]["problem"]
                .as_str()
                .unwrap()
                .contains("receiver busy")
        );
        let doctor = Running::start(command(&f.graph).args(["doctor", "--json"])).json();
        assert!(
            doctor["data"]["checks"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c["name"] == "builds" && c["status"] == "warning")
        );
        lock.unlock().unwrap();
    }
    for path in [
        hooks.join("receiver/observer.lock"),
        hooks.join("threads.lock"),
    ] {
        fs::remove_file(path).unwrap();
    }
    status(&f.graph);
    assert!(!hooks.join("receiver/observer.lock").exists());
    assert!(
        !hooks.join("threads.lock").exists(),
        "snapshot must not create locks"
    );
}

#[test]
fn unsafe_registry_directories_are_refused_by_inventory_and_publish() {
    let f = inventory_fixture();
    let dir = mdkg_run::state::run_dir(&f.graph).unwrap();
    let common = mdkg_coord_sqlite::git::common_dir(&f.graph).unwrap();
    let current = current_build(&f.graph);
    let mut old = current.clone();
    old.commit = "sample-old-build".into();
    let _build = builds::publish(&dir, Kind::Mcp, Path::new(BIN), old, vec![]).unwrap();
    for path in [
        common.clone(),
        common.join("mdkg"),
        dir.clone(),
        dir.join("builds"),
    ] {
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        for writable in [0o020, 0o002] {
            fs::set_permissions(&path, fs::Permissions::from_mode(mode | writable)).unwrap();
            assert!(builds::live(&dir).is_err(), "unsafe {}", path.display());
            assert!(
                builds::publish(&dir, Kind::Mcp, Path::new(BIN), current.clone(), vec![]).is_err()
            );
            assert_eq!(build_check(&f.graph)["status"], "warning");
            let data = status(&f.graph);
            assert!(data["processes"].as_array().unwrap().is_empty());
            assert!(!data["build_warnings"].as_array().unwrap().is_empty());
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                mode | writable,
                "checks must not chmod existing directories"
            );
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        }
    }
}

#[test]
fn permissive_umask_still_creates_private_registry_directories() {
    let f = inventory_fixture();
    let mut shell = Command::new("/bin/sh");
    isolated(&mut shell, &f.graph)
        .current_dir(&f.graph)
        .args(["-c", "umask 000; exec \"$@\"", "fixture", BIN, "--root"])
        .arg(&f.graph)
        .args(["--as", "fixture-reader", "mcp"]);
    let mut mcp = Running::start(&mut shell);
    mcp.initialize_mcp();
    let common = mdkg_coord_sqlite::git::common_dir(&f.graph).unwrap();
    let dir = mdkg_run::state::run_dir(&f.graph).unwrap();
    for path in [common.join("mdkg"), dir.clone(), dir.join("builds")] {
        let metadata = fs::symlink_metadata(&path).unwrap();
        assert_eq!(
            metadata.permissions().mode() & 0o777,
            0o700,
            "{}",
            path.display()
        );
        assert_eq!(metadata.uid(), rustix::process::geteuid().as_raw());
    }
    assert_eq!(status(&f.graph)["processes"].as_array().unwrap().len(), 1);
    mcp.finish();
}
