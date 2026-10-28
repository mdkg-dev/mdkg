//! `mdkg agents` end to end: `up` wakes the configured agents (a fake
//! Claude) on their mail under their policies, the loop guard pauses a
//! thread, and a person's `run` or `agents resume` lets it run again.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};

mod common;

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).ok();
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
        .env("MDKG_WAIT_POLL_MS", "20")
        .env("MDKG_RUN_CLAUDE", fixture("fake-claude.sh"))
        // Never the real Keychain: no key there, as far as tests know.
        .env("MDKG_RUN_SECURITY", "/usr/bin/false")
        .env_remove("MDKG_RUN_MDKG")
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../mdkg-run/tests/fixtures")
        .join(name)
}

fn sh(dir: &Path, args: &[&str]) {
    let out = isolated(Command::new("git").arg("-C").arg(dir).args(args))
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
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

/// A repository on `main` with the launcher's graph worktree, whose config
/// gets `agents`.
fn make_repo(agents: Value) -> (Scratch, PathBuf) {
    let dir = common::temp::dir("mdkg-cli-agents");
    let dir = fs::canonicalize(dir).expect("canonical");
    let repo = dir.join("repo");
    let conformance = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance");
    fs::create_dir_all(&repo).expect("repo");
    copy_tree(&conformance.join("base"), &repo);
    copy_tree(&conformance.join("cases/g01-valid-basic"), &repo);
    fs::remove_file(repo.join("commands")).ok();
    fs::remove_file(repo.join("README.md")).ok();
    fs::write(
        repo.join(".gitignore"),
        "*.args\n*.prompt\n*.environ\n/.worktrees/\n",
    )
    .expect("ignore");
    let config = repo.join(".mdkg/config.json");
    let mut cfg: Value = serde_json::from_slice(&fs::read(&config).expect("config")).expect("json");
    cfg["agents"] = agents;
    fs::write(&config, cfg.to_string()).expect("w");
    sh(&repo, &["init", "-q", "-b", "main"]);
    sh(&repo, &["add", "."]);
    sh(&repo, &["commit", "-q", "-m", "init"]);
    // The conformance input is legacy; preserve its handles while giving the
    // managed checkout a committed 0.7 graph identity.
    let migration = ok(&repo, "launcher", &["migrate", "--dry-run"]);
    let plan = migration["data"]["plan"].as_str().expect("migration plan");
    ok(&repo, "launcher", &["migrate", "--apply", plan]);
    let graph = repo.join(".worktrees/graph");
    sh(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "mdkg-graph",
            graph.to_str().expect("utf-8"),
            "main",
        ],
    );
    // Register this legacy fixture checkout before managed graph writes.
    ok(&repo, "launcher", &["install"]);
    (Scratch(dir), repo)
}

fn graph(repo: &Path) -> PathBuf {
    repo.join(".worktrees/graph")
}

/// Config changes in a launcher fixture are deliberate committed setup.
fn commit_graph_config(graph: &Path) {
    sh(graph, &["add", ".mdkg/config.json"]);
    sh(graph, &["commit", "-qm", "configure fixture"]);
}

fn mdkg_out(root: &Path, actor: &str, args: &[&str]) -> (i32, String) {
    let out = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
        .current_dir(root)
        .arg("--root")
        .arg(root)
        .args(["--as", actor])
        .args(common::agent_turn_args(root, args))
        .arg("--json")
        .output()
        .expect("run mdkg");
    (
        out.status.code().unwrap_or(-1),
        if args.first() == Some(&"turn") {
            common::one_turn(&out.stdout).to_string()
        } else {
            String::from_utf8_lossy(&out.stdout).into_owned()
        },
    )
}

fn mdkg(root: &Path, actor: &str, args: &[&str]) -> (i32, Value) {
    let (code, out) = mdkg_out(root, actor, args);
    let envelope: Value = serde_json::from_str(&out).unwrap_or_else(|_| panic!("not JSON: {out}"));
    if args.first() != Some(&"turn") {
        common::conforms_envelope(&envelope);
    }
    (code, envelope)
}

fn ok(root: &Path, actor: &str, args: &[&str]) -> Value {
    let (code, out) = mdkg(root, actor, args);
    assert_eq!(code, 0, "{args:?}: {out}");
    out
}

/// `agents up --once`: its exit code and one envelope per event.
fn up(repo: &Path, extra: &[&str]) -> (i32, Vec<Value>) {
    let mut args = vec!["agents", "up", "--once"];
    args.extend_from_slice(extra);
    let (code, out) = mdkg_out(&graph(repo), "launcher", &args);
    let events: Vec<Value> = out
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap_or_else(|_| panic!("not JSON lines: {out}")))
        .collect();
    for e in &events {
        common::conforms_envelope(e);
    }
    (code, events)
}

fn send(repo: &Path, to: &str, text: &str, reply_to: Option<&str>) -> String {
    let mut args = vec![
        "msg", "send", "--to", to, "--kind", "handoff", "--text", text,
    ];
    if let Some(r) = reply_to {
        args.extend(["--reply-to", r]);
    }
    ok(repo, "nick", &args)["data"]["message"]["messageId"]
        .as_str()
        .expect("id")
        .to_string()
}

fn turns<'a>(events: &'a [Value], agent: &str) -> Vec<&'a Value> {
    events
        .iter()
        .filter(|e| e["data"]["agent"] == agent && e["data"]["event"] == "turn")
        .collect()
}

fn git_text(root: &Path, args: &[&str]) -> String {
    let out = isolated(Command::new("git").arg("-C").arg(root).args(args))
        .output()
        .expect("git runs");
    assert!(out.status.success(), "{args:?}: {out:?}");
    String::from_utf8(out.stdout)
        .expect("Git output is UTF-8")
        .trim()
        .to_string()
}

fn assert_clean(root: &Path) {
    assert_eq!(git_text(root, &["status", "--porcelain"]), "");
}

fn assert_definition_commit(root: &Path, out: &Value, action: &str) {
    assert_clean(root);
    let commit = git_text(root, &["rev-parse", "HEAD"]);
    assert_eq!(out["data"]["commit"], commit);
    assert_eq!(
        out["data"]["changesets"]
            .as_array()
            .expect("changesets")
            .len(),
        1
    );
    assert_eq!(
        git_text(root, &["log", "-1", "--format=%s"]),
        format!("{action} agent writer")
    );
    assert_eq!(
        git_text(
            root,
            &["diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"]
        ),
        ".mdkg/agents/writer.json"
    );
    let status = ok(root, "launcher", &["status"]);
    assert!(
        status["data"]["leases"]
            .as_array()
            .expect("leases")
            .iter()
            .all(|l| l["scope"] != ".mdkg/agents/writer.json")
    );
}

#[test]
fn definition_edits_twice_then_graph_write_keep_checkout_clean() {
    let (_dir, repo) = make_repo(json!({}));
    let root = graph(&repo);
    // A tracked starting definition reproduces the D12 freeze after edit.
    fs::create_dir_all(root.join(".mdkg/agents")).unwrap();
    fs::write(
        root.join(".mdkg/agents/writer.json"),
        json!({"schema":"mdkg.agent.v1", "harness":"claude", "role":"worker"}).to_string(),
    )
    .unwrap();
    sh(&root, &["add", ".mdkg/agents/writer.json"]);
    sh(&root, &["commit", "-qm", "seed definition"]);
    for instructions in ["First edit", "Second edit"] {
        let out = ok(
            &root,
            "launcher",
            &["agents", "edit", "writer", "--instructions", instructions],
        );
        assert_definition_commit(&root, &out, "Edit");
    }
    ok(&root, "launcher", &["done", "task-1"]);
    assert_clean(&root);
}

