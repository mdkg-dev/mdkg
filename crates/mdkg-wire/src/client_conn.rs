//! The client side of one connection to `serve` (S4): writing a signed
//! request, reading its response, and reading a stream as an
//! [`EventStream`] (A15). The framing only: the remote backend, its
//! command routing and retries are lane L9's `client.rs`, built on this.
//!
//! **Requests.** The request line, `Host`, the caller's headers (the
//! signature's among them, from [`crate::sign`]), then the framing the
//! client owns: every body is JSON (S4), so a body gets
//! `Content-Type: application/json` and its `Content-Length`; a method
//! that may carry a body says `content-length: 0` when it has none. A
//! caller can't set `Host`, `Content-Length`, `Transfer-Encoding` or
//! `Connection`, or a `Content-Type` that isn't JSON. `HEAD` isn't sent:
//! serve answers it 405, and its response would carry a length with no
//! body after it (Terra 2 #10561).
//!
//! **Responses** are read strictly and within bounds, as the codec reads
//! requests: HTTP/1.1 only, CRLF lines, a head of at most
//! [`MAX_RESPONSE_HEAD`], no `Transfer-Encoding` (serve never chunks), one
//! `Content-Length` of plain digits, and a body of at most the caller's
//! limit. A response without a length is refused unless it is a stream.
//! Interim 1xx responses are skipped. After a response that says
//! `Connection: close` the caller drops the connection at once, so the
//! host's lingering read ends (Terra 2 #10418).
//!
//! **Streams.** Each SSE event's `data` is a bare event (A18): heartbeats
//! (`mdkg.stream.heartbeat`) are consumed silently (#7910), and
//! `mdkg.stream.error`'s `data.diagnostics` end the stream as its
//! diagnostics; a code this build doesn't know becomes `api.unsupported`,
//! naming it, never `api.unreachable`, which retries would take for a
//! network fault (Terra 2 #10561). Any other event goes to the caller
//! with its cursor (the SSE `id:`) and its compact JSON line.

use std::io::{self, Read};
use std::time::{Duration, Instant};

use mdkg_format::{Code, Diagnostic, Severity};
use mdkg_ops::wire::{Event, EventStream};
use serde_json::Value;

use crate::codec::{Io, METHODS};
use crate::sign::{Outgoing, header_ok};
use crate::sse::SseReader;

/// The `Host` every request carries. A host is reached by its socket or
/// its pinned key, so the name means nothing, and it isn't signed; RFC
/// 9112 section 3.2 still requires the header.
pub const HOST: &str = "mdkg";
/// The largest response head read.
pub const MAX_RESPONSE_HEAD: usize = 64 * 1024;
/// How long a stream may stay silent: three of serve's 15 s heartbeats.
pub const STREAM_IDLE: Duration = Duration::from_secs(45);

const CHUNK: usize = 8 * 1024;
/// Headers the client sets itself.
const FRAMING: [&str; 4] = ["host", "content-length", "transfer-encoding", "connection"];

fn invalid(why: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, why.to_owned())
}

fn bad_response(why: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, why.to_owned())
}

fn json_type(value: &str) -> bool {
    value
        .split(';')
        .next()
        .is_some_and(|t| t.trim().eq_ignore_ascii_case("application/json"))
}

/// A response's status and headers: names lowercased, values trimmed, in
/// the order sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponseHead {
    pub status: u16,
    pub headers: Vec<(String, String)>,
}

