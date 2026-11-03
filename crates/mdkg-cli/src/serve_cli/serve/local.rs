//! The local listener (dec-01a18dd9; API v1 A15, A16): HTTP/1.1 on the
//! graph's local socket, where the socket's file mode is the
//! authentication, as the store file's is for the CLI.
//!
//! - **No signature Gate, no served lease mode.** A request on this
//!   socket is a local request, from this OS user, in the coordinator's
//!   one trust domain: whoever can open it can already open the store.
//!   The peer's user is checked anyway (beside the 0600 mode), and a
//!   connection from another user is closed unanswered.
//! - **What it answers** is a [`Dispatch`]'s: `GET /`, and a CLI-style
//!   refusal for everything else until serve-ops' registry and backend
//!   route the CLI's operations (dec-01a18dd9, item 3).
//! - **Framing** is mdkg-wire's HTTP/1.1 codec: one request at a time
//!   per connection, bounded head and body, a `HEAD` answered 405.
//! - **The drain** (S3): no new request once it starts; requests under
//!   way get [`DRAIN_INFLIGHT`]; those still running then are cut, which
//!   the caller reports with a non-zero exit.
//!
//! The networked listeners (mdkg-serve's `Service`, its Gate, enrolment
//! and TLS) stay in the tree, unwired: nothing here reaches them.

use std::io;
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::UnixListener;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use mdkg_format::{Code, Diagnostic};
use mdkg_graph::ErrorKind;
use mdkg_ops::wire::Report;
use mdkg_wire::codec::{
    Closer, Codec, HeadLimits, HttpConnection, Io, ReadError, RequestHead, Response,
};
use serde_json::{Value, json};

use crate::output::DiagnosticOut;

/// The local socket's capability: a client checks it before using the
/// unsigned socket, so a later signed one can't be mistaken for it.
pub const CAPABILITY: &str = "serve.local";
/// `GET /`'s schema.
pub const ROOT_SCHEMA: &str = "mdkg.api.serve.v1";
/// How long requests under way may take once the drain stops new ones
/// (S3 step 3).
pub const DRAIN_INFLIGHT: Duration = Duration::from_secs(10);

/// How long a kept-alive connection may idle before its next request.
const IDLE: Duration = Duration::from_secs(60);
/// A head's whole read, from its first byte.
const HEAD_READ: Duration = Duration::from_secs(10);
/// A body's read, and a response's write.
const BODY_READ: Duration = Duration::from_secs(30);
const WRITE: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(20);
/// How long a drained stream has to write its last frame.
const STREAM_END: Duration = Duration::from_secs(2);
/// How often a request under way checks that its client is still there.
const WATCH: Duration = Duration::from_millis(100);
/// The most a long poll or a stream reads and drops of what its client
/// sends while it runs: the codec's own bound for a lingering read.
const AHEAD: usize = mdkg_wire::codec::LINGER_BYTES;
/// How long requests the drain cut off have to stop their children.
const REAP: Duration = Duration::from_secs(2);

/// Where a connection came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Peer {
    /// The local socket, from this OS user: the connecting process's pid,
    /// when the kernel gave it.
    Unix { pid: Option<u32> },
    /// The loopback listener, with the bearer token checked: no pid, so
    /// only the actor half of the turn check (see [`turn_check`]).
    Loopback,
}

/// The turn check (dec-01a18dd9): whether a caller, by its pid and the
/// actor it names, is inside an agent turn. `Err` holds the reason. Pid 0
/// (loopback, which has none) checks the actor alone.
/// Production's walks the launcher ancestry
/// (`mdkg_run::approval::outside_turn_from`); tests inject their own.
pub type TurnCheck = Arc<dyn Fn(u32, &str) -> Result<(), String> + Send + Sync>;

/// Production's turn check for the repository whose git common dir is
/// `common`: no turn mark to read (the caller's environment isn't
/// visible), no roots (a turn merely live in this checkout refuses
/// nobody), the actor the request names, the caller's pid.
///
/// It is a guard rail, as the CLI's turn checks are, not a boundary. Two
/// residuals are stated (the serve edd's 0.7.1 amendment): a process a
/// turn detaches (`setsid`, a double fork) is reparented to init, so the
/// ancestry walk no longer finds the launcher, and it passes when it
/// names an actor with no live turn; and a pid reused between the
/// connect and the check (see `mdkg_serve::listen::peer_pid`). Neither
/// gives a turn more than the CLI does: a detached process escapes its
/// checks the same way.
pub fn turn_check(common: std::path::PathBuf) -> TurnCheck {
    Arc::new(move |pid, actor| {
        mdkg_run::approval::outside_turn_from(&common, actor, &[], false, pid)
    })
}

