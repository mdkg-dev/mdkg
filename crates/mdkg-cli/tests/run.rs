//! Agent execution and install end to end: the
//! launcher wakes a fake Claude agent through the real `msg` commands, and
//! the installer writes every agent's config.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

mod common;

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).ok();
    }
}

/// Keep the product's chosen git when a test hides the parent's PATH.
/// Expose only git: its real directory may also contain an installed mdkg,
/// which would invalidate the binary-naming and missing-program tests.
fn git_only_bin(scratch: &Path) -> PathBuf {
    let bin = scratch.join("git-only-bin");
    fs::create_dir(&bin).expect("git bin");
    std::os::unix::fs::symlink(&mdkg_coord_sqlite::git::git_program().0, bin.join("git"))
        .expect("git link");
    bin
}

fn narrow_path(git_bin: &Path, first: &[&Path]) -> OsString {
    std::env::join_paths(first.iter().copied().chain([
        git_bin,
        Path::new("/usr/bin"),
        Path::new("/bin"),
    ]))
    .expect("test PATH")
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

/// A repository on `main` with a worktree `lane` on branch
/// `agent/lane-agent`, the worker `lane-agent`'s own.
fn repo() -> (Scratch, PathBuf, PathBuf) {
    let dir = common::temp::dir("mdkg-cli-run");
    let dir = fs::canonicalize(dir).expect("canonical");
    let repo = dir.join("repo");
    let conformance = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance");
    fs::create_dir_all(&repo).expect("repo");
    copy_tree(&conformance.join("base"), &repo);
    copy_tree(&conformance.join("cases/g01-valid-basic"), &repo);
    fs::remove_file(repo.join("commands")).ok();
    fs::remove_file(repo.join("README.md")).ok();
    // The fake agent's scratch files stay out of git.
    fs::write(
        repo.join(".gitignore"),
        "*.args\n*.prompt\n*.environ\n/.worktrees/\n",
    )
    .expect("ignore");
    sh(&repo, &["init", "-q", "-b", "main"]);
    sh(&repo, &["add", "."]);
    sh(&repo, &["commit", "-q", "-m", "init"]);
    // Keep the legacy handles while committing the graph's 0.7 identity.
    let migration = ok(&repo, "launcher", &["migrate", "--dry-run"]);
    let plan = migration["data"]["plan"].as_str().expect("migration plan");
    ok(&repo, "launcher", &["migrate", "--apply", plan]);
    // The launcher's root: a worktree on its own branch, never main.
    let graph = repo.join(".worktrees/graph");
    let path = graph.to_str().expect("utf-8");
    sh(
        &repo,
        &["worktree", "add", "-q", "-b", "mdkg-graph", path, "main"],
    );
    // Register the manually created graph checkout through the public path.
    ok(&repo, "launcher", &["install"]);
    let lane = dir.join("lane");
    let path = lane.to_str().expect("utf-8");
    sh(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "agent/lane-agent",
            path,
            "main",
        ],
    );
    (Scratch(dir), repo, lane)
}

/// The launcher's graph worktree in a repository made by [`repo`].
fn graph(repo: &Path) -> PathBuf {
    repo.join(".worktrees/graph")
}

/// Config changes in a launcher fixture are deliberate committed setup.
fn commit_graph_config(graph: &Path) {
    sh(graph, &["add", ".mdkg/config.json"]);
    sh(graph, &["commit", "-qm", "configure fixture"]);
}

fn mdkg(root: &Path, actor: &str, args: &[&str]) -> (i32, Value) {
    let out = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
        .current_dir(root)
        .arg("--root")
        .arg(root)
        .args(["--as", actor])
        .args(common::agent_turn_args(root, args))
        .arg("--json")
        .output()
        .expect("run mdkg");
    let envelope = if args.first() == Some(&"turn") {
        common::one_turn(&out.stdout)
    } else {
        let value: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|_| {
            panic!(
                "not JSON: {}\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
        });
        common::conforms_envelope(&value);
        value
    };
    (out.status.code().unwrap_or(-1), envelope)
}

fn ok(root: &Path, actor: &str, args: &[&str]) -> Value {
    let (code, out) = mdkg(root, actor, args);
    assert_eq!(code, 0, "{args:?}: {out}");
    out
}

#[test]
fn a_launched_agent_replies_through_the_real_mail_commands() {
    let (_dir, repo, lane) = repo();
    let sent = ok(
        &repo,
        "orchestrator",
        &[
            "msg",
            "send",
            "--to",
            "lane-agent",
            "--kind",
            "handoff",
            "--text",
            "pick up task-1",
        ],
    );
    let cwd = lane.to_str().expect("utf-8");
    let first = [
        "turn",
        "lane-agent",
        "--harness",
        "claude",
        "--role",
        "worker",
        "--cwd",
        cwd,
    ];
    // Never from the worker's own checkout: there it could rewrite the
    // `.git` file the launcher finds its repository through.
    let (code, refused) = mdkg(&lane, "launcher", &first);
    assert_eq!(code, 1, "{refused}");
    assert_eq!(refused["diagnostics"][0]["code"], "run.cwd_outside");
    assert!(
        refused.to_string().contains("no worker may use"),
        "{refused}"
    );
    let out = ok(&graph(&repo), "launcher", &first);
    let data = &out["data"];
    assert_eq!(data["outcome"], "replied", "{out}");
    let reply = data["reply_id"].as_str().expect("reply id").to_string();
    assert!(
        fs::read_to_string(lane.join("claude.prompt"))
            .expect("prompt")
            .contains("pick up task-1")
    );

    let inbox = ok(&repo, "orchestrator", &["msg", "inbox"]);
    let msg = &inbox["data"]["messages"][0];
    assert_eq!(msg["messageId"], reply.as_str());
    assert_eq!(msg["sender"], "lane-agent");
    assert_eq!(msg["via"], "launcher");
    assert_eq!(msg["parts"][0]["text"], "claude reply");
    // In the handoff's conversation, referring to it.
    let handoff = sent["data"]["message"]["messageId"].as_str().expect("id");
    assert_eq!(msg["contextId"], handoff, "{inbox}");
    assert_eq!(msg["referenceIds"][0], handoff, "{inbox}");
    let lane_inbox = ok(&lane, "lane-agent", &["msg", "inbox"]);
    assert_eq!(lane_inbox["data"]["messages"], serde_json::json!([]));

    // The agent is remembered, and with no mail its next wake is idle.
    let again = ok(&graph(&repo), "launcher", &["turn", "lane-agent"]);
    assert_eq!(again["data"]["outcome"], "idle");
    // The turn lease was released.
    let status = ok(&repo, "orchestrator", &["status"]);
    assert!(!status.to_string().contains("run:lane-agent"), "{status}");
}

#[test]
fn a_worker_in_the_graph_worktree_is_refused_and_a_reviewer_runs_there() {
    let (_dir, repo, _lane) = repo();
    let graph = graph(&repo);
    // No --cwd: it defaults to the graph worktree the launcher runs from.
    let worker = [
        "turn",
        "w",
        "--harness",
        "claude",
        "--role",
        "worker",
        "--dry-run",
    ];
    let (code, refused) = mdkg(&graph, "launcher", &worker);
    assert_eq!(code, 1, "{refused}");
    assert_eq!(refused["diagnostics"][0]["code"], "run.cwd_outside");
    let hint = refused["diagnostics"][0]["hint"].as_str().expect("hint");
    assert!(hint.contains("mdkg agents edit w --worktree"), "{hint}");
    // The definition is present, but refused execution saved no state.
    assert!(!repo.join(".git/mdkg/run/w.json").exists());
    // Also with --cwd naming it, and below it.
    let inner = graph.join(".mdkg");
    for cwd in [&graph, &inner] {
        let mut args = worker.to_vec();
        args.extend(["--cwd", cwd.to_str().expect("utf-8")]);
        let (code, refused) = mdkg(&graph, "launcher", &args);
        assert_eq!(code, 1, "{refused}");
        assert_eq!(refused["diagnostics"][0]["code"], "run.cwd_outside");
    }
    // The main checkout holds the graph worktree: refused, and said so.
    let mut args = worker.to_vec();
    args.extend(["--cwd", repo.to_str().expect("utf-8")]);
    let (code, refused) = mdkg(&graph, "launcher", &args);
    assert_eq!(code, 1, "{refused}");
    let message = refused["diagnostics"][0]["message"]
        .as_str()
        .expect("message");
    assert!(
        message.contains("holds the checkout mdkg runs from"),
        "{message}"
    );
    // A reviewer can't write, so it may run there.
    let out = ok(
        &graph,
        "launcher",
        &[
            "turn",
            "r",
            "--harness",
            "claude",
            "--role",
            "reviewer",
            "--prompt",
            "x",
            "--dry-run",
        ],
    );
    assert_eq!(out["ok"], true, "{out}");
}

#[test]
fn mail_from_the_launchers_own_actor_is_refused_before_the_agent_runs() {
    let (_dir, repo, lane) = repo();
    ok(
        &repo,
        "orch",
        &[
            "msg",
            "send",
            "--to",
            "lane-agent",
            "--kind",
            "note",
            "--text",
            "hi",
        ],
    );
    let cwd = lane.to_str().expect("utf-8");
    let (code, refused) = mdkg(
        &graph(&repo),
        "orch",
        &[
            "turn",
            "lane-agent",
            "--harness",
            "claude",
            "--role",
            "worker",
            "--cwd",
            cwd,
        ],
    );
    assert_eq!(code, 1, "{refused}");
    assert_eq!(refused["diagnostics"][0]["code"], "run.mail_failed");
    assert!(
        refused
            .to_string()
            .contains("the actor this launcher runs as"),
        "{refused}"
    );
    assert!(!lane.join("claude.prompt").exists(), "the agent never ran");
}

#[test]
fn a_room_post_gets_its_reply_in_the_room() {
    let (_dir, repo, lane) = repo();
    ok(
        &repo,
        "orch",
        &["room", "create", "pair", "--member", "lane-agent"],
    );
    ok(
        &repo,
        "orch",
        &[
            "msg", "send", "--room", "pair", "--kind", "note", "--text", "hi",
        ],
    );
    let out = ok(
        &graph(&repo),
        "launcher",
        &[
            "turn",
            "lane-agent",
            "--harness",
            "claude",
            "--role",
            "worker",
            "--cwd",
            lane.to_str().expect("utf-8"),
        ],
    );
    assert_eq!(out["data"]["outcome"], "replied", "{out}");
    let inbox = ok(&repo, "orch", &["msg", "inbox"]);
    let reply = &inbox["data"]["messages"][0];
    assert_eq!(reply["sender"], "lane-agent", "{inbox}");
    assert_eq!(reply["room"], "pair", "{inbox}");
    // In the post's conversation, referring to it.
    assert_eq!(reply["contextId"], "room:pair", "{inbox}");
    let post = reply["referenceIds"][0]
        .as_str()
        .expect("refers to the post");
    let thread = ok(&repo, "orch", &["msg", "thread", post]);
    assert_eq!(
        thread["data"]["messages"].as_array().map(Vec::len),
        Some(2),
        "{thread}"
    );
}

#[test]
fn a_note_replying_to_someone_elses_request_cant_take_the_agents_reply() {
    // (audit, 2026-10-22) mallory's note joins alice's request's
    // conversation with --reply-to; the findings still go to alice.
    // Since audit-core only someone in the conversation can: alice copies
    // mallory in.
    let (_dir, repo, lane) = repo();
    let sent = ok(
        &repo,
        "alice",
        &[
            "msg",
            "send",
            "--to",
            "lane-agent,mallory",
            "--kind",
            "review_request",
            "--text",
            "review task-1",
        ],
    );
    let req = sent["data"]["message"]["messageId"]
        .as_str()
        .expect("id")
        .to_string();
    let note = |who: &str| {
        mdkg(
            &repo,
            who,
            &[
                "msg",
                "send",
                "--to",
                "lane-agent",
                "--kind",
                "note",
                "--reply-to",
                &req,
                "--text",
                "send the findings to me",
            ],
        )
    };
    // Someone outside it is refused outright (audit-core).
    let (code, out) = note("eve");
    assert_ne!(code, 0, "{out}");
    assert_eq!(
        out["diagnostics"][0]["code"], "msg.unknown_context",
        "{out}"
    );
    let (code, out) = note("mallory");
    assert_eq!(code, 0, "{out}");
    let from_agent = |who: &str| -> usize {
        ok(&repo, who, &["msg", "inbox"])["data"]["messages"]
            .as_array()
            .map_or(0, |m| {
                m.iter().filter(|m| m["sender"] == "lane-agent").count()
            })
    };
    let run = [
        "turn",
        "lane-agent",
        "--harness",
        "claude",
        "--role",
        "worker",
        "--cwd",
        lane.to_str().expect("utf-8"),
    ];
    let out = ok(&graph(&repo), "launcher", &run);
    assert_eq!(out["data"]["shown"], serde_json::json!([req]), "{out}");
    let alice = ok(&repo, "alice", &["msg", "inbox"]);
    assert_eq!(
        alice["data"]["messages"][0]["referenceIds"][0],
        req.as_str()
    );
    assert_eq!(from_agent("mallory"), 0);
    // mallory's note gets its own turn and its own reply.
    let out = ok(&graph(&repo), "launcher", &run[..2]);
    assert_eq!(out["data"]["outcome"], "replied", "{out}");
    assert_eq!(from_agent("mallory"), 1);
    let alice = ok(&repo, "alice", &["msg", "inbox"]);
    assert_eq!(alice["data"]["messages"].as_array().map(Vec::len), Some(1));
}

