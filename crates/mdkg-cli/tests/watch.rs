//! API v1 conformance through the CLI transport. JSON cases use resource
//! operations and named placeholders, so another transport can reuse them.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use serde_json::{Value, json};

mod common;

#[path = "../../../conformance/api-v1/runner.rs"]
mod shared_cases;

use shared_cases::{Bindings, expect_partial, resolved};

#[cfg(unix)]
#[path = "watch/transports.rs"]
mod transports;

fn case(name: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../conformance/api-v1/cases")
        .join(format!("{name}.json"));
    serde_json::from_slice(&fs::read(path).expect("fixture value")).expect("fixture value")
}

/// Expected refusals are one-shot calls. If a regression turns one into a
/// follow, kill and reap the owned child instead of hanging the suite.
fn bounded_output(mut command: Command, timeout: Duration) -> Result<std::process::Output, String> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("fixture child");
    let mut stdout = child.stdout.take().expect("stdout");
    let mut stderr = child.stderr.take().expect("stderr");
    let out = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).expect("stdout read");
        bytes
    });
    let err = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).expect("stderr read");
        bytes
    });
    let until = std::time::Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if std::time::Instant::now() >= until => {
                break Err("one-shot fixture command timed out".to_string());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(error) => break Err(format!("fixture child wait: {error}")),
        }
    };
    if status.is_err() {
        child.kill().ok();
        child.wait().ok();
    }
    let stdout = out.join().expect("stdout reader");
    let stderr = err.join().expect("stderr reader");
    status.map(|status| std::process::Output {
        status,
        stdout,
        stderr,
    })
}

fn restore_overlap_failure(event: &Value) -> bool {
    let diagnostic = &event["data"]["diagnostics"][0];
    diagnostic["code"] == "coord.store_failed"
        && diagnostic["message"].as_str().is_some_and(|message| {
            message.starts_with("coordinator store:")
                && (message.contains("database is locked")
                    || message.contains("database table is locked"))
        })
}

