//! The listeners (S4, S21): the Unix socket and TCP, each connection's
//! request loop, and the timer that ends a pre-auth connection on time.
//!
//! Every request on either listener goes through one path, in S5's
//! order: the size limits (with the route's own body cap), then the
//! route's authentication, then the router's admission, then the body,
//! its digest, and the handler. No listener produces `Origin::Local`
//! (#8984): host-class operations are refused by the router whoever
//! signed, and `person approve` is served only as the remote `Present`
//! path (#8901, Terra 3 #9112).
//!
//! On a signed request, a route or path-parameter refusal is reported
//! only once the signature verified, so a tampered path gets the
//! authentication error (R12, SC6, S4). After any refusal that leaves a
//! declared body unread, the connection is closed: its bytes are never
//! read as another request (orchestrator #9464, Terra 4 #9457).
//!
//! HTTP/1.1 framing and TLS are `mdkg-wire`'s: its [`Codec`] and
//! [`HttpConnection`] (`codec::Http1` in production), its [`Io`] over the
//! Unix socket and TLS, and its [`TlsAcceptor`] (`tls::HostAcceptor`).
//! The listener keeps its own [`Closer`] per connection and the pre-auth
//! timer; the TLS handshake gets what is left of the 10 s (#10448).

use std::io;
use std::net::TcpListener;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::Duration;

use mdkg_format::Code;
use mdkg_graph::ErrorKind;
use mdkg_ops::registry::{Kind, OpSpec};
use mdkg_wire::codec::{Closer, Codec, HttpConnection, Io, ReadError};
use mdkg_wire::tls::TlsAcceptor;
use serde_json::{Value, json};

use crate::auth::{Authenticated, Gate};
use crate::enroll::{Enroller, Ephemeral};
use crate::http::{HeadExt, RequestHead, Response};
use crate::limits::{Caps, Conn, Limits, Listener, Peer};
use crate::person_proof::PersonProofs;
use crate::refusal::Refusal;
use crate::router::{PERSON_APPROVE, Routed, Router};
use crate::tail::{Closed, Frames, Next};

/// A stream a handler opened: its frames, and the cursor it starts from
/// (the client's `after`, or the head), for heartbeats until an event is
/// sent.
pub struct StreamStart {
    pub frames: Box<dyn Frames>,
    pub head: Option<String>,
}

impl std::fmt::Debug for StreamStart {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamStart")
            .field("head", &self.head)
            .finish_non_exhaustive()
    }
}

/// What the listener doesn't decide: the public `GET /`, the person
/// bootstrap's first request, the registry's operations, and the fan-out
/// of ephemeral events.
pub trait Handlers: Send + Sync {
    /// `GET /` (S4): unsigned, the public fields only (`auth` is
    /// `None`); signed by an approved machine, the graph record too.
    fn root(&self, auth: Option<&Authenticated>) -> Response;
    /// `POST /persons:enroll` (S41), from an approved machine whose body
    /// matched its digest.
    fn person_enroll(
        &self,
        auth: &Authenticated,
        head: &RequestHead,
        body: &[u8],
    ) -> Result<Response, Refusal>;
    /// A registry operation the router admitted, its `{name}` values
    /// decoded once (S4). A remote `person approve` carries its signed
    /// root bytes in `body`, which the approval adapter verifies against
    /// unchanged (orchestrator #9464 on #9112 item 2).
    fn op(
        &self,
        auth: &Authenticated,
        op: &'static OpSpec,
        params: &[String],
        head: &RequestHead,
        body: &[u8],
    ) -> Result<Response, Refusal>;
    /// An enrolment's ephemeral event (`durable: false`).
    fn ephemeral(&self, event: Ephemeral);
    /// A stream operation (`Kind::Stream`: `GET /v1/events`, a turn's
    /// output, `msg wait`), its cursor and filters checked: its frames.
    /// `/v1/events` opens a fan-out subscription with
    /// [`crate::tail::Fanout::open`], whose grant is the authenticated
    /// machine's and person's, read from the store at each check.
    fn stream(
        &self,
        auth: &Authenticated,
        op: &'static OpSpec,
        params: &[String],
        head: &RequestHead,
    ) -> Result<StreamStart, Refusal>;
}

/// One `serve`'s request path, shared by its listeners.
pub struct Service {
    gate: Arc<Gate>,
    router: Router,
    enroller: Arc<Enroller>,
    proofs: Arc<PersonProofs>,
    caps: Arc<Caps>,
    limits: Limits,
    handlers: Arc<dyn Handlers>,
    timer: Arc<Timer>,
    /// Our own effective uid, as the state folder's ownership is
    /// checked: the only peer user the socket serves (R9, Terra 5 #9795).
    uid: u32,
    /// Connection threads, each listener's bounded by its own pool: a
    /// thread holds its permit until it exits, so threads stay bounded
    /// even when the timer has given back a stalled connection's slots
    /// (Terra 5 #9795).
    tcp_workers: Arc<Workers>,
    unix_workers: Arc<Workers>,
    /// TCP threads not yet authenticated, at most the pre-auth slots
    /// (64), so authenticated work always has the rest of the pool even
    /// while verifications stall (orchestrator #9931, Terra 2 #9922).
    preauth_workers: Arc<Workers>,
    /// Part of a fallback request id, so two services' ids differ.
    salt: [u8; 4],
    /// The drain (S3): refusing new turns; no longer reading new
    /// requests; the grace cut short by a second signal.
    draining: AtomicBool,
    stopping: AtomicBool,
    grace_ended: AtomicBool,
    /// Every open connection, for the drain.
    conns: Mutex<Vec<Weak<Shared>>>,
    /// Request ids when randomness fails.
    requests: AtomicU64,
}

impl std::fmt::Debug for Service {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Service").finish_non_exhaustive()
    }
}