#[test]
fn definition_concurrent_same_actor_edits_preserve_both_changes() {
    use std::time::{Duration, Instant};

    struct Running(std::process::Child);
    impl Drop for Running {
        fn drop(&mut self) {
            self.0.kill().ok();
            self.0.wait().ok();
        }
    }

    let (dir, repo) = make_repo(json!({}));
    let root = graph(&repo);
    let added = ok(
        &root,
        "launcher",
        &[
            "agents",
            "add",
            "writer",
            "--harness",
            "claude",
            "--role",
            "worker",
        ],
    );
    assert_definition_commit(&root, &added, "Add");
    let seed = added["data"]["commit"].as_str().expect("seed commit");
    let path = ".mdkg/agents/writer.json";
    let original = fs::read(root.join(path)).expect("seed definition");
    // Both commands have the same actor and initially share its exact lease.
    // The graph guard must serialize them even though lease reuse is allowed.
    ok(&root, "launcher", &["claim", path]);
    let lock = fs::File::options()
        .read(true)
        .write(true)
        .open(repo.join(".git/mdkg/graph-worktree.lock"))
        .expect("graph lock");
    lock.lock().expect("hold graph lock");
    let mut writers = Vec::new();
    for (field, value) in [
        ("--instructions", "Keep both edits."),
        ("--model", "sample"),
    ] {
        let stdout = dir.0.join(format!("{field}.out"));
        let stderr = dir.0.join(format!("{field}.err"));
        let child = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
            .current_dir(&root)
            .arg("--root")
            .arg(&root)
            .args([
                "--as", "launcher", "agents", "edit", "writer", field, value, "--json",
            ])
            .stdout(fs::File::create(&stdout).expect("stdout"))
            .stderr(fs::File::create(&stderr).expect("stderr"))
            .spawn()
            .expect("definition writer");
        writers.push((Running(child), stdout, stderr));
    }
    // Gate both processes on a clean checkout, not on a timing-sensitive dirty
    // file. Neither may read/modify/commit while another graph user owns it.
    let until = Instant::now() + Duration::from_secs(2);
    while Instant::now() < until {
        for (writer, stdout, stderr) in &mut writers {
            assert!(
                writer.0.try_wait().expect("writer status").is_none(),
                "writer escaped graph lock: stdout={:?}, stderr={:?}",
                fs::read_to_string(stdout),
                fs::read_to_string(stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        fs::read(root.join(path)).expect("waiting definition"),
        original
    );
    assert_eq!(git_text(&root, &["rev-parse", "HEAD"]), seed);
    assert_clean(&root);
    drop(lock);

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut commits = Vec::new();
    let mut changesets = Vec::new();
    for (writer, stdout, stderr) in &mut writers {
        let status = loop {
            if let Some(status) = writer.0.try_wait().expect("writer status") {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "definition writer did not finish"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        let out = fs::read_to_string(stdout).expect("writer stdout");
        assert!(
            status.success(),
            "writer failed: {status}, stdout={out}, stderr={:?}",
            fs::read_to_string(stderr)
        );
        let receipt: Value = serde_json::from_str(&out).expect("writer receipt");
        common::conforms_envelope(&receipt);
        assert_eq!(receipt["ok"], true);
        let recorded = receipt["data"]["changesets"]
            .as_array()
            .expect("changesets");
        assert_eq!(recorded.len(), 1);
        changesets.push(recorded[0].clone());
        let commit = receipt["data"]["commit"].as_str().expect("commit");
        assert_eq!(
            git_text(&root, &["log", "-1", "--format=%s", commit]),
            "Edit agent writer"
        );
        assert_eq!(
            git_text(
                &root,
                &["diff-tree", "--no-commit-id", "--name-only", "-r", commit]
            ),
            path
        );
        commits.push(commit.to_string());
    }
    assert_ne!(changesets[0], changesets[1]);
    commits.sort();
    assert_ne!(commits[0], commits[1]);
    let range = format!("{seed}..HEAD");
    let mut history: Vec<String> = git_text(&root, &["rev-list", &range])
        .lines()
        .map(str::to_string)
        .collect();
    history.sort();
    assert_eq!(
        commits, history,
        "both receipts must be successive graph commits"
    );
    let definition: Value = serde_json::from_slice(&fs::read(root.join(path)).expect("definition"))
        .expect("definition JSON");
    assert_eq!(definition["instructions"], "Keep both edits.");
    assert_eq!(definition["model"], "sample");
    assert_clean(&root);
    let status = ok(&root, "launcher", &["status"]);
    assert!(
        status["data"]["leases"]
            .as_array()
            .expect("leases")
            .iter()
            .all(|l| l["scope"] != path)
    );
    ok(&root, "launcher", &["done", "task-1"]);
    assert_clean(&root);
}

#[test]
fn definition_commands_preserve_fields_validate_before_writing_and_remove_only_the_file() {
    let (_dir, repo) = make_repo(json!({}));
    let root = graph(&repo);
    let added = ok(
        &root,
        "launcher",
        &[
            "agents",
            "add",
            "writer",
            "--harness",
            "claude",
            "--role",
            "worker",
            "--model",
            "sample",
            "--instructions",
            "Use leases.",
        ],
    );
    let path = root.join(".mdkg/agents/writer.json");
    assert_definition_commit(&root, &added, "Add");
    assert_eq!(added["data"]["path"], path.to_string_lossy().as_ref());
    let original = fs::read(&path).expect("definition");
    let definition: Value = serde_json::from_slice(&original).expect("json");
    assert_eq!(definition["schema"], "mdkg.agent.v1");
    assert_eq!(definition["reports_to"], "orchestrator");
    let (code, _) = mdkg(
        &root,
        "launcher",
        &[
            "agents",
            "add",
            "writer",
            "--harness",
            "codex",
            "--role",
            "worker",
        ],
    );
    assert_ne!(code, 0);
    assert_eq!(fs::read(&path).unwrap(), original);
    for extra in [
        vec!["--effort", "impossible"],
        vec!["--policy", "[]"],
        vec!["--policy", "{\"unknown\":true}"],
        vec!["--cwd", "elsewhere", "--worktree"],
    ] {
        let mut args = vec!["agents", "edit", "writer"];
        args.extend(extra);
        let (code, _) = mdkg(&root, "launcher", &args);
        assert_ne!(code, 0, "{args:?}");
        assert_eq!(fs::read(&path).unwrap(), original, "{args:?}");
    }
    let edited = ok(
        &root,
        "launcher",
        &[
            "agents",
            "edit",
            "writer",
            "--unset",
            "model",
            "--policy",
            "{\"timeout\":90}",
            "--reports-to",
            "coordinator",
        ],
    );
    let result = &edited["data"]["definition"];
    assert_definition_commit(&root, &edited, "Edit");
    assert!(result.get("model").is_none());
    assert_eq!(result["instructions"], "Use leases.");
    assert_eq!(result["policy"]["timeout"], 90);
    assert_eq!(result["reports_to"], "coordinator");
    let local = repo.join(".git/mdkg/run/writer.json");
    fs::create_dir_all(local.parent().unwrap()).unwrap();
    fs::write(&local, "machine-local state").unwrap();
    let removed = ok(&root, "launcher", &["agents", "rm", "writer"]);
    assert_definition_commit(&root, &removed, "Remove");
    assert!(!path.exists());
    assert_eq!(fs::read_to_string(local).unwrap(), "machine-local state");
    for name in ["defaults", "../escape", "run:writer"] {
        let (code, _) = mdkg(
            &root,
            "launcher",
            &[
                "agents",
                "add",
                name,
                "--harness",
                "claude",
                "--role",
                "worker",
            ],
        );
        assert_ne!(code, 0, "{name}");
    }
}

#[test]
fn definition_files_drive_headless_selection_and_override_inline_entries() {
    let (_dir, repo) = make_repo(json!({
        "defaults": {"timeout": 60},
        "writer": {"harness": "codex", "role": "reviewer", "model": "legacy-model"}
    }));
    let root = graph(&repo);
    ok(
        &root,
        "launcher",
        &[
            "agents",
            "add",
            "writer",
            "--harness",
            "claude",
            "--role",
            "worker",
            "--model",
            "file-model",
            "--instructions",
            "Read the file definition.",
            "--policy",
            "{\"loop_guard\":false}",
        ],
    );
    let status = ok(&root, "launcher", &["agents", "status"]);
    let writer = &status["data"]["agents"][0];
    assert_eq!(writer["configured"], true);
    assert_eq!(writer["policy"]["timeout"], 60);
    assert!(writer["policy"]["loop_guard"].is_null());
    send(&repo, "writer", "Run a file-defined turn.", None);
    let (code, events) = up(&repo, &[]);
    assert_eq!(code, 0, "{events:?}");
    assert_eq!(turns(&events, "writer").len(), 1, "{events:?}");
    let state: Value =
        serde_json::from_slice(&fs::read(repo.join(".git/mdkg/run/writer.json")).unwrap()).unwrap();
    assert_eq!(state["spec"]["harness"], "claude");
    assert_eq!(state["spec"]["role"], "worker");
    assert_eq!(state["spec"]["model"], "file-model");
    assert!(
        fs::read_to_string(repo.join(".worktrees/writer/claude.prompt"))
            .unwrap()
            .contains("Read the file definition.")
    );
    let (code, out) = mdkg(&root, "launcher", &["agents", "rm", "writer"]);
    assert_ne!(code, 0, "{out}");
    assert!(root.join(".mdkg/agents/writer.json").is_file());
}

#[test]
fn legacy_definitions_warn_and_can_be_copied_to_files_with_edit() {
    let (_dir, repo) =
        make_repo(json!({"old": {"harness":"claude", "role":"reviewer", "model":"keep"}}));
    let root = graph(&repo);
    let out = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
        .current_dir(&root)
        .args(["agents", "status", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("inline agents"));
    let edited = ok(&root, "launcher", &["agents", "edit", "old"]);
    assert_eq!(edited["data"]["definition"]["model"], "keep");
    let out = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
        .current_dir(&root)
        .args(["agents", "status", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(!String::from_utf8_lossy(&out.stderr).contains("inline agents"));
}

#[test]
fn a_complete_file_can_repair_an_invalid_definition_and_flags_override_it() {
    let (_dir, repo) = make_repo(json!({}));
    let root = graph(&repo);
    let dir = root.join(".mdkg/agents");
    fs::create_dir(&dir).unwrap();
    fs::write(dir.join("writer.json"), "broken JSON").unwrap();
    let source = repo.join("source.json");
    fs::write(
        &source,
        json!({"schema":"mdkg.agent.v1", "harness":"codex", "role":"worker", "model":"original"})
            .to_string(),
    )
    .unwrap();
    let result = ok(
        &root,
        "launcher",
        &[
            "agents",
            "edit",
            "writer",
            "--file",
            source.to_str().unwrap(),
            "--model",
            "replacement",
        ],
    );
    assert_eq!(result["data"]["definition"]["harness"], "codex");
    assert_eq!(result["data"]["definition"]["model"], "replacement");
    assert_eq!(result["data"]["definition"]["reports_to"], "orchestrator");
    fs::write(dir.join("writer.json"), "broken again").unwrap();
    // Intentional fixture damage is committed: the freeze must still reject
    // a dirty graph checkout, even when the command would remove the file.
    sh(&root, &["add", ".mdkg/agents/writer.json"]);
    sh(&root, &["commit", "-qm", "seed invalid definition"]);
    ok(&root, "launcher", &["agents", "rm", "writer"]);
    assert!(!dir.join("writer.json").exists());
}

#[test]
fn definition_writes_respect_leases_and_roll_back_failed_commits() {
    let (_dir, repo) = make_repo(json!({}));
    let root = graph(&repo);
    let target = ".mdkg/agents/writer.json";
    let path = root.join(target);
    let add = [
        "agents",
        "add",
        "writer",
        "--harness",
        "claude",
        "--role",
        "worker",
    ];
    // The path exists in the other writer's checkout, before publication.
    fs::create_dir_all(repo.join(".mdkg/agents")).unwrap();
    fs::write(repo.join(target), "other writer's definition").unwrap();
    ok(&repo, "other", &["claim", target]);
    assert_eq!(mdkg(&root, "launcher", &add).0, 5);
    assert!(!path.exists());
    ok(&root, "other", &["release", target]);
    fs::remove_file(repo.join(target)).unwrap();

    let failed = |args: &[&str]| {
        for injected in [false, true] {
            let before = fs::read(&path).ok();
            let head = git_text(&root, &["rev-parse", "HEAD"]);
            let mut command = Command::new(env!("CARGO_BIN_EXE_mdkg"));
            isolated(&mut command)
                .current_dir(&root)
                .args(["--as", "launcher"])
                .args(args)
                .arg("--json");
            if injected {
                command.env("MDKG_FAIL_AFTER", "definition-written");
            } else {
                command
                    .env("GIT_AUTHOR_NAME", "")
                    .env("GIT_COMMITTER_NAME", "");
            }
            let out = command.output().unwrap();
            assert!(!out.status.success(), "{out:?}");
            assert_eq!(fs::read(&path).ok(), before);
            assert_eq!(git_text(&root, &["rev-parse", "HEAD"]), head);
            assert_clean(&root);
            let status = ok(&root, "launcher", &["status"]);
            assert!(
                status["data"]["leases"].as_array().unwrap().is_empty(),
                "{status}"
            );
        }
    };
    failed(&add);
    ok(&root, "launcher", &add);
    let edit = [
        "agents",
        "edit",
        "writer",
        "--instructions",
        "New instructions",
    ];
    let remove = ["agents", "rm", "writer"];
    ok(&root, "other", &["claim", target]);
    let before = fs::read(&path).unwrap();
    for args in [&edit[..], &remove[..]] {
        assert_eq!(mdkg(&root, "launcher", args).0, 5);
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_clean(&root);
    }
    ok(&root, "other", &["release", target]);
    failed(&edit);
    failed(&remove);
    // A failed validation must not release a lease the caller already held.
    ok(&root, "launcher", &["claim", target]);
    assert_ne!(
        mdkg(
            &root,
            "launcher",
            &["agents", "edit", "writer", "--effort", "impossible"]
        )
        .0,
        0
    );
    assert_eq!(
        ok(&root, "launcher", &["holds", target])["data"]["holds"],
        true
    );
    let out = ok(&root, "launcher", &edit);
    assert_definition_commit(&root, &out, "Edit");
}

#[test]
fn definition_process_crash_after_add_recovers_with_edit() {
    let (_dir, repo) = make_repo(json!({}));
    let root = graph(&repo);
    let head = git_text(&root, &["rev-parse", "HEAD"]);
    let out = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
        .current_dir(&root)
        .args([
            "--as",
            "launcher",
            "agents",
            "add",
            "writer",
            "--harness",
            "claude",
            "--role",
            "worker",
        ])
        .env("MDKG_CRASH_AFTER", "definition-written")
        .output()
        .unwrap();
    // The subprocess exits without unwinding: neither rollback nor lease
    // cleanup runs, just as when the writer dies at this boundary.
    assert_eq!(out.status.code(), Some(86), "{out:?}");
    let path = root.join(".mdkg/agents/writer.json");
    assert!(path.is_file());
    assert_eq!(git_text(&root, &["rev-parse", "HEAD"]), head);
    assert_eq!(
        git_text(&root, &["ls-files", "--", ".mdkg/agents/writer.json"]),
        ""
    );
    let status = ok(&root, "launcher", &["status"]);
    assert!(
        status["data"]["leases"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l["scope"] == ".mdkg/agents/writer.json" && l["owner"] == "launcher")
    );
    let (code, retry) = mdkg(
        &root,
        "launcher",
        &[
            "agents",
            "add",
            "writer",
            "--harness",
            "claude",
            "--role",
            "worker",
        ],
    );
    assert_ne!(code, 0);
    assert!(
        retry["diagnostics"][0]["message"]
            .as_str()
            .unwrap()
            .contains("use agents edit")
    );
    let edited = ok(&root, "launcher", &["agents", "edit", "writer"]);
    assert_definition_commit(&root, &edited, "Edit");
    ok(&root, "launcher", &["done", "task-1"]);
    assert_clean(&root);
}

#[test]
fn definition_process_crash_edit_and_rm_recover_with_printed_restore_commands() {
    for args in [
        vec!["agents", "edit", "writer", "--instructions", "Crash edit"],
        vec!["agents", "rm", "writer"],
    ] {
        let (_dir, repo) = make_repo(json!({}));
        let root = graph(&repo);
        let target = ".mdkg/agents/writer.json";
        ok(
            &root,
            "launcher",
            &[
                "agents",
                "add",
                "writer",
                "--harness",
                "claude",
                "--role",
                "worker",
            ],
        );
        let before = fs::read(root.join(target)).unwrap();
        let head = git_text(&root, &["rev-parse", "HEAD"]);
        let out = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
            .current_dir(&root)
            .args(["--as", "launcher"])
            .args(&args)
            .env("MDKG_CRASH_AFTER", "definition-written")
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(86), "{args:?}: {out:?}");
        assert_eq!(git_text(&root, &["rev-parse", "HEAD"]), head);
        assert_ne!(fs::read(root.join(target)).ok(), Some(before.clone()));
        let (code, frozen) = mdkg(&root, "launcher", &["done", "task-1"]);
        assert_eq!(code, 4, "{frozen}");
        let diagnostic = &frozen["diagnostics"][0];
        assert!(diagnostic["message"].as_str().unwrap().contains(target));
        let hint = diagnostic["hint"].as_str().unwrap();
        assert!(
            hint.contains(target) && hint.contains("restore --source=HEAD"),
            "{hint}"
        );
        assert_eq!(mdkg(&root, "launcher", &["agents", "edit", "writer"]).0, 4);
        let commands: Vec<_> = hint
            .lines()
            .filter(|line| line.contains("restore --source=HEAD"))
            .collect();
        assert_eq!(commands.len(), 2, "{hint}");
        for line in commands {
            let cmd = line.split_once(": git ").expect("exact printed command").1;
            let restored = isolated(&mut Command::new("/bin/sh"))
                .current_dir(&root)
                .args(["-c", &format!("git {cmd}")])
                .output()
                .unwrap();
            assert!(restored.status.success(), "{restored:?}");
        }
        assert_eq!(fs::read(root.join(target)).unwrap(), before);
        assert_clean(&root);
        // Neither the crash nor the freeze silently releases the lease.
        let status = ok(&root, "launcher", &["status"]);
        assert!(
            status["data"]["leases"]
                .as_array()
                .unwrap()
                .iter()
                .any(|l| l["scope"] == target && l["owner"] == "launcher")
        );
        // The child has exited; the operator can release its exact lease
        // immediately rather than waiting for its two-hour deadline.
        ok(&root, "launcher", &["release", target]);
        let edited = ok(
            &root,
            "replacement",
            &["agents", "edit", "writer", "--instructions", "After repair"],
        );
        assert_definition_commit(&root, &edited, "Edit");
        ok(&root, "replacement", &["done", "task-1"]);
        assert_clean(&root);
    }
}

#[test]
fn definition_noops_and_untracked_removal_preserve_head() {
    let (_dir, repo) = make_repo(json!({}));
    let root = graph(&repo);
    let added = ok(
        &root,
        "launcher",
        &[
            "agents",
            "add",
            "writer",
            "--harness",
            "claude",
            "--role",
            "worker",
        ],
    );
    let edited = ok(&root, "launcher", &["agents", "edit", "writer"]);
    assert_eq!(added["data"]["commit"], edited["data"]["commit"]);
    assert_clean(&root);
    fs::write(
        root.join(".mdkg/agents/temporary.json"),
        "invalid hand-created definition",
    )
    .unwrap();
    let removed = ok(&root, "launcher", &["agents", "rm", "temporary"]);
    assert_eq!(removed["data"]["commit"], added["data"]["commit"]);
    assert_eq!(removed["data"]["changesets"], json!([]));
    assert_clean(&root);
}

#[test]
fn definition_text_receipts_and_commits_preserve_unrelated_staging() {
    let (_dir, repo) = make_repo(json!({}));
    fs::write(repo.join("unrelated.txt"), "keep staged").unwrap();
    sh(&repo, &["add", "unrelated.txt"]);
    for args in [
        vec![
            "agents",
            "add",
            "writer",
            "--harness",
            "claude",
            "--role",
            "worker",
        ],
        vec!["agents", "edit", "writer", "--instructions", "Changed"],
        vec!["agents", "rm", "writer"],
    ] {
        let out = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
            .current_dir(&repo)
            .args(["--as", "launcher"])
            .args(&args)
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}: {out:?}");
        let head = git_text(&repo, &["rev-parse", "HEAD"]);
        assert!(String::from_utf8_lossy(&out.stdout).contains(&head));
        assert_eq!(
            git_text(&repo, &["diff", "--cached", "--name-only"]),
            "unrelated.txt"
        );
        assert_eq!(
            git_text(
                &repo,
                &["diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"]
            ),
            ".mdkg/agents/writer.json"
        );
    }
}

#[test]
fn extends_is_preserved_and_refused_at_launch_without_remote_access() {
    let (_dir, repo) = make_repo(json!({}));
    let root = graph(&repo);
    let added = ok(
        &root,
        "launcher",
        &[
            "agents",
            "add",
            "future",
            "--extends",
            "https://example.invalid/agent.json",
        ],
    );
    assert_eq!(
        added["data"]["definition"]["extends"],
        "https://example.invalid/agent.json"
    );
    let (code, events) = up(&repo, &["future"]);
    assert_ne!(code, 0);
    assert!(
        events[0]["diagnostics"][0]["message"]
            .as_str()
            .unwrap()
            .contains("extends is reserved"),
        "{events:?}"
    );
    assert!(!repo.join(".git/mdkg/run/future.json").exists());
    assert!(!repo.join(".worktrees/future").exists());
}

#[test]
fn definitions_reject_unknown_fields_wrong_schema_large_files_and_links() {
    let (_dir, repo) = make_repo(json!({}));
    let root = graph(&repo);
    let dir = root.join(".mdkg/agents");
    fs::create_dir(&dir).unwrap();
    let path = dir.join("bad.json");
    for definition in [
        json!({"schema":"other", "harness":"claude", "role":"worker"}),
        json!({"schema":"mdkg.agent.v1", "harness":"claude", "role":"worker", "typo":true}),
        json!({"schema":"mdkg.agent.v1", "harness":"claude"}),
    ] {
        fs::write(&path, definition.to_string()).unwrap();
        let (code, _) = mdkg(&root, "launcher", &["agents", "status"]);
        assert_ne!(code, 0, "{definition}");
    }
    fs::write(&path, " ".repeat((1 << 20) + 1)).unwrap();
    let (code, _) = mdkg(&root, "launcher", &["agents", "status"]);
    assert_ne!(code, 0);
    fs::remove_file(&path).unwrap();
    let outside = repo.join("outside.json");
    fs::write(
        &outside,
        json!({"schema":"mdkg.agent.v1", "harness":"claude", "role":"worker"}).to_string(),
    )
    .unwrap();
    std::os::unix::fs::symlink(&outside, &path).unwrap();
    let (code, _) = mdkg(&root, "launcher", &["agents", "status"]);
    assert_ne!(code, 0);
    fs::remove_file(&path).unwrap();
    fs::remove_dir(&dir).unwrap();
    let outside_dir = repo.join("outside-agents");
    fs::create_dir(&outside_dir).unwrap();
    std::os::unix::fs::symlink(&outside_dir, &dir).unwrap();
    let (code, _) = mdkg(
        &root,
        "launcher",
        &[
            "agents",
            "add",
            "writer",
            "--harness",
            "claude",
            "--role",
            "worker",
        ],
    );
    assert_ne!(code, 0);
    assert_eq!(fs::read_dir(outside_dir).unwrap().count(), 0);
}

#[test]
fn run_is_removed_and_definition_fields_belong_to_management_commands() {
    let (_dir, repo) = make_repo(json!({}));
    for args in [
        &["run", "writer", "--json"][..],
        &["agents", "up", "writer", "--harness", "claude", "--json"],
    ] {
        let out = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
            .current_dir(graph(&repo))
            .args(args)
            .output()
            .unwrap();
        assert!(!out.status.success(), "{args:?}");
    }
    assert!(!repo.join(".git/mdkg/run/writer.json").exists());
}

#[test]
fn codex_ultra_definition_reaches_a_fake_turn_and_other_harnesses_refuse_it() {
    let (dir, repo) = make_repo(json!({}));
    let root = graph(&repo);
    let home = dir.0.join("isolated-home");
    fs::create_dir(&home).unwrap();
    let run = |actor: &str, args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_mdkg"))
            .current_dir(&root)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").expect("runner PATH"))
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .env("MDKG_RUN_CODEX", fixture("fake-codex.sh"))
            .env("MDKG_RUN_SECURITY", "/usr/bin/false")
            .args(["--as", actor])
            .args(args)
            .arg("--json")
            .output()
            .expect("fixture command")
    };
    let created = run(
        "launcher",
        &[
            "agents",
            "add",
            "astra",
            "--harness",
            "codex",
            "--role",
            "worker",
            "--model",
            "gpt-6-astra",
            "--effort",
            "ultra",
        ],
    );
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stdout)
    );
    let definition = root.join(".mdkg/agents/astra.json");
    let bytes = fs::read(&definition).unwrap();
    // agents add commits the definition itself (bug-01a1807c).
    for harness in ["claude", "opencode"] {
        let refused = run(
            "launcher",
            &["agents", "edit", "astra", "--harness", harness],
        );
        assert!(!refused.status.success(), "{harness}");
        let result: Value = serde_json::from_slice(&refused.stdout).unwrap();
        assert!(
            result["diagnostics"].as_array().unwrap().iter().any(|d| {
                d["code"] == "usage.invalid_arguments"
                    && d["message"].as_str().is_some_and(|s| s.contains("effort"))
            }),
            "{result}"
        );
        assert_eq!(fs::read(&definition).unwrap(), bytes, "{harness}");
    }
    let sent = run(
        "sender",
        &[
            "msg",
            "send",
            "--to",
            "astra",
            "--kind",
            "note",
            "--text",
            "Fixture turn",
        ],
    );
    assert!(
        sent.status.success(),
        "{}",
        String::from_utf8_lossy(&sent.stdout)
    );
    let turn = run("launcher", &["agents", "up", "astra", "--once"]);
    let output = String::from_utf8_lossy(&turn.stdout);
    assert!(
        turn.status.success(),
        "{output}\n{}",
        String::from_utf8_lossy(&turn.stderr)
    );
    let events: Vec<Value> = output
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(
        events
            .iter()
            .any(|e| e["data"]["turn"]["outcome"] == "replied"),
        "{output}"
    );
    let args = fs::read_to_string(repo.join(".worktrees/astra/codex.args")).unwrap();
    let args: Vec<_> = args.lines().collect();
    assert!(
        args.windows(2)
            .any(|pair| pair == ["-c", "model_reasoning_effort=\"ultra\""])
    );
    assert!(args.windows(2).any(|pair| pair == ["-m", "gpt-6-astra"]));
    let saved: Value =
        serde_json::from_slice(&fs::read(repo.join(".git/mdkg/run/astra.json")).unwrap()).unwrap();
    assert_eq!(saved["spec"]["effort"], "ultra");
    assert_eq!(saved["turns"], 1);

    // Legacy inline entries still reach the harness-aware state validator
    // before either a saved spec or a worker checkout can be created.
    let config_path = root.join(".mdkg/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    for harness in ["codex", "claude", "opencode"] {
        config["agents"][format!("inline-{harness}")] = json!({
            "harness": harness, "role": "worker", "effort": "ultra"
        });
    }
    fs::write(&config_path, config.to_string()).unwrap();
    commit_graph_config(&root);
    for harness in ["codex", "claude", "opencode"] {
        let name = format!("inline-{harness}");
        let result = run("launcher", &["agents", "up", &name, "--once", "--dry-run"]);
        assert_eq!(
            result.status.success(),
            harness == "codex",
            "{}",
            String::from_utf8_lossy(&result.stdout)
        );
        assert!(!repo.join(format!(".git/mdkg/run/{name}.json")).exists());
        assert!(!repo.join(format!(".worktrees/{name}")).exists());
    }

    // Without a definition or harness/effort overrides, a resumed agent must
    // still reject invalid machine-local state before preparing a turn.
    fs::remove_file(&definition).unwrap();
    sh(&root, &["add", ".mdkg/agents/astra.json"]);
    sh(&root, &["commit", "-qm", "remove fixture definition"]);
    let state_path = repo.join(".git/mdkg/run/astra.json");
    let mut saved = saved;
    saved["spec"]["harness"] = json!("claude");
    let invalid_bytes = saved.to_string();
    fs::write(&state_path, &invalid_bytes).unwrap();
    let resumed = run(
        "launcher",
        &["agents", "up", "astra", "--once", "--dry-run"],
    );
    let result: Value = serde_json::from_slice(&resumed.stdout).unwrap();
    assert!(!resumed.status.success(), "{result}");
    assert!(
        result["diagnostics"].as_array().unwrap().iter().any(|d| {
            d["code"] == "usage.invalid_arguments"
                && d["message"].as_str().is_some_and(|s| s.contains("effort"))
        }),
        "{result}"
    );
    assert_eq!(fs::read_to_string(state_path).unwrap(), invalid_bytes);
    assert!(!repo.join(".worktrees/astra/claude.args").exists());

    // Follow the actual OpenCode recovery hint through the public CLI. The
    // earlier hint mistakenly named a --harness flag on `agents up`.
    let state_path = repo.join(".git/mdkg/run/astra.json");
    saved["spec"]["harness"] = json!("opencode");
    saved["spec"]["effort"] = json!("low");
    fs::write(&state_path, saved.to_string()).unwrap();
    let refused = run(
        "launcher",
        &["agents", "up", "astra", "--once", "--dry-run"],
    );
    assert!(!refused.status.success());
    let result: Value = serde_json::from_slice(&refused.stdout).unwrap();
    let hint = result["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|d| d["hint"].as_str())
        .expect("recovery hint");
    let command = hint
        .split('`')
        .nth(1)
        .expect("hint command")
        .replace("<name>", "astra")
        .replace("<role>", "worker");
    let args = command.split_whitespace().skip(1).collect::<Vec<_>>();
    let recovered = run("launcher", &args);
    assert!(
        recovered.status.success(),
        "{}",
        String::from_utf8_lossy(&recovered.stdout)
    );
    let definition_value: Value = serde_json::from_slice(&fs::read(&definition).unwrap()).unwrap();
    assert_eq!(definition_value["harness"], "opencode");
    assert!(definition_value.get("effort").is_none());
    // The hinted agents add commits the recovered definition itself (bug-01a1807c).
    let resumed = run(
        "launcher",
        &["agents", "up", "astra", "--once", "--dry-run"],
    );
    assert!(
        resumed.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&resumed.stdout),
        String::from_utf8_lossy(&resumed.stderr)
    );
}

#[test]
fn definitions_apply_edits_and_unsets_when_a_manual_pass_reloads_saved_state() {
    let (_dir, repo) = make_repo(json!({}));
    let root = graph(&repo);
    ok(
        &root,
        "launcher",
        &[
            "agents",
            "add",
            "writer",
            "--harness",
            "claude",
            "--role",
            "worker",
            "--model",
            "old",
            "--effort",
            "high",
            "--policy",
            "{\"trigger\":\"manual\"}",
        ],
    );
    let (code, events) = up(&repo, &["writer"]);
    assert_eq!(code, 0, "{events:?}");
    assert_eq!(events[0]["data"]["turn"]["outcome"], "idle");
    send(&repo, "writer", "A manual turn.", None);
    let (code, events) = up(&repo, &["writer"]);
    assert_eq!(code, 0, "{events:?}");
    assert_eq!(events[0]["data"]["turn"]["outcome"], "replied");
    ok(
        &root,
        "launcher",
        &[
            "agents", "edit", "writer", "--unset", "model", "--unset", "effort",
        ],
    );
    let (code, events) = up(&repo, &["writer"]);
    assert_eq!(code, 0, "{events:?}");
    let saved: Value =
        serde_json::from_slice(&fs::read(repo.join(".git/mdkg/run/writer.json")).unwrap()).unwrap();
    assert!(saved["spec"].get("model").is_none());
    assert!(saved["spec"].get("effort").is_none());
    assert_eq!(saved["turns"], 1);
}

#[test]
fn auto_mode_and_native_overrides_are_preserved_and_other_presets_are_refused() {
    let (_dir, repo) = make_repo(json!({}));
    let root = graph(&repo);
    let added = ok(
        &root,
        "launcher",
        &[
            "agents",
            "add",
            "writer",
            "--harness",
            "codex",
            "--role",
            "worker",
            "--sandbox",
            "workspace-write",
            "--approvals",
            "on-request",
        ],
    );
    assert_eq!(added["data"]["definition"]["mode"], "auto");
    assert_eq!(added["data"]["definition"]["sandbox"], "workspace-write");
    assert_eq!(added["data"]["definition"]["approvals"], "on-request");
    let (code, error) = mdkg(
        &root,
        "launcher",
        &["agents", "edit", "writer", "--mode", "other"],
    );
    assert_eq!(code, 1, "{error}");
    assert!(error.to_string().contains("mode must be auto"));
    let edited = ok(
        &root,
        "launcher",
        &[
            "agents",
            "edit",
            "writer",
            "--unset",
            "sandbox",
            "--unset",
            "approvals",
        ],
    );
    assert!(edited["data"]["definition"].get("sandbox").is_none());
    assert!(edited["data"]["definition"].get("approvals").is_none());
}

#[test]
fn native_codex_overrides_reach_saved_state_and_removed_fields_clear() {
    for (field, value) in [("sandbox", "read-only"), ("approvals", "never")] {
        let (_dir, repo) = make_repo(json!({}));
        let root = graph(&repo);
        ok(
            &root,
            "launcher",
            &[
                "agents",
                "add",
                "writer",
                "--harness",
                "codex",
                "--role",
                "worker",
                &format!("--{field}"),
                value,
            ],
        );
        let (code, events) = up(&repo, &["writer"]);
        assert_eq!(code, 0, "{events:?}");
        assert_eq!(events[0]["data"]["turn"]["outcome"], "idle");
        let dir = mdkg_run::state::run_dir(&root).expect("run dir");
        let read = || {
            serde_json::from_slice::<Value>(&fs::read(dir.join("writer.json")).unwrap()).unwrap()
        };
        let saved = read();
        assert_eq!(saved["spec"][field], value);
        assert_eq!(saved["spec"]["mode"], "auto");
        assert_eq!(saved["spec"]["reports_to"], "orchestrator");
        ok(
            &root,
            "launcher",
            &[
                "agents",
                "edit",
                "writer",
                "--unset",
                field,
                "--reports-to",
                "lead",
            ],
        );
        assert_eq!(up(&repo, &["writer"]).0, 0);
        let saved = read();
        assert!(saved["spec"].get(field).is_none());
        assert_eq!(saved["spec"]["reports_to"], "lead");
    }
}

#[test]
fn unsupported_native_overrides_are_refused_before_writing_a_definition() {
    let (_dir, repo) = make_repo(json!({}));
    let root = graph(&repo);
    for (n, (harness, role, field, value)) in [
        ("codex", "reviewer", "sandbox", "read-only"),
        ("codex", "reviewer", "approvals", "never"),
        ("claude", "reviewer", "approvals", "manual"),
        ("opencode", "reviewer", "sandbox", "read-only"),
        ("claude", "worker", "sandbox", "read-only"),
        ("opencode", "worker", "sandbox", "read-only"),
        ("opencode", "worker", "approvals", "allow"),
    ]
    .iter()
    .enumerate()
    {
        let name = format!("bad{n}");
        let (code, out) = mdkg(
            &root,
            "launcher",
            &[
                "agents",
                "add",
                &name,
                "--harness",
                harness,
                "--role",
                role,
                &format!("--{field}"),
                value,
            ],
        );
        assert_ne!(code, 0, "{out}");
        let text = out.to_string();
        assert!(
            text.contains(if *role == "reviewer" {
                "reviewers are read-only"
            } else if *harness == "claude" {
                "Claude has no native sandbox"
            } else {
                "OpenCode sandbox and approvals overrides are not supported"
            }),
            "{out}"
        );
        assert!(!root.join(format!(".mdkg/agents/{name}.json")).exists());
    }
}

/// `agents status` names the allow rules a configured agent's turns would
/// drop before it has run once, read from its config (RC1 security audit
/// SEC5), so a cloned repository's rules can be checked first.
#[test]
fn status_names_dropped_rules_of_an_agent_not_yet_made() {
    let (_dir, repo) = make_repo(json!({
        "w": {"harness": "claude", "role": "worker",
              "allow": ["Bash(mdkg *)", "Bash(cargo test *)"]},
    }));
    let status = ok(&graph(&repo), "launcher", &["agents", "status"]);
    let w = &status["data"]["agents"][0];
    assert_eq!(w["name"], "w");
    assert_eq!(w["known"], false, "{w}");
    let dropped: Vec<&str> = w["dropped_allow"]
        .as_array()
        .expect("dropped rules")
        .iter()
        .filter_map(|d| d["rule"].as_str())
        .collect();
    assert_eq!(dropped, ["Bash(mdkg *)"], "{w}");
}

#[test]
fn up_runs_each_configured_agent_on_its_mail_and_makes_a_workers_worktree() {
    let (_dir, repo) = make_repo(json!({
        "coder": {"harness": "claude", "role": "worker",
                  "instructions": "Standing order: act on handoffs from nick."},
        "rev": {"harness": "claude", "role": "reviewer"},
        "idle": {"harness": "claude", "role": "reviewer"},
    }));
    // Before any agent is made, status still reads each one's account.
    let before = ok(&graph(&repo), "launcher", &["agents", "status"]);
    let first = &before["data"]["agents"][0];
    assert_eq!(first["known"], false);
    assert_eq!(first["auth"]["source"], "subscription", "{first}");
    // In the folder its turns will run in: a configured cwd where the
    // (fake) harness is logged out says so.
    fs::create_dir_all(graph(&repo).join("elsewhere")).expect("mkdir");
    fs::write(graph(&repo).join("elsewhere/fake-logged-out"), "").expect("w");
    let config = graph(&repo).join(".mdkg/config.json");
    let mut cfg: Value = serde_json::from_slice(&fs::read(&config).expect("r")).expect("json");
    cfg["agents"]["away"] = json!({"harness": "claude", "role": "reviewer", "cwd": "elsewhere"});
    fs::write(&config, cfg.to_string()).expect("w");
    commit_graph_config(&graph(&repo));
    let status = ok(&graph(&repo), "launcher", &["agents", "status"]);
    let away = status["data"]["agents"]
        .as_array()
        .expect("agents")
        .iter()
        .find(|a| a["name"] == "away")
        .expect("away")
        .clone();
    assert_eq!(away["auth"]["source"], "none", "{away}");
    cfg["agents"].as_object_mut().expect("o").remove("away");
    fs::write(&config, cfg.to_string()).expect("w");
    commit_graph_config(&graph(&repo));
    send(&repo, "coder", "pick up task-1", None);
    send(&repo, "rev", "review it", None);
    let (code, events) = up(&repo, &[]);
    assert_eq!(code, 0, "{events:?}");
    for agent in ["coder", "rev"] {
        let t = turns(&events, agent);
        assert_eq!(t.len(), 1, "{agent}: {events:?}");
        assert_eq!(t[0]["data"]["turn"]["outcome"], "replied", "{agent}");
    }
    assert!(turns(&events, "idle").is_empty(), "no mail, no turn");
    // The worker got its worktree from the orchestration names.
    let made = repo.join(".worktrees/coder");
    assert!(made.join(".git").is_file());
    let prompt = fs::read_to_string(made.join("claude.prompt")).expect("prompt");
    assert!(
        prompt.contains("Standing order: act on handoffs from nick."),
        "{prompt}"
    );
    assert!(prompt.contains("pick up task-1"));
    // The replies reached nick.
    let inbox = ok(&repo, "nick", &["msg", "inbox"]);
    let from: Vec<&str> = inbox["data"]["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .filter_map(|m| m["sender"].as_str())
        .collect();
    assert!(from.contains(&"coder") && from.contains(&"rev"), "{inbox}");
    // Nothing new: another pass runs nothing.
    let (code, events) = up(&repo, &[]);
    assert_eq!(code, 0);
    assert!(events.is_empty(), "{events:?}");
    let status = ok(&graph(&repo), "launcher", &["agents", "status"]);
    let coder = status["data"]["agents"]
        .as_array()
        .expect("agents")
        .iter()
        .find(|a| a["name"] == "coder")
        .expect("coder")
        .clone();
    assert_eq!(coder["turns"], 1);
    assert_eq!(coder["policy"]["loop_guard"]["turns"], 10);
    assert_eq!(coder["policy"]["trigger"]["kind"], "mail");
    // Its account: the (fake) Claude's subscription login; no fallback.
    assert_eq!(coder["auth"]["source"], "subscription", "{coder}");
    assert_eq!(coder["auth"]["method"], "claude.ai");
    assert_eq!(coder["auth"]["api_key_fallback"], false);
    assert_eq!(coder["policy"]["auth"]["prefer"], "subscription");
}

#[test]
fn default_up_keeps_configured_or_known_selection_and_only_appends_interactive() {
    let (_dir, repo) = make_repo(json!({"coder": {"harness":"claude", "role":"reviewer"}}));
    let dir = repo.join(".git/mdkg/run");
    let retired = mdkg_run::state::resolve(
        "retired",
        None,
        mdkg_run::state::SpecFlags {
            harness: Some(mdkg_run::Harness::Claude),
            role: Some(mdkg_run::Role::Reviewer),
            ..Default::default()
        },
        &graph(&repo),
    )
    .unwrap();
    mdkg_run::state::save(&dir, &retired).unwrap();
    // Even a retained interactive registration must not re-add a saved worker.
    let evidence = repo.join(".git/mdkg/hooks/receiver");
    for actor in ["retired", "interactive"] {
        fs::create_dir_all(evidence.join(format!("{actor}.pending"))).unwrap();
        fs::write(evidence.join(format!("{actor}.pending/overflow")), "").unwrap();
    }
    send(&repo, "retired", "must stay dormant", None);
    send(&repo, "coder", "configured work", None);
    let (code, events) = up(&repo, &[]);
    assert_eq!(code, 0, "{events:?}");
    assert_eq!(turns(&events, "coder").len(), 1, "{events:?}");
    assert!(
        events.iter().all(|e| e["data"]["agent"] != "retired"),
        "{events:?}"
    );
    assert!(
        events.iter().any(|e| e["data"]["agent"] == "interactive"),
        "{events:?}"
    );
    assert_eq!(
        mdkg_run::state::load(&dir, "retired")
            .unwrap()
            .unwrap()
            .turns,
        0
    );
    // With no configured names, saved workers remain the default, as before.
    let path = graph(&repo).join(".mdkg/config.json");
    let mut cfg: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    cfg["agents"] = json!({"defaults": {}});
    fs::write(path, cfg.to_string()).unwrap();
    commit_graph_config(&graph(&repo));
    let (code, events) = up(&repo, &[]);
    assert_eq!(code, 0, "{events:?}");
    assert_eq!(turns(&events, "retired").len(), 1, "{events:?}");
}

#[test]
fn the_loop_guard_pauses_a_thread_and_a_person_lets_it_run_again() {
    let (_dir, repo) = make_repo(json!({
        "defaults": {"loop_guard": {"turns": 2, "window_secs": 3600}},
        "rev": {"harness": "claude", "role": "reviewer"},
    }));
    let first = send(&repo, "rev", "one", None);
    assert_eq!(turns(&up(&repo, &[]).1, "rev").len(), 1);
    send(&repo, "rev", "two", Some(&first));
    assert_eq!(turns(&up(&repo, &[]).1, "rev").len(), 1);
    // A third turn in the same thread would pass the limit: paused, its
    // mail left unacked.
    let third = send(&repo, "rev", "three", Some(&first));
    let (code, events) = up(&repo, &[]);
    assert_eq!(code, 0);
    assert!(turns(&events, "rev").is_empty(), "{events:?}");
    let paused = events
        .iter()
        .find(|e| e["data"]["event"] == "paused")
        .expect("a paused event");
    assert_eq!(paused["data"]["paused"].as_array().map(Vec::len), Some(1));
    assert!(
        paused["data"]["cooldown_secs"]
            .as_u64()
            .is_some_and(|s| s > 3000)
    );
    let inbox = ok(&graph(&repo), "rev", &["msg", "inbox"]);
    assert!(inbox.to_string().contains(&third), "still unacked");
    // Another thread still runs.
    send(&repo, "rev", "a new topic", None);
    let events = up(&repo, &[]).1;
    assert_eq!(turns(&events, "rev").len(), 1, "{events:?}");
    let status = ok(&graph(&repo), "launcher", &["agents", "status"]);
    let thread = status["data"]["agents"][0]["paused"][0]
        .as_str()
        .expect("a paused thread")
        .to_string();
    // A person lets it run again.
    let resumed = ok(
        &graph(&repo),
        "launcher",
        &["agents", "resume", "rev", "--thread", &thread],
    );
    assert_eq!(resumed["data"]["resumed"], json!([thread]));
    let events = up(&repo, &[]).1;
    assert_eq!(turns(&events, "rev").len(), 1, "{events:?}");
    let t = &turns(&events, "rev")[0]["data"]["turn"];
    assert_eq!(t["shown"], json!([third]));
}

#[test]
fn a_persons_run_on_a_paused_thread_clears_it() {
    let (_dir, repo) = make_repo(json!({
        "defaults": {"loop_guard": {"turns": 1}},
        "rev": {"harness": "claude", "role": "reviewer"},
    }));
    let first = send(&repo, "rev", "one", None);
    assert_eq!(turns(&up(&repo, &[]).1, "rev").len(), 1);
    send(&repo, "rev", "two", Some(&first));
    assert!(turns(&up(&repo, &[]).1, "rev").is_empty(), "paused");
    ok(&graph(&repo), "launcher", &["turn", "rev"]);
    let status = ok(&graph(&repo), "launcher", &["agents", "status"]);
    assert_eq!(
        status["data"]["agents"][0]["paused"],
        Value::Null,
        "{status}"
    );
}

#[test]
fn a_manual_agent_isnt_woken_and_a_later_kind_refuses_the_run() {
    let (_dir, repo) = make_repo(json!({
        "rev": {"harness": "claude", "role": "reviewer", "policy": {"trigger": "manual"}},
    }));
    send(&repo, "rev", "one", None);
    let (code, events) = up(&repo, &[]);
    assert_eq!(code, 0);
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["data"]["event"], "manual");
    let out = isolated(
        Command::new(env!("CARGO_BIN_EXE_mdkg"))
            .arg("--root")
            .arg(graph(&repo))
            .args(["agents", "up", "--once"]),
    )
    .output()
    .expect("manual guidance");
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("agents up rev --once"), "{text}");
    assert!(!text.contains("mdkg run"), "{text}");

    let (_dir2, repo2) = make_repo(json!({
        "rev": {"harness": "claude", "role": "reviewer",
                "policy": {"trigger": {"kind": "schedule", "cron": "0 * * * *"}}},
    }));
    let (code, events) = up(&repo2, &[]);
    assert_eq!(code, 1, "{events:?}");
    assert_eq!(events[0]["diagnostics"][0]["code"], "run.unsupported");
    // Status reads it all the same, and says why it can't run.
    let status = ok(&graph(&repo2), "launcher", &["agents", "status"]);
    let a = &status["data"]["agents"][0];
    assert_eq!(a["policy"]["trigger"]["kind"], "later");
    assert!(
        a["problem"]
            .as_str()
            .is_some_and(|p| p.contains("trigger schedule")),
        "{a}"
    );
}

#[test]
fn bad_agent_config_and_no_agents_are_refused() {
    let (_dir, repo) =
        make_repo(json!({"rev": {"harness": "claude", "role": "reviewer", "colour": "x"}}));
    let (code, events) = up(&repo, &[]);
    assert_eq!(code, 1);
    assert!(events[0].to_string().contains("colour"), "{events:?}");
    let (_dir2, repo2) = make_repo(json!({}));
    let (code, events) = up(&repo2, &[]);
    assert_eq!(code, 1);
    assert!(
        events[0].to_string().contains("no agents to run"),
        "{events:?}"
    );
}

#[test]
fn a_missing_definition_waits_with_guidance_in_text_and_json() {
    let (_dir, repo) = make_repo(json!({}));
    let root = graph(&repo);
    let config = fs::read(root.join(".mdkg/config.json")).unwrap();
    send(
        &repo,
        "alice",
        "must wait for interactive registration",
        None,
    );

    let (code, events) = up(
        &repo,
        &[
            "alice",
            "--limit",
            "1",
            "--timeout",
            "1",
            "--max-turns",
            "1",
            "--wait",
            "1",
        ],
    );
    assert_eq!(code, 0, "{events:?}");
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["ok"], true);
    assert_eq!(events[0]["data"]["wake"]["status"], "waiting");
    let advice = &events[0]["diagnostics"][0];
    assert_eq!(advice["severity"], "info");
    assert!(
        advice["message"]
            .as_str()
            .unwrap()
            .contains("no definition named alice"),
        "{advice}"
    );
    let hint = advice["hint"].as_str().unwrap();
    assert!(hint.contains("mdkg agents add alice"), "{hint}");
    assert!(hint.contains("creates a headless agent"), "{hint}");

    let out = isolated(
        Command::new(env!("CARGO_BIN_EXE_mdkg"))
            .current_dir(&root)
            .arg("--root")
            .arg(&root)
            .args(["--as", "launcher", "agents", "up", "alice", "--once"]),
    )
    .output()
    .unwrap();
    assert!(out.status.success(), "{out:?}");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("receiver Waiting"), "{text}");
    assert!(text.contains("no definition named alice"), "{text}");
    assert!(text.contains("mdkg agents add alice"), "{text}");
    assert!(text.contains("creates a headless agent"), "{text}");

    assert_eq!(fs::read(root.join(".mdkg/config.json")).unwrap(), config);
    assert!(!root.join(".mdkg/agents/alice.json").exists());
    assert!(!repo.join(".git/mdkg/run/alice.json").exists());
    assert!(!repo.join(".worktrees/alice").exists());
    assert!(mdkg_run::receiver::route::actors(&repo.join(".git")).is_empty());
}