struct Fixture {
    dir: PathBuf,
    repo: PathBuf,
    home: PathBuf,
    replay_pages_from_start: bool,
    reject_extensions: bool,
}
impl Fixture {
    fn new(spec: &Value) -> Self {
        let dir = common::temp::dir("mdkg-cli-watch");
        let repo = dir.join("repo");
        let home = dir.join("home");
        fs::create_dir_all(repo.join(".mdkg")).expect("fixture value");
        fs::create_dir_all(&home).expect("fixture value");
        let base =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance/base/.mdkg/config.json");
        let mut config: Value =
            serde_json::from_slice(&fs::read(base).expect("fixture value")).expect("fixture value");
        config["schema_version"] = 3.into();
        config["format_version"] = "0.7".into();
        config["graph_id"] = spec["graph_id"].clone();
        fs::write(
            repo.join(".mdkg/config.json"),
            serde_json::to_vec_pretty(&config).expect("fixture value"),
        )
        .expect("fixture value");
        for (name, contents) in spec["files"].as_object().expect("fixture object") {
            assert!(!name.starts_with('/') && !name.split('/').any(|p| p == ".."));
            let file = repo.join(name);
            fs::create_dir_all(file.parent().expect("fixture value")).expect("fixture value");
            fs::write(file, contents.as_str().expect("fixture string")).expect("fixture value");
        }
        let fixture = Self {
            dir,
            repo,
            home,
            replay_pages_from_start: spec["replay_pages_from_start"] == true,
            reject_extensions: spec["reject_extensions"] == true,
        };
        fixture.git(&["init", "-q", "-b", "main"]);
        fixture.git(&["add", "."]);
        fixture.git(&["commit", "-q", "-m", "fixture"]);
        drop(
            mdkg_coord_sqlite::Store::open_with(
                &fixture.store_path(),
                60_000,
                mdkg_coord_sqlite::Upgrade::Allow,
            )
            .expect("fixture value"),
        );
        fixture
    }
    fn isolated<'a>(&self, command: &'a mut Command) -> &'a mut Command {
        common::without_launcher_env(command)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .env_remove("GIT_INDEX_FILE")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Fixture")
            .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
            .env("GIT_COMMITTER_NAME", "Fixture")
            .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.home.join("config"))
            .env("XDG_DATA_HOME", self.home.join("data"))
            .env("CODEX_HOME", self.home.join("codex"))
            .env("MDKG_NOW_MS", "1000")
            .env("MDKG_LEASE_TTL_MS", "60000")
            .env("MDKG_WAIT_POLL_MS", "20")
    }
    fn git(&self, args: &[&str]) {
        let out = self
            .isolated(Command::new("git").arg("-C").arg(&self.repo).args(args))
            .output()
            .expect("fixture value");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    fn command(&self, actor: &str, args: &[String]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_mdkg"));
        self.isolated(&mut command)
            .arg("--root")
            .arg(&self.repo)
            .args(["--as", actor])
            .args(args)
            .arg("--json");
        command
    }
    fn cli(&self, actor: &str, args: &[String]) -> (i32, Value) {
        let out = bounded_output(self.command(actor, args), Duration::from_secs(10))
            .expect("one-shot fixture command completed");
        let value: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|_| {
            panic!(
                "not one JSON envelope: {}\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
        });
        common::conforms_envelope(&value);
        (out.status.code().unwrap_or(-1), value)
    }
    fn op(&self, operation: &Value, bindings: &mut Bindings) -> Value {
        let args = resolved(&operation["args"], bindings);
        let text = |key: &str| args[key].as_str().expect("fixture string").to_string();
        let mut words = Vec::<String>::new();
        let legacy = operation["cli"]["legacy"] == true;
        let channel_command = if legacy { "room" } else { "channel" };
        let channel_flag = if legacy { "--room" } else { "--channel" };
        let channel_kind_flag = if legacy {
            "--room-kind"
        } else {
            "--channel-kind"
        };
        match operation["op"].as_str().expect("fixture string") {
            "channel.create" | "channel.join" | "channel.leave" => {
                words.extend([
                    channel_command.into(),
                    operation["op"]
                        .as_str()
                        .expect("fixture value")
                        .split('.')
                        .nth(1)
                        .expect("fixture value")
                        .into(),
                    text("name"),
                ]);
                if let Some(members) = args["members"].as_array() {
                    for member in members {
                        words.extend([
                            "--member".into(),
                            member.as_str().expect("fixture string").into(),
                        ]);
                    }
                }
            }
            "message.send" => {
                words.extend([
                    "msg".into(),
                    "send".into(),
                    "--kind".into(),
                    text("kind"),
                    "--id".into(),
                    text("id"),
                    "--text".into(),
                    text("text"),
                ]);
                if args["channel"].is_string() {
                    words.extend([channel_flag.into(), text("channel")]);
                } else {
                    words.extend(["--to".into(), text("mailbox")]);
                }
                if args["context"].is_string() {
                    words.extend(["--context".into(), text("context")]);
                }
            }
            "channel.list" => words.extend([channel_command.into(), "list".into()]),
            "message.list" | "message.wait" => {
                words.extend([
                    "msg".into(),
                    if operation["op"] == "message.list" {
                        "inbox"
                    } else {
                        "wait"
                    }
                    .into(),
                ]);
                if operation["op"] == "message.list" {
                    words.extend(["--all".into(), "--no-mark".into()]);
                }
                if args["channel"].is_string() {
                    words.extend([channel_flag.into(), text("channel")]);
                }
                if let Some(kinds) = args["channel_kinds"].as_array() {
                    for kind in kinds {
                        words.extend([
                            channel_kind_flag.into(),
                            kind.as_str().expect("kind").into(),
                        ]);
                    }
                }
            }
            "context.read" => {
                words.extend(["msg".into(), "thread".into()]);
                if args["channel"].is_string() {
                    words.extend([channel_flag.into(), text("channel")]);
                } else {
                    words.push(text("context"));
                }
            }
            "events.watch" => words.extend([
                "msg".into(),
                "watch".into(),
                "--after".into(),
                text("after"),
            ]),
            "lease.claim" => words.extend(["claim".into(), text("scope")]),
            "lease.release" => words.extend(["release".into(), text("target")]),
            "lease.renew" => words.push("renew".into()),
            other => panic!("CLI driver doesn't implement {other}"),
        }
        let (exit, envelope) =
            self.cli(operation["actor"].as_str().expect("fixture string"), &words);
        if operation.get("cli").is_some() {
            assert_eq!(
                exit,
                operation["cli"]["exit"].as_i64().expect("CLI exit") as i32,
                "{envelope}"
            );
            expect_partial(&operation["cli"]["envelope"], &envelope, bindings);
            if operation["cli"]["deprecated"] == true {
                assert!(
                    envelope["diagnostics"]
                        .as_array()
                        .expect("diagnostics")
                        .iter()
                        .any(|d| d["code"] == "usage.deprecated")
                );
            }
            expect_partial(&operation["expect"], &resource_result(&envelope), bindings);
        } else {
            assert_eq!(
                exit,
                operation["expect"]["exit"].as_i64().expect("fixture exit") as i32,
                "{envelope}"
            );
            expect_partial(&operation["expect"]["envelope"], &envelope, bindings);
        }
        envelope
    }
    fn inbox(&self, limit: Option<u64>, after: Option<u64>) -> Value {
        let mut args = vec![
            "msg".into(),
            "inbox".into(),
            "--all".into(),
            "--no-mark".into(),
        ];
        if let Some(limit) = limit {
            args.extend(["--limit".into(), limit.to_string()]);
        }
        if let Some(after) = after {
            args.extend(["--after".into(), after.to_string()]);
        }
        let (exit, envelope) = self.cli("b", &args);
        assert_eq!(exit, 0, "{envelope}");
        assert!(envelope["data"]["cursor"].is_u64());
        assert!(envelope["data"]["event_cursor"].is_string());
        assert!(envelope["data"].get("eventCursor").is_none());
        envelope
    }
    fn store_path(&self) -> PathBuf {
        mdkg_coord_sqlite::git::store_path(&self.repo).expect("fixture value")
    }
    fn head(&self) -> String {
        mdkg_coord_sqlite::events::head(&self.store_path()).expect("fixture value")
    }
    fn records(&self) -> Vec<(u64, String, String)> {
        let connection = rusqlite::Connection::open_with_flags(
            self.store_path(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .expect("fixture value");
        let mut statement = connection
            .prepare("SELECT seq,hash,record FROM log ORDER BY seq")
            .expect("prepare fixture records");
        statement
            .query_map([], |row| {
                Ok((
                    u64::try_from(row.get::<_, i64>(0)?).expect("positive log sequence"),
                    row.get(1)?,
                    row.get(2)?,
                ))
            })
            .expect("fixture value")
            .collect::<Result<_, _>>()
            .expect("collect fixture records")
    }
    fn watch(&self, after: Option<&str>, filters: &[&str]) -> Stream {
        self.watch_as("a", after, filters)
    }
    fn watch_as(&self, actor: &str, after: Option<&str>, filters: &[&str]) -> Stream {
        let mut args = vec!["msg".into(), "watch".into()];
        if let Some(after) = after {
            args.extend(["--after".into(), after.into()]);
        }
        args.extend(filters.iter().map(|s| (*s).to_string()));
        Stream::new(self.command(actor, &args), self.reject_extensions)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.dir).ok();
    }
}

/// The live follow and injected-frame checks share this wire decoder.
fn wire_frames(
    input: impl BufRead,
    reject_extensions: bool,
) -> impl Iterator<Item = Result<Value, String>> {
    input.lines().map(move |line| {
        let line = line.map_err(|e| e.to_string())?;
        let value = serde_json::from_str(&line)
            .map_err(|e| format!("not one JSON event per line: {e}: {line}"))?;
        // Deliberately faulty decoder used by the oracle regression: a client
        // that applies the producer's declared-field check rejects extensions.
        if reject_extensions {
            let schema = api_schema("mdkg.api.event.v1");
            let mut errors = Vec::new();
            common::api_schema::check_emitted(&schema, &schema, &value, "$", &mut errors);
            if !errors.is_empty() {
                return Err(format!("closed fixture decoder: {errors:?}"));
            }
        }
        Ok(value)
    })
}

struct Stream {
    reject_extensions: bool,
    child: Child,
    lines: Receiver<Result<Value, String>>,
    reader: Option<std::thread::JoinHandle<()>>,
}
impl Stream {
    fn new(mut command: Command, reject_extensions: bool) -> Self {
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("fixture value");
        let stdout = child.stdout.take().expect("fixture value");
        let (tx, lines) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            for parsed in wire_frames(BufReader::new(stdout), reject_extensions) {
                if tx.send(parsed).is_err() {
                    break;
                }
            }
        });
        Self {
            reject_extensions,
            child,
            lines,
            reader: Some(reader),
        }
    }
    fn next(&self) -> Value {
        let value = self
            .lines
            .recv_timeout(Duration::from_secs(10))
            .expect("stream made progress")
            .expect("stream JSON");
        validate_event(&value);
        value
    }
    /// The next frame other than the head event each stream sends when it
    /// starts (and on each move).
    fn next_but_head(&self) -> Value {
        loop {
            let value = self.next();
            if value["type"] != "mdkg.graph.head_changed" {
                return value;
            }
        }
    }
    fn stop(&mut self, signal: rustix::process::Signal) -> i32 {
        let pid = rustix::process::Pid::from_raw(self.child.id() as i32).expect("fixture value");
        rustix::process::kill_process(pid, signal).expect("fixture value");
        let until = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.child.try_wait().expect("fixture value") {
                return status.code().unwrap_or(-1);
            }
            assert!(
                std::time::Instant::now() < until,
                "signal did not stop stream"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
impl Drop for Stream {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            self.child.kill().ok();
        }
        self.child.wait().ok();
        if let Some(reader) = self.reader.take() {
            reader.join().ok();
        }
    }
}

