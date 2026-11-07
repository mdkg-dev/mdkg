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
    let (base, case) = (
        conformance.join("base"),
        conformance.join("cases/g01-valid-basic"),
    );
    fs::create_dir_all(&repo).expect("repo");
    // The first commit and the migration don't depend on where the fixture
    // lives, so they're built once for each agents config and copied.
    let agents_text = agents.to_string();
    let mut config_key = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(&agents_text, &mut config_key);
    common::template::copy(
        &format!(
            "agents-repo-{:016x}",
            std::hash::Hasher::finish(&config_key)
        ),
        &[&base, &case],
        |repo| {
            copy_tree(&base, repo);
            copy_tree(&case, repo);
            fs::remove_file(repo.join("commands")).ok();
            fs::remove_file(repo.join("README.md")).ok();
            fs::write(
                repo.join(".gitignore"),
                "*.args\n*.prompt\n*.environ\nfake-reply\n/.worktrees/\n",
            )
            .expect("ignore");
            let config = repo.join(".mdkg/config.json");
            let mut cfg: Value =
                serde_json::from_slice(&fs::read(&config).expect("config")).expect("json");
            cfg["agents"] = agents;
            fs::write(&config, cfg.to_string()).expect("w");
            sh(repo, &["init", "-q", "-b", "main"]);
            sh(repo, &["add", "."]);
            sh(repo, &["commit", "-q", "-m", "init"]);
            // The conformance input is legacy; preserve its handles while
            // giving the managed checkout a committed 0.7 graph identity.
            let migration = ok(repo, "launcher", &["migrate", "--dry-run"]);
            let plan = migration["data"]["plan"].as_str().expect("migration plan");
            ok(repo, "launcher", &["migrate", "--apply", plan]);
        },
        &repo,
    );
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