#[test]
fn a_missing_definition_still_refuses_headless_only_flags() {
    let (_dir, repo) = make_repo(json!({}));
    for flags in [
        vec!["--fresh"],
        vec!["--dry-run"],
        vec!["--prompt", "hello"],
        vec!["--reply-kind", "findings"],
        vec!["--keep-logs"],
        vec!["--allow-zen"],
        vec!["--binary", env!("CARGO_BIN_EXE_mdkg")],
    ] {
        let flag = flags[0];
        let mut args = vec!["alice"];
        args.extend(flags);
        let (code, events) = up(&repo, &args);
        assert_ne!(code, 0, "{args:?}: {events:?}");
        let message = events[0]["diagnostics"][0]["message"].as_str().unwrap();
        assert!(message.contains(flag), "{args:?}: {events:?}");
        assert!(
            message.contains("headless definition"),
            "{args:?}: {events:?}"
        );
    }
    assert!(!repo.join(".git/mdkg/run/alice.json").exists());
    assert!(!graph(&repo).join(".mdkg/agents/alice.json").exists());
    assert!(mdkg_run::receiver::route::actors(&repo.join(".git")).is_empty());
}

// Migration/install already created local state. A rejected launch must leave
// those bytes alone and create no agent state or lease activity.
fn state_snapshot(dir: &Path) -> std::collections::BTreeMap<PathBuf, Option<Vec<u8>>> {
    let mut files = std::collections::BTreeMap::new();
    for entry in fs::read_dir(dir).expect("state directory") {
        let entry = entry.expect("state entry");
        if entry.file_type().expect("state type").is_dir() {
            files.insert(entry.path(), None);
            files.extend(state_snapshot(&entry.path()));
        } else {
            files.insert(
                entry.path(),
                Some(fs::read(entry.path()).expect("state bytes")),
            );
        }
    }
    files
}