/// A connection's slots, which the pre-auth timer may take, and the
/// handle that closes it.
struct Shared {
    conn: Mutex<Option<Conn>>,
    closer: Box<dyn Closer>,
    /// A TCP connection's pre-auth worker permit: given back once it is
    /// promoted, or when its thread exits, never when the timer cuts it
    /// (orchestrator #9931).
    preauth: Mutex<Option<Permit>>,
    /// A new TCP connection's 5 s pre-auth header budget, from accept:
    /// the timer cuts it then unless a whole head has arrived, since the
    /// codec measures a head's own deadline from its first byte
    /// (orchestrator #10194, Terra 3 #10182). `None` once one has.
    head_by: Mutex<Option<Duration>>,
    /// While a stream frame is being written: since when. The timer cuts
    /// a stream whose write has been stuck past the stall limit, 30 s
    /// (S4's backpressure).
    writing_since: Mutex<Option<Duration>>,
    /// Whether the connection carries a stream, so the timer keeps
    /// watching it.
    streaming: AtomicBool,
    /// A stream's cut was asked for (a revocation, an overflow): a write
    /// under way is cut at once, and one that starts later cuts itself,
    /// so a cut landing between `next` and the write isn't lost (Terra 4
    /// #10604).
    cut_requested: AtomicBool,
    /// A request is being handled: its head read, its response not yet
    /// written. The drain lets these finish (S3 step 3).
    busy: AtomicU8,
}