fn api_schema(name: &str) -> Value {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<BTreeMap<String, Value>>> =
        std::sync::OnceLock::new();
    let mut cache = CACHE
        .get_or_init(Default::default)
        .lock()
        .expect("schema cache");
    cache
        .entry(name.to_string())
        .or_insert_with(|| {
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../docs/api")
                .join(format!("{name}.json"));
            common::api_schema::load(&path)
        })
        .clone()
}

fn validate_event(event: &Value) {
    let schema = api_schema("mdkg.api.event.v1");
    let mut errors = Vec::new();
    common::api_schema::check_emitted(&schema, &schema, event, "$", &mut errors);
    assert!(errors.is_empty(), "event schema: {errors:?}: {event}");
    assert!(mdkg_log::record::parse_ts(event["time"].as_str().expect("fixture string")).is_some());
    if event["durable"] == true {
        assert!(mdkg_log::record::valid_hash(
            event["id"].as_str().expect("fixture string")
        ));
    }
}

fn all_events(fixture: &Fixture) -> Vec<Value> {
    let count = fixture.records().len();
    let stream = fixture.watch(Some("start"), &[]);
    (0..count).map(|_| stream.next()).collect()
}

fn store_files(path: &Path) -> BTreeMap<std::ffi::OsString, Vec<u8>> {
    fs::read_dir(path.parent().expect("store parent"))
        .expect("store directory")
        .map(|entry| entry.expect("store entry"))
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("coord.sqlite")
        })
        .map(|entry| {
            (
                entry.file_name(),
                fs::read(entry.path()).expect("store bytes"),
            )
        })
        .collect()
}

#[test]
fn watch_store_absence_preserves_legacy_artifacts() {
    for kind in ["missing", "zero", "header"] {
        let spec = case("03-list-then-watch");
        let fixture = Fixture::new(&spec["fixture"]);
        let path = fixture.store_path();
        fs::remove_file(&path).expect("remove fixture store");
        match kind {
            "zero" => {
                fs::write(&path, b"").expect("empty store");
                for suffix in ["-wal", "-shm", "-journal"] {
                    fs::write(path.with_file_name(format!("coord.sqlite{suffix}")), suffix)
                        .expect("sidecar");
                }
            }
            "header" => {
                let conn = rusqlite::Connection::open(&path).expect("empty database");
                conn.pragma_update(None, "journal_mode", "WAL")
                    .expect("header");
            }
            _ => {}
        }
        let before = store_files(&path);
        let (exit, report) = fixture.cli("a", &["msg".into(), "watch".into()]);
        assert_eq!(exit, 4, "{report}");
        let diagnostic = &report["diagnostics"][0];
        assert_eq!(diagnostic["code"], "coord.store_failed");
        let message = diagnostic["message"].as_str().expect("error message");
        assert!(message.contains("absent"), "{kind}: {message}");
        assert!(!message.contains("msg inbox"), "{message}");
        let hint = diagnostic["hint"].as_str().expect("a hint");
        assert!(hint.contains("msg inbox"), "{kind}: {hint}");
        assert!(hint.contains("--root "), "{kind}: {hint}");
        let after = store_files(&path);
        if kind == "header" {
            // Read-only SQLite can maintain WAL-index caches (contract B).
            assert_eq!(
                after.get(std::ffi::OsStr::new("coord.sqlite")),
                before.get(std::ffi::OsStr::new("coord.sqlite"))
            );
        } else {
            assert_eq!(after, before, "{kind}");
        }
    }
}

#[test]
fn read_only_commands_preserve_zero_store_sidecars() {
    for words in [
        vec!["doctor".into()],
        vec!["msg".into(), "decisions".into()],
        vec!["status".into()],
        vec!["agents".into(), "status".into()],
    ] {
        let spec = case("03-list-then-watch");
        let fixture = Fixture::new(&spec["fixture"]);
        let path = fixture.store_path();
        fs::write(&path, b"").expect("empty store");
        for suffix in ["-wal", "-shm", "-journal"] {
            fs::write(path.with_file_name(format!("coord.sqlite{suffix}")), suffix)
                .expect("sidecar");
        }
        let before = store_files(&path);
        let (exit, report) = fixture.cli("a", &words);
        assert_eq!(store_files(&path), before, "{words:?}");
        if words[0] == "doctor" {
            let store = report["data"]["checks"]
                .as_array()
                .expect("doctor checks")
                .iter()
                .find(|check| check["name"] == "store")
                .expect("store check");
            assert_eq!(store["status"], "ok", "{store}");
        } else {
            assert_eq!(exit, 0, "{report}");
        }
    }
}

#[test]
fn read_only_commands_refuse_dangling_leaf_and_parent_links() {
    use std::os::unix::fs::symlink;
    for parent in [false, true] {
        for words in [
            vec!["doctor".into()],
            vec!["msg".into(), "decisions".into()],
            vec!["msg".into(), "watch".into()],
        ] {
            let spec = case("03-list-then-watch");
            let fixture = Fixture::new(&spec["fixture"]);
            let path = fixture.store_path();
            let missing = fixture.dir.join("missing-target");
            let link = if parent {
                path.parent().expect("parent")
            } else {
                &path
            };
            if parent {
                fs::remove_dir_all(link).expect("remove parent");
            } else {
                fs::remove_file(link).expect("remove file");
            }
            symlink(&missing, link).expect("dangling link");
            let (_, report) = fixture.cli("a", &words);
            let message = if words[0] == "doctor" {
                let store = report["data"]["checks"]
                    .as_array()
                    .expect("checks")
                    .iter()
                    .find(|check| check["name"] == "store")
                    .expect("store check");
                assert_eq!(store["status"], "error", "{store}");
                store["summary"].as_str().expect("summary")
            } else {
                report["diagnostics"][0]["message"].as_str().expect("error")
            };
            assert!(message.contains("restore"), "{words:?}: {message}");
            assert!(
                fs::symlink_metadata(link)
                    .expect("link remains")
                    .is_symlink()
            );
            assert!(!missing.exists());
        }
    }
}

