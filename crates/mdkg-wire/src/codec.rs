//! HTTP/1.1 framing behind the traits `serve`'s listener is generic over
//! (S4, S21; serve-core's shapes, #9543): [`Io`] and [`Closer`] for the
//! byte stream, [`Codec`] and [`HttpConnection`] for one connection's
//! requests, responses and SSE frames.
//!
//! **The head, strictly.** A framing ambiguity is where request smuggling
//! starts, so what a lenient parser would guess at is refused as
//! [`ReadError::Malformed`]: a line not ending in CRLF (a bare CR or LF),
//! a NUL, obsolete line folding, any version but HTTP/1.1, a target that
//! isn't origin-form or carries a fragment, and a header value that isn't
//! visible ASCII, space or tab.
//!
//! **Bodies are serve's to judge.** The reader never de-chunks and never
//! acts on `Content-Length` or `Transfer-Encoding`: `serve` decides the
//! length (`RequestHead::body_length` there) and asks for exactly that
//! many bytes with [`HttpConnection::read_body`]. Bytes read past a head
//! stay buffered for that body, never for another request.
//!
//! **Limits and time.** [`HeadLimits`] are checked while the head is read,
//! so nothing is read past one. A head's first byte must arrive within
//! `read_head`'s `first_byte` of the call, and its last within `whole` of
//! its first byte; a body within its own timeout; a response within its own: a
//! client trickling a byte at a time can't hold a connection. After any error the connection is closed by
//! the caller: what follows on it can't be trusted to start a request.

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

/// A request's head, as `serve` reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestHead {
    pub method: String,
    /// The raw path, its percent-encoding intact (RFC 9421's `@path`).
    pub path: String,
    /// The query as sent, without the `?`: `Some("")` for a bare `?`.
    pub query: Option<String>,
    /// Names lowercased, values trimmed, in the order sent.
    pub headers: Vec<(String, String)>,
}

/// A response `serve` writes. The codec adds `Content-Length`, and
/// `Connection: close` when asked; `headers` must not carry either, or
/// `Transfer-Encoding`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Size limits the codec enforces while reading a head.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeadLimits {
    /// The request line, without its CRLF.
    pub request_line: usize,
    /// The header lines after the request line, CRLFs included.
    pub header_bytes: usize,
    pub header_count: usize,
}

/// A byte stream: the Unix socket, or TLS over TCP.
///
/// **Timeouts are per operation** (orchestrator's call on #9945): after
/// `set_read_timeout(Some(t))`, each `read` returns within `t`, data or
/// not, until the timeout is set again; the same for writes. Over TLS a
/// read or write bounds every socket operation it makes by its own
/// timeout.
pub trait Io: Read + Write + Send {
    fn set_read_timeout(&self, t: Option<Duration>) -> io::Result<()>;
    fn set_write_timeout(&self, t: Option<Duration>) -> io::Result<()>;
    /// A handle another thread uses to close the connection: the
    /// listener's pre-auth timer, which must close it and free its slots
    /// even while a verification runs (S21, #7678).
    fn closer(&self) -> io::Result<Box<dyn Closer>>;
    /// Ends the stream cleanly, so the peer can tell it from a cut one:
    /// TLS sends close_notify; both half-close the socket (#9945).
    fn close_gracefully(&mut self) -> io::Result<()> {
        Ok(())
    }
    /// One write that returns by `deadline`, partial or `TimedOut`. A
    /// socket's send timeout alone doesn't do this on macOS, where it
    /// restarts whenever the peer drains a little (Terra 1 #9908), so
    /// every implementation must bound it itself: the sockets here write
    /// without blocking and wait in short steps. No default, so a wrapper
    /// can't inherit an unbounded write (Terra 2 #10025).
    fn write_by(&mut self, buf: &[u8], deadline: Instant) -> io::Result<usize>;
}

/// Closes a connection from another thread.
pub trait Closer: Send + Sync {
    /// `shutdown(Both)` on the socket: a blocked read or write returns.
    fn close(&self);
    /// Whether the peer has gone: a read now would find the end of the
    /// stream, or the socket failed. It peeks without waiting and never
    /// consumes, so bytes the peer sent ahead (a pipelined request) stay
    /// for the reader, and a peer that sent bytes before closing reads as
    /// not gone until they're read. A peer that only shut down its
    /// sending half reads as gone. False when it can't tell.
    fn peer_closed(&self) -> bool {
        false
    }
    /// Reads and drops what the peer has sent, without waiting, up to
    /// `max` bytes: for a connection that will read no more requests, so
    /// a peer that sent bytes and then closed is still seen to have gone
    /// ([`peer_closed`](Self::peer_closed) sees the bytes, never the end
    /// behind them). Nothing gone and nothing dropped when it can't tell.
    fn drop_ahead(&self, max: usize) -> Ahead {
        let _ = max;
        Ahead::default()
    }
}

/// What [`Closer::drop_ahead`] found.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Ahead {
    /// The peer has gone: the end of the stream, or the socket failed.
    pub gone: bool,
    /// Bytes read and dropped.
    pub dropped: usize,
}