impl Shared {
    /// A whole head has arrived: the header budget is met.
    fn head_arrived(&self) {
        *self.head_by.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    fn head_by(&self) -> Option<Duration> {
        *self.head_by.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A request's head came: idle becomes busy, unless the drain cut the
    /// connection first. False then, and the request must not run.
    fn claim_request(&self) -> bool {
        self.busy
            .compare_exchange(IDLE, BUSY, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    /// The drain's idle pass: idle becomes cut, unless a request claimed
    /// the connection first. True when the drain may cut it.
    fn claim_cut(&self) -> bool {
        self.busy
            .compare_exchange(IDLE, CUT, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    /// A request answered: busy back to idle (a cut stays cut).
    fn request_done(&self) {
        let _ = self
            .busy
            .compare_exchange(BUSY, IDLE, Ordering::SeqCst, Ordering::SeqCst);
    }

    fn writing(&self, since: Option<Duration>) {
        *self.writing_since.lock().unwrap_or_else(|e| e.into_inner()) = since;
    }

    fn writing_since(&self) -> Option<Duration> {
        *self.writing_since.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Conn>> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Gives back the pre-auth worker permit, once promoted.
    fn preauth_worker(&self) {
        self.preauth
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
    }

    /// Closes the connection and gives back its slots.
    fn cut(&self) {
        self.lock().take();
        self.closer.close();
    }
}

/// What a request came to.
enum Step {
    /// Answer, then close if asked to.
    Respond(Response, bool),
    /// Close without answering: the timer cut the connection, or the
    /// peer stopped part way.
    Abort,
}

fn refused(r: &Refusal, body_unread: bool) -> Step {
    Step::Respond(r.response(), body_unread)
}

impl Service {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        gate: Arc<Gate>,
        router: Router,
        enroller: Arc<Enroller>,
        proofs: Arc<PersonProofs>,
        caps: Arc<Caps>,
        limits: Limits,
        handlers: Arc<dyn Handlers>,
    ) -> Arc<Self> {
        let timer = Timer::start(gate.clone(), limits.stream_stall);
        let (limits_tcp, limits_unix, limits_preauth) = (
            limits.connections,
            limits.unix_connections,
            limits.tcp_preauth,
        );
        let salt = salt_for(&gate);
        Arc::new(Self {
            gate,
            router,
            enroller,
            proofs,
            caps,
            limits,
            handlers,
            timer,
            uid: rustix::process::geteuid().as_raw(),
            tcp_workers: Arc::new(Workers::new(limits_tcp)),
            unix_workers: Arc::new(Workers::new(limits_unix)),
            preauth_workers: Arc::new(Workers::new(limits_preauth)),
            salt,
            draining: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
            grace_ended: AtomicBool::new(false),
            conns: Mutex::new(Vec::new()),
            requests: AtomicU64::new(0),
        })
    }

    /// Accepts on the socket until `stop`. A peer whose user isn't ours
    /// is closed at once, as is one past the socket's pool.
    pub fn run_unix(
        self: &Arc<Self>,
        listener: UnixListener,
        codec: Arc<dyn Codec>,
        stop: Arc<AtomicBool>,
    ) -> io::Result<()> {
        listener.set_nonblocking(true)?;
        while !stop.load(Ordering::SeqCst) {
            let stream = match listener.accept() {
                Ok((s, _)) => s,
                Err(e) => {
                    pause_after(&e);
                    continue;
                }
            };
            if stream.set_nonblocking(false).is_err() || peer_uid(&stream) != Some(self.uid) {
                continue;
            }
            let Some(permit) = self.unix_workers.take() else {
                continue;
            };
            let Ok(conn) =
                self.caps
                    .connection(Listener::Unix, &self.limits, self.gate.observe_mono())
            else {
                continue;
            };
            let me = self.clone();
            let codec = codec.clone();
            std::thread::spawn(move || {
                let _permit = permit;
                let Ok(closer) = Io::closer(&stream) else {
                    return;
                };
                let shared = Arc::new(Shared {
                    conn: Mutex::new(Some(conn)),
                    closer,
                    preauth: Mutex::new(None),
                    head_by: Mutex::new(None),
                    writing_since: Mutex::new(None),
                    streaming: AtomicBool::new(false),
                    cut_requested: AtomicBool::new(false),
                    busy: AtomicU8::new(IDLE),
                });
                me.register(&shared);
                let http = codec.connection(Box::new(stream), me.limits.head());
                me.connection(http, &shared, &Peer::Unix);
            });
        }
        Ok(())
    }

    /// Accepts on TCP until `stop`. Each connection holds a TCP slot and
    /// a pre-auth slot from accept, and the timer closes it at its 10 s
    /// pre-auth deadline, the TLS handshake included, and at 5 s from
    /// accept unless a whole head has arrived by then (S21, orchestrator
    /// #10194).
    pub fn run_tcp(
        self: &Arc<Self>,
        listener: TcpListener,
        tls: Arc<dyn TlsAcceptor>,
        codec: Arc<dyn Codec>,
        stop: Arc<AtomicBool>,
    ) -> io::Result<()> {
        listener.set_nonblocking(true)?;
        while !stop.load(Ordering::SeqCst) {
            let (stream, addr) = match listener.accept() {
                Ok(a) => a,
                Err(e) => {
                    pause_after(&e);
                    continue;
                }
            };
            if stream.set_nonblocking(false).is_err() {
                continue;
            }
            let Some(permit) = self.tcp_workers.take() else {
                continue;
            };
            let Some(preauth) = self.preauth_workers.take() else {
                continue;
            };
            let now = self.gate.observe_mono();
            let Ok(conn) = self.caps.connection(Listener::Tcp, &self.limits, now) else {
                continue;
            };
            let Ok(closer) = Io::closer(&stream) else {
                continue;
            };
            let shared = Arc::new(Shared {
                conn: Mutex::new(Some(conn)),
                closer,
                preauth: Mutex::new(Some(preauth)),
                head_by: Mutex::new(Some(now + self.limits.preauth_header_read)),
                writing_since: Mutex::new(None),
                streaming: AtomicBool::new(false),
                cut_requested: AtomicBool::new(false),
                busy: AtomicU8::new(IDLE),
            });
            self.register(&shared);
            self.timer.watch(&shared);
            let me = self.clone();
            let (tls, codec) = (tls.clone(), codec.clone());
            std::thread::spawn(move || {
                let _permit = permit;
                let now = me.gate.observe_mono();
                let left = match shared.lock().as_ref() {
                    Some(c) => c.deadline(me.limits.preauth_lifetime, now),
                    None => return,
                };
                if left.is_zero() {
                    shared.cut();
                    return;
                }
                let Ok(io) = tls.accept(stream, left) else {
                    shared.cut();
                    return;
                };
                let http = codec.connection(io, me.limits.head());
                me.connection(http, &shared, &Peer::of(addr));
            });
        }
        Ok(())
    }

    /// One connection's requests, one at a time, until it closes. Its
    /// slots go back when it ends, or when the timer cuts it.
    fn connection(&self, mut http: Box<dyn HttpConnection>, shared: &Arc<Shared>, peer: &Peer) {
        loop {
            // Draining: no new request on any connection (S3 step 3).
            if self.stopping.load(Ordering::SeqCst) {
                break;
            }
            let now = self.gate.observe_mono();
            let (first_byte, whole) = match shared.lock().as_ref() {
                None => return,
                Some(c) => read_deadlines(c, &self.limits, now),
            };
            if whole.is_zero() || first_byte.is_zero() {
                break;
            }
            let head = match http.read_head(first_byte, whole) {
                Ok(Some(h)) => {
                    // One swap decides it against the drain's: a request
                    // either runs to its answer or is cut before it runs
                    // (Terra 4 #10626, #10644).
                    if !shared.claim_request() || shared.lock().is_none() {
                        break;
                    }
                    shared.head_arrived();
                    h
                }
                Ok(None) | Err(ReadError::TimedOut | ReadError::Truncated | ReadError::Io(_)) => {
                    break;
                }
                Err(ReadError::TooLarge { reason, head }) => {
                    // A head has come, if refused: the budget is spent, so
                    // the timer can't cut the refusal mid-write (Terra 4
                    // #10375).
                    shared.head_arrived();
                    let r = Refusal::new(Code::ApiTooLarge, ErrorKind::Usage, reason);
                    self.refuse_unread(&mut *http, &r, head);
                    break;
                }
                Err(ReadError::Malformed { reason, head }) => {
                    shared.head_arrived();
                    let r = Refusal::new(Code::UsageInvalidArguments, ErrorKind::Usage, reason);
                    self.refuse_unread(&mut *http, &r, head);
                    break;
                }
            };
            let (response, close) = match self.request(&mut *http, shared, peer, &head) {
                Step::Respond(r, close) => (r, close),
                Step::Abort => break,
            };
            let keep = match shared.lock().as_ref() {
                Some(c) => c.keep_alive(),
                None => false,
            };
            let close = close || !keep || asks_close(&head) || self.stopping.load(Ordering::SeqCst);
            // A peer that stops reading is cut at the write deadline.
            let written = http.write_response(&self.finish(response), close, self.limits.write);
            shared.request_done();
            if written.is_err() || close {
                break;
            }
        }
        shared.request_done();
        shared.cut();
    }

    /// A head that couldn't be read: its refusal, without a body when the
    /// request was a `HEAD` (Terra 2 #9894), and then the connection
    /// closes.
    fn refuse_unread(&self, http: &mut dyn HttpConnection, r: &Refusal, head: bool) {
        let mut response = self.finish(r.response());
        if head {
            response.body.clear();
        }
        let _ = http.write_response(&response, true, self.limits.write);
    }

    /// A UUIDv7, never blank: should the system's randomness fail, a
    /// counter fills its random bits (Claude #9754, Terra 5 #9795, Terra 2
    /// #9894).
    fn request_id(&self, random: io::Result<[u8; 10]>) -> String {
        let random = random.unwrap_or_else(|_| {
            fallback_bits(self.salt, self.requests.fetch_add(1, Ordering::Relaxed))
        });
        uuid_v7(self.gate.now_ms(), random)
    }

    /// Counts a connection for the drain.
    fn register(&self, shared: &Arc<Shared>) {
        let mut conns = self.conns.lock().unwrap_or_else(|e| e.into_inner());
        conns.retain(|w| w.strong_count() > 0);
        conns.push(Arc::downgrade(shared));
    }

    fn live_conns(&self) -> Vec<Arc<Shared>> {
        let mut conns = self.conns.lock().unwrap_or_else(|e| e.into_inner());
        conns.retain(|w| w.strong_count() > 0);
        conns
            .iter()
            .filter_map(Weak::upgrade)
            .filter(|s| s.lock().is_some())
            .collect()
    }

    /// Whether `serve` is draining: a new turn is refused.
    pub fn draining(&self) -> bool {
        self.draining.load(Ordering::SeqCst)
    }

    /// The first SIGTERM or SIGINT: from now `turn.admit` is refused with
    /// `api.draining` (503), every other request still served (S3 step
    /// 1). The process's signal handling calls it, then [`Service::drain`].
    pub fn begin_drain(&self) {
        self.draining.store(true, Ordering::SeqCst);
    }

    /// A second signal: the drain grace ends at once (S3 step 2).
    pub fn end_grace(&self) {
        self.grace_ended.store(true, Ordering::SeqCst);
    }

    /// The drain, after [`Service::begin_drain`] (S3):
    /// 2. the grace: requests are served until `plan.grace` passes, no
    ///    supervisor reports a running turn, or a second signal ends it;
    /// 3. the listeners stop (`plan.stop`), idle connections close, and
    ///    requests under way get up to [`DRAIN_INFLIGHT`] to finish;
    ///    those still running then are cut;
    /// 4. every SSE stream ends with a normal close, so clients resume
    ///    from their cursors (A15).
    ///
    /// Then the caller removes the socket and `serve.json` and releases
    /// the lock ([`finish_drain`]), and exits 0 for [`Drained::Clean`],
    /// non-zero for [`Drained::CutOff`].
    pub fn drain(&self, plan: DrainPlan<'_>) -> Drained {
        self.begin_drain();
        let started = self.gate.observe_mono();
        while !self.grace_ended.load(Ordering::SeqCst)
            && (plan.turns_running)()
            && self.gate.observe_mono() < started + plan.grace
        {
            std::thread::sleep(GRACE_POLL);
        }
        // 3. No new connections, no new requests.
        plan.stop.store(true, Ordering::SeqCst);
        self.stopping.store(true, Ordering::SeqCst);
        for s in self.live_conns() {
            if !s.streaming.load(Ordering::SeqCst) && s.claim_cut() {
                s.cut();
            }
        }
        let until = self.gate.observe_mono() + DRAIN_INFLIGHT;
        let busy = |s: &Arc<Shared>| {
            s.busy.load(Ordering::SeqCst) == BUSY && !s.streaming.load(Ordering::SeqCst)
        };
        let mut outcome = Drained::Clean;
        loop {
            let running: Vec<_> = self.live_conns().into_iter().filter(busy).collect();
            if running.is_empty() {
                break;
            }
            if self.gate.observe_mono() >= until {
                for s in running {
                    s.cut();
                }
                outcome = Drained::CutOff;
                break;
            }
            std::thread::sleep(DRAIN_POLL);
        }
        // 4. Streams end with a normal close.
        if let Some(fanout) = plan.fanout {
            fanout.close_all(Closed::Draining);
        }
        // On the gate's clock, as the other drain limits are (Terra 4
        // #10626).
        let until = self.gate.observe_mono() + CUT_GRACE * 2;
        while self
            .live_conns()
            .iter()
            .any(|s| s.streaming.load(Ordering::SeqCst))
            && self.gate.observe_mono() < until
        {
            std::thread::sleep(DRAIN_POLL);
        }
        for s in self.live_conns() {
            s.cut();
        }
        outcome
    }

    /// The response's own headers: its request id, the boot, the date.
    fn finish(&self, mut r: Response) -> Response {
        let id = self.request_id(crate::random::bytes::<10>());
        r.headers.push(("Mdkg-Request-Id".into(), id));
        if let Ok(boot) = self.gate.boot_id() {
            r.headers.push(("Mdkg-Boot".into(), boot));
        }
        r.headers
            .push(("Date".into(), http_date(self.gate.now_ms() / 1000)));
        r
    }

    /// The answer to a `HEAD`: 405, with `Allow` naming the methods the
    /// path is served for, when it is, and no body.
    fn no_head(&self, head: &RequestHead) -> Response {
        let allowed: Vec<&str> = ["GET", "POST"]
            .into_iter()
            .filter(|m| {
                let probe = RequestHead {
                    method: (*m).into(),
                    ..head.clone()
                };
                self.router.route(&probe).is_ok()
            })
            .collect();
        let mut headers = Vec::new();
        if !allowed.is_empty() {
            headers.push(("Allow".into(), allowed.join(", ")));
        }
        Response {
            status: 405,
            headers,
            body: Vec::new(),
        }
    }

    /// The route's own body cap, inside the global one (S21).
    fn body_cap(&self, routed: &Result<Routed, Refusal>) -> Option<u64> {
        match routed {
            Ok(Routed::Enroll(_)) => Some(self.limits.enroll_body_bytes),
            Ok(Routed::PersonEnroll | Routed::PersonProve(_)) => {
                Some(self.limits.person_body_bytes)
            }
            Ok(Routed::Op { op, .. }) if op.name == PERSON_APPROVE => {
                Some(self.limits.approve_body_bytes)
            }
            _ => None,
        }
    }

    fn request(
        &self,
        http: &mut dyn HttpConnection,
        shared: &Arc<Shared>,
        peer: &Peer,
        head: &RequestHead,
    ) -> Step {
        // Each request reads the repository fresh (#13066 D2).
        mdkg_git::fresh_reads();
        // Whether a body is declared: a refusal before it is read closes
        // the connection.
        let declared =
            matches!(head.body_length(), Ok(Some(n)) if n > 0) || head.body_length().is_err();
        // No HEAD: answered before routing or authentication, with no
        // body, and the connection closed (S4, orchestrator #9774).
        if head.method == "HEAD" {
            return Step::Respond(self.no_head(head), true);
        }
        let routed = self.router.route(head);
        if let Err(e) = self.limits.check_head(head, self.body_cap(&routed)) {
            return refused(&e, declared);
        }
        let routed = match routed {
            // Unsigned, the public fields; a signed `GET /` is
            // authenticated as any request, so a tampered one is refused
            // and a valid one sees the graph too (S4, Terra 5 #9795).
            Ok(Routed::Root) if !signed(head) => {
                if let Err(e) = self
                    .gate
                    .check_origin(head)
                    .and_then(|()| media(head, declared))
                {
                    return refused(&e, declared);
                }
                return Step::Respond(self.handlers.root(None), declared);
            }
            Ok(Routed::Enroll(route)) => return self.enroll(http, shared, peer, head, &route),
            other => other,
        };
        // Signed: authenticate first, so a tampered path gets the
        // authentication error, never the route's (R12, SC6).
        let mut auth = match self.gate.authenticate(head) {
            Ok(a) => a,
            Err(e) => return refused(&e, declared),
        };
        let routed = match routed {
            Ok(r) => r,
            Err(e) => return refused(&e, declared),
        };
        // After the origin, which authentication checked first (Terra 2
        // #9922).
        if let Err(e) = media(head, declared) {
            return refused(&e, declared);
        }
        if let Err(e) = self.router.admit(&routed, &auth) {
            return refused(&e, declared);
        }
        // A later request is charged on its head; the first only once its
        // body matched its digest (S21, Conn::charge).
        let charged_first = match shared.lock().as_mut() {
            None => return Step::Abort,
            Some(c) if c.machine().is_some() => {
                if let Err(e) = c.charge(&auth, &self.limits, self.gate.observe_mono()) {
                    return refused(&e, true);
                }
                false
            }
            Some(_) => true,
        };
        let body = match self.body(http, shared, head) {
            Ok(b) => b,
            Err(step) => return step,
        };
        if let Err(e) = self.gate.check_body(&mut auth, &body) {
            return refused(&e, false);
        }
        if charged_first {
            match shared.lock().as_mut() {
                None => return Step::Abort,
                Some(c) => {
                    if let Err(e) = c.charge(&auth, &self.limits, self.gate.observe_mono()) {
                        return refused(&e, true);
                    }
                }
            }
            // Promoted: no longer pre-auth work.
            shared.preauth_worker();
        }
        let done = match &routed {
            Routed::PersonProve(id) => self.proofs.prove(&auth, id, &body, &|| self.gate.now_ms()),
            Routed::PersonEnroll => self.handlers.person_enroll(&auth, head, &body),
            // Draining: no new turn (S3 step 1); every other request is
            // still served through the grace.
            Routed::Op { op, .. } if op.name == TURN_ADMIT && self.draining() => {
                Err(Refusal::new(Code::ApiDraining, ErrorKind::Conflict, "draining").retry_after(1))
            }
            Routed::Op { op, params } if op.kind == Kind::Stream => {
                return self.stream(http, shared, &auth, op, params, head);
            }
            Routed::Op { op, params } => self.handlers.op(&auth, op, params, head, &body),
            Routed::Root => Ok(self.handlers.root(Some(&auth))),
            Routed::Enroll(_) => return Step::Abort,
        };
        match done {
            Ok(r) => Step::Respond(r, false),
            Err(e) => refused(&e, false),
        }
    }

    /// A stream operation: the handler opens it, and its frames are
    /// written as SSE until it closes or the connection does. It holds
    /// one of the machine's 32 stream slots (S21) while it runs; a
    /// revocation or an overflow cuts its connection through the frames'
    /// cut hook, and a write stuck past the stall limit is cut by the
    /// timer (S4). A refusal before the stream starts is an ordinary
    /// envelope (A15).
    fn stream(
        &self,
        http: &mut dyn HttpConnection,
        shared: &Arc<Shared>,
        auth: &Authenticated,
        op: &'static OpSpec,
        params: &[String],
        head: &RequestHead,
    ) -> Step {
        let slot = match self.caps.stream(auth.machine(), &self.limits) {
            Ok(s) => s,
            Err(e) => return refused(&e, false),
        };
        let StreamStart {
            mut frames,
            head: mut cursor,
        } = match self.handlers.stream(auth, op, params, head) {
            Ok(s) => s,
            Err(e) => return refused(&e, false),
        };
        // The cut releases a writer blocked in a write; an idle one, waiting
        // in `next`, is left to write the stream's last error frame. Without
        // a closer the 1 s bound couldn't hold, so the stream isn't started
        // (Terra 4 #10578).
        let Ok(closer) = http.closer() else {
            return Step::Abort;
        };
        let writer = shared.clone();
        frames.set_cut(Box::new(move || {
            // Recorded first, then checked: either this sees the write, or
            // the write sees the flag (`timed_write`).
            writer.cut_requested.store(true, Ordering::SeqCst);
            if writer.writing_since().is_some() {
                closer.close();
            }
        }));
        shared.streaming.store(true, Ordering::SeqCst);
        self.timer.watch(shared);
        let started = self.finish(Response {
            status: 200,
            headers: vec![],
            body: vec![],
        });
        let ok = self.timed_write(shared, false, || http.start_stream(&started.headers));
        let mut ok = ok.is_ok();
        while ok {
            let (id, event, data, end) = match frames.next() {
                Next::Event(e) => {
                    let event = event_type(&e.data);
                    let id = (!e.id.is_empty()).then_some(e.id);
                    (id, event, e.data, false)
                }
                Next::TooLarge { id, seq } => {
                    // The frame's own cursor and head are the skipped
                    // record's, as its `id:` is (Terra 4 #10578).
                    cursor = Some(id.clone());
                    let data = self.stream_error(
                        "api.too_large",
                        "event_too_large",
                        Some(seq),
                        cursor.as_deref(),
                    );
                    (Some(id), "mdkg.stream.error".to_owned(), data, false)
                }
                Next::Heartbeat => {
                    let data = self.heartbeat(cursor.as_deref());
                    (None, "mdkg.stream.heartbeat".to_owned(), data, false)
                }
                // A drain is a normal close: the client resumes (A15).
                Next::Closed(Closed::Draining) => break,
                Next::Closed(why) => {
                    let (code, reason) = closed_code(why);
                    let data = self.stream_error(code, reason, None, cursor.as_deref());
                    (None, "mdkg.stream.error".to_owned(), data, true)
                }
            };
            ok = self
                .timed_write(shared, end, || {
                    http.write_event(id.as_deref(), &event, &data)
                })
                .is_ok();
            if let Some(id) = id {
                cursor = Some(id);
            }
            if end {
                break;
            }
        }
        let _ = http.close_gracefully();
        shared.streaming.store(false, Ordering::SeqCst);
        drop(slot);
        Step::Abort
    }

    /// A stream write, marked for the timer's stall check while it runs.
    /// Once a cut is asked for, a frame write starting after it cuts
    /// itself: it is the in-flight one (#10141) and mustn't block (Terra 4
    /// #10604). The stream's last error frame (`last`) is still written,
    /// since an idle writer owes it, and the timer then gives it only
    /// [`CUT_GRACE`].
    fn timed_write(
        &self,
        shared: &Shared,
        last: bool,
        write: impl FnOnce() -> io::Result<()>,
    ) -> io::Result<()> {
        shared.writing(Some(self.gate.observe_mono()));
        if !last && shared.cut_requested.load(Ordering::SeqCst) {
            shared.closer.close();
        }
        let r = write();
        shared.writing(None);
        r
    }

    /// An ephemeral event's frame (A9): `durable` false, its `cursor` the
    /// last durable one the stream sent.
    fn ephemeral_frame(&self, kind: &str, cursor: Option<&str>, data: Value) -> String {
        let id = self.request_id(crate::random::bytes::<10>());
        json!({
            "schema": "mdkg.api.event.v1",
            "id": id,
            "cursor": cursor.unwrap_or_default(),
            "type": kind,
            "time": rfc3339_ms(self.gate.now_ms()),
            "actor": "mdkg:serve",
            "subjects": [],
            "durable": false,
            "data": data,
        })
        .to_string()
    }

    /// `mdkg.stream.heartbeat`, with the stream's head cursor (A9).
    fn heartbeat(&self, cursor: Option<&str>) -> String {
        self.ephemeral_frame("mdkg.stream.heartbeat", cursor, json!({"head": cursor}))
    }

    /// `mdkg.stream.error`: `{diagnostics, head}` (A15), and the `seq` of
    /// a record skipped as too large (orchestrator #10487).
    fn stream_error(
        &self,
        code: &str,
        reason: &str,
        seq: Option<u64>,
        cursor: Option<&str>,
    ) -> String {
        // The code's registered message, as an envelope's diagnostic has;
        // the reason word is apart (Terra 4 #10578).
        let message =
            Code::parse(code).map_or_else(|| reason.to_owned(), |c| c.summary().to_owned());
        let mut data = json!({
            "diagnostics": [{"code": code, "severity": "error", "message": message}],
            "head": cursor,
            "reason": reason,
        });
        if let Some(seq) = seq {
            data["seq"] = Value::from(seq);
        }
        self.ephemeral_frame("mdkg.stream.error", cursor, data)
    }

    fn enroll(
        &self,
        http: &mut dyn HttpConnection,
        shared: &Arc<Shared>,
        peer: &Peer,
        head: &RequestHead,
        route: &crate::enroll::Route,
    ) -> Step {
        let declared = !matches!(head.body_length(), Ok(None | Some(0)));
        // The origin on the head, before the source's rate or the body
        // (Terra 5 #9795); `handle` checks it again.
        if let Err(e) = self.gate.check_origin(head) {
            return refused(&e, declared);
        }
        // Admitted first, so a request refused for its media type is still
        // charged to its source (Terra 2 #9922).
        if let Err(e) = self.enroller.admit(route, head, peer) {
            return refused(&e, declared);
        }
        if let Err(e) = media(head, declared) {
            let e = self.enroller.refused_before_body(route, peer, e);
            return refused(&e, declared);
        }
        let body = match self.body(http, shared, head) {
            Ok(b) => b,
            Err(step) => return step,
        };
        match self.enroller.handle(route, head, &body, peer) {
            Ok(h) => {
                if let Some(ev) = h.event {
                    self.handlers.ephemeral(ev);
                }
                Step::Respond(h.response, false)
            }
            Err(e) => refused(&e, false),
        }
    }

    /// The declared body, read within the body deadline (and before
    /// authentication, the pre-auth one). Its length was checked against
    /// its cap with the head.
    fn body(
        &self,
        http: &mut dyn HttpConnection,
        shared: &Arc<Shared>,
        head: &RequestHead,
    ) -> Result<Vec<u8>, Step> {
        let len = match head.body_length() {
            Ok(Some(n)) => n,
            Ok(None) => return Ok(Vec::new()),
            Err(_) => return Err(Step::Abort),
        };
        let timeout = match shared.lock().as_ref() {
            Some(c) => c.body_deadline(&self.limits, self.gate.observe_mono()),
            None => return Err(Step::Abort),
        };
        if timeout.is_zero() {
            return Err(Step::Abort);
        }
        http.read_body(len, timeout).map_err(|_| Step::Abort)
    }
}

/// The waits for the next head: its first byte, then all of it. Idle
/// keep-alive for the first byte once the connection may keep alive (the
/// socket's always, TCP's once authenticated; Terra 5 #9795), else the
/// pre-auth header deadline for both.
fn read_deadlines(c: &Conn, limits: &Limits, now: Duration) -> (Duration, Duration) {
    let whole = c.header_deadline(limits, now);
    if c.preauth_deadline().is_none() {
        (limits.idle, whole)
    } else {
        (whole, whole)
    }
}

/// Whether the request carries a signature: then it is authenticated,
/// whatever its route.
fn signed(head: &RequestHead) -> bool {
    head.has("signature") || head.has("signature-input")
}

/// Every body is JSON (S4): a declared body with another media type is
/// refused, unread.
fn media(head: &RequestHead, declared: bool) -> Result<(), Refusal> {
    if !declared || json_body(head) {
        return Ok(());
    }
    Err(Refusal::new(
        Code::UsageInvalidArguments,
        ErrorKind::Usage,
        "content_type",
    )
    .with_status(415))
}

/// `Content-Type: application/json`, parameters allowed (S4).
fn json_body(head: &RequestHead) -> bool {
    match head.one("content-type") {
        Ok(Some(v)) => v
            .split(';')
            .next()
            .is_some_and(|t| t.trim().eq_ignore_ascii_case("application/json")),
        _ => false,
    }
}

/// A service's salt for fallback request ids, so another service's
/// differ: fresh randomness; else, should that fail, a hash of the boot
/// id; else, should the boot id fail too (a rotation that found no
/// randomness), a hash of the process and the monotonic time, never a
/// fixed value (Terra 4 #10077).
fn salt_for(gate: &Gate) -> [u8; 4] {
    use sha2::Digest;
    if let Ok(b) = crate::random::bytes::<4>() {
        return b;
    }
    let seed = gate.boot_id().unwrap_or_else(|_| {
        format!(
            "{}:{:?}:{:p}",
            std::process::id(),
            gate.observe_mono(),
            gate
        )
    });
    let mut salt = [0u8; 4];
    salt.copy_from_slice(&sha2::Sha256::digest(seed.as_bytes())[..4]);
    salt
}

/// A fallback id's random bits: the salt whole, where the UUID's version
/// and variant bits don't fall (random bytes 3 to 6, the id's bytes 9 to
/// 12), and a 24-bit counter after it (Terra 4 #10077).
fn fallback_bits(salt: [u8; 4], n: u64) -> [u8; 10] {
    let mut b = [0u8; 10];
    b[3..7].copy_from_slice(&salt);
    b[7..].copy_from_slice(&n.to_be_bytes()[5..]);
    b
}

/// A UUIDv7 (RFC 9562): 48 bits of Unix milliseconds, then the version,
/// 12 random bits, the variant and 62 random bits.
fn uuid_v7(ms: u64, random: [u8; 10]) -> String {
    let mut b = [0u8; 16];
    b[..6].copy_from_slice(&ms.to_be_bytes()[2..]);
    b[6..].copy_from_slice(&random);
    b[6] = 0x70 | (b[6] & 0x0f);
    b[8] = 0x80 | (b[8] & 0x3f);
    let h = crate::random::hex(&b);
    format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    )
}

/// A bounded count of connection threads.
struct Workers {
    held: Mutex<usize>,
    cap: usize,
}

/// One thread's permit, given back when it exits.
struct Permit(Arc<Workers>);

impl Drop for Permit {
    fn drop(&mut self) {
        let mut n = self.0.held.lock().unwrap_or_else(|e| e.into_inner());
        *n = n.saturating_sub(1);
    }
}

impl Workers {
    fn new(cap: usize) -> Self {
        Self {
            held: Mutex::new(0),
            cap,
        }
    }

    fn take(self: &Arc<Self>) -> Option<Permit> {
        let mut n = self.held.lock().unwrap_or_else(|e| e.into_inner());
        if *n >= self.cap {
            return None;
        }
        *n += 1;
        Some(Permit(self.clone()))
    }

    #[cfg(test)]
    fn held(&self) -> usize {
        *self.held.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// `Connection: close` from the client.
fn asks_close(head: &RequestHead) -> bool {
    head.all("connection")
        .any(|v| v.split(',').any(|t| t.trim().eq_ignore_ascii_case("close")))
}

/// After a failed accept: a brief pause for one that would block or a
/// spent resource (EMFILE), none for an interrupted or aborted one.
fn pause_after(e: &io::Error) {
    match e.kind() {
        io::ErrorKind::Interrupted | io::ErrorKind::ConnectionAborted => {}
        _ => std::thread::sleep(Duration::from_millis(20)),
    }
}

/// The socket peer's user (`getpeereid` or `SO_PEERCRED`; R9). `None`
/// when it can't be read, which refuses the connection.
pub fn peer_uid(s: &UnixStream) -> Option<u32> {
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
    {
        nix::unistd::getpeereid(s).ok().map(|(uid, _)| uid.as_raw())
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        rustix::net::sockopt::socket_peercred(s)
            .ok()
            .map(|c| c.uid.as_raw())
    }
    // Elsewhere the peer can't be read, so the socket serves no one.
    #[cfg(not(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "linux",
        target_os = "android"
    )))]
    {
        let _ = s;
        None
    }
}

/// The socket peer's process id, as the kernel recorded it at connect
/// (`LOCAL_PEERPID` on macOS, `SO_PEERCRED` on Linux): for serve-local's
/// turn check, which walks the caller's ancestry (dec-01a18dd9). `None`
/// when it can't be read; the caller then refuses what needs it. The pid
/// is the connecting process's: one that exits and whose pid is reused
/// before the check runs is a stated residual, as is a connecting process
/// its turn detached, whose ancestry no longer reaches the launcher.
pub fn peer_pid(s: &UnixStream) -> Option<u32> {
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    {
        nix::sys::socket::getsockopt(s, nix::sys::socket::sockopt::LocalPeerPid)
            .ok()
            .and_then(|pid| u32::try_from(pid).ok())
            .filter(|pid| *pid != 0)
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        rustix::net::sockopt::socket_peercred(s)
            .ok()
            .map(|c| c.pid.as_raw_nonzero().get())
            .and_then(|pid| u32::try_from(pid).ok())
    }
    #[cfg(not(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "linux",
        target_os = "android"
    )))]
    {
        let _ = s;
        None
    }
}

