use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::Mutex;

use mdkg_ops::registry::{Class, Keyed, Kind, Method, Person, Route};
use mdkg_ops::wire::MachineState;
use mdkg_wire::codec::HeadLimits;

use super::*;
use crate::auth::tests::{FakeKeys, FakeVerifier, GRAPH, Req, T0, fake_sign, key_of};
use crate::auth::{PersonCheck, PersonKey, PersonVerifier};
use crate::boot::Boot;
use crate::boot::test_clocks::ManualClocks;
use crate::enroll::{EnrollStore, Enrolment, NewPending, PendingRefused, StoreFailed};
use crate::limits::{Rate, RateLimiter};
use crate::nonce::NonceCache;
use crate::person_proof::{PersonPendingRow, PersonPendingStore, Proved};

// A test codec: just enough HTTP/1.1 to drive the listener with exact
// control of timing and framing. The tests at the end of this file run
// the same listener through wire's own `codec::Http1` (#10448).
struct TestCodec;

struct TestConn {
    io: Box<dyn Io>,
    buf: Vec<u8>,
    limits: HeadLimits,
}

impl Codec for TestCodec {
    fn connection(&self, io: Box<dyn Io>, limits: HeadLimits) -> Box<dyn HttpConnection> {
        Box::new(TestConn {
            io,
            buf: Vec::new(),
            limits,
        })
    }
}

fn read_error(e: io::Error) -> ReadError {
    match e.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => ReadError::TimedOut,
        _ => ReadError::Io(e),
    }
}

impl TestConn {
    fn fill(&mut self) -> Result<usize, ReadError> {
        let mut chunk = [0u8; 4096];
        let n = self.io.read(&mut chunk).map_err(read_error)?;
        self.buf.extend_from_slice(&chunk[..n]);
        Ok(n)
    }
}

impl HttpConnection for TestConn {
    fn read_head(
        &mut self,
        first_byte: Duration,
        whole: Duration,
    ) -> Result<Option<RequestHead>, ReadError> {
        self.io
            .set_read_timeout(Some(first_byte))
            .map_err(ReadError::Io)?;
        // `whole` runs from the first byte (Terra 2 #9922).
        let mut until = (!self.buf.is_empty()).then(|| std::time::Instant::now() + whole);
        let end = loop {
            if let Some(i) = self.buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break i;
            }
            if self.buf.len() > self.limits.request_line + self.limits.header_bytes {
                return Err(ReadError::TooLarge {
                    reason: "header_bytes",
                    head: self.buf.starts_with(b"HEAD "),
                });
            }
            let empty = self.buf.is_empty();
            if self.fill()? == 0 {
                return if empty {
                    Ok(None)
                } else {
                    Err(ReadError::Truncated)
                };
            }
            let until = *until.get_or_insert_with(|| std::time::Instant::now() + whole);
            let left = until.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return Err(ReadError::TimedOut);
            }
            self.io
                .set_read_timeout(Some(left))
                .map_err(ReadError::Io)?;
        };
        let text =
            String::from_utf8(self.buf[..end].to_vec()).map_err(|_| ReadError::Malformed {
                reason: "utf8",
                head: self.buf.starts_with(b"HEAD "),
            })?;
        self.buf.drain(..end + 4);
        let is_head = text.starts_with("HEAD ");
        let mut lines = text.split("\r\n");
        let line = lines.next().unwrap_or_default();
        let mut parts = line.split(' ');
        let (Some(method), Some(target), Some("HTTP/1.1"), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(ReadError::Malformed {
                reason: "request_line",
                head: is_head,
            });
        };
        let (path, query) = match target.split_once('?') {
            Some((p, q)) => (p.to_owned(), Some(q.to_owned())),
            None => (target.to_owned(), None),
        };
        let mut headers = Vec::new();
        for l in lines {
            let (n, v) = l.split_once(':').ok_or(ReadError::Malformed {
                reason: "header",
                head: is_head,
            })?;
            headers.push((n.to_ascii_lowercase(), v.trim().to_owned()));
        }
        if headers.len() > self.limits.header_count {
            return Err(ReadError::TooLarge {
                reason: "header_count",
                head: is_head,
            });
        }
        Ok(Some(RequestHead {
            method: method.into(),
            path,
            query,
            headers,
        }))
    }

    fn read_body(&mut self, len: u64, timeout: Duration) -> Result<Vec<u8>, ReadError> {
        self.io
            .set_read_timeout(Some(timeout))
            .map_err(ReadError::Io)?;
        let len = len as usize;
        while self.buf.len() < len {
            if self.fill()? == 0 {
                return Err(ReadError::Truncated);
            }
        }
        Ok(self.buf.drain(..len).collect())
    }

    fn write_response(&mut self, r: &Response, close: bool, timeout: Duration) -> io::Result<()> {
        // The deadline runs from the call's entry (Terra 3 #9949).
        let until = std::time::Instant::now() + timeout;
        let mut out = format!("HTTP/1.1 {} X\r\n", r.status);
        for (n, v) in &r.headers {
            out.push_str(&format!("{n}: {v}\r\n"));
        }
        out.push_str(&format!("Content-Length: {}\r\n", r.body.len()));
        if close {
            out.push_str("Connection: close\r\n");
        }
        out.push_str("\r\n");
        let mut bytes = out.into_bytes();
        bytes.extend_from_slice(&r.body);
        // Each write gets what is left.
        let mut sent = 0;
        while sent < bytes.len() {
            let end = bytes.len().min(sent + 4096);
            sent += self.io.write_by(&bytes[sent..end], until)?;
        }
        self.io.flush()
    }

    fn start_stream(&mut self, headers: &[(String, String)]) -> io::Result<()> {
        let mut out = "HTTP/1.1 200 X\r\nContent-Type: text/event-stream\r\n".to_owned();
        for (n, v) in headers {
            out.push_str(&format!("{n}: {v}\r\n"));
        }
        out.push_str("Connection: close\r\n\r\n");
        self.io.write_all(out.as_bytes())?;
        self.io.flush()
    }

    fn write_event(&mut self, id: Option<&str>, event: &str, data: &str) -> io::Result<()> {
        let mut out = String::new();
        if let Some(id) = id {
            out.push_str(&format!("id: {id}\n"));
        }
        out.push_str(&format!("event: {event}\ndata: {data}\n\n"));
        self.io.write_all(out.as_bytes())?;
        self.io.flush()
    }

    fn write_comment(&mut self, text: &str) -> io::Result<()> {
        self.io.write_all(format!(": {text}\n\n").as_bytes())?;
        self.io.flush()
    }

    fn closer(&self) -> io::Result<Box<dyn Closer>> {
        self.io.closer()
    }
}

/// A person key verifier over fake signatures: `pk-ann` verifies Present
/// (an attested FIDO2 key whose assertion has neither backup flag),
/// `pk-file` machine grade. Wire's real grading, BE and BS included, is
/// what `a_backed_up_assertion_can_t_approve_remotely` runs through.
struct Persons;

impl PersonVerifier for Persons {
    fn verify(&self, key_id: &str, _alg: &str, base: &[u8], signature: &[u8]) -> PersonCheck {
        let machine_grade = match key_id {
            "pk-ann" => false,
            "pk-file" => true,
            _ => return PersonCheck::Unknown,
        };
        if signature != fake_sign(&key_of(key_id), base).as_slice() {
            return PersonCheck::Invalid;
        }
        PersonCheck::Verified(PersonKey {
            key_id: key_id.into(),
            human: "human:ann".into(),
            machine_grade,
        })
    }
}

/// No pending enrolments, no machines: every lookup finds nothing.
struct NoEnrolments;

impl EnrollStore for NoEnrolments {
    fn request(
        &self,
        _: &NewPending,
        _: usize,
        _: u64,
    ) -> Result<Result<(), PendingRefused>, StoreFailed> {
        Ok(Ok(()))
    }
    fn lookup(&self, _: &str, _: u64) -> Result<Option<Enrolment>, StoreFailed> {
        Ok(None)
    }
    fn reveal(&self, _: &str, _: &[u8; 32], _: u64) -> Result<bool, StoreFailed> {
        Ok(false)
    }
}

struct NoPersons;

impl PersonPendingStore for NoPersons {
    fn prove(
        &self,
        _: &str,
        _: &str,
        _: &[u8; 32],
        _: &dyn Fn() -> u64,
        _: &mut dyn FnMut(&PersonPendingRow) -> bool,
    ) -> Result<Proved, StoreFailed> {
        Ok(Proved::NotPending)
    }
}

/// What the handlers were asked.
#[derive(Default)]
struct Calls {
    ops: Mutex<Vec<(String, Vec<String>, usize)>>,
    /// What each op and stream handler's `auth.verified()` gave: the
    /// kernel's remote context is built from it (#10811).
    verified: Mutex<Vec<Option<mdkg_ops::wire::Verified>>>,
    /// The fan-out `GET /v1/events` streams open on.
    fanout: Mutex<Option<Arc<crate::tail::Fanout>>>,
    /// Revokes every stream's machine.
    revoked: Arc<AtomicBool>,
    /// Lapses every stream's person grant, the machine still approved.
    person_lapsed: Arc<AtomicBool>,
    /// The stream handler's own refusal, when set.
    refuse_streams: AtomicBool,
    /// Frames the stream handler hands out instead of a subscription.
    custom_frames: Mutex<Option<Box<dyn crate::tail::Frames>>>,
    /// `block.op` waits for this; `slow.op` and `block.op` say they began.
    release: Arc<AtomicBool>,
    began: Arc<AtomicBool>,
}