/// One connection's HTTP/1.1 framing, made per connection because its
/// reader buffers.
pub trait Codec: Send + Sync {
    fn connection(&self, io: Box<dyn Io>, limits: HeadLimits) -> Box<dyn HttpConnection>;
}

pub trait HttpConnection: Send {
    /// The next head: its first byte within `first_byte` of the call, all
    /// of it within `whole` of its first byte, so a kept-alive connection
    /// may idle up to `first_byte` and then has `whole` for its head
    /// (orchestrator's call on serve-core #10023 and Terra 2 #10025).
    /// Bytes already buffered start `whole` at the call. `Ok(None)` on a
    /// clean close before any byte, and once the codec has ended the
    /// connection after a response (see
    /// [`write_response`](Self::write_response)), after a lingering read
    /// of what the peer still sends. Never consumes a body byte it doesn't buffer for
    /// [`read_body`](Self::read_body). Both are intervals: an absolute
    /// cap, like the listener's pre-auth close at accept plus 5 s, is the
    /// caller's to enforce with its [`Closer`] (orchestrator's call on
    /// Terra 3 #10182).
    fn read_head(
        &mut self,
        first_byte: Duration,
        whole: Duration,
    ) -> Result<Option<RequestHead>, ReadError>;
    /// Exactly `len` body bytes, the buffered ones first, within
    /// `timeout`. The codec counts them against the head's declared
    /// `content-length` (see [`write_response`](Self::write_response)).
    fn read_body(&mut self, len: u64, timeout: Duration) -> Result<Vec<u8>, ReadError>;
    /// A response with `Content-Length`, all of it within `timeout` from
    /// the call. A final response with `close` says `Connection: close`,
    /// and the codec then ends the connection itself (below).
    ///
    /// A final response also says `Connection: close`, and the codec ends
    /// the connection after it, when what follows can't be trusted to
    /// start a request (Terra 3 #10083 and #10182, Terra 2 #10352):
    /// - a head or body read failed;
    /// - the head declared a body that `read_body` hasn't read all of, as
    ///   when serve answers before reading it or reads less: any
    ///   `transfer-encoding`, whose end the codec can't find, or a
    ///   `content-length` other than `0` that `read_body`'s lengths
    ///   haven't added up to (Terra 2 #10386);
    /// - bytes are buffered past what `read_body` took: a pipelined
    ///   request, never parsed. Only bytes already read count; a request
    ///   arriving after the response is the next one on a kept-alive
    ///   connection.
    ///
    /// Ending, the codec half-closes the connection cleanly, then reads and
    /// drops what the peer still sends until it closes, [`LINGER`] passes
    /// or [`LINGER_BYTES`] arrive, before this returns: closing with bytes
    /// unread would reset the connection under the answer (RFC 9112
    /// section 9.6; Terra 2 #10352, #10386). That wait isn't part of
    /// `timeout`. The next `read_head` returns `Ok(None)`.
    ///
    /// A 1xx does none of this, and after a failed read it is refused.
    /// After a head read failed before a method other than `HEAD` arrived
    /// (a bodiless refusal, see [`ReadError::Malformed`], or a timeout or
    /// cut there), the answer's body is dropped and it says
    /// `content-length: 0` (Terra 2 #10361).
    fn write_response(&mut self, r: &Response, close: bool, timeout: Duration) -> io::Result<()>;
    /// SSE: the 200 head (`text/event-stream`, no length, the connection
    /// closed at the stream's end), then one frame per event.
    fn start_stream(&mut self, headers: &[(String, String)]) -> io::Result<()>;
    fn write_event(&mut self, id: Option<&str>, event: &str, data: &str) -> io::Result<()>;
    /// An SSE comment line, for keep-alive.
    fn write_comment(&mut self, text: &str) -> io::Result<()>;
    fn closer(&self) -> io::Result<Box<dyn Closer>>;
    /// Ends the connection cleanly (TLS close_notify), as at the end of
    /// an SSE stream, with the lingering read `write_response` makes when
    /// it ends one; the closer is for cutting it. An error here (say
    /// "not connected", the peer already gone) needs no handling: the
    /// connection is ending either way (Claude 2 #10029).
    fn close_gracefully(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Why a head or body couldn't be read.
#[derive(Debug)]
pub enum ReadError {
    TimedOut,
    /// Past a [`HeadLimits`] limit: `"request_line"`, `"header_bytes"` or
    /// `"header_count"` (413 or 431, the rest unread).
    TooLarge {
        reason: &'static str,
        head: bool,
    },
    /// Not HTTP/1.1, a bad line, obs-fold, a bare CR or LF, a NUL, a
    /// method outside [`METHODS`] (400).
    ///
    /// In both refusals `head` means the answer carries no body: the
    /// request was a `HEAD`, or it was refused before one of the other
    /// [`METHODS`] was established (a malformed start, an unknown or
    /// overlong method), so it might have been one. The codec drops any
    /// body serve gives that answer, and the connection ends after it
    /// (orchestrator's call on Terra 3 #10182).
    Malformed {
        reason: &'static str,
        head: bool,
    },
    /// EOF part way through.
    Truncated,
    Io(io::Error),
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadError::TimedOut => f.write_str("timed out"),
            ReadError::TooLarge { reason, .. } => write!(f, "{reason} too large"),
            ReadError::Malformed { reason, .. } => write!(f, "malformed request: {reason}"),
            ReadError::Truncated => f.write_str("connection closed part way through"),
            ReadError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ReadError {}

/// The methods the codec reads (RFC 9110 section 9, and PATCH from RFC
/// 5789); any other is refused as malformed, with no body in its answer.
pub const METHODS: [&str; 9] = [
    "GET", "HEAD", "POST", "PUT", "DELETE", "CONNECT", "OPTIONS", "TRACE", "PATCH",
];

/// Whether a refusal of the head starting `buf` answers with no body: the
/// head isn't known to start with a method other than `HEAD`, then a
/// space.
fn bodiless(buf: &[u8]) -> bool {
    !METHODS
        .iter()
        .any(|m| *m != "HEAD" && buf.starts_with(m.as_bytes()) && buf.get(m.len()) == Some(&b' '))
}

impl ReadError {
    /// Marks a refusal whose answer carries no body (serve-core's shape).
    fn for_head(self, head: bool) -> Self {
        match self {
            ReadError::TooLarge { reason, .. } => ReadError::TooLarge { reason, head },
            ReadError::Malformed { reason, .. } => ReadError::Malformed { reason, head },
            other => other,
        }
    }
}

fn read_error(e: io::Error) -> ReadError {
    match e.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => ReadError::TimedOut,
        io::ErrorKind::UnexpectedEof => ReadError::Truncated,
        _ => ReadError::Io(e),
    }
}

/// A socket closer: a clone of the socket, shut down both ways.
pub(crate) struct SocketCloser<S>(pub(crate) S);

impl Closer for SocketCloser<UnixStream> {
    fn close(&self) {
        let _ = self.0.shutdown(Shutdown::Both);
    }
    fn peer_closed(&self) -> bool {
        peer_closed(&self.0)
    }
    fn drop_ahead(&self, max: usize) -> Ahead {
        drop_ahead(&self.0, max)
    }
}

impl Closer for SocketCloser<TcpStream> {
    fn close(&self) {
        let _ = self.0.shutdown(Shutdown::Both);
    }
    fn peer_closed(&self) -> bool {
        peer_closed(&self.0)
    }
    fn drop_ahead(&self, max: usize) -> Ahead {
        drop_ahead(&self.0, max)
    }
}

/// [`Closer::peer_closed`] for a socket: a one-byte peek that doesn't
/// wait (`MSG_DONTWAIT`, so the shared socket's own blocking mode is left
/// alone). End of stream, or an error other than "nothing yet", is gone.
fn peer_closed(socket: impl std::os::fd::AsFd) -> bool {
    use rustix::io::Errno;
    use rustix::net::RecvFlags;
    let mut byte = [0u8; 1];
    match rustix::net::recv(socket, &mut byte, RecvFlags::PEEK | RecvFlags::DONTWAIT) {
        Ok((0, _)) => true,
        Ok(_) => false,
        Err(e) => e != Errno::AGAIN && e != Errno::WOULDBLOCK && e != Errno::INTR,
    }
}

/// [`Closer::drop_ahead`] for a socket: reads that don't wait
/// (`MSG_DONTWAIT`, as [`peer_closed`]), until nothing more has arrived,
/// the end, a failure, or `max` bytes.
fn drop_ahead(socket: impl std::os::fd::AsFd, max: usize) -> Ahead {
    use rustix::io::Errno;
    use rustix::net::RecvFlags;
    let mut chunk = [0u8; 8192];
    let mut ahead = Ahead::default();
    while ahead.dropped < max {
        let want = chunk.len().min(max - ahead.dropped);
        match rustix::net::recv(&socket, &mut chunk[..want], RecvFlags::DONTWAIT) {
            Ok((0, _)) => {
                ahead.gone = true;
                break;
            }
            Ok((n, _)) => ahead.dropped += n,
            Err(Errno::INTR) => {}
            Err(e) => {
                ahead.gone = e != Errno::AGAIN && e != Errno::WOULDBLOCK;
                break;
            }
        }
    }
    ahead
}

/// A write on a socket that can't block past `deadline`: non-blocking,
/// waiting in steps of at most 2 ms while the socket is full.
pub(crate) fn write_nonblocking<S: Write>(
    sock: &mut S,
    set_nonblocking: impl Fn(&S, bool) -> io::Result<()>,
    buf: &[u8],
    deadline: Instant,
) -> io::Result<usize> {
    set_nonblocking(sock, true)?;
    let r = loop {
        // Checked before every attempt: a writable socket would take
        // bytes past an expired deadline (Terra 2 #10025).
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break Err(io::Error::new(io::ErrorKind::TimedOut, "write timed out"));
        }
        match sock.write(buf) {
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(left.min(Duration::from_millis(2)));
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            r => break r,
        }
    };
    set_nonblocking(sock, false)?;
    r
}

/// A Unix socket's timeout set, except where macOS refuses it on a socket
/// whose peer has closed: `setsockopt` then fails with EINVAL, though a
/// read still returns what is buffered, then the end, and a write fails
/// at once. Neither can block, so the timeout isn't needed, and the read
/// that follows sees the close as a close, not as an error. Zero is still
/// refused, as std refuses it before any `setsockopt`.
fn unix_timeout(r: io::Result<()>, t: Option<Duration>) -> io::Result<()> {
    match r {
        Err(e)
            if cfg!(target_os = "macos")
                && t != Some(Duration::ZERO)
                && e.raw_os_error() == Some(22) =>
        {
            Ok(())
        }
        r => r,
    }
}

impl Io for UnixStream {
    fn set_read_timeout(&self, t: Option<Duration>) -> io::Result<()> {
        unix_timeout(UnixStream::set_read_timeout(self, t), t)
    }
    fn set_write_timeout(&self, t: Option<Duration>) -> io::Result<()> {
        unix_timeout(UnixStream::set_write_timeout(self, t), t)
    }
    fn closer(&self) -> io::Result<Box<dyn Closer>> {
        Ok(Box::new(SocketCloser(self.try_clone()?)))
    }
    fn write_by(&mut self, buf: &[u8], deadline: Instant) -> io::Result<usize> {
        write_nonblocking(self, UnixStream::set_nonblocking, buf, deadline)
    }
    fn close_gracefully(&mut self) -> io::Result<()> {
        self.shutdown(Shutdown::Write)
    }
}

impl Io for TcpStream {
    fn set_read_timeout(&self, t: Option<Duration>) -> io::Result<()> {
        TcpStream::set_read_timeout(self, t)
    }
    fn set_write_timeout(&self, t: Option<Duration>) -> io::Result<()> {
        TcpStream::set_write_timeout(self, t)
    }
    fn closer(&self) -> io::Result<Box<dyn Closer>> {
        Ok(Box::new(SocketCloser(self.try_clone()?)))
    }
    fn write_by(&mut self, buf: &[u8], deadline: Instant) -> io::Result<usize> {
        write_nonblocking(self, TcpStream::set_nonblocking, buf, deadline)
    }
    fn close_gracefully(&mut self) -> io::Result<()> {
        self.shutdown(Shutdown::Write)
    }
}

/// The httparse codec.
#[derive(Debug, Default, Clone, Copy)]
pub struct Http1;

impl Codec for Http1 {
    fn connection(&self, io: Box<dyn Io>, limits: HeadLimits) -> Box<dyn HttpConnection> {
        Box::new(Connection::new(io, limits))
    }
}

/// How much one read asks for.
const CHUNK: usize = 8 * 1024;

/// How long one SSE frame's write may take: a peer that stops reading
/// fails it instead of holding the connection (S4's 30 s; Terra 4 #9744).
/// A response's write takes the timeout `write_response` is given.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// The longest the lingering read when the codec ends a connection runs.
pub const LINGER: Duration = Duration::from_secs(2);
/// The most the lingering read drops before it gives up.
pub const LINGER_BYTES: usize = 1 << 20;

/// How far a head has been scanned: each byte is looked at once, however
/// the head arrives (Terra 4 #9744).
#[derive(Debug, Clone, Copy, Default)]
struct Scan {
    /// The next byte to look at.
    pos: usize,
    /// Where the request line's CR is, once its CRLF has arrived.
    line_end: Option<usize>,
    /// The last LF seen, ending the request line or a header line.
    last_lf: usize,
    /// Header lines so far.
    lines: usize,
}

/// One connection: its stream, and the bytes read but not yet consumed.
///
/// **Read-ahead** (orchestrator's call on Terra 4 #9744): while a head is
/// read, one read asks for no more than the head limits still allow, so
/// at most `request_line + header_bytes + 4` bytes are ever buffered, and
/// any body bytes among them only those that came in the same read as the
/// head's end. They are never parsed or delivered until `read_body` is
/// called after admission; a connection closed after a refusal (#9464)
/// drops them unread. The bound is this buffer's: over TLS, rustls also
/// holds at most one decrypted record (16 KiB plus overhead) it has read
/// past, equally never delivered before `read_body` (orchestrator's call
/// on Terra 1 #9908).
pub struct Connection {
    io: Box<dyn Io>,
    limits: HeadLimits,
    buf: Vec<u8>,
    scan: Scan,
    write_timeout: Duration,
    /// Requests read and not yet given a final response. Only at zero is
    /// the connection between exchanges, and only then is an unclean close
    /// (TLS without close_notify) before a head's first byte taken as a
    /// clean one (#9945; orchestrator's calls on #10083 and #10296).
    outstanding: usize,
    /// A read failed: what follows can't be trusted to start a request, so
    /// the next final response ends the connection. `Some(true)` when that
    /// answer carries no body (see [`ReadError::Malformed`]).
    failed: Option<bool>,
    /// What the last head declared of its body that `read_body` hasn't
    /// read yet.
    unread: Unread,
    /// Ended by the codec (see [`HttpConnection::write_response`]).
    done: bool,
    /// How long the lingering read when ending may run.
    linger: Duration,
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection")
            .field("limits", &self.limits)
            .field("buffered", &self.buf.len())
            .finish_non_exhaustive()
    }
}

