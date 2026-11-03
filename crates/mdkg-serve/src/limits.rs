//! Limits (S21, R45, R46): sizes, time limits, connection and stream caps,
//! and token buckets per machine.
//!
//! The codec (`mdkg-wire`) enforces the sizes while it reads, so nothing
//! is read past a limit; [`Limits::check_head`] checks the parsed head
//! again, and the declared body length before any body byte. The host's
//! own machine has no rate limits, but does have the connection caps.
//!
//! Every time here is monotonic: callers pass the gate's
//! [`crate::auth::Gate::observe_mono`], never the wall clock, so a clock
//! step neither refills a bucket early nor stalls it.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mdkg_format::Code;
use mdkg_graph::ErrorKind;
use mdkg_wire::codec::HeadLimits;
use serde::Deserialize;

use crate::auth::Authenticated;
use crate::http::{HeadExt, Length, RequestHead};
use crate::refusal::Refusal;

/// Sizes and caps (`serve.limits`, with S21's fixed values).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    pub request_line_bytes: usize,
    pub header_bytes: usize,
    pub header_count: usize,
    pub body_bytes: u64,
    /// The enrolment endpoints' body cap (S6).
    pub enroll_body_bytes: u64,
    /// The person enrolment routes' (`POST /persons:enroll`, `:prove`):
    /// room for a 16 KiB attestation object (S21, Terra 3 #9112).
    pub person_body_bytes: u64,
    /// A remote `person approve`'s: room for 64 KiB of DER roots (S21,
    /// Opus #9143).
    pub approve_body_bytes: u64,
    pub header_read: Duration,
    /// The header deadline of a TCP connection that hasn't yet sent an
    /// authenticated request (Decided, #6704).
    pub preauth_header_read: Duration,
    /// The most a TCP connection may stay open before its first
    /// authenticated request, from accept, however many public requests
    /// it sends (orchestrator #6822, Terra 4 #6786).
    pub preauth_lifetime: Duration,
    pub body_read: Duration,
    /// A response's write: a peer that stops reading is cut at it, so it
    /// can't pin a slot and a thread (Claude #9754).
    pub write: Duration,
    pub idle: Duration,
    /// TCP connections in all.
    pub connections: usize,
    /// TCP connections that haven't authenticated yet, within
    /// `connections` (Decided, #6704).
    pub tcp_preauth: usize,
    /// Unix-socket connections: their own pool, which TCP can't use up
    /// (Decided, #6704).
    pub unix_connections: usize,
    pub connections_per_machine: usize,
    pub streams_per_machine: usize,
    /// Stream backpressure: bytes buffered, and how long unwritable.
    pub stream_buffer_bytes: usize,
    pub stream_stall: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            request_line_bytes: 8 * 1024,
            header_bytes: 16 * 1024,
            header_count: 64,
            body_bytes: 4 * 1024 * 1024,
            enroll_body_bytes: 4 * 1024,
            person_body_bytes: 32 * 1024,
            approve_body_bytes: 128 * 1024,
            header_read: Duration::from_secs(10),
            preauth_header_read: Duration::from_secs(5),
            preauth_lifetime: Duration::from_secs(10),
            body_read: Duration::from_secs(30),
            write: Duration::from_secs(30),
            idle: Duration::from_secs(60),
            connections: 256,
            tcp_preauth: 64,
            unix_connections: 64,
            connections_per_machine: 64,
            streams_per_machine: 32,
            stream_buffer_bytes: 1024 * 1024,
            stream_stall: Duration::from_secs(30),
        }
    }
}

/// The configurable part, as `serve.limits` in the machine or user
/// config.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LimitsConfig {
    pub body_bytes: Option<u64>,
    pub streams_per_machine: Option<usize>,
    pub connections: Option<usize>,
}

impl Limits {
    /// The head limits wire's codec enforces while it reads a head.
    pub fn head(&self) -> HeadLimits {
        HeadLimits {
            request_line: self.request_line_bytes,
            header_bytes: self.header_bytes,
            header_count: self.header_count,
        }
    }

    /// The configured limits. `connections` must be at least 2: the
    /// pre-auth workers take at most half the pool (and at most 64), so
    /// authenticated work always keeps the rest (orchestrator #9931,
    /// Claude #10013).
    pub fn from_config(c: &LimitsConfig) -> Result<Self, String> {
        let mut l = Self::default();
        if let Some(n) = c.body_bytes {
            l.body_bytes = n;
        }
        if let Some(n) = c.streams_per_machine {
            l.streams_per_machine = n;
        }
        if let Some(n) = c.connections {
            if n < 2 {
                return Err(format!(
                    "serve.limits.connections is {n}; it must be at least 2"
                ));
            }
            l.connections = n;
            l.tcp_preauth = l.tcp_preauth.min(n / 2);
        }
        Ok(l)
    }