/// The loopback listener's bearer token. Never in a log, a diagnostic or
/// `Debug` output.
pub struct Token(String);

impl Token {
    pub fn new(secret: String) -> Self {
        Self(secret)
    }

    /// Whether `given` is the token, in time that doesn't depend on where
    /// they differ.
    fn matches(&self, given: &str) -> bool {
        let (a, b) = (self.0.as_bytes(), given.as_bytes());
        let mut diff = a.len() ^ b.len();
        for (i, x) in a.iter().enumerate() {
            diff |= usize::from(x ^ b.get(i).copied().unwrap_or(0));
        }
        diff == 0
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(..)")
    }
}

/// One request, read whole.
#[derive(Debug)]
pub struct Request<'a> {
    pub peer: Peer,
    pub head: &'a RequestHead,
    pub body: &'a [u8],
    /// When the call it runs must stop.
    pub cancel: &'a Cancel,
}

/// When a request's CLI call must stop, killing its child and everything
/// that started (B4). A long poll is a read, so it stops as soon as its
/// client goes away or serve starts stopping; any other call runs to its
/// end, even with its client gone, unless the drain cuts it off.
#[derive(Debug, Default)]
pub struct Cancel {
    /// A long poll: the client went away, or serve is stopping.
    pub poll: AtomicBool,
    /// Any call: the drain cut it off.
    pub call: AtomicBool,
    /// The request is a long poll, so what its client sends while it
    /// waits is read and dropped, to see the client go behind it.
    pub long_poll: AtomicBool,
    /// Bytes were dropped so: the connection closes after the answer, as
    /// it does for a pipelined request the codec buffered.
    dropped: AtomicBool,
}

impl Cancel {
    fn reset(&self) {
        self.poll.store(false, Ordering::SeqCst);
        self.call.store(false, Ordering::SeqCst);
        self.long_poll.store(false, Ordering::SeqCst);
        self.dropped.store(false, Ordering::SeqCst);
    }

    /// The drain's cut: every call stops.
    fn cut(&self) {
        self.poll.store(true, Ordering::SeqCst);
        self.call.store(true, Ordering::SeqCst);
    }
}

/// What a request gets: a response, or an event stream.
pub enum Reply {
    Response(Response),
    Stream(super::events::Stream),
}

impl From<Response> for Reply {
    fn from(r: Response) -> Self {
        Reply::Response(r)
    }
}

