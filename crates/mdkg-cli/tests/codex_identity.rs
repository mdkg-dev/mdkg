//! Codex registration uses the checked worktree's actor, not daemon env.
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("mdkg-codex-identity-{}", uuid::Uuid::now_v7()));
        for dir in [".git/objects", ".git/refs", ".mdkg", "src"] {
            fs::create_dir_all(root.join(dir)).expect("identity test fixture succeeds");
        }
        fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n")
            .expect("identity test fixture succeeds");
        fs::write(
            root.join(".git/config"),
            "[core]\nrepositoryformatversion = 0\nbare = false\n",
        )
        .expect("identity test fixture succeeds");
        fs::write(root.join("src/lib.rs"), "// test\n").expect("identity test fixture succeeds");
        let f = Self(fs::canonicalize(root).expect("identity test fixture succeeds"));
        f.config(json!({"alice":{"cwd":f.0}}));
        f
    }
    fn config(&self, agents: Value) {
        let mut config: Value =
            serde_json::from_str(include_str!("../../../conformance/base/.mdkg/config.json"))
                .expect("identity test fixture succeeds");
        config["agents"] = agents;
        fs::write(self.0.join(".mdkg/config.json"), config.to_string())
            .expect("identity test fixture succeeds");
    }
    fn mdkg(&self, args: &[&str], vars: &[(&str, &str)]) -> Output {
        self.mdkg_as("alice", args, vars)
    }
    fn mdkg_as(&self, actor: &str, args: &[&str], vars: &[(&str, &str)]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_mdkg"))
            .current_dir(&self.0)
            .env_remove("MDKG_RUN_TURN")
            .env_remove("MDKG_TRUSTED_COMMON_DIR")
            .env_remove("MDKG_TRUSTED_WORKTREE")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .envs(vars.iter().copied())
            .args(["--as", actor])
            .args(args)
            .arg("--json")
            .output()
            .expect("fixture mdkg")
    }
    fn hook(&self, event: &str, explicit_actor: Option<&str>) -> Output {
        let input = json!({
            "session_id":"019a1111-1111-7111-8111-111111111111", "cwd":self.0,
            "hook_event_name":if event == "edit" {"PreToolUse"} else {"UserPromptSubmit"},
            "tool_name":"apply_patch",
            "tool_input":{"command":"*** Begin Patch\n*** Add File: src/new.rs\n+test\n*** End Patch"}
        });
        self.call_hook(event, explicit_actor, &input)
    }
    fn call_hook(&self, event: &str, explicit_actor: Option<&str>, input: &Value) -> Output {
        self.call_hook_env(event, explicit_actor, input, &[])
    }
    fn call_hook_env(
        &self,
        event: &str,
        explicit_actor: Option<&str>,
        input: &Value,
        vars: &[(&str, &str)],
    ) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_mdkg"));
        cmd.current_dir(&self.0)
            .env("MDKG_ACTOR", "wrong-daemon-actor")
            .env_remove("MDKG_RUN_TURN")
            .env_remove("MDKG_TRUSTED_COMMON_DIR")
            .env_remove("MDKG_TRUSTED_WORKTREE")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(["hook", event, "--harness", "codex"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(actor) = explicit_actor {
            cmd.args(["--as", actor]);
        }
        cmd.envs(vars.iter().copied());
        let mut child = cmd.spawn().expect("identity test fixture succeeds");
        child
            .stdin
            .take()
            .expect("identity test fixture succeeds")
            .write_all(input.to_string().as_bytes())
            .expect("identity test fixture succeeds");
        child
            .wait_with_output()
            .expect("identity test fixture succeeds")
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).ok();
    }
}

#[test]
fn daemon_environment_cannot_change_the_registered_actor() {
    let f = Fixture::new();
    let result = f.hook("start", None);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let threads = mdkg_run::hooks::threads(&f.0.join(".git"));
    assert_eq!(threads.len(), 1);
    assert_eq!(threads[0].actor, "alice");
    assert_eq!(threads[0].cwd, f.0);
}

#[test]
fn ambiguity_prevents_registration_and_denies_edits_even_with_explicit_actor() {
    let f = Fixture::new();
    f.config(json!({"alice":{"cwd":f.0},"bob":{"cwd":f.0}}));
    let start = f.hook("start", None);
    assert_eq!(start.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&start.stderr).contains("couldn't record the thread observation")
    );
    assert!(start.stdout.is_empty());
    assert!(mdkg_run::hooks::threads(&f.0.join(".git")).is_empty());
    let result = f.hook("edit", Some("alice"));
    assert_eq!(result.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("mdkg can't read this session's binding")
    );
    assert!(result.stdout.is_empty());
}

#[test]
fn unknown_worktree_never_falls_back_to_environment_or_basename() {
    let f = Fixture::new();
    f.config(json!({}));
    let result = f.hook("edit", None);
    assert_eq!(result.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("mdkg can't read this session's binding")
    );
    assert!(result.stdout.is_empty());
}