#[test]
fn a_dry_run_prints_no_escape_sequences_from_a_body() {
    let (_dir, repo, lane) = repo();
    ok(
        &repo,
        "alice",
        &[
            "msg",
            "send",
            "--to",
            "lane-agent",
            "--kind",
            "note",
            "--text",
            "hi\u{1b}]0;pwned\u{7}\u{1b}[2J there",
        ],
    );
    let out = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
        .current_dir(graph(&repo))
        .arg("--root")
        .arg(graph(&repo))
        .args(["--as", "launcher"])
        .args(common::agent_turn_args(
            &graph(&repo),
            &[
                "turn",
                "lane-agent",
                "--harness",
                "claude",
                "--role",
                "worker",
                "--cwd",
                lane.to_str().unwrap(),
                "--dry-run",
            ],
        ))
        .output()
        .expect("run mdkg");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}");
    assert!(stdout.contains("hi there"), "{stdout:?}");
    assert!(
        !stdout.contains('\u{1b}') && !stdout.contains('\u{7}'),
        "{stdout:?}"
    );
}

#[test]
fn a_room_post_from_the_launchers_actor_is_refused_before_the_agent_runs() {
    let (_dir, repo, lane) = repo();
    ok(
        &repo,
        "orch",
        &["room", "create", "pair", "--member", "lane-agent"],
    );
    ok(
        &repo,
        "orch",
        &[
            "msg", "send", "--room", "pair", "--kind", "note", "--text", "hi",
        ],
    );
    let (code, refused) = mdkg(
        &graph(&repo),
        "orch",
        &[
            "turn",
            "lane-agent",
            "--harness",
            "claude",
            "--role",
            "worker",
            "--cwd",
            lane.to_str().expect("utf-8"),
        ],
    );
    assert_eq!(code, 1, "{refused}");
    assert!(
        refused
            .to_string()
            .contains("the actor this launcher runs as"),
        "{refused}"
    );
    assert!(!lane.join("claude.prompt").exists(), "the agent never ran");
}

#[test]
fn mail_from_the_agent_itself_is_acked_as_not_answered_and_the_agent_doesnt_run() {
    let (_dir, repo, lane) = repo();
    ok(
        &repo,
        "lane-agent",
        &[
            "msg", "send", "--to", "role:rev", "--kind", "note", "--text", "hi",
        ],
    );
    ok(&repo, "lane-agent", &["claim", "role:rev"]);
    let (code, refused) = mdkg(
        &graph(&repo),
        "launcher",
        &[
            "turn",
            "lane-agent",
            "--harness",
            "claude",
            "--role",
            "worker",
            "--cwd",
            lane.to_str().expect("utf-8"),
        ],
    );
    assert_eq!(code, 0, "{refused}");
    assert_eq!(refused["data"]["outcome"], "idle");
    assert_eq!(
        refused["data"]["not_answered"].as_array().map(Vec::len),
        Some(1)
    );
    assert!(!lane.join("claude.prompt").exists(), "the agent never ran");
    // Acked: the next run has nothing to do.
    let again = ok(
        &graph(&repo),
        "launcher",
        &[
            "turn",
            "lane-agent",
            "--harness",
            "claude",
            "--role",
            "worker",
            "--cwd",
            lane.to_str().expect("utf-8"),
        ],
    );
    assert_eq!(again["data"]["outcome"], "idle");
    assert!(again["data"]["not_answered"].is_null(), "{again}");
}

#[test]
fn a_tag_named_main_doesnt_hide_a_root_on_main() {
    let (_dir, repo, _lane) = repo();
    sh(&repo, &["tag", "main"]);
    let (code, refused) = mdkg(
        &repo,
        "launcher",
        &[
            "turn",
            "r",
            "--harness",
            "claude",
            "--role",
            "reviewer",
            "--prompt",
            "x",
            "--dry-run",
        ],
    );
    assert_eq!(code, 1, "{refused}");
    assert_eq!(refused["diagnostics"][0]["code"], "run.branch_refused");
}

/// Variables withheld but not named in `KEY_VARS`: by family (a provider
/// switch, a header that can carry a key, an endpoint, one Claude Code
/// might add later), and from OpenCode's pinned catalog (reviews of
/// 14867b8 and c569050).
const WITHHELD_BY_FAMILY: &[&str] = &[
    "ANTHROPIC_CUSTOM_HEADERS",
    "CLAUDE_CODE_USE_MANTLE",
    "CLAUDE_CODE_USE_ANTHROPIC_AWS",
    "ANTHROPIC_GOOGLE_CLOUD_PROJECT",
    "ANTHROPIC_SOMETHING_NEW",
    "OPENAI_BASE_URL",
    "SNOWFLAKE_CORTEX_PAT",
    "GOOGLE_VERTEX_PROJECT",
    "ZHIPU_API_KEY",
    "NVIDIA_API_KEY",
    // A desktop host session's own (spike, 2026-10-25): bare mode skips
    // a worker's hooks.
    "CLAUDE_CODE_SIMPLE",
    "CLAUDECODE",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_AGENT_SDK_VERSION",
    "CLAUDE_CODE_SESSION_ACCESS_TOKEN",
    "CLAUDE_CODE_REMOTE_SESSION_ID",
    "CLAUDE_SESSION_INGRESS_TOKEN_FILE",
    "CLAUDE_BRIDGE_OAUTH_TOKEN",
    "CLAUDE_API_KEY",
];

#[test]
fn an_agent_runs_with_the_launchers_environment_but_not_its_pins() {
    let (_dir, repo, lane) = repo();
    ok(
        &repo,
        "orchestrator",
        &[
            "msg",
            "send",
            "--to",
            "lane-agent",
            "--kind",
            "note",
            "--text",
            "hi",
        ],
    );
    let canary = format!("canary-{}", std::process::id());
    let out = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
        .current_dir(graph(&repo))
        .arg("--root")
        .arg(graph(&repo))
        .args(["--as", "launcher"])
        .args(common::agent_turn_args(
            &graph(&repo),
            &[
                "turn",
                "lane-agent",
                "--harness",
                "claude",
                "--role",
                "worker",
                "--cwd",
                lane.to_str().unwrap(),
                "--json",
            ],
        ))
        .env("ANTHROPIC_API_KEY", &canary)
        .env("GITHUB_TOKEN", &canary)
        .env("AWS_SECRET_ACCESS_KEY", &canary)
        .env("AWS_PROFILE", &canary)
        .env("GOOGLE_APPLICATION_CREDENTIALS", &canary)
        .envs(mdkg_run::auth::KEY_VARS.iter().map(|k| (k, &canary)))
        .envs(WITHHELD_BY_FAMILY.iter().map(|k| (k, &canary)))
        .env("ANTHROPIC_MODEL", &canary)
        .env("CLAUDE_CODE_ENABLE_TELEMETRY", &canary)
        .env("CLAUDE_REMOTE_CONTROL_SESSION_NAME_PREFIX", &canary)
        .env("DATABASE_URL", &canary)
        // A launcher run with its own pins (by another launcher): they
        // stay its own.
        .env(
            "MDKG_TRUSTED_WORKTREE",
            fs::canonicalize(graph(&repo)).expect("graph"),
        )
        .env("LC_ALL", "C")
        .output()
        .expect("run mdkg");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let env = fs::read_to_string(lane.join("claude.environ")).expect("the agent's environment");
    // The launcher recorded the turn's mark while it ran, and removed it.
    assert!(
        lane.join("claude.mark.environ").exists(),
        "the turn's mark was recorded"
    );
    let turns = repo.join(".git/mdkg/hooks/turns");
    assert!(
        fs::read_dir(&turns).map_or(true, |mut d| d.next().is_none()),
        "no mark left after the turn"
    );
    // The login check sees what the agent will: a stray key or provider
    // switch can't make it report a login the turn wouldn't use.
    let login = fs::read_to_string(lane.join("claude-auth.environ")).expect("the login check's");
    // The user's environment, as they'd start the agent themselves, less
    // the provider key variables and switches: a stray one would move the
    // turn from the subscription to API or cloud billing.
    for seen in [&env, &login] {
        for var in [
            "GITHUB_TOKEN",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_PROFILE",
            "GOOGLE_APPLICATION_CREDENTIALS",
            "DATABASE_URL",
            "ANTHROPIC_MODEL",
            "CLAUDE_CODE_ENABLE_TELEMETRY",
            "CLAUDE_REMOTE_CONTROL_SESSION_NAME_PREFIX",
        ] {
            assert!(
                seen.lines().any(|l| l == format!("{var}={canary}")),
                "{var} missing"
            );
        }
        for var in mdkg_run::auth::KEY_VARS.iter().chain(WITHHELD_BY_FAMILY) {
            assert!(
                !seen.lines().any(|l| l.starts_with(&format!("{var}="))),
                "{var} reached the agent or its login check"
            );
        }
    }
    for var in ["PATH=", "HOME=", "LC_ALL=C", "MDKG_ACTOR=lane-agent"] {
        assert!(
            env.lines().any(|l| l.starts_with(var)),
            "{var} missing:\n{env}"
        );
    }
    // No pins: they'd make every mdkg the agent's tests build refuse.
    assert!(!env.contains("MDKG_TRUSTED_"), "{env}");
}