impl ResponseHead {
    /// The first value of `name` (lowercase).
    pub fn get(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    /// Whether the host ends the connection after this response.
    pub fn closes(&self) -> bool {
        self.headers
            .iter()
            .filter(|(n, _)| n == "connection")
            .flat_map(|(_, v)| v.split(','))
            .any(|t| t.trim().eq_ignore_ascii_case("close"))
    }

    fn is_stream(&self) -> bool {
        self.get("content-type").is_some_and(|v| {
            v.split(';')
                .next()
                .is_some_and(|t| t.trim().eq_ignore_ascii_case("text/event-stream"))
        })
    }

    /// The body's length: `None` for a stream, 0 where a status has none.
    fn length(&self) -> io::Result<Option<u64>> {
        if matches!(self.status, 204 | 304) {
            return Ok(Some(0));
        }
        let mut length = None;
        for (_, v) in self.headers.iter().filter(|(n, _)| n == "content-length") {
            if v.is_empty() || !v.bytes().all(|b| b.is_ascii_digit()) {
                return Err(bad_response("a content-length isn't a number"));
            }
            let n: u64 = v
                .parse()
                .map_err(|_| bad_response("a content-length is too large"))?;
            if length.is_some_and(|l| l != n) {
                return Err(bad_response("two content-lengths differ"));
            }
            length = Some(n);
        }
        match length {
            Some(n) => Ok(Some(n)),
            None if self.is_stream() => Ok(None),
            None => Err(bad_response("a response has no content-length")),
        }
    }
}

/// One connection to `serve`, one request at a time.
pub struct ClientConnection {
    io: Box<dyn Io>,
    buf: Vec<u8>,
}

impl std::fmt::Debug for ClientConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientConnection")
            .field("buffered", &self.buf.len())
            .finish_non_exhaustive()
    }
}

impl ClientConnection {
    pub fn new(io: Box<dyn Io>) -> Self {
        Self {
            io,
            buf: Vec::new(),
        }
    }

    /// Writes `req`, all of it within `timeout`.
    pub fn send(&mut self, req: &Outgoing, timeout: Duration) -> io::Result<()> {
        let deadline = Instant::now() + timeout;
        let bytes = request_bytes(req)?;
        let mut rest = &bytes[..];
        while !rest.is_empty() {
            match self.io.write_by(rest, deadline) {
                Ok(0) => return Err(io::Error::from(io::ErrorKind::WriteZero)),
                Ok(n) => rest = &rest[n..],
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        self.io.set_write_timeout(Some(left))?;
        self.io.flush()
    }

    /// Reads once, before `deadline`; 0 is the end of the stream.
    fn fill(&mut self, deadline: Instant) -> io::Result<usize> {
        let mut chunk = [0u8; CHUNK];
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::ErrorKind::TimedOut.into());
            }
            self.io.set_read_timeout(Some(left))?;
            match self.io.read(&mut chunk) {
                Ok(n) => {
                    self.buf.extend_from_slice(&chunk[..n]);
                    return Ok(n);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    return Err(io::ErrorKind::TimedOut.into());
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// The response's head, within `timeout`, past any interim 1xx.
    pub fn read_head(&mut self, timeout: Duration) -> io::Result<ResponseHead> {
        let deadline = Instant::now() + timeout;
        loop {
            let head = loop {
                if let Some(end) = find_head_end(&self.buf) {
                    if end > MAX_RESPONSE_HEAD {
                        return Err(bad_response("a response head is too large"));
                    }
                    let head = parse_response(&self.buf[..end])?;
                    self.buf.drain(..end);
                    break head;
                }
                if self.buf.len() > MAX_RESPONSE_HEAD {
                    return Err(bad_response("a response head is too large"));
                }
                if self.fill(deadline)? == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "the connection closed before a response",
                    ));
                }
            };
            match head.status {
                101 => return Err(bad_response("a response switches protocols")),
                100..=199 => continue,
                _ => return Ok(head),
            }
        }
    }

    /// The body of a response whose head was just read: exactly its
    /// `Content-Length`, at most `max` bytes, within `timeout`.
    pub fn read_body(
        &mut self,
        head: &ResponseHead,
        max: usize,
        timeout: Duration,
    ) -> io::Result<Vec<u8>> {
        let deadline = Instant::now() + timeout;
        let Some(len) = head.length()? else {
            return Err(bad_response("a stream has no body to read whole"));
        };
        let len = usize::try_from(len)
            .ok()
            .filter(|n| *n <= max)
            .ok_or_else(|| bad_response("a response body is too large"))?;
        while self.buf.len() < len {
            if self.fill(deadline)? == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the connection closed part way through a response",
                ));
            }
        }
        let body: Vec<u8> = self.buf.drain(..len).collect();
        if head.closes() {
            self.buf.clear();
        }
        Ok(body)
    }

    /// The stream a `200 text/event-stream` head starts, each read within
    /// `idle`.
    pub fn into_events(self, head: &ResponseHead, idle: Duration) -> io::Result<Events> {
        if head.status != 200 || !head.is_stream() || head.length()?.is_some() {
            return Err(bad_response("the response isn't an event stream"));
        }
        Ok(Events {
            sse: SseReader::new(StreamBody {
                io: self.io,
                buffered: self.buf,
                pos: 0,
                idle,
            }),
        })
    }
}