#[test]
fn watch_store_hot_journal_errors_advise_writer_recovery() {
    let spec = case("03-list-then-watch");
    let fixture = Fixture::new(&spec["fixture"]);
    let path = fixture.store_path();
    let head = fixture.head();
    let writer = rusqlite::Connection::open(&path).expect("writer");
    writer
        .execute_batch("PRAGMA journal_mode=DELETE; PRAGMA cache_size=1; BEGIN IMMEDIATE;")
        .expect("rollback mode");
    for seq in 0..200 {
        writer
            .execute(
                "INSERT INTO events(seq,event) VALUES (?1,?2)",
                rusqlite::params![seq, "x".repeat(4000)],
            )
            .expect("spill pages");
    }
    let journal = path.with_file_name("coord.sqlite-journal");
    let database_bytes = fs::read(&path).expect("hot database");
    let journal_bytes = fs::read(&journal).expect("hot journal");
    writer.execute_batch("ROLLBACK;").expect("rollback source");
    drop(writer);
    fs::write(&path, database_bytes).expect("restore hot database");
    fs::write(&journal, journal_bytes).expect("restore hot journal");
    let before = store_files(&path);
    let (exit, report) = fixture.cli("a", &["msg".into(), "watch".into()]);
    assert_eq!(exit, 4, "{report}");
    let diagnostic = &report["diagnostics"][0];
    assert_eq!(diagnostic["code"], "coord.store_failed");
    let message = diagnostic["message"].as_str().expect("error message");
    assert!(message.contains("journal recovery"), "{message}");
    // The store's message names no command; the hint says what to run,
    // with the --root it reads (#17360).
    assert!(!message.contains("msg inbox"), "{message}");
    let hint = diagnostic["hint"].as_str().expect("a hint");
    assert!(hint.contains("msg inbox"), "{hint}");
    assert!(hint.contains("--root "), "{hint}");
    assert!(!hint.contains("--read-only"), "{hint}");
    assert_eq!(store_files(&path), before);
    drop(
        mdkg_coord_sqlite::Store::open_with(&path, 60_000, mdkg_coord_sqlite::Upgrade::Refuse)
            .expect("writer recovers"),
    );
    assert_eq!(fixture.head(), head);
}

#[test]
fn conformance_03_list_then_watch_under_varied_concurrent_writes() {
    shared_cases::run::<Fixture>(&case("03-list-then-watch"));
}

#[test]
fn conformance_07_order_and_history_match_the_log_and_named_expectations() {
    shared_cases::run::<Fixture>(&case("07-ordering-and-history"));
}

#[test]
fn conformance_05_cursor_errors_keep_refusals_before_the_stream() {
    shared_cases::run::<Fixture>(&case("05-cursor-errors"));
}

#[test]
fn direct_and_channel_threads_return_a_cursor_that_resumes_their_snapshot() {
    let case = case("03-list-then-watch");
    let fixture = Fixture::new(&case["fixture"]);
    let mut bindings = Bindings::new();
    fixture.op(&case["operations"][0], &mut bindings);
    let channel = case["operations"][1].clone();
    fixture.op(&channel, &mut bindings);
    let mut direct = channel.clone();
    direct["args"]
        .as_object_mut()
        .expect("fixture object")
        .remove("channel");
    direct["args"]["mailbox"] = "b".into();
    direct["args"]["id"] = "direct-0".into();
    direct["args"]["context"] = "direct-context".into();
    fixture.op(&direct, &mut bindings);
    for (index, (target, mut operation)) in [
        (vec!["direct-context"], direct.clone()),
        (vec!["direct-0"], direct),
        (vec!["--channel", "dev"], channel.clone()),
        (vec!["--channel", "dev", "--member"], channel),
    ]
    .into_iter()
    .enumerate()
    {
        let words = ["msg", "thread"]
            .into_iter()
            .chain(target)
            .map(String::from)
            .collect::<Vec<_>>();
        let (exit, snapshot) = fixture.cli("b", &words);
        assert_eq!(exit, 0, "{snapshot}");
        let cursor = snapshot["data"]["event_cursor"]
            .as_str()
            .expect("thread event_cursor");
        assert!(snapshot["data"].get("eventCursor").is_none());
        assert!(snapshot["data"]["cursor"].is_u64());
        assert_eq!(cursor, fixture.head());
        operation["args"]["id"] = format!("thread-follow-{index}").into();
        fixture.op(&operation, &mut bindings);
        let stream = fixture.watch(Some(cursor), &["--types", "mdkg.message.sent"]);
        let event = stream.next();
        assert_eq!(
            event["data"]["message"]["messageId"],
            operation["args"]["id"]
        );
        assert_eq!(
            event["data"]["message"]["contextId"],
            snapshot["data"]["contextId"]
        );
    }
}

#[cfg(unix)]
#[test]
fn sigint_and_sigterm_close_a_follow_cleanly() {
    let case = case("03-list-then-watch");
    let fixture = Fixture::new(&case["fixture"]);
    for signal in [rustix::process::Signal::INT, rustix::process::Signal::TERM] {
        let mut stream = fixture.watch(Some("start"), &[]);
        stream.next();
        assert_eq!(stream.stop(signal), 0);
    }
}

#[cfg(unix)]
#[test]
fn a_second_signal_ends_a_watch_blocked_on_a_stalled_reader() {
    use std::os::unix::process::ExitStatusExt;
    let case = case("03-list-then-watch");
    let fixture = Fixture::new(&case["fixture"]);
    // About a megabyte of events: more than any pipe buffer holds.
    let text = "x".repeat(60_000);
    for n in 0..18 {
        let (code, envelope) = fixture.cli(
            "a",
            &[
                "msg".into(),
                "send".into(),
                "--to".into(),
                "b".into(),
                "--kind".into(),
                "note".into(),
                "--id".into(),
                format!("stalled-{n}"),
                "--text".into(),
                text.clone(),
            ],
        );
        assert_eq!(code, 0, "{envelope}");
    }
    let mut command = fixture.command("a", &["msg".into(), "watch".into(), "--from-start".into()]);
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("fixture value");
    // One byte read means the stream started, so its handlers are in place;
    // the reader then stalls and the watch blocks writing.
    let mut stdout = child.stdout.take().expect("fixture value");
    stdout.read_exact(&mut [0u8]).expect("stream started");
    let pid = rustix::process::Pid::from_raw(child.id() as i32).expect("fixture value");
    let term = || rustix::process::kill_process(pid, rustix::process::Signal::TERM);
    std::thread::sleep(Duration::from_millis(200));
    term().expect("fixture value");
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        child.try_wait().expect("fixture value").is_none(),
        "the first signal asks for a clean close, which waits on the blocked write"
    );
    term().expect("fixture value");
    let until = std::time::Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().expect("fixture value") {
            break status;
        }
        if std::time::Instant::now() >= until {
            child.kill().ok();
            child.wait().ok();
            panic!("a second signal did not end the blocked watch");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(
        status.signal(),
        Some(rustix::process::Signal::TERM.as_raw()),
        "{status:?}"
    );
    drop(stdout);
}