#[test]
fn a_codex_agent_runs_with_the_launchers_environment_and_codex_home() {
    let (_dir, repo, lane) = repo();
    ok(
        &repo,
        "orchestrator",
        &[
            "msg",
            "send",
            "--to",
            "lane-agent",
            "--kind",
            "note",
            "--text",
            "hi",
        ],
    );
    let canary = format!("canary-{}", std::process::id());
    let out = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
        .current_dir(graph(&repo))
        .arg("--root")
        .arg(graph(&repo))
        .args(["--as", "launcher"])
        .args(common::agent_turn_args(
            &graph(&repo),
            &[
                "turn",
                "lane-agent",
                "--harness",
                "codex",
                "--role",
                "reviewer",
                "--cwd",
                lane.to_str().unwrap(),
                "--json",
            ],
        ))
        .env("MDKG_RUN_CODEX", fixture("fake-codex.sh"))
        .env("CODEX_HOME", repo.join("user-codex"))
        .env("OPENAI_API_KEY", &canary)
        .env("CODEX_API_KEY", &canary)
        .env("E2E_CANARY", &canary)
        .output()
        .expect("run mdkg");
    let env = fs::read_to_string(lane.join("codex.environ")).unwrap_or_default();
    assert!(
        !env.is_empty(),
        "codex never ran: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(env.lines().any(|l| l == format!("E2E_CANARY={canary}")));
    for var in ["OPENAI_API_KEY", "CODEX_API_KEY"] {
        assert!(
            !env.lines().any(|l| l.starts_with(&format!("{var}="))),
            "{var} reached Codex"
        );
    }
    // The user's own CODEX_HOME, not one of the launcher's.
    let home = format!("CODEX_HOME={}", repo.join("user-codex").display());
    assert!(env.lines().any(|l| l == home), "{env}");
    assert!(!env.contains("MDKG_TRUSTED_"));
}

/// An OpenCode worker turn asks a private OpenCode server for the resolved
/// agents first, with the turn's own HOME and XDG folders, and refuses when
/// build is disabled, the preflight fails or OpenCode isn't 2.1.0. The fake
/// stands in for OpenCode's resolution of the temp user config.
#[test]
fn an_opencode_worker_turn_runs_only_after_its_build_preflight() {
    let (dir, repo, lane) = repo();
    let home = dir.0.join("home");
    let xdg = dir.0.join("xdg");
    fs::create_dir_all(xdg.join("opencode")).expect("xdg");
    fs::create_dir_all(&home).expect("home");
    let user = xdg.join("opencode/opencode.json");
    // One note, which every refused turn must leave unread for the next.
    let sent = ok(
        &repo,
        "orchestrator",
        &[
            "msg",
            "send",
            "--to",
            "lane-agent",
            "--kind",
            "note",
            "--text",
            "hi",
        ],
    );
    let note = sent["data"]["message"]["messageId"]
        .as_str()
        .expect("id")
        .to_string();
    let turn = |env: &[(&str, &str)]| {
        let _ = fs::remove_file(lane.join("opencode.args"));
        let _ = fs::remove_file(lane.join("fake-opencode-preflight"));
        let out = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
            .current_dir(graph(&repo))
            .arg("--root")
            .arg(graph(&repo))
            .args(["--as", "launcher"])
            .args(common::agent_turn_args(
                &graph(&repo),
                &[
                    "turn",
                    "lane-agent",
                    "--harness",
                    "opencode",
                    "--role",
                    "worker",
                    "--cwd",
                    lane.to_str().unwrap(),
                    "--json",
                ],
            ))
            .env("MDKG_RUN_OPENCODE", fixture("fake-opencode.sh"))
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", &xdg)
            .env_remove("FAKE_OPENCODE_VERSION")
            .env_remove("FAKE_OPENCODE_PREFLIGHT")
            .envs(env.iter().copied())
            .output()
            .expect("run mdkg");
        let preflight =
            fs::read_to_string(lane.join("fake-opencode-preflight")).unwrap_or_default();
        (common::one_turn(&out.stdout), preflight)
    };
    let refused = |(out, _): &(Value, String), why: &str| {
        assert_eq!(out["ok"], false, "{out}");
        let message = out["diagnostics"][0]["message"]
            .as_str()
            .unwrap_or_default();
        assert!(
            message.starts_with("OpenCode worker preflight refused the turn")
                && message.contains(why),
            "{out}"
        );
        assert!(!lane.join("opencode.args").exists(), "the worker ran");
        let inbox = ok(&lane, "lane-agent", &["msg", "inbox"]);
        let ids: Vec<&str> = inbox["data"]["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .filter_map(|m| m["messageId"].as_str())
            .collect();
        assert_eq!(
            ids,
            [note.as_str()],
            "the refused turn used its mail: {inbox}"
        );
    };

    // The user's resolved config disables build.
    fs::write(&user, r#"{"agent": {"build": {"disable": true}}}"#).expect("config");
    let disabled = turn(&[]);
    refused(&disabled, "build is disabled");
    assert!(disabled.1.ends_with("agent.list\n"), "{}", disabled.1);

    fs::write(&user, r#"{"agent": {"build": {"model": "x/y"}}}"#).expect("config");
    // The preflight's server fails.
    refused(
        &turn(&[("FAKE_OPENCODE_PREFLIGHT", "fail")]),
        "private server",
    );
    // OpenCode older than 2.1.0: refused before any server starts.
    let old = turn(&[("FAKE_OPENCODE_VERSION", "1.2.0")]);
    refused(&old, "not qualified");
    assert_eq!(old.1, "opencode v1.2.0\n");

    // Otherwise the worker runs, after the full preflight.
    let (out, preflight) = turn(&[]);
    assert_eq!(out["ok"], true, "{out}");
    assert_eq!(
        preflight,
        "opencode v2.1.0\nserve\nserver.info\nmodel.list\nplugin.list\nagent.list\n"
    );
    assert!(lane.join("opencode.args").exists(), "the worker never ran");
    assert_eq!(out["data"]["shown"][0], note.as_str(), "{out}");
    let env = fs::read_to_string(lane.join("opencode.environ")).expect("environ");
    assert!(
        env.lines()
            .any(|l| l == format!("XDG_CONFIG_HOME={}", xdg.display()))
    );
}

#[test]
fn codex_and_opencode_run_in_either_role() {
    let (_dir, repo, lane) = repo();
    // The lane is on agent/lane-agent, the worker's own branch.
    for (name, harness, role) in [
        ("lane-agent", "codex", "worker"),
        ("oc-rev", "opencode", "reviewer"),
    ] {
        let (code, out) = mdkg(
            &graph(&repo),
            "launcher",
            &[
                "turn",
                name,
                "--harness",
                harness,
                "--role",
                role,
                "--cwd",
                lane.to_str().expect("utf-8"),
                "--prompt",
                "x",
                "--dry-run",
            ],
        );
        assert_eq!(code, 0, "{harness} {role}: {out}");
        assert_eq!(out["data"]["outcome"], "dry_run");
    }
}

/// Every role's Claude and Codex agent may call the mdkg MCP server's
/// `pack`, a read tool the workflow recommends (RC1 audit U4).
#[test]
fn every_role_may_call_the_pack_tool() {
    let (_dir, repo, lane) = repo();
    let lane = lane.to_str().expect("utf-8");
    let root = graph(&repo);
    let root = root.to_str().expect("utf-8");
    for (name, harness, role, cwd) in [
        ("lane-agent", "claude", "worker", lane),
        ("c-rev", "claude", "reviewer", root),
        ("lane-agent", "codex", "worker", lane),
        ("x-rev", "codex", "reviewer", root),
    ] {
        let out = ok(
            &graph(&repo),
            "launcher",
            &[
                "turn",
                name,
                "--harness",
                harness,
                "--role",
                role,
                "--cwd",
                cwd,
                "--prompt",
                "x",
                "--dry-run",
            ],
        );
        let args: Vec<&str> = out["data"]["command"]["args"]
            .as_array()
            .expect("args")
            .iter()
            .filter_map(Value::as_str)
            .collect();
        let offered = match harness {
            "claude" if role == "worker" => {
                // Native worker permissions expose the whole configured server;
                // only reviewers use an mdkg tool allowlist.
                let config = args
                    .windows(2)
                    .find(|a| a[0] == "--mcp-config")
                    .and_then(|a| serde_json::from_str::<Value>(a[1]).ok())
                    .expect("MCP config");
                assert!(config["mcpServers"]["mdkg"]["command"].is_string());
                !args.contains(&"--allowedTools")
                    && !args.contains(&"--tools")
                    && !args.contains(&"mcp__mdkg__pack")
            }
            "claude" => args.contains(&"mcp__mdkg__pack"),
            _ => args
                .iter()
                .any(|a| a.contains("enabled_tools") && a.contains("\"pack\"")),
        };
        assert!(offered, "{harness} {role}: {args:?}");
    }
}

#[test]
fn an_unknown_actor_waits_but_headless_and_reserved_invocations_are_refused() {
    let (_dir, repo, _lane) = repo();
    // Use public argv here: the `turn` fixture marker translates to this
    // interactive-capable command rather than a separate headless command.
    let (code, out) = mdkg(
        &graph(&repo),
        "launcher",
        &["agents", "up", "nobody", "--once"],
    );
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["data"]["wake"]["status"], "waiting");
    assert_eq!(out["diagnostics"][0]["severity"], "info");
    assert_eq!(out["diagnostics"][0]["code"], "run.unknown_agent");
    assert!(!repo.join(".git/mdkg/run/nobody.json").exists());
    // A headless-only request still requires an actual definition.
    let (code, out) = mdkg(
        &graph(&repo),
        "launcher",
        &["agents", "up", "nobody", "--once", "--prompt", "work"],
    );
    assert_eq!(code, 1);
    assert_eq!(out["diagnostics"][0]["code"], "usage.invalid_arguments");
    assert!(
        out["diagnostics"][0]["message"]
            .as_str()
            .unwrap()
            .contains("--prompt requires a headless definition")
    );
    assert!(!repo.join(".git/mdkg/run/nobody.json").exists());
    let (code, out) = mdkg(
        &graph(&repo),
        "launcher",
        &["agents", "up", "run:x", "--once"],
    );
    assert_eq!(code, 1);
    // A malformed name, as for a mailbox or a room (D2); not quoted.
    assert_eq!(out["diagnostics"][0]["code"], "msg.bad_id");
    assert!(
        !out["diagnostics"][0]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("run:x")
    );
    // Waiting did not create a resumable headless agent.
    let (code, out) = mdkg(&graph(&repo), "launcher", &["agents", "resume", "nobody"]);
    assert_eq!(code, 3, "{out}");
    assert_eq!(out["diagnostics"][0]["code"], "run.unknown_agent");
}

#[test]
fn install_writes_every_agents_config_and_is_idempotent() {
    let (_dir, repo, _lane) = repo();
    let projections = "agents-md,mcp:claude,mcp:codex,mcp:opencode";
    let dry = ok(
        &repo,
        "nick",
        &["install", "--enable", projections, "--dry-run"],
    );
    common::conforms("install", &dry);
    assert!(!repo.join(".mcp.json").exists());
    ok(&repo, "nick", &["install", "--enable", projections]);
    for f in [
        ".mcp.json",
        ".codex/config.toml",
        "opencode.json",
        "AGENTS.md",
    ] {
        assert!(repo.join(f).is_file(), "{f}");
    }
    assert!(!repo.join("CLAUDE.md").exists());
    let again = ok(&repo, "nick", &["install"]);
    assert!(
        again["data"]["changes"]
            .as_array()
            .expect("changes")
            .iter()
            .all(|c| c["action"] == "unchanged"),
        "{again}"
    );
}

#[test]
fn a_binary_that_isnt_utf8_is_refused_before_anything_runs() {
    use std::os::unix::ffi::OsStrExt as _;
    let (_dir, repo, lane) = repo();
    let bad = std::ffi::OsStr::from_bytes(b"/tmp/mdkg-\xff");
    for args in [
        vec![
            "turn",
            "lane-agent",
            "--harness",
            "claude",
            "--role",
            "worker",
            "--cwd",
            lane.to_str().unwrap(),
            "--prompt",
            "x",
        ],
        vec!["install"],
    ] {
        let out = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
            .current_dir(graph(&repo))
            .args(["--as", "launcher"])
            .args(common::agent_turn_args(&graph(&repo), &args))
            .arg("--binary")
            .arg(bad)
            .arg("--json")
            .output()
            .expect("turn");
        let v: Value = serde_json::from_slice(&out.stdout).expect("json");
        assert_eq!(out.status.code(), Some(1), "{args:?}: {v}");
        assert!(v.to_string().contains("isn't UTF-8"), "{v}");
    }
    assert!(!lane.join("claude.args").exists(), "no agent started");
}

#[test]
fn install_and_doctor_enforce_git_floor_before_install_writes() {
    use std::os::unix::fs::PermissionsExt as _;

    // Exercise first install: the shared repo() fixture already migrates and
    // installs, creating the coordinator this refusal must not create.
    let dir = Scratch(fs::canonicalize(common::temp::dir("mdkg-cli-git-floor")).expect("scratch"));
    let repo = dir.0.join("repo");
    fs::create_dir(&repo).expect("repo");
    sh(&repo, &["init", "-q", "-b", "main"]);
    ok(&repo, "launcher", &["init"]);
    sh(&repo, &["add", "."]);
    sh(&repo, &["commit", "-qm", "initial graph"]);
    assert!(!repo.join(".git/mdkg").exists(), "fixture is not installed");
    let shim = dir.0.join("versioned-git");
    fs::create_dir(&shim).expect("git shim directory");
    fs::write(
        shim.join("git"),
        "#!/bin/sh\nif [ \"$1\" = --version ]; then\n  printf '%s\\n' \"$TEST_GIT_VERSION\"\n  exit 0\nfi\ncase \"$TEST_GIT_VERSION\" in\n  'git version 2.39.'*|'git version 2.40.'*)\n    for arg do\n      case \"$arg\" in --attr-source=*) printf '%s\\n' 'unknown option: --attr-source' >&2; exit 129;; esac\n    done;;\nesac\nexec \"$TEST_REAL_GIT\" \"$@\"\n",
    )
    .expect("git shim");
    fs::set_permissions(shim.join("git"), fs::Permissions::from_mode(0o755)).expect("chmod");
    let real_git = &mdkg_coord_sqlite::git::git_program().0;
    let run = |version: &str, args: &[&str]| {
        let out = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
            .current_dir(&repo)
            .env("PATH", narrow_path(&shim, &[]))
            .env("TEST_GIT_VERSION", version)
            .env("TEST_REAL_GIT", real_git)
            .args(["--as", "installer"])
            .args(args)
            .arg("--json")
            .output()
            .expect("mdkg");
        let value: Value = serde_json::from_slice(&out.stdout).expect("JSON");
        (out.status.code(), value)
    };
    let config = fs::read(repo.join(".mdkg/config.json")).expect("original config");
    let git_config = fs::read(repo.join(".git/config")).expect("original Git config");
    let agents = fs::read(repo.join("AGENTS.md")).ok();
    let status = || {
        let out = isolated(Command::new(real_git).arg("-C").arg(&repo))
            .args(["status", "--porcelain", "--untracked-files=all"])
            .output()
            .unwrap();
        assert!(out.status.success());
        out.stdout
    };
    let before = status();
    let assert_unchanged = || {
        assert_eq!(status(), before, "changed repository files");
        assert_eq!(fs::read(repo.join(".mdkg/config.json")).unwrap(), config);
        assert_eq!(fs::read(repo.join(".git/config")).unwrap(), git_config);
        assert_eq!(fs::read(repo.join("AGENTS.md")).ok(), agents);
        let graph_ref = isolated(Command::new(real_git).arg("-C").arg(&repo))
            .args(["show-ref", "--verify", "--quiet", "refs/heads/mdkg-graph"])
            .output()
            .expect("check graph ref");
        assert_eq!(
            graph_ref.status.code(),
            Some(1),
            "unexpected graph ref or Git error: {graph_ref:?}"
        );
        for path in [
            ".git/mdkg",
            ".git/worktrees",
            ".mcp.json",
            ".codex",
            ".claude",
            ".agents",
            ".opencode",
            "opencode.json",
        ] {
            assert!(!repo.join(path).exists(), "unexpected creation: {path}");
        }
    };
    for version in [
        "git version 2.39.5 (Apple Git-154)",
        "git version 2.40.9",
        "unrecognized git version",
    ] {
        for args in [&["install"][..], &["install", "--undo"][..]] {
            let (code, install) = run(version, args);
            assert_eq!(code, Some(2), "{install}");
            assert_eq!(install["diagnostics"][0]["code"], "io.git_failed");
            let diagnostic = install["diagnostics"][0].to_string();
            for expected in [
                version,
                "2.41.0",
                "brew install git",
                "first on PATH",
                "install --undo",
            ] {
                assert!(diagnostic.contains(expected), "{diagnostic}");
            }
            assert_unchanged();
        }

        let (code, doctor) = run(version, &["doctor"]);
        assert_eq!(code, Some(2), "{doctor}");
        let git = doctor["data"]["checks"]
            .as_array()
            .expect("checks")
            .iter()
            .find(|c| c["name"] == "git")
            .expect("git check");
        assert_eq!(git["status"], "error", "{doctor}");
        assert!(git["summary"].as_str().unwrap().contains(version), "{git}");
        assert!(git["summary"].as_str().unwrap().contains("2.41.0"), "{git}");
        assert!(
            git["hint"].as_str().unwrap().contains("brew install git"),
            "{git}"
        );
        assert_unchanged();
    }
    // The exact boundary is accepted by both entry points. The shim delegates
    // plumbing to the host Git; this tests enforcement, not Git 2.41 itself.
    let (code, install) = run("git version 2.41.0", &["install", "--dry-run"]);
    assert_eq!(code, Some(0), "{install}");
    let (_, doctor) = run("git version 2.41.0", &["doctor"]);
    let git = doctor["data"]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "git")
        .unwrap();
    assert_eq!(git["status"], "ok", "{doctor}");
}

#[test]
fn install_uses_the_npm_launcher_name_for_every_agent_config() {
    use std::os::unix::fs::PermissionsExt as _;

    let (dir, repo, lane) = repo();
    let git_bin = git_only_bin(&dir.0);
    let package = dir.0.join("prefix/lib/node_modules/mdkg");
    let binaries = package.join("node_modules/.bin_real");
    let links = dir.0.join("prefix/bin");
    let shadow = dir.0.join("shadow");
    for path in [&binaries, &links, &shadow] {
        fs::create_dir_all(path).expect("mkdir");
    }
    let real = binaries.join("mdkg");
    fs::copy(env!("CARGO_BIN_EXE_mdkg"), &real).expect("copy binary");
    let launcher = package.join("run-mdkg.js");
    let body = "#!/usr/bin/env node\n\nconst { run } = require(\"./binary\");\nrun(\"mdkg\");\n";
    fs::write(&launcher, body).expect("launcher");
    fs::set_permissions(&launcher, fs::Permissions::from_mode(0o755)).expect("chmod");
    std::os::unix::fs::symlink(&launcher, links.join("mdkg")).expect("npm bin link");
    fs::write(shadow.join("mdkg"), "#!/bin/sh\nexit 1\n").expect("shadow");
    fs::set_permissions(shadow.join("mdkg"), fs::Permissions::from_mode(0o755)).expect("chmod");

    // Opt-in setup: the npm launcher starts its .bin_real child. Invoke that
    // child directly so this fixture needs neither Node nor an npm download.
    let config_path = repo.join(".mdkg/config.json");
    let mut config: Value =
        serde_json::from_slice(&fs::read(&config_path).expect("config")).expect("json");
    config["projections"] = serde_json::json!([]);
    fs::write(
        &config_path,
        serde_json::to_vec_pretty(&config).expect("json"),
    )
    .expect("config");
    let path = narrow_path(&git_bin, &[&links]);
    let install = |search: &OsStr, extra: &[&str], expected: &str| {
        let out = isolated(&mut Command::new(&real))
            .current_dir(&repo)
            .env("PATH", search)
            .args([
                "--as",
                "installer",
                "install",
                "--enable",
                "agents-md,mcp:claude,mcp:codex,mcp:opencode",
            ])
            .args(extra)
            .arg("--json")
            .output()
            .expect("run npm binary");
        assert!(out.status.success(), "{out:?}");
        let claude: Value =
            serde_json::from_slice(&fs::read(repo.join(".mcp.json")).expect("Claude config"))
                .expect("json");
        assert_eq!(claude["mcpServers"]["mdkg"]["command"], expected);
        assert_eq!(
            claude["mcpServers"]["mdkg"]["args"],
            serde_json::json!(["mcp"])
        );
        let codex: toml_edit::DocumentMut = fs::read_to_string(repo.join(".codex/config.toml"))
            .expect("Codex config")
            .parse()
            .expect("toml");
        assert_eq!(
            codex["mcp_servers"]["mdkg"]["command"].as_str(),
            Some(expected)
        );
        let opencode: Value =
            serde_json::from_slice(&fs::read(repo.join("opencode.json")).expect("OpenCode config"))
                .expect("json");
        assert_eq!(
            opencode["mcp"]["mdkg"]["command"],
            serde_json::json!([expected, "mcp"])
        );
        let agents = fs::read_to_string(repo.join("AGENTS.md")).expect("agent instructions");
        assert!(agents.contains("`mdkg msg inbox`"), "{agents}");
    };
    install(&path, &[], "mdkg");
    install(&path, &["--binary", "/opt/custom/mdkg"], "/opt/custom/mdkg");
    // A matching launcher later on PATH cannot override the first program.
    let absolute = real.to_str().expect("utf-8");
    install(&narrow_path(&git_bin, &[&shadow, &links]), &[], absolute);
    // The expected layout alone is not enough: the launcher must run mdkg.
    fs::write(&launcher, "#!/usr/bin/env node\nrun(\"other\");\n").expect("wrong launcher");
    install(&path, &[], absolute);
    fs::write(&launcher, body).expect("restore launcher");
    install(&path, &[], "mdkg");

    // The shared selection rule also reaches the worker's MCP configuration.
    // Its mail preflight invokes the launcher. Stand in for Node by forwarding
    // that launcher's arguments to this fixture's binary, never a real Node.
    let node = links.join("node");
    fs::write(&node, "#!/bin/sh\nshift\nexec \"$TEST_NPM_REAL\" \"$@\"\n").expect("fake Node");
    fs::set_permissions(&node, fs::Permissions::from_mode(0o755)).expect("chmod");
    let out = isolated(&mut Command::new(&real))
        .current_dir(graph(&repo))
        .env("PATH", &path)
        .env("TEST_NPM_REAL", &real)
        .args(["--as", "launcher"])
        .args(common::agent_turn_args(
            &graph(&repo),
            &[
                "turn",
                "lane-agent",
                "--harness",
                "claude",
                "--role",
                "worker",
                "--cwd",
                lane.to_str().unwrap(),
                "--prompt",
                "x",
                "--dry-run",
                "--json",
            ],
        ))
        .output()
        .expect("preview worker");
    assert!(out.status.success(), "{out:?}");
    let dry = common::one_turn(&out.stdout);
    let args = dry["data"]["command"]["args"].as_array().expect("args");
    let index = args
        .iter()
        .position(|arg| arg == "--mcp-config")
        .expect("mcp");
    let mcp: Value = serde_json::from_str(args[index + 1].as_str().expect("config")).expect("json");
    assert_eq!(mcp["mcpServers"]["mdkg"]["command"], "mdkg");
}

#[test]
fn install_and_run_name_the_binary_by_its_bare_name_on_path_else_as_invoked() {
    let (dir, repo, lane) = repo();
    let git_bin = git_only_bin(&dir.0);
    // A copy under another name, as after the rename to `mdkg`, where PATH
    // could still find 0.6.0's `mdkg`; and a link to it, as Homebrew's
    // bin/ links into its versioned Cellar.
    let cellar = dir.0.join("cellar");
    let links = dir.0.join("links");
    let elsewhere = dir.0.join("elsewhere");
    for d in [&cellar, &links, &elsewhere] {
        fs::create_dir_all(d).expect("mkdir");
    }
    let real = cellar.join("some-mdkg");
    fs::copy(env!("CARGO_BIN_EXE_mdkg"), &real).expect("copy");
    let link = links.join("some-mdkg");
    std::os::unix::fs::symlink(&real, &link).expect("link");
    // Another program of the same name.
    fs::write(elsewhere.join("some-mdkg"), "#!/bin/sh\nexit 1\n").expect("w");
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(
            elsewhere.join("some-mdkg"),
            fs::Permissions::from_mode(0o755),
        )
        .expect("chmod");
    }
    let install = |program: &Path, path: &Path, extra: &[&str]| -> Value {
        let out = isolated(&mut Command::new(program))
            .current_dir(&repo)
            .env("PATH", narrow_path(&git_bin, &[path]))
            .args([
                "--as",
                "nick",
                "install",
                "--enable",
                "mcp:claude,mcp:codex,mcp:opencode",
            ])
            .args(extra)
            .arg("--json")
            .output()
            .expect("run the copy");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stdout)
        );
        serde_json::from_slice(&fs::read(repo.join(".mcp.json")).expect("r")).expect("json")
    };
    let command = |mcp: &Value| mcp["mcpServers"]["mdkg"]["command"].clone();
    // Its name on PATH is this same file, through the link: the bare name,
    // which survives an upgrade that moves the file.
    assert_eq!(command(&install(&link, &links, &[])), "some-mdkg");
    assert_eq!(command(&install(&real, &links, &[])), "some-mdkg");
    // PATH finds another program under the name: the path as invoked,
    // which macOS reports as the link itself and Linux as where it leads
    // (/proc/self/exe).
    let invoked = if cfg!(target_os = "linux") {
        fs::canonicalize(&link).expect("real")
    } else {
        link.clone()
    };
    let mcp = install(&link, &elsewhere, &[]);
    assert_eq!(command(&mcp), invoked.to_str().expect("utf-8"));
    // A relative or empty entry ahead of the match depends on the folder a
    // command runs in: the path, never the bare name.
    for shadow in ["tools", ""] {
        let path = narrow_path(&git_bin, &[Path::new(shadow), &links]);
        let out = isolated(&mut Command::new(&link))
            .current_dir(&repo)
            .env("PATH", path)
            .args(["--as", "nick", "install", "--json"])
            .output()
            .expect("turn");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stdout)
        );
        let mcp: Value =
            serde_json::from_slice(&fs::read(repo.join(".mcp.json")).expect("r")).expect("json");
        assert_eq!(
            command(&mcp),
            invoked.to_str().expect("utf-8"),
            "{shadow:?}"
        );
    }
    install(&link, &elsewhere, &[]);
    let codex: toml_edit::DocumentMut = fs::read_to_string(repo.join(".codex/config.toml"))
        .expect("r")
        .parse()
        .expect("toml");
    assert_eq!(
        codex["mcp_servers"]["mdkg"]["command"].as_str(),
        invoked.to_str()
    );
    let oc: Value =
        serde_json::from_slice(&fs::read(repo.join("opencode.json")).expect("r")).expect("json");
    assert_eq!(
        oc["mcp"]["mdkg"]["command"][0],
        invoked.to_str().expect("utf-8")
    );
    // --binary names it as given.
    let mcp = install(&link, &elsewhere, &["--binary", "/opt/x/mdkg"]);
    assert_eq!(command(&mcp), "/opt/x/mdkg");
    // A turn's MCP server follows the same rule.
    let out = isolated(&mut Command::new(&link))
        .current_dir(graph(&repo))
        .env("PATH", narrow_path(&git_bin, &[&elsewhere]))
        .args(["--as", "launcher"])
        .args(common::agent_turn_args(
            &graph(&repo),
            &[
                "turn",
                "lane-agent",
                "--harness",
                "claude",
                "--role",
                "worker",
                "--cwd",
                lane.to_str().unwrap(),
                "--prompt",
                "x",
                "--dry-run",
                "--json",
            ],
        ))
        .output()
        .expect("turn");
    let dry = common::one_turn(&out.stdout);
    let args = dry["data"]["command"]["args"].as_array().expect("args");
    let i = args.iter().position(|a| a == "--mcp-config").expect("mcp");
    let cfg: Value = serde_json::from_str(args[i + 1].as_str().expect("s")).expect("json");
    assert_eq!(
        cfg["mcpServers"]["mdkg"]["command"],
        invoked.to_str().expect("utf-8"),
        "{dry}"
    );
}