#[test]
fn mixed_names_are_preflighted_before_any_agent_preparation() {
    let (_dir, repo) = make_repo(json!({
        "reviewer": {"harness":"claude", "role":"reviewer"},
    }));
    let config = graph(&repo).join(".mdkg/config.json");
    let before = fs::read(&config).unwrap();
    let metadata = repo.join(".git/mdkg");
    let before_state = state_snapshot(&metadata);
    for flags in [
        vec!["--keep-logs"],
        vec!["--fresh"],
        vec!["--dry-run"],
        vec!["--prompt", "hello"],
        vec!["--reply-kind", "findings"],
        vec!["--allow-zen"],
        vec!["--binary", env!("CARGO_BIN_EXE_mdkg")],
    ] {
        for names in [["reviewer", "typo"], ["typo", "reviewer"]] {
            let mut args = names.to_vec();
            args.extend_from_slice(&flags);
            let (code, events) = up(&repo, &args);
            assert_ne!(code, 0, "{args:?}: {events:?}");
            let message = events[0]["diagnostics"][0]["message"].as_str().unwrap();
            assert!(
                message.contains("typo") && message.contains(flags[0]),
                "{events:?}"
            );
            assert_eq!(
                state_snapshot(&metadata),
                before_state,
                "{args:?} wrote state or lease activity"
            );
            assert_eq!(fs::read(&config).unwrap(), before);
            assert!(!graph(&repo).join(".mdkg/agents/typo.json").exists());
            assert!(!repo.join(".worktrees/typo").exists());
        }
    }
}