    fn too_large(reason: &'static str) -> Refusal {
        Refusal::new(Code::ApiTooLarge, ErrorKind::Usage, reason)
    }

    /// The parsed head against the size limits, and the declared body
    /// length against the smaller of `body_bytes` and the endpoint's own
    /// cap (`None` for none). Header bytes are counted in their shortest
    /// wire form (`name:value` and CRLF), so no valid head is refused;
    /// the codec counts the exact bytes as it reads.
    pub fn check_head(&self, head: &RequestHead, endpoint_cap: Option<u64>) -> Result<(), Refusal> {
        let line = head.method.len()
            + 1
            + head.path.len()
            + head.query.as_ref().map_or(0, |q| q.len() + 1)
            + " HTTP/1.1".len();
        if line > self.request_line_bytes {
            return Err(Self::too_large("request_line"));
        }
        if head.headers.len() > self.header_count {
            return Err(Self::too_large("header_count"));
        }
        let bytes: usize = head
            .headers
            .iter()
            .map(|(n, v)| n.len() + 1 + v.len() + 2)
            .sum();
        if bytes > self.header_bytes {
            return Err(Self::too_large("header_bytes"));
        }
        let cap = endpoint_cap.map_or(self.body_bytes, |c| c.min(self.body_bytes));
        match head.body_length() {
            Ok(Some(n)) if n > cap => Err(Self::too_large("body")),
            Ok(_) => Ok(()),
            Err(Length::TooLarge) => Err(Self::too_large("body")),
            Err(Length::Invalid) => Err(Refusal::new(
                Code::UsageInvalidArguments,
                ErrorKind::Usage,
                "body_length",
            )),
        }
    }
}

/// A token bucket: `rate` tokens a second, up to `burst`, on monotonic
/// time.
#[derive(Debug, Clone, Copy)]
pub struct Bucket {
    rate: f64,
    burst: f64,
    tokens: f64,
    at: Duration,
}

impl Bucket {
    pub fn new(rate: f64, burst: f64, mono_now: Duration) -> Self {
        Self {
            rate,
            burst,
            tokens: burst,
            at: mono_now,
        }
    }

    fn refill(&mut self, mono_now: Duration) {
        let dt = mono_now.saturating_sub(self.at).as_secs_f64();
        self.tokens = (self.tokens + dt * self.rate).min(self.burst);
        self.at = self.at.max(mono_now);
    }

    /// Takes one token, or says how many whole seconds until one is due.
    pub fn take(&mut self, mono_now: Duration) -> Result<(), u64> {
        self.refill(mono_now);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            Ok(())
        } else {
            Err(((1.0 - self.tokens) / self.rate).ceil().max(1.0) as u64)
        }
    }

    /// Full again: nothing to remember.
    pub(crate) fn idle(&mut self, mono_now: Duration) -> bool {
        self.refill(mono_now);
        self.tokens >= self.burst
    }
}

/// `serve.rate`.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rate {
    pub writes_per_s: f64,
    pub write_burst: f64,
    pub reads_per_s: f64,
    pub read_burst: f64,
}

impl Default for Rate {
    fn default() -> Self {
        Self {
            writes_per_s: 20.0,
            write_burst: 100.0,
            reads_per_s: 100.0,
            read_burst: 400.0,
        }
    }
}

impl Rate {
    /// Rates must be finite and positive, and bursts at least one, or a
    /// bucket could never admit, or never refill.
    pub fn validate(&self) -> Result<(), String> {
        for (name, v, min) in [
            ("writes_per_s", self.writes_per_s, f64::MIN_POSITIVE),
            ("reads_per_s", self.reads_per_s, f64::MIN_POSITIVE),
            ("write_burst", self.write_burst, 1.0),
            ("read_burst", self.read_burst, 1.0),
        ] {
            if !v.is_finite() || v < min {
                return Err(format!(
                    "serve.rate.{name} must be a finite number of at least {min}"
                ));
            }
        }
        Ok(())
    }
}