/// Round 6: install writes its lease record just after its claim, but its
/// marker before; a turn refused in between is still told install holds it.
#[test]
fn a_turn_refused_before_installs_record_is_written_still_says_why() {
    let (_dir, repo, lane) = repo();
    let cwd = lane.to_str().expect("utf-8");
    let first = [
        "turn",
        "lane-agent",
        "--harness",
        "claude",
        "--role",
        "worker",
        "--cwd",
        cwd,
        "--prompt",
        "hello",
    ];
    ok(&graph(&repo), "launcher", &first);
    let holder = format!("nick@install-{}", std::process::id());
    ok(&lane, &holder, &["claim", "run:lane-agent"]);
    let marker = mdkg_run::state::InstallMarker {
        record: mdkg_run::turn::lease_record(holder.clone()),
        top: fs::canonicalize(&lane).expect("lane"),
        until_ms: mdkg_run::state::now_ms() + mdkg_run::state::MARKER_TTL_MS,
    };
    let held =
        mdkg_run::state::mark_installing(&repo.join(".git/mdkg/run"), &marker).expect("mark");
    let (code, out) = mdkg(
        &graph(&repo),
        "launcher",
        &["turn", "lane-agent", "--prompt", "hi"],
    );
    assert_ne!(code, 0, "{out}");
    assert_eq!(out["diagnostics"][0]["code"], "coord.lease_held", "{out}");
    assert!(
        out["diagnostics"][0]["hint"]
            .as_str()
            .is_some_and(|h| h.starts_with(mdkg_run::turn::INSTALLING)),
        "{out}"
    );
    drop(held);
    ok(&lane, &holder, &["release", "run:lane-agent"]);
}