/// A stream's body: bytes read with its head first, then the connection,
/// each read within `idle`.
struct StreamBody {
    io: Box<dyn Io>,
    buffered: Vec<u8>,
    pos: usize,
    idle: Duration,
}

impl Read for StreamBody {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos < self.buffered.len() {
            let n = buf.len().min(self.buffered.len() - self.pos);
            buf[..n].copy_from_slice(&self.buffered[self.pos..self.pos + n]);
            self.pos += n;
            return Ok(n);
        }
        self.io.set_read_timeout(Some(self.idle))?;
        match self.io.read(buf) {
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Err(io::ErrorKind::TimedOut.into()),
            r => r,
        }
    }
}

/// A stream from `serve`, as the CLI reads it.
pub struct Events {
    sse: SseReader<StreamBody>,
}

impl std::fmt::Debug for Events {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Events")
            .field("last_id", &self.sse.last_id())
            .finish()
    }
}

impl Events {
    /// The cursor to resume from: the last event id at an event boundary.
    pub fn last_id(&self) -> Option<&str> {
        self.sse.last_id()
    }
}

fn broken(message: String) -> Vec<Diagnostic> {
    vec![Diagnostic::error(Code::ApiUnreachable, message)]
}

impl EventStream for Events {
    fn next_event(&mut self) -> Result<Option<Event>, Vec<Diagnostic>> {
        loop {
            let ev = match self.sse.next_event() {
                Ok(Some(ev)) => ev,
                Ok(None) => return Ok(None),
                Err(e) => return Err(broken(format!("the event stream broke: {e}"))),
            };
            let value: Value = serde_json::from_str(&ev.data)
                .map_err(|_| broken("an event isn't JSON".to_owned()))?;
            match value.get("type").and_then(Value::as_str) {
                Some("mdkg.stream.heartbeat") => continue,
                Some("mdkg.stream.error") => return Err(stream_error(&value)),
                Some(_) => {}
                None => return Err(broken("an event has no type".to_owned())),
            }
            return Ok(Some(Event {
                id: ev.id.unwrap_or_default(),
                data: ev.data,
            }));
        }
    }
}

/// `mdkg.stream.error`'s `data.diagnostics`, as the CLI's diagnostics. A
/// code this build doesn't know is `api.unsupported`, its name kept in
/// the message: the coordinator sent it on purpose, so it isn't a
/// network fault to retry.
fn stream_error(value: &Value) -> Vec<Diagnostic> {
    let list = value
        .pointer("/data/diagnostics")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut out: Vec<Diagnostic> = list
        .iter()
        .map(|d| {
            let text = |k: &str| d.get(k).and_then(Value::as_str).map(str::to_owned);
            let message = text("message").unwrap_or_default();
            let named = text("code").unwrap_or_default();
            let (code, message) = match Code::parse(&named) {
                Some(code) => (code, message),
                None => (
                    Code::ApiUnsupported,
                    format!("the coordinator reported {named:?}: {message}"),
                ),
            };
            let severity = match text("severity").as_deref() {
                Some("warning") => Severity::Warning,
                Some("info") => Severity::Info,
                _ => Severity::Error,
            };
            let mut diag = Diagnostic::new(code, severity, message);
            diag.hint = text("hint");
            diag.node = text("node");
            diag.path = text("path");
            diag.line = d
                .get("line")
                .and_then(Value::as_u64)
                .and_then(|l| usize::try_from(l).ok());
            diag
        })
        .collect();
    if out.is_empty() {
        out = broken("the stream ended with an error it didn't name".to_owned());
    }
    out
}