impl Fixture {
    fn git_head(&self) -> String {
        let out = self
            .isolated(
                Command::new("git")
                    .arg("-C")
                    .arg(&self.repo)
                    .args(["rev-parse", "HEAD"]),
            )
            .output()
            .expect("git");
        String::from_utf8(out.stdout)
            .expect("utf-8")
            .trim()
            .to_owned()
    }
}

fn head_event(stream: &Stream, graph: &str) -> Value {
    let event = stream
        .lines
        .recv_timeout(Duration::from_secs(60))
        .expect("a head_changed event")
        .expect("stream JSON");
    validate_event(&event);
    assert_eq!(event["type"], "mdkg.graph.head_changed", "{event}");
    assert_eq!(event["durable"], false);
    assert_eq!(event["data"]["graph"], graph);
    assert_eq!(
        event["subjects"],
        json!([format!("/v1/graphs/{graph}")]),
        "{event}"
    );
    event
}

/// The stream names the head it starts from, so a client that waits for
/// it misses no move after; then each move is announced once (Terra
/// #19918).
#[test]
fn a_watch_names_the_checkouts_head_at_its_start_and_each_move_once() {
    let case = case("03-list-then-watch");
    let fixture = Fixture::new(&case["fixture"]);
    let graph = case["fixture"]["graph_id"]
        .as_str()
        .expect("graph id")
        .to_owned();
    let start = fixture.git_head();
    let stream = fixture.watch(None, &["--types", "mdkg.graph.head_changed"]);
    assert_eq!(head_event(&stream, &graph)["data"]["head"], start.as_str());
    fixture.git(&["commit", "-q", "--allow-empty", "-m", "moved"]);
    assert_eq!(
        head_event(&stream, &graph)["data"]["head"],
        fixture.git_head().as_str()
    );
    // Nothing moved since: no other one.
    assert!(
        stream
            .lines
            .recv_timeout(Duration::from_millis(1500))
            .is_err(),
        "announced once"
    );
}

/// A checkout whose `HEAD` names no commit starts with `head` null, its
/// first commit is announced, and so is a switch back to a branch with no
/// commit (Terra #19918, #20083).
#[test]
fn a_watch_in_an_unborn_checkout_starts_null_and_announces_each_change() {
    let case = case("03-list-then-watch");
    let fixture = Fixture::new(&case["fixture"]);
    let graph = case["fixture"]["graph_id"]
        .as_str()
        .expect("graph id")
        .to_owned();
    fixture.git(&["checkout", "-q", "--orphan", "fresh"]);
    let stream = fixture.watch(None, &["--types", "mdkg.graph.head_changed"]);
    assert_eq!(
        head_event(&stream, &graph)["data"].get("head"),
        Some(&Value::Null)
    );
    fixture.git(&["commit", "-q", "-m", "first"]);
    assert_eq!(
        head_event(&stream, &graph)["data"]["head"],
        fixture.git_head().as_str()
    );
    fixture.git(&["checkout", "-q", "--orphan", "again"]);
    assert_eq!(
        head_event(&stream, &graph)["data"].get("head"),
        Some(&Value::Null)
    );
}

#[test]
fn default_watch_includes_other_actors_mail_and_filters_are_explicit() {
    let case = case("03-list-then-watch");
    let fixture = Fixture::new(&case["fixture"]);
    let mut bindings = Bindings::new();
    fixture.op(&case["operations"][0], &mut bindings);
    let baseline = fixture.head();
    let mut direct = case["operations"][1].clone();
    direct["args"]
        .as_object_mut()
        .expect("fixture value")
        .remove("channel");
    direct["args"]["mailbox"] = "c".into();
    direct["args"]["id"] = "other-mailbox-message".into();
    fixture.op(&direct, &mut bindings);
    let selected = case["operations"][2].clone();
    fixture.op(&selected, &mut bindings);
    let all = fixture.watch_as(
        "outsider",
        Some(&baseline),
        &["--types", "mdkg.message.sent"],
    );
    let other = all.next();
    assert_eq!(other["data"]["message"]["to"], json!(["c"]));
    assert_eq!(
        other["data"]["message"]["parts"][0]["text"],
        direct["args"]["text"]
    );
    assert_eq!(
        all.next()["data"]["message"]["messageId"],
        selected["args"]["id"]
    );
    let mailbox = fixture.watch_as(
        "outsider",
        Some(&baseline),
        &["--types", "mdkg.message.*", "--mailbox", "b"],
    );
    let channel = fixture.watch_as(
        "outsider",
        Some(&baseline),
        &["--types", "mdkg.message.*", "--channel", "dev"],
    );
    assert_eq!(
        mailbox.next()["data"]["message"]["messageId"],
        selected["args"]["id"]
    );
    assert_eq!(
        channel.next()["data"]["message"]["messageId"],
        selected["args"]["id"]
    );
    let marker = case["operations"][3].clone();
    fixture.op(&marker, &mut bindings);
    assert_eq!(
        mailbox.next()["data"]["message"]["messageId"],
        marker["args"]["id"]
    );
    assert_eq!(
        channel.next()["data"]["message"]["messageId"],
        marker["args"]["id"]
    );
}