/// Per-machine buckets, reads and writes apart (R46).
#[derive(Debug)]
pub struct RateLimiter {
    rate: Rate,
    host: String,
    buckets: Mutex<HashMap<String, (Bucket, Bucket)>>,
}

impl RateLimiter {
    /// `host` is the host's own machine id, which has no rate limits.
    /// `rate` must have passed [`Rate::validate`].
    pub fn new(rate: Rate, host: String) -> Self {
        Self {
            rate,
            host,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// One request from `machine`: `api.rate_limited` (429) with
    /// `Retry-After` when its bucket is empty. Machines are approved ones
    /// only (the gate calls this after the signature verifies), so the
    /// table is bounded by the graph's machines.
    pub fn admit(&self, machine: &str, write: bool, mono_now: Duration) -> Result<(), Refusal> {
        if machine == self.host {
            return Ok(());
        }
        let mut buckets = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        let r = self.rate;
        let (reads, writes) = buckets.entry(machine.to_owned()).or_insert_with(|| {
            (
                Bucket::new(r.reads_per_s, r.read_burst, mono_now),
                Bucket::new(r.writes_per_s, r.write_burst, mono_now),
            )
        });
        let bucket = if write { writes } else { reads };
        bucket.take(mono_now).map_err(|secs| {
            Refusal::new(
                Code::ApiRateLimited,
                ErrorKind::Conflict,
                if write { "writes" } else { "reads" },
            )
            .retry_after(secs)
        })
    }

    /// Drops full buckets (the 60 s timer).
    pub fn drop_idle(&self, mono_now: Duration) -> usize {
        let mut buckets = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        let before = buckets.len();
        buckets.retain(|_, (r, w)| !(r.idle(mono_now) && w.idle(mono_now)));
        before - buckets.len()
    }
}

/// Where a connection came from, for enrolment's per-source limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Peer {
    /// The Unix socket: the host user, or a forward of it. One source.
    Unix,
    /// A TCP peer's address; the port never counts.
    Tcp(IpAddr),
    /// An IPv6 peer with a scope (a link-local address, say), so two
    /// peers on different interfaces stay apart (#7066). It counts as
    /// its address does.
    TcpScoped(Ipv6Addr, u32),
}

impl Peer {
    /// The peer of an accepted TCP connection: its address, and an IPv6
    /// scope id when it has one.
    pub fn of(addr: SocketAddr) -> Self {
        match addr {
            SocketAddr::V6(a) if a.scope_id() != 0 => Peer::TcpScoped(*a.ip(), a.scope_id()),
            a => Peer::Tcp(a.ip()),
        }
    }

    /// The peer as recorded with a pending request: `unix`, or the
    /// address (v4-mapped as v4) with its IPv6 scope id, as
    /// `fe80::1%2`, without its port.
    pub fn address(&self) -> String {
        match self {
            Peer::Unix => "unix".into(),
            Peer::TcpScoped(a, scope) => format!("{a}%{scope}"),
            Peer::Tcp(IpAddr::V6(a)) if a.to_ipv4_mapped().is_some() => a
                .to_ipv4_mapped()
                .map(|v4| v4.to_string())
                .unwrap_or_default(),
            Peer::Tcp(ip) => ip.to_string(),
        }
    }

    /// The key enrolment counts against: the IPv4 address, or the IPv6
    /// /64 (one host's usual allocation), with v4-mapped addresses as v4.
    /// The one exception: a link-local address (`fe80::/10`) keeps its
    /// scope, so the same link-local /64 on two interfaces is two sources
    /// (orchestrator #7413, Terra 5 #8175).
    pub fn source(&self) -> String {
        match self {
            Peer::Unix => "unix".into(),
            // Link-local addresses repeat on every interface: their source
            // keeps the scope, so one interface's peers can't spend
            // another's limits (orchestrator #7413).
            Peer::TcpScoped(a, scope) if a.segments()[0] & 0xffc0 == 0xfe80 => {
                format!("{}%{scope}", Peer::Tcp(IpAddr::V6(*a)).source())
            }
            Peer::TcpScoped(a, _) => Peer::Tcp(IpAddr::V6(*a)).source(),
            Peer::Tcp(IpAddr::V4(a)) => a.to_string(),
            Peer::Tcp(IpAddr::V6(a)) => match a.to_ipv4_mapped() {
                Some(v4) => v4.to_string(),
                None => {
                    let s = a.segments();
                    format!("{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3])
                }
            },
        }
    }
}

/// A fixed one-minute window per source: at most `per_minute` new
/// enrolments (S6), in a table of at most `capacity` sources. A new
/// source when the table is full, after dropping ended windows, is
/// refused; an active window is never evicted.
#[derive(Debug)]
pub struct EnrollLimiter {
    per_minute: u32,
    capacity: usize,
    /// `data.reason` when it refuses.
    reason: &'static str,
    windows: Mutex<HashMap<String, (Duration, u32)>>,
}

impl Default for EnrollLimiter {
    fn default() -> Self {
        Self::new(5, "enroll")
    }
}

impl EnrollLimiter {
    const WINDOW: Duration = Duration::from_secs(60);

