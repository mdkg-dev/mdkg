//! Offline receiver integration: the fake Codex records argv, never a model.
use mdkg_run::{
    hooks::{self, HarnessEvent, HookEvent, HookObservation, ThreadState},
    receiver::route,
};
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use std::{
    fs,
    io::{Read, Write},
    path::PathBuf,
    process::{Child, Command, Output, Stdio},
    time::{Duration, Instant},
};
const SESSION: &str = "019a1111-1111-7111-8111-111111111111";
const OTHER: &str = "019a2222-2222-7222-8222-222222222222";

// Scheduling/process startup gets its own allowance, never the lease TTL or
// guard deadline being asserted. A signal/lock/receipt, not a sleep, is ready.
const STARTUP_ALLOWANCE: Duration = Duration::from_secs(30);

fn wait_for_ready<T>(child: &mut Child, what: &str, mut ready: impl FnMut() -> Option<T>) -> T {
    let start = Instant::now();
    loop {
        let value = ready();
        let status = child.try_wait().expect("readiness child status");
        if status.is_some() || start.elapsed() >= STARTUP_ALLOWANCE {
            if status.is_none() {
                let _ = child.kill();
            }
            let status = child.wait().expect("reap readiness child");
            let mut out = String::new();
            let mut err = String::new();
            if let Some(mut pipe) = child.stdout.take() {
                let _ = pipe.read_to_string(&mut out);
            }
            if let Some(mut pipe) = child.stderr.take() {
                let _ = pipe.read_to_string(&mut err);
            }
            panic!("{what}: child exited or startup allowance elapsed: {status}\n{out}\n{err}");
        }
        if let Some(value) = value {
            return value;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_exit(child: &mut Child, what: &str) -> std::process::ExitStatus {
    wait_for_exit_with(child, what, STARTUP_ALLOWANCE)
}

fn wait_for_exit_with(
    child: &mut Child,
    what: &str,
    allowance: Duration,
) -> std::process::ExitStatus {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().expect("child status") {
            return status;
        }
        if start.elapsed() >= allowance {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{what}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_pid_gone(pid: rustix::process::Pid, allowance: Duration) {
    let start = Instant::now();
    loop {
        match rustix::process::test_kill_process(pid) {
            Err(rustix::io::Errno::SRCH) => return,
            Ok(()) => {}
            Err(error) => panic!("can't inspect fixture process: {error}"),
        }
        assert!(
            start.elapsed() < allowance,
            "fixture process survived cancellation"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn recorded_pid(path: &std::path::Path) -> Option<rustix::process::Pid> {
    rustix::process::Pid::from_raw(fs::read_to_string(path).ok()?.parse().ok()?)
}

// Direct CliMail calls inherit their caller's environment. Run those tests in
// a subprocess with fixture trust context, never mutate parallel-test env.
fn clean_child(test: &str) -> bool {
    if std::env::var("MDKG_RECEIVER_TEST_CHILD").as_deref() == Ok(test) {
        return false;
    }
    let result = Command::new(std::env::current_exe().expect("test executable"))
        .args(["--exact", test, "--nocapture"])
        .env("MDKG_RECEIVER_TEST_CHILD", test)
        .env_remove("MDKG_RUN_TURN")
        .env_remove("MDKG_TRUSTED_COMMON_DIR")
        .env_remove("MDKG_TRUSTED_WORKTREE")
        .env_remove("MDKG_RUN_MDKG")
        .output()
        .expect("isolated test");
    assert!(
        result.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    true
}

struct Fixture {
    root: PathBuf,
    cwd: PathBuf,
    program: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("mdkg-receiver-cli-{}", uuid::Uuid::now_v7()));
        for d in [
            ".git/objects",
            ".git/refs",
            ".git/worktrees/alice",
            ".mdkg",
            ".worktrees/alice/.mdkg",
        ] {
            fs::create_dir_all(root.join(d)).expect("receiver fixture succeeds");
        }
        let root = fs::canonicalize(root).expect("receiver fixture succeeds");
        let cwd = root.join(".worktrees/alice");
        fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n")
            .expect("receiver fixture succeeds");
        fs::write(
            root.join(".git/config"),
            "[core]\nrepositoryformatversion = 0\nbare = false\n",
        )
        .expect("receiver fixture succeeds");
        let admin = root.join(".git/worktrees/alice");
        fs::write(admin.join("HEAD"), "ref: refs/heads/agent/alice\n")
            .expect("receiver fixture succeeds");
        fs::write(admin.join("commondir"), "../..\n").expect("receiver fixture succeeds");
        fs::write(
            admin.join("gitdir"),
            format!("{}\n", cwd.join(".git").display()),
        )
        .expect("receiver fixture succeeds");
        fs::write(cwd.join(".git"), format!("gitdir: {}\n", admin.display()))
            .expect("receiver fixture succeeds");
        let mut config: Value =
            serde_json::from_str(include_str!("../../../conformance/base/.mdkg/config.json"))
                .expect("fixture config");
        config["projections"] = json!(["hooks:codex"]);
        let config = config.to_string();
        fs::write(root.join(".mdkg/config.json"), &config).expect("receiver fixture succeeds");
        fs::write(cwd.join(".mdkg/config.json"), config).expect("receiver fixture succeeds");
        let program = root.join("fake-codex");
        let f = Self { root, cwd, program };
        f.fake("exit 0");
        f
    }
    fn fake(&self, tail: &str) {
        fs::write(&self.program, format!("#!/bin/sh\nif [ \"$1\" = --fixture-ready ]; then printf READY; exit 0; fi\nprintf '%s\\n' \"$@\" >> \"$RECEIVER_ARGV\"\nprintf 'BEGIN\\n' >> \"$RECEIVER_CALLS\"\n{tail}\n")).expect("receiver fixture succeeds");
        fs::set_permissions(&self.program, fs::Permissions::from_mode(0o700))
            .expect("receiver fixture succeeds");
        fixture_executable_ready(&self.program);
    }
    fn command(&self, actor: &str) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_mdkg"));
        c.current_dir(&self.root)
            .args(["--as", actor])
            .env_remove("MDKG_RUN_TURN")
            .env_remove("MDKG_TRUSTED_COMMON_DIR")
            .env_remove("MDKG_TRUSTED_WORKTREE")
            .env_remove("MDKG_RUN_MDKG")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("MDKG_RUN_CODEX", &self.program)
            .env("RECEIVER_ARGV", self.root.join("argv"))
            .env("RECEIVER_CALLS", self.root.join("calls"));
        c
    }
    fn call(&self, actor: &str, args: &[&str]) -> Output {
        self.command(actor)
            .args(args)
            .arg("--json")
            .output()
            .expect("receiver fixture succeeds")
    }
    fn data(&self, actor: &str, args: &[&str]) -> Value {
        let o = self.call(actor, args);
        let text = String::from_utf8_lossy(&o.stdout);
        assert!(
            o.status.success(),
            "{text} {}",
            String::from_utf8_lossy(&o.stderr)
        );
        let e: Value = serde_json::from_str(text.trim()).expect("receiver fixture succeeds");
        assert_eq!(e["ok"], true, "{e}");
        e["data"].clone()
    }
    fn hook_start(&self, session: &str, agent_id: Option<&str>) {
        let o = self.hook_start_output(session, agent_id);
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    }
    fn hook_start_output(&self, session: &str, agent_id: Option<&str>) -> Output {
        let input = json!({"session_id":session,"cwd":self.cwd,"hook_event_name":"SessionStart", "source":"startup", "agent_id":agent_id});
        let mut c = self.command("alice");
        c.current_dir(&self.cwd)
            .args(["hook", "start", "--harness", "codex"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = c.spawn().expect("receiver fixture succeeds");
        child
            .stdin
            .take()
            .expect("receiver fixture succeeds")
            .write_all(input.to_string().as_bytes())
            .expect("receiver fixture succeeds");
        child.wait_with_output().expect("receiver fixture succeeds")
    }
    fn observe(&self, session: &str, event: HarnessEvent, working: bool) {
        route::observe(
            &self.root.join(".git"),
            "alice",
            session,
            &self.cwd,
            if working {
                ThreadState::Working
            } else {
                ThreadState::Idle
            },
            &HookObservation {
                event: if event == HarnessEvent::Stop {
                    HookEvent::Stop
                } else {
                    HookEvent::Start
                },
                harness_event: Some(event),
                start_provenance: None,
                at_ms: mdkg_run::state::now_ms(),
            },
        )
        .expect("receiver fixture succeeds");
    }
    fn ready(&self) {
        self.hook_start(SESSION, None);
        self.observe(SESSION, HarnessEvent::Stop, false);
    }
    fn post(&self, kind: &str) -> Value {
        self.data(
            "sender",
            &[
                "msg",
                "send",
                "--to",
                "alice",
                "--kind",
                kind,
                "--text",
                "PRIVATE_BODY",
            ],
        )
    }
    fn up(&self) -> Value {
        self.data("launcher", &["agents", "up", "alice", "--once"])
    }
    fn route(&self) -> route::Route {
        route::read(&self.root.join(".git"), "alice")
            .expect("receiver fixture succeeds")
            .expect("receiver fixture succeeds")
    }
    fn calls(&self) -> usize {
        fs::read_to_string(self.root.join("calls"))
            .unwrap_or_default()
            .lines()
            .count()
    }
    fn renewal_shim(&self) -> PathBuf {
        let path = self.root.join("renewal-shim");
        fs::write(&path, r#"#!/bin/sh
if [ "$1" = --fixture-ready ]; then printf READY; exit 0; fi
if [ -n "$RECEIVER_CLOCK" ]; then export MDKG_NOW_MS=$(cat "$RECEIVER_CLOCK"); fi
for arg in "$@"; do
  if [ "$arg" = --local-turn ]; then
    n=0
    [ ! -f "$RENEWAL_COUNT" ] || n=$(cat "$RENEWAL_COUNT")
    n=$((n + 1))
    printf '%s\n' "$n" > "$RENEWAL_COUNT"
    fail=no
    case "$FAIL_RENEWAL" in
      5) [ "$n" -lt 5 ] || [ ! -s "$RECEIVER_CALLS" ] || fail=yes ;;
      *) [ "$n" -ne "$FAIL_RENEWAL" ] || fail=yes ;;
    esac
    if [ "$fail" = yes ]; then
      if [ "$RENEWAL_FAILURE" = delay ]; then /bin/sleep 1.2
      elif [ "$RENEWAL_FAILURE" = timeout ]; then exec /bin/sleep 30
      else
      printf '%s\n' '{"ok":false,"diagnostics":[{"code":"run.mail_failed","message":"injected renewal failure"}]}'
      exit 1
      fi
    fi
  fi
done
exec "$REAL_MDKG" "$@"
"#).expect("write renewal shim");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
            .expect("make renewal shim executable");
        fixture_executable_ready(&path);
        path
    }
    fn command_with_renewal_behavior(&self, stage: u32, failure: &str) -> Command {
        let mut c = self.command("launcher");
        c.env("MDKG_RUN_MDKG", self.renewal_shim())
            .env("REAL_MDKG", env!("CARGO_BIN_EXE_mdkg"))
            .env("RENEWAL_COUNT", self.root.join("renewal-count"))
            .env("FAIL_RENEWAL", stage.to_string())
            .env("RENEWAL_FAILURE", failure);
        c
    }
}

// Confirm new scripts are executable before lease/guard clocks start. In
// particular, macOS's first-execution assessment gets the startup allowance,
// rather than consuming the production renewal budget being tested.
fn fixture_executable_ready(program: &std::path::Path) {
    let mut child = Command::new(program)
        .arg("--fixture-ready")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("fixture executable startup");
    let start = Instant::now();
    while child.try_wait().expect("fixture startup status").is_none() {
        if start.elapsed() >= STARTUP_ALLOWANCE {
            let _ = child.kill();
            let _ = child.wait();
            panic!("fixture executable did not acknowledge readiness");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().expect("fixture startup receipt");
    assert!(
        output.status.success(),
        "fixture startup failed: {output:?}"
    );
    assert_eq!(output.stdout, b"READY");
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn missing_registration_waits_without_creating_a_headless_spec() {
    let f = Fixture::new();
    f.post("handoff");
    assert_eq!(f.up()["wake"]["status"], "waiting");
    assert!(!f.root.join(".git/mdkg/run/alice.json").exists());
    assert_eq!(f.calls(), 0);
}

#[test]
fn slow_pre_spawn_renewal_still_queues_exactly_once() {
    let f = Fixture::new();
    f.ready();
    let seq = f.post("question")["message"]["seq"].as_u64().unwrap();
    let out = f
        .command_with_renewal_behavior(4, "delay")
        .args(["agents", "up", "alice", "--once", "--json"])
        .output()
        .unwrap();
    let event: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(event["data"]["wake"]["status"], "queued", "{event}");
    assert_eq!(f.calls(), 1);
    let attempt = f.route().last_attempt.unwrap();
    assert_eq!(attempt.delivery, route::Delivery::Queued);
    assert_eq!(attempt.through, seq);
    assert_eq!(
        hooks::try_reminded_through(&f.root.join(".git"), "alice", SESSION).unwrap(),
        seq
    );
}

#[test]
fn pre_spawn_renewal_failure_is_not_sent_and_retryable() {
    for stage in [4, 1, 2, 3] {
        for failure in ["reject", "timeout"] {
            let f = Fixture::new();
            f.ready();
            let message = f.post("question");
            let seq = message["message"]["seq"].as_u64().unwrap();
            let out = f
                .command_with_renewal_behavior(stage, failure)
                .args(["agents", "up", "alice", "--once", "--json"])
                .output()
                .unwrap();
            let event: Value = serde_json::from_slice(&out.stdout).unwrap();
            assert_eq!(
                event["data"]["wake"]["status"], "not_sent",
                "stage {stage}, {failure}: {event}"
            );
            assert_eq!(f.calls(), 0);
            assert_eq!(
                hooks::try_reminded_through(&f.root.join(".git"), "alice", SESSION).unwrap(),
                0
            );
            if stage == 4 {
                assert_eq!(
                    f.route().last_attempt.unwrap().delivery,
                    route::Delivery::NotSent
                );
            } else {
                assert!(f.route().last_attempt.is_none(), "no attempt reserved yet");
            }
            assert_eq!(f.up()["wake"]["status"], "queued");
            assert_eq!(f.calls(), 1);
            assert_eq!(f.route().last_attempt.unwrap().through, seq);
        }
    }
}

#[test]
fn post_spawn_renewal_failure_stays_unknown_and_consumed() {
    for failure in ["reject", "timeout"] {
        let f = Fixture::new();
        f.ready();
        let message = f.post("question");
        let seq = message["message"]["seq"].as_u64().unwrap();
        f.fake("exec /bin/sleep 30");
        // Startup may exceed the first 100 ms heartbeat interval. The
        // failure must wait for the fake's own startup receipt.
        let script = fs::read_to_string(&f.program).unwrap();
        fs::write(
            &f.program,
            script.replacen("#!/bin/sh\n", "#!/bin/sh\n/bin/sleep 0.3\n", 1),
        )
        .unwrap();
        let out = f
            .command_with_renewal_behavior(5, failure)
            .args(["agents", "up", "alice", "--once", "--json"])
            .output()
            .unwrap();
        let event: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(
            event["data"]["wake"]["status"], "delivery_unknown",
            "{failure}: {event}"
        );
        assert_eq!(f.calls(), 1);
        assert_eq!(
            f.route().last_attempt.unwrap().delivery,
            route::Delivery::DeliveryUnknown
        );
        assert_eq!(
            hooks::try_reminded_through(&f.root.join(".git"), "alice", SESSION).unwrap(),
            seq
        );
        f.fake("exit 0");
        assert_eq!(f.up()["wake"]["status"], "no_mail");
        assert_eq!(f.calls(), 1);
    }
}

#[test]
fn supervisor_retries_pre_spawn_timeout_on_its_next_cycle() {
    let f = Fixture::new();
    f.ready();
    f.post("question");
    let mut child = f
        .command_with_renewal_behavior(4, "timeout")
        .args([
            "agents",
            "up",
            "alice",
            "--max-turns",
            "1",
            "--wait",
            "1",
            "--json",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let began = Instant::now();
    while child.try_wait().unwrap().is_none() {
        if began.elapsed() >= STARTUP_ALLOWANCE {
            let _ = child.kill();
            let _ = child.wait();
            panic!("supervisor did not retry the unsent reminder");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let events: Vec<Value> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    let statuses: Vec<_> = events
        .iter()
        .map(|e| e["data"]["wake"]["status"].as_str())
        .collect();
    assert_eq!(statuses, [Some("not_sent"), Some("queued")], "{events:?}");
    assert_eq!(f.calls(), 1);
}

#[test]
fn early_renewal_failure_preserves_a_previous_unknown_attempt() {
    for stage in 1..=3 {
        let f = Fixture::new();
        f.ready();
        let old = f.post("question")["message"]["seq"].as_u64().unwrap();
        f.fake("exit 1");
        assert_eq!(f.up()["wake"]["status"], "delivery_unknown");
        let previous = f.route().last_attempt.unwrap();
        let next = f.post("handoff")["message"]["seq"].as_u64().unwrap();
        let out = f
            .command_with_renewal_behavior(stage, "reject")
            .args(["agents", "up", "alice", "--once", "--json"])
            .output()
            .unwrap();
        let event: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(event["data"]["wake"]["status"], "not_sent", "{event}");
        let after = f.route().last_attempt.unwrap();
        assert_eq!(after.id, previous.id);
        assert_eq!(after.delivery, route::Delivery::DeliveryUnknown);
        assert_eq!(
            hooks::try_reminded_through(&f.root.join(".git"), "alice", SESSION).unwrap(),
            old
        );
        f.fake("exit 0");
        assert_eq!(f.up()["wake"]["status"], "queued");
        assert_eq!(f.route().last_attempt.unwrap().through, next);
        assert_eq!(f.calls(), 2);
    }
}
#[test]
fn queue_uses_only_new_headers_and_leaves_mail_unshown() {
    if clean_child("queue_uses_only_new_headers_and_leaves_mail_unshown") {
        return;
    }
    let f = Fixture::new();
    f.ready();
    let m = f.post("handoff");
    assert_eq!(f.up()["wake"]["status"], "queued");
    let argv = fs::read_to_string(f.root.join("argv")).expect("receiver fixture succeeds");
    assert!(
        argv.starts_with(&format!(
            "queue\n--remote\nunix://\n--thread\n{SESSION}\n--message\n"
        )),
        "{argv}"
    );
    assert!(argv.contains("handoff from sender"));
    assert!(!argv.contains("PRIVATE_BODY"));
    assert_eq!(f.calls(), 1);
    assert_eq!(
        f.route()
            .last_attempt
            .expect("receiver fixture succeeds")
            .delivery,
        route::Delivery::Queued
    );
    assert_eq!(f.up()["wake"]["status"], "no_mail");
    assert_eq!(f.calls(), 1);
    let id = m["message"]["messageId"]
        .as_str()
        .expect("receiver fixture succeeds");
    let ack = f.call("alice", &["msg", "ack", id]);
    assert!(!ack.status.success());
    assert!(String::from_utf8_lossy(&ack.stdout).contains("msg.not_shown"));
    // An actual no-mark wait uses the same typed header parser and leaves ack refused.
    use mdkg_run::Mail;
    let mut mail = mdkg_run::CliMail::new(
        env!("CARGO_BIN_EXE_mdkg").into(),
        f.root.clone(),
        "launcher".into(),
    );
    assert_eq!(
        mail.wait_headers("alice", None, 1)
            .expect("receiver fixture succeeds")
            .expect("receiver fixture succeeds")
            .messages
            .len(),
        1
    );
    assert!(!f.call("alice", &["msg", "ack", id]).status.success());
}
#[test]
fn ambiguous_delivery_consumes_old_header_when_new_mail_arrives() {
    let f = Fixture::new();
    f.ready();
    f.fake("exit 7");
    let first = f.post("handoff");
    assert_eq!(f.up()["wake"]["status"], "delivery_unknown");
    assert_eq!(f.up()["wake"]["status"], "no_mail");
    assert_eq!(f.calls(), 1);
    fs::write(f.root.join("argv"), "").expect("receiver fixture succeeds");
    let second = f.post("question");
    assert_eq!(f.up()["wake"]["status"], "delivery_unknown");
    assert_eq!(f.calls(), 2);
    let argv = fs::read_to_string(f.root.join("argv")).expect("receiver fixture succeeds");
    assert!(!argv.contains(&format!("#{} handoff", first["message"]["seq"])));
    assert!(
        argv.contains(&format!("#{} question", second["message"]["seq"])),
        "{argv}"
    );
}
#[test]
fn definite_spawn_failure_can_be_retried_but_busy_and_conflict_do_not_claim() {
    let f = Fixture::new();
    f.hook_start(SESSION, None);
    f.post("question");
    assert_eq!(f.up()["wake"]["status"], "busy");
    assert_eq!(
        hooks::try_reminded_through(&f.root.join(".git"), "alice", SESSION)
            .expect("receiver fixture succeeds"),
        0
    );
    f.observe(SESSION, HarnessEvent::Stop, false);
    fs::remove_file(&f.program).expect("receiver fixture succeeds");
    assert_eq!(f.up()["wake"]["status"], "not_sent");
    assert_eq!(
        hooks::try_reminded_through(&f.root.join(".git"), "alice", SESSION)
            .expect("receiver fixture succeeds"),
        0
    );
    f.fake("exit 0");
    assert_eq!(f.up()["wake"]["status"], "queued");
    f.hook_start(OTHER, None);
    f.observe(SESSION, HarnessEvent::Stop, false);
    f.post("findings");
    assert!(f.route().suspended);
    assert_eq!(f.up()["wake"]["status"], "suspended");
    assert_eq!(f.calls(), 1);
}
#[test]
fn child_sessions_do_not_latch_and_reset_requires_a_new_primary_prompt() {
    let f = Fixture::new();
    f.ready();
    f.hook_start(OTHER, Some("child-1"));
    assert!(!f.route().suspended);
    f.observe(OTHER, HarnessEvent::Stop, false);
    assert!(!f.route().suspended);
    assert!(
        f.data("launcher", &["agents", "resume", "alice"])["waiting_for_prompt"]
            .as_bool()
            .expect("receiver fixture succeeds")
    );
    f.observe(SESSION, HarnessEvent::Stop, false);
    assert!(f.route().session.is_none());
    f.observe(OTHER, HarnessEvent::UserPromptSubmit, true);
    assert!(f.route().session.is_none());
    f.observe(SESSION, HarnessEvent::UserPromptSubmit, true);
    assert_eq!(f.route().session.as_deref(), Some(SESSION));
}
#[test]
fn corrupted_route_and_reminder_never_dispatch() {
    let f = Fixture::new();
    f.ready();
    f.post("note");
    // Direct notes wake; damaged reminder evidence must still refuse dispatch.
    let common = f.root.join(".git");
    hooks::claim_reminder(&common, "alice", SESSION, 1);
    let reminder = hooks::dir(&common).join("reminded/alice.json");
    fs::write(reminder, "broken").expect("receiver fixture succeeds");
    let o = f.call("launcher", &["agents", "up", "alice", "--once"]);
    assert!(String::from_utf8_lossy(&o.stdout).contains("trouble"));
    assert_eq!(f.calls(), 0);
    fs::write(common.join("mdkg/hooks/receiver/alice.json"), "broken")
        .expect("receiver fixture succeeds");
    let o = f.call("launcher", &["agents", "up", "alice", "--once"]);
    assert!(String::from_utf8_lossy(&o.stdout).contains("trouble"));
    assert_eq!(f.calls(), 0);
    let status = f.data("launcher", &["agents", "status"]);
    assert!(status["receivers"][0]["problem"].is_string());
}

#[test]
fn loop_guard_stays_paused_until_explicit_thread_resume() {
    let f = Fixture::new();
    f.ready();
    f.post("question");
    let dir = f.root.join(".git/mdkg/run");
    let key = format!("codex:{SESSION}");
    let policy = mdkg_run::policy::Policy::default();
    let g = policy.loop_guard.expect("receiver fixture succeeds");
    mdkg_run::supervise::Guard::update(&dir, "alice", false, |guard| {
        for _ in 0..g.turns {
            guard.record(&g, &key, mdkg_run::state::now_ms());
        }
        Ok(())
    })
    .expect("receiver fixture succeeds");
    assert_eq!(f.up()["wake"]["status"], "paused");
    let status = f.data("launcher", &["agents", "status"]);
    assert_eq!(status["agents"][0]["name"], "alice");
    assert_eq!(status["agents"][0]["paused"][0], key);
    let text = f
        .command("launcher")
        .args(["agents", "status"])
        .output()
        .expect("status");
    assert!(String::from_utf8_lossy(&text.stdout).contains("receiver alice: paused by loop guard"));

    assert_eq!(f.calls(), 0);
    assert_eq!(
        hooks::try_reminded_through(&f.root.join(".git"), "alice", SESSION)
            .expect("receiver fixture succeeds"),
        0
    );
    let reset = f.data("launcher", &["agents", "resume", "alice", "--thread", &key]);
    assert_eq!(reset["waiting_for_prompt"], false);
    assert_eq!(f.route().session.as_deref(), Some(SESSION));
    assert_eq!(f.up()["wake"]["status"], "queued");
}

#[test]
fn default_up_includes_registered_actors_without_saving_specs() {
    let f = Fixture::new();
    f.ready();
    f.post("review_request");
    let data = f.data("launcher", &["agents", "up", "--once"]);
    assert_eq!(data["agent"], "alice");
    assert_eq!(data["wake"]["status"], "queued");
    assert!(!f.root.join(".git/mdkg/run/alice.json").exists());
}

#[test]
fn bounded_header_wait_rechecks_busy_state_without_new_mail() {
    let f = Fixture::new();
    f.hook_start(SESSION, None);
    f.post("resolution");
    let events = f.root.join("receiver-events.jsonl");
    let mut child = f
        .command("launcher")
        .args([
            "agents",
            "up",
            "alice",
            "--max-turns",
            "1",
            "--wait",
            "1",
            "--json",
        ])
        .stdin(Stdio::null())
        .stdout(fs::File::create(&events).expect("receiver events"))
        .stderr(Stdio::null())
        .spawn()
        .expect("receiver fixture succeeds");
    wait_for_ready(
        &mut child,
        "receiver did not observe the busy thread",
        || {
            fs::read_to_string(&events)
                .unwrap_or_default()
                .lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .any(|event| event["data"]["wake"]["status"] == "busy")
                .then_some(())
        },
    );
    f.observe(SESSION, HarnessEvent::Stop, false);
    assert!(wait_for_exit(&mut child, "receiver failed to recheck idle state").success());
    assert_eq!(f.calls(), 1);
}

#[test]
fn reset_cannot_change_the_route_during_an_admitted_queue_process() {
    let f = Fixture::new();
    f.ready();
    f.post("question");
    // Keep the admitted queue alive until the reset assertion has run, rather
    // than hoping the test process is scheduled within a two-second sleep.
    f.fake("while [ ! -e \"$RECEIVER_RELEASE\" ]; do /bin/sleep 0.01; done");
    let release = f.root.join("release-queue");
    let mut child = f
        .command("launcher")
        .args(["agents", "up", "alice", "--once", "--json"])
        .env("RECEIVER_RELEASE", &release)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("receiver fixture succeeds");
    wait_for_ready(&mut child, "queue was not invoked", || {
        (f.calls() > 0).then_some(())
    });
    let result = f.call("launcher", &["agents", "resume", "alice"]);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stdout).contains("coord.lease_held"));
    assert_eq!(f.route().session.as_deref(), Some(SESSION));
    fs::write(release, "go").expect("release admitted queue");
    assert!(child.wait().expect("receiver fixture succeeds").success());
    assert_eq!(
        f.data("launcher", &["agents", "resume", "alice"])["waiting_for_prompt"],
        true
    );
}

#[test]
fn interactive_receivers_refuse_a_model_turn_concurrency_cap() {
    let f = Fixture::new();
    f.ready();
    let file = f.root.join(".mdkg/config.json");
    let mut cfg: Value =
        serde_json::from_slice(&fs::read(&file).expect("receiver fixture succeeds"))
            .expect("receiver fixture succeeds");
    cfg["agents"] = json!({"defaults":{"concurrency":1}});
    fs::write(file, cfg.to_string()).expect("receiver fixture succeeds");
    let result = f.call("launcher", &["agents", "up", "alice", "--once"]);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stdout).contains("concurrency cap"));
    assert_eq!(f.calls(), 0);
}

#[test]
fn install_and_undo_exclude_interactive_dispatch_after_installation_check() {
    let f = Fixture::new();
    let install = |args: &[&str]| {
        f.command("installer")
            .current_dir(&f.cwd)
            .args(args)
            .arg("--json")
            .output()
            .unwrap()
    };
    assert!(install(&["install"]).status.success());
    f.ready();
    f.post("question");
    assert!(!f.root.join(".git/mdkg/run/alice.json").exists());
    let config = fs::read(f.cwd.join(".mdkg/config.json")).unwrap();
    let hooks = fs::read(f.cwd.join(".codex/hooks.json")).unwrap();

    // headers() follows both installation checks, and precedes the loop-guard
    // lock. Observe that call, then keep dispatch paused on the real lock.
    let dir = f.root.join(".git/mdkg/run");
    let guard_path = mdkg_run::supervise::guard_path(&dir, "alice").with_extension("lock");
    fs::create_dir_all(guard_path.parent().unwrap()).unwrap();
    let guard = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(guard_path)
        .unwrap();
    guard.lock().unwrap();
    let checked = f.root.join("installation-checked");
    let shim = f.root.join("mail-shim");
    fs::write(&shim, "#!/bin/sh\nfor arg in \"$@\"; do\nif [ \"$arg\" = inbox ]; then : > \"$INSTALL_CHECKED\"; fi\ndone\nexec \"$REAL_MDKG\" \"$@\"\n").unwrap();
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o700)).unwrap();
    let release = f.root.join("release-queue");
    f.fake("while [ ! -f \"$RELEASE_QUEUE\" ]; do /bin/sleep 0.01; done");
    let mut child = f
        .command("launcher")
        .args(["agents", "up", "alice", "--once", "--json"])
        .env("MDKG_RUN_MDKG", &shim)
        .env("REAL_MDKG", env!("CARGO_BIN_EXE_mdkg"))
        .env("INSTALL_CHECKED", &checked)
        .env("RELEASE_QUEUE", &release)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for_ready(
        &mut child,
        "receiver never passed its installation check",
        || checked.exists().then_some(()),
    );
    assert_eq!(f.calls(), 0);
    let assert_excluded = || {
        for args in [
            &["install", "--disable", "hooks:codex"][..],
            &["install", "--undo"][..],
        ] {
            let out = install(args);
            assert_eq!(
                out.status.code(),
                Some(5),
                "{args:?}: {} {}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(String::from_utf8_lossy(&out.stdout).contains("coord.lease_held"));
            assert_eq!(fs::read(f.cwd.join(".mdkg/config.json")).unwrap(), config);
            assert_eq!(fs::read(f.cwd.join(".codex/hooks.json")).unwrap(), hooks);
        }
    };
    assert_excluded();
    guard.unlock().unwrap();
    wait_for_ready(&mut child, "queue never started", || {
        (f.calls() == 1).then_some(())
    });
    assert_excluded();
    // A new route location cannot authorize install over an already admitted
    // queue in the old worktree. The active run lease remains authoritative.
    let elsewhere = f.root.join("elsewhere");
    fs::create_dir(&elsewhere).unwrap();
    let mut moved = f.route();
    moved.cwd = Some(elsewhere);
    fs::write(
        f.root.join(".git/mdkg/hooks/receiver/alice.json"),
        serde_json::to_vec(&moved).unwrap(),
    )
    .unwrap();
    assert_excluded();
    fs::write(release, "go").unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{} {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("queued"));
    assert!(
        install(&["install", "--disable", "hooks:codex"])
            .status
            .success()
    );
    assert!(install(&["install", "--undo"]).status.success());
}

#[test]
fn install_and_undo_include_active_turns_without_saved_state_or_registration() {
    let f = Fixture::new();
    f.data("installer", &["install"]);
    f.data("holder", &["claim", "run:alice"]);
    for args in [&["install"][..], &["install", "--undo"][..]] {
        let out = f.call("installer", args);
        assert_eq!(
            out.status.code(),
            Some(5),
            "{}",
            String::from_utf8_lossy(&out.stdout)
        );
        assert!(String::from_utf8_lossy(&out.stdout).contains("coord.lease_held"));
        assert_eq!(f.data("holder", &["holds", "run:alice"])["holds"], true);
    }
    f.data("holder", &["release", "run:alice"]);
    f.data("installer", &["install", "--undo"]);
}

#[test]
fn install_holds_idle_interactive_registration_before_writing() {
    let f = Fixture::new();
    f.ready();
    assert!(!f.root.join(".git/mdkg/run/alice.json").exists());
    let common = f.root.join(".git");
    let path =
        mdkg_run::install::undo_dir(&common.join("mdkg/install"), &f.cwd).with_extension("lock");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .unwrap();
    lock.lock().unwrap();
    let mut child = f
        .command("installer")
        .current_dir(&f.cwd)
        .args(["install", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let holder = format!("installer@install-{}", child.id());
    let record_path = mdkg_run::state::lease_record_path(&common.join("mdkg/run"), "alice");
    wait_for_ready(
        &mut child,
        "install never held the registered interactive actor",
        || {
            let record: mdkg_run::state::LeaseRecord =
                serde_json::from_slice(&fs::read(&record_path).ok()?).ok()?;
            (record.holder == holder).then_some(())
        },
    );
    assert_eq!(f.data(&holder, &["holds", "run:alice"])["holds"], true);
    assert!(!f.cwd.join(".codex/hooks.json").exists());
    // No saved agent exists, but its registered conversation is excluded.
    let out = f.call("receiver", &["claim", "run:alice"]);
    assert_eq!(out.status.code(), Some(5));
    lock.unlock().unwrap();
    assert!(child.wait().unwrap().success());
    assert!(f.cwd.join(".codex/hooks.json").exists());
    assert!(!record_path.exists());
    f.data("receiver", &["claim", "run:alice"]);
    f.data("receiver", &["release", "run:alice"]);
}

#[test]
fn headless_setup_rechecks_intent_before_saving_a_spec() {
    let f = Fixture::new();
    let config = f.root.join(".mdkg/config.json");
    let clean = f.root.join("clean-config");
    fs::copy(&config, &clean).expect("fixture config");
    let mut cfg: Value = serde_json::from_slice(&fs::read(&config).expect("config")).expect("json");
    cfg["agents"] = json!({"alice":{"cwd":f.cwd,"harness":"codex","role":"reviewer"}});
    fs::write(&config, cfg.to_string()).expect("configured actor");
    let shim = f.root.join("mail-shim");
    fs::write(&shim,"#!/bin/sh\nfor arg in \"$@\"; do\nif [ \"$arg\" = claim ]; then /bin/cp \"$CLEAN_CONFIG\" \"$TEST_CONFIG\"; fi\ndone\nexec \"$REAL_MDKG\" \"$@\"\n").expect("shim");
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o700)).expect("mode");
    let result = f
        .command("launcher")
        .env("MDKG_RUN_MDKG", &shim)
        .env("REAL_MDKG", env!("CARGO_BIN_EXE_mdkg"))
        .env("CLEAN_CONFIG", &clean)
        .env("TEST_CONFIG", &config)
        .args(["agents", "up", "alice", "--once", "--json"])
        .output()
        .expect("up");
    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stdout).contains("became interactive"),
        "{}",
        String::from_utf8_lossy(&result.stdout)
    );
    assert!(!f.root.join(".git/mdkg/run/alice.json").exists());
    assert_eq!(f.calls(), 0);
}

#[test]
fn large_body_batches_are_small_header_pages_and_stay_unshown() {
    if clean_child("large_body_batches_are_small_header_pages_and_stay_unshown") {
        return;
    }
    use mdkg_run::Mail;
    let f = Fixture::new();
    let body = "x".repeat(63 * 1024);
    let mut ids = Vec::new();
    // Enough valid body bytes to exceed the original reader's 4 MiB cap
    // when a large batch policy asks for 200 messages.
    for _ in 0..67 {
        let m = f.data(
            "sender",
            &[
                "msg", "send", "--to", "alice", "--kind", "note", "--text", &body,
            ],
        );
        ids.push(m["message"]["messageId"].as_str().expect("id").to_string());
    }
    let mut mail = mdkg_run::CliMail::new(
        env!("CARGO_BIN_EXE_mdkg").into(),
        f.root.clone(),
        "launcher".into(),
    );
    let page = mail
        .headers("alice", None, 200)
        .expect("bounded header read");
    assert_eq!(page.messages.len(), 5);
    assert!(page.more);
    let next = mail
        .headers("alice", page.cursor, 200)
        .expect("next headers");
    assert_eq!(next.messages.len(), 5);
    assert!(next.messages[0].seq > page.messages[4].seq);
    assert!(!f.call("alice", &["msg", "ack", &ids[0]]).status.success());
}

#[test]
fn supervisor_interruption_stops_a_delayed_queue_and_keeps_reservation() {
    use std::os::unix::process::CommandExt;
    for signal in ["-INT", "-KILL"] {
        let f = Fixture::new();
        f.ready();
        f.post("question");
        f.fake("printf '%s' \"$$\" > \"$QUEUE_PID\"; while [ ! -e \"$RECEIVER_RELEASE\" ]; do /bin/sleep 0.01; done; printf 'LATE\\n' >> \"$RECEIVER_ARGV\"");
        let release = f.root.join("release-canary");
        let queue_pid = f.root.join("queue-pid");
        let mut child = f
            .command("launcher")
            .args(["agents", "up", "alice", "--once", "--json"])
            .env("RECEIVER_RELEASE", &release)
            .env("QUEUE_PID", &queue_pid)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .expect("supervisor");
        let queue = wait_for_ready(&mut child, "queue did not start", || {
            recorded_pid(&queue_pid)
        });
        let killed = Command::new("/bin/kill")
            .args([signal, "--", &format!("-{}", child.id())])
            .status()
            .expect("signal supervisor group");
        assert!(killed.success());
        assert!(!child.wait().expect("reap supervisor").success());
        // Verify death before releasing the canary, rather than assuming a
        // surviving process would be scheduled during a fixed sleep window.
        wait_pid_gone(queue, STARTUP_ALLOWANCE);
        fs::write(release, "go").expect("release canary");
        let argv = fs::read_to_string(f.root.join("argv")).expect("argv");
        assert!(
            !argv.contains("LATE"),
            "queue escaped parent death: {signal}"
        );
        assert_eq!(
            f.route().last_attempt.expect("attempt").delivery,
            route::Delivery::DeliveryUnknown
        );
        assert!(
            hooks::try_reminded_through(&f.root.join(".git"), "alice", SESSION).expect("watermark")
                > 0
        );
    }
}

#[test]
fn resume_reclaims_full_provenance_without_treating_old_children_as_primary() {
    let f = Fixture::new();
    f.ready();
    let path = f.root.join(".git/mdkg/hooks/receiver/alice.json");
    let mut r = serde_json::to_value(f.route()).expect("route");
    let old_child = uuid::Uuid::now_v7().to_string();
    let mut children = vec![old_child.clone()];
    children.extend((0..254).map(|_| uuid::Uuid::now_v7().to_string()));
    r["subagents"] = json!(children);
    fs::write(&path, r.to_string()).expect("full provenance fixture");
    assert_eq!(
        f.data("launcher", &["agents", "resume", "alice"])["waiting_for_prompt"],
        true
    );
    assert_eq!(
        f.data("launcher", &["agents", "resume", "alice"])["waiting_for_prompt"],
        true
    );
    f.hook_start(OTHER, None);
    f.observe(OTHER, HarnessEvent::UserPromptSubmit, true);
    assert_eq!(f.route().session.as_deref(), Some(OTHER));
    assert!(!f.route().suspended);
    f.observe(&old_child, HarnessEvent::UserPromptSubmit, true);
    assert!(
        f.route().suspended,
        "discarded provenance cannot silently classify a child as primary"
    );
}

#[test]
fn independent_timeout_stops_queue_while_supervisor_is_stopped() {
    use std::os::unix::process::CommandExt;
    let f = Fixture::new();
    f.ready();
    f.post("question");
    let path = f.root.join(".mdkg/config.json");
    let mut cfg: Value = serde_json::from_slice(&fs::read(&path).expect("config")).expect("json");
    // This native CLI smoke check keeps the real clock. Startup gets the same
    // allowance as every other process fixture; unit tests advance a fake
    // clock after readiness to cover the short deadline edge deterministically.
    let queue_timeout = STARTUP_ALLOWANCE.as_secs();
    cfg["agents"] = json!({"defaults":{"timeout":queue_timeout}});
    fs::write(path, cfg.to_string()).expect("timeout");
    let guard_pid = f.root.join("guard-pid");
    let queue_pid = f.root.join("queue-pid");
    let release = f.root.join("release-canary");
    f.fake("printf '%s' \"$PPID\" > \"$GUARD_PID\"; printf '%s' \"$$\" > \"$QUEUE_PID\"; while [ ! -e \"$RECEIVER_RELEASE\" ]; do /bin/sleep 0.01; done; printf 'LATE\\n' >> \"$RECEIVER_ARGV\"");
    let mut child = f
        .command("launcher")
        .args(["agents", "up", "alice", "--once", "--json"])
        .env("GUARD_PID", &guard_pid)
        .env("QUEUE_PID", &queue_pid)
        .env("RECEIVER_RELEASE", &release)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .expect("supervisor");
    // Both PID receipts gate SIGSTOP. The independent guard must cancel the
    // queue without any progress by the stopped receiver.
    let (native, queue) = wait_for_ready(&mut child, "queue did not start", || {
        Some((recorded_pid(&guard_pid)?, recorded_pid(&queue_pid)?))
    });
    assert!(
        Command::new("/bin/kill")
            .args(["-STOP", "--", &child.id().to_string()])
            .status()
            .expect("stop")
            .success()
    );
    wait_pid_gone(
        native,
        STARTUP_ALLOWANCE + Duration::from_secs(queue_timeout),
    );
    wait_pid_gone(queue, STARTUP_ALLOWANCE);
    fs::write(release, "go").expect("release only after cancellation");
    let late = fs::read_to_string(f.root.join("argv"))
        .expect("argv")
        .contains("LATE");
    assert!(
        Command::new("/bin/kill")
            .args(["-CONT", "--", &child.id().to_string()])
            .status()
            .expect("continue")
            .success()
    );
    assert!(child.wait().expect("supervisor exit").success());
    assert!(
        !late,
        "independent timeout failed while supervisor was stopped"
    );
    assert_eq!(
        f.route().last_attempt.expect("attempt").delivery,
        route::Delivery::DeliveryUnknown
    );
}

#[test]
fn failed_status_observation_latches_route_before_shared_registry_access() {
    let f = Fixture::new();
    f.ready();
    let lock = f.root.join(".git/mdkg/hooks/threads.lock");
    fs::remove_file(&lock).expect("remove test lock");
    fs::create_dir(&lock).expect("fail status writes");
    let output = f.hook_start_output(OTHER, None);
    assert!(!output.status.success());
    assert!(f.route().suspended);
    assert!(f.route().observation_pending);
    fs::remove_dir(lock).expect("restore status storage");
    // A new receiver invocation simulates restart; no surviving hook process
    // or general-status record is needed to preserve the conflicting evidence.
    f.post("question");
    assert_eq!(f.up()["wake"]["status"], "suspended");
    f.observe(SESSION, HarnessEvent::Stop, false);
    assert!(f.route().suspended);
    assert_eq!(f.up()["wake"]["status"], "suspended");
    assert_eq!(f.calls(), 0);
}

#[test]
fn evidence_only_registration_is_visible_and_resettable() {
    let f = Fixture::new();
    let evidence = f.root.join(".git/mdkg/hooks/receiver/alice.pending");
    fs::create_dir_all(&evidence).unwrap();
    // The first durable write of a hook which died before making any route.
    fs::write(evidence.join(uuid::Uuid::now_v7().to_string()), "").unwrap();
    let status = f.data("launcher", &["agents", "status"]);
    let receiver = status["receivers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["actor"] == "alice")
        .expect("incomplete registration is visible");
    assert_eq!(receiver["route"]["observation_pending"], true);
    assert_eq!(receiver["route"]["session"], Value::Null);
    // Retained observed evidence alone has the same discovery/recovery path.
    let observed = evidence.with_extension("observed");
    fs::rename(&evidence, &observed).unwrap();
    let status = f.data("launcher", &["agents", "status"]);
    assert_eq!(status["receivers"][0]["route"]["observation_pending"], true);
    f.post("question");
    assert_eq!(f.up()["wake"]["status"], "suspended");
    assert_eq!(f.calls(), 0);
    assert_eq!(
        f.data("launcher", &["agents", "resume", "alice"])["waiting_for_prompt"],
        true
    );
    assert_eq!(fs::read_dir(evidence).unwrap().count(), 0);
    assert_eq!(fs::read_dir(observed).unwrap().count(), 0);
    f.ready();
    f.observe(SESSION, HarnessEvent::UserPromptSubmit, true);
    f.observe(SESSION, HarnessEvent::Stop, false);
    f.post("handoff");
    assert_eq!(f.up()["wake"]["status"], "queued");
    assert_eq!(f.calls(), 1);
}

#[test]
fn abandoned_marker_blocks_restart_after_registry_pruning_until_full_resume() {
    let f = Fixture::new();
    f.ready();
    let common = f.root.join(".git");
    let old = fs::read(common.join("mdkg/hooks/receiver/alice.json")).expect("old route");
    let _pending = route::begin_observation(
        &common,
        "alice",
        OTHER,
        &f.cwd,
        &HookObservation {
            event: HookEvent::Start,
            harness_event: Some(HarnessEvent::SessionStart),
            start_provenance: Some(hooks::StartProvenance::parse(Some("startup"), None)),
            at_ms: mdkg_run::state::now_ms(),
        },
    )
    .expect("pending observation");
    // Reproduce the old route at the pre-truncation boundary; the actual
    // failure/crash injection lives in dispatch's private-seam unit tests.
    fs::write(common.join("mdkg/hooks/receiver/alice.json"), old).expect("old disk authority");
    fs::remove_file(common.join(format!("mdkg/hooks/threads/codex-{OTHER}.json")))
        .expect("prune conflicting status");
    f.post("question");
    assert_eq!(f.up()["wake"]["status"], "suspended");
    assert_eq!(f.calls(), 0);
    assert!(f.route().session.is_some());
    let status = f.data("launcher", &["agents", "status"]);
    let receiver = status["receivers"]
        .as_array()
        .expect("receivers")
        .iter()
        .find(|r| r["actor"] == "alice")
        .expect("alice");
    assert_eq!(receiver["route"]["observation_pending"], true);
    assert_eq!(
        f.data("launcher", &["agents", "resume", "alice"])["waiting_for_prompt"],
        true
    );
    f.observe(SESSION, HarnessEvent::UserPromptSubmit, true);
    f.observe(SESSION, HarnessEvent::Stop, false);
    assert_eq!(f.up()["wake"]["status"], "queued");
    assert_eq!(f.calls(), 1);
}

#[test]
fn corrupt_registry_refuses_before_reserving_the_reminder() {
    let f = Fixture::new();
    f.ready();
    let common = f.root.join(".git");
    let registry = common.join(format!("mdkg/hooks/threads/codex-{SESSION}.json"));
    let saved = fs::read(&registry).expect("thread");
    fs::write(&registry, b"{").expect("damaged registry");
    f.post("question");
    let out = f.call("launcher", &["agents", "up", "alice", "--once"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("trouble"));
    assert_eq!(
        hooks::try_reminded_through(&common, "alice", SESSION).expect("reminder"),
        0
    );
    assert_eq!(f.calls(), 0);
    let status = f.data("launcher", &["agents", "status"]);
    assert!(status["receivers"][0]["problem"].as_str().is_some());
    fs::write(registry, saved).expect("restore registry");
    assert_eq!(f.up()["wake"]["status"], "queued");
}

#[test]
fn incomplete_or_partial_observation_cannot_reuse_old_idle_authority() {
    let f = Fixture::new();
    f.ready();
    let before = f.route().generation;
    let observation = HookObservation {
        event: HookEvent::Start,
        harness_event: Some(HarnessEvent::UserPromptSubmit),
        start_provenance: None,
        at_ms: mdkg_run::state::now_ms(),
    };
    let _unfinished =
        route::begin_observation(&f.root.join(".git"), "alice", SESSION, &f.cwd, &observation)
            .expect("begin");
    assert!(f.route().generation > before);
    assert!(f.route().observation_pending);
    f.post("question");
    assert_eq!(f.up()["wake"]["status"], "suspended");
    // A crash after truncation leaves unreadable authority, never the old
    // ready record. Repeated independent invocations must keep refusing.
    fs::write(
        f.root.join(".git/mdkg/hooks/receiver/alice.json"),
        b"{\"generation\":",
    )
    .expect("partial write");
    for _ in 0..2 {
        let out = f.call("launcher", &["agents", "up", "alice", "--once"]);
        assert!(String::from_utf8_lossy(&out.stdout).contains("trouble"));
    }
    assert_eq!(f.calls(), 0);
}

#[test]
fn legacy_routes_need_a_complete_observation_and_stale_tickets_cannot_clear_pending() {
    let f = Fixture::new();
    f.ready();
    let path = f.root.join(".git/mdkg/hooks/receiver/alice.json");
    let mut legacy = serde_json::to_value(f.route()).expect("route");
    legacy.as_object_mut().expect("object").remove("generation");
    legacy
        .as_object_mut()
        .expect("object")
        .remove("observation_pending");
    fs::write(path, legacy.to_string()).expect("legacy record");
    assert!(f.route().observation_pending);
    let observation = HookObservation {
        event: HookEvent::Stop,
        harness_event: Some(HarnessEvent::Stop),
        start_provenance: None,
        at_ms: mdkg_run::state::now_ms(),
    };
    let common = f.root.join(".git");
    let first =
        route::begin_observation(&common, "alice", SESSION, &f.cwd, &observation).expect("first");
    let second =
        route::begin_observation(&common, "alice", SESSION, &f.cwd, &observation).expect("second");
    route::finish_observation(&common, "alice", first, ThreadState::Idle).expect("stale finish");
    assert!(f.route().observation_pending);
    route::finish_observation(&common, "alice", second, ThreadState::Idle).expect("current finish");
    assert!(!f.route().observation_pending);
}

#[test]
fn hooks_projection_is_required_for_registration_and_dispatch() {
    let f = Fixture::new();
    let path = f.cwd.join(".mdkg/config.json");
    let enabled = fs::read(&path).expect("config");
    let mut config: Value = serde_json::from_slice(&enabled).expect("json");
    config
        .as_object_mut()
        .expect("config object")
        .remove("projections");
    fs::write(&path, config.to_string()).expect("legacy defaults");
    f.hook_start(SESSION, None);
    assert!(
        route::read(&f.root.join(".git"), "alice")
            .expect("route read")
            .is_none()
    );
    fs::write(&path, enabled).expect("enable");
    f.ready();
    config["projections"] = json!([]);
    fs::write(&path, config.to_string()).expect("disable");
    f.post("question");
    assert_eq!(f.up()["wake"]["status"], "waiting");
    assert_eq!(f.calls(), 0);
}

#[test]
fn explicit_resume_preserves_partial_route_and_requires_fresh_registration() {
    let f = Fixture::new();
    f.ready();
    let folder = f.root.join(".git/mdkg/hooks/receiver");
    let path = folder.join("alice.json");
    let broken = b"{\"generation\":";
    fs::write(&path, broken).expect("crashed write");
    f.post("question");
    let start = f.hook_start_output(SESSION, None);
    assert!(
        String::from_utf8_lossy(&start.stderr).contains("couldn't record the thread observation")
    );
    assert_eq!(fs::read(&path).expect("original"), broken);
    let rejected = f.call(
        "launcher",
        &[
            "agents",
            "resume",
            "alice",
            "--thread",
            &format!("codex:{SESSION}"),
        ],
    );
    assert!(!rejected.status.success());
    assert_eq!(
        f.data("launcher", &["agents", "resume", "alice"])["waiting_for_prompt"],
        true
    );
    let saved: Vec<_> = fs::read_dir(&folder)
        .expect("records")
        .map(|e| e.expect("entry").path())
        .filter(|p| {
            p.file_name()
                .expect("name")
                .to_string_lossy()
                .starts_with("alice.json.damaged-")
        })
        .collect();
    assert_eq!(saved.len(), 1);
    assert_eq!(fs::read(&saved[0]).expect("preserved"), broken);
    assert_eq!(
        fs::metadata(&saved[0])
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert!(f.route().session.is_none());
    assert!(f.route().observation_pending);
    assert_eq!(f.up()["wake"]["status"], "suspended");
    // With damaged provenance discarded, even the prior primary must identify
    // itself again. A Stop or unclassified prompt alone cannot select it.
    f.observe(SESSION, HarnessEvent::Stop, false);
    f.observe(SESSION, HarnessEvent::UserPromptSubmit, true);
    assert!(f.route().session.is_none());
    f.hook_start(SESSION, None);
    assert!(f.route().session.is_none());
    f.observe(SESSION, HarnessEvent::UserPromptSubmit, true);
    assert_eq!(f.route().session.as_deref(), Some(SESSION));
    assert_eq!(f.calls(), 0);
}

#[test]
fn explicit_resume_never_repairs_a_link_or_directory_route() {
    let f = Fixture::new();
    f.ready();
    let path = f.root.join(".git/mdkg/hooks/receiver/alice.json");
    fs::remove_file(&path).expect("remove fixture");
    let outside = f.root.join("untouched");
    fs::write(&outside, "broken").expect("target");
    std::os::unix::fs::symlink(&outside, &path).expect("link");
    assert!(
        !f.call("launcher", &["agents", "resume", "alice"])
            .status
            .success()
    );
    assert!(
        fs::symlink_metadata(&path)
            .expect("link")
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read_to_string(&outside).expect("target"), "broken");
    fs::remove_file(&path).expect("remove link");
    fs::create_dir(&path).expect("directory");
    assert!(
        !f.call("launcher", &["agents", "resume", "alice"])
            .status
            .success()
    );
    assert!(path.is_dir());
}

#[test]
fn lease_expiry_during_registry_wait_refuses_queue_and_preserves_takeover() {
    for takeover in [false, true] {
        let f = Fixture::new();
        f.ready();
        f.post("question");
        // Clock pinning is scoped to child commands. A normal two-hour lease
        // cannot expire on the way to the lock, regardless of CPU scheduling.
        // Advance only after observing that the receiver holds the route lock.
        let now = mdkg_run::state::now_ms() + 3_600_000;
        let clock = f.root.join("coordinator-clock");
        fs::write(&clock, now.to_string()).unwrap();
        let common = f.root.join(".git");
        let registry = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(common.join("mdkg/hooks/threads.lock"))
            .unwrap();
        registry.lock().unwrap();
        let probe = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(common.join("mdkg/hooks/receiver/alice.lock"))
            .unwrap();
        let mut child = f
            .command("launcher")
            .args(["agents", "up", "alice", "--once", "--json"])
            .env("MDKG_RUN_MDKG", f.renewal_shim())
            .env("REAL_MDKG", env!("CARGO_BIN_EXE_mdkg"))
            .env("RECEIVER_CLOCK", &clock)
            .env("RENEWAL_COUNT", f.root.join("renewal-count"))
            .env("FAIL_RENEWAL", "9999")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        wait_for_ready(
            &mut child,
            "receiver never reached registry wait",
            || match probe.try_lock() {
                Err(fs::TryLockError::WouldBlock) => Some(()),
                Ok(()) => {
                    probe.unlock().unwrap();
                    None
                }
                Err(e) => panic!("{e}"),
            },
        );
        let expired = now + 2 * 60 * 60 * 1000 + 1;
        fs::write(&clock, expired.to_string()).unwrap();
        let dir = common.join("mdkg/run");
        let path = mdkg_run::state::lease_record_path(&dir, "alice");
        let replacement = mdkg_run::turn::lease_record("replacement".into());
        if takeover {
            let _lock = mdkg_run::state::lock_lease_record(&dir, "alice").unwrap();
            let out = f
                .command("replacement")
                .args(["claim", "run:alice", "--json"])
                .env("MDKG_NOW_MS", expired.to_string())
                .output()
                .unwrap();
            assert!(out.status.success(), "{out:?}");
            fs::write(&path, serde_json::to_vec(&replacement).unwrap()).unwrap();
        }
        registry.unlock().unwrap();
        wait_for_exit(&mut child, "receiver didn't abort after lease expiry");
        let out = child.wait_with_output().unwrap();
        assert_eq!(
            f.calls(),
            0,
            "expired receiver queued: {}",
            String::from_utf8_lossy(&out.stdout)
        );
        let event: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(event["data"]["wake"]["status"], "not_sent", "{event}");
        if takeover {
            assert_eq!(
                serde_json::from_slice::<mdkg_run::state::LeaseRecord>(&fs::read(path).unwrap())
                    .unwrap(),
                replacement
            );
            assert_eq!(
                f.data("replacement", &["holds", "run:alice"])["holds"],
                true
            );
        }
    }
}

#[test]
fn paused_guard_startup_past_absolute_deadline_does_not_launch() {
    use rustix::process::{Pid, Signal, WaitOptions, kill_process, waitpid};
    use std::os::unix::process::CommandExt;
    use std::time::Duration;
    let f = Fixture::new();
    let deadline = mdkg_run::state::now_ms() + 700;
    let spec = json!({
        "deadline": deadline,
        "program": f.program, "session": SESSION, "line": "headers"
    });
    // Suspend before even exec-ing mdkg: neither argv parsing nor process
    // creation may give the new guard a fresh relative countdown.
    let mut child = Command::new("/bin/sh")
        .args([
            "-p",
            "-c",
            "kill -STOP $$; \"$@\" \"$$\" & child=$!; wait \"$child\"",
            "paused-guard",
        ])
        .arg(env!("CARGO_BIN_EXE_mdkg"))
        .arg("__receiver-queue-guard")
        .arg(spec.to_string())
        .current_dir(&f.root)
        .env("RECEIVER_ARGV", f.root.join("argv"))
        .env("RECEIVER_CALLS", f.root.join("calls"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .expect("paused guard");
    let pid = Pid::from_raw(child.id() as i32).expect("pid");
    assert!(
        waitpid(Some(pid), WaitOptions::UNTRACED)
            .expect("wait for startup pause")
            .expect("child stopped")
            .1
            .stopped()
    );
    std::thread::sleep(Duration::from_millis(
        deadline.saturating_sub(mdkg_run::state::now_ms()) + 50,
    ));
    kill_process(pid, Signal::CONT).expect("resume guard after expiry");
    assert_eq!(child.wait().expect("guard refuses").code(), Some(125));
    assert_eq!(f.calls(), 0, "nothing capable of sending may start");
}

#[test]
fn absolute_guard_deadline_stops_running_child_and_never_reports_delivery() {
    use std::os::unix::process::CommandExt;
    use std::time::Duration;
    let f = Fixture::new();
    // Warm the disposable executable before applying a short runtime bound.
    f.command("launcher")
        .args(["--version"])
        .output()
        .expect("warm mdkg");
    Command::new(&f.program)
        .env("RECEIVER_ARGV", f.root.join("warm-argv"))
        .env("RECEIVER_CALLS", f.root.join("warm-calls"))
        .status()
        .expect("warm fake");
    let release = f.root.join("release-canary");
    let queue_pid = f.root.join("queue-pid");
    f.fake("printf '%s' \"$$\" > \"$QUEUE_PID\"; while [ ! -e \"$RECEIVER_RELEASE\" ]; do /bin/sleep 0.01; done; printf LATE >> \"$RECEIVER_ARGV\"");
    let deadline = mdkg_run::state::now_ms() + STARTUP_ALLOWANCE.as_millis() as u64 + 1000;
    let spec = json!({
        "deadline": deadline,
        "program": f.program, "session": SESSION, "line": "headers"
    });
    let mut child = Command::new("/bin/sh")
        .args([
            "-p",
            "-c",
            "\"$@\" \"$$\" & child=$!; wait \"$child\"",
            "guard-owner",
        ])
        .arg(env!("CARGO_BIN_EXE_mdkg"))
        .env("RECEIVER_ARGV", f.root.join("argv"))
        .env("RECEIVER_CALLS", f.root.join("calls"))
        .env("QUEUE_PID", &queue_pid)
        .env("RECEIVER_RELEASE", &release)
        .current_dir(&f.root)
        .arg("__receiver-queue-guard")
        .arg(spec.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .expect("guard");
    let queue = wait_for_ready(&mut child, "native guard never started queue", || {
        recorded_pid(&queue_pid)
    });
    let status = wait_for_exit_with(
        &mut child,
        "absolute guard deadline did not stop child",
        STARTUP_ALLOWANCE + Duration::from_secs(2),
    );
    assert!(!status.success());
    assert_ne!(status.code(), Some(125), "child actually started");
    assert_eq!(f.calls(), 1);
    let overshoot = mdkg_run::state::now_ms().saturating_sub(deadline);
    eprintln!("fake queue expiry: guard reaped {overshoot} ms after deadline (5 ms test polling)");
    wait_pid_gone(queue, STARTUP_ALLOWANCE);
    fs::write(release, "go").expect("release only after cancellation");
    assert!(
        !fs::read_to_string(f.root.join("argv"))
            .expect("argv")
            .contains("LATE")
    );
}

#[test]
fn terminating_native_guard_cleans_queue_even_with_stopped_receiver() {
    use rustix::process::{Pid, Signal, kill_process};
    use std::os::unix::process::CommandExt;
    for stopped in [false, true] {
        let f = Fixture::new();
        f.ready();
        f.post("question");
        // The fake's parent is the native clock guard. Its signal supervisor
        // must remain responsible for this group after that guard terminates.
        f.fake("printf '%s' \"$$\" > \"$QUEUE_PID\"; printf '%s' \"$PPID\" > \"$RECEIVER_GUARD_PID\"; while [ ! -e \"$RECEIVER_RELEASE\" ]; do /bin/sleep 0.01; done; printf LATE >> \"$RECEIVER_ARGV\"");
        let pid_file = f.root.join("native-guard-pid");
        let queue_pid = f.root.join("queue-pid");
        let release = f.root.join("release-canary");
        let mut receiver = f
            .command("launcher")
            .args(["agents", "up", "alice", "--once", "--json"])
            .env("RECEIVER_GUARD_PID", &pid_file)
            .env("QUEUE_PID", &queue_pid)
            .env("RECEIVER_RELEASE", &release)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .expect("receiver");
        let native = wait_for_ready(&mut receiver, "native guard did not start fake", || {
            let text = fs::read_to_string(&pid_file).ok()?;
            Pid::from_raw(text.parse::<i32>().ok()?)
        });
        let parent = Pid::from_raw(receiver.id() as i32).expect("receiver pid");
        if stopped {
            kill_process(parent, Signal::STOP).expect("stop receiver");
        }
        kill_process(native, Signal::TERM).expect("terminate only native guard");
        wait_pid_gone(
            recorded_pid(&queue_pid).expect("queue receipt"),
            STARTUP_ALLOWANCE,
        );
        fs::write(release, "go").expect("release canary after guard termination");
        if stopped {
            kill_process(parent, Signal::CONT).expect("continue receiver");
        }
        let _ = receiver.wait().expect("reap receiver");
        assert!(
            !fs::read_to_string(f.root.join("argv"))
                .expect("argv")
                .contains("LATE")
        );
        assert_eq!(
            f.route().last_attempt.expect("attempt").delivery,
            route::Delivery::DeliveryUnknown
        );
        assert!(
            hooks::try_reminded_through(&f.root.join(".git"), "alice", SESSION)
                .expect("reservation")
                > 0
        );
    }
}