impl Connection {
    pub fn new(io: Box<dyn Io>, limits: HeadLimits) -> Self {
        Self {
            io,
            limits,
            buf: Vec::new(),
            scan: Scan::default(),
            write_timeout: WRITE_TIMEOUT,
            outstanding: 0,
            failed: None,
            unread: Unread::Nothing,
            done: false,
            linger: LINGER,
        }
    }

    /// Another write timeout than [`WRITE_TIMEOUT`].
    pub fn with_write_timeout(mut self, t: Duration) -> Self {
        self.write_timeout = t;
        self
    }

    /// Another bound on the lingering read than [`LINGER`].
    pub fn with_linger(mut self, t: Duration) -> Self {
        self.linger = t;
        self
    }

    /// Reads once, at most `max` bytes, before `deadline`. Returns how
    /// many arrived; 0 is the end of the stream. An interrupted read is
    /// retried with the time still left.
    fn fill(&mut self, deadline: Instant, max: usize) -> Result<usize, ReadError> {
        let mut chunk = [0u8; CHUNK];
        let want = max.clamp(1, CHUNK);
        let n = loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(ReadError::TimedOut);
            }
            self.io
                .set_read_timeout(Some(left))
                .map_err(ReadError::Io)?;
            match self.io.read(&mut chunk[..want]) {
                Ok(n) => break n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(read_error(e)),
            }
        };
        self.buf.extend_from_slice(&chunk[..n]);
        Ok(n)
    }

    /// Scans the bytes not yet looked at, refusing what's malformed or
    /// over a limit as soon as it shows, and stopping at the head's end:
    /// bytes after it are a body's, and never scanned (Claude #9693).
    /// Returns where the head ends, once it has.
    fn scan(&mut self) -> Result<Option<usize>, ReadError> {
        let l = self.limits;
        let buf = &self.buf;
        let mut s = self.scan;
        let mut end = None;
        while s.pos < buf.len() {
            let i = s.pos;
            let b = buf[i];
            if b == 0 {
                return Err(ReadError::Malformed {
                    reason: "NUL",
                    head: false,
                });
            }
            if i > 0 && buf[i - 1] == b'\r' && b != b'\n' {
                return Err(ReadError::Malformed {
                    reason: "bare CR",
                    head: false,
                });
            }
            if b == b'\n' && (i == 0 || buf[i - 1] != b'\r') {
                return Err(ReadError::Malformed {
                    reason: "bare LF",
                    head: false,
                });
            }
            match s.line_end {
                None => {
                    if b == b'\n' && i == 1 {
                        // An empty line before the request line: refused,
                        // so a method is always at byte 0 and a HEAD is
                        // never missed (Terra 2 #10025).
                        return Err(ReadError::Malformed {
                            reason: "empty line before the request line",
                            head: false,
                        });
                    }
                    if b == b'\n' {
                        s.line_end = Some(i - 1);
                        s.last_lf = i;
                    } else {
                        // A CR still waiting for its LF isn't content.
                        let content = if b == b'\r' { i } else { i + 1 };
                        if content > l.request_line {
                            return Err(ReadError::TooLarge {
                                reason: "request_line",
                                head: false,
                            });
                        }
                    }
                }
                Some(line_end) => {
                    let line_start = i == s.last_lf + 1 || (b == b'\n' && i == s.last_lf + 2);
                    if b == b'\n' && i == s.last_lf + 2 {
                        // A blank line: the head ends here, before any
                        // later bytes are counted against it.
                        s.pos = i + 1;
                        end = Some(i + 1);
                        break;
                    }
                    if b == b'\n' {
                        s.lines += 1;
                        if s.lines > l.header_count {
                            return Err(ReadError::TooLarge {
                                reason: "header_count",
                                head: false,
                            });
                        }
                        s.last_lf = i;
                    }
                    // The header bytes so far, CRLFs included, but not a
                    // CR that may start the blank line.
                    let mut bytes = i + 1 - (line_end + 2);
                    if b == b'\r' && line_start {
                        bytes -= 1;
                    }
                    if bytes > l.header_bytes {
                        return Err(ReadError::TooLarge {
                            reason: "header_bytes",
                            head: false,
                        });
                    }
                }
            }
            s.pos += 1;
        }
        self.scan = s;
        Ok(end)
    }

    /// How many more bytes a head may still need, so a read never takes
    /// more than the limits allow.
    fn head_room(&self) -> usize {
        let l = self.limits;
        (l.request_line + 2 + l.header_bytes + 2)
            .saturating_sub(self.buf.len())
            .max(1)
    }

    /// Writes all of `bytes` by `deadline`: before each write the time
    /// left is set, for reads too (a TLS write may need one), so a peer
    /// reading a little at a time, or not at all, can't stretch it
    /// (Claude #9884, Terra 1 #9908).
    fn write_by(&mut self, bytes: &[u8], deadline: Instant) -> io::Result<()> {
        let mut rest = bytes;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "write timed out"));
            }
            self.io.set_write_timeout(Some(left))?;
            self.io.set_read_timeout(Some(left))?;
            if rest.is_empty() {
                return self.io.flush();
            }
            match self.io.write_by(rest, deadline) {
                Ok(0) => return Err(io::Error::from(io::ErrorKind::WriteZero)),
                Ok(n) => rest = &rest[n..],
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }

    /// `read_head`'s work; the caller records a failure.
    fn next_head(
        &mut self,
        first_byte: Duration,
        whole: Duration,
    ) -> Result<Option<RequestHead>, ReadError> {
        // The first byte within `first_byte` of the call; the whole head
        // within `whole` of its first byte, so a keep-alive connection can
        // idle for `first_byte` and still get all of `whole` for its next
        // head (Terra 2 #10025). Bytes already buffered start it now.
        let start = Instant::now();
        let first_by = start + first_byte;
        let mut whole_by = (!self.buf.is_empty()).then(|| start + whole);
        loop {
            let bodiless = bodiless(&self.buf);
            let scanned = self.scan().map_err(|e| e.for_head(bodiless))?;
            if let Some(end) = scanned {
                let head =
                    parse_head(&self.buf[..end], self.limits).map_err(|e| e.for_head(bodiless))?;
                self.buf.drain(..end);
                self.scan = Scan::default();
                self.outstanding += 1;
                self.unread = declared_body(&head);
                return Ok(Some(head));
            }
            let room = self.head_room();
            let was_empty = self.buf.is_empty();
            let deadline = whole_by.unwrap_or(first_by);
            let n = match self.fill(deadline, room) {
                // An unclean close before a head's first byte, between
                // exchanges: the peer is done (orchestrator's call on
                // #9945). Anywhere else it's Truncated. A reset there is
                // a close too: Linux resets a Unix socket whose peer left
                // without reading its answer, where macOS reads the end.
                Err(ReadError::Truncated) if was_empty && self.outstanding == 0 => return Ok(None),
                Err(ReadError::Io(e))
                    if e.kind() == io::ErrorKind::ConnectionReset
                        && was_empty
                        && self.outstanding == 0 =>
                {
                    return Ok(None);
                }
                r => r?,
            };
            if n == 0 {
                return if was_empty {
                    Ok(None)
                } else {
                    Err(ReadError::Truncated)
                };
            }
            if whole_by.is_none() {
                whole_by = Some(Instant::now() + whole);
            }
        }
    }

    /// `read_body`'s work; the caller records a failure.
    fn body(&mut self, len: u64, timeout: Duration) -> Result<Vec<u8>, ReadError> {
        let deadline = Instant::now() + timeout;
        let want = usize::try_from(len).map_err(|_| ReadError::TooLarge {
            reason: "body",
            head: false,
        })?;
        let mut out = Vec::with_capacity(want.min(1 << 20));
        let take = want.min(self.buf.len());
        out.extend(self.buf.drain(..take));
        while out.len() < want {
            // Read straight into the body, never past it.
            if self.fill(deadline, want - out.len())? == 0 {
                return Err(ReadError::Truncated);
            }
            out.append(&mut self.buf);
        }
        Ok(out)
    }

    /// Ends the connection, once: half-closes it cleanly, then reads and
    /// drops what the peer still sends, until it closes, `linger` passes
    /// or [`LINGER_BYTES`] arrive, so the close doesn't reset the
    /// connection under the last answer (RFC 9112 section 9.6; Terra 2
    /// #10352, #10386).
    fn end(&mut self) -> io::Result<()> {
        if self.done {
            return Ok(());
        }
        self.done = true;
        self.buf.clear();
        let closed = self.io.close_gracefully();
        let deadline = Instant::now() + self.linger;
        let mut dropped = 0;
        while dropped < LINGER_BYTES {
            match self.fill(deadline, CHUNK) {
                Ok(n) if n > 0 => dropped += n,
                _ => break,
            }
            self.buf.clear();
        }
        self.buf.clear();
        closed
    }

    /// Records a failed read: the next final response ends the connection.
    fn fail(&mut self, e: &ReadError) {
        let bodiless = match e {
            ReadError::TooLarge { head, .. } | ReadError::Malformed { head, .. } => *head,
            _ => false,
        };
        self.failed = Some(bodiless);
    }

    /// A frame of a stream, within the write timeout.
    fn write_all_flush(&mut self, bytes: &[u8]) -> io::Result<()> {
        let deadline = Instant::now() + self.write_timeout;
        self.write_by(bytes, deadline)
    }
}