    /// `per_minute` requests per source in each fixed minute.
    pub fn new(per_minute: u32, reason: &'static str) -> Self {
        Self {
            per_minute,
            capacity: 4096,
            reason,
            windows: Mutex::new(HashMap::new()),
        }
    }

    /// A smaller source table, for tests.
    #[cfg(test)]
    pub(crate) fn with_capacity(mut self, capacity: usize) -> Self {
        self.capacity = capacity;
        self
    }

    /// Charges one request to `peer`'s window, refusing it when the
    /// window is spent or the table is full.
    pub fn admit(&self, peer: &Peer, mono_now: Duration) -> Result<(), Refusal> {
        self.step(peer, mono_now, true)
    }

    /// Whether `peer`'s window is spent, without charging it or adding
    /// it to the table: a request that is charged only later, once its
    /// outcome is known, is refused here while the window lasts (#7056).
    pub fn peek(&self, peer: &Peer, mono_now: Duration) -> Result<(), Refusal> {
        self.step(peer, mono_now, false)
    }

    fn step(&self, peer: &Peer, mono_now: Duration, charge: bool) -> Result<(), Refusal> {
        let refused = |secs: u64| {
            Refusal::new(Code::ApiRateLimited, ErrorKind::Conflict, self.reason).retry_after(secs)
        };
        let mut windows = self.windows.lock().unwrap_or_else(|e| e.into_inner());
        let source = peer.source();
        // The Unix socket is one source with its own room: TCP sources
        // filling the table never refuse it (Terra 4 #6786). The table
        // holds at most `capacity` TCP sources and the socket.
        let tcp_sources = windows.len() - usize::from(windows.contains_key("unix"));
        if *peer != Peer::Unix && !windows.contains_key(&source) && tcp_sources >= self.capacity {
            windows.retain(|_, (start, _)| mono_now.saturating_sub(*start) < Self::WINDOW);
            if windows.len() - usize::from(windows.contains_key("unix")) >= self.capacity {
                // A new TCP source when the table is full is refused,
                // peeked or charged, so no source goes uncounted.
                return Err(refused(60));
            }
        }
        if !charge && !windows.contains_key(&source) {
            return Ok(());
        }
        let (start, count) = windows.entry(source).or_insert((mono_now, 0));
        if mono_now.saturating_sub(*start) >= Self::WINDOW {
            if !charge {
                // A peek never restarts an ended window, so it can't keep
                // a source's entry alive without a charge (Terra 3 #7411).
                return Ok(());
            }
            *start = mono_now;
            *count = 0;
        }
        if *count >= self.per_minute {
            let left = Self::WINDOW.saturating_sub(mono_now.saturating_sub(*start));
            return Err(refused(left.as_secs_f64().ceil() as u64));
        }
        if charge {
            *count += 1;
        }
        Ok(())
    }

    /// The 60 s timer: drops ended windows.
    pub fn drop_idle(&self, mono_now: Duration) -> usize {
        let mut windows = self.windows.lock().unwrap_or_else(|e| e.into_inner());
        let before = windows.len();
        windows.retain(|_, (start, _)| mono_now.saturating_sub(*start) < Self::WINDOW);
        before - windows.len()
    }
}

/// Which listener a connection came in on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Listener {
    Unix,
    Tcp,
}

/// Connection and stream counts. Each slot is a guard that gives itself
/// back when dropped, so a panicking or closed connection never leaks one.
#[derive(Debug, Default)]
pub struct Caps {
    counts: Mutex<CapCounts>,
}

#[derive(Debug, Default)]
struct CapCounts {
    unix: usize,
    tcp: usize,
    preauth: usize,
    per_machine: HashMap<String, usize>,
    streams: HashMap<String, usize>,
}

/// What a [`Slot`] holds.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SlotKind {
    Unix,
    Tcp,
    Preauth,
    Machine(String),
    Stream(String),
}