#[test]
fn midstream_store_error_is_a_final_bare_event() {
    let case = case("03-list-then-watch");
    let fixture = Fixture::new(&case["fixture"]);
    let mut stream = fixture.watch(Some("start"), &[]);
    let first = stream.next();
    let head = fixture.head();
    let connection = rusqlite::Connection::open(fixture.store_path()).expect("fixture value");
    connection
        .execute(
            "UPDATE meta SET value='damaged-head' WHERE key='log_head'",
            [],
        )
        .expect("fixture value");
    drop(connection);
    let final_event = stream.next_but_head();
    assert_eq!(final_event["type"], "mdkg.stream.error");
    assert!(
        !restore_overlap_failure(&final_event),
        "corruption is not restore contention"
    );
    assert_eq!(final_event["durable"], false);
    assert_eq!(final_event["cursor"], first["cursor"]);
    assert_eq!(final_event["data"]["head"], head);
    assert_eq!(
        final_event["data"]["diagnostics"][0]["code"],
        "coord.store_failed"
    );
    let until = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = stream.child.try_wait().expect("fixture value") {
            assert_eq!(status.code(), Some(4));
            break;
        }
        assert!(std::time::Instant::now() < until);
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_restore_can_proceed_while_watching_and_ends_with_a_cursor_reset_frame() {
    let case = case("03-list-then-watch");
    let fixture = Fixture::new(&case["fixture"]);
    let backup =
        mdkg_coord_sqlite::snapshot(&fixture.store_path(), "same-history").expect("fixture value");
    let mut stream = fixture.watch(Some("start"), &[]);
    let delivered = stream.next();
    let delivered_cursor = delivered["cursor"]
        .as_str()
        .expect("delivered cursor")
        .to_string();
    assert!(stream.child.try_wait().expect("watch status").is_none());
    // A short poll connection may make restore refuse before changing
    // anything. Retry only that specific transient refusal in this fixture.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match mdkg_coord_sqlite::restore(&fixture.store_path(), &backup) {
            Ok(_) => break,
            Err(mdkg_coord_sqlite::StoreError::Restore(why))
                if why.contains("another process may have")
                    && why.contains("database is locked")
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("fixture restore: {error}"),
        }
    }
    let event = stream.next_but_head();
    assert_eq!(event["type"], "mdkg.stream.error");
    let code = event["data"]["diagnostics"][0]["code"]
        .as_str()
        .expect("terminal diagnostic");
    assert!(
        code == "api.cursor_reset" || restore_overlap_failure(&event),
        "unrelated stream failure: {event}"
    );
    assert_eq!(event["cursor"], delivered_cursor);
    assert!(event["data"]["head"].is_string());
    if code == "api.cursor_reset" {
        assert_eq!(event["data"]["head"], fixture.head());
    }
    let (status, refusal) = fixture.cli(
        "a",
        &[
            "msg".into(),
            "watch".into(),
            "--after".into(),
            delivered_cursor,
        ],
    );
    assert_eq!(status, 5);
    assert_eq!(refusal["diagnostics"][0]["code"], "api.cursor_reset");
    assert_eq!(refusal["data"]["head"], fixture.head());
    let expected_exit = if code == "api.cursor_reset" { 5 } else { 4 };
    let until = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = stream.child.try_wait().expect("fixture value") {
            assert_eq!(status.code(), Some(expected_exit));
            break;
        }
        assert!(std::time::Instant::now() < until);
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn event_payload_schemas_check_required_types_fields_and_open_values() {
    let case = case("07-ordering-and-history");
    let fixture = Fixture::new(&case["fixture"]);
    let mut bindings = Bindings::new();
    for operation in case["operations"].as_array().expect("operations") {
        fixture.op(operation, &mut bindings);
    }
    let events = all_events(&fixture);
    let schema = api_schema("mdkg.api.event.v1");
    let message = events
        .iter()
        .find(|event| event["type"] == "mdkg.message.sent")
        .expect("message event");
    let lease = events
        .iter()
        .find(|event| event["type"] == "mdkg.lease.claimed")
        .expect("lease event");
    let mut missing = message.clone();
    missing["data"]["message"]
        .as_object_mut()
        .expect("message object")
        .remove("retention");
    let mut wrong_type = message.clone();
    wrong_type["data"]["message"]["retention"] = 17.into();
    let mut undeclared = message.clone();
    undeclared["data"]["message"]["undocumented_producer_field"] = true.into();
    let mut closed_status = lease.clone();
    closed_status["data"]["lease"]["status"] = "future_status".into();
    for invalid in [missing, wrong_type, undeclared, closed_status] {
        let mut errors = Vec::new();
        common::api_schema::check_emitted(&schema, &schema, &invalid, "$", &mut errors);
        assert!(!errors.is_empty(), "invalid payload passed: {invalid}");
    }
    let mut open = message.clone();
    open["data"]["message"]["kind"] = "future_kind".into();
    open["data"]["message"]["body"] = "future_body_state".into();
    open["data"]["message"]["deliveries"][0]["state"] = "future_delivery_state".into();
    let mut errors = Vec::new();
    common::api_schema::check_emitted(&schema, &schema, &open, "$", &mut errors);
    assert!(errors.is_empty(), "open sets rejected: {errors:?}");
}

/// Resource records are the shared assertions. Frozen envelope spellings and
/// deprecated CLI fields are checked before this adapter normalizes them.
fn resource_result(envelope: &Value) -> Value {
    let mut value = envelope.clone();
    if value.get("data").is_none() {
        value["data"] = Value::Null;
    }
    if let Some(data) = value["data"].as_object_mut() {
        if let Some(channel) = data.remove("room") {
            data.insert("channel".into(), channel);
        }
        if let Some(channels) = data.remove("rooms") {
            data.insert("channels".into(), channels);
        }
        if let Some(message) = data.get_mut("message").and_then(Value::as_object_mut) {
            message.remove("room");
        }
        if let Some(messages) = data.get_mut("messages").and_then(Value::as_array_mut) {
            for message in messages {
                message.as_object_mut().expect("message").remove("room");
            }
        }
    }
    if let Some(diagnostics) = value["diagnostics"].as_array_mut() {
        diagnostics.retain(|d| d["code"] != "usage.deprecated");
        for diagnostic in diagnostics {
            if let Some(code) = diagnostic["code"]
                .as_str()
                .and_then(|s| s.strip_prefix("room."))
            {
                diagnostic["code"] = format!("channel.{code}").into();
            }
        }
    }
    for (one, many, name) in [
        ("message", "messages", "mdkg.api.message.v1"),
        ("channel", "channels", "mdkg.api.channel.v1"),
    ] {
        let schema = api_schema(name);
        let records = value["data"]
            .get(one)
            .into_iter()
            .chain(value["data"][many].as_array().into_iter().flatten());
        for record in records {
            let mut errors = Vec::new();
            common::api_schema::check_emitted(
                &schema,
                &schema["properties"]["data"]["anyOf"][0],
                record,
                "$resource",
                &mut errors,
            );
            assert!(errors.is_empty(), "{name}: {errors:?}");
        }
    }
    value
}
impl shared_cases::Events for Stream {
    fn next_emitted(&self) -> Value {
        Stream::next(self)
    }
    fn decode_injected(&self, frames: &[Value]) -> Vec<Value> {
        let mut bytes = Vec::new();
        for frame in frames {
            serde_json::to_writer(&mut bytes, frame).expect("fixture frame");
            bytes.push(b'\n');
        }
        wire_frames(BufReader::new(bytes.as_slice()), self.reject_extensions)
            .map(|frame| frame.expect("injected frame decoded"))
            .collect()
    }
}
impl shared_cases::Driver for Fixture {
    type Stream = Stream;
    fn new(spec: &Value) -> Self {
        Fixture::new(spec)
    }
    fn invoke(&self, operation: &Value) -> Value {
        resource_result(&self.op(operation, &mut Bindings::new()))
    }
    fn follow(&self, after: Option<&str>, filters: &Value) -> Self::Stream {
        let mut words = Vec::<String>::new();
        for (key, flag) in [("types", "--types"), ("channels", "--channel")] {
            if let Some(values) = filters[key].as_array() {
                for value in values {
                    words.extend([flag.into(), value.as_str().expect("filter").into()]);
                }
            }
        }
        if let Some(mailbox) = filters["mailbox"].as_str() {
            words.extend(["--mailbox".into(), mailbox.into()]);
        }
        let words: Vec<_> = words.iter().map(String::as_str).collect();
        let after = if self.replay_pages_from_start && after.is_some_and(|value| value != "start") {
            Some("start")
        } else {
            after
        };
        self.watch(after, &words)
    }
    fn inbox(&self, limit: Option<u64>, page: Option<&Value>) -> Value {
        let mut value = resource_result(&Fixture::inbox(
            self,
            limit,
            page.map(|p| p.as_u64().expect("CLI numeric page")),
        ));
        value["data"]["next"] = if value["data"]["more"] == true {
            value["data"]["cursor"].clone()
        } else {
            Value::Null
        };
        value
    }
    fn supports_aliases() -> bool {
        true
    }
    fn record_count(&self) -> usize {
        self.records().len()
    }
    fn records(&self) -> Vec<(u64, String, String)> {
        Fixture::records(self)
    }
    fn store_path(&self) -> PathBuf {
        Fixture::store_path(self)
    }
    fn head(&self) -> String {
        Fixture::head(self)
    }
    fn import_fixture(&self, history: &Value) {
        shared_cases::seed_import_history(&self.store_path(), history);
    }
}
/// Case 22: the same operations over the CLI, MCP and serve-local's
/// socket give the same envelope, each with its own transport's outcome.
#[cfg(unix)]
#[test]
fn an_operation_gives_the_same_envelope_over_the_cli_mcp_and_the_socket() {
    let case = case("22-same-envelope");
    let cli = transports::CliLocal(Fixture::new(&case["fixture"]));
    let mcp = transports::Mcp(Fixture::new(&case["fixture"]));
    let http = transports::HttpSocket::new(&case["fixture"]);
    shared_cases::same_envelope(&case, &[&cli, &mcp, &http]);
}