/// Round 6: the launcher's `holds` asks about a node file as its node, so a
/// copy of a node someone else holds isn't held, whatever the copy's lease.
#[test]
fn holds_with_a_node_sees_it_held_under_another_path() {
    if common::rerun_without_launcher_env("holds_with_a_node_sees_it_held_under_another_path") {
        return;
    }
    use mdkg_run::mail::{CliMail, Mail};
    let (_dir, repo, _lane) = repo();
    ok(&repo, "w1", &["claim", "task-1"]);
    let shown = ok(&repo, "w1", &["show", "task-1"]);
    let id = shown["data"]["node"]["id"].as_str().expect("migrated id");
    let copy = ".mdkg/work/task-1-copy.md";
    ok(&repo, "w2", &["claim", "--new", copy]);
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_mdkg"));
    let mut mail = CliMail::new(exe, graph(&repo), "launcher".into());
    assert!(mail.holds("w2", copy, None).expect("holds"));
    let as_node = mail
        .holds("w2", copy, Some(id))
        .expect_err("the node's real path differs from the copy's lease");
    assert_eq!(as_node.kind, mdkg_run::error::ErrorKind::Conflict);
    assert_eq!(as_node.code, "coord.lease_held");
    assert_eq!(
        as_node.message,
        format!("the node {id} is at another path here, not at the scope asked for")
    );
}

/// Round 6: a turn lease is renewed mid-turn by `renew`, since claiming a
/// turn one holds is refused; another launcher can't renew it.
#[test]
fn a_turn_lease_renews_mid_turn() {
    if common::rerun_without_launcher_env("a_turn_lease_renews_mid_turn") {
        return;
    }
    use mdkg_run::mail::{CliMail, Mail};
    let (_dir, repo, _lane) = repo();
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_mdkg"));
    let mut mine = CliMail::new(exe.clone(), graph(&repo), "launcher".into());
    let mut other = CliMail::new(exe, graph(&repo), "launcher".into());
    mine.claim("run:lane-agent").expect("claim");
    assert!(mine.claim("run:lane-agent").is_err());
    // Renewal reads the turn's deadline from `holds`, answered locally.
    let held = ok(&graph(&repo), &mine.holder(), &["holds", "run:lane-agent"]);
    assert_eq!(held["data"]["holds"], true, "{held}");
    assert!(held["data"]["lease"]["deadline_ms"].is_u64(), "{held}");
    mine.renew("run:lane-agent").expect("renew");
    mine.renew("run:lane-agent").expect("renew again");
    let e = other.renew("run:lane-agent").expect_err("held");
    assert_eq!(e.code, "coord.lease_held", "{e:?}");
    mine.release("run:lane-agent").expect("release");
    // Lapsed or released: taken again.
    mine.renew("run:lane-agent").expect("taken again");
}

#[test]
fn install_prints_no_escape_sequences_from_a_file_name() {
    // A skill's file names are anyone's text: install's listing escapes
    // them (lease-guard, 2026-10-22).
    let (_dir, repo, _lane) = repo();
    let skill = repo.join(".mdkg/skills/review");
    fs::create_dir_all(&skill).expect("skill");
    fs::write(skill.join("SKILL.md"), "---\nname: review\n---\nReview.\n").expect("w");
    fs::write(skill.join("x\u{1b}]0;owned\u{7}.md"), "x").expect("w");
    fs::write(skill.join("y\nnote: forged\u{202e}.md"), "y").expect("w");
    let out = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
        .current_dir(&repo)
        .arg("--root")
        .arg(&repo)
        .args(["--as", "nick", "install", "--dry-run"])
        .output()
        .expect("install");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}");
    assert!(stdout.contains("review/x"), "{stdout}");
    assert!(
        !stdout.contains('\u{1b}') && !stdout.contains('\u{7}'),
        "{stdout:?}"
    );
    // Nor a line break or a right-to-left mark (round 6).
    assert!(!stdout.contains("\nnote: forged"), "{stdout:?}");
    assert!(!stdout.contains('\u{202e}'), "{stdout:?}");
}

/// Runs mdkg without --json: exit code
/// and stderr.
fn mdkg_text(root: &Path, args: &[&str]) -> (i32, String) {
    let out = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
        .current_dir(root)
        .arg("--root")
        .arg(root)
        .args(["--as", "launcher"])
        .args(common::agent_turn_args(root, args))
        .output()
        .expect("run mdkg");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn watch_refuses_zero_waits_and_turns() {
    let (_dir, repo, _lane) = repo();
    for (args, flag) in [
        (&["turn", "a", "--watch", "--wait", "0"][..], "--wait"),
        (
            &["turn", "a", "--watch", "--max-turns", "0"][..],
            "--max-turns",
        ),
    ] {
        let (code, err) = mdkg_text(&graph(&repo), args);
        assert_eq!(code, 1, "{args:?}: {err}");
        assert!(err.contains(flag), "{args:?}: {err}");
    }
}

#[test]
fn zero_limits_and_watch_only_flags_are_refused() {
    let (_dir, repo, _lane) = repo();
    // Checked before anything is read: even where no repository is.
    let bare = common::temp::dir("mdkg-run-bare");
    fs::create_dir_all(bare.join(".mdkg")).expect("bare");
    fs::copy(
        graph(&repo).join(".mdkg/config.json"),
        bare.join(".mdkg/config.json"),
    )
    .expect("config");
    let (code, out) = mdkg(&bare, "launcher", &["turn", "a", "--limit", "0"]);
    fs::remove_dir_all(&bare).ok();
    assert_eq!(
        (code, out["diagnostics"][0]["code"].as_str()),
        (1, Some("usage.bad_limit")),
        "{out}"
    );
    // A limit that isn't positive: the same code as every limit (D2).
    for args in [
        &["turn", "a", "--limit", "0"][..],
        &["turn", "a", "--limit", "-1"],
    ] {
        let (code, out) = mdkg(&graph(&repo), "launcher", args);
        assert_eq!(code, 1, "{args:?}: {out}");
        assert_eq!(
            out["diagnostics"][0]["code"], "usage.bad_limit",
            "{args:?}: {out}"
        );
    }
}

#[test]
fn install_refuses_a_graph_below_the_top_and_run_refuses_a_folder_outside() {
    let (dir, repo, _lane) = repo();
    let (code, out) = mdkg(
        &graph(&repo),
        "launcher",
        &[
            "turn",
            "a",
            "--harness",
            "claude",
            "--role",
            "reviewer",
            "--cwd",
            dir.0.to_str().expect("utf-8"),
        ],
    );
    assert_eq!(code, 1, "{out}");
    assert_eq!(out["diagnostics"][0]["code"], "run.cwd_outside");
    // 0.7 install requires a root graph; a graph only in a subfolder is
    // unsupported, even when that nested graph has a valid 0.7 identity.
    let nested = dir.0.join("nested");
    fs::create_dir_all(nested.join("sub")).expect("mkdir");
    copy_tree(&repo.join(".mdkg"), &{
        let d = nested.join("sub/.mdkg");
        fs::create_dir_all(&d).expect("mkdir");
        d
    });
    sh(&nested, &["init", "-q", "-b", "main"]);
    sh(&nested, &["add", "."]);
    sh(&nested, &["commit", "-q", "-m", "init"]);
    assert!(!nested.join(".mdkg").exists());
    let (code, out) = mdkg(&nested.join("sub"), "nick", &["install"]);
    assert_eq!(code, 1, "{out}");
    assert_eq!(out["diagnostics"][0]["code"], "install.unsupported_layout");
    assert_eq!(
        out["diagnostics"][0]["message"],
        "the graph is in sub below the worktree's top; install needs it at the top"
    );
    assert!(!nested.join(".mdkg").exists());
    assert!(!nested.join(".worktrees/graph").exists());
    assert!(!nested.join(".mcp.json").exists());
}

#[test]
fn run_refuses_folders_that_arent_this_repositorys_worktrees() {
    let (dir, repo, lane) = repo();
    let nested = lane.join("nested");
    fs::create_dir_all(&nested).expect("mkdir");
    sh(&nested, &["init", "-q", "-b", "main"]);
    let missing = dir.0.join("missing");
    let git_dir = repo.join(".git");
    for cwd in [&missing, &git_dir, &nested] {
        let (code, out) = mdkg(
            &graph(&repo),
            "launcher",
            &[
                "turn",
                "a",
                "--harness",
                "claude",
                "--role",
                "reviewer",
                "--cwd",
                cwd.to_str().expect("utf-8"),
                "--dry-run",
            ],
        );
        assert_eq!(code, 1, "{cwd:?}: {out}");
        assert_eq!(out["diagnostics"][0]["code"], "run.cwd_outside", "{cwd:?}");
    }
}

#[test]
fn run_and_install_refuse_an_inherited_git_dir() {
    let (_dir, repo, _lane) = repo();
    for args in [&["turn", "a"][..], &["install", "--dry-run"][..]] {
        let out = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
            .current_dir(&repo)
            .env("GIT_DIR", repo.join(".git"))
            .arg("--root")
            .arg(&repo)
            .args(["--as", "launcher"])
            .args(common::agent_turn_args(&repo, args))
            .arg("--json")
            .output()
            .expect("run mdkg");
        let envelope: Value = serde_json::from_slice(&out.stdout).expect("json");
        assert_eq!(out.status.code(), Some(1), "{args:?}: {envelope}");
        assert_eq!(envelope["diagnostics"][0]["code"], "run.unsupported");
        assert!(envelope.to_string().contains("GIT_DIR"));
    }
}

#[test]
fn run_worktree_gives_a_worker_its_own_and_the_main_checkout_is_refused() {
    let (_dir, repo, _lane) = repo();
    let args = [
        "turn",
        "w",
        "--harness",
        "claude",
        "--role",
        "worker",
        "--worktree",
    ];
    let out = ok(&graph(&repo), "launcher", &args);
    assert_eq!(out["data"]["outcome"], "idle", "{out}");
    let made = repo.join(".worktrees/w");
    assert!(made.join(".git").is_file(), "a linked worktree");
    let exclude = fs::read_to_string(repo.join(".git/info/exclude")).unwrap_or_default();
    assert!(exclude.lines().any(|l| l == "/.worktrees/w/"), "{exclude}");
    let branch = Command::new("git")
        .arg("-C")
        .arg(&made)
        .args(["symbolic-ref", "--short", "HEAD"])
        .output()
        .expect("git");
    assert_eq!(String::from_utf8_lossy(&branch.stdout).trim(), "agent/w");
    // Again: the same worktree, reused.
    ok(&graph(&repo), "launcher", &["turn", "w", "--worktree"]);
    // A dry run plans a new agent's worktree and makes nothing: no folder,
    // no branch, no agent.
    let out = ok(
        &graph(&repo),
        "launcher",
        &[
            "turn",
            "v",
            "--harness",
            "claude",
            "--role",
            "worker",
            "--worktree",
            "--dry-run",
            "--prompt",
            "x",
        ],
    );
    assert_eq!(out["data"]["outcome"], "dry_run", "{out}");
    assert_eq!(
        out["data"]["command"]["cwd"],
        repo.join(".worktrees/v").to_str().expect("utf-8"),
        "{out}"
    );
    assert!(!repo.join(".worktrees/v").exists());
    assert!(!repo.join(".git/mdkg/run/v.json").exists());
    let branches = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["branch", "--list", "agent/v"])
        .output()
        .expect("git");
    assert!(branches.stdout.is_empty(), "no branch made");
    // The plan makes the real run's branch checks: a branch another
    // worktree has checked out (the lane holds agent/lane-agent), and a
    // protected one, are refused, as the real run would be (review of
    // b9bc3d0).
    for args in [
        &[
            "turn",
            "lane-agent",
            "--harness",
            "claude",
            "--role",
            "worker",
            "--worktree",
            "--dry-run",
            "--prompt",
            "x",
        ][..],
        &[
            "turn",
            "p",
            "--harness",
            "claude",
            "--role",
            "worker",
            "--worktree",
            "--protect",
            "agent/p",
            "--dry-run",
            "--prompt",
            "x",
        ][..],
        // With no mail and no prompt, before the turn could return idle
        // (review of 92f3ef5).
        &[
            "turn",
            "lane-agent",
            "--harness",
            "claude",
            "--role",
            "worker",
            "--worktree",
            "--dry-run",
        ][..],
    ] {
        let (code, out) = mdkg(&graph(&repo), "launcher", args);
        assert_eq!(code, 1, "{args:?}: {out}");
        assert_eq!(out["diagnostics"][0]["code"], "run.branch_refused", "{out}");
    }
    assert!(!repo.join(".worktrees/lane-agent").exists());
    assert!(!repo.join(".worktrees/p").exists());
    // An existing one's dry run plans the worktree it has.
    let out = ok(
        &graph(&repo),
        "launcher",
        &["turn", "w", "--worktree", "--dry-run", "--prompt", "x"],
    );
    assert_eq!(
        out["data"]["command"]["cwd"],
        made.to_str().expect("utf-8"),
        "{out}"
    );
    // Not with --cwd.
    let (code, _) = mdkg(
        &graph(&repo),
        "launcher",
        &[
            "turn",
            "v",
            "--harness",
            "claude",
            "--role",
            "worker",
            "--worktree",
            "--cwd",
            repo.to_str().expect("utf-8"),
        ],
    );
    assert_eq!(code, 1);
    // This fixture deliberately wrote an invalid definition; unlike add/edit,
    // the fixture builder does not reject it before publication.
    fs::remove_file(graph(&repo).join(".mdkg/agents/v.json")).unwrap();
    // The main checkout now holds a worktree: no worker may use it.
    let cwd = repo.to_str().expect("utf-8");
    let (code, out) = mdkg(
        &graph(&repo),
        "launcher",
        &[
            "turn",
            "m",
            "--harness",
            "claude",
            "--role",
            "worker",
            "--cwd",
            cwd,
            "--prompt",
            "x",
        ],
    );
    assert_eq!(code, 1, "{out}");
    assert_eq!(out["diagnostics"][0]["code"], "run.cwd_outside");
}