/// The SSE `event:` name of a rendered event: its `type`.
fn event_type(data: &str) -> String {
    serde_json::from_str::<Value>(data)
        .ok()
        .and_then(|v| v.get("type").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_else(|| "message".to_owned())
}

/// The code and reason of a stream's last frame.
fn closed_code(why: Closed) -> (&'static str, &'static str) {
    match why {
        Closed::Draining => ("api.draining", "draining"),
        Closed::Overflow => ("api.rate_limited", "stream_overflow"),
        Closed::MachineRevoked => ("api.machine_revoked", "machine_revoked"),
        Closed::PersonLapsed => ("api.person_unknown", "person_lapsed"),
        Closed::Failed => ("coord.store_failed", "store"),
        Closed::Reset => ("api.cursor_reset", "cursor_reset"),
        Closed::Corrupt => ("coord.store_failed", "corrupt_log"),
    }
}

/// Year, month and day of `days` since 1970-01-01 (Howard Hinnant's
/// civil-from-days).
fn civil(days: u64) -> (i64, u64, u64) {
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m as u64, d as u64)
}

/// RFC 3339 UTC to the millisecond: `2026-10-30T03:08:38.123Z`.
fn rfc3339_ms(ms: u64) -> String {
    let secs = ms / 1000;
    let (y, m, d) = civil(secs / 86_400);
    let rem = secs % 86_400;
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        ms % 1000
    )
}