impl HttpConnection for Connection {
    fn read_head(
        &mut self,
        first_byte: Duration,
        whole: Duration,
    ) -> Result<Option<RequestHead>, ReadError> {
        if self.done || self.failed.is_some() {
            return Ok(None);
        }
        let r = self.next_head(first_byte, whole);
        if let Err(e) = &r {
            self.fail(e);
            // A head that timed out or was cut is judged as a refused one,
            // from what of it arrived: `HEAD /x HT`, then a stall, gets a
            // bodiless 408 (Terra 2 #10361).
            if bodiless(&self.buf) {
                self.failed = Some(true);
            }
        }
        r
    }

    fn read_body(&mut self, len: u64, timeout: Duration) -> Result<Vec<u8>, ReadError> {
        let r = self.body(len, timeout);
        match &r {
            Ok(body) => self.unread = self.unread.less(body.len()),
            Err(e) => self.fail(e),
        }
        r
    }

    fn write_response(&mut self, r: &Response, close: bool, timeout: Duration) -> io::Result<()> {
        let deadline = Instant::now() + timeout;
        let final_response = r.status >= 200;
        if self.failed.is_some() && !final_response {
            return Err(invalid("a failed read gets a final response"));
        }
        // After a refusal that may have been a HEAD's, no body, whatever
        // serve gave (orchestrator's call on Terra 3 #10182).
        let body: &[u8] = if self.failed == Some(true) {
            &[]
        } else {
            &r.body
        };
        let mut out = status_line(r.status)?;
        push_headers(&mut out, &r.headers)?;
        // 1xx, 204 and 304 carry no body and no Content-Length (RFC 9110
        // sections 8.6 and 15; Claude #9693). 205 carries no body but
        // says so, `content-length: 0`, so a kept-alive connection stays
        // framed (Terra 3 #9769).
        let no_body = (100..200).contains(&r.status) || matches!(r.status, 204 | 205 | 304);
        if no_body && !body.is_empty() {
            return Err(invalid("a 1xx, 204, 205 or 304 response has a body"));
        }
        if r.status == 205 {
            out.extend_from_slice(b"content-length: 0\r\n");
        } else if !no_body {
            out.extend_from_slice(format!("content-length: {}\r\n", body.len()).as_bytes());
        }
        // serve doesn't pipeline: bytes still buffered after a final
        // response, past what read_body took, are never parsed. The
        // response says so and the connection ends after it (orchestrator's
        // call on Terra 3 #10083), as it does after a failed read or with
        // a declared body unread (Terra 2 #10352).
        let leftover = !self.buf.is_empty() || self.unread != Unread::Nothing;
        let ending = final_response && (close || leftover || self.failed.is_some());
        if close || ending {
            out.extend_from_slice(b"connection: close\r\n");
        }
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(body);
        self.write_by(&out, deadline)?;
        if final_response {
            self.outstanding = self.outstanding.saturating_sub(1);
            self.unread = Unread::Nothing;
        }
        if ending {
            // Ending either way: an error here needs no handling.
            let _ = self.end();
        }
        Ok(())
    }