/// One held slot.
#[derive(Debug)]
pub struct Slot {
    caps: Arc<Caps>,
    kind: SlotKind,
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut c = self.caps.counts.lock().unwrap_or_else(|e| e.into_inner());
        match &self.kind {
            SlotKind::Unix => c.unix = c.unix.saturating_sub(1),
            SlotKind::Tcp => c.tcp = c.tcp.saturating_sub(1),
            SlotKind::Preauth => c.preauth = c.preauth.saturating_sub(1),
            SlotKind::Machine(m) => dec(&mut c.per_machine, m),
            SlotKind::Stream(m) => dec(&mut c.streams, m),
        }
    }
}

fn dec(map: &mut HashMap<String, usize>, key: &str) {
    if let Some(n) = map.get_mut(key) {
        *n = n.saturating_sub(1);
        if *n == 0 {
            map.remove(key);
        }
    }
}

fn capped(reason: &'static str) -> Refusal {
    Refusal::new(Code::ApiRateLimited, ErrorKind::Conflict, reason).retry_after(1)
}

impl Caps {
    fn take(self: &Arc<Self>, kind: SlotKind, cap: usize) -> Option<Slot> {
        let mut c = self.counts.lock().unwrap_or_else(|e| e.into_inner());
        let held = match &kind {
            SlotKind::Unix => c.unix,
            SlotKind::Tcp => c.tcp,
            SlotKind::Preauth => c.preauth,
            SlotKind::Machine(m) => c.per_machine.get(m).copied().unwrap_or(0),
            SlotKind::Stream(m) => c.streams.get(m).copied().unwrap_or(0),
        };
        if held >= cap {
            return None;
        }
        match &kind {
            SlotKind::Unix => c.unix += 1,
            SlotKind::Tcp => c.tcp += 1,
            SlotKind::Preauth => c.preauth += 1,
            SlotKind::Machine(m) => *c.per_machine.entry(m.clone()).or_default() += 1,
            SlotKind::Stream(m) => *c.streams.entry(m.clone()).or_default() += 1,
        }
        Some(Slot {
            caps: self.clone(),
            kind,
        })
    }

    /// A connection, at accept (`now`, monotonic). The Unix socket has its
    /// own pool; a TCP connection takes a TCP slot and a
    /// pre-authentication slot, which it gives back at its first
    /// authenticated request.
    pub fn connection(
        self: &Arc<Self>,
        on: Listener,
        limits: &Limits,
        now: Duration,
    ) -> Result<Conn, Refusal> {
        let (slot, preauth) = match on {
            Listener::Unix => (
                self.take(SlotKind::Unix, limits.unix_connections)
                    .ok_or_else(|| capped("connections"))?,
                None,
            ),
            Listener::Tcp => {
                let slot = self
                    .take(SlotKind::Tcp, limits.connections)
                    .ok_or_else(|| capped("connections"))?;
                let pre = self
                    .take(SlotKind::Preauth, limits.tcp_preauth)
                    .ok_or_else(|| capped("preauth"))?;
                (slot, Some(pre))
            }
        };
        Ok(Conn {
            caps: self.clone(),
            _slot: slot,
            preauth,
            machine: None,
            preauth_until: now + limits.preauth_lifetime,
        })
    }

    /// An SSE stream of `machine`.
    pub fn stream(self: &Arc<Self>, machine: &str, limits: &Limits) -> Result<Slot, Refusal> {
        self.take(
            SlotKind::Stream(machine.to_owned()),
            limits.streams_per_machine,
        )
        .ok_or_else(|| capped("streams"))
    }

    /// Open connections: (Unix, TCP, TCP not yet authenticated).
    pub fn connections(&self) -> (usize, usize, usize) {
        self.counts
            .lock()
            .map(|c| (c.unix, c.tcp, c.preauth))
            .unwrap_or((0, 0, 0))
    }
}

/// One connection's slots, and the machine it is charged to.
#[derive(Debug)]
pub struct Conn {
    caps: Arc<Caps>,
    _slot: Slot,
    preauth: Option<Slot>,
    machine: Option<(String, Slot)>,
    /// The pre-auth deadline: 10 s from accept (monotonic).
    preauth_until: Duration,
}