/// `name`'s `last_exit` in `agents status --no-probe --json`, which a
/// supervisor leaves when it ends (#20587); null when none.
fn last_exit(root: &Path, name: &str) -> Value {
    let status = ok(root, "launcher", &["agents", "status", "--no-probe"]);
    status["data"]["agents"]
        .as_array()
        .expect("agents")
        .iter()
        .find(|a| a["name"] == name)
        .unwrap_or_else(|| panic!("{name} in {status}"))
        .get("last_exit")
        .cloned()
        .unwrap_or(Value::Null)
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
    // Definitions are read only from committed authority, so each bad one
    // is committed setup.
    let commit = || {
        sh(&root, &["add", ".mdkg/agents/bad.json"]);
        sh(&root, &["commit", "-qm", "fixture definition"]);
    };
    for definition in [
        json!({"schema":"other", "harness":"claude", "role":"worker"}),
        json!({"schema":"mdkg.agent.v1", "harness":"claude", "role":"worker", "typo":true}),
        json!({"schema":"mdkg.agent.v1", "harness":"claude"}),
    ] {
        fs::write(&path, definition.to_string()).unwrap();
        commit();
        let (code, _) = mdkg(&root, "launcher", &["agents", "status"]);
        assert_ne!(code, 0, "{definition}");
    }
    fs::write(&path, " ".repeat((1 << 20) + 1)).unwrap();
    commit();
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
    commit();
    let (code, _) = mdkg(&root, "launcher", &["agents", "status"]);
    assert_ne!(code, 0);
    sh(&root, &["rm", "-q", ".mdkg/agents/bad.json"]);
    sh(&root, &["commit", "-qm", "remove fixture definition"]);
    if dir.exists() {
        fs::remove_dir(&dir).unwrap();
    }
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
    assert_eq!(last_exit(&root, "astra")["reason"], "once");

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
    let text = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
        .current_dir(graph(&repo))
        .args(["--as", "launcher", "agents", "up", "--once"])
        .output()
        .unwrap();
    assert!(text.status.success());
    let text = String::from_utf8_lossy(&text.stdout);
    assert!(text.contains("agents resume rev --thread"), "{text}");
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
    // Its reason is its trigger, never a failure or a cap (#20921).
    assert_eq!(last_exit(&graph(&repo), "rev")["reason"], "manual");
    for extra in [&["rev"][..], &["rev", "--max-turns", "2"]] {
        let mut args = vec!["agents", "up"];
        args.extend_from_slice(extra);
        let (code, out) = mdkg_out(&graph(&repo), "launcher", &args);
        assert_eq!(code, 0, "{extra:?}: {out}");
        let exit = last_exit(&graph(&repo), "rev");
        assert_eq!(exit["reason"], "manual", "{extra:?}: {exit}");
        assert!(exit.get("code").is_none(), "{exit}");
    }
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

/// The session an agent's saved state resumes next.
fn read_state_session(repo: &Path, agent: &str) -> Option<String> {
    let path = repo.join(".git/mdkg/run").join(format!("{agent}.json"));
    let state: Value = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    state["session"].as_str().map(String::from)
}

fn progress_file(repo: &Path, agent: &str) -> PathBuf {
    repo.join(".git/mdkg/run/progress")
        .join(format!("{agent}.json"))
}

fn read_progress(repo: &Path, agent: &str) -> Value {
    serde_json::from_slice(&fs::read(progress_file(repo, agent)).expect("read progress"))
        .expect("decode progress")
}

#[test]
fn durable_manual_completion_is_not_repeated_after_restart_before_finish() {
    let (_dir, repo) = make_repo(json!({"rev": {"harness":"claude", "role":"reviewer"}}));
    let (code, events) = up(&repo, &["rev", "--prompt", "exactly one manual pass"]);
    assert_eq!(code, 0, "{events:?}");
    assert_eq!(turns(&events, "rev").len(), 1);
    let mut record = read_progress(&repo, "rev");
    assert_eq!(record["progress"]["ran"], 1);
    assert_eq!(record["finished"], true);
    let watch = record["watch_id"].clone();
    // The durable completion immediately preceding finish: simulate process
    // death in that gap without inventing an admission or changing accounting.
    record["finished"] = json!(false);
    fs::write(progress_file(&repo, "rev"), record.to_string()).unwrap();
    let (code, events) = up(&repo, &["rev", "--prompt", "exactly one manual pass"]);
    assert_eq!(code, 0, "{events:?}");
    assert!(turns(&events, "rev").is_empty(), "{events:?}");
    let after = read_progress(&repo, "rev");
    assert_eq!(after["watch_id"], watch);
    assert_eq!(after["progress"]["ran"], 1);
    let saved: Value =
        serde_json::from_slice(&fs::read(repo.join(".git/mdkg/run/rev.json")).unwrap()).unwrap();
    assert_eq!(saved["turns"], 1);
}

#[test]
fn durable_mixed_group_finalization_refuses_before_writes_and_recovers_only_unfinished_work() {
    for unrun in [false, true] {
        let (_dir, repo) = make_repo(json!({
            "a": {"harness":"claude", "role":"reviewer"},
            "b": {"harness":"claude", "role":"reviewer"}
        }));
        let root = graph(&repo);
        let prompt = "one pass with 'quoted' instructions";
        if unrun {
            // Prepare b's runtime state before taking its turn lease, so the
            // refusal occurs at its turn, after a has already completed.
            let dir = mdkg_run::state::run_dir(&root).unwrap();
            let b = mdkg_run::state::resolve(
                "b",
                None,
                mdkg_run::state::SpecFlags {
                    harness: Some(mdkg_run::Harness::Claude),
                    role: Some(mdkg_run::Role::Reviewer),
                    ..Default::default()
                },
                &root,
            )
            .unwrap();
            mdkg_run::state::save(&dir, &b).unwrap();
            ok(&root, "blocker", &["claim", "run:b"]);
        }
        let (code, events) = up(&repo, &["a", "b", "--prompt", prompt]);
        assert_eq!(code == 0, !unrun, "{events:?}");
        assert_eq!(turns(&events, "a").len(), 1, "{events:?}");
        if unrun {
            ok(&root, "blocker", &["release", "run:b"]);
        }
        let mut b = read_progress(&repo, "b");
        assert_eq!(b["progress"]["ran"], if unrun { 0 } else { 1 });
        // This is the exact per-file state left when the process dies after
        // finishing a and before b's finish publishes. Library fault tests
        // exercise the actual process death and failed-write boundaries.
        b["finished"] = json!(false);
        fs::write(progress_file(&repo, "b"), b.to_string()).unwrap();
        let before: Vec<_> = ["a", "b"]
            .map(|name| fs::read(progress_file(&repo, name)).unwrap())
            .into();
        let (code, events) = up(&repo, &["a", "b", "--prompt", prompt]);
        assert_ne!(code, 0, "{events:?}");
        assert_eq!(events.len(), 1, "{events:?}");
        let diagnostic = &events[0]["diagnostics"][0];
        assert_eq!(diagnostic["code"], "run.progress_invalid");
        let message = diagnostic["message"].as_str().unwrap();
        assert!(
            message.contains("a: finished") && message.contains("b: unfinished"),
            "{message}"
        );
        let hint = diagnostic["hint"].as_str().unwrap();
        assert!(
            hint.contains("agents status") && hint.contains("agents resume b"),
            "{hint}"
        );
        assert!(
            !hint.contains("agents resume a") && !hint.contains("agents up a"),
            "{hint}"
        );
        for (name, bytes) in ["a", "b"].iter().zip(&before) {
            assert_eq!(&fs::read(progress_file(&repo, name)).unwrap(), bytes);
        }
        ok(&root, "launcher", &["agents", "resume", "b"]);
        if unrun {
            assert!(hint.contains("agents up b --once"), "{hint}");
            assert!(
                hint.contains("--prompt 'one pass with '\"'\"'quoted'\"'\"' instructions'"),
                "{hint}"
            );
            let (code, events) = up(&repo, &["b", "--prompt", prompt]);
            assert_eq!(code, 0, "{events:?}");
            assert_eq!(turns(&events, "b").len(), 1);
        } else {
            assert!(!hint.contains("agents up b"), "{hint}");
            assert!(hint.contains("already charged manual pass"), "{hint}");
        }
        for name in ["a", "b"] {
            let state: Value = serde_json::from_slice(
                &fs::read(repo.join(format!(".git/mdkg/run/{name}.json"))).unwrap(),
            )
            .unwrap();
            assert_eq!(state["turns"], 1, "{name}: {state}");
            assert_eq!(read_progress(&repo, name)["finished"], true);
        }
    }
}

#[test]
fn durable_interrupted_cli_turn_requires_resume_and_carries_recovery_context() {
    use std::os::unix::{fs::PermissionsExt, process::CommandExt};
    use std::time::{Duration, Instant};
    struct Running(std::process::Child);
    impl Drop for Running {
        fn drop(&mut self) {
            // Without `--`, procps-ng 4.0.4's kill (Ubuntu 24.04) misreads
            // the group id and signals every process the user owns, the CI
            // runner included.
            let _ = Command::new("/bin/kill")
                .args(["-KILL", "--", &format!("-{}", self.0.id())])
                .status();
            self.0.kill().ok();
            self.0.wait().ok();
        }
    }
    let (scratch, repo) = make_repo(json!({"rev": {"harness":"claude", "role":"reviewer"}}));
    let root = graph(&repo);
    let message = send(&repo, "rev", "inspect once", None);
    let script = scratch.0.join("blocked-claude");
    let started = scratch.0.join("harness-started");
    fs::write(&script, format!("#!/bin/sh\nif [ \"$1\" = auth ]; then echo '{{\"loggedIn\":true,\"authMethod\":\"claude.ai\"}}'; exit 0; fi\ncat >/dev/null\ntouch '{}'\nsleep 30\n", started.display())).unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let stderr = scratch.0.join("watch.err");
    let child = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
        .current_dir(&root)
        .arg("--root")
        .arg(&root)
        .args([
            "--as",
            "launcher",
            "agents",
            "up",
            "rev",
            "--max-turns",
            "2",
            "--wait",
            "1",
            "--json",
        ])
        .env("MDKG_RUN_CLAUDE", &script)
        .stdout(fs::File::create(scratch.0.join("watch.out")).unwrap())
        .stderr(fs::File::create(&stderr).unwrap())
        .process_group(0)
        .spawn()
        .unwrap();
    let mut child = Running(child);
    let deadline = Instant::now() + Duration::from_secs(40);
    while !started.exists() {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "{}",
            fs::read_to_string(&stderr).unwrap()
        );
        assert!(
            Instant::now() < deadline,
            "harness did not start: {}",
            fs::read_to_string(&stderr).unwrap()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let before = read_progress(&repo, "rev");
    assert_eq!(before["current"]["mail_ids"], json!([message]));
    assert!(before["current"]["start_head"].is_string());
    let began = Instant::now();
    let status = ok(&root, "launcher", &["agents", "status"]);
    let active = &status["data"]["progress"][0];
    assert_eq!(active["active"], true);
    assert_eq!(active["record"]["current"], before["current"]);
    assert!(active["problem"].is_null(), "{active}");
    assert!(active["repair"].is_null(), "{active}");
    let doctor = ok(&root, "launcher", &["doctor"]);
    let check = doctor["data"]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "supervisor_progress")
        .unwrap();
    assert_eq!(check["status"], "ok", "{check}");
    assert!(began.elapsed() < Duration::from_secs(15));
    drop(child); // actual process death with an admitted, uncompleted turn
    let bytes = fs::read(progress_file(&repo, "rev")).unwrap();
    let (code, events) = up(&repo, &["rev"]);
    assert_ne!(code, 0, "{events:?}");
    assert_eq!(events[0]["diagnostics"][0]["code"], "run.turn_interrupted");
    let status = ok(&root, "launcher", &["agents", "status"]);
    let p = &status["data"]["progress"][0];
    assert_eq!(p["active"], false);
    assert_eq!(p["record"]["current"], before["current"]);
    assert!(p["repair"].as_str().unwrap().contains("agents resume rev"));
    let doctor = ok(&root, "launcher", &["doctor"]);
    let check = doctor["data"]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "supervisor_progress")
        .unwrap();
    assert_eq!(check["status"], "warning");
    assert!(
        check["summary"]
            .as_str()
            .unwrap()
            .contains("agents resume rev")
    );
    let (code, _) = mdkg(
        &root,
        "launcher",
        &["agents", "resume", "rev", "--store", "wrong"],
    );
    assert_ne!(code, 0);
    assert_eq!(bytes, fs::read(progress_file(&repo, "rev")).unwrap());
    // A sandbox may deny process start-time lookup, so automatic stale-lease
    // detection need not prove this holder dead. The child was killed and
    // reaped above: release only its exact fixture holder, as an operator.
    let lease: Value =
        serde_json::from_slice(&fs::read(repo.join(".git/mdkg/run/rev.lease")).unwrap()).unwrap();
    ok(
        &root,
        lease["holder"].as_str().unwrap(),
        &["release", "run:rev"],
    );
    let resumed = ok(&root, "launcher", &["agents", "resume", "rev"]);
    assert_eq!(
        resumed["data"]["progress_before"]["current"],
        before["current"]
    );
    let after = read_progress(&repo, "rev");
    assert!(after["current"].is_null());
    assert_eq!(after["progress"]["ran"], 1);
    assert_eq!(after["recovery"][0], before["current"]);
    let (code, events) = up(&repo, &["rev", "--prompt", "check prior state first"]);
    assert_eq!(code, 0, "{events:?}");
    assert_eq!(turns(&events, "rev").len(), 1);
    let prompt = fs::read_to_string(root.join("claude.prompt")).unwrap();
    assert!(prompt.contains("Previous work was interrupted"));
    assert!(prompt.contains(&message));
    assert!(prompt.contains(before["current"]["start_head"].as_str().unwrap()));
    assert!(
        read_progress(&repo, "rev")["recovery"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn durable_corrupt_or_foreign_progress_refuses_before_agent_setup() {
    for legacy in [true, false] {
        let (_scratch, repo) = make_repo(json!({
            "a": {"harness":"claude", "role":"reviewer"},
            "z": {"harness":"claude", "role":"reviewer"}
        }));
        assert_eq!(up(&repo, &["z"]).0, 0);
        let root = graph(&repo);
        let file = progress_file(&repo, "z");
        let valid = fs::read(&file).unwrap();
        fs::write(&file, b"{").unwrap();
        let (code, events) = up(&repo, &["a", "z"]);
        assert_ne!(code, 0, "{events:?}");
        assert!(!repo.join(".git/mdkg/run/a.json").exists());
        assert!(!progress_file(&repo, "a").exists());
        let mut foreign: Value = serde_json::from_slice(&valid).unwrap();
        assert_eq!(foreign["schema"], "mdkg.supervisor-progress.v3");
        let old_store = if legacy {
            foreign["schema"] = json!("mdkg.supervisor-progress.v2");
            foreign["store"] = json!([0, 1]);
            "0000000000000000:0000000000000001".to_owned()
        } else {
            // A different nonce remains foreign even with this folder's inode.
            foreign["store"]["id"] = json!("00000000-0000-4000-8000-000000000001");
            format!(
                "{}:{:016x}",
                foreign["store"]["id"].as_str().unwrap(),
                foreign["store"]["inode"].as_u64().unwrap()
            )
        };
        fs::write(&file, foreign.to_string()).unwrap();
        let (code, _) = up(&repo, &["a", "z"]);
        assert_ne!(code, 0);
        assert!(!repo.join(".git/mdkg/run/a.json").exists());
        // A copied foreign record may lack its sidecar. Read-only diagnostics
        // must still show both store identities, without creating a lock.
        let lock = file.with_extension("lock");
        fs::remove_file(&lock).unwrap();
        let status = ok(&root, "launcher", &["agents", "status"]);
        let p = &status["data"]["progress"][0];
        let current = p["current_store"].as_str().unwrap();
        assert_eq!(p["record"]["schema"], foreign["schema"]);
        assert!(p["problem"].as_str().unwrap().contains(current));
        assert!(p["problem"].as_str().unwrap().contains(&old_store));
        let doctor = ok(&root, "launcher", &["doctor"]);
        let check = doctor["data"]["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == "supervisor_progress")
            .unwrap();
        let summary = check["summary"].as_str().unwrap();
        assert!(
            summary.contains(current) && summary.contains(&old_store),
            "{summary}"
        );
        assert!(!lock.exists());
        let refused = mdkg(&root, "launcher", &["agents", "resume", "z"]);
        assert_ne!(refused.0, 0);
        let before = fs::read(&file).unwrap();
        let resumed = ok(
            &root,
            "launcher",
            &[
                "agents",
                "resume",
                "z",
                "--store",
                current,
                "--archive-context",
            ],
        );
        let archive = resumed["data"]["recovery_archive"].as_str().unwrap();
        assert_eq!(fs::read(archive).unwrap(), before);
        assert_eq!(
            read_progress(&repo, "z")["schema"],
            "mdkg.supervisor-progress.v3"
        );
        let (code, events) = up(&repo, &["z", "--prompt", "inspect recovery archive"]);
        assert_eq!(code, 0, "{events:?}");
        assert!(
            fs::read_to_string(root.join("claude.prompt"))
                .unwrap()
                .contains(archive)
        );
        assert!(Path::new(archive).exists());
    }
}

#[test]
fn a_running_watch_reloads_an_edited_definition_before_its_next_turn() {
    use std::os::unix::process::CommandExt;
    use std::time::{Duration, Instant};
    struct Running(std::process::Child);
    impl Drop for Running {
        fn drop(&mut self) {
            let _ = Command::new("/bin/kill")
                .args(["-KILL", "--", &format!("-{}", self.0.id())])
                .status();
            self.0.kill().ok();
            self.0.wait().ok();
        }
    }
    // The reload is the supervisor's, not the harness's: each one gets the
    // same drain and re-exec.
    for (harness, prompt_file) in [
        ("claude", "claude.prompt"),
        ("codex", "codex.prompt"),
        ("opencode", "opencode.args"),
    ] {
        let (scratch, repo) = make_repo(json!({}));
        let root = graph(&repo);
        let home = scratch.0.join("home");
        fs::create_dir_all(home.join(".codex")).unwrap();
        let add = [
            "agents",
            "add",
            "rev",
            "--harness",
            harness,
            "--role",
            "reviewer",
            "--instructions",
            "FIRST-DEFINITION",
        ];
        ok(&root, "launcher", &add);
        let first = git_text(&root, &["rev-parse", "HEAD"]);
        send(&repo, "rev", "first request", None);
        let stderr = scratch.0.join("watch.err");
        let child = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
            .current_dir(&root)
            .arg("--root")
            .arg(&root)
            .args(["--as", "launcher", "agents", "up", "rev"])
            .args(["--max-turns", "2", "--wait", "1", "--json"])
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .env("CODEX_HOME", home.join(".codex"))
            .env("MDKG_RUN_CODEX", fixture("fake-codex.sh"))
            .env("MDKG_RUN_OPENCODE", fixture("fake-opencode.sh"))
            .stdout(fs::File::create(scratch.0.join("watch.out")).unwrap())
            .stderr(fs::File::create(&stderr).unwrap())
            .process_group(0)
            .spawn()
            .unwrap();
        let mut child = Running(child);
        let wait_for = |what: &str, child: &mut Running, done: &dyn Fn() -> bool| {
            let deadline = Instant::now() + Duration::from_secs(60);
            while !done() {
                let err = fs::read_to_string(&stderr).unwrap_or_default();
                assert!(
                    child.0.try_wait().unwrap().is_none(),
                    "{harness}: {what}: {err}"
                );
                assert!(Instant::now() < deadline, "{harness}: {what}: {err}");
                std::thread::sleep(Duration::from_millis(50));
            }
        };
        let prompt = || fs::read_to_string(root.join(prompt_file)).unwrap_or_default();
        wait_for("first turn", &mut child, &|| {
            prompt().contains("FIRST-DEFINITION")
        });
        assert!(
            prompt().contains(&format!("last committed in {first}")),
            "{harness}: {}",
            prompt()
        );
        ok(
            &root,
            "launcher",
            &[
                "agents",
                "edit",
                "rev",
                "--instructions",
                "SECOND-DEFINITION",
            ],
        );
        let second = git_text(&root, &["rev-parse", "HEAD"]);
        wait_for("reload", &mut child, &|| {
            fs::read_to_string(&stderr)
                .unwrap_or_default()
                .contains("turns finished, restarting")
        });
        send(&repo, "rev", "second request", None);
        let deadline = Instant::now() + Duration::from_secs(60);
        let status = loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "{harness}: no second turn");
            std::thread::sleep(Duration::from_millis(50));
        };
        let err = fs::read_to_string(&stderr).unwrap();
        assert!(status.success(), "{harness}: {err}");
        let text = prompt();
        assert!(text.contains("SECOND-DEFINITION"), "{harness}: {text}");
        assert!(!text.contains("FIRST-DEFINITION"), "{harness}: {text}");
        assert!(
            text.contains(&format!("last committed in {second}")),
            "{harness}: {text}"
        );
        let environ = fs::read_to_string(root.join(format!("{harness}.environ"))).unwrap();
        assert!(
            !environ.contains("MDKG_SUPERVISOR_EXEC"),
            "{harness}: token leaked"
        );
        // One watch across the exec: one count and one record.
        let progress = read_progress(&repo, "rev");
        assert_eq!(progress["progress"]["ran"], 2, "{harness}: {progress}");
        assert_eq!(progress["finished"], true);
        let out = fs::read_to_string(scratch.0.join("watch.out")).unwrap();
        let events: Vec<Value> = out
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(turns(&events, "rev").len(), 2, "{harness}: {out}");
        assert_eq!(
            err.matches("turns finished, restarting").count(),
            1,
            "{harness}: {err}"
        );
        // The cap, recorded by the re-execed image under the one id.
        let exit = last_exit(&root, "rev");
        assert_eq!(exit["reason"], "cap", "{harness}: {exit}");
        assert!(exit.get("code").is_none(), "{harness}: {exit}");
        assert!(exit["at"].is_string(), "{harness}: {exit}");
    }
}

/// A long-running `agents up` for the drain tests: its group is killed if
/// the test fails first.
struct Watching {
    child: std::process::Child,
    stderr: PathBuf,
}

impl Drop for Watching {
    fn drop(&mut self) {
        let _ = Command::new("/bin/kill")
            .args(["-KILL", "--", &format!("-{}", self.child.id())])
            .status();
        self.child.kill().ok();
        self.child.wait().ok();
    }
}

impl Watching {
    /// `program` runs `agents up rev` from `root`, as the operator would,
    /// with `claude` as the harness.
    fn start(scratch: &Path, root: &Path, program: &Path, claude: &Path, extra: &[&str]) -> Self {
        use std::os::unix::process::CommandExt;
        let home = scratch.join("home");
        fs::create_dir_all(home.join(".codex")).expect("home");
        let stderr = scratch.join("watch.err");
        let child = isolated(&mut Command::new(program))
            .current_dir(root)
            .arg("--root")
            .arg(root)
            .args(["--as", "launcher", "agents", "up", "rev", "--wait", "1"])
            .args(extra)
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .env("CODEX_HOME", home.join(".codex"))
            .env("MDKG_RUN_CLAUDE", claude)
            .stdout(fs::File::create(scratch.join("watch.out")).expect("watch stdout"))
            .stderr(fs::File::create(&stderr).expect("watch stderr"))
            .process_group(0)
            .spawn()
            .expect("start watch");
        Self { child, stderr }
    }

    /// Its stderr, then its stdout.
    fn err(&self) -> String {
        let out = self.stderr.with_file_name("watch.out");
        format!(
            "{}\n{}",
            fs::read_to_string(&self.stderr).unwrap_or_default(),
            fs::read_to_string(out).unwrap_or_default()
        )
    }

    fn wait_for(&mut self, what: &str, done: impl Fn(&Self) -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while !done(self) {
            assert!(
                self.child.try_wait().expect("poll watch").is_none(),
                "{what}: {}",
                self.err()
            );
            assert!(
                std::time::Instant::now() < deadline,
                "{what}: {}",
                self.err()
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    fn exit(&mut self) -> std::process::ExitStatus {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            if let Some(status) = self.child.try_wait().expect("poll watch") {
                return status;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "no exit: {}",
                self.err()
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    /// Signals the supervisor's whole process group, its turn's included.
    fn signal_group(&self, signal: &str) {
        let sent = Command::new("/bin/kill")
            .args([signal, "--", &format!("-{}", self.child.id())])
            .status()
            .expect("signal watch group");
        assert!(sent.success());
    }

    /// Signals the supervisor's own process, as a service manager stopping
    /// it by pid does. Not its group: the turn's group guard shares that
    /// group, and a group SIGTERM stops running turns at once.
    fn signal(&self, signal: &str) {
        let sent = Command::new("/bin/kill")
            .args([signal, &self.child.id().to_string()])
            .status()
            .expect("signal watch");
        assert!(sent.success());
    }
}

/// A Claude stand-in whose turn touches `<scratch>/started`, then waits
/// for `<scratch>/release`. It gives up, failing the turn, once the
/// process that started it is gone, the scratch folder is removed, or
/// after ten minutes: a test that panics or is killed never leaves it
/// behind, though its turn's process group outlives the supervisor's.
fn held_claude(scratch: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let script = scratch.join("held-claude");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\n[ \"$1\" = auth ] && exec '{fake}' \"$@\"\ntouch '{dir}/started'\nn=0\nwhile [ ! -e '{dir}/release' ]; do\n  [ -d '{dir}' ] && kill -0 \"$PPID\" 2>/dev/null && [ \"$n\" -lt 12000 ] || exit 1\n  n=$((n + 1))\n  sleep 0.05\ndone\nexec '{fake}' \"$@\"\n",
            fake = fixture("fake-claude.sh").display(),
            dir = scratch.display()
        ),
    )
    .expect("held claude");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("held claude mode");
    script
}

#[test]
fn sigterm_lets_the_current_turn_finish_then_leaves_the_watch_finished() {
    let (scratch, repo) = make_repo(json!({}));
    let root = graph(&repo);
    ok(
        &root,
        "launcher",
        &[
            "agents",
            "add",
            "rev",
            "--harness",
            "claude",
            "--role",
            "reviewer",
        ],
    );
    send(&repo, "rev", "a held request", None);
    let claude = held_claude(&scratch.0);
    let mdkg = Path::new(env!("CARGO_BIN_EXE_mdkg"));
    let mut watch = Watching::start(&scratch.0, &root, mdkg, &claude, &[]);
    watch.wait_for("turn start", |_| scratch.0.join("started").exists());
    watch.signal("-TERM");
    watch.wait_for("drain notice", |w| {
        w.err().contains("SIGTERM; finishing turns first")
    });
    // Still running: the turn decides when it ends.
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert!(watch.child.try_wait().unwrap().is_none(), "{}", watch.err());
    fs::write(scratch.0.join("release"), "").unwrap();
    let status = watch.exit();
    assert!(status.success(), "{}", watch.err());
    assert!(watch.err().contains("stopped by SIGTERM; turns finished"));
    let progress = read_progress(&repo, "rev");
    assert_eq!(progress["progress"]["ran"], 1, "{progress}");
    assert_eq!(progress["finished"], true, "{progress}");
    assert!(progress["current"].is_null());
    // A plain restart needs no resume.
    send(&repo, "rev", "after the stop", None);
    let (code, events) = up(&repo, &["rev"]);
    assert_eq!(code, 0, "{events:?}");
}

#[test]
fn sigterm_during_a_reload_drain_stops_instead_of_re_execing() {
    let (scratch, repo) = make_repo(json!({}));
    let root = graph(&repo);
    ok(
        &root,
        "launcher",
        &[
            "agents",
            "add",
            "rev",
            "--harness",
            "claude",
            "--role",
            "reviewer",
        ],
    );
    send(&repo, "rev", "a held request", None);
    let claude = held_claude(&scratch.0);
    let mdkg = Path::new(env!("CARGO_BIN_EXE_mdkg"));
    let mut watch = Watching::start(&scratch.0, &root, mdkg, &claude, &[]);
    watch.wait_for("turn start", |_| scratch.0.join("started").exists());
    // A committed edit starts a reload's drain while the turn is held.
    ok(
        &root,
        "launcher",
        &["agents", "edit", "rev", "--instructions", "EDITED"],
    );
    watch.wait_for("reload drain", |w| {
        w.err().contains("changed; finishing turns first")
    });
    // The watcher has stopped polling; this TERM must still win.
    watch.signal("-TERM");
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert!(
        watch.child.try_wait().expect("poll watch").is_none(),
        "{}",
        watch.err()
    );
    fs::write(scratch.0.join("release"), "").expect("release");
    let status = watch.exit();
    assert!(status.success(), "{}", watch.err());
    assert!(
        watch.err().contains("stopped by SIGTERM; turns finished"),
        "{}",
        watch.err()
    );
    assert!(!watch.err().contains("restarting"), "{}", watch.err());
    assert_eq!(last_exit(&root, "rev")["reason"], "signal");
    let progress = read_progress(&repo, "rev");
    assert_eq!(progress["progress"]["ran"], 1, "{progress}");
    assert_eq!(progress["finished"], true, "{progress}");
}

/// `agents stop … --drain` as `launcher`: its exit code and envelope.
fn stop(root: &Path, extra: &[&str]) -> (i32, Value) {
    let mut args = vec!["agents", "stop"];
    args.extend_from_slice(extra);
    mdkg(root, "launcher", &args)
}

#[test]
fn agents_stop_drains_a_turn_in_flight_then_the_supervisor_exits() {
    let (scratch, repo) = make_repo(json!({}));
    let root = graph(&repo);
    ok(
        &root,
        "launcher",
        &[
            "agents",
            "add",
            "rev",
            "--harness",
            "claude",
            "--role",
            "reviewer",
        ],
    );
    send(&repo, "rev", "a held request", None);
    let claude = held_claude(&scratch.0);
    let mdkg = Path::new(env!("CARGO_BIN_EXE_mdkg"));
    let mut watch = Watching::start(&scratch.0, &root, mdkg, &claude, &[]);
    watch.wait_for("turn start", |_| scratch.0.join("started").exists());
    let (code, out) = stop(&root, &["rev", "--drain"]);
    assert_eq!(code, 0, "{out}");
    let agent = &out["data"]["agents"][0];
    assert_eq!(agent["name"], "rev", "{out}");
    assert_eq!(agent["state"], "draining", "{out}");
    let supervisor = agent["supervisor_id"].as_str().expect("supervisor id");
    assert!(uuid::Uuid::parse_str(supervisor).is_ok(), "{out}");
    assert!(agent["in_flight_turn"].is_string(), "{out}");
    let request = repo.join(".git/mdkg/run/stop/rev.json");
    let written: Value = serde_json::from_slice(&fs::read(&request).unwrap()).unwrap();
    assert_eq!(written["schema"], "mdkg.run.stop.v1");
    assert_eq!(written["mode"], "drain");
    assert_eq!(written["requested_by"], "launcher");
    assert_eq!(written["supervisor_id"], supervisor);
    // A second request while draining changes nothing; a short wait times out
    // with the turn still running, and kills nothing.
    let (code, out) = stop(&root, &["rev", "--drain", "--wait", "1"]);
    assert_eq!(code, 6, "{out}");
    assert_eq!(out["diagnostics"][0]["code"], "msg.wait_timeout", "{out}");
    assert_eq!(out["data"]["agents"][0]["state"], "draining", "{out}");
    let again: Value = serde_json::from_slice(&fs::read(&request).unwrap()).unwrap();
    assert_eq!(again, written);
    assert!(watch.child.try_wait().unwrap().is_none(), "{}", watch.err());
    fs::write(scratch.0.join("release"), "").unwrap();
    let status = watch.exit();
    assert!(status.success(), "{}", watch.err());
    let err = watch.err();
    assert!(err.contains("rev: stopped by agents stop"), "{err}");
    assert!(err.contains("every agent stopped by agents stop"), "{err}");
    let exit = last_exit(&root, "rev");
    assert_eq!(exit["reason"], "drained", "{exit}");
    assert!(uuid::Uuid::parse_str(exit["supervisor_id"].as_str().unwrap()).is_ok());
    assert!(!request.exists());
    let progress = read_progress(&repo, "rev");
    assert_eq!(progress["progress"]["ran"], 1, "{progress}");
    assert_eq!(progress["finished"], true, "{progress}");
    assert!(progress["current"].is_null(), "{progress}");
    // No supervisor runs it now: stopping again succeeds and changes nothing.
    let (code, out) = stop(&root, &["rev", "--drain"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["data"]["agents"][0]["state"], "not_running", "{out}");
    assert!(out["data"]["agents"][0]["supervisor_id"].is_null(), "{out}");
    assert!(!request.exists());
    // A plain restart needs no resume.
    send(&repo, "rev", "after the stop", None);
    let (code, events) = up(&repo, &["rev"]);
    assert_eq!(code, 0, "{events:?}");
}

#[test]
fn agents_stop_reaches_an_idle_agent_within_a_poll_and_clears_a_dead_supervisors_request() {
    let (scratch, repo) = make_repo(json!({}));
    let root = graph(&repo);
    ok(
        &root,
        "launcher",
        &[
            "agents",
            "add",
            "rev",
            "--harness",
            "claude",
            "--role",
            "reviewer",
        ],
    );
    // A request a supervisor that died never acted on.
    let stop_dir = repo.join(".git/mdkg/run/stop");
    fs::create_dir_all(&stop_dir).unwrap();
    let dead = uuid::Uuid::now_v7().to_string();
    fs::write(
        stop_dir.join("rev.json"),
        json!({"schema": "mdkg.run.stop.v1", "mode": "drain", "requested_by": "nick",
            "requested_at": "2026-10-30T12:00:00.000Z", "supervisor_id": dead})
        .to_string(),
    )
    .unwrap();
    let claude = held_claude(&scratch.0);
    let mdkg = Path::new(env!("CARGO_BIN_EXE_mdkg"));
    let mut watch = Watching::start(&scratch.0, &root, mdkg, &claude, &[]);
    watch.wait_for("stale request removed", |w| {
        w.err().contains("removed rev's stop request")
    });
    assert!(watch.err().contains(&dead), "{}", watch.err());
    let supervisors = repo.join(".git/mdkg/run/supervisors");
    watch.wait_for("supervisor record", |_| {
        fs::read_dir(&supervisors).is_ok_and(|d| {
            d.flatten()
                .any(|e| e.path().extension().is_some_and(|x| x == "json"))
        })
    });
    // Let the supervisor reach its idle wait.
    std::thread::sleep(std::time::Duration::from_millis(500));
    let started = std::time::Instant::now();
    let (code, out) = stop(&root, &["--all", "--drain", "--wait", "30"]);
    assert_eq!(code, 0, "{out}");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(15),
        "{out}"
    );
    let agent = &out["data"]["agents"][0];
    assert_eq!(agent["name"], "rev", "{out}");
    assert_eq!(agent["state"], "stopped", "{out}");
    assert!(agent["in_flight_turn"].is_null(), "{out}");
    let status = watch.exit();
    assert!(status.success(), "{}", watch.err());
    assert!(!scratch.0.join("started").exists(), "no turn ran");
    assert!(!stop_dir.join("rev.json").exists());
    // The supervisor removed its record on exit.
    assert_eq!(
        fs::read_dir(&supervisors)
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .count(),
        0
    );
    // --all with no supervisor running stops nothing.
    let (code, out) = stop(&root, &["--all", "--drain"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["data"]["agents"], json!([]), "{out}");
}

/// A drained agent's reason is recorded as it's let go, so the
/// supervisor that drained it neither hides nor overwrites a later run's
/// (#20921): drain rev while the watch runs rev2, run rev once by hand,
/// then SIGTERM the watch.
#[test]
fn a_drained_agents_later_run_keeps_its_own_exit_reason() {
    let (scratch, repo) = make_repo(json!({
        "rev": {"harness": "claude", "role": "reviewer"},
        "rev2": {"harness": "claude", "role": "reviewer"},
    }));
    let root = graph(&repo);
    // Turns don't wait: only the watch's idle loop matters here.
    fs::write(scratch.0.join("release"), "").unwrap();
    let claude = held_claude(&scratch.0);
    let mdkg = Path::new(env!("CARGO_BIN_EXE_mdkg"));
    let mut watch = Watching::start(&scratch.0, &root, mdkg, &claude, &["rev2"]);
    watch.wait_for("watch start", |_| progress_file(&repo, "rev2").exists());
    std::thread::sleep(std::time::Duration::from_millis(500));
    let supervisors = repo.join(".git/mdkg/run/supervisors");
    watch.wait_for("supervisor record", |_| {
        fs::read_dir(&supervisors).is_ok_and(|d| {
            d.flatten()
                .any(|e| e.path().extension().is_some_and(|x| x == "json"))
        })
    });
    std::thread::sleep(std::time::Duration::from_millis(500));
    let (code, out) = stop(&root, &["rev", "--drain", "--wait", "30"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        out["data"]["agents"][0]["state"],
        "stopped",
        "{out}\n{}",
        watch.err()
    );
    // The watch still names rev, stopped: rev's reason shows.
    let drained = last_exit(&root, "rev");
    assert_eq!(drained["reason"], "drained", "{drained}");
    assert!(last_exit(&root, "rev2").is_null());
    // A pass by hand, while the first watch still runs rev2.
    send(&repo, "rev", "by hand", None);
    let (code, out) = mdkg_out(&root, "launcher", &["agents", "up", "--once", "rev"]);
    assert_eq!(code, 0, "{out}");
    let once = last_exit(&root, "rev");
    assert_eq!(once["reason"], "once", "{once}");
    assert_ne!(once["supervisor_id"], drained["supervisor_id"], "{once}");
    // The first watch ends: rev keeps the later reason, rev2 gets its own.
    watch.signal("-TERM");
    let status = watch.exit();
    assert!(status.success(), "{}", watch.err());
    assert_eq!(last_exit(&root, "rev"), once);
    assert_eq!(last_exit(&root, "rev2")["reason"], "signal");
}

/// A name no definition, saved agent or registration has is nothing to
/// act on: exit 3 with run.unknown_agent, no raw OS error, and one pass
/// over it fails instead of reporting it waiting (e2e B12).
#[test]
fn unknown_agent_exits_3() {
    use std::os::unix::fs::PermissionsExt;
    let (_scratch, repo) = make_repo(json!({}));
    let root = graph(&repo);
    // A complete file doesn't make edit create what isn't there (review
    // of 26581a80): that's add's.
    let file = repo.join("nobody.json");
    fs::write(
        &file,
        json!({"schema":"mdkg.agent.v1", "harness":"claude", "role":"worker"}).to_string(),
    )
    .expect("file");
    let file = file.to_str().expect("utf-8").to_string();
    for args in [
        &["agents", "logs", "nobody"][..],
        &["agents", "edit", "nobody", "--role", "worker"],
        &["agents", "edit", "nobody", "--file", &file],
        &["agents", "rm", "nobody"],
        &["agents", "use-key", "nobody"],
        &["agents", "up", "nobody", "--once"],
    ] {
        let (code, out) = mdkg(&root, "launcher", args);
        assert_eq!(code, 3, "{args:?}: {out}");
        assert_eq!(out["ok"], false, "{args:?}: {out}");
        let d = &out["diagnostics"][0];
        assert_eq!(d["code"], "run.unknown_agent", "{args:?}: {out}");
        let message = d["message"].as_str().unwrap_or_default();
        assert!(!message.contains("os error"), "{args:?}: {out}");
        assert!(d["hint"].as_str().is_some(), "{args:?}: {out}");
    }
    // A valid complete file still made nothing (review of 73189c1d).
    assert!(!root.join(".mdkg/agents/nobody.json").exists());
    assert!(!repo.join(".mdkg/agents/nobody.json").exists());
    // Saved supervisor progress is something to recover: a pass over
    // that name waits for its registration instead of failing.
    let progress = repo.join(".git/mdkg/run/progress");
    fs::create_dir_all(&progress).expect("progress");
    for dir in [
        repo.join(".git/mdkg"),
        repo.join(".git/mdkg/run"),
        progress.clone(),
    ] {
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("mode");
    }
    fs::write(progress.join("ghost.json"), "{}").expect("ghost progress");
    let (code, out) = mdkg(&root, "launcher", &["agents", "up", "ghost", "--once"]);
    assert_ne!(
        out["diagnostics"][0]["code"], "run.unknown_agent",
        "{code}: {out}"
    );
}

#[test]
fn agents_stop_refuses_a_forced_stop_no_mode_and_an_unknown_agent() {
    let (_scratch, repo) = make_repo(json!({}));
    let root = graph(&repo);
    ok(
        &root,
        "launcher",
        &[
            "agents",
            "add",
            "rev",
            "--harness",
            "codex",
            "--role",
            "reviewer",
        ],
    );
    let (code, out) = stop(&root, &["rev", "--drain", "--now"]);
    assert_eq!(code, 1, "{out}");
    assert_eq!(out["diagnostics"][0]["code"], "api.unsupported", "{out}");
    let (code, out) = stop(&root, &["rev"]);
    assert_eq!(code, 1, "{out}");
    assert_eq!(
        out["diagnostics"][0]["code"], "usage.invalid_arguments",
        "{out}"
    );
    let (code, out) = stop(&root, &["nobody", "--drain"]);
    assert_ne!(code, 0, "{out}");
    assert_eq!(out["diagnostics"][0]["code"], "run.unknown_agent", "{out}");
    // Defined but not running.
    let out = ok(&root, "launcher", &["agents", "stop", "rev", "--drain"]);
    assert_eq!(out["data"]["agents"][0]["state"], "not_running", "{out}");
}

/// Inside an agent's turn, `agents stop` stops only that agent
/// (orchestrator's decision on review #11899, option b).
#[test]
fn agents_stop_inside_a_turn_may_stop_only_that_agent() {
    let (_scratch, repo) = make_repo(json!({}));
    let root = graph(&repo);
    for name in ["keyed", "other"] {
        ok(
            &root,
            "launcher",
            &[
                "agents",
                "add",
                name,
                "--harness",
                "codex",
                "--role",
                "reviewer",
            ],
        );
    }
    let refused = |out: &Value| {
        assert_eq!(out["ok"], false, "{out}");
        assert_eq!(out["diagnostics"][0]["code"], "run.unsupported", "{out}");
    };
    // A mark naming no live turn: can't tell whose turn, so refused.
    let out = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
        .current_dir(&root)
        .env("MDKG_RUN_TURN", "1")
        .arg("--root")
        .arg(&root)
        .args([
            "--as", "launcher", "agents", "stop", "keyed", "--drain", "--json",
        ])
        .output()
        .expect("run mdkg");
    refused(&serde_json::from_slice(&out.stdout).expect("json"));
    // A live turn of keyed's in the process chain, under `env -i`.
    let common = mdkg_coord_sqlite::git::common_dir(&repo).expect("common");
    let _turn = mdkg_run::hooks::record_turn_context(
        &common,
        "turn-live-stop",
        &mdkg_run::hooks::TurnContext {
            actor: "keyed".into(),
            harness: mdkg_run::profile::Harness::Codex,
            session: None,
            cwd: repo.clone(),
            starting_head: None,
        },
    )
    .expect("live turn");
    let inside = |args: &str| -> Value {
        let script = format!(
            "cd / && env -i PATH=\"$PATH\" '{}' --root '{}' --as launcher agents stop {args} --drain --json",
            env!("CARGO_BIN_EXE_mdkg"),
            root.display()
        );
        let mut cmd = Command::new(std::env::current_exe().expect("test binary"));
        isolated(&mut cmd)
            .args([
                "--exact",
                "use_key_turn_child_helper",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("MDKG_TEST_TURN_CHILD", &script)
            .env("MDKG_RUN_TURN", "turn-live-stop");
        let out = cmd.output().expect("child");
        let text = String::from_utf8_lossy(&out.stdout);
        let start = text
            .find('{')
            .unwrap_or_else(|| panic!("no envelope: {text}"));
        serde_json::Deserializer::from_str(&text[start..])
            .into_iter::<Value>()
            .next()
            .expect("an envelope")
            .expect("json")
    };
    for args in ["other", "keyed other", "--all"] {
        let out = inside(args);
        refused(&out);
        assert!(
            out["diagnostics"][0]["message"]
                .as_str()
                .is_some_and(|m| m.contains("keyed's turn")),
            "{args}: {out}"
        );
    }
    let out = inside("keyed");
    assert_eq!(out["ok"], true, "{out}");
    assert_eq!(out["data"]["agents"][0]["state"], "not_running", "{out}");
    // Waiting would wait on the caller's own turn.
    let out = inside("keyed --wait 5");
    assert_eq!(out["ok"], false, "{out}");
    assert_eq!(
        out["diagnostics"][0]["code"], "usage.invalid_arguments",
        "{out}"
    );
    // Outside any turn, any agent.
    let out = ok(&root, "launcher", &["agents", "stop", "other", "--drain"]);
    assert_eq!(out["data"]["agents"][0]["state"], "not_running", "{out}");
}

fn status_agent(root: &Path, name: &str) -> (Value, Value) {
    let out = ok(root, "launcher", &["agents", "status"]);
    let agent = out["data"]["agents"]
        .as_array()
        .expect("agents")
        .iter()
        .find(|a| a["name"] == name)
        .unwrap_or_else(|| panic!("no {name}: {out}"))
        .clone();
    (out, agent)
}

fn index_lines(repo: &Path, agent: &str) -> Vec<Value> {
    fs::read_to_string(repo.join(format!(".git/mdkg/run/{agent}/turns.jsonl")))
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).expect("index line"))
        .collect()
}

/// A turn-output event conforms to `mdkg.api.turn_output.v1`, its `data`
/// typed by its `type`, every field declared.
fn turn_output_conforms(event: &Value) {
    static SCHEMA: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    let schema = SCHEMA.get_or_init(|| {
        common::api_schema::load(
            &Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../docs/api/mdkg.api.turn_output.v1.json"),
        )
    });
    let mut errors = Vec::new();
    common::api_schema::check_emitted(schema, schema, event, "$", &mut errors);
    assert!(errors.is_empty(), "{event}: {errors:?}");
}

fn journal_kinds(repo: &Path, agent: &str, turn_id: &str) -> Vec<String> {
    fs::read_to_string(repo.join(format!(
        ".git/mdkg/run/{agent}/journals/{turn_id}.events.jsonl"
    )))
    .unwrap_or_default()
    .lines()
    .map(|l| {
        let e: Value = serde_json::from_str(l).expect("journal line");
        assert_eq!(e["schema"], "mdkg.api.turn_output.v1", "{e}");
        turn_output_conforms(&e);
        e["type"].as_str().expect("event type").to_string()
    })
    .collect()
}

#[test]
fn agents_status_gives_what_a_client_needs_to_re_attach() {
    let (scratch, repo) = make_repo(json!({}));
    let root = graph(&repo);
    ok(
        &root,
        "launcher",
        &[
            "agents",
            "add",
            "rev",
            "--harness",
            "claude",
            "--role",
            "reviewer",
        ],
    );
    // Never run: no supervisor, no turns, and the first turn's position.
    let (out, rev) = status_agent(&root, "rev");
    assert!(
        out["data"]["event_cursor"]
            .as_str()
            .is_some_and(|c| c.starts_with("c1.")),
        "{out}"
    );
    assert_eq!(rev["state"], "stopped", "{rev}");
    assert!(rev["supervisor"].is_null(), "{rev}");
    assert!(rev["current_turn"].is_null(), "{rev}");
    assert!(rev["last_turn"].is_null(), "{rev}");
    assert_eq!(rev["follow_from"], "turn:1", "{rev}");
    send(&repo, "rev", "a held request", None);
    let claude = held_claude(&scratch.0);
    let mdkg = Path::new(env!("CARGO_BIN_EXE_mdkg"));
    let mut watch = Watching::start(&scratch.0, &root, mdkg, &claude, &[]);
    watch.wait_for("turn start", |_| scratch.0.join("started").exists());
    let (_, rev) = status_agent(&root, "rev");
    assert_eq!(rev["state"], "running", "{rev}");
    assert_eq!(rev["supervisor"]["state"], "running", "{rev}");
    assert_eq!(rev["supervisor"]["pid"], watch.child.id(), "{rev}");
    let current = &rev["current_turn"];
    assert_eq!(current["number"], 1, "{rev}");
    let turn_id = current["turn_id"].as_str().expect("turn id").to_string();
    // turn.started and harness.starting are in its journal by the spawn.
    assert_eq!(current["output_after"], 2, "{rev}");
    assert_eq!(rev["follow_from"], format!("{turn_id}:2"), "{rev}");
    assert_eq!(
        journal_kinds(&repo, "rev", &turn_id),
        ["turn.started", "harness.starting"]
    );
    let lines = index_lines(&repo, "rev");
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0]["line"], "start");
    assert_eq!(lines[0]["number"], 1);
    assert_eq!(lines[0]["pid"], watch.child.id());
    assert_eq!(
        lines[0]["supervisor_id"], rev["supervisor"]["supervisor_id"],
        "{lines:?}"
    );
    fs::write(scratch.0.join("release"), "").unwrap();
    watch.wait_for("turn end", |_| index_lines(&repo, "rev").len() == 2);
    // The index's end line comes before the supervisor releases the turn
    // (its lease, then the agent's clean leases, each a command that reads
    // the committed config) and records the agent idle: wait for that.
    watch.wait_for("idle", |_| status_agent(&root, "rev").1["state"] == "idle");
    let (_, rev) = status_agent(&root, "rev");
    assert_eq!(rev["state"], "idle", "{rev}");
    assert!(rev["current_turn"].is_null(), "{rev}");
    assert_eq!(rev["last_turn"]["turn_id"], turn_id.as_str(), "{rev}");
    assert_eq!(rev["last_turn"]["number"], 1, "{rev}");
    assert_eq!(rev["last_turn"]["outcome"], "replied", "{rev}");
    assert_eq!(rev["follow_from"], format!("{turn_id}:end"), "{rev}");
    let kinds = journal_kinds(&repo, "rev", &turn_id);
    assert_eq!(kinds.first().map(String::as_str), Some("turn.started"));
    assert_eq!(kinds.last().map(String::as_str), Some("turn.ended"));
    let lines = index_lines(&repo, "rev");
    assert_eq!(lines[1]["line"], "end", "{lines:?}");
    assert_eq!(lines[1]["outcome"], "replied", "{lines:?}");
    let (code, _) = stop(&root, &["rev", "--drain", "--wait", "30"]);
    assert_eq!(code, 0);
    assert!(watch.exit().success(), "{}", watch.err());
    let (_, rev) = status_agent(&root, "rev");
    assert_eq!(rev["state"], "stopped", "{rev}");
    assert!(rev["supervisor"].is_null(), "{rev}");
    assert_eq!(rev["follow_from"], format!("{turn_id}:end"), "{rev}");
}

#[test]
fn a_turn_whose_launcher_died_after_the_spawn_reads_interrupted_never_running() {
    let (scratch, repo) = make_repo(json!({}));
    let root = graph(&repo);
    ok(
        &root,
        "launcher",
        &[
            "agents",
            "add",
            "rev",
            "--harness",
            "claude",
            "--role",
            "reviewer",
        ],
    );
    send(&repo, "rev", "a held request", None);
    let claude = held_claude(&scratch.0);
    let mdkg = Path::new(env!("CARGO_BIN_EXE_mdkg"));
    let mut watch = Watching::start(&scratch.0, &root, mdkg, &claude, &[]);
    watch.wait_for("turn start", |_| scratch.0.join("started").exists());
    // The whole group, harness included, at once.
    watch.signal_group("-KILL");
    let _ = watch.exit();
    let (_, rev) = status_agent(&root, "rev");
    assert!(rev["current_turn"].is_null(), "{rev}");
    assert_eq!(rev["last_turn"]["outcome"], "interrupted", "{rev}");
    assert_eq!(rev["last_turn"]["number"], 1, "{rev}");
    assert_eq!(rev["state"], "stopped", "{rev}");
    assert_eq!(rev["supervisor"]["state"], "stale", "{rev}");
    let turn_id = rev["last_turn"]["turn_id"].as_str().unwrap();
    assert_eq!(rev["follow_from"], format!("{turn_id}:end"), "{rev}");
}

/// A Claude stand-in that streams a tool call, touches `<scratch>/started`,
/// waits for `<scratch>/release` (removing it), then finishes as the fake.
fn streaming_claude(scratch: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let script = scratch.join("streaming-claude");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\n[ \"$1\" = auth ] && exec '{fake}' \"$@\"\n\
             echo '{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"Looking.\"}},{{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"Bash\",\"input\":{{\"command\":\"ls\"}}}}]}}}}'\n\
             echo 'a notice' >&2\n\
             echo '{{\"type\":\"user\",\"message\":{{\"content\":[{{\"type\":\"tool_result\",\"tool_use_id\":\"t1\",\"is_error\":false}}]}}}}'\n\
             touch '{dir}/started'\nwhile [ ! -e '{dir}/release' ]; do sleep 0.05; done\nrm -f '{dir}/release' '{dir}/started'\n\
             exec '{fake}' \"$@\"\n",
            fake = fixture("fake-claude.sh").display(),
            dir = scratch.display()
        ),
    )
    .expect("streaming claude");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("mode");
    script
}

/// `agents logs … --json` running as a child: its lines, one at a time.
struct Follower {
    child: std::process::Child,
    lines: std::sync::mpsc::Receiver<String>,
}

impl Follower {
    fn start(root: &Path, args: &[&str]) -> Self {
        use std::io::BufRead as _;
        let mut child = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
            .current_dir(root)
            .arg("--root")
            .arg(root)
            .args(["--as", "launcher", "agents", "logs"])
            .args(args)
            .arg("--json")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("start follower");
        let stdout = child.stdout.take().expect("follower stdout");
        let (send, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout)
                .lines()
                .map_while(Result::ok)
            {
                if send.send(line).is_err() {
                    break;
                }
            }
        });
        Self { child, lines }
    }

    fn next(&self) -> Value {
        let line = self
            .lines
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("a frame within a minute");
        let event: Value = serde_json::from_str(&line).expect("an NDJSON frame");
        assert_eq!(event["schema"], "mdkg.api.turn_output.v1", "{event}");
        turn_output_conforms(&event);
        event
    }

    /// Frames until one of type `kind`, that one included.
    fn until(&self, kind: &str) -> Vec<Value> {
        let mut got = Vec::new();
        loop {
            let event = self.next();
            let done = event["type"] == kind;
            got.push(event);
            if done {
                return got;
            }
        }
    }

    fn stop(mut self) -> i32 {
        let _ = Command::new("/bin/kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            if let Some(status) = self.child.try_wait().expect("poll follower") {
                return status.code().unwrap_or(-1);
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the follower didn't stop"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}

impl Drop for Follower {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
    }
}

fn types(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .map(|e| e["type"].as_str().expect("type").to_string())
        .collect()
}

#[test]
fn agents_logs_follows_an_agents_turns_in_order_from_a_status_position() {
    let (scratch, repo) = make_repo(json!({}));
    let root = graph(&repo);
    ok(
        &root,
        "launcher",
        &[
            "agents",
            "add",
            "rev",
            "--harness",
            "claude",
            "--role",
            "reviewer",
        ],
    );
    let (_, rev) = status_agent(&root, "rev");
    let from = rev["follow_from"]
        .as_str()
        .expect("follow_from")
        .to_string();
    assert_eq!(from, "turn:1");
    let follow = Follower::start(&root, &["rev", "--follow", "--after", &from]);
    let first = follow.next();
    assert_eq!(first["type"], "agent.state", "{first}");
    assert!(first["turn"].is_null() && first["seq"].is_null(), "{first}");
    send(&repo, "rev", "first request", None);
    let claude = streaming_claude(&scratch.0);
    let mdkg = Path::new(env!("CARGO_BIN_EXE_mdkg"));
    let mut watch = Watching::start(&scratch.0, &root, mdkg, &claude, &[]);
    watch.wait_for("turn start", |_| scratch.0.join("started").exists());
    let mut turn1 = follow.until("tool.ended");
    // Across stdout and stderr, the launcher's arrival order: the stderr
    // line may come either side of the tool's end.
    if !types(&turn1).contains(&"status".to_string()) {
        turn1.extend(follow.until("status"));
    }
    turn1.retain(|e| e["type"] != "agent.state");
    let mut stdout_only = turn1.clone();
    stdout_only.retain(|e| e["type"] != "status");
    assert_eq!(
        types(&stdout_only),
        [
            "turn.started",
            "harness.starting",
            "text",
            "tool.started",
            "tool.ended"
        ]
    );
    let status = turn1
        .iter()
        .find(|e| e["type"] == "status")
        .expect("status");
    assert_eq!(status["data"]["text"], "a notice");
    let turn1 = stdout_only;
    assert_eq!(turn1[0]["data"]["number"], 1, "{turn1:?}");
    let id1 = turn1[0]["data"]["turn_id"]
        .as_str()
        .expect("turn id")
        .to_string();
    assert_eq!(turn1[0]["turn"], format!("/v1/agents/rev/turns/{id1}"));
    let seqs: Vec<u64> = turn1
        .iter()
        .map(|e| e["seq"].as_u64().expect("seq"))
        .collect();
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "{seqs:?}");
    assert_eq!(
        turn1[3]["data"],
        json!({"class": "shell", "name": "Bash", "call_id": "t1", "summary": "ls"})
    );
    fs::write(scratch.0.join("release"), "").expect("release");
    let mut rest = follow.until("turn.ended");
    rest.retain(|e| e["type"] != "agent.state");
    assert_eq!(
        rest.last().expect("end")["data"]["outcome"],
        "replied",
        "{rest:?}"
    );
    assert!(types(&rest).contains(&"usage".to_string()), "{rest:?}");
    // Never the native init event without --native.
    assert!(!types(&rest).contains(&"native".to_string()), "{rest:?}");
    // The next turn follows on the same stream, in number order.
    watch.wait_for("idle", |_| !scratch.0.join("started").exists());
    send(&repo, "rev", "second request", None);
    watch.wait_for("second turn", |_| scratch.0.join("started").exists());
    fs::write(scratch.0.join("release"), "").expect("release");
    let mut turn2 = follow.until("turn.ended");
    turn2.retain(|e| e["type"] != "agent.state");
    assert_eq!(turn2[0]["type"], "turn.started", "{turn2:?}");
    assert_eq!(turn2[0]["data"]["number"], 2, "{turn2:?}");
    assert_eq!(follow.stop(), 0);
    // Without --follow: the last turn, then exit. With --native, the same
    // events keep their seq, and the native ones join them.
    let (code, out) = mdkg_out(&root, "launcher", &["agents", "logs", "rev"]);
    assert_eq!(code, 0, "{out}");
    let plain: Vec<Value> = out
        .lines()
        .map(|l| serde_json::from_str(l).expect("frame"))
        .collect();
    assert_eq!(plain[0]["data"]["number"], 2, "{out}");
    let (code, out) = mdkg_out(&root, "launcher", &["agents", "logs", "rev", "--native"]);
    assert_eq!(code, 0, "{out}");
    let native: Vec<Value> = out
        .lines()
        .map(|l| serde_json::from_str(l).expect("frame"))
        .collect();
    assert!(types(&native).contains(&"native".to_string()), "{out}");
    let shared: Vec<&Value> = native.iter().filter(|e| e["type"] != "native").collect();
    assert_eq!(
        shared.iter().map(|e| &e["seq"]).collect::<Vec<_>>(),
        plain.iter().map(|e| &e["seq"]).collect::<Vec<_>>()
    );
    // Reconnecting after an event resumes with the next, then the next turn.
    let at = &turn1[2];
    let position = format!("{id1}:{}", at["seq"]);
    let again = Follower::start(&root, &["rev", "--follow", "--after", &position]);
    let mut resumed = again.until("turn.ended");
    resumed.retain(|e| e["type"] != "agent.state");
    // The next event after `text` in seq order, whichever pipe it came from.
    let after = at["seq"].as_u64().expect("seq");
    assert_eq!(resumed[0]["seq"].as_u64(), Some(after + 1), "{resumed:?}");
    assert!(
        resumed.iter().all(|e| e["seq"].as_u64() > Some(after)),
        "{resumed:?}"
    );
    let mut next = again.until("turn.ended");
    next.retain(|e| e["type"] != "agent.state");
    assert_eq!(next[0]["data"]["number"], 2, "{next:?}");
    assert_eq!(again.stop(), 0);
    let (code, _) = stop(&root, &["rev", "--drain", "--wait", "30"]);
    assert_eq!(code, 0);
    assert!(watch.exit().success(), "{}", watch.err());
}

#[test]
fn agents_logs_normalizes_codex_and_opencode_turns_and_refuses_what_it_cant_show() {
    let (scratch, repo) = make_repo(json!({}));
    let root = graph(&repo);
    for (agent, harness) in [("cx", "codex"), ("oc", "opencode")] {
        ok(
            &root,
            "launcher",
            &[
                "agents",
                "add",
                agent,
                "--harness",
                harness,
                "--role",
                "reviewer",
            ],
        );
        send(&repo, agent, "a request", None);
        let (code, events, err) = up_harnesses(&scratch.0, &root, agent);
        assert_eq!(code, 0, "{agent}: {events:?} {err}");
        let (code, out) = mdkg_out(&root, "launcher", &["agents", "logs", agent]);
        assert_eq!(code, 0, "{out}");
        let frames: Vec<Value> = out
            .lines()
            .map(|l| serde_json::from_str(l).expect("frame"))
            .collect();
        let kinds = types(&frames);
        assert_eq!(
            kinds.first().map(String::as_str),
            Some("turn.started"),
            "{out}"
        );
        assert_eq!(
            kinds.last().map(String::as_str),
            Some("turn.ended"),
            "{out}"
        );
        let reply = if harness == "codex" {
            "codex reply"
        } else {
            "opencode reply"
        };
        assert!(
            frames.iter().any(|e| e["type"] == "text"
                && e["data"]["text"]
                    .as_str()
                    .is_some_and(|t| t.contains(reply))),
            "{agent}: {out}"
        );
        if harness == "codex" {
            assert!(kinds.contains(&"status".to_string()), "{out}");
            assert!(!kinds.iter().any(|k| k.starts_with("tool.")), "{out}");
        }
    }
    // Refusals: one envelope each, nothing streamed.
    let (code, out) = mdkg(&root, "launcher", &["agents", "logs", "nobody"]);
    assert_ne!(code, 0, "{out}");
    assert_eq!(out["diagnostics"][0]["code"], "run.unknown_agent", "{out}");
    let (code, out) = mdkg(
        &root,
        "launcher",
        &["agents", "logs", "cx", "--follow", "--after", "bogus"],
    );
    assert_eq!(code, 1, "{out}");
    assert_eq!(out["schema"], "mdkg.agents.logs.v1", "{out}");
    assert_eq!(out["diagnostics"][0]["code"], "api.bad_cursor", "{out}");
    let stranger = uuid::Uuid::now_v7().to_string();
    let (code, out) = mdkg(
        &root,
        "launcher",
        &[
            "agents",
            "logs",
            "cx",
            "--follow",
            "--after",
            &format!("{stranger}:end"),
        ],
    );
    assert_eq!(code, 5, "{out}");
    assert_eq!(out["diagnostics"][0]["code"], "api.cursor_expired", "{out}");
    assert_eq!(out["data"]["earliest"], "turn:1", "{out}");
    let (code, out) = mdkg(
        &root,
        "launcher",
        &["agents", "logs", "cx", "--turn", &stranger],
    );
    assert_eq!(code, 3, "{out}");
    assert_eq!(
        out["diagnostics"][0]["code"], "api.unknown_resource",
        "{out}"
    );
    // A purged journal: its receipt is the lasting record.
    let lines = index_lines(&repo, "cx");
    let id = lines[0]["turn_id"].as_str().expect("turn id");
    fs::remove_file(repo.join(format!(".git/mdkg/run/cx/journals/{id}.events.jsonl")))
        .expect("purge");
    let (code, out) = mdkg(&root, "launcher", &["agents", "logs", "cx", "--turn", id]);
    assert_eq!(code, 3, "{out}");
    assert_eq!(out["diagnostics"][0]["code"], "api.output_gone", "{out}");
    assert!(
        out["data"]["receipt"]
            .as_str()
            .is_some_and(|r| r.ends_with(&format!("{id}.txt"))),
        "{out}"
    );
    // An agent follow still shows that turn, as gone.
    let follow = Follower::start(&root, &["cx", "--follow", "--after", "turn:1"]);
    let mut gone = follow.until("turn.ended");
    gone.retain(|e| e["type"] != "agent.state");
    assert_eq!(types(&gone), ["turn.started", "turn.ended"], "{gone:?}");
    assert_eq!(gone[1]["data"]["output_gone"], true, "{gone:?}");
    assert_eq!(gone[1]["data"]["outcome"], "replied", "{gone:?}");
    assert_eq!(follow.stop(), 0);
}

/// A turn just starting has an index line and, for a moment, no journal:
/// that's a live turn, never purged output (review #11899).
#[test]
fn agents_logs_waits_for_a_turn_just_starting_and_never_calls_it_purged() {
    use std::io::Write as _;
    let (scratch, repo) = make_repo(json!({}));
    let root = graph(&repo);
    ok(
        &root,
        "launcher",
        &[
            "agents",
            "add",
            "cx",
            "--harness",
            "codex",
            "--role",
            "reviewer",
        ],
    );
    send(&repo, "cx", "a request", None);
    let (code, events, err) = up_harnesses(&scratch.0, &root, "cx");
    assert_eq!(code, 0, "{events:?} {err}");
    // The next attempt's start line, its launcher this live process.
    let id = uuid::Uuid::now_v7().to_string();
    let host = Command::new("uname").arg("-n").output().expect("uname");
    let start = json!({
        "line": "start", "turn_id": id, "number": 2, "files": "turn-2",
        "started_at": "2026-10-30T00:00:00Z", "supervisor_id": null,
        "host": String::from_utf8_lossy(&host.stdout).trim(),
        "pid": std::process::id(), "pid_started_at": null,
    });
    let index = repo.join(".git/mdkg/run/cx/turns.jsonl");
    let append = |line: &Value| {
        let mut f = fs::OpenOptions::new()
            .append(true)
            .open(&index)
            .expect("index");
        writeln!(f, "{line}").expect("append");
    };
    append(&start);
    // Without --follow: nothing yet, and no refusal.
    for args in [
        vec!["agents", "logs", "cx", "--turn", &id],
        vec!["agents", "logs", "cx"],
    ] {
        let (code, out) = mdkg_out(&root, "launcher", &args);
        assert_eq!((code, out.as_str()), (0, ""), "{args:?}");
    }
    // A journal that's there but can't be read is refused, never waited on.
    {
        use std::os::unix::fs::PermissionsExt as _;
        let journal = repo.join(format!(".git/mdkg/run/cx/journals/{id}.events.jsonl"));
        fs::write(&journal, "").expect("journal");
        fs::set_permissions(&journal, fs::Permissions::from_mode(0o000)).expect("mode");
        let (code, out) = mdkg(&root, "launcher", &["agents", "logs", "cx", "--turn", &id]);
        assert_ne!(code, 0, "{out}");
        assert_eq!(out["diagnostics"][0]["code"], "run.io_failed", "{out}");
        fs::remove_file(&journal).expect("remove");
    }
    // With --follow it waits; an attempt that then ends without starting
    // is no turn to show.
    let mut follow = Follower::start(&root, &["cx", "--turn", &id, "--follow"]);
    std::thread::sleep(std::time::Duration::from_millis(1500));
    assert!(
        follow.child.try_wait().expect("follower").is_none(),
        "it waits"
    );
    append(&json!({
        "line": "end", "turn_id": id, "ended_at": "2026-10-30T00:00:01Z",
        "outcome": "not_started",
    }));
    // One envelope, pretty-printed, then the follower exits.
    let mut text = String::new();
    while let Ok(line) = follow
        .lines
        .recv_timeout(std::time::Duration::from_secs(60))
    {
        text.push_str(&line);
        text.push('\n');
    }
    let refusal: Value = serde_json::from_str(&text).expect("an envelope");
    assert_eq!(refusal["schema"], "mdkg.agents.logs.v1", "{refusal}");
    assert_eq!(
        refusal["diagnostics"][0]["code"], "api.unknown_resource",
        "{refusal}"
    );
    assert_eq!(follow.child.wait().expect("exit").code(), Some(3));
    let (code, out) = mdkg(&root, "launcher", &["agents", "logs", "cx", "--turn", &id]);
    assert_eq!(code, 3, "{out}");
    assert_eq!(
        out["diagnostics"][0]["code"], "api.unknown_resource",
        "{out}"
    );
}

/// A launch wrapper outside the repository: it records its arguments
/// (NUL-separated) and a copy of its turn spec with the spec's mode, refuses
/// with 125 while `<scratch>/refuse` exists, and otherwise runs what follows
/// its `--` unchanged.
fn fake_wrapper(scratch: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let script = scratch.join("wrap.sh");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\nd='{dir}'\nprintf '%s\\0' \"$@\" > \"$d/wrapper.argv\"\n\
             [ \"$1\" = --profile ] && [ \"$2\" = turn ] || exit 99\n\
             cp \"$3\" \"$d/wrapper.spec.json\"\nls -l \"$3\" | cut -c1-10 > \"$d/wrapper.spec.mode\"\n\
             [ \"$4\" = -- ] || exit 98\n[ -e \"$d/refuse\" ] && exit 125\nshift 4\nexec \"$@\"\n",
            dir = scratch.display()
        ),
    )
    .expect("fake wrapper");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("wrapper mode");
    script
}

/// Names `wrapper` in the user config of the scratch home `agents up`
/// runs with.
fn user_wrapper(scratch: &Path, wrapper: &Path) {
    let config = scratch.join("home/.config/mdkg/config.json");
    fs::create_dir_all(config.parent().expect("folder")).expect("user config folder");
    fs::write(
        &config,
        json!({"run": {"wrapper": {
            "command": [wrapper.to_str().expect("utf-8"), "--profile", "turn"],
            "timeout_grace_ms": 3000}}})
        .to_string(),
    )
    .expect("user config");
}

/// `agents up <agent> --once --json` with every harness's fake and a home
/// inside the scratch: exit code, events, stderr.
fn up_harnesses(scratch: &Path, root: &Path, agent: &str) -> (i32, Vec<Value>, String) {
    let home = scratch.join("home");
    fs::create_dir_all(home.join(".codex")).expect("home");
    let out = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
        .current_dir(root)
        .arg("--root")
        .arg(root)
        .args([
            "--as", "launcher", "agents", "up", agent, "--once", "--json",
        ])
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("CODEX_HOME", home.join(".codex"))
        .env("MDKG_RUN_CODEX", fixture("fake-codex.sh"))
        .env("MDKG_RUN_OPENCODE", fixture("fake-opencode.sh"))
        .output()
        .expect("run agents up");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let events = stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap_or_else(|_| panic!("not JSON lines: {stdout}")))
        .collect();
    (
        out.status.code().unwrap_or(-1),
        events,
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn a_launch_wrapper_runs_each_harness_unchanged_and_can_refuse_a_turn() {
    let (scratch, repo) = make_repo(json!({}));
    let root = graph(&repo);
    let wrapper = fake_wrapper(&scratch.0);
    // The user layer, under the scratch home agents up runs with.
    user_wrapper(&scratch.0, &wrapper);
    for (harness, role) in [
        ("claude", "reviewer"),
        ("codex", "reviewer"),
        ("opencode", "worker"),
    ] {
        let agent = format!("w-{harness}");
        ok(
            &root,
            "launcher",
            &[
                "agents",
                "add",
                &agent,
                "--harness",
                harness,
                "--role",
                role,
            ],
        );
        send(&repo, &agent, "wrapped request", None);
        let (code, events, err) = up_harnesses(&scratch.0, &root, &agent);
        assert_eq!(code, 0, "{harness}: {events:?} {err}");
        let turn = turns(&events, &agent)[0];
        let spec: Value =
            serde_json::from_slice(&fs::read(scratch.0.join("wrapper.spec.json")).unwrap())
                .unwrap();
        assert_eq!(spec["schema"], "mdkg.run.wrapper.v1", "{spec}");
        assert_eq!(spec["agent"], agent.as_str());
        assert_eq!(spec["harness"], harness);
        assert_eq!(spec["role"], role);
        assert_eq!(spec["read_only"], role == "reviewer", "{spec}");
        let access = if role == "reviewer" { "read" } else { "write" };
        assert_eq!(spec["paths"]["worktree"]["access"], access, "{spec}");
        assert_eq!(spec["paths"]["git_common_dir"]["access"], access, "{spec}");
        assert!(
            uuid::Uuid::parse_str(spec["turn_id"].as_str().unwrap()).is_ok(),
            "{spec}"
        );
        // Names, never values. The prompt is in the spec only where the
        // harness takes it in its argv (OpenCode); stdin stays the harness's.
        let keys = spec["env_keys"].as_array().unwrap();
        assert!(keys.iter().any(|k| k == "MDKG_RUN_TURN"), "{spec}");
        assert_eq!(
            spec.to_string().contains("wrapped request"),
            harness == "opencode",
            "{spec}"
        );
        assert_eq!(
            fs::read_to_string(scratch.0.join("wrapper.spec.mode"))
                .unwrap()
                .trim(),
            "-rw-------"
        );
        // After its own argv, the spec and `--`, exactly the harness's.
        let argv = fs::read(scratch.0.join("wrapper.argv")).unwrap();
        let argv: Vec<String> = argv
            .split(|b| *b == 0)
            .filter(|a| !a.is_empty())
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect();
        let harness_argv: Vec<String> = spec["argv"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a.as_str().unwrap().to_string())
            .collect();
        assert_eq!(argv[3], "--", "{argv:?}");
        assert_eq!(argv[4..].to_vec(), harness_argv, "{harness}");
        let program = match harness {
            "claude" => fixture("fake-claude.sh"),
            "codex" => fixture("fake-codex.sh"),
            _ => fixture("fake-opencode.sh"),
        };
        assert_eq!(Path::new(&harness_argv[0]), program, "{spec}");
        // The receipt records that it ran.
        let receipt = fs::read_to_string(turn["data"]["turn"]["receipt"]["path"].as_str().unwrap())
            .expect("retained receipt");
        let data: Value = serde_json::from_str(
            receipt
                .lines()
                .find(|l| l.starts_with('{'))
                .expect("receipt JSON"),
        )
        .unwrap();
        // In the attempt's process exit; the top-level receipt field is
        // a-flow's to add (A19, option c).
        let wrap = fs::canonicalize(&wrapper).unwrap();
        let ran = &data["process_exits"][0]["wrapper"];
        assert_eq!(ran["command"], json!([wrap]), "{data}");
        assert!(
            ran["spec_digest"]
                .as_str()
                .is_some_and(|d| d.starts_with("sha256:")),
            "{data}"
        );
    }
    // Exit 125: refused, reported, and the agent isn't run.
    fs::write(scratch.0.join("refuse"), "").unwrap();
    fs::remove_file(root.join("claude.args")).ok();
    send(&repo, "w-claude", "refused request", None);
    let (code, events, err) = up_harnesses(&scratch.0, &root, "w-claude");
    assert_ne!(code, 0, "{events:?} {err}");
    let turn = turns(&events, "w-claude")[0];
    assert_eq!(
        turn["diagnostics"][0]["code"], "run.wrapper_refused",
        "{turn}"
    );
    assert!(!root.join("claude.args").exists(), "the harness ran");
    fs::remove_file(scratch.0.join("refuse")).unwrap();
}

#[test]
fn a_turn_that_rewrites_the_machine_local_config_keeps_the_users_wrapper() {
    use std::os::unix::fs::PermissionsExt;
    let (scratch, repo) = make_repo(json!({}));
    let root = graph(&repo);
    let wrapper = fake_wrapper(&scratch.0);
    user_wrapper(&scratch.0, &wrapper);
    // A worker that writes <common>/mdkg/config.local.json during its turn,
    // with what `<scratch>/local-config` holds.
    let claude = scratch.0.join("rewriting-claude");
    fs::write(
        &claude,
        format!(
            "#!/bin/sh\n[ \"$1\" = auth ] && exec '{fake}' \"$@\"\n\
             common=$(git rev-parse --path-format=absolute --git-common-dir)\n\
             cp '{dir}/local-config' \"$common/mdkg/config.local.json\"\n\
             exec '{fake}' \"$@\"\n",
            fake = fixture("fake-claude.sh").display(),
            dir = scratch.0.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&claude, fs::Permissions::from_mode(0o755)).unwrap();
    ok(
        &root,
        "launcher",
        &[
            "agents",
            "add",
            "wk",
            "--harness",
            "claude",
            "--role",
            "worker",
        ],
    );
    let wrap = fs::canonicalize(&wrapper).unwrap();
    for local in [
        json!({"run": {"wrapper": null}}),
        json!({"run": {"wrapper": {"command": ["/bin/sh", "-c", "shift 2; exec \"$@\"", "sh"]}}}),
    ] {
        fs::write(scratch.0.join("local-config"), local.to_string()).unwrap();
        for request in ["the rewriting turn", "the next turn"] {
            fs::remove_file(scratch.0.join("wrapper.argv")).ok();
            send(&repo, "wk", request, None);
            let out = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
                .current_dir(&root)
                .arg("--root")
                .arg(&root)
                .args(["--as", "launcher", "agents", "up", "wk", "--once", "--json"])
                .env("HOME", scratch.0.join("home"))
                .env("XDG_CONFIG_HOME", scratch.0.join("home/.config"))
                .env("MDKG_RUN_CLAUDE", &claude)
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert!(out.status.success(), "{local}: {stdout}");
            assert!(
                scratch.0.join("wrapper.argv").exists(),
                "{local}: {request} ran unwrapped"
            );
            let turn: Value = serde_json::from_str(stdout.lines().last().unwrap()).unwrap();
            let receipt = fs::read_to_string(
                turn["data"]["turn"]["receipt"]["path"]
                    .as_str()
                    .expect("receipt"),
            )
            .unwrap();
            let data: Value =
                serde_json::from_str(receipt.lines().find(|l| l.starts_with('{')).unwrap())
                    .unwrap();
            let ran = &data["process_exits"][0]["wrapper"];
            assert_eq!(ran["command"], json!([wrap]), "{local}: {data}");
            let spec = fs::read(scratch.0.join("wrapper.spec.json")).unwrap();
            assert_eq!(
                ran["spec_digest"],
                format!("sha256:{}", mdkg_format::Digest::of_bytes(&spec).hex()),
                "{local}"
            );
        }
    }
}

#[test]
fn a_committed_launch_wrapper_is_ignored_with_a_warning_and_a_bad_one_refuses() {
    let (scratch, repo) = make_repo(json!({}));
    let root = graph(&repo);
    let wrapper = fake_wrapper(&scratch.0);
    ok(
        &root,
        "launcher",
        &[
            "agents",
            "add",
            "rev",
            "--harness",
            "claude",
            "--role",
            "reviewer",
        ],
    );
    let config = root.join(".mdkg/config.json");
    let mut cfg: Value = serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
    cfg["run"] = json!({"wrapper": {"command": [wrapper.to_str().unwrap()]}});
    fs::write(&config, serde_json::to_string_pretty(&cfg).unwrap()).unwrap();
    commit_graph_config(&root);
    send(&repo, "rev", "unwrapped request", None);
    let (code, events, err) = up_harnesses(&scratch.0, &root, "rev");
    assert_eq!(code, 0, "{events:?} {err}");
    assert!(
        err.contains("run.wrapper in the committed config is ignored"),
        "{err}"
    );
    assert!(!scratch.0.join("wrapper.argv").exists(), "the wrapper ran");
    // So is one in the machine-local config, which a turn could write.
    fs::write(
        repo.join(".git/mdkg/config.local.json"),
        json!({"run": {"wrapper": {"command": [wrapper.to_str().unwrap()]}}}).to_string(),
    )
    .unwrap();
    send(&repo, "rev", "local wrapper", None);
    let (code, events, err) = up_harnesses(&scratch.0, &root, "rev");
    assert_eq!(code, 0, "{events:?} {err}");
    assert!(
        err.contains("run.wrapper in the machine-local config is ignored"),
        "{err}"
    );
    assert!(!scratch.0.join("wrapper.argv").exists(), "the wrapper ran");
    // A user wrapper that can't be trusted refuses the turn rather than
    // run it bare.
    let inside = repo.join("wrap.sh");
    fs::copy(&wrapper, &inside).unwrap();
    let config = scratch.0.join("home/.config/mdkg/config.json");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(
        &config,
        json!({"run": {"wrapper": {"command": [inside.to_str().unwrap()]}}}).to_string(),
    )
    .unwrap();
    fs::remove_file(root.join("claude.args")).ok();
    send(&repo, "rev", "untrusted wrapper", None);
    let (code, events, err) = up_harnesses(&scratch.0, &root, "rev");
    assert_ne!(code, 0, "{events:?} {err}");
    let text = format!("{events:?}");
    assert!(text.contains("inside the repository"), "{text}");
    assert!(!root.join("claude.args").exists(), "the harness ran");
}

#[test]
fn use_key_marks_one_key_turn_only_where_the_policy_allows_it() {
    let (_scratch, repo) = make_repo(json!({}));
    let root = graph(&repo);
    for (name, harness, policy) in [
        ("keyed", "claude", r#"{"auth":{"api_key_fallback":true}}"#),
        ("cx", "codex", r#"{"auth":{"api_key_fallback":true}}"#),
        ("plain", "claude", "{}"),
        ("oc", "opencode", r#"{"auth":{"api_key_fallback":true}}"#),
    ] {
        ok(
            &root,
            "launcher",
            &[
                "agents",
                "add",
                name,
                "--harness",
                harness,
                "--role",
                "reviewer",
                "--policy",
                policy,
            ],
        );
    }
    for name in ["keyed", "cx"] {
        let out = ok(&root, "launcher", &["agents", "use-key", name]);
        assert_eq!(out["data"]["agent"], name, "{out}");
        let mark = PathBuf::from(out["data"]["mark"].as_str().expect("mark"));
        assert!(mark.is_file(), "{out}");
        // A note wakes the agent's supervisor for that turn.
        assert_eq!(out["data"]["note_sent"], true, "{out}");
    }
    let inbox = ok(&root, "keyed", &["msg", "inbox"]);
    assert!(
        inbox.to_string().contains("use-key: your next turn bills"),
        "{inbox}"
    );
    // The note's id is the one use-key recorded for the turn to know it by.
    let recorded = fs::read_to_string(repo.join(".git/mdkg/run/billing/keyed.wake")).expect("wake");
    let id = recorded.lines().last().expect("an id");
    assert!(inbox.to_string().contains(id), "{id}: {inbox}");
    // A dangling link where a saved agent's state would be still names an
    // agent: never read, never taken for absence (review of 9989c849).
    let run = repo.join(".git/mdkg/run");
    fs::create_dir_all(&run).expect("run");
    std::os::unix::fs::symlink(run.join("nowhere.json"), run.join("ghost.json")).expect("link");
    let (code, out) = mdkg(&root, "ghost", &["agents", "use-key", "cx"]);
    assert_ne!(code, 0, "{out}");
    assert!(
        out["diagnostics"][0]["message"]
            .as_str()
            .is_some_and(|m| m.contains("ghost is an agent")),
        "{out}"
    );
    // An agent's own actor can't bill the key for itself.
    let (code, out) = mdkg(&root, "keyed", &["agents", "use-key", "cx"]);
    assert_ne!(code, 0, "{out}");
    assert!(
        out["diagnostics"][0]["message"]
            .as_str()
            .is_some_and(|m| m.contains("keyed is an agent")),
        "{out}"
    );
    // Not allowed by the policy, no key fallback, or no such agent.
    for name in ["plain", "oc", "nobody"] {
        let (code, out) = mdkg(&root, "launcher", &["agents", "use-key", name]);
        assert_ne!(code, 0, "{name}: {out}");
        assert_eq!(out["ok"], false, "{out}");
    }
    // Inside an agent turn (its MCP server included), an agent can't bill
    // the key for itself.
    let out = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
        .current_dir(&root)
        .env("MDKG_RUN_TURN", "1")
        .args(["--root"])
        .arg(&root)
        .args(["--as", "launcher", "agents", "use-key", "keyed", "--json"])
        .output()
        .expect("run mdkg");
    let refused: Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(refused["ok"], false, "{refused}");
    assert!(
        refused["diagnostics"][0]["message"]
            .as_str()
            .is_some_and(|m| m.contains("refused in an agent turn")),
        "{refused}"
    );
    // A live turn of this repository in the process chain makes it an
    // agent's, though the command runs under `env -i` as a person.
    let common = mdkg_coord_sqlite::git::common_dir(&repo).expect("common");
    let _turn = mdkg_run::hooks::record_turn_context(
        &common,
        "turn-live-1",
        &mdkg_run::hooks::TurnContext {
            actor: "keyed".into(),
            harness: mdkg_run::profile::Harness::Claude,
            session: Some("s".into()),
            cwd: repo.clone(),
            starting_head: None,
        },
    )
    .expect("live turn");
    let script = format!(
        "cd / && env -i PATH=\"$PATH\" '{}' --root '{}' --as launcher agents use-key keyed --json",
        env!("CARGO_BIN_EXE_mdkg"),
        root.display()
    );
    let child = |mark: &str| {
        let mut cmd = Command::new(std::env::current_exe().expect("test binary"));
        isolated(&mut cmd)
            .args([
                "--exact",
                "use_key_turn_child_helper",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("MDKG_TEST_TURN_CHILD", &script)
            .env("MDKG_RUN_TURN", mark);
        for m in [
            "CLAUDECODE",
            "CODEX_THREAD_ID",
            "OPENCODE",
            "OPENCODE_SESSION_ID",
            "AI_AGENT",
        ] {
            cmd.env_remove(m);
        }
        cmd.output().expect("child")
    };
    let inside = child("turn-live-1");
    let out = String::from_utf8_lossy(&inside.stdout);
    assert!(out.contains("this runs under keyed's turn"), "{out}");
    let outside = child("turn-not-live");
    let out = String::from_utf8_lossy(&outside.stdout);
    assert!(out.contains("\"ok\": true"), "{out}");
    let status = ok(&root, "launcher", &["agents", "status"]);
    let billing = |name: &str| {
        status["data"]["agents"]
            .as_array()
            .expect("agents")
            .iter()
            .find(|a| a["name"] == name)
            .map(|a| a["billing"].clone())
            .unwrap_or_default()
    };
    assert!(
        billing("keyed")
            .as_str()
            .is_some_and(|b| b.contains("next turn bills the API key")),
        "{status}"
    );
    assert!(billing("plain").is_null(), "{status}");
}

/// Not a test of its own: `use_key_marks_one_key_turn...` runs this binary
/// again as a child carrying a turn's mark, standing in for the harness
/// process (macOS hides the environment of its own shells).
#[test]
fn use_key_turn_child_helper() {
    let Ok(script) = std::env::var("MDKG_TEST_TURN_CHILD") else {
        return;
    };
    let status = Command::new("/bin/bash")
        .arg("-c")
        .arg(&script)
        .status()
        .expect("bash");
    std::process::exit(status.code().unwrap_or(1));
}

#[test]
fn two_capacity_failures_in_a_row_start_the_next_turn_fresh() {
    use std::os::unix::fs::PermissionsExt;
    let (scratch, repo) = make_repo(json!({}));
    let root = graph(&repo);
    ok(
        &root,
        "launcher",
        &[
            "agents",
            "add",
            "rev",
            "--harness",
            "claude",
            "--role",
            "reviewer",
        ],
    );
    // Claude turned away at capacity whenever it resumes a session, once
    // `capacity-on` exists; a fresh session runs.
    let dir = scratch.0.display().to_string();
    let claude = scratch.0.join("capacity-claude");
    fs::write(
        &claude,
        format!(
            "#!/bin/sh\nfor a in \"$@\"; do\n  if [ \"$a\" = --resume ] && [ -e '{dir}/capacity-on' ]; then\n    echo resumed >> '{dir}/capacity-log'\n    echo 'ERROR: Selected model is at capacity.' >&2\n    exit 1\n  fi\ndone\nexec '{fake}' \"$@\"\n",
            fake = fixture("fake-claude.sh").display()
        ),
    )
    .expect("wrapper");
    fs::set_permissions(&claude, fs::Permissions::from_mode(0o755)).expect("mode");
    let mdkg = Path::new(env!("CARGO_BIN_EXE_mdkg"));
    let mut watch = Watching::start(&scratch.0, &root, mdkg, &claude, &["--json"]);
    let turns_seen = |w: &Watching| {
        fs::read_to_string(w.stderr.with_file_name("watch.out"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|e| e["data"]["agent"] == "rev" && e["data"]["event"] == "turn")
            .count()
    };
    // A first turn makes the session the next ones resume.
    send(&repo, "rev", "first", None);
    watch.wait_for("first turn", |w| turns_seen(w) == 1);
    let session = read_state_session(&repo, "rev");
    assert!(session.is_some());
    fs::write(scratch.0.join("capacity-on"), "").expect("capacity on");
    for (n, text) in [(2, "second"), (3, "third")] {
        send(&repo, "rev", text, None);
        watch.wait_for(text, |w| turns_seen(w) == n);
    }
    assert_eq!(
        fs::read_to_string(scratch.0.join("capacity-log")).unwrap_or_default(),
        "resumed\nresumed\n",
        "{}",
        watch.err()
    );
    // Two in a row: the next turn starts fresh, and runs.
    send(&repo, "rev", "fourth", None);
    watch.wait_for("fresh turn", |w| turns_seen(w) == 4);
    assert_eq!(
        fs::read_to_string(scratch.0.join("capacity-log")).unwrap_or_default(),
        "resumed\nresumed\n",
        "no third resume"
    );
    let out = fs::read_to_string(scratch.0.join("watch.out")).expect("out");
    let last = out
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .rfind(|e| e["data"]["event"] == "turn")
        .expect("turn");
    assert_eq!(last["ok"], true, "{out}");
    assert_ne!(read_state_session(&repo, "rev"), session, "a new session");
}

#[test]
fn a_replaced_binary_drains_and_re_execs_the_new_one() {
    let (scratch, repo) = make_repo(json!({}));
    let root = graph(&repo);
    ok(
        &root,
        "launcher",
        &[
            "agents",
            "add",
            "rev",
            "--harness",
            "claude",
            "--role",
            "reviewer",
        ],
    );
    // Never the build on PATH: a copy in the fixture, replaced as an install
    // replaces one (a new file renamed over it).
    let bin = scratch.0.join("bin");
    fs::create_dir(&bin).unwrap();
    let program = bin.join("mdkg");
    fs::copy(env!("CARGO_BIN_EXE_mdkg"), &program).unwrap();
    let claude = fixture("fake-claude.sh");
    let mut watch = Watching::start(
        &scratch.0,
        &root,
        &program,
        &claude,
        &["--max-turns", "1", "--json"],
    );
    watch.wait_for("watch start", |_| progress_file(&repo, "rev").exists());
    let before = read_progress(&repo, "rev");
    let fresh = bin.join("mdkg.new");
    fs::copy(env!("CARGO_BIN_EXE_mdkg"), &fresh).unwrap();
    fs::rename(&fresh, &program).unwrap();
    watch.wait_for("restart", |w| {
        w.err()
            .contains("the mdkg binary was replaced; turns finished, restarting")
    });
    send(&repo, "rev", "after the install", None);
    let status = watch.exit();
    assert!(status.success(), "{}", watch.err());
    let after = read_progress(&repo, "rev");
    assert_eq!(
        after["watch_id"], before["watch_id"],
        "one watch across the exec"
    );
    assert_eq!(after["progress"]["ran"], 1, "{after}");
    assert_eq!(after["finished"], true);
    let out = fs::read_to_string(scratch.0.join("watch.out")).unwrap();
    let events: Vec<Value> = out
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(turns(&events, "rev").len(), 1, "{out}");
}

/// Sets `field` in `rev`'s definition in the worktree, without committing.
fn edit_definition(root: &Path, field: &str, value: Value) {
    let path = root.join(".mdkg/agents/rev.json");
    let mut definition: Value =
        serde_json::from_slice(&fs::read(&path).expect("definition")).expect("JSON");
    definition[field] = value;
    fs::write(&path, definition.to_string()).expect("edit definition");
}

/// A checkout of its own branch, from the graph's commits, as a lane works
/// in: unlike the graph checkout, it may hold uncommitted edits.
fn lane(repo: &Path) -> PathBuf {
    let lane = repo.join(".worktrees/lane");
    git_text(
        repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "lane",
            &lane.to_string_lossy(),
            "mdkg-graph",
        ],
    );
    lane
}

fn commit_all(root: &Path, message: &str) {
    git_text(
        root,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.invalid",
            "commit",
            "-qam",
            message,
        ],
    );
}

#[test]
fn a_re_exec_runs_the_committed_definition_not_an_uncommitted_edit() {
    let (scratch, repo) = make_repo(json!({}));
    ok(
        &graph(&repo),
        "launcher",
        &[
            "agents",
            "add",
            "rev",
            "--harness",
            "claude",
            "--role",
            "reviewer",
            "--instructions",
            "COMMITTED",
        ],
    );
    let bin = scratch.0.join("bin");
    fs::create_dir(&bin).expect("bin");
    let program = bin.join("mdkg");
    fs::copy(env!("CARGO_BIN_EXE_mdkg"), &program).expect("copy mdkg");
    let root = lane(&repo);
    let mut watch = Watching::start(
        &scratch.0,
        &root,
        &program,
        &fixture("fake-claude.sh"),
        &["--json", "--max-turns", "1"],
    );
    watch.wait_for("watch start", |_| progress_file(&repo, "rev").exists());
    // Edited but not committed, then an install restarts the watch.
    edit_definition(&root, "instructions", json!("UNCOMMITTED"));
    let fresh = bin.join("mdkg.new");
    fs::copy(env!("CARGO_BIN_EXE_mdkg"), &fresh).expect("copy mdkg");
    fs::rename(&fresh, &program).expect("replace mdkg");
    watch.wait_for("warning", |w| {
        w.err()
            .contains(".mdkg/agents/rev.json in this worktree differs from the graph authority's")
            && w.err()
                .contains("this restarted watch runs the authority's definitions")
    });
    // The restarted watch keeps going, on the committed definition.
    send(&repo, "rev", "after the restart", None);
    let status = watch.exit();
    assert!(status.success(), "{}", watch.err());
    let prompt = [
        root.join("claude.prompt"),
        graph(&repo).join("claude.prompt"),
    ]
    .iter()
    .find_map(|p| fs::read_to_string(p).ok())
    .expect("a turn ran");
    assert!(prompt.contains("COMMITTED"), "{prompt}");
    assert!(!prompt.contains("UNCOMMITTED"), "{prompt}");
    let progress = read_progress(&repo, "rev");
    assert_eq!(progress["finished"], true, "{progress}");
    assert_eq!(progress["progress"]["ran"], 1, "{progress}");
}

#[test]
fn a_committed_definition_the_re_exec_refuses_leaves_the_watch_finished() {
    let (scratch, repo) = make_repo(json!({}));
    ok(
        &graph(&repo),
        "launcher",
        &[
            "agents",
            "add",
            "rev",
            "--harness",
            "claude",
            "--role",
            "reviewer",
        ],
    );
    let mdkg = Path::new(env!("CARGO_BIN_EXE_mdkg"));
    let root = lane(&repo);
    let mut watch = Watching::start(
        &scratch.0,
        &root,
        mdkg,
        &fixture("fake-claude.sh"),
        &["--json"],
    );
    watch.wait_for("watch start", |_| progress_file(&repo, "rev").exists());
    // Every command still parses it, so the running watch keeps going;
    // only `agents up` refuses it, after the restart. Definitions come only
    // from the graph authority (R1), so the change is committed there.
    edit_definition(
        &graph(&repo),
        "policy",
        json!({"trigger": {"kind": "schedule", "cron": "0 * * * *"}}),
    );
    commit_all(&graph(&repo), "a policy this build doesn't run");
    let status = watch.exit();
    assert!(!status.success(), "{}", watch.err());
    assert!(watch.err().contains("restarting"), "{}", watch.err());
    assert!(watch.err().contains("trigger schedule"), "{}", watch.err());
    let progress = read_progress(&repo, "rev");
    assert_eq!(progress["finished"], true, "{progress}");
    // The re-exec failed before it registered: its failure is still the
    // reason, under the supervisor's id (#20921).
    let exit = last_exit(&root, "rev");
    assert_eq!(exit["reason"], "failed", "{exit}");
    assert!(
        exit["code"].as_str().is_some_and(|c| !c.is_empty()),
        "{exit}"
    );
    assert!(uuid::Uuid::parse_str(exit["supervisor_id"].as_str().unwrap()).is_ok());
    // Fixed and started by hand: no resume needed.
    git_text(
        &graph(&repo),
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.invalid",
            "revert",
            "--no-edit",
            "HEAD",
        ],
    );
    send(&repo, "rev", "after the fix", None);
    let (code, out) = mdkg_out(&root, "launcher", &["agents", "up", "--once", "rev"]);
    assert_eq!(code, 0, "{out}");
}

#[test]
fn the_watcher_sees_a_store_a_newer_build_moved_and_not_its_own() {
    use mdkg_run::supervise::reload::{Definitions, Reason, Watch};
    let (_scratch, repo) = make_repo(json!({}));
    let root = graph(&repo);
    let store = repo.join(".git/mdkg/coord.sqlite");
    let authority = mdkg_run::orchestration::authority(&root).expect("authority");
    let definitions = Definitions::new(&root, &root, "", &[], &authority);
    let watch = Watch::new(definitions, None, Some(store.clone()));
    // The store as this build left it isn't a reason to restart.
    assert_eq!(watch.changed(), None);
    rusqlite::Connection::open(&store)
        .expect("store")
        .execute("UPDATE meta SET value = '99' WHERE key = 'schema'", [])
        .expect("bump schema");
    match watch.changed() {
        Some(Reason::Store(label)) => assert!(label.starts_with("99"), "{label}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_store_moved_under_the_same_binary_stops_once_with_what_to_do() {
    let (scratch, repo) = make_repo(json!({}));
    let root = graph(&repo);
    ok(
        &root,
        "launcher",
        &[
            "agents",
            "add",
            "rev",
            "--harness",
            "claude",
            "--role",
            "reviewer",
        ],
    );
    let mdkg = Path::new(env!("CARGO_BIN_EXE_mdkg"));
    let mut watch = Watching::start(
        &scratch.0,
        &root,
        mdkg,
        &fixture("fake-claude.sh"),
        &["--json"],
    );
    watch.wait_for("watch start", |_| progress_file(&repo, "rev").exists());
    // What a newer build's upgrade leaves: versions this build doesn't use.
    let store = repo.join(".git/mdkg/coord.sqlite");
    rusqlite::Connection::open(&store)
        .unwrap()
        .execute("UPDATE meta SET value = '99' WHERE key = 'schema'", [])
        .unwrap();
    let status = watch.exit();
    assert!(!status.success());
    // Whichever sees it first, the watcher or a worker's own store call,
    // the watch stops once, saying a newer build has the store. The
    // watcher's part is pinned down by
    // the_watcher_sees_a_store_a_newer_build_moved_and_not_its_own.
    let out = fs::read_to_string(scratch.0.join("watch.out")).unwrap();
    let last: Value = serde_json::from_str(out.lines().last().unwrap()).unwrap();
    let diagnostic = &last["diagnostics"][0];
    assert_eq!(diagnostic["code"], "coord.store_failed", "{out}");
    assert!(
        diagnostic["hint"]
            .as_str()
            .unwrap()
            .contains("a newer mdkg upgraded"),
        "{out}"
    );
    assert!(!watch.err().contains("restarting"), "{}", watch.err());
}

// API v1 conformance (A22), the CLI drivers of cases 10, 12, 14, 15, 16
// and 19. Their case files are conformance/api-v1/cases/; case 9's driver
// is in mdkg-run's turn/live.rs, case 11's are the wrapper tests above.

/// Whether a lease on `scope` is active, from everyone's in `status`.
fn lease_active(root: &Path, scope: &str) -> bool {
    let out = ok(root, "launcher", &["status"]);
    out["data"]["leases"]
        .as_array()
        .expect("leases")
        .iter()
        .any(|l| l["scope"] == scope && l["status"] == "active")
}

/// `agents up <agent> --once --json` with `claude` as the harness and a
/// home inside the scratch: exit code and stdout.
fn up_with(scratch: &Path, root: &Path, agent: &str, claude: &Path) -> (i32, String) {
    let home = scratch.join("home");
    fs::create_dir_all(home.join(".codex")).expect("home");
    let out = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
        .current_dir(root)
        .arg("--root")
        .arg(root)
        .args([
            "--as", "launcher", "agents", "up", agent, "--once", "--json",
        ])
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("CODEX_HOME", home.join(".codex"))
        .env("MDKG_RUN_CLAUDE", claude)
        .output()
        .expect("run agents up");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

fn add_reviewer(root: &Path, name: &str) {
    ok(
        root,
        "launcher",
        &[
            "agents",
            "add",
            name,
            "--harness",
            "claude",
            "--role",
            "reviewer",
        ],
    );
}

/// The frames of an agent follow up to and including the `n`th
/// `turn.ended`, without `agent.state` frames.
fn turns_until(follow: &Follower, n: usize) -> Vec<Value> {
    let mut got = Vec::new();
    for _ in 0..n {
        got.extend(follow.until("turn.ended"));
    }
    got.retain(|e| e["type"] != "agent.state");
    got
}

/// Whether `seqs` rise strictly from 1: a filtered stream skips the
/// `native` events' seqs and never renumbers (A11).
fn rising_from_one(seqs: &[u64]) -> bool {
    seqs.first() == Some(&1) && seqs.windows(2).all(|w| w[0] < w[1])
}

/// The turn numbers of a follow's `turn.started` frames, in order.
fn started_numbers(events: &[Value]) -> Vec<u64> {
    events
        .iter()
        .filter(|e| e["type"] == "turn.started")
        .map(|e| e["data"]["number"].as_u64().expect("number"))
        .collect()
}

#[test]
fn conformance_10_a_drain_lets_the_turn_finish_records_no_interruption_and_holds_nothing() {
    let (scratch, repo) = make_repo(json!({}));
    let root = graph(&repo);
    add_reviewer(&root, "rev");
    send(&repo, "rev", "a held request", None);
    let claude = held_claude(&scratch.0);
    let mdkg = Path::new(env!("CARGO_BIN_EXE_mdkg"));
    let mut watch = Watching::start(&scratch.0, &root, mdkg, &claude, &[]);
    watch.wait_for("turn start", |_| scratch.0.join("started").exists());
    assert!(lease_active(&root, "run:rev"));
    // A wait shorter than the turn: draining, exit 6, nothing killed.
    let (code, out) = stop(&root, &["rev", "--drain", "--wait", "1"]);
    assert_eq!(code, 6, "{out}");
    assert_eq!(out["diagnostics"][0]["code"], "msg.wait_timeout", "{out}");
    assert_eq!(out["data"]["agents"][0]["state"], "draining", "{out}");
    assert!(watch.child.try_wait().unwrap().is_none(), "{}", watch.err());
    // A7: the agent reads draining while its turn still runs.
    let (_, rev) = status_agent(&root, "rev");
    assert_eq!(rev["state"], "draining", "{rev}");
    assert!(rev["current_turn"].is_object(), "{rev}");
    fs::write(scratch.0.join("release"), "").unwrap();
    assert!(watch.exit().success(), "{}", watch.err());
    // The turn finished as it would have: replied, never interrupted.
    let (_, rev) = status_agent(&root, "rev");
    assert_eq!(rev["state"], "stopped", "{rev}");
    assert_eq!(rev["last_turn"]["outcome"], "replied", "{rev}");
    let lines = index_lines(&repo, "rev");
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(lines[1]["outcome"], "replied", "{lines:?}");
    assert!(!lease_active(&root, "run:rev"));
    assert!(!repo.join(".git/mdkg/run/stop/rev.json").exists());
}

#[test]
fn conformance_12_a_client_re_attaches_from_agents_status() {
    use std::io::BufRead as _;
    let (scratch, repo) = make_repo(json!({}));
    let root = graph(&repo);
    add_reviewer(&root, "rev");
    send(&repo, "rev", "a held request", None);
    let claude = streaming_claude(&scratch.0);
    let mdkg = Path::new(env!("CARGO_BIN_EXE_mdkg"));
    let mut watch = Watching::start(&scratch.0, &root, mdkg, &claude, &[]);
    watch.wait_for("turn start", |_| scratch.0.join("started").exists());
    // A client that knows nothing but the agent's name.
    let (out, rev) = status_agent(&root, "rev");
    let cursor = out["data"]["event_cursor"].as_str().expect("event cursor");
    let from = rev["follow_from"].as_str().expect("follow_from");
    let turn_id = rev["current_turn"]["turn_id"].as_str().expect("turn id");
    assert_eq!(
        from,
        format!("{turn_id}:{}", rev["current_turn"]["output_after"]),
        "{rev}"
    );
    let mut events = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
        .current_dir(&root)
        .arg("--root")
        .arg(&root)
        .args([
            "--as",
            "launcher",
            "msg",
            "watch",
            "--after",
            cursor,
            "--types",
            "mdkg.message.sent",
            "--json",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("msg watch");
    let stdout = events.stdout.take().expect("watch stdout");
    let (tx, sent) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
        {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let follow = Follower::start(&root, &["rev", "--follow", "--after", from]);
    fs::write(scratch.0.join("release"), "").unwrap();
    let output = turns_until(&follow, 1);
    // Picks up after what status had seen: no turn.started again.
    assert_ne!(output[0]["type"], "turn.started", "{output:?}");
    assert!(
        output
            .iter()
            .all(|e| e["turn"] == format!("/v1/agents/rev/turns/{turn_id}")),
        "{output:?}"
    );
    let end = output.last().expect("end");
    assert_eq!(end["data"]["outcome"], "replied", "{output:?}");
    // The turn's reply, on the event stream after the status read.
    let reply = loop {
        let line = sent
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("the reply's event within a minute");
        let event: Value = serde_json::from_str(&line).expect("an event");
        if event["type"] == "mdkg.message.sent"
            && event["data"]["message"]["sender"]
                .as_str()
                .is_some_and(|s| s.ends_with("rev"))
        {
            break event;
        }
    };
    assert!(reply["seq"].is_u64() || reply["id"].is_string(), "{reply}");
    events.kill().ok();
    events.wait().ok();
    assert_eq!(follow.stop(), 0);
    let (code, _) = stop(&root, &["rev", "--drain", "--wait", "30"]);
    assert_eq!(code, 0);
    assert!(watch.exit().success(), "{}", watch.err());
}

#[test]
fn conformance_14_re_attach_across_turns_and_before_the_first() {
    let (scratch, repo) = make_repo(json!({}));
    let root = graph(&repo);
    add_reviewer(&root, "rev");
    let claude = fixture("fake-claude.sh");
    // Never run: the first turn starts and ends before the follow attaches.
    let (_, rev) = status_agent(&root, "rev");
    let from = rev["follow_from"]
        .as_str()
        .expect("follow_from")
        .to_string();
    assert_eq!(from, "turn:1");
    send(&repo, "rev", "first", None);
    let (code, out) = up_with(&scratch.0, &root, "rev", &claude);
    assert_eq!(code, 0, "{out}");
    let follow = Follower::start(&root, &["rev", "--follow", "--after", &from]);
    let first = follow.next();
    assert_eq!(first["type"], "agent.state", "{first}");
    let one = turns_until(&follow, 1);
    assert_eq!(started_numbers(&one), [1], "{one:?}");
    assert_eq!(one[0]["type"], "turn.started", "{one:?}");
    send(&repo, "rev", "second", None);
    let (code, out) = up_with(&scratch.0, &root, "rev", &claude);
    assert_eq!(code, 0, "{out}");
    let two = turns_until(&follow, 1);
    assert_eq!(started_numbers(&two), [2], "{two:?}");
    assert_eq!(follow.stop(), 0);
    // Idle: a turn starts between the status read and the follow, and a
    // later one after it attaches. Both, in number order, no gap.
    let (_, rev) = status_agent(&root, "rev");
    assert_eq!(rev["state"], "stopped", "{rev}");
    let from = rev["follow_from"]
        .as_str()
        .expect("follow_from")
        .to_string();
    assert!(from.ends_with(":end"), "{rev}");
    send(&repo, "rev", "third", None);
    let (code, out) = up_with(&scratch.0, &root, "rev", &claude);
    assert_eq!(code, 0, "{out}");
    let follow = Follower::start(&root, &["rev", "--follow", "--after", &from]);
    let first = follow.next();
    assert_eq!(first["type"], "agent.state", "{first}");
    let three = turns_until(&follow, 1);
    send(&repo, "rev", "fourth", None);
    let (code, out) = up_with(&scratch.0, &root, "rev", &claude);
    assert_eq!(code, 0, "{out}");
    let four = turns_until(&follow, 1);
    let both: Vec<Value> = three.into_iter().chain(four).collect();
    assert_eq!(started_numbers(&both), [3, 4], "{both:?}");
    // Each turn whole: from its turn.started, seq 1 upward, to its end,
    // with gaps only where native events were filtered out.
    for turn in [3, 4] {
        let id = both
            .iter()
            .find(|e| e["type"] == "turn.started" && e["data"]["number"] == turn)
            .expect("started")["turn"]
            .clone();
        let frames: Vec<&Value> = both.iter().filter(|e| e["turn"] == id).collect();
        let seqs: Vec<u64> = frames
            .iter()
            .map(|e| e["seq"].as_u64().expect("seq"))
            .collect();
        assert!(rising_from_one(&seqs), "{seqs:?} {both:?}");
        assert_eq!(
            frames.last().expect("frames")["type"],
            "turn.ended",
            "{both:?}"
        );
    }
    assert_eq!(follow.stop(), 0);
}

/// A Claude stand-in that alternates stdout text events with stderr
/// lines, four of each, then finishes as the fake.
fn interleaving_claude(scratch: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let script = scratch.join("interleaving-claude");
    let mut body = format!(
        "#!/bin/sh\n[ \"$1\" = auth ] && exec '{fake}' \"$@\"\n",
        fake = fixture("fake-claude.sh").display()
    );
    for i in 1..=4 {
        body.push_str(&format!(
            "echo '{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"out-{i}\"}}]}}}}'\n\
             echo 'err-{i}' >&2\n"
        ));
    }
    body.push_str(&format!(
        "exec '{fake}' \"$@\"\n",
        fake = fixture("fake-claude.sh").display()
    ));
    fs::write(&script, body).expect("interleaving claude");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("mode");
    script
}

#[test]
fn conformance_15_every_reader_sees_the_same_events_and_seq() {
    let (scratch, repo) = make_repo(json!({}));
    let root = graph(&repo);
    add_reviewer(&root, "rev");
    let claude = interleaving_claude(&scratch.0);
    // Two readers attached before the turn.
    let a = Follower::start(&root, &["rev", "--follow", "--after", "turn:1"]);
    let b = Follower::start(&root, &["rev", "--follow", "--after", "turn:1"]);
    send(&repo, "rev", "a request", None);
    let (code, out) = up_with(&scratch.0, &root, "rev", &claude);
    assert_eq!(code, 0, "{out}");
    let seen_a = turns_until(&a, 1);
    let seen_b = turns_until(&b, 1);
    assert_eq!(seen_a, seen_b);
    let seqs: Vec<u64> = seen_a
        .iter()
        .map(|e| e["seq"].as_u64().expect("seq"))
        .collect();
    assert!(rising_from_one(&seqs), "{seqs:?}");
    // Within each pipe, the order it was written in.
    let texts = |kind: &str, prefix: &str| -> Vec<String> {
        seen_a
            .iter()
            .filter(|e| e["type"] == kind)
            .filter_map(|e| e["data"]["text"].as_str())
            .filter(|t| t.starts_with(prefix))
            .map(str::to_string)
            .collect()
    };
    assert_eq!(texts("text", "out-"), ["out-1", "out-2", "out-3", "out-4"]);
    assert_eq!(
        texts("status", "err-"),
        ["err-1", "err-2", "err-3", "err-4"]
    );
    assert_eq!(a.stop(), 0);
    assert_eq!(b.stop(), 0);
    // A reader that reconnects after an event gets the rest, identical.
    let id = seen_a[0]["data"]["turn_id"].as_str().expect("turn id");
    let cut = 3;
    let again = Follower::start(
        &root,
        &[
            "rev",
            "--follow",
            "--after",
            &format!("{id}:{}", seqs[cut - 1]),
        ],
    );
    let rest = turns_until(&again, 1);
    assert_eq!(rest, seen_a[cut..]);
    assert_eq!(again.stop(), 0);
    // --native adds events and changes no seq.
    let frames = |extra: &[&str]| -> Vec<Value> {
        let mut args = vec!["agents", "logs", "rev", "--turn", id];
        args.extend_from_slice(extra);
        let (code, out) = mdkg_out(&root, "launcher", &args);
        assert_eq!(code, 0, "{out}");
        out.lines()
            .map(|l| serde_json::from_str(l).expect("frame"))
            .collect()
    };
    let plain = frames(&[]);
    assert_eq!(plain, seen_a);
    let native = frames(&["--native"]);
    assert!(native.len() > plain.len(), "{native:?}");
    let shared: Vec<Value> = native
        .into_iter()
        .filter(|e| e["type"] != "native")
        .collect();
    assert_eq!(shared, plain);
}

#[test]
fn conformance_16_and_19_retention_and_recovery_from_the_turn_index() {
    let (scratch, repo) = make_repo(json!({}));
    let root = graph(&repo);
    add_reviewer(&root, "rev");
    // 19: a crash right after the spawn reads interrupted, never running.
    send(&repo, "rev", "a held request", None);
    let claude = held_claude(&scratch.0);
    let bin = Path::new(env!("CARGO_BIN_EXE_mdkg"));
    let mut watch = Watching::start(&scratch.0, &root, bin, &claude, &[]);
    watch.wait_for("turn start", |_| scratch.0.join("started").exists());
    watch.signal_group("-KILL");
    let _ = watch.exit();
    let (_, rev) = status_agent(&root, "rev");
    assert_eq!(rev["last_turn"]["outcome"], "interrupted", "{rev}");
    let id1 = rev["last_turn"]["turn_id"]
        .as_str()
        .expect("turn id")
        .to_string();
    assert_eq!(index_lines(&repo, "rev").len(), 1, "no end line yet");
    // An agent follow sends the derived end.
    let follow = Follower::start(&root, &["rev", "--follow", "--after", "turn:1"]);
    let crashed = turns_until(&follow, 1);
    assert_eq!(crashed[0]["data"]["turn_id"], id1.as_str(), "{crashed:?}");
    let end = crashed.last().expect("end");
    assert_eq!(end["data"]["outcome"], "interrupted", "{crashed:?}");
    assert_eq!(follow.stop(), 0);
    let journal = repo.join(format!(".git/mdkg/run/rev/journals/{id1}.events.jsonl"));
    let kept = fs::read(&journal).expect("turn 1's journal");
    // A torn tail: a line the crash cut before its newline.
    let index = repo.join(".git/mdkg/run/rev/turns.jsonl");
    let mut torn = fs::OpenOptions::new().append(true).open(&index).unwrap();
    std::io::Write::write_all(&mut torn, br#"{"line":"start","turn_id":"torn","number":7"#)
        .unwrap();
    drop(torn);
    // Resume, then the next turn closes turn 1 without running it again,
    // as its own attempt, number 2, with its own journal.
    ok(&root, "launcher", &["agents", "resume", "rev"]);
    fs::remove_file(scratch.0.join("started")).ok();
    let fake = fixture("fake-claude.sh");
    let (code, out) = up_with(&scratch.0, &root, "rev", &fake);
    assert_eq!(code, 0, "{out}");
    let lines = index_lines(&repo, "rev");
    assert!(lines.iter().all(|l| l["turn_id"] != "torn"), "{lines:?}");
    let ends: Vec<&Value> = lines.iter().filter(|l| l["line"] == "end").collect();
    assert_eq!(ends[0]["turn_id"], id1.as_str(), "{lines:?}");
    assert_eq!(ends[0]["outcome"], "interrupted", "{lines:?}");
    let starts: Vec<u64> = lines
        .iter()
        .filter(|l| l["line"] == "start")
        .map(|l| l["number"].as_u64().unwrap())
        .collect();
    assert_eq!(starts, [1, 2], "{lines:?}");
    assert_eq!(
        fs::read(&journal).unwrap(),
        kept,
        "turn 1's journal changed"
    );
    // 16: a reader attaching just after a successful turn ends gets it.
    let (_, rev) = status_agent(&root, "rev");
    assert_eq!(rev["last_turn"]["number"], 2, "{rev}");
    assert_eq!(rev["last_turn"]["outcome"], "replied", "{rev}");
    let id2 = rev["last_turn"]["turn_id"].as_str().unwrap().to_string();
    let (code, out) = mdkg_out(
        &root,
        "launcher",
        &["agents", "logs", "rev", "--turn", &id2],
    );
    assert_eq!(code, 0, "{out}");
    let last: Value = serde_json::from_str(out.lines().last().expect("frames")).unwrap();
    assert_eq!(last["type"], "turn.ended", "{out}");
    assert_eq!(last["data"]["outcome"], "replied", "{out}");
    // Two more turns: the first is older than the last 3, so its output is
    // gone, and it still reads as ended with its outcome.
    for request in ["third", "fourth"] {
        send(&repo, "rev", request, None);
        let (code, out) = up_with(&scratch.0, &root, "rev", &fake);
        assert_eq!(code, 0, "{out}");
    }
    assert!(!journal.exists(), "turn 1's journal outlived retention");
    let (code, out) = mdkg(
        &root,
        "launcher",
        &["agents", "logs", "rev", "--turn", &id1],
    );
    assert_eq!(code, 3, "{out}");
    assert_eq!(out["diagnostics"][0]["code"], "api.output_gone", "{out}");
    let follow = Follower::start(&root, &["rev", "--follow", "--after", "turn:1"]);
    let all = turns_until(&follow, 4);
    assert_eq!(started_numbers(&all), [1, 2, 3, 4], "{all:?}");
    let gone = all
        .iter()
        .find(|e| e["type"] == "turn.ended" && e["data"]["output_gone"] == true)
        .expect("turn 1 as gone");
    assert_eq!(gone["data"]["outcome"], "interrupted", "{all:?}");
    assert_eq!(follow.stop(), 0);
}

/// A legacy inline entry with a bad field doesn't stop `edit --file`
/// replacing it, and `rm` on an inline-only agent says where to remove
/// its entry, without offering `edit`, which leaves it (review of
/// 73189c1d).
#[test]
fn an_inline_entry_is_replaced_by_file_and_rm_names_where_it_lives() {
    let (_scratch, repo) = make_repo(json!({
        "legacy": {"harness": "claude", "role": "worker", "cwd": 7},
        "inline": {"harness": "claude", "role": "worker"},
    }));
    let root = graph(&repo);
    let file = repo.join("legacy.json");
    fs::write(
        &file,
        json!({"schema":"mdkg.agent.v1", "harness":"claude", "role":"worker"}).to_string(),
    )
    .expect("file");
    let file = file.to_str().expect("utf-8").to_string();
    let out = ok(
        &root,
        "launcher",
        &["agents", "edit", "legacy", "--file", &file],
    );
    assert_eq!(out["ok"], true, "{out}");
    let (code, out) = mdkg(&root, "launcher", &["agents", "rm", "inline"]);
    assert_eq!(code, 3, "{out}");
    let hint = out["diagnostics"][0]["hint"].as_str().unwrap_or_default();
    assert!(hint.contains("remove agents.inline from"), "{out}");
    assert!(!hint.contains("agents edit"), "{out}");
}

/// Text `agents status` prints an agent's effective settings one field a
/// line, nested objects indented under their field, so no value reads as
/// another field's (Opus final on a6ba7012).
#[test]
fn agents_status_text_nests_effective_settings() {
    let (_dir, repo) = make_repo(json!({}));
    let root = graph(&repo);
    ok(
        &root,
        "launcher",
        &[
            "agents",
            "add",
            "rev",
            "--harness",
            "claude",
            "--role",
            "reviewer",
        ],
    );
    let out = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
        .current_dir(&root)
        .args(["--as", "launcher", "agents", "status"])
        .output()
        .expect("status");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}");
    let lines: Vec<&str> = text.lines().collect();
    let at = lines
        .iter()
        .position(|l| *l == "  effective:")
        .unwrap_or_else(|| panic!("no effective block: {text}"));
    // Its fields follow one indent deeper, and none is a flattened
    // `key=value` run.
    let block: Vec<&str> = lines[at + 1..]
        .iter()
        .take_while(|l| l.starts_with("    "))
        .copied()
        .collect();
    assert!(!block.is_empty(), "{text}");
    assert!(
        block.iter().any(|l| l.trim_start() == "harness: claude"),
        "{text}"
    );
    assert!(block.iter().all(|l| !l.contains("=layer=")), "{text}");
}

/// `agents turns` lists an agent's turns newest first, a page at a time,
/// or one by number, with each turn's derived outcome and its receipt;
/// `agents logs` lines carry the position `--after` resumes from.
#[test]
fn agents_turns_pages_newest_first_and_logs_lines_carry_their_position() {
    use std::io::Write as _;
    let (scratch, repo) = make_repo(json!({}));
    let root = graph(&repo);
    ok(
        &root,
        "launcher",
        &[
            "agents",
            "add",
            "cx",
            "--harness",
            "codex",
            "--role",
            "reviewer",
        ],
    );
    send(&repo, "cx", "a request", None);
    let (code, events, err) = up_harnesses(&scratch.0, &root, "cx");
    assert_eq!(code, 0, "{events:?} {err}");
    // Three more ended turns, as the index records them.
    let index = repo.join(".git/mdkg/run/cx/turns.jsonl");
    // ids[n] is turn n's (turn 1 is the real one, read below).
    let mut ids = vec![String::new(), String::new()];
    for n in 2..=4u64 {
        let id = uuid::Uuid::now_v7().to_string();
        let mut f = fs::OpenOptions::new()
            .append(true)
            .open(&index)
            .expect("index");
        for line in [
            json!({"line": "start", "turn_id": id, "number": n, "files": format!("turn-{n}"),
                "started_at": format!("2026-10-30T00:00:0{n}Z"), "supervisor_id": null,
                "host": "elsewhere", "pid": 1, "pid_started_at": "never"}),
            json!({"line": "end", "turn_id": id, "ended_at": format!("2026-10-30T00:01:0{n}Z"),
                "outcome": "idle"}),
        ] {
            writeln!(f, "{line}").expect("append");
        }
        ids.push(id);
    }
    // Not `mdkg`: its schema is checked against the generated one, which
    // the wave writes (schemas/ isn't committed on a lane branch).
    let mdkg = |root: &Path, actor: &str, args: &[&str]| -> (i32, Value) {
        let (code, out) = mdkg_out(root, actor, args);
        (
            code,
            serde_json::from_str(&out).unwrap_or_else(|_| panic!("not JSON: {out}")),
        )
    };
    let numbers = |out: &Value| -> Vec<u64> {
        out["data"]["turns"]
            .as_array()
            .expect("turns")
            .iter()
            .map(|t| t["number"].as_u64().expect("number"))
            .collect()
    };
    let (code, all) = mdkg(&root, "launcher", &["agents", "turns", "cx"]);
    assert_eq!(code, 0, "{all}");
    assert_eq!(all["schema"], "mdkg.agents.turns.v1", "{all}");
    assert_eq!(numbers(&all), [4, 3, 2, 1], "{all}");
    assert_eq!(all["data"]["next_before"], Value::Null, "{all}");
    let (_, page) = mdkg(
        &root,
        "launcher",
        &["agents", "turns", "cx", "--limit", "2"],
    );
    assert_eq!(numbers(&page), [4, 3], "{page}");
    assert_eq!(page["data"]["next_before"], 3, "{page}");
    let (_, page) = mdkg(
        &root,
        "launcher",
        &["agents", "turns", "cx", "--limit", "2", "--before", "3"],
    );
    assert_eq!(numbers(&page), [2, 1], "{page}");
    assert_eq!(page["data"]["next_before"], Value::Null, "{page}");
    let three = &all["data"]["turns"][1];
    assert_eq!(three["turn_id"], ids[3].as_str(), "{three}");
    assert_eq!(three["started"], "2026-10-30T00:00:03Z", "{three}");
    assert_eq!(three["ended"], "2026-10-30T00:01:03Z", "{three}");
    assert_eq!(
        (&three["state"], &three["outcome"], &three["receipt"]),
        (&json!("ended"), &json!("idle"), &Value::Null),
        "{three}"
    );
    // The real turn: its receipt is there, and its outcome is the receipt's.
    let (code, one) = mdkg(&root, "launcher", &["agents", "turns", "cx", "--turn", "1"]);
    assert_eq!(code, 0, "{one}");
    let first = &one["data"]["turns"][0];
    assert_eq!(numbers(&one), [1], "{one}");
    assert_eq!(first["state"], "ended", "{first}");
    assert!(first["outcome"].is_string(), "{first}");
    assert!(first["ended"].is_string(), "{first}");
    let receipt = first["receipt"].as_str().expect("a receipt");
    assert!(Path::new(receipt).is_file(), "{first}");
    // Refusals: no such turn, a bad limit, no such agent.
    let (code, none) = mdkg(&root, "launcher", &["agents", "turns", "cx", "--turn", "9"]);
    assert_ne!(code, 0, "{none}");
    assert_eq!(
        none["diagnostics"][0]["code"], "api.unknown_resource",
        "{none}"
    );
    let (code, bad) = mdkg(
        &root,
        "launcher",
        &["agents", "turns", "cx", "--limit", "0"],
    );
    assert_ne!(code, 0, "{bad}");
    assert_eq!(bad["diagnostics"][0]["code"], "usage.bad_value", "{bad}");
    let (code, who) = mdkg(&root, "launcher", &["agents", "turns", "nobody"]);
    assert_ne!(code, 0, "{who}");
    assert_eq!(who["diagnostics"][0]["code"], "run.unknown_agent", "{who}");
    // Each logs line's position resumes after that line.
    let turn_id = first["turn_id"].as_str().expect("id").to_string();
    let lines = |args: &[&str]| -> Vec<Value> {
        let (code, out) = mdkg_out(&root, "launcher", args);
        assert_eq!(code, 0, "{out}");
        out.lines()
            .map(|l| serde_json::from_str(l).expect("a JSON line"))
            .collect()
    };
    let log = lines(&["agents", "logs", "cx", "--turn", &turn_id]);
    assert!(log.len() >= 2, "{log:?}");
    for line in &log {
        assert_eq!(
            line["position"],
            format!("{turn_id}:{}", line["seq"]),
            "{line}"
        );
    }
    let position = log[0]["position"].as_str().expect("position");
    let rest = lines(&[
        "agents", "logs", "cx", "--turn", &turn_id, "--after", position,
    ]);
    assert_eq!(rest, log[1..], "{rest:?}");
}

/// A synthesized line's position is one a resume honours: a purged turn's
/// `turn.ended` and an interrupted turn's give `<turn_id>:end`, so a resume
/// from either starts with the next turn and never sees it again; the
/// purged turn's synthesized `turn.started` gives none.
#[test]
fn synthesized_lines_resume_past_their_turn_and_are_never_repeated() {
    use std::io::Write as _;
    let (scratch, repo) = make_repo(json!({}));
    let root = graph(&repo);
    ok(
        &root,
        "launcher",
        &[
            "agents",
            "add",
            "cx",
            "--harness",
            "codex",
            "--role",
            "reviewer",
        ],
    );
    send(&repo, "cx", "a request", None);
    let (code, events, err) = up_harnesses(&scratch.0, &root, "cx");
    assert_eq!(code, 0, "{events:?} {err}");
    let run = repo.join(".git/mdkg/run/cx");
    let mut index = fs::OpenOptions::new()
        .append(true)
        .open(run.join("turns.jsonl"))
        .expect("index");
    // Turn 2: ended, its journal purged. Turn 3: a journal that stops after
    // the harness started, its launcher on another host (interrupted).
    let purged = uuid::Uuid::now_v7().to_string();
    let cut = uuid::Uuid::now_v7().to_string();
    for (n, id, end) in [(2u64, &purged, true), (3, &cut, false)] {
        writeln!(
            index,
            "{}",
            json!({"line": "start", "turn_id": id, "number": n, "files": format!("turn-{n}"),
                "started_at": "2026-10-30T00:00:00Z", "supervisor_id": null,
                "host": "elsewhere", "pid": 1, "pid_started_at": "never"})
        )
        .expect("start");
        if end {
            writeln!(
                index,
                "{}",
                json!({"line": "end", "turn_id": id, "ended_at": "2026-10-30T00:00:01Z",
                    "outcome": "idle"})
            )
            .expect("end");
        }
    }
    drop(index);
    let uri = format!("/v1/agents/cx/turns/{cut}");
    let journal: String = [
        json!({"schema": "mdkg.api.turn_output.v1", "turn": uri, "seq": 1,
            "time": "2026-10-30T00:00:00.000Z", "type": "turn.started",
            "data": {"turn_id": cut, "number": 3}}),
        json!({"schema": "mdkg.api.turn_output.v1", "turn": uri, "seq": 2,
            "time": "2026-10-30T00:00:00.001Z", "type": "harness.starting",
            "data": {"harness": "codex"}}),
    ]
    .iter()
    .map(|l| format!("{l}\n"))
    .collect();
    fs::write(run.join(format!("journals/{cut}.events.jsonl")), journal).expect("journal");
    let lines = |f: &Follower, kind: &str| -> Vec<Value> {
        f.until(kind)
            .into_iter()
            .filter(|e| e["type"] != "agent.state")
            .collect()
    };
    let positions =
        |got: &[Value]| -> Vec<Value> { got.iter().map(|e| e["position"].clone()).collect() };
    // From turn 2: the purged turn, then the interrupted one.
    let follow = Follower::start(&root, &["cx", "--follow", "--after", "turn:2"]);
    let two = lines(&follow, "turn.ended");
    assert_eq!(
        positions(&two),
        [Value::Null, json!(format!("{purged}:end"))],
        "{two:?}"
    );
    assert_eq!(two[1]["data"]["output_gone"], true, "{two:?}");
    let three = lines(&follow, "turn.ended");
    assert_eq!(
        positions(&three),
        [
            json!(format!("{cut}:1")),
            json!(format!("{cut}:2")),
            json!(format!("{cut}:end"))
        ],
        "{three:?}"
    );
    assert_eq!(follow.stop(), 0);
    // Resuming from the purged turn's end starts with turn 3.
    let follow = Follower::start(
        &root,
        &["cx", "--follow", "--after", &format!("{purged}:end")],
    );
    let next = lines(&follow, "turn.ended");
    assert_eq!(next[0]["data"]["number"], 3, "{next:?}");
    assert_eq!(next.len(), 3, "{next:?}");
    assert_eq!(follow.stop(), 0);
    // Resuming from the interrupted turn's end shows nothing of it again.
    let follow = Follower::start(&root, &["cx", "--follow", "--after", &format!("{cut}:end")]);
    let mut again = Vec::new();
    while let Ok(line) = follow.lines.recv_timeout(std::time::Duration::from_secs(3)) {
        let event: Value = serde_json::from_str(&line).expect("frame");
        if event["type"] != "agent.state" {
            again.push(event);
        }
    }
    assert!(again.is_empty(), "{again:?}");
    assert_eq!(follow.stop(), 0);
    // And one turn, followed: its synthesized end once, and nothing when
    // resumed from that end.
    let (code, out) = mdkg_out(
        &root,
        "launcher",
        &["agents", "logs", "cx", "--turn", &cut, "--follow"],
    );
    assert_eq!(code, 0, "{out}");
    let one: Vec<Value> = out
        .lines()
        .map(|l| serde_json::from_str(l).expect("frame"))
        .collect();
    assert_eq!(
        positions(&one),
        [
            json!(format!("{cut}:1")),
            json!(format!("{cut}:2")),
            json!(format!("{cut}:end"))
        ],
        "{out}"
    );
    let resumed = format!("{cut}:end");
    let (code, out) = mdkg_out(
        &root,
        "launcher",
        &[
            "agents", "logs", "cx", "--turn", &cut, "--follow", "--after", &resumed,
        ],
    );
    assert_eq!((code, out.as_str()), (0, ""), "{out}");
}

/// Review of 7de881a3: a follower never synthesizes an end (whose
/// `<turn_id>:end` position skips the rest of the turn on a resume) for a
/// turn it can't prove over. A journal it can't read just now isn't purged,
/// and a turn whose launcher still lives isn't over because its supervisor
/// record doesn't show it running (a failed record write leaves it idle).
#[test]
fn a_follower_never_ends_a_turn_it_cannot_prove_over() {
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt;
    let (scratch, repo) = make_repo(json!({}));
    let root = graph(&repo);
    ok(
        &root,
        "launcher",
        &[
            "agents",
            "add",
            "cx",
            "--harness",
            "codex",
            "--role",
            "reviewer",
        ],
    );
    send(&repo, "cx", "a request", None);
    let (code, events, err) = up_harnesses(&scratch.0, &root, "cx");
    assert_eq!(code, 0, "{events:?} {err}");
    let run = repo.join(".git/mdkg/run/cx");
    let host = String::from_utf8(
        Command::new("uname")
            .arg("-n")
            .output()
            .expect("uname")
            .stdout,
    )
    .expect("host")
    .trim()
    .to_string();
    // Turn 2: ended, its journal retained but unreadable for now. Turn 3:
    // started by a launcher that's alive (this test), under a supervisor
    // whose record never showed the agent running, with no end yet.
    let unreadable = uuid::Uuid::now_v7().to_string();
    let live = uuid::Uuid::now_v7().to_string();
    let mut index = fs::OpenOptions::new()
        .append(true)
        .open(run.join("turns.jsonl"))
        .expect("index");
    writeln!(
        index,
        "{}",
        json!({"line": "start", "turn_id": unreadable, "number": 2, "files": "turn-2",
            "started_at": "2026-10-30T00:00:00Z", "supervisor_id": null,
            "host": "elsewhere", "pid": 1, "pid_started_at": "never"})
    )
    .expect("start");
    writeln!(
        index,
        "{}",
        json!({"line": "end", "turn_id": unreadable, "ended_at": "2026-10-30T00:00:01Z",
            "outcome": "replied"})
    )
    .expect("end");
    writeln!(
        index,
        "{}",
        json!({"line": "start", "turn_id": live, "number": 3, "files": "turn-3",
            "started_at": "2026-10-30T00:00:02Z", "supervisor_id": "sup-gone",
            "host": host, "pid": std::process::id(), "pid_started_at": null})
    )
    .expect("start");
    drop(index);
    let journal = |id: &str, n: u64| -> String {
        let uri = format!("/v1/agents/cx/turns/{id}");
        [
            json!({"schema": "mdkg.api.turn_output.v1", "turn": uri, "seq": 1,
                "time": "2026-10-30T00:00:00.000Z", "type": "turn.started",
                "data": {"turn_id": id, "number": n}}),
            json!({"schema": "mdkg.api.turn_output.v1", "turn": uri, "seq": 2,
                "time": "2026-10-30T00:00:00.001Z", "type": "harness.starting",
                "data": {"harness": "codex"}}),
        ]
        .iter()
        .map(|l| format!("{l}\n"))
        .collect()
    };
    let unreadable_path = run.join(format!("journals/{unreadable}.events.jsonl"));
    fs::write(&unreadable_path, journal(&unreadable, 2)).expect("journal");
    fs::write(
        run.join(format!("journals/{live}.events.jsonl")),
        journal(&live, 3),
    )
    .expect("journal");
    fs::set_permissions(&unreadable_path, fs::Permissions::from_mode(0o000)).expect("chmod");
    let quiet = |f: &Follower| -> Vec<Value> {
        let mut got = Vec::new();
        while let Ok(line) = f.lines.recv_timeout(std::time::Duration::from_secs(3)) {
            let event: Value = serde_json::from_str(&line).expect("frame");
            if event["type"] != "agent.state" {
                got.push(event);
            }
        }
        got
    };
    let lines = |f: &Follower, kind: &str| -> Vec<Value> {
        f.until(kind)
            .into_iter()
            .filter(|e| e["type"] != "agent.state")
            .collect()
    };
    let positions =
        |got: &[Value]| -> Vec<Value> { got.iter().map(|e| e["position"].clone()).collect() };
    let follow = Follower::start(&root, &["cx", "--follow", "--after", "turn:2"]);
    // The unreadable journal: no purged turn, no end, nothing past it.
    let early = quiet(&follow);
    assert!(early.is_empty(), "{early:?}");
    // Readable again: all of it, then its end.
    fs::set_permissions(&unreadable_path, fs::Permissions::from_mode(0o644)).expect("chmod");
    let two = lines(&follow, "turn.ended");
    assert_eq!(
        positions(&two),
        [
            json!(format!("{unreadable}:1")),
            json!(format!("{unreadable}:2")),
            json!(format!("{unreadable}:end"))
        ],
        "{two:?}"
    );
    assert_eq!(two[2]["data"]["output_gone"], Value::Null, "{two:?}");
    // The live turn: its lines, and no end while its launcher lives.
    let three: Vec<Value> = (0..2)
        .map(|_| {
            loop {
                let e = follow.next();
                if e["type"] != "agent.state" {
                    break e;
                }
            }
        })
        .collect();
    assert_eq!(
        positions(&three),
        [json!(format!("{live}:1")), json!(format!("{live}:2"))],
        "{three:?}"
    );
    let more = quiet(&follow);
    assert!(more.is_empty(), "{more:?}");
    // Status re-attaches to it, and a default follow starts with it.
    let (_, cx) = status_agent(&root, "cx");
    assert_eq!(cx["current_turn"]["turn_id"], live.as_str(), "{cx}");
    assert_eq!(cx["follow_from"], format!("{live}:2"), "{cx}");
    let default = Follower::start(&root, &["cx", "--follow"]);
    let got: Vec<Value> = (0..2)
        .map(|_| {
            loop {
                let e = default.next();
                if e["type"] != "agent.state" {
                    break e;
                }
            }
        })
        .collect();
    assert_eq!(
        positions(&got),
        [json!(format!("{live}:1")), json!(format!("{live}:2"))],
        "{got:?}"
    );
    let rest = quiet(&default);
    assert!(rest.is_empty(), "{rest:?}");
    assert_eq!(default.stop(), 0);
    // One turn followed: the same.
    let one = Follower::start(&root, &["cx", "--turn", &live, "--follow"]);
    let got = quiet(&one);
    assert_eq!(
        positions(&got),
        [json!(format!("{live}:1")), json!(format!("{live}:2"))],
        "{got:?}"
    );
    // Its end line is terminal evidence: now both end it, once.
    let mut index = fs::OpenOptions::new()
        .append(true)
        .open(run.join("turns.jsonl"))
        .expect("index");
    writeln!(
        index,
        "{}",
        json!({"line": "end", "turn_id": live, "ended_at": "2026-10-30T00:00:03Z",
            "outcome": "replied"})
    )
    .expect("end");
    drop(index);
    for f in [&follow, &one] {
        let end = lines(f, "turn.ended");
        assert_eq!(positions(&end), [json!(format!("{live}:end"))], "{end:?}");
        assert_eq!(end[0]["data"]["outcome"], "replied", "{end:?}");
    }
    assert_eq!(follow.stop(), 0);
    assert_eq!(one.stop(), 0);
    // One turn followed whose journal can't be read at first: it waits,
    // then sends all of it.
    fs::set_permissions(&unreadable_path, fs::Permissions::from_mode(0o000)).expect("chmod");
    let one = Follower::start(&root, &["cx", "--turn", &unreadable, "--follow"]);
    let early = quiet(&one);
    assert!(early.is_empty(), "{early:?}");
    fs::set_permissions(&unreadable_path, fs::Permissions::from_mode(0o644)).expect("chmod");
    let all = lines(&one, "turn.ended");
    assert_eq!(
        positions(&all),
        [
            json!(format!("{unreadable}:1")),
            json!(format!("{unreadable}:2")),
            json!(format!("{unreadable}:end"))
        ],
        "{all:?}"
    );
    assert_eq!(one.stop(), 0);
}

/// A turn whose final message is exactly NO_REPLY ends `no_reply`, not
/// `replied`: in `agents up --json`, the index's end line, the journal's
/// `turn.ended` and `agents status` last_turn, with no `reply` and no
/// `reply_id`; the mail shown is still acked.
#[test]
fn a_no_reply_turn_ends_no_reply_everywhere() {
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
            "--policy",
            "{\"trigger\":\"manual\"}",
        ],
    );
    send(&repo, "writer", "The first request.", None);
    let (code, events) = up(&repo, &["writer"]);
    assert_eq!(code, 0, "{events:?}");
    assert_eq!(
        events[0]["data"]["turn"]["outcome"], "replied",
        "{events:?}"
    );
    assert!(
        events[0]["data"]["turn"]["reply_id"].is_string(),
        "{events:?}"
    );
    fs::write(repo.join(".worktrees/writer/fake-reply"), "NO_REPLY").expect("reply");
    send(&repo, "writer", "Nothing to answer.", None);
    let (code, events) = up(&repo, &["writer"]);
    assert_eq!(code, 0, "{events:?}");
    let turn = &events[0]["data"]["turn"];
    assert_eq!(turn["outcome"], "no_reply", "{events:?}");
    assert!(turn.get("reply").is_none(), "{events:?}");
    assert!(turn.get("reply_id").is_none(), "{events:?}");
    let lines = index_lines(&repo, "writer");
    let end = lines.last().expect("an end line");
    assert_eq!(
        (&end["line"], &end["outcome"]),
        (&json!("end"), &json!("no_reply")),
        "{lines:?}"
    );
    let turn_id = end["turn_id"].as_str().expect("turn id");
    let journal = fs::read_to_string(repo.join(format!(
        ".git/mdkg/run/writer/journals/{turn_id}.events.jsonl"
    )))
    .expect("journal");
    let ended: Value = journal
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).expect("journal line"))
        .find(|e| e["type"] == "turn.ended")
        .expect("turn.ended");
    turn_output_conforms(&ended);
    assert_eq!(ended["data"]["outcome"], "no_reply", "{ended}");
    let (_, writer) = status_agent(&root, "writer");
    assert_eq!(writer["last_turn"]["outcome"], "no_reply", "{writer}");
    let inbox = ok(&root, "writer", &["msg", "inbox"]);
    assert_eq!(inbox["data"]["messages"], json!([]), "{inbox}");
}

/// A turn that already answered its mail itself (a handoff to its sender
/// in its context, by MCP or the CLI) has its final message kept, not
/// imported as a second reply: the recipient gets one message, the turn
/// ends `no_reply` and the shown mail is acked (#20931). Mail to someone
/// else, or a question to the sender, doesn't count (review of 710641d2).
#[test]
fn a_turn_that_already_mailed_its_recipient_imports_no_second_reply() {
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
            "--policy",
            "{\"trigger\":\"manual\"}",
        ],
    );
    let lane = repo.join(".worktrees/writer");
    let mdkg = env!("CARGO_BIN_EXE_mdkg");
    // The turn's own mail: to `to`, of `kind`, in the context of the mail
    // the turn was shown (the newest unacked message's).
    let mails = |to: &str, kind: &str| {
        fs::write(
            lane.join("fake-send"),
            format!(
                "ctx=$('{mdkg}' --as writer msg inbox --json | sed -n 's/.*\"contextId\": *\"\\([^\"]*\\)\".*/\\1/p' | tail -n 1)\n\
                 '{mdkg}' --as writer msg send --to {to} --kind {kind} --context \"$ctx\" --text 'mid-turn' --json\n"
            ),
        )
        .expect("fake-send")
    };
    let from_writer = || {
        let inbox = ok(
            &root,
            "nick",
            &["msg", "inbox", "--all", "--from", "writer"],
        );
        inbox["data"]["messages"]
            .as_array()
            .expect("messages")
            .len()
    };
    // The first turn makes the worktree; it mails no one and replies.
    send(&repo, "writer", "The first request.", None);
    let (code, events) = up(&repo, &["writer"]);
    assert_eq!(code, 0, "{events:?}");
    assert_eq!(
        events[0]["data"]["turn"]["outcome"], "replied",
        "{events:?}"
    );
    assert_eq!(from_writer(), 1);
    // Mail to someone else: the reply is still imported.
    mails("orchestrator", "handoff");
    send(&repo, "writer", "Tell the orchestrator.", None);
    let (code, events) = up(&repo, &["writer"]);
    assert_eq!(code, 0, "{events:?}");
    assert_eq!(
        events[0]["data"]["turn"]["outcome"], "replied",
        "{events:?}"
    );
    assert_eq!(from_writer(), 2);
    // A question to the sender in the same context isn't an answer: the
    // final message is still imported (review of 710641d2).
    mails("nick", "question");
    send(&repo, "writer", "Ask me something first.", None);
    let (code, events) = up(&repo, &["writer"]);
    assert_eq!(code, 0, "{events:?}");
    assert_eq!(
        events[0]["data"]["turn"]["outcome"], "replied",
        "{events:?}"
    );
    assert!(
        fs::read_to_string(lane.join("fake-send.out"))
            .unwrap_or_default()
            .contains("\"ok\": true"),
        "the question was sent"
    );
    assert_eq!(from_writer(), 4, "the question and the reply");
    // A handoff to the sender itself: no second reply. The final message
    // holds a credential, which the retained receipt must not keep, as it
    // was never imported through the mail filters (review of 8a8ff4b8).
    mails("nick", "handoff");
    let secret = concat!("https://someone:SYNTHETIC_SKIPPED", "@example.test/d");
    fs::write(lane.join("fake-reply"), format!("done; {secret}")).expect("fake-reply");
    send(&repo, "writer", "Hand it back.", None);
    let (code, events) = up(&repo, &["writer"]);
    assert_eq!(code, 0, "{events:?}");
    let turn = &events[0]["data"]["turn"];
    assert_eq!(turn["outcome"], "no_reply", "{events:?}");
    assert!(turn.get("reply_id").is_none(), "{events:?}");
    assert!(
        fs::read_to_string(lane.join("fake-send.out"))
            .unwrap_or_default()
            .contains("\"ok\": true"),
        "the handoff was sent"
    );
    assert_eq!(from_writer(), 5, "the handoff alone, not a second reply");
    let receipt = turn["receipt"]["path"].as_str().expect("receipt path");
    let receipt = fs::read_to_string(receipt).expect("receipt");
    assert!(
        receipt.contains("not imported: the turn already mailed its recipient"),
        "{receipt}"
    );
    assert!(!receipt.contains("SYNTHETIC_SKIPPED"), "{receipt}");
    assert!(
        receipt.contains("withheld: not imported, as the turn already mailed its recipient"),
        "{receipt}"
    );
    let inbox = ok(&root, "writer", &["msg", "inbox"]);
    assert_eq!(
        inbox["data"]["messages"],
        json!([]),
        "the shown mail was acked"
    );
}

/// A SIGTERM during a continuous `agents up`'s startup git (here a
/// worker's `git worktree add`) waits for that git, then stops before any
/// worker takes a turn, even with mail queued (reviews of 9e274d1f and
/// 64b6b93e). That no turn starts once the handler has run, whatever the
/// timing, is supervise::control's an_external_flag_stops_at_once.
#[test]
fn sigterm_during_startup_git_stops_before_any_turn_is_admitted() {
    use std::os::unix::fs::PermissionsExt;
    let (scratch, repo) = make_repo(json!({}));
    let root = graph(&repo);
    ok(
        &root,
        "launcher",
        &[
            "agents",
            "add",
            "w",
            "--harness",
            "claude",
            "--role",
            "worker",
        ],
    );
    send(&repo, "w", "queued work", None);
    let dir = &scratch.0;
    let real = String::from_utf8(
        Command::new("/bin/sh")
            .args(["-c", "command -v git"])
            .output()
            .expect("sh")
            .stdout,
    )
    .expect("utf-8");
    let shim = dir.join("shim");
    fs::create_dir_all(&shim).expect("shim");
    fs::write(
        shim.join("git"),
        format!(
            "#!/bin/sh\n\
             case \" $* \" in *' worktree add '*)\n\
             : > '{in_add}'\n\
             n=0; while [ ! -e '{release}' ] && [ $n -lt 1200 ]; do sleep 0.05; n=$((n+1)); done ;;\n\
             esac\n\
             exec {real} \"$@\"\n",
            in_add = dir.join("in-add").display(),
            release = dir.join("release").display(),
            real = real.trim(),
        ),
    )
    .expect("shim");
    fs::set_permissions(shim.join("git"), fs::Permissions::from_mode(0o755)).expect("chmod");
    let claude = dir.join("claude");
    fs::write(
        &claude,
        format!("#!/bin/sh\n: > '{}'\nexit 1\n", dir.join("ran").display()),
    )
    .expect("claude");
    fs::set_permissions(&claude, fs::Permissions::from_mode(0o755)).expect("chmod");
    let home = dir.join("home");
    fs::create_dir_all(home.join(".codex")).expect("home");
    let mut up = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
        .current_dir(&root)
        .arg("--root")
        .arg(&root)
        .args(["--as", "launcher", "agents", "up", "w", "--wait", "1"])
        .env(
            "PATH",
            format!(
                "{}:{}",
                shim.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("CODEX_HOME", home.join(".codex"))
        .env("MDKG_RUN_CLAUDE", &claude)
        .stdout(std::process::Stdio::null())
        .stderr(fs::File::create(dir.join("up.err")).expect("stderr"))
        .spawn()
        .expect("agents up");
    // On a failed assertion, agents up and the held git end too: the git
    // shim's wait is bounded (60 s), and this kills agents up.
    struct KillOnPanic(u32);
    impl Drop for KillOnPanic {
        fn drop(&mut self) {
            if std::thread::panicking() {
                let _ = Command::new("/bin/kill")
                    .args(["-KILL", &self.0.to_string()])
                    .status();
            }
        }
    }
    let _guard = KillOnPanic(up.id());
    let err = || fs::read_to_string(dir.join("up.err")).unwrap_or_default();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while !dir.join("in-add").exists() {
        assert!(
            up.try_wait().expect("wait").is_none(),
            "agents up ended before its worktree add: {}",
            err()
        );
        assert!(
            std::time::Instant::now() < deadline,
            "no worktree add: {}",
            err()
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let _ = Command::new("/bin/kill")
        .args(["-TERM", &up.id().to_string()])
        .status();
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert!(
        up.try_wait().expect("wait").is_none(),
        "agents up died under its git: {}",
        err()
    );
    fs::write(dir.join("release"), "").expect("release");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let status = loop {
        if let Some(status) = up.try_wait().expect("wait") {
            break status;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "agents up never stopped: {}",
            err()
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    assert!(status.success(), "{status:?}: {}", err());
    assert!(
        !dir.join("ran").exists(),
        "a turn was admitted after the SIGTERM: {}",
        err()
    );
    let inbox = ok(&root, "w", &["msg", "inbox"]);
    assert_eq!(
        inbox["data"]["messages"].as_array().map(Vec::len),
        Some(1),
        "{inbox}"
    );
}

#[test]
fn delivery_flag_sets_shape_without_losing_other_policy_fields() {
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
            "--policy",
            "{\"timeout\":90}",
            "--delivery",
            "digest",
        ],
    );
    assert_eq!(added["data"]["definition"]["policy"]["shape"], "digest");
    assert_eq!(added["data"]["definition"]["policy"]["timeout"], 90);
    let edited = ok(
        &root,
        "launcher",
        &["agents", "edit", "writer", "--delivery", "batch"],
    );
    assert_eq!(edited["data"]["definition"]["policy"]["shape"], "batch");
    assert_eq!(edited["data"]["definition"]["policy"]["timeout"], 90);
}

#[test]
fn named_once_uses_the_digest_policy() {
    let (_dir, repo) = make_repo(
        json!({"writer": {"harness":"claude", "role":"worker", "reports_to":"coordinator", "policy":{"shape":"digest"}}}),
    );
    send(&repo, "writer", "first topic", None);
    send(&repo, "writer", "second topic", None);
    let (code, events) = up(&repo, &["writer", "--dry-run"]);
    assert_eq!(code, 0, "{events:?}");
    let turn = &events[0]["data"]["turn"];
    let prompt = turn["prompt"].as_str().expect("prompt");
    assert!(
        prompt.contains("first topic") && prompt.contains("second topic"),
        "{prompt}"
    );
    assert!(
        prompt.contains("final reply goes to coordinator"),
        "{prompt}"
    );
}