    fn start_stream(&mut self, headers: &[(String, String)]) -> io::Result<()> {
        // The stream's own headers are the codec's (Terra 4 #9744).
        if headers.iter().any(|(n, _)| {
            n.eq_ignore_ascii_case("content-type") || n.eq_ignore_ascii_case("cache-control")
        }) {
            return Err(invalid(
                "an SSE stream sets its own content-type and cache-control",
            ));
        }
        let mut out = status_line(200)?;
        out.extend_from_slice(b"content-type: text/event-stream\r\ncache-control: no-store\r\n");
        push_headers(&mut out, headers)?;
        out.extend_from_slice(b"connection: close\r\n\r\n");
        self.write_all_flush(&out)
    }

    fn write_event(&mut self, id: Option<&str>, event: &str, data: &str) -> io::Result<()> {
        self.write_all_flush(&sse_event(id, event, data)?)
    }

    fn write_comment(&mut self, text: &str) -> io::Result<()> {
        if !one_line(text) {
            return Err(invalid("an SSE comment holds a line break"));
        }
        self.write_all_flush(format!(": {text}\n\n").as_bytes())
    }

    fn closer(&self) -> io::Result<Box<dyn Closer>> {
        self.io.closer()
    }

    fn close_gracefully(&mut self) -> io::Result<()> {
        self.end()
    }
}