fn target_ok(s: &str) -> bool {
    s.bytes().all(|b| b > b' ' && b != 0x7f && b != b'#')
}

/// The bytes of a request.
fn request_bytes(req: &Outgoing) -> io::Result<Vec<u8>> {
    if !METHODS.contains(&req.method.as_str()) || req.method == "HEAD" {
        return Err(invalid("a method the client doesn't send"));
    }
    if !req.path.starts_with('/') || !target_ok(&req.path) || req.path.contains('?') {
        return Err(invalid("a path that can't go on the request line"));
    }
    if req.query.as_deref().is_some_and(|q| !target_ok(q)) {
        return Err(invalid("a query that can't go on the request line"));
    }
    let mut out = format!("{} {}", req.method, req.path).into_bytes();
    if let Some(q) = &req.query {
        out.push(b'?');
        out.extend_from_slice(q.as_bytes());
    }
    out.extend_from_slice(b" HTTP/1.1\r\nhost: ");
    out.extend_from_slice(HOST.as_bytes());
    out.extend_from_slice(b"\r\n");
    let mut typed = false;
    for (name, value) in &req.headers {
        if !header_ok(name, value) {
            return Err(invalid("a header that can't be sent as given"));
        }
        if FRAMING.iter().any(|f| name.eq_ignore_ascii_case(f)) {
            return Err(invalid("a request sets its own framing header"));
        }
        if name.eq_ignore_ascii_case("content-type") {
            if !json_type(value) {
                return Err(invalid("every request body is JSON (S4)"));
            }
            typed = true;
        }
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(value.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    let bodied = req.method != "GET";
    if !req.body.is_empty() {
        if !bodied {
            return Err(invalid("a GET request carries no body"));
        }
        if !typed {
            out.extend_from_slice(b"content-type: application/json\r\n");
        }
    }
    if bodied {
        out.extend_from_slice(format!("content-length: {}\r\n", req.body.len()).as_bytes());
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(&req.body);
    Ok(out)
}

/// Where a head ends: just past its blank line.
fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

fn parse_response(raw: &[u8]) -> io::Result<ResponseHead> {
    // A bare LF or CR, a NUL, or obsolete folding: refused, not guessed at.
    for (i, b) in raw.iter().enumerate() {
        let bare_lf = *b == b'\n' && (i == 0 || raw[i - 1] != b'\r');
        let bare_cr = *b == b'\r' && raw.get(i + 1) != Some(&b'\n');
        if *b == 0 || bare_lf || bare_cr {
            return Err(bad_response("a response line isn't CRLF-ended text"));
        }
    }
    if raw
        .windows(3)
        .any(|w| w[0] == b'\n' && (w[1] == b' ' || w[1] == b'\t') && w[2] != b'\r')
    {
        return Err(bad_response("a response header is folded"));
    }
    let mut storage = [httparse::EMPTY_HEADER; 64];
    let mut resp = httparse::Response::new(&mut storage);
    match resp.parse(raw) {
        Ok(httparse::Status::Complete(n)) if n == raw.len() => {}
        _ => return Err(bad_response("a response head doesn't parse")),
    }
    if resp.version != Some(1) {
        return Err(bad_response("a response isn't HTTP/1.1"));
    }
    let status = resp
        .code
        .ok_or_else(|| bad_response("a response has no status"))?;
    let mut headers = Vec::with_capacity(resp.headers.len());
    for h in resp.headers.iter() {
        let value = std::str::from_utf8(h.value)
            .ok()
            .filter(|v| v.bytes().all(|b| b == b'\t' || (b' '..=b'~').contains(&b)))
            .ok_or_else(|| bad_response("a response header value isn't text"))?;
        let name = h.name.to_ascii_lowercase();
        if name == "transfer-encoding" {
            return Err(bad_response("a response is chunked; serve never chunks"));
        }
        headers.push((name, value.trim_matches([' ', '\t']).to_owned()));
    }
    Ok(ResponseHead { status, headers })
}

#[cfg(test)]
mod tests;