/// The envelope oracle's mutants: a transport whose envelope differs from
/// the first's, that gives another transport's outcome, or that comes out
/// of the case's order fails it; so does a per-copy value left unmasked.
#[test]
fn same_envelope_fails_a_transport_that_answers_differently() {
    /// Answers with each operation's expectation and `outcome_of`'s
    /// outcome; its inbox cursor is its own, as each store's is.
    struct Canned {
        name: &'static str,
        outcome_of: &'static str,
        altered: Option<&'static str>,
        case: Value,
    }
    impl shared_cases::Transport for Canned {
        fn name(&self) -> &'static str {
            self.name
        }
        fn call(&self, op: &str, actor: &str, _: &Value) -> (Value, Value) {
            let operation = self.case["operations"]
                .as_array()
                .expect("operations")
                .iter()
                .find(|o| o["op"] == op && o["actor"] == actor)
                .expect("operation");
            let mut envelope = operation["expect"].clone();
            if op == "msg.inbox" {
                envelope["data"] = json!({"event_cursor": self.name});
            }
            if self.altered == Some(op) {
                envelope["extra"] = true.into();
            }
            (envelope, operation["outcomes"][self.outcome_of].clone())
        }
    }
    let passes = |case: &Value, spec: [(&'static str, &'static str, Option<&'static str>); 3]| {
        let transports = spec.map(|(name, outcome_of, altered)| Canned {
            name,
            outcome_of,
            altered,
            case: case.clone(),
        });
        let refs: Vec<&dyn shared_cases::Transport> = transports
            .iter()
            .map(|t| t as &dyn shared_cases::Transport)
            .collect();
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            shared_cases::same_envelope(case, &refs)
        }))
        .is_ok()
    };
    let case = case("22-same-envelope");
    let (cli, mcp, http) = ("cli-local", "mcp", "http-socket");
    assert!(passes(
        &case,
        [(cli, cli, None), (mcp, mcp, None), (http, http, None)]
    ));
    let added = [
        (cli, cli, None),
        (mcp, mcp, None),
        (http, http, Some("msg.send")),
    ];
    assert!(!passes(&case, added), "a field added over HTTP");
    let outcome = [(cli, cli, None), (mcp, cli, None), (http, http, None)];
    assert!(!passes(&case, outcome), "MCP giving the CLI's exit code");
    let order = [(cli, cli, None), (http, http, None), (mcp, mcp, None)];
    assert!(!passes(&case, order), "out of the case's order");
    let mut strict = case.clone();
    for operation in strict["operations"].as_array_mut().expect("operations") {
        operation
            .as_object_mut()
            .expect("operation")
            .remove("per_copy");
    }
    let same = [(cli, cli, None), (mcp, mcp, None), (http, http, None)];
    assert!(!passes(&strict, same), "each copy's own cursor, compared");
}

#[test]
fn shared_cases_cover_resume_keys_aliases_forward_compatibility_pages_paths_and_imports() {
    for name in [
        "04-resume",
        "06-idempotency",
        "08-channel-aliases",
        "13-forward-compatibility",
        "17-pages",
        "18-canonical-paths",
        "20-imported-history",
    ] {
        shared_cases::run::<Fixture>(&case(name));
    }
}
#[test]
fn shared_case_reserves_mdkgs_kinds_and_fault_ids() {
    shared_cases::run::<Fixture>(&case("21-reserved-kinds"));
}
#[test]
fn stream_filters_preserve_positions() {
    let case = case("03-list-then-watch");
    let fixture = Fixture::new(&case["fixture"]);
    let mut bindings = Bindings::new();
    fixture.op(&case["operations"][0], &mut bindings);
    fixture.op(&case["operations"][1], &mut bindings);
    let cursor = fixture.head();
    fixture.op(&case["operations"][2], &mut bindings);
    let selected = fixture.watch(
        Some(&cursor),
        &[
            "--types",
            "mdkg.message.*",
            "--channel",
            "dev",
            "--mailbox",
            "b",
        ],
    );
    let all = fixture.watch(Some(&cursor), &[]);
    assert_eq!(selected.next(), all.next());
}

#[test]
fn a_one_shot_probe_kills_a_follow_that_does_not_close() {
    let case = case("03-list-then-watch");
    let fixture = Fixture::new(&case["fixture"]);
    let command = fixture.command("a", &["msg".into(), "watch".into(), "--from-start".into()]);
    assert_eq!(
        bounded_output(command, Duration::from_millis(100)).unwrap_err(),
        "one-shot fixture command timed out"
    );
}

