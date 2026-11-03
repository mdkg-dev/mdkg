//! Real coordinator delivery, isolated homes and fake harness processes.
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt as _;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};
mod common;

struct Fixture {
    dir: PathBuf,
    repo: PathBuf,
    root: PathBuf,
    home: PathBuf,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}
impl Fixture {
    fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut c = Command::new(program);
        c.env_clear()
            .env("PATH", std::env::var_os("PATH").expect("fixture PATH"))
            .env("HOME", &self.home)
            .env("CODEX_HOME", self.home.join(".codex"))
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("XDG_DATA_HOME", self.home.join(".local/share"))
            .env("XDG_STATE_HOME", self.home.join(".local/state"))
            .env("XDG_CACHE_HOME", self.home.join(".cache"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Fixture")
            .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
            .env("GIT_COMMITTER_NAME", "Fixture")
            .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
            .env("MDKG_RUN_SECURITY", "/usr/bin/false");
        for h in ["claude", "codex", "opencode"] {
            c.env(
                format!("MDKG_RUN_{}", h.to_uppercase()),
                self.dir.join(format!("{h}.sh")),
            );
        }
        c
    }
    fn git(&self, root: &Path, args: &[&str]) -> String {
        let out = self
            .command("git")
            .current_dir(root)
            .args(args)
            .output()
            .expect("execute fixture Git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout)
            .expect("UTF-8 Git output")
            .trim()
            .into()
    }
    fn output(&self, root: &Path, actor: &str, args: &[&str], override_transport: bool) -> Output {
        let mut c = self.command(env!("CARGO_BIN_EXE_mdkg"));
        c.current_dir(root)
            .arg("--root")
            .arg(root)
            .args(["--as", actor])
            .args(args)
            .arg("--json");
        if override_transport {
            c.env("MDKG_RUN_MDKG", env!("CARGO_BIN_EXE_mdkg"));
        }
        c.output().expect("execute fixture CLI")
    }
    fn ok(&self, root: &Path, actor: &str, args: &[&str]) -> Value {
        let out = self.output(root, actor, args, false);
        assert!(
            out.status.success(),
            "{args:?}: {}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).expect("CLI JSON envelope")
    }
    fn new(harness: &str, role: &str, fail: bool) -> Self {
        fn copy(source: &Path, dest: &Path) {
            fs::create_dir_all(dest).expect("create fixture destination");
            for e in fs::read_dir(source).expect("read fixture source") {
                let e = e.expect("read fixture entry");
                let to = dest.join(e.file_name());
                if e.file_type().expect("fixture entry type").is_dir() {
                    copy(&e.path(), &to);
                } else {
                    fs::copy(e.path(), to).expect("copy fixture file");
                }
            }
        }
        let dir = fs::canonicalize(common::temp::dir("mdkg-receipts"))
            .expect("canonical fixture directory");
        let f = Self {
            repo: dir.join("repo"),
            root: dir.join("repo/.worktrees/graph"),
            home: dir.join("home"),
            dir,
        };
        fs::create_dir(&f.home).expect("create fixture home");
        for h in ["claude", "codex", "opencode"] {
            let fake = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join(format!("../mdkg-run/tests/fixtures/fake-{h}.sh"));
            let action = if fail {
                "exit 7"
            } else if role == "worker" {
                "printf 'turn change\\n' > receipt-change.txt\n\"$MDKG_BIN\" claim receipt-change.txt --json > /dev/null\ngit add receipt-change.txt\ngit -c commit.gpgsign=false -c core.hooksPath=/dev/null commit -qm 'worker-owned receipt fixture'\n\"$MDKG_BIN\" release receipt-change.txt --json > /dev/null"
            } else {
                ":"
            };
            let script = format!(
                "#!/bin/sh\nset -eu\ncase \"${{1-}}\" in -p|exec|run)\nprintf 'call\\n' >> \"$HOME/agent-calls\"\n{action}\n;; esac\nexec '{}' \"$@\"\n",
                fake.display().to_string().replace('\'', "'\\''")
            );
            let path = f.dir.join(format!("{h}.sh"));
            fs::write(&path, script).expect("write fake harness");
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))
                .expect("make fake harness executable");
        }
        // The first commit and the migration don't depend on where the
        // fixture lives, so they're built once and copied. The build runs
        // with this test's HOME and harness folder, so it checks that
        // nothing it wrote names them: such a path would outlive this test.
        let conformance = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance");
        let (base, case) = (
            conformance.join("base"),
            conformance.join("cases/g01-valid-basic"),
        );
        fs::create_dir(&f.repo).expect("create fixture repository");
        common::template::copy(
            "receipts-repo",
            &[&base, &case],
            |repo| {
                copy(&base, repo);
                copy(&case, repo);
                fs::write(
                    repo.join(".gitignore"),
                    "*.args\n*.prompt\n*.environ\n/.worktrees/\n",
                )
                .expect("write fixture ignore rules");
                f.git(repo, &["init", "-q", "-b", "main"]);
                f.git(repo, &["add", "."]);
                f.git(repo, &["commit", "-qm", "fixture"]);
                let migration = f.ok(repo, "orchestrator", &["migrate", "--dry-run"]);
                f.ok(
                    repo,
                    "orchestrator",
                    &[
                        "migrate",
                        "--apply",
                        migration["data"]["plan"].as_str().expect("migration plan"),
                    ],
                );
                common::template::names_none(repo, &f.dir);
            },
            &f.repo,
        );
        f.git(
            &f.repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "mdkg-graph",
                f.root.to_str().expect("UTF-8 fixture root"),
                "main",
            ],
        );
        f.ok(&f.repo, "orchestrator", &["install"]);
        f.ok(
            &f.root,
            "orchestrator",
            &[
                "agents",
                "add",
                "probe",
                "--harness",
                harness,
                "--role",
                role,
            ],
        );
        f.ok(
            &f.root,
            "requester",
            &[
                "msg",
                "send",
                "--to",
                "probe",
                "--kind",
                "note",
                "--text",
                "Fixture turn",
            ],
        );
        f
    }
    fn up(&self, overridden: bool) -> (Output, Vec<Value>) {
        let out = self.output(
            &self.root,
            "orchestrator",
            &["agents", "up", "probe", "--once"],
            overridden,
        );
        let events: Vec<Value> = out
            .stdout
            .split(|b| *b == b'\n')
            .filter(|b| !b.is_empty())
            .map(|b| serde_json::from_slice(b).expect("event JSON envelope"))
            .collect();
        for event in &events {
            common::conforms_envelope(event);
        }
        (out, events)
    }
    fn status(&self) -> (Value, Value) {
        let report = self.ok(&self.root, "orchestrator", &["agents", "status"]);
        common::conforms_envelope(&report);
        let agent = report["data"]["agents"]
            .as_array()
            .expect("status agents")
            .iter()
            .find(|agent| agent["name"] == "probe")
            .expect("probe status")
            .clone();
        (report, agent)
    }
    fn receipt(&self) -> (Value, String) {
        // Hidden from an inbox read unless it names the kind (A26, R5).
        let plain = self.ok(&self.root, "orchestrator", &["msg", "inbox", "--all"]);
        common::conforms_envelope(&plain);
        assert!(
            plain["data"]["messages"]
                .as_array()
                .expect("inbox message list")
                .iter()
                .all(|m| m["kind"] != "receipt" && m["sender"] != "mdkg:receipts"),
            "{plain}"
        );
        let inbox = self.ok(
            &self.root,
            "orchestrator",
            &["msg", "inbox", "--all", "--kind", "receipt"],
        );
        common::conforms_envelope(&inbox);
        let messages = inbox["data"]["messages"]
            .as_array()
            .expect("inbox message list");
        let receipts: Vec<_> = messages
            .iter()
            .filter(|m| m["sender"] == "mdkg:receipts")
            .collect();
        assert_eq!(receipts.len(), 1, "{inbox}");
        assert!(receipts[0]["via"].is_null());
        // Kind `receipt`, kept a day: the retained file is the record (R4).
        assert_eq!(receipts[0]["kind"], "receipt");
        assert_eq!(receipts[0]["retention"], "ttl:1");
        let body = receipts[0]["parts"][0]["text"]
            .as_str()
            .expect("receipt text part")
            .to_string();
        let data: Value = serde_json::from_str(
            body.lines()
                .find(|line| line.starts_with('{'))
                .expect("receipt JSON line"),
        )
        .expect("receipt JSON data");
        (data, body)
    }
}