impl Conn {
    /// Charges the connection to the approved machine its request
    /// authenticated as: only an [`Authenticated`] request releases the
    /// pre-auth slot, never a pending key's (enrolment, reveal, poll),
    /// which keeps its slot and its 10 s lifetime (orchestrator #7129).
    /// The first request takes the machine's slot and gives back the
    /// pre-authentication one; a later request on the same connection
    /// from another machine is refused, and the listener closes the
    /// connection, so one machine's slots never carry another's requests
    /// (Terra 3 #6700). Verification is a pre-auth step too: an
    /// authentication finished after the pre-auth deadline is refused
    /// (`preauth_expired`) and the connection stays pre-auth.
    ///
    /// The first request is charged after its body checked out
    /// ([`Gate::check_body`](crate::auth::Gate::check_body)), on either
    /// listener; one that hasn't is refused (`body_unchecked`). Each later
    /// request is charged on its head, before its body is read.
    pub fn charge(
        &mut self,
        auth: &Authenticated,
        limits: &Limits,
        now: Duration,
    ) -> Result<(), Refusal> {
        if self.preauth.is_some() && now >= self.preauth_until {
            return Err(Refusal::unauthenticated("preauth_expired"));
        }
        // A connection's first request is charged only once its body
        // matched its signed digest, so a TCP connection stays pre-auth
        // through it (orchestrator #7678), and the socket's is held to the
        // same rule (Opus #8420). Later requests are charged on their
        // heads, before their bodies are read (Opus #8152).
        if self.machine.is_none() && !auth.body_checked() {
            return Err(Refusal::unauthenticated("body_unchecked"));
        }
        let machine = auth.machine();
        match &self.machine {
            Some((m, _)) if m == machine => Ok(()),
            Some(_) => Err(Refusal::unauthenticated("machine_changed")),
            None => {
                let slot = self
                    .caps
                    .take(
                        SlotKind::Machine(machine.to_owned()),
                        limits.connections_per_machine,
                    )
                    .ok_or_else(|| capped("machine_connections"))?;
                self.machine = Some((machine.to_owned(), slot));
                self.preauth = None;
                Ok(())
            }
        }
    }

    /// The one absolute deadline of a TCP connection that hasn't yet
    /// sent an approved machine's authenticated request: 10 s from
    /// accept, on the monotonic clock given to [`Caps::connection`]. It
    /// covers the whole pre-auth connection: the TLS handshake, the head,
    /// the body and verification (orchestrator #7129, Terra 2 #7123).
    /// `None` once authenticated, and on the Unix socket.
    ///
    /// The listener (L2's, in this crate) must apply it to every pre-auth
    /// step, through
    /// [`Conn::deadline`], and close the connection once it has passed.
    /// It must enforce it independently of verification: a timer closes
    /// the connection and releases its slots when the deadline passes,
    /// even while a verification is still running (orchestrator #7678).
    pub fn preauth_deadline(&self) -> Option<Duration> {
        self.preauth.as_ref().map(|_| self.preauth_until)
    }

    /// The time a step may take from `now`: its own limit, and before
    /// authentication never past [`Conn::preauth_deadline`]. Zero means
    /// the deadline has passed: the listener closes the connection rather
    /// than arm a read with it (a zero read timeout is an error in `std`).
    pub fn deadline(&self, own: Duration, now: Duration) -> Duration {
        match self.preauth_deadline() {
            Some(until) => own.min(until.saturating_sub(now)),
            None => own,
        }
    }

    /// The next request's head: the short pre-auth read (Decided, #6704)
    /// until authenticated, within the pre-auth deadline.
    pub fn header_deadline(&self, limits: &Limits, now: Duration) -> Duration {
        let own = if self.preauth.is_some() {
            limits.preauth_header_read
        } else {
            limits.header_read
        };
        self.deadline(own, now)
    }

    /// The rest of a request's body: `body_read`, within the pre-auth
    /// deadline, so a pre-auth `POST /enroll` sent a byte at a time can't
    /// hold its slots past 10 s (Opus #7109).
    pub fn body_deadline(&self, limits: &Limits, now: Duration) -> Duration {
        self.deadline(limits.body_read, now)
    }

    /// Whether the connection may carry another request after this
    /// response. A TCP connection that hasn't authenticated is closed
    /// after each response, public or refused: no keep-alive before
    /// authentication (orchestrator #6822, Terra 4 #6786).
    pub fn keep_alive(&self) -> bool {
        self.preauth.is_none()
    }

    pub fn machine(&self) -> Option<&str> {
        self.machine.as_ref().map(|(m, _)| m.as_str())
    }
}

#[cfg(test)]
mod tests;