/// What answers a request. Production's is [`Routes`]; tests inject
/// their own.
pub trait Dispatch: Send + Sync {
    fn handle(&self, req: &Request<'_>) -> Reply;
}

/// The local listener's own facts, for `GET /`.
#[derive(Debug, Clone)]
pub struct Identity {
    pub serve_id: String,
    pub graph_id: String,
}

/// Production's dispatch: `GET /`, `GET /v1/events`, and the CLI's
/// operations ([`super::ops::Ops`]; `None` in tests that run no CLI).
#[derive(Debug)]
pub struct Routes {
    pub identity: Identity,
    pub watch: super::events::Watch,
    pub ops: Option<super::ops::Ops>,
}

impl Dispatch for Routes {
    fn handle(&self, req: &Request<'_>) -> Reply {
        // Each request reads the repository fresh (#13066 D2).
        mdkg_git::fresh_reads();
        let (method, path) = (req.head.method.as_str(), req.head.path.as_str());
        match (method, path) {
            ("GET", "/") => root(&self.identity).into(),
            ("GET", super::events::PATH) => {
                let actor = req
                    .head
                    .headers
                    .iter()
                    .find(|(n, _)| n == "mdkg-actor")
                    .map(|(_, v)| v.as_str());
                match self.watch.open(req.head, actor) {
                    Ok(stream) => Reply::Stream(stream),
                    Err(r) => r.into(),
                }
            }
            _ => match &self.ops {
                Some(ops) => ops.answer(req).into(),
                None => unsupported().into(),
            },
        }
    }
}

/// `GET /`: what this listener is.
pub fn root(id: &Identity) -> Response {
    envelope(
        ROOT_SCHEMA,
        200,
        Some(json!({
            "serve_id": id.serve_id,
            "graph_id": id.graph_id,
            "api": ["v1"],
            "capabilities": [CAPABILITY],
        })),
        &[],
    )
}

/// A CLI envelope as a response. Refusals are the CLI's envelopes, never
/// `mdkg.api.error.v1` (dec-01a18dd9's named conflict).
pub fn envelope(schema: &str, status: u16, data: Option<Value>, d: &[Diagnostic]) -> Response {
    let ok = !d.iter().any(|d| d.severity == mdkg_format::Severity::Error);
    let diagnostics: Vec<DiagnosticOut> = d.iter().map(DiagnosticOut::from).collect();
    let mut body = json!({"schema": schema, "ok": ok, "diagnostics": diagnostics});
    if let Some(data) = data {
        body["data"] = data;
    }
    Response {
        status,
        headers: vec![("Content-Type".into(), "application/json".into())],
        body: serde_json::to_vec(&body).unwrap_or_default(),
    }
}

/// A refusal: `mdkg.cli.v1`, the envelope the CLI gives a command it
/// can't run, with A12's status for `kind` and `code`.
/// A refusal with a status the code table doesn't give.
pub(super) fn refusal_at(status: u16, d: Diagnostic) -> Response {
    envelope("mdkg.cli.v1", status, None, &[d])
}

pub fn refusal(kind: ErrorKind, d: Diagnostic) -> Response {
    let status = mdkg_ops::codes::http_status::<()>(None, &Report::refused(kind, d.clone()));
    envelope("mdkg.cli.v1", status, None, &[d])
}

fn unsupported() -> Response {
    refusal(
        ErrorKind::Usage,
        Diagnostic::error(
            Code::ApiUnsupported,
            "this serve doesn't serve that route yet",
        )
        .with_hint("serve answers GET /, GET /v1/events and the CLI's operations"),
    )
}

/// Whether a `Host` value names this machine's loopback: `localhost`,
/// `127.0.0.1` or `[::1]`, with or without a port.
fn loopback_host(host: &str) -> bool {
    let name = if let Some(rest) = host.strip_prefix('[') {
        match rest.split_once(']') {
            Some((inner, port)) if port.is_empty() || port_ok(port) => return inner == "::1",
            _ => return false,
        }
    } else {
        match host.rsplit_once(':') {
            Some((name, port)) if port_ok(&format!(":{port}")) => name,
            Some(_) => return false,
            None => host,
        }
    };
    name.eq_ignore_ascii_case("localhost") || name == "127.0.0.1"
}

/// `:<digits>`, a port.
fn port_ok(s: &str) -> bool {
    s.strip_prefix(':')
        .is_some_and(|p| !p.is_empty() && p.len() <= 5 && p.bytes().all(|b| b.is_ascii_digit()))
}

/// A connection's request state: one swap decides between a request and
/// the drain's cut of an idle connection.
const IDLE_STATE: u8 = 0;
const BUSY: u8 = 1;
const CUT: u8 = 2;

struct Conn {
    state: AtomicU8,
    /// It carries an event stream: the drain ends it with its last frame
    /// rather than waiting for it as a request under way.
    streaming: AtomicBool,
    closer: Box<dyn Closer>,
    /// The request under way's.
    cancel: Cancel,
}

impl Conn {
    fn claim_request(&self) -> bool {
        self.state
            .compare_exchange(IDLE_STATE, BUSY, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    fn claim_cut(&self) -> bool {
        self.state
            .compare_exchange(IDLE_STATE, CUT, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    fn done(&self) {
        let _ = self
            .state
            .compare_exchange(BUSY, IDLE_STATE, Ordering::SeqCst, Ordering::SeqCst);
    }

    fn busy(&self) -> bool {
        self.state.load(Ordering::SeqCst) == BUSY && !self.streaming.load(Ordering::SeqCst)
    }
}

/// Sizes the listener keeps to.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub head: HeadLimits,
    pub body_bytes: u64,
    pub connections: usize,
}

impl Limits {
    pub fn from(l: &mdkg_serve::limits::Limits) -> Self {
        Self {
            head: l.head(),
            body_bytes: l.body_bytes,
            connections: l.connections,
        }
    }
}

/// How the drain ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Drained {
    /// Every request under way finished.
    Clean,
    /// The 10 s limit cut a request off.
    CutOff,
}

/// One `serve`'s local listener.
pub struct Local {
    dispatch: Arc<dyn Dispatch>,
    turn: TurnCheck,
    /// The loopback listener's token, when it listens.
    token: Option<Token>,
    codec: Arc<dyn Codec>,
    limits: Limits,
    uid: u32,
    stopping: AtomicBool,
    grace_ended: AtomicBool,
    live: AtomicUsize,
    conns: Mutex<Vec<Weak<Conn>>>,
    /// Where its requests' children are recorded, for the drain's kill.
    scope: u64,
}

impl std::fmt::Debug for Local {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Local").finish_non_exhaustive()
    }
}

impl Local {
    pub fn new(
        dispatch: Arc<dyn Dispatch>,
        turn: TurnCheck,
        codec: Arc<dyn Codec>,
        limits: Limits,
        token: Option<Token>,
    ) -> Arc<Self> {
        Arc::new(Self {
            dispatch,
            turn,
            token,
            codec,
            limits,
            uid: mdkg_serve::state::own_uid(),
            stopping: AtomicBool::new(false),
            grace_ended: AtomicBool::new(false),
            live: AtomicUsize::new(0),
            conns: Mutex::new(Vec::new()),
            scope: crate::ops_exec::new_scope(),
        })
    }