fn invalid(why: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, why.to_owned())
}

fn one_line(s: &str) -> bool {
    !s.contains(['\r', '\n', '\0'])
}

/// One SSE frame (WHATWG HTML's event stream): `id:`, `event:`, then a
/// `data:` line per line of `data`, then a blank line. A CR, LF or NUL in
/// the id or event name, or a CR or NUL in the data, would let the text
/// end the frame early, so it is refused.
pub fn sse_event(id: Option<&str>, event: &str, data: &str) -> io::Result<Vec<u8>> {
    if id.is_some_and(|i| !one_line(i)) || !one_line(event) || event.is_empty() {
        return Err(invalid(
            "an SSE id or event name is empty or holds a line break",
        ));
    }
    if data.contains(['\r', '\0']) {
        return Err(invalid("SSE data holds a CR or NUL"));
    }
    let mut out = Vec::new();
    if let Some(id) = id {
        out.extend_from_slice(format!("id: {id}\n").as_bytes());
    }
    out.extend_from_slice(format!("event: {event}\n").as_bytes());
    for line in data.split('\n') {
        out.extend_from_slice(format!("data: {line}\n").as_bytes());
    }
    out.push(b'\n');
    Ok(out)
}

/// Headers `write_response` and `start_stream` set themselves.
const FRAMING: [&str; 3] = ["content-length", "transfer-encoding", "connection"];

