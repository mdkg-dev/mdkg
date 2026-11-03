//! `mdkg serve` (serve-local) as a process: the lock and a restart after
//! SIGKILL (SC3), the stop (R5's exit), the local socket's modes (R9), a
//! failed start that binds nothing (R4), and `serve status` across start
//! and stop.
//!
//! Each test has its own repository, home and state folders, so nothing
//! reaches the operator's. serve-local needs no store setup: it opens no
//! store, and asks for no lease mode.

#![cfg(unix)]

mod common;

use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use mdkg_wire::client_conn::ClientConnection;
use mdkg_wire::sign::Outgoing;
use serde_json::{Value, json};

fn isolated<'a>(cmd: &'a mut Command, home: &Path) -> &'a mut Command {
    common::without_launcher_env(cmd)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("CODEX_HOME", home.join(".codex"))
        .env("TMPDIR", home)
        .env_remove("XDG_RUNTIME_DIR")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
}

fn copy_tree(src: &Path, dest: &Path) {
    for entry in std::fs::read_dir(src).expect("read dir") {
        let entry = entry.expect("entry");
        let target = dest.join(entry.file_name());
        if entry.file_type().expect("type").is_dir() {
            std::fs::create_dir_all(&target).expect("mkdir");
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copy");
        }
    }
}

struct Fixture {
    dir: PathBuf,
    repo: PathBuf,
    home: PathBuf,
    /// The `--root` every command gets: the repository, or a linked
    /// worktree of it.
    root: PathBuf,
}