impl Handlers for Calls {
    fn root(&self, auth: Option<&Authenticated>) -> Response {
        Response {
            status: 200,
            headers: vec![],
            body: match auth {
                None => b"root".to_vec(),
                Some(a) => format!("root+graph for {}", a.machine()).into_bytes(),
            },
        }
    }
    fn person_enroll(
        &self,
        _: &Authenticated,
        _: &RequestHead,
        _: &[u8],
    ) -> Result<Response, Refusal> {
        Ok(Response {
            status: 200,
            headers: vec![],
            body: b"enrolled".to_vec(),
        })
    }
    fn op(
        &self,
        auth: &Authenticated,
        op: &'static OpSpec,
        params: &[String],
        _: &RequestHead,
        body: &[u8],
    ) -> Result<Response, Refusal> {
        self.verified.lock().expect("lock").push(auth.verified());
        self.ops
            .lock()
            .expect("lock")
            .push((op.name.into(), params.to_vec(), body.len()));
        if op.name == "slow.op" || op.name == "block.op" {
            self.began.store(true, Ordering::SeqCst);
            let until = std::time::Instant::now()
                + if op.name == "slow.op" {
                    Duration::from_millis(400)
                } else {
                    Duration::from_secs(20)
                };
            while std::time::Instant::now() < until
                && !(op.name == "block.op" && self.release.load(Ordering::SeqCst))
            {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        let body = if op.name == "big.read" {
            vec![b'b'; 16 * 1024 * 1024]
        } else {
            op.name.as_bytes().to_vec()
        };
        Ok(Response {
            status: 200,
            headers: vec![],
            body,
        })
    }
    fn ephemeral(&self, _: Ephemeral) {}
    fn stream(
        &self,
        auth: &Authenticated,
        _: &'static OpSpec,
        _: &[String],
        _: &RequestHead,
    ) -> Result<StreamStart, Refusal> {
        self.verified.lock().expect("lock").push(auth.verified());
        if self.refuse_streams.load(Ordering::SeqCst) {
            return Err(Refusal::new(
                Code::CoordStoreFailed,
                ErrorKind::Failure,
                "corrupt_log",
            ));
        }
        if let Some(frames) = self.custom_frames.lock().expect("lock").take() {
            return Ok(StreamStart { frames, head: None });
        }
        let fanout = self
            .fanout
            .lock()
            .expect("lock")
            .clone()
            .expect("a fan-out");
        let revoked = self.revoked.clone();
        let lapsed = self.person_lapsed.clone();
        let grant: crate::tail::Grant = Arc::new(move || {
            Ok(if revoked.load(Ordering::SeqCst) {
                crate::tail::Granted::MachineRevoked
            } else if lapsed.load(Ordering::SeqCst) {
                crate::tail::Granted::PersonLapsed
            } else {
                crate::tail::Granted::Valid
            })
        });
        let sub = fanout.open(auth.machine(), Duration::from_millis(100), grant)?;
        Ok(StreamStart {
            frames: Box::new(sub),
            head: Some("s1:0".into()),
        })
    }
}

const fn op(name: &'static str, method: Method, path: &'static str, class: Class) -> OpSpec {
    OpSpec {
        name,
        kind: Kind::Read,
        capability: "x",
        cli: &["x"],
        mcp: &[],
        http: Some(Route { method, path }),
        input_schema: "s",
        output_schema: "s",
        keyed: Keyed::Accepted,
        class,
        person: Person::Signed,
        remote_plan: false,
    }
}

static OPS: &[OpSpec] = &[
    OpSpec {
        kind: Kind::Stream,
        ..op("events.stream", Method::Get, "/v1/events", Class::Any)
    },
    op("nodes.show", Method::Get, "/v1/nodes/{id}", Class::Any),
    op("turn.admit", Method::Post, "/v1/turns:admit", Class::Any),
    op("slow.op", Method::Get, "/v1/slow", Class::Any),
    op("block.op", Method::Get, "/v1/block", Class::Any),
    op("big.read", Method::Get, "/v1/big", Class::Any),
    op(
        "store.migrate",
        Method::Post,
        "/v1/store:migrate",
        Class::Host,
    ),
    op(
        PERSON_APPROVE,
        Method::Post,
        "/v1/persons/{id}:approve",
        Class::Operator,
    ),
];

struct World {
    service: Arc<Service>,
    clocks: Arc<ManualClocks>,
    caps: Arc<Caps>,
    calls: Arc<Calls>,
    boot: String,
}

fn world() -> World {
    world_with(Arc::new(FakeVerifier), Limits::default())
}

fn world_with(verifier: Arc<dyn crate::auth::Verifier>, limits: Limits) -> World {
    world_of(verifier, Arc::new(Persons), key_of("m-ok"), limits)
}

/// A world whose approved machine `m-ok` has `machine_key`, checked by
/// `verifier`, and whose person signatures `persons` checks.
fn world_of(
    verifier: Arc<dyn crate::auth::Verifier>,
    persons: Arc<dyn PersonVerifier>,
    machine_key: [u8; 32],
    limits: Limits,
) -> World {
    let clocks = Arc::new(ManualClocks::at(T0));
    let keys = Arc::new(FakeKeys::default());
    keys.0.lock().expect("lock").insert(
        "m-ok".into(),
        mdkg_ops::wire::MachineKey {
            machine: "m-ok".into(),
            public_key: machine_key,
            state: MachineState::Approved,
        },
    );
    let gate = Arc::new(Gate::new(
        GRAPH.into(),
        Boot::start(&*clocks).expect("boot"),
        clocks.clone(),
        NonceCache::default(),
        keys,
        verifier,
        persons,
        vec![],
        Arc::new(RateLimiter::new(Rate::default(), "m-ok".into())),
    ));
    let enroller = Arc::new(Enroller::new(
        gate.clone(),
        Arc::new(NoEnrolments),
        limits.clone(),
    ));
    let proofs = Arc::new(PersonProofs::new(GRAPH.into(), Arc::new(NoPersons)));
    let caps = Arc::new(Caps::default());
    let calls = Arc::new(Calls::default());
    let boot = gate.boot_id().expect("boot id");
    let service = Service::new(
        gate,
        Router::new(OPS),
        enroller,
        proofs,
        caps.clone(),
        limits,
        calls.clone(),
    );
    World {
        service,
        clocks,
        caps,
        calls,
        boot,
    }
}

/// A connection to the service over a socket pair, held as `on` would
/// hold it (a TCP one is pre-auth until authenticated), with the timer
/// watching it as the TCP listener does.
fn connect(w: &World, on: Listener) -> (UnixStream, std::thread::JoinHandle<()>) {
    connect_via(w, on, Arc::new(TestCodec))
}

/// [`connect`], framed by `codec`.
fn connect_via(
    w: &World,
    on: Listener,
    codec: Arc<dyn Codec>,
) -> (UnixStream, std::thread::JoinHandle<()>) {
    let (client, server) = UnixStream::pair().expect("pair");
    let conn = w
        .caps
        .connection(on, &w.service.limits, w.service.gate.observe_mono())
        .expect("a slot");
    let head_by = matches!(on, Listener::Tcp)
        .then(|| w.service.gate.observe_mono() + w.service.limits.preauth_header_read);
    let shared = Arc::new(Shared {
        conn: Mutex::new(Some(conn)),
        closer: Io::closer(&server).expect("closer"),
        preauth: Mutex::new(None),
        head_by: Mutex::new(head_by),
        writing_since: Mutex::new(None),
        streaming: AtomicBool::new(false),
        cut_requested: AtomicBool::new(false),
        busy: std::sync::atomic::AtomicU8::new(IDLE),
    });
    w.service.register(&shared);
    if matches!(on, Listener::Tcp) {
        w.service.timer.watch(&shared);
    }
    let service = w.service.clone();
    let t = std::thread::spawn(move || {
        let http = codec.connection(Box::new(server), service.limits.head());
        service.connection(http, &shared, &Peer::Unix);
    });
    client
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    (client, t)
}

/// Bytes on the wire for a head and body.
fn wire(head: &RequestHead, body: &[u8]) -> Vec<u8> {
    let target = match &head.query {
        Some(q) => format!("{}?{q}", head.path),
        None => head.path.clone(),
    };
    let mut out = format!("{} {target} HTTP/1.1\r\n", head.method);
    for (n, v) in &head.headers {
        out.push_str(&format!("{n}: {v}\r\n"));
    }
    out.push_str("\r\n");
    let mut out = out.into_bytes();
    out.extend_from_slice(body);
    out
}

struct Answer {
    status: u16,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

impl Answer {
    fn reason(&self) -> String {
        let v: serde_json::Value = serde_json::from_slice(&self.body).expect("json");
        v["data"]["reason"].as_str().unwrap_or_default().to_owned()
    }
    fn code(&self) -> String {
        let v: serde_json::Value = serde_json::from_slice(&self.body).expect("json");
        v["diagnostics"][0]["code"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }
    fn closes(&self) -> bool {
        self.headers.get("connection").map(String::as_str) == Some("close")
    }
}

/// The next response, or `None` at the end of the stream.
fn answer(r: &mut BufReader<UnixStream>) -> Option<Answer> {
    let mut line = String::new();
    if r.read_line(&mut line).ok()? == 0 {
        return None;
    }
    let status = line.split(' ').nth(1)?.parse().ok()?;
    let mut headers = HashMap::new();
    loop {
        let mut l = String::new();
        r.read_line(&mut l).ok()?;
        let l = l.trim_end();
        if l.is_empty() {
            break;
        }
        let (n, v) = l.split_once(": ")?;
        headers.insert(n.to_ascii_lowercase(), v.to_owned());
    }
    let len: usize = headers.get("content-length")?.parse().ok()?;
    let mut body = vec![0; len];
    r.read_exact(&mut body).ok()?;
    Some(Answer {
        status,
        headers,
        body,
    })
}

/// Whether the stream is at a true end: nothing buffered, and a read
/// returns zero bytes, never a timeout or more bytes (Terra 5 #9849,
/// Terra 2 #9894).
fn at_eof(r: &mut BufReader<UnixStream>) -> bool {
    if !r.buffer().is_empty() {
        return false;
    }
    let mut b = [0u8; 1];
    matches!(r.get_mut().read(&mut b), Ok(0))
}

fn send(c: &mut UnixStream, bytes: &[u8]) {
    c.write_all(bytes).expect("write");
}

/// One request on its own connection, and its answer. The write may be
/// cut short by a refusal that closes the connection before its body.
fn once(w: &World, on: Listener, head: &RequestHead, body: &[u8]) -> (Answer, bool) {
    let (mut c, t) = connect(w, on);
    let mut r = BufReader::new(c.try_clone().expect("clone"));
    let _ = c.write_all(&wire(head, body));
    let a = answer(&mut r).expect("answer");
    // After a close, anything more would be a second answer; on a kept
    // connection, the client's own close ends it.
    let more = a.closes() && !at_eof(&mut r);
    drop(c);
    drop(r);
    t.join().expect("join");
    (a, more)
}

#[test]
fn get_root_is_public_and_a_preauth_tcp_connection_closes_after_it() {
    let w = world();
    let (mut c, t) = connect(&w, Listener::Tcp);
    let head = RequestHead {
        method: "GET".into(),
        path: "/".into(),
        query: None,
        headers: vec![],
    };
    send(&mut c, &wire(&head, b""));
    let mut r = BufReader::new(c.try_clone().expect("clone"));
    let a = answer(&mut r).expect("answer");
    assert_eq!((a.status, a.body.as_slice()), (200, b"root".as_slice()));
    assert!(a.closes(), "no keep-alive before authentication");
    for h in ["mdkg-request-id", "mdkg-boot", "date"] {
        assert!(a.headers.contains_key(h), "{h}");
    }
    assert!(at_eof(&mut r));
    t.join().expect("join");
    assert_eq!(w.caps.connections(), (0, 0, 0), "slots given back");
}

#[test]
fn each_request_reads_fresh() {
    let w = world();
    let head = RequestHead {
        method: "GET".into(),
        path: "/".into(),
        query: None,
        headers: vec![],
    };
    let before = mdkg_git::fresh_reads_count();
    for _ in 0..2 {
        assert_eq!(once(&w, Listener::Unix, &head, b"").0.status, 200);
    }
    assert!(mdkg_git::fresh_reads_count() >= before + 2);
}

#[test]
fn a_signed_request_promotes_the_connection_which_then_keeps_alive() {
    let w = world();
    let (mut c, t) = connect(&w, Listener::Tcp);
    let mut r = BufReader::new(c.try_clone().expect("clone"));
    let first = Req::get("/v1/nodes/n%3A1", &w.boot).signed();
    send(&mut c, &wire(&first, b""));
    let a = answer(&mut r).expect("answer");
    assert_eq!(
        (a.status, a.body.as_slice()),
        (200, b"nodes.show".as_slice())
    );
    assert!(!a.closes());
    assert_eq!(w.caps.connections().2, 0, "the pre-auth slot is given back");
    let second = Req::get("/v1/nodes/n2", &w.boot).signed();
    send(&mut c, &wire(&second, b""));
    assert_eq!(answer(&mut r).expect("second").status, 200);
    assert_eq!(
        *w.calls.ops.lock().expect("lock"),
        vec![
            ("nodes.show".to_owned(), vec!["n:1".to_owned()], 0),
            ("nodes.show".to_owned(), vec!["n2".to_owned()], 0)
        ],
        "each capture decoded once"
    );
    drop(c);
    drop(r);
    t.join().expect("join");
}

#[test]
fn a_tampered_path_gets_the_authentication_error_not_the_route_s() {
    // R12, SC6, S4: a signed request's route refusal waits for its
    // signature.
    let w = world();
    let (mut c, t) = connect(&w, Listener::Unix);
    let mut r = BufReader::new(c.try_clone().expect("clone"));
    let mut head = Req::get("/v1/nodes/n1", &w.boot).signed();
    head.path = "/v1/nope".into();
    send(&mut c, &wire(&head, b""));
    let a = answer(&mut r).expect("answer");
    assert_eq!((a.status, a.code().as_str()), (401, "api.unauthenticated"));
    // Signed as sent, the same unknown route is the route's refusal.
    let head = Req::get("/v1/nope", &w.boot).signed();
    send(&mut c, &wire(&head, b""));
    let a = answer(&mut r).expect("answer");
    assert_eq!((a.status, a.reason().as_str()), (501, "route"));
    // So is a malformed escape, after the signature.
    let head = Req::get("/v1/nodes/n%zz", &w.boot).signed();
    send(&mut c, &wire(&head, b""));
    assert_eq!(answer(&mut r).expect("answer").reason(), "path_escape");
    drop(c);
    drop(r);
    t.join().expect("join");
}

#[test]
fn host_class_and_machine_only_approval_are_refused_on_every_listener() {
    // #8984, #8901, Terra 3 #9112: the host's own key included.
    let w = world();
    for on in [Listener::Unix, Listener::Tcp] {
        let (mut c, t) = connect(&w, on);
        let mut r = BufReader::new(c.try_clone().expect("clone"));
        let head = Req::post("/v1/store:migrate", &w.boot, b"").signed();
        send(&mut c, &wire(&head, b""));
        let a = answer(&mut r).expect("answer");
        assert_eq!((a.status, a.reason().as_str()), (501, "host_class"));
        drop(c);
        drop(r);
        t.join().expect("join");
    }
    // Each refusal before the body closes its connection (#9464), so
    // each request has its own.
    let body = b"{}";
    let head = Req::post("/v1/persons/p1:approve", &w.boot, body)
        .header("content-type", "application/json")
        .signed();
    let (a, _) = once(&w, Listener::Unix, &head, body);
    assert_eq!(
        (a.code().as_str(), a.reason().as_str()),
        ("api.person_signature_required", "person_signature")
    );
    assert!(a.closes(), "refused with its body unread");
    let head = Req::post("/v1/persons/p1:approve", &w.boot, body)
        .header("content-type", "application/json")
        .header("mdkg-actor", "human:ann")
        .person_signed("pk-file", key_of("pk-file"));
    assert_eq!(
        once(&w, Listener::Unix, &head, body).0.reason(),
        "present_required"
    );
    let head = Req::post("/v1/persons/p1:approve", &w.boot, body)
        .header("content-type", "application/json")
        .header("mdkg-actor", "human:ann")
        .person_signed("pk-ann", key_of("pk-ann"));
    let (a, _) = once(&w, Listener::Unix, &head, body);
    assert_eq!(
        (a.status, a.body.as_slice()),
        (200, PERSON_APPROVE.as_bytes())
    );
    assert!(!a.closes());
}

#[test]
fn each_route_s_body_cap_refuses_unread_and_closes() {
    // S21, SC25, Terra 4 #9457: one byte over the route's own cap is
    // refused before a body byte is read, and the connection closes, so
    // the body's bytes are never answered as a request.
    let w = world();
    let smuggled = b"GET / HTTP/1.1\r\n\r\n";
    for (method, path, cap) in [
        ("POST", "/v1/persons/p1:approve", 128 * 1024),
        ("POST", "/persons:enroll", 32 * 1024),
        (
            "POST",
            "/persons/01a18800-0000-7000-8000-000000000003:prove",
            32 * 1024,
        ),
        ("POST", "/enroll", 4 * 1024),
    ] {
        let head = RequestHead {
            method: method.into(),
            path: path.into(),
            query: None,
            headers: vec![("content-length".into(), (cap + 1).to_string())],
        };
        let mut body = smuggled.to_vec();
        body.extend(std::iter::repeat_n(b'x', 64));
        let (a, more) = once(&w, Listener::Unix, &head, &body);
        assert_eq!(
            (a.status, a.code().as_str()),
            (413, "api.too_large"),
            "{path}"
        );
        assert!(a.closes(), "{path}");
        assert!(!more, "{path}: the body was not a request");
    }
    // Under the cap, the approval's body is read.
    let (mut c, t) = connect(&w, Listener::Unix);
    let mut r = BufReader::new(c.try_clone().expect("clone"));
    let body = vec![b'r'; 100 * 1024];
    let head = Req::post("/v1/persons/p1:approve", &w.boot, &body)
        .header("content-type", "application/json")
        .header("mdkg-actor", "human:ann")
        .person_signed("pk-ann", key_of("pk-ann"));
    send(&mut c, &wire(&head, &body));
    assert_eq!(answer(&mut r).expect("answer").status, 200);
    assert_eq!(
        w.calls.ops.lock().expect("lock").last().map(|o| o.2),
        Some(100 * 1024)
    );
    drop(c);
    drop(r);
    t.join().expect("join");
}

#[test]
fn an_unread_body_closes_an_authenticated_keep_alive_connection() {
    // Terra 5 #9589: on a connection already authenticated and kept
    // alive, where the pre-auth rule can't be what closes it, a refused
    // body that holds a complete signed request is never answered.
    let w = world();
    for on in [Listener::Unix, Listener::Tcp] {
        let (mut c, t) = connect(&w, on);
        let mut r = BufReader::new(c.try_clone().expect("clone"));
        send(
            &mut c,
            &wire(&Req::get("/v1/nodes/n1", &w.boot).signed(), b""),
        );
        let a = answer(&mut r).expect("first");
        assert_eq!(a.status, 200);
        assert!(!a.closes(), "kept alive once authenticated");
        let inner = wire(&Req::get("/v1/nodes/n2", &w.boot).signed(), b"");
        let head = RequestHead {
            method: "POST".into(),
            path: "/persons:enroll".into(),
            query: None,
            headers: vec![("content-length".into(), (32 * 1024 + 1).to_string())],
        };
        let _ = c.write_all(&wire(&head, &inner));
        let a = answer(&mut r).expect("refusal");
        assert_eq!(a.status, 413);
        assert!(a.closes());
        assert!(at_eof(&mut r), "the inner request was not served");
        t.join().expect("join");
    }
    assert_eq!(
        w.calls.ops.lock().expect("lock").len(),
        2,
        "only the two first requests"
    );
}

/// A verifier that holds every verification until released, and says
/// when one has begun.
struct Stalled {
    entered: Mutex<bool>,
    release: Mutex<bool>,
    wake: Condvar,
}

impl crate::auth::Verifier for Stalled {
    fn verify(&self, public_key: &[u8; 32], base: &[u8], signature: &[u8; 64]) -> bool {
        *self.entered.lock().expect("lock") = true;
        self.wake.notify_all();
        let mut go = self.release.lock().expect("lock");
        while !*go {
            go = self.wake.wait(go).expect("wait");
        }
        FakeVerifier.verify(public_key, base, signature)
    }
}

#[test]
fn the_timer_cuts_a_connection_whose_verification_stalls() {
    // S21, orchestrator #7678: the slots go back at the deadline while
    // the verification is still running, and the request is never
    // answered once it ends.
    let stalled = Arc::new(Stalled {
        entered: Mutex::new(false),
        release: Mutex::new(false),
        wake: Condvar::new(),
    });
    let w = world_with(stalled.clone(), Limits::default());
    let (mut c, t) = connect(&w, Listener::Tcp);
    send(
        &mut c,
        &wire(&Req::get("/v1/nodes/n1", &w.boot).signed(), b""),
    );
    // Wait until the request is in verification, then pass the deadline.
    let until = std::time::Instant::now() + Duration::from_secs(5);
    while !*stalled.entered.lock().expect("lock") {
        assert!(
            std::time::Instant::now() < until,
            "verification never began"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    w.clocks.advance(10_001);
    let mut r = BufReader::new(c);
    assert!(at_eof(&mut r), "closed without an answer");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while w.caps.connections() != (0, 0, 0) {
        assert!(
            std::time::Instant::now() < deadline,
            "{:?}",
            w.caps.connections()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    *stalled.release.lock().expect("lock") = true;
    stalled.wake.notify_all();
    t.join().expect("join");
    assert!(w.calls.ops.lock().expect("lock").is_empty(), "never served");
    assert_eq!(w.caps.connections(), (0, 0, 0));
}

#[test]
fn head_is_a_bodiless_405_and_closes() {
    // Orchestrator #9774: before routing or authentication, `Allow`
    // naming the path's methods when it is served, length 0, no body.
    let w = world();
    for (path, allow) in [
        ("/v1/nodes/n1", Some("GET")),
        ("/v1/persons/p1:approve", Some("POST")),
        ("/", Some("GET")),
        ("/v1/nope", None),
    ] {
        let (mut c, t) = connect(&w, Listener::Unix);
        let mut r = BufReader::new(c.try_clone().expect("clone"));
        // Signed for GET, sent as HEAD: never authenticated either way.
        let mut head = Req::get(path, &w.boot).signed();
        head.method = "HEAD".into();
        send(&mut c, &wire(&head, b""));
        let a = answer(&mut r).expect("answer");
        assert_eq!(a.status, 405, "{path}");
        assert_eq!(a.headers.get("allow").map(String::as_str), allow, "{path}");
        assert_eq!(
            a.headers.get("content-length").map(String::as_str),
            Some("0")
        );
        assert!(a.body.is_empty() && a.closes(), "{path}");
        assert!(at_eof(&mut r), "{path}: closed");
        t.join().expect("join");
    }
    assert!(w.calls.ops.lock().expect("lock").is_empty());
}

#[test]
fn a_peer_that_stops_reading_is_cut_at_the_write_deadline() {
    // Claude #9754: a response the peer never reads can't pin the slot
    // and the thread.
    let limits = Limits {
        write: Duration::from_millis(200),
        ..Limits::default()
    };
    let w = world_with(Arc::new(FakeVerifier), limits);
    let (mut c, t) = connect(&w, Listener::Unix);
    send(&mut c, &wire(&Req::get("/v1/big", &w.boot).signed(), b""));
    // A timed wait, so a deadline that regressed fails here instead of
    // hanging the suite (Terra 2 #9894).
    let (done, finished) = std::sync::mpsc::channel();
    std::thread::spawn(move || done.send(t.join().is_ok()));
    let joined = finished.recv_timeout(Duration::from_secs(5));
    drop(c);
    assert_eq!(joined, Ok(true), "cut at the write deadline");
    assert_eq!(w.caps.connections(), (0, 0, 0), "the slot came back");
}

#[test]
fn a_peer_that_reads_a_little_at_a_time_is_cut_at_the_whole_response_s_deadline() {
    // Claude #9843, Terra 2 #9894: the deadline is the response's, from
    // the call, not each write's, so a slow reader can't stretch it.
    let limits = Limits {
        write: Duration::from_millis(300),
        ..Limits::default()
    };
    let w = world_with(Arc::new(FakeVerifier), limits);
    let (mut c, t) = connect(&w, Listener::Unix);
    send(&mut c, &wire(&Req::get("/v1/big", &w.boot).signed(), b""));
    let started = std::time::Instant::now();
    let mut got = 0usize;
    // 4 KiB every 20 ms: each write progresses, which a per-write timeout
    // would never end, at a rate that would take over a minute for all of
    // it.
    let mut buf = [0u8; 4096];
    let ended = loop {
        match c.read(&mut buf) {
            Ok(0) => break Ok(()),
            Ok(n) => got += n,
            Err(e) => break Err(e),
        }
        assert!(started.elapsed() < Duration::from_secs(5), "never cut");
        // Well inside any per-write timeout.
        std::thread::sleep(Duration::from_millis(20));
    };
    // A real close, after real progress, never a read timeout (Terra 3
    // #9949).
    let reset = |e: &io::Error| e.kind() == io::ErrorKind::ConnectionReset;
    assert!(ended.as_ref().map_or_else(reset, |()| true), "{ended:?}");
    assert!(got > 4096, "the response was under way: {got}");
    assert!(got < 16 * 1024 * 1024, "cut before the whole response");
    let (done, finished) = std::sync::mpsc::channel();
    std::thread::spawn(move || done.send(t.join().is_ok()));
    let joined = finished.recv_timeout(Duration::from_secs(5));
    drop(c);
    assert_eq!(joined, Ok(true));
    assert_eq!(w.caps.connections(), (0, 0, 0));
}

#[test]
fn a_head_whose_head_can_t_be_read_is_refused_without_a_body() {
    // Terra 2 #9894: 65 headers on a HEAD is a 413 with no body.
    let w = world();
    let (mut c, t) = connect(&w, Listener::Unix);
    let mut r = BufReader::new(c.try_clone().expect("clone"));
    let mut bytes = b"HEAD / HTTP/1.1\r\n".to_vec();
    for i in 0..65 {
        bytes.extend_from_slice(format!("x-{i}: y\r\n").as_bytes());
    }
    bytes.extend_from_slice(b"\r\n");
    send(&mut c, &bytes);
    let a = answer(&mut r).expect("answer");
    assert_eq!(a.status, 413);
    assert_eq!(
        a.headers.get("content-length").map(String::as_str),
        Some("0")
    );
    assert!(a.body.is_empty() && a.closes());
    assert!(at_eof(&mut r));
    t.join().expect("join");
}

#[test]
fn a_request_id_is_a_uuid_v7_when_randomness_fails() {
    let w = world();
    let a = w.service.request_id(Err(io::Error::other("no entropy")));
    let b = w.service.request_id(Err(io::Error::other("no entropy")));
    assert_ne!(a, b);
    for id in [&a, &b] {
        assert_eq!(id.len(), 36, "{id}");
        assert_eq!(&id[14..15], "7", "{id}");
        assert!("89ab".contains(&id[19..20]), "the variant: {id}");
    }
}

#[test]
fn a_chunked_body_is_refused_unread_and_the_connection_closes() {
    let w = world();
    for extra in [vec![], vec![("content-length".to_owned(), "5".to_owned())]] {
        let (mut c, t) = connect(&w, Listener::Unix);
        let mut r = BufReader::new(c.try_clone().expect("clone"));
        let mut headers = vec![("transfer-encoding".to_owned(), "chunked".to_owned())];
        headers.extend(extra);
        let head = RequestHead {
            method: "POST".into(),
            path: "/persons:enroll".into(),
            query: None,
            headers,
        };
        send(&mut c, &wire(&head, b"5\r\nGET /\r\n0\r\n\r\n"));
        let a = answer(&mut r).expect("answer");
        assert_eq!((a.status, a.reason().as_str()), (400, "body_length"));
        assert!(a.closes());
        assert!(at_eof(&mut r));
        t.join().expect("join");
    }
}

#[test]
fn the_timer_closes_a_silent_preauth_connection_and_frees_its_slots() {
    // S21, #7678: the listener's own timer, not the read, ends it.
    let w = world();
    let (c, t) = connect(&w, Listener::Tcp);
    assert_eq!(w.caps.connections(), (0, 1, 1));
    w.clocks.advance(10_001);
    let mut r = BufReader::new(c);
    assert!(at_eof(&mut r), "closed without an answer");
    t.join().expect("join");
    assert_eq!(w.caps.connections(), (0, 0, 0));
}

#[test]
fn a_head_trickled_past_the_5_s_budget_is_cut_at_5_s() {
    // Orchestrator #10194: its first byte at 4 s, the rest trickled; the
    // timer closes it at accept plus 5 s, well before the 10 s cap.
    let w = world();
    let (mut c, t) = connect(&w, Listener::Tcp);
    w.clocks.advance(4_000);
    send(&mut c, b"GET / HT");
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(w.caps.connections(), (0, 1, 1), "still open at 4 s");
    w.clocks.advance(1_001);
    let _ = c.write_all(b"TP/1.1\r\n");
    // Only the timer can close it this soon: the codec's own deadline is
    // seconds of real time away (Terra 4 #10375).
    c.set_read_timeout(Some(Duration::from_millis(1500)))
        .expect("timeout");
    let started = std::time::Instant::now();
    let mut r = BufReader::new(c);
    assert!(at_eof(&mut r), "closed without an answer");
    assert!(started.elapsed() < Duration::from_millis(1500));
    t.join().expect("join");
    assert_eq!(w.caps.connections(), (0, 0, 0));
}

#[test]
fn run_tcp_gives_a_new_connection_its_5_s_head_budget() {
    // Terra 4 #10375: the budget run_tcp sets, not the test helper's.
    let w = world();
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let stop = Arc::new(AtomicBool::new(false));
    let (service, stop2) = (w.service.clone(), stop.clone());
    let t = std::thread::spawn(move || {
        service.run_tcp(listener, Arc::new(NoTls), Arc::new(TestCodec), stop2)
    });
    let mut c = TcpStream::connect(addr).expect("connect");
    let until = std::time::Instant::now() + Duration::from_secs(5);
    while w.caps.connections().1 == 0 {
        assert!(std::time::Instant::now() < until, "never accepted");
        std::thread::sleep(Duration::from_millis(5));
    }
    c.write_all(b"GET / HT").expect("write");
    std::thread::sleep(Duration::from_millis(100));
    w.clocks.advance(5_001);
    assert_closed_unanswered(&mut c);
    stop.store(true, Ordering::SeqCst);
    t.join().expect("join").expect("ran");
}

#[test]
fn write_by_returns_by_its_deadline_against_a_slow_reader() {
    // Terra 1 #10372: nonblocking writes, so the kernel can't run a send
    // on past the deadline while the peer drains a little at a time.
    let (mut writer, mut reader) = UnixStream::pair().expect("pair");
    let stop = Arc::new(AtomicBool::new(false));
    let s2 = stop.clone();
    let slow = std::thread::spawn(move || {
        let mut b = [0u8; 64];
        while !s2.load(Ordering::SeqCst) {
            let _ = reader.read(&mut b);
            std::thread::sleep(Duration::from_millis(10));
        }
    });
    let big = vec![b'x'; 8 * 1024 * 1024];
    let started = std::time::Instant::now();
    let deadline = started + Duration::from_millis(300);
    let mut sent = 0;
    while sent < big.len() {
        match Io::write_by(&mut writer, &big[sent..], deadline) {
            Ok(n) => sent += n,
            Err(e) => {
                assert_eq!(e.kind(), io::ErrorKind::TimedOut);
                break;
            }
        }
    }
    assert!(
        started.elapsed() < Duration::from_millis(600),
        "{:?}",
        started.elapsed()
    );
    assert!(sent < big.len());
    stop.store(true, Ordering::SeqCst);
    drop(writer);
    slow.join().expect("join");
}

#[test]
fn a_head_whole_within_5_s_is_served_after_it() {
    let w = world();
    let (mut c, t) = connect(&w, Listener::Tcp);
    w.clocks.advance(4_900);
    send(&mut c, b"GET / HTTP/1.1\r\n\r\n");
    let mut r = BufReader::new(c.try_clone().expect("clone"));
    assert_eq!(answer(&mut r).expect("answer").status, 200);
    drop(c);
    drop(r);
    t.join().expect("join");
}

#[test]
fn the_unix_listener_serves_its_own_user_until_stopped() {
    let w = world();
    let dir = std::env::temp_dir().join(format!(
        "mdkg-listen-{}-{}",
        std::process::id(),
        crate::random::hex(&crate::random::bytes::<4>().expect("urandom"))
    ));
    std::fs::create_dir_all(&dir).expect("dir");
    let path = dir.join("s");
    let listener = UnixListener::bind(&path).expect("bind");
    let stop = Arc::new(AtomicBool::new(false));
    let (service, stop2) = (w.service.clone(), stop.clone());
    let t = std::thread::spawn(move || service.run_unix(listener, Arc::new(TestCodec), stop2));
    let mut c = UnixStream::connect(&path).expect("connect");
    c.set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    send(&mut c, b"GET / HTTP/1.1\r\n\r\n");
    let mut r = BufReader::new(c.try_clone().expect("clone"));
    assert_eq!(answer(&mut r).expect("answer").status, 200);
    stop.store(true, Ordering::SeqCst);
    t.join().expect("join").expect("ran");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_signed_root_is_authenticated_and_sees_the_graph() {
    // S4, Terra 5 #9795: unsigned, the public fields; signed, the graph
    // too; signed and tampered, the authentication error.
    let w = world();
    let unsigned = RequestHead {
        method: "GET".into(),
        path: "/".into(),
        query: None,
        headers: vec![],
    };
    let (a, _) = once(&w, Listener::Unix, &unsigned, b"");
    assert_eq!(a.body, b"root");
    let (a, _) = once(&w, Listener::Unix, &Req::get("/", &w.boot).signed(), b"");
    assert_eq!(
        (a.status, a.body.as_slice()),
        (200, b"root+graph for m-ok".as_slice())
    );
    let mut tampered = Req::get("/v1/nodes/n1", &w.boot).signed();
    tampered.path = "/".into();
    let (a, _) = once(&w, Listener::Unix, &tampered, b"");
    assert_eq!((a.status, a.code().as_str()), (401, "api.unauthenticated"));
}

#[test]
fn a_body_that_isn_t_json_is_415_unread() {
    // S4: `Content-Type: application/json` for every body.
    let w = world();
    let body = b"{}";
    for ct in [None, Some("text/plain"), Some("application/jsonx")] {
        let mut req =
            Req::post("/v1/persons/p1:approve", &w.boot, body).header("mdkg-actor", "human:ann");
        if let Some(ct) = ct {
            req = req.header("content-type", ct);
        }
        let head = req.person_signed("pk-ann", key_of("pk-ann"));
        let (a, more) = once(&w, Listener::Unix, &head, body);
        assert_eq!(
            (a.status, a.reason().as_str()),
            (415, "content_type"),
            "{ct:?}"
        );
        assert!(a.closes() && !more, "{ct:?}");
    }
    let head = Req::post("/v1/persons/p1:approve", &w.boot, body)
        .header("content-type", "application/json; charset=utf-8")
        .header("mdkg-actor", "human:ann")
        .person_signed("pk-ann", key_of("pk-ann"));
    assert_eq!(once(&w, Listener::Unix, &head, body).0.status, 200);
    assert_eq!(w.calls.ops.lock().expect("lock").len(), 1);
}

#[test]
fn an_enrolment_s_origin_is_refused_on_its_head() {
    // Terra 5 #9795: before the source's rate or the body.
    let w = world();
    let head = RequestHead {
        method: "POST".into(),
        path: "/enroll".into(),
        query: None,
        headers: vec![
            ("origin".into(), "https://evil.example".into()),
            ("content-type".into(), "application/json".into()),
            ("content-length".into(), "100".into()),
        ],
    };
    // The body is never sent: the refusal can't wait for it.
    let (a, more) = once(&w, Listener::Tcp, &head, b"");
    assert_eq!(a.code(), "api.origin_refused");
    assert!(a.closes() && !more);
}

/// Person keys for wire's real verifier: their records, and what each
/// enrolment recorded about its attestation.
struct PersonKeys(HashMap<String, (mdkg_ops::wire::PersonKey, mdkg_wire::person::Attestation)>);

impl mdkg_ops::wire::KeyDirectory for PersonKeys {
    fn machine(&self, _: &str) -> Option<mdkg_ops::wire::MachineKey> {
        None
    }
    fn person(&self, key_id: &str) -> Option<mdkg_ops::wire::PersonKey> {
        self.0.get(key_id).map(|(k, _)| k.clone())
    }
}

impl mdkg_wire::person::PersonAttestations for PersonKeys {
    fn attestation(&self, key_id: &str) -> mdkg_wire::person::Attestation {
        self.0
            .get(key_id)
            .map_or(mdkg_wire::person::Attestation::None, |(_, a)| *a)
    }
}

/// A FIDO2 Ed25519 authenticator: it asserts for mdkg's relying party
/// with the authenticator-data `flags` given.
struct Fido2 {
    key: mdkg_wire::keys::MachineKey,
    flags: u8,
}

impl mdkg_ops::wire::Signer for Fido2 {
    fn key_id(&self) -> &str {
        mdkg_ops::wire::Signer::key_id(&self.key)
    }
    fn alg(&self) -> &'static str {
        mdkg_wire::sign::ALG_FIDO2_ED25519
    }
    fn sign(&self, base: &[u8]) -> Result<mdkg_ops::wire::Signature, Box<mdkg_format::Diagnostic>> {
        use sha2::Digest;
        let mut auth_data = sha2::Sha256::digest(mdkg_wire::person::RP_ID.as_bytes()).to_vec();
        auth_data.push(self.flags);
        auth_data.extend_from_slice(&7u32.to_be_bytes());
        let msg = mdkg_wire::person::assertion_message(&auth_data, base);
        let sig = mdkg_ops::wire::Signer::sign(&self.key, &msg)?;
        Ok(mdkg_wire::person::encode_assertion(&auth_data, &sig.0).expect("an assertion"))
    }
}

#[test]
fn a_backed_up_assertion_can_t_approve_remotely() {
    // Orchestrator #9808, through wire's real PersonVerifier (#10676,
    // #10681 item 4): a FIDO2 key enrolled with a verified attestation
    // approves remotely while its assertion sets neither backup flag. One
    // setting BE or BS grades machine grade, whatever enrolment
    // recorded, so the approval is refused. Wire's client signs, and the
    // Gate checks with wire's strict Ed25519.
    const UP: u8 = 0x01;
    const BE: u8 = 0x08;
    const BS: u8 = 0x10;
    let machine = mdkg_wire::keys::MachineKey::from_parts("m-ok", [5; 32]);
    let authenticator = mdkg_wire::keys::MachineKey::from_parts("pk-ann", [6; 32]);
    let persons = PersonKeys(HashMap::from([(
        "pk-ann".to_owned(),
        (
            mdkg_ops::wire::PersonKey {
                key_id: "pk-ann".into(),
                person: "human:ann".into(),
                kind: mdkg_ops::wire::PersonKeyKind::Fido2,
                public_key: authenticator.public_key().to_vec(),
                user_verified: false,
                revoked: false,
            },
            mdkg_wire::person::Attestation::Verified,
        ),
    )]));
    let w = world_of(
        Arc::new(mdkg_wire::keys::Ed25519),
        Arc::new(mdkg_wire::person::PersonVerifier::new(persons)),
        machine.public_key(),
        Limits::default(),
    );
    let approve = |flags: u8, nonce: u8| {
        let mut req = mdkg_wire::sign::Outgoing::new("POST", "/v1/persons/p1:approve", None);
        for (n, v) in [
            ("Mdkg-Graph", GRAPH),
            ("Mdkg-Boot", w.boot.as_str()),
            ("Mdkg-Actor", "human:ann"),
            ("Content-Type", "application/json"),
        ] {
            req.header(n, v).expect("header");
        }
        // A key of its own, so no answer can be another request's replay
        // whatever the order of the checks (Terra 1 #10784).
        req.header(
            "Idempotency-Key",
            format!("01a188cc-0000-7000-8000-0000000000{nonce:02x}"),
        )
        .expect("key");
        req.body(b"{}".to_vec()).expect("body");
        mdkg_wire::sign::sign_machine(&mut req, &machine, T0 / 1000, [nonce; 16]).expect("machine");
        let key = mdkg_wire::keys::MachineKey::from_parts("pk-ann", [6; 32]);
        mdkg_wire::sign::sign_person(&mut req, &Fido2 { key, flags }, T0 / 1000).expect("person");
        let mut headers: Vec<(String, String)> = req
            .headers
            .iter()
            .map(|(n, v)| (n.to_ascii_lowercase(), v.clone()))
            .collect();
        headers.push(("content-length".into(), req.body.len().to_string()));
        let head = RequestHead {
            method: req.method.clone(),
            path: req.path.clone(),
            query: None,
            headers,
        };
        once(&w, Listener::Unix, &head, &req.body).0
    };
    let a = approve(UP, 1);
    assert_eq!(a.status, 200, "neither flag: Present, so approved");
    assert_eq!(w.calls.ops.lock().expect("lock").len(), 1);
    for (flags, nonce) in [(UP | BE, 2), (UP | BS, 3), (UP | BE | BS, 4)] {
        let a = approve(flags, nonce);
        assert_eq!(
            (a.code().as_str(), a.reason().as_str()),
            ("api.person_signature_required", "present_required"),
            "flags {flags:#x}"
        );
    }
    assert_eq!(w.calls.ops.lock().expect("lock").len(), 1, "none approved");
}

#[test]
fn a_kept_connection_waits_the_idle_limit_for_its_next_request() {
    // Terra 5 #9795: the socket's connections, and TCP's once
    // authenticated, wait 60 s for a first byte; a pre-auth one, 5 s.
    let w = world();
    let l = &w.service.limits;
    let now = w.service.gate.observe_mono();
    let unix = w.caps.connection(Listener::Unix, l, now).expect("unix");
    assert_eq!(read_deadlines(&unix, l, now), (l.idle, l.header_read));
    let tcp = w.caps.connection(Listener::Tcp, l, now).expect("tcp");
    assert_eq!(
        read_deadlines(&tcp, l, now),
        (l.preauth_header_read, l.preauth_header_read)
    );
}

#[test]
fn request_ids_are_uuid_v7() {
    let id = uuid_v7(0x0190_0000_0001, [0xff; 10]);
    assert_eq!(id, "01900000-0001-7fff-bfff-ffffffffffff");
    let id = uuid_v7(1_793_329_718_000, [0; 10]);
    assert_eq!(&id[14..15], "7");
    assert_eq!(&id[19..20], "8");
}

/// TLS as a pass-through, for the TCP accept loop.
struct NoTls;

impl TlsAcceptor for NoTls {
    fn accept(&self, tcp: TcpStream, _: Duration) -> io::Result<Box<dyn Io>> {
        Ok(Box::new(tcp))
    }
}

#[test]
fn a_stalled_connection_s_thread_holds_its_permit_after_its_slots_go_back() {
    // Terra 5 #9795: the timer gives back a stalled connection's slots,
    // but its thread still counts against the pool until it exits, so
    // stalls can't pile up threads past the caps.
    let stalled = Arc::new(Stalled {
        entered: Mutex::new(false),
        release: Mutex::new(false),
        wake: Condvar::new(),
    });
    let limits = Limits {
        connections: 1,
        tcp_preauth: 1,
        ..Limits::default()
    };
    let w = world_with(stalled.clone(), limits);
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let stop = Arc::new(AtomicBool::new(false));
    let (service, stop2) = (w.service.clone(), stop.clone());
    let t = std::thread::spawn(move || {
        service.run_tcp(listener, Arc::new(NoTls), Arc::new(TestCodec), stop2)
    });
    let dial = || {
        let c = TcpStream::connect(addr).expect("connect");
        c.set_read_timeout(Some(Duration::from_secs(10)))
            .expect("timeout");
        c
    };
    let mut a = dial();
    a.write_all(&wire(&Req::get("/v1/nodes/n1", &w.boot).signed(), b""))
        .expect("write");
    let until = std::time::Instant::now() + Duration::from_secs(5);
    while !*stalled.entered.lock().expect("lock") {
        assert!(std::time::Instant::now() < until, "never verified");
        std::thread::sleep(Duration::from_millis(5));
    }
    w.clocks.advance(10_001);
    while w.caps.connections() != (0, 0, 0) {
        assert!(
            std::time::Instant::now() < until,
            "{:?}",
            w.caps.connections()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        w.service.tcp_workers.held(),
        1,
        "the stalled thread's permit"
    );
    // Its slots are back, but the pool of threads is full: refused.
    let mut b = dial();
    let _ = b.write_all(b"GET / HTTP/1.1\r\n\r\n");
    assert_closed_unanswered(&mut b);
    *stalled.release.lock().expect("lock") = true;
    stalled.wake.notify_all();
    while w.service.tcp_workers.held() != 0 {
        assert!(std::time::Instant::now() < until + Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(5));
    }
    let mut c = dial();
    c.write_all(b"GET / HTTP/1.1\r\n\r\n").expect("write");
    let mut r = BufReader::new(c);
    let mut line = String::new();
    r.read_line(&mut line).expect("answer");
    assert!(line.starts_with("HTTP/1.1 200"), "{line}");
    drop(a);
    stop.store(true, Ordering::SeqCst);
    t.join().expect("join").expect("ran");
}

/// The peer closed the connection without answering, promptly: a zero
/// read or a reset, never a read timeout (Terra 2 #9922).
fn assert_closed_unanswered(c: &mut TcpStream) {
    c.set_read_timeout(Some(Duration::from_secs(5)))
        .expect("timeout");
    let started = std::time::Instant::now();
    let mut buf = [0u8; 1];
    match c.read(&mut buf) {
        Ok(0) => {}
        Err(e) if e.kind() == io::ErrorKind::ConnectionReset => {}
        other => panic!("not closed unanswered: {other:?}"),
    }
    assert!(started.elapsed() < Duration::from_secs(2));
}

/// A verifier that holds every bad signature until released: a valid one
/// verifies at once.
struct StallsBad {
    held: Mutex<usize>,
    release: Mutex<bool>,
    wake: Condvar,
}

impl crate::auth::Verifier for StallsBad {
    fn verify(&self, public_key: &[u8; 32], base: &[u8], signature: &[u8; 64]) -> bool {
        if FakeVerifier.verify(public_key, base, signature) {
            return true;
        }
        *self.held.lock().expect("lock") += 1;
        let mut go = self.release.lock().expect("lock");
        while !*go {
            go = self.wake.wait(go).expect("wait");
        }
        false
    }
}

#[test]
fn stalled_preauth_work_leaves_authenticated_work_its_share_of_the_pool() {
    // Orchestrator #9931, Terra 2 #9922: four waves of stalled
    // verifications fill only the pre-auth workers, and an approved
    // client's kept connection is still served.
    let stalls = Arc::new(StallsBad {
        held: Mutex::new(0),
        release: Mutex::new(false),
        wake: Condvar::new(),
    });
    let limits = Limits {
        connections: 4,
        tcp_preauth: 2,
        ..Limits::default()
    };
    let w = world_with(stalls.clone(), limits);
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let stop = Arc::new(AtomicBool::new(false));
    let (service, stop2) = (w.service.clone(), stop.clone());
    let t = std::thread::spawn(move || {
        service.run_tcp(listener, Arc::new(NoTls), Arc::new(TestCodec), stop2)
    });
    let dial = || {
        let c = TcpStream::connect(addr).expect("connect");
        c.set_read_timeout(Some(Duration::from_secs(10)))
            .expect("timeout");
        c
    };
    let approved = dial();
    let mut r = BufReader::new(approved.try_clone().expect("clone"));
    let ask = |r: &mut BufReader<TcpStream>, path: &str| {
        let mut c = approved.try_clone().expect("clone");
        c.write_all(&wire(&Req::get(path, &w.boot).signed(), b""))
            .expect("write");
        let mut line = String::new();
        r.read_line(&mut line).expect("status line");
        // The rest of the answer: its headers and its body.
        let mut len = 0usize;
        loop {
            let mut h = String::new();
            r.read_line(&mut h).expect("header");
            if h.trim_end().is_empty() {
                break;
            }
            if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length: ") {
                len = v.trim().parse().expect("length");
            }
        }
        let mut body = vec![0; len];
        r.read_exact(&mut body).expect("body");
        line
    };
    assert!(ask(&mut r, "/v1/nodes/a").starts_with("HTTP/1.1 200"));
    let until = std::time::Instant::now() + Duration::from_secs(10);
    let mut stalled = Vec::new();
    for wave in 0..4 {
        for _ in 0..2 {
            let mut c = dial();
            let mut bad = Req::get("/v1/nodes/x", &w.boot);
            bad.sign_with = key_of("someone-else");
            let _ = c.write_all(&wire(&bad.signed(), b""));
            stalled.push(c);
        }
        if wave == 0 {
            while *stalls.held.lock().expect("lock") < 2 {
                assert!(std::time::Instant::now() < until, "never stalled");
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        // Cut the wave: its slots come back, its threads stay.
        w.clocks.advance(10_001);
        std::thread::sleep(Duration::from_millis(150));
    }
    assert_eq!(w.service.preauth_workers.held(), 2, "only the pre-auth cap");
    assert_eq!(
        *stalls.held.lock().expect("lock"),
        2,
        "later waves closed at accept"
    );
    // New unauthenticated work is closed unanswered...
    let mut late = dial();
    let _ = late.write_all(b"GET / HTTP/1.1\r\n\r\n");
    assert_closed_unanswered(&mut late);
    // ...while the approved client's kept connection is served.
    assert!(ask(&mut r, "/v1/nodes/b").starts_with("HTTP/1.1 200"));
    *stalls.release.lock().expect("lock") = true;
    stalls.wake.notify_all();
    drop(stalled);
    drop(r);
    drop(approved);
    stop.store(true, Ordering::SeqCst);
    t.join().expect("join").expect("ran");
}

#[test]
fn two_services_fallback_request_ids_differ() {
    // Terra 2 #9922, Terra 3 #9949, Terra 4 #10077: the salt survives the
    // version and variant bits whole, so two salts give two ids, whatever
    // the millisecond and the counter.
    let ms = 1_793_329_718_000;
    let a = uuid_v7(ms, fallback_bits([1, 2, 3, 4], 0));
    let b = uuid_v7(ms, fallback_bits([1, 2, 3, 5], 0));
    assert_ne!(a, b);
    assert_eq!(&a.replace('-', "")[18..26], "01020304", "{a}");
    assert_ne!(
        uuid_v7(ms, fallback_bits([9; 4], 1)),
        uuid_v7(ms, fallback_bits([9; 4], 2))
    );
    // Two running services have their own salts.
    let (x, y) = (world(), world());
    assert_ne!(x.service.salt, y.service.salt);
}

#[test]
fn a_signed_request_s_body_of_another_media_type_is_415_unread() {
    // Claude #10013: on a registry route, after authentication, before
    // the body is read; the connection closes.
    let w = world();
    let body = b"{}";
    let head = Req::post("/v1/persons/p1:approve", &w.boot, body)
        .header("content-type", "text/plain")
        .header("mdkg-actor", "human:ann")
        .person_signed("pk-ann", key_of("pk-ann"));
    // The head alone: the body is never sent, and needn't be.
    let (a, more) = once(&w, Listener::Unix, &head, b"");
    assert_eq!((a.status, a.reason().as_str()), (415, "content_type"));
    assert!(a.closes() && !more);
    assert!(w.calls.ops.lock().expect("lock").is_empty());
}

#[test]
fn a_reveal_or_poll_refused_for_its_media_type_is_charged_to_its_source() {
    // Terra 4 #10077: as any failed reveal or poll, sixty a minute.
    let w = world();
    let reveal = RequestHead {
        method: "POST".into(),
        path: "/enroll/01a188cc-aaaa-7000-8000-000000000081:reveal".into(),
        query: None,
        headers: vec![
            ("content-type".to_owned(), "text/plain".to_owned()),
            ("content-length".to_owned(), "2".to_owned()),
        ],
    };
    for _ in 0..60 {
        assert_eq!(once(&w, Listener::Unix, &reveal, b"{}").0.status, 415);
    }
    let (a, _) = once(&w, Listener::Unix, &reveal, b"{}");
    assert_eq!(a.code(), "api.rate_limited", "the sixty were charged");
}

#[test]
fn an_enrolment_s_origin_and_rate_come_before_its_media_type() {
    // Terra 2 #9922, orchestrator #9931: a bad origin is the origin's
    // refusal whatever the media type, and a media-type refusal is still
    // charged to its source.
    let w = world();
    let mk = |origin: Option<&str>| {
        let mut headers = vec![
            ("content-type".to_owned(), "text/plain".to_owned()),
            ("content-length".to_owned(), "2".to_owned()),
        ];
        if let Some(o) = origin {
            headers.push(("origin".into(), o.into()));
        }
        RequestHead {
            method: "POST".into(),
            path: "/enroll".into(),
            query: None,
            headers,
        }
    };
    let (a, _) = once(&w, Listener::Unix, &mk(Some("https://evil.example")), b"{}");
    assert_eq!(a.code(), "api.origin_refused");
    for _ in 0..5 {
        let (a, _) = once(&w, Listener::Unix, &mk(None), b"{}");
        assert_eq!((a.status, a.reason().as_str()), (415, "content_type"));
    }
    let (a, _) = once(&w, Listener::Unix, &mk(None), b"{}");
    assert_eq!(a.code(), "api.rate_limited", "the five were charged");
}

/// One SSE frame as the client reads it.
#[derive(Debug, Default, PartialEq)]
struct Frame {
    id: Option<String>,
    event: String,
    data: serde_json::Value,
}

/// The next SSE frame, or `None` at the end of the stream.
fn frame(r: &mut BufReader<UnixStream>) -> Option<Frame> {
    let mut f = Frame::default();
    let mut any = false;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).ok()? == 0 {
            return any.then_some(f);
        }
        let line = line.trim_end_matches('\n');
        if line.is_empty() {
            if any {
                return Some(f);
            }
            continue;
        }
        any = true;
        if let Some(v) = line.strip_prefix("id: ") {
            f.id = Some(v.to_owned());
        } else if let Some(v) = line.strip_prefix("event: ") {
            f.event = v.to_owned();
        } else if let Some(v) = line.strip_prefix("data: ") {
            f.data = serde_json::from_str(v).expect("json data");
        }
    }
}

/// The stream's response head, read past.
fn stream_head(r: &mut BufReader<UnixStream>) -> String {
    let mut status = String::new();
    r.read_line(&mut status).expect("status");
    loop {
        let mut l = String::new();
        r.read_line(&mut l).expect("header");
        if l.trim_end().is_empty() {
            return status;
        }
    }
}

fn tailed(seq: u64, data: String) -> crate::tail::Tailed {
    crate::tail::Tailed {
        seq,
        event: mdkg_ops::wire::Event {
            id: format!("s1:{seq}"),
            data,
        },
    }
}

fn page(events: Vec<crate::tail::Tailed>) -> crate::tail::Page {
    let through = events.last().map(|t| t.seq);
    crate::tail::Page {
        events,
        after_anchor: None,
        through,
        through_anchor: None,
        more: false,
    }
}

fn typed(seq: u64) -> String {
    format!("{{\"type\":\"mdkg.lease.claimed\",\"seq\":{seq}}}")
}

/// A world whose stream handler opens on a fan-out of `buffer` bytes.
fn streaming(buffer: usize, limits: Limits) -> (World, Arc<crate::tail::Fanout>) {
    let w = world_with(Arc::new(FakeVerifier), limits);
    let fanout = crate::tail::Fanout::new(0, None, buffer, "host");
    *w.calls.fanout.lock().expect("lock") = Some(fanout.clone());
    (w, fanout)
}

#[test]
fn a_stream_writes_events_with_their_cursors_and_heartbeats_with_the_head() {
    let (w, fanout) = streaming(1 << 20, Limits::default());
    let (mut c, t) = connect(&w, Listener::Unix);
    send(
        &mut c,
        &wire(&Req::get("/v1/events", &w.boot).signed(), b""),
    );
    let mut r = BufReader::new(c.try_clone().expect("clone"));
    assert!(stream_head(&mut r).starts_with("HTTP/1.1 200"));
    fanout
        .publish(&page(vec![tailed(1, typed(1)), tailed(2, typed(2))]))
        .expect("publish");
    let a = frame(&mut r).expect("frame");
    assert_eq!(
        (a.id.as_deref(), a.event.as_str()),
        (Some("s1:1"), "mdkg.lease.claimed")
    );
    assert_eq!(frame(&mut r).expect("frame").id.as_deref(), Some("s1:2"));
    // Nothing more: a heartbeat, with no id and the head it reached.
    let hb = frame(&mut r).expect("heartbeat");
    assert_eq!((hb.id, hb.event.as_str()), (None, "mdkg.stream.heartbeat"));
    assert_eq!(hb.data["durable"], false);
    assert_eq!(hb.data["cursor"], "s1:2");
    assert_eq!(hb.data["data"]["head"], "s1:2");
    // A drain is a normal close: no error frame.
    fanout.close_all(Closed::Draining);
    let mut rest = Vec::new();
    while let Some(f) = frame(&mut r) {
        rest.push(f.event);
    }
    assert!(
        rest.iter().all(|e| e == "mdkg.stream.heartbeat"),
        "{rest:?}"
    );
    t.join().expect("join");
}

#[test]
fn an_event_too_large_is_an_error_frame_with_its_cursor_and_the_stream_goes_on() {
    // Orchestrator #10487.
    let (w, fanout) = streaming(400, Limits::default());
    let (mut c, t) = connect(&w, Listener::Unix);
    send(
        &mut c,
        &wire(&Req::get("/v1/events", &w.boot).signed(), b""),
    );
    let mut r = BufReader::new(c.try_clone().expect("clone"));
    stream_head(&mut r);
    let big = format!(
        "{{\"type\":\"mdkg.message.sent\",\"x\":\"{}\"}}",
        "y".repeat(1000)
    );
    fanout
        .publish(&page(vec![tailed(1, big), tailed(2, typed(2))]))
        .expect("publish");
    let e = frame(&mut r).expect("frame");
    assert_eq!(
        (e.id.as_deref(), e.event.as_str()),
        (Some("s1:1"), "mdkg.stream.error")
    );
    assert_eq!(e.data["data"]["reason"], "event_too_large");
    assert_eq!(e.data["data"]["seq"], 1);
    // Its cursor and head are the skipped record's (Terra 4 #10578).
    assert_eq!(e.data["cursor"], "s1:1");
    assert_eq!(e.data["data"]["head"], "s1:1");
    // The code's registered message, the word apart.
    assert_eq!(
        e.data["data"]["diagnostics"][0]["message"],
        Code::ApiTooLarge.summary()
    );
    assert_eq!(e.data["data"]["diagnostics"][0]["code"], "api.too_large");
    assert_eq!(
        frame(&mut r).expect("frame").id.as_deref(),
        Some("s1:2"),
        "carried on"
    );
    fanout.close_all(Closed::Draining);
    drop(c);
    drop(r);
    t.join().expect("join");
}

#[test]
fn a_revoked_stream_is_cut_and_its_slot_given_back() {
    let limits = Limits {
        streams_per_machine: 1,
        ..Limits::default()
    };
    let (w, fanout) = streaming(1 << 20, limits);
    let (mut c, t) = connect(&w, Listener::Unix);
    send(
        &mut c,
        &wire(&Req::get("/v1/events", &w.boot).signed(), b""),
    );
    let mut r = BufReader::new(c.try_clone().expect("clone"));
    stream_head(&mut r);
    // The machine's one stream slot is taken: a second is refused.
    let (a, _) = once(
        &w,
        Listener::Unix,
        &Req::get("/v1/events", &w.boot).signed(),
        b"",
    );
    assert_eq!((a.status, a.reason().as_str()), (429, "streams"));
    w.calls.revoked.store(true, Ordering::SeqCst);
    fanout.check_grants();
    let mut frames = Vec::new();
    while let Some(f) = frame(&mut r) {
        frames.push(f);
    }
    // An idle writer isn't cut: it writes the stream's last error frame
    // (Terra 4 #10578).
    let last = frames.last().expect("a last frame");
    assert_eq!(last.event, "mdkg.stream.error");
    assert_eq!(
        last.data["data"]["diagnostics"][0]["code"],
        "api.machine_revoked"
    );
    assert_eq!(last.data["data"]["reason"], "machine_revoked");
    assert!(
        frames[..frames.len() - 1]
            .iter()
            .all(|f| f.event == "mdkg.stream.heartbeat"),
        "{frames:?}"
    );
    t.join().expect("join");
    // The slot is back: a new stream opens.
    w.calls.revoked.store(false, Ordering::SeqCst);
    let (mut c2, t2) = connect(&w, Listener::Unix);
    send(
        &mut c2,
        &wire(&Req::get("/v1/events", &w.boot).signed(), b""),
    );
    let mut r2 = BufReader::new(c2.try_clone().expect("clone"));
    assert!(stream_head(&mut r2).starts_with("HTTP/1.1 200"));
    fanout.close_all(Closed::Draining);
    drop(c2);
    drop(r2);
    t2.join().expect("join");
}

#[test]
fn a_revocation_cuts_a_writer_blocked_in_a_write_within_a_second_or_two() {
    // Terra 4 #10578, S6/R17: a client that stopped reading holds the
    // writer in a write; the revocation's cut releases it, with no move
    // of the clock, so not the 30 s stall.
    let (w, fanout) = streaming(64 << 20, Limits::default());
    let (mut c, t) = connect(&w, Listener::Unix);
    send(
        &mut c,
        &wire(&Req::get("/v1/events", &w.boot).signed(), b""),
    );
    let mut r = BufReader::new(c.try_clone().expect("clone"));
    stream_head(&mut r);
    let events: Vec<_> = (1..=64)
        .map(|seq| {
            let data = format!(
                "{{\"type\":\"mdkg.x\",\"p\":\"{}\"}}",
                "z".repeat(64 * 1024)
            );
            tailed(seq, data)
        })
        .collect();
    fanout.publish(&page(events)).expect("publish");
    std::thread::sleep(Duration::from_millis(300));
    let started = std::time::Instant::now();
    w.calls.revoked.store(true, Ordering::SeqCst);
    fanout.check_grants();
    let (done, finished) = std::sync::mpsc::channel();
    std::thread::spawn(move || done.send(t.join().is_ok()));
    let joined = finished.recv_timeout(Duration::from_secs(2));
    drop(c);
    drop(r);
    assert_eq!(joined, Ok(true), "cut by the revocation");
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn a_lapsed_person_grant_ends_the_stream_with_the_person_s_code() {
    // Orchestrator #10592: never `api.machine_revoked` for a person's own
    // lapse.
    let (w, fanout) = streaming(1 << 20, Limits::default());
    let (mut c, t) = connect(&w, Listener::Unix);
    send(
        &mut c,
        &wire(&Req::get("/v1/events", &w.boot).signed(), b""),
    );
    let mut r = BufReader::new(c.try_clone().expect("clone"));
    stream_head(&mut r);
    w.calls.person_lapsed.store(true, Ordering::SeqCst);
    fanout.check_grants();
    let mut last = None;
    while let Some(f) = frame(&mut r) {
        last = Some(f);
    }
    let last = last.expect("a last frame");
    assert_eq!(last.event, "mdkg.stream.error");
    assert_eq!(
        last.data["data"]["diagnostics"][0]["code"],
        "api.person_unknown"
    );
    assert_eq!(last.data["data"]["reason"], "person_lapsed");
    t.join().expect("join");
}

/// Frames whose cut fires just after an event passed its check and
/// before the writer writes it: the window Terra 4 #10604 found.
struct CutBeforeWrite {
    cut: Mutex<Option<crate::tail::Cut>>,
    handed: AtomicBool,
}

impl crate::tail::Frames for CutBeforeWrite {
    fn next(&mut self) -> Next {
        if self.handed.swap(true, Ordering::SeqCst) {
            return Next::Closed(Closed::MachineRevoked);
        }
        if let Some(cut) = self.cut.lock().expect("lock").as_ref() {
            cut();
        }
        // Far more than the socket holds: written to a client that reads
        // nothing, it blocks.
        Next::Event(mdkg_ops::wire::Event {
            id: "s1:1".into(),
            data: format!("{{\"type\":\"mdkg.x\",\"p\":\"{}\"}}", "z".repeat(8 << 20)),
        })
    }
    fn set_cut(&self, cut: crate::tail::Cut) {
        *self.cut.lock().expect("lock") = Some(cut);
    }
}

#[test]
fn a_cut_landing_between_next_and_the_write_still_cuts_it() {
    // Terra 4 #10604: the in-flight event (#10141) is cut as it starts,
    // never left blocked until the 30 s stall.
    let (w, _fanout) = streaming(1 << 20, Limits::default());
    *w.calls.custom_frames.lock().expect("lock") = Some(Box::new(CutBeforeWrite {
        cut: Mutex::new(None),
        handed: AtomicBool::new(false),
    }));
    let (mut c, t) = connect(&w, Listener::Unix);
    send(
        &mut c,
        &wire(&Req::get("/v1/events", &w.boot).signed(), b""),
    );
    let started = std::time::Instant::now();
    let (done, finished) = std::sync::mpsc::channel();
    std::thread::spawn(move || done.send(t.join().is_ok()));
    let joined = finished.recv_timeout(Duration::from_secs(2));
    drop(c);
    assert_eq!(joined, Ok(true), "cut as the write started");
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn a_stream_the_handler_refuses_is_an_envelope_not_a_stream() {
    // A15: a refusal before the stream starts is an ordinary envelope;
    // the corrupt-log refusal (Terra 2 #10378) is one.
    let (w, _fanout) = streaming(1 << 20, Limits::default());
    w.calls.refuse_streams.store(true, Ordering::SeqCst);
    let (a, _) = once(
        &w,
        Listener::Unix,
        &Req::get("/v1/events", &w.boot).signed(),
        b"",
    );
    assert_eq!((a.status, a.code().as_str()), (500, "coord.store_failed"));
    assert_eq!(a.reason(), "corrupt_log");
}

#[test]
fn a_stream_write_stuck_past_the_stall_limit_is_cut() {
    // S4: a client whose socket stays unwritable for 30 s is disconnected.
    let (w, fanout) = streaming(64 << 20, Limits::default());
    let (mut c, t) = connect(&w, Listener::Unix);
    send(
        &mut c,
        &wire(&Req::get("/v1/events", &w.boot).signed(), b""),
    );
    let mut r = BufReader::new(c.try_clone().expect("clone"));
    stream_head(&mut r);
    // Far more than the socket holds; the client reads none of it.
    let events: Vec<_> = (1..=64)
        .map(|seq| {
            let data = format!(
                "{{\"type\":\"mdkg.x\",\"p\":\"{}\"}}",
                "z".repeat(64 * 1024)
            );
            tailed(seq, data)
        })
        .collect();
    fanout.publish(&page(events)).expect("publish");
    std::thread::sleep(Duration::from_millis(300));
    w.clocks.advance(30_001);
    let (done, finished) = std::sync::mpsc::channel();
    std::thread::spawn(move || done.send(t.join().is_ok()));
    let joined = finished.recv_timeout(Duration::from_secs(5));
    drop(c);
    drop(r);
    assert_eq!(joined, Ok(true), "cut at the stall limit");
}

#[test]
fn rfc3339_times_are_to_the_millisecond() {
    assert_eq!(rfc3339_ms(0), "1970-01-01T00:00:00.000Z");
    assert_eq!(rfc3339_ms(1_793_329_718_123), "2026-10-30T03:08:38.123Z");
}

#[test]
fn dates_are_imf_fixdates() {
    assert_eq!(http_date(0), "Thu, 01 Jan 1970 00:00:00 GMT");
    assert_eq!(http_date(1_793_329_718), "Fri, 30 Oct 2026 03:08:38 GMT");
    assert_eq!(
        http_date(951_782_400 + 86_399),
        "Tue, 29 Feb 2000 23:59:59 GMT"
    );
}

#[test]
fn a_timeout_on_a_socket_whose_peer_left_is_no_error() {
    // Orchestrator #10568 (wire 8d975231): on macOS, setting a timeout on
    // a Unix socket whose peer has closed fails with EINVAL, though reads
    // still return the buffered bytes and then EOF. The peer is gone, so
    // reading carries on to EOF.
    let (mut client, mut server) = UnixStream::pair().expect("pair");
    client.write_all(b"buffered").expect("write");
    drop(client);
    Io::set_read_timeout(&server, Some(Duration::from_secs(1))).expect("read timeout");
    Io::set_write_timeout(&server, Some(Duration::from_secs(1))).expect("write timeout");
    let mut got = Vec::new();
    server.read_to_end(&mut got).expect("to EOF");
    assert_eq!(got, b"buffered");
}

#[test]
fn a_request_from_a_client_that_already_left_is_still_read_and_handled() {
    // The same, through the listener: the whole request was sent before
    // the client closed, so it is read and handled; only the answer is
    // lost.
    let w = world();
    let (mut c, t) = connect(&w, Listener::Unix);
    send(
        &mut c,
        &wire(&Req::get("/v1/nodes/n1", &w.boot).signed(), b""),
    );
    drop(c);
    t.join().expect("join");
    assert_eq!(
        w.calls
            .ops
            .lock()
            .expect("lock")
            .first()
            .map(|o| o.0.clone()),
        Some("nodes.show".to_owned())
    );
}

#[test]
fn a_cut_stream_s_last_frame_gets_only_the_cut_grace() {
    // Terra 4 #10604: once a cut is asked for, the write under way (the
    // stream's last frame) is cut by the timer after CUT_GRACE, not after
    // the 30 s stall.
    let w = world();
    let (client, server) = UnixStream::pair().expect("pair");
    let now = w.service.gate.observe_mono();
    let conn = w
        .caps
        .connection(Listener::Unix, &w.service.limits, now)
        .expect("a slot");
    let shared = Arc::new(Shared {
        conn: Mutex::new(Some(conn)),
        closer: Io::closer(&server).expect("closer"),
        preauth: Mutex::new(None),
        head_by: Mutex::new(None),
        writing_since: Mutex::new(Some(now)),
        streaming: AtomicBool::new(true),
        cut_requested: AtomicBool::new(true),
        busy: std::sync::atomic::AtomicU8::new(IDLE),
    });
    w.service.timer.watch(&shared);
    std::thread::sleep(Duration::from_millis(150));
    assert!(shared.lock().is_some(), "not before the grace");
    w.clocks.advance(1_001);
    let until = std::time::Instant::now() + Duration::from_secs(2);
    while shared.lock().is_some() {
        assert!(std::time::Instant::now() < until, "never cut");
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut r = BufReader::new(client);
    r.get_mut()
        .set_read_timeout(Some(Duration::from_secs(1)))
        .expect("timeout");
    assert!(at_eof(&mut r), "the socket was closed");
    drop(server);
}

/// Runs the drain on its own thread; its outcome comes on the channel.
fn drain_in_background(
    w: &World,
    grace: Duration,
    turns: Arc<AtomicBool>,
    fanout: Option<Arc<crate::tail::Fanout>>,
) -> (std::sync::mpsc::Receiver<Drained>, Arc<AtomicBool>) {
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = std::sync::mpsc::channel();
    let (service, stop2) = (w.service.clone(), stop.clone());
    std::thread::spawn(move || {
        let running = move || turns.load(Ordering::SeqCst);
        let outcome = service.drain(DrainPlan {
            grace,
            turns_running: &running,
            fanout: fanout.as_deref(),
            stop: &stop2,
        });
        let _ = tx.send(outcome);
    });
    (rx, stop)
}

#[test]
fn draining_refuses_a_new_turn_and_serves_everything_else() {
    // S3 step 1: `api.draining` (503) for `turn.admit` alone.
    let w = world();
    w.service.begin_drain();
    let head = Req::post("/v1/turns:admit", &w.boot, b"").signed();
    let (a, _) = once(&w, Listener::Unix, &head, b"");
    assert_eq!((a.status, a.code().as_str()), (503, "api.draining"));
    assert!(a.headers.contains_key("retry-after"));
    let (a, _) = once(
        &w,
        Listener::Unix,
        &Req::get("/v1/nodes/n1", &w.boot).signed(),
        b"",
    );
    assert_eq!(a.status, 200, "everything else is served");
    assert!(
        w.calls
            .ops
            .lock()
            .expect("lock")
            .iter()
            .all(|o| o.0 != "turn.admit")
    );
}

#[test]
fn the_grace_lasts_while_a_turn_runs_until_it_ends_or_a_second_signal() {
    // S3 step 2.
    let w = world();
    let turns = Arc::new(AtomicBool::new(true));
    let (rx, stop) = drain_in_background(&w, Duration::from_secs(120), turns.clone(), None);
    assert!(
        rx.recv_timeout(Duration::from_millis(200)).is_err(),
        "still in its grace"
    );
    assert!(!stop.load(Ordering::SeqCst), "the listeners still accept");
    turns.store(false, Ordering::SeqCst);
    assert_eq!(rx.recv_timeout(Duration::from_secs(2)), Ok(Drained::Clean));
    assert!(stop.load(Ordering::SeqCst));
    // A second signal ends it at once, a turn still running.
    let w = world();
    let (rx, _) = drain_in_background(
        &w,
        Duration::from_secs(120),
        Arc::new(AtomicBool::new(true)),
        None,
    );
    assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
    w.service.end_grace();
    assert_eq!(rx.recv_timeout(Duration::from_secs(2)), Ok(Drained::Clean));
    // And the grace's end does too.
    let w = world();
    let (rx, _) = drain_in_background(
        &w,
        Duration::from_secs(120),
        Arc::new(AtomicBool::new(true)),
        None,
    );
    assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
    w.clocks.advance(120_001);
    assert_eq!(rx.recv_timeout(Duration::from_secs(2)), Ok(Drained::Clean));
}

#[test]
fn a_request_under_way_finishes_and_an_idle_connection_closes() {
    // S3 step 3: the response is still written; nothing new is read.
    let w = world();
    let (mut idle, t_idle) = connect(&w, Listener::Unix);
    let mut ri = BufReader::new(idle.try_clone().expect("clone"));
    send(
        &mut idle,
        &wire(&Req::get("/v1/nodes/n1", &w.boot).signed(), b""),
    );
    assert_eq!(answer(&mut ri).expect("first").status, 200);
    let (mut c, t) = connect(&w, Listener::Unix);
    let mut r = BufReader::new(c.try_clone().expect("clone"));
    send(&mut c, &wire(&Req::get("/v1/slow", &w.boot).signed(), b""));
    while !w.calls.began.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(5));
    }
    let (rx, _) = drain_in_background(
        &w,
        Duration::from_secs(120),
        Arc::new(AtomicBool::new(false)),
        None,
    );
    let a = answer(&mut r).expect("the slow answer");
    assert_eq!(a.status, 200);
    assert!(a.closes(), "and the connection closes after it");
    assert_eq!(rx.recv_timeout(Duration::from_secs(3)), Ok(Drained::Clean));
    assert!(at_eof(&mut ri), "the idle connection was closed");
    t.join().expect("join");
    t_idle.join().expect("join");
}

#[test]
fn a_request_past_the_10_s_limit_is_cut_off() {
    // S3 steps 3 and 5: cut, and the drain says so.
    let w = world();
    let (mut c, t) = connect(&w, Listener::Unix);
    let mut r = BufReader::new(c.try_clone().expect("clone"));
    send(&mut c, &wire(&Req::get("/v1/block", &w.boot).signed(), b""));
    while !w.calls.began.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(5));
    }
    let (rx, _) = drain_in_background(
        &w,
        Duration::from_secs(120),
        Arc::new(AtomicBool::new(false)),
        None,
    );
    std::thread::sleep(Duration::from_millis(100));
    w.clocks.advance(10_001);
    assert_eq!(rx.recv_timeout(Duration::from_secs(2)), Ok(Drained::CutOff));
    w.calls.release.store(true, Ordering::SeqCst);
    assert!(at_eof(&mut r), "no answer");
    t.join().expect("join");
}

#[test]
fn the_drain_ends_streams_with_a_normal_close() {
    // S3 step 4, A15: no error frame; the client resumes from its cursor.
    let (w, fanout) = streaming(1 << 20, Limits::default());
    let (mut c, t) = connect(&w, Listener::Unix);
    send(
        &mut c,
        &wire(&Req::get("/v1/events", &w.boot).signed(), b""),
    );
    let mut r = BufReader::new(c.try_clone().expect("clone"));
    stream_head(&mut r);
    let (rx, _) = drain_in_background(
        &w,
        Duration::from_secs(120),
        Arc::new(AtomicBool::new(false)),
        Some(fanout),
    );
    assert_eq!(rx.recv_timeout(Duration::from_secs(5)), Ok(Drained::Clean));
    let mut events = Vec::new();
    while let Some(f) = frame(&mut r) {
        events.push(f.event);
    }
    assert!(
        events.iter().all(|e| e == "mdkg.stream.heartbeat"),
        "{events:?}"
    );
    t.join().expect("join");
}

#[test]
fn finishing_the_drain_removes_the_socket_and_releases_the_lock() {
    // S3 step 5.
    let common = std::env::temp_dir().join(format!(
        "mdkg-drain-{}-{}",
        std::process::id(),
        crate::random::hex(&crate::random::bytes::<4>().expect("urandom"))
    ));
    std::fs::create_dir_all(&common).expect("dir");
    let lock = crate::state::ServeLock::acquire(&common).expect("lock");
    let socket = common.join("serve.sock");
    std::fs::write(&socket, b"").expect("socket stand-in");
    finish_drain(lock, &socket).expect("finished");
    assert!(!socket.exists());
    assert!(
        !crate::state::lock_held(&common).expect("probe"),
        "released"
    );
    // A second start takes the lock.
    drop(crate::state::ServeLock::acquire(&common).expect("free again"));
    std::fs::remove_dir_all(&common).ok();
}

#[test]
fn a_request_and_the_drain_s_cut_are_decided_by_one_swap() {
    // Terra 4 #10644: whichever swaps first wins, and the other backs off,
    // so a request is cut before it runs or runs to its answer.
    let make = || {
        let (_c, server) = UnixStream::pair().expect("pair");
        Shared {
            conn: Mutex::new(None),
            closer: Io::closer(&server).expect("closer"),
            preauth: Mutex::new(None),
            head_by: Mutex::new(None),
            writing_since: Mutex::new(None),
            streaming: AtomicBool::new(false),
            cut_requested: AtomicBool::new(false),
            busy: std::sync::atomic::AtomicU8::new(IDLE),
        }
    };
    let drain_first = make();
    assert!(drain_first.claim_cut());
    assert!(
        !drain_first.claim_request(),
        "cut first: the request never runs"
    );
    let request_first = make();
    assert!(request_first.claim_request());
    assert!(
        !request_first.claim_cut(),
        "claimed first: the drain lets it finish"
    );
    request_first.request_done();
    assert!(
        request_first.claim_cut(),
        "answered: idle again, so cut now"
    );
    assert!(!request_first.claim_request());
}

// The same listener behind wire's own codec, as `serve` runs it (#10448):
// the trait swap must keep S5's order, the bodiless refusals, the close
// after an unread body, keep-alive after authentication, and streams.

fn http1() -> Arc<dyn Codec> {
    Arc::new(mdkg_wire::codec::Http1)
}

#[test]
fn through_wire_s_codec_a_signed_request_is_served_and_kept_alive() {
    let w = world();
    let (mut c, t) = connect_via(&w, Listener::Tcp, http1());
    let mut r = BufReader::new(c.try_clone().expect("clone"));
    send(
        &mut c,
        &wire(&Req::get("/v1/nodes/n%3A1", &w.boot).signed(), b""),
    );
    let a = answer(&mut r).expect("answer");
    assert_eq!(
        (a.status, a.body.as_slice()),
        (200, b"nodes.show".as_slice())
    );
    assert!(!a.closes(), "kept alive once authenticated");
    for h in ["mdkg-request-id", "mdkg-boot", "date"] {
        assert!(a.headers.contains_key(h), "{h}");
    }
    send(
        &mut c,
        &wire(&Req::get("/v1/nodes/n2", &w.boot).signed(), b""),
    );
    assert_eq!(answer(&mut r).expect("second").status, 200);
    drop(c);
    drop(r);
    t.join().expect("join");
    assert_eq!(w.calls.ops.lock().expect("lock").len(), 2);
    assert_eq!(w.caps.connections(), (0, 0, 0), "slots given back");
}

#[test]
fn through_wire_s_codec_an_unsigned_root_closes_a_preauth_connection() {
    let w = world();
    let head = RequestHead {
        method: "GET".into(),
        path: "/".into(),
        query: None,
        headers: vec![],
    };
    let (mut c, t) = connect_via(&w, Listener::Tcp, http1());
    let mut r = BufReader::new(c.try_clone().expect("clone"));
    send(&mut c, &wire(&head, b""));
    let a = answer(&mut r).expect("answer");
    assert_eq!((a.status, a.body.as_slice()), (200, b"root".as_slice()));
    assert!(a.closes() && at_eof(&mut r));
    t.join().expect("join");
}

#[test]
fn through_wire_s_codec_head_is_a_bodiless_405_and_closes() {
    let w = world();
    let (mut c, t) = connect_via(&w, Listener::Unix, http1());
    let mut r = BufReader::new(c.try_clone().expect("clone"));
    let mut head = Req::get("/v1/nodes/n1", &w.boot).signed();
    head.method = "HEAD".into();
    send(&mut c, &wire(&head, b""));
    let a = answer(&mut r).expect("answer");
    assert_eq!(a.status, 405);
    assert_eq!(a.headers.get("allow").map(String::as_str), Some("GET"));
    assert_eq!(
        a.headers.get("content-length").map(String::as_str),
        Some("0")
    );
    assert!(a.body.is_empty() && a.closes() && at_eof(&mut r));
    t.join().expect("join");
    assert!(w.calls.ops.lock().expect("lock").is_empty());
}

#[test]
fn through_wire_s_codec_a_body_over_the_route_cap_is_refused_unread_and_closes() {
    // The smuggled request in the body is never answered: wire ends the
    // connection after the refusal, as serve asks.
    let w = world();
    let head = RequestHead {
        method: "POST".into(),
        path: "/enroll".into(),
        query: None,
        headers: vec![("content-length".into(), (4 * 1024 + 1).to_string())],
    };
    let mut body = b"GET / HTTP/1.1\r\n\r\n".to_vec();
    body.extend(std::iter::repeat_n(b'x', 64));
    let (mut c, t) = connect_via(&w, Listener::Unix, http1());
    let mut r = BufReader::new(c.try_clone().expect("clone"));
    let _ = c.write_all(&wire(&head, &body));
    let a = answer(&mut r).expect("answer");
    assert_eq!((a.status, a.code().as_str()), (413, "api.too_large"));
    assert!(a.closes());
    assert!(at_eof(&mut r), "the body was not a request");
    drop(c);
    drop(r);
    t.join().expect("join");
    assert!(w.calls.ops.lock().expect("lock").is_empty());
}

#[test]
fn through_wire_s_codec_a_head_over_its_limits_is_refused_without_a_body() {
    // wire's codec enforces the head limits serve gives it: 65 headers on
    // a HEAD are refused (serve's 413, api.too_large) with no body, and
    // the connection ends.
    let w = world();
    let (mut c, t) = connect_via(&w, Listener::Unix, http1());
    let mut r = BufReader::new(c.try_clone().expect("clone"));
    let mut bytes = b"HEAD / HTTP/1.1\r\n".to_vec();
    for i in 0..65 {
        bytes.extend_from_slice(format!("x-{i}: y\r\n").as_bytes());
    }
    bytes.extend_from_slice(b"\r\n");
    send(&mut c, &bytes);
    let a = answer(&mut r).expect("answer");
    assert_eq!(a.status, 413);
    assert_eq!(
        a.headers.get("content-length").map(String::as_str),
        Some("0")
    );
    assert!(a.body.is_empty() && a.closes() && at_eof(&mut r));
    t.join().expect("join");
}

#[test]
fn through_wire_s_codec_a_tampered_path_gets_the_authentication_error() {
    let w = world();
    let mut head = Req::get("/v1/nodes/n1", &w.boot).signed();
    head.path = "/v1/nodes/n2".into();
    let (mut c, t) = connect_via(&w, Listener::Unix, http1());
    let mut r = BufReader::new(c.try_clone().expect("clone"));
    send(&mut c, &wire(&head, b""));
    let a = answer(&mut r).expect("answer");
    assert_eq!((a.status, a.code().as_str()), (401, "api.unauthenticated"));
    drop(c);
    drop(r);
    t.join().expect("join");
    assert!(w.calls.ops.lock().expect("lock").is_empty());
}

#[test]
fn through_wire_s_codec_a_stream_carries_events_and_ends_normally_on_a_drain() {
    let (w, fanout) = streaming(1 << 20, Limits::default());
    let (mut c, t) = connect_via(&w, Listener::Unix, http1());
    send(
        &mut c,
        &wire(&Req::get("/v1/events", &w.boot).signed(), b""),
    );
    let mut r = BufReader::new(c.try_clone().expect("clone"));
    let mut status = String::new();
    r.read_line(&mut status).expect("status");
    assert!(status.starts_with("HTTP/1.1 200"), "{status}");
    let mut headers = Vec::new();
    loop {
        let mut l = String::new();
        r.read_line(&mut l).expect("header");
        if l.trim_end().is_empty() {
            break;
        }
        headers.push(l.trim_end().to_ascii_lowercase());
    }
    for h in [
        "content-type: text/event-stream",
        "cache-control: no-store",
        "connection: close",
    ] {
        assert!(headers.iter().any(|l| l == h), "{h}: {headers:?}");
    }
    assert!(headers.iter().any(|l| l.starts_with("mdkg-request-id: ")));
    fanout
        .publish(&page(vec![tailed(1, typed(1))]))
        .expect("publish");
    let f = frame(&mut r).expect("frame");
    assert_eq!(
        (f.id.as_deref(), f.event.as_str()),
        (Some("s1:1"), "mdkg.lease.claimed")
    );
    fanout.close_all(Closed::Draining);
    let mut rest = Vec::new();
    while let Some(f) = frame(&mut r) {
        rest.push(f.event);
    }
    assert!(
        rest.iter().all(|e| e == "mdkg.stream.heartbeat"),
        "a drain is a normal close: {rest:?}"
    );
    assert!(at_eof(&mut r), "wire ends the stream cleanly");
    t.join().expect("join");
}

#[test]
fn every_handler_gets_a_request_whose_verified_is_ready_for_the_kernel() {
    // Terra 1 #10811: the listener calls a handler only after the body
    // matched its digest, so `auth.verified()`, what the kernel's remote
    // context is built from (`Ctx::remote`), is always there, with the
    // person signature's key and human. Building the Ctx is serve-run's
    // dispatch.
    let (w, fanout) = streaming(1 << 20, Limits::default());
    let body = b"{}";
    let approve = Req::post("/v1/persons/p1:approve", &w.boot, body)
        .header("content-type", "application/json")
        .header("mdkg-actor", "human:ann")
        .person_signed("pk-ann", key_of("pk-ann"));
    assert_eq!(once(&w, Listener::Unix, &approve, body).0.status, 200);
    let read = Req::get("/v1/nodes/n1", &w.boot).signed();
    assert_eq!(once(&w, Listener::Tcp, &read, b"").0.status, 200);
    let (mut c, t) = connect(&w, Listener::Unix);
    send(
        &mut c,
        &wire(&Req::get("/v1/events", &w.boot).signed(), b""),
    );
    let mut r = BufReader::new(c.try_clone().expect("clone"));
    assert!(stream_head(&mut r).starts_with("HTTP/1.1 200"));
    drop(c);
    drop(r);
    fanout.close_all(Closed::Draining);
    t.join().expect("join");
    let seen = w.calls.verified.lock().expect("lock").clone();
    let seen: Vec<_> = seen
        .into_iter()
        .map(|v| {
            v.map(|v| {
                let person = v.person().map(|(k, h)| (k.to_owned(), h.to_owned()));
                (v.machine().to_owned(), person)
            })
        })
        .collect();
    assert_eq!(
        seen,
        vec![
            Some((
                "m-ok".to_owned(),
                Some(("pk-ann".to_owned(), "human:ann".to_owned()))
            )),
            Some(("m-ok".to_owned(), None)),
            Some(("m-ok".to_owned(), None)),
        ]
    );
}