#[test]
fn a_later_unreadable_definition_refuses_before_preparing_an_earlier_agent() {
    let (_dir, repo) = make_repo(json!({
        "reviewer": {"harness":"claude", "role":"reviewer"},
        "broken": 1,
    }));
    let state = repo.join(".git/mdkg");
    let before = state_snapshot(&state);
    let (code, events) = up(&repo, &["reviewer", "broken"]);
    assert_ne!(code, 0, "{events:?}");
    assert_eq!(
        state_snapshot(&state),
        before,
        "wrote state or lease activity"
    );
}

#[test]
fn headless_only_flags_remain_available_to_defined_agents() {
    let (_dir, repo) = make_repo(json!({
        "reviewer": {"harness":"claude", "role":"reviewer"},
    }));
    let (code, events) = up(
        &repo,
        &[
            "reviewer",
            "--dry-run",
            "--fresh",
            "--prompt",
            "review this work",
            "--reply-kind",
            "findings",
            "--keep-logs",
            "--allow-zen",
            "--binary",
            env!("CARGO_BIN_EXE_mdkg"),
        ],
    );
    assert_eq!(code, 0, "{events:?}");
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["data"]["turn"]["outcome"], "dry_run");
    assert!(
        events[0]["data"]["turn"]["prompt"]
            .as_str()
            .unwrap()
            .contains("review this work"),
        "{events:?}"
    );
}