    /// Accepts on the socket until `stop`. A peer of another user, or one
    /// past the connection cap, is closed unanswered.
    pub fn run_unix(
        self: &Arc<Self>,
        listener: UnixListener,
        stop: Arc<AtomicBool>,
    ) -> io::Result<()> {
        listener.set_nonblocking(true)?;
        while !stop.load(Ordering::SeqCst) {
            let stream = match listener.accept() {
                Ok((s, _)) => s,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(POLL);
                    continue;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    std::thread::sleep(POLL);
                    continue;
                }
            };
            if stream.set_nonblocking(false).is_err()
                || mdkg_serve::listen::peer_uid(&stream) != Some(self.uid)
            {
                continue;
            }
            let pid = mdkg_serve::listen::peer_pid(&stream);
            self.spawn(Box::new(stream), Peer::Unix { pid });
        }
        Ok(())
    }

    /// Accepts on the loopback listener until `stop`. A peer that isn't a
    /// loopback address is closed unanswered (the listener is only ever
    /// bound to one, so this is a second check).
    pub fn run_tcp(
        self: &Arc<Self>,
        listener: TcpListener,
        stop: Arc<AtomicBool>,
    ) -> io::Result<()> {
        listener.set_nonblocking(true)?;
        while !stop.load(Ordering::SeqCst) {
            let (stream, addr): (TcpStream, _) = match listener.accept() {
                Ok(a) => a,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(POLL);
                    continue;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    std::thread::sleep(POLL);
                    continue;
                }
            };
            if !addr.ip().is_loopback() || stream.set_nonblocking(false).is_err() {
                continue;
            }
            self.spawn(Box::new(stream), Peer::Loopback);
        }
        Ok(())
    }

    fn spawn(self: &Arc<Self>, stream: Box<dyn Io>, peer: Peer) {
        if self.live.fetch_add(1, Ordering::SeqCst) >= self.limits.connections {
            self.live.fetch_sub(1, Ordering::SeqCst);
            return;
        }
        let Ok(closer) = stream.closer() else {
            self.live.fetch_sub(1, Ordering::SeqCst);
            return;
        };
        let conn = Arc::new(Conn {
            state: AtomicU8::new(IDLE_STATE),
            streaming: AtomicBool::new(false),
            closer,
            cancel: Cancel::default(),
        });
        self.register(&conn);
        let me = self.clone();
        std::thread::spawn(move || {
            crate::ops_exec::enter_scope(me.scope);
            let http = me.codec.connection(stream, me.limits.head);
            me.connection(http, &conn, peer);
            conn.closer.close();
            me.live.fetch_sub(1, Ordering::SeqCst);
        });
    }

    fn register(&self, conn: &Arc<Conn>) {
        let mut conns = self.conns.lock().unwrap_or_else(|e| e.into_inner());
        conns.retain(|w| w.strong_count() > 0);
        conns.push(Arc::downgrade(conn));
    }

    fn live_conns(&self) -> Vec<Arc<Conn>> {
        let mut conns = self.conns.lock().unwrap_or_else(|e| e.into_inner());
        conns.retain(|w| w.strong_count() > 0);
        conns.iter().filter_map(Weak::upgrade).collect()
    }

