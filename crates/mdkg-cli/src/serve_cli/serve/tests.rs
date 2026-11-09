//! `serve` in this process, on a fixture repository: requests over its
//! real local socket, answered by production's routes or a test's own
//! dispatch where a test needs a slow request.

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use mdkg_serve::boot::SystemClocks;
use mdkg_serve::state::{Found, ServeState};
use mdkg_wire::client_conn::ClientConnection;
use mdkg_wire::sign::Outgoing;
use serde_json::{Value, json};

use super::config;
use super::local::{Dispatch, Identity, Request, root};
use super::run::{self, Signals};
use super::start::{self, Answer, Env, Options};

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
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

/// A committed format 0.7 repository with its own home.
struct Fixture {
    dir: PathBuf,
    repo: PathBuf,
    /// The program serve watches, when a test replaces it.
    program: Option<PathBuf>,
}

impl Fixture {
    fn new() -> Self {
        Self::at("repo")
    }

    /// The repository in the folder `name`, under the fixture's own.
    fn at(name: &str) -> Self {
        // Short, so the socket's path stays within the platform's limit:
        // macOS's own temp folder is already 50 bytes long.
        let dir = PathBuf::from("/tmp").join(format!(
            "mdkg-srv-{}",
            &uuid::Uuid::now_v7().simple().to_string()[20..]
        ));
        std::fs::create_dir_all(&dir).expect("dir");
        let dir = std::fs::canonicalize(dir).expect("canonical");
        let repo = dir.join(name);
        std::fs::create_dir_all(&repo).expect("repo");
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
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "init"]);
        Self {
            dir,
            repo,
            program: None,
        }
    }

    fn common(&self) -> PathBuf {
        self.repo.join(".git")
    }

    fn env(&self) -> Env {
        Env {
            config: config::Env {
                home: Some(self.dir.join("home")),
                xdg_config_home: None,
            },
            runtime: self.dir.clone(),
            state: Some(self.dir.join("state")),
            clocks: Arc::new(SystemClocks::new()),
            program: self.program.clone(),
        }
    }

    /// The user layer's config.
    fn user_config(&self, v: Value) {
        let p = self.dir.join("home/.config/mdkg/config.json");
        std::fs::create_dir_all(p.parent().expect("parent")).expect("dir");
        std::fs::write(p, v.to_string()).expect("user config");
    }

    fn ready(&self, answer: Answer) -> Result<start::Ready, mdkg_graph::GraphError> {
        self.ready_on(None, answer)
    }

    fn ready_on(
        &self,
        listen: Option<&str>,
        answer: Answer,
    ) -> Result<start::Ready, mdkg_graph::GraphError> {
        mdkg_coord_sqlite::git::with_worktree_trust(&Default::default(), || {
            start::start(
                &Options {
                    root: &self.repo,
                    listen,
                    graph_id: None,
                    graph_config: None,
                },
                &self.env(),
                answer,
                &mut || {},
            )
        })
    }

    /// Starts `serve`, running on a thread of its own; its flag is its
    /// SIGTERM.
    fn start(&self, answer: Answer) -> Running {
        self.start_on(None, answer)
    }

    fn start_on(&self, listen: Option<&str>, answer: Answer) -> Running {
        let ready = self.ready_on(listen, answer).expect("serve starts");
        let data = ready.data().clone();
        let flag = Arc::new(AtomicBool::new(false));
        let signals = Signals::manual(flag.clone());
        let thread = std::thread::spawn(move || run::run(ready, &signals));
        Running {
            data,
            flag,
            thread: Some(thread),
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct Running {
    data: super::ServeData,
    flag: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<u8>>,
}

impl Running {
    fn signal(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }

    fn stop(mut self) -> u8 {
        self.signal();
        self.join()
    }

    fn join(&mut self) -> u8 {
        self.thread
            .take()
            .expect("running")
            .join()
            .expect("serve thread")
    }

    fn finished(&self) -> bool {
        self.thread.as_ref().is_none_or(|t| t.is_finished())
    }

    fn stops_within(&self, limit: Duration) -> bool {
        let until = Instant::now() + limit;
        while !self.finished() {
            if Instant::now() >= until {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        true
    }
}

impl Drop for Running {
    /// A test that fails before its stop doesn't leave serve running.
    fn drop(&mut self) {
        self.signal();
    }
}

fn send(socket: &str, req: &Outgoing) -> (u16, Value) {
    let mut c = ClientConnection::new(Box::new(UnixStream::connect(socket).expect("connect")));
    c.send(req, Duration::from_secs(10)).expect("send");
    let head = c.read_head(Duration::from_secs(30)).expect("head");
    let body = c
        .read_body(&head, 1 << 20, Duration::from_secs(30))
        .expect("body");
    (
        head.status,
        serde_json::from_slice(&body).unwrap_or(Value::Null),
    )
}

/// Production's root, and a slow route that sets a flag once it has
/// begun, so a test knows the request is in flight.
struct Slow {
    identity: Identity,
    delay: Duration,
    started: AtomicBool,
}

impl Slow {
    fn new(delay: Duration) -> Arc<Self> {
        Arc::new(Self {
            identity: Identity {
                serve_id: "test".into(),
                graph_id: "test".into(),
            },
            delay,
            started: AtomicBool::new(false),
        })
    }

    fn wait_started(&self) {
        let until = Instant::now() + Duration::from_secs(10);
        while !self.started.load(Ordering::SeqCst) {
            assert!(Instant::now() < until, "the slow request never started");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Dispatch for Slow {
    fn handle(&self, req: &Request<'_>) -> super::local::Reply {
        if req.head.path == "/slow" {
            self.started.store(true, Ordering::SeqCst);
            std::thread::sleep(self.delay);
            return super::local::envelope("mdkg.slow.v1", 200, Some(json!({})), &[]).into();
        }
        root(&self.identity).into()
    }
}

/// An agent turn that runs while the returned file stays locked.
fn running_turn(f: &Fixture) -> std::fs::File {
    let dir = mdkg_run::hooks::dir(&f.common()).join("turns");
    std::fs::create_dir_all(&dir).expect("turns dir");
    let file = std::fs::File::create(dir.join("turn-1.json")).expect("turn record");
    file.lock().expect("lock");
    file
}

#[test]
fn the_local_socket_answers_root_and_refuses_the_rest_as_the_cli_does() {
    let f = Fixture::new();
    let r = f.start(Answer::Production);
    let socket = PathBuf::from(&r.data.socket);
    assert_eq!(socket, f.common().join("mdkg/local.sock"));
    use std::os::unix::fs::PermissionsExt;
    let mode = |p: &Path| std::fs::metadata(p).expect("meta").permissions().mode() & 0o7777;
    assert_eq!(mode(&socket), 0o600);
    assert_eq!(mode(socket.parent().expect("folder")), 0o700);
    assert_eq!(r.data.capabilities, vec!["serve.local".to_owned()]);

    let (status, v) = send(&r.data.socket, &Outgoing::new("GET", "/", None));
    assert_eq!(status, 200);
    assert_eq!(v["schema"], "mdkg.api.serve.v1");
    assert_eq!(v["data"]["serve_id"], r.data.serve_id);
    assert_eq!(v["data"]["graph_id"], r.data.graph_id);
    assert_eq!(v["data"]["capabilities"], json!(["serve.local"]));

    // No signature is asked for, and none is checked: an unsigned write
    // reaches the routes, which refuse a path no operation has as the CLI
    // would (the op routes need the real mdkg; tests/serve.rs runs them).
    let mut req = Outgoing::new("POST", "/v1/unrouted", None);
    req.body(b"{}".to_vec()).expect("body");
    let (status, v) = send(&r.data.socket, &req);
    assert_eq!(status, 501);
    assert_eq!(v["schema"], "mdkg.cli.v1");
    assert_eq!(v["diagnostics"][0]["code"], "api.unsupported");

    assert_eq!(r.stop(), 0);
    assert!(!socket.exists(), "the socket goes on a clean stop");
}

#[test]
fn serve_json_and_status_across_start_crash_and_stop() {
    let f = Fixture::new();
    let state = || super::status_data(&f.repo).expect("status");
    assert_eq!(state().state, "absent");
    // A start whose process goes without its cleanup, as a crash would.
    let ready = f.ready(Answer::Production).expect("start");
    let id = ready.data().serve_id.clone();
    let s = state();
    assert_eq!(s.state, "running");
    let file = s.serve.expect("serve.json");
    assert_eq!(file.serve_id, id);
    assert_eq!(file.pid, std::process::id());
    assert_eq!(file.socket, ready.data().socket);
    drop(ready);
    let s = state();
    assert_eq!(s.state, "stale");
    assert_eq!(s.serve.map(|s| s.serve_id), Some(id.clone()));
    // The next start replaces it, and its own stale socket; a clean stop
    // removes both.
    let r = f.start(Answer::Production);
    assert_eq!(state().state, "running");
    assert_ne!(r.data.serve_id, id);
    assert_eq!(r.stop(), 0);
    assert_eq!(state().state, "absent");
    assert_eq!(ServeState::find(&f.common()).expect("find"), Found::None);
}

#[test]
fn a_second_start_is_refused_and_a_bad_setting_binds_nothing() {
    let f = Fixture::new();
    let r = f.start(Answer::Production);
    let e = f.ready(Answer::Production).expect_err("held");
    assert_eq!(e.diagnostic.code, mdkg_format::Code::ApiServeRunning);
    assert_eq!(r.stop(), 0);
    // A settings error refuses before the lock: nothing taken, nothing
    // bound (R4).
    f.user_config(json!({"serve": {"drain_grace_s": 99999}}));
    let e = f.ready(Answer::Production).expect_err("bad setting");
    assert_eq!(e.diagnostic.code, mdkg_format::Code::ConfigInvalid);
    assert!(!f.common().join("mdkg/local.sock").exists());
    assert!(!mdkg_serve::state::lock_held(&f.common()).expect("probe"));
}

#[test]
fn a_drain_lets_the_request_under_way_finish_and_exits_0() {
    let f = Fixture::new();
    let slow = Slow::new(Duration::from_millis(1500));
    let r = f.start(Answer::With(slow.clone()));
    let socket = r.data.socket.clone();
    let write = std::thread::spawn(move || send(&socket, &Outgoing::new("GET", "/slow", None)).0);
    slow.wait_started();
    let path = PathBuf::from(&r.data.socket);
    let code = r.stop();
    assert_eq!(write.join().expect("write"), 200, "the request finished");
    assert_eq!(code, 0);
    assert!(!path.exists());
}

#[test]
fn a_request_the_drain_cuts_off_exits_non_zero() {
    let f = Fixture::new();
    let slow = Slow::new(Duration::from_secs(14));
    let r = f.start(Answer::With(slow.clone()));
    let socket = r.data.socket.clone();
    let write = std::thread::spawn(move || {
        let mut c = ClientConnection::new(Box::new(UnixStream::connect(socket).expect("connect")));
        c.send(
            &Outgoing::new("GET", "/slow", None),
            Duration::from_secs(10),
        )
        .expect("send");
        c.read_head(Duration::from_secs(30)).is_ok()
    });
    slow.wait_started();
    let started = Instant::now();
    let code = r.stop();
    assert!(
        started.elapsed() >= Duration::from_secs(9),
        "requests get 10 s"
    );
    assert_ne!(code, 0, "a request cut off exits non-zero");
    assert!(
        !write.join().expect("write"),
        "the cut request gets no answer"
    );
}

#[test]
fn the_grace_lasts_while_a_turn_runs_and_a_second_signal_ends_it() {
    let f = Fixture::new();
    f.user_config(json!({"serve": {"drain_grace_s": 120}}));
    let mut r = f.start(Answer::Production);
    let _turn = running_turn(&f);
    let socket = PathBuf::from(&r.data.socket);
    r.signal();
    assert!(!r.stops_within(Duration::from_secs(2)), "the grace held");
    let (status, _) = send(&r.data.socket, &Outgoing::new("GET", "/", None));
    assert_eq!(status, 200, "requests are served through the grace");
    r.signal();
    assert!(
        r.stops_within(Duration::from_secs(5)),
        "the second signal ends it"
    );
    assert_eq!(r.join(), 0);
    assert!(!socket.exists());
}

#[test]
fn a_lock_lost_during_the_drain_ends_it_and_removes_nothing() {
    let f = Fixture::new();
    f.user_config(json!({"serve": {"drain_grace_s": 120}}));
    let mut r = f.start(Answer::Production);
    let _turn = running_turn(&f);
    let socket = PathBuf::from(&r.data.socket);
    let mdkg = f.common().join("mdkg");
    r.signal();
    assert!(!r.stops_within(Duration::from_secs(1)), "the grace held");
    std::fs::rename(mdkg.join("serve.lock"), mdkg.join("serve.lock.aside")).expect("rename");
    assert!(
        r.stops_within(Duration::from_secs(15)),
        "the lost lock ends the drain"
    );
    assert_eq!(r.join(), mdkg_graph::ErrorKind::Failure.exit_code());
    assert!(socket.exists(), "the socket may be another serve's");
    assert!(
        mdkg.join("serve.json").exists(),
        "serve.json may be another serve's"
    );
}

#[test]
fn a_socket_path_too_long_falls_back_to_a_short_one_in_a_private_folder() {
    let f = Fixture::at(&"r".repeat(90));
    let long = f.common().join("mdkg/local.sock");
    assert!(long.as_os_str().len() > super::socket::MAX_PATH);
    let r = f.start(Answer::Production);
    let socket = PathBuf::from(&r.data.socket);
    let uid = mdkg_serve::state::own_uid();
    assert_eq!(
        socket.parent(),
        Some(f.dir.join(format!("mdkg-{uid}")).as_path())
    );
    use std::os::unix::fs::PermissionsExt;
    let mode = |p: &Path| std::fs::metadata(p).expect("meta").permissions().mode() & 0o7777;
    assert_eq!(mode(&socket), 0o600);
    assert_eq!(mode(socket.parent().expect("folder")), 0o700);
    match ServeState::find(&f.common()).expect("find") {
        Found::Running(s) => assert_eq!(PathBuf::from(s.socket), socket),
        other => panic!("not running: {other:?}"),
    }
    let (status, _) = send(&r.data.socket, &Outgoing::new("GET", "/", None));
    assert_eq!(status, 200);
    assert_eq!(r.stop(), 0);
    assert!(!socket.exists());
}

/// A free loopback port, for a test to name.
fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    l.local_addr().expect("addr").port()
}

/// One raw request over loopback TCP, headers as given (wire's client
/// always sends its own `Host`): the status and the envelope.
fn raw(addr: &str, headers: &[(&str, &str)]) -> (u16, Value) {
    raw_to(addr, "GET", "/", headers)
}

/// `method path` over loopback TCP, headers as given.
fn raw_to(addr: &str, method: &str, path: &str, headers: &[(&str, &str)]) -> (u16, Value) {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect(addr).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    let mut req = format!("{method} {path} HTTP/1.1\r\n");
    for (n, v) in headers {
        req.push_str(&format!("{n}: {v}\r\n"));
    }
    req.push_str("connection: close\r\n\r\n");
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

#[test]
fn loopback_needs_its_token_and_refuses_browsers_and_foreign_hosts() {
    let f = Fixture::new();
    let addr = format!("127.0.0.1:{}", free_port());
    let r = f.start_on(Some(&addr), Answer::Production);
    assert_eq!(r.data.tcp.as_deref(), Some(addr.as_str()));
    let token_file = PathBuf::from(r.data.token_file.clone().expect("token file"));
    assert!(token_file.starts_with(f.dir.join("state/mdkg/serve/tokens")));
    assert!(!token_file.starts_with(&f.repo), "outside the repository");
    let token = std::fs::read_to_string(&token_file).expect("token");
    let bearer = format!("Bearer {}", token.trim());
    let host = addr.as_str();

    let (status, v) = raw(&addr, &[("host", host), ("authorization", &bearer)]);
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["data"]["serve_id"], r.data.serve_id);
    for h in ["localhost", "127.0.0.1", "localhost:80"] {
        let (status, _) = raw(&addr, &[("host", h), ("authorization", &bearer)]);
        assert_eq!(status, 200, "{h}");
    }
    // No token, a wrong one, another scheme: refused, the token unnamed.
    let basic = format!("Basic {}", token.trim());
    for auth in [None, Some("Bearer nope"), Some(basic.as_str())] {
        let mut hs = vec![("host", host)];
        if let Some(a) = auth {
            hs.push(("authorization", a));
        }
        let (status, v) = raw(&addr, &hs);
        assert_eq!(status, 401, "{auth:?}");
        assert_eq!(v["diagnostics"][0]["code"], "api.unauthenticated");
        assert!(!v.to_string().contains(token.trim()));
    }
    // A browser's request, even with the token: refused.
    let (status, v) = raw(
        &addr,
        &[
            ("host", host),
            ("authorization", &bearer),
            ("origin", "http://evil.example"),
        ],
    );
    assert_eq!(status, 403);
    assert_eq!(v["diagnostics"][0]["code"], "api.origin_refused");
    let (status, _) = raw(
        &addr,
        &[
            ("host", host),
            ("authorization", &bearer),
            ("sec-fetch-site", "cross-site"),
        ],
    );
    assert_eq!(status, 403);
    // DNS rebinding: a Host that isn't loopback.
    for h in [
        "evil.example",
        "evil.example:1",
        "127.0.0.1.evil.example",
        "[::2]",
    ] {
        let (status, _) = raw(&addr, &[("host", h), ("authorization", &bearer)]);
        assert_eq!(status, 403, "{h}");
    }
    assert_eq!(raw(&addr, &[("authorization", &bearer)]).0, 403, "no Host");

    // The token is nowhere but its file.
    let json = std::fs::read_to_string(f.common().join("mdkg/serve.json")).expect("serve.json");
    assert!(!json.contains(token.trim()));
    assert!(
        !serde_json::to_string(&r.data)
            .expect("data")
            .contains(token.trim())
    );
    // The local socket still needs no token.
    assert_eq!(
        send(&r.data.socket, &Outgoing::new("GET", "/", None)).0,
        200
    );
    assert_eq!(r.stop(), 0);
    assert!(
        std::net::TcpStream::connect(&addr).is_err(),
        "the port is closed"
    );
}

#[test]
fn only_an_explicit_loopback_port_is_bound_and_a_taken_one_fails_visibly() {
    for bad in [
        "0.0.0.0:7461",
        "[::]:7461",
        "192.0.2.10:7461",
        "127.0.0.1:0",
        "localhost:7461",
        "127.0.0.2:7461",
    ] {
        let e = start::listen_addr(Some(bad)).expect_err(bad);
        assert_eq!(
            e.diagnostic.code,
            mdkg_format::Code::UsageInvalidArguments,
            "{bad}"
        );
        assert!(
            !e.diagnostic.message.contains(bad),
            "never echoes the input"
        );
    }
    assert!(
        start::listen_addr(Some("[::1]:7461"))
            .expect("v6")
            .is_some()
    );
    let f = Fixture::new();
    // A refused address binds nothing and takes no lock.
    assert!(
        f.ready_on(Some("0.0.0.0:7461"), Answer::Production)
            .is_err()
    );
    assert!(!mdkg_serve::state::lock_held(&f.common()).expect("probe"));
    // A port that's taken: the start fails, the socket it bound goes,
    // the lock is released, and no other port is tried.
    let taken = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = taken.local_addr().expect("addr").to_string();
    let e = f
        .ready_on(Some(&addr), Answer::Production)
        .expect_err("taken");
    assert_eq!(e.diagnostic.code, mdkg_format::Code::IoWriteFailed);
    assert!(!f.common().join("mdkg/local.sock").exists());
    assert!(!f.common().join("mdkg/serve.json").exists());
    assert!(!mdkg_serve::state::lock_held(&f.common()).expect("probe"));
}

#[test]
fn loopback_without_a_run_wrapper_warns_that_a_turn_could_read_the_token() {
    let warned = |r: &start::Ready| {
        r.warnings()
            .iter()
            .any(|d| d.code == mdkg_format::Code::RunSandboxMismatch)
    };
    let f = Fixture::new();
    // The local socket alone: nothing to warn about.
    let r = f.ready(Answer::Production).expect("socket only");
    assert!(!warned(&r));
    drop(r);
    // Loopback with no wrapper: the warning, which never names the token.
    let addr = format!("127.0.0.1:{}", free_port());
    let r = f
        .ready_on(Some(&addr), Answer::Production)
        .expect("loopback");
    assert!(warned(&r), "{:?}", r.warnings());
    let token =
        std::fs::read_to_string(r.data().token_file.as_deref().expect("file")).expect("token");
    assert!(!format!("{:?}", r.warnings()).contains(token.trim()));
    drop(r);
    // A wrapper only in the machine-local layer isn't read (run.wrapper's
    // trust rule), so it still warns; one in the user layer silences it.
    std::fs::write(
        f.common().join("mdkg/config.local.json"),
        json!({"run": {"wrapper": {"command": ["/usr/bin/true"]}}}).to_string(),
    )
    .expect("local config");
    let r = f
        .ready_on(Some(&addr), Answer::Production)
        .expect("loopback");
    assert!(warned(&r));
    drop(r);
    f.user_config(json!({"run": {"wrapper": {"command": ["/usr/bin/true"]}}}));
    let r = f
        .ready_on(Some(&addr), Answer::Production)
        .expect("loopback");
    assert!(!warned(&r), "{:?}", r.warnings());
}

/// A launcher turn record for `actor`, held live by its lock, whose
/// launcher is the process `launcher` (its start time read as the
/// ancestry check reads it).
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

fn with_actor(path: &str, actor: &str) -> Outgoing {
    let mut req = Outgoing::new("POST", path, None);
    req.header("Mdkg-Actor", actor).expect("actor");
    req.body(b"{}".to_vec()).expect("body");
    req
}

#[test]
fn a_caller_inside_a_turn_is_refused_by_its_pid_and_actor() {
    let f = Fixture::new();
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let turn: super::local::TurnCheck = {
        let seen = seen.clone();
        Arc::new(move |pid, actor| {
            seen.lock().expect("seen").push((pid, actor.to_owned()));
            if actor == "a-agent" {
                Err("a live turn runs as a-agent".into())
            } else {
                Ok(())
            }
        })
    };
    let routes = Arc::new(super::local::Routes {
        identity: Identity {
            serve_id: "test".into(),
            graph_id: "test".into(),
        },
        watch: super::events::Watch {
            binary: Arc::new(super::binary::Binary::fixed(Path::new("/nonexistent"))),
            root: f.repo.clone(),
        },
        ops: None,
    });
    let r = f.start(Answer::WithTurns(routes, turn));
    let (status, v) = send(&r.data.socket, &with_actor("/v1/leases", "a-agent"));
    assert_eq!(status, 403, "{v}");
    assert_eq!(v["diagnostics"][0]["code"], "api.actor_not_allowed");
    // Outside a turn the request reaches the routes (none yet: 501).
    assert_eq!(
        send(&r.data.socket, &with_actor("/v1/leases", "lane-b")).0,
        501
    );
    // GET / is answered whoever asks, and never checked.
    let before = seen.lock().expect("seen").len();
    assert_eq!(
        send(&r.data.socket, &Outgoing::new("GET", "/", None)).0,
        200
    );
    assert_eq!(seen.lock().expect("seen").len(), before);
    // The check saw the caller's own pid: this test process.
    let seen = seen.lock().expect("seen").clone();
    assert!(
        seen.iter().all(|(pid, _)| *pid == std::process::id()),
        "{seen:?}"
    );
    assert_eq!(seen.len(), 2);
    assert_eq!(r.stop(), 0);
}

#[test]
fn the_production_check_walks_the_callers_ancestry() {
    let f = Fixture::new();
    let r = f.start(Answer::Production);
    // A live turn launched by an unrelated process: this test isn't
    // inside it, so a request naming another actor passes...
    let mut other = Command::new("sleep").arg("60").spawn().expect("sleep");
    let _turn = live_turn(&f, "a-agent", other.id());
    assert_eq!(
        send(&r.data.socket, &with_actor("/v1/unrouted", "lane-b")).0,
        501
    );
    // ...and one naming the turn's own actor is refused.
    let (status, v) = send(&r.data.socket, &with_actor("/v1/unrouted", "a-agent"));
    assert_eq!(status, 403, "{v}");
    let _ = other.kill();
    let _ = other.wait();
    drop(_turn);
    // A live turn launched by this test's own process tree: the caller is
    // its descendant, so it's refused whatever actor it names.
    let mine = live_turn(&f, "b-agent", std::process::id());
    let (status, v) = send(&r.data.socket, &with_actor("/v1/unrouted", "lane-b"));
    assert_eq!(status, 403, "{v}");
    assert_eq!(v["diagnostics"][0]["code"], "api.actor_not_allowed");
    assert_eq!(
        send(&r.data.socket, &Outgoing::new("GET", "/", None)).0,
        200
    );
    // End the turn before the stop: the drain's grace lasts while a turn
    // runs, so a turn still held here would hold the stop for the whole
    // grace (120 s by default).
    drop(mine);
    assert_eq!(r.stop(), 0);
}

#[test]
fn loopback_runs_the_actor_half_of_the_turn_check() {
    let f = Fixture::new();
    let addr = format!("127.0.0.1:{}", free_port());
    let r = f.start_on(Some(&addr), Answer::Production);
    let token =
        std::fs::read_to_string(r.data.token_file.as_deref().expect("file")).expect("token");
    let bearer = format!("Bearer {}", token.trim());
    // A live turn of this test's own process tree: on the socket every
    // caller here is its descendant; loopback has no pid, so only the
    // actor it names counts.
    let turn = live_turn(&f, "a-agent", std::process::id());
    let ask = |actor: &str| {
        raw_to(
            &addr,
            "POST",
            "/v1/unrouted",
            &[
                ("host", addr.as_str()),
                ("authorization", bearer.as_str()),
                ("mdkg-actor", actor),
                ("content-length", "0"),
            ],
        )
    };
    let (status, v) = ask("a-agent");
    assert_eq!(status, 403, "{v}");
    assert_eq!(v["diagnostics"][0]["code"], "api.actor_not_allowed");
    // Another actor reaches the routes (none yet: 501); the walk the
    // socket would make is the stated residual, 0.8's TCP peer pid.
    assert_eq!(ask("lane-b").0, 501);
    let (status, _) = send(&r.data.socket, &with_actor("/v1/unrouted", "lane-b"));
    assert_eq!(status, 403, "the socket walks the ancestry");
    drop(turn);
    assert_eq!(r.stop(), 0);
}

#[test]
fn a_replaced_program_refuses_the_spawn_and_drains_serve() {
    use std::os::unix::fs::PermissionsExt;
    let mut f = Fixture::new();
    let program = f.dir.join("bin/mdkg");
    std::fs::create_dir_all(program.parent().expect("dir")).expect("dir");
    std::fs::write(&program, "#!/bin/sh\nexit 1\n").expect("program");
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).expect("mode");
    f.program = Some(program.clone());
    let mut r = f.start(Answer::Production);
    assert_eq!(
        send(&r.data.socket, &Outgoing::new("GET", "/", None)).0,
        200
    );
    // A reinstall: a new file renamed over the old one.
    let next = f.dir.join("bin/mdkg.next");
    std::fs::write(&next, "#!/bin/sh\nexit 2\n").expect("next");
    std::fs::set_permissions(&next, std::fs::Permissions::from_mode(0o755)).expect("mode");
    std::fs::rename(&next, &program).expect("replace");
    let (status, v) = send(
        &r.data.socket,
        &Outgoing::new("GET", "/v1/events", Some("after=start".into())),
    );
    assert_eq!(status, 503, "{v}");
    assert_eq!(v["diagnostics"][0]["code"], "api.draining");
    // serve drains on its own, cleanly, and its names go.
    assert!(r.stops_within(Duration::from_secs(30)));
    assert_eq!(r.join(), 0);
    assert!(!f.common().join("mdkg/serve.json").exists());
}

#[test]
fn an_idle_serve_drains_by_itself_when_its_program_is_replaced() {
    use std::os::unix::fs::PermissionsExt;
    let mut f = Fixture::new();
    let program = f.dir.join("bin/mdkg");
    std::fs::create_dir_all(program.parent().expect("dir")).expect("dir");
    std::fs::write(&program, "#!/bin/sh\nexit 1\n").expect("program");
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).expect("mode");
    f.program = Some(program.clone());
    let mut r = f.start(Answer::Production);
    assert!(
        !r.stops_within(Duration::from_millis(500)),
        "serves while unchanged"
    );
    // A reinstall, and no request at all.
    let next = f.dir.join("bin/mdkg.next");
    std::fs::write(&next, "#!/bin/sh\nexit 2\n").expect("next");
    std::fs::set_permissions(&next, std::fs::Permissions::from_mode(0o755)).expect("mode");
    std::fs::rename(&next, &program).expect("replace");
    assert!(r.stops_within(Duration::from_secs(30)));
    assert_eq!(r.join(), 0);
    assert!(!f.common().join("mdkg/serve.json").exists());
}

/// A long poll that waits until it's cancelled, and records that it was.
struct Poll {
    started: AtomicBool,
    cancelled: AtomicBool,
}

impl Dispatch for Poll {
    fn handle(&self, req: &Request<'_>) -> super::local::Reply {
        req.cancel.long_poll.store(true, Ordering::SeqCst);
        self.started.store(true, Ordering::SeqCst);
        let until = Instant::now() + Duration::from_secs(30);
        while Instant::now() < until {
            if req.cancel.poll.load(Ordering::SeqCst) {
                self.cancelled.store(true, Ordering::SeqCst);
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        super::local::envelope("mdkg.poll.v1", 200, Some(json!({})), &[]).into()
    }
}

#[test]
fn a_long_poll_s_client_that_sends_up_to_the_read_ahead_bound_is_gone() {
    use std::io::Write;
    let f = Fixture::new();
    let poll = Arc::new(Poll {
        started: AtomicBool::new(false),
        cancelled: AtomicBool::new(false),
    });
    let r = f.start(Answer::With(poll.clone()));
    let mut raw = UnixStream::connect(&r.data.socket).expect("connect");
    raw.write_all(b"GET /poll HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .expect("send");
    let until = Instant::now() + Duration::from_secs(10);
    while !poll.started.load(Ordering::SeqCst) {
        assert!(Instant::now() < until, "the poll never started");
        std::thread::sleep(Duration::from_millis(10));
    }
    // Just under the bound, the client still there: the poll goes on.
    let bound = mdkg_wire::codec::LINGER_BYTES;
    raw.write_all(&vec![b'x'; bound - 1])
        .expect("under the bound");
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        !poll.cancelled.load(Ordering::SeqCst),
        "a long poll was cancelled below the bound"
    );
    // The byte that reaches it cancels the poll, the client still open.
    raw.write_all(b"x").expect("the last byte");
    let until = Instant::now() + Duration::from_secs(5);
    while !poll.cancelled.load(Ordering::SeqCst) {
        assert!(
            Instant::now() < until,
            "the long poll outlived a client that sent up to the bound"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(raw);
    assert_eq!(r.stop(), 0);
}

/// A request whose worker ignores every cancel and outlives the drain,
/// holding a recorded child, as a thread delayed under load would.
struct Stuck {
    started: AtomicBool,
    child: std::sync::Mutex<Option<std::process::Child>>,
}

impl Dispatch for Stuck {
    fn handle(&self, _req: &Request<'_>) -> super::local::Reply {
        use std::os::unix::process::CommandExt;
        let mut c = Command::new("sleep");
        c.arg("60").process_group(0);
        let child = c.spawn().expect("sleep");
        crate::ops_exec::register_group(child.id());
        *self.child.lock().expect("child") = Some(child);
        self.started.store(true, Ordering::SeqCst);
        std::thread::sleep(Duration::from_secs(25));
        super::local::envelope("mdkg.stuck.v1", 200, Some(json!({})), &[]).into()
    }
}

#[test]
fn the_drain_kills_a_recorded_child_whose_worker_outlives_it() {
    let f = Fixture::new();
    let stuck = Arc::new(Stuck {
        started: AtomicBool::new(false),
        child: std::sync::Mutex::new(None),
    });
    let r = f.start(Answer::With(stuck.clone()));
    let socket = r.data.socket.clone();
    std::thread::spawn(move || {
        let mut c = ClientConnection::new(Box::new(UnixStream::connect(socket).expect("connect")));
        let _ = c.send(
            &Outgoing::new("GET", "/stuck", None),
            Duration::from_secs(10),
        );
        let _ = c.read_head(Duration::from_secs(60));
    });
    let until = Instant::now() + Duration::from_secs(10);
    while !stuck.started.load(Ordering::SeqCst) {
        assert!(Instant::now() < until, "the request never started");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_ne!(r.stop(), 0, "the stuck request was cut off");
    // serve's drain has returned: the child is already dead, its worker
    // still asleep.
    let mut guard = stuck.child.lock().expect("child");
    let child = guard.as_mut().expect("spawned");
    let until = Instant::now() + Duration::from_secs(2);
    loop {
        if child.try_wait().expect("wait").is_some() {
            break;
        }
        assert!(Instant::now() < until, "the child outlived the drain");
        std::thread::sleep(Duration::from_millis(20));
    }
}