fn matrix(harness: &str, role: &str) {
    let f = Fixture::new(harness, role, false);
    let (out, events) = f.up(false);
    assert!(
        out.status.success(),
        "{events:?}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let turn = &events
        .iter()
        .find(|e| e["data"].get("turn").is_some())
        .expect("turn event")["data"]["turn"];
    assert_eq!(turn["outcome"], "replied");
    assert!(turn["receipt"]["error"].is_null(), "{turn}");
    let (data, body) = f.receipt();
    assert_eq!(data["agent"], "probe");
    assert_eq!(data["launcher_actor"], "orchestrator");
    assert_eq!(data["harness"], harness);
    assert_eq!(data["role"], role);
    assert_eq!(data["reply_id"], turn["reply_id"]);
    assert_eq!(data["reply_status"], "imported and acknowledged");
    let expected = if harness == "codex" {
        "{\"summary\": \"codex reply\", \"findings\": []}\n".to_string()
    } else {
        format!("{harness} reply")
    };
    assert_eq!(data["final_summary"], expected);
    assert_eq!(data["process_exits"], json!([{"code":0,"signal":null}]));
    assert!(data["activity"].get("unavailable").is_none(), "{data}");
    let claims = data["activity"]["claims"]
        .as_array()
        .expect("native claim facts");
    assert_eq!(claims.len(), usize::from(role == "worker"), "{data}");
    if role == "worker" {
        assert_eq!(claims[0]["scope"], "receipt-change.txt");
        assert!(
            data["activity"]["leases_held"]
                .as_array()
                .expect("held leases")
                .iter()
                .all(|lease| lease["scope"] != "receipt-change.txt"),
            "released in this turn"
        );
    }
    assert!(data["task_evidence_error"].is_null(), "{data}");
    assert_eq!(
        data["git"]["commits"]
            .as_array()
            .expect("commit list")
            .len(),
        usize::from(role == "worker")
    );
    assert!(body.contains("commits in the observed turn span ("));
    let retained = PathBuf::from(
        turn["receipt"]["path"]
            .as_str()
            .expect("retained receipt path"),
    );
    assert_eq!(
        fs::read_to_string(retained).expect("read retained receipt"),
        body
    );
    let (_, status) = f.status();
    assert_eq!(status["receipt"], turn["receipt"]);
    assert!(status["receipt_problem"].is_null());
    let (second, _) = f.up(false);
    assert!(second.status.success());
    assert_eq!(
        fs::read_to_string(f.home.join("agent-calls")).expect("fake harness call log"),
        "call\n"
    );
    f.receipt(); // Still exactly one: idle polling does not manufacture turns.
}
#[test]
fn claude_worker_receipt() {
    matrix("claude", "worker");
}
#[test]
fn claude_reviewer_receipt() {
    matrix("claude", "reviewer");
}
#[test]
fn codex_worker_receipt() {
    matrix("codex", "worker");
}
#[test]
fn codex_reviewer_receipt() {
    matrix("codex", "reviewer");
}
#[test]
fn opencode_worker_receipt() {
    matrix("opencode", "worker");
}
#[test]
fn opencode_reviewer_receipt() {
    matrix("opencode", "reviewer");
}

#[test]
fn receipt_transport_override_keeps_success_and_local_bytes_without_delivery_or_rerun() {
    let f = Fixture::new("claude", "worker", false);
    let (out, events) = f.up(true);
    assert!(out.status.success(), "{events:?}");
    let turn = &events
        .iter()
        .find(|e| e["data"].get("turn").is_some())
        .expect("turn event")["data"]["turn"];
    assert_eq!(turn["outcome"], "replied");
    assert_eq!(
        turn["receipt"]["error"],
        "receipt delivery refused: MDKG_RUN_MDKG overrides the mail transport"
    );
    let path = PathBuf::from(
        turn["receipt"]["path"]
            .as_str()
            .expect("retained receipt path"),
    );
    assert!(fs::read_to_string(path).unwrap().contains("claude reply"));
    let status = mdkg_run::turn::receipt::delivery::latest(&f.repo.join(".git/mdkg/run"), "probe")
        .unwrap()
        .unwrap();
    assert!(status.message_id.is_none());
    assert!(status.error.unwrap().contains("MDKG_RUN_MDKG"));
    let inbox = f.ok(
        &f.root,
        "orchestrator",
        &["msg", "inbox", "--all", "--kind", "receipt"],
    );
    assert!(
        inbox["data"]["messages"]
            .as_array()
            .expect("inbox message list")
            .is_empty()
    );
    assert!(events.iter().any(|event| {
        event["ok"] == true
            && event["diagnostics"]
                .as_array()
                .expect("event diagnostics")
                .iter()
                .any(|d| {
                    d["severity"] == "warning"
                        && d["message"]
                            .as_str()
                            .unwrap_or_default()
                            .contains("MDKG_RUN_MDKG")
                })
    }));
    let (report, status) = f.status();
    assert_eq!(status["receipt"], turn["receipt"]);
    assert!(status["problem"].is_null());
    assert_eq!(report["ok"], true);
    assert!(report["diagnostics"].as_array().unwrap().iter().any(|d| {
        d["severity"] == "warning"
            && d["message"]
                .as_str()
                .unwrap_or_default()
                .contains("MDKG_RUN_MDKG")
    }));
    let (second, _) = f.up(true);
    assert!(second.status.success());
    assert_eq!(
        fs::read_to_string(f.home.join("agent-calls")).expect("fake harness call log"),
        "call\n"
    );
}

#[test]
fn harness_failure_delivers_receipt_without_replacing_failure() {
    let f = Fixture::new("claude", "worker", true);
    let (out, _) = f.up(false);
    assert!(!out.status.success());
    let (data, body) = f.receipt();
    assert_eq!(data["result"]["status"], "error");
    assert_eq!(data["process_exits"], json!([{"code":7,"signal":null}]));
    assert!(data["final_summary"].is_null());
    let status = mdkg_run::turn::receipt::delivery::latest(&f.repo.join(".git/mdkg/run"), "probe")
        .unwrap()
        .unwrap();
    assert_eq!(fs::read_to_string(status.path.unwrap()).unwrap(), body);
    assert_eq!(
        fs::read_to_string(f.home.join("agent-calls")).expect("fake harness call log"),
        "call\n"
    );
}

#[test]
fn long_summaries_deliver_previews_and_keep_full_local_receipts() {
    for summary in [
        "x".repeat(70_000),
        "日本語🙂".repeat(5000),
        format!(
            "credential: {}{}",
            "ghp_", "abcdefghijklmnopqrstuvwxyz0123456789"
        ),
    ] {
        let f = Fixture::new("claude", "worker", false);
        let inbox = f.ok(&f.root, "probe", &["msg", "inbox"]);
        let id = inbox["data"]["messages"][0]["messageId"].as_str().unwrap();
        f.ok(&f.root, "probe", &["msg", "ack", id]);
        let result = json!({"type":"result","subtype":"success","is_error":false,"result":summary});
        fs::write(f.dir.join("result.json"), result.to_string()).unwrap();
        let script = format!(
            "#!/bin/sh\nset -eu\nif [ \"$1\" = auth ]; then echo '{{\"loggedIn\":true,\"authMethod\":\"claude.ai\"}}'; exit; fi\ncat > /dev/null\nprintf 'call\\n' >> \"$HOME/agent-calls\"\ncat '{}'\n",
            f.dir.join("result.json").display()
        );
        fs::write(f.dir.join("claude.sh"), script).unwrap();
        let out = f.output(
            &f.root,
            "orchestrator",
            &["agents", "up", "probe", "--once", "--prompt", "task only"],
            false,
        );
        let event = common::one_turn(&out.stdout);
        assert!(out.status.success(), "{event}");
        assert_eq!(event["data"]["outcome"], "ran");
        assert_eq!(event["data"]["reply"], summary);
        let receipt = &event["data"]["receipt"];
        let body = fs::read_to_string(receipt["path"].as_str().unwrap()).unwrap();
        let data: Value = serde_json::from_str(
            body.lines()
                .find(|line| line.starts_with('{'))
                .expect("receipt JSON line"),
        )
        .expect("receipt JSON data");
        assert_eq!(data["final_summary"], summary);
        if summary.starts_with("credential:") {
            assert!(receipt["message_id"].is_null());
            assert!(
                receipt["error"].as_str().unwrap().contains("credential"),
                "{receipt}"
            );
            let inbox = f.ok(&f.root, "orchestrator", &["msg", "inbox", "--all"]);
            assert!(inbox["data"]["messages"].as_array().unwrap().is_empty());
        } else {
            assert!(receipt["message_id"].is_string(), "{receipt}");
            assert!(receipt["error"].is_null(), "{receipt}");
            let (mailed, _) = f.receipt();
            assert_eq!(mailed["final_summary_truncated"], true);
            let preview = mailed["final_summary"].as_str().unwrap();
            assert!(summary.starts_with(preview) && preview.len() < summary.len());
            assert_eq!(mailed["final_summary_original_bytes"], summary.len());
            assert_eq!(mailed["local_receipt"], receipt["path"]);
            assert_eq!(mailed["git"], data["git"]);
            assert_eq!(mailed["activity"], data["activity"]);
        }
        assert_eq!(
            fs::read_to_string(f.home.join("agent-calls")).expect("fake harness call log"),
            "call\n"
        );
    }
}

#[test]
fn unreadable_receipt_status_is_nonfatal_and_never_rewrites_the_record() {
    let f = Fixture::new("claude", "reviewer", false);
    assert!(f.up(false).0.status.success());
    let file = f.repo.join(".git/mdkg/run/receipts/probe.json");
    fs::write(&file, b"broken status").unwrap();
    let (report, agent) = f.status();
    assert_eq!(report["ok"], true);
    assert!(agent["problem"].is_null());
    assert!(agent["receipt"].is_null());
    assert!(
        agent["receipt_problem"]
            .as_str()
            .unwrap()
            .contains("unavailable")
    );
    assert!(
        report["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["severity"] == "warning")
    );
    assert_eq!(fs::read(&file).unwrap(), b"broken status");
    assert_eq!(
        fs::read_to_string(f.home.join("agent-calls")).unwrap(),
        "call\n"
    );
}

#[test]
fn receipts_are_hidden_from_wait_but_stay_in_the_event_stream() {
    use std::io::BufRead as _;
    let f = Fixture::new("claude", "worker", false);
    let (out, _) = f.up(false);
    assert!(out.status.success());
    f.receipt();
    // `msg wait` reads like `msg inbox`: a receipt alone doesn't end it.
    let waited = f.output(
        &f.root,
        "orchestrator",
        &["msg", "wait", "--timeout", "1"],
        false,
    );
    assert_eq!(waited.status.code(), Some(6), "{waited:?}");
    let envelope: Value = serde_json::from_slice(&waited.stdout).expect("wait envelope");
    assert_eq!(envelope["diagnostics"][0]["code"], "msg.wait_timeout");
    let named = f.ok(
        &f.root,
        "orchestrator",
        &["msg", "wait", "--timeout", "1", "--kind", "receipt"],
    );
    common::conforms_envelope(&named);
    assert_eq!(named["data"]["messages"][0]["kind"], "receipt");
    // `msg watch` hides nothing: consumers filter on the event's kind.
    let mut watch = f.command(env!("CARGO_BIN_EXE_mdkg"));
    let mut child = watch
        .current_dir(&f.root)
        .arg("--root")
        .arg(&f.root)
        .args(["--as", "orchestrator", "msg", "watch", "--from-start"])
        .args(["--mailbox", "orchestrator", "--json"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("start msg watch");
    let stdout = child.stdout.take().expect("watch stdout");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut seen = false;
    while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
        let Ok(line) = rx.recv_timeout(left) else {
            break;
        };
        let event: Value = serde_json::from_str(&line).expect("event JSON");
        if event["type"] == "mdkg.message.sent" && event["data"]["message"]["kind"] == "receipt" {
            seen = true;
            break;
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    assert!(seen, "msg watch streams the receipt's send");
}

/// The orchestrator's faults, acked or not, from mdkg:supervisor.
fn faults(f: &Fixture) -> Vec<Value> {
    let inbox = f.ok(
        &f.root,
        "orchestrator",
        &["msg", "inbox", "--all", "--kind", "fault"],
    );
    common::conforms_envelope(&inbox);
    inbox["data"]["messages"]
        .as_array()
        .expect("inbox message list")
        .clone()
}

fn fault_turn(harness: &str) {
    let f = Fixture::new(harness, "worker", true);
    // Fail while $HOME/fail exists, so the same fake can succeed later.
    let script = f.dir.join(format!("{harness}.sh"));
    let text = fs::read_to_string(&script).expect("fake harness");
    fs::write(
        &script,
        text.replace("\nexit 7\n", "\n[ ! -e \"$HOME/fail\" ] || exit 7\n"),
    )
    .expect("rewrite fake harness");
    fs::write(f.home.join("fail"), "").expect("fail switch");
    let (out, _) = f.up(false);
    assert!(!out.status.success(), "{harness}: the turn fails");
    let first = faults(&f);
    assert_eq!(first.len(), 1, "{harness}: one fault to reports_to");
    let m = &first[0];
    assert_eq!(m["kind"], "fault");
    assert_eq!(m["sender"], "mdkg:supervisor");
    assert_eq!(m["to"], json!(["orchestrator"]));
    let data = &m["parts"][1]["data"];
    assert_eq!(data["schema"], "mdkg.run.fault.v1");
    let schema = common::schema("run.fault");
    let mut errors = Vec::new();
    common::api_schema::check_emitted(&schema, &schema, data, "$", &mut errors);
    assert!(errors.is_empty(), "{harness}: {data}: {errors:?}");
    assert_eq!(data["agent"], "probe");
    assert_eq!(data["code"], "run.agent_failed");
    assert_eq!(data["count"], 1);
    assert!(data["turn_id"].is_string(), "{data}");
    assert!(data["nodes"].is_array(), "{data}");
    assert!(data["worktree"]["changed"].is_u64(), "{data}");
    assert_eq!(
        m["messageId"],
        format!("fault.{}.r0", data["fault_id"].as_str().expect("fault id"))
    );
    let (receipt, _) = f.receipt();
    assert_eq!(data["turn_id"], receipt["turn_id"], "the receipt's turn");
    // A second failure, in a worktree `agents up` made and that now holds
    // an uncommitted file, supersedes the first, with count 2.
    fs::write(
        f.repo.join(".worktrees/probe/private-name.txt"),
        "uncommitted",
    )
    .expect("dirty file in the agent's worktree");
    f.ok(
        &f.root,
        "requester",
        &[
            "msg", "send", "--to", "probe", "--kind", "note", "--text", "again",
        ],
    );
    let (out, _) = f.up(false);
    assert!(!out.status.success());
    let both = faults(&f);
    assert_eq!(both.len(), 2, "{harness}");
    assert_eq!(both[0]["deliveries"][0]["ackedVia"], "supersede");
    assert_eq!(both[1]["parts"][1]["data"]["count"], 2);
    let worktree = &both[1]["parts"][1]["data"]["worktree"];
    assert!(
        worktree["changed"].as_u64().expect("changed count") >= 1,
        "{worktree}"
    );
    let text = both[1].to_string();
    assert!(!text.contains("private-name"), "no path names: {text}");
    assert_eq!(
        both[1]["parts"][1]["data"]["first_at"],
        both[0]["parts"][1]["data"]["first_at"]
    );
    // A bulk ack skips the open fault and names it (R6).
    let listed = f.ok(&f.root, "orchestrator", &["msg", "inbox"]);
    let cursor = listed["data"]["cursor"].to_string();
    let acked = f.ok(
        &f.root,
        "orchestrator",
        &["msg", "ack", "--through", &cursor],
    );
    assert!(
        acked["data"]["skipped"]
            .as_array()
            .expect("skipped list")
            .iter()
            .any(|s| s["messageId"] == both[1]["messageId"] && s["reason"] == "fault"),
        "{acked}"
    );
    // The next turn without an error clears it (R3).
    fs::remove_file(f.home.join("fail")).expect("succeed now");
    f.ok(
        &f.root,
        "requester",
        &[
            "msg", "send", "--to", "probe", "--kind", "note", "--text", "third",
        ],
    );
    let (out, events) = f.up(false);
    assert!(out.status.success(), "{harness}: {events:?}");
    let after = faults(&f);
    assert_eq!(after.len(), 2, "a good turn sends no fault");
    assert_eq!(after[1]["deliveries"][0]["state"], "acked");
    assert_eq!(after[1]["deliveries"][0]["ackedBy"], "mdkg");
    assert_eq!(after[1]["deliveries"][0]["ackedVia"], "clear");
}

#[test]
fn claude_failed_turn_is_a_fault() {
    fault_turn("claude");
}
#[test]
fn codex_failed_turn_is_a_fault() {
    fault_turn("codex");
}
#[test]
fn opencode_failed_turn_is_a_fault() {
    fault_turn("opencode");
}

#[test]
fn doctor_lists_pending_faults_per_agent() {
    let f = Fixture::new("claude", "worker", false);
    let check = |f: &Fixture| -> Value {
        let out = f.output(&f.root, "orchestrator", &["doctor"], false);
        let v: Value = serde_json::from_slice(&out.stdout).expect("doctor JSON");
        v["data"]["checks"]
            .as_array()
            .and_then(|c| c.iter().find(|c| c["name"] == "faults"))
            .cloned()
            .unwrap_or(Value::Null)
    };
    assert_eq!(check(&f)["status"], "ok");
    let run = mdkg_run::state::run_dir(&f.root).expect("run directory");
    fs::create_dir_all(&run).expect("create run directory");
    let fault = mdkg_run::fault::Fault::new(
        "probe",
        "run.agent_failed",
        "turn-1",
        vec!["orchestrator".into()],
        mdkg_run::fault::CLEARS_TURN,
        "mdkg agents status probe".into(),
    );
    mdkg_run::fault::write_pending(&run, &fault).expect("pending fault");
    let c = check(&f);
    assert_eq!(c["status"], "warning", "{c}");
    assert_eq!(c["summary"], "probe: 1 pending fault", "{c}");
}