    /// One connection's requests, one at a time, until it closes.
    fn connection(&self, mut http: Box<dyn HttpConnection>, conn: &Conn, peer: Peer) {
        loop {
            if self.stopping.load(Ordering::SeqCst) {
                return;
            }
            let head = match http.read_head(IDLE, HEAD_READ) {
                Ok(Some(h)) => h,
                Ok(None) | Err(ReadError::TimedOut | ReadError::Truncated | ReadError::Io(_)) => {
                    return;
                }
                Err(ReadError::TooLarge { reason, .. }) => {
                    let r = refusal(
                        ErrorKind::Usage,
                        Diagnostic::error(Code::ApiTooLarge, reason),
                    );
                    let _ = http.write_response(&r, true, WRITE);
                    return;
                }
                Err(ReadError::Malformed { reason, .. }) => {
                    let r = refusal(
                        ErrorKind::Usage,
                        Diagnostic::error(Code::UsageInvalidArguments, reason),
                    );
                    let _ = http.write_response(&r, true, WRITE);
                    return;
                }
            };
            conn.cancel.reset();
            // One swap decides it against the drain's cut: a request either
            // runs to its answer or is cut before it runs.
            if !conn.claim_request() {
                return;
            }
            let (reply, close) = self.answer(&mut *http, &head, peer, conn);
            let response = match reply {
                Reply::Response(r) => r,
                Reply::Stream(stream) => {
                    conn.streaming.store(true, Ordering::SeqCst);
                    self.stream(&mut *http, stream, conn);
                    conn.done();
                    return;
                }
            };
            let close = close
                || self.stopping.load(Ordering::SeqCst)
                || head.headers.iter().any(|(n, v)| {
                    n == "connection"
                        && v.split(',').any(|t| t.trim().eq_ignore_ascii_case("close"))
                });
            let written = http.write_response(&response, close, WRITE);
            conn.done();
            if written.is_err() || close {
                return;
            }
        }
    }

    /// An event stream: its frames until the child ends, the client goes,
    /// or the drain ends it with a last `mdkg.stream.error`. Dropping the
    /// stream stops its `msg watch` child, so a client that went away
    /// leaves none behind, however quiet the store (B4).
    fn stream(
        &self,
        http: &mut dyn HttpConnection,
        mut stream: super::events::Stream,
        conn: &Conn,
    ) {
        // The codec sets the stream's content-type and cache-control.
        if let Err(e) = http.start_stream(&[]) {
            super::log::line(
                "error",
                "serve.stream_failed",
                json!({"error": e.to_string()}),
            );
            return;
        }
        loop {
            if self.stopping.load(Ordering::SeqCst) {
                let frame = stream.draining_frame();
                let _ = http.write_event(None, "mdkg.stream.error", &frame);
                break;
            }
            match stream.next(Duration::from_millis(250)) {
                super::events::Next::Frame(id, kind, data) => {
                    if http.write_event(id.as_deref(), &kind, &data).is_err() {
                        break;
                    }
                }
                super::events::Next::Idle => {
                    // A stream reads no more requests: what its client
                    // sends is dropped, so its end behind it is seen.
                    if conn.closer.peer_closed() || conn.closer.drop_ahead(AHEAD).gone {
                        break;
                    }
                }
                super::events::Next::Ended => break,
            }
        }
        // Its child goes first, so a client slow to close can't keep it.
        drop(stream);
        let _ = http.close_gracefully();
    }