/// An IMF-fixdate (RFC 9110): `Thu, 29 Oct 2026 03:30:00 GMT`.
fn http_date(secs: u64) -> String {
    const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (y, m, d) = civil(days);
    format!(
        "{}, {:02} {} {} {:02}:{:02}:{:02} GMT",
        DAYS[(days % 7) as usize],
        d,
        MONTHS[(m - 1) as usize],
        y,
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// The pre-auth timer: closes a TCP connection at its 10 s deadline and
/// frees its slots, even while its thread is still verifying (S21,
/// orchestrator #7678). One thread for every connection, on the gate's
/// monotonic clock.
struct Timer {
    gate: Arc<Gate>,
    /// How long a stream write may be stuck before the stream is cut.
    stall: Duration,
    watched: Mutex<Vec<Weak<Shared>>>,
    wake: Condvar,
}

/// How long a cut stream's last frame may take to write before the timer
/// cuts the connection (Terra 4 #10604).
const CUT_GRACE: Duration = Duration::from_secs(1);

/// A connection's request state: one compare-and-swap decides between a
/// request and the drain's cut of an idle connection (Terra 4 #10644).
const IDLE: u8 = 0;
const BUSY: u8 = 1;
const CUT: u8 = 2;

/// The operation the drain refuses: a new turn (S3 step 1).
pub const TURN_ADMIT: &str = "turn.admit";

/// How long requests under way may take to finish once the listeners stop
/// (S3 step 3).
pub const DRAIN_INFLIGHT: Duration = Duration::from_secs(10);

const DRAIN_POLL: Duration = Duration::from_millis(20);

/// How often the grace asks whether a turn still runs: that may read the
/// store, and the grace lasts up to 120 s (Terra 4 #10626).
const GRACE_POLL: Duration = Duration::from_millis(250);

/// What the drain needs from the process.
pub struct DrainPlan<'a> {
    /// `serve.drain_grace_s` (default 120 s).
    pub grace: Duration,
    /// Whether a supervisor still reports a running turn in its heartbeat.
    pub turns_running: &'a dyn Fn() -> bool,
    /// The `/v1/events` fan-out, whose streams end with a normal close.
    pub fanout: Option<&'a crate::tail::Fanout>,
    /// The accept loops' stop flag.
    pub stop: &'a AtomicBool,
}

impl std::fmt::Debug for DrainPlan<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DrainPlan")
            .field("grace", &self.grace)
            .finish_non_exhaustive()
    }
}