#[test]
fn stored_legacy_and_future_parts_replay_unchanged() {
    use mdkg_coord_sqlite::mail::{Outgoing, Retention, To};
    let case = case("03-list-then-watch");
    let fixture = Fixture::new(&case["fixture"]);
    let bodies = vec![
        json!([
            {"kind":"text","text":"known part"}, {"text":"legacy"},
            {"kind":"text","text":7}, {"kind":null,"payload":"legacy"},
            {"kind":"future_part","payload":{"state":"future_state"}},
            "legacy scalar", 17, 3.5, true, null, ["legacy array"]
        ]),
        json!("stored scalar body"),
        json!({"legacy":"object body"}),
        json!(17),
        json!(true),
        Value::Null,
    ];
    let mut store = mdkg_coord_sqlite::Store::open_with(
        &fixture.store_path(),
        60_000,
        mdkg_coord_sqlite::Upgrade::Refuse,
    )
    .expect("fixture store");
    for (index, parts) in bodies.iter().enumerate() {
        store
            .send(&Outgoing {
                message_id: format!("stored-parts-{index}"),
                context_id: None,
                kind: "note".into(),
                sender: "a".into(),
                via: None,
                sender_mailboxes: vec![],
                to: To::Mailboxes(vec!["b".into()]),
                reference_ids: vec![],
                parts: parts.clone(),
                retention: Retention::Durable,
                human_hold_days: 1,
                now_ms: 1000,
            })
            .expect("stored legacy body");
    }
    drop(store);
    let messages: Vec<_> = all_events(&fixture)
        .into_iter()
        .filter(|event| event["type"] == "mdkg.message.sent")
        .collect();
    assert_eq!(messages.len(), bodies.len());
    for (event, parts) in messages.iter().zip(&bodies) {
        assert_eq!(&event["data"]["message"]["parts"], parts);
    }
}

#[test]
fn api_audit_metadata_extensions_never_relax_authorization_decoding() {
    let schema = api_schema("mdkg.api.message.v1");
    let mut provenance = json!({
        "sender":"a","human":"human:fixture","sourceFile":"/fixture/delegations.toml",
        "action":"implement","nodeRef":"fixture:task-example","nodeCommit":"fixture-commit",
        "nodePath":".mdkg/work/task-example.md","nodeDigest":"fixture-digest",
        "graphConfigPath":".mdkg/config.json","messageId":"fixture-decision",
        "outsideAgentTurn":true,"sentAtMs":1000,"bodyDigest":"fixture-body-digest"
    });
    assert!(serde_json::from_value::<mdkg_run::approval::Provenance>(provenance.clone()).is_ok());
    provenance["future_audit_field"] = json!({"state":"future_state"});
    let mut errors = Vec::new();
    common::api_schema::check(
        &schema,
        &schema["properties"]["data"]["anyOf"][0]["properties"]["decision"]["anyOf"][0],
        &provenance,
        "$",
        &mut errors,
    );
    assert!(
        errors.is_empty(),
        "API audit extension rejected: {errors:?}"
    );
    let mut producer_errors = Vec::new();
    common::api_schema::check_emitted(
        &schema,
        &schema["properties"]["data"]["anyOf"][0]["properties"]["decision"]["anyOf"][0],
        &provenance,
        "$",
        &mut producer_errors,
    );
    assert!(
        !producer_errors.is_empty(),
        "undeclared audit fields must fail producer conformance"
    );
    assert!(
        serde_json::from_value::<mdkg_run::approval::Provenance>(provenance).is_err(),
        "audit extensions must not weaken the launcher's strict authorization input"
    );
}

#[test]
fn paging_oracle_rejects_stale_streamed_messages() {
    let mut case = case("17-pages");
    case["fixture"]["replay_pages_from_start"] = true.into();
    expect_oracle_failure(
        || shared_cases::run::<Fixture>(&case),
        "post-snapshot event identity, order and payload",
    );
}

#[test]
fn forward_oracle_rejects_a_decoder_that_closes_future_fields() {
    let mut case = case("13-forward-compatibility");
    case["fixture"]["reject_extensions"] = true.into();
    expect_oracle_failure(
        || shared_cases::run::<Fixture>(&case),
        "closed fixture decoder",
    );
}

fn expect_oracle_failure(run: impl FnOnce() + std::panic::UnwindSafe, expected: &str) {
    let failure = std::panic::catch_unwind(run).expect_err("faulty transport passed the oracle");
    let message = failure
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| failure.downcast_ref::<&str>().copied())
        .expect("panic message");
    assert!(
        message.contains(expected),
        "unexpected oracle failure: {message}"
    );
}

#[test]
fn forward_oracle_handles_a_type_matching_its_old_injected_name() {
    use shared_cases::Events;
    struct FutureStream<'a> {
        live: &'a Stream,
        event: Value,
    }
    impl Events for FutureStream<'_> {
        fn next_emitted(&self) -> Value {
            self.event.clone()
        }
        fn decode_injected(&self, frames: &[Value]) -> Vec<Value> {
            self.live.decode_injected(frames)
        }
    }
    let case = case("03-list-then-watch");
    let fixture = Fixture::new(&case["fixture"]);
    let live = fixture.watch(Some("start"), &[]);
    let original = live.next();
    for kind in ["mdkg.future.changed", "mdkg.future.extended"] {
        let mut event = original.clone();
        event["type"] = kind.into();
        validate_event(&event);
        assert_eq!(FutureStream { live: &live, event }.next()["type"], kind);
    }
}

#[test]
fn known_cli_write_parts_keep_their_validated_shapes() {
    let case = case("03-list-then-watch");
    let fixture = Fixture::new(&case["fixture"]);
    let data = fixture.dir.join("part.json");
    fs::write(&data, br#"{"value":1}"#).expect("fixture data part");
    let (exit, response) = fixture.cli(
        "a",
        &[
            "msg".into(),
            "send".into(),
            "--to".into(),
            "b".into(),
            "--kind".into(),
            "note".into(),
            "--id".into(),
            "known-shapes".into(),
            "--text".into(),
            "known text".into(),
            "--data-file".into(),
            data.display().to_string(),
            "--url".into(),
            "https://example.invalid/part".into(),
        ],
    );
    assert_eq!(exit, 0, "{response}");
    // Fixture::cli checks the unchanged, strict frozen CLI part schema.
    let kinds: Vec<_> = response["data"]["message"]["parts"]
        .as_array()
        .expect("parts")
        .iter()
        .map(|part| part["kind"].as_str().expect("known kind"))
        .collect();
    assert_eq!(kinds, ["text", "data", "url"]);
}