    /// A request's answer, and whether the connection must close after it
    /// (a body left unread, a refusal before it).
    fn answer(
        &self,
        http: &mut dyn HttpConnection,
        head: &RequestHead,
        peer: Peer,
        conn: &Conn,
    ) -> (Reply, bool) {
        if head.method == "HEAD" {
            return (
                Response {
                    status: 405,
                    headers: vec![("Allow".into(), "GET, POST".into())],
                    body: Vec::new(),
                }
                .into(),
                true,
            );
        }
        let one = |name: &str| -> Result<Option<&str>, ()> {
            let mut values = head
                .headers
                .iter()
                .filter(|(n, _)| n == name)
                .map(|(_, v)| v.as_str());
            let first = values.next();
            if values.next().is_some() {
                Err(())
            } else {
                Ok(first)
            }
        };
        let bad = |reason: &str| -> (Reply, bool) {
            (
                refusal(
                    ErrorKind::Usage,
                    Diagnostic::error(Code::UsageInvalidArguments, reason),
                )
                .into(),
                true,
            )
        };
        // Loopback: the token, and no browser, before the body is read.
        if peer == Peer::Loopback
            && let Err(r) = self.loopback_check(head)
        {
            return (r.into(), true);
        }
        if let Err(r) = self.turn_refusal(head, peer) {
            return (r.into(), true);
        }
        if head.headers.iter().any(|(n, _)| n == "transfer-encoding") {
            return bad("send a body with content-length, not transfer-encoding");
        }
        let len = match one("content-length") {
            Err(()) => return bad("content-length is given twice"),
            Ok(None) => 0,
            Ok(Some(v)) if !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()) => {
                v.parse::<u64>().unwrap_or(u64::MAX)
            }
            Ok(Some(_)) => return bad("content-length isn't a number"),
        };
        if len > self.limits.body_bytes {
            let r = refusal(
                ErrorKind::Usage,
                Diagnostic::error(Code::ApiTooLarge, "the request body is too large"),
            );
            return (r.into(), true);
        }
        let body = if len == 0 {
            Vec::new()
        } else {
            match http.read_body(len, BODY_READ) {
                Ok(b) => b,
                Err(_) => return bad("the request body couldn't be read"),
            }
        };
        let req = Request {
            peer,
            head,
            body: &body,
            cancel: &conn.cancel,
        };
        let dispatch = self.dispatch.clone();
        let done = AtomicBool::new(false);
        let answer = std::thread::scope(|scope| {
            // While the request runs, a watcher stops a long poll whose
            // client went away or once serve is stopping. Without a
            // thread for it, the drain still stops one.
            let watcher = std::thread::Builder::new().spawn_scoped(scope, || {
                let mut dropped = 0;
                while !done.load(Ordering::SeqCst) {
                    let mut gone = conn.closer.peer_closed();
                    // A long poll's client that sent more and then went
                    // away: the bytes hide the end from a peek, so they're
                    // read and dropped, up to the codec's linger bound. A
                    // client that sends that much while it waits is
                    // treated as gone, so nothing past the bound can hide
                    // its end (Terra #15396).
                    if !gone && conn.cancel.long_poll.load(Ordering::SeqCst) {
                        let ahead = conn.closer.drop_ahead(AHEAD - dropped);
                        dropped += ahead.dropped;
                        if ahead.dropped > 0 {
                            conn.cancel.dropped.store(true, Ordering::SeqCst);
                        }
                        gone = ahead.gone || dropped >= AHEAD;
                    }
                    if self.stopping.load(Ordering::SeqCst) || gone {
                        conn.cancel.poll.store(true, Ordering::SeqCst);
                        return;
                    }
                    std::thread::park_timeout(WATCH);
                }
            });
            let answer =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| dispatch.handle(&req)));
            done.store(true, Ordering::SeqCst);
            if let Ok(w) = watcher {
                w.thread().unpark();
            }
            answer
        });
        match answer {
            Ok(r) => (r, conn.cancel.dropped.load(Ordering::SeqCst)),
            Err(_) => (
                refusal(
                    ErrorKind::Failure,
                    Diagnostic::error(Code::IoReadFailed, "serve failed while answering"),
                )
                .into(),
                true,
            ),
        }
    }

    /// The loopback listener's own refusals (dec-01a18dd9): any `Origin`
    /// (a browser's request, which a page on any site can make to a
    /// loopback port), a `Host` other than localhost or the loopback
    /// address (DNS rebinding), and a missing or wrong bearer token. A
    /// refusal names the rule, never the token.
    fn loopback_check(&self, head: &RequestHead) -> Result<(), Response> {
        let values = |name: &str| -> Vec<&str> {
            head.headers
                .iter()
                .filter(|(n, _)| n == name)
                .map(|(_, v)| v.as_str())
                .collect()
        };
        let origin = |message: &str| {
            refusal_at(
                403,
                Diagnostic::error(Code::ApiOriginRefused, message)
                    .with_hint("the loopback listener serves local programs, never a browser page"),
            )
        };
        if !values("origin").is_empty()
            || head
                .headers
                .iter()
                .any(|(n, _)| n.starts_with("sec-fetch-"))
        {
            return Err(origin(
                "a request with an Origin or Sec-Fetch header is refused",
            ));
        }
        match values("host").as_slice() {
            [host] if loopback_host(host) => {}
            _ => {
                return Err(origin(
                    "the Host header must be localhost or the loopback address",
                ));
            }
        }
        let unauthenticated = || {
            refusal_at(
                401,
                Diagnostic::error(
                    Code::ApiUnauthenticated,
                    "the loopback listener needs its bearer token",
                )
                .with_hint(
                    "send `Authorization: Bearer <token>`, the token file `serve.json` and `serve status` name",
                ),
            )
        };
        let Some(token) = &self.token else {
            return Err(unauthenticated());
        };
        match values("authorization").as_slice() {
            [v] => match v.strip_prefix("Bearer ") {
                Some(given) if token.matches(given.trim()) => Ok(()),
                _ => Err(unauthenticated()),
            },
            _ => Err(unauthenticated()),
        }
    }

    /// A request from inside an agent turn is refused (dec-01a18dd9): an
    /// agent in a turn uses its CLI and MCP tools, whose own checks apply,
    /// never serve. Only `GET /`, which changes nothing and shows nothing
    /// of the graph's state, is answered regardless. On the socket the
    /// caller's pid feeds the launcher-ancestry check, and a pid the
    /// kernel didn't give refuses (fail closed). Loopback has no peer
    /// pid: the token stands for being outside a turn, which holds only
    /// for a sandboxed turn (`run.wrapper`, off by default; an unsandboxed
    /// one can read the file, a stated residual, see `token`), so the
    /// actor half of the check runs there too: a request naming a live
    /// turn's actor is refused (orchestrator #12136). The pid's half, by
    /// the TCP peer's ports, is 0.8's.
    fn turn_refusal(&self, head: &RequestHead, peer: Peer) -> Result<(), Response> {
        if head.method == "GET" && head.path == "/" {
            return Ok(());
        }
        let actor = head
            .headers
            .iter()
            .find(|(n, _)| n == "mdkg-actor")
            .map_or("", |(_, v)| v.as_str());
        let pid = match peer {
            Peer::Unix { pid } => pid,
            // Pid 0 ends the ancestry walk at once; the actor is checked.
            Peer::Loopback => Some(0),
        };
        let refused = |why: &str| {
            refusal_at(
                403,
                Diagnostic::error(
                    Code::ApiActorNotAllowed,
                    "serve refuses requests from inside an agent turn",
                )
                .with_hint(format!(
                    "{why}; an agent in a turn uses its mdkg CLI or MCP tools"
                )),
            )
        };
        let Some(pid) = pid else {
            return Err(refused("the caller's process couldn't be identified"));
        };
        (self.turn)(pid, actor).map_err(|why| refused(&why))
    }

    /// A second signal: the grace ends at once.
    pub fn end_grace(&self) {
        self.grace_ended.store(true, Ordering::SeqCst);
    }

    /// The drain (S3): the grace while `turns_running` and until `grace`
    /// or [`Local::end_grace`]; then no new connection or request (`stop`
    /// for the accept loops), idle connections closed, long polls stopped
    /// at once (they answer `api.draining`), and other requests under way
    /// given [`DRAIN_INFLIGHT`] before they're cut. A cut request's call
    /// is stopped too, and given [`REAP`] to kill and reap its child, so
    /// none outlives serve (B4).
    pub fn drain(
        &self,
        grace: Duration,
        turns_running: &dyn Fn() -> bool,
        stop: &AtomicBool,
    ) -> Drained {
        let started = Instant::now();
        while !self.grace_ended.load(Ordering::SeqCst)
            && started.elapsed() < grace
            && turns_running()
        {
            std::thread::sleep(Duration::from_millis(250));
        }
        stop.store(true, Ordering::SeqCst);
        self.stopping.store(true, Ordering::SeqCst);
        for c in self.live_conns() {
            if c.claim_cut() {
                c.closer.close();
            } else {
                c.cancel.poll.store(true, Ordering::SeqCst);
            }
        }
        let until = Instant::now() + DRAIN_INFLIGHT;
        let mut outcome = Drained::Clean;
        loop {
            let busy: Vec<_> = self.live_conns().into_iter().filter(|c| c.busy()).collect();
            if busy.is_empty() {
                break;
            }
            if Instant::now() >= until {
                for c in busy {
                    c.cancel.cut();
                    c.closer.close();
                }
                outcome = Drained::CutOff;
                let until = Instant::now() + REAP;
                while Instant::now() < until && self.live_conns().iter().any(|c| c.busy()) {
                    std::thread::sleep(POLL);
                }
                break;
            }
            std::thread::sleep(POLL);
        }
        // Streams see the stop and end with their last frame; they get a
        // moment to write it before every connection is cut.
        let until = Instant::now() + STREAM_END;
        while Instant::now() < until
            && self.live_conns().iter().any(|c| {
                c.streaming.load(Ordering::SeqCst) && c.state.load(Ordering::SeqCst) == BUSY
            })
        {
            std::thread::sleep(POLL);
        }
        for c in self.live_conns() {
            c.closer.close();
        }
        // A stream cut here stops its `msg watch` child as it ends: given
        // [`REAP`] for that, so none outlives serve (B4).
        let until = Instant::now() + REAP;
        while Instant::now() < until
            && self.live_conns().iter().any(|c| {
                c.streaming.load(Ordering::SeqCst) && c.state.load(Ordering::SeqCst) == BUSY
            })
        {
            std::thread::sleep(POLL);
        }
        // Whatever child is still live, its owner late past every wait,
        // is killed now with its process group, so none outlives serve
        // (orchestrator #15424). Its owner still reaps it.
        let left = crate::ops_exec::kill_live_groups(self.scope);
        if left > 0 {
            super::log::line("warn", "serve.children_killed", json!({"count": left}));
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(method: &str, path: &str) -> RequestHead {
        RequestHead {
            method: method.into(),
            path: path.into(),
            query: None,
            headers: vec![],
        }
    }

    #[test]
    fn root_names_the_local_capability_and_anything_else_is_the_cli_refusal() {
        let routes = Routes {
            identity: Identity {
                serve_id: "s1".into(),
                graph_id: "g1".into(),
            },
            watch: super::super::events::Watch {
                binary: Arc::new(super::super::binary::Binary::fixed(std::path::Path::new(
                    "/nonexistent",
                ))),
                root: ".".into(),
            },
            ops: None,
        };
        let response = |r: Reply| match r {
            Reply::Response(r) => r,
            Reply::Stream(_) => panic!("not a stream"),
        };
        let r = response(routes.handle(&Request {
            peer: Peer::Unix { pid: Some(1) },
            head: &head("GET", "/"),
            body: b"",
            cancel: &Cancel::default(),
        }));
        assert_eq!(r.status, 200);
        let v: Value = serde_json::from_slice(&r.body).expect("json");
        assert_eq!(v["schema"], ROOT_SCHEMA);
        assert_eq!(v["data"]["capabilities"], json!([CAPABILITY]));
        // `version --json` lists it too, so a client can tell (A17).
        let version = crate::version::data();
        assert!(
            version
                .capabilities
                .is_some_and(|c| c.iter().any(|c| c == CAPABILITY))
        );
        assert_eq!(v["data"]["serve_id"], "s1");
        let r = response(routes.handle(&Request {
            peer: Peer::Unix { pid: Some(1) },
            head: &head("POST", "/v1/leases"),
            body: b"{}",
            cancel: &Cancel::default(),
        }));
        assert_eq!(r.status, 501);
        let v: Value = serde_json::from_slice(&r.body).expect("json");
        assert_eq!(v["schema"], "mdkg.cli.v1");
        assert_eq!(v["ok"], false);
        assert_eq!(v["diagnostics"][0]["code"], "api.unsupported");
    }

    #[test]
    fn each_request_reads_fresh() {
        let routes = Routes {
            identity: Identity {
                serve_id: "s1".into(),
                graph_id: "g1".into(),
            },
            watch: super::super::events::Watch {
                binary: Arc::new(super::super::binary::Binary::fixed(std::path::Path::new(
                    "/nonexistent",
                ))),
                root: ".".into(),
            },
            ops: None,
        };
        let before = mdkg_git::fresh_reads_count();
        for _ in 0..2 {
            routes.handle(&Request {
                peer: Peer::Unix { pid: Some(1) },
                head: &head("GET", "/"),
                body: b"",
                cancel: &Cancel::default(),
            });
        }
        assert!(mdkg_git::fresh_reads_count() >= before + 2);
    }

    #[test]
    fn only_loopback_hosts_pass_and_the_token_compares_whole() {
        for ok in [
            "localhost",
            "LOCALHOST:7461",
            "127.0.0.1",
            "127.0.0.1:7461",
            "[::1]",
            "[::1]:7461",
        ] {
            assert!(loopback_host(ok), "{ok}");
        }
        for bad in [
            "evil.example",
            "localhost.evil.example",
            "127.0.0.1.evil.example",
            "127.0.0.2",
            "[::2]:1",
            "[::1]x",
            "localhost:",
            "localhost:123456",
            "localhost:7a",
            "",
        ] {
            assert!(!loopback_host(bad), "{bad}");
        }
        let t = Token::new("a".repeat(64));
        assert!(t.matches(&"a".repeat(64)));
        assert!(!t.matches(&"a".repeat(63)));
        assert!(!t.matches(&"a".repeat(65)));
        assert!(!t.matches(&format!("{}b", "a".repeat(63))));
        assert!(!t.matches(""));
    }

    /// Every file of this command, recursively.
    fn files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("read dir") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                files(&path, out);
            } else {
                out.push(path);
            }
        }
    }

    #[test]
    fn serve_reaches_no_networked_context() {
        // #10686 item 3, #8984, dec-01a18dd9: the local listener builds no
        // kernel context and no direct-host one, and reaches neither the
        // signature Gate nor enrolment, which stay unwired. The names are
        // assembled so this file doesn't hold them.
        let banned = [
            concat!("in_process", "_host_command"),
            concat!("Direct", "Host"),
            concat!("Ctx::", "local"),
            concat!("Origin::", "Local"),
            concat!("Verified", "::new"),
            concat!("auth::", "Gate"),
            concat!("enroll::", "Enroller"),
        ];
        let here = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/serve_cli");
        let mut all = vec![here.join("serve.rs")];
        files(&here.join("serve"), &mut all);
        assert!(all.len() >= 5, "the walk sees the command's files");
        for path in all {
            let text = std::fs::read_to_string(&path).expect("read");
            for name in banned {
                assert!(!text.contains(name), "{} names {name}", path.display());
            }
        }
    }
}