#[test]
fn the_configs_orchestration_names_place_a_workers_branch_and_worktree() {
    let (dir, repo, lane) = repo();
    // The launcher reads them from its graph root's config.
    let config = graph(&repo).join(".mdkg/config.json");
    let mut cfg: Value = serde_json::from_slice(&fs::read(&config).expect("config")).expect("json");
    cfg["orchestration"] =
        serde_json::json!({"branch": "lanes/{agent}", "worktree": "../agents/wt-{agent}"});
    fs::write(&config, cfg.to_string()).expect("w");
    commit_graph_config(&graph(&repo));
    let out = ok(
        &graph(&repo),
        "launcher",
        &[
            "turn",
            "w",
            "--harness",
            "claude",
            "--role",
            "worker",
            "--worktree",
        ],
    );
    assert_eq!(out["data"]["outcome"], "idle", "{out}");
    let made = dir.0.join("agents/wt-w");
    assert!(
        made.join(".git").is_file(),
        "a linked worktree at the pattern"
    );
    let branch = Command::new("git")
        .arg("-C")
        .arg(&made)
        .args(["symbolic-ref", "--short", "HEAD"])
        .output()
        .expect("git");
    assert_eq!(String::from_utf8_lossy(&branch.stdout).trim(), "lanes/w");
    // Under these names, a worker on the default pattern's branch isn't on
    // its own.
    let (code, out) = mdkg(
        &graph(&repo),
        "launcher",
        &[
            "turn",
            "lane-agent",
            "--harness",
            "claude",
            "--role",
            "worker",
            "--cwd",
            lane.to_str().expect("utf-8"),
            "--prompt",
            "x",
        ],
    );
    assert_eq!(code, 1, "{out}");
    assert_eq!(out["diagnostics"][0]["code"], "run.branch_refused");
    assert!(out.to_string().contains("lanes/lane-agent"), "{out}");
    // Nothing goes inside the git folder, through a parent that doesn't
    // exist yet either, nor inside another worktree (the lane's).
    for (pattern, made) in [
        (
            "../repo/.git/agent-trees/{agent}",
            "repo/.git/agent-trees/g",
        ),
        ("../lane/children/{agent}", "lane/children/g"),
    ] {
        cfg["orchestration"] = serde_json::json!({"worktree": pattern});
        fs::write(&config, cfg.to_string()).expect("w");
        commit_graph_config(&graph(&repo));
        let (code, out) = mdkg(
            &graph(&repo),
            "launcher",
            &[
                "turn",
                "g",
                "--harness",
                "claude",
                "--role",
                "worker",
                "--worktree",
            ],
        );
        assert_eq!(code, 1, "{pattern}: {out}");
        assert!(!dir.0.join(made).exists(), "{pattern}");
    }
    // Nor around a registered worktree whose folder is gone (not pruned).
    let child = dir.0.join("gone/h/child");
    sh(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "child",
            child.to_str().expect("utf-8"),
            "main",
        ],
    );
    fs::remove_dir_all(dir.0.join("gone")).expect("rm");
    cfg["orchestration"] = serde_json::json!({"worktree": "../gone/{agent}"});
    fs::write(&config, cfg.to_string()).expect("w");
    commit_graph_config(&graph(&repo));
    let (code, out) = mdkg(
        &graph(&repo),
        "launcher",
        &[
            "turn",
            "h",
            "--harness",
            "claude",
            "--role",
            "worker",
            "--worktree",
        ],
    );
    assert_eq!(code, 1, "{out}");
    assert!(out.to_string().contains("overlap"), "{out}");
    assert!(!dir.0.join("gone/h").exists());
    // A worktree deep in the main checkout excludes only itself.
    cfg["orchestration"] = serde_json::json!({"worktree": "src/wt-{agent}"});
    fs::write(&config, cfg.to_string()).expect("w");
    commit_graph_config(&graph(&repo));
    ok(
        &graph(&repo),
        "launcher",
        &[
            "turn",
            "s",
            "--harness",
            "claude",
            "--role",
            "worker",
            "--worktree",
        ],
    );
    let exclude = fs::read_to_string(repo.join(".git/info/exclude")).unwrap_or_default();
    assert!(exclude.lines().any(|l| l == "/src/wt-s/"), "{exclude}");
    assert!(!exclude.lines().any(|l| l == "/src/"), "{exclude}");
    // A pattern that could share a folder is refused before anything runs.
    cfg["orchestration"] = serde_json::json!({"worktree": "{agent}/wt"});
    fs::write(&config, cfg.to_string()).expect("w");
    commit_graph_config(&graph(&repo));
    let (code, out) = mdkg(
        &graph(&repo),
        "launcher",
        &[
            "turn",
            "x",
            "--harness",
            "claude",
            "--role",
            "worker",
            "--worktree",
        ],
    );
    assert_eq!(code, 1, "{out}");
    assert!(out.to_string().contains("orchestration.worktree"), "{out}");
    assert!(!dir.0.join("x").exists());
}

#[test]
fn with_a_declared_subgraph_mcp_opens_it_and_a_worker_there_is_refused() {
    let (dir, _repo, _lane) = repo();
    let nested = dir.0.join("nested");
    fs::create_dir_all(nested.join("sub/.mdkg")).expect("mkdir");
    copy_tree(&_repo.join(".mdkg"), &nested.join("sub/.mdkg"));
    sh(&nested, &["init", "-q", "-b", "main"]);
    // The supported 0.7 layout has a root graph declaring this workspace.
    // The rootless layout's refusal is pinned by
    // install_refuses_a_graph_below_the_top_and_run_refuses_a_folder_outside.
    ok(&nested, "launcher", &["init"]);
    let config = nested.join(".mdkg/config.json");
    let mut cfg: Value = serde_json::from_slice(&fs::read(&config).expect("config")).expect("json");
    cfg["workspaces"]["root"] = serde_json::json!({"path": "."});
    cfg["workspaces"]["sub"] = serde_json::json!({"path": "sub"});
    fs::write(&config, cfg.to_string()).expect("config");
    sh(&nested, &["add", "."]);
    sh(&nested, &["commit", "-q", "-m", "init"]);
    // Exercise normal config validation as well as the raw identity reader.
    ok(&nested, "launcher", &["validate"]);
    let lane = dir.0.join("nested-lane");
    sh(
        &nested,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "lane",
            lane.to_str().expect("utf-8"),
            "main",
        ],
    );
    let review = [
        "turn",
        "r",
        "--harness",
        "claude",
        "--role",
        "reviewer",
        "--cwd",
        lane.to_str().expect("utf-8"),
        "--prompt",
        "x",
        "--dry-run",
    ];
    // Never from a checkout of main: the hint names the graph worktree,
    // with the graph's folder in it.
    let (code, refused) = mdkg(&nested.join("sub"), "launcher", &review);
    assert_eq!(code, 1, "{refused}");
    assert_eq!(refused["diagnostics"][0]["code"], "run.branch_refused");
    let hint = refused["diagnostics"][0]["hint"].as_str().expect("hint");
    assert!(
        hint.contains("worktree add .worktrees/graph -b mdkg-graph main")
            && hint.contains("/.worktrees/graph/sub agents up"),
        "{hint}"
    );
    let graph = nested.join(".worktrees/graph");
    ok(&nested, "launcher", &["install"]);
    let sub = graph.join("sub");
    let out = ok(&sub, "launcher", &review);
    let want = format!(
        "\\\"--root\\\",\\\"{}\\\",\\\"mcp\\\"",
        fs::canonicalize(&sub).expect("sub").display()
    );
    assert!(out.to_string().contains(&want), "{want} not in {out}");
    // Pinned to the worktree the graph root is in, not the agent's.
    let pin = format!(
        "\\\"MDKG_TRUSTED_WORKTREE\\\":\\\"{}\\\"",
        fs::canonicalize(&graph).expect("graph").display()
    );
    assert!(out.to_string().contains(&pin), "{pin} not in {out}");
    // A worker in the checkout mdkg runs from (the graph's worktree).
    let (code, refused) = mdkg(
        &sub,
        "launcher",
        &[
            "turn",
            "w",
            "--harness",
            "claude",
            "--role",
            "worker",
            "--cwd",
            graph.to_str().expect("utf-8"),
            "--prompt",
            "x",
        ],
    );
    assert_eq!(code, 1, "{refused}");
    assert_eq!(refused["diagnostics"][0]["code"], "run.cwd_outside");
    assert!(
        refused.to_string().contains("checkout mdkg runs from"),
        "{refused}"
    );
}

#[test]
fn worktree_names_never_collide_and_a_refused_run_leaves_nothing() {
    let (_dir, repo, _lane) = repo();
    for name in ["codex:pr19", "codex-pr19"] {
        ok(
            &graph(&repo),
            "launcher",
            &[
                "turn",
                name,
                "--harness",
                "claude",
                "--role",
                "worker",
                "--worktree",
            ],
        );
    }
    assert!(repo.join(".worktrees/codex%3Apr19").is_dir());
    assert!(repo.join(".worktrees/codex-pr19").is_dir());
    // Refused before anything is made: a bad name, a new agent with no role.
    for args in [
        &[
            "turn",
            "Upper",
            "--harness",
            "claude",
            "--role",
            "worker",
            "--worktree",
        ][..],
        &["turn", "norole", "--harness", "claude", "--worktree"][..],
    ] {
        let (code, _) = mdkg(&graph(&repo), "launcher", args);
        assert_eq!(code, 1, "{args:?}");
    }
    assert!(!repo.join(".worktrees/Upper").exists());
    assert!(!repo.join(".worktrees/norole").exists());
}

#[test]
fn a_refused_worktree_run_makes_nothing_and_a_linked_name_is_refused() {
    let (_dir, repo, _lane) = repo();
    // A name git can't use as a branch.
    let (code, out) = mdkg(
        &graph(&repo),
        "launcher",
        &[
            "turn",
            "a..b",
            "--harness",
            "claude",
            "--role",
            "worker",
            "--worktree",
        ],
    );
    assert_eq!(code, 1, "{out}");
    assert!(out.to_string().contains("can't name a git branch"), "{out}");
    assert!(!repo.join(".worktrees/a..b").exists());
    // `.worktrees/b` a link to agent a's worktree: never shared.
    ok(
        &graph(&repo),
        "launcher",
        &[
            "turn",
            "a",
            "--harness",
            "claude",
            "--role",
            "worker",
            "--worktree",
        ],
    );
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(repo.join(".worktrees/a"), repo.join(".worktrees/b"))
            .expect("link");
        let (code, out) = mdkg(
            &graph(&repo),
            "launcher",
            &[
                "turn",
                "b",
                "--harness",
                "claude",
                "--role",
                "worker",
                "--worktree",
            ],
        );
        assert_eq!(code, 1, "{out}");
        assert!(
            out.to_string()
                .contains("isn't one of the repository's worktrees"),
            "{out}"
        );
    }
}

/// A plain `run` that changes an existing agent keeps the change even when
/// the turn is idle, so the next wake runs with it; a dry run changes
/// nothing (RC1 audit O3).
#[test]
fn an_idle_run_still_saves_the_agents_new_role() {
    let (_dir, repo, _lane) = repo();
    let saved = |name: &str| -> Value {
        serde_json::from_slice(
            &fs::read(repo.join(".git/mdkg/run").join(format!("{name}.json"))).expect("state"),
        )
        .expect("json")
    };
    let out = ok(
        &graph(&repo),
        "launcher",
        &[
            "turn",
            "coder",
            "--harness",
            "claude",
            "--role",
            "worker",
            "--worktree",
        ],
    );
    assert_eq!(out["data"]["outcome"], "idle", "{out}");
    assert_eq!(saved("coder")["spec"]["role"], "worker");
    let out = ok(
        &graph(&repo),
        "launcher",
        &["turn", "coder", "--role", "reviewer", "--model", "haiku"],
    );
    assert_eq!(out["data"]["outcome"], "idle", "{out}");
    let state = saved("coder");
    assert_eq!(state["spec"]["role"], "reviewer", "{state}");
    assert_eq!(state["spec"]["model"], "haiku", "{state}");
    // A dry run shows the change and saves none of it.
    let out = ok(
        &graph(&repo),
        "launcher",
        &[
            "turn",
            "coder",
            "--role",
            "worker",
            "--dry-run",
            "--prompt",
            "p",
        ],
    );
    assert_eq!(out["data"]["role"], "worker", "{out}");
    assert_eq!(saved("coder")["spec"]["role"], "reviewer");
    // A dry run leaves saved state alone; the next pass applies the current definition.
    ok(
        &repo,
        "orchestrator",
        &[
            "msg", "send", "--to", "coder", "--kind", "note", "--text", "hi",
        ],
    );
    let out = ok(&graph(&repo), "launcher", &["turn", "coder"]);
    assert_eq!(out["data"]["role"], "worker", "{out}");
    assert_eq!(out["data"]["outcome"], "replied", "{out}");
}