fn push_headers(out: &mut Vec<u8>, headers: &[(String, String)]) -> io::Result<()> {
    for (name, value) in headers {
        let token = !name.is_empty()
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b));
        if !token {
            return Err(invalid("a response header name isn't a token"));
        }
        if FRAMING.iter().any(|f| name.eq_ignore_ascii_case(f)) {
            return Err(invalid("a response sets its own framing header"));
        }
        if !value
            .bytes()
            .all(|b| b == b'\t' || (b' '..=b'~').contains(&b))
        {
            return Err(invalid("a response header value holds a control byte"));
        }
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(value.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    Ok(())
}

fn status_line(status: u16) -> io::Result<Vec<u8>> {
    if !(100..=599).contains(&status) {
        return Err(invalid("a status outside 100 to 599"));
    }
    Ok(format!("HTTP/1.1 {status} {}\r\n", reason(status)).into_bytes())
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        205 => "Reset Content",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        409 => "Conflict",
        411 => "Length Required",
        413 => "Content Too Large",
        414 => "URI Too Long",
        415 => "Unsupported Media Type",
        417 => "Expectation Failed",
        421 => "Misdirected Request",
        422 => "Unprocessable Content",
        425 => "Too Early",
        428 => "Precondition Required",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        505 => "HTTP Version Not Supported",
        _ => "Status",
    }
}