#[test]
fn a_turn_that_fails_before_the_agent_runs_isnt_retried_until_new_mail() {
    let (_dir, repo) = make_repo(json!({"rev": {"harness": "claude", "role": "reviewer"}}));
    // Mail from the actor the launcher runs as refuses the turn before the
    // agent's cursor moves.
    ok(
        &repo,
        "launcher",
        &[
            "msg",
            "send",
            "--to",
            "rev",
            "--kind",
            "note",
            "--text",
            "from myself",
        ],
    );
    let start = std::time::Instant::now();
    let (code, events) = up(&repo, &[]);
    assert_eq!(code, 0, "{events:?}");
    let t = turns(&events, "rev");
    assert_eq!(t.len(), 1, "one failed turn, not a loop: {events:?}");
    assert_eq!(t[0]["ok"], false);
    assert!(start.elapsed() < std::time::Duration::from_secs(30));
}

#[test]
fn early_failures_of_up_are_one_json_line() {
    let (_dir, repo) = make_repo(json!({"rev": {"harness": "claude", "role": "reviewer"}}));
    // Refused by argument parsing, before any command runs.
    let (code, events) = up(&repo, &["--wait", "0"]);
    assert_eq!(code, 1);
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["schema"], "mdkg.agents.up.v1");
    // Refused before dispatch: no graph here.
    let empty = repo.join("no-graph");
    fs::create_dir_all(&empty).expect("mkdir");
    let (code, out) = mdkg_out(&empty, "launcher", &["agents", "up", "--once"]);
    assert_eq!(code, 1);
    assert_eq!(out.lines().count(), 1, "{out}");
}