/// The operator's standing instructions come before the mail on every
/// turn, a person's own `run` and `run --watch` too, not only `agents up`
/// (RC1 audit O2).
#[test]
fn a_manual_run_shows_the_agents_standing_instructions() {
    let (_dir, repo, _lane) = repo();
    let config = graph(&repo).join(".mdkg/config.json");
    let mut cfg: Value = serde_json::from_slice(&fs::read(&config).expect("config")).expect("json");
    cfg["agents"] = serde_json::json!({
        "probe": {"harness": "claude", "role": "reviewer", "instructions": "STANDING_ORDER_7"}
    });
    fs::write(&config, cfg.to_string()).expect("w");
    commit_graph_config(&graph(&repo));
    let args = ["turn", "probe", "--harness", "claude", "--role", "reviewer"];
    let dry = ok(
        &graph(&repo),
        "launcher",
        &[&args[..], &["--dry-run", "--prompt", "THIS_RUN"]].concat(),
    );
    let prompt = dry["data"]["prompt"].as_str().expect("prompt");
    let (order, this_run) = (
        prompt.find("STANDING_ORDER_7").expect("standing order"),
        prompt.find("THIS_RUN").expect("this run's prompt"),
    );
    assert!(order < this_run, "{prompt}");
    // A real turn, and a watched one, show it too.
    ok(&graph(&repo), "launcher", &args);
    for watch in [false, true] {
        ok(
            &repo,
            "orchestrator",
            &[
                "msg", "send", "--to", "probe", "--kind", "note", "--text", "hi",
            ],
        );
        if watch {
            let (code, err) = mdkg_text(
                &graph(&repo),
                &["turn", "probe", "--watch", "--max-turns", "1"],
            );
            assert_eq!(code, 0, "{err}");
        } else {
            ok(&graph(&repo), "launcher", &["turn", "probe"]);
        }
        let shown = fs::read_to_string(graph(&repo).join("claude.prompt")).expect("prompt");
        assert!(shown.contains("STANDING_ORDER_7"), "watch {watch}: {shown}");
        fs::remove_file(graph(&repo).join("claude.prompt")).expect("rm");
    }
}

#[test]
fn worktree_moves_an_existing_agent_there() {
    let (_dir, repo, lane) = repo();
    let cwd = lane.to_str().expect("utf-8");
    ok(
        &graph(&repo),
        "launcher",
        &[
            "turn",
            "x",
            "--harness",
            "claude",
            "--role",
            "worker",
            "--cwd",
            cwd,
        ],
    );
    ok(
        &repo,
        "orchestrator",
        &["msg", "send", "--to", "x", "--kind", "note", "--text", "hi"],
    );
    ok(&graph(&repo), "launcher", &["turn", "x", "--worktree"]);
    let moved = repo.join(".worktrees/x");
    assert!(
        moved.join("claude.prompt").exists(),
        "the turn ran in the new worktree"
    );
    assert!(!lane.join("claude.prompt").exists());
}

#[test]
fn install_undo_puts_the_project_back_and_refuses_after_an_edit() {
    let (_dir, repo, _lane) = repo();
    let install = [
        "install",
        "--enable",
        "agents-md,mcp:claude,mcp:codex,mcp:opencode",
    ];
    fs::write(
        repo.join(".mcp.json"),
        r#"{"mcpServers": {"mine": {"command": "x"}}}"#,
    )
    .expect("w");
    let before = fs::read(repo.join(".mcp.json")).expect("r");
    ok(&repo, "nick", &install);
    // A dry run changes nothing.
    let dry = ok(&repo, "nick", &["install", "--undo", "--dry-run"]);
    common::conforms("install", &dry);
    assert!(repo.join("AGENTS.md").exists());
    ok(&repo, "nick", &["install", "--undo"]);
    assert_eq!(fs::read(repo.join(".mcp.json")).expect("r"), before);
    for f in [".codex/config.toml", "opencode.json", "AGENTS.md"] {
        assert!(!repo.join(f).exists(), "{f}");
    }
    // Install again, then edit one of its files: undo refuses.
    ok(&repo, "nick", &install);
    fs::write(repo.join("AGENTS.md"), "# mine now\n").expect("w");
    let (code, out) = mdkg(&repo, "nick", &["install", "--undo"]);
    assert_eq!(code, 1, "{out}");
    assert_eq!(out["diagnostics"][0]["code"], "install.unmergeable");
    assert!(out.to_string().contains("AGENTS.md"), "{out}");
    assert!(repo.join("opencode.json").exists(), "nothing was undone");
}

#[test]
fn a_workers_node_changes_stay_uncommitted_with_or_without_a_lease() {
    let (_dir, repo, lane) = repo();
    // The fake worker leaves a message; a node in its folder changed too.
    fs::write(lane.join("fake-commit"), "").expect("w");
    let node = lane.join(".mdkg/work/task-1-basic.md");
    let text = fs::read_to_string(&node).expect("node");
    fs::write(&node, format!("{text}\nedited by hand\n")).expect("w");
    let send = |n: &str| {
        ok(
            &repo,
            "orchestrator",
            &[
                "msg",
                "send",
                "--to",
                "lane-agent",
                "--kind",
                "handoff",
                "--text",
                n,
            ],
        )
    };
    let cwd = lane.to_str().expect("utf-8");
    let run = [
        "turn",
        "lane-agent",
        "--harness",
        "claude",
        "--role",
        "worker",
        "--cwd",
        cwd,
    ];
    let head = |r: &str| {
        let out = Command::new("git")
            .arg("-C")
            .arg(&lane)
            .args(["rev-parse", r])
            .output()
            .expect("git");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    let before = head("HEAD");
    send("one");
    // Turn finalization never stages or commits graph changes.
    let out = ok(&graph(&repo), "launcher", &run);
    assert!(out["data"]["commit_refused"].is_null(), "{out}");
    assert_eq!(head("HEAD"), before);
    let inbox = ok(&repo, "orchestrator", &["msg", "inbox"]);
    let messages = inbox["data"]["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 1, "only the worker's reply: {inbox}");
    assert_eq!(messages[0]["sender"], "lane-agent");
    assert_eq!(messages[0]["parts"][0]["text"], "claude reply");
    assert!(out["data"]["commit_note"].is_null());
    for m in messages {
        ok(
            &repo,
            "orchestrator",
            &["msg", "ack", m["messageId"].as_str().expect("id")],
        );
    }
    // A lease permits edits; it does not ask the launcher to commit.
    ok(&repo, "lane-agent", &["claim", "task-1"]);
    send("two");
    let out = ok(&graph(&repo), "launcher", &run);
    assert!(out["data"]["commit"].is_null(), "{out}");
    assert_eq!(head("HEAD"), before, "{out}");
    assert!(
        fs::read_to_string(&node)
            .expect("node")
            .contains("edited by hand")
    );
}

#[test]
fn install_and_undo_wait_for_an_agents_turn_in_the_worktree() {
    // audit, 2026-10-22: a running agent could swap a folder install
    // writes in.
    let (_dir, repo, lane) = repo();
    let run = repo.join(".git/mdkg/run");
    fs::create_dir_all(&run).expect("mkdir");
    let spec = serde_json::json!({
        "spec": {
            "name": "lane-agent",
            "harness": "claude",
            "role": "worker",
            "cwd": lane.join("sub"),
        }
    });
    fs::write(run.join("lane-agent.json"), spec.to_string()).expect("w");
    ok(&lane, "launcher", &["claim", "run:lane-agent"]);
    for args in [&["install"][..], &["install", "--undo"][..]] {
        let (code, out) = mdkg(&lane, "nick", args);
        assert_eq!(code, 5, "{args:?}: {out}");
        assert_eq!(out["diagnostics"][0]["code"], "coord.lease_held");
        assert!(out.to_string().contains("lane-agent"), "{out}");
        assert!(
            out["diagnostics"][0]["hint"]
                .as_str()
                .is_some_and(|h| h.contains("run:lane-agent")),
            "{out}"
        );
    }
    assert!(!lane.join(".mcp.json").exists());
    // A dry run writes nothing, and another worktree isn't the agent's.
    ok(&lane, "nick", &["install", "--dry-run"]);
    ok(&repo, "nick", &["install"]);
    ok(&lane, "launcher", &["release", "run:lane-agent"]);
    // Give undo a real projection change to restore after the turn finishes.
    ok(&lane, "nick", &["install", "--enable", "mcp:claude"]);
    // Install held the turn while it ran, and let it go.
    assert!(!run.join("lane-agent.lease").exists());
    ok(&lane, "launcher", &["claim", "run:lane-agent"]);
    ok(&lane, "launcher", &["release", "run:lane-agent"]);
    ok(&lane, "nick", &["install", "--undo"]);
    ok(&lane, "launcher", &["claim", "run:lane-agent"]);
}

#[test]
fn a_turn_that_starts_while_install_holds_it_is_refused_and_says_why() {
    // Review of d0a0f84: install holds `run:<agent>` from before it reads
    // until it's done, as `<actor>@install-<pid>`, with a lease record.
    let (_dir, repo, lane) = repo();
    let cwd = lane.to_str().expect("utf-8");
    let first = [
        "turn",
        "lane-agent",
        "--harness",
        "claude",
        "--role",
        "worker",
        "--cwd",
        cwd,
        "--prompt",
        "hello",
    ];
    ok(&graph(&repo), "launcher", &first);
    // As an install in this (live) process holds it.
    let holder = format!("nick@install-{}", std::process::id());
    ok(&lane, &holder, &["claim", "run:lane-agent"]);
    let record = mdkg_run::turn::lease_record(holder.clone());
    fs::write(
        repo.join(".git/mdkg/run/lane-agent.lease"),
        serde_json::to_vec(&record).expect("json"),
    )
    .expect("w");
    let (code, out) = mdkg(
        &graph(&repo),
        "launcher",
        &["turn", "lane-agent", "--prompt", "hi"],
    );
    assert_ne!(code, 0, "{out}");
    assert_eq!(out["diagnostics"][0]["code"], "coord.lease_held", "{out}");
    // The coordinator names no holder since audit-core: the hint says it.
    assert!(
        out["diagnostics"][0]["hint"]
            .as_str()
            .is_some_and(|h| h.contains("install is changing this agent's worktree")),
        "{out}"
    );
    ok(&lane, &holder, &["release", "run:lane-agent"]);
    ok(
        &graph(&repo),
        "launcher",
        &["turn", "lane-agent", "--prompt", "hi"],
    );
}

fn worker_run<'a>(name: &'a str, cwd: &'a str) -> Vec<&'a str> {
    vec![
        "turn",
        name,
        "--harness",
        "claude",
        "--role",
        "worker",
        "--cwd",
        cwd,
    ]
}

/// A turn that leaves uncommitted graph work; the launcher only replies.
fn dirty_lane_turn(repo: &Path, lane: &Path) -> Value {
    fs::write(lane.join("fake-commit"), "").expect("w");
    let node = lane.join(".mdkg/work/task-1-basic.md");
    let text = fs::read_to_string(&node).expect("node");
    fs::write(&node, format!("{text}\nedited\n")).expect("w");
    let out = ok(
        &graph(repo),
        "launcher",
        &worker_run("lane-agent", lane.to_str().expect("utf-8")),
    );
    assert!(out["data"]["commit_refused"].is_null(), "{out}");
    out
}

#[test]
fn an_orchestrator_receives_a_dirty_workers_reply_without_a_launcher_commit_note() {
    let (dir, repo, lane) = repo();
    let orch = dir.0.join("orch");
    sh(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "agent/orchestrator",
            orch.to_str().expect("utf-8"),
            "main",
        ],
    );
    ok(
        &repo,
        "orchestrator",
        &[
            "msg",
            "send",
            "--to",
            "lane-agent",
            "--kind",
            "handoff",
            "--text",
            "do it",
        ],
    );
    let out = dirty_lane_turn(&repo, &lane);
    assert!(out["data"]["commit_note"].is_null(), "{out}");
    // Only the worker's own reply reaches the orchestrator.
    let o = orch.to_str().expect("utf-8");
    let first = ok(&graph(&repo), "launcher", &worker_run("orchestrator", o));
    assert_eq!(first["data"]["outcome"], "replied", "{first}");
    assert_eq!(
        first["data"]["launcher_notes"].as_array().map(Vec::len),
        None,
        "{first}"
    );
    let prompt = fs::read_to_string(orch.join("claude.prompt")).expect("prompt");
    assert!(prompt.contains("claude reply"), "{prompt}");
    assert!(!prompt.contains("commit refused"), "{prompt}");
    let second = ok(&graph(&repo), "launcher", &worker_run("orchestrator", o));
    assert_eq!(second["data"]["outcome"], "idle", "{second}");
    let inbox = ok(&repo, "orchestrator", &["msg", "inbox"]);
    assert_eq!(inbox["data"]["messages"], serde_json::json!([]), "{inbox}");
}

#[test]
fn a_room_posts_dirty_work_gets_only_the_workers_reply() {
    let (_dir, repo, lane) = repo();
    ok(
        &repo,
        "orch",
        &["room", "create", "pair", "--member", "lane-agent"],
    );
    ok(
        &repo,
        "orch",
        &[
            "msg", "send", "--room", "pair", "--kind", "note", "--text", "hi",
        ],
    );
    let out = dirty_lane_turn(&repo, &lane);
    assert!(out["data"]["commit_note"].is_null(), "{out}");
    assert!(out["data"]["commit_note_failed"].is_null(), "{out}");
    // Only the room reply arrives; there is no commit-refusal side channel.
    let inbox = ok(&repo, "orch", &["msg", "inbox"]);
    let messages = inbox["data"]["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 1, "{inbox}");
    assert_eq!(messages[0]["sender"], "lane-agent");
    // The agent itself never gets it: its next turn has nothing to do.
    fs::remove_file(lane.join("fake-commit")).ok();
    let next = ok(
        &graph(&repo),
        "launcher",
        &worker_run("lane-agent", lane.to_str().expect("utf-8")),
    );
    assert_eq!(next["data"]["outcome"], "idle", "{next}");
}

/// A record of a holder whose process is gone: this host, a pid that isn't
/// running.
fn dead_record(holder: &str) -> Vec<u8> {
    let mut r = mdkg_run::turn::lease_record(holder.to_string());
    // A process that ran and ended: its pid is free (`ps` names a pid past
    // the system's limit as an error, which never proves a holder gone).
    let mut child = Command::new("/usr/bin/true").spawn().expect("true");
    child.wait().expect("wait");
    r.pid = child.id();
    r.started = "Thu Jan  1 00:00:00 1970".into();
    serde_json::to_vec(&r).expect("json")
}

#[test]
fn install_takes_back_a_turn_lease_whose_holder_is_gone_and_names_a_live_one() {
    // Review of c99eb3b: an install stopped with Ctrl-C, or a launcher that
    // died, blocked install until the lease ran out.
    let (_dir, repo, lane) = repo();
    let run = repo.join(".git/mdkg/run");
    fs::create_dir_all(&run).expect("mkdir");
    let spec = serde_json::json!({
        "spec": { "name": "lane-agent", "harness": "claude", "role": "worker", "cwd": lane }
    });
    fs::write(run.join("lane-agent.json"), spec.to_string()).expect("w");
    for holder in ["nick@install-999999", "launcher@run-gone"] {
        ok(&lane, holder, &["claim", "run:lane-agent"]);
        fs::write(run.join("lane-agent.lease"), dead_record(holder)).expect("w");
        ok(&lane, "nick", &["install"]);
        assert!(!run.join("lane-agent.lease").exists(), "{holder}");
    }
    // A live launcher's turn: refused, and said so.
    let holder = format!("launcher@run-{}", std::process::id());
    ok(&lane, &holder, &["claim", "run:lane-agent"]);
    let record = mdkg_run::turn::lease_record(holder.clone());
    fs::write(
        run.join("lane-agent.lease"),
        serde_json::to_vec(&record).expect("json"),
    )
    .expect("w");
    let (code, out) = mdkg(&lane, "nick", &["install", "--undo"]);
    assert_eq!(code, 5, "{out}");
    let d = &out["diagnostics"][0];
    assert!(
        d["message"]
            .as_str()
            .is_some_and(|m| m.contains("agent `lane-agent`'s turn is running")),
        "{out}"
    );
    assert!(
        d["hint"]
            .as_str()
            .is_some_and(|h| h.contains("between turns holds nothing")),
        "{out}"
    );
    assert!(run.join("lane-agent.lease").exists(), "a live record stays");
    ok(&lane, &holder, &["release", "run:lane-agent"]);
    // No marker is left behind.
    let markers = fs::read_dir(&run)
        .expect("r")
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "installing"))
        .count();
    assert_eq!(markers, 0);
}