/// What a head declared of its body that `read_body` hasn't read. serve
/// judges the length; this only tells the codec there may be bytes it
/// must not take for a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unread {
    Nothing,
    Bytes(u64),
    /// A `transfer-encoding`, or a `content-length` that isn't one number:
    /// the codec can't tell where the body ends.
    Unknown,
}

impl Unread {
    /// After `read_body` returned `n` bytes.
    fn less(self, n: usize) -> Self {
        match self {
            Unread::Bytes(left) => match left.saturating_sub(n as u64) {
                0 => Unread::Nothing,
                left => Unread::Bytes(left),
            },
            other => other,
        }
    }
}

fn declared_body(head: &RequestHead) -> Unread {
    let mut length = None;
    for (name, value) in &head.headers {
        match name.as_str() {
            "transfer-encoding" => return Unread::Unknown,
            "content-length" => {
                let n = match value.parse::<u64>() {
                    Ok(n) if value.bytes().all(|b| b.is_ascii_digit()) => n,
                    _ => return Unread::Unknown,
                };
                if length.is_some_and(|l| l != n) {
                    return Unread::Unknown;
                }
                length = Some(n);
            }
            _ => {}
        }
    }
    match length {
        None | Some(0) => Unread::Nothing,
        Some(n) => Unread::Bytes(n),
    }
}

/// Parses a complete head, already scanned by [`Connection::scan`].
fn parse_head(raw: &[u8], limits: HeadLimits) -> Result<RequestHead, ReadError> {
    // Obsolete line folding: a header line starting with space or tab.
    if raw
        .windows(3)
        .any(|w| w[0] == b'\n' && (w[1] == b' ' || w[1] == b'\t') && w[2] != b'\n')
    {
        return Err(ReadError::Malformed {
            reason: "obs-fold",
            head: false,
        });
    }
    let mut storage = vec![httparse::EMPTY_HEADER; limits.header_count.max(1)];
    let mut req = httparse::Request::new(&mut storage);
    match req.parse(raw) {
        Ok(httparse::Status::Complete(n)) if n == raw.len() => {}
        Ok(_) => {
            return Err(ReadError::Malformed {
                reason: "head",
                head: false,
            });
        }
        Err(httparse::Error::TooManyHeaders) => {
            return Err(ReadError::TooLarge {
                reason: "header_count",
                head: false,
            });
        }
        Err(httparse::Error::Version) => {
            return Err(ReadError::Malformed {
                reason: "version",
                head: false,
            });
        }
        Err(httparse::Error::HeaderValue) => {
            return Err(ReadError::Malformed {
                reason: "header value",
                head: false,
            });
        }
        Err(httparse::Error::HeaderName) => {
            return Err(ReadError::Malformed {
                reason: "header name",
                head: false,
            });
        }
        Err(_) => {
            return Err(ReadError::Malformed {
                reason: "head",
                head: false,
            });
        }
    }
    if req.version != Some(1) {
        return Err(ReadError::Malformed {
            reason: "version",
            head: false,
        });
    }
    let method = req
        .method
        .ok_or(ReadError::Malformed {
            reason: "method",
            head: false,
        })?
        .to_owned();
    if !METHODS.contains(&method.as_str()) {
        return Err(ReadError::Malformed {
            reason: "method",
            head: false,
        });
    }
    let target = req.path.ok_or(ReadError::Malformed {
        reason: "target",
        head: false,
    })?;
    if !target.starts_with('/') || target.contains('#') {
        return Err(ReadError::Malformed {
            reason: "target",
            head: false,
        });
    }
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_owned(), Some(q.to_owned())),
        None => (target.to_owned(), None),
    };
    let mut headers = Vec::with_capacity(req.headers.len());
    for h in req.headers.iter() {
        if !h
            .value
            .iter()
            .all(|b| *b == b'\t' || (b' '..=b'~').contains(b))
        {
            return Err(ReadError::Malformed {
                reason: "header value",
                head: false,
            });
        }
        let value = std::str::from_utf8(h.value).map_err(|_| ReadError::Malformed {
            reason: "header value",
            head: false,
        })?;
        headers.push((
            h.name.to_ascii_lowercase(),
            value.trim_matches([' ', '\t']).to_owned(),
        ));
    }
    Ok(RequestHead {
        method,
        path,
        query,
        headers,
    })
}

#[cfg(test)]
mod tests;