/// How the drain ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Drained {
    /// Every request under way finished: exit 0.
    Clean,
    /// The 10 s limit cut off a request: exit non-zero (S3 step 5).
    CutOff,
}

/// S3 step 5: removes the socket and `serve.json`, then releases the lock
/// (dropping it). Each is attempted; the first failure is returned.
pub fn finish_drain(lock: crate::state::ServeLock, socket: &std::path::Path) -> io::Result<()> {
    let socket = match std::fs::remove_file(socket) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    };
    let state = crate::state::ServeState::remove(&lock);
    drop(lock);
    socket.and(state)
}

/// How often the timer looks: a deadline is met within this.
const TICK: Duration = Duration::from_millis(50);

impl Timer {
    fn start(gate: Arc<Gate>, stall: Duration) -> Arc<Self> {
        let t = Arc::new(Self {
            gate,
            stall,
            watched: Mutex::new(Vec::new()),
            wake: Condvar::new(),
        });
        let weak = Arc::downgrade(&t);
        std::thread::spawn(move || {
            while let Some(t) = weak.upgrade() {
                t.tick();
                let guard = t.watched.lock().unwrap_or_else(|e| e.into_inner());
                let _ = t.wake.wait_timeout(guard, TICK);
            }
        });
        t
    }

    fn watch(&self, s: &Arc<Shared>) {
        self.watched
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Arc::downgrade(s));
    }

    /// Cuts every connection past its pre-auth deadline, or past its 5 s
    /// header budget without a whole head, and forgets the ended and the
    /// promoted ones. All under the list's lock, so a
    /// connection watched meanwhile is never dropped from it unwatched.
    fn tick(&self) {
        let now = self.gate.observe_mono();
        let mut watched = self.watched.lock().unwrap_or_else(|e| e.into_inner());
        watched.retain(|w| {
            let Some(s) = w.upgrade() else {
                return false;
            };
            let Some(preauth) = s.lock().as_ref().map(Conn::preauth_deadline) else {
                return false;
            };
            let preauth_due =
                preauth.is_some_and(|d| now >= d || s.head_by().is_some_and(|h| now >= h));
            // A stream whose cut was asked for gets only CUT_GRACE for the
            // write under way, its last frame (Terra 4 #10604).
            let limit = if s.cut_requested.load(Ordering::SeqCst) {
                CUT_GRACE
            } else {
                self.stall
            };
            let stalled = s.writing_since().is_some_and(|w| now >= w + limit);
            if preauth_due || stalled {
                s.cut();
                return false;
            }
            preauth.is_some() || s.streaming.load(Ordering::SeqCst)
        });
    }
}

#[cfg(test)]
mod tests;