#[test]
fn a_watcher_waits_for_install_instead_of_stopping() {
    // Review of c99eb3b: an idle watcher that got mail while install held
    // the turn exited with 5.
    let (_dir, repo, lane) = repo();
    let cwd = lane.to_str().expect("utf-8");
    ok(
        &graph(&repo),
        "launcher",
        &[
            "turn",
            "lane-agent",
            "--harness",
            "claude",
            "--role",
            "worker",
            "--cwd",
            cwd,
            "--prompt",
            "hello",
        ],
    );
    ok(
        &repo,
        "orchestrator",
        &[
            "msg",
            "send",
            "--to",
            "lane-agent",
            "--kind",
            "handoff",
            "--text",
            "go",
        ],
    );
    let holder = format!("nick@install-{}", std::process::id());
    ok(&lane, &holder, &["claim", "run:lane-agent"]);
    let run = repo.join(".git/mdkg/run");
    let record = mdkg_run::turn::lease_record(holder.clone());
    fs::write(
        run.join("lane-agent.lease"),
        serde_json::to_vec(&record).expect("json"),
    )
    .expect("w");
    let root = graph(&repo);
    let mut watcher = isolated(&mut Command::new(env!("CARGO_BIN_EXE_mdkg")))
        .current_dir(&root)
        .arg("--root")
        .arg(&root)
        .args(["--as", "launcher", "agents", "up", "lane-agent"])
        .args(["--max-turns", "1", "--wait", "1"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("watch");
    std::thread::sleep(std::time::Duration::from_secs(2));
    assert!(
        watcher.try_wait().expect("wait").is_none(),
        "the watcher stopped while install held the turn"
    );
    fs::remove_file(run.join("lane-agent.lease")).expect("rm");
    ok(&lane, &holder, &["release", "run:lane-agent"]);
    let started = std::time::Instant::now();
    let status = loop {
        if let Some(s) = watcher.try_wait().expect("wait") {
            break s;
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(60),
            "the watcher never ran its turn"
        );
        std::thread::sleep(std::time::Duration::from_millis(200));
    };
    let out = watcher.wait_with_output().expect("output");
    assert!(status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("1 turn(s)"));
}

#[test]
fn a_marker_left_by_a_gone_install_never_blocks_a_turn() {
    // Review of 08ff0f9: a marker from a host name that changed since (or
    // a Ctrl-C'd install) refused turns for good. Nothing holds its lock.
    let (_dir, repo, lane) = repo();
    let cwd = lane.to_str().expect("utf-8");
    let first = [
        "turn",
        "lane-agent",
        "--harness",
        "claude",
        "--role",
        "worker",
        "--cwd",
        cwd,
        "--prompt",
        "hello",
    ];
    ok(&graph(&repo), "launcher", &first);
    let run = repo.join(".git/mdkg/run");
    let mut record = mdkg_run::turn::lease_record("nick@install-1".into());
    record.host = "old-name.local".into();
    let marker = mdkg_run::state::InstallMarker {
        record,
        top: lane.clone(),
        until_ms: mdkg_run::state::now_ms() + mdkg_run::state::MARKER_TTL_MS,
    };
    let left = run.join("install-left.installing");
    fs::write(&left, serde_json::to_vec(&marker).expect("json")).expect("w");
    ok(
        &graph(&repo),
        "launcher",
        &["turn", "lane-agent", "--prompt", "again"],
    );
    // A new install sweeps it.
    ok(&lane, "nick", &["install"]);
    assert!(!left.exists());
}

#[test]
fn install_mirrors_many_skills_with_few_file_descriptors() {
    // Review of 2d679cd: a handle kept per skill ran out under
    // `ulimit -n 256` with 300 skills, and install said ok with 50 missing.
    let (_dir, _repo, lane) = repo();
    for i in 0..120 {
        let d = lane.join(format!(".mdkg/skills/s{i:03}"));
        fs::create_dir_all(&d).expect("mkdir");
        fs::write(d.join("SKILL.md"), format!("skill {i}\n")).expect("w");
    }
    let out = isolated(&mut Command::new("/bin/sh"))
        .current_dir(&lane)
        .args(["-c", "ulimit -n 64 && exec \"$0\" \"$@\""])
        .arg(env!("CARGO_BIN_EXE_mdkg"))
        .arg("--root")
        .arg(&lane)
        .args(["--as", "nick", "install", "--json"])
        .output()
        .expect("install");
    let envelope: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|_| {
        panic!(
            "not JSON: {}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    });
    assert!(out.status.success(), "{envelope}");
    assert!(
        !envelope.to_string().contains("can't be read"),
        "{envelope}"
    );
    for i in 0..120 {
        for m in [".agents/skills", ".claude/skills"] {
            assert!(
                lane.join(m).join(format!("s{i:03}/SKILL.md")).is_file(),
                "{m} s{i:03}"
            );
        }
    }
}

/// The orchestrator's fail-closed rule (2026-10-25): an allow rule that
/// can't be read is named in the turn's report and never reaches the
/// harness's settings.
#[test]
fn an_unreadable_allow_rule_is_reported_and_never_passed_on() {
    let (_dir, repo, _lane) = repo();
    let graph = graph(&repo);
    let bad = "Bash('/opt/rc builds/mdkg-next *)";
    let out = ok(
        &graph,
        "launcher",
        &[
            "turn",
            "r",
            "--harness",
            "claude",
            "--role",
            "reviewer",
            "--prompt",
            "x",
            "--allow",
            bad,
            "--allow",
            "Bash(npm test *)",
            "--dry-run",
        ],
    );
    let dropped = &out["data"]["dropped_allow"];
    assert_eq!(dropped.as_array().map(Vec::len), Some(1), "{out}");
    assert_eq!(dropped[0]["rule"], bad, "{out}");
    let fix = dropped[0]["fix"].as_str().unwrap_or_default();
    assert!(fix.contains("fix its quoting"), "{out}");
    let args = out["data"]["command"]["args"].to_string();
    assert!(!args.contains("/opt/rc builds"), "{args}");
    // The binary the turn runs is mdkg whatever its name (round 2).
    let out = ok(
        &graph,
        "launcher",
        &[
            "turn",
            "r2",
            "--harness",
            "claude",
            "--role",
            "reviewer",
            "--prompt",
            "x",
            "--binary",
            "/opt/bin/graphctl",
            "--allow",
            "Bash(graphctl --root . *)",
            "--dry-run",
        ],
    );
    assert_eq!(
        out["data"]["dropped_allow"][0]["rule"], "Bash(graphctl --root . *)",
        "{out}"
    );
    // The warning names the rule and says what to allow instead.
    let (code, err) = mdkg_text(
        &graph,
        &[
            "turn",
            "r",
            "--harness",
            "claude",
            "--role",
            "reviewer",
            "--prompt",
            "x",
            "--allow",
            "Bash(mdkg-next *)",
            "--dry-run",
        ],
    );
    assert_eq!(code, 0, "{err}");
    assert!(
        err.contains("allow rule Bash(mdkg-next *) isn't passed to the agent")
            && err.contains("`Bash(mdkg-next validate *)`")
            && err.contains("the mdkg MCP server"),
        "{err}"
    );
}

#[test]
fn codex_hook_trust_receipts_distinguish_cli_previews_and_spawn_attempts() {
    let (dir, repo, lane) = repo();
    let git_bin = git_only_bin(&dir.0);
    fs::create_dir_all(lane.join(".codex")).expect("codex directory");
    fs::write(
        lane.join(".codex/hooks.json"),
        mdkg_run::codex_hooks::codex_fragment(env!("CARGO_BIN_EXE_mdkg")),
    )
    .expect("hooks");
    let invoke = |dry: bool, json: bool| {
        let mut c = Command::new(env!("CARGO_BIN_EXE_mdkg"));
        isolated(&mut c)
            .current_dir(graph(&repo))
            .arg("--root")
            .arg(graph(&repo))
            .args(["--as", "launcher"])
            .args(common::agent_turn_args(
                &graph(&repo),
                &[
                    "turn",
                    "lane-agent",
                    "--harness",
                    "codex",
                    "--role",
                    "worker",
                    "--cwd",
                    lane.to_str().unwrap(),
                    "--prompt",
                    "offline trust test",
                ],
            ))
            .env("MDKG_RUN_CODEX", fixture("fake-codex.sh"))
            // Ensure this build's absolute executable spelling is selected.
            .env("PATH", narrow_path(&git_bin, &[]))
            .env_remove("MDKG_RUN_TURN")
            .env_remove("MDKG_TRUSTED_COMMON_DIR")
            .env_remove("MDKG_TRUSTED_WORKTREE");
        if dry {
            c.arg("--dry-run");
        }
        if json {
            c.arg("--json");
        }
        c.output().expect("mdkg")
    };
    let text = invoke(true, false);
    assert!(
        text.status.success(),
        "{}",
        String::from_utf8_lossy(&text.stderr)
    );
    let text = String::from_utf8_lossy(&text.stdout);
    assert!(
        text.contains("Codex hook trust, preview 1: all hook sources were not verified"),
        "{text}"
    );
    assert!(!lane.join("codex.args").exists());
    let out = invoke(false, true);
    let envelope = common::one_turn(&out.stdout);
    assert!(out.status.success(), "{envelope}");
    let receipt = &envelope["data"]["codex_hook_trust"][0];
    assert_eq!(receipt["attempt"], 1);
    assert_eq!(receipt["decision"], "required");
    assert_eq!(receipt["preview"], false);
    assert_eq!(
        receipt["reason"],
        mdkg_run::profile::CodexHookTrust::Required.reason()
    );
    fs::remove_file(lane.join(".codex/hooks.json")).expect("remove hooks");
    let text = invoke(false, false);
    assert!(
        text.status.success(),
        "{}",
        String::from_utf8_lossy(&text.stderr)
    );
    let text = String::from_utf8_lossy(&text.stdout);
    assert!(
        text.contains("Codex hook trust, spawn attempt 1: all hook sources were not verified"),
        "{text}"
    );
    fs::write(lane.join("fake-sandbox"), "danger-full-access").expect("bad header");
    let failed = invoke(false, true);
    assert!(!failed.status.success());
    let failed: Value = serde_json::from_slice(&failed.stdout).expect("failed JSON");
    assert!(
        failed["diagnostics"]
            .to_string()
            .contains(mdkg_run::profile::CodexHookTrust::Required.reason()),
        "{failed}"
    );
}