impl Fixture {
    /// A committed format 0.7 repository whose store is served.
    fn new() -> Self {
        // Short, so the socket path fits: macOS's temp folder is long.
        let dir = PathBuf::from("/tmp").join(format!(
            "mdkg-srvt-{}",
            &uuid::Uuid::now_v7().simple().to_string()[20..]
        ));
        std::fs::create_dir_all(&dir).expect("dir");
        let dir = std::fs::canonicalize(dir).expect("canonical");
        let (repo, home) = (dir.join("repo"), dir.join("home"));
        std::fs::create_dir_all(&repo).expect("repo");
        std::fs::create_dir_all(&home).expect("home");
        copy_tree(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance/base"),
            &repo,
        );
        let path = repo.join(".mdkg/config.json");
        let mut config: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("config")).expect("json");
        config["schema_version"] = 3.into();
        config["format_version"] = "0.7".into();
        config["graph_id"] = mdkg_format::Uuid7::now().to_string().into();
        config["workspaces"] = json!({"root": {"path": ".", "enabled": true, "mdkg_dir": ".mdkg"}});
        std::fs::write(&path, config.to_string()).expect("write config");
        let f = Self::bare(dir, repo, home);
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["add", "."],
            &["commit", "-q", "-m", "init"],
        ] {
            let out = isolated(
                Command::new("git").arg("-C").arg(&f.repo).args(args),
                &f.home,
            )
            .output()
            .expect("git");
            assert!(out.status.success(), "git {args:?}");
        }
        f
    }

    fn bare(dir: PathBuf, repo: PathBuf, home: PathBuf) -> Self {
        let root = repo.clone();
        Self {
            dir,
            repo,
            home,
            root,
        }
    }

    /// A repository in graph-ref mode: the graph is committed on
    /// `refs/heads/mdkg-graph`, and the checkout has no `.mdkg`.
    fn ref_mode() -> Self {
        let dir = PathBuf::from("/tmp").join(format!(
            "mdkg-srvr-{}",
            &uuid::Uuid::now_v7().simple().to_string()[20..]
        ));
        std::fs::create_dir_all(&dir).expect("dir");
        let dir = std::fs::canonicalize(dir).expect("canonical");
        let (repo, home) = (dir.join("repo"), dir.join("home"));
        std::fs::create_dir_all(&repo).expect("repo");
        std::fs::create_dir_all(&home).expect("home");
        std::fs::write(repo.join("a.txt"), "a\n").expect("file");
        let f = Self::bare(dir, repo, home);
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["add", "."],
            &["commit", "-q", "-m", "init"],
            &["config", "mdkg.graph.ref", "refs/heads/mdkg-graph"],
            // Graph commits read git's config for their identity.
            &["config", "user.name", "Test"],
            &["config", "user.email", "test@example.invalid"],
        ] {
            f.git(args);
        }
        let out = f
            .mdkg()
            .arg("init")
            .stdin(Stdio::null())
            .output()
            .expect("init");
        assert!(
            out.status.success(),
            "init: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(!f.repo.join(".mdkg").exists(), "ref mode writes no .mdkg");
        f
    }

    fn git(&self, args: &[&str]) -> String {
        let out = isolated(
            Command::new("git").arg("-C").arg(&self.repo).args(args),
            &self.home,
        )
        .output()
        .expect("git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn common(&self) -> PathBuf {
        self.repo.join(".git")
    }

    fn mdkg(&self) -> Command {
        self.mdkg_as("serve-test")
    }

    /// The CLI, as `actor`.
    fn mdkg_as(&self, actor: &str) -> Command {
        self.program_as(Path::new(env!("CARGO_BIN_EXE_mdkg")), actor)
    }

    /// `program` (mdkg, or a copy of it), as `actor`.
    fn program_as(&self, program: &Path, actor: &str) -> Command {
        let mut cmd = Command::new(program);
        isolated(&mut cmd, &self.home)
            .arg("--root")
            .arg(&self.root)
            .args(["--as", actor]);
        cmd
    }

    /// Runs a command to its end: its exit code and envelope.
    fn run(&self, args: &[&str]) -> (i32, Value) {
        let out = self.mdkg().args(args).arg("--json").output().expect("mdkg");
        let v = serde_json::from_slice(&out.stdout).unwrap_or_else(|_| {
            panic!(
                "{args:?}: not JSON: {}\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
        });
        (out.status.code().unwrap_or(-1), v)
    }

    /// Starts `serve` and reads its start envelope.
    fn serve(&self, extra: &[&str]) -> Serving {
        self.serve_from(Path::new(env!("CARGO_BIN_EXE_mdkg")), extra)
    }

    /// Starts `serve` from `program`.
    fn serve_from(&self, program: &Path, extra: &[&str]) -> Serving {
        // serve's JSON log lines go to a file, never an unread pipe that a
        // long run could fill (#11581 item 3).
        let log = self
            .dir
            .join(format!("serve-{}.log", uuid::Uuid::now_v7().simple()));
        let stderr = std::fs::File::create(&log).expect("log file");
        let mut child = self
            .program_as(program, "serve-test")
            .args(["serve", "--json"])
            .args(extra)
            .stdout(Stdio::piped())
            .stderr(stderr)
            .spawn()
            .expect("serve");
        let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));
        let mut text = String::new();
        loop {
            let mut line = String::new();
            let n = stdout.read_line(&mut line).expect("read");
            text.push_str(&line);
            if n == 0 || line == "}\n" {
                break;
            }
        }
        let envelope: Value = serde_json::from_str(&text).unwrap_or_else(|_| {
            let err = std::fs::read_to_string(&log).unwrap_or_default();
            panic!("serve's envelope isn't JSON: {text}\n{err}")
        });
        Serving {
            child,
            envelope,
            _stdout: stdout,
        }
    }

    fn status(&self) -> Value {
        let (code, v) = self.run(&["serve", "status"]);
        assert_eq!(code, 0, "{v}");
        v
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct Serving {
    child: Child,
    envelope: Value,
    _stdout: BufReader<std::process::ChildStdout>,
}

impl Serving {
    fn data(&self) -> &Value {
        &self.envelope["data"]
    }

    fn socket(&self) -> PathBuf {
        PathBuf::from(self.data()["socket"].as_str().expect("socket"))
    }

    fn signal(&self, sig: &str) {
        let out = Command::new("kill")
            .args([sig, &self.child.id().to_string()])
            .output()
            .expect("kill");
        assert!(out.status.success(), "kill {sig}");
    }

    /// The exit code, waiting at most `limit`.
    fn wait(mut self, limit: Duration) -> Option<i32> {
        let until = Instant::now() + limit;
        loop {
            if let Some(status) = self.child.try_wait().expect("wait") {
                return Some(status.code().unwrap_or(-1));
            }
            if Instant::now() >= until {
                let _ = self.child.kill();
                let _ = self.child.wait();
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn kill9(mut self) {
        self.child.kill().expect("SIGKILL");
        self.child.wait().expect("reaped");
    }
}

impl Drop for Serving {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn mode(p: &Path) -> u32 {
    std::fs::metadata(p).expect("meta").permissions().mode() & 0o7777
}

fn code(v: &Value) -> &str {
    v["diagnostics"][0]["code"].as_str().unwrap_or_default()
}

/// `GET /` over `io`, unsigned: the public fields.
fn get_root(io: Box<dyn mdkg_wire::codec::Io>) -> Value {
    let mut c = ClientConnection::new(io);
    c.send(&Outgoing::new("GET", "/", None), Duration::from_secs(10))
        .expect("send");
    let head = c.read_head(Duration::from_secs(10)).expect("head");
    assert_eq!(head.status, 200);
    let body = c
        .read_body(&head, 1 << 16, Duration::from_secs(10))
        .expect("body");
    serde_json::from_slice(&body).expect("json")
}

#[test]
fn sc3_a_second_serve_is_refused_and_a_restart_after_sigkill_starts() {
    let f = Fixture::new();
    assert_eq!(f.status()["data"]["state"], "absent");
    let first = f.serve(&[]);
    assert_eq!(first.envelope["ok"], true, "{}", first.envelope);
    let socket = first.socket();

    // R9: the socket is 0600, in a 0700 folder.
    assert_eq!(mode(&socket), 0o600);
    assert_eq!(mode(socket.parent().expect("folder")), 0o700);
    let root = get_root(Box::new(
        std::os::unix::net::UnixStream::connect(&socket).expect("connect"),
    ));
    assert_eq!(root["data"]["serve_id"], first.data()["serve_id"]);

    let status = f.status();
    assert_eq!(status["data"]["state"], "running");
    assert_eq!(
        status["data"]["serve"]["serve_id"],
        first.data()["serve_id"]
    );
    assert_eq!(
        status["data"]["serve"]["pid"],
        json!(first.child.id()),
        "serve.json names the process"
    );

    // A second serve is refused, and binds nothing (R3).
    let (exit, v) = f.run(&["serve"]);
    assert_eq!(exit, 5, "{v}");
    assert_eq!(code(&v), "api.serve_running");
    assert_eq!(
        f.status()["data"]["serve"]["serve_id"],
        first.data()["serve_id"]
    );

    // SIGKILL: the lock goes with the process; serve.json is left, stale,
    // and the next start replaces it and the socket.
    let old_id = first.data()["serve_id"].clone();
    first.kill9();
    assert_eq!(f.status()["data"]["state"], "stale");
    let second = f.serve(&[]);
    assert_eq!(second.envelope["ok"], true, "{}", second.envelope);
    assert_ne!(second.data()["serve_id"], old_id);
    assert_eq!(f.status()["data"]["state"], "running");

    // SIGTERM: a clean stop exits 0 and removes the socket and serve.json.
    second.signal("-TERM");
    assert_eq!(second.wait(Duration::from_secs(30)), Some(0));
    assert!(!socket.exists());
    assert_eq!(f.status()["data"]["state"], "absent");
}

#[test]
fn serve_status_fails_as_serve_status_outside_a_repository() {
    // #11581 item 1, #11680: an early failure answers under the status
    // schema, as its success does.
    let f = Fixture::new();
    let outside = f.dir.join("not-a-repo");
    std::fs::create_dir_all(&outside).expect("dir");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mdkg"));
    let out = isolated(&mut cmd, &f.home)
        .arg("--root")
        .arg(&outside)
        .args(["serve", "status", "--json"])
        .output()
        .expect("mdkg");
    assert_ne!(out.status.code(), Some(0));
    let v: Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(v["schema"], "mdkg.serve.status.v1", "{v}");
    assert_eq!(v["ok"], false);
}

#[test]
fn sigint_drains_as_sigterm_does() {
    let f = Fixture::new();
    let s = f.serve(&[]);
    let socket = s.socket();
    s.signal("-INT");
    assert_eq!(s.wait(Duration::from_secs(30)), Some(0));
    assert!(!socket.exists());
}

#[test]
fn r4_a_failed_start_releases_the_lock_and_binds_nothing() {
    let f = Fixture::new();
    // A settings error: refused before the lock, nothing bound.
    let user = f.home.join(".config/mdkg/config.json");
    std::fs::create_dir_all(user.parent().expect("parent")).expect("dir");
    std::fs::write(
        &user,
        json!({"serve": {"drain_grace_s": 99999}}).to_string(),
    )
    .expect("write");
    let (exit, v) = f.run(&["serve"]);
    assert_ne!(exit, 0, "{v}");
    assert_eq!(code(&v), "config.invalid", "{v}");
    assert!(!f.common().join("mdkg/local.sock").exists());
    assert!(!f.common().join("mdkg/serve.json").exists());
    assert!(!mdkg_serve::state::lock_held(&f.common()).expect("probe"));
    std::fs::remove_file(&user).expect("rm");

    // Then it starts.
    let s = f.serve(&[]);
    assert_eq!(s.envelope["ok"], true, "{}", s.envelope);
}

#[test]
fn loopback_listens_with_a_token_file_and_refuses_other_addresses() {
    let f = Fixture::new();
    let (exit, v) = f.run(&["serve", "--listen", "0.0.0.0:7461"]);
    assert_eq!(exit, 1, "{v}");
    assert_eq!(code(&v), "usage.invalid_arguments");
    assert!(
        !v.to_string().contains("0.0.0.0"),
        "never echoes the address"
    );
    assert!(!f.common().join("mdkg/serve.json").exists());

    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port();
    let addr = format!("127.0.0.1:{port}");
    let s = f.serve(&["--listen", &addr]);
    assert_eq!(s.envelope["ok"], true, "{}", s.envelope);
    assert_eq!(s.data()["tcp"], addr.as_str());
    let token_file = PathBuf::from(s.data()["token_file"].as_str().expect("token file"));
    assert!(
        token_file.starts_with(&f.home),
        "under the user's state folder"
    );
    assert_eq!(mode(&token_file), 0o600);
    let token = std::fs::read_to_string(&token_file).expect("token");
    assert!(!s.envelope.to_string().contains(token.trim()));
    let json = std::fs::read_to_string(f.common().join("mdkg/serve.json")).expect("serve.json");
    assert!(json.contains(&addr) && !json.contains(token.trim()));
    // A client attaching later finds the file's path, never the token.
    let (exit, st) = f.run(&["serve", "status"]);
    assert_eq!(exit, 0, "{st}");
    assert_eq!(
        st["data"]["serve"]["token_file"].as_str(),
        token_file.to_str(),
        "{st}"
    );
    let shown = st.to_string();
    assert!(!shown.contains(token.trim()));
    s.signal("-TERM");
    assert_eq!(s.wait(Duration::from_secs(30)), Some(0));
}

/// `msg watch --json`'s first `n` lines from the log's start.
fn watch_lines(f: &Fixture, n: usize) -> Vec<String> {
    let mut child = f
        .mdkg()
        .args(["msg", "watch", "--json", "--after", "start"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("watch");
    let mut out = BufReader::new(child.stdout.take().expect("stdout"));
    let mut lines = Vec::new();
    while lines.len() < n {
        let mut line = String::new();
        if out.read_line(&mut line).expect("read") == 0 {
            break;
        }
        let line = line.trim_end().to_owned();
        if !line.contains("mdkg.stream.heartbeat") {
            lines.push(line);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    lines
}

/// `GET <path>` on the socket, as an event stream.
fn events(socket: &Path, path: &str, query: Option<&str>) -> mdkg_wire::client_conn::Events {
    let mut c = ClientConnection::new(Box::new(
        std::os::unix::net::UnixStream::connect(socket).expect("connect"),
    ));
    c.send(
        &Outgoing::new("GET", path, query.map(str::to_owned)),
        Duration::from_secs(10),
    )
    .expect("send");
    let head = c.read_head(Duration::from_secs(10)).expect("head");
    if head.status != 200 {
        let body = c.read_body(&head, 1 << 20, Duration::from_secs(10));
        panic!(
            "{}: {:?}",
            head.status,
            body.map(|b| String::from_utf8_lossy(&b).into_owned())
        );
    }
    c.into_events(&head, Duration::from_secs(30))
        .expect("events")
}

#[test]
fn events_are_msg_watch_lines_and_a_drain_ends_them_with_a_last_error() {
    use mdkg_ops::wire::EventStream;
    let f = Fixture::new();
    for text in ["one", "two"] {
        let (exit, v) = f.run(&[
            "msg", "send", "--to", "lane-b", "--kind", "note", "--text", text,
        ]);
        assert_eq!(exit, 0, "{v}");
    }
    let expected = watch_lines(&f, 2);
    assert_eq!(expected.len(), 2);
    let s = f.serve(&[]);
    let socket = s.socket();

    // The same records, the same bytes, and each id its cursor.
    let mut stream = events(&socket, "/v1/events", Some("after=start"));
    let mut got = Vec::new();
    while got.len() < 2 {
        let e = stream.next_event().expect("event").expect("an event");
        got.push(e);
    }
    for (e, line) in got.iter().zip(&expected) {
        assert_eq!(&e.data, line, "the msg watch line itself");
        let v: Value = serde_json::from_str(line).expect("json");
        assert_eq!(e.id, v["cursor"].as_str().expect("cursor"));
    }

    // A bad cursor: msg watch's own refusal envelope, as a response.
    let mut c = ClientConnection::new(Box::new(
        std::os::unix::net::UnixStream::connect(&socket).expect("connect"),
    ));
    c.send(
        &Outgoing::new("GET", "/v1/events", Some("after=nope".into())),
        Duration::from_secs(10),
    )
    .expect("send");
    let head = c.read_head(Duration::from_secs(10)).expect("head");
    let body = c
        .read_body(&head, 1 << 20, Duration::from_secs(10))
        .expect("body");
    let v: Value = serde_json::from_slice(&body).expect("json");
    assert_eq!(head.status, 400, "{v}");
    assert_eq!(v["schema"], "mdkg.msg.watch.v1");
    assert_eq!(code(&v), "api.bad_cursor");

    // SIGTERM: the open stream gets its last mdkg.stream.error, then
    // serve exits 0.
    s.signal("-TERM");
    let end = loop {
        match stream.next_event() {
            Ok(Some(_)) => continue,
            other => break other,
        }
    };
    match end {
        Err(d) => assert_eq!(d[0].code.as_str(), "api.draining", "{d:?}"),
        other => panic!("not a last error frame: {other:?}"),
    }
    assert_eq!(s.wait(Duration::from_secs(30)), Some(0));
}

/// One request on the socket: its status and JSON body.
fn ask(
    socket: &Path,
    method: &str,
    path: &str,
    actor: Option<&str>,
    body: Option<&Value>,
) -> (u16, Value) {
    let io = std::os::unix::net::UnixStream::connect(socket).expect("connect");
    let mut c = ClientConnection::new(Box::new(io));
    let (path, query) = match path.split_once('?') {
        Some((p, q)) => (p, Some(q.to_owned())),
        None => (path, None),
    };
    let mut req = Outgoing::new(method, path, query);
    if let Some(actor) = actor {
        req.header("Mdkg-Actor", actor).expect("actor");
    }
    if let Some(body) = body {
        req.header("Content-Type", "application/json")
            .expect("type");
        req.body(body.to_string().into_bytes()).expect("body");
    }
    c.send(&req, Duration::from_secs(10)).expect("send");
    let head = c.read_head(Duration::from_secs(60)).expect("head");
    let body = c
        .read_body(&head, 1 << 22, Duration::from_secs(60))
        .expect("body");
    (
        head.status,
        serde_json::from_slice(&body).unwrap_or(Value::Null),
    )
}

/// One request on loopback with the token: its status and JSON body.
fn ask_tcp(
    addr: &str,
    token: &str,
    method: &str,
    path: &str,
    actor: &str,
    body: &Value,
) -> (u16, Value) {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect(addr).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(60)))
        .expect("timeout");
    let body = body.to_string();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nhost: {addr}\r\nauthorization: Bearer {}\r\nmdkg-actor: {actor}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        token.trim(),
        body.len()
    );
    s.write_all(req.as_bytes()).expect("write");
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out);
    let text = String::from_utf8_lossy(&out).into_owned();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let body = text.split_once("\r\n\r\n").map_or("", |(_, b)| b);
    (status, serde_json::from_str(body).unwrap_or(Value::Null))
}

/// The CLI's exit code and envelope for `args`, run as `actor`.
fn cli_as(f: &Fixture, actor: &str, args: &[&str]) -> (i32, Value) {
    let out = f
        .mdkg_as(actor)
        .args(args)
        .arg("--json")
        .output()
        .expect("mdkg");
    let v = serde_json::from_slice(&out.stdout).expect("json");
    (out.status.code().unwrap_or(-1), v)
}

/// A launcher turn record for `actor`, held live by its lock, whose
/// launcher is the process `launcher`, as the turn check reads it.
fn live_turn(f: &Fixture, actor: &str, launcher: u32) -> std::fs::File {
    let out = Command::new("ps")
        .env("TZ", "UTC")
        .env("LC_ALL", "C")
        .args(["-o", "lstart=", "-p", &launcher.to_string()])
        .output()
        .expect("ps");
    let started = String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let dir = mdkg_run::hooks::dir(&f.common()).join("turns");
    std::fs::create_dir_all(&dir).expect("turns dir");
    let path = dir.join(format!("{actor}.json"));
    let record = json!({
        "actor": actor,
        "harness": "claude",
        "cwd": f.repo,
        "launcher_process": {"pid": launcher, "started": started},
    });
    std::fs::write(&path, record.to_string()).expect("record");
    let file = std::fs::OpenOptions::new()
        .read(true)
        .open(&path)
        .expect("open");
    file.lock().expect("lock");
    file
}

/// B1 (e2e 0.7.1): in graph-ref mode serve runs against the checkout and
/// serves the graph on the ref, from the repository and from a linked
/// worktree, and `serve status` follows it.
#[test]
fn serve_starts_and_answers_in_ref_mode() {
    let mut f = Fixture::ref_mode();
    let (code, created) = f.run(&["new", "task", "Served from the ref"]);
    assert_eq!(code, 0, "{created}");
    let gid = f.git(&["show", "refs/heads/mdkg-graph:.mdkg/config.json"]);
    let gid: Value = serde_json::from_str(&gid).expect("config");
    let gid = gid["graph_id"].as_str().expect("graph id").to_owned();
    let wt = f.dir.join("wt");
    f.git(&[
        "worktree",
        "add",
        "-q",
        "-b",
        "wt",
        wt.to_str().expect("utf-8"),
    ]);
    for root in [f.repo.clone(), wt] {
        f.root = root;
        assert_eq!(f.status()["data"]["state"], "absent");
        let s = f.serve(&[]);
        assert_eq!(s.envelope["ok"], true, "{}", s.envelope);
        assert_eq!(s.data()["graph_id"], gid.as_str());
        let socket = s.socket();
        assert_eq!(socket, f.common().join("mdkg/local.sock"));
        let root = get_root(Box::new(
            std::os::unix::net::UnixStream::connect(&socket).expect("connect"),
        ));
        assert_eq!(root["data"]["graph_id"], gid.as_str(), "{root}");
        let status = f.status();
        assert_eq!(status["data"]["state"], "running", "{status}");
        // A read answers the CLI's own envelope, the ref's node in it.
        let nodes = format!("/v1/graphs/{gid}/nodes");
        let (code, http) = ask(&socket, "GET", &nodes, Some("serve-test"), None);
        let (exit, cli) = cli_as(&f, "serve-test", &["list"]);
        assert_eq!((code, exit), (200, 0), "{http}");
        assert_eq!(http, cli);
        assert!(http.to_string().contains("Served from the ref"), "{http}");
        s.signal("-TERM");
        assert_eq!(s.wait(Duration::from_secs(30)), Some(0));
        assert_eq!(f.status()["data"]["state"], "absent");
    }
}

/// #16656: in ref mode serve starts past a malformed `.mdkg/config.json`
/// in the checkout (not the graph's), still applies the user and local
/// layers, and warns about a `serve` key in the graph ref's committed
/// config, which it ignores.
#[test]
fn ref_mode_serve_reads_the_refs_config_not_the_checkouts() {
    let f = Fixture::ref_mode();
    let config = f.git(&["show", "refs/heads/mdkg-graph:.mdkg/config.json"]);
    let mut config: Value = serde_json::from_str(&config).expect("config");
    config["serve"] = json!({"listen": "127.0.0.1:1"});
    commit_on_graph(&f, ".mdkg/config.json", &config);
    std::fs::create_dir_all(f.repo.join(".mdkg")).expect("checkout .mdkg");
    std::fs::write(f.repo.join(".mdkg/config.json"), "<<<<<<< conflict").expect("broken");
    let user = f.home.join(".config/mdkg/config.json");
    std::fs::create_dir_all(user.parent().expect("parent")).expect("user dir");
    // The user layer's grace applies; the local layer is read too: its
    // listen is ignored with its own warning.
    std::fs::write(&user, json!({"serve": {"drain_grace_s": 11}}).to_string()).expect("user");
    let local = f.common().join("mdkg/config.local.json");
    std::fs::create_dir_all(local.parent().expect("parent")).expect("local dir");
    let listen = json!({"serve": {"listen": "127.0.0.1:2"}});
    std::fs::write(&local, listen.to_string()).expect("local");
    let s = f.serve(&[]);
    assert_eq!(s.envelope["ok"], true, "{}", s.envelope);
    assert_eq!(s.data()["drain_grace_s"], 11, "{}", s.envelope);
    assert_eq!(s.data()["tcp"], Value::Null, "{}", s.envelope);
    let diagnostics = s.envelope["diagnostics"].as_array().expect("diagnostics");
    let with = |text: &str| {
        diagnostics
            .iter()
            .filter(|d| d["message"].as_str().is_some_and(|m| m.contains(text)))
            .collect::<Vec<_>>()
    };
    let committed = with("committed .mdkg/config.json");
    assert_eq!(committed.len(), 1, "{}", s.envelope);
    // The hint names this run's user config, and keeps listen out of the
    // machine-local one.
    let hint = committed[0]["hint"].as_str().expect("hint");
    assert!(hint.contains(user.to_str().expect("utf-8")), "{hint}");
    assert!(hint.contains("ignores serve.listen"), "{hint}");
    let local = with("machine-local config ignored");
    assert_eq!(local.len(), 1, "{}", s.envelope);
    let hint = local[0]["hint"].as_str().expect("hint");
    assert!(hint.contains(user.to_str().expect("utf-8")), "{hint}");
    s.signal("-TERM");
    assert_eq!(s.wait(Duration::from_secs(30)), Some(0));
}

/// Commits `value` at `path` on the graph ref, as a graph write from
/// another clone would. Git runs isolated, hooks off.
fn commit_on_graph(f: &Fixture, path: &str, value: &Value) {
    let file = f.dir.join("graph-file.json");
    std::fs::write(&file, serde_json::to_vec_pretty(value).expect("json")).expect("write");
    let index = f.dir.join("graph-file.index");
    let git = |args: &[&str]| {
        let out = isolated(&mut Command::new("git"), &f.home)
            .current_dir(&f.repo)
            .env("GIT_INDEX_FILE", &index)
            .args(["-c", "core.hooksPath=/dev/null"])
            .args(args)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    };
    let blob = git(&["hash-object", "-w", file.to_str().expect("utf-8")]);
    git(&["read-tree", "refs/heads/mdkg-graph"]);
    let entry = format!("100644,{blob},{path}");
    git(&["update-index", "--add", "--cacheinfo", &entry]);
    let tree = git(&["write-tree"]);
    let commit = git(&[
        "commit-tree",
        &tree,
        "-p",
        "refs/heads/mdkg-graph",
        "-m",
        path,
    ]);
    git(&["update-ref", "refs/heads/mdkg-graph", &commit]);
}

/// A second graph at sub/.mdkg on the graph ref, as a sync from a clone
/// with a nested graph would leave it; its id. Git runs isolated, hooks
/// off.
fn nested_graph(f: &Fixture) -> String {
    let config = f.git(&["show", "refs/heads/mdkg-graph:.mdkg/config.json"]);
    let mut config: Value = serde_json::from_str(&config).expect("config");
    let nested = uuid::Uuid::now_v7().to_string();
    config["graph_id"] = nested.clone().into();
    let file = f.dir.join("nested-config.json");
    std::fs::write(&file, serde_json::to_vec_pretty(&config).expect("json")).expect("write");
    let index = f.dir.join("nested.index");
    let git = |args: &[&str]| {
        let out = isolated(&mut Command::new("git"), &f.home)
            .current_dir(&f.repo)
            .env("GIT_INDEX_FILE", &index)
            .args(["-c", "core.hooksPath=/dev/null"])
            .args(args)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    };
    let blob = git(&["hash-object", "-w", file.to_str().expect("utf-8")]);
    git(&["read-tree", "refs/heads/mdkg-graph"]);
    let entry = format!("100644,{blob},sub/.mdkg/config.json");
    git(&["update-index", "--add", "--cacheinfo", &entry]);
    let tree = git(&["write-tree"]);
    let commit = git(&[
        "commit-tree",
        &tree,
        "-p",
        "refs/heads/mdkg-graph",
        "-m",
        "nested",
    ]);
    git(&["update-ref", "refs/heads/mdkg-graph", &commit]);
    nested
}

/// `serve` from `f.root`, expected to refuse: bounded, so a regression
/// that starts serving fails the test rather than hanging it.
fn serve_refused(f: &Fixture) -> Value {
    let mut child = f
        .mdkg()
        .args(["serve", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("serve");
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait") {
            break status;
        }
        if std::time::Instant::now() > deadline {
            child.kill().ok();
            child.wait().ok();
            panic!("serve started instead of refusing");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let mut out = Vec::new();
    std::io::Read::read_to_end(&mut child.stdout.take().expect("stdout"), &mut out).expect("read");
    let v: Value = serde_json::from_slice(&out).expect("envelope");
    assert!(!status.success(), "{v}");
    v
}

/// B1 review (#15279, #15389): served calls bind a ref-mode clone's root
/// graph, so serve refuses a graph whose root isn't the checkout top,
/// whichever way it's recorded, and still serves the root graph.
#[test]
fn serve_refuses_a_nested_graph_in_ref_mode() {
    // `.ref` and `.root` records, and a 0.7.0 `.worktree` record alone.
    for legacy in [false, true] {
        let mut f = Fixture::ref_mode();
        let nested = nested_graph(&f);
        if legacy {
            f.git(&["config", &format!("mdkg.graph.{nested}.worktree"), "graph"]);
        } else {
            f.git(&[
                "config",
                &format!("mdkg.graph.{nested}.ref"),
                "refs/heads/mdkg-graph",
            ]);
            f.git(&["config", &format!("mdkg.graph.{nested}.root"), "sub"]);
        }
        f.root = f.repo.join("sub");
        std::fs::create_dir_all(&f.root).expect("sub");
        let out = serve_refused(&f);
        assert!(out.to_string().contains("nested graph"), "{out}");
        assert_eq!(f.status()["data"]["state"], "absent");
        f.root = f.repo.clone();
        let s = f.serve(&[]);
        assert_eq!(s.envelope["ok"], true, "{}", s.envelope);
        s.signal("-TERM");
        assert_eq!(s.wait(Duration::from_secs(30)), Some(0));
    }
}

/// #15491: a legacy nested graph (a `.worktree` record, no `.root`)
/// needs git to say where the checkout top is. When git can't (here a
/// folder whose name ends in a space, which git's output loses), serve
/// doesn't start under the nested graph's id. This trigger is refused
/// before serve's own check, which fails closed for any other one.
#[test]
fn serve_refuses_when_the_checkout_top_is_unknown() {
    let mut f = Fixture::ref_mode();
    let nested = nested_graph(&f);
    f.git(&["config", &format!("mdkg.graph.{nested}.worktree"), "graph"]);
    let spaced = f.dir.join("repo ");
    std::fs::rename(&f.repo, &spaced).expect("rename");
    f.repo = spaced.clone();
    f.root = spaced.join("sub");
    std::fs::create_dir_all(&f.root).expect("sub");
    serve_refused(&f);
    f.root = spaced;
    serve_refused(&f);
}

/// #15389 minor: the root graph recorded with `.root = .` is the root,
/// and serves.
#[test]
fn serve_takes_a_dot_root_record_as_the_root() {
    let f = Fixture::ref_mode();
    let config = f.git(&["show", "refs/heads/mdkg-graph:.mdkg/config.json"]);
    let config: Value = serde_json::from_str(&config).expect("config");
    let gid = config["graph_id"].as_str().expect("graph id").to_owned();
    f.git(&["config", &format!("mdkg.graph.{gid}.root"), "."]);
    let s = f.serve(&[]);
    assert_eq!(s.envelope["ok"], true, "{}", s.envelope);
    assert_eq!(s.data()["graph_id"], gid.as_str());
    s.signal("-TERM");
    assert_eq!(s.wait(Duration::from_secs(30)), Some(0));
}

#[test]
fn an_operation_over_the_socket_answers_the_cli_s_own_envelope() {
    let f = Fixture::new();
    let s = f.serve(&[]);
    let socket = s.socket();
    let gid = s.data()["graph_id"].as_str().expect("graph id").to_owned();
    let nodes = format!("/v1/graphs/{gid}/nodes");
    // A read: the CLI's envelope, value for value.
    let (status, http) = ask(&socket, "GET", &nodes, Some("serve-test"), None);
    let (exit, cli) = cli_as(&f, "serve-test", &["list"]);
    assert_eq!((status, exit), (200, 0), "{http}");
    assert_eq!(http, cli);
    // A refusal: the CLI's envelope, with A12's status for its exit code.
    let missing = format!("{nodes}/task-nope");
    let (status, http) = ask(&socket, "GET", &missing, Some("serve-test"), None);
    let (exit, cli) = cli_as(&f, "serve-test", &["show", "task-nope"]);
    assert_eq!(http, cli);
    assert_eq!((status, exit), (404, 3), "{http}");
    // A write through the real backend: the CLI then sees its lease.
    let (status, claimed) = ask(
        &socket,
        "POST",
        "/v1/leases",
        Some("lane-http"),
        Some(&json!({"target": "src/served.rs", "new": true})),
    );
    // A create (POST to a collection) is 201 (A12).
    assert_eq!(status, 201, "{claimed}");
    assert_eq!(claimed["schema"], "mdkg.claim.v1");
    let (_, holds) = cli_as(&f, "lane-http", &["holds", "src/served.rs"]);
    assert!(holds.to_string().contains("lane-http"), "{holds}");
    // Claiming it again renews it: nothing is created, so 200.
    let (status, again) = ask(
        &socket,
        "POST",
        "/v1/leases",
        Some("lane-http"),
        Some(&json!({"target": "src/served.rs", "new": true})),
    );
    assert_eq!(status, 200, "{again}");
    assert_eq!(again["data"]["renewed"], true, "{again}");
    // A read with a body, an unknown graph and no actor: refused before
    // any CLI runs, each in the CLI's envelope.
    let (status, v) = ask(
        &socket,
        "GET",
        "/v1/graphs/other/nodes",
        Some("serve-test"),
        None,
    );
    assert_eq!(status, 404, "{v}");
    let (status, v) = ask(&socket, "GET", &nodes, None, None);
    assert_eq!(status, 400, "{v}");
    assert_eq!(v["ok"], false);
    assert!(
        v["diagnostics"][0]["message"]
            .as_str()
            .is_some_and(|m| m.contains("Mdkg-Actor")),
        "the refusal names the header: {v}"
    );
    // A positional goes by its argument's name: `new`'s <TYPE> is
    // `node_type`, as docs/serve.md says.
    let (status, made) = ask(
        &socket,
        "POST",
        &nodes,
        Some("lane-http"),
        Some(&json!({"node_type": "task", "title": "Made over serve"})),
    );
    assert_eq!(status, 201, "{made}");
    let (status, v) = ask(
        &socket,
        "POST",
        &nodes,
        Some("lane-http"),
        Some(&json!({"type": "task", "title": "Not made"})),
    );
    assert_eq!(status, 400, "{v}");
    assert!(v.to_string().contains("node_type"), "{v}");
    // An unknown route or method: api.unsupported (A12), with a hint.
    for (method, path) in [("GET", "/v1/nope"), ("PUT", nodes.as_str())] {
        let (status, v) = ask(&socket, method, path, Some("serve-test"), None);
        assert_eq!(status, 501, "{method} {path}: {v}");
        assert_eq!(code(&v), "api.unsupported");
        assert!(v["diagnostics"][0]["hint"].is_string(), "{v}");
    }
    // A long poll over its longest is refused, not cut short (S3).
    let (status, v) = ask(
        &socket,
        "GET",
        "/v1/mailboxes/lane-http:wait?timeout=601",
        Some("lane-http"),
        None,
    );
    assert_eq!(status, 400, "{v}");
    assert_eq!(code(&v), "usage.invalid_arguments");
    s.signal("-TERM");
    assert_eq!(s.wait(Duration::from_secs(30)), Some(0));
}

#[test]
fn a_request_from_inside_a_live_turn_is_refused_before_routing() {
    // #11982: with the real backend wired, a caller descending from a
    // live turn's launcher, or naming a live turn's actor, gets 403
    // api.actor_not_allowed, and no CLI runs.
    let f = Fixture::new();
    let s = f.serve(&[]);
    let socket = s.socket();
    let gid = s.data()["graph_id"].as_str().expect("graph id").to_owned();
    let claim = |actor: &str, path: &str| {
        ask(
            &socket,
            "POST",
            "/v1/leases",
            Some(actor),
            Some(&json!({"target": path, "new": true})),
        )
    };
    // A turn launched by an unrelated process: only its actor is refused.
    let mut other = Command::new("sleep").arg("120").spawn().expect("sleep");
    let turn = live_turn(&f, "a-agent", other.id());
    let (status, v) = claim("a-agent", "src/a.rs");
    assert_eq!(status, 403, "{v}");
    assert_eq!(code(&v), "api.actor_not_allowed");
    let (status, v) = claim("lane-b", "src/b.rs");
    assert_eq!(status, 201, "{v}");
    drop(turn);
    let _ = other.kill();
    let _ = other.wait();
    // A turn launched by this test's process: every request from it is a
    // descendant's, refused whatever actor it names, reads included.
    let mine = live_turn(&f, "b-agent", std::process::id());
    let (status, v) = claim("lane-c", "src/c.rs");
    assert_eq!(status, 403, "{v}");
    assert_eq!(code(&v), "api.actor_not_allowed");
    let (status, _) = ask(
        &socket,
        "GET",
        &format!("/v1/graphs/{gid}/nodes"),
        Some("lane-c"),
        None,
    );
    assert_eq!(status, 403);
    // Nothing ran for the refused claims.
    let (_, holds) = cli_as(&f, "lane-c", &["holds", "src/c.rs"]);
    assert!(
        holds["ok"] == true && !holds.to_string().contains("lane-c"),
        "{holds}"
    );
    drop(mine);
    s.signal("-TERM");
    assert_eq!(s.wait(Duration::from_secs(30)), Some(0));
}

#[test]
fn a_person_s_ledger_writes_are_refused_on_loopback_whatever_the_spelling() {
    let f = Fixture::new();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port();
    let addr = format!("127.0.0.1:{port}");
    let s = f.serve(&["--listen", &addr]);
    let gid = s.data()["graph_id"].as_str().expect("graph id").to_owned();
    let token = std::fs::read_to_string(s.data()["token_file"].as_str().expect("token file"))
        .expect("token");
    let done = format!("/v1/graphs/{gid}/nodes/task-nope:done");
    let residual = json!({"residual": "accepted"});
    for actor in ["human:ana", "Human:Ana", "HUMAN:ana", " human:ana "] {
        let (status, v) = ask_tcp(&addr, &token, "POST", &done, actor, &residual);
        assert_eq!(status, 403, "{actor}: {v}");
        assert_eq!(code(&v), "api.actor_not_allowed", "{actor}");
    }
    // An agent's done, and a person's read, reach the CLI (no such node).
    let (status, v) = ask_tcp(&addr, &token, "POST", &done, "lane-a", &residual);
    assert_eq!(status, 404, "{v}");
    // The same person's request over the socket isn't serve's to refuse:
    // the caller's process is checked there, and the CLI answers.
    let (status, v) = ask(
        &s.socket(),
        "POST",
        &done,
        Some("human:ana"),
        Some(&residual),
    );
    assert_eq!(status, 404, "{v}");
    s.signal("-TERM");
    assert_eq!(s.wait(Duration::from_secs(30)), Some(0));
}

#[test]
fn a_replaced_program_answers_api_draining_and_serve_exits_0() {
    let f = Fixture::new();
    let bin = f.dir.join("bin");
    std::fs::create_dir_all(&bin).expect("bin");
    let program = bin.join("mdkg");
    std::fs::copy(env!("CARGO_BIN_EXE_mdkg"), &program).expect("copy");
    // A running turn holds the drain's grace open, so serve keeps
    // answering after it sees the change; its launcher is unrelated, so
    // this test's requests aren't refused as a turn's.
    let mut other = Command::new("sleep").arg("120").spawn().expect("sleep");
    let turn = live_turn(&f, "a-agent", other.id());
    let s = f.serve_from(&program, &[]);
    let gid = s.data()["graph_id"].as_str().expect("graph id").to_owned();
    let nodes = format!("/v1/graphs/{gid}/nodes");
    assert_eq!(ask(&s.socket(), "GET", &nodes, Some("lane-b"), None).0, 200);
    // A reinstall: another file renamed over the program.
    let next = bin.join("mdkg.next");
    std::fs::write(&next, "#!/bin/sh\nexit 9\n").expect("next");
    std::fs::set_permissions(&next, std::fs::Permissions::from_mode(0o755)).expect("mode");
    std::fs::rename(&next, &program).expect("replace");
    let (status, v) = ask(&s.socket(), "GET", &nodes, Some("lane-b"), None);
    assert_eq!(status, 503, "{v}");
    assert_eq!(code(&v), "api.draining");
    // The turn ends; serve finishes its drain and exits 0.
    drop(turn);
    let _ = other.kill();
    let _ = other.wait();
    assert_eq!(s.wait(Duration::from_secs(60)), Some(0));
    assert!(!f.common().join("mdkg/serve.json").exists());
}

/// The pids of `mdkg msg <sub>` processes started for `f`'s repository.
fn children(f: &Fixture, sub: &str) -> Vec<String> {
    let out = Command::new("ps")
        .args(["-axo", "pid=,command="])
        .output()
        .expect("ps");
    let repo = f.repo.to_string_lossy().into_owned();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.contains(&repo) && l.contains(&format!(" msg {sub}")))
        .map(|l| l.split_whitespace().next().unwrap_or_default().to_owned())
        .collect()
}

/// Waits up to `limit` for `f`'s `msg <sub>` children to number `n`.
fn until_children(f: &Fixture, sub: &str, n: usize, limit: Duration) -> Vec<String> {
    let until = Instant::now() + limit;
    loop {
        let got = children(f, sub);
        if got.len() == n || Instant::now() >= until {
            return got;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A long poll sent on its own connection, left open.
fn wait_request(socket: &Path) -> ClientConnection {
    wait_request_raw(socket).0
}

/// [`wait_request`], with the socket itself too.
fn wait_request_raw(socket: &Path) -> (ClientConnection, std::os::unix::net::UnixStream) {
    let io = std::os::unix::net::UnixStream::connect(socket).expect("connect");
    let raw = io.try_clone().expect("clone");
    let mut c = ClientConnection::new(Box::new(io));
    let mut req = Outgoing::new("GET", "/v1/mailboxes/erin:wait", Some("timeout=300".into()));
    req.header("Mdkg-Actor", "erin").expect("actor");
    c.send(&req, Duration::from_secs(10)).expect("send");
    (c, raw)
}

#[test]
fn serve_cancels_wait_on_client_disconnect_and_stop() {
    use std::io::Write;
    let f = Fixture::new();
    // The store exists before any child is killed, so a kill never lands
    // in its creation.
    let (exit, v) = f.run(&["msg", "inbox"]);
    assert_eq!(exit, 0, "{v}");
    let s = f.serve(&[]);
    let socket = s.socket();
    let long = Duration::from_secs(15);

    // A long poll whose client goes away stops its child.
    let c = wait_request(&socket);
    assert_eq!(
        until_children(&f, "wait", 1, long).len(),
        1,
        "no msg wait child"
    );
    drop(c);
    let left = until_children(&f, "wait", 0, Duration::from_secs(3));
    assert!(left.is_empty(), "msg wait outlived its client: {left:?}");

    // Bytes sent after the request don't hide the client's going.
    let (c, mut raw) = wait_request_raw(&socket);
    assert_eq!(
        until_children(&f, "wait", 1, long).len(),
        1,
        "no msg wait child"
    );
    raw.write_all(b"GET / HTTP/1.1\r\n").expect("more");
    std::thread::sleep(Duration::from_millis(300));
    drop((c, raw));
    let left = until_children(&f, "wait", 0, Duration::from_secs(3));
    assert!(left.is_empty(), "msg wait outlived its client: {left:?}");

    // A client that shuts down its sending half has disconnected, as
    // docs/serve.md says: its long poll is answered api.draining.
    let (mut c, raw) = wait_request_raw(&socket);
    assert_eq!(
        until_children(&f, "wait", 1, long).len(),
        1,
        "no msg wait child"
    );
    raw.shutdown(std::net::Shutdown::Write).expect("half close");
    let head = c.read_head(Duration::from_secs(10)).expect("head");
    assert_eq!(head.status, 503);
    let body = c
        .read_body(&head, 1 << 20, Duration::from_secs(10))
        .expect("body");
    let v: Value = serde_json::from_slice(&body).expect("json");
    assert_eq!(code(&v), "api.draining", "{v}");
    let left = until_children(&f, "wait", 0, Duration::from_secs(3));
    assert!(left.is_empty(), "msg wait outlived its client: {left:?}");

    // So does an event stream on a quiet store.
    let stream = events(&socket, "/v1/events", None);
    assert_eq!(
        until_children(&f, "watch", 1, long).len(),
        1,
        "no msg watch child"
    );
    drop(stream);
    let left = until_children(&f, "watch", 0, Duration::from_secs(3));
    assert!(left.is_empty(), "msg watch outlived its client: {left:?}");

    // And one whose client sent bytes before it went.
    let mut raw = std::os::unix::net::UnixStream::connect(&socket).expect("connect");
    raw.write_all(b"GET /v1/events HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .expect("send");
    assert_eq!(
        until_children(&f, "watch", 1, long).len(),
        1,
        "no msg watch child"
    );
    raw.write_all(b"x").expect("more");
    std::thread::sleep(Duration::from_millis(300));
    drop(raw);
    let left = until_children(&f, "watch", 0, Duration::from_secs(3));
    assert!(left.is_empty(), "msg watch outlived its client: {left:?}");

    // A SIGTERM with only a long poll and a stream under way stops clean,
    // exit 0, and their children go with them, even with the stream's
    // client never reading or closing.
    let mut c = wait_request(&socket);
    assert_eq!(
        until_children(&f, "wait", 1, long).len(),
        1,
        "no msg wait child"
    );
    let held = events(&socket, "/v1/events", None);
    assert_eq!(
        until_children(&f, "watch", 1, long).len(),
        1,
        "no msg watch child"
    );
    s.signal("-TERM");
    let head = c.read_head(Duration::from_secs(30)).expect("head");
    assert_eq!(head.status, 503);
    assert_eq!(s.wait(Duration::from_secs(30)), Some(0));
    // Checked at once: serve has exited, so nothing of it may be left.
    let left = children(&f, "wait");
    assert!(left.is_empty(), "msg wait outlived serve: {left:?}");
    let left = children(&f, "watch");
    assert!(left.is_empty(), "msg watch outlived serve: {left:?}");
    drop(held);
}

/// B4 on loopback: a long poll or an event stream whose TCP client goes
/// away stops its child, as on the socket.
#[test]
fn serve_cancels_wait_and_watch_on_a_loopback_client_s_disconnect() {
    use std::io::{Read, Write};
    let f = Fixture::new();
    let (exit, v) = f.run(&["msg", "inbox"]);
    assert_eq!(exit, 0, "{v}");
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port();
    let addr = format!("127.0.0.1:{port}");
    let s = f.serve(&["--listen", &addr]);
    let token = std::fs::read_to_string(s.data()["token_file"].as_str().expect("token file"))
        .expect("token");
    let long = Duration::from_secs(15);
    let send = |path: &str| {
        let mut c = std::net::TcpStream::connect(&addr).expect("connect");
        let req = format!(
            "GET {path} HTTP/1.1\r\nhost: {addr}\r\nauthorization: Bearer {}\r\nmdkg-actor: erin\r\n\r\n",
            token.trim()
        );
        c.write_all(req.as_bytes()).expect("write");
        c
    };

    // A long poll whose client sent more before it went (drop_ahead).
    let mut c = send("/v1/mailboxes/erin:wait?timeout=300");
    assert_eq!(
        until_children(&f, "wait", 1, long).len(),
        1,
        "no msg wait child"
    );
    c.write_all(b"GET / HTTP/1.1\r\n").expect("more");
    std::thread::sleep(Duration::from_millis(300));
    drop(c);
    let left = until_children(&f, "wait", 0, Duration::from_secs(3));
    assert!(left.is_empty(), "msg wait outlived its client: {left:?}");

    // An established, idle event stream: its 200 is read first.
    let mut c = send("/v1/events");
    c.set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        assert_eq!(c.read(&mut byte).expect("head"), 1, "the stream closed");
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head).into_owned();
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_eq!(
        until_children(&f, "watch", 1, long).len(),
        1,
        "no msg watch child"
    );
    c.write_all(b"x").expect("more");
    std::thread::sleep(Duration::from_millis(300));
    drop(c);
    let left = until_children(&f, "watch", 0, Duration::from_secs(3));
    assert!(left.is_empty(), "msg watch outlived its client: {left:?}");

    s.signal("-TERM");
    assert_eq!(s.wait(Duration::from_secs(30)), Some(0));
}