#[test]
fn session_start_provenance_reaches_the_registry_and_survives_prompt_and_stop() {
    use mdkg_run::hooks::{HarnessEvent, StartProvenance};
    let f = Fixture::new();
    for (event, name, expected) in [
        ("start", "SessionStart", HarnessEvent::SessionStart),
        ("start", "UserPromptSubmit", HarnessEvent::UserPromptSubmit),
        ("stop", "Stop", HarnessEvent::Stop),
    ] {
        let input = json!({
            "session_id":"019a1111-1111-7111-8111-111111111111", "cwd":f.0,
            "hook_event_name":name, "source":"fork", "agent_id":"agent_01-ABC",
            "prompt":"PRIVATE_PROMPT", "transcript_path":"PRIVATE_TRANSCRIPT",
            "last_assistant_message":"PRIVATE_MESSAGE"
        });
        let result = f.call_hook(event, None, &input);
        // Stop may block for this fixture's unleased files; the observation
        // must still retain its source and agent id from SessionStart.
        assert!(matches!(result.status.code(), Some(0 | 2)));
        let threads = mdkg_run::hooks::threads(&f.0.join(".git"));
        assert_eq!(threads.len(), 1);
        let last = threads[0].last_observation.as_ref().expect("observation");
        assert_eq!(last.harness_event, Some(expected));
        if name != "SessionStart" {
            assert!(last.start_provenance.is_none());
        }
        let start = threads[0]
            .last_session_start
            .as_ref()
            .expect("session start retained");
        assert_eq!(start.harness_event, Some(HarnessEvent::SessionStart));
        assert_eq!(
            start.start_provenance,
            Some(StartProvenance::parse(Some("fork"), Some("agent_01-ABC")))
        );
        assert!(last.at_ms >= start.at_ms);
        assert!(
            !serde_json::to_string(&threads)
                .expect("serialize")
                .contains("PRIVATE_")
        );
    }
}

#[test]
fn conflicting_daemon_pins_cannot_prevent_registration() {
    let f = Fixture::new();
    let input = json!({
        "session_id":"019a1111-1111-7111-8111-111111111111", "cwd":f.0,
        "hook_event_name":"UserPromptSubmit"
    });
    let result = f.call_hook_env(
        "start",
        None,
        &input,
        &[
            ("MDKG_TRUSTED_COMMON_DIR", "/wrong-daemon-repository/.git"),
            ("MDKG_TRUSTED_WORKTREE", "/wrong-daemon-worktree"),
        ],
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let threads = mdkg_run::hooks::threads(&f.0.join(".git"));
    assert_eq!(threads.len(), 1);
    assert_eq!(threads[0].actor, "alice");
}

const WRONG_PINS: &[(&str, &str)] = &[
    ("MDKG_TRUSTED_COMMON_DIR", "/wrong-daemon-repository/.git"),
    ("MDKG_TRUSTED_WORKTREE", "/wrong-daemon-worktree"),
];

#[test]
fn ordinary_commands_still_enforce_each_inherited_pin() {
    let f = Fixture::new();
    for pin in WRONG_PINS {
        let result = f.mdkg(&["status"], &[*pin]);
        assert!(!result.status.success());
        let result: Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(result["ok"], false);
        assert_eq!(result["diagnostics"][0]["code"], "coord.store_failed");
    }
    assert!(f.mdkg(&["status"], &[]).status.success());
}

#[test]
fn stale_pins_do_not_disable_mail_lease_checks_or_the_stop_audit() {
    let f = Fixture::new();
    let sent = f.mdkg_as(
        "sender",
        &[
            "msg",
            "send",
            "--to",
            "alice",
            "--kind",
            "handoff",
            "--text",
            "PRIVATE_BODY",
        ],
        &[],
    );
    assert!(
        sent.status.success(),
        "{}",
        String::from_utf8_lossy(&sent.stdout)
    );
    let sent: Value = serde_json::from_slice(&sent.stdout).unwrap();
    let message = &sent["data"]["message"];
    let session = "019a1111-1111-7111-8111-111111111111";
    let start = f.call_hook_env(
        "start",
        None,
        &json!({
            "session_id":session, "cwd":f.0, "hook_event_name":"UserPromptSubmit"
        }),
        WRONG_PINS,
    );
    assert!(
        start.status.success(),
        "{}",
        String::from_utf8_lossy(&start.stderr)
    );
    let context = String::from_utf8_lossy(&start.stdout);
    assert!(context.contains(&format!("#{} handoff from sender", message["seq"])));
    assert!(!context.contains("PRIVATE_BODY"));
    let ack = f.mdkg(&["msg", "ack", message["messageId"].as_str().unwrap()], &[]);
    let ack: Value = serde_json::from_slice(&ack.stdout).unwrap();
    assert_eq!(ack["ok"], false);
    assert_eq!(ack["diagnostics"][0]["code"], "msg.not_shown");

    assert!(f.mdkg(&["claim", "src/lib.rs"], &[]).status.success());
    let mut edit = json!({"session_id":session, "cwd":f.0, "hook_event_name":"PreToolUse",
        "tool_name":"apply_patch", "tool_input":{"command":"*** Begin Patch\n*** Delete File: src/lib.rs\n*** End Patch"}});
    let held = f.call_hook_env("edit", None, &edit, WRONG_PINS);
    assert!(
        held.status.success(),
        "{}",
        String::from_utf8_lossy(&held.stderr)
    );
    assert!(held.stdout.is_empty());
    edit["tool_input"]["command"] =
        json!("*** Begin Patch\n*** Add File: src/new.rs\n+new\n*** End Patch");
    let unheld = f.call_hook_env("edit", None, &edit, WRONG_PINS);
    assert_eq!(unheld.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&unheld.stderr).contains("no lease of yours"));
    let stop = f.call_hook_env(
        "stop",
        None,
        &json!({"session_id":session,
        "cwd":f.0, "hook_event_name":"Stop", "stop_hook_active":false}),
        WRONG_PINS,
    );
    assert_eq!(stop.status.code(), Some(2));
    let reason = String::from_utf8_lossy(&stop.stderr);
    assert!(
        reason.contains("no lease of yours") && reason.contains("changed"),
        "{reason}"
    );
}
